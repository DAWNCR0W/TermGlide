//! Local Chrome coverage for a stalled HTTP body, bounded CDP failure, cancellation, and recovery.
//!
//! The fixture owns one loopback origin only. It keeps one deliberately incomplete response open,
//! then verifies that the attached target remains usable for a normal local recovery and bounded
//! terminal-cell projection. No real-site behavior or browser display feed is involved.

use std::error::Error;
use std::fs;
use std::io;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::Value;
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
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use url::Url;

const CDP_TIMEOUT: Duration = Duration::from_secs(2);
const HTTP_TIMEOUT: Duration = Duration::from_secs(3);
const REQUEST_HEADER_IDLE_TIMEOUT: Duration = Duration::from_millis(250);
const JOURNEY_TIMEOUT: Duration = Duration::from_secs(12);
const MAX_REQUEST_BYTES: usize = 16 * 1024;
const MAX_RESPONSE_BYTES: usize = 32 * 1024;
const MAX_CONNECTIONS: usize = 8;
const ROOT_ALLOCATION_ATTEMPTS: u8 = 32;
const PROFILE_PARENT: &str = "profiles";
const STALLED_PATH: &str = "stall";
const RECOVERED_PATH: &str = "recovered";
const STALLED_CONTENT_LENGTH: usize = 4096;

const RECOVERED_DOCUMENT: &str = r##"<!doctype html>
<meta charset="utf-8">
<title>Local HTTP recovery</title>
<link rel="icon" href="data:,">
<style>
:root { color-scheme: light; font: 18px/1.4 sans-serif; }
body { margin: 0; min-height: 100vh; background: #ecfdf5; color: #14532d; }
main { display: grid; gap: 14px; max-width: 680px; padding: 48px; }
#recovery-marker { padding: 16px; background: #bbf7d0; border: 2px solid #15803d; }
</style>
<body data-recovery="ready">
  <main>
    <h1>Local HTTP recovery</h1>
    <p id="recovery-marker">The supervised target recovered on the same loopback origin.</p>
  </main>
</body>
"##;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Debug, Deserialize)]
struct RecoveryState {
    path: String,
    title: String,
    recovery: String,
}

#[derive(Debug)]
enum StalledLoadOutcome {
    Cancelled,
    Settled(Result<(), CdpError>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FixtureEvent {
    StalledResponseStarted,
}

#[tokio::test]
async fn external_http_recovery_chrome_journey_is_observable_and_cleaned() -> TestResult {
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
            let journey = exercise_http_recovery_journey(&endpoint, &mut fixture).await;
            let operation = combine_results(
                endpoint_loopback,
                executable_provenance,
                journey,
                "external HTTP recovery journey preconditions",
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

async fn exercise_http_recovery_journey(
    endpoint: &str,
    fixture: &mut LoopbackFixture,
) -> TestResult {
    let mut session = CdpSession::connect(endpoint, cdp_limits()).await?;
    let operation = match timeout(
        JOURNEY_TIMEOUT,
        run_http_recovery_journey(&mut session, fixture),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(test_error(format!(
            "HTTP recovery journey exceeded {JOURNEY_TIMEOUT:?}"
        ))),
    };
    let close = session.close().await.map_err(box_error);
    finish_with_cleanup(operation, vec![("CDP close", close)])
}

async fn run_http_recovery_journey(
    session: &mut CdpSession,
    fixture: &mut LoopbackFixture,
) -> TestResult {
    let version = session.browser_version().await?;
    ensure(
        !version.protocol_version.is_empty()
            && !version.product.is_empty()
            && !version.revision.is_empty()
            && !version.user_agent.is_empty()
            && !version.js_version.is_empty(),
        "Browser.getVersion returned incomplete Chrome metadata",
    )?;

    let recovered_url = fixture.url(RECOVERED_PATH)?;
    let stalled_url = fixture.url(STALLED_PATH)?;
    let mut target = session.attach_first_page().await?;
    target.page_enable().await?;
    target.set_device_metrics(800, 600, 1.0).await?;
    navigate_and_wait(&mut target, recovered_url.as_str(), "initial recovery page").await?;
    assert_recovery_state(&current_recovery_state(&mut target).await?)?;
    let attached_session = target.session_id().to_owned();

    let stalled = target.navigate(stalled_url.as_str()).await?;
    ensure(
        stalled.error_text.is_none(),
        format!(
            "stalled loopback navigation failed before its HTTP body could stall: {:?}",
            stalled.error_text
        ),
    )?;
    fixture.wait_for_stalled_response().await?;
    assert_stalled_load_deadline(&mut target).await?;
    cancel_stalled_load(&mut target).await?;

    navigate_and_wait(
        &mut target,
        recovered_url.as_str(),
        "post-stall recovery page",
    )
    .await?;
    ensure(
        target.session_id() == attached_session.as_str(),
        format!(
            "same supervised target was not retained after hostile HTTP recovery: before={attached_session:?}, after={:?}",
            target.session_id()
        ),
    )?;
    assert_recovery_state(&current_recovery_state(&mut target).await?)?;
    project_recovered_screenshot(&mut target).await?;
    Ok(())
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

async fn assert_stalled_load_deadline(target: &mut CdpTargetSession<'_>) -> TestResult {
    match target.wait_for_load().await {
        Err(CdpError::Timeout) => Ok(()),
        Ok(load) => Err(test_error(format!(
            "stalled HTTP body emitted a load event instead of reaching the CDP deadline: {load:?}"
        ))),
        Err(error) => Err(test_error(format!(
            "stalled HTTP body did not return typed CdpError::Timeout: {error}"
        ))),
    }
}

async fn cancel_stalled_load(target: &mut CdpTargetSession<'_>) -> TestResult {
    let cancellation = Cancellation::new();
    let cancellation_sender = cancellation.clone();
    let cancellation_task = tokio::spawn(async move {
        tokio::task::yield_now().await;
        cancellation_sender.cancel();
    });
    let outcome = tokio::select! {
        _ = cancellation.cancelled() => StalledLoadOutcome::Cancelled,
        load = target.wait_for_load() => StalledLoadOutcome::Settled(load.map(|_| ())),
    };
    cancellation_task.await.map_err(|error| {
        test_error(format!(
            "stalled-load cancellation task stopped abnormally: {error}"
        ))
    })?;
    ensure(
        cancellation.is_cancelled(),
        "stalled-load cancellation signal did not settle",
    )?;
    match outcome {
        StalledLoadOutcome::Cancelled => Ok(()),
        StalledLoadOutcome::Settled(Ok(())) => Err(test_error(
            "stalled HTTP body loaded before the cancellation boundary completed",
        )),
        StalledLoadOutcome::Settled(Err(error)) => Err(test_error(format!(
            "stalled HTTP wait settled with an error before cancellation: {error}"
        ))),
    }
}

async fn current_recovery_state(target: &mut CdpTargetSession<'_>) -> TestResult<RecoveryState> {
    let evaluation = target
        .runtime_evaluate(
            "JSON.stringify({path:location.pathname,title:document.title,recovery:document.body.dataset.recovery || ''})",
            true,
        )
        .await?;
    ensure(
        evaluation.exception_details.is_none(),
        "recovered-page state evaluation raised a JavaScript exception",
    )?;
    let encoded = evaluation
        .result
        .get("value")
        .and_then(Value::as_str)
        .ok_or_else(|| test_error("recovered-page state evaluation did not return JSON"))?;
    Ok(serde_json::from_str(encoded)?)
}

fn assert_recovery_state(state: &RecoveryState) -> TestResult {
    ensure(
        state.path == "/recovered"
            && state.title == "Local HTTP recovery"
            && state.recovery == "ready",
        format!("normal loopback recovery state was not observable: {state:?}"),
    )
}

async fn project_recovered_screenshot(target: &mut CdpTargetSession<'_>) -> TestResult {
    let screenshot = target.capture_screenshot().await?;
    ensure(
        !screenshot.data.is_empty(),
        "Chrome returned an empty recovered-page screenshot",
    )?;
    let cancellation = Cancellation::new();
    let clock = SystemClock::default();
    let control = TerminalTransactionControl::new(&cancellation, &clock, None);
    let mut sequence = ExternalFrameSequence::new();
    let projected = sequence.push_png_base64_controlled(
        &screenshot.data,
        ExternalFrameOptions::new(
            Backend::Cells,
            80,
            24,
            ExternalFrameDecodeLimits::BROWSER_DEFAULT,
            ProjectionLimits::default(),
            ExternalFrameSequenceLimits::BROWSER_DEFAULT,
        ),
        &control,
    )?;
    ensure(
        !projected.operations().is_empty() && !projected.output().is_empty(),
        "terminal projection did not emit a bounded recovered-page cell frame",
    )?;
    ensure(
        sequence.previous().is_some(),
        "terminal projection did not retain its recovered-page cell baseline",
    )?;
    cancellation.cancel();
    ensure(
        matches!(
            control.checkpoint(),
            Err(TerminalTransactionError::Cancelled)
        ),
        "terminal projection cancellation was not observed after the recovered frame",
    )
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
            "external HTTP recovery cleanup failed: {}",
            cleanup_errors.join("; ")
        ))),
        (Err(error), true) => Err(error),
        (Err(error), false) => Err(test_error(format!(
            "external HTTP recovery journey failed: {error}; cleanup also failed: {}",
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
    cancellation: Cancellation,
    events: mpsc::Receiver<FixtureEvent>,
    task: Option<JoinHandle<TestResult>>,
}

impl LoopbackFixture {
    async fn start() -> TestResult<Self> {
        ensure(
            RECOVERED_DOCUMENT.len() <= MAX_RESPONSE_BYTES,
            "loopback recovery document exceeded its response bound",
        )?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let origin = Url::parse(&format!("http://{address}/"))?;
        let cancellation = Cancellation::new();
        let server_cancellation = cancellation.child();
        let (event_sender, events) = mpsc::channel(MAX_CONNECTIONS);
        let task = tokio::spawn(async move {
            let mut connections = Vec::new();
            loop {
                tokio::select! {
                    _ = server_cancellation.cancelled() => break,
                    accepted = listener.accept() => {
                        let (stream, _) = accepted?;
                        if connections.len() >= MAX_CONNECTIONS {
                            return Err(test_error("loopback recovery fixture exceeded its connection bound"));
                        }
                        let connection_cancellation = server_cancellation.child();
                        let events = event_sender.clone();
                        connections.push(tokio::spawn(async move {
                            tokio::select! {
                                _ = connection_cancellation.cancelled() => Ok(()),
                                result = serve_connection(stream, connection_cancellation.child(), events) => result,
                            }
                        }));
                    }
                }
            }
            drop(event_sender);
            for connection in connections {
                let joined = timeout(HTTP_TIMEOUT, connection).await.map_err(|_| {
                    test_error("loopback recovery connection did not stop before deadline")
                })?;
                joined.map_err(|error| {
                    test_error(format!(
                        "loopback recovery connection stopped abnormally: {error}"
                    ))
                })??;
            }
            Ok(())
        });
        Ok(Self {
            origin,
            cancellation,
            events,
            task: Some(task),
        })
    }

    fn url(&self, path: &str) -> TestResult<Url> {
        Ok(self.origin.join(path)?)
    }

    async fn wait_for_stalled_response(&mut self) -> TestResult {
        let event = timeout(HTTP_TIMEOUT, self.events.recv())
            .await
            .map_err(|_| test_error("stalled loopback response did not begin before deadline"))?
            .ok_or_else(|| test_error("loopback fixture stopped before stalled response began"))?;
        ensure(
            event == FixtureEvent::StalledResponseStarted,
            format!("loopback fixture emitted an unrelated hostile-response event: {event:?}"),
        )
    }

    async fn shutdown(&mut self) -> TestResult {
        self.cancellation.cancel();
        let task = self
            .task
            .take()
            .ok_or_else(|| test_error("loopback recovery fixture was already shut down"))?;
        let joined = timeout(HTTP_TIMEOUT, task)
            .await
            .map_err(|_| test_error("loopback recovery fixture did not stop before deadline"))?;
        joined.map_err(|error| {
            test_error(format!("loopback fixture task stopped abnormally: {error}"))
        })??;
        Ok(())
    }
}

async fn serve_connection(
    mut stream: TcpStream,
    cancellation: Cancellation,
    events: mpsc::Sender<FixtureEvent>,
) -> TestResult {
    let Some(request) = read_request(&mut stream).await? else {
        return Ok(());
    };
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| test_error("loopback recovery request omitted a target"))?;
    if target == "/stall" {
        send_stalled_response(&mut stream, &cancellation, events).await
    } else if target == "/recovered" {
        send_html_response(&mut stream, "200 OK", RECOVERED_DOCUMENT).await
    } else {
        send_html_response(&mut stream, "404 Not Found", "not found").await
    }
}

async fn send_stalled_response(
    stream: &mut TcpStream,
    cancellation: &Cancellation,
    events: mpsc::Sender<FixtureEvent>,
) -> TestResult {
    let body_fragment = "partial local response";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nContent-Length: {STALLED_CONTENT_LENGTH}\r\nConnection: keep-alive\r\n\r\n{body_fragment}",
    );
    ensure(
        response.len() <= MAX_RESPONSE_BYTES,
        "stalled loopback response exceeded its output bound",
    )?;
    match timeout(HTTP_TIMEOUT, stream.write_all(response.as_bytes())).await {
        Ok(Ok(())) => {}
        // Chrome may close the connection as soon as it has what it needs; on Windows an
        // in-flight write then fails with a connection reset, which is not a fixture failure.
        Ok(Err(error)) if client_gone(&error) => return Ok(()),
        Ok(Err(error)) => return Err(Box::new(error)),
        Err(_) => {
            return Err(test_error("stalled loopback response write timed out"));
        }
    }
    events
        .send(FixtureEvent::StalledResponseStarted)
        .await
        .map_err(|_| test_error("stalled loopback response event receiver closed"))?;
    cancellation.cancelled().await;
    Ok(())
}

async fn send_html_response(stream: &mut TcpStream, status: &str, body: &str) -> TestResult {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    );
    ensure(
        response.len() <= MAX_RESPONSE_BYTES,
        "loopback recovery response exceeded its output bound",
    )?;
    match timeout(HTTP_TIMEOUT, stream.write_all(response.as_bytes())).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) if client_gone(&error) => return Ok(()),
        Ok(Err(error)) => return Err(Box::new(error)),
        Err(_) => {
            return Err(test_error("loopback recovery response write timed out"));
        }
    }
    match timeout(HTTP_TIMEOUT, stream.shutdown()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) if client_gone(&error) => {}
        Ok(Err(error)) => return Err(Box::new(error)),
        Err(_) => {
            return Err(test_error("loopback recovery response shutdown timed out"));
        }
    }
    Ok(())
}

fn client_gone(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe
    )
}

async fn read_request(stream: &mut TcpStream) -> TestResult<Option<String>> {
    let mut bytes = vec![0_u8; MAX_REQUEST_BYTES];
    let mut used = 0;
    loop {
        if used == bytes.len() {
            return Err(test_error(
                "loopback recovery request exceeded its header/body bound",
            ));
        }
        let read = match timeout(REQUEST_HEADER_IDLE_TIMEOUT, stream.read(&mut bytes[used..])).await
        {
            Ok(Ok(read)) => read,
            Ok(Err(error)) if client_gone(&error) => return Ok(None),
            Ok(Err(error)) => return Err(Box::new(error)),
            Err(_) if used == 0 => return Ok(None),
            Err(_) => return Err(test_error("loopback recovery request headers stalled")),
        };
        if read == 0 {
            if used == 0 {
                return Ok(None);
            }
            return Err(test_error("loopback recovery peer closed mid-request"));
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
                "termglide-http-recovery-chrome-{}-{timestamp}-{attempt}",
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
            "could not allocate a unique HTTP recovery Chrome test root",
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
                "HTTP recovery test root was not empty: {}",
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
