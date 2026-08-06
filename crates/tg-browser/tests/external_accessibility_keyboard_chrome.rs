//! Local Chrome evidence for keyboard interaction and browser-exposed accessibility semantics.
//!
//! This is a test-owned semantic/keyboard fixture, not a full audit of native OS screen-reader
//! speech, platform accessibility APIs, or assistive-technology compatibility.

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
    ExternalEngineError, ExternalEngineLaunchOptions, ExternalEngineViewport, ExternalInputAdapter,
    ExternalInputError, ExternalInputGeometry, discover_external_engine, launch_external_engine,
};
use tg_core::{Cancellation, SystemClock};
use tg_network::{CdpError, CdpLimits, CdpSession, CdpTargetSession};
use tg_terminal::{
    Backend, ExternalFrameDecodeLimits, ExternalFrameOptions, ExternalFrameSequence,
    ExternalFrameSequenceLimits, InputEvent, KeyCode, KeyEventKind, Modifiers, ProjectionLimits,
    TerminalTransactionControl, TerminalTransactionError,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use url::Url;

const CDP_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_TIMEOUT: Duration = Duration::from_secs(3);
const JOURNEY_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_REQUEST_BYTES: usize = 16 * 1024;
const MAX_RESPONSE_BYTES: usize = 32 * 1024;
const ROOT_ALLOCATION_ATTEMPTS: u8 = 32;
const PROFILE_PARENT: &str = "profiles";
const TYPED_MESSAGE: &str = "termglide";
const EXPECTED_FOCUS_ORDER: [&str; 5] = ["skip", "navigation-link", "entry", "confirm", "toggle"];

const DOCUMENT: &str = r##"<!doctype html>
<meta charset="utf-8">
<title>Keyboard accessibility fixture</title>
<link rel="icon" href="data:,">
<style>
:root { color-scheme: dark; font: 18px/1.4 sans-serif; }
body { margin: 0; min-height: 100vh; background: #102a43; color: #f0f4f8; }
header, nav, main, footer { padding: 16px 24px; }
header { background: #243b53; }
nav { background: #334e68; }
main { display: grid; gap: 12px; max-width: 680px; }
label { display: grid; gap: 4px; max-width: 360px; }
input, button, a { font: inherit; }
input, button { padding: 8px; }
#status { padding: 10px; min-height: 28px; background: #486581; color: white; }
#motion-marker { width: 72px; height: 20px; background: #f6ad55; transition: transform 0.32s linear; }
body[data-workflow="toggled"] #motion-marker { transform: translateX(36px); background: #68d391; }
:focus, :focus-visible { outline: 4px solid #f6e05e; outline-offset: 3px; }
@media (prefers-reduced-motion: reduce) {
  #motion-marker { transition-duration: 0s; }
}
</style>
<body id="fixture-body" tabindex="-1" data-workflow="ready">
  <header role="banner" aria-label="Keyboard accessibility fixture">
    <a id="skip" href="#workflow">Skip to keyboard workflow</a>
  </header>
  <nav role="navigation" aria-label="Primary navigation">
    <a id="navigation-link" href="#keyboard-help">Keyboard help</a>
  </nav>
  <main id="workflow" role="main" aria-label="Keyboard workflow">
    <h1>Keyboard workflow</h1>
    <label for="entry">Message
      <input id="entry" name="message" type="text" autocomplete="off">
    </label>
    <button id="confirm" type="button">Confirm message</button>
    <button id="toggle" type="button" aria-pressed="false">Toggle setting</button>
    <div id="motion-marker" aria-hidden="true"></div>
    <output id="status" role="status" aria-label="Workflow status" aria-live="polite">ready</output>
  </main>
  <footer role="contentinfo" aria-label="Fixture status">Local fixture only</footer>
  <script>
  (() => {
    const entry = document.getElementById("entry");
    const confirm = document.getElementById("confirm");
    const toggle = document.getElementById("toggle");
    const status = document.getElementById("status");
    const motion = document.getElementById("motion-marker");
    const state = { focusOrder: [], confirmed: "", toggled: false };

    function render() {
      const workflow = state.toggled ? "toggled" : state.confirmed ? "confirmed" : "ready";
      const visible = state.toggled ? "toggle:on" : state.confirmed ? `confirmed:${state.confirmed}` : "ready";
      document.body.dataset.workflow = workflow;
      toggle.setAttribute("aria-pressed", String(state.toggled));
      status.dataset.workflow = workflow;
      status.textContent = visible;
    }

    document.addEventListener("focusin", event => {
      state.focusOrder.push(event.target.id || event.target.tagName.toLowerCase());
    });
    confirm.addEventListener("click", () => {
      state.confirmed = entry.value;
      render();
    });
    toggle.addEventListener("click", () => {
      state.toggled = !state.toggled;
      render();
    });

    window.__termglideA11y = {
      resetKeyboardStart() {
        document.body.focus();
        state.focusOrder.length = 0;
      },
      snapshot() {
        const active = document.activeElement;
        const focus = getComputedStyle(active);
        return {
          navigation: location.pathname,
          focusOrder: [...state.focusOrder],
          input: entry.value,
          confirmed: state.confirmed,
          toggled: state.toggled,
          active: active && active.id || "",
          focusVisible: focus.outlineStyle === "solid" && focus.outlineWidth === "4px",
          status: status.textContent,
          pressed: toggle.getAttribute("aria-pressed"),
          reducedMotion: matchMedia("(prefers-reduced-motion: reduce)").matches,
          motionDuration: getComputedStyle(motion).transitionDuration
        };
      }
    };
    render();
  })();
  </script>
</body>
"##;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Debug, Clone, Copy)]
enum ReducedMotionEmulation {
    Applied,
    Unavailable,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_accessibility_keyboard_chrome_journey_is_observable_and_cleaned() -> TestResult {
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
                    "external Chrome selected unexpected executable: {}",
                    process.executable_path().display()
                ),
            );
            let journey = exercise_keyboard_journey(&endpoint, &origin).await;
            let operation = combine_results(
                endpoint_loopback,
                executable_provenance,
                journey,
                "external accessibility keyboard journey preconditions",
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

async fn exercise_keyboard_journey(endpoint: &str, origin: &Url) -> TestResult {
    let mut session = CdpSession::connect(endpoint, cdp_limits()).await?;
    let operation = match timeout(JOURNEY_TIMEOUT, run_keyboard_journey(&mut session, origin)).await
    {
        Ok(result) => result,
        Err(_) => Err(test_error(format!(
            "accessibility keyboard journey exceeded {JOURNEY_TIMEOUT:?}"
        ))),
    };
    let close = session.close().await.map_err(box_error);
    finish_with_cleanup(operation, vec![("CDP close", close)])
}

async fn run_keyboard_journey(session: &mut CdpSession, origin: &Url) -> TestResult {
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
    target.set_device_metrics(800, 600, 1.0).await?;
    let navigation = target.navigate(origin.as_str()).await?;
    ensure(
        navigation.error_text.is_none(),
        format!("loopback navigation failed: {:?}", navigation.error_text),
    )?;
    target.wait_for_load().await?;

    assert_accessibility_tree(&mut target).await?;
    let reduced_motion = emulate_reduced_motion_if_available(&mut target).await?;
    reset_keyboard_start(&mut target).await?;

    let input = ExternalInputAdapter::new(ExternalInputGeometry::new(80, 24, 800.0, 600.0)?)?;
    let cancellation = Cancellation::new();
    assert_cancelled_input_is_fail_closed(&input, &mut target).await?;
    ensure(
        input
            .dispatch(&mut target, &InputEvent::Focus(true), &cancellation)
            .await?
            .sent_actions()
            == 1,
        "keyboard fixture did not enable focus emulation exactly once",
    )?;

    for expected in EXPECTED_FOCUS_ORDER[..3].iter().copied() {
        press_terminal_key(&input, &mut target, KeyCode::Tab, None, &cancellation).await?;
        assert_active_element(&mut target, expected).await?;
    }
    type_text(&input, &mut target, TYPED_MESSAGE, &cancellation).await?;
    let after_text = current_state(&mut target).await?;
    ensure(
        after_text.input == TYPED_MESSAGE,
        format!(
            "keyboard text input was not observed: {:?}",
            after_text.input
        ),
    )?;

    press_terminal_key(&input, &mut target, KeyCode::Tab, None, &cancellation).await?;
    assert_active_element(&mut target, "confirm").await?;
    press_terminal_key(&input, &mut target, KeyCode::Enter, None, &cancellation).await?;
    let after_confirm = current_state(&mut target).await?;
    ensure(
        after_confirm.confirmed == TYPED_MESSAGE
            && after_confirm.status == format!("confirmed:{TYPED_MESSAGE}"),
        format!("Enter did not activate the labelled confirm control: {after_confirm:?}"),
    )?;

    press_terminal_key(&input, &mut target, KeyCode::Tab, None, &cancellation).await?;
    assert_active_element(&mut target, "toggle").await?;
    press_terminal_key(
        &input,
        &mut target,
        KeyCode::Character(' '),
        None,
        &cancellation,
    )
    .await?;
    let final_state = current_state(&mut target).await?;
    assert_final_keyboard_state(&final_state, reduced_motion)?;
    project_current_screenshot(&mut target).await?;
    Ok(())
}

async fn assert_accessibility_tree(target: &mut CdpTargetSession<'_>) -> TestResult {
    target.send("Accessibility.enable", json!({})).await?;
    let tree = target
        .send("Accessibility.getFullAXTree", json!({}))
        .await?;
    let nodes = tree
        .result
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| test_error("Accessibility.getFullAXTree returned no nodes array"))?;
    for (role, name) in [
        ("banner", "Keyboard accessibility fixture"),
        ("navigation", "Primary navigation"),
        ("main", "Keyboard workflow"),
        ("contentinfo", "Fixture status"),
        ("textbox", "Message"),
        ("button", "Confirm message"),
        ("button", "Toggle setting"),
    ] {
        ensure(
            has_ax_role_and_name(nodes, role, name),
            format!("Chrome accessibility tree did not expose semantic {role:?} named {name:?}"),
        )?;
    }
    Ok(())
}

fn has_ax_role_and_name(nodes: &[Value], role: &str, name: &str) -> bool {
    nodes.iter().any(|node| {
        ax_string(node, "/role/value") == Some(role) && ax_string(node, "/name/value") == Some(name)
    })
}

fn ax_string<'a>(node: &'a Value, pointer: &str) -> Option<&'a str> {
    node.pointer(pointer).and_then(Value::as_str)
}

async fn emulate_reduced_motion_if_available(
    target: &mut CdpTargetSession<'_>,
) -> TestResult<ReducedMotionEmulation> {
    match target
        .send(
            "Emulation.setEmulatedMedia",
            json!({
                "features": [{
                    "name": "prefers-reduced-motion",
                    "value": "reduce",
                }],
            }),
        )
        .await
    {
        Ok(_) => Ok(ReducedMotionEmulation::Applied),
        Err(CdpError::Protocol(error)) if error.code == -32601 => {
            Ok(ReducedMotionEmulation::Unavailable)
        }
        Err(error) => Err(box_error(error)),
    }
}

async fn reset_keyboard_start(target: &mut CdpTargetSession<'_>) -> TestResult {
    let reset = target
        .runtime_evaluate("window.__termglideA11y.resetKeyboardStart()", false)
        .await?;
    ensure(
        reset.exception_details.is_none(),
        "keyboard fixture reset raised a JavaScript exception",
    )?;
    Ok(())
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
        "pre-cancelled keyboard input was not rejected before CDP dispatch",
    )?;
    let after = current_state(target).await?;
    ensure(
        after.active == before.active && after.focus_order == before.focus_order,
        format!(
            "pre-cancelled keyboard input mutated fixture state: before={before:?}, after={after:?}"
        ),
    )
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

async fn assert_active_element(target: &mut CdpTargetSession<'_>, expected: &str) -> TestResult {
    let state = current_state(target).await?;
    ensure(
        state.active == expected,
        format!(
            "Tab focus order was not deterministic: expected {expected:?}, got {:?}",
            state.active
        ),
    )
}

async fn current_state(target: &mut CdpTargetSession<'_>) -> TestResult<KeyboardState> {
    let evaluation = target
        .runtime_evaluate("JSON.stringify(window.__termglideA11y.snapshot())", true)
        .await?;
    ensure(
        evaluation.exception_details.is_none(),
        "keyboard fixture state evaluation raised a JavaScript exception",
    )?;
    let encoded = evaluation
        .result
        .get("value")
        .and_then(Value::as_str)
        .ok_or_else(|| test_error("keyboard fixture state evaluation did not return JSON"))?;
    Ok(serde_json::from_str(encoded)?)
}

fn assert_final_keyboard_state(
    state: &KeyboardState,
    reduced_motion: ReducedMotionEmulation,
) -> TestResult {
    ensure(
        state.navigation == "/",
        format!("unexpected local navigation path: {:?}", state.navigation),
    )?;
    let tab_focus_order = if state
        .focus_order
        .first()
        .is_some_and(|value| value == "fixture-body")
    {
        &state.focus_order[1..]
    } else {
        state.focus_order.as_slice()
    };
    ensure(
        tab_focus_order
            .iter()
            .map(String::as_str)
            .eq(EXPECTED_FOCUS_ORDER),
        format!(
            "keyboard focus order differed from the semantic fixture order: {:?}",
            state.focus_order
        ),
    )?;
    ensure(
        state.input == TYPED_MESSAGE && state.confirmed == TYPED_MESSAGE,
        format!("keyboard text/Enter activation state was incomplete: {state:?}"),
    )?;
    ensure(
        state.toggled && state.pressed == "true" && state.status == "toggle:on",
        format!("Space activation did not update visible toggle state: {state:?}"),
    )?;
    ensure(
        state.active == "toggle" && state.focus_visible,
        format!("keyboard focus did not remain visibly rendered on the final control: {state:?}"),
    )?;
    match reduced_motion {
        ReducedMotionEmulation::Applied => {
            ensure(
                state.reduced_motion && state.motion_duration == "0s",
                format!(
                    "public CDP reduced-motion emulation was not reflected in the fixture: {state:?}"
                ),
            )?;
        }
        ReducedMotionEmulation::Unavailable => {
            let expected_duration = if state.reduced_motion { "0s" } else { "0.32s" };
            ensure(
                state.motion_duration == expected_duration,
                format!(
                    "fixture media-query styling was not observable without CDP emulation: {state:?}"
                ),
            )?;
        }
    }
    Ok(())
}

async fn project_current_screenshot(target: &mut CdpTargetSession<'_>) -> TestResult {
    let screenshot = target.capture_screenshot().await?;
    ensure(
        !screenshot.data.is_empty(),
        "Chrome returned an empty keyboard-accessibility screenshot",
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
        "terminal projection did not emit a bounded accessibility cell frame",
    )?;
    ensure(
        sequence.previous().is_some(),
        "terminal projection did not retain its screenshot cell baseline",
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
            "external accessibility keyboard cleanup failed: {}",
            cleanup_errors.join("; ")
        ))),
        (Err(error), true) => Err(error),
        (Err(error), false) => Err(test_error(format!(
            "external accessibility keyboard journey failed: {error}; cleanup also failed: {}",
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KeyboardState {
    navigation: String,
    focus_order: Vec<String>,
    input: String,
    confirmed: String,
    toggled: bool,
    active: String,
    focus_visible: bool,
    status: String,
    pressed: String,
    reduced_motion: bool,
    motion_duration: String,
}

struct LoopbackFixture {
    origin: Url,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<TestResult>>,
}

impl LoopbackFixture {
    async fn start() -> TestResult<Self> {
        ensure(
            DOCUMENT.len() <= MAX_RESPONSE_BYTES,
            "loopback accessibility document exceeded its response bound",
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
            .ok_or_else(|| test_error("loopback accessibility fixture was already shut down"))?;
        let joined = timeout(HTTP_TIMEOUT, task).await.map_err(|_| {
            test_error("loopback accessibility fixture did not stop before deadline")
        })?;
        joined.map_err(|error| test_error(format!("loopback fixture task failed: {error}")))??;
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
        .ok_or_else(|| test_error("loopback accessibility request omitted a target"))?;
    let (status, body) = if target == "/" {
        ("200 OK", DOCUMENT)
    } else {
        ("404 Not Found", "not found")
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    ensure(
        response.len() <= MAX_RESPONSE_BYTES,
        "loopback accessibility response exceeded its output bound",
    )?;
    timeout(HTTP_TIMEOUT, stream.write_all(response.as_bytes()))
        .await
        .map_err(|_| test_error("loopback accessibility response write timed out"))??;
    timeout(HTTP_TIMEOUT, stream.shutdown())
        .await
        .map_err(|_| test_error("loopback accessibility response shutdown timed out"))??;
    Ok(())
}

async fn read_request(stream: &mut TcpStream) -> TestResult<Option<String>> {
    let mut bytes = vec![0_u8; MAX_REQUEST_BYTES];
    let mut used = 0;
    loop {
        if used == bytes.len() {
            return Err(test_error(
                "loopback accessibility request exceeded its header/body bound",
            ));
        }
        let read = timeout(HTTP_TIMEOUT, stream.read(&mut bytes[used..]))
            .await
            .map_err(|_| test_error("loopback accessibility request read timed out"))??;
        if read == 0 {
            if used == 0 {
                return Ok(None);
            }
            return Err(test_error(
                "loopback accessibility peer closed before sending headers",
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
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        for attempt in 0..ROOT_ALLOCATION_ATTEMPTS {
            let path = parent.join(format!(
                "termglide-accessibility-keyboard-chrome-{}-{timestamp}-{attempt}",
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
            "could not allocate a unique accessibility keyboard Chrome test root",
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
                "accessibility keyboard test root was not empty: {}",
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
