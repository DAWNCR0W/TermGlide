//! Real-Chrome checks for the externally visible CDP endpoint and profile lifecycle boundaries.

use std::error::Error;
use std::fs;
use std::io::{self, ErrorKind};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tg_browser::{
    DevToolsEndpoint, ExternalEngineError, ExternalEngineLaunchOptions, ExternalEngineProcess,
    ExternalEngineViewport, discover_external_engine, launch_external_engine,
};
use tg_core::Cancellation;
use tg_network::{CdpError, CdpLimits, CdpSession};
use url::Url;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

const GENERIC_DATA_URL: &str =
    "data:text/html,%3C!doctype%20html%3E%3Cmain%3Edebug-endpoint%3C%2Fmain%3E";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(25);
const CDP_OPERATION_TIMEOUT: Duration = Duration::from_secs(5);
const ROOT_ALLOCATION_ATTEMPTS: u8 = 32;
const REFUSED_NON_LOOPBACK_HOST: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 1);

static ROOT_COUNTER: AtomicU64 = AtomicU64::new(0);

#[tokio::test]
async fn external_debug_endpoint_security_chrome() -> TestResult {
    let mut root = TestRoot::new().map_err(box_error)?;
    let operation = exercise_debug_endpoint_boundaries(&root).await;
    let root_cleanup = root.verify_empty_and_remove().map_err(box_error);
    preserve_results(operation, root_cleanup)
}

async fn exercise_debug_endpoint_boundaries(root: &TestRoot) -> TestResult {
    let invalid_executable = require_invalid_executable_refusal(root);
    let pre_cancelled = require_pre_cancelled_refusal(root);

    let selected_executable = match discover_external_engine(None) {
        Ok(path) => Some(path),
        Err(ExternalEngineError::ExecutableNotFound) => None,
        Err(error) => return Err(box_error(error)),
    };
    let live_launch = match selected_executable {
        Some(executable) if executable.is_file() => {
            exercise_live_loopback_endpoint(root, executable.as_path()).await
        }
        Some(_) => Err(test_error("discovered executable is not a regular file")),
        None => Ok(()),
    };

    preserve_results(
        preserve_results(invalid_executable, pre_cancelled),
        live_launch,
    )
}

fn require_invalid_executable_refusal(root: &TestRoot) -> TestResult {
    let profile_parent = root.path().join("invalid-executable-profile-parent");
    let missing_executable = root.path().join("missing-chromium");
    let options = launch_options(&missing_executable, &profile_parent)?;
    let cancellation = Cancellation::new();

    let result = match launch_external_engine(&options, &cancellation) {
        Err(ExternalEngineError::InvalidExecutableOverride { path })
            if path == missing_executable =>
        {
            Ok(())
        }
        Err(error) => Err(test_error(format!(
            "missing executable returned an unexpected launch error: {error}"
        ))),
        Ok(process) => reject_unexpected_process(
            process,
            &profile_parent,
            "missing executable unexpectedly produced a supervised browser process",
        ),
    };
    preserve_results(
        result,
        require_path_absent(
            &profile_parent,
            "invalid executable launch created a temporary profile parent",
        ),
    )
}

fn require_pre_cancelled_refusal(root: &TestRoot) -> TestResult {
    let profile_parent = root.path().join("pre-cancelled-profile-parent");
    let unused_executable = root.path().join("unused-pre-cancelled-chromium");
    let options = launch_options(&unused_executable, &profile_parent)?;
    let cancellation = Cancellation::new();
    cancellation.cancel();

    let result = match launch_external_engine(&options, &cancellation) {
        Err(ExternalEngineError::Cancelled) => Ok(()),
        Err(error) => Err(test_error(format!(
            "pre-cancelled launch returned an unexpected error: {error}"
        ))),
        Ok(process) => reject_unexpected_process(
            process,
            &profile_parent,
            "pre-cancelled launch unexpectedly produced a supervised browser process",
        ),
    };
    preserve_results(
        result,
        require_path_absent(
            &profile_parent,
            "pre-cancelled launch created a temporary profile parent",
        ),
    )
}

async fn exercise_live_loopback_endpoint(root: &TestRoot, executable: &Path) -> TestResult {
    let profile_parent = root.path().join("live-profile-parent");
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
    let endpoint = process.endpoint().clone();
    let endpoint_url = endpoint.websocket_url();

    let preconditions = require_live_endpoint_and_profile(
        &process,
        executable,
        &profile_parent,
        &profile,
        &endpoint,
    );
    let cdp_flow = match preconditions {
        Ok(()) => exercise_cdp_boundary(&endpoint, &endpoint_url).await,
        Err(error) => Err(error),
    };
    let liveness = require_process_running(&mut process);
    let process_cleanup = shutdown_and_verify(process, &profile);
    let parent_cleanup = remove_empty_directory_if_present(&profile_parent);

    preserve_results(
        preserve_results(cdp_flow, liveness),
        preserve_results(process_cleanup, parent_cleanup),
    )
}

fn launch_options(
    executable: &Path,
    profile_parent: &Path,
) -> TestResult<ExternalEngineLaunchOptions> {
    let initial_target = Url::parse(GENERIC_DATA_URL).map_err(box_error)?;
    let mut options = ExternalEngineLaunchOptions::new(initial_target);
    options.executable_override = Some(executable.to_path_buf());
    options.viewport = Some(ExternalEngineViewport::new(640, 480).map_err(box_error)?);
    options.startup_timeout = STARTUP_TIMEOUT;
    options.poll_interval = POLL_INTERVAL;
    options.temporary_profile_parent = Some(profile_parent.to_path_buf());
    Ok(options)
}

fn require_live_endpoint_and_profile(
    process: &ExternalEngineProcess,
    executable: &Path,
    profile_parent: &Path,
    profile: &Path,
    endpoint: &DevToolsEndpoint,
) -> TestResult {
    if process.executable_path() != executable {
        return Err(test_error(
            "external process executable provenance did not match the selected executable",
        ));
    }
    if profile.parent() != Some(profile_parent) || profile == profile_parent || !profile.is_dir() {
        return Err(test_error(format!(
            "isolated profile was not a dedicated child of the caller-selected root: {}",
            profile.display()
        )));
    }
    require_owner_only_profile_permissions(profile)?;

    if endpoint.host != Ipv4Addr::LOCALHOST || endpoint.port == 0 {
        return Err(test_error(
            "published DevTools endpoint was not a non-zero IPv4 loopback endpoint",
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
            "published DevTools websocket URL was not a credential-free IPv4 loopback URL",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn require_owner_only_profile_permissions(profile: &Path) -> TestResult {
    use std::os::unix::fs::PermissionsExt;

    let mode = fs::metadata(profile)
        .map_err(box_error)?
        .permissions()
        .mode();
    if mode & 0o077 == 0 {
        Ok(())
    } else {
        Err(test_error(format!(
            "isolated profile grants group or other permissions: {:o}",
            mode & 0o777
        )))
    }
}

#[cfg(not(unix))]
fn require_owner_only_profile_permissions(_profile: &Path) -> TestResult {
    Ok(())
}

async fn exercise_cdp_boundary(endpoint: &DevToolsEndpoint, endpoint_url: &str) -> TestResult {
    require_rejected_cdp_endpoint(
        format!(
            "ws://test-user:test-secret@{}:{}{}",
            endpoint.host, endpoint.port, endpoint.websocket_path
        ),
        "credentialed endpoint",
    )
    .await?;
    require_rejected_cdp_endpoint(
        format!("ws://{REFUSED_NON_LOOPBACK_HOST}:9/devtools/browser/rejected-endpoint"),
        "non-loopback endpoint",
    )
    .await?;

    let mut connection = CdpSession::connect(endpoint_url, bounded_cdp_limits())
        .await
        .map_err(box_error)?;
    let flow: TestResult = async {
        let mut target = connection.attach_first_page().await.map_err(box_error)?;
        target.page_enable().await.map_err(box_error)?;
        let navigation = target.navigate(GENERIC_DATA_URL).await.map_err(box_error)?;
        if let Some(message) = navigation.error_text {
            return Err(test_error(format!(
                "generic data navigation through the loopback endpoint failed: {message}"
            )));
        }
        target.wait_for_load().await.map_err(box_error)?;
        Ok(())
    }
    .await;
    let close = connection.close().await.map_err(box_error);
    preserve_results(flow, close)
}

async fn require_rejected_cdp_endpoint(endpoint: String, label: &str) -> TestResult {
    match CdpSession::connect(&endpoint, bounded_cdp_limits()).await {
        Err(CdpError::InvalidEndpoint) => Ok(()),
        Err(error) => Err(test_error(format!(
            "{label} returned an unexpected CDP error: {error}"
        ))),
        Ok(mut connection) => {
            let close = connection.close().await.map_err(box_error);
            preserve_results(
                Err(test_error(format!("{label} was unexpectedly accepted"))),
                close,
            )
        }
    }
}

fn bounded_cdp_limits() -> CdpLimits {
    CdpLimits {
        operation_timeout: CDP_OPERATION_TIMEOUT,
        ..CdpLimits::default()
    }
}

fn require_process_running(process: &mut ExternalEngineProcess) -> TestResult {
    if process.process_id() == 0 {
        return Err(test_error(
            "external process did not expose an OS process identifier",
        ));
    }
    match process.try_wait().map_err(box_error)? {
        None => Ok(()),
        Some(status) => Err(test_error(format!(
            "external browser exited before supervised shutdown: {status:?}"
        ))),
    }
}

fn shutdown_and_verify(mut process: ExternalEngineProcess, profile: &Path) -> TestResult {
    let shutdown = process.shutdown().map_err(box_error);
    let reaped = if shutdown.is_ok() {
        match process.try_wait().map_err(box_error) {
            Ok(Some(_)) => Ok(()),
            Ok(None) => Err(test_error(
                "external browser child remained running after supervised shutdown",
            )),
            Err(error) => Err(error),
        }
    } else {
        Ok(())
    };
    drop(process);
    preserve_results(
        shutdown,
        preserve_results(
            reaped,
            require_path_absent(profile, "external browser profile remained"),
        ),
    )
}

fn reject_unexpected_process(
    process: ExternalEngineProcess,
    profile_parent: &Path,
    message: &str,
) -> TestResult {
    let profile = process.isolated_profile_dir().to_path_buf();
    let cleanup = shutdown_and_verify(process, &profile);
    let parent_cleanup = remove_empty_directory_if_present(profile_parent);
    preserve_results(
        Err(test_error(message)),
        preserve_results(cleanup, parent_cleanup),
    )
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
            "temporary profile parent was not empty after cleanup: {}",
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
                "termglide-external-debug-endpoint-{timestamp}-{counter}"
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
            "could not allocate an external-debug-endpoint test root",
        ))
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn verify_empty_and_remove(&mut self) -> io::Result<()> {
        let mut entries = fs::read_dir(&self.path)?;
        if entries.next().is_some() {
            return Err(io::Error::other(format!(
                "test root retained files after cleanup: {}",
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
