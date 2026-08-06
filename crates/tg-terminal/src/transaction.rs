//! Receipt-bound cell-diff publication for caller-owned terminal writers.

use std::io::{ErrorKind, Write};
use std::sync::Arc;
use std::time::Duration;

use tg_core::{Cancellation, Clock, SystemClock};
use thiserror::Error;

use crate::{
    Backend, Color, ColorLevel, Surface, SurfaceError, TerminalBackendFeedback,
    TerminalCapabilities, TerminalOp, diff, encode_operations_bounded,
};

const CHECKPOINT_INTERVAL: usize = 1_024;
const ANSI16_PALETTE: [(u8, u8, u8); 16] = [
    (0, 0, 0),
    (205, 0, 0),
    (0, 205, 0),
    (205, 205, 0),
    (0, 0, 238),
    (205, 0, 205),
    (0, 205, 205),
    (229, 229, 229),
    (127, 127, 127),
    (255, 0, 0),
    (0, 255, 0),
    (255, 255, 0),
    (92, 92, 255),
    (255, 0, 255),
    (0, 255, 255),
    (255, 255, 255),
];
const ANSI256_CUBE: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// Immutable identity of a caller-owned terminal writer.
///
/// A generation change means the previous terminal contents cannot be used as a diff baseline.
/// The transaction therefore forces one full redraw after a successful rebind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TerminalWriterId {
    id: u64,
    generation: u64,
}

impl TerminalWriterId {
    pub const fn new(id: u64, generation: u64) -> Self {
        Self { id, generation }
    }

    pub const fn id(self) -> u64 {
        self.id
    }

    pub const fn generation(self) -> u64 {
        self.generation
    }

    fn validate(self) -> Result<(), TerminalTransactionError> {
        if self.id == 0 || self.generation == 0 {
            Err(TerminalTransactionError::InvalidWriterId)
        } else {
            Ok(())
        }
    }
}

/// Physical metadata for a canonical cell surface.
///
/// The encoder never converts this metadata into device I/O. It exists so a producer cannot
/// claim a finite terminal viewport while passing malformed logical cell geometry to the public
/// transaction boundary.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TerminalFrameGeometry {
    pub columns: u16,
    pub rows: u16,
    pub cell_width: f32,
    pub cell_height: f32,
}

impl TerminalFrameGeometry {
    pub const fn for_surface(surface: &Surface) -> Self {
        Self {
            columns: surface.columns(),
            rows: surface.rows(),
            cell_width: 1.0,
            cell_height: 1.0,
        }
    }

    /// Validates physical metadata before it crosses a terminal boundary.
    pub fn validate(self) -> Result<(), TerminalTransactionError> {
        if self.columns == 0
            || self.rows == 0
            || !self.cell_width.is_finite()
            || !self.cell_height.is_finite()
            || self.cell_width <= 0.0
            || self.cell_height <= 0.0
        {
            return Err(TerminalTransactionError::InvalidGeometry);
        }
        let logical_width = self.cell_width * f32::from(self.columns);
        let logical_height = self.cell_height * f32::from(self.rows);
        if !logical_width.is_finite()
            || !logical_height.is_finite()
            || logical_width <= 0.0
            || logical_height <= 0.0
        {
            return Err(TerminalTransactionError::InvalidGeometry);
        }
        Ok(())
    }
}

/// Bounded work and storage limits for one terminal cell transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalTransactionLimits {
    pub max_cells: usize,
    pub max_surface_bytes: usize,
    pub max_operations: usize,
    pub max_output_bytes: usize,
}

impl TerminalTransactionLimits {
    pub const BROWSER_DEFAULT: Self = Self {
        max_cells: 1_000_000,
        max_surface_bytes: 64 * 1024 * 1024,
        max_operations: 3_000_000,
        max_output_bytes: 64 * 1024 * 1024,
    };

    fn validate(self) -> Result<(), TerminalTransactionError> {
        if self.max_cells == 0
            || self.max_surface_bytes == 0
            || self.max_operations == 0
            || self.max_output_bytes == 0
        {
            Err(TerminalTransactionError::InvalidLimits)
        } else {
            Ok(())
        }
    }
}

impl Default for TerminalTransactionLimits {
    fn default() -> Self {
        Self::BROWSER_DEFAULT
    }
}

/// Color representation used by the bounded cell encoder after capability negotiation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalColorMode {
    Monochrome,
    Ansi16,
    Ansi256,
    TrueColor,
}

impl TerminalColorMode {
    fn from_capabilities(capabilities: TerminalCapabilities) -> Self {
        if capabilities.dumb {
            return Self::Monochrome;
        }
        match capabilities.color {
            ColorLevel::Monochrome => Self::Monochrome,
            ColorLevel::Ansi16 => Self::Ansi16,
            ColorLevel::Ansi256 => Self::Ansi256,
            ColorLevel::TrueColor => Self::TrueColor,
        }
    }
}

/// Immutable producer inputs for one canonical cell-diff transaction.
pub struct TerminalTransactionRequest<'a> {
    pub surface: &'a Surface,
    pub geometry: TerminalFrameGeometry,
    pub capabilities: TerminalCapabilities,
    pub requested_backend: Backend,
    pub feedback: TerminalBackendFeedback,
}

impl<'a> TerminalTransactionRequest<'a> {
    pub fn new(
        surface: &'a Surface,
        capabilities: TerminalCapabilities,
        requested_backend: Backend,
    ) -> Self {
        Self {
            surface,
            geometry: TerminalFrameGeometry::for_surface(surface),
            capabilities,
            requested_backend,
            feedback: TerminalBackendFeedback::Initial,
        }
    }
}

/// Cancellation and deadline controls for a terminal transaction.
pub struct TerminalTransactionControl<'a> {
    cancellation: &'a Cancellation,
    clock: &'a dyn Clock,
    deadline: Option<Duration>,
}

impl<'a> TerminalTransactionControl<'a> {
    pub const fn new(
        cancellation: &'a Cancellation,
        clock: &'a dyn Clock,
        deadline: Option<Duration>,
    ) -> Self {
        Self {
            cancellation,
            clock,
            deadline,
        }
    }

    pub fn checkpoint(&self) -> Result<(), TerminalTransactionError> {
        self.now_checked().map(|_| ())
    }

    /// Returns the cancellation authority shared by one composed terminal transaction.
    pub const fn cancellation(&self) -> &Cancellation {
        self.cancellation
    }

    /// Returns the monotonic clock shared by one composed terminal transaction.
    pub fn clock(&self) -> &dyn Clock {
        self.clock
    }

    /// Returns the optional deadline shared by one composed terminal transaction.
    pub const fn deadline(&self) -> Option<Duration> {
        self.deadline
    }

    /// Returns the caller clock after enforcing this transaction's cancellation and deadline.
    pub fn now_checked(&self) -> Result<Duration, TerminalTransactionError> {
        if self.cancellation.is_cancelled() {
            return Err(TerminalTransactionError::Cancelled);
        }
        let now = self.clock.monotonic_now();
        if self.deadline.is_some_and(|deadline| now >= deadline) {
            return Err(TerminalTransactionError::DeadlineExceeded);
        }
        Ok(now)
    }
}

/// Canonical cell diff and bounded terminal bytes awaiting one exact writer receipt.
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalOutputFrame {
    sequence: u64,
    writer: TerminalWriterId,
    requested_backend: Backend,
    applied_backend: Backend,
    color_mode: TerminalColorMode,
    geometry: TerminalFrameGeometry,
    surface: Arc<Surface>,
    operations: Arc<[TerminalOp]>,
    output: Arc<[u8]>,
    changed_cells: usize,
    prepared_at: Duration,
}

impl TerminalOutputFrame {
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    pub const fn writer(&self) -> TerminalWriterId {
        self.writer
    }

    pub const fn requested_backend(&self) -> Backend {
        self.requested_backend
    }

    pub const fn applied_backend(&self) -> Backend {
        self.applied_backend
    }

    pub const fn color_mode(&self) -> TerminalColorMode {
        self.color_mode
    }

    pub const fn geometry(&self) -> TerminalFrameGeometry {
        self.geometry
    }

    pub fn surface(&self) -> &Surface {
        &self.surface
    }

    pub fn operations(&self) -> &[TerminalOp] {
        &self.operations
    }

    pub fn output(&self) -> &[u8] {
        &self.output
    }

    pub fn output_bytes(&self) -> usize {
        self.output.len()
    }

    pub const fn changed_cells(&self) -> usize {
        self.changed_cells
    }

    pub const fn prepared_at(&self) -> Duration {
        self.prepared_at
    }
}

/// Outcome attested by a caller-owned terminal writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalWriteReceiptOutcome {
    FullWriteAndFlush,
    PartialWrite,
    WriteFailed,
    FlushFailed,
    Lost,
}

/// Exact writer acknowledgement for one [`TerminalOutputFrame`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalWriteReceipt {
    sequence: u64,
    writer: TerminalWriterId,
    output_bytes: usize,
    written_bytes: usize,
    outcome: TerminalWriteReceiptOutcome,
    completed_at: Duration,
    flush_duration: Duration,
}

impl TerminalWriteReceipt {
    pub fn full_write_and_flush(
        sequence: u64,
        writer: TerminalWriterId,
        output_bytes: usize,
        completed_at: Duration,
        flush_duration: Duration,
    ) -> Self {
        Self {
            sequence,
            writer,
            output_bytes,
            written_bytes: output_bytes,
            outcome: TerminalWriteReceiptOutcome::FullWriteAndFlush,
            completed_at,
            flush_duration,
        }
    }

    pub fn partial_write(
        sequence: u64,
        writer: TerminalWriterId,
        output_bytes: usize,
        written_bytes: usize,
        completed_at: Duration,
    ) -> Self {
        Self {
            sequence,
            writer,
            output_bytes,
            written_bytes,
            outcome: TerminalWriteReceiptOutcome::PartialWrite,
            completed_at,
            flush_duration: Duration::ZERO,
        }
    }

    pub fn write_failed(
        sequence: u64,
        writer: TerminalWriterId,
        output_bytes: usize,
        completed_at: Duration,
    ) -> Self {
        Self {
            sequence,
            writer,
            output_bytes,
            written_bytes: 0,
            outcome: TerminalWriteReceiptOutcome::WriteFailed,
            completed_at,
            flush_duration: Duration::ZERO,
        }
    }

    pub fn flush_failed(
        sequence: u64,
        writer: TerminalWriterId,
        output_bytes: usize,
        completed_at: Duration,
        flush_duration: Duration,
    ) -> Self {
        Self {
            sequence,
            writer,
            output_bytes,
            written_bytes: output_bytes,
            outcome: TerminalWriteReceiptOutcome::FlushFailed,
            completed_at,
            flush_duration,
        }
    }

    pub fn lost(
        sequence: u64,
        writer: TerminalWriterId,
        output_bytes: usize,
        completed_at: Duration,
    ) -> Self {
        Self {
            sequence,
            writer,
            output_bytes,
            written_bytes: 0,
            outcome: TerminalWriteReceiptOutcome::Lost,
            completed_at,
            flush_duration: Duration::ZERO,
        }
    }

    pub const fn sequence(self) -> u64 {
        self.sequence
    }

    pub const fn writer(self) -> TerminalWriterId {
        self.writer
    }

    pub const fn output_bytes(self) -> usize {
        self.output_bytes
    }

    pub const fn written_bytes(self) -> usize {
        self.written_bytes
    }

    pub const fn outcome(self) -> TerminalWriteReceiptOutcome {
        self.outcome
    }

    pub const fn completed_at(self) -> Duration {
        self.completed_at
    }

    pub const fn flush_duration(self) -> Duration {
        self.flush_duration
    }

    fn is_full_write_and_flush(self) -> bool {
        matches!(self.outcome, TerminalWriteReceiptOutcome::FullWriteAndFlush)
            && self.written_bytes == self.output_bytes
    }
}

/// Successful settlement of one terminal output frame and its exact receipt.
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalTransactionResult {
    frame: TerminalOutputFrame,
    receipt: TerminalWriteReceipt,
}

impl TerminalTransactionResult {
    pub fn frame(&self) -> &TerminalOutputFrame {
        &self.frame
    }

    pub const fn receipt(&self) -> TerminalWriteReceipt {
        self.receipt
    }
}

/// Errors that leave terminal writer and scene ownership with the caller.
#[derive(Debug, Error)]
pub enum TerminalTransactionError {
    #[error(transparent)]
    Surface(#[from] SurfaceError),
    #[error("terminal writer identity must have non-zero id and generation")]
    InvalidWriterId,
    #[error("terminal frame geometry is malformed or non-finite")]
    InvalidGeometry,
    #[error("terminal transaction limits must be non-zero")]
    InvalidLimits,
    #[error("terminal transaction exceeded its cell or operation quota")]
    ItemLimit,
    #[error("terminal transaction exceeded its bounded allocation quota")]
    ResourceLimit,
    #[error("terminal transaction exceeded its bounded output quota")]
    OutputLimit,
    #[error("terminal transaction sequence space is exhausted")]
    SequenceExhausted,
    #[error("terminal transaction was cancelled")]
    Cancelled,
    #[error("terminal transaction deadline elapsed")]
    DeadlineExceeded,
    #[error("terminal writer clock moved backwards")]
    ClockMovedBackward,
    #[error("terminal transaction expected writer {expected:?}, received {received:?}")]
    StaleWriter {
        expected: TerminalWriterId,
        received: TerminalWriterId,
    },
    #[error("terminal transaction is awaiting receipt for sequence {sequence}")]
    PendingReceipt { sequence: u64 },
    #[error("terminal transaction has no pending receipt")]
    ReceiptUnavailable,
    #[error("terminal receipt for sequence {sequence} was already settled")]
    ReceiptReplay { sequence: u64 },
    #[error("terminal receipt sequence mismatch: expected {expected}, received {received}")]
    ReceiptSequenceMismatch { expected: u64, received: u64 },
    #[error("terminal receipt output mismatch: expected {expected}, received {received}")]
    ReceiptOutputMismatch { expected: usize, received: usize },
    #[error("terminal receipt completion precedes frame preparation")]
    ReceiptTimestampOutOfOrder,
    #[error("terminal receipt wrote {written} of {expected} bytes")]
    PartialWrite { expected: usize, written: usize },
    #[error("terminal writer failed with {kind:?}")]
    WriteFailed { kind: ErrorKind },
    #[error("terminal writer flush failed with {kind:?}")]
    FlushFailed { kind: ErrorKind },
    #[error("terminal writer receipt was not recoverably acknowledged")]
    WriteLost,
}

/// Stateful last-known-good transaction owner for one terminal writer generation.
pub struct TerminalTransaction {
    writer: TerminalWriterId,
    limits: TerminalTransactionLimits,
    system_clock: Arc<SystemClock>,
    next_sequence: u64,
    baseline: Option<Arc<Surface>>,
    last_good: Option<TerminalOutputFrame>,
    last_receipt: Option<TerminalWriteReceipt>,
    pending: Option<TerminalOutputFrame>,
}

impl TerminalTransaction {
    pub fn new(
        writer: TerminalWriterId,
        limits: TerminalTransactionLimits,
    ) -> Result<Self, TerminalTransactionError> {
        writer.validate()?;
        limits.validate()?;
        Ok(Self {
            writer,
            limits,
            system_clock: Arc::new(SystemClock::default()),
            next_sequence: 1,
            baseline: None,
            last_good: None,
            last_receipt: None,
            pending: None,
        })
    }

    pub const fn writer(&self) -> TerminalWriterId {
        self.writer
    }

    pub const fn limits(&self) -> TerminalTransactionLimits {
        self.limits
    }

    pub fn last_good(&self) -> Option<&TerminalOutputFrame> {
        self.last_good.as_ref()
    }

    pub const fn last_receipt(&self) -> Option<TerminalWriteReceipt> {
        self.last_receipt
    }

    pub fn pending_frame(&self) -> Option<&TerminalOutputFrame> {
        self.pending.as_ref()
    }

    /// Invalidates only the retained terminal diff baseline.
    ///
    /// The last known-good frame remains available for recovery diagnostics. A caller cannot
    /// discard a baseline while an exact writer receipt is outstanding, so a scroll, resize, or
    /// transport reset cannot race a pending write into an incremental diff.
    pub fn invalidate_baseline(&mut self) -> Result<(), TerminalTransactionError> {
        if let Some(pending) = &self.pending {
            return Err(TerminalTransactionError::PendingReceipt {
                sequence: pending.sequence(),
            });
        }
        self.baseline = None;
        Ok(())
    }

    /// Validates one caller-owned terminal request before it is retained by a bounded transport.
    ///
    /// This intentionally does not reserve a transaction sequence or alter the current pending
    /// receipt. It lets a latest-frame transport keep at most one already-validated source
    /// surface while the transaction remains the sole diff, operation, byte, and receipt owner.
    pub fn validate_request_controlled(
        &self,
        writer: TerminalWriterId,
        request: &TerminalTransactionRequest<'_>,
        control: &TerminalTransactionControl<'_>,
    ) -> Result<(), TerminalTransactionError> {
        self.ensure_writer(writer)?;
        self.validate_request(request, control)
    }

    /// Binds a fresh caller-owned terminal writer and invalidates only its diff baseline.
    pub fn rebind_writer(
        &mut self,
        writer: TerminalWriterId,
    ) -> Result<(), TerminalTransactionError> {
        writer.validate()?;
        if let Some(pending) = &self.pending {
            return Err(TerminalTransactionError::PendingReceipt {
                sequence: pending.sequence(),
            });
        }
        if self.writer != writer {
            self.writer = writer;
            self.baseline = None;
        }
        Ok(())
    }

    pub fn prepare(
        &mut self,
        writer: TerminalWriterId,
        request: TerminalTransactionRequest<'_>,
    ) -> Result<TerminalOutputFrame, TerminalTransactionError> {
        let cancellation = Cancellation::new();
        let clock = Arc::clone(&self.system_clock);
        let control = TerminalTransactionControl::new(&cancellation, clock.as_ref(), None);
        self.prepare_controlled(writer, request, &control)
    }

    pub fn prepare_controlled(
        &mut self,
        writer: TerminalWriterId,
        request: TerminalTransactionRequest<'_>,
        control: &TerminalTransactionControl<'_>,
    ) -> Result<TerminalOutputFrame, TerminalTransactionError> {
        self.ensure_writer(writer)?;
        if let Some(pending) = &self.pending {
            return Err(TerminalTransactionError::PendingReceipt {
                sequence: pending.sequence(),
            });
        }
        self.validate_request(&request, control)?;
        let color_mode = TerminalColorMode::from_capabilities(request.capabilities);
        let surface = Arc::new(degrade_surface(request.surface, color_mode, control)?);
        let operations = self.operations_for(&surface)?;
        if operations.len() > self.limits.max_operations {
            return Err(TerminalTransactionError::ItemLimit);
        }
        control.checkpoint()?;
        let output = encode_operations_bounded(&operations, self.limits.max_output_bytes)
            .map_err(map_surface_error)?;
        if output.len() > self.limits.max_output_bytes {
            return Err(TerminalTransactionError::OutputLimit);
        }
        control.checkpoint()?;
        let prepared_at = control.now_checked()?;
        let sequence = self.next_sequence;
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or(TerminalTransactionError::SequenceExhausted)?;
        let frame = TerminalOutputFrame {
            sequence,
            writer,
            requested_backend: request.requested_backend,
            applied_backend: negotiated_cell_backend(
                request.capabilities,
                request.requested_backend,
                request.feedback,
            ),
            color_mode,
            geometry: request.geometry,
            changed_cells: self.changed_cells(&surface),
            surface,
            operations: operations.into(),
            output: output.into(),
            prepared_at,
        };
        self.pending = Some(frame.clone());
        Ok(frame)
    }

    pub fn acknowledge(
        &mut self,
        receipt: TerminalWriteReceipt,
    ) -> Result<TerminalTransactionResult, TerminalTransactionError> {
        let cancellation = Cancellation::new();
        let clock = Arc::clone(&self.system_clock);
        let control = TerminalTransactionControl::new(&cancellation, clock.as_ref(), None);
        self.acknowledge_controlled(receipt, &control)
    }

    pub fn acknowledge_controlled(
        &mut self,
        receipt: TerminalWriteReceipt,
        control: &TerminalTransactionControl<'_>,
    ) -> Result<TerminalTransactionResult, TerminalTransactionError> {
        control.checkpoint()?;
        self.settle_receipt(receipt)
    }

    pub fn write_pending<W: Write + ?Sized>(
        &mut self,
        writer: TerminalWriterId,
        output: &mut W,
    ) -> Result<TerminalTransactionResult, TerminalTransactionError> {
        let cancellation = Cancellation::new();
        let clock = Arc::clone(&self.system_clock);
        let control = TerminalTransactionControl::new(&cancellation, clock.as_ref(), None);
        self.write_pending_controlled(writer, output, &control)
    }

    pub fn write_pending_controlled<W: Write + ?Sized>(
        &mut self,
        writer: TerminalWriterId,
        output: &mut W,
        control: &TerminalTransactionControl<'_>,
    ) -> Result<TerminalTransactionResult, TerminalTransactionError> {
        self.ensure_writer(writer)?;
        let (sequence, output_bytes) = self.pending_identity()?;
        let before_write = match control.now_checked() {
            Ok(now) => now,
            Err(error) => {
                self.abandon_pending(TerminalWriteReceipt::lost(
                    sequence,
                    writer,
                    output_bytes,
                    Duration::ZERO,
                ));
                return Err(error);
            }
        };
        let write_result = {
            let frame = self
                .pending
                .as_ref()
                .ok_or(TerminalTransactionError::ReceiptUnavailable)?;
            output.write(frame.output())
        };
        let after_write = match control.now_checked() {
            Ok(now) if now >= before_write => now,
            Ok(_) => {
                self.abandon_pending(TerminalWriteReceipt::lost(
                    sequence,
                    writer,
                    output_bytes,
                    before_write,
                ));
                return Err(TerminalTransactionError::ClockMovedBackward);
            }
            Err(error) => {
                self.abandon_pending(TerminalWriteReceipt::lost(
                    sequence,
                    writer,
                    output_bytes,
                    before_write,
                ));
                return Err(error);
            }
        };
        match write_result {
            Err(error) => self
                .settle_receipt(TerminalWriteReceipt::write_failed(
                    sequence,
                    writer,
                    output_bytes,
                    after_write,
                ))
                .map_err(|settlement| match settlement {
                    TerminalTransactionError::WriteFailed { .. } => {
                        TerminalTransactionError::WriteFailed { kind: error.kind() }
                    }
                    other => other,
                }),
            Ok(written_bytes) if written_bytes != output_bytes => {
                self.settle_receipt(TerminalWriteReceipt::partial_write(
                    sequence,
                    writer,
                    output_bytes,
                    written_bytes,
                    after_write,
                ))
            }
            Ok(_) => {
                let flush_result = output.flush();
                let after_flush = match control.now_checked() {
                    Ok(now) if now >= after_write => now,
                    Ok(_) => {
                        self.abandon_pending(TerminalWriteReceipt::lost(
                            sequence,
                            writer,
                            output_bytes,
                            after_write,
                        ));
                        return Err(TerminalTransactionError::ClockMovedBackward);
                    }
                    Err(error) => {
                        self.abandon_pending(TerminalWriteReceipt::lost(
                            sequence,
                            writer,
                            output_bytes,
                            after_write,
                        ));
                        return Err(error);
                    }
                };
                let flush_duration = after_flush
                    .checked_sub(after_write)
                    .ok_or(TerminalTransactionError::ClockMovedBackward)?;
                match flush_result {
                    Ok(()) => self.settle_receipt(TerminalWriteReceipt::full_write_and_flush(
                        sequence,
                        writer,
                        output_bytes,
                        after_flush,
                        flush_duration,
                    )),
                    Err(error) => self
                        .settle_receipt(TerminalWriteReceipt::flush_failed(
                            sequence,
                            writer,
                            output_bytes,
                            after_flush,
                            flush_duration,
                        ))
                        .map_err(|settlement| match settlement {
                            TerminalTransactionError::FlushFailed { .. } => {
                                TerminalTransactionError::FlushFailed { kind: error.kind() }
                            }
                            other => other,
                        }),
                }
            }
        }
    }

    pub fn transact<W: Write + ?Sized>(
        &mut self,
        writer: TerminalWriterId,
        request: TerminalTransactionRequest<'_>,
        output: &mut W,
    ) -> Result<TerminalTransactionResult, TerminalTransactionError> {
        let cancellation = Cancellation::new();
        let clock = Arc::clone(&self.system_clock);
        let control = TerminalTransactionControl::new(&cancellation, clock.as_ref(), None);
        self.transact_controlled(writer, request, output, &control)
    }

    pub fn transact_controlled<W: Write + ?Sized>(
        &mut self,
        writer: TerminalWriterId,
        request: TerminalTransactionRequest<'_>,
        output: &mut W,
        control: &TerminalTransactionControl<'_>,
    ) -> Result<TerminalTransactionResult, TerminalTransactionError> {
        self.prepare_controlled(writer, request, control)?;
        self.write_pending_controlled(writer, output, control)
    }

    fn ensure_writer(&self, writer: TerminalWriterId) -> Result<(), TerminalTransactionError> {
        writer.validate()?;
        if writer != self.writer {
            return Err(TerminalTransactionError::StaleWriter {
                expected: self.writer,
                received: writer,
            });
        }
        Ok(())
    }

    fn validate_request(
        &self,
        request: &TerminalTransactionRequest<'_>,
        control: &TerminalTransactionControl<'_>,
    ) -> Result<(), TerminalTransactionError> {
        request.geometry.validate()?;
        if request.geometry.columns != request.surface.columns()
            || request.geometry.rows != request.surface.rows()
        {
            return Err(TerminalTransactionError::InvalidGeometry);
        }
        request.surface.validate()?;
        if request.surface.cells().len() > self.limits.max_cells {
            return Err(TerminalTransactionError::ItemLimit);
        }
        bounded_surface_bytes(request.surface, self.limits.max_surface_bytes, control)
    }

    fn operations_for(
        &self,
        surface: &Surface,
    ) -> Result<Vec<TerminalOp>, TerminalTransactionError> {
        match self.baseline.as_deref() {
            Some(previous)
                if previous.columns() == surface.columns() && previous.rows() == surface.rows() =>
            {
                diff(previous, surface).map_err(map_surface_error)
            }
            _ => {
                let blank = Surface::new(0, 0, 0).map_err(map_surface_error)?;
                diff(&blank, surface).map_err(map_surface_error)
            }
        }
    }

    fn changed_cells(&self, surface: &Surface) -> usize {
        let Some(previous) = self.baseline.as_deref() else {
            return surface.cells().len();
        };
        if previous.columns() != surface.columns() || previous.rows() != surface.rows() {
            return surface.cells().len();
        }
        previous
            .cells()
            .iter()
            .zip(surface.cells())
            .filter(|(before, after)| before != after)
            .count()
    }

    fn pending_identity(&self) -> Result<(u64, usize), TerminalTransactionError> {
        let Some(frame) = self.pending.as_ref() else {
            return match self.last_receipt {
                Some(receipt) => Err(TerminalTransactionError::ReceiptReplay {
                    sequence: receipt.sequence(),
                }),
                None => Err(TerminalTransactionError::ReceiptUnavailable),
            };
        };
        Ok((frame.sequence(), frame.output_bytes()))
    }

    fn settle_receipt(
        &mut self,
        receipt: TerminalWriteReceipt,
    ) -> Result<TerminalTransactionResult, TerminalTransactionError> {
        let Some(frame) = self.pending.as_ref() else {
            return match self.last_receipt {
                Some(previous) if previous.sequence() == receipt.sequence() => {
                    Err(TerminalTransactionError::ReceiptReplay {
                        sequence: receipt.sequence(),
                    })
                }
                _ => Err(TerminalTransactionError::ReceiptUnavailable),
            };
        };
        if receipt.writer() != self.writer {
            return Err(TerminalTransactionError::StaleWriter {
                expected: self.writer,
                received: receipt.writer(),
            });
        }
        if receipt.sequence() != frame.sequence() {
            return Err(TerminalTransactionError::ReceiptSequenceMismatch {
                expected: frame.sequence(),
                received: receipt.sequence(),
            });
        }
        if receipt.output_bytes() != frame.output_bytes() {
            return Err(TerminalTransactionError::ReceiptOutputMismatch {
                expected: frame.output_bytes(),
                received: receipt.output_bytes(),
            });
        }
        if receipt.completed_at() < frame.prepared_at() {
            return Err(TerminalTransactionError::ReceiptTimestampOutOfOrder);
        }
        if matches!(
            receipt.outcome(),
            TerminalWriteReceiptOutcome::FullWriteAndFlush
        ) && receipt.written_bytes() != receipt.output_bytes()
        {
            return Err(TerminalTransactionError::ReceiptOutputMismatch {
                expected: receipt.output_bytes(),
                received: receipt.written_bytes(),
            });
        }
        self.last_receipt = Some(receipt);
        if receipt.is_full_write_and_flush() {
            let frame = self
                .pending
                .take()
                .ok_or(TerminalTransactionError::ReceiptUnavailable)?;
            self.baseline = Some(Arc::clone(&frame.surface));
            self.last_good = Some(frame.clone());
            return Ok(TerminalTransactionResult { frame, receipt });
        }
        self.abandon_pending(receipt);
        match receipt.outcome() {
            TerminalWriteReceiptOutcome::PartialWrite => {
                Err(TerminalTransactionError::PartialWrite {
                    expected: receipt.output_bytes(),
                    written: receipt.written_bytes(),
                })
            }
            TerminalWriteReceiptOutcome::WriteFailed => {
                Err(TerminalTransactionError::WriteFailed {
                    kind: ErrorKind::Other,
                })
            }
            TerminalWriteReceiptOutcome::FlushFailed => {
                Err(TerminalTransactionError::FlushFailed {
                    kind: ErrorKind::Other,
                })
            }
            TerminalWriteReceiptOutcome::Lost => Err(TerminalTransactionError::WriteLost),
            TerminalWriteReceiptOutcome::FullWriteAndFlush => {
                Err(TerminalTransactionError::ReceiptUnavailable)
            }
        }
    }

    fn abandon_pending(&mut self, receipt: TerminalWriteReceipt) {
        self.pending = None;
        self.baseline = None;
        self.last_receipt = Some(receipt);
    }
}

fn negotiated_cell_backend(
    capabilities: TerminalCapabilities,
    requested: Backend,
    feedback: TerminalBackendFeedback,
) -> Backend {
    match capabilities.select_safe_backend(requested, feedback) {
        Backend::Auto => Backend::Text,
        backend => backend,
    }
}

fn bounded_surface_bytes(
    surface: &Surface,
    maximum: usize,
    control: &TerminalTransactionControl<'_>,
) -> Result<(), TerminalTransactionError> {
    let mut total = 0usize;
    for (index, cell) in surface.cells().iter().enumerate() {
        if index.is_multiple_of(CHECKPOINT_INTERVAL) {
            control.checkpoint()?;
        }
        total = total
            .checked_add(std::mem::size_of_val(cell))
            .and_then(|value| value.checked_add(cell.grapheme.len()))
            .ok_or(TerminalTransactionError::ResourceLimit)?;
        if total > maximum {
            return Err(TerminalTransactionError::ResourceLimit);
        }
    }
    control.checkpoint()
}

pub(crate) fn degrade_surface_for_capabilities(
    source: &Surface,
    capabilities: TerminalCapabilities,
    control: &TerminalTransactionControl<'_>,
) -> Result<Surface, TerminalTransactionError> {
    degrade_surface(
        source,
        TerminalColorMode::from_capabilities(capabilities),
        control,
    )
}

fn degrade_surface(
    source: &Surface,
    color_mode: TerminalColorMode,
    control: &TerminalTransactionControl<'_>,
) -> Result<Surface, TerminalTransactionError> {
    let mut surface = source.clone();
    for (index, cell) in surface.cells.iter_mut().enumerate() {
        if index.is_multiple_of(CHECKPOINT_INTERVAL) {
            control.checkpoint()?;
        }
        cell.foreground = quantize_color(cell.foreground, color_mode);
        cell.background = quantize_color(cell.background, color_mode);
    }
    surface.validate()?;
    control.checkpoint()?;
    Ok(surface)
}

fn quantize_color(color: Color, color_mode: TerminalColorMode) -> Color {
    match color_mode {
        TerminalColorMode::TrueColor => color,
        TerminalColorMode::Monochrome => Color::Default,
        TerminalColorMode::Ansi256 => color_to_rgb(color)
            .map(quantize_ansi256)
            .map(Color::Indexed)
            .unwrap_or(Color::Default),
        TerminalColorMode::Ansi16 => color_to_rgb(color)
            .map(quantize_ansi16)
            .map(Color::Indexed)
            .unwrap_or(Color::Default),
    }
}

fn color_to_rgb(color: Color) -> Option<(u8, u8, u8)> {
    match color {
        Color::Default => None,
        Color::Rgb(red, green, blue) => Some((red, green, blue)),
        Color::Indexed(index) => Some(indexed_color(index)),
    }
}

fn quantize_ansi256((red, green, blue): (u8, u8, u8)) -> u8 {
    let quantize = |value: u8| ((u16::from(value) * 5 + 127) / 255) as u8;
    16 + quantize(red) * 36 + quantize(green) * 6 + quantize(blue)
}

fn quantize_ansi16(pixel: (u8, u8, u8)) -> u8 {
    ANSI16_PALETTE
        .iter()
        .copied()
        .enumerate()
        .fold((0usize, u32::MAX), |best, (index, candidate)| {
            let distance = color_distance(pixel, candidate);
            if distance < best.1 {
                (index, distance)
            } else {
                best
            }
        })
        .0 as u8
}

fn indexed_color(index: u8) -> (u8, u8, u8) {
    match index {
        0..=15 => ANSI16_PALETTE[index as usize],
        16..=231 => {
            let offset = index - 16;
            let red = ANSI256_CUBE[(offset / 36) as usize];
            let green = ANSI256_CUBE[((offset % 36) / 6) as usize];
            let blue = ANSI256_CUBE[(offset % 6) as usize];
            (red, green, blue)
        }
        232..=255 => {
            let grayscale = 8 + (index - 232) * 10;
            (grayscale, grayscale, grayscale)
        }
    }
}

fn color_distance(left: (u8, u8, u8), right: (u8, u8, u8)) -> u32 {
    let red = i32::from(left.0) - i32::from(right.0);
    let green = i32::from(left.1) - i32::from(right.1);
    let blue = i32::from(left.2) - i32::from(right.2);
    (red * red + green * green + blue * blue) as u32
}

fn map_surface_error(error: SurfaceError) -> TerminalTransactionError {
    match error {
        SurfaceError::OutputLimit => TerminalTransactionError::OutputLimit,
        other => TerminalTransactionError::Surface(other),
    }
}
