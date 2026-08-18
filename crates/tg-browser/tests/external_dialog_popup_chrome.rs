//! Local Chrome coverage for modal dialogs, popup targets, cancellation, and primary recovery.
//!
//! Every page belongs to one bounded loopback origin. Dialogs are observed and resolved through
//! target-scoped CDP events; no real-site behavior or browser display feed is involved.

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
const JOURNEY_TIMEOUT: Duration = Duration::from_secs(12);
const MAX_REQUEST_BYTES: usize = 16 * 1024;
const MAX_RESPONSE_BYTES: usize = 32 * 1024;
const MAX_TARGET_CREATED_EVENTS: usize = 8;
const MAX_TARGET_DESTROYED_EVENTS: usize = 8;
const ROOT_ALLOCATION_ATTEMPTS: u8 = 32;
const PROFILE_PARENT: &str = "profiles";
const PRIMARY_PATH: &str = "primary";
const POPUP_PATH: &str = "popup";
const RECOVERY_PATH: &str = "after-beforeunload";

const PRIMARY_DOCUMENT: &str = r##"<!doctype html>
<meta charset="utf-8">
<title>Local dialog primary</title>
<link rel="icon" href="data:,">
<style>
:root { color-scheme: light; font: 18px/1.4 sans-serif; }
body { margin: 0; min-height: 100vh; background: #eff6ff; color: #1e3a8a; }
main { display: grid; gap: 14px; max-width: 680px; padding: 48px; }
#marker { padding: 16px; background: #bfdbfe; border: 2px solid #2563eb; }
</style>
<body data-alert="pending" data-confirm="pending">
  <main>
    <h1>Local dialog primary</h1>
    <p id="marker">CDP resolves local alert, confirm, and beforeunload dialogs.</p>
  </main>
  <script>
  (() => {
    window.addEventListener("beforeunload", event => {
      event.preventDefault();
      event.returnValue = "";
    });
    window.__termglideDialog = {
      startAlert() {
        setTimeout(() => {
          alert("local alert");
          document.body.dataset.alert = "handled";
        }, 0);
      },
      startConfirm() {
        setTimeout(() => {
          document.body.dataset.confirm = String(confirm("local confirm"));
        }, 0);
      },
      openPopup() {
        return window.open("/popup", "termglide-local-popup", "popup");
      },
      startBeforeUnload() {
        setTimeout(() => location.assign("/after-beforeunload"), 0);
      }
    };
  })();
  </script>
</body>
"##;

const POPUP_DOCUMENT: &str = r##"<!doctype html>
<meta charset="utf-8">
<title>Local dialog popup</title>
<link rel="icon" href="data:,">
<style>
:root { color-scheme: light; font: 18px/1.4 sans-serif; }
body { margin: 0; min-height: 100vh; background: #fdf2f8; color: #831843; }
main { display: grid; gap: 14px; max-width: 560px; padding: 40px; }
#marker { padding: 16px; background: #fbcfe8; border: 2px solid #db2777; }
</style>
<body data-popup="ready">
  <main>
    <h1>Local dialog popup</h1>
    <p id="marker">This is a newly created secondary page target.</p>
  </main>
</body>
"##;

const RECOVERY_DOCUMENT: &str = r##"<!doctype html>
<meta charset="utf-8">
<title>Local dialog recovery</title>
<link rel="icon" href="data:,">
<style>
:root { color-scheme: light; font: 18px/1.4 sans-serif; }
body { margin: 0; min-height: 100vh; background: #ecfdf5; color: #14532d; }
main { display: grid; gap: 14px; max-width: 680px; padding: 48px; }
#marker { padding: 16px; background: #bbf7d0; border: 2px solid #15803d; }
</style>
<body data-recovery="ready">
  <main>
    <h1>Local dialog recovery</h1>
    <p id="marker">The primary target remained renderable after modal handling.</p>
  </main>
</body>
"##;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Debug, Deserialize)]
struct PrimaryState {
    path: String,
    title: String,
    alert: String,
    confirm: String,
}

#[derive(Debug, Deserialize)]
struct PopupState {
    path: String,
    title: String,
    popup: String,
}

#[derive(Debug, Deserialize)]
struct RecoveryState {
    path: String,
    title: String,
    recovery: String,
}

#[derive(Debug)]
enum DialogWaitOutcome {
    Cancelled,
    Settled(Result<(), CdpError>),
}

#[tokio::test]
async fn external_dialog_popup_chrome_journey_is_observable_and_cleaned() -> TestResult {
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
            let journey = exercise_dialog_popup_journey(&endpoint, &origin).await;
            let operation = combine_results(
                endpoint_loopback,
                executable_provenance,
                journey,
                "external dialog and popup journey preconditions",
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

async fn exercise_dialog_popup_journey(endpoint: &str, origin: &Url) -> TestResult {
    let mut session = CdpSession::connect(endpoint, cdp_limits()).await?;
    let operation = match timeout(
        JOURNEY_TIMEOUT,
        run_dialog_popup_journey(&mut session, origin),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(test_error(format!(
            "dialog and popup journey exceeded {JOURNEY_TIMEOUT:?}"
        ))),
    };
    let close = session.close().await.map_err(box_error);
    finish_with_cleanup(operation, vec![("CDP close", close)])
}

async fn run_dialog_popup_journey(session: &mut CdpSession, origin: &Url) -> TestResult {
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

    let primary_url = origin.join(PRIMARY_PATH)?;
    let popup_url = origin.join(POPUP_PATH)?;
    let recovery_url = origin.join(RECOVERY_PATH)?;
    let primary_session_id = {
        let mut primary = session.attach_first_page().await?;
        primary.page_enable().await?;
        primary.set_device_metrics(800, 600, 1.0).await?;
        navigate_and_wait(&mut primary, primary_url.as_str(), "dialog primary").await?;
        assert_primary_ready(&current_primary_state(&mut primary).await?)?;
        cancel_unresolved_dialog_wait(&mut primary).await?;

        invoke_with_user_gesture(&mut primary, "window.__termglideDialog.startAlert()").await?;
        handle_next_dialog(&mut primary, "alert", true).await?;
        let after_alert = current_primary_state(&mut primary).await?;
        ensure(
            after_alert.alert == "handled" && after_alert.confirm == "pending",
            format!("alert handling did not settle primary state: {after_alert:?}"),
        )?;

        invoke_with_user_gesture(&mut primary, "window.__termglideDialog.startConfirm()").await?;
        handle_next_dialog(&mut primary, "confirm", false).await?;
        let after_confirm = current_primary_state(&mut primary).await?;
        ensure(
            after_confirm.alert == "handled" && after_confirm.confirm == "false",
            format!("confirm rejection did not settle primary state: {after_confirm:?}"),
        )?;

        invoke_with_user_gesture(&mut primary, "window.__termglideDialog.openPopup()").await?;
        primary.session_id().to_owned()
    };

    let popup_target_id = wait_for_popup_target(session, &popup_url).await?;
    let popup_session_id = attach_target(session, &popup_target_id).await?;
    {
        let mut popup = session.session(popup_session_id.clone())?;
        popup.page_enable().await?;
        popup.set_device_metrics(640, 480, 1.0).await?;
        navigate_and_wait(&mut popup, popup_url.as_str(), "dialog popup").await?;
        assert_popup_ready(&current_popup_state(&mut popup).await?)?;
        project_popup_screenshot(&mut popup).await?;
    }
    close_page_target(session, &popup_target_id).await?;
    wait_for_target_destroyed(session, &popup_target_id).await?;

    {
        let mut primary = session.session(primary_session_id.clone())?;
        invoke_with_user_gesture(&mut primary, "window.__termglideDialog.startBeforeUnload()")
            .await?;
        handle_next_dialog(&mut primary, "beforeunload", true).await?;
        primary.wait_for_load().await?;
        assert_recovery_ready(&current_recovery_state(&mut primary).await?)?;
        ensure(
            current_location(&mut primary).await? == recovery_url.as_str(),
            "primary navigation after beforeunload did not reach the local recovery URL",
        )?;
        project_primary_recovery_screenshot(&mut primary).await?;
    }
    Ok(())
}

async fn cancel_unresolved_dialog_wait(target: &mut CdpTargetSession<'_>) -> TestResult {
    let cancellation = Cancellation::new();
    let cancellation_sender = cancellation.clone();
    let cancellation_task = tokio::spawn(async move {
        tokio::task::yield_now().await;
        cancellation_sender.cancel();
    });
    let outcome = tokio::select! {
        _ = cancellation.cancelled() => DialogWaitOutcome::Cancelled,
        event = target.wait_for_event("Page.javascriptDialogOpening") => DialogWaitOutcome::Settled(event.map(|_| ())),
    };
    cancellation_task.await.map_err(|error| {
        test_error(format!(
            "dialog-wait cancellation task stopped abnormally: {error}"
        ))
    })?;
    ensure(
        cancellation.is_cancelled(),
        "dialog-wait cancellation signal did not settle",
    )?;
    match outcome {
        DialogWaitOutcome::Cancelled => Ok(()),
        DialogWaitOutcome::Settled(Ok(())) => Err(test_error(
            "a dialog opened before the cancellation boundary completed",
        )),
        DialogWaitOutcome::Settled(Err(error)) => Err(test_error(format!(
            "dialog wait settled with an error before cancellation: {error}"
        ))),
    }
}

async fn invoke_with_user_gesture(
    target: &mut CdpTargetSession<'_>,
    expression: &str,
) -> TestResult {
    let reply = target
        .send(
            "Runtime.evaluate",
            json!({
                "expression": expression,
                "userGesture": true,
                "awaitPromise": false,
            }),
        )
        .await?;
    ensure(
        reply.result.get("exceptionDetails").is_none(),
        format!(
            "dialog fixture invocation raised a JavaScript exception: {:?}",
            reply.result
        ),
    )
}

async fn handle_next_dialog(
    target: &mut CdpTargetSession<'_>,
    kind: &str,
    accept: bool,
) -> TestResult {
    let opening = target
        .wait_for_event("Page.javascriptDialogOpening")
        .await?;
    let observed_kind = opening
        .params
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| test_error("Page.javascriptDialogOpening omitted dialog type"))?;
    ensure(
        observed_kind == kind,
        format!("dialog kind differed: wanted {kind:?}, got {observed_kind:?}"),
    )?;
    target
        .send("Page.handleJavaScriptDialog", json!({ "accept": accept }))
        .await?;
    Ok(())
}

async fn wait_for_popup_target(session: &mut CdpSession, popup_url: &Url) -> TestResult<String> {
    for _ in 0..MAX_TARGET_CREATED_EVENTS {
        let event = session.wait_for_event("Target.targetCreated").await?;
        let info = event
            .params
            .get("targetInfo")
            .and_then(Value::as_object)
            .ok_or_else(|| test_error("Target.targetCreated omitted targetInfo"))?;
        let target_id = info
            .get("targetId")
            .and_then(Value::as_str)
            .ok_or_else(|| test_error("Target.targetCreated omitted targetId"))?;
        let target_type = info.get("type").and_then(Value::as_str);
        let target_url = info.get("url").and_then(Value::as_str);
        let has_opener = info.get("openerId").and_then(Value::as_str).is_some();
        if target_type == Some("page") && (target_url == Some(popup_url.as_str()) || has_opener) {
            return Ok(target_id.to_owned());
        }
    }
    Err(test_error(
        "window.open did not produce a bounded discoverable popup page target",
    ))
}

async fn attach_target(session: &mut CdpSession, target_id: &str) -> TestResult<String> {
    let attached = session
        .send(
            "Target.attachToTarget",
            json!({ "targetId": target_id, "flatten": true }),
        )
        .await?;
    let session_id = attached
        .result
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| test_error("Target.attachToTarget omitted sessionId"))?;
    ensure(
        !session_id.is_empty(),
        "Target.attachToTarget returned an empty sessionId",
    )?;
    Ok(session_id.to_owned())
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
    for _ in 0..MAX_TARGET_DESTROYED_EVENTS {
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
        "Target.targetDestroyed did not identify popup target {target_id:?} within its event bound"
    )))
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

async fn current_primary_state(target: &mut CdpTargetSession<'_>) -> TestResult<PrimaryState> {
    let evaluation = target
        .runtime_evaluate(
            "JSON.stringify({path:location.pathname,title:document.title,alert:document.body.dataset.alert || '',confirm:document.body.dataset.confirm || ''})",
            true,
        )
        .await?;
    ensure(
        evaluation.exception_details.is_none(),
        "primary dialog state evaluation raised a JavaScript exception",
    )?;
    let encoded = evaluation
        .result
        .get("value")
        .and_then(Value::as_str)
        .ok_or_else(|| test_error("primary dialog state evaluation did not return JSON"))?;
    Ok(serde_json::from_str(encoded)?)
}

async fn current_popup_state(target: &mut CdpTargetSession<'_>) -> TestResult<PopupState> {
    let evaluation = target
        .runtime_evaluate(
            "JSON.stringify({path:location.pathname,title:document.title,popup:document.body.dataset.popup || ''})",
            true,
        )
        .await?;
    ensure(
        evaluation.exception_details.is_none(),
        "popup state evaluation raised a JavaScript exception",
    )?;
    let encoded = evaluation
        .result
        .get("value")
        .and_then(Value::as_str)
        .ok_or_else(|| test_error("popup state evaluation did not return JSON"))?;
    Ok(serde_json::from_str(encoded)?)
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
        "primary recovery state evaluation raised a JavaScript exception",
    )?;
    let encoded = evaluation
        .result
        .get("value")
        .and_then(Value::as_str)
        .ok_or_else(|| test_error("primary recovery state evaluation did not return JSON"))?;
    Ok(serde_json::from_str(encoded)?)
}

async fn current_location(target: &mut CdpTargetSession<'_>) -> TestResult<String> {
    let evaluation = target.runtime_evaluate("location.href", true).await?;
    ensure(
        evaluation.exception_details.is_none(),
        "primary location evaluation raised a JavaScript exception",
    )?;
    evaluation
        .result
        .get("value")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| test_error("primary location evaluation did not return a string"))
}

fn assert_primary_ready(state: &PrimaryState) -> TestResult {
    ensure(
        state.path == "/primary"
            && state.title == "Local dialog primary"
            && state.alert == "pending"
            && state.confirm == "pending",
        format!("initial dialog primary state was not observable: {state:?}"),
    )
}

fn assert_popup_ready(state: &PopupState) -> TestResult {
    ensure(
        state.path == "/popup" && state.title == "Local dialog popup" && state.popup == "ready",
        format!("popup target state was not observable: {state:?}"),
    )
}

fn assert_recovery_ready(state: &RecoveryState) -> TestResult {
    ensure(
        state.path == "/after-beforeunload"
            && state.title == "Local dialog recovery"
            && state.recovery == "ready",
        format!("primary recovery after modal handling was not observable: {state:?}"),
    )
}

async fn project_popup_screenshot(target: &mut CdpTargetSession<'_>) -> TestResult {
    project_current_screenshot(target, 64, 24, "popup target").await
}

async fn project_primary_recovery_screenshot(target: &mut CdpTargetSession<'_>) -> TestResult {
    project_current_screenshot(target, 80, 24, "primary recovery").await
}

async fn project_current_screenshot(
    target: &mut CdpTargetSession<'_>,
    columns: u16,
    rows: u16,
    label: &str,
) -> TestResult {
    let screenshot = target.capture_screenshot().await?;
    ensure(
        !screenshot.data.is_empty(),
        format!("Chrome returned an empty {label} screenshot"),
    )?;
    let cancellation = Cancellation::new();
    let clock = SystemClock::default();
    let control = TerminalTransactionControl::new(&cancellation, &clock, None);
    let mut sequence = ExternalFrameSequence::new();
    let projected = sequence.push_png_base64_controlled(
        &screenshot.data,
        ExternalFrameOptions::new(
            Backend::Cells,
            columns,
            rows,
            ExternalFrameDecodeLimits::BROWSER_DEFAULT,
            ProjectionLimits::default(),
            ExternalFrameSequenceLimits::BROWSER_DEFAULT,
        ),
        &control,
    )?;
    ensure(
        !projected.operations().is_empty() && !projected.output().is_empty(),
        format!("terminal projection did not emit a bounded {label} cell frame"),
    )?;
    ensure(
        sequence.previous().is_some(),
        format!("terminal projection did not retain its {label} cell baseline"),
    )?;
    cancellation.cancel();
    ensure(
        matches!(
            control.checkpoint(),
            Err(TerminalTransactionError::Cancelled)
        ),
        format!("terminal projection cancellation was not observed after the {label} frame"),
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
            "external dialog and popup cleanup failed: {}",
            cleanup_errors.join("; ")
        ))),
        (Err(error), true) => Err(error),
        (Err(error), false) => Err(test_error(format!(
            "external dialog and popup journey failed: {error}; cleanup also failed: {}",
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
            ("popup", POPUP_DOCUMENT),
            ("recovery", RECOVERY_DOCUMENT),
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
            .ok_or_else(|| test_error("loopback dialog fixture was already shut down"))?;
        let joined = timeout(HTTP_TIMEOUT, task)
            .await
            .map_err(|_| test_error("loopback dialog fixture did not stop before deadline"))?;
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
        .ok_or_else(|| test_error("loopback dialog request omitted a target"))?;
    let (status, body) = if target == "/primary" {
        ("200 OK", PRIMARY_DOCUMENT)
    } else if target == "/popup" {
        ("200 OK", POPUP_DOCUMENT)
    } else if target == "/after-beforeunload" {
        ("200 OK", RECOVERY_DOCUMENT)
    } else {
        ("404 Not Found", "not found")
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    );
    ensure(
        response.len() <= MAX_RESPONSE_BYTES,
        "loopback dialog response exceeded its output bound",
    )?;
    match timeout(HTTP_TIMEOUT, stream.write_all(response.as_bytes())).await {
        Ok(Ok(())) => {}
        // Chrome may close the connection as soon as it has what it needs; on Windows an
        // in-flight write then fails with a connection reset, which is not a fixture failure.
        Ok(Err(error)) if client_gone(&error) => return Ok(()),
        Ok(Err(error)) => return Err(Box::new(error)),
        Err(_) => {
            return Err(test_error("loopback dialog response write timed out"));
        }
    }
    match timeout(HTTP_TIMEOUT, stream.shutdown()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) if client_gone(&error) => {}
        Ok(Err(error)) => return Err(Box::new(error)),
        Err(_) => {
            return Err(test_error("loopback dialog response shutdown timed out"));
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
                "loopback dialog request exceeded its header/body bound",
            ));
        }
        let read = match timeout(REQUEST_HEADER_IDLE_TIMEOUT, stream.read(&mut bytes[used..])).await
        {
            Ok(Ok(read)) => read,
            Ok(Err(error)) if client_gone(&error) => return Ok(None),
            Ok(Err(error)) => return Err(Box::new(error)),
            Err(_) if used == 0 => return Ok(None),
            Err(_) => {
                return Err(test_error(
                    "loopback dialog request headers stalled after partial input",
                ));
            }
        };
        if read == 0 {
            if used == 0 {
                return Ok(None);
            }
            return Err(test_error("loopback dialog peer closed mid-request"));
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
                "termglide-dialog-popup-chrome-{}-{timestamp}-{attempt}",
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
            "could not allocate a unique dialog popup Chrome test root",
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
                "dialog popup test root was not empty: {}",
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
