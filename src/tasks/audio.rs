//! I2S microphone DMA ping-pong buffers and audio streaming (Phase 4).
//!
//! The I2S peripheral samples the microphone into two statically allocated
//! ping-pong buffers.  While the DMA engine fills one buffer the Wi-Fi task
//! streams the other over a TCP socket.  The swap is governed by atomic
//! operations; no heap allocation or mutex is needed in the hot path.
//!
//! # Memory layout
//!
//! ```text
//! 2 × I2S_FRAME_SIZE = 2 × 1 024 bytes = 2 048 bytes (2 KB)
//! ```
//!
//! A single frame holds [`I2S_FRAME_SAMPLES`] × [`I2S_BYTES_PER_SAMPLE`] bytes
//! (16-bit, mono, little-endian) captured at [`I2S_SAMPLE_RATE`] Hz.  At
//! 16 kHz with 512 samples per frame, each buffer covers exactly 32 ms of audio.
//!
//! # Zero-copy streaming
//!
//! 1. The I2S DMA completion callback calls [`PingPongState::swap`] with the
//!    number of valid bytes that were just captured.
//! 2. The Wi-Fi (or audio) task calls [`PingPongState::ready_buffer`] to obtain
//!    a `&'static [u8]` pointing directly into the completed buffer half.
//! 3. The slice is passed to `socket.write_all(samples).await` — no copy is made.
//!
//! # Task lifecycle (firmware)
//!
//! 1. [`crate::hardware::probe_hardware`] detects the I2S codec at address
//!    [`crate::hardware::AUDIO_CODEC_ADDR`] (`0x1A`).
//! 2. If found, the firmware spawns `audio_task()`.
//! 3. `audio_task` enables the I2S peripheral and starts the DMA engine.

use crate::ipc::Command;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

// ── Constants ─────────────────────────────────────────────────────────────────

/// I2S microphone sample rate in Hz.
///
/// 16 kHz is the standard for voice audio and is supported by all common MEMS
/// microphones (e.g. INMP441, ICS-43434).
pub const I2S_SAMPLE_RATE: u32 = 16_000;

/// Number of 16-bit PCM samples captured per DMA transfer.
///
/// 512 samples at 16 kHz = 32 ms of audio per buffer.  This is long enough to
/// amortise the per-transfer overhead while short enough for real-time
/// streaming.
pub const I2S_FRAME_SAMPLES: usize = 512;

/// Bytes per 16-bit PCM sample (little-endian, mono).
pub const I2S_BYTES_PER_SAMPLE: usize = 2;

/// Total byte size of one I2S DMA frame buffer.
///
/// Calculated as: [`I2S_FRAME_SAMPLES`] × [`I2S_BYTES_PER_SAMPLE`] =
/// **1 024 bytes**.
pub const I2S_FRAME_SIZE: usize = I2S_FRAME_SAMPLES * I2S_BYTES_PER_SAMPLE;

// ── Static ping-pong buffers ──────────────────────────────────────────────────

/// "Ping" half of the I2S DMA ping-pong pair (1 KB, zero-initialised).
///
/// # Safety
///
/// Only the [`PingPongState`] API writes to or reads from this buffer.
/// [`PingPongState::swap`] guarantees that the DMA engine and the consumer
/// always operate on **different** halves.
#[allow(static_mut_refs)]
pub static mut I2S_PING: [u8; I2S_FRAME_SIZE] = [0u8; I2S_FRAME_SIZE];

/// "Pong" half of the I2S DMA ping-pong pair (1 KB, zero-initialised).
///
/// # Safety
///
/// See [`I2S_PING`].
#[allow(static_mut_refs)]
pub static mut I2S_PONG: [u8; I2S_FRAME_SIZE] = [0u8; I2S_FRAME_SIZE];

// ── Ping-pong state ───────────────────────────────────────────────────────────

/// Identifies which of the two I2S DMA buffers is being referenced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PingPong {
    /// The first ("ping") buffer.
    Ping,
    /// The second ("pong") buffer.
    Pong,
}

impl PingPong {
    /// Human-readable label for logging / defmt output.
    pub fn label(&self) -> &'static str {
        match self {
            PingPong::Ping => "Ping",
            PingPong::Pong => "Pong",
        }
    }

    /// Return the other buffer half.
    pub fn other(&self) -> PingPong {
        match self {
            PingPong::Ping => PingPong::Pong,
            PingPong::Pong => PingPong::Ping,
        }
    }
}

/// Atomic state tracker for the I2S ping-pong DMA buffers.
///
/// The struct tracks which half is currently being written by the DMA engine
/// and how many valid bytes are in the completed half.  The raw byte storage
/// lives in [`I2S_PING`] / [`I2S_PONG`].
///
/// Construct a compile-time instance with [`PingPongState::new`] and store it
/// in a `static` for use across tasks.
pub struct PingPongState {
    /// `false` = DMA writing into Ping; `true` = DMA writing into Pong.
    active_is_pong: AtomicBool,
    /// Number of valid bytes in the most recently completed buffer half.
    ready_len: AtomicUsize,
}

/// Global ping-pong state instance.
///
/// Firmware code uses this singleton together with [`I2S_PING`] / [`I2S_PONG`]
/// to implement zero-copy audio streaming.
pub static PING_PONG_STATE: PingPongState = PingPongState::new();

impl Default for PingPongState {
    fn default() -> Self {
        Self::new()
    }
}

impl PingPongState {
    /// Create a new ping-pong state with DMA writing to Ping and no samples ready.
    pub const fn new() -> Self {
        Self {
            active_is_pong: AtomicBool::new(false),
            ready_len: AtomicUsize::new(0),
        }
    }

    /// Return which buffer half the I2S DMA is currently writing into.
    pub fn active(&self) -> PingPong {
        if self.active_is_pong.load(Ordering::Acquire) {
            PingPong::Pong
        } else {
            PingPong::Ping
        }
    }

    /// Return which buffer half contains the most recently completed samples.
    pub fn ready(&self) -> PingPong {
        self.active().other()
    }

    /// Return the number of valid bytes in the ready (completed) buffer.
    ///
    /// Returns `0` when no samples have been captured yet.
    pub fn ready_len(&self) -> usize {
        self.ready_len.load(Ordering::Acquire)
    }

    /// Called from the I2S DMA completion callback.
    ///
    /// Marks `len` bytes as ready in the currently active buffer, then switches
    /// the DMA engine to the other buffer.  The completed buffer becomes the
    /// "ready" half and can be accessed via [`ready_buffer_from`] or
    /// [`ready_buffer`].
    ///
    /// `len` is clamped to [`I2S_FRAME_SIZE`].
    pub fn swap(&self, len: usize) {
        let clamped = len.min(I2S_FRAME_SIZE);
        self.ready_len.store(clamped, Ordering::Release);
        // Toggle the active buffer: false → true → false → …
        self.active_is_pong.fetch_xor(true, Ordering::AcqRel);
    }

    /// Return a zero-copy slice into the completed buffer.
    ///
    /// Accepts explicit references to the ping and pong buffers so this method
    /// is fully testable on the host without touching the global statics.
    /// Firmware code should use [`ready_buffer`].
    ///
    /// Returns an empty slice when no samples have been captured yet.
    pub fn ready_buffer_from<'a>(
        &self,
        ping: &'a [u8; I2S_FRAME_SIZE],
        pong: &'a [u8; I2S_FRAME_SIZE],
    ) -> &'a [u8] {
        let len = self.ready_len.load(Ordering::Acquire);
        if len == 0 {
            return &[];
        }
        match self.ready() {
            PingPong::Ping => &ping[..len],
            PingPong::Pong => &pong[..len],
        }
    }

    /// Return a zero-copy `&'static [u8]` into the completed I2S DMA buffer.
    ///
    /// The returned slice is passed directly to `socket.write_all(samples).await`
    /// in the Wi-Fi task without any intermediate copy.
    ///
    /// Returns an empty slice when no samples have been captured yet.
    ///
    /// # Safety
    ///
    /// After [`swap`], the DMA engine writes to the *other* half, so the
    /// returned reference is always into a quiescent buffer.
    #[allow(static_mut_refs)]
    pub fn ready_buffer(&self) -> &'static [u8] {
        // SAFETY: ready() returns the half that is NOT currently being filled
        // by the DMA engine.  The Release/Acquire pair in swap() ensures the
        // write is visible before this read.
        unsafe { self.ready_buffer_from(&I2S_PING, &I2S_PONG) }
    }
}

// ── Audio state machine ───────────────────────────────────────────────────────

/// Current operational state of the audio pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioState {
    /// I2S DMA is idle; no samples are being captured.
    Idle,
    /// I2S DMA is actively capturing samples into the ping-pong buffers.
    Streaming,
}

impl AudioState {
    /// Human-readable label for logging / defmt output.
    pub fn label(&self) -> &'static str {
        match self {
            AudioState::Idle => "Idle",
            AudioState::Streaming => "Streaming",
        }
    }
}

/// Transition the audio state in response to an incoming [`Command`].
///
/// Only [`Command::StreamAudio`] changes the audio state; all other commands
/// are ignored and the current state is returned unchanged.
///
/// This pure function is fully testable on the host without any hardware.
pub fn handle_audio_command(state: AudioState, cmd: &Command) -> AudioState {
    match cmd {
        Command::StreamAudio(true) => AudioState::Streaming,
        Command::StreamAudio(false) => AudioState::Idle,
        _ => state,
    }
}

// ── Async task (firmware-only) ────────────────────────────────────────────────

/// Continuously await commands from [`crate::ipc::COMMAND_BUS`] and control
/// the I2S audio pipeline.
///
/// In firmware this function initialises the I2S peripheral, starts the DMA
/// engine into the ping-pong buffers, and responds to [`Command::StreamAudio`]
/// commands from the bus.
///
/// Marked `#[cfg(feature = "firmware")]` to exclude it from host-side test
/// builds (the I2S HAL driver is not available on the host).
#[cfg(feature = "firmware")]
pub async fn audio_task_loop() {
    use crate::ipc::COMMAND_BUS;
    let mut state = AudioState::Idle;
    loop {
        let cmd = COMMAND_BUS.receive().await;
        state = handle_audio_command(state, &cmd);
        esp_println::println!("[audio_task] state = {}", state.label());
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{Command, Expression};

    // ── Constants ─────────────────────────────────────────────────────────────

    #[test]
    fn i2s_sample_rate_is_16khz() {
        assert_eq!(I2S_SAMPLE_RATE, 16_000);
    }

    #[test]
    fn i2s_frame_samples_is_512() {
        assert_eq!(I2S_FRAME_SAMPLES, 512);
    }

    #[test]
    fn i2s_frame_size_is_1kb() {
        assert_eq!(
            I2S_FRAME_SIZE, 1024,
            "I2S frame must be exactly 1 024 bytes"
        );
    }

    #[test]
    fn i2s_total_pingpong_size_is_2kb() {
        assert_eq!(
            I2S_FRAME_SIZE * 2,
            2048,
            "total ping-pong buffer must be 2 KB"
        );
    }

    // ── Static buffer layout ──────────────────────────────────────────────────

    #[test]
    fn i2s_ping_static_has_correct_size() {
        assert_eq!(
            core::mem::size_of_val(unsafe { &*core::ptr::addr_of!(I2S_PING) }),
            I2S_FRAME_SIZE,
        );
    }

    #[test]
    fn i2s_pong_static_has_correct_size() {
        assert_eq!(
            core::mem::size_of_val(unsafe { &*core::ptr::addr_of!(I2S_PONG) }),
            I2S_FRAME_SIZE,
        );
    }

    // ── PingPong helpers ──────────────────────────────────────────────────────

    #[test]
    fn ping_pong_other_returns_opposite() {
        assert_eq!(PingPong::Ping.other(), PingPong::Pong);
        assert_eq!(PingPong::Pong.other(), PingPong::Ping);
    }

    #[test]
    fn ping_pong_labels() {
        assert_eq!(PingPong::Ping.label(), "Ping");
        assert_eq!(PingPong::Pong.label(), "Pong");
    }

    // ── PingPongState initial state ───────────────────────────────────────────

    #[test]
    fn pingpong_state_starts_writing_to_ping() {
        let state = PingPongState::new();
        assert_eq!(
            state.active(),
            PingPong::Ping,
            "DMA should start writing to Ping"
        );
        assert_eq!(
            state.ready(),
            PingPong::Pong,
            "Pong is ready (empty) at start"
        );
        assert_eq!(state.ready_len(), 0);
    }

    #[test]
    fn pingpong_state_ready_buffer_empty_on_start() {
        let state = PingPongState::new();
        let ping = [0u8; I2S_FRAME_SIZE];
        let pong = [0u8; I2S_FRAME_SIZE];
        assert_eq!(state.ready_buffer_from(&ping, &pong), &[] as &[u8]);
    }

    // ── swap ──────────────────────────────────────────────────────────────────

    #[test]
    fn swap_switches_active_buffer() {
        let state = PingPongState::new();
        assert_eq!(state.active(), PingPong::Ping);
        state.swap(512);
        assert_eq!(state.active(), PingPong::Pong);
        state.swap(512);
        assert_eq!(state.active(), PingPong::Ping);
    }

    #[test]
    fn swap_updates_ready_len() {
        let state = PingPongState::new();
        state.swap(768);
        assert_eq!(state.ready_len(), 768);
        state.swap(1024);
        assert_eq!(state.ready_len(), 1024);
    }

    #[test]
    fn swap_clamps_len_to_frame_size() {
        let state = PingPongState::new();
        state.swap(I2S_FRAME_SIZE + 9999);
        assert_eq!(state.ready_len(), I2S_FRAME_SIZE);
    }

    #[test]
    fn swap_ready_buffer_switches_between_ping_and_pong() {
        let state = PingPongState::new();
        // Before any swap: active = Ping, ready = Pong.
        assert_eq!(state.ready(), PingPong::Pong);
        state.swap(100);
        // After first swap: active = Pong, ready = Ping.
        assert_eq!(state.ready(), PingPong::Ping);
        state.swap(200);
        // After second swap: active = Ping, ready = Pong.
        assert_eq!(state.ready(), PingPong::Pong);
    }

    // ── ready_buffer_from (zero-copy) ─────────────────────────────────────────

    #[test]
    fn ready_buffer_from_returns_ping_after_first_swap() {
        // After one swap: DMA moves to Pong → ready = Ping.
        let state = PingPongState::new();
        let mut ping = [0u8; I2S_FRAME_SIZE];
        let pong = [0u8; I2S_FRAME_SIZE];
        ping[0] = 0xDE;
        ping[1] = 0xAD;
        state.swap(2);
        let buf = state.ready_buffer_from(&ping, &pong);
        assert_eq!(buf, &[0xDE, 0xAD]);
    }

    #[test]
    fn ready_buffer_from_returns_pong_after_second_swap() {
        // After two swaps: DMA back to Ping → ready = Pong.
        let state = PingPongState::new();
        let ping = [0u8; I2S_FRAME_SIZE];
        let mut pong = [0u8; I2S_FRAME_SIZE];
        pong[0] = 0xBE;
        pong[1] = 0xEF;
        state.swap(512); // first swap: active → Pong, ready → Ping
        state.swap(2); // second swap: active → Ping, ready → Pong
        let buf = state.ready_buffer_from(&ping, &pong);
        assert_eq!(buf, &[0xBE, 0xEF]);
    }

    #[test]
    fn ready_buffer_from_returns_full_frame_when_full_length() {
        let state = PingPongState::new();
        let ping = [0xAAu8; I2S_FRAME_SIZE];
        let pong = [0u8; I2S_FRAME_SIZE];
        state.swap(I2S_FRAME_SIZE);
        let buf = state.ready_buffer_from(&ping, &pong);
        assert_eq!(buf.len(), I2S_FRAME_SIZE);
        assert!(buf.iter().all(|&b| b == 0xAA));
    }

    // ── AudioState labels ─────────────────────────────────────────────────────

    #[test]
    fn audio_state_idle_label() {
        assert_eq!(AudioState::Idle.label(), "Idle");
    }

    #[test]
    fn audio_state_streaming_label() {
        assert_eq!(AudioState::Streaming.label(), "Streaming");
    }

    // ── handle_audio_command state machine ────────────────────────────────────

    #[test]
    fn handle_audio_command_stream_audio_true_starts_streaming() {
        let state = handle_audio_command(AudioState::Idle, &Command::StreamAudio(true));
        assert_eq!(state, AudioState::Streaming);
    }

    #[test]
    fn handle_audio_command_stream_audio_false_stops_streaming() {
        let state = handle_audio_command(AudioState::Streaming, &Command::StreamAudio(false));
        assert_eq!(state, AudioState::Idle);
    }

    #[test]
    fn handle_audio_command_stream_audio_false_on_idle_stays_idle() {
        let state = handle_audio_command(AudioState::Idle, &Command::StreamAudio(false));
        assert_eq!(state, AudioState::Idle);
    }

    #[test]
    fn handle_audio_command_stream_audio_true_on_streaming_stays_streaming() {
        let state = handle_audio_command(AudioState::Streaming, &Command::StreamAudio(true));
        assert_eq!(state, AudioState::Streaming);
    }

    #[test]
    fn handle_audio_command_stream_video_preserves_state() {
        let state = handle_audio_command(AudioState::Streaming, &Command::StreamVideo(true));
        assert_eq!(
            state,
            AudioState::Streaming,
            "video command must not change audio state"
        );
    }

    #[test]
    fn handle_audio_command_set_face_preserves_state() {
        let state = handle_audio_command(
            AudioState::Idle,
            &Command::SetFaceExpression(Expression::Happy),
        );
        assert_eq!(state, AudioState::Idle);
    }

    #[test]
    fn handle_audio_command_move_servo_preserves_state() {
        let state = handle_audio_command(AudioState::Streaming, &Command::MoveServo(0, 90));
        assert_eq!(state, AudioState::Streaming);
    }

    #[test]
    fn handle_audio_command_chain_idle_stream_stop() {
        let s0 = AudioState::Idle;
        let s1 = handle_audio_command(s0, &Command::StreamAudio(true));
        let s2 = handle_audio_command(s1, &Command::MoveServo(1, 45)); // no-op
        let s3 = handle_audio_command(s2, &Command::StreamAudio(false));
        assert_eq!(s3, AudioState::Idle);
    }
}
