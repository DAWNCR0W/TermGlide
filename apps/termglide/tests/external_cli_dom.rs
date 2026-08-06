//! Real-engine CLI coverage for the external Chrome DOM output boundary.

use std::error::Error;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{self, Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tg_browser::{ExternalEngineError, discover_external_engine};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CAPTURE_BYTES: usize = 16 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(25);
const TEMP_ROOT_ATTEMPTS: u8 = 32;
const TERMGLIDE_PROFILE_PREFIX: &str = "termglide-cdp-";
const DOM_MARKER: &str = "termglide-dom-e2e";
const DATA_HTML_FIXTURE: &str = "data:text/html,%3C!doctype%20html%3E%3Ctitle%3ETermGlide%20DOM%20E2E%3C%2Ftitle%3E%3Cmain%20id%3D%22termglide-dom-e2e%22%3EExternal%20DOM%20boundary%3C%2Fmain%3E";

type TestResult = Result<(), Box<dyn Error>>;

#[test]
fn chrome_cli_e2e_dumps_dom_and_fails_closed_on_dom_quota() -> TestResult {
    let executable = match discover_external_engine(None) {
        Ok(path) => path,
        Err(ExternalEngineError::ExecutableNotFound) => return Ok(()),
        Err(error) => {
            return Err(test_error(format!(
                "external engine discovery failed unexpectedly: {error}"
            )));
        }
    };

    let mut temp_root = TestTempRoot::new()?;
    let checks = run_dom_checks(&executable, temp_root.path());
    let cleanup = temp_root.verify_empty_and_remove();

    match (checks, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(test_error(format!(
            "external DOM CLI E2E cleanup failed after successful checks: {error}"
        ))),
        (Err(check_error), Err(cleanup_error)) => Err(test_error(format!(
            "external DOM CLI E2E checks failed: {check_error}; cleanup also failed: {cleanup_error}"
        ))),
    }
}

fn run_dom_checks(executable: &Path, temp_root: &Path) -> TestResult {
    let mut dom_dump = chrome_command(temp_root, executable);
    dom_dump
        .arg("dump")
        .arg("--format")
        .arg("dom")
        .arg(DATA_HTML_FIXTURE);
    let dom_dump = run_bounded_child(dom_dump, "Chrome DOM dump")?;
    require_success(&dom_dump, "Chrome DOM dump")?;
    require_outer_html(&dom_dump)?;
    assert_temp_root_empty(temp_root)?;

    let mut quota = chrome_command(temp_root, executable);
    quota
        .arg("--data-limit")
        .arg("1B")
        .arg("dump")
        .arg("--format")
        .arg("dom")
        .arg(DATA_HTML_FIXTURE);
    let quota = run_bounded_child(quota, "small external Chrome DOM data limit")?;
    require_failure_with_stderr(
        &quota,
        "small external Chrome DOM data limit",
        "external Chrome DOM output used",
    )?;
    require_failure_with_stderr(
        &quota,
        "small external Chrome DOM data limit quota",
        "exceeding its quota",
    )?;
    if !quota.stdout.bytes.is_empty() {
        return Err(test_error(format!(
            "small external Chrome DOM data limit wrote partial stdout: {}",
            output_report(&quota)
        )));
    }
    assert_temp_root_empty(temp_root)?;
    Ok(())
}

fn chrome_command(temp_root: &Path, executable: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_termglide"));
    command
        .current_dir(temp_root)
        .env("TMPDIR", temp_root)
        .env("TMP", temp_root)
        .env("TEMP", temp_root)
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
    if output.status.success() {
        return Ok(());
    }
    Err(test_error(format!(
        "{label} did not succeed: {}",
        output_report(output)
    )))
}

fn require_outer_html(output: &CapturedOutput) -> TestResult {
    let outer_html = std::str::from_utf8(&output.stdout.bytes).map_err(|error| {
        test_error(format!(
            "Chrome DOM dump did not produce UTF-8 outer HTML ({error}): {}",
            output_report(output)
        ))
    })?;
    let document_element = outer_html
        .strip_prefix("<!DOCTYPE html>")
        .unwrap_or(outer_html);
    if !document_element.starts_with("<html")
        || !outer_html.contains(DOM_MARKER)
        || !outer_html.contains("External DOM boundary")
    {
        return Err(test_error(format!(
            "Chrome DOM dump did not produce the fixture's outer HTML: {}",
            output_report(output)
        )));
    }
    if outer_html.ends_with('\n') {
        return Err(test_error(format!(
            "Chrome DOM dump added output beyond the exact DOM serialization: {}",
            output_report(output)
        )));
    }
    Ok(())
}

fn require_failure_with_stderr(output: &CapturedOutput, label: &str, expected: &str) -> TestResult {
    if output.status.success() {
        return Err(test_error(format!(
            "{label} unexpectedly succeeded: {}",
            output_report(output)
        )));
    }
    let stderr = String::from_utf8_lossy(&output.stderr.bytes);
    if !stderr.contains(expected) {
        return Err(test_error(format!(
            "{label} did not provide the expected typed diagnostic {expected:?}: {}",
            output_report(output)
        )));
    }
    Ok(())
}

fn assert_temp_root_empty(temp_root: &Path) -> io::Result<()> {
    let entries = fs::read_dir(temp_root)?.collect::<io::Result<Vec<_>>>()?;
    let profiles = entries
        .iter()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(TERMGLIDE_PROFILE_PREFIX)
        })
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    if !profiles.is_empty() {
        return Err(io::Error::other(format!(
            "TermGlide isolated Chrome profiles were not removed: {}",
            display_paths(&profiles)
        )));
    }
    if entries.is_empty() {
        return Ok(());
    }
    let paths = entries.iter().map(|entry| entry.path()).collect::<Vec<_>>();
    Err(io::Error::other(format!(
        "external DOM CLI E2E temp root was not empty: {}",
        display_paths(&paths)
    )))
}

struct TestTempRoot {
    path: PathBuf,
    removed: bool,
}

impl TestTempRoot {
    fn new() -> io::Result<Self> {
        let parent = std::env::temp_dir();
        fs::create_dir_all(&parent)?;
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        for attempt in 0..TEMP_ROOT_ATTEMPTS {
            let path = parent.join(format!(
                "termglide-external-cli-dom-{}-{timestamp}-{attempt}",
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
            "could not allocate a unique TermGlide external DOM CLI E2E temp root",
        ))
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn verify_empty_and_remove(&mut self) -> io::Result<()> {
        assert_temp_root_empty(&self.path)?;
        fs::remove_dir(&self.path)?;
        self.removed = true;
        Ok(())
    }
}

impl Drop for TestTempRoot {
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
