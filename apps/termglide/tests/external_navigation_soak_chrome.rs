//! Bounded real-Chrome lifecycle coverage for repeated external navigation.
//!
//! This test deliberately tracks only observable ownership proxies (issued CDP exchanges,
//! projected frames, and temporary profile cardinality). Those bounds are useful regression
//! signals, but they are not a browser heap-usage measurement.

use std::error::Error;
use std::fs;
use std::io;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tg_browser::{
    ExternalEngineError, ExternalEngineLaunchOptions, ExternalEngineProcess,
    ExternalEngineViewport, discover_external_engine, launch_external_engine,
};
use tg_core::{Cancellation, SystemClock};
use tg_network::{CdpLimits, CdpSession, CdpTargetSession};
use tg_terminal::{
    Backend, ExternalFrameDecodeLimits, ExternalFrameOptions, ExternalFrameSequence,
    ExternalFrameSequenceLimits, ProjectionLimits, TerminalTransactionControl,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};
use url::Url;

type TestError = Box<dyn Error + Send + Sync>;
type TestResult<T = ()> = Result<T, TestError>;

const PRIMARY_NAVIGATIONS: usize = 4;
const RELAUNCH_NAVIGATIONS: usize = 1;
const MAX_TOTAL_NAVIGATIONS: usize = PRIMARY_NAVIGATIONS + RELAUNCH_NAVIGATIONS;
const MAX_LAUNCHES: usize = 2;
const MAX_FRAMES: usize = MAX_TOTAL_NAVIGATIONS + 3;
const MAX_CDP_MESSAGES: usize = 128;
const MAX_LOOPBACK_REQUESTS: usize = 32;
const MAX_ACTIVE_PROFILES: usize = 1;
const TOTAL_DEADLINE: Duration = Duration::from_secs(75);
const SERVER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const SERVER_REQUEST_TIMEOUT: Duration = Duration::from_secs(1);
const TEMP_ROOT_ATTEMPTS: u8 = 32;
const TERMGLIDE_PROFILE_PREFIX: &str = "termglide-cdp-";
const LOOPBACK_RESPONSE_MAX_REQUEST_BYTES: usize = 8 * 1024;
const MAX_CDP_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
const MAX_CDP_EVENTS: usize = 128;
const MAX_CDP_PENDING_RESPONSES: usize = 32;
const CDP_OPERATION_TIMEOUT: Duration = Duration::from_secs(10);
const MUTATION_READY_ATTEMPTS: usize = 4;
const MUTATION_READY_INTERVAL: Duration = Duration::from_millis(25);
const FRAME_MAX_BYTES: usize = 128 * 1024;
const FRAME_MAX_OPERATIONS: usize = 4_096;
const FRAME_MAX_BASE64_BYTES: usize = 2 * 1024 * 1024;
const FRAME_MAX_PNG_BYTES: usize = 1024 * 1024;
const FRAME_MAX_DECODED_BYTES: usize = 512 * 1024;

const DATA_DOCUMENT: &str = r#"<!doctype html>
<meta charset="utf-8">
<main id="soak-marker">data baseline</main>
<script>window.__termglideSoakSource = "data";</script>"#;
const LOOPBACK_DOCUMENT: &str = r#"<!doctype html>
<meta charset="utf-8">
<main id="soak-marker">loopback baseline</main>
<script>window.__termglideSoakSource = "loopback";</script>"#;

#[tokio::test]
async fn external_navigation_soak_reuses_target_then_relaunches_cleanly() -> TestResult {
    let executable = match discover_external_engine(None) {
        Ok(path) => path,
        Err(ExternalEngineError::ExecutableNotFound) => return Ok(()),
        Err(error) => return Err(box_error(error)),
    };
    if !executable.is_file() {
        return Err(failure("discovered external engine is not a regular file"));
    }

    let mut root = TestRoot::new()?;
    let mut server = LoopbackServer::start().await?;
    let operation = run_soak(&executable, root.path(), server.url()).await;
    let server_cleanup = server.shutdown().await;
    let root_cleanup = root.verify_empty_and_remove();
    preserve_results(preserve_results(operation, server_cleanup), root_cleanup)
}

async fn run_soak(executable: &Path, profile_root: &Path, loopback: &Url) -> TestResult {
    let data = data_url()?;
    let options = launch_options(executable, profile_root)?;
    let mut budget = SoakBudget::new();

    let first_profile = launch_and_exercise(
        &options,
        profile_root,
        &data,
        loopback,
        PRIMARY_NAVIGATIONS,
        true,
        &mut budget,
    )
    .await?;
    let second_profile = launch_and_exercise(
        &options,
        profile_root,
        &data,
        loopback,
        RELAUNCH_NAVIGATIONS,
        false,
        &mut budget,
    )
    .await?;
    if first_profile == second_profile {
        return Err(failure(
            "supervised relaunch unexpectedly reused the prior isolated profile path",
        ));
    }

    assert_cancelled_launch(&options, profile_root, &mut budget)?;
    budget.finish()?;
    assert_profile_count(profile_root, 0)?;
    Ok(())
}

fn launch_options(
    executable: &Path,
    profile_root: &Path,
) -> Result<ExternalEngineLaunchOptions, TestError> {
    let mut options = ExternalEngineLaunchOptions::new(Url::parse("about:blank")?);
    options.executable_override = Some(executable.to_path_buf());
    options.viewport = Some(ExternalEngineViewport::new(320, 180).map_err(box_error)?);
    options.startup_timeout = Duration::from_secs(15);
    options.poll_interval = Duration::from_millis(25);
    options.temporary_profile_parent = Some(profile_root.to_path_buf());
    Ok(options)
}

async fn launch_and_exercise(
    options: &ExternalEngineLaunchOptions,
    profile_root: &Path,
    data: &Url,
    loopback: &Url,
    navigations: usize,
    verify_stable_frame: bool,
    budget: &mut SoakBudget,
) -> Result<PathBuf, TestError> {
    budget.check_deadline("before external browser launch")?;
    let cancellation = Cancellation::new();
    let mut process = launch_external_engine(options, &cancellation).map_err(box_error)?;
    budget.record_launch()?;

    let profile = process.isolated_profile_dir().to_path_buf();
    if !profile.is_dir() {
        let cleanup = shutdown_process(process, &profile, profile_root);
        return preserve_results(
            Err(failure(
                "external browser did not create an isolated profile",
            )),
            cleanup,
        )
        .map(|_| profile);
    }
    assert_profile_count(profile_root, MAX_ACTIVE_PROFILES)?;
    let endpoint = process.endpoint().websocket_url();
    let phase = exercise_reused_target(
        &endpoint,
        data,
        loopback,
        navigations,
        verify_stable_frame,
        budget,
    )
    .await;
    let liveness = require_process_running(&mut process);
    let cleanup = shutdown_process(process, &profile, profile_root);
    preserve_results(preserve_results(phase, liveness), cleanup)?;
    budget.check_deadline("after external browser shutdown")?;
    Ok(profile)
}

async fn exercise_reused_target(
    endpoint: &str,
    data: &Url,
    loopback: &Url,
    navigations: usize,
    verify_stable_frame: bool,
    budget: &mut SoakBudget,
) -> TestResult {
    let limits = cdp_limits();
    let max_events = limits.max_events;
    let mut connection = CdpSession::connect(endpoint, limits)
        .await
        .map_err(box_error)?;
    let operation = async {
        let version = connection.browser_version().await?;
        budget.record_cdp_messages(2)?;
        if version.product.is_empty() || version.protocol_version.is_empty() {
            return Err(failure("Browser.getVersion returned empty bounded metadata"));
        }

        {
            let mut target = connection.attach_first_page().await?;
            budget.record_cdp_messages(4)?;
            target.page_enable().await?;
            budget.record_cdp_messages(2)?;

            let session_id = target.session_id().to_owned();
            if session_id.is_empty() {
                return Err(failure("attached target session ID was empty"));
            }
            let cancellation = Cancellation::new();
            let clock = SystemClock::default();
            let mut sequence = ExternalFrameSequence::new();
            let mut previous_viewport = None;

            for _ in 0..navigations {
                let iteration = budget.next_iteration()?;
                let viewport = viewport_for(iteration);
                let control = TerminalTransactionControl::new(&cancellation, &clock, None);
                if let Some(previous) = previous_viewport {
                    let resized = sequence.resize_controlled(
                        viewport.columns,
                        viewport.rows,
                        &control,
                    )?;
                    if previous != viewport && !resized {
                        return Err(failure(
                            "terminal frame sequence did not invalidate its prior geometry on resize",
                        ));
                    }
                    if resized {
                        budget.record_resize()?;
                    }
                }

                target
                    .set_device_metrics(viewport.width, viewport.height, 1.0)
                    .await?;
                budget.record_cdp_messages(2)?;
                let url = if iteration.is_multiple_of(2) {
                    loopback
                } else {
                    data
                };
                let navigation = target.navigate(url.as_str()).await?;
                budget.record_cdp_messages(2)?;
                if let Some(message) = navigation.error_text {
                    return Err(failure(format!(
                        "generic soak navigation {iteration} failed: {message}"
                    )));
                }
                target.wait_for_load().await?;
                budget.record_cdp_messages(1)?;
                if target.session_id() != session_id {
                    return Err(failure(
                        "same-process navigation replaced the attached target session unexpectedly",
                    ));
                }

                mutate_dom(&mut target, iteration, budget).await?;
                let root = target.dom_get_document().await?;
                budget.record_cdp_messages(2)?;
                let outer_html = target.dom_get_outer_html(root).await?;
                budget.record_cdp_messages(2)?;
                let expected = format!("iteration-{iteration}");
                if !outer_html.contains("soak-marker") || !outer_html.contains(&expected) {
                    return Err(failure(format!(
                        "typed DOM read did not preserve the script mutation for iteration {iteration}"
                    )));
                }

                let screenshot = target.capture_screenshot().await?;
                budget.record_cdp_messages(2)?;
                if screenshot.data.is_empty() {
                    return Err(failure("CDP screenshot payload was empty"));
                }
                let frame = project_frame(&mut sequence, &screenshot.data, viewport, &control)?;
                if frame.is_identical() || frame.operations().is_empty() || frame.output().is_empty()
                {
                    return Err(failure(format!(
                        "mutated soak frame {iteration} did not produce a bounded terminal update"
                    )));
                }
                budget.record_frame(frame.operations().len(), frame.output().len())?;
                if sequence.previous().is_none() {
                    return Err(failure("terminal frame sequence lost its only retained baseline"));
                }
                previous_viewport = Some(viewport);
                budget.check_deadline("after navigation iteration")?;
            }

            if verify_stable_frame {
                let viewport = previous_viewport.ok_or_else(|| {
                    failure("primary soak phase completed without a viewport or projected frame")
                })?;
                let control = TerminalTransactionControl::new(&cancellation, &clock, None);
                let screenshot = target.capture_screenshot().await?;
                budget.record_cdp_messages(2)?;
                let repeated = project_frame(&mut sequence, &screenshot.data, viewport, &control)?;
                if !repeated.is_identical()
                    || !repeated.operations().is_empty()
                    || !repeated.output().is_empty()
                {
                    return Err(failure(
                        "an unchanged external screenshot grew the terminal projection stream",
                    ));
                }
                budget.record_frame(repeated.operations().len(), repeated.output().len())?;
            }
        }

        if connection.queued_event_count() >= max_events {
            return Err(failure("CDP event queue reached its configured bounded capacity"));
        }
        budget.check_deadline("after same-process target reuse")
    }
    .await;
    let close = connection.close().await.map_err(box_error);
    preserve_results(operation, close)
}

async fn mutate_dom(
    target: &mut CdpTargetSession<'_>,
    iteration: usize,
    budget: &mut SoakBudget,
) -> TestResult {
    let color = if iteration.is_multiple_of(2) {
        "#a52a2a"
    } else {
        "#0033cc"
    };
    let expected = format!("iteration-{iteration}");
    let expression = format!(
        "(() => {{ const marker = document.getElementById('soak-marker'); if (!marker) return {{ ready: false, state: document.readyState, href: location.href, html: document.documentElement?.outerHTML.slice(0, 192) }}; document.body.style.cssText = 'margin:0;padding:20px;background:{color};color:#fff;font:28px monospace'; marker.dataset.iteration = '{iteration}'; marker.textContent = '{expected}'; return {{ ready: true, value: marker.textContent }}; }})()"
    );
    let mut last_result = Value::Null;
    let mut last_exception = None;
    for attempt in 0..MUTATION_READY_ATTEMPTS {
        let result = target.runtime_evaluate(&expression, true).await?;
        budget.record_cdp_messages(2)?;
        let value = result.result.get("value");
        if result.exception_details.is_none()
            && value
                .and_then(|value| value.get("ready"))
                .and_then(Value::as_bool)
                == Some(true)
            && value
                .and_then(|value| value.get("value"))
                .and_then(Value::as_str)
                == Some(expected.as_str())
        {
            return Ok(());
        }
        last_result = result.result;
        last_exception = result.exception_details;
        if attempt + 1 < MUTATION_READY_ATTEMPTS {
            sleep(MUTATION_READY_INTERVAL).await;
        }
    }
    let observation = format!("result={last_result}; exception={last_exception:?}")
        .chars()
        .take(512)
        .collect::<String>();
    Err(failure(format!(
        "script mutation did not return the expected iteration marker {expected} after {MUTATION_READY_ATTEMPTS} bounded attempts: {observation}"
    )))
}

fn project_frame(
    sequence: &mut ExternalFrameSequence,
    screenshot: &str,
    viewport: SoakViewport,
    control: &TerminalTransactionControl<'_>,
) -> Result<tg_terminal::ExternalFrameSequenceOutput, TestError> {
    sequence
        .push_png_base64_controlled(
            screenshot,
            ExternalFrameOptions::new(
                Backend::Cells,
                viewport.columns,
                viewport.rows,
                frame_decode_limits(),
                projection_limits(viewport),
                ExternalFrameSequenceLimits {
                    max_operations: FRAME_MAX_OPERATIONS,
                    max_output_bytes: FRAME_MAX_BYTES,
                },
            ),
            control,
        )
        .map_err(box_error)
}

fn frame_decode_limits() -> ExternalFrameDecodeLimits {
    ExternalFrameDecodeLimits {
        max_base64_bytes: FRAME_MAX_BASE64_BYTES,
        max_png_bytes: FRAME_MAX_PNG_BYTES,
        max_width: 512,
        max_height: 320,
        max_pixels: 512 * 320,
        max_decoded_bytes: FRAME_MAX_DECODED_BYTES,
    }
}

fn projection_limits(viewport: SoakViewport) -> ProjectionLimits {
    ProjectionLimits {
        max_cells: usize::from(viewport.columns) * usize::from(viewport.rows),
        // The soak's two device-metric viewports are at most 400x240; keep this independent of
        // platform-sized conversions while leaving a small, explicit decode margin.
        max_pixels: 512 * 320,
        max_output_bytes: FRAME_MAX_BYTES,
    }
}

fn cdp_limits() -> CdpLimits {
    CdpLimits {
        max_message_bytes: MAX_CDP_MESSAGE_BYTES,
        max_events: MAX_CDP_EVENTS,
        max_pending_responses: MAX_CDP_PENDING_RESPONSES,
        operation_timeout: CDP_OPERATION_TIMEOUT,
        ..CdpLimits::default()
    }
}

fn viewport_for(iteration: usize) -> SoakViewport {
    if iteration.is_multiple_of(2) {
        SoakViewport {
            width: 400,
            height: 240,
            columns: 50,
            rows: 15,
        }
    } else {
        SoakViewport {
            width: 320,
            height: 180,
            columns: 40,
            rows: 12,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SoakViewport {
    width: u32,
    height: u32,
    columns: u16,
    rows: u16,
}

#[derive(Debug)]
struct SoakBudget {
    started: Instant,
    iterations: usize,
    launches: usize,
    frames: usize,
    cdp_messages: usize,
    resizes: usize,
}

impl SoakBudget {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            iterations: 0,
            launches: 0,
            frames: 0,
            cdp_messages: 0,
            resizes: 0,
        }
    }

    fn check_deadline(&self, step: &str) -> TestResult {
        if self.started.elapsed() > TOTAL_DEADLINE {
            return Err(failure(format!(
                "external navigation soak exceeded its total deadline while {step}"
            )));
        }
        Ok(())
    }

    fn next_iteration(&mut self) -> Result<usize, TestError> {
        self.check_deadline("allocating the next navigation iteration")?;
        let next = self
            .iterations
            .checked_add(1)
            .ok_or_else(|| failure("navigation iteration counter overflowed"))?;
        if next > MAX_TOTAL_NAVIGATIONS {
            return Err(failure("navigation iteration budget was exceeded"));
        }
        self.iterations = next;
        Ok(next)
    }

    fn record_launch(&mut self) -> TestResult {
        self.launches = self
            .launches
            .checked_add(1)
            .ok_or_else(|| failure("external launch counter overflowed"))?;
        if self.launches > MAX_LAUNCHES {
            return Err(failure("external launch budget was exceeded"));
        }
        self.check_deadline("recording an external browser launch")
    }

    fn record_cdp_messages(&mut self, count: usize) -> TestResult {
        self.cdp_messages = self
            .cdp_messages
            .checked_add(count)
            .ok_or_else(|| failure("CDP message proxy counter overflowed"))?;
        if self.cdp_messages > MAX_CDP_MESSAGES {
            return Err(failure("bounded CDP message proxy was exceeded"));
        }
        Ok(())
    }

    fn record_frame(&mut self, operations: usize, output_bytes: usize) -> TestResult {
        if operations > FRAME_MAX_OPERATIONS || output_bytes > FRAME_MAX_BYTES {
            return Err(failure(
                "terminal frame exceeded its explicit projection allowance",
            ));
        }
        self.frames = self
            .frames
            .checked_add(1)
            .ok_or_else(|| failure("projected frame counter overflowed"))?;
        if self.frames > MAX_FRAMES {
            return Err(failure("projected frame budget was exceeded"));
        }
        Ok(())
    }

    fn record_resize(&mut self) -> TestResult {
        self.resizes = self
            .resizes
            .checked_add(1)
            .ok_or_else(|| failure("resize counter overflowed"))?;
        Ok(())
    }

    fn finish(&self) -> TestResult {
        self.check_deadline("finishing the external navigation soak")?;
        if self.iterations != MAX_TOTAL_NAVIGATIONS {
            return Err(failure(format!(
                "navigation iterations were {}, expected {MAX_TOTAL_NAVIGATIONS}",
                self.iterations
            )));
        }
        if self.launches != MAX_LAUNCHES {
            return Err(failure(format!(
                "external launches were {}, expected {MAX_LAUNCHES}",
                self.launches
            )));
        }
        if self.frames == 0 || self.resizes == 0 {
            return Err(failure(
                "soak did not observe both bounded projected frames and resize invalidation",
            ));
        }
        Ok(())
    }
}

fn assert_cancelled_launch(
    options: &ExternalEngineLaunchOptions,
    profile_root: &Path,
    budget: &mut SoakBudget,
) -> TestResult {
    budget.check_deadline("checking a cancelled launch")?;
    let cancellation = Cancellation::new();
    cancellation.cancel();
    match launch_external_engine(options, &cancellation) {
        Err(ExternalEngineError::Cancelled) => assert_profile_count(profile_root, 0),
        Err(error) => Err(box_error(error)),
        Ok(process) => {
            let profile = process.isolated_profile_dir().to_path_buf();
            let cleanup = shutdown_process(process, &profile, profile_root);
            preserve_results(
                Err(failure(
                    "cancelled external launch unexpectedly started a browser",
                )),
                cleanup,
            )
        }
    }
}

fn require_process_running(process: &mut ExternalEngineProcess) -> TestResult {
    match process.try_wait().map_err(box_error)? {
        None => Ok(()),
        Some(status) => Err(failure(format!(
            "external browser exited before supervised shutdown: {status}"
        ))),
    }
}

fn shutdown_process(
    mut process: ExternalEngineProcess,
    profile: &Path,
    profile_root: &Path,
) -> TestResult {
    let shutdown = process.shutdown().map_err(box_error);
    drop(process);
    preserve_results(
        shutdown,
        preserve_results(
            require_profile_removed(profile),
            assert_profile_count(profile_root, 0),
        ),
    )
}

fn require_profile_removed(profile: &Path) -> TestResult {
    if profile.exists() {
        Err(failure(format!(
            "external browser profile remained after supervised cleanup: {}",
            profile.display()
        )))
    } else {
        Ok(())
    }
}

fn assert_profile_count(root: &Path, expected: usize) -> TestResult {
    let entries = fs::read_dir(root)?
        .collect::<Result<Vec<_>, io::Error>>()?
        .into_iter()
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    let profiles = entries
        .iter()
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(TERMGLIDE_PROFILE_PREFIX))
        })
        .collect::<Vec<_>>();
    if profiles.len() > MAX_ACTIVE_PROFILES || profiles.len() != expected {
        return Err(failure(format!(
            "isolated profile count was {}, expected {expected} with maximum {MAX_ACTIVE_PROFILES}: {}",
            profiles.len(),
            display_paths(&entries)
        )));
    }
    if entries.len() != profiles.len() {
        return Err(failure(format!(
            "external profile root retained non-profile entries: {}",
            display_paths(&entries)
        )));
    }
    Ok(())
}

fn data_url() -> Result<Url, url::ParseError> {
    let encoded = url::form_urlencoded::byte_serialize(DATA_DOCUMENT.as_bytes())
        .collect::<String>()
        .replace('+', "%20");
    Url::parse(&format!("data:text/html,{encoded}"))
}

struct LoopbackServer {
    url: Url,
    requests: Arc<AtomicUsize>,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl LoopbackServer {
    async fn start() -> TestResult<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let url = Url::parse(&format!("http://{address}/soak"))?;
        let requests = Arc::new(AtomicUsize::new(0));
        let response = Arc::new(loopback_response());
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        let task_requests = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else {
                            break;
                        };
                        serve_loopback(
                            stream,
                            Arc::clone(&response),
                            Arc::clone(&task_requests),
                        ).await;
                    }
                }
            }
        });
        Ok(Self {
            url,
            requests,
            shutdown: Some(shutdown_tx),
            task: Some(task),
        })
    }

    fn url(&self) -> &Url {
        &self.url
    }

    async fn shutdown(&mut self) -> TestResult {
        if let Some(shutdown) = self.shutdown.take() {
            let _ignored = shutdown.send(());
        }
        let stopped = {
            let Some(task) = self.task.as_mut() else {
                return Ok(());
            };
            match timeout(SERVER_SHUTDOWN_TIMEOUT, &mut *task).await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => Err(box_error(error)),
                Err(_) => {
                    task.abort();
                    Err(failure(
                        "loopback fixture did not stop within its bounded deadline",
                    ))
                }
            }
        };
        self.task = None;
        stopped?;

        let requests = self.requests.load(Ordering::SeqCst);
        if requests == 0 || requests > MAX_LOOPBACK_REQUESTS {
            return Err(failure(format!(
                "loopback request proxy was {requests}, outside 1..={MAX_LOOPBACK_REQUESTS}"
            )));
        }
        Ok(())
    }
}

impl Drop for LoopbackServer {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn serve_loopback(mut stream: TcpStream, response: Arc<Vec<u8>>, requests: Arc<AtomicUsize>) {
    let mut request = [0_u8; LOOPBACK_RESPONSE_MAX_REQUEST_BYTES];
    let read = match timeout(SERVER_REQUEST_TIMEOUT, stream.read(&mut request)).await {
        Ok(Ok(read)) => read,
        Ok(Err(_)) | Err(_) => return,
    };
    if read == 0 {
        return;
    }
    if stream.write_all(&response).await.is_err() || stream.shutdown().await.is_err() {
        return;
    }
    requests.fetch_add(1, Ordering::SeqCst);
}

fn loopback_response() -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        LOOPBACK_DOCUMENT.len()
    )
    .into_bytes();
    response.extend_from_slice(LOOPBACK_DOCUMENT.as_bytes());
    response
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
                "termglide-external-navigation-soak-{}-{timestamp}-{attempt}",
                std::process::id()
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
            "could not allocate a unique external navigation soak root",
        ))
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn verify_empty_and_remove(&mut self) -> TestResult {
        assert_profile_count(&self.path, 0)?;
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

fn preserve_results(primary: TestResult, cleanup: TestResult) -> TestResult {
    match (primary, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(primary), Err(cleanup)) => Err(failure(format!(
            "primary failure: {primary}; cleanup failure: {cleanup}"
        ))),
    }
}

fn display_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn box_error(error: impl Error + Send + Sync + 'static) -> TestError {
    Box::new(error)
}

fn failure(message: impl Into<String>) -> TestError {
    Box::new(io::Error::other(message.into()))
}
