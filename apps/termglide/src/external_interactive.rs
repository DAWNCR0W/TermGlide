//! Interactive external-Chrome loop ownership for terminal hosts.
//!
//! The loop launches one isolated Chrome profile, keeps CDP attached to its first page target,
//! projects bounded screenshots into incremental ANSI, and forwards only decoded terminal input.
//! It intentionally does not own CLI parsing, persistent profiles, credentials, or raw terminal
//! byte handling.

use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

use futures_util::{Stream, StreamExt};
use tg_browser::{
    ExternalEngineError, ExternalEngineImagePolicy, ExternalEngineJavaScriptPolicy,
    ExternalEngineLaunchOptions, ExternalEngineProcess, ExternalEngineProxy,
    ExternalEngineProxyBypass, ExternalEngineViewport, ExternalInputAdapter, ExternalInputError,
    ExternalInputGeometry, ShellInputEvent, launch_external_engine,
};
use tg_core::{Cancellation, SystemClock};
use tg_network::{CdpError, CdpLimits, CdpSession};
use tg_platform::{PlatformError, TerminalGuard, shutdown_signal};
use tg_terminal::{
    Backend, ExternalFrameDecodeLimits, ExternalFrameError, ExternalFrameOptions,
    ExternalFrameSequence, ExternalFrameSequenceLimits, ExternalFrameSequenceOutput, InputEvent,
    KeyCode, KeyEventKind, ProjectionLimits, Surface, TerminalBackendFeedback,
    TerminalCapabilities, TerminalTransactionControl,
};
use thiserror::Error;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use url::Url;

use crate::external::{ExternalEnginePolicies, ExternalRenderRequest};
use crate::interactive::{
    InteractiveError, InteractiveEvent, UnsupportedInputFeature, external_crossterm_events,
};

const ABOUT_BLANK: &str = "about:blank";

/// Bounded configuration for one interactive external-Chrome journey.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalInteractiveLimits {
    /// Maximum decoded interactive events accepted before the runner exits with an error.
    pub max_events: usize,
    /// Bounded CDP transport limits for the loopback DevTools connection.
    pub cdp: CdpLimits,
    /// Bounds applied while decoding each Chrome screenshot.
    pub frame_decode: ExternalFrameDecodeLimits,
    /// Bounds applied while projecting each Chrome screenshot to terminal cells.
    pub projection: ProjectionLimits,
    /// Bounds applied while producing each incremental ANSI frame.
    pub sequence: ExternalFrameSequenceLimits,
    /// Interval used to notice a genuinely exited external browser process.
    pub crash_poll_interval: Duration,
    /// One real relaunch is supported after an observed external-browser exit.
    pub max_relaunches: usize,
}

impl Default for ExternalInteractiveLimits {
    fn default() -> Self {
        Self {
            max_events: 1_000_000,
            cdp: CdpLimits::default(),
            frame_decode: ExternalFrameDecodeLimits::BROWSER_DEFAULT,
            projection: ProjectionLimits::default(),
            sequence: ExternalFrameSequenceLimits::BROWSER_DEFAULT,
            crash_poll_interval: Duration::from_millis(250),
            max_relaunches: 1,
        }
    }
}

impl ExternalInteractiveLimits {
    fn validate(&self) -> Result<(), ExternalInteractiveError> {
        if self.max_events == 0 || self.crash_poll_interval.is_zero() || self.max_relaunches > 1 {
            return Err(ExternalInteractiveError::InvalidLimits);
        }
        validate_frame_limits(self.frame_decode, self.projection, self.sequence)
    }
}

/// Why an interactive external-Chrome runner stopped normally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalInteractiveExitReason {
    Quit,
    EndOfInput,
    Cancelled,
}

/// Bounded completion metadata for one interactive external-Chrome journey.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExternalInteractiveReport {
    pub reason: ExternalInteractiveExitReason,
    pub events_processed: usize,
    pub frames_written: usize,
    pub relaunches: usize,
}

/// An interactive event that cannot authorize a CDP input operation in this loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalInteractiveUnsupportedEvent {
    InteractiveFeature(UnsupportedInputFeature),
    ImeComposition,
}

/// Cleanup failures retained when a primary external-interactive operation also fails.
#[derive(Debug, Error)]
pub enum ExternalInteractiveCleanupError {
    #[error("failed to close the CDP session: {0}")]
    CdpClose(#[source] CdpError),
    #[error("failed to shut down the external browser: {0}")]
    BrowserShutdown(#[source] ExternalEngineError),
    #[error("external browser shutdown task failed: {message}")]
    BrowserShutdownTask { message: String },
    #[error("external browser profile remained after shutdown: {profile_dir}")]
    ProfileNotRemoved { profile_dir: PathBuf },
    #[error("failed to close CDP ({cdp}) and clean up the browser ({browser})")]
    CdpAndBrowserShutdown {
        cdp: CdpError,
        browser: Box<ExternalInteractiveCleanupError>,
    },
}

/// Errors confined to the interactive external-Chrome boundary.
#[derive(Debug, Error)]
pub enum ExternalInteractiveError {
    #[error(
        "external interactive limits are zero, inconsistent, or exceed the one-relaunch policy"
    )]
    InvalidLimits,
    #[error("external interactive request has invalid {field}")]
    InvalidRequest { field: &'static str },
    #[error("external interactive target URL contains credentials")]
    TargetCredentialsDisallowed,
    #[error("proxy bypass entries require an explicit proxy endpoint")]
    ProxyBypassWithoutProxy,
    #[error("external interactive rendering requires a cell backend, but selected {backend:?}")]
    UnsupportedBackend { backend: Backend },
    #[error("external interactive event is unsupported: {event:?}")]
    UnsupportedEvent {
        event: ExternalInteractiveUnsupportedEvent,
    },
    #[error("external interactive resize cannot be represented safely")]
    ResizeOverflow,
    #[error("external interactive ANSI output used {actual} bytes, exceeding {maximum}")]
    OutputLimit { actual: usize, maximum: usize },
    #[error("external interactive event quota was exhausted")]
    EventLimit,
    #[error("external interactive counter space was exhausted")]
    CounterExhausted,
    #[error("external interactive lifecycle was cancelled")]
    Cancelled,
    #[error("external interactive navigation failed: {message}")]
    NavigationFailed { message: String },
    #[error("external interactive recovery exhausted its {max_relaunches} relaunch budget")]
    RecoveryExhausted { max_relaunches: usize },
    #[error("external interactive recovery cleanup failed: {cleanup}")]
    RecoveryCleanup {
        cleanup: Box<ExternalInteractiveCleanupError>,
    },
    #[error("external interactive state was unavailable")]
    StateUnavailable,
    #[error("external interactive launch task failed: {message}")]
    BlockingTask { message: String },
    #[error("external interactive URL is invalid: {0}")]
    Url(#[from] url::ParseError),
    #[error("external interactive browser lifecycle failed: {0}")]
    Engine(#[from] ExternalEngineError),
    #[error("external interactive CDP operation failed: {0}")]
    Cdp(#[from] CdpError),
    #[error("external interactive frame projection failed: {0}")]
    Frame(#[from] ExternalFrameError),
    #[error("external interactive input dispatch failed: {0}")]
    Input(#[from] ExternalInputError),
    #[error("external interactive event source failed: {0}")]
    Interactive(#[source] Box<InteractiveError>),
    #[error("external interactive terminal I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("external interactive terminal operation failed: {0}")]
    Platform(#[from] PlatformError),
    #[error("external interactive operation failed: {operation}; cleanup also failed: {cleanup}")]
    OperationAndCleanup {
        operation: Box<ExternalInteractiveError>,
        cleanup: Box<ExternalInteractiveCleanupError>,
    },
    #[error("external interactive cleanup failed: {0}")]
    Cleanup(#[source] ExternalInteractiveCleanupError),
    #[error(
        "external interactive operation failed: {operation}; terminal restore also failed: {restore}"
    )]
    OperationAndRestore {
        operation: Box<ExternalInteractiveError>,
        restore: PlatformError,
    },
    #[error("external interactive terminal restore failed: {0}")]
    TerminalRestore(#[source] PlatformError),
}

impl From<InteractiveError> for ExternalInteractiveError {
    fn from(error: InteractiveError) -> Self {
        Self::Interactive(Box::new(error))
    }
}

/// Bounded, generation-aware screenshot projection owned by one interactive Chrome session.
///
/// This adapter neither opens Chrome nor decodes PNG itself. It delegates caller-supplied CDP
/// screenshot data to [`ExternalFrameSequence`], retaining only the last accepted cell baseline.
/// The live runner uses [`Self::project_next_png_base64_controlled`] so every captured frame has a
/// strictly increasing local generation; the explicit-generation entry point is available for
/// callers that already own a frame ordering boundary.
#[derive(Debug)]
pub struct ExternalInteractiveFrameAdapter {
    capabilities: TerminalCapabilities,
    requested_backend: Backend,
    selected_backend: Backend,
    columns: u16,
    rows: u16,
    decode_limits: ExternalFrameDecodeLimits,
    projection_limits: ProjectionLimits,
    sequence_limits: ExternalFrameSequenceLimits,
    max_ansi_bytes: usize,
    sequence: ExternalFrameSequence,
    issued_generation: u64,
}

impl ExternalInteractiveFrameAdapter {
    /// Creates an in-memory projection adapter for one caller-owned interactive terminal.
    pub fn new(
        capabilities: TerminalCapabilities,
        requested_backend: Backend,
        columns: u16,
        rows: u16,
        decode_limits: ExternalFrameDecodeLimits,
        projection_limits: ProjectionLimits,
        sequence_limits: ExternalFrameSequenceLimits,
    ) -> Result<Self, ExternalInteractiveError> {
        if columns == 0 || rows == 0 {
            return Err(ExternalInteractiveError::InvalidRequest {
                field: "terminal geometry",
            });
        }
        validate_frame_limits(decode_limits, projection_limits, sequence_limits)?;
        let selected_backend =
            capabilities.select_safe_backend(requested_backend, TerminalBackendFeedback::Initial);
        if !matches!(
            selected_backend,
            Backend::Cells | Backend::Halfblock | Backend::Quadrant | Backend::Braille
        ) {
            return Err(ExternalInteractiveError::UnsupportedBackend {
                backend: selected_backend,
            });
        }
        let max_ansi_bytes = projection_limits
            .max_output_bytes
            .min(sequence_limits.max_output_bytes);
        if max_ansi_bytes == 0 {
            return Err(ExternalInteractiveError::InvalidLimits);
        }
        let projection_limits = ProjectionLimits {
            max_output_bytes: max_ansi_bytes,
            ..projection_limits
        };
        let sequence_limits = ExternalFrameSequenceLimits {
            max_output_bytes: max_ansi_bytes,
            ..sequence_limits
        };

        Ok(Self {
            capabilities,
            requested_backend,
            selected_backend,
            columns,
            rows,
            decode_limits,
            projection_limits,
            sequence_limits,
            max_ansi_bytes,
            sequence: ExternalFrameSequence::new(),
            issued_generation: 0,
        })
    }

    /// Returns the capability-selected cell projection backend.
    pub const fn selected_backend(&self) -> Backend {
        self.selected_backend
    }

    /// Returns the active terminal cell geometry.
    pub const fn columns(&self) -> u16 {
        self.columns
    }

    /// Returns the active terminal cell geometry.
    pub const fn rows(&self) -> u16 {
        self.rows
    }

    /// Returns the last accepted generation from the underlying bounded frame sequence.
    pub const fn latest_generation(&self) -> Option<u64> {
        self.sequence.latest_generation()
    }

    /// Returns the last known-good cell projection, if one has been accepted.
    pub fn previous(&self) -> Option<&Surface> {
        self.sequence.previous()
    }

    /// Invalidates the retained terminal baseline after an already-applied viewport resize.
    pub fn resize_controlled(
        &mut self,
        columns: u16,
        rows: u16,
        control: &TerminalTransactionControl<'_>,
    ) -> Result<bool, ExternalInteractiveError> {
        if columns == 0 || rows == 0 {
            return Err(ExternalInteractiveError::InvalidRequest {
                field: "resize geometry",
            });
        }
        let changed = self.sequence.resize_controlled(columns, rows, control)?;
        self.columns = columns;
        self.rows = rows;
        Ok(changed)
    }

    /// Drops the retained baseline after an observed browser recovery or clean session teardown.
    pub fn reset_controlled(
        &mut self,
        control: &TerminalTransactionControl<'_>,
    ) -> Result<bool, ExternalInteractiveError> {
        Ok(self.sequence.reset_controlled(control)?)
    }

    /// Projects one CDP base64 screenshot using the next locally issued frame generation.
    pub fn project_next_png_base64_controlled(
        &mut self,
        screenshot: &str,
        control: &TerminalTransactionControl<'_>,
    ) -> Result<ExternalFrameSequenceOutput, ExternalInteractiveError> {
        let generation = self
            .issued_generation
            .checked_add(1)
            .ok_or(ExternalInteractiveError::CounterExhausted)?;
        self.issued_generation = generation;
        self.project_png_base64_for_generation_controlled(generation, screenshot, control)
    }

    /// Projects one caller-ordered CDP base64 screenshot without reimplementing image decode.
    ///
    /// A failed stale, cancelled, decode, projection, or output-quota candidate leaves the last
    /// accepted terminal baseline and generation owned by [`ExternalFrameSequence`] unchanged.
    pub fn project_png_base64_for_generation_controlled(
        &mut self,
        generation: u64,
        screenshot: &str,
        control: &TerminalTransactionControl<'_>,
    ) -> Result<ExternalFrameSequenceOutput, ExternalInteractiveError> {
        if generation > self.issued_generation {
            self.issued_generation = generation;
        }
        let output = self.sequence.push_png_base64_for_generation_controlled(
            generation,
            screenshot,
            &self.capabilities,
            ExternalFrameOptions::new(
                self.requested_backend,
                self.columns,
                self.rows,
                self.decode_limits,
                self.projection_limits,
                self.sequence_limits,
            ),
            control,
        )?;
        if output.output().len() > self.max_ansi_bytes {
            return Err(ExternalInteractiveError::OutputLimit {
                actual: output.output().len(),
                maximum: self.max_ansi_bytes,
            });
        }
        Ok(output)
    }
}

fn validate_frame_limits(
    frame_decode: ExternalFrameDecodeLimits,
    projection: ProjectionLimits,
    sequence: ExternalFrameSequenceLimits,
) -> Result<(), ExternalInteractiveError> {
    if frame_decode.max_base64_bytes == 0
        || frame_decode.max_png_bytes == 0
        || frame_decode.max_width == 0
        || frame_decode.max_height == 0
        || frame_decode.max_pixels == 0
        || frame_decode.max_decoded_bytes == 0
        || projection.max_cells == 0
        || projection.max_pixels == 0
        || projection.max_output_bytes == 0
        || sequence.max_operations == 0
        || sequence.max_output_bytes == 0
    {
        return Err(ExternalInteractiveError::InvalidLimits);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayoutDensity {
    Readable,
    Balanced,
    Overview,
}

impl LayoutDensity {
    const fn ratio(self) -> (u32, u32) {
        match self {
            Self::Readable => (1, 1),
            Self::Balanced => (3, 2),
            Self::Overview => (2, 1),
        }
    }

    const fn zoom_in(self) -> Option<Self> {
        match self {
            Self::Readable => None,
            Self::Balanced => Some(Self::Readable),
            Self::Overview => Some(Self::Balanced),
        }
    }

    const fn zoom_out(self) -> Option<Self> {
        match self {
            Self::Readable => Some(Self::Balanced),
            Self::Balanced => Some(Self::Overview),
            Self::Overview => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ZoomDirection {
    In,
    Out,
}

struct ExternalInteractiveState {
    target: Url,
    launch_options: ExternalEngineLaunchOptions,
    viewport: ExternalEngineViewport,
    device_scale: f64,
    columns: u16,
    rows: u16,
    horizontal_samples: u32,
    vertical_samples: u32,
    layout_density: Option<LayoutDensity>,
    input: ExternalInputAdapter,
    frame: ExternalInteractiveFrameAdapter,
    /// Address bar mode: when true, input is captured for URL entry
    address_bar_active: bool,
    /// Buffer for URL input in address bar mode
    address_bar_buffer: String,
}

impl ExternalInteractiveState {
    fn from_request(
        request: &ExternalRenderRequest,
        limits: &ExternalInteractiveLimits,
    ) -> Result<Self, ExternalInteractiveError> {
        limits.validate()?;
        if request.terminal_columns == 0 || request.terminal_rows == 0 {
            return Err(ExternalInteractiveError::InvalidRequest {
                field: "terminal geometry",
            });
        }
        if request.limits.max_bytes == 0 || request.limits.max_cells == 0 {
            return Err(ExternalInteractiveError::InvalidRequest {
                field: "render limits",
            });
        }
        if !request.viewport.device_scale.is_finite() || request.viewport.device_scale <= 0.0 {
            return Err(ExternalInteractiveError::InvalidRequest {
                field: "device scale",
            });
        }

        let target = Url::parse(&request.target)?;
        if !target.username().is_empty() || target.password().is_some() {
            return Err(ExternalInteractiveError::TargetCredentialsDisallowed);
        }
        let viewport =
            ExternalEngineViewport::new(request.viewport.width, request.viewport.height)?;
        let selected_backend = request
            .capabilities
            .select_safe_backend(request.backend, TerminalBackendFeedback::Initial);
        if !matches!(
            selected_backend,
            Backend::Cells | Backend::Halfblock | Backend::Quadrant | Backend::Braille
        ) {
            return Err(ExternalInteractiveError::UnsupportedBackend {
                backend: selected_backend,
            });
        }
        let (horizontal_samples, vertical_samples) = projection_samples(selected_backend);
        let layout_density = infer_layout_density(
            viewport,
            request.terminal_columns,
            request.terminal_rows,
            horizontal_samples,
            vertical_samples,
        );
        let max_ansi_bytes = request
            .limits
            .max_bytes
            .min(limits.projection.max_output_bytes)
            .min(limits.sequence.max_output_bytes);
        let max_cells = request.limits.max_cells.min(limits.projection.max_cells);
        if max_ansi_bytes == 0 || max_cells == 0 {
            return Err(ExternalInteractiveError::InvalidRequest {
                field: "effective output limits",
            });
        }
        let projection_limits = ProjectionLimits {
            max_cells,
            max_pixels: limits.projection.max_pixels,
            max_output_bytes: max_ansi_bytes,
        };
        let sequence_limits = ExternalFrameSequenceLimits {
            max_operations: limits.sequence.max_operations,
            max_output_bytes: max_ansi_bytes,
        };
        let input = input_adapter(request.terminal_columns, request.terminal_rows, viewport)?;
        let launch_options = launch_options(&request.policies, viewport)?;
        let frame = ExternalInteractiveFrameAdapter::new(
            request.capabilities,
            request.backend,
            request.terminal_columns,
            request.terminal_rows,
            limits.frame_decode,
            projection_limits,
            sequence_limits,
        )?;

        Ok(Self {
            target,
            launch_options,
            viewport,
            device_scale: request.viewport.device_scale,
            columns: request.terminal_columns,
            rows: request.terminal_rows,
            horizontal_samples,
            vertical_samples,
            layout_density,
            input,
            frame,
            address_bar_active: false,
            address_bar_buffer: String::new(),
        })
    }

    fn prepare_resize(
        &self,
        columns: u16,
        rows: u16,
    ) -> Result<(ExternalEngineViewport, ExternalInputAdapter), ExternalInteractiveError> {
        if columns == 0 || rows == 0 {
            return Err(ExternalInteractiveError::InvalidRequest {
                field: "resize geometry",
            });
        }
        let (width, height) = match self.layout_density {
            Some(density) => (
                layout_dimension(columns, self.horizontal_samples, density),
                layout_dimension(rows, self.vertical_samples, density),
            ),
            None => (
                scaled_viewport_dimension(self.viewport.width(), self.columns, columns)?,
                scaled_viewport_dimension(self.viewport.height(), self.rows, rows)?,
            ),
        };
        let viewport = ExternalEngineViewport::new(width, height)?;
        let input = input_adapter(columns, rows, viewport)?;
        Ok((viewport, input))
    }

    fn prepare_zoom(
        &self,
        direction: ZoomDirection,
    ) -> Result<
        Option<(ExternalEngineViewport, ExternalInputAdapter, LayoutDensity)>,
        ExternalInteractiveError,
    > {
        let Some(current) = self.layout_density else {
            return Ok(None);
        };
        let next = match direction {
            ZoomDirection::In => current.zoom_in(),
            ZoomDirection::Out => current.zoom_out(),
        };
        let Some(next) = next else {
            return Ok(None);
        };
        let viewport = ExternalEngineViewport::new(
            layout_dimension(self.columns, self.horizontal_samples, next),
            layout_dimension(self.rows, self.vertical_samples, next),
        )?;
        let input = input_adapter(self.columns, self.rows, viewport)?;
        Ok(Some((viewport, input, next)))
    }

    fn commit_resize(
        &mut self,
        viewport: ExternalEngineViewport,
        input: ExternalInputAdapter,
        columns: u16,
        rows: u16,
        control: &TerminalTransactionControl<'_>,
    ) -> Result<(), ExternalInteractiveError> {
        let _resized = self.frame.resize_controlled(columns, rows, control)?;
        self.viewport = viewport;
        self.launch_options.viewport = Some(viewport);
        self.columns = columns;
        self.rows = rows;
        self.input = input;
        Ok(())
    }

    fn commit_zoom(
        &mut self,
        viewport: ExternalEngineViewport,
        input: ExternalInputAdapter,
        density: LayoutDensity,
        control: &TerminalTransactionControl<'_>,
    ) -> Result<(), ExternalInteractiveError> {
        self.frame.reset_controlled(control)?;
        self.viewport = viewport;
        self.launch_options.viewport = Some(viewport);
        self.input = input;
        self.layout_density = Some(density);
        Ok(())
    }
}

/// Navigate to a URL via the CDP Page.navigate command.
async fn navigate_to_url(
    running: &mut RunningExternalEngine,
    url: &str,
) -> Result<(), ExternalInteractiveError> {
    let session_id = running.target_session_id.clone();
    let mut target = running.cdp.session(session_id)?;
    let navigation = target.navigate(url).await?;
    if let Some(message) = navigation.error_text.filter(|m| !m.is_empty()) {
        return Err(ExternalInteractiveError::NavigationFailed { message });
    }
    target.wait_for_load().await?;
    Ok(())
}

struct RunningExternalEngine {
    process: ExternalEngineProcess,
    cdp: CdpSession,
    target_session_id: String,
}

/// Runs one testable external-Chrome interaction loop over a caller-owned event stream and writer.
///
/// The stream must already contain decoded [`InteractiveEvent`] values. Only terminal
/// [`InputEvent`] values are forwarded to CDP; every other ordinary unsupported event is rejected
/// with [`ExternalInteractiveError::UnsupportedEvent`].
pub async fn run_external_interactive<S, W>(
    request: ExternalRenderRequest,
    mut events: S,
    writer: &mut W,
    cancellation: &Cancellation,
    limits: ExternalInteractiveLimits,
) -> Result<ExternalInteractiveReport, ExternalInteractiveError>
where
    S: Stream<Item = Result<InteractiveEvent, InteractiveError>> + Unpin,
    W: Write + ?Sized,
{
    let mut state = ExternalInteractiveState::from_request(&request, &limits)?;
    if cancellation.is_cancelled() {
        return Ok(ExternalInteractiveReport {
            reason: ExternalInteractiveExitReason::Cancelled,
            events_processed: 0,
            frames_written: 0,
            relaunches: 0,
        });
    }

    let mut running = Some(open_running_engine(&state, &limits, cancellation).await?);
    let operation = run_loop(
        &mut running,
        &mut state,
        &mut events,
        writer,
        cancellation,
        &limits,
    )
    .await;
    let cleanup = match running {
        Some(running) => cleanup_resources(Some(running.cdp), running.process).await,
        None => Ok(()),
    };
    preserve_operation_cleanup(operation, cleanup)
}

/// Runs the external interactive loop through the real crossterm event adapter and terminal guard.
///
/// The wrapper scopes the stdout lock to the active terminal lifetime, then preserves a restore
/// failure alongside the loop result instead of discarding either error.
pub async fn run_external_interactive_terminal(
    request: ExternalRenderRequest,
    cancellation: &Cancellation,
    limits: ExternalInteractiveLimits,
) -> Result<ExternalInteractiveReport, ExternalInteractiveError> {
    let mut guard = TerminalGuard::enter_buttons_only()?;
    // Watch for Ctrl-C / SIGTERM / SIGHUP and cancel the loop so the browser, CDP
    // connection, and terminal state are cleaned up when the process is signalled.
    let signal_cancellation = cancellation.clone();
    let signal_task = tokio::spawn(async move {
        // Cancel on both a received signal and a monitoring failure (registration error or a
        // closed signal stream) so browser, CDP, and terminal cleanup still runs.
        let _ = shutdown_signal().await;
        signal_cancellation.cancel();
    });
    let operation = {
        let stdout = io::stdout();
        let mut writer = stdout.lock();
        run_external_interactive(
            request,
            external_crossterm_events(),
            &mut writer,
            cancellation,
            limits,
        )
        .await
    };
    signal_task.abort();
    let restore = guard.restore();
    preserve_terminal_restore(operation, restore)
}

const ADDRESS_BAR_PREFIX: &str = "URL (Esc)> ";
const STATUS_BAR_TEXT: &str = " g URL | +/- Zoom | l Reload | Ctrl-C Quit | Mouse Click/Scroll | Tab/Enter Activate | Arrows/PgUp/PgDn Scroll ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AddressBarEdit {
    None,
    Redraw,
    Submit,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AddressInputDisposition {
    Forward,
    Handled,
    HandledAndRender,
}

fn edit_address_bar(buffer: &mut String, code: &KeyCode, kind: KeyEventKind) -> AddressBarEdit {
    if matches!(kind, KeyEventKind::Release) {
        return AddressBarEdit::None;
    }
    match code {
        KeyCode::Enter if matches!(kind, KeyEventKind::Press) => AddressBarEdit::Submit,
        KeyCode::Escape if matches!(kind, KeyEventKind::Press) => AddressBarEdit::Cancel,
        KeyCode::Backspace | KeyCode::Delete => {
            if buffer.pop().is_some() {
                AddressBarEdit::Redraw
            } else {
                AddressBarEdit::None
            }
        }
        KeyCode::Character(character) => {
            buffer.push(*character);
            AddressBarEdit::Redraw
        }
        _ => AddressBarEdit::None,
    }
}

fn visible_address_suffix(buffer: &str, columns: u16) -> &str {
    let available = usize::from(columns).saturating_sub(ADDRESS_BAR_PREFIX.width());
    let mut width = 0usize;
    let mut start = buffer.len();
    for (index, character) in buffer.char_indices().rev() {
        let character_width = character.width().unwrap_or(0);
        if width.saturating_add(character_width) > available {
            break;
        }
        width = width.saturating_add(character_width);
        start = index;
    }
    &buffer[start..]
}

fn visible_text_prefix(text: &str, columns: u16) -> &str {
    let available = usize::from(columns);
    let mut width = 0usize;
    let mut end = 0usize;
    for (index, character) in text.char_indices() {
        let character_width = character.width().unwrap_or(0);
        if width.saturating_add(character_width) > available {
            break;
        }
        width = width.saturating_add(character_width);
        end = index.saturating_add(character.len_utf8());
    }
    &text[..end]
}

fn draw_address_bar<W>(
    writer: &mut W,
    rows: u16,
    columns: u16,
    buffer: &str,
) -> Result<(), ExternalInteractiveError>
where
    W: Write + ?Sized,
{
    let output = format!(
        "\x1b[{rows};1H\x1b[0m\x1b[2K\x1b[1m{ADDRESS_BAR_PREFIX}\x1b[0m{}",
        visible_address_suffix(buffer, columns)
    );
    writer.write_all(output.as_bytes())?;
    writer.flush()?;
    Ok(())
}

fn draw_status_bar<W>(
    writer: &mut W,
    rows: u16,
    columns: u16,
) -> Result<(), ExternalInteractiveError>
where
    W: Write + ?Sized,
{
    let output = format!(
        "\x1b[{rows};1H\x1b[0m\x1b[2K\x1b[7m{}\x1b[0m",
        visible_text_prefix(STATUS_BAR_TEXT, columns)
    );
    writer.write_all(output.as_bytes())?;
    writer.flush()?;
    Ok(())
}

fn draw_terminal_controls<W>(
    writer: &mut W,
    state: &ExternalInteractiveState,
) -> Result<(), ExternalInteractiveError>
where
    W: Write + ?Sized,
{
    if state.address_bar_active {
        draw_address_bar(writer, state.rows, state.columns, &state.address_bar_buffer)
    } else {
        draw_status_bar(writer, state.rows, state.columns)
    }
}

fn invalidate_address_bar_overlay(
    state: &mut ExternalInteractiveState,
    cancellation: &Cancellation,
    clock: &SystemClock,
) -> Result<(), ExternalInteractiveError> {
    let control = TerminalTransactionControl::new(cancellation, clock, None);
    state.frame.reset_controlled(&control)?;
    Ok(())
}

/// Handles address-bar input locally and reports whether Chrome needs another frame capture.
async fn handle_address_input(
    running: &mut Option<RunningExternalEngine>,
    state: &mut ExternalInteractiveState,
    input: &InputEvent,
    cancellation: &Cancellation,
    clock: &SystemClock,
) -> Result<AddressInputDisposition, ExternalInteractiveError> {
    match input {
        // Press 'g' to enter address bar mode (only when not already active)
        InputEvent::Key {
            code,
            kind: KeyEventKind::Press,
            ..
        } if !state.address_bar_active => {
            if matches!(code, KeyCode::Character('g')) {
                state.address_bar_active = true;
                state.address_bar_buffer.clear();
                return Ok(AddressInputDisposition::Handled);
            }
            let zoom = match code {
                KeyCode::Character('+') => Some(ZoomDirection::In),
                KeyCode::Character('-') => Some(ZoomDirection::Out),
                _ => None,
            };
            if let Some(direction) = zoom {
                return if apply_zoom(running, state, direction, cancellation, clock).await? {
                    Ok(AddressInputDisposition::HandledAndRender)
                } else {
                    Ok(AddressInputDisposition::Handled)
                };
            }
            // Press 'l' to reload current page
            if matches!(code, KeyCode::Character('l')) {
                if let Some(running) = running {
                    let session_id = running.target_session_id.clone();
                    let mut target = running.cdp.session(session_id)?;
                    target.reload().await?;
                }
                return Ok(AddressInputDisposition::HandledAndRender);
            }
            Ok(AddressInputDisposition::Forward)
        }
        // Handle input when in address bar mode
        InputEvent::Key { code, kind, .. } if state.address_bar_active => {
            match edit_address_bar(&mut state.address_bar_buffer, code, *kind) {
                AddressBarEdit::Submit => {
                    state.address_bar_active = false;
                    invalidate_address_bar_overlay(state, cancellation, clock)?;
                    let url = std::mem::take(&mut state.address_bar_buffer);
                    if !url.is_empty() {
                        let url = if url.starts_with("http://")
                            || url.starts_with("https://")
                            || url.starts_with("about:")
                        {
                            url
                        } else if url.contains('.') && !url.contains(' ') {
                            format!("https://{}", url)
                        } else {
                            format!("https://www.google.com/search?q={}", url.replace(' ', "+"))
                        };
                        if let Some(running) = running.as_mut() {
                            navigate_to_url(running, &url).await?;
                            state.target = Url::parse(&url)?;
                        }
                    }
                    Ok(AddressInputDisposition::HandledAndRender)
                }
                AddressBarEdit::Cancel => {
                    state.address_bar_active = false;
                    state.address_bar_buffer.clear();
                    invalidate_address_bar_overlay(state, cancellation, clock)?;
                    Ok(AddressInputDisposition::HandledAndRender)
                }
                AddressBarEdit::None | AddressBarEdit::Redraw => {
                    Ok(AddressInputDisposition::Handled)
                }
            }
        }
        _ => Ok(AddressInputDisposition::Forward),
    }
}

async fn run_loop<S, W>(
    running: &mut Option<RunningExternalEngine>,
    state: &mut ExternalInteractiveState,
    events: &mut S,
    writer: &mut W,
    cancellation: &Cancellation,
    limits: &ExternalInteractiveLimits,
) -> Result<ExternalInteractiveReport, ExternalInteractiveError>
where
    S: Stream<Item = Result<InteractiveEvent, InteractiveError>> + Unpin,
    W: Write + ?Sized,
{
    let clock = SystemClock::default();
    let mut frames_written = 0usize;
    let mut events_processed = 0usize;
    let mut relaunches = 0usize;
    if cancellation.is_cancelled() {
        return Ok(report(
            ExternalInteractiveExitReason::Cancelled,
            events_processed,
            frames_written,
            relaunches,
        ));
    }
    if render_current_frame_with_controls(running, state, writer, cancellation, &clock).await? {
        increment(&mut frames_written)?;
    }

    let mut crash_poll = tokio::time::interval(limits.crash_poll_interval);
    crash_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = cancellation.cancelled() => {
                return Ok(report(
                    ExternalInteractiveExitReason::Cancelled,
                    events_processed,
                    frames_written,
                    relaunches,
                ));
            }
            _ = crash_poll.tick() => {
                let recovered = recover_if_browser_exited(
                    running,
                    state,
                    limits,
                    cancellation,
                    &clock,
                    &mut relaunches,
                )
                .await?;
                if recovered
                    && render_current_frame_with_controls(
                        running,
                        state,
                        writer,
                        cancellation,
                        &clock,
                    )
                    .await?
                {
                    increment(&mut frames_written)?;
                }
            }
            event = events.next() => {
                let Some(event) = event else {
                    return Ok(report(
                        ExternalInteractiveExitReason::EndOfInput,
                        events_processed,
                        frames_written,
                        relaunches,
                    ));
                };
                if events_processed >= limits.max_events {
                    return Err(ExternalInteractiveError::EventLimit);
                }
                increment(&mut events_processed)?;
                let event = event?;
                let recovered = recover_if_browser_exited(
                    running,
                    state,
                    limits,
                    cancellation,
                    &clock,
                    &mut relaunches,
                )
                .await?;
                if recovered
                    && render_current_frame_with_controls(
                        running,
                        state,
                        writer,
                        cancellation,
                        &clock,
                    )
                    .await?
                {
                    increment(&mut frames_written)?;
                }
                match event {
                    InteractiveEvent::Quit => {
                        return Ok(report(
                            ExternalInteractiveExitReason::Quit,
                            events_processed,
                            frames_written,
                            relaunches,
                        ));
                    }
                    InteractiveEvent::Input(ShellInputEvent::Terminal(input)) => {
                        let disposition =
                            handle_address_input(running, state, &input, cancellation, &clock)
                                .await?;
                        let render = match disposition {
                            AddressInputDisposition::Forward => {
                                dispatch_terminal_input(running, state, &input, cancellation).await?;
                                true
                            }
                            AddressInputDisposition::Handled => false,
                            AddressInputDisposition::HandledAndRender => true,
                        };
                        if render {
                            if render_current_frame_with_controls(
                                running,
                                state,
                                writer,
                                cancellation,
                                &clock,
                            )
                            .await?
                            {
                                increment(&mut frames_written)?;
                            }
                        } else {
                            draw_terminal_controls(writer, state)?;
                        }
                    }
                    InteractiveEvent::Input(ShellInputEvent::Ime(_)) => {
                        return Err(ExternalInteractiveError::UnsupportedEvent {
                            event: ExternalInteractiveUnsupportedEvent::ImeComposition,
                        });
                    }
                    InteractiveEvent::Resize { columns, rows } => {
                        apply_resize(running, state, columns, rows, cancellation, &clock).await?;
                        if render_current_frame_with_controls(
                            running,
                            state,
                            writer,
                            cancellation,
                            &clock,
                        )
                        .await?
                        {
                            increment(&mut frames_written)?;
                        }
                    }
                    InteractiveEvent::Unsupported(feature) => {
                        return Err(ExternalInteractiveError::UnsupportedEvent {
                            event: ExternalInteractiveUnsupportedEvent::InteractiveFeature(feature),
                        });
                    }
                }
            }
        }
    }
}

async fn open_running_engine(
    state: &ExternalInteractiveState,
    limits: &ExternalInteractiveLimits,
    cancellation: &Cancellation,
) -> Result<RunningExternalEngine, ExternalInteractiveError> {
    let process = launch_process(state.launch_options.clone(), cancellation.clone()).await?;
    let endpoint = process.endpoint().websocket_url();
    let mut cdp = match CdpSession::connect(&endpoint, limits.cdp.clone()).await {
        Ok(cdp) => cdp,
        Err(error) => {
            return Err(finish_startup_failure(
                ExternalInteractiveError::Cdp(error),
                None,
                process,
            )
            .await);
        }
    };
    let target_session_id = match configure_target(&mut cdp, state).await {
        Ok(session_id) => session_id,
        Err(operation) => {
            return Err(finish_startup_failure(operation, Some(cdp), process).await);
        }
    };
    Ok(RunningExternalEngine {
        process,
        cdp,
        target_session_id,
    })
}

async fn configure_target(
    cdp: &mut CdpSession,
    state: &ExternalInteractiveState,
) -> Result<String, ExternalInteractiveError> {
    let mut target = cdp.attach_first_page().await?;
    target.page_enable().await?;
    target
        .set_device_metrics(
            state.viewport.width(),
            state.viewport.height(),
            state.device_scale,
        )
        .await?;
    let navigation = target.navigate(state.target.as_str()).await?;
    if let Some(message) = navigation.error_text {
        return Err(ExternalInteractiveError::NavigationFailed { message });
    }
    target.wait_for_load().await?;
    Ok(target.session_id().to_owned())
}

async fn dispatch_terminal_input(
    running: &mut Option<RunningExternalEngine>,
    state: &ExternalInteractiveState,
    input: &InputEvent,
    cancellation: &Cancellation,
) -> Result<(), ExternalInteractiveError> {
    if cancellation.is_cancelled() {
        return Err(ExternalInteractiveError::Cancelled);
    }
    let running = running
        .as_mut()
        .ok_or(ExternalInteractiveError::StateUnavailable)?;
    let session_id = running.target_session_id.clone();
    let mut target = running.cdp.session(session_id)?;
    state
        .input
        .dispatch(&mut target, input, cancellation)
        .await?;
    Ok(())
}

async fn apply_resize(
    running: &mut Option<RunningExternalEngine>,
    state: &mut ExternalInteractiveState,
    columns: u16,
    rows: u16,
    cancellation: &Cancellation,
    clock: &SystemClock,
) -> Result<(), ExternalInteractiveError> {
    if cancellation.is_cancelled() {
        return Err(ExternalInteractiveError::Cancelled);
    }
    let (viewport, input) = state.prepare_resize(columns, rows)?;
    let running = running
        .as_mut()
        .ok_or(ExternalInteractiveError::StateUnavailable)?;
    let session_id = running.target_session_id.clone();
    {
        let mut target = running.cdp.session(session_id)?;
        target
            .set_device_metrics(viewport.width(), viewport.height(), state.device_scale)
            .await?;
    }
    let control = TerminalTransactionControl::new(cancellation, clock, None);
    state.commit_resize(viewport, input, columns, rows, &control)
}

async fn apply_zoom(
    running: &mut Option<RunningExternalEngine>,
    state: &mut ExternalInteractiveState,
    direction: ZoomDirection,
    cancellation: &Cancellation,
    clock: &SystemClock,
) -> Result<bool, ExternalInteractiveError> {
    if cancellation.is_cancelled() {
        return Err(ExternalInteractiveError::Cancelled);
    }
    let Some((viewport, input, density)) = state.prepare_zoom(direction)? else {
        return Ok(false);
    };
    let running = running
        .as_mut()
        .ok_or(ExternalInteractiveError::StateUnavailable)?;
    let session_id = running.target_session_id.clone();
    {
        let mut target = running.cdp.session(session_id)?;
        target
            .set_device_metrics(viewport.width(), viewport.height(), state.device_scale)
            .await?;
    }
    let control = TerminalTransactionControl::new(cancellation, clock, None);
    state.commit_zoom(viewport, input, density, &control)?;
    Ok(true)
}

async fn render_current_frame<W>(
    running: &mut Option<RunningExternalEngine>,
    state: &mut ExternalInteractiveState,
    writer: &mut W,
    cancellation: &Cancellation,
    clock: &SystemClock,
) -> Result<bool, ExternalInteractiveError>
where
    W: Write + ?Sized,
{
    if cancellation.is_cancelled() {
        return Err(ExternalInteractiveError::Cancelled);
    }
    let running = running
        .as_mut()
        .ok_or(ExternalInteractiveError::StateUnavailable)?;
    let session_id = running.target_session_id.clone();
    let screenshot = {
        let mut target = running.cdp.session(session_id)?;
        target.capture_screenshot().await?
    };
    let control = TerminalTransactionControl::new(cancellation, clock, None);
    let frame = state
        .frame
        .project_next_png_base64_controlled(&screenshot.data, &control)?;
    if !frame.output().is_empty() {
        writer.write_all(frame.output())?;
        writer.flush()?;
    }
    Ok(!frame.is_identical())
}

async fn render_current_frame_with_controls<W>(
    running: &mut Option<RunningExternalEngine>,
    state: &mut ExternalInteractiveState,
    writer: &mut W,
    cancellation: &Cancellation,
    clock: &SystemClock,
) -> Result<bool, ExternalInteractiveError>
where
    W: Write + ?Sized,
{
    let mut output = Vec::new();
    let changed = render_current_frame(running, state, &mut output, cancellation, clock).await?;
    if !changed {
        return Ok(false);
    }
    draw_terminal_controls(&mut output, state)?;
    writer.write_all(&output)?;
    writer.flush()?;
    Ok(changed)
}

async fn recover_if_browser_exited(
    running: &mut Option<RunningExternalEngine>,
    state: &mut ExternalInteractiveState,
    limits: &ExternalInteractiveLimits,
    cancellation: &Cancellation,
    clock: &SystemClock,
    relaunches: &mut usize,
) -> Result<bool, ExternalInteractiveError> {
    let exited = running
        .as_mut()
        .ok_or(ExternalInteractiveError::StateUnavailable)?
        .process
        .try_wait()?
        .is_some();
    if !exited {
        return Ok(false);
    }
    if *relaunches >= limits.max_relaunches {
        return Err(ExternalInteractiveError::RecoveryExhausted {
            max_relaunches: limits.max_relaunches,
        });
    }
    let crashed = running
        .take()
        .ok_or(ExternalInteractiveError::StateUnavailable)?;
    if let Err(cleanup) =
        cleanup_resources_after_observed_browser_exit(Some(crashed.cdp), crashed.process).await
    {
        return Err(ExternalInteractiveError::RecoveryCleanup {
            cleanup: Box::new(cleanup),
        });
    }
    if cancellation.is_cancelled() {
        return Err(ExternalInteractiveError::Cancelled);
    }
    let rebuilt = open_running_engine(state, limits, cancellation).await?;
    let control = TerminalTransactionControl::new(cancellation, clock, None);
    if let Err(operation) = state.frame.reset_controlled(&control) {
        return Err(finish_startup_failure(operation, Some(rebuilt.cdp), rebuilt.process).await);
    }
    *running = Some(rebuilt);
    increment(relaunches)?;
    Ok(true)
}

async fn finish_startup_failure(
    operation: ExternalInteractiveError,
    cdp: Option<CdpSession>,
    process: ExternalEngineProcess,
) -> ExternalInteractiveError {
    match cleanup_resources(cdp, process).await {
        Ok(()) => operation,
        Err(cleanup) => ExternalInteractiveError::OperationAndCleanup {
            operation: Box::new(operation),
            cleanup: Box::new(cleanup),
        },
    }
}

async fn cleanup_resources(
    cdp: Option<CdpSession>,
    process: ExternalEngineProcess,
) -> Result<(), ExternalInteractiveCleanupError> {
    cleanup_resources_with_close_policy(cdp, process, false).await
}

async fn cleanup_resources_after_observed_browser_exit(
    cdp: Option<CdpSession>,
    process: ExternalEngineProcess,
) -> Result<(), ExternalInteractiveCleanupError> {
    cleanup_resources_with_close_policy(cdp, process, true).await
}

async fn cleanup_resources_with_close_policy(
    cdp: Option<CdpSession>,
    process: ExternalEngineProcess,
    tolerate_terminal_cdp_close: bool,
) -> Result<(), ExternalInteractiveCleanupError> {
    let cdp_error = match cdp {
        Some(mut cdp) => match cdp.close().await {
            Ok(()) => None,
            Err(error)
                if tolerate_terminal_cdp_close && is_expected_post_exit_cdp_close_error(&error) =>
            {
                None
            }
            Err(error) => Some(error),
        },
        None => None,
    };
    let browser_error = shutdown_process(process).await.err();
    match (cdp_error, browser_error) {
        (None, None) => Ok(()),
        (Some(cdp), None) => Err(ExternalInteractiveCleanupError::CdpClose(cdp)),
        (None, Some(browser)) => Err(browser),
        (Some(cdp), Some(browser)) => Err(ExternalInteractiveCleanupError::CdpAndBrowserShutdown {
            cdp,
            browser: Box::new(browser),
        }),
    }
}

fn is_expected_post_exit_cdp_close_error(error: &CdpError) -> bool {
    matches!(error, CdpError::Closed | CdpError::WebSocket(_))
}

async fn launch_process(
    options: ExternalEngineLaunchOptions,
    cancellation: Cancellation,
) -> Result<ExternalEngineProcess, ExternalInteractiveError> {
    let result =
        tokio::task::spawn_blocking(move || launch_external_engine(&options, &cancellation))
            .await
            .map_err(|error| ExternalInteractiveError::BlockingTask {
                message: error.to_string(),
            })?;
    result.map_err(ExternalInteractiveError::Engine)
}

async fn shutdown_process(
    process: ExternalEngineProcess,
) -> Result<(), ExternalInteractiveCleanupError> {
    let profile_dir = process.isolated_profile_dir().to_path_buf();
    let (shutdown, profile_dir, removed) = tokio::task::spawn_blocking(move || {
        let mut process = process;
        let shutdown = process.shutdown();
        drop(process);
        let removed = !profile_dir.exists();
        (shutdown, profile_dir, removed)
    })
    .await
    .map_err(
        |error| ExternalInteractiveCleanupError::BrowserShutdownTask {
            message: error.to_string(),
        },
    )?;
    match shutdown {
        Err(error) => Err(ExternalInteractiveCleanupError::BrowserShutdown(error)),
        Ok(()) if !removed => {
            Err(ExternalInteractiveCleanupError::ProfileNotRemoved { profile_dir })
        }
        Ok(()) => Ok(()),
    }
}

fn launch_options(
    policies: &ExternalEnginePolicies,
    viewport: ExternalEngineViewport,
) -> Result<ExternalEngineLaunchOptions, ExternalInteractiveError> {
    let mut options = ExternalEngineLaunchOptions::new(Url::parse(ABOUT_BLANK)?);
    options.executable_override = policies.executable_override.clone();
    options.viewport = Some(viewport);
    options.user_agent = policies.user_agent.clone();
    options.javascript = if policies.disable_javascript {
        ExternalEngineJavaScriptPolicy::Disabled
    } else {
        ExternalEngineJavaScriptPolicy::Enabled
    };
    options.images = if policies.disable_images {
        ExternalEngineImagePolicy::Disabled
    } else {
        ExternalEngineImagePolicy::Enabled
    };
    options.proxy = match &policies.proxy {
        Some(proxy) => {
            let mut proxy = ExternalEngineProxy::new(Url::parse(proxy)?)?;
            if !policies.proxy_bypass.is_empty() {
                proxy = proxy.with_bypass(ExternalEngineProxyBypass::new(
                    policies.proxy_bypass.iter().map(String::as_str),
                )?);
            }
            Some(proxy)
        }
        None if policies.proxy_bypass.is_empty() => None,
        None => return Err(ExternalInteractiveError::ProxyBypassWithoutProxy),
    };
    Ok(options)
}

fn input_adapter(
    columns: u16,
    rows: u16,
    viewport: ExternalEngineViewport,
) -> Result<ExternalInputAdapter, ExternalInteractiveError> {
    let geometry = ExternalInputGeometry::new(
        columns,
        rows,
        f64::from(viewport.width()),
        f64::from(viewport.height()),
    )?;
    ExternalInputAdapter::new(geometry).map_err(ExternalInteractiveError::Input)
}

const fn projection_samples(backend: Backend) -> (u32, u32) {
    match backend {
        Backend::Quadrant | Backend::Braille => (2, 4),
        Backend::Auto | Backend::Text | Backend::Cells | Backend::Halfblock => (1, 2),
    }
}

fn layout_dimension(cells: u16, samples: u32, density: LayoutDensity) -> u32 {
    let (numerator, denominator) = density.ratio();
    u32::from(cells)
        .saturating_mul(samples)
        .saturating_mul(numerator)
        .div_ceil(denominator)
}

fn infer_layout_density(
    viewport: ExternalEngineViewport,
    columns: u16,
    rows: u16,
    horizontal_samples: u32,
    vertical_samples: u32,
) -> Option<LayoutDensity> {
    [
        LayoutDensity::Readable,
        LayoutDensity::Balanced,
        LayoutDensity::Overview,
    ]
    .into_iter()
    .find(|density| {
        viewport.width() == layout_dimension(columns, horizontal_samples, *density)
            && viewport.height() == layout_dimension(rows, vertical_samples, *density)
    })
}

fn scaled_viewport_dimension(
    current_dimension: u32,
    current_cells: u16,
    next_cells: u16,
) -> Result<u32, ExternalInteractiveError> {
    if current_cells == 0 || next_cells == 0 {
        return Err(ExternalInteractiveError::InvalidRequest {
            field: "resize geometry",
        });
    }
    let divisor = u64::from(current_cells);
    let numerator = u64::from(current_dimension)
        .checked_mul(u64::from(next_cells))
        .ok_or(ExternalInteractiveError::ResizeOverflow)?;
    let rounded = numerator
        .checked_add(divisor / 2)
        .ok_or(ExternalInteractiveError::ResizeOverflow)?;
    let scaled = rounded / divisor;
    u32::try_from(scaled).map_err(|_| ExternalInteractiveError::ResizeOverflow)
}

fn report(
    reason: ExternalInteractiveExitReason,
    events_processed: usize,
    frames_written: usize,
    relaunches: usize,
) -> ExternalInteractiveReport {
    ExternalInteractiveReport {
        reason,
        events_processed,
        frames_written,
        relaunches,
    }
}

fn increment(value: &mut usize) -> Result<(), ExternalInteractiveError> {
    *value = value
        .checked_add(1)
        .ok_or(ExternalInteractiveError::CounterExhausted)?;
    Ok(())
}

fn preserve_operation_cleanup<T>(
    operation: Result<T, ExternalInteractiveError>,
    cleanup: Result<(), ExternalInteractiveCleanupError>,
) -> Result<T, ExternalInteractiveError> {
    match (operation, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(cleanup)) => Err(ExternalInteractiveError::Cleanup(cleanup)),
        (Err(operation), Ok(())) => Err(operation),
        (Err(operation), Err(cleanup)) => Err(ExternalInteractiveError::OperationAndCleanup {
            operation: Box::new(operation),
            cleanup: Box::new(cleanup),
        }),
    }
}

fn preserve_terminal_restore<T>(
    operation: Result<T, ExternalInteractiveError>,
    restore: Result<(), PlatformError>,
) -> Result<T, ExternalInteractiveError> {
    match (operation, restore) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(restore)) => Err(ExternalInteractiveError::TerminalRestore(restore)),
        (Err(operation), Ok(())) => Err(operation),
        (Err(operation), Err(restore)) => Err(ExternalInteractiveError::OperationAndRestore {
            operation: Box::new(operation),
            restore,
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use tg_browser::ExternalEngineViewport;
    use tg_network::{CdpError, CdpProtocolError};
    use tg_terminal::{KeyCode, KeyEventKind};

    use super::{
        AddressBarEdit, LayoutDensity, STATUS_BAR_TEXT, draw_address_bar, draw_status_bar,
        edit_address_bar, infer_layout_density, is_expected_post_exit_cdp_close_error,
        layout_dimension, visible_address_suffix,
    };

    #[test]
    fn layout_density_moves_between_readable_balanced_and_overview_widths()
    -> Result<(), Box<dyn Error>> {
        assert_eq!(layout_dimension(251, 1, LayoutDensity::Readable), 251);
        assert_eq!(layout_dimension(251, 1, LayoutDensity::Balanced), 377);
        assert_eq!(layout_dimension(251, 1, LayoutDensity::Overview), 502);
        assert_eq!(
            LayoutDensity::Balanced.zoom_in(),
            Some(LayoutDensity::Readable)
        );
        assert_eq!(
            LayoutDensity::Balanced.zoom_out(),
            Some(LayoutDensity::Overview)
        );
        let viewport = ExternalEngineViewport::new(377, 225)?;
        assert_eq!(
            infer_layout_density(viewport, 251, 75, 1, 2),
            Some(LayoutDensity::Balanced)
        );
        Ok(())
    }

    #[test]
    fn address_bar_editing_redraws_backspace_delete_and_unicode_safely() {
        let mut buffer = "https://example.com한".to_owned();
        assert_eq!(
            edit_address_bar(&mut buffer, &KeyCode::Backspace, KeyEventKind::Press),
            AddressBarEdit::Redraw
        );
        assert_eq!(buffer, "https://example.com");
        assert_eq!(
            edit_address_bar(&mut buffer, &KeyCode::Delete, KeyEventKind::Repeat),
            AddressBarEdit::Redraw
        );
        assert_eq!(buffer, "https://example.co");
        let released =
            edit_address_bar(&mut buffer, &KeyCode::Character('x'), KeyEventKind::Release);
        assert_eq!(released, AddressBarEdit::None);
        assert_eq!(buffer, "https://example.co");
        let repeated =
            edit_address_bar(&mut buffer, &KeyCode::Character('m'), KeyEventKind::Repeat);
        assert_eq!(repeated, AddressBarEdit::Redraw);
        assert_eq!(buffer, "https://example.com");
        assert_eq!(
            edit_address_bar(&mut buffer, &KeyCode::Enter, KeyEventKind::Press),
            AddressBarEdit::Submit
        );
        assert_eq!(
            edit_address_bar(&mut buffer, &KeyCode::Escape, KeyEventKind::Press),
            AddressBarEdit::Cancel
        );
    }

    #[test]
    fn address_bar_redraw_is_single_line_and_keeps_the_visible_suffix() -> Result<(), Box<dyn Error>>
    {
        assert_eq!(visible_address_suffix("ab한글", 15), "한글");
        let mut output = Vec::new();
        draw_address_bar(&mut output, 24, 18, "https://example.com")?;
        assert_eq!(
            String::from_utf8(output)?,
            "\x1b[24;1H\x1b[0m\x1b[2K\x1b[1mURL (Esc)> \x1b[0mple.com"
        );
        let mut status = Vec::new();
        draw_status_bar(&mut status, 24, 120)?;
        assert_eq!(
            String::from_utf8(status)?,
            format!("\x1b[24;1H\x1b[0m\x1b[2K\x1b[7m{STATUS_BAR_TEXT}\x1b[0m")
        );
        Ok(())
    }

    #[test]
    fn observed_browser_exit_tolerates_only_terminal_cdp_close_errors() {
        assert!(is_expected_post_exit_cdp_close_error(&CdpError::Closed));
        assert!(is_expected_post_exit_cdp_close_error(&CdpError::WebSocket(
            "connection reset".to_owned()
        )));
        assert!(!is_expected_post_exit_cdp_close_error(&CdpError::Timeout));
        assert!(!is_expected_post_exit_cdp_close_error(
            &CdpError::MalformedMessage
        ));
        assert!(!is_expected_post_exit_cdp_close_error(&CdpError::Protocol(
            Box::new(CdpProtocolError {
                request_id: 1,
                session_id: None,
                code: -32_000,
                message: "unexpected protocol failure".to_owned(),
                data: None,
            })
        )));
    }
}
