//! Local Chrome coverage for a generic form, pointer, scroll, cookie, and history journey.
//!
//! The fixture stays on one loopback origin and the assertions inspect only target-scoped CDP
//! state and bounded terminal projections. It neither embeds a real site's behavior nor depends
//! on a streamed browser display.

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
    ExternalEngineError, ExternalEngineLaunchOptions, ExternalEngineViewport, ExternalInputAdapter,
    ExternalInputError, ExternalInputGeometry, discover_external_engine, launch_external_engine,
};
use tg_core::{Cancellation, SystemClock};
use tg_network::{CdpLimits, CdpSession, CdpTargetSession};
use tg_terminal::{
    Backend, ExternalFrameDecodeLimits, ExternalFrameOptions, ExternalFrameSequence,
    ExternalFrameSequenceLimits, InputEvent, KeyCode, KeyEventKind, Modifiers, MouseButton,
    ProjectionLimits, TerminalTransactionControl, TerminalTransactionError,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::{Instant as TokioInstant, sleep, timeout};
use url::Url;

const CDP_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_TIMEOUT: Duration = Duration::from_secs(3);
const REQUEST_HEADER_IDLE_TIMEOUT: Duration = Duration::from_millis(250);
const STATE_POLL_INTERVAL: Duration = Duration::from_millis(25);
const JOURNEY_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_REQUEST_BYTES: usize = 16 * 1024;
const MAX_RESPONSE_BYTES: usize = 32 * 1024;
const ROOT_ALLOCATION_ATTEMPTS: u8 = 32;
const PROFILE_PARENT: &str = "profiles";
const FORM_VALUE: &str = "termglide";
const INITIAL_VIEWPORT: (u32, u32) = (800, 600);
const RESIZED_VIEWPORT: (u32, u32) = (640, 480);
const TERMINAL_COLUMNS: u16 = 80;
const TERMINAL_ROWS: u16 = 30;

const FORM_DOCUMENT: &str = r##"<!doctype html>
<meta charset="utf-8">
<title>Local form journey</title>
<link rel="icon" href="data:,">
<style>
:root { color-scheme: light; font: 18px/1.4 sans-serif; }
body { margin: 0; min-height: 100vh; background: #f5f7fa; color: #1f2933; }
main { display: grid; gap: 18px; max-width: 640px; padding: 48px; }
form { display: grid; gap: 12px; max-width: 360px; }
label { display: grid; gap: 6px; }
input, button { font: inherit; padding: 10px; }
:focus, :focus-visible { outline: 3px solid #2563eb; outline-offset: 3px; }
</style>
<body id="fixture-body" tabindex="-1">
  <main>
    <h1>Local form journey</h1>
    <p>Submit a local value to continue.</p>
    <form id="journey-form" action="/submitted" method="get">
      <label for="entry">Message
        <input id="entry" name="message" type="text" autocomplete="off">
      </label>
      <button id="submit" type="submit">Submit local value</button>
    </form>
  </main>
</body>
"##;

const SUBMITTED_DOCUMENT: &str = r##"<!doctype html>
<meta charset="utf-8">
<title>Local form submitted</title>
<link rel="icon" href="data:,">
<style>
:root { color-scheme: light; font: 18px/1.4 sans-serif; }
body { margin: 0; min-height: 100vh; background: #ecfdf5; color: #14532d; }
#details-link {
  position: absolute;
  left: 20px;
  top: 20px;
  box-sizing: border-box;
  width: 200px;
  min-height: 40px;
  padding: 8px;
  background: #166534;
  color: white;
  text-decoration: none;
}
main { padding: 96px 48px; }
:focus, :focus-visible { outline: 3px solid #f59e0b; outline-offset: 3px; }
</style>
<body>
  <a id="details-link" href="/details">Open local details</a>
  <main>
    <h1>Submission received</h1>
    <p>The next route is intentionally a pointer target.</p>
  </main>
</body>
"##;

const DETAILS_DOCUMENT: &str = r##"<!doctype html>
<meta charset="utf-8">
<title>Local scroll details</title>
<link rel="icon" href="data:,">
<style>
:root { color-scheme: light; font: 18px/1.4 sans-serif; }
body { margin: 0; min-height: 2800px; background: linear-gradient(#eff6ff, #dbeafe); color: #172554; }
header { position: sticky; top: 0; padding: 20px 48px; background: #1d4ed8; color: white; }
main { padding: 48px; }
#scroll-sentinel { position: absolute; top: 1900px; left: 48px; padding: 24px; background: #fef3c7; }
</style>
<body>
  <header>Local details</header>
  <main>
    <h1>Scroll boundary</h1>
    <p>Pointer wheel input should move this ordinary document.</p>
    <section id="scroll-sentinel">Scroll sentinel</section>
  </main>
</body>
"##;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DocumentState {
    path: String,
    query: String,
    cookie: String,
    scroll_y: f64,
    width: f64,
    height: f64,
    active: String,
}

#[tokio::test]
async fn external_form_scroll_cookie_chrome_journey_is_observable_and_cleaned() -> TestResult {
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
    options.viewport = Some(ExternalEngineViewport::new(
        INITIAL_VIEWPORT.0,
        INITIAL_VIEWPORT.1,
    )?);
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
            let journey = exercise_form_scroll_cookie_journey(&endpoint, &origin).await;
            let operation = combine_results(
                endpoint_loopback,
                executable_provenance,
                journey,
                "external form, scroll, cookie, and history journey preconditions",
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

async fn exercise_form_scroll_cookie_journey(endpoint: &str, origin: &Url) -> TestResult {
    let mut session = CdpSession::connect(endpoint, cdp_limits()).await?;
    let operation = match timeout(
        JOURNEY_TIMEOUT,
        run_form_scroll_cookie_journey(&mut session, origin),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(test_error(format!(
            "form, scroll, cookie, and history journey exceeded {JOURNEY_TIMEOUT:?}"
        ))),
    };
    let close = session.close().await.map_err(box_error);
    finish_with_cleanup(operation, vec![("CDP close", close)])
}

async fn run_form_scroll_cookie_journey(session: &mut CdpSession, origin: &Url) -> TestResult {
    let version = session.browser_version().await?;
    ensure(
        !version.protocol_version.is_empty()
            && !version.product.is_empty()
            && !version.revision.is_empty()
            && !version.user_agent.is_empty()
            && !version.js_version.is_empty(),
        "Browser.getVersion returned incomplete Chrome metadata",
    )?;

    let mut target = session.attach_first_page().await?;
    target.page_enable().await?;
    target
        .set_device_metrics(INITIAL_VIEWPORT.0, INITIAL_VIEWPORT.1, 1.0)
        .await?;
    let navigation = target.navigate(origin.as_str()).await?;
    ensure(
        navigation.error_text.is_none(),
        format!(
            "loopback form navigation failed: {:?}",
            navigation.error_text
        ),
    )?;
    target.wait_for_load().await?;

    let initial = current_state(&mut target).await?;
    ensure(
        initial.path == "/" && initial.cookie.contains("journey_seed=ready"),
        format!("initial loopback cookie state was not observable: {initial:?}"),
    )?;

    let input = ExternalInputAdapter::new(ExternalInputGeometry::new(
        TERMINAL_COLUMNS,
        TERMINAL_ROWS,
        f64::from(INITIAL_VIEWPORT.0),
        f64::from(INITIAL_VIEWPORT.1),
    )?)?;
    reset_keyboard_start(&mut target).await?;
    assert_cancelled_input_is_fail_closed(&input, &mut target).await?;
    let cancellation = Cancellation::new();
    ensure(
        input
            .dispatch(&mut target, &InputEvent::Focus(true), &cancellation)
            .await?
            .sent_actions()
            == 1,
        "form journey did not enable focus emulation exactly once",
    )?;

    submit_keyboard_form(&input, &mut target, &cancellation)
        .await
        .map_err(|error| test_error(format!("keyboard form submission failed: {error}")))?;
    let submitted = current_state(&mut target).await?;
    assert_submitted_state(&submitted)?;

    follow_details_pointer(&input, &mut target, &cancellation)
        .await
        .map_err(|error| test_error(format!("first pointer navigation failed: {error}")))?;
    target
        .navigate_history_delta(-1)
        .await
        .map_err(|error| test_error(format!("history-back command failed: {error}")))?;
    let history = wait_for_path(&mut target, "/submitted").await?;
    assert_submitted_state(&history)?;

    follow_details_pointer(&input, &mut target, &cancellation)
        .await
        .map_err(|error| test_error(format!("second pointer navigation failed: {error}")))?;
    scroll_details(&input, &mut target, &cancellation)
        .await
        .map_err(|error| test_error(format!("pointer scroll failed: {error}")))?;
    let scrolled = current_state(&mut target).await?;
    ensure(
        scrolled.path == "/details" && scrolled.scroll_y >= 300.0,
        format!("pointer wheel input did not scroll the local detail route: {scrolled:?}"),
    )?;
    project_current_screenshot(&mut target, TERMINAL_COLUMNS, TERMINAL_ROWS)
        .await
        .map_err(|error| test_error(format!("initial screenshot projection failed: {error}")))?;

    target
        .set_device_metrics(RESIZED_VIEWPORT.0, RESIZED_VIEWPORT.1, 1.0)
        .await?;
    let resized = current_state(&mut target).await?;
    ensure(
        resized.width == f64::from(RESIZED_VIEWPORT.0)
            && resized.height == f64::from(RESIZED_VIEWPORT.1),
        format!("resized viewport was not observable through the target: {resized:?}"),
    )?;
    project_current_screenshot(&mut target, 64, 24)
        .await
        .map_err(|error| test_error(format!("resized screenshot projection failed: {error}")))?;
    Ok(())
}

async fn reset_keyboard_start(target: &mut CdpTargetSession<'_>) -> TestResult {
    let reset = target
        .runtime_evaluate("document.getElementById('fixture-body').focus()", false)
        .await?;
    ensure(
        reset.exception_details.is_none(),
        "form fixture keyboard reset raised a JavaScript exception",
    )
}

async fn assert_cancelled_input_is_fail_closed(
    input: &ExternalInputAdapter,
    target: &mut CdpTargetSession<'_>,
) -> TestResult {
    let before = current_state(target).await?;
    let cancellation = Cancellation::new();
    cancellation.cancel();
    let cancelled = input
        .dispatch(target, &InputEvent::Focus(true), &cancellation)
        .await;
    ensure(
        matches!(cancelled, Err(ExternalInputError::Cancelled)),
        "pre-cancelled form input was not rejected before CDP dispatch",
    )?;
    let after = current_state(target).await?;
    ensure(
        after.path == before.path
            && after.query == before.query
            && after.cookie == before.cookie
            && after.active == before.active,
        format!("pre-cancelled form input mutated local state: before={before:?}, after={after:?}"),
    )
}

async fn submit_keyboard_form(
    input: &ExternalInputAdapter,
    target: &mut CdpTargetSession<'_>,
    cancellation: &Cancellation,
) -> TestResult {
    press_terminal_key(input, target, KeyCode::Tab, None, cancellation).await?;
    assert_active_element(target, "entry").await?;
    type_text(input, target, FORM_VALUE, cancellation).await?;
    let entered = current_state(target).await?;
    ensure(
        entered.active == "entry",
        format!("keyboard edit lost input focus: {entered:?}"),
    )?;
    press_terminal_key(input, target, KeyCode::Tab, None, cancellation).await?;
    assert_active_element(target, "submit").await?;
    press_terminal_key(input, target, KeyCode::Enter, None, cancellation).await?;
    target.wait_for_load().await?;
    Ok(())
}

async fn follow_details_pointer(
    input: &ExternalInputAdapter,
    target: &mut CdpTargetSession<'_>,
    cancellation: &Cancellation,
) -> TestResult {
    for pressed in [true, false] {
        let dispatched = input
            .dispatch(
                target,
                &InputEvent::Mouse {
                    button: MouseButton::Left,
                    column: 8,
                    row: 2,
                    pressed,
                    modifiers: Modifiers::empty(),
                },
                cancellation,
            )
            .await?;
        ensure(
            dispatched.sent_actions() == 1,
            "details pointer click did not send exactly one bounded CDP action",
        )?;
    }
    target.wait_for_load().await?;
    let details = current_state(target).await?;
    ensure(
        details.path == "/details"
            && details.cookie.contains("journey_seed=ready")
            && details.cookie.contains("journey_submit=done"),
        format!("pointer navigation did not retain same-origin cookie state: {details:?}"),
    )
}

async fn scroll_details(
    input: &ExternalInputAdapter,
    target: &mut CdpTargetSession<'_>,
    cancellation: &Cancellation,
) -> TestResult {
    for _ in 0..8 {
        let dispatched = input
            .dispatch(
                target,
                &InputEvent::Mouse {
                    button: MouseButton::WheelDown,
                    column: 10,
                    row: 10,
                    pressed: true,
                    modifiers: Modifiers::empty(),
                },
                cancellation,
            )
            .await?;
        ensure(
            dispatched.sent_actions() == 1,
            "pointer wheel input did not send exactly one bounded CDP action",
        )?;
    }
    Ok(())
}

async fn press_terminal_key(
    input: &ExternalInputAdapter,
    target: &mut CdpTargetSession<'_>,
    code: KeyCode,
    text: Option<String>,
    cancellation: &Cancellation,
) -> TestResult {
    let pressed = input
        .dispatch(
            target,
            &InputEvent::Key {
                code: code.clone(),
                modifiers: Modifiers::empty(),
                kind: KeyEventKind::Press,
                shifted_key: None,
                base_layout_key: None,
                text,
            },
            cancellation,
        )
        .await?;
    ensure(
        pressed.sent_actions() > 0,
        "keyboard press sent no bounded CDP actions",
    )?;
    let released = input
        .dispatch(
            target,
            &InputEvent::Key {
                code,
                modifiers: Modifiers::empty(),
                kind: KeyEventKind::Release,
                shifted_key: None,
                base_layout_key: None,
                text: None,
            },
            cancellation,
        )
        .await?;
    ensure(
        released.sent_actions() == 1,
        "keyboard release did not send exactly one bounded CDP action",
    )
}

async fn type_text(
    input: &ExternalInputAdapter,
    target: &mut CdpTargetSession<'_>,
    text: &str,
    cancellation: &Cancellation,
) -> TestResult {
    for character in text.chars() {
        press_terminal_key(
            input,
            target,
            KeyCode::Character(character),
            Some(character.to_string()),
            cancellation,
        )
        .await?;
    }
    Ok(())
}

async fn assert_active_element(target: &mut CdpTargetSession<'_>, target_id: &str) -> TestResult {
    let state = current_state(target).await?;
    ensure(
        state.active == target_id,
        format!(
            "keyboard focus order was not deterministic: wanted {target_id:?}, got {:?}",
            state.active
        ),
    )
}

async fn current_state(target: &mut CdpTargetSession<'_>) -> TestResult<DocumentState> {
    let evaluation = target
        .runtime_evaluate(
            "JSON.stringify({path:location.pathname,query:new URLSearchParams(location.search).get('message') || '',cookie:document.cookie,scrollY:window.scrollY,width:window.innerWidth,height:window.innerHeight,active:(document.activeElement && document.activeElement.id) || ''})",
            true,
        )
        .await?;
    if let Some(details) = evaluation.exception_details {
        return Err(test_error(format!(
            "form fixture state evaluation raised a JavaScript exception: {details}"
        )));
    }
    let encoded = evaluation
        .result
        .get("value")
        .and_then(Value::as_str)
        .ok_or_else(|| test_error("form fixture state evaluation did not return JSON"))?;
    Ok(serde_json::from_str(encoded)?)
}

async fn wait_for_path(
    target: &mut CdpTargetSession<'_>,
    expected_path: &str,
) -> TestResult<DocumentState> {
    let deadline = TokioInstant::now() + CDP_TIMEOUT;
    loop {
        let state = current_state(target).await?;
        if state.path == expected_path {
            return Ok(state);
        }
        let now = TokioInstant::now();
        if now >= deadline {
            return Err(test_error(format!(
                "history navigation did not reach {expected_path:?}; last state: {state:?}"
            )));
        }
        sleep(STATE_POLL_INTERVAL.min(deadline.saturating_duration_since(now))).await;
    }
}

fn assert_submitted_state(state: &DocumentState) -> TestResult {
    ensure(
        state.path == "/submitted"
            && state.query == FORM_VALUE
            && state.cookie.contains("journey_seed=ready")
            && state.cookie.contains("journey_submit=done"),
        format!("keyboard form submission was not observable: {state:?}"),
    )
}

async fn project_current_screenshot(
    target: &mut CdpTargetSession<'_>,
    columns: u16,
    rows: u16,
) -> TestResult {
    let screenshot = target.capture_screenshot().await?;
    ensure(
        !screenshot.data.is_empty(),
        "Chrome returned an empty form journey screenshot",
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
        "terminal projection did not emit a bounded form journey cell frame",
    )?;
    ensure(
        sequence.previous().is_some(),
        "terminal projection did not retain its form journey cell baseline",
    )?;
    cancellation.cancel();
    ensure(
        matches!(
            control.checkpoint(),
            Err(TerminalTransactionError::Cancelled)
        ),
        "terminal projection cancellation was not observed after the completed frame",
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
            "external form journey cleanup failed: {}",
            cleanup_errors.join("; ")
        ))),
        (Err(error), true) => Err(error),
        (Err(error), false) => Err(test_error(format!(
            "external form journey failed: {error}; cleanup also failed: {}",
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
            ("form", FORM_DOCUMENT),
            ("submitted", SUBMITTED_DOCUMENT),
            ("details", DETAILS_DOCUMENT),
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
            .ok_or_else(|| test_error("loopback form fixture was already shut down"))?;
        let joined = timeout(HTTP_TIMEOUT, task)
            .await
            .map_err(|_| test_error("loopback form fixture did not stop before deadline"))?;
        joined.map_err(|error| test_error(format!("loopback fixture task failed: {error}")))??;
        Ok(())
    }
}

struct HttpResponse {
    status: &'static str,
    body: &'static str,
    cookie: Option<&'static str>,
}

async fn serve_connection(mut stream: TcpStream) -> TestResult {
    let Some(request) = read_request(&mut stream).await? else {
        return Ok(());
    };
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| test_error("loopback form request omitted a target"))?;
    let response = response_for_target(target);
    let cookie_header = match response.cookie {
        Some(cookie) => format!("Set-Cookie: {cookie}\r\n"),
        None => String::new(),
    };
    let encoded = format!(
        "HTTP/1.1 {}\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        response.status,
        cookie_header,
        response.body.len(),
        response.body,
    );
    ensure(
        encoded.len() <= MAX_RESPONSE_BYTES,
        "loopback form response exceeded its output bound",
    )?;
    match timeout(HTTP_TIMEOUT, stream.write_all(encoded.as_bytes())).await {
        Ok(Ok(())) => {}
        // Chrome may close the connection as soon as it has what it needs; on Windows an
        // in-flight write then fails with a connection reset, which is not a fixture failure.
        Ok(Err(error)) if client_gone(&error) => return Ok(()),
        Ok(Err(error)) => return Err(Box::new(error)),
        Err(_) => {
            return Err(test_error("loopback form response write timed out"));
        }
    }
    match timeout(HTTP_TIMEOUT, stream.shutdown()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) if client_gone(&error) => {}
        Ok(Err(error)) => return Err(Box::new(error)),
        Err(_) => {
            return Err(test_error("loopback form response shutdown timed out"));
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

fn response_for_target(target: &str) -> HttpResponse {
    if target == "/" {
        HttpResponse {
            status: "200 OK",
            body: FORM_DOCUMENT,
            cookie: Some("journey_seed=ready; Path=/; SameSite=Lax"),
        }
    } else if target.starts_with("/submitted") {
        HttpResponse {
            status: "200 OK",
            body: SUBMITTED_DOCUMENT,
            cookie: Some("journey_submit=done; Path=/; SameSite=Lax"),
        }
    } else if target == "/details" {
        HttpResponse {
            status: "200 OK",
            body: DETAILS_DOCUMENT,
            cookie: None,
        }
    } else if target == "/favicon.ico" {
        HttpResponse {
            status: "204 No Content",
            body: "",
            cookie: None,
        }
    } else {
        HttpResponse {
            status: "404 Not Found",
            body: "not found",
            cookie: None,
        }
    }
}

async fn read_request(stream: &mut TcpStream) -> TestResult<Option<String>> {
    let mut bytes = vec![0_u8; MAX_REQUEST_BYTES];
    let mut used = 0;
    loop {
        if used == bytes.len() {
            return Err(test_error(
                "loopback form request exceeded its header/body bound",
            ));
        }
        let read = match timeout(REQUEST_HEADER_IDLE_TIMEOUT, stream.read(&mut bytes[used..])).await
        {
            Ok(Ok(read)) => read,
            Ok(Err(error)) if client_gone(&error) => return Ok(None),
            Ok(Err(error)) => return Err(Box::new(error)),
            Err(_) if used == 0 => return Ok(None),
            Err(_) => return Err(test_error("loopback form request headers stalled")),
        };
        if read == 0 {
            if used == 0 {
                return Ok(None);
            }
            return Err(test_error("loopback form peer closed mid-request"));
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
                "termglide-form-scroll-cookie-chrome-{}-{timestamp}-{attempt}",
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
            "could not allocate a unique form journey Chrome test root",
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
                "form journey test root was not empty: {}",
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
