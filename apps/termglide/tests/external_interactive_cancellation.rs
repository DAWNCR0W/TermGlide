//! Cancellation and cleanup coverage for the external interactive Chrome boundary.

use std::collections::{BTreeSet, HashMap};
use std::error::Error;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{Stream, stream};
use termglide::external::{
    ExternalEnginePolicies, ExternalRenderLimits, ExternalRenderRequest, ExternalViewport,
};
use termglide::interactive::{InteractiveError, InteractiveEvent};
use termglide::{
    ExternalInteractiveError, ExternalInteractiveExitReason, ExternalInteractiveLimits,
    ExternalInteractiveReport, run_external_interactive,
};
use tg_browser::{ExternalEngineError, discover_external_engine};
use tg_core::Cancellation;
use tg_terminal::{Backend, TerminalCapabilities};
use tokio::sync::Notify;

type TestResult = Result<(), Box<dyn Error>>;

const PRE_CANCELLED_TIMEOUT: Duration = Duration::from_secs(2);
const LIVE_CANCELLATION_TIMEOUT: Duration = Duration::from_secs(30);
const INVALID_EXECUTABLE_ATTEMPTS: u8 = 32;
const TERMGLIDE_PROFILE_PREFIX: &str = "termglide-cdp-";
const LOCAL_DOCUMENT_TARGET: &str = "data:text/html,%3C!doctype%20html%3E%3Cmeta%20charset%3Dutf-8%3E%3Cstyle%3Ehtml%2Cbody%7Bmargin%3A0%3Bbackground%3A%23000%3Bcolor%3A%23fff%3Bfont-family%3Amonospace%7D%3C%2Fstyle%3E%3Cmain%3Eexternal%20interactive%20cancellation%3C%2Fmain%3E";

static INVALID_EXECUTABLE_NONCE: AtomicU64 = AtomicU64::new(0);

#[tokio::test]
async fn external_interactive_cancellation_stops_before_launch_and_cleans_after_loop_entry()
-> TestResult {
    pre_cancelled_request_stops_before_launch().await?;
    live_chrome_cancellation_stops_a_pending_loop().await
}

async fn pre_cancelled_request_stops_before_launch() -> TestResult {
    let profiles_before = current_process_profiles()?;
    let invalid_executable = intentionally_invalid_executable()?;
    let cancellation = Cancellation::new();
    cancellation.cancel();
    let mut ansi = Vec::new();

    let operation = tokio::time::timeout(
        PRE_CANCELLED_TIMEOUT,
        run_external_interactive(
            external_request(Some(invalid_executable)),
            stream::pending::<Result<InteractiveEvent, InteractiveError>>(),
            &mut ansi,
            &cancellation,
            ExternalInteractiveLimits::default(),
        ),
    )
    .await
    .map_err(|_| timeout_error("pre-cancelled external interactive request"))?;
    let report = preserve_external_interactive_error(operation)?;

    assert_cancelled_before_launch(report)?;
    if !ansi.is_empty() {
        return Err(io::Error::other("pre-cancelled request wrote ANSI before launch").into());
    }
    assert_profiles_unchanged(&profiles_before, "pre-cancelled request")
}

async fn live_chrome_cancellation_stops_a_pending_loop() -> TestResult {
    let executable = match discover_external_engine(None) {
        Ok(path) => path,
        Err(ExternalEngineError::ExecutableNotFound) => return Ok(()),
        Err(error) => return Err(Box::new(error)),
    };
    if !executable.is_file() {
        return Err(io::Error::other("discovered executable is not a regular file").into());
    }

    let profiles_before = current_process_profiles()?;
    let cancellation = Cancellation::new();
    let loop_entered = Arc::new(AtomicBool::new(false));
    let pending_event = Arc::new(Notify::new());
    let events = PendingEventStream::new(Arc::clone(&loop_entered), Arc::clone(&pending_event));
    let mut ansi = Vec::new();
    let mut operation = Box::pin(run_external_interactive(
        external_request(Some(executable)),
        events,
        &mut ansi,
        &cancellation,
        ExternalInteractiveLimits::default(),
    ));
    let mut waiting_for_event = Box::pin(pending_event.notified());

    let operation = tokio::time::timeout(LIVE_CANCELLATION_TIMEOUT, async {
        tokio::select! {
            biased;
            () = waiting_for_event.as_mut() => {
                cancellation.cancel();
                operation.as_mut().await
            }
            result = operation.as_mut() => result,
        }
    })
    .await
    .map_err(|_| {
        cancellation.cancel();
        timeout_error("live external interactive cancellation")
    })?;
    let report = preserve_external_interactive_error(operation)?;

    if !loop_entered.load(Ordering::Acquire) {
        return Err(io::Error::other(
            "external interactive runner stopped before waiting for input",
        )
        .into());
    }
    if !cancellation.is_cancelled() {
        return Err(io::Error::other("caller cancellation did not fire after loop entry").into());
    }
    if report.reason != ExternalInteractiveExitReason::Cancelled {
        return Err(io::Error::other(format!(
            "external interactive runner exited for {:?}, not Cancelled",
            report.reason
        ))
        .into());
    }
    if report.events_processed != 0 || report.relaunches != 0 {
        return Err(io::Error::other(format!(
            "cancelled pending loop processed {} events and relaunched {} times",
            report.events_processed, report.relaunches
        ))
        .into());
    }
    assert_profiles_unchanged(&profiles_before, "live cancellation cleanup")
}

fn external_request(executable_override: Option<PathBuf>) -> ExternalRenderRequest {
    ExternalRenderRequest {
        target: LOCAL_DOCUMENT_TARGET.to_owned(),
        policies: ExternalEnginePolicies {
            executable_override,
            disable_images: true,
            disable_javascript: true,
            ..ExternalEnginePolicies::default()
        },
        viewport: ExternalViewport {
            width: 640,
            height: 360,
            device_scale: 1.0,
        },
        terminal_columns: 64,
        terminal_rows: 18,
        backend: Backend::Cells,
        capabilities: supported_terminal_capabilities(),
        limits: ExternalRenderLimits {
            max_bytes: 128 * 1024,
            max_cells: 4_096,
        },
    }
}

fn supported_terminal_capabilities() -> TerminalCapabilities {
    let environment = HashMap::from([
        ("TERM".to_owned(), "xterm-256color".to_owned()),
        ("COLORTERM".to_owned(), "truecolor".to_owned()),
    ]);
    TerminalCapabilities::from_environment(&environment)
}

fn intentionally_invalid_executable() -> io::Result<PathBuf> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let temp_dir = std::env::temp_dir();
    for attempt in 0..INVALID_EXECUTABLE_ATTEMPTS {
        let nonce = INVALID_EXECUTABLE_NONCE.fetch_add(1, Ordering::Relaxed);
        let candidate = temp_dir.join(format!(
            "termglide-external-interactive-invalid-executable-{}-{timestamp}-{nonce}-{attempt}",
            std::process::id()
        ));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate an intentionally invalid executable path",
    ))
}

fn current_process_profiles() -> io::Result<BTreeSet<PathBuf>> {
    let temp_dir = std::env::temp_dir();
    let entries = match fs::read_dir(&temp_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(error) => return Err(error),
    };
    let prefix = format!("{TERMGLIDE_PROFILE_PREFIX}{}-", std::process::id());
    let mut profiles = BTreeSet::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            profiles.insert(entry.path());
        }
    }
    Ok(profiles)
}

fn assert_profiles_unchanged(before: &BTreeSet<PathBuf>, label: &str) -> TestResult {
    let after = current_process_profiles()?;
    if &after == before {
        return Ok(());
    }
    let added = after.difference(before).collect::<Vec<_>>();
    let removed = before.difference(&after).collect::<Vec<_>>();
    Err(io::Error::other(format!(
        "{label} changed this test process's isolated Chrome profiles; added: {added:?}; removed: {removed:?}"
    ))
    .into())
}

fn assert_cancelled_before_launch(report: ExternalInteractiveReport) -> TestResult {
    if report.reason != ExternalInteractiveExitReason::Cancelled {
        return Err(io::Error::other(format!(
            "pre-cancelled request exited for {:?}, not Cancelled",
            report.reason
        ))
        .into());
    }
    if report.events_processed != 0 || report.frames_written != 0 || report.relaunches != 0 {
        return Err(io::Error::other(format!(
            "pre-cancelled request processed {} events, wrote {} frames, and relaunched {} times",
            report.events_processed, report.frames_written, report.relaunches
        ))
        .into());
    }
    Ok(())
}

fn preserve_external_interactive_error<T>(
    result: Result<T, ExternalInteractiveError>,
) -> Result<T, Box<dyn Error>> {
    result.map_err(|error| Box::new(error) as Box<dyn Error>)
}

fn timeout_error(label: &str) -> Box<dyn Error> {
    Box::new(io::Error::new(
        io::ErrorKind::TimedOut,
        format!("{label} exceeded its bounded wait"),
    ))
}

struct PendingEventStream {
    loop_entered: Arc<AtomicBool>,
    pending_event: Arc<Notify>,
}

impl PendingEventStream {
    fn new(loop_entered: Arc<AtomicBool>, pending_event: Arc<Notify>) -> Self {
        Self {
            loop_entered,
            pending_event,
        }
    }
}

impl Stream for PendingEventStream {
    type Item = Result<InteractiveEvent, InteractiveError>;

    fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if !this.loop_entered.swap(true, Ordering::AcqRel) {
            this.pending_event.notify_one();
        }
        Poll::Pending
    }
}
