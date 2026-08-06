//! Local Chrome WebSocket coverage through the supervised external-engine and CDP boundaries.

use std::error::Error;
use std::fs;
use std::io;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::Value;
use tg_browser::{
    ExternalEngineError, ExternalEngineLaunchOptions, ExternalEngineViewport,
    discover_external_engine, launch_external_engine,
};
use tg_core::Cancellation;
use tg_network::{CdpLimits, CdpSession, CdpTargetSession};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};
use tokio_tungstenite::tungstenite::Error as TungsteniteError;
use tokio_tungstenite::tungstenite::protocol::frame::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::{Message, WebSocketConfig};
use tokio_tungstenite::{WebSocketStream, accept_async_with_config};
use url::Url;

const CDP_TIMEOUT: Duration = Duration::from_secs(5);
const FIXTURE_TIMEOUT: Duration = Duration::from_secs(3);
const JOURNEY_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(25);
const MAX_HTTP_REQUEST_BYTES: usize = 16 * 1024;
const MAX_HTTP_RESPONSE_BYTES: usize = 16 * 1024;
const MAX_SOCKET_MESSAGE_BYTES: usize = 128;
const HARD_SOCKET_MESSAGE_BYTES: usize = 1024;
const OVERSIZED_PAYLOAD_BYTES: usize = MAX_SOCKET_MESSAGE_BYTES * 2;
const EXPECTED_WEBSOCKET_CONNECTIONS: usize = 2;
const ROOT_ALLOCATION_ATTEMPTS: u8 = 32;
const PROFILE_PARENT: &str = "profiles";
const NORMAL_TEXT: &str = "termglide-echo";
const NORMAL_BINARY: &[u8] = &[3, 1, 4, 1];
const NORMAL_CLOSE_CODE: u16 = 1000;
const NORMAL_CLOSE_REASON: &str = "complete";
const LIMIT_CLOSE_CODE: u16 = 1009;
const LIMIT_CLOSE_REASON: &str = "message-too-large";

const DOCUMENT_TEMPLATE: &str = r##"<!doctype html>
<meta charset="utf-8">
<title>TermGlide WebSocket Journey</title>
<script>
(() => {
  const endpoint = "__WEBSOCKET_ENDPOINT__";
  const oversized = "x".repeat(__OVERSIZED_PAYLOAD_BYTES__);
  const state = window.__termglideWebSocket = {
    navigation: location.pathname,
    normalOpen: false, normalText: null, normalBinary: null, normalClose: null, normalError: null,
    limitOpen: false, limitClose: null, limitError: null
  };

  const normal = new WebSocket(endpoint);
  normal.binaryType = "arraybuffer";
  normal.onopen = () => {
    state.normalOpen = true;
    normal.send(__NORMAL_TEXT__);
  };
  normal.onmessage = event => {
    if (typeof event.data === "string") {
      state.normalText = event.data;
      normal.send(new Uint8Array(__NORMAL_BINARY__));
      return;
    }
    if (event.data instanceof ArrayBuffer) {
      state.normalBinary = Array.from(new Uint8Array(event.data));
      normal.close(1000, "complete");
      return;
    }
    state.normalError = "unexpected-message-shape";
  };
  normal.onerror = () => { state.normalError = state.normalError || "websocket-error"; };
  normal.onclose = event => {
    state.normalClose = { code: event.code, reason: event.reason, clean: event.wasClean };
  };

  const limited = new WebSocket(endpoint);
  limited.onopen = () => {
    state.limitOpen = true;
    limited.send(oversized);
  };
  limited.onerror = () => { state.limitError = state.limitError || "websocket-error"; };
  limited.onclose = event => {
    state.limitClose = { code: event.code, reason: event.reason, clean: event.wasClean };
  };
})();
</script>
"##;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;
type ServerSocket = WebSocketStream<TcpStream>;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_websocket_chrome_journey_is_observable_and_cleaned() -> TestResult {
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
    let mut websocket = WebSocketFixture::start().await?;
    let websocket_endpoint = websocket.endpoint().clone();
    let mut http = HttpFixture::start(&websocket_endpoint).await?;
    ensure(
        !websocket.is_cancelled() && !http.is_cancelled(),
        "loopback fixtures were cancelled before the Chrome journey",
    )?;

    let mut options = ExternalEngineLaunchOptions::new(Url::parse("about:blank")?);
    options.executable_override = Some(executable.clone());
    options.viewport = Some(ExternalEngineViewport::new(800, 600)?);
    options.temporary_profile_parent = Some(profile_parent.clone());

    let launch_cancellation = Cancellation::new();
    let launch = launch_external_engine(&options, &launch_cancellation);
    let (operation, websocket_completion, liveness, shutdown, profile_cleanup) = match launch {
        Ok(mut process) => {
            let profile_dir = process.isolated_profile_dir().to_path_buf();
            let endpoint = process.endpoint().websocket_url();
            let origin = http.origin().clone();
            let operation = exercise_websocket_journey(&endpoint, &origin).await;
            let websocket_completion = websocket.wait_for_completion().await;
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
            (
                operation,
                websocket_completion,
                liveness,
                shutdown,
                profile_cleanup,
            )
        }
        Err(error) => (Err(box_error(error)), Ok(()), Ok(()), Ok(()), Ok(())),
    };

    launch_cancellation.cancel();
    let launch_cancellation_cleanup = ensure(
        launch_cancellation.is_cancelled(),
        "launcher cancellation did not settle during cleanup",
    );
    let http_cleanup = http.shutdown().await;
    let websocket_cleanup = websocket.shutdown().await;
    let profile_parent_cleanup = root.remove_empty_directory(&profile_parent);
    let root_cleanup = root.verify_empty_and_remove();
    finish_with_cleanup(
        operation,
        vec![
            ("browser liveness", liveness),
            ("browser shutdown", shutdown),
            ("browser profile", profile_cleanup),
            ("launcher cancellation", launch_cancellation_cleanup),
            ("HTTP fixture", http_cleanup),
            ("WebSocket fixture journey", websocket_completion),
            ("WebSocket fixture", websocket_cleanup),
            ("profile parent", profile_parent_cleanup),
            ("test root", root_cleanup),
        ],
    )
}

async fn exercise_websocket_journey(endpoint: &str, origin: &Url) -> TestResult {
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

        let state = wait_for_websocket_state(&mut target).await?;
        assert_websocket_state(&state)?;
        Ok::<(), Box<dyn Error + Send + Sync>>(())
    }
    .await;
    let close = session.close().await.map_err(box_error);
    finish_with_cleanup(operation, vec![("CDP close", close)])
}

async fn wait_for_websocket_state(
    target: &mut CdpTargetSession<'_>,
) -> TestResult<WebSocketJourneyState> {
    let deadline = Instant::now() + JOURNEY_TIMEOUT;
    loop {
        let state = current_websocket_state(target).await?;
        if state.normal_close.is_some() && state.limit_close.is_some() {
            return Ok(state);
        }
        if Instant::now() >= deadline {
            return Err(test_error(format!(
                "WebSocket journey did not settle before {JOURNEY_TIMEOUT:?}: {state:?}"
            )));
        }
        sleep(POLL_INTERVAL).await;
    }
}

async fn current_websocket_state(
    target: &mut CdpTargetSession<'_>,
) -> TestResult<WebSocketJourneyState> {
    let evaluation = target
        .runtime_evaluate("JSON.stringify(window.__termglideWebSocket)", true)
        .await?;
    ensure(
        evaluation.exception_details.is_none(),
        "WebSocket state evaluation raised a JavaScript exception",
    )?;
    let encoded = evaluation
        .result
        .get("value")
        .and_then(Value::as_str)
        .ok_or_else(|| test_error("WebSocket state evaluation did not return a JSON string"))?;
    Ok(serde_json::from_str(encoded)?)
}

fn assert_websocket_state(state: &WebSocketJourneyState) -> TestResult {
    ensure(
        state.navigation == "/",
        format!("unexpected local navigation path: {:?}", state.navigation),
    )?;
    ensure(state.normal_open, "normal WebSocket never opened")?;
    ensure(
        state.normal_text.as_deref() == Some(NORMAL_TEXT),
        format!("text echo was not observable: {:?}", state.normal_text),
    )?;
    ensure(
        state.normal_binary.as_deref() == Some(NORMAL_BINARY),
        format!("binary echo was not observable: {:?}", state.normal_binary),
    )?;
    ensure(
        state.normal_error.is_none(),
        format!(
            "normal WebSocket reported an error: {:?}",
            state.normal_error
        ),
    )?;
    let normal_close = state
        .normal_close
        .as_ref()
        .ok_or_else(|| test_error("normal WebSocket did not publish a close result"))?;
    ensure(
        normal_close.code == NORMAL_CLOSE_CODE
            && normal_close.reason == NORMAL_CLOSE_REASON
            && normal_close.clean,
        format!("normal WebSocket close was not clean: {normal_close:?}"),
    )?;

    ensure(state.limit_open, "bounded WebSocket never opened")?;
    let limit_close = state
        .limit_close
        .as_ref()
        .ok_or_else(|| test_error("bounded WebSocket did not publish a close result"))?;
    ensure(
        limit_close.code == LIMIT_CLOSE_CODE && limit_close.reason == LIMIT_CLOSE_REASON,
        format!(
            "oversized WebSocket was not deterministically rejected: {limit_close:?}; error={:?}",
            state.limit_error
        ),
    )?;
    Ok(())
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
            "external WebSocket cleanup failed: {}",
            cleanup_errors.join("; ")
        ))),
        (Err(error), true) => Err(error),
        (Err(error), false) => Err(test_error(format!(
            "external WebSocket journey failed: {error}; cleanup also failed: {}",
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
struct WebSocketJourneyState {
    navigation: String,
    normal_open: bool,
    normal_text: Option<String>,
    normal_binary: Option<Vec<u8>>,
    normal_close: Option<BrowserClose>,
    normal_error: Option<String>,
    limit_open: bool,
    limit_close: Option<BrowserClose>,
    limit_error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BrowserClose {
    code: u16,
    reason: String,
    clean: bool,
}

struct HttpFixture {
    origin: Url,
    cancellation: Cancellation,
    task: Option<JoinHandle<TestResult>>,
}

impl HttpFixture {
    async fn start(websocket_endpoint: &Url) -> TestResult<Self> {
        let document = websocket_document(websocket_endpoint)?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let origin = Url::parse(&format!("http://{address}/"))?;
        let cancellation = Cancellation::new();
        let task_cancellation = cancellation.clone();
        let task = tokio::spawn(async move {
            let result = run_http_fixture(listener, document, &task_cancellation).await;
            if task_cancellation.is_cancelled() {
                Ok(())
            } else {
                result
            }
        });
        Ok(Self {
            origin,
            cancellation,
            task: Some(task),
        })
    }

    fn origin(&self) -> &Url {
        &self.origin
    }

    fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    async fn shutdown(&mut self) -> TestResult {
        self.cancellation.cancel();
        ensure(
            self.cancellation.is_cancelled(),
            "HTTP fixture cancellation did not settle",
        )?;
        let task = self
            .task
            .take()
            .ok_or_else(|| test_error("HTTP fixture was already shut down"))?;
        join_fixture_task(task, "HTTP fixture").await
    }
}

impl Drop for HttpFixture {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn run_http_fixture(
    listener: TcpListener,
    document: String,
    cancellation: &Cancellation,
) -> TestResult {
    loop {
        tokio::select! {
            _ = cancellation.cancelled() => return Ok(()),
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                serve_http_connection(stream, &document, cancellation).await?;
            }
        }
    }
}

async fn serve_http_connection(
    mut stream: TcpStream,
    document: &str,
    cancellation: &Cancellation,
) -> TestResult {
    let Some(request) = read_http_request(&mut stream, cancellation).await? else {
        return Ok(());
    };
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| test_error("loopback HTTP request omitted a request target"))?;
    let (status, body) = if target == "/" {
        ("200 OK", document)
    } else {
        ("404 Not Found", "not found")
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    ensure(
        response.len() <= MAX_HTTP_RESPONSE_BYTES,
        "loopback HTTP response exceeded its output bound",
    )?;
    write_http(&mut stream, response.as_bytes(), cancellation).await?;
    shutdown_http(&mut stream, cancellation).await
}

async fn read_http_request(
    stream: &mut TcpStream,
    cancellation: &Cancellation,
) -> TestResult<Option<String>> {
    let mut bytes = vec![0_u8; MAX_HTTP_REQUEST_BYTES];
    let mut used = 0;
    loop {
        if used == bytes.len() {
            return Err(test_error(
                "loopback HTTP request exceeded its header/body bound",
            ));
        }
        let read = tokio::select! {
            _ = cancellation.cancelled() => return Err(test_error("loopback HTTP read cancelled")),
            result = timeout(FIXTURE_TIMEOUT, stream.read(&mut bytes[used..])) => {
                result.map_err(|_| test_error("loopback HTTP read timed out"))??
            }
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

async fn write_http(
    stream: &mut TcpStream,
    response: &[u8],
    cancellation: &Cancellation,
) -> TestResult {
    tokio::select! {
        _ = cancellation.cancelled() => Err(test_error("loopback HTTP write cancelled")),
        result = timeout(FIXTURE_TIMEOUT, stream.write_all(response)) => {
            result.map_err(|_| test_error("loopback HTTP write timed out"))??;
            Ok(())
        }
    }
}

async fn shutdown_http(stream: &mut TcpStream, cancellation: &Cancellation) -> TestResult {
    tokio::select! {
        _ = cancellation.cancelled() => Err(test_error("loopback HTTP shutdown cancelled")),
        result = timeout(FIXTURE_TIMEOUT, stream.shutdown()) => {
            result.map_err(|_| test_error("loopback HTTP shutdown timed out"))??;
            Ok(())
        }
    }
}

fn header_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

struct WebSocketFixture {
    endpoint: Url,
    cancellation: Cancellation,
    task: Option<JoinHandle<TestResult>>,
}

impl WebSocketFixture {
    async fn start() -> TestResult<Self> {
        ensure(
            NORMAL_TEXT.len() <= MAX_SOCKET_MESSAGE_BYTES
                && NORMAL_BINARY.len() <= MAX_SOCKET_MESSAGE_BYTES
                && OVERSIZED_PAYLOAD_BYTES <= HARD_SOCKET_MESSAGE_BYTES,
            "WebSocket fixture constants exceeded their configured bounds",
        )?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let endpoint = Url::parse(&format!("ws://{address}/socket"))?;
        let cancellation = Cancellation::new();
        let task_cancellation = cancellation.clone();
        let task = tokio::spawn(async move {
            let result = run_websocket_fixture(listener, &task_cancellation).await;
            if task_cancellation.is_cancelled() {
                Ok(())
            } else {
                result
            }
        });
        Ok(Self {
            endpoint,
            cancellation,
            task: Some(task),
        })
    }

    fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    async fn shutdown(&mut self) -> TestResult {
        self.cancellation.cancel();
        ensure(
            self.cancellation.is_cancelled(),
            "WebSocket fixture cancellation did not settle",
        )?;
        match self.task.take() {
            Some(task) => join_fixture_task(task, "WebSocket fixture").await,
            None => Ok(()),
        }
    }

    async fn wait_for_completion(&mut self) -> TestResult {
        let task = self
            .task
            .take()
            .ok_or_else(|| test_error("WebSocket fixture completion was already collected"))?;
        join_fixture_task(task, "WebSocket fixture journey").await
    }
}

impl Drop for WebSocketFixture {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn run_websocket_fixture(listener: TcpListener, cancellation: &Cancellation) -> TestResult {
    let mut outcomes = Vec::with_capacity(EXPECTED_WEBSOCKET_CONNECTIONS);
    while outcomes.len() < EXPECTED_WEBSOCKET_CONNECTIONS {
        let stream = tokio::select! {
            _ = cancellation.cancelled() => return Ok(()),
            accepted = listener.accept() => accepted?.0,
        };
        outcomes.push(handle_websocket_connection(stream, cancellation).await?);
    }
    let normal = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, SocketOutcome::NormalEchoed))
        .count();
    let bounded = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, SocketOutcome::OversizedRejected))
        .count();
    ensure(
        normal == 1 && bounded == 1,
        format!("unexpected WebSocket fixture outcomes: {outcomes:?}"),
    )
}

async fn handle_websocket_connection(
    stream: TcpStream,
    cancellation: &Cancellation,
) -> TestResult<SocketOutcome> {
    let config = WebSocketConfig::default()
        .max_message_size(Some(HARD_SOCKET_MESSAGE_BYTES))
        .max_frame_size(Some(HARD_SOCKET_MESSAGE_BYTES));
    let mut socket = accept_websocket(stream, config, cancellation).await?;
    let first = next_non_control_message(&mut socket, cancellation).await?;
    let Message::Text(text) = first else {
        return Err(test_error(
            "WebSocket client did not begin with a text payload",
        ));
    };
    let text = text.to_string();
    if text.len() > MAX_SOCKET_MESSAGE_BYTES {
        send_websocket_message(
            &mut socket,
            Message::Close(Some(CloseFrame {
                code: LIMIT_CLOSE_CODE.into(),
                reason: LIMIT_CLOSE_REASON.into(),
            })),
            cancellation,
        )
        .await?;
        let close_reply = next_non_control_message(&mut socket, cancellation).await?;
        ensure(
            matches!(close_reply, Message::Close(_)),
            "oversized WebSocket client did not complete the close handshake",
        )?;
        finish_server_close(&mut socket, cancellation).await?;
        return Ok(SocketOutcome::OversizedRejected);
    }
    ensure(
        text == NORMAL_TEXT,
        format!("unexpected normal WebSocket text payload: {text:?}"),
    )?;
    send_websocket_message(&mut socket, Message::Text(text.into()), cancellation).await?;

    let binary = next_non_control_message(&mut socket, cancellation).await?;
    let Message::Binary(binary) = binary else {
        return Err(test_error(
            "WebSocket client did not send its binary echo payload",
        ));
    };
    ensure(
        binary.len() <= MAX_SOCKET_MESSAGE_BYTES && binary.as_ref() == NORMAL_BINARY,
        format!("unexpected normal WebSocket binary payload: {binary:?}"),
    )?;
    send_websocket_message(&mut socket, Message::Binary(binary), cancellation).await?;

    let close = next_non_control_message(&mut socket, cancellation).await?;
    let Message::Close(Some(frame)) = close else {
        return Err(test_error(
            "normal WebSocket client did not send a close frame",
        ));
    };
    ensure(
        u16::from(frame.code) == NORMAL_CLOSE_CODE && frame.reason == NORMAL_CLOSE_REASON,
        format!("unexpected normal WebSocket close frame: {frame:?}"),
    )?;
    finish_server_close(&mut socket, cancellation).await?;
    Ok(SocketOutcome::NormalEchoed)
}

async fn accept_websocket(
    stream: TcpStream,
    config: WebSocketConfig,
    cancellation: &Cancellation,
) -> TestResult<ServerSocket> {
    tokio::select! {
        _ = cancellation.cancelled() => Err(test_error("WebSocket handshake cancelled")),
        result = timeout(FIXTURE_TIMEOUT, accept_async_with_config(stream, Some(config))) => {
            match result {
                Ok(Ok(socket)) => Ok(socket),
                Ok(Err(error)) => Err(box_error(error)),
                Err(_) => Err(test_error("WebSocket handshake timed out")),
            }
        }
    }
}

async fn next_non_control_message(
    socket: &mut ServerSocket,
    cancellation: &Cancellation,
) -> TestResult<Message> {
    loop {
        let message = next_websocket_message(socket, cancellation).await?;
        match message {
            Message::Ping(_) => flush_websocket(socket, cancellation).await?,
            Message::Pong(_) => {}
            message => return Ok(message),
        }
    }
}

async fn next_websocket_message(
    socket: &mut ServerSocket,
    cancellation: &Cancellation,
) -> TestResult<Message> {
    tokio::select! {
        _ = cancellation.cancelled() => Err(test_error("WebSocket read cancelled")),
        result = timeout(FIXTURE_TIMEOUT, socket.next()) => {
            match result {
                Ok(Some(Ok(message))) => Ok(message),
                Ok(Some(Err(error))) => Err(box_error(error)),
                Ok(None) => Err(test_error("WebSocket peer closed without a close frame")),
                Err(_) => Err(test_error("WebSocket read timed out")),
            }
        }
    }
}

async fn send_websocket_message(
    socket: &mut ServerSocket,
    message: Message,
    cancellation: &Cancellation,
) -> TestResult {
    tokio::select! {
        _ = cancellation.cancelled() => Err(test_error("WebSocket write cancelled")),
        result = timeout(FIXTURE_TIMEOUT, socket.send(message)) => {
            result.map_err(|_| test_error("WebSocket write timed out"))??;
            Ok(())
        }
    }
}

async fn finish_server_close(socket: &mut ServerSocket, cancellation: &Cancellation) -> TestResult {
    tokio::select! {
        _ = cancellation.cancelled() => Err(test_error("WebSocket close flush cancelled")),
        result = timeout(FIXTURE_TIMEOUT, socket.flush()) => {
            match result {
                Ok(Ok(())) | Ok(Err(TungsteniteError::ConnectionClosed)) => Ok(()),
                Ok(Err(error)) => Err(box_error(error)),
                Err(_) => Err(test_error("WebSocket close flush timed out")),
            }
        }
    }
}

async fn flush_websocket(socket: &mut ServerSocket, cancellation: &Cancellation) -> TestResult {
    tokio::select! {
        _ = cancellation.cancelled() => Err(test_error("WebSocket flush cancelled")),
        result = timeout(FIXTURE_TIMEOUT, socket.flush()) => {
            match result {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => Err(box_error(error)),
                Err(_) => Err(test_error("WebSocket flush timed out")),
            }
        }
    }
}

async fn join_fixture_task(mut task: JoinHandle<TestResult>, label: &str) -> TestResult {
    match timeout(FIXTURE_TIMEOUT, &mut task).await {
        Ok(joined) => {
            joined.map_err(|error| test_error(format!("{label} task failed: {error}")))??;
            Ok(())
        }
        Err(_) => {
            task.abort();
            let _aborted_result = task.await;
            Err(test_error(format!(
                "{label} did not stop before its cleanup deadline"
            )))
        }
    }
}

fn websocket_document(websocket_endpoint: &Url) -> TestResult<String> {
    let endpoint = serde_json::to_string(websocket_endpoint.as_str())?;
    let text = serde_json::to_string(NORMAL_TEXT)?;
    let binary = serde_json::to_string(NORMAL_BINARY)?;
    let document = DOCUMENT_TEMPLATE
        .replace("\"__WEBSOCKET_ENDPOINT__\"", &endpoint)
        .replace(
            "__OVERSIZED_PAYLOAD_BYTES__",
            &OVERSIZED_PAYLOAD_BYTES.to_string(),
        )
        .replace("__NORMAL_TEXT__", &text)
        .replace("__NORMAL_BINARY__", &binary);
    ensure(
        document.len() <= MAX_HTTP_RESPONSE_BYTES,
        "loopback WebSocket document exceeded its response bound",
    )?;
    Ok(document)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SocketOutcome {
    NormalEchoed,
    OversizedRejected,
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
                "termglide-websocket-chrome-{}-{timestamp}-{attempt}",
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
            "could not allocate a unique WebSocket Chrome test root",
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
                "WebSocket test root was not empty: {}",
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
