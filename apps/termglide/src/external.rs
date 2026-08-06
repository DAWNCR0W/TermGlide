//! Noninteractive Chrome/Chromium rendering boundary for the TermGlide application.
//!
//! This module owns app-level policy translation and lifecycle cleanup only. Chrome process
//! isolation remains in `tg-browser`, CDP transport remains in `tg-network`, and screenshot
//! projection remains in `tg-terminal`.

use std::fmt;
use std::path::PathBuf;

use serde::Serialize;
use tg_browser::{
    ExternalEngineError, ExternalEngineImagePolicy, ExternalEngineJavaScriptPolicy,
    ExternalEngineLaunchOptions, ExternalEngineProcess, ExternalEngineProxy,
    ExternalEngineProxyBypass, ExternalEngineViewport, discover_external_engine,
    launch_external_engine,
};
use tg_core::{Cancellation, SystemClock};
use tg_network::{CdpAccessibilityTree, CdpBrowserVersion, CdpError, CdpLimits, CdpSession};
use tg_terminal::{
    Backend, ExternalFrameDecodeLimits, ExternalFrameError, ExternalFrameOptions,
    ExternalFrameSequence, ExternalFrameSequenceLimits, ProjectionLimits, Surface,
    TerminalCapabilities, TerminalTransactionControl,
};
use thiserror::Error;
use url::Url;

/// Caller-selected policy inputs for one isolated external-engine launch.
#[derive(Debug, Clone, Default)]
pub struct ExternalEnginePolicies {
    pub executable_override: Option<PathBuf>,
    pub proxy: Option<String>,
    pub proxy_bypass: Vec<String>,
    pub user_agent: Option<String>,
    pub disable_images: bool,
    pub disable_javascript: bool,
}

/// Browser viewport and device scale used for the external renderer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExternalViewport {
    pub width: u32,
    pub height: u32,
    pub device_scale: f64,
}

/// Bounded output limits owned by the application boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExternalRenderLimits {
    pub max_bytes: usize,
    pub max_cells: usize,
}

/// Inputs for one noninteractive external-engine rendering pass.
#[derive(Clone)]
pub struct ExternalRenderRequest {
    /// Serialized, already-classified browser target selected by the CLI layer.
    pub target: String,
    pub policies: ExternalEnginePolicies,
    pub viewport: ExternalViewport,
    pub terminal_columns: u16,
    pub terminal_rows: u16,
    pub backend: Backend,
    pub capabilities: TerminalCapabilities,
    pub limits: ExternalRenderLimits,
}

/// Selected Browser.getVersion fields retained for inspect output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExternalBrowserMetadata {
    pub protocol_version: String,
    pub product: String,
    pub revision: String,
    pub user_agent: String,
    pub javascript_version: String,
}

/// A fully rendered external page that is safe for a later output write.
#[derive(Debug, Clone)]
pub struct ExternalRenderedPage {
    pub target: String,
    pub browser: ExternalBrowserMetadata,
    pub cells: Option<Surface>,
    pub ansi: Option<Vec<u8>>,
    pub outer_html: Option<String>,
    pub accessibility: Option<CdpAccessibilityTree>,
}

/// The output shape requested after a successful external render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalOutputKind {
    Inspect,
    SnapshotCells,
    SnapshotAnsi,
    DumpCells,
    DumpDom,
    DumpAccessibility,
    RenderOnce,
}

impl fmt::Display for ExternalOutputKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Inspect => "inspect metadata",
            Self::SnapshotCells => "cell snapshot",
            Self::SnapshotAnsi => "ANSI snapshot",
            Self::DumpCells => "cell dump",
            Self::DumpDom => "DOM dump",
            Self::DumpAccessibility => "accessibility tree dump",
            Self::RenderOnce => "one-shot render",
        })
    }
}

/// Errors at the application-owned external-engine boundary.
#[derive(Debug, Error)]
pub enum ExternalError {
    #[error("external Chrome output does not support {output}")]
    UnsupportedOutput { output: ExternalOutputKind },
    #[error("proxy bypass entries require an explicit proxy endpoint")]
    ProxyBypassWithoutProxy,
    #[error("external Chrome target URLs with embedded credentials are not allowed")]
    TargetCredentialsDisallowed,
    #[error("external Chrome device scale must be finite and positive")]
    InvalidDeviceScale,
    #[error("external Chrome requires non-zero terminal geometry")]
    InvalidTerminalGeometry,
    #[error("external Chrome render limits must be non-zero")]
    InvalidRenderLimits,
    #[error("external Chrome screenshot did not produce a terminal cell surface")]
    MissingCellSurface,
    #[error("external Chrome screenshot did not produce ANSI output")]
    MissingAnsiOutput,
    #[error("external Chrome frame generation counter was exhausted")]
    FrameGenerationExhausted,
    #[error("external Chrome DOM dump did not produce outer HTML")]
    MissingOuterHtml,
    #[error("external Chrome DOM output used {actual} bytes, exceeding its quota of {maximum}")]
    DomOutputLimit { actual: usize, maximum: usize },
    #[error("external Chrome accessibility tree was not captured for this output")]
    MissingAccessibilityTree,
    #[error("external engine target or proxy URL is invalid: {0}")]
    Url(#[from] url::ParseError),
    #[error("external engine lifecycle failed: {0}")]
    Engine(#[from] ExternalEngineError),
    #[error("external engine CDP operation failed: {0}")]
    Cdp(#[from] CdpError),
    #[error("external screenshot projection failed: {0}")]
    Frame(#[from] ExternalFrameError),
    #[error("external engine blocking task failed: {0}")]
    BlockingTask(String),
    #[error("external browser navigation failed: {message}")]
    NavigationFailed { message: String },
    #[error("external engine operation failed: {operation}; cleanup also failed: {cleanup}")]
    OperationAndCleanup {
        operation: Box<Self>,
        cleanup: Box<ExternalCleanupError>,
    },
    #[error("external engine cleanup failed: {0}")]
    Cleanup(#[source] ExternalCleanupError),
}

/// Result of probing one isolated local Chrome process for doctor-style diagnostics.
///
/// Discovery failures are returned as [`ExternalError`] because no executable path exists to
/// report. Once an executable is selected, startup and CDP failures stay in the diagnostic so a
/// caller can show both the selected path and a recovery error.
#[derive(Debug)]
pub enum ExternalEngineDiagnostic {
    Ready {
        executable: PathBuf,
        browser: ExternalBrowserMetadata,
    },
    Unhealthy {
        executable: PathBuf,
        error: ExternalError,
    },
}

/// Executes one noninteractive Chrome rendering pass without writing terminal output.
///
/// The caller receives a page only after the CDP connection has been closed and Chrome has been
/// shut down, including removal of its isolated temporary profile. This makes output publication
/// an explicit later step rather than a side effect of an incomplete browser operation.
pub async fn render(request: ExternalRenderRequest) -> Result<ExternalRenderedPage, ExternalError> {
    render_for_output(request, ExternalOutputKind::RenderOnce).await
}

/// Executes one noninteractive Chrome rendering pass for the requested output shape.
///
/// DOM output remains separate from screenshot projection so a caller never receives fabricated
/// cells or ANSI data for `dump --format dom`.
pub async fn render_for_output(
    request: ExternalRenderRequest,
    output: ExternalOutputKind,
) -> Result<ExternalRenderedPage, ExternalError> {
    let options = launch_options_for(&request)?;
    let process = launch_process(options).await?;
    let endpoint = process.endpoint().websocket_url();

    let CdpRenderOutcome {
        operation,
        close_error,
    } = render_via_cdp(&endpoint, &request, output).await;
    let browser_shutdown = shutdown_process(process).await.err();
    finish_with_cleanup(operation, cleanup_error(close_error, browser_shutdown))
}

/// Runs only cross-platform executable discovery on a blocking worker.
///
/// This supports automatic interactive routing without accidentally launching an unsupported
/// interactive Chrome session.
pub async fn discover(executable_override: Option<PathBuf>) -> Result<PathBuf, ExternalError> {
    tokio::task::spawn_blocking(move || discover_external_engine(executable_override.as_deref()))
        .await
        .map_err(|source| ExternalError::BlockingTask(source.to_string()))?
        .map_err(ExternalError::from)
}

/// Discovers an executable, starts one isolated `about:blank` process, and reads
/// `Browser.getVersion` without navigating to a caller target.
///
/// The process and its temporary profile are shut down before this function resolves. A selected
/// executable that cannot launch or answer CDP is returned as [`ExternalEngineDiagnostic::Unhealthy`]
/// so doctor output can retain the path that needs recovery.
pub async fn diagnose(
    executable_override: Option<PathBuf>,
) -> Result<ExternalEngineDiagnostic, ExternalError> {
    let executable = discover(executable_override).await?;
    let mut options = ExternalEngineLaunchOptions::new(Url::parse("about:blank")?);
    options.executable_override = Some(executable.clone());

    let process = match launch_process(options).await {
        Ok(process) => process,
        Err(error) => {
            return Ok(ExternalEngineDiagnostic::Unhealthy { executable, error });
        }
    };
    let executable = process.executable_path().to_path_buf();
    let endpoint = process.endpoint().websocket_url();
    let (operation, close_error) = diagnose_via_cdp(&endpoint).await;
    let browser_shutdown = shutdown_process(process).await.err();
    match finish_with_cleanup(operation, cleanup_error(close_error, browser_shutdown)) {
        Ok(browser) => Ok(ExternalEngineDiagnostic::Ready {
            executable,
            browser,
        }),
        Err(error) => Ok(ExternalEngineDiagnostic::Unhealthy { executable, error }),
    }
}

impl ExternalRenderedPage {
    /// Produces the stable plain-text cell representation used by `dump --format cells`.
    pub fn cell_text(&self) -> Result<String, ExternalError> {
        self.cells
            .as_ref()
            .map(surface_text)
            .ok_or(ExternalError::MissingCellSurface)
    }

    /// Returns screenshot ANSI output for a screenshot-rendered page.
    pub fn ansi(&self) -> Result<&[u8], ExternalError> {
        self.ansi.as_deref().ok_or(ExternalError::MissingAnsiOutput)
    }

    /// Returns serialized page HTML for a DOM-rendered page.
    pub fn outer_html(&self) -> Result<&str, ExternalError> {
        self.outer_html
            .as_deref()
            .ok_or(ExternalError::MissingOuterHtml)
    }
}

/// Builds the isolated `about:blank` launch policy for an app-selected target.
///
/// An interactive follow-up may reuse this policy translation without accepting raw Chromium
/// arguments or independently parsing proxy/user-agent settings.
pub(crate) fn launch_options_for(
    request: &ExternalRenderRequest,
) -> Result<ExternalEngineLaunchOptions, ExternalError> {
    launch_options(request)
}

/// Launches a previously validated isolated engine policy on Tokio's blocking worker pool.
pub(crate) async fn launch_process(
    options: ExternalEngineLaunchOptions,
) -> Result<ExternalEngineProcess, ExternalError> {
    let cancellation = Cancellation::new();
    tokio::task::spawn_blocking(move || launch_external_engine(&options, &cancellation))
        .await
        .map_err(|source| ExternalError::BlockingTask(source.to_string()))?
        .map_err(ExternalError::from)
}

/// Shuts down an app-owned external process and removes its temporary profile on a blocking worker.
pub(crate) async fn shutdown_process(
    mut process: ExternalEngineProcess,
) -> Result<(), BrowserShutdownFailure> {
    tokio::task::spawn_blocking(move || process.shutdown())
        .await
        .map_err(|source| BrowserShutdownFailure::Task(source.to_string()))?
        .map_err(BrowserShutdownFailure::Engine)
}

async fn render_via_cdp(
    endpoint: &str,
    request: &ExternalRenderRequest,
    output: ExternalOutputKind,
) -> CdpRenderOutcome {
    let mut session = match CdpSession::connect(endpoint, CdpLimits::default()).await {
        Ok(session) => session,
        Err(error) => {
            return CdpRenderOutcome {
                operation: Err(error.into()),
                close_error: None,
            };
        }
    };

    let operation = render_connected_session(&mut session, request, output).await;
    let close_error = session.close().await.err();
    CdpRenderOutcome {
        operation,
        close_error,
    }
}

async fn diagnose_via_cdp(
    endpoint: &str,
) -> (
    Result<ExternalBrowserMetadata, ExternalError>,
    Option<CdpError>,
) {
    let mut session = match CdpSession::connect(endpoint, CdpLimits::default()).await {
        Ok(session) => session,
        Err(error) => return (Err(error.into()), None),
    };
    let operation = session
        .browser_version()
        .await
        .map(browser_metadata)
        .map_err(ExternalError::from);
    let close_error = session.close().await.err();
    (operation, close_error)
}

async fn render_connected_session(
    session: &mut CdpSession,
    request: &ExternalRenderRequest,
    output: ExternalOutputKind,
) -> Result<ExternalRenderedPage, ExternalError> {
    let browser = browser_metadata(session.browser_version().await?);
    let mut page = {
        let mut page = session.attach_first_page().await?;
        page.page_enable().await?;
        page.set_device_metrics(
            request.viewport.width,
            request.viewport.height,
            request.viewport.device_scale,
        )
        .await?;
        let navigation = page.navigate(&request.target).await?;
        if let Some(message) = navigation.error_text.filter(|message| !message.is_empty()) {
            return Err(ExternalError::NavigationFailed { message });
        }
        page.wait_for_load().await?;
        page
    };

    let (cells, ansi, outer_html, accessibility) = match output {
        ExternalOutputKind::DumpDom => {
            let root_node_id = page.dom_get_document().await?;
            let outer_html = page.dom_get_outer_html(root_node_id).await?;
            validate_dom_output(request, &outer_html)?;
            (None, None, Some(outer_html), None)
        }
        ExternalOutputKind::DumpAccessibility => {
            page.accessibility_enable().await?;
            let accessibility = page.accessibility_tree().await?;
            (None, None, None, Some(accessibility))
        }
        ExternalOutputKind::Inspect
        | ExternalOutputKind::SnapshotCells
        | ExternalOutputKind::SnapshotAnsi
        | ExternalOutputKind::DumpCells
        | ExternalOutputKind::RenderOnce => {
            let screenshot = page.capture_screenshot().await?;
            let (cells, ansi) = project_screenshot(request, &screenshot.data)?;
            (Some(cells), Some(ansi), None, None)
        }
    };

    Ok(ExternalRenderedPage {
        target: request.target.clone(),
        browser,
        cells,
        ansi,
        outer_html,
        accessibility,
    })
}

fn launch_options(
    request: &ExternalRenderRequest,
) -> Result<ExternalEngineLaunchOptions, ExternalError> {
    validate_request(request)?;
    let target = Url::parse(&request.target)?;
    if !target.username().is_empty() || target.password().is_some() {
        return Err(ExternalError::TargetCredentialsDisallowed);
    }

    let mut options = ExternalEngineLaunchOptions::new(Url::parse("about:blank")?);
    options.executable_override = request.policies.executable_override.clone();
    options.viewport = Some(ExternalEngineViewport::new(
        request.viewport.width,
        request.viewport.height,
    )?);
    options.user_agent = request.policies.user_agent.clone();
    options.proxy = external_proxy(&request.policies)?;
    options.javascript = if request.policies.disable_javascript {
        ExternalEngineJavaScriptPolicy::Disabled
    } else {
        ExternalEngineJavaScriptPolicy::Enabled
    };
    options.images = if request.policies.disable_images {
        ExternalEngineImagePolicy::Disabled
    } else {
        ExternalEngineImagePolicy::Enabled
    };
    Ok(options)
}

fn validate_request(request: &ExternalRenderRequest) -> Result<(), ExternalError> {
    if request.terminal_columns == 0 || request.terminal_rows == 0 {
        return Err(ExternalError::InvalidTerminalGeometry);
    }
    if request.limits.max_bytes == 0 || request.limits.max_cells == 0 {
        return Err(ExternalError::InvalidRenderLimits);
    }
    if !request.viewport.device_scale.is_finite() || request.viewport.device_scale <= 0.0 {
        return Err(ExternalError::InvalidDeviceScale);
    }
    Ok(())
}

fn validate_dom_output(
    request: &ExternalRenderRequest,
    outer_html: &str,
) -> Result<(), ExternalError> {
    if outer_html.len() > request.limits.max_bytes {
        return Err(ExternalError::DomOutputLimit {
            actual: outer_html.len(),
            maximum: request.limits.max_bytes,
        });
    }
    Ok(())
}

fn external_proxy(
    policies: &ExternalEnginePolicies,
) -> Result<Option<ExternalEngineProxy>, ExternalError> {
    let proxy = policies
        .proxy
        .as_deref()
        .filter(|proxy| !proxy.trim().is_empty());
    let Some(proxy) = proxy else {
        return if policies.proxy_bypass.is_empty() {
            Ok(None)
        } else {
            Err(ExternalError::ProxyBypassWithoutProxy)
        };
    };

    let proxy = ExternalEngineProxy::new(Url::parse(proxy)?)?;
    if policies.proxy_bypass.is_empty() {
        return Ok(Some(proxy));
    }
    let bypass = ExternalEngineProxyBypass::new(
        policies
            .proxy_bypass
            .iter()
            .flat_map(|entries| entries.split(',')),
    )?;
    Ok(Some(proxy.with_bypass(bypass)))
}

fn browser_metadata(version: CdpBrowserVersion) -> ExternalBrowserMetadata {
    ExternalBrowserMetadata {
        protocol_version: version.protocol_version,
        product: version.product,
        revision: version.revision,
        user_agent: version.user_agent,
        javascript_version: version.js_version,
    }
}

fn project_screenshot(
    request: &ExternalRenderRequest,
    screenshot: &str,
) -> Result<(Surface, Vec<u8>), ExternalError> {
    ExternalFrameProjector::new(request)?.project(screenshot)
}

/// Retains an `ExternalFrameSequence` for one caller-owned external screenshot stream.
///
/// The type stays crate-private so a future interactive adapter can reuse the exact decode,
/// projection, and ANSI bounds without widening the external process boundary.
pub(crate) struct ExternalFrameProjector {
    backend: Backend,
    capabilities: TerminalCapabilities,
    columns: u16,
    rows: u16,
    decode_limits: ExternalFrameDecodeLimits,
    projection_limits: ProjectionLimits,
    sequence_limits: ExternalFrameSequenceLimits,
    sequence: ExternalFrameSequence,
}

impl ExternalFrameProjector {
    pub(crate) fn new(request: &ExternalRenderRequest) -> Result<Self, ExternalError> {
        validate_request(request)?;
        Ok(Self {
            backend: cell_projection_backend(request.backend),
            capabilities: request.capabilities,
            columns: request.terminal_columns,
            rows: request.terminal_rows,
            decode_limits: decode_limits(request.limits),
            projection_limits: projection_limits(request.limits),
            sequence_limits: sequence_limits(request.limits),
            sequence: ExternalFrameSequence::new(),
        })
    }

    /// Converts the next CDP screenshot into a bounded cell surface and ANSI diff.
    pub(crate) fn project(
        &mut self,
        screenshot: &str,
    ) -> Result<(Surface, Vec<u8>), ExternalError> {
        let cancellation = Cancellation::new();
        let clock = SystemClock::default();
        let control = TerminalTransactionControl::new(&cancellation, &clock, None);
        let generation = self
            .sequence
            .latest_generation()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(ExternalError::FrameGenerationExhausted)?;
        let output = self.sequence.push_png_base64_for_generation_controlled(
            generation,
            screenshot,
            &self.capabilities,
            ExternalFrameOptions::new(
                self.backend,
                self.columns,
                self.rows,
                self.decode_limits,
                self.projection_limits,
                self.sequence_limits,
            ),
            &control,
        )?;
        let cells = self
            .sequence
            .previous()
            .cloned()
            .ok_or(ExternalError::MissingCellSurface)?;
        let (_, ansi) = output.into_parts();
        Ok((cells, ansi))
    }
}

const fn cell_projection_backend(backend: Backend) -> Backend {
    match backend {
        Backend::Cells | Backend::Halfblock | Backend::Quadrant | Backend::Braille => backend,
        Backend::Auto | Backend::Text => Backend::Cells,
    }
}

fn decode_limits(limits: ExternalRenderLimits) -> ExternalFrameDecodeLimits {
    let defaults = ExternalFrameDecodeLimits::BROWSER_DEFAULT;
    ExternalFrameDecodeLimits {
        max_base64_bytes: defaults
            .max_base64_bytes
            .min(limits.max_bytes.saturating_mul(2)),
        max_png_bytes: defaults.max_png_bytes.min(limits.max_bytes),
        max_width: defaults.max_width,
        max_height: defaults.max_height,
        max_pixels: defaults.max_pixels,
        max_decoded_bytes: defaults.max_decoded_bytes.min(limits.max_bytes),
    }
}

fn projection_limits(limits: ExternalRenderLimits) -> ProjectionLimits {
    ProjectionLimits {
        max_cells: limits.max_cells,
        max_pixels: ExternalFrameDecodeLimits::BROWSER_DEFAULT.max_pixels,
        max_output_bytes: limits.max_bytes,
    }
}

fn sequence_limits(limits: ExternalRenderLimits) -> ExternalFrameSequenceLimits {
    ExternalFrameSequenceLimits {
        max_operations: ExternalFrameSequenceLimits::BROWSER_DEFAULT.max_operations,
        max_output_bytes: limits.max_bytes,
    }
}

fn surface_text(surface: &Surface) -> String {
    let mut output = String::new();
    for row in 0..surface.rows() {
        let mut line = String::new();
        for column in 0..surface.columns() {
            if let Some(cell) = surface.get(column, row) {
                line.push_str(&cell.grapheme);
            }
        }
        output.push_str(line.trim_end());
        output.push('\n');
    }
    output
}

fn finish_with_cleanup<T>(
    operation: Result<T, ExternalError>,
    cleanup: Option<ExternalCleanupError>,
) -> Result<T, ExternalError> {
    match (operation, cleanup) {
        (Ok(value), None) => Ok(value),
        (Ok(_), Some(cleanup)) => Err(ExternalError::Cleanup(cleanup)),
        (Err(operation), None) => Err(operation),
        (Err(operation), Some(cleanup)) => Err(ExternalError::OperationAndCleanup {
            operation: Box::new(operation),
            cleanup: Box::new(cleanup),
        }),
    }
}

fn cleanup_error(
    cdp: Option<CdpError>,
    browser: Option<BrowserShutdownFailure>,
) -> Option<ExternalCleanupError> {
    match (cdp, browser) {
        (None, None) => None,
        (Some(cdp), None) => Some(ExternalCleanupError::CdpClose(cdp)),
        (None, Some(BrowserShutdownFailure::Engine(browser))) => {
            Some(ExternalCleanupError::BrowserShutdown(browser))
        }
        (None, Some(BrowserShutdownFailure::Task(task))) => {
            Some(ExternalCleanupError::BrowserShutdownTask(task))
        }
        (Some(cdp), Some(BrowserShutdownFailure::Engine(browser))) => {
            Some(ExternalCleanupError::CdpAndBrowserShutdown { cdp, browser })
        }
        (Some(cdp), Some(BrowserShutdownFailure::Task(task))) => {
            Some(ExternalCleanupError::CdpAndBrowserShutdownTask { cdp, task })
        }
    }
}

struct CdpRenderOutcome {
    operation: Result<ExternalRenderedPage, ExternalError>,
    close_error: Option<CdpError>,
}

#[derive(Debug)]
pub(crate) enum BrowserShutdownFailure {
    Engine(ExternalEngineError),
    Task(String),
}

/// Cleanup failures retained alongside the primary operation error when both occur.
#[derive(Debug, Error)]
pub enum ExternalCleanupError {
    #[error("failed to close the CDP session: {0}")]
    CdpClose(#[source] CdpError),
    #[error("failed to shut down the external browser: {0}")]
    BrowserShutdown(#[source] ExternalEngineError),
    #[error("external browser shutdown task failed: {0}")]
    BrowserShutdownTask(String),
    #[error(
        "failed to close the CDP session ({cdp}) and shut down the external browser ({browser})"
    )]
    CdpAndBrowserShutdown {
        #[source]
        cdp: CdpError,
        browser: ExternalEngineError,
    },
    #[error("failed to close the CDP session ({cdp}) and run browser shutdown ({task})")]
    CdpAndBrowserShutdownTask {
        #[source]
        cdp: CdpError,
        task: String,
    },
}

#[cfg(test)]
mod tests {
    use tg_terminal::Backend;

    use super::cell_projection_backend;

    #[test]
    fn screenshot_projection_stays_in_the_cell_and_ansi_boundary() {
        assert_eq!(cell_projection_backend(Backend::Cells), Backend::Cells);
        assert_eq!(
            cell_projection_backend(Backend::Halfblock),
            Backend::Halfblock
        );
        assert_eq!(cell_projection_backend(Backend::Braille), Backend::Braille);
        assert_eq!(
            cell_projection_backend(Backend::Quadrant),
            Backend::Quadrant
        );
        assert_eq!(cell_projection_backend(Backend::Text), Backend::Cells);
    }
}
