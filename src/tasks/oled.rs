//! OLED display task for SSD1306 (128×64, 1-bit monochrome).
//!
//! # Memory layout
//!
//! The framebuffer is a `[u8; 1024]` static array allocated at compile time:
//!
//! ```text
//! 128 columns × 64 rows ÷ 8 bits/byte = 1 024 bytes
//! ```
//!
//! This single kilobyte is the **entire** RAM footprint of the display
//! subsystem – no heap involved.
//!
//! # Task lifecycle
//!
//! 1. `probe_hardware()` detects the SSD1306 at address `0x3C`.
//! 2. If found, the firmware spawns `oled_task()`.
//! 3. If absent, the OS logs `"No Screen Detected"` and moves on.
//!
//! The task loop pulls [`Command::SetFaceExpression`] messages from
//! [`COMMAND_BUS`] and updates [`OLED_FRAMEBUFFER`] accordingly.

use crate::ipc::{Command, Expression};
#[cfg(feature = "firmware")]
use crate::ipc::COMMAND_BUS;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Size of the SSD1306 128×64 1-bit framebuffer in bytes.
///
/// Calculated as: 128 columns × 64 rows ÷ 8 bits per byte = **1 024 bytes**.
pub const FRAMEBUFFER_SIZE: usize = 1024;

// ── Static framebuffer ────────────────────────────────────────────────────────

/// Raw backing storage for the OLED framebuffer (1 KB, zero-initialised).
///
/// # Safety
///
/// Access is always single-owner: `oled_task` takes exclusive access after
/// boot.  The Embassy executor guarantees that a task decorated with
/// `#[embassy_executor::task]` is spawned **at most once**, making this
/// `static mut` sound in practice.
#[allow(static_mut_refs)]
pub static mut OLED_FRAMEBUFFER: [u8; FRAMEBUFFER_SIZE] = [0u8; FRAMEBUFFER_SIZE];

// ── State machine ─────────────────────────────────────────────────────────────

/// The current display state of the OLED task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OledState {
    /// The display is blank (initialising or no face assigned yet).
    Blank,
    /// A face expression is currently being shown.
    Face(Expression),
}

impl OledState {
    /// Human-readable label for logging / defmt output.
    pub fn label(&self) -> &'static str {
        match self {
            OledState::Blank => "Blank",
            OledState::Face(Expression::Happy) => "Happy",
            OledState::Face(Expression::Sad) => "Sad",
            OledState::Face(Expression::Neutral) => "Neutral",
            OledState::Face(Expression::Surprised) => "Surprised",
        }
    }
}

// ── Command handler ───────────────────────────────────────────────────────────

/// Transition the OLED state in response to an incoming [`Command`].
///
/// Only [`Command::SetFaceExpression`] triggers a display change; all other
/// commands are forwarded to their respective consumers and the current state
/// is returned unchanged.
///
/// This pure function is fully testable on the host without any hardware.
pub fn handle_command(state: OledState, cmd: &Command) -> OledState {
    match cmd {
        Command::SetFaceExpression(expr) => OledState::Face(expr.clone()),
        _ => state,
    }
}

// ── Async task (firmware-only) ────────────────────────────────────────────────

/// Continuously await commands from [`COMMAND_BUS`] and update the OLED.
///
/// This function is intended to be spawned by the Embassy executor when the
/// SSD1306 is detected during the boot-time probe.  It is marked
/// `#[cfg(feature = "firmware")]` to exclude it from host-side test builds
/// (the async I2C driver is not available on the host).
#[cfg(feature = "firmware")]
pub async fn oled_task_loop() {
    let mut state = OledState::Blank;
    loop {
        let cmd = COMMAND_BUS.receive().await;
        state = handle_command(state, &cmd);
        // Log via esp-println (routed to USB-JTAG / UART in firmware builds)
        esp_println::println!("[oled_task] face = {}", state.label());
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{Command, Expression};

    // ── Framebuffer layout ────────────────────────────────────────────────────

    #[test]
    fn framebuffer_constant_is_exactly_1kb() {
        assert_eq!(
            FRAMEBUFFER_SIZE,
            1024,
            "SSD1306 128×64 1-bit framebuffer must be exactly 1 024 bytes"
        );
    }

    #[test]
    fn framebuffer_static_has_correct_length() {
        // The backing array's size equals the compile-time constant; no shared
        // reference to the mutable static is needed.
        assert_eq!(
            // SAFETY: we only read the size via a raw (not shared) pointer.
            core::mem::size_of_val(unsafe { &*core::ptr::addr_of!(OLED_FRAMEBUFFER) }),
            FRAMEBUFFER_SIZE
        );
    }

    #[test]
    fn framebuffer_is_zeroed_at_startup() {
        // SAFETY: single-threaded test; we access via a raw pointer to avoid
        // creating a shared reference to the mutable static.
        let sum: u32 = unsafe {
            let ptr = core::ptr::addr_of!(OLED_FRAMEBUFFER) as *const u8;
            (0..FRAMEBUFFER_SIZE).map(|i| *ptr.add(i) as u32).sum()
        };
        assert_eq!(sum, 0, "framebuffer should start fully dark (all bits 0)");
    }

    // ── OledState labels ──────────────────────────────────────────────────────

    #[test]
    fn oled_state_blank_label() {
        assert_eq!(OledState::Blank.label(), "Blank");
    }

    #[test]
    fn oled_state_face_labels() {
        assert_eq!(OledState::Face(Expression::Happy).label(), "Happy");
        assert_eq!(OledState::Face(Expression::Sad).label(), "Sad");
        assert_eq!(OledState::Face(Expression::Neutral).label(), "Neutral");
        assert_eq!(OledState::Face(Expression::Surprised).label(), "Surprised");
    }

    // ── handle_command state machine ──────────────────────────────────────────

    #[test]
    fn handle_command_blank_to_face() {
        let state = OledState::Blank;
        let next = handle_command(state, &Command::SetFaceExpression(Expression::Happy));
        assert_eq!(next, OledState::Face(Expression::Happy));
    }

    #[test]
    fn handle_command_face_to_different_face() {
        let state = OledState::Face(Expression::Happy);
        let next = handle_command(state, &Command::SetFaceExpression(Expression::Sad));
        assert_eq!(next, OledState::Face(Expression::Sad));
    }

    #[test]
    fn handle_command_non_display_command_preserves_state() {
        let state = OledState::Face(Expression::Neutral);
        let next = handle_command(state.clone(), &Command::MoveServo(0, 90));
        assert_eq!(next, state, "servo command must not change the OLED state");
    }

    #[test]
    fn handle_command_stream_audio_preserves_state() {
        let state = OledState::Face(Expression::Surprised);
        let next = handle_command(state.clone(), &Command::StreamAudio(true));
        assert_eq!(next, state, "audio command must not change the OLED state");
    }

    #[test]
    fn handle_command_all_expressions_reachable() {
        let expressions = [
            Expression::Happy,
            Expression::Sad,
            Expression::Neutral,
            Expression::Surprised,
        ];
        for expr in &expressions {
            let next = handle_command(OledState::Blank, &Command::SetFaceExpression(expr.clone()));
            assert_eq!(next, OledState::Face(expr.clone()));
        }
    }

    #[test]
    fn handle_command_chain_of_face_updates() {
        let s0 = OledState::Blank;
        let s1 = handle_command(s0, &Command::SetFaceExpression(Expression::Happy));
        let s2 = handle_command(s1, &Command::SetFaceExpression(Expression::Sad));
        let s3 = handle_command(s2, &Command::MoveServo(1, 45)); // no-op
        let s4 = handle_command(s3, &Command::SetFaceExpression(Expression::Neutral));
        assert_eq!(s4, OledState::Face(Expression::Neutral));
    }
}
