//! Bounded Chrome screenshot decoding for the existing terminal projection path.
//!
//! Chrome DevTools `Page.captureScreenshot` returns a base64-encoded PNG. This module accepts
//! that caller-supplied payload without opening a browser or retaining an external frame store:
//! it validates the bounded PNG, converts it to [`RgbaSurface`], and delegates projection to the
//! established [`project_rgba_with_limits`] implementation.

use std::io::Cursor;

use base64::{DecodeSliceError, Engine as _, engine::general_purpose::STANDARD as BASE64};
use thiserror::Error;

use crate::{
    Backend, Projection, ProjectionLimits, Rgba, RgbaSurface, Surface, SurfaceError,
    TerminalBackendFeedback, TerminalCapabilities, TerminalOp, TerminalTransactionControl,
    TerminalTransactionError, diff, encode_operations_bounded, project_rgba_with_limits,
    transaction::degrade_surface_for_capabilities,
};

const PIXEL_CHECKPOINT_INTERVAL: usize = 4_096;
const PNG_DECODER_OVERHEAD_BYTES: usize = 64 * 1024;
const DEFAULT_TERMINAL_BACKGROUND: Rgba = Rgba {
    red: u8::MAX,
    green: u8::MAX,
    blue: u8::MAX,
    alpha: u8::MAX,
};

/// Decode limits for one externally supplied Chrome screenshot.
///
/// Limits are checked before allocating the base64 destination, before allocating the PNG
/// decoder output, and again after decode. The default fits one 4096x4096 RGBA screenshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExternalFrameDecodeLimits {
    pub max_base64_bytes: usize,
    pub max_png_bytes: usize,
    pub max_width: u32,
    pub max_height: u32,
    pub max_pixels: usize,
    pub max_decoded_bytes: usize,
}

impl ExternalFrameDecodeLimits {
    pub const BROWSER_DEFAULT: Self = Self {
        max_base64_bytes: 24 * 1024 * 1024,
        max_png_bytes: 16 * 1024 * 1024,
        max_width: 4_096,
        max_height: 4_096,
        max_pixels: 16 * 1024 * 1024,
        max_decoded_bytes: 64 * 1024 * 1024,
    };

    fn validate(self) -> Result<(), ExternalFrameError> {
        if self.max_base64_bytes == 0
            || self.max_png_bytes == 0
            || self.max_width == 0
            || self.max_height == 0
            || self.max_pixels == 0
            || self.max_decoded_bytes == 0
        {
            return Err(ExternalFrameError::InvalidDecodeLimits);
        }
        Ok(())
    }
}

impl Default for ExternalFrameDecodeLimits {
    fn default() -> Self {
        Self::BROWSER_DEFAULT
    }
}

/// Errors contained at the externally supplied PNG boundary.
#[derive(Debug, Error)]
pub enum ExternalFrameError {
    #[error(transparent)]
    Transaction(#[from] TerminalTransactionError),
    #[error(transparent)]
    Projection(#[from] SurfaceError),
    #[error("external frame decode limits are invalid")]
    InvalidDecodeLimits,
    #[error("external frame base64 input used {actual} bytes, exceeding its quota of {maximum}")]
    Base64InputLimit { actual: usize, maximum: usize },
    #[error("external frame PNG input used {actual} bytes, exceeding its quota of {maximum}")]
    PngInputLimit { actual: usize, maximum: usize },
    #[error("external frame base64 length cannot be represented safely")]
    Base64LengthOverflow,
    #[error("external frame base64 is invalid: {0}")]
    Base64(#[source] base64::DecodeError),
    #[error("base64 output required more than its validated destination")]
    Base64DestinationInvariant,
    #[error("external frame decode allocation of {requested} items was refused")]
    Allocation { requested: usize },
    #[error("external frame PNG decode failed: {0}")]
    Png(#[from] png::DecodingError),
    #[error(
        "external frame dimensions {width}x{height} exceed the configured {max_width}x{max_height} bound"
    )]
    DimensionLimit {
        width: u32,
        height: u32,
        max_width: u32,
        max_height: u32,
    },
    #[error("external frame pixel count {actual} exceeds its quota of {maximum}")]
    PixelLimit { actual: usize, maximum: usize },
    #[error("external frame decoded byte count {actual} exceeds its quota of {maximum}")]
    DecodedByteLimit { actual: usize, maximum: usize },
    #[error("external frame decoded byte count is not representable")]
    DecodedByteOverflow,
    #[error("external frame PNG animation is unsupported")]
    AnimatedPngUnsupported,
    #[error(
        "external frame PNG output dimensions changed from {expected_width}x{expected_height} to {actual_width}x{actual_height}"
    )]
    DecodedDimensionMismatch {
        expected_width: u32,
        expected_height: u32,
        actual_width: u32,
        actual_height: u32,
    },
    #[error("external frame PNG output format {color_type:?} at {bit_depth:?} is unsupported")]
    UnsupportedColorFormat {
        color_type: png::ColorType,
        bit_depth: png::BitDepth,
    },
    #[error("external frame decoded {actual} bytes, but {expected} bytes were required")]
    DecodedByteCountMismatch { expected: usize, actual: usize },
    #[error("external frame sequence limits are invalid")]
    InvalidSequenceLimits,
    #[error("external frame sequence requires a non-zero terminal cell geometry")]
    InvalidSequenceGeometry,
    #[error("external frame generation must be non-zero")]
    InvalidGeneration,
    #[error(
        "external frame generation {received} is stale; latest accepted generation is {latest}"
    )]
    StaleGeneration { received: u64, latest: u64 },
    #[error(
        "external frame requested {requested:?}, but terminal capabilities selected non-cell backend {selected:?}"
    )]
    UnsupportedCellBackend {
        requested: Backend,
        selected: Backend,
    },
    #[error("external frame terminal background must be opaque")]
    TransparentTerminalBackground,
    #[error(
        "external frame sequence produced {actual} operations, exceeding its quota of {maximum}"
    )]
    SequenceOperationLimit { actual: usize, maximum: usize },
}

/// Bounded terminal-diff allowance for one accepted external frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExternalFrameSequenceLimits {
    pub max_operations: usize,
    pub max_output_bytes: usize,
}

impl ExternalFrameSequenceLimits {
    pub const BROWSER_DEFAULT: Self = Self {
        max_operations: 3_000_000,
        max_output_bytes: 64 * 1024 * 1024,
    };

    fn validate(self) -> Result<(), ExternalFrameError> {
        if self.max_operations == 0 || self.max_output_bytes == 0 {
            return Err(ExternalFrameError::InvalidSequenceLimits);
        }
        Ok(())
    }
}

impl Default for ExternalFrameSequenceLimits {
    fn default() -> Self {
        Self::BROWSER_DEFAULT
    }
}

/// Complete bounded projection contract for one Chrome screenshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExternalFrameOptions {
    pub backend: Backend,
    pub columns: u16,
    pub rows: u16,
    pub decode_limits: ExternalFrameDecodeLimits,
    pub projection_limits: ProjectionLimits,
    pub sequence_limits: ExternalFrameSequenceLimits,
}

impl ExternalFrameOptions {
    pub const fn new(
        backend: Backend,
        columns: u16,
        rows: u16,
        decode_limits: ExternalFrameDecodeLimits,
        projection_limits: ProjectionLimits,
        sequence_limits: ExternalFrameSequenceLimits,
    ) -> Self {
        Self {
            backend,
            columns,
            rows,
            decode_limits,
            projection_limits,
            sequence_limits,
        }
    }
}

/// One bounded terminal update emitted by [`ExternalFrameSequence`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalFrameSequenceOutput {
    operations: Vec<TerminalOp>,
    output: Vec<u8>,
}

impl ExternalFrameSequenceOutput {
    pub fn operations(&self) -> &[TerminalOp] {
        &self.operations
    }

    pub fn output(&self) -> &[u8] {
        &self.output
    }

    pub fn is_identical(&self) -> bool {
        self.operations.is_empty()
    }

    pub fn into_parts(self) -> (Vec<TerminalOp>, Vec<u8>) {
        (self.operations, self.output)
    }
}

/// Retains only the prior cell projection needed to diff the next external screenshot.
///
/// The adapter owns no terminal writer. It delegates PNG decode, projection, cell diffing, and
/// ANSI encoding to the existing terminal primitives, commits a new baseline only after bounded
/// encoding succeeds, and emits no bytes for an identical next projection. Generation-aware
/// entry points also reject stale candidates and composite screenshot alpha over one caller-owned
/// opaque terminal background before projection.
#[derive(Debug)]
pub struct ExternalFrameSequence {
    previous: Option<Surface>,
    latest_generation: Option<u64>,
    terminal_background: Rgba,
}

impl Default for ExternalFrameSequence {
    fn default() -> Self {
        Self::new()
    }
}

impl ExternalFrameSequence {
    pub const fn new() -> Self {
        Self {
            previous: None,
            latest_generation: None,
            terminal_background: DEFAULT_TERMINAL_BACKGROUND,
        }
    }

    pub fn previous(&self) -> Option<&Surface> {
        self.previous.as_ref()
    }

    /// Returns the latest successfully emitted generation, if a frame has been accepted.
    pub const fn latest_generation(&self) -> Option<u64> {
        self.latest_generation
    }

    /// Returns the opaque color used below transparent screenshot pixels.
    pub const fn terminal_background(&self) -> Rgba {
        self.terminal_background
    }

    /// Changes the caller-owned terminal background and invalidates any prior cell baseline.
    ///
    /// The background is required to be opaque because a terminal cell background has no useful
    /// second compositing target. The returned flag reports whether the configured color changed.
    /// A caller must still submit a strictly newer generation after an invalidation.
    pub fn set_terminal_background_controlled(
        &mut self,
        terminal_background: Rgba,
        control: &TerminalTransactionControl<'_>,
    ) -> Result<bool, ExternalFrameError> {
        control.checkpoint()?;
        validate_terminal_background(terminal_background)?;
        let changed = self.terminal_background != terminal_background;
        if !changed {
            return Ok(false);
        }
        control.checkpoint()?;
        self.terminal_background = terminal_background;
        self.previous = None;
        Ok(true)
    }

    /// Selects a capability-attested terminal cell backend without opening or probing a terminal.
    ///
    /// Graphics protocols are deliberately disabled at this adapter boundary. A safe fallback is
    /// accepted only when it remains one of the established cell projections.
    pub fn select_cell_backend(
        capabilities: &TerminalCapabilities,
        requested: Backend,
    ) -> Result<Backend, ExternalFrameError> {
        let selected =
            capabilities.select_safe_backend(requested, TerminalBackendFeedback::Initial);
        if matches!(
            selected,
            Backend::Cells | Backend::Halfblock | Backend::Quadrant | Backend::Braille
        ) {
            Ok(selected)
        } else {
            Err(ExternalFrameError::UnsupportedCellBackend {
                requested,
                selected,
            })
        }
    }

    /// Forgets the prior terminal baseline. The next accepted cell frame performs a full redraw.
    pub fn reset_controlled(
        &mut self,
        control: &TerminalTransactionControl<'_>,
    ) -> Result<bool, ExternalFrameError> {
        control.checkpoint()?;
        Ok(self.previous.take().is_some())
    }

    /// Invalidates the prior baseline when a caller-owned terminal geometry changes.
    pub fn resize_controlled(
        &mut self,
        columns: u16,
        rows: u16,
        control: &TerminalTransactionControl<'_>,
    ) -> Result<bool, ExternalFrameError> {
        control.checkpoint()?;
        if columns == 0 || rows == 0 {
            return Err(ExternalFrameError::InvalidSequenceGeometry);
        }
        let changed = self
            .previous
            .as_ref()
            .is_some_and(|previous| previous.columns() != columns || previous.rows() != rows);
        if changed {
            self.previous = None;
        }
        Ok(changed)
    }

    /// Decodes one raw Chrome screenshot, projects it with the established RGBA path, and emits
    /// one bounded cell diff against the retained prior projection.
    pub fn push_png_bytes_controlled(
        &mut self,
        png_bytes: &[u8],
        options: ExternalFrameOptions,
        control: &TerminalTransactionControl<'_>,
    ) -> Result<ExternalFrameSequenceOutput, ExternalFrameError> {
        control.checkpoint()?;
        options.sequence_limits.validate()?;
        let projection = project_external_frame_png_bytes_controlled(png_bytes, options, control)?;
        self.push_projection_controlled(projection, options.sequence_limits, control)
    }

    /// Decodes one base64 Chrome screenshot, projects it with the established RGBA path, and
    /// emits one bounded cell diff against the retained prior projection.
    pub fn push_png_base64_controlled(
        &mut self,
        png_base64: &str,
        options: ExternalFrameOptions,
        control: &TerminalTransactionControl<'_>,
    ) -> Result<ExternalFrameSequenceOutput, ExternalFrameError> {
        control.checkpoint()?;
        options.sequence_limits.validate()?;
        let projection =
            project_external_frame_png_base64_controlled(png_base64, options, control)?;
        self.push_projection_controlled(projection, options.sequence_limits, control)
    }

    /// Decodes one raw screenshot candidate for a strictly newer generation and emits a bounded
    /// cell diff. Transparent screenshot pixels are composited over the configured terminal
    /// background before the established RGBA projection path runs.
    pub fn push_png_bytes_for_generation_controlled(
        &mut self,
        generation: u64,
        png_bytes: &[u8],
        capabilities: &TerminalCapabilities,
        options: ExternalFrameOptions,
        control: &TerminalTransactionControl<'_>,
    ) -> Result<ExternalFrameSequenceOutput, ExternalFrameError> {
        control.checkpoint()?;
        self.validate_generation(generation)?;
        let backend = Self::select_cell_backend(capabilities, options.backend)?;
        options.sequence_limits.validate()?;
        let surface =
            decode_external_frame_png_bytes_controlled(png_bytes, options.decode_limits, control)?;
        let projection = project_external_frame_rgba_with_terminal_background_controlled(
            surface,
            backend,
            options.columns,
            options.rows,
            options.projection_limits,
            self.terminal_background,
            control,
        )?;
        let projection =
            degrade_projection_for_capabilities_controlled(projection, *capabilities, control)?;
        self.push_generation_projection_controlled(
            generation,
            projection,
            options.sequence_limits,
            control,
        )
    }

    /// Decodes one base64 screenshot candidate for a strictly newer generation and emits a
    /// bounded cell diff. This preserves the same generation and alpha-compositing contract as
    /// [`Self::push_png_bytes_for_generation_controlled`].
    pub fn push_png_base64_for_generation_controlled(
        &mut self,
        generation: u64,
        png_base64: &str,
        capabilities: &TerminalCapabilities,
        options: ExternalFrameOptions,
        control: &TerminalTransactionControl<'_>,
    ) -> Result<ExternalFrameSequenceOutput, ExternalFrameError> {
        control.checkpoint()?;
        self.validate_generation(generation)?;
        let backend = Self::select_cell_backend(capabilities, options.backend)?;
        options.sequence_limits.validate()?;
        let surface = decode_external_frame_png_base64_controlled(
            png_base64,
            options.decode_limits,
            control,
        )?;
        let projection = project_external_frame_rgba_with_terminal_background_controlled(
            surface,
            backend,
            options.columns,
            options.rows,
            options.projection_limits,
            self.terminal_background,
            control,
        )?;
        let projection =
            degrade_projection_for_capabilities_controlled(projection, *capabilities, control)?;
        self.push_generation_projection_controlled(
            generation,
            projection,
            options.sequence_limits,
            control,
        )
    }

    fn validate_generation(&self, generation: u64) -> Result<(), ExternalFrameError> {
        if generation == 0 {
            return Err(ExternalFrameError::InvalidGeneration);
        }
        if let Some(latest) = self.latest_generation
            && generation <= latest
        {
            return Err(ExternalFrameError::StaleGeneration {
                received: generation,
                latest,
            });
        }
        Ok(())
    }

    fn push_generation_projection_controlled(
        &mut self,
        generation: u64,
        projection: Projection,
        sequence_limits: ExternalFrameSequenceLimits,
        control: &TerminalTransactionControl<'_>,
    ) -> Result<ExternalFrameSequenceOutput, ExternalFrameError> {
        let output = self.push_projection_controlled(projection, sequence_limits, control)?;
        self.latest_generation = Some(generation);
        Ok(output)
    }

    fn push_projection_controlled(
        &mut self,
        projection: Projection,
        sequence_limits: ExternalFrameSequenceLimits,
        control: &TerminalTransactionControl<'_>,
    ) -> Result<ExternalFrameSequenceOutput, ExternalFrameError> {
        control.checkpoint()?;
        sequence_limits.validate()?;
        let Projection::Cells(next) = projection;
        if self
            .previous
            .as_ref()
            .is_some_and(|previous| previous == &next)
        {
            control.checkpoint()?;
            return Ok(ExternalFrameSequenceOutput {
                operations: Vec::new(),
                output: Vec::new(),
            });
        }

        let operations = match self.previous.as_ref() {
            Some(previous) => diff(previous, &next)?,
            None => full_redraw_operations(&next)?,
        };
        if operations.len() > sequence_limits.max_operations {
            return Err(ExternalFrameError::SequenceOperationLimit {
                actual: operations.len(),
                maximum: sequence_limits.max_operations,
            });
        }
        control.checkpoint()?;
        let output = encode_operations_bounded(&operations, sequence_limits.max_output_bytes)?;
        control.checkpoint()?;
        self.previous = Some(next);
        Ok(ExternalFrameSequenceOutput { operations, output })
    }
}

fn degrade_projection_for_capabilities_controlled(
    projection: Projection,
    capabilities: TerminalCapabilities,
    control: &TerminalTransactionControl<'_>,
) -> Result<Projection, ExternalFrameError> {
    let Projection::Cells(surface) = projection;
    Ok(Projection::Cells(degrade_surface_for_capabilities(
        &surface,
        capabilities,
        control,
    )?))
}

fn full_redraw_operations(next: &Surface) -> Result<Vec<TerminalOp>, ExternalFrameError> {
    // A dimension-mismatched, valid empty surface delegates the initial clear/redraw policy to
    // the existing public `diff` primitive instead of reproducing terminal operations here.
    let empty = Surface::new(0, 0, 0)?;
    Ok(diff(&empty, next)?)
}

/// Decodes one raw PNG returned by Chrome `Page.captureScreenshot` into an RGBA surface.
///
/// CRC and Adler-32 verification are explicitly enabled. The decoder is driven through
/// [`png::Reader::finish`] before this function returns, so IEND, trailing chunks, and their CRC
/// failures are not accepted after the first image frame.
pub fn decode_external_frame_png_bytes_controlled(
    png_bytes: &[u8],
    decode_limits: ExternalFrameDecodeLimits,
    control: &TerminalTransactionControl<'_>,
) -> Result<RgbaSurface, ExternalFrameError> {
    control.checkpoint()?;
    decode_limits.validate()?;
    if png_bytes.len() > decode_limits.max_png_bytes {
        return Err(ExternalFrameError::PngInputLimit {
            actual: png_bytes.len(),
            maximum: decode_limits.max_png_bytes,
        });
    }
    control.checkpoint()?;

    let mut decode_options = png::DecodeOptions::default();
    decode_options.set_ignore_adler32(false);
    decode_options.set_ignore_crc(false);
    decode_options.set_skip_ancillary_crc_failures(false);
    decode_options.set_ignore_text_chunk(true);
    decode_options.set_ignore_iccp_chunk(true);
    let mut decoder = png::Decoder::new_with_options(Cursor::new(png_bytes), decode_options);
    decoder.set_limits(png::Limits {
        bytes: decoder_allocation_limit(decode_limits)?,
    });
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    // The configured reader retains strict checksum verification while consuming the complete
    // frame and IEND. Text and ICC payloads are skipped only to avoid retaining unused metadata.
    let mut reader = decoder.read_info()?;
    control.checkpoint()?;

    let (width, height, animated) = {
        let info = reader.info();
        (info.width, info.height, info.animation_control.is_some())
    };
    validate_dimensions(width, height, decode_limits)?;
    if animated {
        return Err(ExternalFrameError::AnimatedPngUnsupported);
    }

    let decoded_capacity = reader
        .output_buffer_size()
        .ok_or(ExternalFrameError::DecodedByteOverflow)?;
    if decoded_capacity > decode_limits.max_decoded_bytes {
        return Err(ExternalFrameError::DecodedByteLimit {
            actual: decoded_capacity,
            maximum: decode_limits.max_decoded_bytes,
        });
    }
    control.checkpoint()?;
    let mut decoded = zeroed_bytes(decoded_capacity)?;
    let output = reader.next_frame(&mut decoded)?;
    control.checkpoint()?;

    if output.width != width || output.height != height {
        return Err(ExternalFrameError::DecodedDimensionMismatch {
            expected_width: width,
            expected_height: height,
            actual_width: output.width,
            actual_height: output.height,
        });
    }
    let channels = output_channels(output.color_type, output.bit_depth)?;
    let pixel_count = checked_pixel_count(width, height)?;
    let expected_bytes = pixel_count
        .checked_mul(channels)
        .ok_or(ExternalFrameError::DecodedByteOverflow)?;
    let actual_bytes = output.buffer_size();
    if actual_bytes != expected_bytes || actual_bytes != decoded_capacity {
        return Err(ExternalFrameError::DecodedByteCountMismatch {
            expected: expected_bytes,
            actual: actual_bytes,
        });
    }
    decoded.truncate(actual_bytes);

    // `next_frame` validates image data; `finish` additionally consumes IEND and all remaining
    // chunks so a corrupt trailing CRC cannot become an accepted screenshot.
    reader.finish()?;
    control.checkpoint()?;
    rgba_surface_from_decoded(&decoded, width, height, channels, pixel_count, control)
}

/// Decodes the base64 payload returned by Chrome `Page.captureScreenshot` into an RGBA surface.
pub fn decode_external_frame_png_base64_controlled(
    png_base64: &str,
    decode_limits: ExternalFrameDecodeLimits,
    control: &TerminalTransactionControl<'_>,
) -> Result<RgbaSurface, ExternalFrameError> {
    control.checkpoint()?;
    decode_limits.validate()?;
    if png_base64.len() > decode_limits.max_base64_bytes {
        return Err(ExternalFrameError::Base64InputLimit {
            actual: png_base64.len(),
            maximum: decode_limits.max_base64_bytes,
        });
    }
    let capacity = base64_capacity(png_base64.len())?;
    if capacity > decode_limits.max_png_bytes {
        return Err(ExternalFrameError::PngInputLimit {
            actual: capacity,
            maximum: decode_limits.max_png_bytes,
        });
    }
    control.checkpoint()?;
    let mut png_bytes = zeroed_bytes(capacity)?;
    let written = match BASE64.decode_slice(png_base64.as_bytes(), &mut png_bytes) {
        Ok(written) => written,
        Err(DecodeSliceError::DecodeError(error)) => return Err(ExternalFrameError::Base64(error)),
        Err(DecodeSliceError::OutputSliceTooSmall) => {
            return Err(ExternalFrameError::Base64DestinationInvariant);
        }
    };
    png_bytes.truncate(written);
    if png_bytes.len() > decode_limits.max_png_bytes {
        return Err(ExternalFrameError::PngInputLimit {
            actual: png_bytes.len(),
            maximum: decode_limits.max_png_bytes,
        });
    }
    control.checkpoint()?;
    decode_external_frame_png_bytes_controlled(&png_bytes, decode_limits, control)
}

/// Decodes raw Chrome screenshot bytes and delegates terminal rendering to the existing bounded
/// RGBA projection implementation.
pub fn project_external_frame_png_bytes_controlled(
    png_bytes: &[u8],
    options: ExternalFrameOptions,
    control: &TerminalTransactionControl<'_>,
) -> Result<Projection, ExternalFrameError> {
    let surface =
        decode_external_frame_png_bytes_controlled(png_bytes, options.decode_limits, control)?;
    control.checkpoint()?;
    Ok(project_rgba_with_limits(
        &surface,
        options.backend,
        options.columns,
        options.rows,
        options.projection_limits,
    )?)
}

/// Decodes a base64 Chrome screenshot and delegates terminal rendering to the existing bounded
/// RGBA projection implementation.
pub fn project_external_frame_png_base64_controlled(
    png_base64: &str,
    options: ExternalFrameOptions,
    control: &TerminalTransactionControl<'_>,
) -> Result<Projection, ExternalFrameError> {
    let surface =
        decode_external_frame_png_base64_controlled(png_base64, options.decode_limits, control)?;
    control.checkpoint()?;
    Ok(project_rgba_with_limits(
        &surface,
        options.backend,
        options.columns,
        options.rows,
        options.projection_limits,
    )?)
}

fn project_external_frame_rgba_with_terminal_background_controlled(
    mut surface: RgbaSurface,
    backend: Backend,
    columns: u16,
    rows: u16,
    projection_limits: ProjectionLimits,
    terminal_background: Rgba,
    control: &TerminalTransactionControl<'_>,
) -> Result<Projection, ExternalFrameError> {
    composite_external_frame_alpha(&mut surface, terminal_background, control)?;
    control.checkpoint()?;
    Ok(project_rgba_with_limits(
        &surface,
        backend,
        columns,
        rows,
        projection_limits,
    )?)
}

fn validate_terminal_background(terminal_background: Rgba) -> Result<(), ExternalFrameError> {
    if terminal_background.alpha != u8::MAX {
        return Err(ExternalFrameError::TransparentTerminalBackground);
    }
    Ok(())
}

fn composite_external_frame_alpha(
    surface: &mut RgbaSurface,
    terminal_background: Rgba,
    control: &TerminalTransactionControl<'_>,
) -> Result<(), ExternalFrameError> {
    control.checkpoint()?;
    validate_terminal_background(terminal_background)?;
    for (index, pixel) in surface.pixels.iter_mut().enumerate() {
        if index.is_multiple_of(PIXEL_CHECKPOINT_INTERVAL) {
            control.checkpoint()?;
        }
        let alpha = pixel.alpha;
        pixel.red = composite_channel(pixel.red, terminal_background.red, alpha);
        pixel.green = composite_channel(pixel.green, terminal_background.green, alpha);
        pixel.blue = composite_channel(pixel.blue, terminal_background.blue, alpha);
        pixel.alpha = u8::MAX;
    }
    control.checkpoint()?;
    Ok(())
}

fn composite_channel(source: u8, terminal_background: u8, alpha: u8) -> u8 {
    let alpha = u16::from(alpha);
    let inverse = 255 - alpha;
    ((u16::from(source) * alpha + u16::from(terminal_background) * inverse) / 255) as u8
}

fn validate_dimensions(
    width: u32,
    height: u32,
    limits: ExternalFrameDecodeLimits,
) -> Result<(), ExternalFrameError> {
    if width == 0 || height == 0 || width > limits.max_width || height > limits.max_height {
        return Err(ExternalFrameError::DimensionLimit {
            width,
            height,
            max_width: limits.max_width,
            max_height: limits.max_height,
        });
    }
    let pixels = checked_pixel_count(width, height)?;
    if pixels > limits.max_pixels {
        return Err(ExternalFrameError::PixelLimit {
            actual: pixels,
            maximum: limits.max_pixels,
        });
    }
    Ok(())
}

fn checked_pixel_count(width: u32, height: u32) -> Result<usize, ExternalFrameError> {
    usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .ok_or(ExternalFrameError::DecodedByteOverflow)
}

fn output_channels(
    color_type: png::ColorType,
    bit_depth: png::BitDepth,
) -> Result<usize, ExternalFrameError> {
    if bit_depth != png::BitDepth::Eight {
        return Err(ExternalFrameError::UnsupportedColorFormat {
            color_type,
            bit_depth,
        });
    }
    match color_type {
        png::ColorType::Grayscale => Ok(1),
        png::ColorType::GrayscaleAlpha => Ok(2),
        png::ColorType::Rgb => Ok(3),
        png::ColorType::Rgba => Ok(4),
        png::ColorType::Indexed => Err(ExternalFrameError::UnsupportedColorFormat {
            color_type,
            bit_depth,
        }),
    }
}

fn base64_capacity(encoded_len: usize) -> Result<usize, ExternalFrameError> {
    encoded_len
        .checked_add(3)
        .map(|value| value / 4)
        .and_then(|groups| groups.checked_mul(3))
        .ok_or(ExternalFrameError::Base64LengthOverflow)
}

fn decoder_allocation_limit(
    limits: ExternalFrameDecodeLimits,
) -> Result<usize, ExternalFrameError> {
    limits
        .max_decoded_bytes
        .checked_mul(2)
        .and_then(|value| value.checked_add(PNG_DECODER_OVERHEAD_BYTES))
        .ok_or(ExternalFrameError::DecodedByteOverflow)
}

fn zeroed_bytes(length: usize) -> Result<Vec<u8>, ExternalFrameError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| ExternalFrameError::Allocation { requested: length })?;
    bytes.resize(length, 0);
    Ok(bytes)
}

fn rgba_surface_from_decoded(
    decoded: &[u8],
    width: u32,
    height: u32,
    channels: usize,
    pixel_count: usize,
    control: &TerminalTransactionControl<'_>,
) -> Result<RgbaSurface, ExternalFrameError> {
    if channels == 0 {
        return Err(ExternalFrameError::DecodedByteCountMismatch {
            expected: 0,
            actual: decoded.len(),
        });
    }
    let mut pixels = Vec::new();
    pixels
        .try_reserve_exact(pixel_count)
        .map_err(|_| ExternalFrameError::Allocation {
            requested: pixel_count,
        })?;
    for (index, pixel) in decoded.chunks_exact(channels).enumerate() {
        if index.is_multiple_of(PIXEL_CHECKPOINT_INTERVAL) {
            control.checkpoint()?;
        }
        let rgba = match pixel {
            [gray] => Rgba {
                red: *gray,
                green: *gray,
                blue: *gray,
                alpha: u8::MAX,
            },
            [gray, alpha] => Rgba {
                red: *gray,
                green: *gray,
                blue: *gray,
                alpha: *alpha,
            },
            [red, green, blue] => Rgba {
                red: *red,
                green: *green,
                blue: *blue,
                alpha: u8::MAX,
            },
            [red, green, blue, alpha] => Rgba {
                red: *red,
                green: *green,
                blue: *blue,
                alpha: *alpha,
            },
            _ => {
                return Err(ExternalFrameError::DecodedByteCountMismatch {
                    expected: pixel_count.saturating_mul(channels),
                    actual: decoded.len(),
                });
            }
        };
        pixels.push(rgba);
    }
    control.checkpoint()?;
    RgbaSurface::new(width, height, pixels).map_err(ExternalFrameError::from)
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::time::SystemTime;

    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
    use tg_core::{Cancellation, VirtualClock};

    use super::*;

    fn limits() -> ExternalFrameDecodeLimits {
        ExternalFrameDecodeLimits {
            max_base64_bytes: 4_096,
            max_png_bytes: 2_048,
            max_width: 16,
            max_height: 16,
            max_pixels: 256,
            max_decoded_bytes: 1_024,
        }
    }

    fn sequence_limits() -> ExternalFrameSequenceLimits {
        ExternalFrameSequenceLimits {
            max_operations: 64,
            max_output_bytes: 4_096,
        }
    }

    fn options(backend: Backend, columns: u16, rows: u16) -> ExternalFrameOptions {
        options_with_sequence(backend, columns, rows, sequence_limits())
    }

    fn options_with_sequence(
        backend: Backend,
        columns: u16,
        rows: u16,
        sequence_limits: ExternalFrameSequenceLimits,
    ) -> ExternalFrameOptions {
        ExternalFrameOptions::new(
            backend,
            columns,
            rows,
            limits(),
            ProjectionLimits::default(),
            sequence_limits,
        )
    }

    fn control<'a>(
        cancellation: &'a Cancellation,
        clock: &'a VirtualClock,
    ) -> TerminalTransactionControl<'a> {
        TerminalTransactionControl::new(cancellation, clock, None)
    }

    fn rgba_png(width: u32, height: u32, pixels: &[u8]) -> Result<Vec<u8>, Box<dyn Error>> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header()?;
            writer.write_image_data(pixels)?;
        }
        Ok(bytes)
    }

    #[test]
    fn external_frame_decodes_base64_and_projects_existing_cells() -> Result<(), Box<dyn Error>> {
        let cancellation = Cancellation::new();
        let clock = VirtualClock::new(SystemTime::UNIX_EPOCH);
        let control = control(&cancellation, &clock);
        let png = rgba_png(2, 1, &[255, 0, 0, 255, 0, 0, 255, 255])?;
        let encoded = BASE64.encode(&png);

        let rgba = decode_external_frame_png_base64_controlled(&encoded, limits(), &control)?;
        assert_eq!(rgba.width(), 2);
        assert_eq!(rgba.height(), 1);
        assert_eq!(
            rgba.pixels(),
            &[
                Rgba {
                    red: 255,
                    green: 0,
                    blue: 0,
                    alpha: 255,
                },
                Rgba {
                    red: 0,
                    green: 0,
                    blue: 255,
                    alpha: 255,
                },
            ]
        );

        let projection = project_external_frame_png_base64_controlled(
            &encoded,
            options(Backend::Cells, 2, 1),
            &control,
        )?;
        let Projection::Cells(cells) = projection;
        assert_eq!(cells.columns(), 2);
        assert_eq!(cells.rows(), 1);
        assert_eq!(
            cells.get(0, 0).map(|cell| cell.background),
            Some(crate::Color::Rgb(255, 0, 0))
        );
        assert_eq!(
            cells.get(1, 0).map(|cell| cell.background),
            Some(crate::Color::Rgb(0, 0, 255))
        );
        Ok(())
    }

    #[test]
    fn external_frame_generation_quantizes_ansi256_surface_and_output() -> Result<(), Box<dyn Error>>
    {
        let cancellation = Cancellation::new();
        let clock = VirtualClock::new(SystemTime::UNIX_EPOCH);
        let control = control(&cancellation, &clock);
        let capabilities = TerminalCapabilities {
            color: crate::ColorLevel::Ansi256,
            dumb: false,
        };
        let png = rgba_png(1, 1, &[18, 52, 86, 255])?;
        let mut sequence = ExternalFrameSequence::new();

        let output = sequence.push_png_bytes_for_generation_controlled(
            1,
            &png,
            &capabilities,
            options(Backend::Cells, 1, 1),
            &control,
        )?;

        assert_eq!(
            sequence
                .previous()
                .and_then(|surface| surface.get(0, 0))
                .map(|cell| cell.background),
            Some(crate::Color::Indexed(24))
        );
        let ansi = std::str::from_utf8(output.output())?;
        assert!(ansi.contains("\x1b[0;39;48;5;24m"));
        assert!(!ansi.contains("48;2;18;52;86"));
        Ok(())
    }

    #[test]
    fn external_frame_rejects_invalid_base64_png_and_corrupt_crc() -> Result<(), Box<dyn Error>> {
        let cancellation = Cancellation::new();
        let clock = VirtualClock::new(SystemTime::UNIX_EPOCH);
        let control = control(&cancellation, &clock);
        assert!(matches!(
            decode_external_frame_png_base64_controlled("not valid base64!", limits(), &control),
            Err(ExternalFrameError::Base64(_))
        ));
        assert!(matches!(
            decode_external_frame_png_bytes_controlled(b"not a png", limits(), &control),
            Err(ExternalFrameError::Png(_))
        ));

        let mut corrupt = rgba_png(1, 1, &[1, 2, 3, 4])?;
        let Some(last) = corrupt.last_mut() else {
            return Err("encoded PNG was unexpectedly empty".into());
        };
        *last ^= 0xff;
        assert!(matches!(
            decode_external_frame_png_bytes_controlled(&corrupt, limits(), &control),
            Err(ExternalFrameError::Png(_))
        ));
        Ok(())
    }

    #[test]
    fn external_frame_enforces_input_dimension_and_decode_quotas() -> Result<(), Box<dyn Error>> {
        let cancellation = Cancellation::new();
        let clock = VirtualClock::new(SystemTime::UNIX_EPOCH);
        let control = control(&cancellation, &clock);
        let png = rgba_png(2, 1, &[1, 2, 3, 4, 5, 6, 7, 8])?;
        let max_png_bytes = png
            .len()
            .checked_sub(1)
            .ok_or("encoded PNG unexpectedly had zero length")?;
        let png_limited = ExternalFrameDecodeLimits {
            max_png_bytes,
            ..limits()
        };
        assert!(matches!(
            decode_external_frame_png_bytes_controlled(&png, png_limited, &control),
            Err(ExternalFrameError::PngInputLimit { .. })
        ));
        let dimension_limited = ExternalFrameDecodeLimits {
            max_width: 1,
            ..limits()
        };
        assert!(matches!(
            decode_external_frame_png_bytes_controlled(&png, dimension_limited, &control),
            Err(ExternalFrameError::DimensionLimit { .. })
        ));
        let decoded_limited = ExternalFrameDecodeLimits {
            max_decoded_bytes: 7,
            ..limits()
        };
        assert!(matches!(
            decode_external_frame_png_bytes_controlled(&png, decoded_limited, &control),
            Err(ExternalFrameError::DecodedByteLimit { .. })
        ));
        Ok(())
    }

    #[test]
    fn external_frame_sequence_diffs_cells_and_skips_identical_projection()
    -> Result<(), Box<dyn Error>> {
        let cancellation = Cancellation::new();
        let clock = VirtualClock::new(SystemTime::UNIX_EPOCH);
        let control = control(&cancellation, &clock);
        let first = rgba_png(2, 1, &[255, 0, 0, 255, 0, 0, 255, 255])?;
        let second = rgba_png(2, 1, &[255, 0, 0, 255, 0, 255, 0, 255])?;
        let first_base64 = BASE64.encode(&first);
        let mut sequence = ExternalFrameSequence::new();

        let initial = sequence.push_png_base64_controlled(
            &first_base64,
            options(Backend::Cells, 2, 1),
            &control,
        )?;
        assert!(!initial.is_identical());
        assert!(matches!(
            initial.operations().first(),
            Some(TerminalOp::ClearScreen)
        ));
        assert!(!initial.output().is_empty());
        assert_eq!(sequence.previous().map(Surface::columns), Some(2));

        let changed =
            sequence.push_png_bytes_controlled(&second, options(Backend::Cells, 2, 1), &control)?;
        assert!(!changed.is_identical());
        assert!(
            !changed
                .operations()
                .iter()
                .any(|operation| matches!(operation, TerminalOp::ClearScreen))
        );
        assert!(!changed.output().is_empty());

        let identical =
            sequence.push_png_bytes_controlled(&second, options(Backend::Cells, 2, 1), &control)?;
        assert!(identical.is_identical());
        assert!(identical.operations().is_empty());
        assert!(identical.output().is_empty());
        Ok(())
    }

    #[test]
    fn external_frame_sequence_reset_resize_and_cancellation_preserve_boundaries()
    -> Result<(), Box<dyn Error>> {
        let cancellation = Cancellation::new();
        let clock = VirtualClock::new(SystemTime::UNIX_EPOCH);
        let control = control(&cancellation, &clock);
        let initial_png = rgba_png(2, 1, &[1, 2, 3, 255, 4, 5, 6, 255])?;
        let resized_png = rgba_png(1, 1, &[7, 8, 9, 255])?;
        let mut sequence = ExternalFrameSequence::new();
        sequence.push_png_bytes_controlled(
            &initial_png,
            options(Backend::Cells, 2, 1),
            &control,
        )?;

        assert!(sequence.resize_controlled(1, 1, &control)?);
        assert!(sequence.previous().is_none());
        let resized = sequence.push_png_bytes_controlled(
            &resized_png,
            options(Backend::Cells, 1, 1),
            &control,
        )?;
        assert!(matches!(
            resized.operations().first(),
            Some(TerminalOp::ClearScreen)
        ));
        assert!(sequence.reset_controlled(&control)?);
        assert!(sequence.previous().is_none());

        let cancelled = Cancellation::new();
        cancelled.cancel();
        let cancelled_control = TerminalTransactionControl::new(&cancelled, &clock, None);
        assert!(matches!(
            sequence.resize_controlled(1, 1, &cancelled_control),
            Err(ExternalFrameError::Transaction(
                TerminalTransactionError::Cancelled
            ))
        ));
        Ok(())
    }

    #[test]
    fn external_frame_sequence_quota_does_not_advance_the_prior_projection()
    -> Result<(), Box<dyn Error>> {
        let cancellation = Cancellation::new();
        let clock = VirtualClock::new(SystemTime::UNIX_EPOCH);
        let control = control(&cancellation, &clock);
        let png = rgba_png(1, 1, &[11, 12, 13, 255])?;
        let mut sequence = ExternalFrameSequence::new();
        let constrained = ExternalFrameSequenceLimits {
            max_operations: 1,
            ..sequence_limits()
        };
        assert!(matches!(
            sequence.push_png_bytes_controlled(
                &png,
                options_with_sequence(Backend::Cells, 1, 1, constrained),
                &control,
            ),
            Err(ExternalFrameError::SequenceOperationLimit { .. })
        ));
        assert!(sequence.previous().is_none());

        let accepted =
            sequence.push_png_bytes_controlled(&png, options(Backend::Cells, 1, 1), &control)?;
        assert!(matches!(
            accepted.operations().first(),
            Some(TerminalOp::ClearScreen)
        ));
        assert!(sequence.previous().is_some());
        Ok(())
    }

    #[test]
    fn external_frame_checks_cancellation_before_any_decode_work() -> Result<(), Box<dyn Error>> {
        let cancellation = Cancellation::new();
        cancellation.cancel();
        let clock = VirtualClock::new(SystemTime::UNIX_EPOCH);
        let control = control(&cancellation, &clock);
        assert!(matches!(
            decode_external_frame_png_bytes_controlled(b"", limits(), &control),
            Err(ExternalFrameError::Transaction(
                TerminalTransactionError::Cancelled
            ))
        ));
        Ok(())
    }
}
