#![cfg(unix)]

//! Real-Chrome coverage for typed external-proxy boundaries over loopback fixtures.

use std::collections::BTreeSet;
use std::error::Error;
use std::fs;
use std::io::{self, ErrorKind};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tg_browser::{
    DevToolsEndpoint, ExternalEngineError, ExternalEngineLaunchOptions, ExternalEngineProcess,
    ExternalEngineProxy, ExternalEngineProxyBypass, ExternalEngineViewport,
    discover_external_engine, launch_external_engine,
};
use tg_core::Cancellation;
use tg_network::{CdpLimits, CdpSession, CdpTargetSession};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};
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
const REQUEST_HEADER_IDLE_TIMEOUT: Duration = Duration::from_millis(250);
const SERVER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const OBSERVATION_TIMEOUT: Duration = Duration::from_secs(5);
const OBSERVATION_POLL_INTERVAL: Duration = Duration::from_millis(20);
const MAX_SERVER_CONNECTIONS: usize = 128;
const MAX_REQUEST_BYTES: usize = 16 * 1024;
const MAX_RESPONSE_BYTES: usize = 16 * 1024;
const ROOT_ALLOCATION_ATTEMPTS: u8 = 32;
const KILL_SIGNAL: i32 = 9;
const PROXY_TARGET_HOST: &str = "proxy-consumer.invalid";
const PROXY_PATH: &str = "/through-proxy";
const BYPASS_PATH: &str = "/direct-bypass";
const ABRUPT_PATH: &str = "/abrupt-proxy-close";
const RELAUNCH_PATH: &str = "/clean-relaunch";
const PROXY_ROUTE_HEADER: &str = "x-termglide-route";
const PROXY_ROUTE_VALUE: &str = "proxy";

static ROOT_COUNTER: AtomicU64 = AtomicU64::new(0);

#[tokio::test]
async fn external_proxy_boundary_chrome() -> TestResult {
    verify_proxy_inputs_fail_closed()?;

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
    let fixture_setup = ProxyFixtures::start().await;
    let operation = match fixture_setup {
        Ok(fixtures) => {
            let flow = exercise_proxy_boundary(&executable, &root, &fixtures).await;
            let fixture_cleanup = fixtures.shutdown().await;
            preserve_results(flow, fixture_cleanup)
        }
        Err(error) => Err(error),
    };
    let root_cleanup = root.verify_empty_and_remove().map_err(box_error);
    preserve_results(operation, root_cleanup)
}

fn verify_proxy_inputs_fail_closed() -> TestResult {
    let credential_proxy = Url::parse("http://user:secret@127.0.0.1:8080").map_err(box_error)?;
    match ExternalEngineProxy::new(credential_proxy) {
        Err(ExternalEngineError::ProxyCredentialsDisallowed) => {}
        Err(error) => {
            return Err(test_error(format!(
                "credential-bearing proxy returned a different fail-closed error: {error}"
            )));
        }
        Ok(_) => {
            return Err(test_error(
                "credential-bearing proxy was accepted before launch",
            ));
        }
    }

    let path_proxy = Url::parse("http://127.0.0.1:8080/proxy").map_err(box_error)?;
    match ExternalEngineProxy::new(path_proxy) {
        Err(ExternalEngineError::ProxyPathDisallowed) => {}
        Err(error) => {
            return Err(test_error(format!(
                "path-bearing proxy returned a different fail-closed error: {error}"
            )));
        }
        Ok(_) => return Err(test_error("path-bearing proxy was accepted before launch")),
    }

    let query_proxy = Url::parse("http://127.0.0.1:8080/?route=direct").map_err(box_error)?;
    match ExternalEngineProxy::new(query_proxy) {
        Err(ExternalEngineError::ProxyQueryOrFragmentDisallowed) => {}
        Err(error) => {
            return Err(test_error(format!(
                "query-bearing proxy returned a different fail-closed error: {error}"
            )));
        }
        Ok(_) => return Err(test_error("query-bearing proxy was accepted before launch")),
    }

    match ExternalEngineProxyBypass::new(["user:secret@origin.invalid"]) {
        Err(ExternalEngineError::ProxyBypassCredentialsDisallowed) => {}
        Err(error) => {
            return Err(test_error(format!(
                "credential-like bypass returned a different fail-closed error: {error}"
            )));
        }
        Ok(_) => {
            return Err(test_error(
                "credential-like bypass was accepted before launch",
            ));
        }
    }

    match ExternalEngineProxyBypass::new(["origin.invalid/path"]) {
        Err(ExternalEngineError::InvalidProxyBypassEntry { .. }) => {}
        Err(error) => {
            return Err(test_error(format!(
                "path-like bypass returned a different fail-closed error: {error}"
            )));
        }
        Ok(_) => return Err(test_error("path-like bypass was accepted before launch")),
    }

    match ExternalEngineProxyBypass::new(["origin.invalid?route=direct"]) {
        Err(ExternalEngineError::InvalidProxyBypassEntry { .. }) => {}
        Err(error) => {
            return Err(test_error(format!(
                "query-like bypass returned a different fail-closed error: {error}"
            )));
        }
        Ok(_) => return Err(test_error("query-like bypass was accepted before launch")),
    }

    match ExternalEngineProxyBypass::new(["*"]) {
        Err(ExternalEngineError::ProxyBypassWildcardDisallowed) => {}
        Err(error) => {
            return Err(test_error(format!(
                "global bypass returned a different fail-closed error: {error}"
            )));
        }
        Ok(_) => return Err(test_error("global bypass was accepted before launch")),
    }

    match ExternalEngineProxyBypass::new(["127.0.0.2", "127.0.0.2"]) {
        Err(ExternalEngineError::ProxyBypassDuplicateEntry) => Ok(()),
        Err(error) => Err(test_error(format!(
            "duplicate bypass returned a different fail-closed error: {error}"
        ))),
        Ok(_) => Err(test_error("duplicate bypass was accepted before launch")),
    }
}

async fn exercise_proxy_boundary(
    executable: &Path,
    root: &TestRoot,
    fixtures: &ProxyFixtures,
) -> TestResult {
    let profile_parent = root.path().join("profiles");
    let options = proxy_launch_options(executable, &profile_parent, fixtures)?;
    let cancellation = Cancellation::new();
    let mut first = match launch_external_engine(&options, &cancellation) {
        Ok(process) => process,
        Err(error) => {
            let parent_cleanup = remove_empty_directory_if_present(&profile_parent);
            return preserve_results(Err(box_error(error)), parent_cleanup);
        }
    };
    let first_profile = first.isolated_profile_dir().to_path_buf();
    let first_endpoint = first.endpoint().websocket_url();

    let preconditions =
        require_live_loopback_process(&first, executable, &profile_parent, &first_profile);
    let route_contract = match preconditions {
        Ok(()) => exercise_proxy_and_bypass_routes(&first_endpoint, fixtures).await,
        Err(error) => Err(error),
    };
    let abrupt_proxy = if route_contract.is_ok() {
        exercise_abrupt_proxy_close(&first_endpoint, &fixtures.proxy, &fixtures.proxied_origin)
            .await
    } else {
        Ok(())
    };

    cancellation.cancel();
    let cancellation_contract = require_cancelled_proxy_launch_refusal(&options, &cancellation);
    let first_exit = force_kill_and_reap(&mut first, &first_profile);
    let stale_endpoint = if first_exit.is_ok() {
        require_stale_endpoint_rejected(&first_endpoint).await
    } else {
        Ok(())
    };
    let relaunch = if first_exit.is_ok() {
        exercise_clean_proxy_relaunch(
            &options,
            executable,
            &profile_parent,
            &first_profile,
            fixtures,
        )
        .await
    } else {
        Ok(())
    };

    let first_cleanup = shutdown_and_verify(&mut first, &first_profile);
    drop(first);
    let parent_cleanup = remove_empty_directory_if_present(&profile_parent);

    let operation = preserve_results(
        route_contract,
        preserve_results(
            abrupt_proxy,
            preserve_results(
                cancellation_contract,
                preserve_results(first_exit, preserve_results(stale_endpoint, relaunch)),
            ),
        ),
    );
    preserve_results(operation, preserve_results(first_cleanup, parent_cleanup))
}

fn proxy_launch_options(
    executable: &Path,
    profile_parent: &Path,
    fixtures: &ProxyFixtures,
) -> TestResult<ExternalEngineLaunchOptions> {
    let initial_target = Url::parse("about:blank").map_err(box_error)?;
    let proxy_server = Url::parse(fixtures.proxy.endpoint()).map_err(box_error)?;
    let bypass_entry = fixtures.direct_origin.address().ip().to_string();
    let bypass = ExternalEngineProxyBypass::new([bypass_entry]).map_err(box_error)?;
    let proxy = ExternalEngineProxy::new(proxy_server)
        .map_err(box_error)?
        .with_bypass(bypass);

    let mut options = ExternalEngineLaunchOptions::new(initial_target);
    options.executable_override = Some(executable.to_path_buf());
    options.viewport = Some(ExternalEngineViewport::new(640, 480).map_err(box_error)?);
    options.startup_timeout = STARTUP_TIMEOUT;
    options.poll_interval = STARTUP_POLL_INTERVAL;
    options.temporary_profile_parent = Some(profile_parent.to_path_buf());
    options.proxy = Some(proxy);
    Ok(options)
}

fn require_live_loopback_process(
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

async fn exercise_proxy_and_bypass_routes(endpoint: &str, fixtures: &ProxyFixtures) -> TestResult {
    let mut connection = CdpSession::connect(endpoint, bounded_cdp_limits())
        .await
        .map_err(box_error)?;
    let flow: TestResult = async {
        let mut target = connection.attach_first_page().await.map_err(box_error)?;
        target.page_enable().await.map_err(box_error)?;

        navigate_and_require_route(
            &mut target,
            &fixtures.proxy.target_url(PROXY_PATH),
            OriginRoute::Proxy,
        )
        .await?;
        fixtures.proxy.wait_for_forwarded(PROXY_PATH).await?;
        fixtures
            .proxied_origin
            .wait_for_route(PROXY_PATH, OriginRoute::Proxy)
            .await?;

        navigate_and_require_route(
            &mut target,
            &fixtures.direct_origin.url(BYPASS_PATH),
            OriginRoute::Direct,
        )
        .await?;
        fixtures
            .direct_origin
            .wait_for_route(BYPASS_PATH, OriginRoute::Direct)
            .await?;
        fixtures.proxy.require_no_request_path(BYPASS_PATH)?;
        Ok(())
    }
    .await;
    let close = connection.close().await.map_err(box_error);
    preserve_results(flow, close)
}

async fn exercise_abrupt_proxy_close(
    endpoint: &str,
    proxy: &LoopbackHttpProxy,
    proxied_origin: &LoopbackOrigin,
) -> TestResult {
    proxy.close_connections_for(ABRUPT_PATH);
    let mut connection = CdpSession::connect(endpoint, bounded_cdp_limits())
        .await
        .map_err(box_error)?;
    let flow: TestResult = async {
        let mut target = connection.attach_first_page().await.map_err(box_error)?;
        target.page_enable().await.map_err(box_error)?;
        let navigation = target
            .navigate(&proxy.target_url(ABRUPT_PATH))
            .await
            .map_err(box_error)?;
        if navigation.error_text.is_none() && target.wait_for_load().await.is_ok() {
            let state = evaluate_value(&mut target, "document.body.dataset.route").await?;
            if runtime_value(&state)?.as_str() == Some(OriginRoute::Proxy.as_str()) {
                return Err(test_error(
                    "abrupt loopback proxy close unexpectedly rendered the proxied origin",
                ));
            }
        }
        Ok(())
    }
    .await;
    let close = connection.close().await.map_err(box_error);
    let observed_close = proxy.wait_for_abrupt_close(ABRUPT_PATH).await;
    let origin_contract = proxied_origin.require_no_route_path(ABRUPT_PATH);
    preserve_results(
        preserve_results(flow, close),
        preserve_results(observed_close, origin_contract),
    )
}

async fn exercise_clean_proxy_relaunch(
    options: &ExternalEngineLaunchOptions,
    executable: &Path,
    profile_parent: &Path,
    stale_profile: &Path,
    fixtures: &ProxyFixtures,
) -> TestResult {
    let cancellation = Cancellation::new();
    let mut replacement = launch_external_engine(options, &cancellation).map_err(box_error)?;
    let replacement_profile = replacement.isolated_profile_dir().to_path_buf();
    let replacement_endpoint = replacement.endpoint().websocket_url();

    let profile_contract = if replacement_profile == stale_profile {
        Err(test_error(
            "clean relaunch reused the terminated browser profile path",
        ))
    } else {
        require_path_absent(stale_profile, "terminated browser profile was reused")
    };
    let preconditions = preserve_results(
        require_live_loopback_process(
            &replacement,
            executable,
            profile_parent,
            &replacement_profile,
        ),
        profile_contract,
    );
    let route_contract = match preconditions {
        Ok(()) => exercise_proxy_route_after_relaunch(&replacement_endpoint, fixtures).await,
        Err(error) => Err(error),
    };
    let liveness = require_process_running(&mut replacement);
    let cleanup = shutdown_and_verify(&mut replacement, &replacement_profile);
    drop(replacement);
    preserve_results(preserve_results(route_contract, liveness), cleanup)
}

async fn exercise_proxy_route_after_relaunch(
    endpoint: &str,
    fixtures: &ProxyFixtures,
) -> TestResult {
    let mut connection = CdpSession::connect(endpoint, bounded_cdp_limits())
        .await
        .map_err(box_error)?;
    let flow: TestResult = async {
        let mut target = connection.attach_first_page().await.map_err(box_error)?;
        target.page_enable().await.map_err(box_error)?;
        navigate_and_require_route(
            &mut target,
            &fixtures.proxy.target_url(RELAUNCH_PATH),
            OriginRoute::Proxy,
        )
        .await?;
        fixtures.proxy.wait_for_forwarded(RELAUNCH_PATH).await?;
        fixtures
            .proxied_origin
            .wait_for_route(RELAUNCH_PATH, OriginRoute::Proxy)
            .await
    }
    .await;
    let close = connection.close().await.map_err(box_error);
    preserve_results(flow, close)
}

async fn navigate_and_require_route(
    target: &mut CdpTargetSession<'_>,
    url: &str,
    route: OriginRoute,
) -> TestResult {
    let navigation = target.navigate(url).await.map_err(box_error)?;
    if let Some(message) = navigation.error_text {
        return Err(test_error(format!(
            "loopback navigation failed for {url}: {message}"
        )));
    }
    target.wait_for_load().await.map_err(box_error)?;
    let state = evaluate_value(target, "document.body.dataset.route").await?;
    let value = runtime_value(&state)?;
    let actual = value.as_str();
    if actual == Some(route.as_str()) {
        Ok(())
    } else {
        Err(test_error(format!(
            "origin response route was {actual:?}, wanted {:?}",
            route.as_str()
        )))
    }
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

fn require_cancelled_proxy_launch_refusal(
    options: &ExternalEngineLaunchOptions,
    cancellation: &Cancellation,
) -> TestResult {
    if !cancellation.is_cancelled() {
        return Err(test_error(
            "external browser cancellation scope did not record cancellation",
        ));
    }
    match launch_external_engine(options, cancellation) {
        Err(ExternalEngineError::Cancelled) => Ok(()),
        Err(error) => Err(test_error(format!(
            "cancelled typed-proxy launch returned an unexpected error: {error}"
        ))),
        Ok(mut process) => {
            let profile = process.isolated_profile_dir().to_path_buf();
            let cleanup = shutdown_and_verify(&mut process, &profile);
            preserve_results(
                Err(test_error(
                    "cancelled typed-proxy launch unexpectedly produced a supervised browser process",
                )),
                cleanup,
            )
        }
    }
}

fn runtime_value(evaluation: &Value) -> TestResult<&Value> {
    evaluation
        .get("value")
        .ok_or_else(|| test_error("runtime evaluation omitted a by-value result"))
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
            "browser child exited before clean proxy relaunch verification: {status:?}"
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

fn preserve_results<T>(primary: TestResult<T>, cleanup: TestResult) -> TestResult<T> {
    match (primary, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OriginRoute {
    Direct,
    Proxy,
}

impl OriginRoute {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Proxy => "proxy",
        }
    }
}

#[derive(Debug, Clone)]
struct OriginRecord {
    path: String,
    route: OriginRoute,
}

struct LoopbackOrigin {
    address: SocketAddr,
    origin: String,
    records: Arc<Mutex<Vec<OriginRecord>>>,
    cancellation: Cancellation,
    task: Option<JoinHandle<TestResult>>,
}

impl LoopbackOrigin {
    async fn start(bind_address: Ipv4Addr) -> TestResult<Self> {
        let listener = timeout(SERVER_START_TIMEOUT, TcpListener::bind((bind_address, 0)))
            .await
            .map_err(|_| test_error("loopback origin listener setup exceeded its bounded wait"))?
            .map_err(box_error)?;
        let address = listener.local_addr().map_err(box_error)?;
        if address.ip() != IpAddr::V4(bind_address) || address.port() == 0 {
            return Err(test_error(
                "loopback origin listener did not retain its ephemeral IPv4 address",
            ));
        }
        let records = Arc::new(Mutex::new(Vec::<OriginRecord>::new()));
        let cancellation = Cancellation::new();
        let task = tokio::spawn(serve_origin(
            listener,
            records.clone(),
            cancellation.clone(),
        ));
        Ok(Self {
            address,
            origin: format!("http://{address}"),
            records,
            cancellation,
            task: Some(task),
        })
    }

    fn address(&self) -> SocketAddr {
        self.address
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.origin)
    }

    async fn wait_for_route(&self, path: &str, route: OriginRoute) -> TestResult {
        let deadline = Instant::now() + OBSERVATION_TIMEOUT;
        loop {
            let found = {
                let records = lock_unpoisoned(&self.records);
                records
                    .iter()
                    .any(|record| record.path == path && record.route == route)
            };
            if found {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(test_error(format!(
                    "loopback origin did not record route {} for {path}",
                    route.as_str()
                )));
            }
            sleep(OBSERVATION_POLL_INTERVAL).await;
        }
    }

    fn require_no_route_path(&self, path: &str) -> TestResult {
        let records = lock_unpoisoned(&self.records);
        if records.iter().any(|record| record.path == path) {
            Err(test_error(format!(
                "abruptly closed proxy path reached the loopback origin: {path}"
            )))
        } else {
            Ok(())
        }
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

#[derive(Debug, Clone)]
struct ProxyRecord {
    path: Option<String>,
    forwarded: bool,
    abruptly_closed: bool,
}

struct LoopbackHttpProxy {
    endpoint: String,
    records: Arc<Mutex<Vec<ProxyRecord>>>,
    abrupt_paths: Arc<Mutex<BTreeSet<String>>>,
    cancellation: Cancellation,
    task: Option<JoinHandle<TestResult>>,
}

impl LoopbackHttpProxy {
    async fn start(origin_address: SocketAddr) -> TestResult<Self> {
        if !origin_address.ip().is_loopback() || origin_address.port() == 0 {
            return Err(test_error(
                "loopback HTTP proxy received a non-loopback forwarding address",
            ));
        }
        let listener = timeout(
            SERVER_START_TIMEOUT,
            TcpListener::bind((Ipv4Addr::LOCALHOST, 0)),
        )
        .await
        .map_err(|_| test_error("loopback proxy listener setup exceeded its bounded wait"))?
        .map_err(box_error)?;
        let address = listener.local_addr().map_err(box_error)?;
        if address.ip() != IpAddr::V4(Ipv4Addr::LOCALHOST) || address.port() == 0 {
            return Err(test_error(
                "loopback proxy listener did not retain its ephemeral IPv4 address",
            ));
        }
        let records = Arc::new(Mutex::new(Vec::<ProxyRecord>::new()));
        let abrupt_paths = Arc::new(Mutex::new(BTreeSet::<String>::new()));
        let cancellation = Cancellation::new();
        let task = tokio::spawn(serve_proxy(
            listener,
            origin_address,
            records.clone(),
            abrupt_paths.clone(),
            cancellation.clone(),
        ));
        Ok(Self {
            endpoint: format!("http://{address}"),
            records,
            abrupt_paths,
            cancellation,
            task: Some(task),
        })
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn target_url(&self, path: &str) -> String {
        format!("http://{PROXY_TARGET_HOST}{path}")
    }

    fn close_connections_for(&self, path: &str) {
        let mut paths = lock_unpoisoned(&self.abrupt_paths);
        paths.insert(path.to_owned());
    }

    async fn wait_for_forwarded(&self, path: &str) -> TestResult {
        let deadline = Instant::now() + OBSERVATION_TIMEOUT;
        loop {
            let found = {
                let records = lock_unpoisoned(&self.records);
                records
                    .iter()
                    .any(|record| record.path.as_deref() == Some(path) && record.forwarded)
            };
            if found {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(test_error(format!(
                    "loopback proxy did not forward the typed proxy request for {path}"
                )));
            }
            sleep(OBSERVATION_POLL_INTERVAL).await;
        }
    }

    async fn wait_for_abrupt_close(&self, path: &str) -> TestResult {
        let deadline = Instant::now() + OBSERVATION_TIMEOUT;
        loop {
            let found = {
                let records = lock_unpoisoned(&self.records);
                records
                    .iter()
                    .any(|record| record.path.as_deref() == Some(path) && record.abruptly_closed)
            };
            if found {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(test_error(format!(
                    "loopback proxy did not close the configured request for {path}"
                )));
            }
            sleep(OBSERVATION_POLL_INTERVAL).await;
        }
    }

    fn require_no_request_path(&self, path: &str) -> TestResult {
        let records = lock_unpoisoned(&self.records);
        if records
            .iter()
            .any(|record| record.path.as_deref() == Some(path))
        {
            Err(test_error(format!(
                "explicit bypass path was received by the loopback proxy: {path}"
            )))
        } else {
            Ok(())
        }
    }

    async fn shutdown(mut self) -> TestResult {
        self.cancellation.cancel();
        let mut task = self
            .task
            .take()
            .ok_or_else(|| test_error("loopback proxy task was already consumed"))?;
        let joined = match timeout(SERVER_SHUTDOWN_TIMEOUT, &mut task).await {
            Ok(joined) => joined,
            Err(_) => {
                task.abort();
                return Err(test_error(
                    "loopback proxy server did not stop before its bounded wait",
                ));
            }
        };
        joined.map_err(box_error)?
    }
}

impl Drop for LoopbackHttpProxy {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

struct ProxyFixtures {
    proxied_origin: LoopbackOrigin,
    direct_origin: LoopbackOrigin,
    proxy: LoopbackHttpProxy,
}

impl ProxyFixtures {
    async fn start() -> TestResult<Self> {
        let proxied_origin = LoopbackOrigin::start(Ipv4Addr::LOCALHOST).await?;
        let direct_origin = match LoopbackOrigin::start(Ipv4Addr::LOCALHOST).await {
            Ok(origin) => origin,
            Err(error) => {
                let cleanup = proxied_origin.shutdown().await;
                return preserve_results(Err(error), cleanup);
            }
        };
        let proxy = match LoopbackHttpProxy::start(proxied_origin.address()).await {
            Ok(proxy) => proxy,
            Err(error) => {
                let direct_cleanup = direct_origin.shutdown().await;
                let proxied_cleanup = proxied_origin.shutdown().await;
                return preserve_results(
                    Err(error),
                    preserve_results(direct_cleanup, proxied_cleanup),
                );
            }
        };
        Ok(Self {
            proxied_origin,
            direct_origin,
            proxy,
        })
    }

    async fn shutdown(self) -> TestResult {
        let proxy = self.proxy.shutdown().await;
        let proxied_origin = self.proxied_origin.shutdown().await;
        let direct_origin = self.direct_origin.shutdown().await;
        preserve_results(proxy, preserve_results(proxied_origin, direct_origin))
    }
}

async fn serve_origin(
    listener: TcpListener,
    records: Arc<Mutex<Vec<OriginRecord>>>,
    cancellation: Cancellation,
) -> TestResult {
    let mut accepted = 0usize;
    loop {
        let Some((stream, peer)) = accept_loopback_connection(&listener, &cancellation).await?
        else {
            return Ok(());
        };
        if !peer.ip().is_loopback() {
            return Err(test_error(format!(
                "loopback origin accepted a non-loopback peer: {peer}"
            )));
        }
        accepted = increment_connection_count(accepted)?;
        serve_origin_connection(stream, &records, &cancellation).await?;
    }
}

async fn serve_proxy(
    listener: TcpListener,
    origin_address: SocketAddr,
    records: Arc<Mutex<Vec<ProxyRecord>>>,
    abrupt_paths: Arc<Mutex<BTreeSet<String>>>,
    cancellation: Cancellation,
) -> TestResult {
    let mut accepted = 0usize;
    loop {
        let Some((stream, peer)) = accept_loopback_connection(&listener, &cancellation).await?
        else {
            return Ok(());
        };
        if !peer.ip().is_loopback() {
            return Err(test_error(format!(
                "loopback proxy accepted a non-loopback peer: {peer}"
            )));
        }
        accepted = increment_connection_count(accepted)?;
        serve_proxy_connection(
            stream,
            origin_address,
            &records,
            &abrupt_paths,
            &cancellation,
        )
        .await?;
    }
}

async fn accept_loopback_connection(
    listener: &TcpListener,
    cancellation: &Cancellation,
) -> TestResult<Option<(TcpStream, SocketAddr)>> {
    tokio::select! {
        () = cancellation.cancelled() => Ok(None),
        accepted = timeout(SERVER_ACCEPT_TIMEOUT, listener.accept()) => {
            match accepted {
                Ok(Ok(connection)) => Ok(Some(connection)),
                Ok(Err(error)) => Err(box_error(error)),
                Err(_) => Err(test_error("loopback fixture did not receive a connection before its bounded wait")),
            }
        }
    }
}

fn increment_connection_count(current: usize) -> TestResult<usize> {
    let next = match current.checked_add(1) {
        Some(value) => value,
        None => return Err(test_error("loopback fixture connection count overflowed")),
    };
    if next > MAX_SERVER_CONNECTIONS {
        return Err(test_error(format!(
            "loopback fixture accepted more than {MAX_SERVER_CONNECTIONS} connections"
        )));
    }
    Ok(next)
}

async fn serve_origin_connection(
    mut stream: TcpStream,
    records: &Arc<Mutex<Vec<OriginRecord>>>,
    cancellation: &Cancellation,
) -> TestResult {
    let Some(request) = read_bounded_request(&mut stream, cancellation).await? else {
        return Ok(());
    };
    if request.method != "GET" {
        let response = http_response("405 Method Not Allowed", "method not allowed")?;
        return write_bounded_response(&mut stream, &response, cancellation).await;
    }
    let path = origin_form_path(&request.target)?;
    let route = match request.header(PROXY_ROUTE_HEADER) {
        Some(value) if value.eq_ignore_ascii_case(PROXY_ROUTE_VALUE) => OriginRoute::Proxy,
        _ => OriginRoute::Direct,
    };
    {
        let mut records = lock_unpoisoned(records);
        records.push(OriginRecord { path, route });
    }
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><link rel=\"icon\" href=\"data:,\"><body data-route=\"{}\">loopback origin</body>",
        route.as_str()
    );
    let response = http_response("200 OK", &body)?;
    write_bounded_response(&mut stream, &response, cancellation).await
}

async fn serve_proxy_connection(
    mut stream: TcpStream,
    origin_address: SocketAddr,
    records: &Arc<Mutex<Vec<ProxyRecord>>>,
    abrupt_paths: &Arc<Mutex<BTreeSet<String>>>,
    cancellation: &Cancellation,
) -> TestResult {
    let Some(request) = read_bounded_request(&mut stream, cancellation).await? else {
        return Ok(());
    };
    let target = Url::parse(&request.target).ok();
    let path = target.as_ref().map(|target| target.path().to_owned());
    let supported = request.method == "GET"
        && target.as_ref().is_some_and(|target| {
            target.scheme() == "http"
                && target.host_str() == Some(PROXY_TARGET_HOST)
                && target.username().is_empty()
                && target.password().is_none()
        });

    if !supported {
        {
            let mut records = lock_unpoisoned(records);
            records.push(ProxyRecord {
                path,
                forwarded: false,
                abruptly_closed: false,
            });
        }
        let response = http_response("502 Bad Gateway", "loopback proxy rejected this target")?;
        return write_bounded_response(&mut stream, &response, cancellation).await;
    }

    let path = match path {
        Some(path) => path,
        None => return Err(test_error("supported proxy target did not contain a path")),
    };
    let abruptly_closed = {
        let paths = lock_unpoisoned(abrupt_paths);
        paths.contains(&path)
    };
    {
        let mut records = lock_unpoisoned(records);
        records.push(ProxyRecord {
            path: Some(path.clone()),
            forwarded: !abruptly_closed,
            abruptly_closed,
        });
    }
    if abruptly_closed {
        return Ok(());
    }

    let Some(response) = forward_to_loopback_origin(origin_address, &path, cancellation).await?
    else {
        return Ok(());
    };
    write_bounded_response(&mut stream, &response, cancellation).await
}

async fn forward_to_loopback_origin(
    origin_address: SocketAddr,
    path: &str,
    cancellation: &Cancellation,
) -> TestResult<Option<Vec<u8>>> {
    if !origin_address.ip().is_loopback() || origin_address.port() == 0 {
        return Err(test_error(
            "proxy forwarding target was not a live loopback origin",
        ));
    }
    let mut origin = tokio::select! {
        () = cancellation.cancelled() => return Ok(None),
        connection = timeout(SERVER_REQUEST_TIMEOUT, TcpStream::connect(origin_address)) => {
            match connection {
                Ok(Ok(stream)) => stream,
                Ok(Err(error)) => return Err(box_error(error)),
                Err(_) => return Err(test_error("loopback proxy forwarding connection exceeded its bounded wait")),
            }
        }
    };
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {PROXY_TARGET_HOST}\r\nX-TermGlide-Route: {PROXY_ROUTE_VALUE}\r\nConnection: close\r\n\r\n"
    );
    tokio::select! {
        () = cancellation.cancelled() => return Ok(None),
        write = timeout(SERVER_REQUEST_TIMEOUT, origin.write_all(request.as_bytes())) => {
            match write {
                Ok(Ok(())) => {},
                Ok(Err(error)) => return Err(box_error(error)),
                Err(_) => return Err(test_error("loopback proxy forwarding write exceeded its bounded wait")),
            }
        }
    }
    origin.shutdown().await.map_err(box_error)?;
    let mut response = Vec::new();
    tokio::select! {
        () = cancellation.cancelled() => return Ok(None),
        read = timeout(SERVER_REQUEST_TIMEOUT, origin.read_to_end(&mut response)) => {
            match read {
                Ok(Ok(_)) => {},
                Ok(Err(error)) => return Err(box_error(error)),
                Err(_) => return Err(test_error("loopback proxy forwarding read exceeded its bounded wait")),
            }
        }
    }
    if response.len() > MAX_RESPONSE_BYTES {
        return Err(test_error(format!(
            "loopback proxy forwarded response exceeded {MAX_RESPONSE_BYTES} bytes"
        )));
    }
    Ok(Some(response))
}

async fn read_bounded_request(
    stream: &mut TcpStream,
    cancellation: &Cancellation,
) -> TestResult<Option<HttpRequest>> {
    let header = tokio::select! {
        () = cancellation.cancelled() => return Ok(None),
        read = timeout(SERVER_REQUEST_TIMEOUT, read_http_header(stream)) => {
            match read {
                Ok(result) => result?,
                Err(_) => return Err(test_error("loopback fixture request input exceeded its bounded wait")),
            }
        }
    };
    let Some(header) = header else {
        return Ok(None);
    };
    Ok(Some(parse_http_request(&header)?))
}

async fn write_bounded_response(
    stream: &mut TcpStream,
    response: &[u8],
    cancellation: &Cancellation,
) -> TestResult {
    tokio::select! {
        () = cancellation.cancelled() => Ok(()),
        write = timeout(SERVER_REQUEST_TIMEOUT, write_http_response(stream, response)) => {
            match write {
                Ok(result) => result,
                Err(_) => Err(test_error("loopback fixture response output exceeded its bounded wait")),
            }
        }
    }
}

async fn read_http_header(stream: &mut TcpStream) -> TestResult<Option<String>> {
    let mut buffer = [0_u8; MAX_REQUEST_BYTES];
    let mut used = 0usize;
    loop {
        if used == buffer.len() {
            return Err(test_error(format!(
                "loopback fixture request exceeded {MAX_REQUEST_BYTES} bytes"
            )));
        }
        let read = match timeout(
            REQUEST_HEADER_IDLE_TIMEOUT,
            stream.read(&mut buffer[used..]),
        )
        .await
        {
            Ok(result) => result.map_err(box_error)?,
            Err(_) if used == 0 => return Ok(None),
            Err(_) => {
                return Err(test_error(
                    "loopback fixture request header stalled after partial input",
                ));
            }
        };
        if read == 0 {
            if used == 0 {
                return Ok(None);
            }
            return Err(test_error(
                "loopback fixture peer closed during a partial request header",
            ));
        }
        used = match used.checked_add(read) {
            Some(value) => value,
            None => return Err(test_error("loopback fixture request length overflowed")),
        };
        let Some(end) = find_header_end(&buffer[..used]) else {
            continue;
        };
        let header = std::str::from_utf8(&buffer[..end]).map_err(box_error)?;
        return Ok(Some(header.to_owned()));
    }
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
}

impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

fn parse_http_request(header: &str) -> TestResult<HttpRequest> {
    let mut lines = header.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| test_error("loopback fixture request omitted a request line"))?;
    let mut fields = request_line.split_whitespace();
    let method = fields
        .next()
        .ok_or_else(|| test_error("loopback fixture request omitted an HTTP method"))?;
    let target = fields
        .next()
        .ok_or_else(|| test_error("loopback fixture request omitted an HTTP target"))?;
    let version = fields
        .next()
        .ok_or_else(|| test_error("loopback fixture request omitted an HTTP version"))?;
    if fields.next().is_some() {
        return Err(test_error(
            "loopback fixture request line contained extra fields",
        ));
    }
    if !matches!(version, "HTTP/1.0" | "HTTP/1.1") {
        return Err(test_error(
            "loopback fixture request used an unsupported HTTP version",
        ));
    }

    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(test_error("loopback fixture request header was malformed"));
        };
        let name = name.trim();
        let value = value.trim();
        if name.is_empty() || name.bytes().any(|byte| byte.is_ascii_control()) {
            return Err(test_error(
                "loopback fixture request header name was malformed",
            ));
        }
        headers.push((name.to_owned(), value.to_owned()));
    }
    Ok(HttpRequest {
        method: method.to_owned(),
        target: target.to_owned(),
        headers,
    })
}

fn origin_form_path(target: &str) -> TestResult<String> {
    let path = target.split_once('?').map_or(target, |(path, _)| path);
    if !path.starts_with('/') || path.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(test_error(
            "loopback origin request target was not an origin-form path",
        ));
    }
    Ok(path.to_owned())
}

fn http_response(status: &str, body: &str) -> TestResult<Vec<u8>> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    if response.len() > MAX_RESPONSE_BYTES {
        return Err(test_error(format!(
            "loopback fixture response exceeded {MAX_RESPONSE_BYTES} bytes"
        )));
    }
    Ok(response.into_bytes())
}

async fn write_http_response(stream: &mut TcpStream, response: &[u8]) -> TestResult {
    stream.write_all(response).await.map_err(box_error)?;
    stream.shutdown().await.map_err(box_error)
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
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
                "termglide-external-proxy-boundary-{timestamp}-{counter}"
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
            "could not allocate an external proxy-boundary test root",
        ))
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn verify_empty_and_remove(&mut self) -> io::Result<()> {
        let mut entries = fs::read_dir(&self.path)?;
        if entries.next().is_some() {
            return Err(io::Error::other(format!(
                "proxy-boundary test root retained files after cleanup: {}",
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
