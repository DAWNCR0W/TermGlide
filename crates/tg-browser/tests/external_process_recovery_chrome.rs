#![cfg(unix)]

//! Real-Chrome recovery coverage for a supervised external browser child.

use std::error::Error;
use std::fs;
use std::io::{self, ErrorKind};
use std::net::Ipv4Addr;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tg_browser::{
    ExternalEngineError, ExternalEngineLaunchOptions, ExternalEngineProcess,
    ExternalEngineViewport, discover_external_engine, launch_external_engine,
};
use tg_core::Cancellation;
use tg_network::{CdpLimits, CdpSession};
use url::Url;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

const GENERIC_DATA_URL: &str =
    "data:text/html,%3C!doctype%20html%3E%3Cmain%3Eprocess-recovery%3C%2Fmain%3E";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(25);
const CDP_OPERATION_TIMEOUT: Duration = Duration::from_secs(5);
const EXIT_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const EXIT_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(10);
const ROOT_ALLOCATION_ATTEMPTS: u8 = 32;
const KILL_SIGNAL: i32 = 9;

static ROOT_COUNTER: AtomicU64 = AtomicU64::new(0);

#[tokio::test]
async fn external_process_recovery_chrome() -> TestResult {
    let mut root = TestRoot::new().map_err(box_error)?;
    let operation = exercise_process_recovery(&root).await;
    let root_cleanup = root.verify_empty_and_remove().map_err(box_error);
    preserve_results(operation, root_cleanup)
}

async fn exercise_process_recovery(root: &TestRoot) -> TestResult {
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

    let profile_parent = root.path().join("profiles");
    let options = launch_options(executable.as_path(), &profile_parent)?;
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

    let first_preconditions = require_live_loopback_process(
        &first,
        executable.as_path(),
        &profile_parent,
        &first_profile,
    );
    let first_readiness = match first_preconditions {
        Ok(()) => exercise_cdp_readiness(&first_endpoint).await,
        Err(error) => Err(error),
    };
    let forced_exit = force_kill_and_reap(&mut first, &first_profile);
    let stale_endpoint = if forced_exit.is_ok() {
        require_stale_endpoint_rejected(&first_endpoint).await
    } else {
        Ok(())
    };
    drop(first);

    let first_result = preserve_results(
        first_readiness,
        preserve_results(forced_exit, stale_endpoint),
    );
    let relaunch = exercise_clean_relaunch(
        &options,
        &cancellation,
        executable.as_path(),
        &profile_parent,
        &first_profile,
    )
    .await;
    let parent_cleanup = remove_empty_directory_if_present(&profile_parent);

    preserve_results(preserve_results(first_result, relaunch), parent_cleanup)
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
            "external profile was not a dedicated child of the selected parent: {}",
            profile.display()
        )));
    }

    let endpoint = process.endpoint();
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

async fn exercise_cdp_readiness(endpoint: &str) -> TestResult {
    let mut connection = CdpSession::connect(endpoint, bounded_cdp_limits())
        .await
        .map_err(box_error)?;
    let flow: TestResult = async {
        let mut target = connection.attach_first_page().await.map_err(box_error)?;
        target.page_enable().await.map_err(box_error)?;
        let navigation = target.navigate(GENERIC_DATA_URL).await.map_err(box_error)?;
        if let Some(message) = navigation.error_text {
            return Err(test_error(format!(
                "generic data navigation through DevTools failed: {message}"
            )));
        }
        target.wait_for_load().await.map_err(box_error)?;
        Ok(())
    }
    .await;
    let close = connection.close().await.map_err(box_error);
    preserve_results(flow, close)
}

fn force_kill_and_reap(process: &mut ExternalEngineProcess, profile: &Path) -> TestResult {
    let kill = force_kill_process(process.process_id());
    let observed_exit = if kill.is_ok() {
        wait_for_killed_child(process)
    } else {
        Ok(())
    };
    let cleanup = shutdown_and_verify(process, profile);
    preserve_results(kill, preserve_results(observed_exit, cleanup))
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

async fn exercise_clean_relaunch(
    options: &ExternalEngineLaunchOptions,
    cancellation: &Cancellation,
    executable: &Path,
    profile_parent: &Path,
    stale_profile: &Path,
) -> TestResult {
    let mut replacement = launch_external_engine(options, cancellation).map_err(box_error)?;
    let replacement_profile = replacement.isolated_profile_dir().to_path_buf();
    let replacement_endpoint = replacement.endpoint().websocket_url();

    let profile_freshness = if replacement_profile == stale_profile {
        Err(test_error(
            "replacement browser reused the terminated browser profile path",
        ))
    } else {
        require_path_absent(stale_profile, "terminated browser profile was reused")
    };
    let replacement_preconditions = preserve_results(
        require_live_loopback_process(
            &replacement,
            executable,
            profile_parent,
            &replacement_profile,
        ),
        profile_freshness,
    );
    let replacement_readiness = match replacement_preconditions {
        Ok(()) => exercise_cdp_readiness(&replacement_endpoint).await,
        Err(error) => Err(error),
    };
    let replacement_liveness = require_process_running(&mut replacement);
    let replacement_cleanup = shutdown_and_verify(&mut replacement, &replacement_profile);
    drop(replacement);

    preserve_results(
        preserve_results(replacement_readiness, replacement_liveness),
        replacement_cleanup,
    )
}

fn require_process_running(process: &mut ExternalEngineProcess) -> TestResult {
    match process.try_wait().map_err(box_error)? {
        None => Ok(()),
        Some(status) => Err(test_error(format!(
            "browser child exited before clean relaunch verification: {status:?}"
        ))),
    }
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
                "termglide-external-process-recovery-{timestamp}-{counter}"
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
            "could not allocate an external process recovery test root",
        ))
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn verify_empty_and_remove(&mut self) -> io::Result<()> {
        let mut entries = fs::read_dir(&self.path)?;
        if entries.next().is_some() {
            return Err(io::Error::other(format!(
                "test root retained files after process recovery cleanup: {}",
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
