//! Public recovery coverage for the generation-aware external PNG projection adapter.
//!
//! Every fixture stays in memory. The tests open no browser, terminal device, or external
//! process; they exercise only the bounded public decoder and terminal projection boundary.

use std::error::Error;
use std::io;
use std::time::SystemTime;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use tg_core::{Cancellation, VirtualClock};
use tg_terminal::{
    Backend, Color, ColorLevel, ExternalFrameDecodeLimits, ExternalFrameError,
    ExternalFrameOptions, ExternalFrameSequence, ExternalFrameSequenceLimits, ProjectionLimits,
    Rgba, Surface, SurfaceError, TerminalCapabilities, TerminalOp, TerminalTransactionControl,
    TerminalTransactionError,
};

type TestError = Box<dyn Error + Send + Sync>;
type TestResult<T = ()> = Result<T, TestError>;

fn decode_limits() -> ExternalFrameDecodeLimits {
    ExternalFrameDecodeLimits {
        max_base64_bytes: 1_024,
        max_png_bytes: 512,
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

fn sequence_limits() -> ExternalFrameSequenceLimits {
    ExternalFrameSequenceLimits {
        max_operations: 128,
        max_output_bytes: 4_096,
    }
}

fn options(backend: Backend, columns: u16, rows: u16) -> ExternalFrameOptions {
    options_with_limits(backend, columns, rows, decode_limits(), sequence_limits())
}

fn options_with_limits(
    backend: Backend,
    columns: u16,
    rows: u16,
    decode_limits: ExternalFrameDecodeLimits,
    sequence_limits: ExternalFrameSequenceLimits,
) -> ExternalFrameOptions {
    ExternalFrameOptions::new(
        backend,
        columns,
        rows,
        decode_limits,
        projection_limits(),
        sequence_limits,
    )
}

fn capabilities(color: ColorLevel, dumb: bool) -> TerminalCapabilities {
    TerminalCapabilities { color, dumb }
}

fn control<'a>(
    cancellation: &'a Cancellation,
    clock: &'a VirtualClock,
) -> TerminalTransactionControl<'a> {
    TerminalTransactionControl::new(cancellation, clock, None)
}

fn rgba_png(width: u32, height: u32, pixels: &[u8]) -> TestResult<Vec<u8>> {
    let required_bytes = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixel_count| pixel_count.checked_mul(4))
        .ok_or_else(|| io::Error::other("fixture PNG dimensions overflowed"))?;
    if pixels.len() != required_bytes {
        return Err(io::Error::other(format!(
            "fixture RGBA length was {}, required {required_bytes}",
            pixels.len()
        ))
        .into());
    }

    let mut png = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(pixels)?;
    }
    Ok(png)
}

fn light_frame() -> TestResult<Vec<u8>> {
    rgba_png(2, 1, &[255, 255, 255, 255, 200, 200, 200, 255])
}

fn dark_frame() -> TestResult<Vec<u8>> {
    rgba_png(2, 1, &[0, 0, 0, 255, 32, 32, 32, 255])
}

#[test]
fn public_generation_adapter_selects_cell_backends_and_composites_alpha() -> TestResult {
    let ansi = capabilities(ColorLevel::Ansi256, false);
    let true_color = capabilities(ColorLevel::TrueColor, false);
    let dumb = capabilities(ColorLevel::Monochrome, true);

    assert_eq!(
        ExternalFrameSequence::select_cell_backend(&ansi, Backend::Cells)?,
        Backend::Cells
    );
    assert_eq!(
        ExternalFrameSequence::select_cell_backend(&true_color, Backend::Halfblock)?,
        Backend::Halfblock
    );
    assert_eq!(
        ExternalFrameSequence::select_cell_backend(&ansi, Backend::Braille)?,
        Backend::Braille
    );
    assert_eq!(
        ExternalFrameSequence::select_cell_backend(&ansi, Backend::Quadrant)?,
        Backend::Quadrant
    );
    assert_eq!(
        ExternalFrameSequence::select_cell_backend(&ansi, Backend::Halfblock)?,
        Backend::Cells
    );
    assert_eq!(
        ExternalFrameSequence::select_cell_backend(&dumb, Backend::Braille)?,
        Backend::Cells
    );

    let cancellation = Cancellation::new();
    let clock = VirtualClock::new(SystemTime::UNIX_EPOCH);
    let transaction_control = control(&cancellation, &clock);
    let transparent_and_partial = rgba_png(
        2,
        1,
        &[
            255, 0, 0, 0, // fully transparent source resolves to the terminal background
            255, 0, 0, 128, // straight-alpha source has deterministic integer composition
        ],
    )?;
    let encoded = BASE64.encode(&transparent_and_partial);
    let terminal_background = Rgba {
        red: 0,
        green: 0,
        blue: 255,
        alpha: u8::MAX,
    };
    let mut sequence = ExternalFrameSequence::new();

    assert!(
        sequence.set_terminal_background_controlled(terminal_background, &transaction_control)?
    );
    assert!(matches!(
        sequence.set_terminal_background_controlled(Rgba::TRANSPARENT, &transaction_control),
        Err(ExternalFrameError::TransparentTerminalBackground)
    ));
    assert_eq!(sequence.terminal_background(), terminal_background);

    let projected = sequence.push_png_base64_for_generation_controlled(
        1,
        &encoded,
        &ansi,
        options(Backend::Cells, 2, 1),
        &transaction_control,
    )?;
    assert!(!projected.is_identical());
    assert_eq!(sequence.latest_generation(), Some(1));
    let surface = sequence
        .previous()
        .ok_or_else(|| io::Error::other("accepted alpha-composited frame was not retained"))?;
    assert_eq!(
        surface.get(0, 0).map(|cell| cell.background),
        Some(Color::Indexed(21))
    );
    assert_eq!(
        surface.get(1, 0).map(|cell| cell.background),
        Some(Color::Indexed(126))
    );

    let candidate_frame = light_frame()?;
    for (requested, capability) in [
        (Backend::Cells, ansi),
        (Backend::Halfblock, true_color),
        (Backend::Quadrant, ansi),
        (Backend::Braille, ansi),
    ] {
        let mut candidate = ExternalFrameSequence::new();
        let output = candidate.push_png_bytes_for_generation_controlled(
            1,
            &candidate_frame,
            &capability,
            options(requested, 2, 1),
            &transaction_control,
        )?;
        assert!(
            !output.is_identical(),
            "{requested:?} capability selection emitted no baseline"
        );
        assert_eq!(candidate.latest_generation(), Some(1));
    }
    Ok(())
}

#[test]
fn public_generation_adapter_preserves_last_good_across_rejections_and_recovers_after_resize()
-> TestResult {
    let cancellation = Cancellation::new();
    let clock = VirtualClock::new(SystemTime::UNIX_EPOCH);
    let transaction_control = control(&cancellation, &clock);
    let cell_capabilities = capabilities(ColorLevel::Ansi256, false);
    let initial = light_frame()?;
    let changed = dark_frame()?;
    let resized = rgba_png(1, 1, &[17, 18, 19, 255])?;
    let mut sequence = ExternalFrameSequence::new();

    let png_limit = initial
        .len()
        .checked_sub(1)
        .ok_or_else(|| io::Error::other("fixture PNG was unexpectedly empty"))?;
    let source_limited = ExternalFrameDecodeLimits {
        max_png_bytes: png_limit,
        ..decode_limits()
    };
    assert!(matches!(
        sequence.push_png_bytes_for_generation_controlled(
            1,
            &initial,
            &cell_capabilities,
            options_with_limits(Backend::Cells, 2, 1, source_limited, sequence_limits()),
            &transaction_control,
        ),
        Err(ExternalFrameError::PngInputLimit { .. })
    ));
    assert!(sequence.previous().is_none());
    assert_eq!(sequence.latest_generation(), None);

    let decoded_limited = ExternalFrameDecodeLimits {
        max_decoded_bytes: 7,
        ..decode_limits()
    };
    assert!(matches!(
        sequence.push_png_bytes_for_generation_controlled(
            1,
            &initial,
            &cell_capabilities,
            options_with_limits(Backend::Cells, 2, 1, decoded_limited, sequence_limits()),
            &transaction_control,
        ),
        Err(ExternalFrameError::DecodedByteLimit { .. })
    ));
    assert!(sequence.previous().is_none());
    assert_eq!(sequence.latest_generation(), None);

    assert!(matches!(
        sequence.push_png_bytes_for_generation_controlled(
            1,
            &initial,
            &cell_capabilities,
            options(Backend::Cells, 0, 1),
            &transaction_control,
        ),
        Err(ExternalFrameError::Projection(SurfaceError::ZeroDimensions))
    ));
    assert!(sequence.previous().is_none());
    assert_eq!(sequence.latest_generation(), None);

    let output_limited = ExternalFrameSequenceLimits {
        max_output_bytes: 1,
        ..sequence_limits()
    };
    assert!(matches!(
        sequence.push_png_bytes_for_generation_controlled(
            1,
            &initial,
            &cell_capabilities,
            options_with_limits(Backend::Cells, 2, 1, decode_limits(), output_limited),
            &transaction_control,
        ),
        Err(ExternalFrameError::Projection(SurfaceError::OutputLimit))
    ));
    assert!(sequence.previous().is_none());
    assert_eq!(sequence.latest_generation(), None);

    let accepted = sequence.push_png_bytes_for_generation_controlled(
        1,
        &initial,
        &cell_capabilities,
        options(Backend::Cells, 2, 1),
        &transaction_control,
    )?;
    assert!(!accepted.is_identical());
    let last_good = sequence
        .previous()
        .cloned()
        .ok_or_else(|| io::Error::other("accepted frame was not retained"))?;
    assert_eq!(sequence.latest_generation(), Some(1));

    assert!(matches!(
        sequence.push_png_bytes_for_generation_controlled(
            1,
            b"stale frames must not enter PNG decode",
            &cell_capabilities,
            options(Backend::Cells, 2, 1),
            &transaction_control,
        ),
        Err(ExternalFrameError::StaleGeneration {
            received: 1,
            latest: 1,
        })
    ));
    assert_eq!(sequence.previous(), Some(&last_good));
    assert_eq!(sequence.latest_generation(), Some(1));

    assert!(matches!(
        sequence.push_png_bytes_for_generation_controlled(
            2,
            &changed,
            &cell_capabilities,
            options_with_limits(Backend::Cells, 2, 1, decode_limits(), output_limited),
            &transaction_control,
        ),
        Err(ExternalFrameError::Projection(SurfaceError::OutputLimit))
    ));
    assert_eq!(sequence.previous(), Some(&last_good));
    assert_eq!(sequence.latest_generation(), Some(1));

    let cancelled = Cancellation::new();
    cancelled.cancel();
    let cancelled_control = control(&cancelled, &clock);
    assert!(matches!(
        sequence.push_png_bytes_for_generation_controlled(
            2,
            &changed,
            &cell_capabilities,
            options(Backend::Cells, 2, 1),
            &cancelled_control,
        ),
        Err(ExternalFrameError::Transaction(
            TerminalTransactionError::Cancelled
        ))
    ));
    assert_eq!(sequence.previous(), Some(&last_good));
    assert_eq!(sequence.latest_generation(), Some(1));

    assert!(sequence.resize_controlled(1, 1, &transaction_control)?);
    assert!(sequence.previous().is_none());
    assert_eq!(sequence.latest_generation(), Some(1));
    assert!(matches!(
        sequence.push_png_bytes_for_generation_controlled(
            1,
            b"stale frames remain rejected after resize",
            &cell_capabilities,
            options(Backend::Cells, 1, 1),
            &transaction_control,
        ),
        Err(ExternalFrameError::StaleGeneration {
            received: 1,
            latest: 1,
        })
    ));
    assert!(sequence.previous().is_none());
    assert_eq!(sequence.latest_generation(), Some(1));

    let recovered = sequence.push_png_bytes_for_generation_controlled(
        2,
        &resized,
        &cell_capabilities,
        options(Backend::Cells, 1, 1),
        &transaction_control,
    )?;
    assert!(!recovered.is_identical());
    assert!(matches!(
        recovered.operations().first(),
        Some(TerminalOp::ClearScreen)
    ));
    assert_eq!(sequence.previous().map(Surface::columns), Some(1));
    assert_eq!(sequence.latest_generation(), Some(2));
    Ok(())
}
