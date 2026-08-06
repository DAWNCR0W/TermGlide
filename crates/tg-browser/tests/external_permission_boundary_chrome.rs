//! Local Chrome coverage for sensitive Web API permission boundaries and isolated profiles.
//!
//! The fixture stays on one bounded loopback origin and observes permission descriptors only.
//! It never invokes location delivery, clipboard reads, or media capture, so no sensitive data is
//! collected while the test confirms that an ungranted profile remains prompt-or-denied.

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
use tg_core::Cancellation;
use tg_network::{CdpError, CdpLimits, CdpSession, CdpTargetSession};
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
const ROOT_ALLOCATION_ATTEMPTS: u8 = 32;
const PROFILE_PARENT: &str = "profiles";
const PERMISSION_PATH: &str = "permission-boundary";

const PERMISSION_DOCUMENT: &str = r##"<!doctype html>
<meta charset="utf-8">
<title>Local permission boundary</title>
<link rel="icon" href="data:,">
<style>
:root { color-scheme: light; font: 18px/1.4 sans-serif; }
body { margin: 0; min-height: 100vh; background: #f8fafc; color: #0f172a; }
main { display: grid; gap: 14px; max-width: 720px; padding: 48px; }
#marker { padding: 16px; background: #dbeafe; border: 2px solid #2563eb; }
</style>
<body data-fixture="permission-boundary">
  <main>
    <h1>Local permission boundary</h1>
    <p id="marker">Only permission descriptors are queried on this loopback page.</p>
  </main>
  <script>
  (() => {
    const queryState = async name => {
      try {
        const status = await navigator.permissions.query({ name });
        return status.state;
      } catch (error) {
        const errorName = error && error.name ? error.name : "unknown";
        return `query-error:${errorName}`;
      }
    };

    window.__termglidePermissionBoundary = {
      observationStarted: false,
      observation: null,
      async snapshot() {
        const [geolocation, notifications, clipboardRead, camera, microphone] = await Promise.all([
          queryState("geolocation"),
          queryState("notifications"),
          queryState("clipboard-read"),
          queryState("camera"),
          queryState("microphone"),
        ]);
        return {
          path: location.pathname,
          title: document.title,
          fixture: document.body.dataset.fixture || "",
          secureContext: window.isSecureContext === true,
          geolocation,
          notifications,
          clipboardRead,
          camera,
          microphone,
          notificationPermission: typeof Notification === "undefined"
            ? "unavailable"
            : Notification.permission,
          clipboardReadExposed: typeof navigator.clipboard?.readText === "function",
          mediaDevicesExposed: typeof navigator.mediaDevices?.getUserMedia === "function",
          observationStarted: this.observationStarted === true,
        };
      },
      startUnresolvedObservation() {
        this.observationStarted = true;
        this.observation = this.snapshot().then(
          () => new Promise(() => {}),
          () => new Promise(() => {}),
        );
        return true;
      },
      awaitUnresolvedObservation() {
        return this.observation;
      },
    };
  })();
  </script>
</body>
"##;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct PermissionSnapshot {
    path: String,
    title: String,
    fixture: String,
    secure_context: bool,
    geolocation: String,
    notifications: String,
    clipboard_read: String,
    camera: String,
    microphone: String,
    notification_permission: String,
    clipboard_read_exposed: bool,
    media_devices_exposed: bool,
    observation_started: bool,
}

#[derive(Debug)]
struct PermissionObservation {
    baseline: PermissionSnapshot,
    after_cancellation: PermissionSnapshot,
    after_override: Option<PermissionSnapshot>,
}

#[derive(Debug)]
struct ProfileObservation {
    label: &'static str,
    profile_dir: PathBuf,
    permissions: PermissionObservation,
}

#[derive(Debug)]
enum PermissionWaitOutcome {
    Cancelled,
    Settled(Result<(), CdpError>),
}

#[tokio::test]
async fn external_permission_boundary_chrome_journey_is_observable_and_cleaned() -> TestResult {
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
    let options = browser_launch_options(&executable, &profile_parent)?;

    let cancelled_launch = assert_pre_cancelled_launch(&options);
    let first_profile =
        run_isolated_profile(&options, fixture.origin(), "first permission profile", true).await;
    let second_profile = run_isolated_profile(
        &options,
        fixture.origin(),
        "relaunch permission profile",
        false,
    )
    .await;
    let profile_isolation = match (&first_profile, &second_profile) {
        (Ok(first), Ok(second)) => assert_profile_isolation(first, second),
        _ => Ok(()),
    };
    let operation = combine_results(
        "external permission boundary journey",
        vec![
            ("pre-cancelled launch", cancelled_launch),
            ("first isolated profile", first_profile.map(|_| ())),
            ("isolated profile relaunch", second_profile.map(|_| ())),
            ("profile permission isolation", profile_isolation),
        ],
    );

    let fixture_cleanup = fixture.shutdown().await;
    let profile_parent_cleanup = root.remove_empty_directory(&profile_parent);
    let root_cleanup = root.verify_empty_and_remove();
    finish_with_cleanup(
        operation,
        vec![
            ("loopback fixture", fixture_cleanup),
            ("profile parent", profile_parent_cleanup),
            ("test root", root_cleanup),
        ],
        "external permission boundary",
    )
}

fn browser_launch_options(
    executable: &Path,
    profile_parent: &Path,
) -> TestResult<ExternalEngineLaunchOptions> {
    let mut options = ExternalEngineLaunchOptions::new(Url::parse("about:blank")?);
    options.executable_override = Some(executable.to_path_buf());
    options.viewport = Some(ExternalEngineViewport::new(800, 600)?);
    options.temporary_profile_parent = Some(profile_parent.to_path_buf());
    Ok(options)
}

fn assert_pre_cancelled_launch(options: &ExternalEngineLaunchOptions) -> TestResult {
    let cancellation = Cancellation::new();
    cancellation.cancel();
    match launch_external_engine(options, &cancellation) {
        Err(ExternalEngineError::Cancelled) => ensure(
            cancellation.is_cancelled(),
            "pre-cancelled external launch lost its cancellation state",
        ),
        Err(error) => Err(box_error(error)),
        Ok(mut process) => {
            let child_id = process.process_id();
            let profile_dir = process.isolated_profile_dir().to_path_buf();
            let shutdown = process.shutdown().map_err(box_error);
            drop(process);
            let profile_cleanup = ensure(
                !profile_dir.exists(),
                format!(
                    "unplanned pre-cancelled browser profile was not removed: {}",
                    profile_dir.display()
                ),
            );
            finish_with_cleanup(
                Err(test_error(format!(
                    "pre-cancelled launch started an unplanned browser child {child_id}"
                ))),
                vec![
                    ("unplanned browser shutdown", shutdown),
                    ("unplanned browser profile", profile_cleanup),
                ],
                "pre-cancelled external launch",
            )
        }
    }
}

async fn run_isolated_profile(
    options: &ExternalEngineLaunchOptions,
    origin: &Url,
    label: &'static str,
    apply_notification_denial: bool,
) -> TestResult<ProfileObservation> {
    let launch_cancellation = Cancellation::new();
    let launch = launch_external_engine(options, &launch_cancellation);
    let mut observation = None;
    let (operation, liveness, shutdown, profile_cleanup) = match launch {
        Ok(mut process) => {
            let profile_dir = process.isolated_profile_dir().to_path_buf();
            let endpoint = process.endpoint().websocket_url();
            let child_id = process.process_id();
            let endpoint_loopback = ensure(
                process.endpoint().host == Ipv4Addr::LOCALHOST,
                format!(
                    "{label} Chrome debugging endpoint was not loopback-only: {}",
                    process.endpoint().host
                ),
            );
            let executable_provenance = ensure(
                options.executable_override.as_deref() == Some(process.executable_path()),
                format!(
                    "{label} Chrome selected an unplanned executable: {}",
                    process.executable_path().display()
                ),
            );
            let child_identity = ensure(
                child_id != 0,
                format!("{label} Chrome child process identifier was zero"),
            );
            let journey_result =
                match exercise_permission_journey(&endpoint, origin, apply_notification_denial)
                    .await
                {
                    Ok(permissions) => {
                        observation = Some(ProfileObservation {
                            label,
                            profile_dir: profile_dir.clone(),
                            permissions,
                        });
                        Ok(())
                    }
                    Err(error) => Err(error),
                };
            let operation = combine_results(
                "isolated permission profile preconditions",
                vec![
                    ("loopback debugging endpoint", endpoint_loopback),
                    ("executable provenance", executable_provenance),
                    ("browser child identity", child_identity),
                    ("permission observation", journey_result),
                ],
            );
            let liveness = live_process_result(process.try_wait());
            let shutdown = process.shutdown().map_err(box_error);
            drop(process);
            let profile_cleanup = ensure(
                !profile_dir.exists(),
                format!(
                    "{label} external profile was not removed: {}",
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
        format!("{label} launcher cancellation did not settle during cleanup"),
    );
    let completion = finish_with_cleanup(
        operation,
        vec![
            ("browser liveness", liveness),
            ("browser shutdown", shutdown),
            ("browser profile", profile_cleanup),
            ("launcher cancellation", launch_cancellation_cleanup),
        ],
        "isolated permission profile",
    );
    match (completion, observation) {
        (Ok(()), Some(observation)) => Ok(observation),
        (Ok(()), None) => Err(test_error(format!(
            "{label} completed without a permission observation"
        ))),
        (Err(error), _) => Err(error),
    }
}

async fn exercise_permission_journey(
    endpoint: &str,
    origin: &Url,
    apply_notification_denial: bool,
) -> TestResult<PermissionObservation> {
    let mut session = CdpSession::connect(endpoint, cdp_limits()).await?;
    let operation = match timeout(
        JOURNEY_TIMEOUT,
        run_permission_journey(&mut session, origin, apply_notification_denial),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(test_error(format!(
            "permission boundary journey exceeded {JOURNEY_TIMEOUT:?}"
        ))),
    };
    let close = session.close().await.map_err(box_error);
    finish_with_cleanup(
        operation,
        vec![("CDP close", close)],
        "permission boundary CDP session",
    )
}

async fn run_permission_journey(
    session: &mut CdpSession,
    origin: &Url,
    apply_notification_denial: bool,
) -> TestResult<PermissionObservation> {
    let version = session.browser_version().await?;
    ensure(
        !version.protocol_version.is_empty()
            && !version.product.is_empty()
            && !version.revision.is_empty()
            && !version.user_agent.is_empty()
            && !version.js_version.is_empty(),
        "Browser.getVersion returned incomplete Chrome metadata",
    )?;

    let permission_url = origin.join(PERMISSION_PATH)?;
    let loopback_origin = permission_origin(origin)?;
    reset_browser_permissions(session).await?;
    let (target_session_id, baseline, after_cancellation) = {
        let mut target = session.attach_first_page().await?;
        target.page_enable().await?;
        target.set_device_metrics(800, 600, 1.0).await?;
        navigate_and_wait(
            &mut target,
            permission_url.as_str(),
            "initial permission boundary page",
        )
        .await?;
        let baseline = current_permission_snapshot(&mut target).await?;
        assert_no_user_grant(&baseline, "initial permission boundary")?;
        cancel_inflight_permission_observation(&mut target).await?;
        let after_cancellation = current_permission_snapshot(&mut target).await?;
        assert_no_user_grant(&after_cancellation, "post-cancellation permission boundary")?;
        ensure(
            after_cancellation.observation_started,
            "cancelled permission observation never reached the loopback page",
        )?;
        (target.session_id().to_owned(), baseline, after_cancellation)
    };

    let after_override = if apply_notification_denial {
        set_notification_permission_denied(session, &loopback_origin).await?;
        let mut target = session.session(target_session_id.clone())?;
        navigate_and_wait(
            &mut target,
            permission_url.as_str(),
            "notification-denied permission override",
        )
        .await?;
        let overridden = current_permission_snapshot(&mut target).await?;
        assert_no_user_grant(&overridden, "notification-denied permission override")?;
        ensure(
            overridden.notifications == "denied" && overridden.notification_permission == "denied",
            format!(
                "Browser.setPermission did not expose a denied notification state: {overridden:?}"
            ),
        )?;
        Some(overridden)
    } else {
        None
    };

    Ok(PermissionObservation {
        baseline,
        after_cancellation,
        after_override,
    })
}

async fn reset_browser_permissions(session: &mut CdpSession) -> TestResult {
    session.send("Browser.resetPermissions", json!({})).await?;
    Ok(())
}

async fn set_notification_permission_denied(session: &mut CdpSession, origin: &str) -> TestResult {
    session
        .send(
            "Browser.setPermission",
            json!({
                "permission": { "name": "notifications" },
                "setting": "denied",
                "origin": origin,
            }),
        )
        .await?;
    Ok(())
}

fn permission_origin(origin: &Url) -> TestResult<String> {
    let serialized = origin.origin().ascii_serialization();
    ensure(
        serialized != "null",
        "loopback permission fixture did not have a tuple origin",
    )?;
    Ok(serialized)
}

async fn cancel_inflight_permission_observation(target: &mut CdpTargetSession<'_>) -> TestResult {
    let started = target
        .send(
            "Runtime.evaluate",
            json!({
                "expression": "window.__termglidePermissionBoundary.startUnresolvedObservation()",
                "awaitPromise": false,
                "returnByValue": true,
            }),
        )
        .await?;
    ensure(
        started.result.get("exceptionDetails").is_none(),
        format!(
            "permission observation start raised a JavaScript exception: {:?}",
            started.result
        ),
    )?;

    let cancellation = Cancellation::new();
    let cancellation_sender = cancellation.clone();
    let cancellation_task = tokio::spawn(async move {
        tokio::task::yield_now().await;
        cancellation_sender.cancel();
    });
    let outcome = tokio::select! {
        _ = cancellation.cancelled() => PermissionWaitOutcome::Cancelled,
        reply = target.send(
            "Runtime.evaluate",
            json!({
                "expression": "window.__termglidePermissionBoundary.awaitUnresolvedObservation()",
                "awaitPromise": true,
                "returnByValue": true,
            }),
        ) => PermissionWaitOutcome::Settled(reply.map(|_| ())),
    };
    cancellation_task.await.map_err(|error| {
        test_error(format!(
            "permission-observation cancellation task stopped abnormally: {error}"
        ))
    })?;
    ensure(
        cancellation.is_cancelled(),
        "permission-observation cancellation signal did not settle",
    )?;
    match outcome {
        PermissionWaitOutcome::Cancelled => Ok(()),
        PermissionWaitOutcome::Settled(Ok(())) => Err(test_error(
            "unresolved permission observation settled before cancellation",
        )),
        PermissionWaitOutcome::Settled(Err(error)) => Err(test_error(format!(
            "permission observation failed before cancellation: {error}"
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

async fn current_permission_snapshot(
    target: &mut CdpTargetSession<'_>,
) -> TestResult<PermissionSnapshot> {
    let evaluation = target
        .send(
            "Runtime.evaluate",
            json!({
                "expression": "window.__termglidePermissionBoundary.snapshot().then(JSON.stringify)",
                "awaitPromise": true,
                "returnByValue": true,
            }),
        )
        .await?;
    ensure(
        evaluation.result.get("exceptionDetails").is_none(),
        format!(
            "permission snapshot evaluation raised a JavaScript exception: {:?}",
            evaluation.result
        ),
    )?;
    let encoded = evaluation
        .result
        .get("result")
        .and_then(|result| result.get("value"))
        .and_then(Value::as_str)
        .ok_or_else(|| test_error("permission snapshot evaluation did not return JSON"))?;
    Ok(serde_json::from_str(encoded)?)
}

fn assert_no_user_grant(state: &PermissionSnapshot, label: &str) -> TestResult {
    ensure(
        state.path == "/permission-boundary"
            && state.title == "Local permission boundary"
            && state.fixture == "permission-boundary"
            && state.secure_context,
        format!("{label} was not the intended secure loopback state: {state:?}"),
    )?;
    ensure(
        state.clipboard_read_exposed && state.media_devices_exposed,
        format!(
            "{label} did not expose the clipboard/media permission surfaces without reading data: {state:?}"
        ),
    )?;
    for (permission, value) in [
        ("geolocation", state.geolocation.as_str()),
        ("notifications", state.notifications.as_str()),
        ("clipboard-read", state.clipboard_read.as_str()),
        ("camera", state.camera.as_str()),
        ("microphone", state.microphone.as_str()),
    ] {
        assert_prompt_or_denied(permission, value, label)?;
    }
    ensure(
        matches!(state.notification_permission.as_str(), "default" | "denied"),
        format!(
            "{label} notification API state became granted or invalid without a user decision: {state:?}"
        ),
    )
}

fn assert_prompt_or_denied(permission: &str, value: &str, label: &str) -> TestResult {
    ensure(
        matches!(value, "prompt" | "denied"),
        format!(
            "{label} {permission} permission was neither a bounded prompt nor rejection: {value:?}"
        ),
    )
}

fn assert_profile_isolation(first: &ProfileObservation, second: &ProfileObservation) -> TestResult {
    ensure(
        first.label == "first permission profile" && second.label == "relaunch permission profile",
        format!(
            "permission profile labels were not retained across relaunch: first={:?}; second={:?}",
            first.label, second.label
        ),
    )?;
    ensure(
        first.profile_dir != second.profile_dir,
        format!(
            "profile relaunch reused one isolated profile directory: {}",
            first.profile_dir.display()
        ),
    )?;
    let first_override = first.permissions.after_override.as_ref().ok_or_else(|| {
        test_error("first isolated profile did not record its notification-denied CDP override")
    })?;
    ensure(
        first_override.notifications == "denied"
            && first_override.notification_permission == "denied",
        format!(
            "first profile did not retain the intended denied-only override: {first_override:?}"
        ),
    )?;
    assert_no_user_grant(
        &first.permissions.after_cancellation,
        "first profile after cancellation",
    )?;
    assert_no_user_grant(&first.permissions.baseline, "first profile clean baseline")?;
    assert_no_user_grant(
        &second.permissions.baseline,
        "fresh relaunched profile baseline",
    )?;
    assert_no_user_grant(
        &second.permissions.after_cancellation,
        "fresh relaunched profile after cancellation",
    )?;
    ensure(
        first.permissions.baseline == second.permissions.baseline,
        format!(
            "permission state crossed an isolated profile boundary: first={:?}; override={first_override:?}; second={:?}",
            first.permissions.baseline, second.permissions.baseline
        ),
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

fn combine_results(label: &str, results: Vec<(&str, TestResult)>) -> TestResult {
    let errors = results
        .into_iter()
        .filter_map(|(name, result)| result.err().map(|error| format!("{name}: {error}")))
        .collect::<Vec<_>>();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(test_error(format!("{label} failed: {}", errors.join("; "))))
    }
}

fn finish_with_cleanup<T>(
    operation: TestResult<T>,
    cleanups: Vec<(&str, TestResult)>,
    context: &str,
) -> TestResult<T> {
    let cleanup_errors = cleanups
        .into_iter()
        .filter_map(|(label, result)| result.err().map(|error| format!("{label}: {error}")))
        .collect::<Vec<_>>();
    match (operation, cleanup_errors.is_empty()) {
        (Ok(value), true) => Ok(value),
        (Ok(_), false) => Err(test_error(format!(
            "{context} cleanup failed: {}",
            cleanup_errors.join("; ")
        ))),
        (Err(error), true) => Err(error),
        (Err(error), false) => Err(test_error(format!(
            "{context} failed: {error}; cleanup also failed: {}",
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
        ensure(
            PERMISSION_DOCUMENT.len() <= MAX_RESPONSE_BYTES,
            "loopback permission document exceeded its response bound",
        )?;
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
            .ok_or_else(|| test_error("loopback permission fixture was already shut down"))?;
        let joined = timeout(HTTP_TIMEOUT, task)
            .await
            .map_err(|_| test_error("loopback permission fixture did not stop before deadline"))?;
        joined.map_err(|error| {
            test_error(format!(
                "loopback permission fixture task stopped abnormally: {error}"
            ))
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
        .ok_or_else(|| test_error("loopback permission request omitted a target"))?;
    let (status, body) = if target == "/permission-boundary" {
        ("200 OK", PERMISSION_DOCUMENT)
    } else {
        ("404 Not Found", "not found")
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    );
    ensure(
        response.len() <= MAX_RESPONSE_BYTES,
        "loopback permission response exceeded its output bound",
    )?;
    timeout(HTTP_TIMEOUT, stream.write_all(response.as_bytes()))
        .await
        .map_err(|_| test_error("loopback permission response write timed out"))??;
    timeout(HTTP_TIMEOUT, stream.shutdown())
        .await
        .map_err(|_| test_error("loopback permission response shutdown timed out"))??;
    Ok(())
}

async fn read_request(stream: &mut TcpStream) -> TestResult<Option<String>> {
    let mut bytes = vec![0_u8; MAX_REQUEST_BYTES];
    let mut used = 0;
    loop {
        if used == bytes.len() {
            return Err(test_error(
                "loopback permission request exceeded its header/body bound",
            ));
        }
        let read = match timeout(REQUEST_HEADER_IDLE_TIMEOUT, stream.read(&mut bytes[used..])).await
        {
            Ok(result) => result?,
            Err(_) if used == 0 => return Ok(None),
            Err(_) => {
                return Err(test_error(
                    "loopback permission request headers stalled after partial input",
                ));
            }
        };
        if read == 0 {
            if used == 0 {
                return Ok(None);
            }
            return Err(test_error("loopback permission peer closed mid-request"));
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
                "termglide-permission-boundary-chrome-{}-{timestamp}-{attempt}",
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
            "could not allocate a unique permission-boundary Chrome test root",
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
                "permission-boundary test root was not empty: {}",
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
