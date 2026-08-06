//! Pure terminal capability, input, surface, projection, and escape encoding logic.

use std::collections::HashMap;

use bitflags::bitflags;
use serde::{Deserialize, Serialize};
use tg_core::{Cancellation, LinkId, NodeId};
use thiserror::Error;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

mod external_frame;
mod transaction;

pub use external_frame::{
    ExternalFrameDecodeLimits, ExternalFrameError, ExternalFrameOptions, ExternalFrameSequence,
    ExternalFrameSequenceLimits, ExternalFrameSequenceOutput,
    decode_external_frame_png_base64_controlled, decode_external_frame_png_bytes_controlled,
    project_external_frame_png_base64_controlled, project_external_frame_png_bytes_controlled,
};

pub use transaction::{
    TerminalColorMode, TerminalFrameGeometry, TerminalOutputFrame, TerminalTransaction,
    TerminalTransactionControl, TerminalTransactionError, TerminalTransactionLimits,
    TerminalTransactionRequest, TerminalTransactionResult, TerminalWriteReceipt,
    TerminalWriteReceiptOutcome, TerminalWriterId,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Color {
    Default,
    Rgb(u8, u8, u8),
    Indexed(u8),
}

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Attributes: u8 {
        const BOLD = 1 << 0;
        const DIM = 1 << 1;
        const ITALIC = 1 << 2;
        const UNDERLINE = 1 << 3;
        const REVERSE = 1 << 4;
        const STRIKE = 1 << 5;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CellWidth {
    Zero,
    One,
    Two,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cell {
    pub grapheme: String,
    pub foreground: Color,
    pub background: Color,
    pub attributes: Attributes,
    pub width: CellWidth,
    pub hyperlink: Option<LinkId>,
    pub source_node: Option<NodeId>,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            grapheme: " ".to_owned(),
            foreground: Color::Default,
            background: Color::Default,
            attributes: Attributes::empty(),
            width: CellWidth::One,
            hyperlink: None,
            source_node: None,
        }
    }
}

#[derive(Debug, Error)]
pub enum SurfaceError {
    #[error("surface dimensions overflow addressable memory")]
    DimensionOverflow,
    #[error("surface exceeds the configured cell limit")]
    CellLimit,
    #[error("cell coordinates are outside the surface")]
    OutOfBounds,
    #[error("cell content must be exactly one printable grapheme cluster")]
    InvalidGrapheme,
    #[error("RGBA input length does not match its dimensions")]
    InvalidRgbaLength,
    #[error("text projection requires a semantic surface")]
    SemanticSurfaceRequired,
    #[error("projection dimensions must be non-zero")]
    ZeroDimensions,
    #[error("projection exceeds the configured pixel limit")]
    PixelLimit,
    #[error("terminal protocol output exceeds the configured byte limit")]
    OutputLimit,
    #[error("a double-width cell does not fit at the requested column")]
    WideCellAtEdge,
    #[error("a continuation cell cannot be inserted directly")]
    InvalidContinuation,
    #[error("surface contains an invalid double-width cell continuation")]
    InvalidCellLayout,
}

impl Cell {
    pub fn new(grapheme: impl Into<String>) -> Result<Self, SurfaceError> {
        let grapheme = grapheme.into();
        validate_grapheme(&grapheme)?;
        let width = match UnicodeWidthStr::width(grapheme.as_str()) {
            1 => CellWidth::One,
            2 => CellWidth::Two,
            _ => return Err(SurfaceError::InvalidGrapheme),
        };
        Ok(Self {
            grapheme,
            width,
            ..Self::default()
        })
    }

    fn continuation(lead: &Self) -> Self {
        Self {
            grapheme: String::new(),
            foreground: lead.foreground,
            background: lead.background,
            attributes: lead.attributes,
            width: CellWidth::Zero,
            hyperlink: lead.hyperlink,
            source_node: lead.source_node,
        }
    }
}

fn validate_grapheme(value: &str) -> Result<(), SurfaceError> {
    let mut graphemes = value.graphemes(true);
    let Some(grapheme) = graphemes.next() else {
        return Err(SurfaceError::InvalidGrapheme);
    };
    if graphemes.next().is_some()
        || grapheme
            .chars()
            .any(|character| character.is_control() || character == '\u{1b}')
    {
        return Err(SurfaceError::InvalidGrapheme);
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Surface {
    columns: u16,
    rows: u16,
    cells: Vec<Cell>,
}

impl Surface {
    pub fn new(columns: u16, rows: u16, max_cells: usize) -> Result<Self, SurfaceError> {
        let count = usize::from(columns)
            .checked_mul(usize::from(rows))
            .ok_or(SurfaceError::DimensionOverflow)?;
        if count > max_cells {
            return Err(SurfaceError::CellLimit);
        }
        Ok(Self {
            columns,
            rows,
            cells: vec![Cell::default(); count],
        })
    }

    pub const fn columns(&self) -> u16 {
        self.columns
    }

    pub const fn rows(&self) -> u16 {
        self.rows
    }

    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    pub fn get(&self, column: u16, row: u16) -> Option<&Cell> {
        self.index(column, row)
            .and_then(|index| self.cells.get(index))
    }

    pub fn set(&mut self, column: u16, row: u16, cell: Cell) -> Result<(), SurfaceError> {
        let index = self.index(column, row).ok_or(SurfaceError::OutOfBounds)?;
        let destination = self.cells.get_mut(index).ok_or(SurfaceError::OutOfBounds)?;
        *destination = cell;
        Ok(())
    }

    pub fn put_cell(&mut self, column: u16, row: u16, cell: Cell) -> Result<(), SurfaceError> {
        if matches!(cell.width, CellWidth::Zero) {
            return Err(SurfaceError::InvalidContinuation);
        }
        let width = match cell.width {
            CellWidth::Zero => 0,
            CellWidth::One => 1,
            CellWidth::Two => 2,
        };
        if width == 2
            && column
                .checked_add(1)
                .is_none_or(|next| next >= self.columns)
        {
            return Err(SurfaceError::WideCellAtEdge);
        }
        self.clear_overlapping_cell(column, row)?;
        if width == 2 {
            self.clear_overlapping_cell(column + 1, row)?;
        }
        self.set(column, row, cell.clone())?;
        if width == 2 {
            self.set(column + 1, row, Cell::continuation(&cell))?;
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), SurfaceError> {
        for row in 0..self.rows {
            for column in 0..self.columns {
                let cell = self.get(column, row).ok_or(SurfaceError::OutOfBounds)?;
                match cell.width {
                    CellWidth::Zero => {
                        if column == 0
                            || !matches!(
                                self.get(column - 1, row).map(|lead| lead.width),
                                Some(CellWidth::Two)
                            )
                        {
                            return Err(SurfaceError::InvalidCellLayout);
                        }
                    }
                    CellWidth::Two => {
                        if !matches!(
                            column
                                .checked_add(1)
                                .and_then(|next| self.get(next, row))
                                .map(|continuation| continuation.width),
                            Some(CellWidth::Zero)
                        ) {
                            return Err(SurfaceError::InvalidCellLayout);
                        }
                    }
                    CellWidth::One => {}
                }
            }
        }
        Ok(())
    }

    fn clear_overlapping_cell(&mut self, column: u16, row: u16) -> Result<(), SurfaceError> {
        let current = self
            .get(column, row)
            .cloned()
            .ok_or(SurfaceError::OutOfBounds)?;
        match current.width {
            CellWidth::Zero if column > 0 => {
                self.set(column - 1, row, Cell::default())?;
                self.set(column, row, Cell::default())?;
            }
            CellWidth::Two => {
                self.set(column, row, Cell::default())?;
                if let Some(next) = column.checked_add(1).filter(|next| *next < self.columns) {
                    self.set(next, row, Cell::default())?;
                }
            }
            CellWidth::Zero | CellWidth::One => {
                self.set(column, row, Cell::default())?;
            }
        }
        Ok(())
    }

    fn index(&self, column: u16, row: u16) -> Option<usize> {
        if column >= self.columns || row >= self.rows {
            return None;
        }
        usize::from(row)
            .checked_mul(usize::from(self.columns))
            .and_then(|value| value.checked_add(usize::from(column)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rgba {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
    pub alpha: u8,
}

impl Rgba {
    pub const TRANSPARENT: Self = Self {
        red: 0,
        green: 0,
        blue: 0,
        alpha: 0,
    };

    fn flatten(self) -> (u8, u8, u8) {
        let alpha = u16::from(self.alpha);
        let inverse = 255 - alpha;
        let channel = |value: u8| ((u16::from(value) * alpha + 255 * inverse) / 255) as u8;
        (channel(self.red), channel(self.green), channel(self.blue))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgbaSurface {
    width: u32,
    height: u32,
    pixels: Vec<Rgba>,
}

impl RgbaSurface {
    pub fn new(width: u32, height: u32, pixels: Vec<Rgba>) -> Result<Self, SurfaceError> {
        let expected = usize::try_from(width)
            .ok()
            .and_then(|value| value.checked_mul(usize::try_from(height).ok()?))
            .ok_or(SurfaceError::DimensionOverflow)?;
        if expected != pixels.len() {
            return Err(SurfaceError::InvalidRgbaLength);
        }
        Ok(Self {
            width,
            height,
            pixels,
        })
    }

    pub const fn width(&self) -> u32 {
        self.width
    }

    pub const fn height(&self) -> u32 {
        self.height
    }

    pub fn pixels(&self) -> &[Rgba] {
        &self.pixels
    }

    fn sample(&self, x: u32, y: u32) -> Rgba {
        if self.width == 0 || self.height == 0 {
            return Rgba::TRANSPARENT;
        }
        let clamped_x = x.min(self.width - 1);
        let clamped_y = y.min(self.height - 1);
        let index = usize::try_from(clamped_y)
            .ok()
            .and_then(|row| row.checked_mul(usize::try_from(self.width).ok()?))
            .and_then(|base| base.checked_add(usize::try_from(clamped_x).ok()?));
        index
            .and_then(|value| self.pixels.get(value).copied())
            .unwrap_or(Rgba::TRANSPARENT)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Auto,
    Text,
    Cells,
    Halfblock,
    Quadrant,
    Braille,
}

/// Bounded feedback from a caller-owned terminal transport after a backend was selected.
///
/// This is deliberately protocol- and device-free. A caller may attest terminal capabilities and
/// report one bounded transport outcome, but this value never probes the OS, opens a TTY, or
/// carries terminal control bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalBackendFeedback {
    /// Select the requested backend if it is both attested and permitted by the caller policy.
    Initial,
    /// The requested protocol was not available at the terminal boundary.
    UnsupportedCapability,
    /// A complete terminal write and flush was not acknowledged.
    WriteRejected,
    /// The bounded compositor or transport output quota was exceeded.
    OutputQuotaExceeded,
    /// The caller-owned bounded queue or transport asked the producer to coalesce work.
    Backpressured,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorLevel {
    Monochrome,
    Ansi16,
    Ansi256,
    TrueColor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalCapabilities {
    pub color: ColorLevel,
    pub dumb: bool,
}

impl TerminalCapabilities {
    pub fn from_environment(environment: &HashMap<String, String>) -> Self {
        let term = environment.get("TERM").map(String::as_str).unwrap_or("");
        let color_term = environment
            .get("COLORTERM")
            .map(String::as_str)
            .unwrap_or("");
        let term_lower = term.to_ascii_lowercase();
        let color_term_lower = color_term.to_ascii_lowercase();
        let dumb = term.is_empty() || term_lower == "dumb";
        let color_override = environment
            .get("TERMGLIDE_COLOR")
            .map(|value| value.to_ascii_lowercase());
        let color = if dumb
            || environment.contains_key("NO_COLOR")
            || matches!(color_override.as_deref(), Some("mono"))
        {
            ColorLevel::Monochrome
        } else if matches!(color_override.as_deref(), Some("16")) {
            ColorLevel::Ansi16
        } else if matches!(color_override.as_deref(), Some("256")) {
            ColorLevel::Ansi256
        } else if matches!(color_override.as_deref(), Some("truecolor" | "24bit"))
            || color_term_lower.contains("truecolor")
            || color_term_lower.contains("24bit")
        {
            ColorLevel::TrueColor
        } else if term_lower.contains("256color") {
            ColorLevel::Ansi256
        } else {
            ColorLevel::Ansi16
        };
        Self { color, dumb }
    }

    pub const fn select_backend(&self, requested: Backend) -> Backend {
        if !matches!(requested, Backend::Auto) {
            return requested;
        }
        if self.dumb {
            Backend::Text
        } else if matches!(self.color, ColorLevel::TrueColor) {
            Backend::Halfblock
        } else {
            Backend::Cells
        }
    }

    /// Returns whether one explicit backend is attested for this terminal.
    ///
    pub const fn supports_backend(&self, backend: Backend) -> bool {
        match backend {
            Backend::Auto => false,
            Backend::Text => true,
            Backend::Cells => true,
            Backend::Quadrant | Backend::Braille => !self.dumb,
            Backend::Halfblock => !self.dumb && matches!(self.color, ColorLevel::TrueColor),
        }
    }

    /// Selects a capability-attested backend without inspecting ambient terminal state.
    ///
    /// Any unsupported request or bounded failure takes one deterministic step toward the
    /// semantic `Text` fallback.
    pub const fn select_safe_backend(
        &self,
        requested: Backend,
        feedback: TerminalBackendFeedback,
    ) -> Backend {
        let requested = match requested {
            Backend::Auto => self.auto_backend(),
            backend => backend,
        };
        match feedback {
            TerminalBackendFeedback::Initial => self.supported_or_semantic_fallback(requested),
            TerminalBackendFeedback::UnsupportedCapability => {
                self.supported_or_semantic_fallback(self.downgrade_backend(requested))
            }
            TerminalBackendFeedback::WriteRejected
            | TerminalBackendFeedback::OutputQuotaExceeded
            | TerminalBackendFeedback::Backpressured => {
                self.supported_or_semantic_fallback(self.downgrade_backend(requested))
            }
        }
    }

    const fn auto_backend(&self) -> Backend {
        if self.dumb {
            Backend::Text
        } else if matches!(self.color, ColorLevel::TrueColor) {
            Backend::Halfblock
        } else {
            Backend::Cells
        }
    }

    const fn supported_or_semantic_fallback(&self, requested: Backend) -> Backend {
        if self.supports_backend(requested) {
            requested
        } else {
            self.semantic_fallback()
        }
    }

    const fn semantic_fallback(&self) -> Backend {
        if self.supports_backend(Backend::Halfblock) {
            Backend::Halfblock
        } else if self.supports_backend(Backend::Cells) {
            Backend::Cells
        } else {
            Backend::Text
        }
    }

    const fn downgrade_backend(&self, backend: Backend) -> Backend {
        match backend {
            Backend::Auto | Backend::Text => Backend::Text,
            Backend::Cells => Backend::Text,
            Backend::Halfblock | Backend::Quadrant | Backend::Braille => Backend::Cells,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Projection {
    Cells(Surface),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectionLimits {
    pub max_cells: usize,
    pub max_pixels: usize,
    pub max_output_bytes: usize,
}

impl Default for ProjectionLimits {
    fn default() -> Self {
        Self {
            max_cells: 1_000_000,
            max_pixels: 16_000_000,
            max_output_bytes: 64 * 1024 * 1024,
        }
    }
}

pub fn project_rgba(
    source: &RgbaSurface,
    backend: Backend,
    columns: u16,
    rows: u16,
) -> Result<Projection, SurfaceError> {
    project_rgba_with_limits(source, backend, columns, rows, ProjectionLimits::default())
}

pub fn project_rgba_with_limits(
    source: &RgbaSurface,
    backend: Backend,
    columns: u16,
    rows: u16,
    limits: ProjectionLimits,
) -> Result<Projection, SurfaceError> {
    if source.width == 0 || source.height == 0 || columns == 0 || rows == 0 {
        return Err(SurfaceError::ZeroDimensions);
    }
    if source.pixels.len() > limits.max_pixels {
        return Err(SurfaceError::PixelLimit);
    }
    let cell_count = usize::from(columns)
        .checked_mul(usize::from(rows))
        .ok_or(SurfaceError::DimensionOverflow)?;
    if cell_count > limits.max_cells {
        return Err(SurfaceError::CellLimit);
    }
    match backend {
        Backend::Auto => Err(SurfaceError::SemanticSurfaceRequired),
        Backend::Text => Err(SurfaceError::SemanticSurfaceRequired),
        Backend::Cells => Ok(Projection::Cells(project_cells(
            source,
            columns,
            rows,
            false,
            limits.max_cells,
        )?)),
        Backend::Halfblock => Ok(Projection::Cells(project_cells(
            source,
            columns,
            rows,
            true,
            limits.max_cells,
        )?)),
        Backend::Quadrant => Ok(Projection::Cells(project_quadrant(
            source,
            columns,
            rows,
            limits.max_cells,
        )?)),
        Backend::Braille => Ok(Projection::Cells(project_braille(
            source,
            columns,
            rows,
            limits.max_cells,
        )?)),
    }
}

fn project_cells(
    source: &RgbaSurface,
    columns: u16,
    rows: u16,
    halfblock: bool,
    max_cells: usize,
) -> Result<Surface, SurfaceError> {
    let mut surface = Surface::new(columns, rows, max_cells)?;
    let sample_rows = u32::from(rows).saturating_mul(if halfblock { 2 } else { 1 });
    for row in 0..rows {
        for column in 0..columns {
            let top = sample_rgb_region(
                source,
                u32::from(column),
                u32::from(row) * if halfblock { 2 } else { 1 },
                u32::from(columns),
                sample_rows,
            );
            let mut cell = if halfblock {
                Cell::new("▀")?
            } else {
                Cell::new(" ")?
            };
            if halfblock {
                let bottom = sample_rgb_region(
                    source,
                    u32::from(column),
                    u32::from(row) * 2 + 1,
                    u32::from(columns),
                    sample_rows,
                );
                cell.foreground = Color::Rgb(top.0, top.1, top.2);
                cell.background = Color::Rgb(bottom.0, bottom.1, bottom.2);
            } else {
                cell.background = Color::Rgb(top.0, top.1, top.2);
            }
            surface.set(column, row, cell)?;
        }
    }
    Ok(surface)
}

fn project_quadrant(
    source: &RgbaSurface,
    columns: u16,
    rows: u16,
    max_cells: usize,
) -> Result<Surface, SurfaceError> {
    let mut surface = Surface::new(columns, rows, max_cells)?;
    let sample_columns = u32::from(columns).saturating_mul(2);
    let sample_rows = u32::from(rows).saturating_mul(2);
    for row in 0..rows {
        for column in 0..columns {
            let mut samples = [(0, 0, 0); 4];
            for sample_y in 0..2u32 {
                for sample_x in 0..2u32 {
                    samples[(sample_y * 2 + sample_x) as usize] = sample_rgb_region(
                        source,
                        u32::from(column) * 2 + sample_x,
                        u32::from(row) * 2 + sample_y,
                        sample_columns,
                        sample_rows,
                    );
                }
            }
            let (pattern, foreground, background) = quantize_quadrant_cell(&samples);
            let mut cell = Cell::new(QUADRANT_GLYPHS[usize::from(pattern)].to_owned())?;
            cell.foreground = Color::Rgb(foreground.0, foreground.1, foreground.2);
            cell.background = Color::Rgb(background.0, background.1, background.2);
            surface.set(column, row, cell)?;
        }
    }
    Ok(surface)
}

fn project_braille(
    source: &RgbaSurface,
    columns: u16,
    rows: u16,
    max_cells: usize,
) -> Result<Surface, SurfaceError> {
    let mut surface = Surface::new(columns, rows, max_cells)?;
    for row in 0..rows {
        for column in 0..columns {
            let mut samples = [(0, 0, 0); 8];
            for dot_x in 0..2u32 {
                for dot_y in 0..4u32 {
                    samples[(dot_x * 4 + dot_y) as usize] = sample_rgb_region(
                        source,
                        u32::from(column) * 2 + dot_x,
                        u32::from(row) * 4 + dot_y,
                        u32::from(columns) * 2,
                        u32::from(rows) * 4,
                    );
                }
            }
            let (pattern, foreground, background) = quantize_braille_cell(&samples);
            let character = char::from_u32(0x2800 + u32::from(pattern)).unwrap_or('\u{2800}');
            let mut cell = Cell::new(character.to_string())?;
            cell.foreground = Color::Rgb(foreground.0, foreground.1, foreground.2);
            cell.background = Color::Rgb(background.0, background.1, background.2);
            surface.set(column, row, cell)?;
        }
    }
    Ok(surface)
}

type Rgb = (u8, u8, u8);

const BRAILLE_DOTS: [u8; 8] = [0x01, 0x02, 0x04, 0x40, 0x08, 0x10, 0x20, 0x80];
const BRAILLE_MIN_SPLIT_DISTANCE_SQUARED: u32 = 12 * 12 * 3;
const QUADRANT_MASKS: [u8; 4] = [0x01, 0x02, 0x04, 0x08];
const QUADRANT_MIN_SPLIT_DISTANCE_SQUARED: u32 = 18 * 18 * 3;
const QUADRANT_GLYPHS: [&str; 16] = [
    " ", "▘", "▝", "▀", "▖", "▌", "▞", "▛", "▗", "▚", "▐", "▜", "▄", "▙", "▟", "█",
];

fn quantize_braille_cell(samples: &[Rgb; 8]) -> (u8, Rgb, Rgb) {
    quantize_binary_cell(samples, &BRAILLE_DOTS, BRAILLE_MIN_SPLIT_DISTANCE_SQUARED)
}

fn quantize_quadrant_cell(samples: &[Rgb; 4]) -> (u8, Rgb, Rgb) {
    quantize_binary_cell(
        samples,
        &QUADRANT_MASKS,
        QUADRANT_MIN_SPLIT_DISTANCE_SQUARED,
    )
}

fn quantize_binary_cell<const N: usize>(
    samples: &[Rgb; N],
    masks: &[u8; N],
    minimum_split_distance_squared: u32,
) -> (u8, Rgb, Rgb) {
    let mut seeds = (0usize, 0usize);
    let mut widest_distance = 0u32;
    for left in 0..samples.len() {
        for right in left + 1..samples.len() {
            let distance = rgb_distance_squared(samples[left], samples[right]);
            if distance > widest_distance {
                widest_distance = distance;
                seeds = (left, right);
            }
        }
    }

    if widest_distance <= minimum_split_distance_squared {
        let color = average_rgb(samples);
        return (0, color, color);
    }

    let mut centers = [samples[seeds.0], samples[seeds.1]];
    let mut assignments = [0u8; N];
    for _ in 0..3 {
        assign_rgb_clusters(samples, centers, &mut assignments);
        let (first, first_count) = average_rgb_cluster(samples, &assignments, 0);
        let (second, second_count) = average_rgb_cluster(samples, &assignments, 1);
        if first_count == 0 || second_count == 0 {
            let color = average_rgb(samples);
            return (0, color, color);
        }
        centers = [first, second];
    }
    assign_rgb_clusters(samples, centers, &mut assignments);
    let (first, first_count) = average_rgb_cluster(samples, &assignments, 0);
    let (second, second_count) = average_rgb_cluster(samples, &assignments, 1);
    centers = [first, second];

    let foreground_cluster = if first_count < second_count {
        0
    } else if second_count < first_count {
        1
    } else if rgb_luminance(centers[0]) <= rgb_luminance(centers[1]) {
        0
    } else {
        1
    };
    let background_cluster = 1 - foreground_cluster;
    let mut pattern = 0u8;
    for (index, cluster) in assignments.iter().copied().enumerate() {
        if usize::from(cluster) == foreground_cluster {
            pattern |= masks[index];
        }
    }
    (
        pattern,
        centers[foreground_cluster],
        centers[background_cluster],
    )
}

fn assign_rgb_clusters<const N: usize>(
    samples: &[Rgb; N],
    centers: [Rgb; 2],
    assignments: &mut [u8; N],
) {
    for (index, sample) in samples.iter().copied().enumerate() {
        assignments[index] = u8::from(
            rgb_distance_squared(sample, centers[1]) < rgb_distance_squared(sample, centers[0]),
        );
    }
}

fn average_rgb<const N: usize>(samples: &[Rgb; N]) -> Rgb {
    let mut sums = (0u32, 0u32, 0u32);
    for sample in samples {
        sums.0 += u32::from(sample.0);
        sums.1 += u32::from(sample.1);
        sums.2 += u32::from(sample.2);
    }
    let count = u32::try_from(N).unwrap_or(1).max(1);
    (
        (sums.0 / count) as u8,
        (sums.1 / count) as u8,
        (sums.2 / count) as u8,
    )
}

fn average_rgb_cluster<const N: usize>(
    samples: &[Rgb; N],
    assignments: &[u8; N],
    cluster: u8,
) -> (Rgb, u32) {
    let mut sums = (0u32, 0u32, 0u32);
    let mut count = 0u32;
    for (sample, assignment) in samples.iter().zip(assignments) {
        if *assignment == cluster {
            sums.0 += u32::from(sample.0);
            sums.1 += u32::from(sample.1);
            sums.2 += u32::from(sample.2);
            count += 1;
        }
    }
    if count == 0 {
        return ((0, 0, 0), 0);
    }
    (
        (
            (sums.0 / count) as u8,
            (sums.1 / count) as u8,
            (sums.2 / count) as u8,
        ),
        count,
    )
}

fn rgb_distance_squared(left: Rgb, right: Rgb) -> u32 {
    let red = i32::from(left.0) - i32::from(right.0);
    let green = i32::from(left.1) - i32::from(right.1);
    let blue = i32::from(left.2) - i32::from(right.2);
    (red * red + green * green + blue * blue) as u32
}

fn rgb_luminance(color: Rgb) -> u32 {
    u32::from(color.0) * 54 + u32::from(color.1) * 183 + u32::from(color.2) * 19
}

fn sample_rgb_region(
    source: &RgbaSurface,
    sample_x: u32,
    sample_y: u32,
    sample_columns: u32,
    sample_rows: u32,
) -> Rgb {
    let start_x = scale_coordinate(sample_x, sample_columns, source.width);
    let start_y = scale_coordinate(sample_y, sample_rows, source.height);
    let end_x = scale_coordinate(sample_x.saturating_add(1), sample_columns, source.width)
        .max(start_x.saturating_add(1))
        .min(source.width);
    let end_y = scale_coordinate(sample_y.saturating_add(1), sample_rows, source.height)
        .max(start_y.saturating_add(1))
        .min(source.height);
    let mut sums = (0u64, 0u64, 0u64);
    let mut count = 0u64;
    for y in start_y..end_y {
        for x in start_x..end_x {
            let color = source.sample(x, y).flatten();
            sums.0 += u64::from(color.0);
            sums.1 += u64::from(color.1);
            sums.2 += u64::from(color.2);
            count += 1;
        }
    }
    if count == 0 {
        return source.sample(start_x, start_y).flatten();
    }
    (
        (sums.0 / count) as u8,
        (sums.1 / count) as u8,
        (sums.2 / count) as u8,
    )
}

fn scale_coordinate(value: u32, source_extent: u32, target_extent: u32) -> u32 {
    if source_extent == 0 || target_extent == 0 {
        0
    } else {
        value.saturating_mul(target_extent) / source_extent
    }
}

fn ensure_output_limit(actual: usize, maximum: usize) -> Result<(), SurfaceError> {
    if actual > maximum {
        Err(SurfaceError::OutputLimit)
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalOp {
    ClearScreen,
    MoveTo {
        column: u16,
        row: u16,
    },
    SetStyle {
        foreground: Color,
        background: Color,
        attributes: Attributes,
    },
    Write(String),
    ResetStyle,
}

pub fn diff(previous: &Surface, next: &Surface) -> Result<Vec<TerminalOp>, SurfaceError> {
    previous.validate()?;
    next.validate()?;
    if previous.columns != next.columns || previous.rows != next.rows {
        return full_redraw(next);
    }
    let mut operations = Vec::new();
    for row in 0..next.rows {
        let mut column = 0u16;
        while column < next.columns {
            let before = previous.get(column, row).ok_or(SurfaceError::OutOfBounds)?;
            let after = next.get(column, row).ok_or(SurfaceError::OutOfBounds)?;
            if before == after {
                column += 1;
                continue;
            }
            if matches!(after.width, CellWidth::Zero) {
                column += 1;
                continue;
            }
            operations.push(TerminalOp::MoveTo { column, row });
            operations.push(style_operation(after));
            let mut text = String::new();
            let style = (after.foreground, after.background, after.attributes);
            while column < next.columns {
                let old = previous.get(column, row).ok_or(SurfaceError::OutOfBounds)?;
                let cell = next.get(column, row).ok_or(SurfaceError::OutOfBounds)?;
                if matches!(cell.width, CellWidth::Zero) {
                    column += 1;
                    continue;
                }
                if old == cell || (cell.foreground, cell.background, cell.attributes) != style {
                    break;
                }
                text.push_str(&cell.grapheme);
                column = column.saturating_add(match cell.width {
                    CellWidth::Two => 2,
                    CellWidth::Zero | CellWidth::One => 1,
                });
            }
            operations.push(TerminalOp::Write(text));
        }
    }
    if !operations.is_empty() {
        operations.push(TerminalOp::ResetStyle);
    }
    Ok(operations)
}

fn full_redraw(surface: &Surface) -> Result<Vec<TerminalOp>, SurfaceError> {
    let blank = Surface::new(surface.columns, surface.rows, surface.cells.len())?;
    let mut operations = vec![TerminalOp::ClearScreen];
    operations.extend(diff(&blank, surface)?);
    Ok(operations)
}

fn style_operation(cell: &Cell) -> TerminalOp {
    TerminalOp::SetStyle {
        foreground: cell.foreground,
        background: cell.background,
        attributes: cell.attributes,
    }
}

pub fn encode_operations(operations: &[TerminalOp]) -> Result<Vec<u8>, SurfaceError> {
    encode_operations_bounded(operations, usize::MAX)
}

pub fn encode_operations_bounded(
    operations: &[TerminalOp],
    max_output_bytes: usize,
) -> Result<Vec<u8>, SurfaceError> {
    let mut output = Vec::new();
    for operation in operations {
        match operation {
            TerminalOp::ClearScreen => {
                append_bounded(&mut output, b"\x1b[2J\x1b[H", max_output_bytes)?
            }
            TerminalOp::MoveTo { column, row } => append_bounded(
                &mut output,
                format!("\u{1b}[{};{}H", u32::from(*row) + 1, u32::from(*column) + 1).as_bytes(),
                max_output_bytes,
            )?,
            TerminalOp::SetStyle {
                foreground,
                background,
                attributes,
            } => {
                let mut style = Vec::with_capacity(64);
                encode_style(&mut style, *foreground, *background, *attributes);
                append_bounded(&mut output, &style, max_output_bytes)?;
            }
            TerminalOp::Write(text) => {
                for grapheme in text.graphemes(true) {
                    if validate_grapheme(grapheme).is_ok() {
                        append_bounded(&mut output, grapheme.as_bytes(), max_output_bytes)?;
                    } else {
                        append_bounded(&mut output, "�".as_bytes(), max_output_bytes)?;
                    }
                }
            }
            TerminalOp::ResetStyle => {
                append_bounded(&mut output, b"\x1b[0m", max_output_bytes)?;
            }
        }
        ensure_output_limit(output.len(), max_output_bytes)?;
    }
    Ok(output)
}

fn append_bounded(
    output: &mut Vec<u8>,
    bytes: &[u8],
    max_output_bytes: usize,
) -> Result<(), SurfaceError> {
    let new_len = output
        .len()
        .checked_add(bytes.len())
        .ok_or(SurfaceError::DimensionOverflow)?;
    ensure_output_limit(new_len, max_output_bytes)?;
    output.extend_from_slice(bytes);
    Ok(())
}

fn encode_style(
    output: &mut Vec<u8>,
    foreground: Color,
    background: Color,
    attributes: Attributes,
) {
    output.extend_from_slice(b"\x1b[0");
    for (flag, code) in [
        (Attributes::BOLD, 1),
        (Attributes::DIM, 2),
        (Attributes::ITALIC, 3),
        (Attributes::UNDERLINE, 4),
        (Attributes::REVERSE, 7),
        (Attributes::STRIKE, 9),
    ] {
        if attributes.contains(flag) {
            output.extend_from_slice(format!(";{code}").as_bytes());
        }
    }
    encode_color(output, foreground, true);
    encode_color(output, background, false);
    output.push(b'm');
}

fn encode_color(output: &mut Vec<u8>, color: Color, foreground: bool) {
    let default_code = if foreground { 39 } else { 49 };
    let indexed_prefix = if foreground { 38 } else { 48 };
    match color {
        Color::Default => output.extend_from_slice(format!(";{default_code}").as_bytes()),
        Color::Indexed(index) => {
            output.extend_from_slice(format!(";{indexed_prefix};5;{index}").as_bytes());
        }
        Color::Rgb(red, green, blue) => {
            output
                .extend_from_slice(format!(";{indexed_prefix};2;{red};{green};{blue}").as_bytes());
        }
    }
}

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Modifiers: u8 {
        const SHIFT = 1 << 0;
        const ALT = 1 << 1;
        const CONTROL = 1 << 2;
        const SUPER = 1 << 3;
        const HYPER = 1 << 4;
        const META = 1 << 5;
        const CAPS_LOCK = 1 << 6;
        const NUM_LOCK = 1 << 7;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyCode {
    Character(char),
    Enter,
    Escape,
    Backspace,
    Tab,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    Function(u8),
    Functional(u32),
    Unidentified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEventKind {
    Press,
    Repeat,
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
    WheelLeft,
    WheelRight,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    Key {
        code: KeyCode,
        modifiers: Modifiers,
        kind: KeyEventKind,
        shifted_key: Option<char>,
        base_layout_key: Option<char>,
        text: Option<String>,
    },
    Mouse {
        button: MouseButton,
        column: u16,
        row: u16,
        pressed: bool,
        modifiers: Modifiers,
    },
    Paste(String),
    Focus(bool),
}

/// Wire family that produced one normalized terminal input event.
///
/// This is metadata only. Capability policy remains caller-owned and no raw sequence is retained
/// after normalization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputProtocol {
    Vt,
    KittyKeyboard,
    SgrMouse,
    Focus,
    BracketedPaste,
}

/// A normalized terminal input event paired with its bounded wire-family provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedInputEvent {
    protocol: InputProtocol,
    event: InputEvent,
}

impl DecodedInputEvent {
    pub const fn protocol(&self) -> InputProtocol {
        self.protocol
    }

    pub fn event(&self) -> &InputEvent {
        &self.event
    }

    pub fn into_event(self) -> InputEvent {
        self.event
    }
}

#[derive(Debug, Error)]
pub enum InputError {
    #[error("terminal input is not valid UTF-8")]
    InvalidUtf8,
    #[error("terminal input sequence is incomplete or unknown")]
    UnknownSequence,
    #[error("terminal coordinate is invalid")]
    InvalidCoordinate,
    #[error("bracketed paste exceeds its configured byte limit")]
    PasteLimit,
    #[error("terminal input sequence exceeds its configured byte limit")]
    SequenceLimit,
    #[error("terminal input contains an invalid key code or modifier")]
    InvalidKeyCode,
    #[error("terminal input decoding was cancelled")]
    Cancelled,
}

pub fn decode_input(sequence: &[u8], max_paste_bytes: usize) -> Result<InputEvent, InputError> {
    decode_input_with_protocol(sequence, max_paste_bytes).map(DecodedInputEvent::into_event)
}

/// Decodes a bounded terminal input sequence and retains only its normalized protocol family.
pub fn decode_input_with_protocol(
    sequence: &[u8],
    max_paste_bytes: usize,
) -> Result<DecodedInputEvent, InputError> {
    let protocol = input_protocol(sequence);
    let event = decode_input_event(sequence, max_paste_bytes)?;
    Ok(DecodedInputEvent { protocol, event })
}

fn input_protocol(sequence: &[u8]) -> InputProtocol {
    if sequence.starts_with(b"\x1b[200~") {
        InputProtocol::BracketedPaste
    } else if sequence == b"\x1b[I" || sequence == b"\x1b[O" {
        InputProtocol::Focus
    } else if sequence.starts_with(b"\x1b[<") {
        InputProtocol::SgrMouse
    } else if sequence.starts_with(b"\x1b[") && sequence.ends_with(b"u") {
        InputProtocol::KittyKeyboard
    } else {
        InputProtocol::Vt
    }
}

fn decode_input_event(sequence: &[u8], max_paste_bytes: usize) -> Result<InputEvent, InputError> {
    if sequence.starts_with(b"\x1b[200~") {
        if !sequence.ends_with(b"\x1b[201~") || sequence.len() < 12 {
            return Err(InputError::UnknownSequence);
        }
        let content = &sequence[6..sequence.len() - 6];
        if content.len() > max_paste_bytes {
            return Err(InputError::PasteLimit);
        }
        return Ok(InputEvent::Paste(
            std::str::from_utf8(content)
                .map_err(|_| InputError::InvalidUtf8)?
                .to_owned(),
        ));
    }
    if sequence == b"\x1b[I" {
        return Ok(InputEvent::Focus(true));
    }
    if sequence == b"\x1b[O" {
        return Ok(InputEvent::Focus(false));
    }
    if sequence.starts_with(b"\x1b[<") {
        return decode_sgr_mouse(sequence);
    }
    if let Some(event) = decode_csi_key(sequence)? {
        return Ok(event);
    }
    let code = match sequence {
        b"\r" | b"\n" => KeyCode::Enter,
        b"\x1b" => KeyCode::Escape,
        b"\x7f" => KeyCode::Backspace,
        b"\t" => KeyCode::Tab,
        b"\x1b[A" => KeyCode::Up,
        b"\x1b[B" => KeyCode::Down,
        b"\x1b[C" => KeyCode::Right,
        b"\x1b[D" => KeyCode::Left,
        b"\x1b[H" | b"\x1b[1~" => KeyCode::Home,
        b"\x1b[F" | b"\x1b[4~" => KeyCode::End,
        b"\x1b[5~" => KeyCode::PageUp,
        b"\x1b[6~" => KeyCode::PageDown,
        b"\x1b[2~" => KeyCode::Insert,
        b"\x1b[3~" => KeyCode::Delete,
        b"\x1bOP" => KeyCode::Function(1),
        b"\x1bOQ" => KeyCode::Function(2),
        b"\x1bOR" => KeyCode::Function(3),
        b"\x1bOS" => KeyCode::Function(4),
        [control] if (1..=26).contains(control) => {
            return Ok(key_event(
                KeyCode::Character(char::from(b'a' + control - 1)),
                Modifiers::CONTROL,
                None,
            ));
        }
        _ => {
            if let Some(rest) = sequence.strip_prefix(b"\x1b") {
                let text = std::str::from_utf8(rest).map_err(|_| InputError::InvalidUtf8)?;
                let mut characters = text.chars();
                let character = characters.next().ok_or(InputError::UnknownSequence)?;
                if characters.next().is_some() || character.is_control() {
                    return Err(InputError::UnknownSequence);
                }
                return Ok(key_event(
                    KeyCode::Character(character),
                    Modifiers::ALT,
                    Some(character.to_string()),
                ));
            }
            let text = std::str::from_utf8(sequence).map_err(|_| InputError::InvalidUtf8)?;
            let mut characters = text.chars();
            let Some(character) = characters.next() else {
                return Err(InputError::UnknownSequence);
            };
            if characters.next().is_some() || character.is_control() {
                return Err(InputError::UnknownSequence);
            }
            return Ok(key_event(
                KeyCode::Character(character),
                Modifiers::empty(),
                Some(character.to_string()),
            ));
        }
    };
    Ok(key_event(code, Modifiers::empty(), None))
}

fn key_event(code: KeyCode, modifiers: Modifiers, text: Option<String>) -> InputEvent {
    InputEvent::Key {
        code,
        modifiers,
        kind: KeyEventKind::Press,
        shifted_key: None,
        base_layout_key: None,
        text,
    }
}

fn decode_csi_key(sequence: &[u8]) -> Result<Option<InputEvent>, InputError> {
    let Some(body) = sequence.strip_prefix(b"\x1b[") else {
        return Ok(None);
    };
    let Some(final_byte) = body.last().copied() else {
        return Ok(None);
    };
    let parameters =
        std::str::from_utf8(&body[..body.len() - 1]).map_err(|_| InputError::InvalidUtf8)?;
    match final_byte {
        b'u' => decode_kitty_key(parameters).map(Some),
        b'A' | b'B' | b'C' | b'D' | b'H' | b'F' | b'P' | b'Q' | b'R' | b'S' | b'Z' => {
            let modifiers = if parameters.is_empty() {
                if final_byte == b'Z' {
                    Modifiers::SHIFT
                } else {
                    Modifiers::empty()
                }
            } else {
                let mut fields = parameters.split(';');
                if fields.next() != Some("1") {
                    return Err(InputError::InvalidKeyCode);
                }
                let modifiers = decode_modifiers(fields.next().unwrap_or("1"))?.0;
                if fields.next().is_some() {
                    return Err(InputError::InvalidKeyCode);
                }
                modifiers
            };
            let code = match final_byte {
                b'A' => KeyCode::Up,
                b'B' => KeyCode::Down,
                b'C' => KeyCode::Right,
                b'D' => KeyCode::Left,
                b'H' => KeyCode::Home,
                b'F' => KeyCode::End,
                b'P' => KeyCode::Function(1),
                b'Q' => KeyCode::Function(2),
                b'R' => KeyCode::Function(3),
                b'S' => KeyCode::Function(4),
                b'Z' => KeyCode::Tab,
                _ => return Err(InputError::InvalidKeyCode),
            };
            Ok(Some(key_event(code, modifiers, None)))
        }
        b'~' => {
            let mut fields = parameters.split(';');
            let number = fields
                .next()
                .ok_or(InputError::InvalidKeyCode)?
                .parse::<u16>()
                .map_err(|_| InputError::InvalidKeyCode)?;
            let modifiers = decode_modifiers(fields.next().unwrap_or("1"))?.0;
            if fields.next().is_some() {
                return Err(InputError::InvalidKeyCode);
            }
            let code = match number {
                2 => KeyCode::Insert,
                3 => KeyCode::Delete,
                5 => KeyCode::PageUp,
                6 => KeyCode::PageDown,
                7 => KeyCode::Home,
                8 => KeyCode::End,
                11 => KeyCode::Function(1),
                12 => KeyCode::Function(2),
                13 => KeyCode::Function(3),
                14 => KeyCode::Function(4),
                15 => KeyCode::Function(5),
                17 => KeyCode::Function(6),
                18 => KeyCode::Function(7),
                19 => KeyCode::Function(8),
                20 => KeyCode::Function(9),
                21 => KeyCode::Function(10),
                23 => KeyCode::Function(11),
                24 => KeyCode::Function(12),
                _ => return Ok(None),
            };
            Ok(Some(key_event(code, modifiers, None)))
        }
        _ => Ok(None),
    }
}

fn decode_kitty_key(parameters: &str) -> Result<InputEvent, InputError> {
    let mut fields = parameters.split(';');
    let key_field = fields.next().ok_or(InputError::InvalidKeyCode)?;
    let modifier_field = fields.next().unwrap_or("1");
    let text_field = fields.next();
    if fields.next().is_some() {
        return Err(InputError::InvalidKeyCode);
    }

    let mut key_parts = key_field.split(':');
    let key_number = parse_key_number(key_parts.next().ok_or(InputError::InvalidKeyCode)?)?;
    let shifted_key = key_parts
        .next()
        .map(parse_optional_character)
        .transpose()?
        .flatten();
    let base_layout_key = key_parts
        .next()
        .map(parse_optional_character)
        .transpose()?
        .flatten();
    if key_parts.next().is_some() {
        return Err(InputError::InvalidKeyCode);
    }
    let (modifiers, kind) = decode_modifiers(modifier_field)?;
    let text = text_field
        .map(decode_associated_text)
        .transpose()?
        .flatten();
    let code = match key_number {
        0 if text.is_some() => KeyCode::Unidentified,
        9 => KeyCode::Tab,
        13 => KeyCode::Enter,
        27 => KeyCode::Escape,
        127 => KeyCode::Backspace,
        57_376..=57_398 => KeyCode::Function((key_number - 57_363) as u8),
        57_344..=63_743 => KeyCode::Functional(key_number),
        value => {
            let character = char::from_u32(value).ok_or(InputError::InvalidKeyCode)?;
            if character.is_control() {
                return Err(InputError::InvalidKeyCode);
            }
            KeyCode::Character(character)
        }
    };
    Ok(InputEvent::Key {
        code,
        modifiers,
        kind,
        shifted_key,
        base_layout_key,
        text,
    })
}

fn parse_key_number(value: &str) -> Result<u32, InputError> {
    value.parse::<u32>().map_err(|_| InputError::InvalidKeyCode)
}

fn parse_optional_character(value: &str) -> Result<Option<char>, InputError> {
    if value.is_empty() {
        return Ok(None);
    }
    let number = parse_key_number(value)?;
    let character = char::from_u32(number).ok_or(InputError::InvalidKeyCode)?;
    if character.is_control() {
        return Err(InputError::InvalidKeyCode);
    }
    Ok(Some(character))
}

fn decode_modifiers(value: &str) -> Result<(Modifiers, KeyEventKind), InputError> {
    let mut fields = value.split(':');
    let encoded = fields
        .next()
        .filter(|field| !field.is_empty())
        .unwrap_or("1")
        .parse::<u16>()
        .map_err(|_| InputError::InvalidKeyCode)?;
    if !(1..=256).contains(&encoded) {
        return Err(InputError::InvalidKeyCode);
    }
    let kind = match fields.next().unwrap_or("1") {
        "1" => KeyEventKind::Press,
        "2" => KeyEventKind::Repeat,
        "3" => KeyEventKind::Release,
        _ => return Err(InputError::InvalidKeyCode),
    };
    if fields.next().is_some() {
        return Err(InputError::InvalidKeyCode);
    }
    Ok((Modifiers::from_bits_retain((encoded - 1) as u8), kind))
}

fn decode_associated_text(value: &str) -> Result<Option<String>, InputError> {
    if value.is_empty() {
        return Ok(None);
    }
    let mut text = String::new();
    for number in value.split(':') {
        let character =
            char::from_u32(parse_key_number(number)?).ok_or(InputError::InvalidKeyCode)?;
        if character.is_control() {
            return Err(InputError::InvalidKeyCode);
        }
        text.push(character);
    }
    Ok(Some(text))
}

#[derive(Debug)]
pub struct InputDecoder {
    buffer: Vec<u8>,
    max_sequence_bytes: usize,
    max_paste_bytes: usize,
    discarding_paste: bool,
    discard_match: usize,
}

impl InputDecoder {
    pub fn new(max_sequence_bytes: usize, max_paste_bytes: usize) -> Self {
        Self {
            buffer: Vec::new(),
            max_sequence_bytes,
            max_paste_bytes,
            discarding_paste: false,
            discard_match: 0,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Vec<Result<InputEvent, InputError>> {
        self.push_with_cancellation(bytes, &Cancellation::new())
    }

    pub fn push_with_cancellation(
        &mut self,
        bytes: &[u8],
        cancellation: &Cancellation,
    ) -> Vec<Result<InputEvent, InputError>> {
        self.push_with_cancellation_map(bytes, cancellation, decode_input)
    }

    /// Decodes the same bounded byte stream as [`Self::push`] while retaining wire-family
    /// provenance for a caller-owned capability policy.
    pub fn push_tagged(&mut self, bytes: &[u8]) -> Vec<Result<DecodedInputEvent, InputError>> {
        self.push_tagged_with_cancellation(bytes, &Cancellation::new())
    }

    pub fn push_tagged_with_cancellation(
        &mut self,
        bytes: &[u8],
        cancellation: &Cancellation,
    ) -> Vec<Result<DecodedInputEvent, InputError>> {
        self.push_with_cancellation_map(bytes, cancellation, decode_input_with_protocol)
    }

    fn push_with_cancellation_map<T, F>(
        &mut self,
        bytes: &[u8],
        cancellation: &Cancellation,
        mut decode: F,
    ) -> Vec<Result<T, InputError>>
    where
        F: FnMut(&[u8], usize) -> Result<T, InputError>,
    {
        let mut results = Vec::new();
        for byte in bytes {
            if cancellation.is_cancelled() {
                results.push(Err(InputError::Cancelled));
                return results;
            }
            if self.discarding_paste {
                self.consume_discarded_paste_byte(*byte);
                continue;
            }
            self.buffer.push(*byte);
            loop {
                match self.next_sequence_length() {
                    Ok(Some(length)) => {
                        let sequence: Vec<u8> = self.buffer.drain(..length).collect();
                        results.push(decode(&sequence, self.max_paste_bytes));
                    }
                    Ok(None) => break,
                    Err(error) => {
                        if matches!(error, InputError::PasteLimit)
                            && !self.buffer.ends_with(b"\x1b[201~")
                        {
                            self.discarding_paste = true;
                            self.discard_match = 0;
                        }
                        self.buffer.clear();
                        results.push(Err(error));
                        break;
                    }
                }
            }
        }
        results
    }

    pub fn finish(&mut self) -> Vec<Result<InputEvent, InputError>> {
        self.finish_map(decode_input)
    }

    /// Finishes the current bounded byte stream with wire-family provenance.
    pub fn finish_tagged(&mut self) -> Vec<Result<DecodedInputEvent, InputError>> {
        self.finish_map(decode_input_with_protocol)
    }

    fn finish_map<T, F>(&mut self, mut decode: F) -> Vec<Result<T, InputError>>
    where
        F: FnMut(&[u8], usize) -> Result<T, InputError>,
    {
        if self.discarding_paste {
            self.discarding_paste = false;
            self.discard_match = 0;
            self.buffer.clear();
            return vec![Err(InputError::UnknownSequence)];
        }
        if self.buffer.is_empty() {
            return Vec::new();
        }
        let sequence = std::mem::take(&mut self.buffer);
        if sequence == b"\x1b" {
            vec![decode(&sequence, self.max_paste_bytes)]
        } else {
            vec![Err(InputError::UnknownSequence)]
        }
    }

    pub fn reset(&mut self) {
        self.buffer.clear();
        self.discarding_paste = false;
        self.discard_match = 0;
    }

    pub fn pending_bytes(&self) -> usize {
        self.buffer.len()
    }

    fn consume_discarded_paste_byte(&mut self, byte: u8) {
        const END: &[u8] = b"\x1b[201~";
        let expected = END.get(self.discard_match).copied();
        if expected == Some(byte) {
            self.discard_match += 1;
            if self.discard_match == END.len() {
                self.discarding_paste = false;
                self.discard_match = 0;
            }
        } else {
            self.discard_match = usize::from(END.first().copied() == Some(byte));
        }
    }

    fn next_sequence_length(&self) -> Result<Option<usize>, InputError> {
        if self.buffer.is_empty() {
            return Ok(None);
        }
        if self.buffer.starts_with(b"\x1b[200~") {
            if let Some(index) = find_subslice(&self.buffer[6..], b"\x1b[201~") {
                let content_len = index;
                if content_len > self.max_paste_bytes {
                    return Err(InputError::PasteLimit);
                }
                return Ok(Some(
                    6usize
                        .checked_add(index)
                        .and_then(|value| value.checked_add(6))
                        .ok_or(InputError::SequenceLimit)?,
                ));
            }
            let maximum = self
                .max_paste_bytes
                .checked_add(12)
                .ok_or(InputError::SequenceLimit)?;
            if self.buffer.len() > maximum {
                return Err(InputError::PasteLimit);
            }
            return Ok(None);
        }

        let first = self.buffer[0];
        if first != 0x1b {
            let length = utf8_sequence_length(first)?;
            if length > self.max_sequence_bytes {
                return Err(InputError::SequenceLimit);
            }
            return Ok((self.buffer.len() >= length).then_some(length));
        }
        if self.buffer.len() == 1 {
            return if self.max_sequence_bytes == 0 {
                Err(InputError::SequenceLimit)
            } else {
                Ok(None)
            };
        }
        if self.buffer[1] == b'[' {
            if let Some(index) = self.buffer[2..]
                .iter()
                .position(|byte| (0x40..=0x7e).contains(byte))
            {
                let length = index.checked_add(3).ok_or(InputError::SequenceLimit)?;
                if length > self.max_sequence_bytes {
                    return Err(InputError::SequenceLimit);
                }
                return Ok(Some(length));
            }
            if self.buffer.len() > self.max_sequence_bytes {
                return Err(InputError::SequenceLimit);
            }
            return Ok(None);
        }
        if self.buffer[1] == b'O' {
            if 3 > self.max_sequence_bytes {
                return Err(InputError::SequenceLimit);
            }
            return Ok((self.buffer.len() >= 3).then_some(3));
        }
        let character_length = utf8_sequence_length(self.buffer[1])?;
        let length = character_length
            .checked_add(1)
            .ok_or(InputError::SequenceLimit)?;
        if length > self.max_sequence_bytes {
            return Err(InputError::SequenceLimit);
        }
        Ok((self.buffer.len() >= length).then_some(length))
    }
}

fn utf8_sequence_length(first: u8) -> Result<usize, InputError> {
    match first {
        0x00..=0x7f => Ok(1),
        0xc2..=0xdf => Ok(2),
        0xe0..=0xef => Ok(3),
        0xf0..=0xf4 => Ok(4),
        _ => Err(InputError::InvalidUtf8),
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn decode_sgr_mouse(sequence: &[u8]) -> Result<InputEvent, InputError> {
    let text = std::str::from_utf8(sequence).map_err(|_| InputError::InvalidUtf8)?;
    let pressed = text.ends_with('M');
    if !pressed && !text.ends_with('m') {
        return Err(InputError::UnknownSequence);
    }
    let payload = text
        .strip_prefix("\u{1b}[<")
        .and_then(|value| value.strip_suffix(['M', 'm']))
        .ok_or(InputError::UnknownSequence)?;
    let mut parts = payload.split(';');
    let encoded = parts
        .next()
        .ok_or(InputError::UnknownSequence)?
        .parse::<u16>()
        .map_err(|_| InputError::UnknownSequence)?;
    let column = parts
        .next()
        .ok_or(InputError::InvalidCoordinate)?
        .parse::<u16>()
        .map_err(|_| InputError::InvalidCoordinate)?
        .checked_sub(1)
        .ok_or(InputError::InvalidCoordinate)?;
    let row = parts
        .next()
        .ok_or(InputError::InvalidCoordinate)?
        .parse::<u16>()
        .map_err(|_| InputError::InvalidCoordinate)?
        .checked_sub(1)
        .ok_or(InputError::InvalidCoordinate)?;
    if parts.next().is_some() {
        return Err(InputError::UnknownSequence);
    }
    let mut modifiers = Modifiers::empty();
    if encoded & 4 != 0 {
        modifiers |= Modifiers::SHIFT;
    }
    if encoded & 8 != 0 {
        modifiers |= Modifiers::ALT;
    }
    if encoded & 16 != 0 {
        modifiers |= Modifiers::CONTROL;
    }
    let button = if encoded & 64 != 0 {
        if encoded & 1 == 0 {
            MouseButton::WheelUp
        } else {
            MouseButton::WheelDown
        }
    } else {
        match encoded & 3 {
            0 => MouseButton::Left,
            1 => MouseButton::Middle,
            2 => MouseButton::Right,
            _ => MouseButton::None,
        }
    };
    Ok(InputEvent::Mouse {
        button,
        column,
        row,
        pressed,
        modifiers,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::error::Error;

    use super::{
        Backend, Cell, CellWidth, Color, InputDecoder, InputError, InputEvent, KeyCode,
        KeyEventKind, Modifiers, Projection, ProjectionLimits, Rgba, RgbaSurface, Surface,
        SurfaceError, TerminalBackendFeedback, TerminalCapabilities, TerminalOp, decode_input,
        diff, encode_operations, encode_operations_bounded, project_rgba, project_rgba_with_limits,
    };
    use tg_core::Cancellation;

    #[test]
    fn capability_selection_stays_with_cell_renderers() {
        let truecolor = HashMap::from([
            ("TERM".to_owned(), "xterm-256color".to_owned()),
            ("COLORTERM".to_owned(), "truecolor".to_owned()),
        ]);
        let capabilities = TerminalCapabilities::from_environment(&truecolor);
        assert_eq!(
            capabilities.select_backend(Backend::Auto),
            Backend::Halfblock
        );

        let ansi = HashMap::from([("TERM".to_owned(), "xterm-256color".to_owned())]);
        let capabilities = TerminalCapabilities::from_environment(&ansi);
        assert_eq!(capabilities.select_backend(Backend::Auto), Backend::Cells);

        let dumb = HashMap::from([("TERM".to_owned(), "dumb".to_owned())]);
        let capabilities = TerminalCapabilities::from_environment(&dumb);
        assert_eq!(capabilities.select_backend(Backend::Auto), Backend::Text);
    }

    #[test]
    fn safe_capability_selection_degrades_one_step() {
        let capabilities = TerminalCapabilities {
            color: super::ColorLevel::TrueColor,
            dumb: false,
        };

        assert!(capabilities.supports_backend(Backend::Cells));
        assert!(capabilities.supports_backend(Backend::Halfblock));
        assert!(capabilities.supports_backend(Backend::Quadrant));
        assert!(capabilities.supports_backend(Backend::Braille));
        assert_eq!(
            capabilities.select_safe_backend(Backend::Braille, TerminalBackendFeedback::Initial,),
            Backend::Braille
        );
        assert_eq!(
            capabilities.select_safe_backend(
                Backend::Halfblock,
                TerminalBackendFeedback::UnsupportedCapability,
            ),
            Backend::Cells
        );
        assert_eq!(
            capabilities.select_safe_backend(
                Backend::Halfblock,
                TerminalBackendFeedback::OutputQuotaExceeded,
            ),
            Backend::Cells
        );
        assert_eq!(
            capabilities
                .select_safe_backend(Backend::Cells, TerminalBackendFeedback::Backpressured,),
            Backend::Text
        );

        let dumb = TerminalCapabilities {
            color: super::ColorLevel::Monochrome,
            dumb: true,
        };
        assert_eq!(
            dumb.select_safe_backend(Backend::Cells, TerminalBackendFeedback::Initial),
            Backend::Cells
        );
    }

    #[test]
    fn web_content_cannot_inject_escape_sequences() -> Result<(), Box<dyn Error>> {
        assert!(Cell::new("\u{1b}").is_err());
        let operations = [super::TerminalOp::Write("safe\u{1b}[31m".to_owned())];
        let encoded = encode_operations(&operations)?;
        assert_eq!(encoded, "safe�[31m".as_bytes());
        Ok(())
    }

    #[test]
    fn halfblock_projection_preserves_top_and_bottom_colors() -> Result<(), Box<dyn Error>> {
        let rgba = RgbaSurface::new(
            1,
            2,
            vec![
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
            ],
        )?;
        let Projection::Cells(surface) = project_rgba(&rgba, Backend::Halfblock, 1, 1)?;
        let cell = surface.get(0, 0).ok_or("missing cell")?;
        assert_eq!(cell.foreground, Color::Rgb(255, 0, 0));
        assert_eq!(cell.background, Color::Rgb(0, 0, 255));
        Ok(())
    }

    #[test]
    fn cell_projection_area_averages_source_pixels() -> Result<(), Box<dyn Error>> {
        let rgba = RgbaSurface::new(
            2,
            1,
            vec![
                Rgba {
                    red: 0,
                    green: 0,
                    blue: 0,
                    alpha: 255,
                },
                Rgba {
                    red: 255,
                    green: 255,
                    blue: 255,
                    alpha: 255,
                },
            ],
        )?;
        let Projection::Cells(surface) = project_rgba(&rgba, Backend::Cells, 1, 1)?;
        assert_eq!(
            surface.get(0, 0).map(|cell| cell.background),
            Some(Color::Rgb(127, 127, 127))
        );
        Ok(())
    }

    #[test]
    fn quadrant_projection_uses_solid_detail_over_majority_background() -> Result<(), Box<dyn Error>>
    {
        let white = Rgba {
            red: 255,
            green: 255,
            blue: 255,
            alpha: 255,
        };
        let black = Rgba {
            red: 0,
            green: 0,
            blue: 0,
            alpha: 255,
        };
        let mut pixels = vec![white; 8];
        pixels[0] = black;
        pixels[2] = black;
        let rgba = RgbaSurface::new(2, 4, pixels)?;
        let Projection::Cells(surface) = project_rgba(&rgba, Backend::Quadrant, 1, 1)?;
        let cell = surface.get(0, 0).ok_or("missing cell")?;
        assert_eq!(cell.grapheme, "▘");
        assert_eq!(cell.foreground, Color::Rgb(0, 0, 0));
        assert_eq!(cell.background, Color::Rgb(255, 255, 255));
        Ok(())
    }

    #[test]
    fn braille_projection_preserves_background_and_isolates_fine_detail()
    -> Result<(), Box<dyn Error>> {
        let white = Rgba {
            red: 255,
            green: 255,
            blue: 255,
            alpha: 255,
        };
        let black = Rgba {
            red: 0,
            green: 0,
            blue: 0,
            alpha: 255,
        };
        let mut pixels = vec![white; 8];
        pixels[0] = black;
        let rgba = RgbaSurface::new(2, 4, pixels)?;
        let Projection::Cells(surface) = project_rgba(&rgba, Backend::Braille, 1, 1)?;
        let cell = surface.get(0, 0).ok_or("missing cell")?;
        assert_eq!(cell.grapheme, "\u{2801}");
        assert_eq!(cell.foreground, Color::Rgb(0, 0, 0));
        assert_eq!(cell.background, Color::Rgb(255, 255, 255));

        let rgba = RgbaSurface::new(2, 4, vec![white; 8])?;
        let Projection::Cells(surface) = project_rgba(&rgba, Backend::Braille, 1, 1)?;
        let cell = surface.get(0, 0).ok_or("missing cell")?;
        assert_eq!(cell.grapheme, "\u{2800}");
        assert_eq!(cell.foreground, Color::Rgb(255, 255, 255));
        assert_eq!(cell.background, Color::Rgb(255, 255, 255));
        Ok(())
    }

    #[test]
    fn unchanged_surfaces_produce_no_terminal_bytes() -> Result<(), Box<dyn Error>> {
        let surface = Surface::new(2, 1, 2)?;
        let operations = diff(&surface, &surface)?;
        assert!(operations.is_empty());
        assert!(encode_operations(&operations)?.is_empty());
        Ok(())
    }

    #[test]
    fn sgr_mouse_and_bounded_paste_are_normalized() -> Result<(), Box<dyn Error>> {
        let event = decode_input(b"\x1b[<0;3;4M", 100)?;
        assert!(matches!(
            event,
            InputEvent::Mouse {
                column: 2,
                row: 3,
                pressed: true,
                ..
            }
        ));
        assert!(decode_input(b"\x1b[200~oversized\x1b[201~", 2).is_err());
        Ok(())
    }

    #[test]
    fn wide_cells_use_non_emitting_continuations_and_validate_atomically()
    -> Result<(), Box<dyn Error>> {
        let previous = Surface::new(4, 1, 4)?;
        let mut next = previous.clone();
        next.put_cell(0, 0, Cell::new("한")?)?;
        next.put_cell(2, 0, Cell::new("x")?)?;
        next.validate()?;
        assert_eq!(
            next.get(1, 0).ok_or("missing continuation")?.width,
            CellWidth::Zero
        );
        let bytes = encode_operations(&diff(&previous, &next)?)?;
        let rendered = String::from_utf8(bytes)?;
        assert!(rendered.contains("한x"));
        assert!(!rendered.contains("한 x"));

        let mut corrupt = Surface::new(2, 1, 2)?;
        corrupt.set(0, 0, Cell::new("한")?)?;
        assert!(matches!(
            diff(&previous, &corrupt),
            Err(SurfaceError::InvalidCellLayout)
        ));
        assert!(matches!(
            next.put_cell(3, 0, Cell::new("界")?),
            Err(SurfaceError::WideCellAtEdge)
        ));
        next.validate()?;
        Ok(())
    }

    #[test]
    fn resize_always_clears_stale_terminal_content() -> Result<(), Box<dyn Error>> {
        let mut previous = Surface::new(2, 1, 2)?;
        previous.put_cell(0, 0, Cell::new("x")?)?;
        let next = Surface::new(3, 1, 3)?;
        let operations = diff(&previous, &next)?;
        assert!(matches!(operations.first(), Some(TerminalOp::ClearScreen)));
        assert!(encode_operations(&operations)?.starts_with(b"\x1b[2J\x1b[H"));
        Ok(())
    }

    #[test]
    fn every_raster_backend_is_deterministic_and_resource_bounded() -> Result<(), Box<dyn Error>> {
        let pixels = vec![
            Rgba {
                red: 255,
                green: 0,
                blue: 0,
                alpha: 255,
            },
            Rgba {
                red: 0,
                green: 255,
                blue: 0,
                alpha: 255,
            },
            Rgba {
                red: 0,
                green: 0,
                blue: 255,
                alpha: 255,
            },
            Rgba {
                red: 255,
                green: 255,
                blue: 255,
                alpha: 255,
            },
        ];
        let rgba = RgbaSurface::new(2, 2, pixels)?;
        for backend in [
            Backend::Cells,
            Backend::Halfblock,
            Backend::Quadrant,
            Backend::Braille,
        ] {
            let first = project_rgba(&rgba, backend, 2, 1)?;
            let second = project_rgba(&rgba, backend, 2, 1)?;
            assert_eq!(first, second);
            assert!(matches!(first, Projection::Cells(_)));
        }
        let limits = ProjectionLimits {
            max_cells: 2,
            max_pixels: 3,
            max_output_bytes: 8,
        };
        assert!(matches!(
            project_rgba_with_limits(&rgba, Backend::Cells, 2, 1, limits),
            Err(SurfaceError::PixelLimit)
        ));
        Ok(())
    }

    #[test]
    fn terminal_output_limit_fails_before_exposing_partial_bytes() -> Result<(), Box<dyn Error>> {
        let operations = [TerminalOp::Write("safe".to_owned())];
        assert!(matches!(
            encode_operations_bounded(&operations, 3),
            Err(SurfaceError::OutputLimit)
        ));
        assert_eq!(encode_operations_bounded(&operations, 4)?, b"safe");
        Ok(())
    }

    #[test]
    fn kitty_keyboard_and_chunked_input_preserve_event_identity() -> Result<(), Box<dyn Error>> {
        let event = decode_input(b"\x1b[97:65:99;6:2;65u", 64)?;
        assert!(matches!(
            event,
            InputEvent::Key {
                code: KeyCode::Character('a'),
                modifiers,
                kind: KeyEventKind::Repeat,
                shifted_key: Some('A'),
                base_layout_key: Some('c'),
                text: Some(ref text),
            } if modifiers == (Modifiers::SHIFT | Modifiers::CONTROL) && text == "A"
        ));

        let mut decoder = InputDecoder::new(64, 16);
        assert!(decoder.push(b"\x1b[200~\xed").is_empty());
        assert!(decoder.push(b"\x95").is_empty());
        let events = decoder.push(b"\x9c\x1b[201~z");
        assert!(matches!(
            events.as_slice(),
            [
                Ok(InputEvent::Paste(value)),
                Ok(InputEvent::Key {
                    code: KeyCode::Character('z'),
                    ..
                })
            ] if value == "한"
        ));
        assert_eq!(decoder.pending_bytes(), 0);
        Ok(())
    }

    #[test]
    fn input_decoder_recovers_after_limits_corruption_and_cancellation() {
        let mut decoder = InputDecoder::new(32, 2);
        let results = decoder.push(b"\x1b[200~oversized\x1b[201~x");
        assert!(matches!(results.first(), Some(Err(InputError::PasteLimit))));
        assert!(matches!(
            results.last(),
            Some(Ok(InputEvent::Key {
                code: KeyCode::Character('x'),
                ..
            }))
        ));
        assert_eq!(decoder.pending_bytes(), 0);

        let cancellation = Cancellation::new();
        cancellation.cancel();
        let results = decoder.push_with_cancellation(b"y", &cancellation);
        assert!(matches!(results.as_slice(), [Err(InputError::Cancelled)]));
        assert_eq!(decoder.pending_bytes(), 0);

        for byte in 0u8..=u8::MAX {
            let mut fuzz_decoder = InputDecoder::new(16, 16);
            let _results = fuzz_decoder.push(&[byte]);
            let _tail = fuzz_decoder.finish();
            assert_eq!(fuzz_decoder.pending_bytes(), 0);
        }
    }

    #[test]
    fn capability_matrix_handles_color_overrides_and_explicit_backends() {
        let truecolor = HashMap::from([
            ("TERM".to_owned(), "xterm-256color".to_owned()),
            ("COLORTERM".to_owned(), "truecolor".to_owned()),
        ]);
        let capabilities = TerminalCapabilities::from_environment(&truecolor);
        assert_eq!(
            capabilities.select_backend(Backend::Auto),
            Backend::Halfblock
        );

        let no_color = HashMap::from([
            ("TERM".to_owned(), "xterm-256color".to_owned()),
            ("COLORTERM".to_owned(), "truecolor".to_owned()),
            ("NO_COLOR".to_owned(), String::new()),
        ]);
        let capabilities = TerminalCapabilities::from_environment(&no_color);
        assert_eq!(capabilities.select_backend(Backend::Auto), Backend::Cells);

        for backend in [
            Backend::Text,
            Backend::Cells,
            Backend::Halfblock,
            Backend::Quadrant,
            Backend::Braille,
        ] {
            assert_eq!(capabilities.select_backend(backend), backend);
        }
    }
}
