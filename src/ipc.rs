//! IPC channel definitions for SrkOS.
//!
//! The [`COMMAND_BUS`] is the single, statically allocated async channel that
//! decouples producers (network, ESP-NOW tasks) from consumers (OLED, servo,
//! audio tasks).  It holds at most 8 messages and **never** allocates heap
//! memory.
//!
//! # Usage
//!
//! **Producer** (e.g. network task):
//! ```rust,ignore
//! COMMAND_BUS.send(Command::SetFaceExpression(Expression::Happy)).await;
//! ```
//!
//! **Consumer** (e.g. OLED task):
//! ```rust,ignore
//! let cmd = COMMAND_BUS.receive().await;
//! ```

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;

// ── Types ─────────────────────────────────────────────────────────────────────

/// Facial expression rendered on the SSD1306 OLED.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(test, derive(Copy))]
pub enum Expression {
    Happy,
    Sad,
    Neutral,
    Surprised,
}

/// System-wide commands dispatched over the [`COMMAND_BUS`].
///
/// Commands are produced by the network / ESP-NOW task and consumed
/// asynchronously by hardware tasks (OLED, servos, audio).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Change the OLED face expression.
    SetFaceExpression(Expression),
    /// Move a servo to the specified angle.
    ///
    /// Fields: `(servo_id, angle_degrees)` where angle is 0–180.
    MoveServo(u8, u16),
    /// Start (`true`) or stop (`false`) audio streaming.
    StreamAudio(bool),
    /// Start (`true`) or stop (`false`) MJPEG video streaming.
    StreamVideo(bool),
}

// ── Global command bus ────────────────────────────────────────────────────────

/// Mutex type used throughout the OS for shared state.
///
/// [`CriticalSectionRawMutex`] works across all Embassy targets (Xtensa,
/// RISC-V, ARM) and is safe from both interrupt and task contexts.
pub type SystemMutex = CriticalSectionRawMutex;

/// Statically allocated async message bus (capacity = 8 commands).
///
/// The channel is a FIFO queue.  When the queue is full, `try_send` returns
/// an error; `send` suspends the caller until a slot is free.
pub static COMMAND_BUS: Channel<SystemMutex, Command, 8> = Channel::new();

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use embassy_sync::blocking_mutex::raw::NoopRawMutex;
    use embassy_sync::channel::Channel;

    /// A single-threaded channel alias suitable for host-side unit tests.
    /// `NoopRawMutex` provides no actual synchronisation (safe for one thread).
    type TestChannel<T, const N: usize> = Channel<NoopRawMutex, T, N>;

    // ── Command equality ──────────────────────────────────────────────────────

    #[test]
    fn command_set_face_expression_equality() {
        assert_eq!(
            Command::SetFaceExpression(Expression::Happy),
            Command::SetFaceExpression(Expression::Happy)
        );
        assert_ne!(
            Command::SetFaceExpression(Expression::Happy),
            Command::SetFaceExpression(Expression::Sad)
        );
    }

    #[test]
    fn command_move_servo_equality() {
        assert_eq!(Command::MoveServo(0, 90), Command::MoveServo(0, 90));
        assert_ne!(Command::MoveServo(0, 90), Command::MoveServo(0, 45));
        assert_ne!(Command::MoveServo(0, 90), Command::MoveServo(1, 90));
    }

    #[test]
    fn command_stream_audio_equality() {
        assert_eq!(Command::StreamAudio(true), Command::StreamAudio(true));
        assert_ne!(Command::StreamAudio(true), Command::StreamAudio(false));
    }

    // ── Channel send / receive ────────────────────────────────────────────────

    #[test]
    fn channel_send_receive_roundtrip() {
        let bus: TestChannel<Command, 8> = Channel::new();

        bus.try_send(Command::SetFaceExpression(Expression::Happy))
            .expect("channel should accept a message when not full");

        let received = bus.try_receive().expect("channel should contain a message");
        assert_eq!(received, Command::SetFaceExpression(Expression::Happy));
    }

    #[test]
    fn channel_fifo_ordering_preserved() {
        let bus: TestChannel<Command, 8> = Channel::new();

        bus.try_send(Command::MoveServo(1, 0)).unwrap();
        bus.try_send(Command::MoveServo(2, 90)).unwrap();
        bus.try_send(Command::MoveServo(3, 180)).unwrap();

        assert_eq!(bus.try_receive().unwrap(), Command::MoveServo(1, 0));
        assert_eq!(bus.try_receive().unwrap(), Command::MoveServo(2, 90));
        assert_eq!(bus.try_receive().unwrap(), Command::MoveServo(3, 180));
    }

    #[test]
    fn channel_respects_compile_time_capacity() {
        let bus: TestChannel<Command, 8> = Channel::new();

        // Fill all 8 slots
        for id in 0..8_u8 {
            bus.try_send(Command::MoveServo(id, 90))
                .unwrap_or_else(|_| panic!("slot {id} should be free"));
        }

        // The 9th message must be rejected without panicking
        assert!(
            bus.try_send(Command::StreamAudio(true)).is_err(),
            "channel must reject messages when full"
        );
    }

    #[test]
    fn channel_receive_from_empty_returns_error() {
        let bus: TestChannel<Command, 8> = Channel::new();
        assert!(
            bus.try_receive().is_err(),
            "receiving from an empty channel must return an error, not panic"
        );
    }

    #[test]
    fn channel_can_be_drained_after_fill() {
        let bus: TestChannel<Command, 4> = Channel::new();

        for i in 0..4_u8 {
            bus.try_send(Command::MoveServo(i, u16::from(i) * 45)).unwrap();
        }
        for i in 0..4_u8 {
            let cmd = bus.try_receive().unwrap();
            assert_eq!(cmd, Command::MoveServo(i, u16::from(i) * 45));
        }
        assert!(bus.try_receive().is_err(), "channel should be empty after drain");
    }

    // ── Verify the global COMMAND_BUS static is well-formed ──────────────────

    #[test]
    fn command_bus_static_is_accessible() {
        // Verify the static compiles and is reachable.  We use try_receive on
        // the global bus (which starts empty) to confirm it is a valid Channel.
        assert!(
            COMMAND_BUS.try_receive().is_err(),
            "the global COMMAND_BUS should start empty"
        );
    }
}
