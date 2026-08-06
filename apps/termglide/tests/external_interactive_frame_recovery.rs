//! Public, process-free recovery coverage for the interactive external screenshot adapter.
//!
//! The PNG fixtures are fixed in-memory base64 CDP payloads. This test starts no Chrome, browser
//! process, terminal device, network connection, or screen stream.

use std::error::Error;
use std::io;
use std::time::SystemTime;

use termglide::external_interactive::{ExternalInteractiveError, ExternalInteractiveFrameAdapter};
use tg_core::{Cancellation, VirtualClock};
use tg_terminal::{
    Backend, ColorLevel, ExternalFrameDecodeLimits, ExternalFrameError,
    ExternalFrameSequenceLimits, ProjectionLimits, Surface, SurfaceError, TerminalCapabilities,
    TerminalOp, TerminalTransactionControl, TerminalTransactionError,
};

type TestError = Box<dyn Error + Send + Sync>;
type TestResult<T = ()> = Result<T, TestError>;

const BLACK_PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNgYGD4DwABBAEAX+XDSwAAAABJRU5ErkJggg==";
const CHECKER_PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAgAAAAECAYAAACzzX7wAAAAF0lEQVR4nGNgYGD4DwI4abySQJqBYhMAdShPsfMXSbYAAAAASUVORK5CYII=";

fn decode_limits() -> ExternalFrameDecodeLimits {
    ExternalFrameDecodeLimits {
        max_base64_bytes: 4_096,
        max_png_bytes: 2_048,
        max_width: 8,
        max_height: 8,
        max_pixels: 64,
        max_decoded_bytes: 256,
    }
}

fn projection_limits() -> ProjectionLimits {
    ProjectionLimits {
        max_cells: 64,
        max_pixels: 64,
        max_output_bytes: 4_096,
    }
}

fn sequence_limits(max_output_bytes: usize) -> ExternalFrameSequenceLimits {
    ExternalFrameSequenceLimits {
        max_operations: 512,
        max_output_bytes,
    }
}

fn capabilities(color: ColorLevel) -> TerminalCapabilities {
    TerminalCapabilities { color, dumb: false }
}

fn adapter(
    terminal_capabilities: TerminalCapabilities,
    requested_backend: Backend,
    columns: u16,
    rows: u16,
    max_output_bytes: usize,
) -> Result<ExternalInteractiveFrameAdapter, ExternalInteractiveError> {
    ExternalInteractiveFrameAdapter::new(
        terminal_capabilities,
        requested_backend,
        columns,
        rows,
        decode_limits(),
        projection_limits(),
        sequence_limits(max_output_bytes),
    )
}

fn control<'a>(
    cancellation: &'a Cancellation,
    clock: &'a VirtualClock,
) -> TerminalTransactionControl<'a> {
    TerminalTransactionControl::new(cancellation, clock, None)
}

fn assert_last_known_good(
    adapter: &ExternalInteractiveFrameAdapter,
    baseline: &Surface,
    generation: u64,
) {
    assert_eq!(adapter.previous(), Some(baseline));
    assert_eq!(adapter.latest_generation(), Some(generation));
}

#[test]
fn public_interactive_frame_adapter_selects_and_projects_cell_backends() -> TestResult {
    let cancellation = Cancellation::new();
    let clock = VirtualClock::new(SystemTime::UNIX_EPOCH);
    let transaction_control = control(&cancellation, &clock);
    let ansi = capabilities(ColorLevel::Ansi256);
    let true_color = capabilities(ColorLevel::TrueColor);

    for (terminal_capabilities, requested_backend, selected_backend) in [
        (ansi, Backend::Cells, Backend::Cells),
        (true_color, Backend::Halfblock, Backend::Halfblock),
        (ansi, Backend::Quadrant, Backend::Quadrant),
        (ansi, Backend::Braille, Backend::Braille),
        (ansi, Backend::Halfblock, Backend::Cells),
    ] {
        let mut frame_adapter = adapter(terminal_capabilities, requested_backend, 1, 1, 4_096)?;
        assert_eq!(frame_adapter.selected_backend(), selected_backend);
        let output = frame_adapter
            .project_next_png_base64_controlled(BLACK_PNG_BASE64, &transaction_control)?;
        assert!(!output.is_identical());
        assert!(!output.output().is_empty());
        assert_eq!(frame_adapter.latest_generation(), Some(1));
    }
    Ok(())
}

#[test]
fn public_interactive_frame_adapter_preserves_lkg_across_hostile_frames_and_recovers() -> TestResult
{
    let active = Cancellation::new();
    let clock = VirtualClock::new(SystemTime::UNIX_EPOCH);
    let transaction_control = control(&active, &clock);
    let ansi = capabilities(ColorLevel::Ansi256);
    let mut frame_adapter = adapter(ansi, Backend::Cells, 8, 4, 200)?;

    let baseline_output =
        frame_adapter.project_next_png_base64_controlled(BLACK_PNG_BASE64, &transaction_control)?;
    assert!(!baseline_output.is_identical());
    assert!(baseline_output.output().len() <= 200);
    let last_good = frame_adapter
        .previous()
        .cloned()
        .ok_or_else(|| io::Error::other("accepted interactive baseline was not retained"))?;
    assert_eq!(frame_adapter.latest_generation(), Some(1));

    assert!(matches!(
        frame_adapter.project_png_base64_for_generation_controlled(
            1,
            "stale screenshots must not enter PNG decode",
            &transaction_control,
        ),
        Err(ExternalInteractiveError::Frame(
            ExternalFrameError::StaleGeneration {
                received: 1,
                latest: 1,
            }
        ))
    ));
    assert_last_known_good(&frame_adapter, &last_good, 1);

    let cancelled = Cancellation::new();
    cancelled.cancel();
    let cancelled_control = control(&cancelled, &clock);
    assert!(matches!(
        frame_adapter.project_next_png_base64_controlled(CHECKER_PNG_BASE64, &cancelled_control),
        Err(ExternalInteractiveError::Frame(
            ExternalFrameError::Transaction(TerminalTransactionError::Cancelled)
        ))
    ));
    assert_last_known_good(&frame_adapter, &last_good, 1);

    assert!(matches!(
        frame_adapter.project_next_png_base64_controlled(CHECKER_PNG_BASE64, &transaction_control),
        Err(ExternalInteractiveError::Frame(
            ExternalFrameError::Projection(SurfaceError::OutputLimit)
        ))
    ));
    assert_last_known_good(&frame_adapter, &last_good, 1);

    let recovered =
        frame_adapter.project_next_png_base64_controlled(BLACK_PNG_BASE64, &transaction_control)?;
    assert!(recovered.is_identical());
    assert_eq!(frame_adapter.latest_generation(), Some(4));
    assert_last_known_good(&frame_adapter, &last_good, 4);

    assert!(frame_adapter.resize_controlled(2, 1, &transaction_control)?);
    assert_eq!(frame_adapter.columns(), 2);
    assert_eq!(frame_adapter.rows(), 1);
    assert!(frame_adapter.previous().is_none());
    assert_eq!(frame_adapter.latest_generation(), Some(4));
    let resized =
        frame_adapter.project_next_png_base64_controlled(BLACK_PNG_BASE64, &transaction_control)?;
    assert!(!resized.is_identical());
    assert!(matches!(
        resized.operations().first(),
        Some(TerminalOp::ClearScreen)
    ));
    assert_eq!(frame_adapter.latest_generation(), Some(5));

    assert!(frame_adapter.reset_controlled(&transaction_control)?);
    assert!(frame_adapter.previous().is_none());
    assert_eq!(frame_adapter.latest_generation(), Some(5));
    let clean_recovery =
        frame_adapter.project_next_png_base64_controlled(BLACK_PNG_BASE64, &transaction_control)?;
    assert!(!clean_recovery.is_identical());
    assert!(matches!(
        clean_recovery.operations().first(),
        Some(TerminalOp::ClearScreen)
    ));
    assert_eq!(frame_adapter.latest_generation(), Some(6));
    Ok(())
}
