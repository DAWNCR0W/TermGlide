//! Real-Chrome terminal projection coverage across the public external application APIs.

use std::error::Error;
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use futures_util::{StreamExt, stream};
use termglide::external::{
    self, ExternalEnginePolicies, ExternalRenderLimits, ExternalRenderRequest,
    ExternalRenderedPage, ExternalViewport,
};
use termglide::interactive::{InteractiveError, InteractiveEvent};
use termglide::{
    ExternalInteractiveError, ExternalInteractiveExitReason, ExternalInteractiveLimits,
    run_external_interactive,
};
use tg_browser::{ExternalEngineError, discover_external_engine};
use tg_core::Cancellation;
use tg_terminal::{Backend, Color, ColorLevel, Surface, TerminalCapabilities};
use tokio::sync::oneshot;
use tokio::time::timeout;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

const VIEWPORT_WIDTH: u32 = 320;
const VIEWPORT_HEIGHT: u32 = 192;
const COLUMNS: u16 = 32;
const ROWS: u16 = 12;
const RESIZED_COLUMNS: u16 = 48;
const RESIZED_ROWS: u16 = 18;
const MAX_RENDER_BYTES: usize = 512 * 1024;
const MAX_RENDER_CELLS: usize = 4_096;
const MAX_INTERACTIVE_BYTES: usize = MAX_RENDER_BYTES * 3;
const FRAME_READY_TIMEOUT: Duration = Duration::from_secs(30);
const RESIZE_EVENT_COUNT: usize = 1;

const FIXTURE_DOCUMENT: &str = r#"<!doctype html>
<meta charset="utf-8">
<style>
html,body{margin:0;width:100%;height:100%;overflow:hidden;background:#000}
.tile{position:fixed;width:50%;height:50%}
#northwest{left:0;top:0;background:#000}
#northeast{right:0;top:0;background:#fff}
#southwest{left:0;bottom:0;background:#f00}
#southeast{right:0;bottom:0;background:#00f}
</style>
<div id="northwest" class="tile"></div><div id="northeast" class="tile"></div>
<div id="southwest" class="tile"></div><div id="southeast" class="tile"></div>"#;

#[tokio::test]
async fn external_terminal_backend_matrix_chrome() -> TestResult {
    let executable = match discover_external_engine(None) {
        Ok(path) => path,
        Err(ExternalEngineError::ExecutableNotFound) => return Ok(()),
        Err(error) => return Err(box_error(error)),
    };
    if !executable.is_file() {
        return Err(test_error(
            "discovered external executable is not a regular file",
        ));
    }

    let target = fixture_target();
    verify_prelaunch_refusals(&executable, &target).await?;

    let cells = render_projection(&executable, &target, Backend::Cells).await?;
    require_bounded_page(&cells, &target, "cells projection")?;
    require_all_graphemes(&cells, " ", "forced cells projection")?;

    let ansi256 = external::render(render_request(
        &executable,
        &target,
        Backend::Cells,
        ansi256_capabilities(),
    ))
    .await
    .map_err(box_error)?;
    require_bounded_page(&ansi256, &target, "ANSI-256 cells projection")?;
    require_ansi256_encoding(&ansi256)?;

    let halfblock = render_projection(&executable, &target, Backend::Halfblock).await?;
    require_bounded_page(&halfblock, &target, "halfblock projection")?;
    require_all_graphemes(&halfblock, "▀", "forced halfblock projection")?;

    let quadrant = render_projection(&executable, &target, Backend::Quadrant).await?;
    require_bounded_page(&quadrant, &target, "quadrant projection")?;
    require_colored_quadrants(&quadrant)?;

    let braille = render_projection(&executable, &target, Backend::Braille).await?;
    require_bounded_page(&braille, &target, "braille projection")?;
    require_colored_braille(&braille)?;

    verify_resize_then_active_cancellation(&executable, &target).await
}

async fn render_projection(
    executable: &Path,
    target: &str,
    backend: Backend,
) -> TestResult<ExternalRenderedPage> {
    external::render(render_request(
        executable,
        target,
        backend,
        supported_capabilities(),
    ))
    .await
    .map_err(box_error)
}

fn render_request(
    executable: &Path,
    target: &str,
    backend: Backend,
    capabilities: TerminalCapabilities,
) -> ExternalRenderRequest {
    ExternalRenderRequest {
        target: target.to_owned(),
        policies: ExternalEnginePolicies {
            executable_override: Some(executable.to_path_buf()),
            ..ExternalEnginePolicies::default()
        },
        viewport: ExternalViewport {
            width: VIEWPORT_WIDTH,
            height: VIEWPORT_HEIGHT,
            device_scale: 1.0,
        },
        terminal_columns: COLUMNS,
        terminal_rows: ROWS,
        backend,
        capabilities,
        limits: ExternalRenderLimits {
            max_bytes: MAX_RENDER_BYTES,
            max_cells: MAX_RENDER_CELLS,
        },
    }
}

fn supported_capabilities() -> TerminalCapabilities {
    TerminalCapabilities {
        color: ColorLevel::TrueColor,
        dumb: false,
    }
}

fn ansi256_capabilities() -> TerminalCapabilities {
    TerminalCapabilities {
        color: ColorLevel::Ansi256,
        ..supported_capabilities()
    }
}

fn dumb_capabilities() -> TerminalCapabilities {
    TerminalCapabilities {
        color: ColorLevel::Monochrome,
        dumb: true,
    }
}

fn require_bounded_page(
    page: &ExternalRenderedPage,
    expected_target: &str,
    label: &str,
) -> TestResult {
    if page.target != expected_target {
        return Err(test_error(format!(
            "{label} retained a different target than the generic data fixture"
        )));
    }
    let cells = require_cells(page, label)?;
    let ansi = require_ansi(page, label)?;
    if cells.columns() != COLUMNS || cells.rows() != ROWS {
        return Err(test_error(format!(
            "{label} used {}x{} cells, expected {COLUMNS}x{ROWS}",
            cells.columns(),
            cells.rows()
        )));
    }
    if cells.cells().len() > MAX_RENDER_CELLS {
        return Err(test_error(format!(
            "{label} exceeded its cell bound: {} cells",
            cells.cells().len()
        )));
    }
    if ansi.is_empty() || ansi.len() > MAX_RENDER_BYTES {
        return Err(test_error(format!(
            "{label} used {} ANSI bytes outside 1..={MAX_RENDER_BYTES}",
            ansi.len()
        )));
    }
    let text = page.cell_text().map_err(box_error)?;
    if text.is_empty() || text.len() > MAX_RENDER_CELLS.saturating_mul(4) {
        return Err(test_error(format!(
            "{label} produced an empty or unbounded cell-text representation"
        )));
    }
    Ok(())
}

fn require_all_graphemes(page: &ExternalRenderedPage, expected: &str, label: &str) -> TestResult {
    if require_cells(page, label)?
        .cells()
        .iter()
        .all(|cell| cell.grapheme == expected)
    {
        Ok(())
    } else {
        Err(test_error(format!(
            "{label} did not preserve its expected terminal grapheme"
        )))
    }
}

fn require_colored_braille(page: &ExternalRenderedPage) -> TestResult {
    let cells = require_cells(page, "braille projection")?;
    let all_braille = cells.cells().iter().all(|cell| {
        cell.grapheme.chars().all(|character| {
            let codepoint = u32::from(character);
            (0x2800..=0x28ff).contains(&codepoint)
        })
    });
    if !all_braille {
        return Err(test_error(
            "forced braille projection emitted a non-braille grapheme",
        ));
    }
    for expected in [
        Color::Rgb(0, 0, 0),
        Color::Rgb(255, 255, 255),
        Color::Rgb(255, 0, 0),
        Color::Rgb(0, 0, 255),
    ] {
        if !cells.cells().iter().any(|cell| cell.background == expected) {
            return Err(test_error(format!(
                "forced braille projection did not preserve background color {expected:?}"
            )));
        }
    }
    Ok(())
}

fn require_colored_quadrants(page: &ExternalRenderedPage) -> TestResult {
    const GLYPHS: [&str; 16] = [
        " ", "▘", "▝", "▀", "▖", "▌", "▞", "▛", "▗", "▚", "▐", "▜", "▄", "▙", "▟", "█",
    ];
    let cells = require_cells(page, "quadrant projection")?;
    if !cells
        .cells()
        .iter()
        .all(|cell| GLYPHS.contains(&cell.grapheme.as_str()))
    {
        return Err(test_error(
            "forced quadrant projection emitted a non-quadrant grapheme",
        ));
    }
    for expected in [
        Color::Rgb(0, 0, 0),
        Color::Rgb(255, 255, 255),
        Color::Rgb(255, 0, 0),
        Color::Rgb(0, 0, 255),
    ] {
        if !cells.cells().iter().any(|cell| cell.background == expected) {
            return Err(test_error(format!(
                "forced quadrant projection did not preserve background color {expected:?}"
            )));
        }
    }
    Ok(())
}

fn require_cells<'a>(page: &'a ExternalRenderedPage, label: &str) -> TestResult<&'a Surface> {
    page.cells
        .as_ref()
        .ok_or_else(|| test_error(format!("{label} omitted its screenshot cell surface")))
}

fn require_ansi<'a>(page: &'a ExternalRenderedPage, label: &str) -> TestResult<&'a [u8]> {
    page.ansi()
        .map_err(|error| test_error(format!("{label} omitted ANSI output: {error}")))
}

fn require_ansi256_encoding(page: &ExternalRenderedPage) -> TestResult {
    let ansi = require_ansi(page, "ANSI-256 cells projection")?;
    if !ansi
        .windows(b"48;5;".len())
        .any(|window| window == b"48;5;")
    {
        return Err(test_error(
            "ANSI-256 cells projection omitted indexed background colors",
        ));
    }
    if ansi
        .windows(b"48;2;".len())
        .any(|window| window == b"48;2;")
    {
        return Err(test_error(
            "ANSI-256 cells projection leaked truecolor background escapes",
        ));
    }
    Ok(())
}

async fn verify_prelaunch_refusals(executable: &Path, target: &str) -> TestResult {
    let limits = interactive_limits();

    let mut invalid_geometry =
        render_request(executable, target, Backend::Cells, supported_capabilities());
    invalid_geometry.terminal_columns = 0;
    let mut invalid_writer = Vec::new();
    let invalid_cancellation = Cancellation::new();
    let invalid_result = run_external_interactive(
        invalid_geometry,
        stream::empty::<Result<InteractiveEvent, InteractiveError>>(),
        &mut invalid_writer,
        &invalid_cancellation,
        limits.clone(),
    )
    .await;
    match invalid_result {
        Err(ExternalInteractiveError::InvalidRequest {
            field: "terminal geometry",
        }) if invalid_writer.is_empty() => {}
        Ok(report) => {
            return Err(test_error(format!(
                "invalid terminal geometry unexpectedly launched an interactive run: {report:?}"
            )));
        }
        Err(error) => {
            return Err(test_error(format!(
                "invalid terminal geometry did not fail closed before launch: {error}"
            )));
        }
    }

    let unsupported = render_request(executable, target, Backend::Text, dumb_capabilities());
    let mut unsupported_writer = Vec::new();
    let unsupported_cancellation = Cancellation::new();
    let unsupported_result = run_external_interactive(
        unsupported,
        stream::empty::<Result<InteractiveEvent, InteractiveError>>(),
        &mut unsupported_writer,
        &unsupported_cancellation,
        limits,
    )
    .await;
    match unsupported_result {
        Err(ExternalInteractiveError::UnsupportedBackend {
            backend: Backend::Text,
        }) if unsupported_writer.is_empty() => Ok(()),
        Ok(report) => Err(test_error(format!(
            "a dumb terminal unexpectedly launched an interactive run: {report:?}"
        ))),
        Err(error) => Err(test_error(format!(
            "a dumb terminal did not fail through the typed text refusal: {error}"
        ))),
    }
}

async fn verify_resize_then_active_cancellation(executable: &Path, target: &str) -> TestResult {
    let request = render_request(executable, target, Backend::Cells, supported_capabilities());
    let events = stream::iter([Ok::<InteractiveEvent, InteractiveError>(
        InteractiveEvent::Resize {
            columns: RESIZED_COLUMNS,
            rows: RESIZED_ROWS,
        },
    )])
    .chain(stream::pending::<Result<InteractiveEvent, InteractiveError>>());
    let cancellation = Cancellation::new();
    let (frame_sender, mut frame_receiver) = oneshot::channel();
    let mut writer = FrameWriter::new(2, MAX_INTERACTIVE_BYTES, frame_sender);
    let report = {
        let operation = run_external_interactive(
            request,
            events,
            &mut writer,
            &cancellation,
            interactive_limits(),
        );
        tokio::pin!(operation);

        tokio::select! {
            result = &mut operation => {
                return match result {
                    Ok(report) => Err(test_error(format!(
                        "interactive resize run ended before active cancellation: {report:?}"
                    ))),
                    Err(error) => Err(box_error(error)),
                };
            }
            ready = timeout(FRAME_READY_TIMEOUT, &mut frame_receiver) => {
                match ready {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        return Err(test_error(format!(
                            "interactive frame writer ended before the resize frame: {error}"
                        )));
                    }
                    Err(_) => {
                        return Err(test_error(
                            "interactive resize did not produce two bounded frames before its deadline",
                        ));
                    }
                }
            }
        }

        cancellation.cancel();
        operation.await.map_err(box_error)?
    };
    if report.reason != ExternalInteractiveExitReason::Cancelled {
        return Err(test_error(format!(
            "interactive runner ended as {:?}, not Cancelled",
            report.reason
        )));
    }
    if report.events_processed != RESIZE_EVENT_COUNT {
        return Err(test_error(format!(
            "interactive runner processed {} events, expected the one resize event",
            report.events_processed
        )));
    }
    if report.frames_written < 2 || writer.write_calls < 2 {
        return Err(test_error(format!(
            "interactive resize/cancellation wrote {} frames across {} writes",
            report.frames_written, writer.write_calls
        )));
    }
    let maximum = MAX_RENDER_BYTES
        .checked_mul(report.frames_written)
        .ok_or_else(|| test_error("interactive ANSI output bound overflowed"))?;
    if writer.bytes.is_empty() || writer.bytes.len() > maximum {
        return Err(test_error(format!(
            "interactive resize/cancellation retained {} bytes outside 1..={maximum}",
            writer.bytes.len()
        )));
    }

    // A successful cancellation report is returned only after CDP close, browser shutdown, and
    // isolated-profile removal have all completed in the public interactive application API.
    Ok(())
}

fn interactive_limits() -> ExternalInteractiveLimits {
    ExternalInteractiveLimits {
        max_events: RESIZE_EVENT_COUNT + 1,
        ..ExternalInteractiveLimits::default()
    }
}

struct FrameWriter {
    bytes: Vec<u8>,
    write_calls: usize,
    ready_after_writes: usize,
    maximum_bytes: usize,
    ready: Option<oneshot::Sender<()>>,
}

impl FrameWriter {
    fn new(ready_after_writes: usize, maximum_bytes: usize, ready: oneshot::Sender<()>) -> Self {
        Self {
            bytes: Vec::new(),
            write_calls: 0,
            ready_after_writes,
            maximum_bytes,
            ready: Some(ready),
        }
    }
}

impl Write for FrameWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let next_length = self
            .bytes
            .len()
            .checked_add(buffer.len())
            .ok_or_else(|| io::Error::other("interactive frame output length overflowed"))?;
        if next_length > self.maximum_bytes {
            return Err(io::Error::other(format!(
                "interactive frame output exceeded {} bytes",
                self.maximum_bytes
            )));
        }
        self.bytes.extend_from_slice(buffer);
        self.write_calls = self
            .write_calls
            .checked_add(1)
            .ok_or_else(|| io::Error::other("interactive frame writer counter overflowed"))?;
        if self.write_calls >= self.ready_after_writes
            && let Some(sender) = self.ready.take()
        {
            sender.send(()).map_err(|()| {
                io::Error::other("interactive frame-ready receiver closed unexpectedly")
            })?;
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn fixture_target() -> String {
    let encoded = url::form_urlencoded::byte_serialize(FIXTURE_DOCUMENT.as_bytes())
        .collect::<String>()
        .replace('+', "%20");
    format!("data:text/html,{encoded}")
}

fn box_error<E>(error: E) -> Box<dyn Error + Send + Sync>
where
    E: Error + Send + Sync + 'static,
{
    Box::new(error)
}

fn test_error(message: impl Into<String>) -> Box<dyn Error + Send + Sync> {
    box_error(io::Error::other(message.into()))
}
