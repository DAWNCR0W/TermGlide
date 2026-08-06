//! Local Chrome coverage for page-target creation, target isolation, close rejection, and recovery.
//!
//! The test uses one bounded loopback HTTP fixture and public CDP commands. It checks target
//! lifecycle behavior without real-site assumptions or a browser display feed.

use std::error::Error;
use std::fs;
use std::io;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{Value, json};
use tg_browser::{
    ExternalEngineError, ExternalEngineLaunchOptions, ExternalEngineViewport,
    discover_external_engine, launch_external_engine,
};
use tg_core::{Cancellation, SystemClock};
use tg_network::{CdpError, CdpLimits, CdpSession, CdpTargetSession};
use tg_terminal::{
    Backend, ExternalFrameDecodeLimits, ExternalFrameOptions, ExternalFrameSequence,
    ExternalFrameSequenceLimits, ProjectionLimits, TerminalTransactionControl,
    TerminalTransactionError,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use url::Url;

const CDP_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_TIMEOUT: Duration = Duration::from_secs(3);
const REQUEST_HEADER_IDLE_TIMEOUT: Duration = Duration::from_millis(250);
const JOURNEY_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_REQUEST_BYTES: usize = 16 * 1024;
const MAX_RESPONSE_BYTES: usize = 32 * 1024;
const MAX_DESTROYED_TARGET_EVENTS: usize = 8;
const ROOT_ALLOCATION_ATTEMPTS: u8 = 32;
const PROFILE_PARENT: &str = "profiles";
const PRIMARY_PATH: &str = "primary";
const SECONDARY_PATH: &str = "secondary";

const PRIMARY_DOCUMENT: &str = r##"<!doctype html>
<meta charset="utf-8">
<title>Local primary target</title>
<link rel="icon" href="data:,">
<style>
:root { color-scheme: light; font: 18px/1.4 sans-serif; }
body { margin: 0; min-height: 100vh; background: #eff6ff; color: #1e3a8a; }
main { display: grid; gap: 14px; max-width: 680px; padding: 48px; }
#marker { padding: 16px; background: #bfdbfe; border: 2px solid #2563eb; }
</style>
<body data-target="primary">
  <main>
    <h1>Local primary target</h1>
    <p id="marker">This state belongs to the first page target.</p>
  </main>
  <script>sessionStorage.setItem("termglide-target-marker", "primary");</script>
</body>
"##;

const SECONDARY_DOCUMENT: &str = r##"<!doctype html>
<meta charset="utf-8">
<title>Local secondary target</title>
<link rel="icon" href="data:,">
<style>
:root { color-scheme: light; font: 18px/1.4 sans-serif; }
body { margin: 0; min-height: 100vh; background: #fdf2f8; color: #831843; }
main { display: grid; gap: 14px; max-width: 680px; padding: 48px; }
#marker { padding: 16px; background: #fbcfe8; border: 2px solid #db2777; }
</style>
<body data-target="secondary">
  <main>
    <h1>Local secondary target</h1>
    <p id="marker">This state belongs only to the newly created page target.</p>
  </main>
  <script>sessionStorage.setItem("termglide-target-marker", "secondary");</script>
</body>
"##;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TargetState {
    path: String,
    title: String,
    target: String,
    session_marker: String,
}

#[derive(Debug)]
enum TargetEventWaitOutcome {
    Cancelled,
    Settled(Result<(), CdpError>),
}

#[tokio::test]
async fn external_target_lifecycle_chrome_journey_is_observable_and_cleaned() -> TestResult {
    let executable = match discover_external_engine(None) {
        Ok(path) => path,
        Err(ExternalEngineError::ExecutableNotFound) => return Ok(()),
        Err(error) => return Err(box_error(error)),
    };
    ensure(
        executable.is_file(),
        "discovered external executable was not a file",
    )?;

    let mut root = TestRoot::new()?;
    let profile_parent = root.path().join(PROFILE_PARENT);
    let mut fixture = LoopbackFixture::start().await?;
    let mut options = ExternalEngineLaunchOptions::new(Url::parse("about:blank")?);
    options.executable_override = Some(executable.clone());
    options.viewport = Some(ExternalEngineViewport::new(800, 600)?);
    options.temporary_profile_parent = Some(profile_parent.clone());

    let launch_cancellation = Cancellation::new();
    let launch = launch_external_engine(&options, &launch_cancellation);
    let (operation, liveness, shutdown, profile_cleanup) = match launch {
        Ok(mut process) => {
            let profile_dir = process.isolated_profile_dir().to_path_buf();
            let endpoint = process.endpoint().websocket_url();
            let origin = fixture.origin().clone();
            let endpoint_loopback = ensure(
                process.endpoint().host == Ipv4Addr::LOCALHOST,
                format!(
                    "external Chrome debugging endpoint was not loopback-only: {}",
                    process.endpoint().host
                ),
            );
            let executable_provenance = ensure(
                process.executable_path() == executable.as_path(),
                format!(
                    "external Chrome selected an unplanned executable: {}",
                    process.executable_path().display()
                ),
            );
            let journey = exercise_target_lifecycle_journey(&endpoint, &origin).await;
            let operation = combine_results(
                endpoint_loopback,
                executable_provenance,
                journey,
                "external target lifecycle journey preconditions",
            );
            let liveness = live_process_result(process.try_wait());
            let shutdown = process.shutdown().map_err(box_error);
            drop(process);
            let profile_cleanup = ensure(
                !profile_dir.exists(),
                format!(
                    "external profile was not removed: {}",
                    profile_dir.display()
                ),
            );
            (operation, liveness, shutdown, profile_cleanup)
        }
        Err(error) => (Err(box_error(error)), Ok(()), Ok(()), Ok(())),
    };

    launch_cancellation.cancel();
    let launch_cancellation_cleanup = ensure(
        launch_cancellation.is_cancelled(),
        "launcher cancellation did not settle during cleanup",
    );
    let fixture_cleanup = fixture.shutdown().await;
    let profile_parent_cleanup = root.remove_empty_directory(&profile_parent);
    let root_cleanup = root.verify_empty_and_remove();
    finish_with_cleanup(
        operation,
        vec![
            ("browser liveness", liveness),
            ("browser shutdown", shutdown),
            ("browser profile", profile_cleanup),
            ("launcher cancellation", launch_cancellation_cleanup),
            ("loopback fixture", fixture_cleanup),
            ("profile parent", profile_parent_cleanup),
            ("test root", root_cleanup),
        ],
    )
}

async fn exercise_target_lifecycle_journey(endpoint: &str, origin: &Url) -> TestResult {
    let mut session = CdpSession::connect(endpoint, cdp_limits()).await?;
    let operation = match timeout(
        JOURNEY_TIMEOUT,
        run_target_lifecycle_journey(&mut session, origin),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(test_error(format!(
            "target lifecycle journey exceeded {JOURNEY_TIMEOUT:?}"
        ))),
    };
    let close = session.close().await.map_err(box_error);
    finish_with_cleanup(operation, vec![("CDP close", close)])
}

async fn run_target_lifecycle_journey(session: &mut CdpSession, origin: &Url) -> TestResult {
    let version = session.browser_version().await?;
    ensure(
        !version.protocol_version.is_empty()
            && !version.product.is_empty()
            && !version.revision.is_empty()
            && !version.user_agent.is_empty()
            && !version.js_version.is_empty(),
        "Browser.getVersion returned incomplete Chrome metadata",
    )?;
    session
        .send("Target.setDiscoverTargets", json!({ "discover": true }))
        .await?;
    cancel_unresolved_target_event_wait(session).await?;

    let primary_url = origin.join(PRIMARY_PATH)?;
    let secondary_url = origin.join(SECONDARY_PATH)?;
    let primary_session_id = {
        let mut primary = session.attach_first_page().await?;
        primary.page_enable().await?;
        primary.set_device_metrics(800, 600, 1.0).await?;
        navigate_and_wait(&mut primary, primary_url.as_str(), "primary target").await?;
        assert_target_state(
            &current_target_state(&mut primary).await?,
            "/primary",
            "Local primary target",
            "primary",
        )?;
        primary.session_id().to_owned()
    };

    let (secondary_target_id, secondary_session_id) =
        create_and_attach_page_target(session).await?;
    ensure(
        secondary_session_id != primary_session_id,
        "new page target reused the first flattened session identifier",
    )?;
    {
        let mut secondary = session.session(secondary_session_id.clone())?;
        secondary.page_enable().await?;
        secondary.set_device_metrics(640, 480, 1.0).await?;
        navigate_and_wait(&mut secondary, secondary_url.as_str(), "secondary target").await?;
        assert_target_state(
            &current_target_state(&mut secondary).await?,
            "/secondary",
            "Local secondary target",
            "secondary",
        )?;
        project_secondary_screenshot(&mut secondary).await?;
    }

    {
        let mut primary = session.session(primary_session_id.clone())?;
        assert_target_state(
            &current_target_state(&mut primary).await?,
            "/primary",
            "Local primary target",
            "primary",
        )?;
    }
    {
        let mut secondary = session.session(secondary_session_id.clone())?;
        assert_target_state(
            &current_target_state(&mut secondary).await?,
            "/secondary",
            "Local secondary target",
            "secondary",
        )?;
    }

    close_page_target(session, &secondary_target_id).await?;
    wait_for_target_destroyed(session, &secondary_target_id).await?;
    assert_stale_flattened_session_rejected(session, &secondary_session_id).await?;

    {
        let mut primary = session.session(primary_session_id.clone())?;
        navigate_and_wait(
            &mut primary,
            primary_url.as_str(),
            "primary target after secondary close",
        )
        .await?;
        assert_target_state(
            &current_target_state(&mut primary).await?,
            "/primary",
            "Local primary target",
            "primary",
        )?;
    }
    Ok(())
}

async fn create_and_attach_page_target(session: &mut CdpSession) -> TestResult<(String, String)> {
    let created = session
        .send("Target.createTarget", json!({ "url": "about:blank" }))
        .await?;
    let target_id = reply_string(&created.result, "targetId", "Target.createTarget")?;
    let attached = session
        .send(
            "Target.attachToTarget",
            json!({ "targetId": target_id.as_str(), "flatten": true }),
        )
        .await?;
    let session_id = reply_string(&attached.result, "sessionId", "Target.attachToTarget")?;
    ensure(
        !target_id.is_empty() && !session_id.is_empty(),
        "new page target returned an empty lifecycle identifier",
    )?;
    Ok((target_id, session_id))
}

async fn cancel_unresolved_target_event_wait(session: &mut CdpSession) -> TestResult {
    let cancellation = Cancellation::new();
    let cancellation_sender = cancellation.clone();
    let cancellation_task = tokio::spawn(async move {
        tokio::task::yield_now().await;
        cancellation_sender.cancel();
    });
    let outcome = tokio::select! {
        _ = cancellation.cancelled() => TargetEventWaitOutcome::Cancelled,
        event = session.wait_for_event("Target.targetDestroyed") => TargetEventWaitOutcome::Settled(event.map(|_| ())),
    };
    cancellation_task.await.map_err(|error| {
        test_error(format!(
            "target-event cancellation task stopped abnormally: {error}"
        ))
    })?;
    ensure(
        cancellation.is_cancelled(),
        "target-event cancellation signal did not settle",
    )?;
    match outcome {
        TargetEventWaitOutcome::Cancelled => Ok(()),
        TargetEventWaitOutcome::Settled(Ok(())) => Err(test_error(
            "an unrelated target destruction completed before target-event cancellation",
        )),
        TargetEventWaitOutcome::Settled(Err(error)) => Err(test_error(format!(
            "target-event wait settled with an error before cancellation: {error}"
        ))),
    }
}

async fn close_page_target(session: &mut CdpSession, target_id: &str) -> TestResult {
    let closed = session
        .send("Target.closeTarget", json!({ "targetId": target_id }))
        .await?;
    ensure(
        closed.result.get("success").and_then(Value::as_bool) == Some(true),
        format!(
            "Target.closeTarget did not report success: {:?}",
            closed.result
        ),
    )
}

async fn wait_for_target_destroyed(session: &mut CdpSession, target_id: &str) -> TestResult {
    for _ in 0..MAX_DESTROYED_TARGET_EVENTS {
        let event = session.wait_for_event("Target.targetDestroyed").await?;
        let destroyed = event
            .params
            .get("targetId")
            .and_then(Value::as_str)
            .ok_or_else(|| test_error("Target.targetDestroyed omitted targetId"))?;
        if destroyed == target_id {
            return Ok(());
        }
    }
    Err(test_error(format!(
        "Target.targetDestroyed did not identify the closed target {target_id:?} within its event bound"
    )))
}

async fn assert_stale_flattened_session_rejected(
    session: &mut CdpSession,
    stale_session_id: &str,
) -> TestResult {
    match session
        .send_in_session(
            stale_session_id,
            "Runtime.evaluate",
            json!({ "expression": "1", "returnByValue": true }),
        )
        .await
    {
        Err(CdpError::Protocol(_)) => Ok(()),
        Ok(reply) => Err(test_error(format!(
            "closed target accepted a stale flattened session command: {:?}",
            reply.result
        ))),
        Err(error) => Err(test_error(format!(
            "closed target did not reject its stale flattened session as a protocol error: {error}"
        ))),
    }
}

async fn navigate_and_wait(
    target: &mut CdpTargetSession<'_>,
    url: &str,
    label: &str,
) -> TestResult {
    let navigation = target.navigate(url).await?;
    ensure(
        navigation.error_text.is_none(),
        format!("{label} navigation failed: {:?}", navigation.error_text),
    )?;
    target.wait_for_load().await?;
    Ok(())
}

async fn current_target_state(target: &mut CdpTargetSession<'_>) -> TestResult<TargetState> {
    let evaluation = target
        .runtime_evaluate(
            "JSON.stringify({path:location.pathname,title:document.title,target:document.body.dataset.target || '',sessionMarker:sessionStorage.getItem('termglide-target-marker') || ''})",
            true,
        )
        .await?;
    ensure(
        evaluation.exception_details.is_none(),
        "target state evaluation raised a JavaScript exception",
    )?;
    let encoded = evaluation
        .result
        .get("value")
        .and_then(Value::as_str)
        .ok_or_else(|| test_error("target state evaluation did not return JSON"))?;
    Ok(serde_json::from_str(encoded)?)
}

fn assert_target_state(state: &TargetState, path: &str, title: &str, marker: &str) -> TestResult {
    ensure(
        state.path == path
            && state.title == title
            && state.target == marker
            && state.session_marker == marker,
        format!("target state was not isolated as required: {state:?}"),
    )
}

async fn project_secondary_screenshot(target: &mut CdpTargetSession<'_>) -> TestResult {
    let screenshot = target.capture_screenshot().await?;
    ensure(
        !screenshot.data.is_empty(),
        "Chrome returned an empty secondary-target screenshot",
    )?;
    let cancellation = Cancellation::new();
    let clock = SystemClock::default();
    let control = TerminalTransactionControl::new(&cancellation, &clock, None);
    let mut sequence = ExternalFrameSequence::new();
    let projected = sequence.push_png_base64_controlled(
        &screenshot.data,
        ExternalFrameOptions::new(
            Backend::Cells,
            64,
            24,
            ExternalFrameDecodeLimits::BROWSER_DEFAULT,
            ProjectionLimits::default(),
            ExternalFrameSequenceLimits::BROWSER_DEFAULT,
        ),
        &control,
    )?;
    ensure(
        !projected.operations().is_empty() && !projected.output().is_empty(),
        "terminal projection did not emit a bounded secondary-target cell frame",
    )?;
    ensure(
        sequence.previous().is_some(),
        "terminal projection did not retain its secondary-target cell baseline",
    )?;
    cancellation.cancel();
    ensure(
        matches!(
            control.checkpoint(),
            Err(TerminalTransactionError::Cancelled)
        ),
        "terminal projection cancellation was not observed after the secondary target frame",
    )
}

fn reply_string(result: &Value, field: &'static str, command: &str) -> TestResult<String> {
    let value = result
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| test_error(format!("{command} omitted {field}")))?;
    ensure(
        !value.is_empty(),
        format!("{command} returned an empty {field}"),
    )?;
    Ok(value.to_owned())
}

fn cdp_limits() -> CdpLimits {
    CdpLimits {
        operation_timeout: CDP_TIMEOUT,
        ..CdpLimits::default()
    }
}

fn live_process_result(result: Result<Option<ExitStatus>, ExternalEngineError>) -> TestResult {
    match result? {
        None => Ok(()),
        Some(status) => Err(test_error(format!(
            "external Chrome exited before cleanup: {status}"
        ))),
    }
}

fn combine_results(
    first: TestResult,
    second: TestResult,
    third: TestResult,
    label: &str,
) -> TestResult {
    let errors = [first.err(), second.err(), third.err()]
        .into_iter()
        .flatten()
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(test_error(format!("{label} failed: {}", errors.join("; "))))
    }
}

fn finish_with_cleanup(operation: TestResult, cleanups: Vec<(&str, TestResult)>) -> TestResult {
    let cleanup_errors = cleanups
        .into_iter()
        .filter_map(|(label, result)| result.err().map(|error| format!("{label}: {error}")))
        .collect::<Vec<_>>();
    match (operation, cleanup_errors.is_empty()) {
        (Ok(()), true) => Ok(()),
        (Ok(()), false) => Err(test_error(format!(
            "external target lifecycle cleanup failed: {}",
            cleanup_errors.join("; ")
        ))),
        (Err(error), true) => Err(error),
        (Err(error), false) => Err(test_error(format!(
            "external target lifecycle journey failed: {error}; cleanup also failed: {}",
            cleanup_errors.join("; ")
        ))),
    }
}

fn ensure(condition: bool, message: impl Into<String>) -> TestResult {
    if condition {
        Ok(())
    } else {
        Err(test_error(message.into()))
    }
}

fn box_error(error: impl Error + Send + Sync + 'static) -> Box<dyn Error + Send + Sync> {
    Box::new(error)
}

fn test_error(message: impl Into<String>) -> Box<dyn Error + Send + Sync> {
    Box::new(io::Error::other(message.into()))
}

struct LoopbackFixture {
    origin: Url,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<TestResult>>,
}

impl LoopbackFixture {
    async fn start() -> TestResult<Self> {
        for (label, document) in [
            ("primary", PRIMARY_DOCUMENT),
            ("secondary", SECONDARY_DOCUMENT),
        ] {
            ensure(
                document.len() <= MAX_RESPONSE_BYTES,
                format!("loopback {label} document exceeded its response bound"),
            )?;
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let origin = Url::parse(&format!("http://{address}/"))?;
        let (shutdown, mut receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut receiver => return Ok(()),
                    accepted = listener.accept() => {
                        let (stream, _) = accepted?;
                        serve_connection(stream).await?;
                    }
                }
            }
        });
        Ok(Self {
            origin,
            shutdown: Some(shutdown),
            task: Some(task),
        })
    }

    fn origin(&self) -> &Url {
        &self.origin
    }

    async fn shutdown(&mut self) -> TestResult {
        if let Some(sender) = self.shutdown.take() {
            let _send_result = sender.send(());
        }
        let task = self
            .task
            .take()
            .ok_or_else(|| test_error("loopback target lifecycle fixture was already shut down"))?;
        let joined = timeout(HTTP_TIMEOUT, task).await.map_err(|_| {
            test_error("loopback target lifecycle fixture did not stop before deadline")
        })?;
        joined.map_err(|error| {
            test_error(format!("loopback fixture task stopped abnormally: {error}"))
        })??;
        Ok(())
    }
}

async fn serve_connection(mut stream: TcpStream) -> TestResult {
    let Some(request) = read_request(&mut stream).await? else {
        return Ok(());
    };
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| test_error("loopback target lifecycle request omitted a target"))?;
    let (status, body) = if target == "/primary" {
        ("200 OK", PRIMARY_DOCUMENT)
    } else if target == "/secondary" {
        ("200 OK", SECONDARY_DOCUMENT)
    } else {
        ("404 Not Found", "not found")
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    );
    ensure(
        response.len() <= MAX_RESPONSE_BYTES,
        "loopback target lifecycle response exceeded its output bound",
    )?;
    timeout(HTTP_TIMEOUT, stream.write_all(response.as_bytes()))
        .await
        .map_err(|_| test_error("loopback target lifecycle response write timed out"))??;
    timeout(HTTP_TIMEOUT, stream.shutdown())
        .await
        .map_err(|_| test_error("loopback target lifecycle response shutdown timed out"))??;
    Ok(())
}

async fn read_request(stream: &mut TcpStream) -> TestResult<Option<String>> {
    let mut bytes = vec![0_u8; MAX_REQUEST_BYTES];
    let mut used = 0;
    loop {
        if used == bytes.len() {
            return Err(test_error(
                "loopback target lifecycle request exceeded its header/body bound",
            ));
        }
        let read = match timeout(REQUEST_HEADER_IDLE_TIMEOUT, stream.read(&mut bytes[used..])).await
        {
            Ok(result) => result?,
            Err(_) if used == 0 => return Ok(None),
            Err(_) => {
                return Err(test_error(
                    "loopback target lifecycle request headers stalled",
                ));
            }
        };
        if read == 0 {
            if used == 0 {
                return Ok(None);
            }
            return Err(test_error(
                "loopback target lifecycle peer closed mid-request",
            ));
        }
        used += read;
        if let Some(end) = header_end(&bytes[..used]) {
            return Ok(Some(std::str::from_utf8(&bytes[..end])?.to_owned()));
        }
    }
}

fn header_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

struct TestRoot {
    path: PathBuf,
    removed: bool,
}

impl TestRoot {
    fn new() -> io::Result<Self> {
        let parent = std::env::temp_dir();
        fs::create_dir_all(&parent)?;
        let timestamp = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(duration) => duration.as_nanos(),
            Err(_) => 0,
        };
        for attempt in 0..ROOT_ALLOCATION_ATTEMPTS {
            let path = parent.join(format!(
                "termglide-target-lifecycle-chrome-{}-{timestamp}-{attempt}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => {
                    return Ok(Self {
                        path,
                        removed: false,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique target lifecycle Chrome test root",
        ))
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn remove_empty_directory(&self, path: &Path) -> TestResult {
        if !path.exists() {
            return Ok(());
        }
        let mut entries = fs::read_dir(path)?;
        if entries.next().is_some() {
            return Err(test_error(format!(
                "test-owned directory was not empty: {}",
                path.display()
            )));
        }
        fs::remove_dir(path)?;
        Ok(())
    }

    fn verify_empty_and_remove(&mut self) -> TestResult {
        let mut entries = fs::read_dir(&self.path)?;
        if entries.next().is_some() {
            return Err(test_error(format!(
                "target lifecycle test root was not empty: {}",
                self.path.display()
            )));
        }
        fs::remove_dir(&self.path)?;
        self.removed = true;
        Ok(())
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        if !self.removed {
            let _cleanup_result = fs::remove_dir_all(&self.path);
        }
    }
}
