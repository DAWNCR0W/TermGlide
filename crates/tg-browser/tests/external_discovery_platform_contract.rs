//! Public executable-discovery contracts without starting an external browser.
//!
//! The only launch-path call deliberately uses a missing explicit override, so discovery must
//! reject before it can allocate a profile or spawn a child process.

use std::env;
use std::error::Error;
use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};

use tg_browser::{
    ExternalEngineError, ExternalEngineLaunchOptions, discover_external_engine,
    launch_external_engine,
};
use tg_core::Cancellation;
use url::Url;

type TestError = Box<dyn Error + Send + Sync>;
type TestResult<T = ()> = Result<T, TestError>;

const ROOT_ALLOCATION_ATTEMPTS: u8 = 32;
#[cfg(unix)]
const EXECUTABLE_MODE: u32 = 0o700;
#[cfg(unix)]
const NON_EXECUTABLE_MODE: u32 = 0o600;
const HOST_DISCOVERY_MISS: &str = "could not discover a Google Chrome or Chromium executable; \
install one or pass --browser-executable <path-to-chrome-or-chromium>";

static ROOT_COUNTER: AtomicU64 = AtomicU64::new(0);

#[test]
fn explicit_regular_override_wins_without_launching_an_engine() -> TestResult {
    let root = TestRoot::new()?;
    let explicit = root.path().join("explicit-browser");
    write_regular_executable(&explicit)?;

    let selected = discover_external_engine(Some(&explicit))?;
    assert_eq!(selected, explicit);
    assert_regular_executable(&selected)?;
    Ok(())
}

#[test]
fn missing_and_directory_overrides_are_typed_refusals_before_profile_allocation() -> TestResult {
    let root = TestRoot::new()?;
    let missing = root.path().join("missing-browser");
    let directory = root.path().join("browser-directory");
    let profile_parent = root.path().join("profiles");
    fs::create_dir(&directory)?;
    fs::create_dir(&profile_parent)?;

    assert_invalid_override(discover_external_engine(Some(&missing)), &missing)?;
    assert_invalid_override(discover_external_engine(Some(&directory)), &directory)?;

    // This is the public launch boundary's preflight only: the missing override is rejected
    // before the launcher has an executable, a child process, or a temporary profile to own.
    let mut options = ExternalEngineLaunchOptions::new(Url::parse("about:blank")?);
    options.executable_override = Some(missing.clone());
    options.temporary_profile_parent = Some(profile_parent.clone());
    assert_invalid_override(
        launch_external_engine(&options, &Cancellation::new()),
        &missing,
    )?;
    assert_directory_empty(&profile_parent)?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn unix_non_executable_and_symlink_overrides_follow_the_public_provenance_contract() -> TestResult {
    let root = TestRoot::new()?;
    let non_executable = root.path().join("non-executable-browser");
    fs::write(&non_executable, b"fixture")?;
    fs::set_permissions(
        &non_executable,
        fs::Permissions::from_mode(NON_EXECUTABLE_MODE),
    )?;
    assert_invalid_override(
        discover_external_engine(Some(&non_executable)),
        &non_executable,
    )?;

    let target = root.path().join("real-browser");
    let supplied_link = root.path().join("browser-link");
    write_regular_executable(&target)?;
    symlink(&target, &supplied_link)?;

    let selected = discover_external_engine(Some(&supplied_link))?;
    // `metadata` validates the link target, while discovery preserves the caller-supplied path
    // as provenance instead of silently returning a different canonical path.
    assert_eq!(selected, supplied_link);
    assert_ne!(selected, target);
    assert_eq!(fs::canonicalize(&selected)?, fs::canonicalize(&target)?);
    assert_regular_executable(&selected)?;
    Ok(())
}

#[test]
fn host_discovery_returns_a_regular_executable_or_explicit_recovery_diagnostic() -> TestResult {
    match discover_external_engine(None) {
        Ok(path) => assert_regular_executable(&path),
        Err(error @ ExternalEngineError::ExecutableNotFound) => {
            assert_eq!(error.to_string(), HOST_DISCOVERY_MISS);
            Ok(())
        }
        Err(error) => Err(io::Error::other(format!(
            "host discovery returned an unexpected public error: {error}"
        ))
        .into()),
    }
}

fn assert_invalid_override<T>(
    result: Result<T, ExternalEngineError>,
    expected_path: &Path,
) -> TestResult {
    match result {
        Err(error) => {
            let message = error.to_string();
            match error {
                ExternalEngineError::InvalidExecutableOverride { path } => {
                    assert_eq!(path.as_path(), expected_path);
                    assert_eq!(
                        message,
                        format!(
                            "browser executable override is not an executable regular file: {}",
                            expected_path.display()
                        )
                    );
                    Ok(())
                }
                error => Err(io::Error::other(format!(
                    "invalid override returned an unexpected public error: {error}"
                ))
                .into()),
            }
        }
        Ok(_) => Err(io::Error::other(format!(
            "invalid override unexpectedly succeeded for {}",
            expected_path.display()
        ))
        .into()),
    }
}

fn write_regular_executable(path: &Path) -> io::Result<()> {
    fs::write(path, b"fixture")?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(EXECUTABLE_MODE))?;
    Ok(())
}

fn assert_regular_executable(path: &Path) -> TestResult {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(io::Error::other(format!(
            "discovery returned a non-regular path: {}",
            path.display()
        ))
        .into());
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o111 == 0 {
        return Err(io::Error::other(format!(
            "discovery returned a non-executable Unix path: {}",
            path.display()
        ))
        .into());
    }
    Ok(())
}

fn assert_directory_empty(path: &Path) -> TestResult {
    if fs::read_dir(path)?.next().is_none() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "discovery unexpectedly allocated profile state under {}",
            path.display()
        ))
        .into())
    }
}

struct TestRoot(PathBuf);

impl TestRoot {
    fn new() -> io::Result<Self> {
        let parent = env::temp_dir();
        for _ in 0..ROOT_ALLOCATION_ATTEMPTS {
            let timestamp = match SystemTime::now().duration_since(UNIX_EPOCH) {
                Ok(duration) => duration.as_nanos(),
                Err(error) => error.duration().as_nanos(),
            };
            let counter = ROOT_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!(
                "termglide-discovery-contract-{timestamp}-{counter}"
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            ErrorKind::AlreadyExists,
            "could not allocate a discovery-contract test root",
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
