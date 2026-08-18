#![cfg(unix)]

//! Real-Chrome coverage for two independently supervised external browser instances.

use std::error::Error;
use std::fs;
use std::io::{self, ErrorKind};
use std::net::{IpAddr, Ipv4Addr};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tg_browser::{
    DevToolsEndpoint, ExternalEngineError, ExternalEngineLaunchOptions, ExternalEngineProcess,
    ExternalEngineViewport, discover_external_engine, launch_external_engine,
};
use tg_core::Cancellation;
use tg_network::{CdpLimits, CdpSession, CdpTargetSession};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use url::Url;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(25);
const CDP_OPERATION_TIMEOUT: Duration = Duration::from_secs(5);
const EXIT_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const EXIT_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(10);
const SERVER_START_TIMEOUT: Duration = Duration::from_secs(2);
const SERVER_ACCEPT_TIMEOUT: Duration = Duration::from_secs(20);
const SERVER_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const SERVER_HEADER_IDLE_TIMEOUT: Duration = Duration::from_millis(250);
const SERVER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_SERVER_CONNECTIONS: usize = 128;
const MAX_REQUEST_BYTES: usize = 16 * 1024;
const MAX_RESPONSE_BYTES: usize = 16 * 1024;
const ROOT_ALLOCATION_ATTEMPTS: u8 = 32;
const KILL_SIGNAL: i32 = 9;
const COOKIE_NAME: &str = "termglide_instance";
const STORAGE_KEY: &str = "termglide.instance";
const FIRST_MARKER: &str = "alpha";
const SECOND_MARKER: &str = "beta";
const FIRST_PATH: &str = "/instance-alpha";
const SECOND_PATH: &str = "/instance-beta";
const LOOPBACK_DOCUMENT: &str =
    "<!doctype html><meta charset=\"utf-8\"><main id=\"instance\">isolated instance</main>";

static ROOT_COUNTER: AtomicU64 = AtomicU64::new(0);

#[tokio::test]
async fn external_multi_instance_isolation_chrome() -> TestResult {
    let executable = match discover_external_engine(None) {
        Ok(path) => path,
        Err(ExternalEngineError::ExecutableNotFound) => return Ok(()),
        Err(error) => return Err(box_error(error)),
    };
    if !executable.is_file() {
        return Err(test_error(
            "discovered browser executable is not a regular file",
        ));
    }

    let mut root = TestRoot::new().map_err(box_error)?;
    let server_setup = LoopbackOrigin::start().await;
    let operation = match server_setup {
        Ok(origin) => {
            let flow = exercise_multi_instance_vertical(&executable, &root, &origin).await;
            let server_cleanup = origin.shutdown().await;
            preserve_results(flow, server_cleanup)
        }
        Err(error) => Err(error),
    };
    let root_cleanup = root.verify_empty_and_remove().map_err(box_error);
    preserve_results(operation, root_cleanup)
}

async fn exercise_multi_instance_vertical(
    executable: &Path,
    root: &TestRoot,
    origin: &LoopbackOrigin,
) -> TestResult {
    let first_parent = root.path().join("alpha-profiles");
    let second_parent = root.path().join("beta-profiles");
    if first_parent == second_parent {
        return Err(test_error(
            "multi-instance profile parents did not remain distinct",
        ));
    }

    let first_options = launch_options(executable, &first_parent)?;
    let second_options = launch_options(executable, &second_parent)?;
    let first_cancellation = Cancellation::new();
    let second_cancellation = Cancellation::new();
    let mut first = match launch_external_engine(&first_options, &first_cancellation) {
        Ok(process) => process,
        Err(error) => {
            let parent_cleanup = remove_empty_profile_parents(&first_parent, &second_parent);
            return preserve_results(Err(box_error(error)), parent_cleanup);
        }
    };
    let first_profile = first.isolated_profile_dir().to_path_buf();
    let first_endpoint = first.endpoint().websocket_url();

    let mut second = match launch_external_engine(&second_options, &second_cancellation) {
        Ok(process) => process,
        Err(error) => {
            let first_cleanup = shutdown_and_verify(&mut first, &first_profile);
            drop(first);
            let parent_cleanup = remove_empty_profile_parents(&first_parent, &second_parent);
            return preserve_results(
                Err(box_error(error)),
                preserve_results(first_cleanup, parent_cleanup),
            );
        }
    };
    let second_profile = second.isolated_profile_dir().to_path_buf();
    let second_endpoint = second.endpoint().websocket_url();

    let launch_contract = preserve_results(
        preserve_results(
            require_supervised_instance(&first, executable, &first_parent, &first_profile),
            require_supervised_instance(&second, executable, &second_parent, &second_profile),
        ),
        require_distinct_instances(
            &first,
            &first_profile,
            &first_endpoint,
            &second,
            &second_profile,
            &second_endpoint,
        ),
    );
    let state_contract = match launch_contract {
        Ok(()) => {
            configure_isolated_instance_state(&first_endpoint, &second_endpoint, origin).await
        }
        Err(error) => Err(error),
    };

    first_cancellation.cancel();
    let cancellation_contract =
        require_cancellation_isolation(&first_cancellation, &second_cancellation);
    let first_exit = force_kill_and_reap(&mut first, &first_profile);
    let stale_endpoint = if first_exit.is_ok() {
        require_stale_endpoint_rejected(&first_endpoint).await
    } else {
        Ok(())
    };
    let peer_contract = if first_exit.is_ok() && cancellation_contract.is_ok() {
        verify_surviving_second_instance(&mut second, &second_profile, &second_endpoint, origin)
            .await
    } else {
        Ok(())
    };

    let first_cleanup = shutdown_and_verify(&mut first, &first_profile);
    let second_cleanup = shutdown_and_verify(&mut second, &second_profile);
    drop(first);
    drop(second);
    let parent_cleanup = remove_empty_profile_parents(&first_parent, &second_parent);

    let operation = preserve_results(
        state_contract,
        preserve_results(
            cancellation_contract,
            preserve_results(first_exit, preserve_results(stale_endpoint, peer_contract)),
        ),
    );
    preserve_results(
        operation,
        preserve_results(
            first_cleanup,
            preserve_results(second_cleanup, parent_cleanup),
        ),
    )
}

fn launch_options(
    executable: &Path,
    profile_parent: &Path,
) -> TestResult<ExternalEngineLaunchOptions> {
    let initial_target = Url::parse("about:blank").map_err(box_error)?;
    let mut options = ExternalEngineLaunchOptions::new(initial_target);
    options.executable_override = Some(executable.to_path_buf());
    options.viewport = Some(ExternalEngineViewport::new(640, 480).map_err(box_error)?);
    options.startup_timeout = STARTUP_TIMEOUT;
    options.poll_interval = STARTUP_POLL_INTERVAL;
    options.temporary_profile_parent = Some(profile_parent.to_path_buf());
    Ok(options)
}

fn require_supervised_instance(
    process: &ExternalEngineProcess,
    executable: &Path,
    profile_parent: &Path,
    profile: &Path,
) -> TestResult {
    if process.executable_path() != executable {
        return Err(test_error(
            "external process executable did not match the discovered browser",
        ));
    }
    if process.process_id() == 0 {
        return Err(test_error(
            "external process did not expose an OS process identifier",
        ));
    }
    if profile.parent() != Some(profile_parent) || profile == profile_parent || !profile.is_dir() {
        return Err(test_error(format!(
            "external profile was not a dedicated child of its selected parent: {}",
            profile.display()
        )));
    }
    require_loopback_endpoint(process.endpoint())
}

fn require_loopback_endpoint(endpoint: &DevToolsEndpoint) -> TestResult {
    if endpoint.host != Ipv4Addr::LOCALHOST || endpoint.port == 0 {
        return Err(test_error(
            "DevTools endpoint was not a non-zero IPv4 loopback endpoint",
        ));
    }
    let endpoint_url = endpoint.websocket_url();
    let parsed = Url::parse(&endpoint_url).map_err(box_error)?;
    if parsed.scheme() != "ws"
        || parsed.host_str() != Some("127.0.0.1")
        || parsed.port() != Some(endpoint.port)
        || parsed.path() != endpoint.websocket_path.as_str()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(test_error(
            "DevTools endpoint did not remain a credential-free loopback websocket",
        ));
    }
    Ok(())
}

fn require_distinct_instances(
    first: &ExternalEngineProcess,
    first_profile: &Path,
    first_endpoint: &str,
    second: &ExternalEngineProcess,
    second_profile: &Path,
    second_endpoint: &str,
) -> TestResult {
    if first.process_id() == second.process_id() {
        return Err(test_error(
            "supervised instances shared one operating-system process identifier",
        ));
    }
    if first_profile == second_profile {
        return Err(test_error(
            "supervised instances reused an isolated profile path",
        ));
    }
    if first_endpoint == second_endpoint {
        return Err(test_error(
            "supervised instances reused one DevTools websocket endpoint",
        ));
    }
    Ok(())
}

async fn configure_isolated_instance_state(
    first_endpoint: &str,
    second_endpoint: &str,
    origin: &LoopbackOrigin,
) -> TestResult {
    let first_write = write_instance_state(first_endpoint, origin, FIRST_MARKER, FIRST_PATH).await;
    let second_write =
        write_instance_state(second_endpoint, origin, SECOND_MARKER, SECOND_PATH).await;
    let observations = if first_write.is_ok() && second_write.is_ok() {
        preserve_results(
            require_instance_state(first_endpoint, origin, FIRST_MARKER, FIRST_PATH).await,
            require_instance_state(second_endpoint, origin, SECOND_MARKER, SECOND_PATH).await,
        )
    } else {
        Ok(())
    };
    preserve_results(first_write, preserve_results(second_write, observations))
}

async fn write_instance_state(
    endpoint: &str,
    origin: &LoopbackOrigin,
    marker: &str,
    path: &str,
) -> TestResult {
    let mut connection = CdpSession::connect(endpoint, bounded_cdp_limits())
        .await
        .map_err(box_error)?;
    let flow: TestResult = async {
        let mut target = connection.attach_first_page().await.map_err(box_error)?;
        target.page_enable().await.map_err(box_error)?;
        navigate(&mut target, &origin.url(path)).await?;
        let expression = format!(
            "(() => {{ document.cookie = '{COOKIE_NAME}={marker}; Path=/; SameSite=Lax'; localStorage.setItem('{STORAGE_KEY}', '{marker}'); document.title = 'termglide-{marker}'; document.body.dataset.instance = '{marker}'; return {{origin:location.origin,cookie:document.cookie,storage:localStorage.getItem('{STORAGE_KEY}'),title:document.title,target:document.body.dataset.instance,path:location.pathname}}; }})()"
        );
        let state = evaluate_value(&mut target, &expression).await?;
        require_instance_state_value(runtime_value(&state)?, origin, marker, path)
    }
    .await;
    let close = connection.close().await.map_err(box_error);
    preserve_results(flow, close)
}

async fn require_instance_state(
    endpoint: &str,
    origin: &LoopbackOrigin,
    marker: &str,
    path: &str,
) -> TestResult {
    let mut connection = CdpSession::connect(endpoint, bounded_cdp_limits())
        .await
        .map_err(box_error)?;
    let flow: TestResult = async {
        let mut target = connection.attach_first_page().await.map_err(box_error)?;
        target.page_enable().await.map_err(box_error)?;
        let state = evaluate_value(
            &mut target,
            "({origin:location.origin,cookie:document.cookie,storage:localStorage.getItem('termglide.instance'),title:document.title,target:document.body.dataset.instance,path:location.pathname})",
        )
        .await?;
        require_instance_state_value(runtime_value(&state)?, origin, marker, path)
    }
    .await;
    let close = connection.close().await.map_err(box_error);
    preserve_results(flow, close)
}

fn require_instance_state_value(
    state: &Value,
    origin: &LoopbackOrigin,
    marker: &str,
    path: &str,
) -> TestResult {
    require_string(state, "origin", origin.origin(), "instance document origin")?;
    require_cookie_value(state, marker)?;
    require_string(state, "storage", marker, "instance localStorage value")?;
    let title = format!("termglide-{marker}");
    require_string(state, "title", &title, "instance title state")?;
    require_string(state, "target", marker, "instance target marker")?;
    require_string(state, "path", path, "instance target path")
}

async fn verify_surviving_second_instance(
    process: &mut ExternalEngineProcess,
    profile: &Path,
    endpoint: &str,
    origin: &LoopbackOrigin,
) -> TestResult {
    let profile_before =
        require_profile_directory(profile, "second profile before peer verification");
    let liveness_before = require_process_running(process);
    let cdp = if profile_before.is_ok() && liveness_before.is_ok() {
        verify_peer_cdp_state(endpoint, origin).await
    } else {
        Ok(())
    };
    let liveness_after = require_process_running(process);
    let profile_after =
        require_profile_directory(profile, "second profile after peer verification");
    preserve_results(
        profile_before,
        preserve_results(
            liveness_before,
            preserve_results(cdp, preserve_results(liveness_after, profile_after)),
        ),
    )
}

async fn verify_peer_cdp_state(endpoint: &str, origin: &LoopbackOrigin) -> TestResult {
    let mut connection = CdpSession::connect(endpoint, bounded_cdp_limits())
        .await
        .map_err(box_error)?;
    let flow: TestResult = async {
        let version = connection.browser_version().await.map_err(box_error)?;
        if version.product.is_empty() || version.protocol_version.is_empty() {
            return Err(test_error(
                "surviving browser did not return bounded DevTools version data",
            ));
        }
        let mut target = connection.attach_first_page().await.map_err(box_error)?;
        target.page_enable().await.map_err(box_error)?;
        let state = evaluate_value(
            &mut target,
            "({origin:location.origin,cookie:document.cookie,storage:localStorage.getItem('termglide.instance'),title:document.title,target:document.body.dataset.instance,path:location.pathname})",
        )
        .await?;
        require_instance_state_value(runtime_value(&state)?, origin, SECOND_MARKER, SECOND_PATH)
    }
    .await;
    let close = connection.close().await.map_err(box_error);
    preserve_results(flow, close)
}

fn require_cancellation_isolation(first: &Cancellation, second: &Cancellation) -> TestResult {
    if !first.is_cancelled() {
        return Err(test_error(
            "first instance cancellation scope did not record cancellation",
        ));
    }
    if second.is_cancelled() {
        return Err(test_error(
            "second instance cancellation scope changed after first cancellation",
        ));
    }
    Ok(())
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

fn require_string(value: &Value, field: &str, wanted: &str, label: &str) -> TestResult {
    let actual = value.get(field).and_then(Value::as_str);
    if actual == Some(wanted) {
        Ok(())
    } else {
        Err(test_error(format!(
            "{label} was {actual:?}, wanted {wanted:?}"
        )))
    }
}

fn require_cookie_value(state: &Value, marker: &str) -> TestResult {
    let Some(cookie) = state.get("cookie").and_then(Value::as_str) else {
        return Err(test_error("instance cookie state was not a string"));
    };
    let wanted = format!("{COOKIE_NAME}={marker}");
    if cookie.split(';').map(str::trim).any(|item| item == wanted) {
        Ok(())
    } else {
        Err(test_error(
            "instance cookie state did not retain its scoped marker",
        ))
    }
}

fn force_kill_and_reap(process: &mut ExternalEngineProcess, profile: &Path) -> TestResult {
    let kill = force_kill_process(process.process_id());
    let kill_succeeded = kill.is_ok();
    let observed_exit = if kill_succeeded {
        wait_for_killed_child(process)
    } else {
        Ok(())
    };
    let profile_cleanup = if kill_succeeded && observed_exit.is_ok() {
        require_path_absent(profile, "first profile remained after SIGKILL reap")
    } else {
        Ok(())
    };
    preserve_results(kill, preserve_results(observed_exit, profile_cleanup))
}

fn force_kill_process(process_id: u32) -> TestResult {
    if process_id == 0 {
        return Err(test_error(
            "cannot terminate a browser child without a process identifier",
        ));
    }
    let status = Command::new("/bin/kill")
        .arg("-KILL")
        .arg(process_id.to_string())
        .status()
        .map_err(box_error)?;
    if status.success() {
        Ok(())
    } else {
        Err(test_error(format!(
            "the operating system did not deliver SIGKILL to browser child {process_id}: {status}"
        )))
    }
}

fn wait_for_killed_child(process: &mut ExternalEngineProcess) -> TestResult {
    let deadline = Instant::now() + EXIT_WAIT_TIMEOUT;
    loop {
        match process.try_wait().map_err(box_error) {
            Ok(Some(status)) => {
                if status.signal() == Some(KILL_SIGNAL) {
                    return Ok(());
                }
                return Err(test_error(format!(
                    "browser child exit was not caused by SIGKILL: {status:?}"
                )));
            }
            Ok(None) => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(test_error(
                        "browser child was still running after the bounded SIGKILL wait",
                    ));
                }
                thread::sleep(EXIT_WAIT_POLL_INTERVAL.min(deadline.saturating_duration_since(now)));
            }
            Err(error) => return Err(error),
        }
    }
}

async fn require_stale_endpoint_rejected(endpoint: &str) -> TestResult {
    match CdpSession::connect(endpoint, bounded_cdp_limits()).await {
        Err(_) => Ok(()),
        Ok(mut connection) => {
            let close = connection.close().await.map_err(box_error);
            preserve_results(
                Err(test_error(
                    "stale DevTools endpoint accepted a new CDP connection after child exit",
                )),
                close,
            )
        }
    }
}

fn require_process_running(process: &mut ExternalEngineProcess) -> TestResult {
    match process.try_wait().map_err(box_error)? {
        None => Ok(()),
        Some(status) => Err(test_error(format!(
            "browser child exited before peer-isolation verification: {status:?}"
        ))),
    }
}

fn shutdown_and_verify(process: &mut ExternalEngineProcess, profile: &Path) -> TestResult {
    let shutdown = process.shutdown().map_err(box_error);
    let reaped = if shutdown.is_ok() {
        match process.try_wait().map_err(box_error) {
            Ok(Some(_)) => Ok(()),
            Ok(None) => Err(test_error(
                "supervised browser child remained running after shutdown",
            )),
            Err(error) => Err(error),
        }
    } else {
        Ok(())
    };
    preserve_results(
        shutdown,
        preserve_results(
            reaped,
            require_path_absent(profile, "external profile remained after process cleanup"),
        ),
    )
}

fn bounded_cdp_limits() -> CdpLimits {
    CdpLimits {
        operation_timeout: CDP_OPERATION_TIMEOUT,
        ..CdpLimits::default()
    }
}

fn require_path_absent(path: &Path, message: &str) -> TestResult {
    if path.exists() {
        Err(test_error(format!("{message}: {}", path.display())))
    } else {
        Ok(())
    }
}

fn require_profile_directory(path: &Path, label: &str) -> TestResult {
    if path.is_dir() {
        Ok(())
    } else {
        Err(test_error(format!(
            "{label} was not present: {}",
            path.display()
        )))
    }
}

fn remove_empty_profile_parents(first: &Path, second: &Path) -> TestResult {
    preserve_results(
        remove_empty_directory_if_present(first),
        remove_empty_directory_if_present(second),
    )
}

fn remove_empty_directory_if_present(path: &Path) -> TestResult {
    let mut entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(box_error(error)),
    };
    if entries.next().is_some() {
        return Err(test_error(format!(
            "profile parent retained entries after process cleanup: {}",
            path.display()
        )));
    }
    fs::remove_dir(path).map_err(box_error)
}

fn preserve_results(primary: TestResult, cleanup: TestResult) -> TestResult {
    match (primary, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(primary), Err(cleanup)) => Err(test_error(format!(
            "primary failure: {primary}; cleanup failure: {cleanup}"
        ))),
    }
}

fn box_error(error: impl Error + Send + Sync + 'static) -> Box<dyn Error + Send + Sync> {
    Box::new(error)
}

fn test_error(message: impl Into<String>) -> Box<dyn Error + Send + Sync> {
    Box::new(io::Error::other(message.into()))
}

struct LoopbackOrigin {
    origin: String,
    cancellation: Cancellation,
    task: Option<JoinHandle<TestResult>>,
}

impl LoopbackOrigin {
    async fn start() -> TestResult<Self> {
        let listener = timeout(
            SERVER_START_TIMEOUT,
            TcpListener::bind((Ipv4Addr::LOCALHOST, 0)),
        )
        .await
        .map_err(|_| test_error("loopback origin listener setup exceeded its bounded wait"))?
        .map_err(box_error)?;
        let address = listener.local_addr().map_err(box_error)?;
        if address.ip() != IpAddr::V4(Ipv4Addr::LOCALHOST) || address.port() == 0 {
            return Err(test_error(
                "loopback origin listener did not retain its ephemeral IPv4 address",
            ));
        }
        let cancellation = Cancellation::new();
        let task = tokio::spawn(serve_loopback(listener, cancellation.clone()));
        Ok(Self {
            origin: format!("http://{address}"),
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
            .ok_or_else(|| test_error("loopback origin task was already consumed"))?;
        let joined = match timeout(SERVER_SHUTDOWN_TIMEOUT, &mut task).await {
            Ok(joined) => joined,
            Err(_) => {
                task.abort();
                return Err(test_error(
                    "loopback origin server did not stop before its bounded wait",
                ));
            }
        };
        joined.map_err(box_error)?
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

async fn serve_loopback(listener: TcpListener, cancellation: Cancellation) -> TestResult {
    let mut accepted = 0usize;
    loop {
        let (stream, peer) = tokio::select! {
            () = cancellation.cancelled() => return Ok(()),
            accepted_connection = timeout(SERVER_ACCEPT_TIMEOUT, listener.accept()) => {
                match accepted_connection {
                    Ok(Ok(connection)) => connection,
                    Ok(Err(error)) => return Err(box_error(error)),
                    Err(_) => return Err(test_error("loopback origin did not receive a request before its bounded wait")),
                }
            }
        };
        if !peer.ip().is_loopback() {
            return Err(test_error(format!(
                "loopback origin accepted a non-loopback peer: {peer}"
            )));
        }
        accepted = match accepted.checked_add(1) {
            Some(value) => value,
            None => return Err(test_error("loopback origin connection count overflowed")),
        };
        if accepted > MAX_SERVER_CONNECTIONS {
            return Err(test_error(format!(
                "loopback origin accepted more than {MAX_SERVER_CONNECTIONS} connections"
            )));
        }
        serve_loopback_connection(stream, &cancellation).await?;
    }
}

async fn serve_loopback_connection(
    mut stream: TcpStream,
    cancellation: &Cancellation,
) -> TestResult {
    tokio::select! {
        () = cancellation.cancelled() => return Ok(()),
        read = timeout(SERVER_REQUEST_TIMEOUT, read_http_header(&mut stream)) => {
            match read {
                Ok(result) => {
                    if !result? {
                        return Ok(());
                    }
                }
                Err(_) => return Err(test_error("loopback origin request input exceeded its bounded wait")),
            }
        }
    }
    let response = loopback_response()?;
    tokio::select! {
        () = cancellation.cancelled() => return Ok(()),
        write = timeout(SERVER_REQUEST_TIMEOUT, write_http_response(&mut stream, &response)) => {
            match write {
                Ok(result) => result?,
                Err(_) => return Err(test_error("loopback origin response output exceeded its bounded wait")),
            }
        }
    }
    Ok(())
}

async fn read_http_header(stream: &mut TcpStream) -> TestResult<bool> {
    let mut buffer = [0_u8; MAX_REQUEST_BYTES];
    let mut used = 0usize;
    loop {
        if used == buffer.len() {
            return Err(test_error(format!(
                "loopback origin request exceeded {MAX_REQUEST_BYTES} bytes"
            )));
        }
        let read = match timeout(SERVER_HEADER_IDLE_TIMEOUT, stream.read(&mut buffer[used..])).await
        {
            Ok(Ok(read)) => read,
            Ok(Err(error)) if client_gone(&error) => return Ok(false),
            Ok(Err(error)) => return Err(box_error(error)),
            Err(_) if used == 0 => return Ok(false),
            Err(_) => return Err(test_error("loopback origin request header stalled")),
        };
        if read == 0 {
            if used == 0 {
                return Ok(false);
            }
            return Err(test_error(
                "loopback origin peer closed before a complete request header",
            ));
        }
        used = match used.checked_add(read) {
            Some(value) => value,
            None => return Err(test_error("loopback origin request length overflowed")),
        };
        if has_http_header_end(&buffer[..used]) {
            return Ok(true);
        }
    }
}

fn has_http_header_end(bytes: &[u8]) -> bool {
    bytes.windows(4).any(|window| window == b"\r\n\r\n")
}

fn loopback_response() -> TestResult<Vec<u8>> {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{LOOPBACK_DOCUMENT}",
        LOOPBACK_DOCUMENT.len()
    );
    if response.len() > MAX_RESPONSE_BYTES {
        return Err(test_error(format!(
            "loopback origin response exceeded {MAX_RESPONSE_BYTES} bytes"
        )));
    }
    Ok(response.into_bytes())
}

async fn write_http_response(stream: &mut TcpStream, response: &[u8]) -> TestResult {
    match stream.write_all(response).await {
        Ok(()) => {}
        // Chrome may close the connection as soon as it has what it needs; on Windows an
        // in-flight write then fails with a connection reset, which is not a fixture failure.
        Err(error) if client_gone(&error) => return Ok(()),
        Err(error) => return Err(box_error(error)),
    }
    match stream.shutdown().await {
        Ok(()) => {}
        Err(error) if client_gone(&error) => {}
        Err(error) => return Err(box_error(error)),
    }
    Ok(())
}

fn client_gone(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe
    )
}

struct TestRoot {
    path: PathBuf,
    removed: bool,
}

impl TestRoot {
    fn new() -> io::Result<Self> {
        let parent = std::env::temp_dir();
        fs::create_dir_all(&parent)?;
        for _ in 0..ROOT_ALLOCATION_ATTEMPTS {
            let timestamp = match SystemTime::now().duration_since(UNIX_EPOCH) {
                Ok(duration) => duration.as_nanos(),
                Err(error) => error.duration().as_nanos(),
            };
            let counter = ROOT_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!(
                "termglide-external-multi-instance-{timestamp}-{counter}"
            ));
            match fs::create_dir(&path) {
                Ok(()) => {
                    return Ok(Self {
                        path,
                        removed: false,
                    });
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            ErrorKind::AlreadyExists,
            "could not allocate an external multi-instance test root",
        ))
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn verify_empty_and_remove(&mut self) -> io::Result<()> {
        let mut entries = fs::read_dir(&self.path)?;
        if entries.next().is_some() {
            return Err(io::Error::other(format!(
                "multi-instance test root retained files after cleanup: {}",
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
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
