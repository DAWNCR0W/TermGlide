//! Real-Chrome coverage for version metadata and the bounded CDP command surface.

use std::error::Error;
use std::fs;
use std::io::{self, ErrorKind};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tg_browser::{
    DevToolsEndpoint, ExternalEngineError, ExternalEngineLaunchOptions, ExternalEngineProcess,
    ExternalEngineViewport, discover_external_engine, launch_external_engine,
};
use tg_core::Cancellation;
use tg_network::{
    CdpBrowserVersion, CdpError, CdpEvaluation, CdpLimits, CdpSession, CdpTargetSession,
};
use url::Url;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

const FIXTURE_URL: &str = "data:text/html,%3C!doctype%20html%3E%3Cmeta%20charset=utf-8%3E%3Cstyle%3Ebody%7Bmargin:0%7D%23entry%7Bposition:fixed;left:8px;top:8px;width:160px;height:28px%7D%23commit%7Bposition:fixed;left:8px;top:48px;width:100px;height:28px%7D%3C/style%3E%3Cinput%20id=entry%20aria-label=entry%3E%3Cbutton%20id=commit%20type=button%3Ecommit%3C/button%3E%3Coutput%20id=state%3Eidle%3C/output%3E%3Cscript%3Edocument.getElementById(%22commit%22).addEventListener(%22click%22,()%3D%3E%7Bdocument.getElementById(%22state%22).textContent%3Ddocument.getElementById(%22entry%22).value%7D)%3C/script%3E";
const INPUT_TEXT: &str = "termglide";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(25);
const CDP_OPERATION_TIMEOUT: Duration = Duration::from_secs(5);
const ROOT_ALLOCATION_ATTEMPTS: u8 = 32;

static ROOT_COUNTER: AtomicU64 = AtomicU64::new(0);

#[tokio::test]
async fn external_version_protocol_chrome() -> TestResult {
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
    let operation = exercise_version_protocol_contract(&executable, &root).await;
    let root_cleanup = root.verify_empty_and_remove().map_err(box_error);
    preserve_results(operation, root_cleanup)
}

async fn exercise_version_protocol_contract(executable: &Path, root: &TestRoot) -> TestResult {
    let profile_parent = root.path().join("profiles");
    let options = launch_options(executable, &profile_parent)?;
    let cancellation = Cancellation::new();
    let mut process = match launch_external_engine(&options, &cancellation) {
        Ok(process) => process,
        Err(error) => {
            let parent_cleanup = remove_empty_directory_if_present(&profile_parent);
            return preserve_results(Err(box_error(error)), parent_cleanup);
        }
    };
    let profile = process.isolated_profile_dir().to_path_buf();
    let endpoint = process.endpoint().websocket_url();

    let preconditions =
        require_live_loopback_process(&process, executable, &profile_parent, &profile);
    let protocol_flow = match preconditions {
        Ok(()) => exercise_version_and_protocol_surface(&endpoint).await,
        Err(error) => Err(error),
    };
    let liveness = require_process_running(&mut process);
    let process_cleanup = shutdown_and_verify(&mut process, &profile);
    drop(process);
    let parent_cleanup = remove_empty_directory_if_present(&profile_parent);

    preserve_results(
        preserve_results(protocol_flow, liveness),
        preserve_results(process_cleanup, parent_cleanup),
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

async fn exercise_version_and_protocol_surface(endpoint: &str) -> TestResult {
    let limits = bounded_cdp_limits();
    let max_metadata_bytes = limits.max_message_bytes;
    let mut connection = CdpSession::connect(endpoint, limits)
        .await
        .map_err(box_error)?;
    let flow: TestResult = async {
        let version = connection.browser_version().await.map_err(box_error)?;
        require_browser_version_contract(&version, max_metadata_bytes)?;

        let targets = connection
            .send("Target.getTargets", json!({}))
            .await
            .map_err(box_error)?;
        require_page_target_inventory(&targets.result)?;

        let mut target = connection.attach_first_page().await.map_err(box_error)?;
        exercise_target_page_runtime_input_accessibility(&mut target).await
    }
    .await;
    let close = connection.close().await.map_err(box_error);
    let disconnected = if close.is_ok() && !connection.is_connected() {
        Ok(())
    } else if close.is_ok() {
        Err(test_error(
            "explicit CDP close left the browser protocol connection live",
        ))
    } else {
        Ok(())
    };
    preserve_results(flow, preserve_results(close, disconnected))
}

fn require_browser_version_contract(
    version: &CdpBrowserVersion,
    max_metadata_bytes: usize,
) -> TestResult {
    for (field, value) in [
        ("product", version.product.as_str()),
        ("protocolVersion", version.protocol_version.as_str()),
        ("revision", version.revision.as_str()),
        ("userAgent", version.user_agent.as_str()),
        ("jsVersion", version.js_version.as_str()),
    ] {
        require_bounded_metadata_field(field, value, max_metadata_bytes)?;
    }

    let Some((product_name, product_version)) = version.product.split_once('/') else {
        return Err(test_error(
            "Browser.getVersion product did not contain a product/version boundary",
        ));
    };
    if product_name.is_empty() || product_version.is_empty() {
        return Err(test_error(
            "Browser.getVersion product contained an empty product/version component",
        ));
    }
    require_decimal_version_structure("protocolVersion", &version.protocol_version)?;
    require_decimal_version_structure("product version", product_version)?;
    require_decimal_version_structure("jsVersion", &version.js_version)?;
    if !version.user_agent.starts_with("Mozilla/") {
        return Err(test_error(
            "Browser.getVersion userAgent did not retain a browser user-agent structure",
        ));
    }
    if !version
        .revision
        .chars()
        .any(|character| character.is_ascii_alphanumeric())
    {
        return Err(test_error(
            "Browser.getVersion revision did not contain an identifier character",
        ));
    }
    Ok(())
}

fn require_bounded_metadata_field(field: &str, value: &str, max_bytes: usize) -> TestResult {
    if value.is_empty() {
        return Err(test_error(format!("Browser.getVersion {field} was empty")));
    }
    if value.len() > max_bytes {
        return Err(test_error(format!(
            "Browser.getVersion {field} exceeded the configured byte bound"
        )));
    }
    if value.chars().any(char::is_control) {
        return Err(test_error(format!(
            "Browser.getVersion {field} contained a control character"
        )));
    }
    Ok(())
}

fn require_decimal_version_structure(field: &str, value: &str) -> TestResult {
    let components = value.split('.').collect::<Vec<_>>();
    if components.len() < 2
        || components.iter().any(|component| {
            component.is_empty() || !component.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return Err(test_error(format!(
            "Browser.getVersion {field} was not a dotted decimal version",
        )));
    }
    Ok(())
}

fn require_page_target_inventory(result: &Value) -> TestResult {
    let targets = result
        .get("targetInfos")
        .and_then(Value::as_array)
        .ok_or_else(|| test_error("Target.getTargets omitted targetInfos"))?;
    if targets
        .iter()
        .any(|target| target.get("type").and_then(Value::as_str) == Some("page"))
    {
        Ok(())
    } else {
        Err(test_error(
            "Target.getTargets did not expose a page target for the local browser",
        ))
    }
}

async fn exercise_target_page_runtime_input_accessibility(
    target: &mut CdpTargetSession<'_>,
) -> TestResult {
    target.page_enable().await.map_err(box_error)?;
    let fixture = Url::parse(FIXTURE_URL).map_err(box_error)?;
    let navigation = target.navigate(fixture.as_str()).await.map_err(box_error)?;
    if let Some(message) = navigation.error_text {
        return Err(test_error(format!(
            "local data fixture navigation returned a protocol error: {message}"
        )));
    }
    target.wait_for_load().await.map_err(box_error)?;

    let readiness = target
        .runtime_evaluate("document.getElementById('entry') !== null", true)
        .await
        .map_err(box_error)?;
    if require_runtime_value(&readiness, "fixture readiness")?.as_bool() != Some(true) {
        return Err(test_error(
            "Runtime.evaluate did not expose the local input fixture",
        ));
    }

    let focus = target
        .runtime_evaluate("document.getElementById('entry').focus()", false)
        .await
        .map_err(box_error)?;
    require_no_runtime_exception(&focus, "fixture focus")?;
    target.key_down("Shift").await.map_err(box_error)?;
    target.key_up("Shift").await.map_err(box_error)?;
    target.insert_text(INPUT_TEXT).await.map_err(box_error)?;
    target.mouse_click(50.0, 62.0).await.map_err(box_error)?;

    let state = target
        .runtime_evaluate(
            "({value:document.getElementById('entry').value,state:document.getElementById('state').textContent})",
            true,
        )
        .await
        .map_err(box_error)?;
    require_no_runtime_exception(&state, "Input command state")?;
    if state.result.pointer("/value/value").and_then(Value::as_str) != Some(INPUT_TEXT) {
        return Err(test_error(
            "Input commands did not publish text into the local fixture",
        ));
    }
    if state.result.pointer("/value/state").and_then(Value::as_str) != Some(INPUT_TEXT) {
        return Err(test_error(
            "Input mouse command did not activate the local fixture button",
        ));
    }

    target
        .send("Accessibility.enable", json!({}))
        .await
        .map_err(box_error)?;
    let accessibility_tree = target
        .send("Accessibility.getFullAXTree", json!({}))
        .await
        .map_err(box_error)?;
    require_accessibility_tree(&accessibility_tree.result)?;

    let malformed = target
        .send("Runtime.evaluate", json!({ "expression": 17 }))
        .await
        .map(|_| ());
    require_typed_protocol_error(malformed, "malformed Runtime.evaluate")?;
    let unsupported = target
        .send("TermGlide.ProtocolSmokeUnsupported", json!({}))
        .await
        .map(|_| ());
    require_typed_protocol_error(unsupported, "unsupported protocol command")?;

    let recovery = target
        .runtime_evaluate("1 + 1", true)
        .await
        .map_err(box_error)?;
    if require_runtime_value(&recovery, "post-error Runtime.evaluate")?.as_i64() != Some(2) {
        return Err(test_error(
            "CDP session did not remain usable after typed protocol errors",
        ));
    }
    Ok(())
}

fn require_no_runtime_exception(evaluation: &CdpEvaluation, operation: &str) -> TestResult {
    if evaluation.exception_details.is_some() {
        Err(test_error(format!(
            "{operation} returned browser exception details",
        )))
    } else {
        Ok(())
    }
}

fn require_runtime_value<'a>(
    evaluation: &'a CdpEvaluation,
    operation: &str,
) -> TestResult<&'a Value> {
    require_no_runtime_exception(evaluation, operation)?;
    evaluation
        .result
        .get("value")
        .ok_or_else(|| test_error(format!("{operation} omitted a by-value result")))
}

fn require_accessibility_tree(result: &Value) -> TestResult {
    let nodes = result
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| test_error("Accessibility.getFullAXTree omitted nodes"))?;
    if nodes.is_empty() || !nodes.iter().all(Value::is_object) {
        return Err(test_error(
            "Accessibility.getFullAXTree did not return object nodes for the local fixture",
        ));
    }
    Ok(())
}

fn require_typed_protocol_error(result: Result<(), CdpError>, operation: &str) -> TestResult {
    match result {
        Err(CdpError::Protocol(error)) => {
            if error.code == 0
                || error.message.is_empty()
                || error.message.chars().any(char::is_control)
            {
                return Err(test_error(format!(
                    "{operation} returned an incomplete typed protocol error",
                )));
            }
            Ok(())
        }
        Err(error) => Err(test_error(format!(
            "{operation} returned a non-protocol CDP error: {error}"
        ))),
        Ok(()) => Err(test_error(format!(
            "{operation} unexpectedly succeeded against the local browser",
        ))),
    }
}

fn require_process_running(process: &mut ExternalEngineProcess) -> TestResult {
    match process.try_wait().map_err(box_error)? {
        None => Ok(()),
        Some(status) => Err(test_error(format!(
            "browser child exited before version/protocol cleanup: {status:?}"
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
                "termglide-external-version-protocol-{timestamp}-{counter}"
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
            "could not allocate an external version/protocol test root",
        ))
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn verify_empty_and_remove(&mut self) -> io::Result<()> {
        let mut entries = fs::read_dir(&self.path)?;
        if entries.next().is_some() {
            return Err(io::Error::other(format!(
                "version/protocol test root retained files after cleanup: {}",
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
