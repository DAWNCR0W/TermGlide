//! Real-Chrome security and privacy coverage over isolated loopback-only boundaries.

use std::error::Error;
use std::fs;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tg_browser::{
    ExternalEngineError, ExternalEngineLaunchOptions, ExternalEngineProcess,
    ExternalEngineViewport, discover_external_engine, launch_external_engine,
};
use tg_core::Cancellation;
use tg_network::{CdpLimits, CdpSession, CdpTargetSession};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use url::Url;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

const SERVER_START_TIMEOUT: Duration = Duration::from_secs(2);
const SERVER_ACCEPT_TIMEOUT: Duration = Duration::from_secs(20);
const SERVER_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const SERVER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_SERVER_CONNECTIONS: usize = 64;
const MAX_REQUEST_BYTES: usize = 16 * 1024;
const MAX_RESPONSE_BYTES: usize = 16 * 1024;
const ROOT_ALLOCATION_ATTEMPTS: u8 = 32;
const STORAGE_VALUE: &str = "primary-only";
const COOKIE_NAME: &str = "termglide_security_session";
const COOKIE_VALUE: &str = "isolated";

const STORAGE_DOCUMENT: &str =
    "<!doctype html><meta charset=\"utf-8\"><main id=\"storage\">primary storage</main>";
const OBSERVE_DOCUMENT: &str =
    "<!doctype html><meta charset=\"utf-8\"><main id=\"observe\">observe only</main>";
const SANDBOX_DOCUMENT: &str = "<!doctype html><meta charset=\"utf-8\"><iframe id=\"opaque\" sandbox=\"allow-scripts\" src=\"/sandbox-child\"></iframe>";
const SANDBOX_CHILD_DOCUMENT: &str = "<!doctype html><meta charset=\"utf-8\"><script>window.sandboxChildExecuted=true;</script><main>sandbox child</main>";
const CSP_DOCUMENT: &str = "<!doctype html><meta charset=\"utf-8\"><script>window.inlineBlocked=\"executed\";</script><script nonce=\"termglide-security-nonce\">window.nonceAllowed=\"executed\";</script><main id=\"csp\">csp</main>";
const COOKIE_HEADER: &str = "termglide_security_session=isolated; Path=/; SameSite=Lax";
const CSP_HEADER: &str = "default-src 'none'; script-src 'nonce-termglide-security-nonce'; base-uri 'none'; frame-ancestors 'none'";
const NO_RESPONSE_HEADERS: &[(&str, &str)] = &[];
const COOKIE_RESPONSE_HEADERS: &[(&str, &str)] = &[("Set-Cookie", COOKIE_HEADER)];
const CSP_RESPONSE_HEADERS: &[(&str, &str)] = &[("Content-Security-Policy", CSP_HEADER)];

static ROOT_COUNTER: AtomicU64 = AtomicU64::new(0);

#[tokio::test]
async fn external_security_privacy_chrome_loopback_vertical() -> TestResult {
    let executable = match discover_external_engine(None) {
        Ok(path) => path,
        Err(ExternalEngineError::ExecutableNotFound) => return Ok(()),
        Err(error) => return Err(box_error(error)),
    };
    if !executable.is_file() {
        return Err(test_error("discovered executable is not a regular file"));
    }

    let mut root = TestRoot::new().map_err(box_error)?;
    let server_setup = LoopbackOrigins::start().await;
    let operation = match server_setup {
        Ok(origins) => {
            let flow = exercise_security_privacy_vertical(&executable, root.path(), &origins).await;
            let cleanup = origins.shutdown().await;
            preserve_results(flow, cleanup)
        }
        Err(error) => Err(error),
    };
    let root_cleanup = root.verify_empty_and_remove().map_err(box_error);
    preserve_results(operation, root_cleanup)
}

async fn exercise_security_privacy_vertical(
    executable: &Path,
    profile_parent: &Path,
    origins: &LoopbackOrigins,
) -> TestResult {
    if origins.primary.origin() == origins.secondary.origin() {
        return Err(test_error(
            "primary and secondary loopback origins were not distinct",
        ));
    }

    let first_profile = exercise_first_launch(executable, profile_parent, origins).await?;
    let second_profile =
        exercise_fresh_launch(executable, profile_parent, &origins.primary).await?;
    if first_profile == second_profile {
        return Err(test_error(
            "fresh external launch reused the first isolated profile path",
        ));
    }
    Ok(())
}

async fn exercise_first_launch(
    executable: &Path,
    profile_parent: &Path,
    origins: &LoopbackOrigins,
) -> TestResult<PathBuf> {
    let options = launch_options(executable, profile_parent)?;
    let cancellation = Cancellation::new();
    let mut process = launch_external_engine(&options, &cancellation).map_err(box_error)?;
    let profile = process.isolated_profile_dir().to_path_buf();
    let endpoint = process.endpoint().websocket_url();
    let preconditions =
        require_supervised_isolated_process(&process, executable, profile_parent, &profile);
    let flow = match preconditions {
        Ok(()) => exercise_first_cdp_session(&endpoint, origins).await,
        Err(error) => Err(error),
    };
    let liveness = require_process_running(&mut process);
    let cleanup = shutdown_process(process, &profile);
    preserve_results(preserve_results(flow, liveness), cleanup)?;
    Ok(profile)
}

async fn exercise_fresh_launch(
    executable: &Path,
    profile_parent: &Path,
    primary: &LoopbackOrigin,
) -> TestResult<PathBuf> {
    let options = launch_options(executable, profile_parent)?;
    let cancellation = Cancellation::new();
    let mut process = launch_external_engine(&options, &cancellation).map_err(box_error)?;
    let profile = process.isolated_profile_dir().to_path_buf();
    let endpoint = process.endpoint().websocket_url();
    let preconditions =
        require_supervised_isolated_process(&process, executable, profile_parent, &profile);
    let flow = match preconditions {
        Ok(()) => exercise_fresh_cdp_session(&endpoint, primary).await,
        Err(error) => Err(error),
    };
    let liveness = require_process_running(&mut process);
    let cleanup = shutdown_process(process, &profile);
    preserve_results(preserve_results(flow, liveness), cleanup)?;
    Ok(profile)
}

fn launch_options(
    executable: &Path,
    profile_parent: &Path,
) -> TestResult<ExternalEngineLaunchOptions> {
    let mut options =
        ExternalEngineLaunchOptions::new(Url::parse("about:blank").map_err(box_error)?);
    options.executable_override = Some(executable.to_path_buf());
    options.viewport = Some(ExternalEngineViewport::new(640, 480).map_err(box_error)?);
    options.temporary_profile_parent = Some(profile_parent.to_path_buf());
    Ok(options)
}

fn require_supervised_isolated_process(
    process: &ExternalEngineProcess,
    executable: &Path,
    profile_parent: &Path,
    profile: &Path,
) -> TestResult {
    if process.executable_path() != executable {
        return Err(test_error(
            "external process executable provenance did not match the selected executable",
        ));
    }
    if profile.parent() != Some(profile_parent) || !profile.is_dir() {
        return Err(test_error(format!(
            "isolated external profile was not created below the test root: {}",
            profile.display()
        )));
    }
    let endpoint = process.endpoint();
    if endpoint.host != Ipv4Addr::LOCALHOST || endpoint.port == 0 {
        return Err(test_error(
            "external DevTools endpoint was not a live IPv4 loopback endpoint",
        ));
    }
    if !endpoint.websocket_url().starts_with("ws://127.0.0.1:") {
        return Err(test_error(
            "external DevTools websocket was not loopback-only",
        ));
    }
    Ok(())
}

fn require_process_running(process: &mut ExternalEngineProcess) -> TestResult {
    match process.try_wait().map_err(box_error)? {
        None => Ok(()),
        Some(status) => Err(test_error(format!(
            "external browser exited before supervised cleanup: {status:?}"
        ))),
    }
}

fn shutdown_process(mut process: ExternalEngineProcess, profile: &Path) -> TestResult {
    let shutdown = process.shutdown().map_err(box_error);
    drop(process);
    preserve_results(shutdown, require_profile_removed(profile))
}

fn require_profile_removed(profile: &Path) -> TestResult {
    if profile.exists() {
        Err(test_error(format!(
            "external browser profile remained after cleanup: {}",
            profile.display()
        )))
    } else {
        Ok(())
    }
}

async fn exercise_first_cdp_session(endpoint: &str, origins: &LoopbackOrigins) -> TestResult {
    let mut connection = CdpSession::connect(endpoint, cdp_limits())
        .await
        .map_err(box_error)?;
    let flow: TestResult = async {
        let mut target = connection.attach_first_page().await.map_err(box_error)?;
        target.page_enable().await.map_err(box_error)?;
        target
            .set_device_metrics(640, 480, 1.0)
            .await
            .map_err(box_error)?;

        verify_same_origin_storage(&mut target, &origins.primary).await?;
        verify_cross_origin_isolation(&mut target, &origins.secondary).await?;
        verify_primary_storage_retained(&mut target, &origins.primary).await?;
        verify_sandboxed_opaque_origin_refusal(&mut target, &origins.primary).await?;
        verify_csp_inline_refusal_and_nonce_allowance(&mut target, &origins.primary).await?;
        Ok(())
    }
    .await;
    let close = connection.close().await.map_err(box_error);
    preserve_results(flow, close)
}

async fn exercise_fresh_cdp_session(endpoint: &str, primary: &LoopbackOrigin) -> TestResult {
    let mut connection = CdpSession::connect(endpoint, cdp_limits())
        .await
        .map_err(box_error)?;
    let flow: TestResult = async {
        let mut target = connection.attach_first_page().await.map_err(box_error)?;
        target.page_enable().await.map_err(box_error)?;
        let url = primary.url("/observe");
        navigate(&mut target, &url).await?;
        let state = evaluate_value(
            &mut target,
            "({cookie:document.cookie,storage:localStorage.getItem('termglide.security.scope')})",
        )
        .await?;
        let state = runtime_value(&state)?;
        require_string(state, "cookie", "", "fresh profile cookie state")?;
        require_null(state, "storage", "fresh profile localStorage state")?;
        Ok(())
    }
    .await;
    let close = connection.close().await.map_err(box_error);
    preserve_results(flow, close)
}

async fn verify_same_origin_storage(
    target: &mut CdpTargetSession<'_>,
    primary: &LoopbackOrigin,
) -> TestResult {
    let url = primary.url("/storage");
    navigate(target, &url).await?;
    let state = evaluate_value(
        target,
        "(() => { localStorage.setItem('termglide.security.scope','primary-only'); return {origin:location.origin,cookie:document.cookie,storage:localStorage.getItem('termglide.security.scope')}; })()",
    )
    .await?;
    let state = runtime_value(&state)?;
    require_string(state, "origin", primary.origin(), "primary document origin")?;
    require_cookie(state, "cookie", "same-origin cookie state")?;
    require_string(
        state,
        "storage",
        STORAGE_VALUE,
        "same-origin localStorage state",
    )
}

async fn verify_cross_origin_isolation(
    target: &mut CdpTargetSession<'_>,
    secondary: &LoopbackOrigin,
) -> TestResult {
    let url = secondary.url("/observe");
    navigate(target, &url).await?;
    let state = evaluate_value(
        target,
        "({origin:location.origin,cookie:document.cookie,storage:localStorage.getItem('termglide.security.scope')})",
    )
    .await?;
    let state = runtime_value(&state)?;
    require_string(
        state,
        "origin",
        secondary.origin(),
        "secondary document origin",
    )?;
    require_string(state, "cookie", "", "cross-origin cookie isolation")?;
    require_null(state, "storage", "cross-origin localStorage isolation")
}

async fn verify_primary_storage_retained(
    target: &mut CdpTargetSession<'_>,
    primary: &LoopbackOrigin,
) -> TestResult {
    let url = primary.url("/observe");
    navigate(target, &url).await?;
    let state = evaluate_value(
        target,
        "({origin:location.origin,cookie:document.cookie,storage:localStorage.getItem('termglide.security.scope')})",
    )
    .await?;
    let state = runtime_value(&state)?;
    require_string(
        state,
        "origin",
        primary.origin(),
        "returned primary document origin",
    )?;
    require_cookie(state, "cookie", "retained same-origin cookie state")?;
    require_string(
        state,
        "storage",
        STORAGE_VALUE,
        "retained same-origin localStorage state",
    )
}

async fn verify_sandboxed_opaque_origin_refusal(
    target: &mut CdpTargetSession<'_>,
    primary: &LoopbackOrigin,
) -> TestResult {
    let url = primary.url("/sandbox");
    navigate(target, &url).await?;
    let state = evaluate_value(
        target,
        "(() => { const frame=document.getElementById('opaque'); try { void frame.contentWindow.document.cookie; return {configuredOpaque:!frame.sandbox.contains('allow-same-origin'),refused:false,error:null}; } catch (error) { return {configuredOpaque:!frame.sandbox.contains('allow-same-origin'),refused:error && error.name === 'SecurityError',error:error && error.name}; } })()",
    )
    .await?;
    let state = runtime_value(&state)?;
    require_bool(state, "configuredOpaque", true, "sandbox configuration")?;
    require_bool(state, "refused", true, "sandbox opaque-origin access")?;
    require_string(
        state,
        "error",
        "SecurityError",
        "sandbox opaque-origin refusal",
    )
}

async fn verify_csp_inline_refusal_and_nonce_allowance(
    target: &mut CdpTargetSession<'_>,
    primary: &LoopbackOrigin,
) -> TestResult {
    let url = primary.url("/csp");
    navigate(target, &url).await?;
    let state = evaluate_value(
        target,
        "({inlineBlocked:typeof window.inlineBlocked === 'undefined',nonceAllowed:window.nonceAllowed || null})",
    )
    .await?;
    let state = runtime_value(&state)?;
    require_bool(state, "inlineBlocked", true, "CSP inline-script refusal")?;
    require_string(
        state,
        "nonceAllowed",
        "executed",
        "CSP nonce-authorized script",
    )
}

async fn navigate(target: &mut CdpTargetSession<'_>, url: &str) -> TestResult {
    let navigation = target.navigate(url).await.map_err(box_error)?;
    if let Some(message) = navigation.error_text {
        return Err(test_error(format!(
            "loopback navigation failed for {url}: {message}"
        )));
    }
    target.wait_for_load().await.map_err(box_error)?;
    Ok(())
}

async fn evaluate_value(target: &mut CdpTargetSession<'_>, expression: &str) -> TestResult<Value> {
    let evaluation = target
        .runtime_evaluate(expression, true)
        .await
        .map_err(box_error)?;
    if evaluation.exception_details.is_some() {
        return Err(test_error(
            "runtime evaluation returned browser exception details",
        ));
    }
    Ok(evaluation.result)
}

fn runtime_value(evaluation: &Value) -> TestResult<&Value> {
    evaluation
        .get("value")
        .ok_or_else(|| test_error("runtime evaluation omitted a by-value result"))
}

fn require_string(value: &Value, field: &str, expected: &str, label: &str) -> TestResult {
    let actual = value.get(field).and_then(Value::as_str);
    if actual == Some(expected) {
        Ok(())
    } else {
        Err(test_error(format!(
            "{label} was {actual:?}, expected {expected:?}"
        )))
    }
}

fn require_bool(value: &Value, field: &str, expected: bool, label: &str) -> TestResult {
    let actual = value.get(field).and_then(Value::as_bool);
    if actual == Some(expected) {
        Ok(())
    } else {
        Err(test_error(format!(
            "{label} was {actual:?}, expected {expected:?}"
        )))
    }
}

fn require_null(value: &Value, field: &str, label: &str) -> TestResult {
    if value.get(field) == Some(&Value::Null) {
        Ok(())
    } else {
        Err(test_error(format!("{label} was not null")))
    }
}

fn require_cookie(value: &Value, field: &str, label: &str) -> TestResult {
    let Some(cookie) = value.get(field).and_then(Value::as_str) else {
        return Err(test_error(format!("{label} was not a string")));
    };
    let expected = format!("{COOKIE_NAME}={COOKIE_VALUE}");
    if cookie
        .split(';')
        .map(str::trim)
        .any(|item| item == expected)
    {
        Ok(())
    } else {
        Err(test_error(format!(
            "{label} did not retain the expected scoped cookie"
        )))
    }
}

fn cdp_limits() -> CdpLimits {
    CdpLimits {
        max_message_bytes: 64 * 1024,
        max_events: 64,
        max_pending_responses: 64,
        operation_timeout: Duration::from_secs(10),
        ..CdpLimits::default()
    }
}

#[derive(Debug, Clone, Copy)]
enum OriginRole {
    Primary,
    Secondary,
}

impl OriginRole {
    const fn bind_address(self) -> Ipv4Addr {
        match self {
            Self::Primary | Self::Secondary => Ipv4Addr::LOCALHOST,
        }
    }

    const fn authority_host(self) -> &'static str {
        match self {
            Self::Primary => "127.0.0.1",
            // Cookies ignore ports, so the second origin uses a distinct loopback host name.
            Self::Secondary => "localhost",
        }
    }
}

struct LoopbackOrigins {
    primary: LoopbackOrigin,
    secondary: LoopbackOrigin,
}

impl LoopbackOrigins {
    async fn start() -> TestResult<Self> {
        let primary = LoopbackOrigin::start(OriginRole::Primary).await?;
        let secondary = match LoopbackOrigin::start(OriginRole::Secondary).await {
            Ok(secondary) => secondary,
            Err(error) => {
                let cleanup = primary.shutdown().await;
                return match cleanup {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(box_error(VerticalError::OperationAndCleanup {
                        operation: error,
                        cleanup,
                    })),
                };
            }
        };
        Ok(Self { primary, secondary })
    }

    async fn shutdown(self) -> TestResult {
        let primary = self.primary.shutdown().await;
        let secondary = self.secondary.shutdown().await;
        preserve_results(primary, secondary)
    }
}

struct LoopbackOrigin {
    origin: String,
    cancellation: Cancellation,
    task: Option<JoinHandle<Result<(), LoopbackServerError>>>,
}

impl LoopbackOrigin {
    async fn start(role: OriginRole) -> TestResult<Self> {
        let listener = timeout(
            SERVER_START_TIMEOUT,
            TcpListener::bind((role.bind_address(), 0)),
        )
        .await
        .map_err(|_| box_error(LoopbackServerError::StartTimeout))?
        .map_err(|source| box_error(loopback_io("bind", source)))?;
        let address = listener
            .local_addr()
            .map_err(|source| box_error(loopback_io("read bound address", source)))?;
        if address.ip() != IpAddr::V4(role.bind_address()) || address.port() == 0 {
            return Err(test_error(
                "loopback listener did not retain its requested ephemeral origin",
            ));
        }
        let cancellation = Cancellation::new();
        let task = tokio::spawn(serve_origin(listener, role, cancellation.clone()));
        Ok(Self {
            origin: format!("http://{}:{}", role.authority_host(), address.port()),
            cancellation,
            task: Some(task),
        })
    }

    fn origin(&self) -> &str {
        &self.origin
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.origin)
    }

    async fn shutdown(mut self) -> TestResult {
        self.cancellation.cancel();
        let mut task = self
            .task
            .take()
            .ok_or_else(|| test_error("loopback server task was already consumed"))?;
        let joined = match timeout(SERVER_SHUTDOWN_TIMEOUT, &mut task).await {
            Ok(joined) => joined,
            Err(_) => {
                task.abort();
                return Err(box_error(LoopbackServerError::ShutdownTimeout));
            }
        };
        let result = joined.map_err(|error| {
            box_error(LoopbackServerError::Task {
                message: error.to_string(),
            })
        })?;
        result.map_err(box_error)
    }
}

impl Drop for LoopbackOrigin {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn serve_origin(
    listener: TcpListener,
    role: OriginRole,
    cancellation: Cancellation,
) -> Result<(), LoopbackServerError> {
    let mut accepted = 0usize;
    loop {
        let accepted_connection = tokio::select! {
            () = cancellation.cancelled() => return Ok(()),
            result = timeout(SERVER_ACCEPT_TIMEOUT, listener.accept()) => {
                result.map_err(|_| LoopbackServerError::AcceptTimeout)?
            }
        };
        let (stream, peer) = accepted_connection.map_err(|source| loopback_io("accept", source))?;
        if !peer.ip().is_loopback() {
            return Err(LoopbackServerError::NonLoopbackPeer { peer });
        }
        accepted = accepted
            .checked_add(1)
            .ok_or(LoopbackServerError::ConnectionLimit {
                maximum: MAX_SERVER_CONNECTIONS,
            })?;
        if accepted > MAX_SERVER_CONNECTIONS {
            return Err(LoopbackServerError::ConnectionLimit {
                maximum: MAX_SERVER_CONNECTIONS,
            });
        }
        serve_connection(stream, role, &cancellation).await?;
    }
}

async fn serve_connection(
    mut stream: TcpStream,
    role: OriginRole,
    cancellation: &Cancellation,
) -> Result<(), LoopbackServerError> {
    let path = tokio::select! {
        () = cancellation.cancelled() => return Ok(()),
        result = timeout(SERVER_REQUEST_TIMEOUT, read_http_path(&mut stream)) => {
            result.map_err(|_| LoopbackServerError::RequestTimeout)??
        }
    };
    let Some(path) = path else {
        return Ok(());
    };
    let response = response_for(role, &path)?;
    tokio::select! {
        () = cancellation.cancelled() => return Ok(()),
        result = timeout(SERVER_REQUEST_TIMEOUT, write_http_response(&mut stream, &response)) => {
            result.map_err(|_| LoopbackServerError::ResponseTimeout)??;
        }
    }
    Ok(())
}

async fn read_http_path(stream: &mut TcpStream) -> Result<Option<String>, LoopbackServerError> {
    let mut buffer = [0_u8; MAX_REQUEST_BYTES];
    let mut used = 0usize;
    loop {
        if used == buffer.len() {
            return Err(LoopbackServerError::RequestTooLarge {
                maximum: MAX_REQUEST_BYTES,
            });
        }
        let read = stream
            .read(&mut buffer[used..])
            .await
            .map_err(|source| loopback_io("read request", source))?;
        if read == 0 {
            if used == 0 {
                return Ok(None);
            }
            return Err(LoopbackServerError::RequestClosed);
        }
        used = used
            .checked_add(read)
            .ok_or(LoopbackServerError::RequestTooLarge {
                maximum: MAX_REQUEST_BYTES,
            })?;
        let Some(end) = find_header_end(&buffer[..used]) else {
            continue;
        };
        let header = std::str::from_utf8(&buffer[..end])
            .map_err(|_| LoopbackServerError::InvalidUtf8Request)?;
        return parse_http_path(header).map(Some);
    }
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

fn parse_http_path(header: &str) -> Result<String, LoopbackServerError> {
    let request_line =
        header
            .split("\r\n")
            .next()
            .ok_or(LoopbackServerError::MalformedRequest {
                reason: "missing request line",
            })?;
    let mut fields = request_line.split_whitespace();
    let method = fields.next().ok_or(LoopbackServerError::MalformedRequest {
        reason: "missing HTTP method",
    })?;
    if method != "GET" {
        return Err(LoopbackServerError::UnsupportedMethod {
            method: method.to_owned(),
        });
    }
    let target = fields.next().ok_or(LoopbackServerError::MalformedRequest {
        reason: "missing request target",
    })?;
    let version = fields.next().ok_or(LoopbackServerError::MalformedRequest {
        reason: "missing HTTP version",
    })?;
    if fields.next().is_some() {
        return Err(LoopbackServerError::MalformedRequest {
            reason: "request line has extra fields",
        });
    }
    if !matches!(version, "HTTP/1.0" | "HTTP/1.1") {
        return Err(LoopbackServerError::UnsupportedHttpVersion {
            version: version.to_owned(),
        });
    }
    let path = target.split_once('?').map_or(target, |(path, _)| path);
    if !path.starts_with('/') || path.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(LoopbackServerError::MalformedRequest {
            reason: "request target was not an absolute path",
        });
    }
    Ok(path.to_owned())
}

fn response_for(role: OriginRole, path: &str) -> Result<Vec<u8>, LoopbackServerError> {
    let (status, body, headers) = match (role, path) {
        (OriginRole::Primary, "/storage") => ("200 OK", STORAGE_DOCUMENT, COOKIE_RESPONSE_HEADERS),
        (OriginRole::Primary, "/observe") => ("200 OK", OBSERVE_DOCUMENT, NO_RESPONSE_HEADERS),
        (OriginRole::Primary, "/sandbox") => ("200 OK", SANDBOX_DOCUMENT, NO_RESPONSE_HEADERS),
        (OriginRole::Primary, "/sandbox-child") => {
            ("200 OK", SANDBOX_CHILD_DOCUMENT, NO_RESPONSE_HEADERS)
        }
        (OriginRole::Primary, "/csp") => ("200 OK", CSP_DOCUMENT, CSP_RESPONSE_HEADERS),
        (OriginRole::Secondary, "/observe") => ("200 OK", OBSERVE_DOCUMENT, NO_RESPONSE_HEADERS),
        (_, "/favicon.ico") => ("204 No Content", "", NO_RESPONSE_HEADERS),
        _ => ("404 Not Found", "not found", NO_RESPONSE_HEADERS),
    };
    let mut response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        response.push_str(name);
        response.push_str(": ");
        response.push_str(value);
        response.push_str("\r\n");
    }
    response.push_str("\r\n");
    response.push_str(body);
    if response.len() > MAX_RESPONSE_BYTES {
        return Err(LoopbackServerError::ResponseTooLarge {
            actual: response.len(),
            maximum: MAX_RESPONSE_BYTES,
        });
    }
    Ok(response.into_bytes())
}

async fn write_http_response(
    stream: &mut TcpStream,
    response: &[u8],
) -> Result<(), LoopbackServerError> {
    stream
        .write_all(response)
        .await
        .map_err(|source| loopback_io("write response", source))?;
    stream
        .shutdown()
        .await
        .map_err(|source| loopback_io("close response", source))
}

fn loopback_io(operation: &'static str, source: io::Error) -> LoopbackServerError {
    LoopbackServerError::Io { operation, source }
}

#[derive(Debug, Error)]
enum LoopbackServerError {
    #[error("loopback listener setup exceeded its bounded wait")]
    StartTimeout,
    #[error("loopback listener did not receive a connection before its deadline")]
    AcceptTimeout,
    #[error("loopback request input exceeded its deadline")]
    RequestTimeout,
    #[error("loopback response output exceeded its deadline")]
    ResponseTimeout,
    #[error("loopback server did not stop before its deadline")]
    ShutdownTimeout,
    #[error("loopback server accepted more than {maximum} connections")]
    ConnectionLimit { maximum: usize },
    #[error("loopback server received a non-loopback peer: {peer}")]
    NonLoopbackPeer { peer: SocketAddr },
    #[error("loopback HTTP request exceeded {maximum} bytes")]
    RequestTooLarge { maximum: usize },
    #[error("loopback HTTP peer closed before a complete request header")]
    RequestClosed,
    #[error("loopback HTTP request header was not valid UTF-8")]
    InvalidUtf8Request,
    #[error("loopback HTTP request was malformed: {reason}")]
    MalformedRequest { reason: &'static str },
    #[error("loopback HTTP method is unsupported: {method}")]
    UnsupportedMethod { method: String },
    #[error("loopback HTTP version is unsupported: {version}")]
    UnsupportedHttpVersion { version: String },
    #[error("loopback HTTP response used {actual} bytes, exceeding {maximum}")]
    ResponseTooLarge { actual: usize, maximum: usize },
    #[error("loopback server {operation} I/O failed: {source}")]
    Io {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("loopback server task failed: {message}")]
    Task { message: String },
}

#[derive(Debug, Error)]
enum VerticalError {
    #[error("security/privacy operation failed: {operation}; cleanup also failed: {cleanup}")]
    OperationAndCleanup {
        #[source]
        operation: Box<dyn Error + Send + Sync>,
        cleanup: Box<dyn Error + Send + Sync>,
    },
}

fn preserve_results<T>(
    operation: Result<T, Box<dyn Error + Send + Sync>>,
    cleanup: TestResult,
) -> Result<T, Box<dyn Error + Send + Sync>> {
    match (operation, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(operation), Err(cleanup)) => Err(box_error(VerticalError::OperationAndCleanup {
            operation,
            cleanup,
        })),
    }
}

fn box_error<E>(error: E) -> Box<dyn Error + Send + Sync>
where
    E: Error + Send + Sync + 'static,
{
    Box::new(error)
}

fn test_error(message: impl Into<String>) -> Box<dyn Error + Send + Sync> {
    box_error(io::Error::other(message.into()))
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
        for _ in 0..ROOT_ALLOCATION_ATTEMPTS {
            let counter = ROOT_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!(
                "termglide-external-security-privacy-{}-{timestamp}-{counter}",
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
            "could not allocate a unique external security/privacy test root",
        ))
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn verify_empty_and_remove(&mut self) -> io::Result<()> {
        let entries = fs::read_dir(&self.path)?.collect::<io::Result<Vec<_>>>()?;
        if !entries.is_empty() {
            let paths = entries.iter().map(|entry| entry.path()).collect::<Vec<_>>();
            return Err(io::Error::other(format!(
                "external security/privacy test root retained paths: {paths:?}"
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
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
