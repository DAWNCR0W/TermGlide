//! Real-Chrome coverage for the public interactive screenshot-to-terminal loop.
//!
//! The parent test starts an isolated copy of this test binary with a private temporary root.
//! That keeps the app-owned Chrome profile observable without changing the test process's global
//! environment. The child uses only a `data:` document, in-memory events, and an in-memory ANSI
//! writer; it opens no remote page or screen stream.

use std::collections::VecDeque;
use std::env;
use std::error::Error;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{self, Child, Command, ExitStatus, Stdio};
use std::task::{Context, Poll};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::Stream;
use termglide::external::{
    ExternalEnginePolicies, ExternalRenderLimits, ExternalRenderRequest, ExternalViewport,
};
use termglide::interactive::{InteractiveError, InteractiveEvent};
use termglide::{
    ExternalInteractiveError, ExternalInteractiveExitReason, ExternalInteractiveLimits,
    run_external_interactive,
};
use tg_browser::{ExternalEngineError, ShellInputEvent, discover_external_engine};
use tg_core::Cancellation;
use tg_terminal::{
    Backend, ColorLevel, ExternalFrameError, InputEvent, SurfaceError, TerminalCapabilities,
};

type TestError = Box<dyn Error>;
type TestResult<T = ()> = Result<T, TestError>;

const TEST_NAME: &str = "external_interactive_frame_chrome_e2e";
const CHILD_ROOT_ENV: &str = "TERMGLIDE_INTERACTIVE_FRAME_CHROME_ROOT";
const CHILD_DEADLINE: Duration = Duration::from_secs(45);
const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(25);
const LOOP_DEADLINE: Duration = Duration::from_secs(30);
const ROOT_CREATE_ATTEMPTS: u8 = 32;
const INITIAL_COLUMNS: u16 = 32;
const INITIAL_ROWS: u16 = 10;
const RESIZED_COLUMNS: u16 = 40;
const RESIZED_ROWS: u16 = 12;
const MAX_NORMAL_ANSI_BYTES: usize = 256 * 1024;
const MAX_QUOTA_ANSI_BYTES: usize = 1;
const MAX_TERMINAL_CELLS: usize = 1_024;
const CLEAR_SCREEN: &[u8] = b"\x1b[2J\x1b[H";

const FIXTURE_DOCUMENT: &str = r#"<!doctype html>
<meta charset="utf-8">
<style>
html,body{margin:0;width:100%;height:100%;overflow:hidden;background:#111;color:#eee;font-family:monospace}
#field{position:absolute;left:8px;top:8px;width:70%;height:32px;font-size:20px}
#state{box-sizing:border-box;width:100%;height:100%;padding:64px 12px;background:#111;color:#eee;font-size:30px}
.changed #state{background:#f4f4f4;color:#101010}
</style>
<input id="field" autofocus>
<div id="state">baseline</div>
<script>
const field=document.getElementById("field");
field.focus();
field.addEventListener("input",()=>{
  document.body.className="changed";
  document.getElementById("state").textContent="changed";
});
</script>"#;

/// Runs directly in the isolated child and otherwise supervises that child with a private
/// temporary root. The child-only branch gives the public app runner ownership of its Chrome
/// process and profile while allowing the parent to inspect the completed cleanup boundary.
#[tokio::test]
async fn external_interactive_frame_chrome_e2e() -> TestResult {
    if let Some(root) = env::var_os(CHILD_ROOT_ENV) {
        return isolated_live_contract(Path::new(&root)).await;
    }

    let mut root = TestRoot::new()?;
    let operation = run_isolated_test_child(root.path());
    let cleanup = root.verify_empty_and_remove();
    preserve_results(operation, cleanup)
}

async fn isolated_live_contract(root: &Path) -> TestResult {
    assert_root_empty(root)?;
    let executable = match discover_external_engine(None) {
        Ok(path) => path,
        Err(ExternalEngineError::ExecutableNotFound) => return Ok(()),
        Err(error) => return Err(box_error(error)),
    };
    if !executable.is_file() {
        return Err(failure(format!(
            "discovered external engine was not a regular file: {}",
            executable.display()
        )));
    }

    let live = run_cancellation_contract(&executable).await;
    let live_cleanup = assert_root_empty(root);
    preserve_results(live, live_cleanup)?;

    let quota = run_quota_contract(&executable).await;
    let quota_cleanup = assert_root_empty(root);
    preserve_results(quota, quota_cleanup)
}

async fn run_cancellation_contract(executable: &Path) -> TestResult {
    let cancellation = Cancellation::new();
    let events = CancellationAfterEvents::new(
        VecDeque::from([
            Ok(InteractiveEvent::Input(ShellInputEvent::Terminal(
                InputEvent::Paste("mutate".to_owned()),
            ))),
            Ok(InteractiveEvent::Resize {
                columns: RESIZED_COLUMNS,
                rows: RESIZED_ROWS,
            }),
        ]),
        cancellation.clone(),
    );
    let request = interactive_request(executable, MAX_NORMAL_ANSI_BYTES);
    let limits = ExternalInteractiveLimits {
        max_events: 2,
        crash_poll_interval: Duration::from_secs(2),
        ..ExternalInteractiveLimits::default()
    };
    let mut writer = FrameWriter::default();
    let report = match tokio::time::timeout(
        LOOP_DEADLINE,
        run_external_interactive(request, events, &mut writer, &cancellation, limits),
    )
    .await
    {
        Ok(result) => result.map_err(box_error)?,
        Err(error) => {
            return Err(failure(format!(
                "interactive Chrome cancellation contract exceeded {LOOP_DEADLINE:?}: {error}"
            )));
        }
    };

    require(
        report.reason == ExternalInteractiveExitReason::Cancelled,
        format!(
            "interactive Chrome loop ended with {:?} instead of cancellation",
            report.reason
        ),
    )?;
    require(
        report.events_processed == 2,
        format!(
            "interactive Chrome loop processed {} events instead of its two in-memory events",
            report.events_processed
        ),
    )?;
    require(
        (2..=3).contains(&report.frames_written),
        format!(
            "interactive Chrome loop wrote {} accepted frames instead of an initial frame, a resized full redraw, and at most one distinct intermediate input frame",
            report.frames_written
        ),
    )?;
    require(
        report.relaunches == 0,
        format!(
            "interactive Chrome loop relaunched {} times during the local fixture journey",
            report.relaunches
        ),
    )?;

    let frames = writer.frames();
    require(
        frames.len() == report.frames_written,
        format!(
            "ANSI writer recorded {} chunks for {} accepted frames",
            frames.len(),
            report.frames_written
        ),
    )?;
    let initial = frame_at(frames, 0, "initial")?;
    let resized = frame_at(frames, frames.len() - 1, "resized")?;
    require(
        !initial.is_empty(),
        "initial Chrome frame had no ANSI bytes",
    )?;
    require(
        !resized.is_empty(),
        "resized Chrome frame had no ANSI bytes",
    )?;
    require_full_redraw(initial, "initial Chrome frame")?;
    if frames.len() == 3 {
        let changed = frame_at(frames, 1, "input-changed")?;
        require(
            !changed.is_empty(),
            "input-changed Chrome frame had no ANSI bytes",
        )?;
        require(
            !changed.starts_with(CLEAR_SCREEN),
            "input-changed frame reset the terminal baseline instead of using the next generation",
        )?;
        require(
            initial != changed,
            "input mutation did not produce a changed terminal generation",
        )?;
    }
    require_full_redraw(resized, "resized Chrome frame")?;
    require(
        initial != resized,
        "terminal resize did not produce a distinct full-redraw generation",
    )
}

async fn run_quota_contract(executable: &Path) -> TestResult {
    let cancellation = Cancellation::new();
    let events = futures_util::stream::empty::<Result<InteractiveEvent, InteractiveError>>();
    let request = interactive_request(executable, MAX_QUOTA_ANSI_BYTES);
    let mut writer = FrameWriter::default();
    let result = match tokio::time::timeout(
        LOOP_DEADLINE,
        run_external_interactive(
            request,
            events,
            &mut writer,
            &cancellation,
            ExternalInteractiveLimits::default(),
        ),
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            return Err(failure(format!(
                "interactive Chrome quota contract exceeded {LOOP_DEADLINE:?}: {error}"
            )));
        }
    };

    match result {
        Err(ExternalInteractiveError::Frame(ExternalFrameError::Projection(
            SurfaceError::OutputLimit,
        ))) => {}
        Err(error) => {
            return Err(failure(format!(
                "interactive Chrome quota contract returned a different error: {error}"
            )));
        }
        Ok(report) => {
            return Err(failure(format!(
                "interactive Chrome quota contract completed with {:?} after {} frames",
                report.reason, report.frames_written
            )));
        }
    }
    require(
        writer.frames().is_empty(),
        "quota-rejected Chrome frame wrote partial ANSI to the in-memory terminal",
    )
}

fn interactive_request(executable: &Path, max_bytes: usize) -> ExternalRenderRequest {
    ExternalRenderRequest {
        target: fixture_target(),
        policies: ExternalEnginePolicies {
            executable_override: Some(executable.to_path_buf()),
            ..ExternalEnginePolicies::default()
        },
        viewport: ExternalViewport {
            width: 320,
            height: 180,
            device_scale: 1.0,
        },
        terminal_columns: INITIAL_COLUMNS,
        terminal_rows: INITIAL_ROWS,
        backend: Backend::Cells,
        capabilities: terminal_capabilities(),
        limits: ExternalRenderLimits {
            max_bytes,
            max_cells: MAX_TERMINAL_CELLS,
        },
    }
}

fn terminal_capabilities() -> TerminalCapabilities {
    TerminalCapabilities {
        color: ColorLevel::TrueColor,
        dumb: false,
    }
}

fn fixture_target() -> String {
    let escaped =
        url::form_urlencoded::byte_serialize(FIXTURE_DOCUMENT.as_bytes()).collect::<String>();
    format!("data:text/html,{escaped}")
}

fn frame_at<'a>(frames: &'a [Vec<u8>], index: usize, label: &str) -> TestResult<&'a [u8]> {
    match frames.get(index) {
        Some(frame) => Ok(frame.as_slice()),
        None => Err(failure(format!(
            "ANSI writer did not retain the {label} frame at index {index}"
        ))),
    }
}

fn require_full_redraw(frame: &[u8], label: &str) -> TestResult {
    require(
        frame.starts_with(CLEAR_SCREEN),
        format!("{label} did not begin with an ANSI full-redraw clear"),
    )
}

fn require(condition: bool, message: impl Into<String>) -> TestResult {
    if condition {
        Ok(())
    } else {
        Err(failure(message))
    }
}

fn box_error(error: impl Error + 'static) -> TestError {
    Box::new(error)
}

fn failure(message: impl Into<String>) -> TestError {
    Box::new(io::Error::other(message.into()))
}

#[derive(Default)]
struct FrameWriter {
    frames: Vec<Vec<u8>>,
}

impl FrameWriter {
    fn frames(&self) -> &[Vec<u8>] {
        &self.frames
    }
}

impl Write for FrameWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.frames.push(bytes.to_vec());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct CancellationAfterEvents {
    events: VecDeque<Result<InteractiveEvent, InteractiveError>>,
    cancellation: Cancellation,
    cancellation_sent: bool,
}

impl CancellationAfterEvents {
    fn new(
        events: VecDeque<Result<InteractiveEvent, InteractiveError>>,
        cancellation: Cancellation,
    ) -> Self {
        Self {
            events,
            cancellation,
            cancellation_sent: false,
        }
    }
}

impl Stream for CancellationAfterEvents {
    type Item = Result<InteractiveEvent, InteractiveError>;

    fn poll_next(mut self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(event) = self.events.pop_front() {
            return Poll::Ready(Some(event));
        }
        if !self.cancellation_sent {
            self.cancellation.cancel();
            self.cancellation_sent = true;
        }
        Poll::Pending
    }
}

fn run_isolated_test_child(root: &Path) -> TestResult {
    let executable = env::current_exe()?;
    let mut command = Command::new(executable);
    command
        .arg("--exact")
        .arg(TEST_NAME)
        .arg("--nocapture")
        .env(CHILD_ROOT_ENV, root)
        .env("TMPDIR", root)
        .env("TMP", root)
        .env("TEMP", root)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let child = command.spawn()?;
    wait_for_test_child(child)
}

fn wait_for_test_child(mut child: Child) -> TestResult {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                return Err(failure(format!(
                    "isolated interactive Chrome test child exited with {status}"
                )));
            }
            Ok(None) if started.elapsed() >= CHILD_DEADLINE => {
                return match stop_test_child(&mut child) {
                    Ok(status) => Err(failure(format!(
                        "isolated interactive Chrome test child exceeded {CHILD_DEADLINE:?} and ended with {status}"
                    ))),
                    Err(cleanup) => Err(failure(format!(
                        "isolated interactive Chrome test child exceeded {CHILD_DEADLINE:?}; child cleanup failed: {cleanup}"
                    ))),
                };
            }
            Ok(None) => thread::sleep(CHILD_POLL_INTERVAL),
            Err(error) => {
                return match stop_test_child(&mut child) {
                    Ok(status) => Err(failure(format!(
                        "could not poll isolated interactive Chrome test child: {error}; child ended with {status}"
                    ))),
                    Err(cleanup) => Err(failure(format!(
                        "could not poll isolated interactive Chrome test child: {error}; child cleanup failed: {cleanup}"
                    ))),
                };
            }
        }
    }
}

fn stop_test_child(child: &mut Child) -> io::Result<ExitStatus> {
    match child.kill() {
        Ok(()) => child.wait(),
        Err(error) if error.kind() == io::ErrorKind::InvalidInput => child.wait(),
        Err(error) => Err(error),
    }
}

struct TestRoot {
    path: PathBuf,
    removed: bool,
}

impl TestRoot {
    fn new() -> io::Result<Self> {
        let parent = env::temp_dir();
        fs::create_dir_all(&parent)?;
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        for attempt in 0..ROOT_CREATE_ATTEMPTS {
            let path = parent.join(format!(
                "termglide-interactive-frame-chrome-{}-{timestamp}-{attempt}",
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
            "could not allocate an isolated interactive Chrome test root",
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
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn assert_root_empty(root: &Path) -> TestResult {
    let mut entries = fs::read_dir(root)?;
    match entries.next() {
        None => Ok(()),
        Some(Ok(entry)) => Err(failure(format!(
            "interactive Chrome child left temporary state under {}: {}",
            root.display(),
            entry.path().display()
        ))),
        Some(Err(error)) => Err(box_error(error)),
    }
}

fn preserve_results(primary: TestResult, cleanup: TestResult) -> TestResult {
    match (primary, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(primary), Err(cleanup)) => Err(failure(format!(
            "interactive Chrome contract failed: {primary}; cleanup also failed: {cleanup}"
        ))),
    }
}
