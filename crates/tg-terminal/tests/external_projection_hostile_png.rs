//! Public hostile-input coverage for the bounded external screenshot projection boundary.
//!
//! The fixtures are tiny in-memory PNGs only. No browser, terminal device, or external process
//! is opened by these tests.

use std::error::Error;
use std::io;
use std::time::SystemTime;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use tg_core::{Cancellation, VirtualClock};
use tg_terminal::{
    Backend, ExternalFrameDecodeLimits, ExternalFrameError, ExternalFrameOptions,
    ExternalFrameSequence, ExternalFrameSequenceLimits, ProjectionLimits, SurfaceError,
    TerminalTransactionControl, TerminalTransactionError,
    decode_external_frame_png_base64_controlled, decode_external_frame_png_bytes_controlled,
    project_external_frame_png_bytes_controlled,
};

type TestError = Box<dyn Error + Send + Sync>;
type TestResult<T = ()> = Result<T, TestError>;

const COLUMNS: u16 = 2;
const ROWS: u16 = 1;

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

fn frame_options(
    backend: Backend,
    columns: u16,
    rows: u16,
    projection_limits: ProjectionLimits,
    sequence_limits: ExternalFrameSequenceLimits,
) -> ExternalFrameOptions {
    ExternalFrameOptions::new(
        backend,
        columns,
        rows,
        decode_limits(),
        projection_limits,
        sequence_limits,
    )
}

fn transaction_control<'a>(
    cancellation: &'a Cancellation,
    clock: &'a VirtualClock,
) -> TerminalTransactionControl<'a> {
    TerminalTransactionControl::new(cancellation, clock, None)
}

fn rgba_png(width: u32, height: u32, pixels: &[u8]) -> TestResult<Vec<u8>> {
    let expected = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| io::Error::other("fixture PNG dimensions overflowed"))?;
    if pixels.len() != expected {
        return Err(io::Error::other(format!(
            "fixture RGBA length was {}, expected {expected}",
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

fn white_frame() -> TestResult<Vec<u8>> {
    rgba_png(
        2,
        2,
        &[
            255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
        ],
    )
}

fn black_frame() -> TestResult<Vec<u8>> {
    rgba_png(
        2,
        2,
        &[0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255],
    )
}

#[test]
fn public_external_png_rejects_malformed_base64_png_crc_chunk_and_input_bounds() -> TestResult {
    let cancellation = Cancellation::new();
    let clock = VirtualClock::new(SystemTime::UNIX_EPOCH);
    let control = transaction_control(&cancellation, &clock);
    let png = white_frame()?;
    let encoded = BASE64.encode(&png);

    assert!(matches!(
        decode_external_frame_png_base64_controlled("not valid base64!", decode_limits(), &control),
        Err(ExternalFrameError::Base64(_))
    ));
    assert!(matches!(
        decode_external_frame_png_bytes_controlled(b"not a PNG", decode_limits(), &control),
        Err(ExternalFrameError::Png(_))
    ));

    let mut corrupt_crc = png.clone();
    let corrupt_byte = corrupt_crc
        .last_mut()
        .ok_or_else(|| io::Error::other("fixture PNG was unexpectedly empty"))?;
    *corrupt_byte ^= 0xff;
    assert!(matches!(
        decode_external_frame_png_bytes_controlled(&corrupt_crc, decode_limits(), &control),
        Err(ExternalFrameError::Png(_))
    ));

    let truncated_length = png
        .len()
        .checked_sub(1)
        .ok_or_else(|| io::Error::other("fixture PNG was too short to truncate"))?;
    let truncated_chunk = &png[..truncated_length];
    assert!(matches!(
        decode_external_frame_png_bytes_controlled(truncated_chunk, decode_limits(), &control),
        Err(ExternalFrameError::Png(_))
    ));

    let base64_limit = encoded
        .len()
        .checked_sub(1)
        .ok_or_else(|| io::Error::other("fixture base64 was unexpectedly empty"))?;
    let encoded_limited = ExternalFrameDecodeLimits {
        max_base64_bytes: base64_limit,
        ..decode_limits()
    };
    assert!(matches!(
        decode_external_frame_png_base64_controlled(&encoded, encoded_limited, &control),
        Err(ExternalFrameError::Base64InputLimit { .. })
    ));

    let png_limit = png
        .len()
        .checked_sub(1)
        .ok_or_else(|| io::Error::other("fixture PNG was unexpectedly empty"))?;
    let png_limited = ExternalFrameDecodeLimits {
        max_png_bytes: png_limit,
        ..decode_limits()
    };
    assert!(matches!(
        decode_external_frame_png_bytes_controlled(&png, png_limited, &control),
        Err(ExternalFrameError::PngInputLimit { .. })
    ));
    Ok(())
}

#[test]
fn public_external_projection_enforces_decoded_pixel_cell_geometry_and_cancellation_bounds()
-> TestResult {
    let cancellation = Cancellation::new();
    let clock = VirtualClock::new(SystemTime::UNIX_EPOCH);
    let control = transaction_control(&cancellation, &clock);
    let png = white_frame()?;

    let decoded_limited = ExternalFrameDecodeLimits {
        max_decoded_bytes: 15,
        ..decode_limits()
    };
    assert!(matches!(
        decode_external_frame_png_bytes_controlled(&png, decoded_limited, &control),
        Err(ExternalFrameError::DecodedByteLimit { .. })
    ));

    let pixel_limited = ExternalFrameDecodeLimits {
        max_pixels: 3,
        ..decode_limits()
    };
    assert!(matches!(
        decode_external_frame_png_bytes_controlled(&png, pixel_limited, &control),
        Err(ExternalFrameError::PixelLimit { .. })
    ));

    let cell_limited = ProjectionLimits {
        max_cells: 3,
        ..projection_limits()
    };
    assert!(matches!(
        project_external_frame_png_bytes_controlled(
            &png,
            frame_options(Backend::Cells, 2, 2, cell_limited, sequence_limits()),
            &control,
        ),
        Err(ExternalFrameError::Projection(SurfaceError::CellLimit))
    ));
    assert!(matches!(
        project_external_frame_png_bytes_controlled(
            &png,
            frame_options(
                Backend::Cells,
                0,
                ROWS,
                projection_limits(),
                sequence_limits(),
            ),
            &control,
        ),
        Err(ExternalFrameError::Projection(SurfaceError::ZeroDimensions))
    ));

    let cancelled = Cancellation::new();
    cancelled.cancel();
    let cancelled_control = transaction_control(&cancelled, &clock);
    assert!(matches!(
        decode_external_frame_png_bytes_controlled(
            b"not a PNG",
            decode_limits(),
            &cancelled_control
        ),
        Err(ExternalFrameError::Transaction(
            TerminalTransactionError::Cancelled
        ))
    ));
    Ok(())
}

#[test]
fn public_external_frame_sequence_preserves_last_good_and_recovers_for_cell_backends() -> TestResult
{
    let first = white_frame()?;
    let first_base64 = BASE64.encode(&first);
    let second = black_frame()?;
    let mut corrupt_candidate = second.clone();
    let corrupt_byte = corrupt_candidate
        .last_mut()
        .ok_or_else(|| io::Error::other("recovery PNG was unexpectedly empty"))?;
    *corrupt_byte ^= 0xff;

    for backend in [
        Backend::Cells,
        Backend::Halfblock,
        Backend::Quadrant,
        Backend::Braille,
    ] {
        let cancellation = Cancellation::new();
        let clock = VirtualClock::new(SystemTime::UNIX_EPOCH);
        let control = transaction_control(&cancellation, &clock);
        let mut sequence = ExternalFrameSequence::new();

        let initial = sequence.push_png_base64_controlled(
            &first_base64,
            frame_options(
                backend,
                COLUMNS,
                ROWS,
                projection_limits(),
                sequence_limits(),
            ),
            &control,
        )?;
        assert!(!initial.is_identical(), "{backend:?} baseline was empty");
        assert!(
            !initial.output().is_empty(),
            "{backend:?} baseline emitted no ANSI"
        );
        let last_good = sequence
            .previous()
            .cloned()
            .ok_or_else(|| io::Error::other("accepted baseline was not retained"))?;

        assert!(matches!(
            sequence.push_png_bytes_controlled(
                &corrupt_candidate,
                frame_options(
                    backend,
                    COLUMNS,
                    ROWS,
                    projection_limits(),
                    sequence_limits(),
                ),
                &control,
            ),
            Err(ExternalFrameError::Png(_))
        ));
        assert_eq!(
            sequence.previous(),
            Some(&last_good),
            "{backend:?} CRC failure changed the baseline"
        );

        let ansi_limited = ExternalFrameSequenceLimits {
            max_output_bytes: 1,
            ..sequence_limits()
        };
        assert!(matches!(
            sequence.push_png_bytes_controlled(
                &second,
                frame_options(backend, COLUMNS, ROWS, projection_limits(), ansi_limited,),
                &control,
            ),
            Err(ExternalFrameError::Projection(SurfaceError::OutputLimit))
        ));
        assert_eq!(
            sequence.previous(),
            Some(&last_good),
            "{backend:?} ANSI limit changed the baseline"
        );

        let cancelled = Cancellation::new();
        cancelled.cancel();
        let cancelled_control = transaction_control(&cancelled, &clock);
        assert!(matches!(
            sequence.push_png_bytes_controlled(
                &second,
                frame_options(
                    backend,
                    COLUMNS,
                    ROWS,
                    projection_limits(),
                    sequence_limits(),
                ),
                &cancelled_control,
            ),
            Err(ExternalFrameError::Transaction(
                TerminalTransactionError::Cancelled
            ))
        ));
        assert_eq!(
            sequence.previous(),
            Some(&last_good),
            "{backend:?} cancellation changed the baseline"
        );

        let recovered = sequence.push_png_bytes_controlled(
            &second,
            frame_options(
                backend,
                COLUMNS,
                ROWS,
                projection_limits(),
                sequence_limits(),
            ),
            &control,
        )?;
        assert!(
            !recovered.is_identical(),
            "{backend:?} did not recover a changed frame"
        );
        assert!(
            !recovered.output().is_empty(),
            "{backend:?} recovery emitted no ANSI"
        );
        assert_ne!(
            sequence.previous(),
            Some(&last_good),
            "{backend:?} recovery did not refresh the baseline"
        );
    }
    Ok(())
}
