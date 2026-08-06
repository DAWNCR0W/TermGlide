//! Deterministic bounded soak coverage for public external PNG projection recovery.
//!
//! The fixture only creates tiny in-memory RGBA PNG payloads. It starts no browser, terminal
//! device, process, or network connection, and it delegates all PNG decoding to the public
//! external-frame API.

use std::error::Error;
use std::io;
use std::time::{Duration, Instant};

use tg_core::{Cancellation, SystemClock};
use tg_terminal::{
    Backend, ColorLevel, ExternalFrameDecodeLimits, ExternalFrameError, ExternalFrameOptions,
    ExternalFrameSequence, ExternalFrameSequenceLimits, ProjectionLimits, Surface, SurfaceError,
    TerminalCapabilities, TerminalOp, TerminalTransactionControl, TerminalTransactionError,
};

type TestError = Box<dyn Error + Send + Sync>;
type TestResult<T = ()> = Result<T, TestError>;

const SOURCE_WIDTH: u32 = 4;
const SOURCE_HEIGHT: u32 = 4;
const SOAK_GENERATIONS: u64 = 384;
const CAPABILITY_BLOCK: u64 = 12;
const RESIZE_PERIOD: u64 = 37;
const STALE_PERIOD: u64 = 17;
const QUOTA_PERIOD: u64 = 23;
const CANCELLATION_PERIOD: u64 = 29;
const MAX_OUTPUT_BYTES: usize = 8 * 1024;
const MAX_RETAINED_CELLS: usize = 9;
const SOAK_WALL_CLOCK_BUDGET: Duration = Duration::from_secs(90);
const GEOMETRIES: [(u16, u16); 3] = [(4, 2), (3, 3), (2, 4)];

fn decode_limits() -> ExternalFrameDecodeLimits {
    ExternalFrameDecodeLimits {
        max_base64_bytes: 8 * 1024,
        max_png_bytes: 4 * 1024,
        max_width: 8,
        max_height: 8,
        max_pixels: 64,
        max_decoded_bytes: 256,
    }
}

fn projection_limits() -> ProjectionLimits {
    ProjectionLimits {
        max_cells: 16,
        max_pixels: 64,
        max_output_bytes: MAX_OUTPUT_BYTES,
    }
}

fn sequence_limits() -> ExternalFrameSequenceLimits {
    ExternalFrameSequenceLimits {
        max_operations: 256,
        max_output_bytes: MAX_OUTPUT_BYTES,
    }
}

fn quota_sequence_limits() -> ExternalFrameSequenceLimits {
    ExternalFrameSequenceLimits {
        max_operations: 256,
        max_output_bytes: 1,
    }
}

fn frame_options(
    backend: Backend,
    columns: u16,
    rows: u16,
    sequence_limits: ExternalFrameSequenceLimits,
) -> ExternalFrameOptions {
    ExternalFrameOptions::new(
        backend,
        columns,
        rows,
        decode_limits(),
        projection_limits(),
        sequence_limits,
    )
}

fn capabilities(color: ColorLevel) -> TerminalCapabilities {
    TerminalCapabilities { color, dumb: false }
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

fn solid_png(color: [u8; 4]) -> TestResult<Vec<u8>> {
    let pixel_count = usize::try_from(SOURCE_WIDTH)
        .ok()
        .and_then(|width| {
            usize::try_from(SOURCE_HEIGHT)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .ok_or_else(|| io::Error::other("fixture pixel count overflowed"))?;
    let mut pixels = Vec::with_capacity(pixel_count.saturating_mul(4));
    for _ in 0..pixel_count {
        pixels.extend_from_slice(&color);
    }
    rgba_png(SOURCE_WIDTH, SOURCE_HEIGHT, &pixels)
}

fn cell_count(columns: u16, rows: u16) -> TestResult<usize> {
    let count = usize::from(columns)
        .checked_mul(usize::from(rows))
        .ok_or_else(|| io::Error::other("terminal geometry overflowed"))?;
    Ok(count)
}

fn assert_preserved(
    sequence: &ExternalFrameSequence,
    baseline: &Option<Surface>,
    generation: Option<u64>,
) {
    assert_eq!(sequence.previous(), baseline.as_ref());
    assert_eq!(sequence.latest_generation(), generation);
}

#[test]
fn public_external_projection_sequence_stays_bounded_under_deterministic_soak() -> TestResult {
    let frames = [
        solid_png([24, 42, 68, u8::MAX])?,
        solid_png([214, 38, 48, u8::MAX])?,
        solid_png([36, 192, 74, u8::MAX])?,
        solid_png([48, 70, 216, u8::MAX])?,
    ];
    let quota_candidate = solid_png([0, 0, 0, u8::MAX])?;
    let ansi = capabilities(ColorLevel::Ansi256);
    let true_color = capabilities(ColorLevel::TrueColor);
    let active = Cancellation::new();
    let clock = SystemClock::default();
    let control = TerminalTransactionControl::new(&active, &clock, Some(SOAK_WALL_CLOCK_BUDGET));
    let started = Instant::now();
    let mut sequence = ExternalFrameSequence::new();
    let mut geometry_index = 0usize;
    let mut geometry = GEOMETRIES[geometry_index];
    let mut last_variant = None;
    let mut last_backend = None;
    let mut accepted_generations = 0u64;
    let mut identical_updates = 0u64;
    let mut delta_updates = 0u64;
    let mut full_redraws = 0u64;
    let mut resize_count = 0u64;
    let mut capability_transitions = 0u64;
    let mut stale_rejections = 0u64;
    let mut quota_rejections = 0u64;
    let mut cancellation_rejections = 0u64;
    let mut total_output_bytes = 0usize;
    let mut max_observed_retained_cells = 0usize;

    for generation in 1..=SOAK_GENERATIONS {
        let mut baseline_reset = false;
        if generation > 1 && generation.is_multiple_of(RESIZE_PERIOD) {
            geometry_index = (geometry_index + 1) % GEOMETRIES.len();
            geometry = GEOMETRIES[geometry_index];
            assert!(sequence.resize_controlled(geometry.0, geometry.1, &control)?);
            baseline_reset = true;
            resize_count += 1;
        }

        let capability_block = (generation - 1) / CAPABILITY_BLOCK;
        let (frame_capabilities, requested_backend) = match capability_block % 4 {
            0 => (&ansi, Backend::Cells),
            1 => (&true_color, Backend::Halfblock),
            2 => (&ansi, Backend::Braille),
            _ => (&ansi, Backend::Halfblock),
        };
        let selected_backend =
            ExternalFrameSequence::select_cell_backend(frame_capabilities, requested_backend)?;
        if last_backend.is_some_and(|previous| previous != selected_backend) {
            capability_transitions += 1;
        }

        let variant = match (generation - 1) % 6 {
            0 | 1 => 0usize,
            2 | 3 => 1usize,
            4 => 2usize,
            _ => 3usize,
        };
        let baseline_before_hostile = sequence.previous().cloned();
        let generation_before_hostile = sequence.latest_generation();
        if generation > 1 {
            assert_eq!(generation_before_hostile, Some(generation - 1));
        }

        if generation.is_multiple_of(STALE_PERIOD) {
            assert!(matches!(
                sequence.push_png_bytes_for_generation_controlled(
                    generation - 1,
                    b"stale candidates must not enter PNG decode",
                    frame_capabilities,
                    frame_options(
                        requested_backend,
                        geometry.0,
                        geometry.1,
                        sequence_limits(),
                    ),
                    &control,
                ),
                Err(ExternalFrameError::StaleGeneration { received, latest })
                    if received == generation - 1 && latest == generation - 1
            ));
            assert_preserved(
                &sequence,
                &baseline_before_hostile,
                generation_before_hostile,
            );
            stale_rejections += 1;
        }

        if generation.is_multiple_of(QUOTA_PERIOD) {
            assert!(matches!(
                sequence.push_png_bytes_for_generation_controlled(
                    generation,
                    &quota_candidate,
                    frame_capabilities,
                    frame_options(
                        requested_backend,
                        geometry.0,
                        geometry.1,
                        quota_sequence_limits(),
                    ),
                    &control,
                ),
                Err(ExternalFrameError::Projection(SurfaceError::OutputLimit))
            ));
            assert_preserved(
                &sequence,
                &baseline_before_hostile,
                generation_before_hostile,
            );
            quota_rejections += 1;
        }

        if generation.is_multiple_of(CANCELLATION_PERIOD) {
            let cancelled = Cancellation::new();
            cancelled.cancel();
            let cancelled_control =
                TerminalTransactionControl::new(&cancelled, &clock, Some(SOAK_WALL_CLOCK_BUDGET));
            assert!(matches!(
                sequence.push_png_bytes_for_generation_controlled(
                    generation,
                    b"cancelled candidates must not enter PNG decode",
                    frame_capabilities,
                    frame_options(requested_backend, geometry.0, geometry.1, sequence_limits(),),
                    &cancelled_control,
                ),
                Err(ExternalFrameError::Transaction(
                    TerminalTransactionError::Cancelled
                ))
            ));
            assert_preserved(
                &sequence,
                &baseline_before_hostile,
                generation_before_hostile,
            );
            cancellation_rejections += 1;
        }

        let should_be_identical = !baseline_reset
            && baseline_before_hostile.is_some()
            && last_variant == Some(variant)
            && last_backend == Some(selected_backend);
        let output = sequence.push_png_bytes_for_generation_controlled(
            generation,
            &frames[variant],
            frame_capabilities,
            frame_options(requested_backend, geometry.0, geometry.1, sequence_limits()),
            &control,
        )?;
        assert_eq!(output.is_identical(), should_be_identical);
        assert!(output.output().len() <= MAX_OUTPUT_BYTES);
        total_output_bytes = total_output_bytes
            .checked_add(output.output().len())
            .ok_or_else(|| io::Error::other("soak output byte count overflowed"))?;

        if output.is_identical() {
            assert!(output.operations().is_empty());
            assert!(output.output().is_empty());
            identical_updates += 1;
        } else if matches!(output.operations().first(), Some(TerminalOp::ClearScreen)) {
            full_redraws += 1;
        } else {
            delta_updates += 1;
        }

        assert_eq!(sequence.latest_generation(), Some(generation));
        let retained = sequence
            .previous()
            .ok_or_else(|| io::Error::other("accepted frame was not retained"))?;
        let current_cell_count = cell_count(geometry.0, geometry.1)?;
        assert_eq!(retained.columns(), geometry.0);
        assert_eq!(retained.rows(), geometry.1);
        assert_eq!(retained.cells().len(), current_cell_count);
        assert!(retained.cells().len() <= MAX_RETAINED_CELLS);
        max_observed_retained_cells = max_observed_retained_cells.max(retained.cells().len());

        last_variant = Some(variant);
        last_backend = Some(selected_backend);
        accepted_generations += 1;
    }

    assert_eq!(accepted_generations, SOAK_GENERATIONS);
    assert!(identical_updates >= SOAK_GENERATIONS / 8);
    assert!(delta_updates > 0);
    assert_eq!(full_redraws, resize_count + 1);
    assert!(capability_transitions >= SOAK_GENERATIONS / CAPABILITY_BLOCK / 2);
    assert!(stale_rejections >= SOAK_GENERATIONS / STALE_PERIOD);
    assert!(quota_rejections >= SOAK_GENERATIONS / QUOTA_PERIOD);
    assert!(cancellation_rejections >= SOAK_GENERATIONS / CANCELLATION_PERIOD);
    assert!(total_output_bytes <= MAX_OUTPUT_BYTES * usize::try_from(SOAK_GENERATIONS)?);
    assert!(max_observed_retained_cells <= MAX_RETAINED_CELLS);
    assert_eq!(sequence.latest_generation(), Some(SOAK_GENERATIONS));
    assert!(started.elapsed() < SOAK_WALL_CLOCK_BUDGET);
    Ok(())
}
