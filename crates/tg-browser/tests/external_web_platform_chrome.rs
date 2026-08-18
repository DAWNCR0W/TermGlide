//! Broad local-Chrome coverage through the supervised external-engine and CDP boundaries.

use std::error::Error;
use std::fs;
use std::io;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::Value;
use tg_browser::{
    ExternalEngineError, ExternalEngineLaunchOptions, ExternalEngineViewport,
    discover_external_engine, launch_external_engine,
};
use tg_core::{Cancellation, SystemClock};
use tg_network::{CdpLimits, CdpSession, CdpTargetSession};
use tg_terminal::{
    Backend, ExternalFrameDecodeLimits, ExternalFrameOptions, ExternalFrameSequence,
    ExternalFrameSequenceLimits, ProjectionLimits, TerminalTransactionControl,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};
use url::Url;

const CDP_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_TIMEOUT: Duration = Duration::from_secs(3);
const JOURNEY_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(25);
const MAX_REQUEST_BYTES: usize = 16 * 1024;
const MAX_RESPONSE_BYTES: usize = 32 * 1024;
const ROOT_ALLOCATION_ATTEMPTS: u8 = 32;
const PROFILE_PARENT: &str = "profiles";

const DOCUMENT: &str = r##"<!doctype html>
<meta charset="utf-8">
<title>TermGlide Web Platform Journey</title>
<style>
body { margin: 0; background: #123456; color: white; font: 16px sans-serif; }
#canvas { position: absolute; left: 20px; top: 20px; }
#graphic { position: absolute; left: 180px; top: 20px; }
#animated { position: absolute; left: 20px; top: 180px; width: 120px; height: 24px; background: #ef476f; }
#entry { position: absolute; left: 20px; top: 230px; width: 200px; height: 28px; }
#commit { position: absolute; left: 20px; top: 275px; width: 110px; height: 34px; }
</style>
<canvas id="canvas" width="120" height="120"></canvas>
<canvas id="glcanvas" width="8" height="8" hidden></canvas>
<svg id="graphic" width="120" height="80" viewBox="0 0 120 80"><rect id="rect" width="120" height="80" fill="#06d6a0" /></svg>
<div id="animated"></div>
<input id="entry" aria-label="entry">
<button id="commit" type="button">commit</button>
<script>
(() => {
  const state = window.__termglide = {
    script: "ready", navigation: location.pathname, storage: "", worker: null, workerError: null,
    canvas: null, svg: null, animation: "pending", animationError: null, wasm: null, wasmError: null,
    focused: false, input: "", form: "", pointer: 0,
    webgl: null, webgpu: null, media: null
  };
  localStorage.setItem("termglide-local", "local-ok");
  sessionStorage.setItem("termglide-session", "session-ok");
  state.storage = localStorage.getItem("termglide-local") + "|" + sessionStorage.getItem("termglide-session");

  const canvas = document.getElementById("canvas");
  const context = canvas.getContext("2d");
  context.fillStyle = "#123456";
  context.fillRect(0, 0, canvas.width, canvas.height);
  state.canvas = Array.from(context.getImageData(0, 0, 1, 1).data);
  const rect = document.getElementById("rect");
  state.svg = { fill: rect.getAttribute("fill"), width: document.getElementById("graphic").getBBox().width };

  const feature = (supported, detail, reason) => supported
    ? { state: "supported", detail }
    : { state: "unsupported", reason };
  const gl = document.getElementById("glcanvas").getContext("webgl2") || document.getElementById("glcanvas").getContext("webgl");
  state.webgl = feature(Boolean(gl), gl ? gl.getParameter(gl.VERSION) : "", "context-unavailable");
  state.webgpu = feature(Boolean(navigator.gpu), navigator.gpu ? "navigator.gpu-present" : "", "navigator.gpu-unavailable");
  const media = document.createElement("audio");
  const mediaType = media.canPlayType('audio/ogg; codecs="vorbis"');
  state.media = feature(Boolean(mediaType), mediaType, "canPlayType-empty");

  const workerSource = "self.onmessage = event => postMessage(event.data + 1);";
  const worker = new Worker(URL.createObjectURL(new Blob([workerSource], { type: "text/javascript" })));
  worker.onmessage = event => { state.worker = event.data; worker.terminate(); };
  worker.onerror = event => { state.workerError = event.message || "worker-error"; worker.terminate(); };
  worker.postMessage(41);

  const animation = document.getElementById("animated").animate(
    [{ opacity: 0 }, { opacity: 1 }], { duration: 80, fill: "forwards" }
  );
  animation.finished.then(() => { state.animation = "finished"; }).catch(error => { state.animationError = String(error); });

  const wasm = new Uint8Array([
    0,97,115,109,1,0,0,0,1,7,1,96,2,127,127,1,127,3,2,1,0,
    7,7,1,3,97,100,100,0,0,10,9,1,7,0,32,0,32,1,106,11
  ]);
  WebAssembly.instantiate(wasm).then(result => { state.wasm = result.instance.exports.add(20, 22); })
    .catch(error => { state.wasmError = String(error); });

  const entry = document.getElementById("entry");
  entry.addEventListener("focus", () => { state.focused = true; });
  entry.addEventListener("input", () => { state.input = entry.value; });
  const commit = document.getElementById("commit");
  commit.addEventListener("pointerdown", () => { state.pointer += 1; });
  commit.addEventListener("click", () => { state.form = entry.value; });
})();
</script>
"##;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_web_platform_chrome_journey_is_observable_and_cleaned() -> TestResult {
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
    let mut options = ExternalEngineLaunchOptions::new(Url::parse("about:blank")?);
    options.executable_override = Some(executable.clone());
    options.viewport = Some(ExternalEngineViewport::new(800, 600)?);
    options.temporary_profile_parent = Some(profile_parent.clone());

    let mut fixture = LoopbackFixture::start().await?;
    let launch = launch_external_engine(&options, &Cancellation::new());
    let (operation, liveness, shutdown, profile_cleanup) = match launch {
        Ok(mut process) => {
            let profile_dir = process.isolated_profile_dir().to_path_buf();
            let endpoint = process.endpoint().websocket_url();
            let origin = fixture.origin().clone();
            let operation = exercise_journey(&endpoint, &origin).await;
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

    let server_cleanup = fixture.shutdown().await;
    let profile_parent_cleanup = root.remove_empty_directory(&profile_parent);
    let root_cleanup = root.verify_empty_and_remove();
    finish_with_cleanup(
        operation,
        vec![
            ("browser liveness", liveness),
            ("browser shutdown", shutdown),
            ("browser profile", profile_cleanup),
            ("loopback fixture", server_cleanup),
            ("profile parent", profile_parent_cleanup),
            ("test root", root_cleanup),
        ],
    )
}

async fn exercise_journey(endpoint: &str, origin: &Url) -> TestResult {
    let mut session = CdpSession::connect(endpoint, cdp_limits()).await?;
    let operation = async {
        let version = session.browser_version().await?;
        ensure(
            !version.protocol_version.is_empty() && !version.product.is_empty(),
            "Browser.getVersion returned incomplete metadata",
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

        let initial = wait_for_async_platform_results(&mut target).await?;
        assert_initial_results(&initial)?;

        let focus = target
            .runtime_evaluate("document.getElementById('entry').focus()", false)
            .await?;
        ensure(
            focus.exception_details.is_none(),
            "focus evaluation raised a JavaScript exception",
        )?;
        target.insert_text("termglide").await?;
        target.mouse_click(75.0, 292.0).await?;

        let form_state = current_state(&mut target).await?;
        ensure(form_state.focused, "input focus event was not observed")?;
        ensure(
            form_state.input == "termglide" && form_state.form == "termglide",
            format!(
                "form input/click state was not observed: input={:?}, form={:?}",
                form_state.input, form_state.form
            ),
        )?;
        ensure(
            form_state.pointer >= 1,
            "button pointer event was not observed",
        )?;

        let screenshot = target.capture_screenshot().await?;
        ensure(
            !screenshot.data.is_empty(),
            "Chrome returned an empty screenshot",
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
            "terminal projection did not emit a bounded initial cell frame",
        )?;
        ensure(
            sequence.previous().is_some(),
            "terminal projection did not retain its cell baseline",
        )?;
        Ok::<(), Box<dyn Error + Send + Sync>>(())
    }
    .await;
    let close = session.close().await.map_err(box_error);
    finish_with_cleanup(operation, vec![("CDP close", close)])
}

async fn wait_for_async_platform_results(
    target: &mut CdpTargetSession<'_>,
) -> TestResult<JourneyState> {
    let deadline = Instant::now() + JOURNEY_TIMEOUT;
    loop {
        let state = current_state(target).await?;
        if state.worker == Some(42) && state.wasm == Some(42) && state.animation == "finished" {
            return Ok(state);
        }
        if Instant::now() >= deadline {
            return Err(test_error(format!(
                "Web Platform async results did not settle before {JOURNEY_TIMEOUT:?}: {state:?}"
            )));
        }
        sleep(POLL_INTERVAL).await;
    }
}

async fn current_state(target: &mut CdpTargetSession<'_>) -> TestResult<JourneyState> {
    let evaluation = target
        .runtime_evaluate("JSON.stringify(window.__termglide)", true)
        .await?;
    ensure(
        evaluation.exception_details.is_none(),
        "platform state evaluation raised a JavaScript exception",
    )?;
    let encoded = evaluation
        .result
        .get("value")
        .and_then(Value::as_str)
        .ok_or_else(|| test_error("platform state evaluation did not return a JSON string"))?;
    Ok(serde_json::from_str(encoded)?)
}

fn assert_initial_results(state: &JourneyState) -> TestResult {
    ensure(state.script == "ready", "page script did not execute")?;
    ensure(
        state.navigation == "/",
        format!("unexpected local navigation path: {:?}", state.navigation),
    )?;
    ensure(
        state.storage == "local-ok|session-ok",
        format!(
            "local/session storage was not observable: {:?}",
            state.storage
        ),
    )?;
    ensure(
        state.worker == Some(42),
        format!("Blob Worker result: {state:?}"),
    )?;
    ensure(
        state.worker_error.is_none(),
        format!("Blob Worker reported an error: {:?}", state.worker_error),
    )?;
    ensure(
        state.wasm == Some(42),
        format!("WebAssembly result: {state:?}"),
    )?;
    ensure(
        state.wasm_error.is_none(),
        format!("WebAssembly reported an error: {:?}", state.wasm_error),
    )?;
    ensure(
        state.animation == "finished",
        format!("Web Animation result: {state:?}"),
    )?;
    ensure(
        state.animation_error.is_none(),
        format!(
            "Web Animation reported an error: {:?}",
            state.animation_error
        ),
    )?;
    ensure(
        state.canvas == [18, 52, 86, 255],
        format!("Canvas 2D pixels were not observable: {:?}", state.canvas),
    )?;
    ensure(
        state.svg.fill == "#06d6a0" && state.svg.width > 0.0,
        format!("SVG state was not observable: {:?}", state.svg),
    )?;
    assert_feature("WebGL", &state.webgl)?;
    assert_feature("WebGPU", &state.webgpu)?;
    assert_feature("media", &state.media)?;
    Ok(())
}

fn assert_feature(name: &str, result: &FeatureStatus) -> TestResult {
    match result.state.as_str() {
        "supported" => ensure(
            result
                .detail
                .as_deref()
                .is_some_and(|detail| !detail.is_empty()),
            format!("{name} claimed support without a typed detail: {result:?}"),
        ),
        "unsupported" => ensure(
            result
                .reason
                .as_deref()
                .is_some_and(|reason| !reason.is_empty()),
            format!("{name} claimed unsupported without a typed reason: {result:?}"),
        ),
        state => Err(test_error(format!(
            "{name} returned an unknown feature state {state:?}: {result:?}"
        ))),
    }
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

fn finish_with_cleanup(operation: TestResult, cleanups: Vec<(&str, TestResult)>) -> TestResult {
    let cleanup_errors = cleanups
        .into_iter()
        .filter_map(|(label, result)| result.err().map(|error| format!("{label}: {error}")))
        .collect::<Vec<_>>();
    match (operation, cleanup_errors.is_empty()) {
        (Ok(()), true) => Ok(()),
        (Ok(()), false) => Err(test_error(format!(
            "external web-platform cleanup failed: {}",
            cleanup_errors.join("; ")
        ))),
        (Err(error), true) => Err(error),
        (Err(error), false) => Err(test_error(format!(
            "external web-platform journey failed: {error}; cleanup also failed: {}",
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
struct JourneyState {
    script: String,
    navigation: String,
    storage: String,
    worker: Option<i64>,
    worker_error: Option<String>,
    canvas: Vec<u8>,
    svg: SvgState,
    animation: String,
    animation_error: Option<String>,
    wasm: Option<i64>,
    wasm_error: Option<String>,
    focused: bool,
    input: String,
    form: String,
    pointer: u64,
    webgl: FeatureStatus,
    webgpu: FeatureStatus,
    media: FeatureStatus,
}

#[derive(Debug, Deserialize)]
struct SvgState {
    fill: String,
    width: f64,
}

#[derive(Debug, Deserialize)]
struct FeatureStatus {
    state: String,
    detail: Option<String>,
    reason: Option<String>,
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
            "loopback document exceeded its response bound",
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
            .ok_or_else(|| test_error("loopback fixture was already shut down"))?;
        let joined = timeout(HTTP_TIMEOUT, task)
            .await
            .map_err(|_| test_error("loopback fixture did not stop before its deadline"))?;
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
        .ok_or_else(|| test_error("loopback request omitted a request target"))?;
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
        "loopback response exceeded its output bound",
    )?;
    match timeout(HTTP_TIMEOUT, stream.write_all(response.as_bytes())).await {
        Ok(Ok(())) => {}
        // Chrome may close the connection as soon as it has what it needs; on Windows an
        // in-flight write then fails with a connection reset, which is not a fixture failure.
        Ok(Err(error)) if client_gone(&error) => return Ok(()),
        Ok(Err(error)) => return Err(Box::new(error)),
        Err(_) => {
            return Err(test_error("loopback response write timed out"));
        }
    }
    match timeout(HTTP_TIMEOUT, stream.shutdown()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) if client_gone(&error) => {}
        Ok(Err(error)) => return Err(Box::new(error)),
        Err(_) => {
            return Err(test_error("loopback response shutdown timed out"));
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
                "loopback request exceeded its body/header bound",
            ));
        }
        let read = match timeout(HTTP_TIMEOUT, stream.read(&mut bytes[used..]))
            .await
            .map_err(|_| test_error("loopback request read timed out"))?
        {
            Ok(read) => read,
            Err(error) if client_gone(&error) => return Ok(None),
            Err(error) => return Err(Box::new(error)),
        };
        if read == 0 {
            return Ok(None);
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
                "termglide-web-platform-chrome-{}-{timestamp}-{attempt}",
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
            "could not allocate a unique web-platform Chrome test root",
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
                "web-platform test root was not empty: {}",
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
