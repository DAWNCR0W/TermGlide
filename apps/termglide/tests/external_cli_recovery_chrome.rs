//! CLI recovery coverage for an early external-engine exit followed by a clean Chrome relaunch.
//!
//! The first child deliberately selects the shipped `termglide` executable as the external
//! engine. It is a valid executable file but exits before publishing Chrome DevTools state, so
//! this exercises the real CLI launch-cleanup path without a site-specific crash fixture.

use std::error::Error;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{self, Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tg_browser::{ExternalEngineError, discover_external_engine};

type TestResult = Result<(), Box<dyn Error>>;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(25);
const MAX_CAPTURE_BYTES: usize = 16 * 1024;
const TEMP_ROOT_ATTEMPTS: u8 = 32;
const TERMGLIDE_PROFILE_PREFIX: &str = "termglide-cdp-";
const TERMGLIDE_PRIVATE_PREFIX: &str = "termglide-private-";
const DATA_HTML_FIXTURE: &str = "data:text/html,%3C!doctype%20html%3E%3Ctitle%3ETermGlide%20CLI%20Recovery%3C%2Ftitle%3E%3Cbody%20style%3D%22margin%3A0%3Bbackground%3A%230033cc%3Bcolor%3Awhite%22%3ECLI%20recovery%20fixture%3C%2Fbody%3E";

#[test]
fn chrome_cli_relaunches_after_early_engine_exit_without_stale_profiles_or_output() -> TestResult {
    let chrome = match discover_external_engine(None) {
        Ok(path) => path,
        Err(ExternalEngineError::ExecutableNotFound) => return Ok(()),
        Err(error) => {
            return Err(test_error(format!(
                "external engine discovery failed unexpectedly: {error}"
            )));
        }
    };
    if !chrome.is_file() {
        return Err(test_error(format!(
            "discovered external engine was not a regular file: {}",
            chrome.display()
        )));
    }

    let mut root = TestRoot::new()?;
    let recovery = run_recovery(&chrome, root.path());
    let cleanup = root.verify_empty_and_remove();
    preserve_results(recovery, cleanup)
}

fn run_recovery(chrome: &Path, root: &Path) -> TestResult {
    // `termglide` accepts no Chromium startup arguments, so this valid executable reliably exits
    // before DevToolsActivePort exists. The outer CLI must reap it and remove the isolated
    // `termglide-cdp-*` directory before the next, real Chrome launch can proceed.
    let fake_engine = Path::new(env!("CARGO_BIN_EXE_termglide"));
    if !fake_engine.is_file() {
        return Err(test_error(format!(
            "the shipped CLI binary was not an executable regular file: {}",
            fake_engine.display()
        )));
    }

    let mut early_exit = chrome_command(root, fake_engine);
    early_exit
        .arg("dump")
        .arg("--format")
        .arg("cells")
        .arg(DATA_HTML_FIXTURE);
    let early_exit = run_bounded_child(early_exit, "early external-engine exit")?;
    require_failure_with_stderr(
        &early_exit,
        "early external-engine exit",
        "external Chrome operation failed",
    )?;
    require_failure_with_stderr(
        &early_exit,
        "early external-engine exit",
        "external browser exited before DevTools became ready",
    )?;
    require_empty_stdout(&early_exit, "early external-engine exit")?;
    assert_root_empty(root)?;

    let mut relaunch = chrome_command(root, chrome);
    relaunch
        .arg("dump")
        .arg("--format")
        .arg("cells")
        .arg(DATA_HTML_FIXTURE);
    let relaunch = run_bounded_child(relaunch, "real Chrome relaunch after early exit")?;
    require_success(&relaunch, "real Chrome relaunch after early exit")?;
    require_cell_grid(&relaunch, "real Chrome relaunch after early exit")?;

    // The failed profile was already absent before the relaunch, so this same-root success cannot
    // inherit or reuse stale external state. Its own isolated profile must be gone as well.
    assert_root_empty(root)
}

fn chrome_command(root: &Path, executable: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_termglide"));
    command
        .current_dir(root)
        // Child-scoped temporary roots keep private and Chrome profiles observable without
        // changing the test process's global environment.
        .env("TMPDIR", root)
        .env("TMP", root)
        .env("TEMP", root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .arg("--browser-executable")
        .arg(executable)
        .arg("--viewport")
        .arg("320x180")
        .arg("--renderer")
        .arg("cells");
    command
}

fn require_success(output: &CapturedOutput, label: &str) -> TestResult {
    require_complete_capture(output, label)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(test_error(format!(
            "{label} did not succeed: {}",
            output_report(output)
        )))
    }
}

fn require_failure_with_stderr(output: &CapturedOutput, label: &str, expected: &str) -> TestResult {
    require_complete_capture(output, label)?;
    if output.status.success() {
        return Err(test_error(format!(
            "{label} unexpectedly succeeded: {}",
            output_report(output)
        )));
    }
    let stderr = String::from_utf8_lossy(&output.stderr.bytes);
    if stderr.contains(expected) {
        Ok(())
    } else {
        Err(test_error(format!(
            "{label} did not provide typed recovery diagnostic {expected:?}: {}",
            output_report(output)
        )))
    }
}

fn require_empty_stdout(output: &CapturedOutput, label: &str) -> TestResult {
    if output.stdout.bytes.is_empty() {
        Ok(())
    } else {
        Err(test_error(format!(
            "{label} wrote partial stdout before recovery: {}",
            output_report(output)
        )))
    }
}

fn require_cell_grid(output: &CapturedOutput, label: &str) -> TestResult {
    if output.stdout.bytes.is_empty() {
        return Err(test_error(format!(
            "{label} produced no cell output: {}",
            output_report(output)
        )));
    }
    let rows = output
        .stdout
        .bytes
        .iter()
        .filter(|byte| **byte == b'\n')
        .count();
    if rows >= 2 {
        Ok(())
    } else {
        Err(test_error(format!(
            "{label} did not produce a meaningful recovered cell grid: {}",
            output_report(output)
        )))
    }
}

fn require_complete_capture(output: &CapturedOutput, label: &str) -> TestResult {
    if !output.stdout.truncated && !output.stderr.truncated {
        Ok(())
    } else {
        Err(test_error(format!(
            "{label} exceeded its bounded stdout/stderr capture: {}",
            output_report(output)
        )))
    }
}

fn assert_root_empty(root: &Path) -> TestResult {
    let entries = fs::read_dir(root)?.collect::<io::Result<Vec<_>>>()?;
    if entries.is_empty() {
        return Ok(());
    }
    let paths = entries.iter().map(|entry| entry.path()).collect::<Vec<_>>();
    let profiles = paths
        .iter()
        .filter(|path| {
            path.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                name.starts_with(TERMGLIDE_PROFILE_PREFIX)
                    || name.starts_with(TERMGLIDE_PRIVATE_PREFIX)
            })
        })
        .cloned()
        .collect::<Vec<_>>();
    let profile_detail = if profiles.is_empty() {
        "no recognizable profile directory".to_owned()
    } else {
        format!("profiles: {}", display_paths(&profiles))
    };
    Err(test_error(format!(
        "CLI recovery child left temporary state ({profile_detail}): {}",
        display_paths(&paths)
    )))
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
        for attempt in 0..TEMP_ROOT_ATTEMPTS {
            let path = parent.join(format!(
                "termglide-external-cli-recovery-{}-{timestamp}-{attempt}",
                process::id()
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
            "could not allocate a unique TermGlide CLI recovery root",
        ))
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn verify_empty_and_remove(&mut self) -> TestResult {
        assert_root_empty(&self.path)?;
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

#[derive(Debug)]
struct CapturedStream {
    bytes: Vec<u8>,
    truncated: bool,
}

#[derive(Debug)]
struct CapturedOutput {
    status: ExitStatus,
    stdout: CapturedStream,
    stderr: CapturedStream,
}

fn run_bounded_child(mut command: Command, label: &str) -> io::Result<CapturedOutput> {
    let mut child = command.spawn()?;
    let stdout = take_stdout(&mut child, label)?;
    let stderr = take_stderr(&mut child, label)?;
    let stdout_reader = thread::spawn(move || capture_stream(stdout));
    let stderr_reader = thread::spawn(move || capture_stream(stderr));
    let status = wait_with_deadline(&mut child, label, COMMAND_TIMEOUT);
    let stdout = join_capture(stdout_reader, "stdout")?;
    let stderr = join_capture(stderr_reader, "stderr")?;

    match status {
        Ok(status) => Ok(CapturedOutput {
            status,
            stdout,
            stderr,
        }),
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!(
                "{label} failed before completion: {error}; stdout:\n{}\nstderr:\n{}",
                stream_text(&stdout),
                stream_text(&stderr)
            ),
        )),
    }
}

fn take_stdout(child: &mut Child, label: &str) -> io::Result<std::process::ChildStdout> {
    if let Some(stdout) = child.stdout.take() {
        return Ok(stdout);
    }
    let _cleanup_result = kill_and_wait(child);
    Err(io::Error::other(format!("{label} stdout was not captured")))
}

fn take_stderr(child: &mut Child, label: &str) -> io::Result<std::process::ChildStderr> {
    if let Some(stderr) = child.stderr.take() {
        return Ok(stderr);
    }
    let _cleanup_result = kill_and_wait(child);
    Err(io::Error::other(format!("{label} stderr was not captured")))
}

fn wait_with_deadline(child: &mut Child, label: &str, timeout: Duration) -> io::Result<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(error) => {
                return match kill_and_wait(child) {
                    Ok(status) => Err(io::Error::new(
                        error.kind(),
                        format!("{label} could not be polled and was terminated with {status}"),
                    )),
                    Err(cleanup_error) => Err(io::Error::new(
                        error.kind(),
                        format!(
                            "{label} could not be polled ({error}) and cleanup failed: {cleanup_error}"
                        ),
                    )),
                };
            }
        }
        if Instant::now() >= deadline {
            return match kill_and_wait(child) {
                Ok(status) => Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("{label} exceeded {timeout:?} and was terminated with {status}"),
                )),
                Err(cleanup_error) => Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "{label} exceeded {timeout:?}; kill-and-wait cleanup failed: {cleanup_error}"
                    ),
                )),
            };
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn kill_and_wait(child: &mut Child) -> io::Result<ExitStatus> {
    let kill = child.kill();
    let wait = child.wait();
    match (kill, wait) {
        (_, Ok(status)) => Ok(status),
        (Ok(()), Err(error)) => Err(error),
        (Err(kill_error), Err(wait_error)) => Err(io::Error::new(
            wait_error.kind(),
            format!("child kill failed ({kill_error}); child wait also failed: {wait_error}"),
        )),
    }
}

fn capture_stream<R: Read>(mut reader: R) -> io::Result<CapturedStream> {
    let mut bytes = Vec::new();
    let mut truncated = false;
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(CapturedStream { bytes, truncated }),
            Ok(read) => {
                let remaining = MAX_CAPTURE_BYTES.saturating_sub(bytes.len());
                let retained = read.min(remaining);
                bytes.extend_from_slice(&buffer[..retained]);
                truncated |= retained < read;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

fn join_capture(
    reader: thread::JoinHandle<io::Result<CapturedStream>>,
    stream: &str,
) -> io::Result<CapturedStream> {
    reader
        .join()
        .map_err(|_| io::Error::other(format!("{stream} capture worker panicked")))?
}

fn preserve_results(primary: TestResult, cleanup: TestResult) -> TestResult {
    match (primary, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(primary), Err(cleanup)) => Err(test_error(format!(
            "CLI recovery failed: {primary}; cleanup also failed: {cleanup}"
        ))),
    }
}

fn output_report(output: &CapturedOutput) -> String {
    format!(
        "status={}; stdout:\n{}\nstderr:\n{}",
        output.status,
        stream_text(&output.stdout),
        stream_text(&output.stderr)
    )
}

fn stream_text(stream: &CapturedStream) -> String {
    let suffix = if stream.truncated {
        "\n[output truncated]"
    } else {
        ""
    };
    format!("{}{suffix}", String::from_utf8_lossy(&stream.bytes))
}

fn display_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn test_error(message: String) -> Box<dyn Error> {
    Box::new(io::Error::other(message))
}
