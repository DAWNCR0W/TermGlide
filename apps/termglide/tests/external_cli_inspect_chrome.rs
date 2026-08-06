//! Real-engine CLI coverage for Chrome inspect metadata and lifecycle boundaries.

use std::error::Error;
use std::fs;
use std::io::{self, Read};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::{self, Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tg_browser::{ExternalEngineError, discover_external_engine};
use url::Url;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_STDOUT_BYTES: usize = 32 * 1024;
const MAX_STDERR_BYTES: usize = 16 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(25);
const TEMP_ROOT_ATTEMPTS: u8 = 32;
const TERMGLIDE_PROFILE_PREFIX: &str = "termglide-cdp-";
const DATA_HTML_INSPECT_FIXTURE: &str = "data:text/html,%3C!doctype%20html%3E%3Cmeta%20charset%3Dutf-8%3E%3Ctitle%3ETermGlide%20inspect%3C/title%3E%3Cbody%3EChrome%20metadata%3C/body%3E";
const DATA_HTML_ACCESSIBILITY_FIXTURE: &str = "data:text/html,%3C!doctype%20html%3E%3Cmeta%20charset%3Dutf-8%3E%3Cmain%20aria-label%3D%22TermGlide%20accessibility%20fixture%22%3E%3Cbutton%20aria-pressed%3D%22true%22%3EExport%20report%3C%2Fbutton%3E%3C%2Fmain%3E";

type TestResult = Result<(), Box<dyn Error>>;

#[test]
fn chrome_cli_inspect_and_accessibility_dump_clean_up() -> TestResult {
    let executable = match discover_external_engine(None) {
        Ok(path) => canonical_selected_executable(path)?,
        Err(ExternalEngineError::ExecutableNotFound) => return Ok(()),
        Err(error) => {
            return Err(test_error(format!(
                "external engine discovery failed unexpectedly: {error}"
            )));
        }
    };

    let mut temp_root = TestTempRoot::new()?;
    let checks = run_chrome_cli_inspect_checks(&executable, temp_root.path());
    let cleanup = temp_root.verify_no_profiles_and_remove();

    match (checks, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(test_error(format!(
            "external Chrome inspect cleanup failed after successful checks: {error}"
        ))),
        (Err(check_error), Err(cleanup_error)) => Err(test_error(format!(
            "external Chrome inspect checks failed: {check_error}; cleanup also failed: {cleanup_error}"
        ))),
    }
}

fn canonical_selected_executable(discovered: PathBuf) -> Result<PathBuf, Box<dyn Error>> {
    let selected = discovered.canonicalize().map_err(|error| {
        test_error(format!(
            "discovered external engine path {} could not be canonicalized: {error}",
            discovered.display()
        ))
    })?;
    if !selected.is_absolute() || !selected.is_file() {
        return Err(test_error(format!(
            "discovered external engine is not an absolute regular executable path: {}",
            selected.display()
        )));
    }

    // The CLI receives this as a filesystem path, never as a credential-bearing URL. Keeping a
    // file URL representation here makes that provenance condition explicit for the real child.
    let provenance = Url::from_file_path(&selected).map_err(|()| {
        test_error(format!(
            "selected external engine cannot be represented as a local file URL: {}",
            selected.display()
        ))
    })?;
    if !provenance.username().is_empty() || provenance.password().is_some() {
        return Err(test_error(format!(
            "selected external engine provenance unexpectedly includes credentials: {provenance}"
        )));
    }
    Ok(selected)
}

fn run_chrome_cli_inspect_checks(executable: &Path, temp_root: &Path) -> TestResult {
    let mut inspect = chrome_inspect_command(temp_root, executable);
    inspect.arg("inspect").arg(DATA_HTML_INSPECT_FIXTURE);
    let inspected = run_bounded_child(inspect, "Chrome inspect")?;
    require_success(&inspected, "Chrome inspect")?;
    let inspection = parse_inspection(&inspected)?;
    require_browser_metadata(&inspection)?;
    assert_optional_executable_provenance(&inspection, executable)?;
    assert_optional_loopback_debugging_metadata(&inspection)?;
    assert_no_isolated_profiles(temp_root)?;

    let mut accessibility = chrome_inspect_command(temp_root, executable);
    accessibility
        .arg("dump")
        .arg("--format")
        .arg("accessibility")
        .arg(DATA_HTML_ACCESSIBILITY_FIXTURE);
    let accessibility = run_bounded_child(accessibility, "Chrome accessibility dump")?;
    require_success(&accessibility, "Chrome accessibility dump")?;
    let tree = parse_accessibility_tree(&accessibility)?;
    require_accessibility_fixture(&tree)?;
    assert_no_isolated_profiles(temp_root)?;

    let missing_executable = temp_root.join("missing-chrome-executable");
    let mut invalid_override = chrome_inspect_command(temp_root, &missing_executable);
    invalid_override
        .arg("dump")
        .arg("--format")
        .arg("accessibility")
        .arg(DATA_HTML_ACCESSIBILITY_FIXTURE);
    let invalid_override =
        run_bounded_child(invalid_override, "invalid Chrome accessibility override")?;
    require_failure_with_stderr(
        &invalid_override,
        "invalid Chrome accessibility override",
        "browser executable override is not an executable regular file",
    )?;
    require_failure_with_stderr(
        &invalid_override,
        "invalid Chrome accessibility override path",
        &missing_executable.display().to_string(),
    )?;
    if !invalid_override.stdout.bytes.is_empty() {
        return Err(test_error(format!(
            "invalid Chrome accessibility override wrote partial stdout: {}",
            output_report(&invalid_override)
        )));
    }
    assert_no_isolated_profiles(temp_root)?;
    Ok(())
}

fn chrome_inspect_command(temp_root: &Path, executable: &Path) -> Command {
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
    require_complete_streams(output, label)?;
    if output.status.success() {
        return Ok(());
    }
    Err(test_error(format!(
        "{label} did not succeed: {}",
        output_report(output)
    )))
}

fn require_failure_with_stderr(output: &CapturedOutput, label: &str, expected: &str) -> TestResult {
    require_complete_streams(output, label)?;
    if output.status.success() {
        return Err(test_error(format!(
            "{label} unexpectedly succeeded: {}",
            output_report(output)
        )));
    }
    let stderr = String::from_utf8_lossy(&output.stderr.bytes);
    if !stderr.contains(expected) {
        return Err(test_error(format!(
            "{label} did not provide the expected diagnostic {expected:?}: {}",
            output_report(output)
        )));
    }
    Ok(())
}

fn require_complete_streams(output: &CapturedOutput, label: &str) -> TestResult {
    if output.stdout.truncated || output.stderr.truncated {
        return Err(test_error(format!(
            "{label} exceeded its bounded CLI output capture: {}",
            output_report(output)
        )));
    }
    Ok(())
}

fn parse_inspection(output: &CapturedOutput) -> Result<Value, Box<dyn Error>> {
    if output.stdout.bytes.is_empty() {
        return Err(test_error(format!(
            "Chrome inspect produced no JSON stdout: {}",
            output_report(output)
        )));
    }
    serde_json::from_slice(&output.stdout.bytes).map_err(|error| {
        test_error(format!(
            "Chrome inspect stdout was not valid JSON: {error}; {}",
            output_report(output)
        ))
    })
}

fn parse_accessibility_tree(output: &CapturedOutput) -> Result<Value, Box<dyn Error>> {
    if output.stdout.bytes.is_empty() {
        return Err(test_error(format!(
            "Chrome accessibility dump produced no JSON stdout: {}",
            output_report(output)
        )));
    }
    serde_json::from_slice(&output.stdout.bytes).map_err(|error| {
        test_error(format!(
            "Chrome accessibility dump stdout was not valid JSON: {error}; {}",
            output_report(output)
        ))
    })
}

fn require_accessibility_fixture(tree: &Value) -> TestResult {
    let nodes = tree.get("nodes").and_then(Value::as_array).ok_or_else(|| {
        test_error(format!(
            "Chrome accessibility dump must contain a nodes array, got {tree}"
        ))
    })?;
    if nodes.is_empty() {
        return Err(test_error(
            "Chrome accessibility dump returned an empty nodes array".to_owned(),
        ));
    }

    let button = nodes
        .iter()
        .find(|node| {
            node.get("role").and_then(Value::as_str) == Some("button")
                && node.get("name").and_then(Value::as_str) == Some("Export report")
        })
        .ok_or_else(|| {
            test_error(format!(
                "Chrome accessibility dump omitted the generic button role/name fixture: {tree}"
            ))
        })?;
    let states = button
        .get("states")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            test_error(format!(
                "Chrome accessibility button omitted its typed states array: {button}"
            ))
        })?;
    let pressed = states
        .iter()
        .find(|state| state.get("name").and_then(Value::as_str) == Some("pressed"))
        .ok_or_else(|| {
            test_error(format!(
                "Chrome accessibility button omitted its pressed state: {button}"
            ))
        })?;
    let value = pressed.get("value").ok_or_else(|| {
        test_error(format!(
            "Chrome accessibility pressed state omitted its typed value: {pressed}"
        ))
    })?;
    let is_true = match (
        value.get("type").and_then(Value::as_str),
        value.get("value"),
    ) {
        (Some("boolean"), Some(value)) => value.as_bool() == Some(true),
        (Some("string"), Some(value)) => value.as_str() == Some("true"),
        _ => false,
    };
    if is_true {
        return Ok(());
    }
    Err(test_error(format!(
        "Chrome accessibility pressed state must retain a typed true value, got {value}"
    )))
}

fn require_browser_metadata(inspection: &Value) -> TestResult {
    if !inspection.is_object() {
        return Err(test_error(format!(
            "Chrome inspect JSON must be an object, got {inspection}"
        )));
    }
    require_exact_string(inspection, "/engine", "chrome")?;
    let target = require_nonempty_string(inspection, "/url")?;
    if target != DATA_HTML_INSPECT_FIXTURE {
        return Err(test_error(format!(
            "Chrome inspect did not retain the generic data URL target: {target:?}"
        )));
    }
    require_credential_free_url(target, "inspect target")?;

    for field in [
        "protocol_version",
        "product",
        "javascript_version",
        "revision",
        "user_agent",
    ] {
        require_nonempty_string(inspection, &format!("/browser/{field}"))?;
    }
    Ok(())
}

fn require_exact_string(inspection: &Value, pointer: &str, expected: &str) -> TestResult {
    let actual = require_nonempty_string(inspection, pointer)?;
    if actual == expected {
        return Ok(());
    }
    Err(test_error(format!(
        "Chrome inspect field {pointer} was {actual:?}, expected {expected:?}"
    )))
}

fn require_nonempty_string<'a>(
    inspection: &'a Value,
    pointer: &str,
) -> Result<&'a str, Box<dyn Error>> {
    let value = inspection.pointer(pointer).ok_or_else(|| {
        test_error(format!(
            "Chrome inspect JSON is missing required field {pointer}"
        ))
    })?;
    let value = value.as_str().ok_or_else(|| {
        test_error(format!(
            "Chrome inspect field {pointer} must be a string, got {value}"
        ))
    })?;
    if value.trim().is_empty() {
        return Err(test_error(format!(
            "Chrome inspect field {pointer} must not be empty"
        )));
    }
    Ok(value)
}

fn require_credential_free_url(value: &str, label: &str) -> TestResult {
    let url = Url::parse(value).map_err(|error| {
        test_error(format!("{label} is not a parseable URL {value:?}: {error}"))
    })?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(test_error(format!(
            "{label} must not include credentials: {value:?}"
        )));
    }
    Ok(())
}

fn assert_optional_executable_provenance(inspection: &Value, selected: &Path) -> TestResult {
    // The current inspect schema deliberately limits itself to Browser.getVersion fields. If
    // executable provenance is added later, it must still identify the exact explicit local
    // executable supplied to this child and must never turn that path into a credential-bearing
    // endpoint.
    for pointer in [
        "/executable",
        "/executable_path",
        "/executablePath",
        "/browser/executable",
        "/browser/executable_path",
        "/browser/executablePath",
    ] {
        let Some(value) = inspection.pointer(pointer) else {
            continue;
        };
        let reported = value.as_str().ok_or_else(|| {
            test_error(format!(
                "Chrome inspect executable provenance {pointer} must be a string, got {value}"
            ))
        })?;
        let reported = executable_path_from_provenance(reported, pointer)?;
        if reported != selected {
            return Err(test_error(format!(
                "Chrome inspect executable provenance {pointer} resolved to {}, expected {}",
                reported.display(),
                selected.display()
            )));
        }
    }
    Ok(())
}

fn executable_path_from_provenance(value: &str, pointer: &str) -> Result<PathBuf, Box<dyn Error>> {
    let path = if Path::new(value).is_absolute() {
        PathBuf::from(value)
    } else {
        match Url::parse(value) {
            Ok(url) => {
                if url.scheme() != "file" {
                    return Err(test_error(format!(
                        "Chrome inspect executable provenance {pointer} must be a local path, got {value:?}"
                    )));
                }
                if !url.username().is_empty() || url.password().is_some() {
                    return Err(test_error(format!(
                        "Chrome inspect executable provenance {pointer} must not include credentials"
                    )));
                }
                url.to_file_path().map_err(|()| {
                test_error(format!(
                    "Chrome inspect executable provenance {pointer} is not a local file path: {value:?}"
                ))
            })?
            }
            Err(_) => PathBuf::from(value),
        }
    };
    path.canonicalize().map_err(|error| {
        test_error(format!(
            "Chrome inspect executable provenance {pointer} could not be canonicalized: {error}"
        ))
    })
}

fn assert_optional_loopback_debugging_metadata(inspection: &Value) -> TestResult {
    for pointer in [
        "/debugging_endpoint",
        "/debuggingEndpoint",
        "/devtools_endpoint",
        "/devtoolsEndpoint",
        "/websocket_url",
        "/webSocketUrl",
        "/debugger_address",
        "/debuggerAddress",
        "/browser/debugging_endpoint",
        "/browser/debuggingEndpoint",
        "/browser/devtools_endpoint",
        "/browser/devtoolsEndpoint",
        "/browser/websocket_url",
        "/browser/webSocketUrl",
        "/browser/debugger_address",
        "/browser/debuggerAddress",
    ] {
        if let Some(value) = inspection.pointer(pointer) {
            let endpoint = value.as_str().ok_or_else(|| {
                test_error(format!(
                    "Chrome inspect debugging metadata {pointer} must be a string, got {value}"
                ))
            })?;
            assert_loopback_debugging_endpoint(pointer, endpoint)?;
        }
    }

    for pointer in ["/debugging", "/browser/debugging"] {
        if let Some(value) = inspection.pointer(pointer) {
            assert_loopback_debugging_object(pointer, value)?;
        }
    }
    Ok(())
}

fn assert_loopback_debugging_endpoint(pointer: &str, endpoint: &str) -> TestResult {
    let url = Url::parse(endpoint)
        .or_else(|_| Url::parse(&format!("ws://{endpoint}")))
        .map_err(|error| {
            test_error(format!(
                "Chrome inspect debugging metadata {pointer} is not a URL or host:port: {error}"
            ))
        })?;
    if !matches!(url.scheme(), "ws" | "http") {
        return Err(test_error(format!(
            "Chrome inspect debugging metadata {pointer} uses an unsupported scheme {}",
            url.scheme()
        )));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(test_error(format!(
            "Chrome inspect debugging metadata {pointer} must not include credentials"
        )));
    }
    let host = url.host_str().ok_or_else(|| {
        test_error(format!(
            "Chrome inspect debugging metadata {pointer} is missing a host"
        ))
    })?;
    require_loopback_ip(host, pointer)?;
    if url.port().is_none() {
        return Err(test_error(format!(
            "Chrome inspect debugging metadata {pointer} is missing a port"
        )));
    }
    Ok(())
}

fn assert_loopback_debugging_object(pointer: &str, value: &Value) -> TestResult {
    let object = value.as_object().ok_or_else(|| {
        test_error(format!(
            "Chrome inspect debugging metadata {pointer} must be an object, got {value}"
        ))
    })?;
    let host = object.get("host").and_then(Value::as_str).ok_or_else(|| {
        test_error(format!(
            "Chrome inspect debugging metadata {pointer} is missing string host"
        ))
    })?;
    require_loopback_ip(host, pointer)?;
    let port = object.get("port").and_then(Value::as_u64).ok_or_else(|| {
        test_error(format!(
            "Chrome inspect debugging metadata {pointer} is missing numeric port"
        ))
    })?;
    if port == 0 || port > u64::from(u16::MAX) {
        return Err(test_error(format!(
            "Chrome inspect debugging metadata {pointer} has invalid port {port}"
        )));
    }
    Ok(())
}

fn require_loopback_ip(host: &str, pointer: &str) -> TestResult {
    let host: IpAddr = host.parse().map_err(|error| {
        test_error(format!(
            "Chrome inspect debugging metadata {pointer} host {host:?} is not an IP address: {error}"
        ))
    })?;
    if host.is_loopback() {
        return Ok(());
    }
    Err(test_error(format!(
        "Chrome inspect debugging metadata {pointer} must use a loopback IP, got {host}"
    )))
}

fn assert_no_isolated_profiles(temp_root: &Path) -> io::Result<()> {
    let profiles = fs::read_dir(temp_root)?
        .collect::<io::Result<Vec<_>>>()?
        .into_iter()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(TERMGLIDE_PROFILE_PREFIX)
        })
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    if profiles.is_empty() {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "TermGlide isolated Chrome profiles were not removed: {}",
        display_paths(&profiles)
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
                "termglide-external-cli-inspect-{}-{timestamp}-{attempt}",
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
            "could not allocate a unique TermGlide external CLI inspect temp root",
        ))
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn verify_no_profiles_and_remove(&mut self) -> io::Result<()> {
        assert_no_isolated_profiles(&self.path)?;
        let entries = fs::read_dir(&self.path)?.collect::<io::Result<Vec<_>>>()?;
        if !entries.is_empty() {
            let paths = entries.iter().map(|entry| entry.path()).collect::<Vec<_>>();
            return Err(io::Error::other(format!(
                "external Chrome inspect temp root was not empty after child cleanup: {}",
                display_paths(&paths)
            )));
        }
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
    let stdout_reader = thread::spawn(move || capture_stream(stdout, MAX_STDOUT_BYTES));
    let stderr_reader = thread::spawn(move || capture_stream(stderr, MAX_STDERR_BYTES));
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

fn capture_stream<R: Read>(mut reader: R, max_bytes: usize) -> io::Result<CapturedStream> {
    let mut bytes = Vec::new();
    let mut truncated = false;
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(CapturedStream { bytes, truncated }),
            Ok(read) => {
                let remaining = max_bytes.saturating_sub(bytes.len());
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
