#![cfg(unix)]

use std::env;
use std::error::Error;
use std::fs;
use std::io::{self, ErrorKind};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tg_browser::{
    ExternalEngineError, ExternalEngineLaunchOptions, ExternalEngineProcess, launch_external_engine,
};
use tg_core::Cancellation;
use url::Url;

const EXIT_BEFORE_DEVTOOLS: i32 = 23;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(2);
const CANCELLATION_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(5);
const RECORD_TIMEOUT: Duration = Duration::from_secs(2);
const FIXTURE_MODE: u32 = 0o700;
const ROOT_ALLOCATION_ATTEMPTS: u8 = 32;

static ROOT_COUNTER: AtomicU64 = AtomicU64::new(0);

#[test]
fn launcher_failure_boundaries_reap_direct_fixtures_and_clean_profiles()
-> Result<(), Box<dyn Error>> {
    let root = TestRoot::new()?;
    let profile_parent = root.path().join("profiles");
    let exited = ShellFixture::new(
        &root,
        "exits-before-devtools",
        &format!("exit {EXIT_BEFORE_DEVTOOLS}"),
    )?;
    let exited_error = launch_failure(
        &exited,
        &profile_parent,
        STARTUP_TIMEOUT,
        &Cancellation::new(),
    )?;
    assert_process_exit(exited_error, EXIT_BEFORE_DEVTOOLS)?;
    let exited_profile = exited.recorded_profile()?;
    assert_profile_removed(&exited_profile, &profile_parent);

    let timeout = ShellFixture::new(&root, "startup-timeout", "exec /bin/sleep 60")?;
    let timeout_error = launch_failure(
        &timeout,
        &profile_parent,
        STARTUP_TIMEOUT,
        &Cancellation::new(),
    )?;
    assert_startup_timeout(timeout_error, STARTUP_TIMEOUT)?;
    let timeout_profile = timeout.recorded_profile()?;
    assert_profile_removed(&timeout_profile, &profile_parent);

    let cancelled = ShellFixture::new(&root, "startup-cancelled", "exec /bin/sleep 60")?;
    let cancellation = Cancellation::new();
    let cancellation_attempt = LaunchAttempt::start(
        launch_options(&cancelled, &profile_parent, CANCELLATION_TIMEOUT)?,
        cancellation.clone(),
    );
    cancelled.wait_for_records(RECORD_TIMEOUT)?;
    cancellation.cancel();
    let cancellation_error = cancellation_attempt.finish()?;
    assert_cancelled(cancellation_error)?;
    let cancellation_profile = cancelled.recorded_profile()?;
    assert_profile_removed(&cancellation_profile, &profile_parent);

    let profiles = [exited_profile, timeout_profile, cancellation_profile];
    assert_unique_profiles(&profiles);
    assert_profile_parent_empty(&profile_parent)?;
    Ok(())
}

fn launch_failure(
    fixture: &ShellFixture,
    profile_parent: &Path,
    startup_timeout: Duration,
    cancellation: &Cancellation,
) -> Result<ExternalEngineError, Box<dyn Error>> {
    launch_result_error(launch_external_engine(
        &launch_options(fixture, profile_parent, startup_timeout)?,
        cancellation,
    ))
}

fn launch_options(
    fixture: &ShellFixture,
    profile_parent: &Path,
    startup_timeout: Duration,
) -> Result<ExternalEngineLaunchOptions, Box<dyn Error>> {
    let mut options = ExternalEngineLaunchOptions::new(Url::parse("about:blank")?);
    options.executable_override = Some(fixture.executable().to_path_buf());
    options.startup_timeout = startup_timeout;
    options.poll_interval = POLL_INTERVAL;
    options.temporary_profile_parent = Some(profile_parent.to_path_buf());
    Ok(options)
}

fn launch_result_error(
    result: Result<ExternalEngineProcess, ExternalEngineError>,
) -> Result<ExternalEngineError, Box<dyn Error>> {
    match result {
        Err(error) => Ok(error),
        Ok(mut process) => {
            let profile_dir = process.isolated_profile_dir().to_path_buf();
            let shutdown = process.shutdown();
            drop(process);
            shutdown?;
            Err(io::Error::other(format!(
                "fixture unexpectedly became ready with profile {}",
                profile_dir.display()
            ))
            .into())
        }
    }
}

fn assert_process_exit(
    error: ExternalEngineError,
    expected_code: i32,
) -> Result<(), Box<dyn Error>> {
    match error {
        ExternalEngineError::ProcessExited { status } => {
            assert_eq!(status.code(), Some(expected_code));
            Ok(())
        }
        error => Err(io::Error::other(format!(
            "fixture returned unexpected launch error: {error}"
        ))
        .into()),
    }
}

fn assert_startup_timeout(
    error: ExternalEngineError,
    expected_timeout: Duration,
) -> Result<(), Box<dyn Error>> {
    match error {
        ExternalEngineError::StartupTimeout { timeout } => {
            assert_eq!(timeout, expected_timeout);
            Ok(())
        }
        error => Err(io::Error::other(format!(
            "fixture returned unexpected launch error: {error}"
        ))
        .into()),
    }
}

fn assert_cancelled(error: ExternalEngineError) -> Result<(), Box<dyn Error>> {
    match error {
        ExternalEngineError::Cancelled => Ok(()),
        error => Err(io::Error::other(format!(
            "fixture returned unexpected launch error: {error}"
        ))
        .into()),
    }
}

fn assert_profile_removed(profile: &Path, profile_parent: &Path) {
    assert_eq!(profile.parent(), Some(profile_parent));
    assert!(!profile.exists());
}

fn assert_unique_profiles(profiles: &[PathBuf]) {
    for (index, profile) in profiles.iter().enumerate() {
        for other in profiles.iter().skip(index + 1) {
            assert_ne!(profile, other);
        }
    }
}

fn assert_profile_parent_empty(profile_parent: &Path) -> Result<(), Box<dyn Error>> {
    let mut entries = fs::read_dir(profile_parent)?;
    assert!(entries.next().is_none());
    Ok(())
}

struct LaunchAttempt {
    cancellation: Cancellation,
    worker: Option<JoinHandle<Result<ExternalEngineProcess, ExternalEngineError>>>,
}

impl LaunchAttempt {
    fn start(options: ExternalEngineLaunchOptions, cancellation: Cancellation) -> Self {
        let worker_cancellation = cancellation.clone();
        let worker = thread::spawn(move || launch_external_engine(&options, &worker_cancellation));
        Self {
            cancellation,
            worker: Some(worker),
        }
    }

    fn finish(mut self) -> Result<ExternalEngineError, Box<dyn Error>> {
        self.cancellation.cancel();
        let worker = self
            .worker
            .take()
            .ok_or_else(|| io::Error::other("launch worker was already consumed"))?;
        let result = worker
            .join()
            .map_err(|_| io::Error::other("launch worker failed"))?;
        launch_result_error(result)
    }
}

impl Drop for LaunchAttempt {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(worker) = self.worker.take()
            && let Ok(Ok(mut process)) = worker.join()
        {
            let _ = process.shutdown();
        }
    }
}

struct ShellFixture {
    executable: PathBuf,
    arguments_path: PathBuf,
}

impl ShellFixture {
    fn new(root: &TestRoot, name: &str, body: &str) -> Result<Self, Box<dyn Error>> {
        let executable = root.path().join(format!("{name}.sh"));
        let arguments_path = root.path().join(format!("{name}.args"));
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\n{body}\n",
            shell_quote(&arguments_path)?,
        );
        fs::write(&executable, script)?;
        fs::set_permissions(&executable, fs::Permissions::from_mode(FIXTURE_MODE))?;
        assert_eq!(
            fs::metadata(&executable)?.permissions().mode() & 0o777,
            FIXTURE_MODE
        );
        Ok(Self {
            executable,
            arguments_path,
        })
    }

    fn executable(&self) -> &Path {
        &self.executable
    }

    fn wait_for_records(&self, timeout: Duration) -> Result<(), Box<dyn Error>> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.arguments_path.is_file() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    ErrorKind::TimedOut,
                    "fixture did not record its launch before the deadline",
                )
                .into());
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn recorded_profile(&self) -> Result<PathBuf, Box<dyn Error>> {
        self.wait_for_records(RECORD_TIMEOUT)?;
        let arguments = fs::read_to_string(&self.arguments_path)?;
        arguments
            .lines()
            .find_map(|argument| argument.strip_prefix("--user-data-dir="))
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::other("fixture did not receive a profile path").into())
    }
}

fn shell_quote(path: &Path) -> Result<String, Box<dyn Error>> {
    let value = path
        .to_str()
        .ok_or_else(|| io::Error::other("fixture path was not valid UTF-8"))?;
    Ok(format!("'{}'", value.replace('\'', "'\"'\"'")))
}

struct TestRoot(PathBuf);

impl TestRoot {
    fn new() -> io::Result<Self> {
        let parent = env::temp_dir();
        fs::create_dir_all(&parent)?;
        for _ in 0..ROOT_ALLOCATION_ATTEMPTS {
            let timestamp = match SystemTime::now().duration_since(UNIX_EPOCH) {
                Ok(duration) => duration.as_nanos(),
                Err(error) => error.duration().as_nanos(),
            };
            let counter = ROOT_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!("termglide-external-engine-{timestamp}-{counter}"));
            match fs::create_dir(&path) {
                Ok(()) => {
                    let root = Self(path);
                    fs::set_permissions(root.path(), fs::Permissions::from_mode(FIXTURE_MODE))?;
                    return Ok(root);
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            ErrorKind::AlreadyExists,
            "could not allocate a test root",
        ))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
