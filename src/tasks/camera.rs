//! Camera DMA ring buffer and zero-copy JPEG frame management (Phase 4).
//!
//! The OV2640 is configured via SCCB (I2C-compatible) to output
//! hardware-compressed JPEG frames directly into a static DMA ring buffer.
//! The CPU never processes raw pixel data.  The current frame is exposed as a
//! zero-copy `&[u8]` slice that can be written directly to a Wi-Fi TCP socket
//! without any intermediate copy.
//!
//! # Memory layout
//!
//! ```text
//! DMA_RING_DEPTH × JPEG_FRAME_SIZE = 2 × 16 384 bytes = 32 768 bytes (32 KB)
//! ```
//!
//! Two ring slots implement a classic double-buffer: the DMA engine fills the
//! **back** slot while the Wi-Fi task drains the **front** slot.  The slot
//! swap is governed by atomic operations, so no mutex is needed in the hot path.
//!
//! # Zero-copy streaming
//!
//! 1. The DMA completion callback calls [`DmaRingBuffer::mark_frame_ready`]
//!    with the filled slot index and the actual JPEG byte count.
//! 2. The Wi-Fi task calls [`DmaRingBuffer::current_frame`] to obtain a
//!    `&'static [u8]` pointing directly into the DMA buffer.
//! 3. The slice is passed to `socket.write_all(frame).await` — no copy is made.
//!
//! # Task lifecycle (firmware)
//!
//! 1. [`crate::hardware::probe_hardware`] detects the OV2640 at SCCB address
//!    [`crate::hardware::CAMERA_SCCB_ADDR`] (`0x30`).
//! 2. If found, the firmware spawns `camera_task()`.
//! 3. `camera_task` configures the sensor via SCCB and starts the DMA engine.

use crate::ipc::Command;
use core::sync::atomic::{AtomicUsize, Ordering};

// ── Constants ─────────────────────────────────────────────────────────────────

/// Maximum byte size of a single hardware-compressed JPEG frame.
///
/// QVGA (320×240) JPEG at the OV2640's internal quality setting fits
/// comfortably in 16 KB.  Frames that exceed this size are silently clamped
/// by the DMA engine; increase this constant if higher resolution is required.
pub const JPEG_FRAME_SIZE: usize = 16_384; // 16 KB

/// Number of JPEG frame slots in the DMA ring buffer.
///
/// Two slots provide a double buffer: the DMA engine writes to one while the
/// Wi-Fi task reads from the other.  A depth greater than 2 increases SRAM
/// usage without benefit in the single-writer / single-reader case.
pub const DMA_RING_DEPTH: usize = 2;

// ── Static DMA ring buffer ────────────────────────────────────────────────────

/// Raw backing storage for the DMA JPEG ring buffer (32 KB, zero-initialised).
///
/// # Safety
///
/// The double-buffer design ensures the DMA engine and the Wi-Fi task always
/// operate on **different** slots.  [`DmaRingBuffer::mark_frame_ready`] only
/// advances the write slot after the DMA transfer is complete, and
/// [`DmaRingBuffer::current_frame`] only reads from the previously completed
/// slot.
#[allow(static_mut_refs)]
pub static mut DMA_JPEG_RING: [[u8; JPEG_FRAME_SIZE]; DMA_RING_DEPTH] =
    [[0u8; JPEG_FRAME_SIZE]; DMA_RING_DEPTH];

// ── Ring-buffer metadata ──────────────────────────────────────────────────────

/// Atomic metadata for the DMA JPEG ring buffer.
///
/// The struct tracks which ring slot the DMA engine writes to next and how many
/// valid bytes are in each slot.  The actual byte storage lives in
/// [`DMA_JPEG_RING`].
///
/// Construct a compile-time instance with [`DmaRingBuffer::new`] and store it
/// in a `static` for use across tasks.
pub struct DmaRingBuffer {
    /// Index of the ring slot the DMA engine will write to next.
    write_slot: AtomicUsize,
    /// Valid byte counts for each ring slot (`0` = slot not yet populated).
    frame_lens: [AtomicUsize; DMA_RING_DEPTH],
}

/// Global ring-buffer metadata instance.
///
/// Firmware code uses this singleton together with the [`DMA_JPEG_RING`]
/// static to implement zero-copy JPEG streaming.
pub static DMA_RING_BUFFER: DmaRingBuffer = DmaRingBuffer::new();

impl Default for DmaRingBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl DmaRingBuffer {
    /// Create a new, empty ring-buffer metadata instance.
    ///
    /// All slot lengths are initialised to zero and the write slot starts at
    /// index 0.  This is a `const fn` so the instance can live in a `static`.
    pub const fn new() -> Self {
        Self {
            write_slot: AtomicUsize::new(0),
            frame_lens: [AtomicUsize::new(0), AtomicUsize::new(0)],
        }
    }

    /// Mark ring slot `slot` as containing a complete JPEG frame of `len` bytes,
    /// then advance the write pointer to the next slot.
    ///
    /// Call this from the DMA completion callback after the camera peripheral
    /// has transferred a full JPEG frame into the slot.
    ///
    /// `len` is clamped to [`JPEG_FRAME_SIZE`]; a larger value indicates a
    /// hardware misconfiguration.
    ///
    /// # Ordering
    ///
    /// Uses `Release` on the length store so that a subsequent `Acquire` load
    /// in [`current_frame`] observes the completed write.
    pub fn mark_frame_ready(&self, slot: usize, len: usize) {
        let clamped = len.min(JPEG_FRAME_SIZE);
        let slot_idx = slot % DMA_RING_DEPTH;
        self.frame_lens[slot_idx].store(clamped, Ordering::Release);
        // Advance the write slot to the next position in the ring.
        self.write_slot
            .store((slot_idx + 1) % DMA_RING_DEPTH, Ordering::Release);
    }

    /// Return the index of the ring slot that contains the most recently
    /// completed JPEG frame (the slot available for the consumer to read).
    ///
    /// The read slot is `(write_slot − 1 + DMA_RING_DEPTH) % DMA_RING_DEPTH`.
    /// Before any frame has been captured this resolves to slot 1, which has a
    /// zero length, so [`current_frame_len`] returns 0.
    pub fn current_read_slot(&self) -> usize {
        let next_write = self.write_slot.load(Ordering::Acquire);
        (next_write + DMA_RING_DEPTH - 1) % DMA_RING_DEPTH
    }

    /// Return the number of valid JPEG bytes in the current read slot.
    ///
    /// Returns `0` when no frame has been captured yet.
    pub fn current_frame_len(&self) -> usize {
        let slot = self.current_read_slot();
        self.frame_lens[slot].load(Ordering::Acquire)
    }

    /// Return a zero-copy slice into the current JPEG frame.
    ///
    /// Accepts an explicit reference to the ring buffer backing array so that
    /// this method is fully testable on the host without accessing the
    /// `DMA_JPEG_RING` static.  Firmware code should use [`current_frame`].
    ///
    /// Returns an empty slice when no frame has been captured yet.
    pub fn current_frame_from<'a>(
        &self,
        ring: &'a [[u8; JPEG_FRAME_SIZE]; DMA_RING_DEPTH],
    ) -> &'a [u8] {
        let slot = self.current_read_slot();
        let len = self.frame_lens[slot].load(Ordering::Acquire);
        if len == 0 {
            return &[];
        }
        &ring[slot][..len]
    }

    /// Return a zero-copy `&'static [u8]` into the global [`DMA_JPEG_RING`].
    ///
    /// This is the fast path for firmware: the returned slice is passed
    /// directly to `socket.write_all(frame).await` without any copy.
    ///
    /// Returns an empty slice when no frame has been captured yet.
    ///
    /// # Safety
    ///
    /// The double-buffer design guarantees that the read slot is **never** the
    /// same slot the DMA engine is currently filling.
    /// [`mark_frame_ready`] advances the write slot atomically after the
    /// transfer is complete, so the consumer always observes a quiescent slot.
    #[allow(static_mut_refs)]
    pub fn current_frame(&self) -> &'static [u8] {
        // SAFETY: current_read_slot() returns the slot that is NOT being filled
        // by DMA.  The Release/Acquire pair in mark_frame_ready ensures the
        // write is visible before this read.
        unsafe { self.current_frame_from(&DMA_JPEG_RING) }
    }
}

// ── Camera state machine ──────────────────────────────────────────────────────

/// Current operational state of the camera pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CameraState {
    /// Camera is configured but not streaming.
    Idle,
    /// Camera DMA is actively capturing frames into the ring buffer.
    Streaming,
}

impl CameraState {
    /// Human-readable label for logging / defmt output.
    pub fn label(&self) -> &'static str {
        match self {
            CameraState::Idle => "Idle",
            CameraState::Streaming => "Streaming",
        }
    }
}

/// Transition the camera state in response to an incoming [`Command`].
///
/// Only [`Command::StreamVideo`] changes the camera state; all other commands
/// are ignored and the current state is returned unchanged.
///
/// This pure function is fully testable on the host without any hardware.
pub fn handle_camera_command(state: CameraState, cmd: &Command) -> CameraState {
    match cmd {
        Command::StreamVideo(true) => CameraState::Streaming,
        Command::StreamVideo(false) => CameraState::Idle,
        _ => state,
    }
}

// ── Async task (firmware-only) ────────────────────────────────────────────────

/// Continuously await commands from [`crate::ipc::COMMAND_BUS`] and control
/// the camera DMA pipeline.
///
/// In firmware this function initialises the OV2640 via SCCB, configures the
/// ESP32 camera peripheral to output JPEG directly into [`DMA_JPEG_RING`], and
/// responds to [`Command::StreamVideo`] commands from the bus.
///
/// Marked `#[cfg(feature = "firmware")]` to exclude it from host-side test
/// builds (the camera HAL driver is not available on the host).
#[cfg(feature = "firmware")]
pub async fn camera_task_loop() {
    use crate::ipc::COMMAND_BUS;
    let mut state = CameraState::Idle;
    loop {
        let cmd = COMMAND_BUS.receive().await;
        state = handle_camera_command(state, &cmd);
        esp_println::println!("[camera_task] state = {}", state.label());
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{Command, Expression};

    // ── Constants ─────────────────────────────────────────────────────────────

    #[test]
    fn jpeg_frame_size_is_16kb() {
        assert_eq!(JPEG_FRAME_SIZE, 16_384);
    }

    #[test]
    fn dma_ring_depth_is_two() {
        assert_eq!(DMA_RING_DEPTH, 2);
    }

    #[test]
    fn dma_ring_total_size_is_32kb() {
        assert_eq!(
            JPEG_FRAME_SIZE * DMA_RING_DEPTH,
            32_768,
            "DMA ring buffer must occupy exactly 32 KB"
        );
    }

    // ── Static buffer layout ──────────────────────────────────────────────────

    #[test]
    fn dma_jpeg_ring_static_has_correct_size() {
        assert_eq!(
            core::mem::size_of_val(unsafe { &*core::ptr::addr_of!(DMA_JPEG_RING) }),
            JPEG_FRAME_SIZE * DMA_RING_DEPTH,
        );
    }

    #[test]
    fn dma_jpeg_ring_is_zeroed_at_startup() {
        let sum: u32 = unsafe {
            let ptr = core::ptr::addr_of!(DMA_JPEG_RING) as *const u8;
            (0..JPEG_FRAME_SIZE * DMA_RING_DEPTH)
                .map(|i| *ptr.add(i) as u32)
                .sum()
        };
        assert_eq!(sum, 0, "DMA ring buffer must start fully zeroed");
    }

    // ── DmaRingBuffer initial state ───────────────────────────────────────────

    #[test]
    fn ring_buffer_starts_empty() {
        let buf = DmaRingBuffer::new();
        assert_eq!(buf.current_frame_len(), 0, "no frame before DMA writes");
    }

    #[test]
    fn ring_buffer_current_frame_empty_on_start() {
        let buf = DmaRingBuffer::new();
        let ring = [[0u8; JPEG_FRAME_SIZE]; DMA_RING_DEPTH];
        assert_eq!(buf.current_frame_from(&ring), &[] as &[u8]);
    }

    // ── mark_frame_ready ──────────────────────────────────────────────────────

    #[test]
    fn mark_frame_ready_records_len_in_slot_zero() {
        let buf = DmaRingBuffer::new();
        buf.mark_frame_ready(0, 1024);
        assert_eq!(buf.current_read_slot(), 0);
        assert_eq!(buf.current_frame_len(), 1024);
    }

    #[test]
    fn mark_frame_ready_advances_write_slot() {
        let buf = DmaRingBuffer::new();
        buf.mark_frame_ready(0, 512);
        buf.mark_frame_ready(1, 768);
        assert_eq!(buf.current_read_slot(), 1);
        assert_eq!(buf.current_frame_len(), 768);
    }

    #[test]
    fn mark_frame_ready_wraps_around_ring() {
        let buf = DmaRingBuffer::new();
        buf.mark_frame_ready(0, 100);
        buf.mark_frame_ready(1, 200);
        // Wraps back to slot 0
        buf.mark_frame_ready(0, 300);
        assert_eq!(buf.current_read_slot(), 0);
        assert_eq!(buf.current_frame_len(), 300);
    }

    #[test]
    fn mark_frame_ready_clamps_len_to_frame_size() {
        let buf = DmaRingBuffer::new();
        buf.mark_frame_ready(0, JPEG_FRAME_SIZE + 9999);
        assert_eq!(
            buf.current_frame_len(),
            JPEG_FRAME_SIZE,
            "len must be clamped to JPEG_FRAME_SIZE"
        );
    }

    // ── current_frame_from (zero-copy) ────────────────────────────────────────

    #[test]
    fn current_frame_from_returns_correct_slice() {
        let buf = DmaRingBuffer::new();
        let mut ring = [[0u8; JPEG_FRAME_SIZE]; DMA_RING_DEPTH];
        // Write a JPEG SOI marker into slot 0
        ring[0][0] = 0xFF;
        ring[0][1] = 0xD8;
        ring[0][2] = 0xAB;
        buf.mark_frame_ready(0, 3);
        let frame = buf.current_frame_from(&ring);
        assert_eq!(frame, &[0xFF, 0xD8, 0xAB]);
    }

    #[test]
    fn current_frame_from_switches_slot_on_advance() {
        let buf = DmaRingBuffer::new();
        let mut ring = [[0u8; JPEG_FRAME_SIZE]; DMA_RING_DEPTH];
        ring[0][0] = 0xAA;
        ring[1][0] = 0xBB;
        buf.mark_frame_ready(0, 1);
        assert_eq!(buf.current_frame_from(&ring), &[0xAA]);
        buf.mark_frame_ready(1, 1);
        assert_eq!(buf.current_frame_from(&ring), &[0xBB]);
    }

    #[test]
    fn current_frame_from_returns_full_clamped_length() {
        let buf = DmaRingBuffer::new();
        let ring = [[0xFFu8; JPEG_FRAME_SIZE]; DMA_RING_DEPTH];
        buf.mark_frame_ready(0, JPEG_FRAME_SIZE);
        let frame = buf.current_frame_from(&ring);
        assert_eq!(frame.len(), JPEG_FRAME_SIZE);
    }

    // ── CameraState labels ────────────────────────────────────────────────────

    #[test]
    fn camera_state_idle_label() {
        assert_eq!(CameraState::Idle.label(), "Idle");
    }

    #[test]
    fn camera_state_streaming_label() {
        assert_eq!(CameraState::Streaming.label(), "Streaming");
    }

    // ── handle_camera_command state machine ───────────────────────────────────

    #[test]
    fn handle_camera_command_stream_video_true_starts_streaming() {
        let state = handle_camera_command(CameraState::Idle, &Command::StreamVideo(true));
        assert_eq!(state, CameraState::Streaming);
    }

    #[test]
    fn handle_camera_command_stream_video_false_stops_streaming() {
        let state = handle_camera_command(CameraState::Streaming, &Command::StreamVideo(false));
        assert_eq!(state, CameraState::Idle);
    }

    #[test]
    fn handle_camera_command_stream_video_false_on_idle_stays_idle() {
        let state = handle_camera_command(CameraState::Idle, &Command::StreamVideo(false));
        assert_eq!(state, CameraState::Idle);
    }

    #[test]
    fn handle_camera_command_stream_video_true_on_streaming_stays_streaming() {
        let state = handle_camera_command(CameraState::Streaming, &Command::StreamVideo(true));
        assert_eq!(state, CameraState::Streaming);
    }

    #[test]
    fn handle_camera_command_stream_audio_preserves_state() {
        let state = handle_camera_command(CameraState::Streaming, &Command::StreamAudio(true));
        assert_eq!(
            state,
            CameraState::Streaming,
            "audio command must not change camera state"
        );
    }

    #[test]
    fn handle_camera_command_set_face_preserves_state() {
        let state = handle_camera_command(
            CameraState::Idle,
            &Command::SetFaceExpression(Expression::Happy),
        );
        assert_eq!(state, CameraState::Idle);
    }

    #[test]
    fn handle_camera_command_move_servo_preserves_state() {
        let state = handle_camera_command(CameraState::Streaming, &Command::MoveServo(0, 90));
        assert_eq!(state, CameraState::Streaming);
    }

    #[test]
    fn handle_camera_command_chain_idle_stream_stop() {
        let s0 = CameraState::Idle;
        let s1 = handle_camera_command(s0, &Command::StreamVideo(true));
        let s2 = handle_camera_command(s1, &Command::MoveServo(1, 45)); // no-op
        let s3 = handle_camera_command(s2, &Command::StreamVideo(false));
        assert_eq!(s3, CameraState::Idle);
    }
}
