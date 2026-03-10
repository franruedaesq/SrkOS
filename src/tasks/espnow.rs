//! ESP-NOW receiver logic for Phase 3: Connectivity.
//!
//! Remote "master" devices (e.g. ESP32-S3) transmit command payloads over
//! ESP-NOW.  The payload format is identical to the HTTP API JSON body so
//! that [`crate::tasks::http::parse_api_body`] serves as the single parser
//! for both channels.
//!
//! # Payload format
//!
//! ESP-NOW frames carry a raw byte payload.  SrkOS expects the payload to be
//! a UTF-8 JSON object in one of the following forms:
//!
//! ```json
//! {"SetFaceExpression": "Happy"}
//! {"MoveServo": [0, 90]}
//! {"StreamAudio": true}
//! ```
//!
//! Any frame whose payload cannot be decoded is silently discarded.
//!
//! # Firmware task (main.rs)
//!
//! The `espnow_task` in `main.rs` initialises the ESP-NOW peer via `esp-wifi`
//! and forwards each received frame's payload to [`parse_espnow_payload`].
//! A `Some(Command)` result is enqueued on [`crate::ipc::COMMAND_BUS`].
//!
//! # Dependency note
//!
//! `esp-wifi` with the `esp-now` feature must be added to `Cargo.toml` before
//! building for the Xtensa target:
//!
//! ```toml
//! esp-wifi = { version = "0.13", default-features = false, features = [
//!     "esp32", "phy-enable-usb", "esp-now",
//! ] }
//! ```

use crate::ipc::Command;
use crate::tasks::http::parse_api_body;

/// Parse a raw ESP-NOW frame payload into a typed system [`Command`].
///
/// The `data` slice must contain a UTF-8 JSON object matching one of the
/// formats documented in the module-level doc.  Returns `None` for any
/// malformed, invalid-UTF-8, or unrecognised payload — the caller should
/// simply discard such frames.
///
/// # Example
///
/// ```rust
/// use srkos::tasks::espnow::parse_espnow_payload;
/// use srkos::ipc::{Command, Expression};
///
/// let payload = br#"{"SetFaceExpression":"Happy"}"#;
/// assert_eq!(
///     parse_espnow_payload(payload),
///     Some(Command::SetFaceExpression(Expression::Happy))
/// );
/// ```
pub fn parse_espnow_payload(data: &[u8]) -> Option<Command> {
    parse_api_body(data)
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{Command, Expression};

    #[test]
    fn parse_espnow_set_face_happy() {
        let payload = br#"{"SetFaceExpression":"Happy"}"#;
        assert_eq!(
            parse_espnow_payload(payload),
            Some(Command::SetFaceExpression(Expression::Happy))
        );
    }

    #[test]
    fn parse_espnow_set_face_sad() {
        let payload = br#"{"SetFaceExpression":"Sad"}"#;
        assert_eq!(
            parse_espnow_payload(payload),
            Some(Command::SetFaceExpression(Expression::Sad))
        );
    }

    #[test]
    fn parse_espnow_set_face_neutral() {
        let payload = br#"{"SetFaceExpression":"Neutral"}"#;
        assert_eq!(
            parse_espnow_payload(payload),
            Some(Command::SetFaceExpression(Expression::Neutral))
        );
    }

    #[test]
    fn parse_espnow_set_face_surprised() {
        let payload = br#"{"SetFaceExpression":"Surprised"}"#;
        assert_eq!(
            parse_espnow_payload(payload),
            Some(Command::SetFaceExpression(Expression::Surprised))
        );
    }

    #[test]
    fn parse_espnow_move_servo() {
        let payload = br#"{"MoveServo":[0,45]}"#;
        assert_eq!(
            parse_espnow_payload(payload),
            Some(Command::MoveServo(0, 45))
        );
    }

    #[test]
    fn parse_espnow_stream_audio_start() {
        let payload = br#"{"StreamAudio":true}"#;
        assert_eq!(
            parse_espnow_payload(payload),
            Some(Command::StreamAudio(true))
        );
    }

    #[test]
    fn parse_espnow_stream_audio_stop() {
        let payload = br#"{"StreamAudio":false}"#;
        assert_eq!(
            parse_espnow_payload(payload),
            Some(Command::StreamAudio(false))
        );
    }

    #[test]
    fn parse_espnow_invalid_payload_returns_none() {
        assert_eq!(parse_espnow_payload(b"garbage"), None);
    }

    #[test]
    fn parse_espnow_empty_payload_returns_none() {
        assert_eq!(parse_espnow_payload(b""), None);
    }

    #[test]
    fn parse_espnow_unknown_expression_returns_none() {
        let payload = br#"{"SetFaceExpression":"Wink"}"#;
        assert_eq!(parse_espnow_payload(payload), None);
    }

    #[test]
    fn parse_espnow_all_face_expressions() {
        let cases: &[(&[u8], Expression)] = &[
            (br#"{"SetFaceExpression":"Happy"}"#, Expression::Happy),
            (br#"{"SetFaceExpression":"Sad"}"#, Expression::Sad),
            (br#"{"SetFaceExpression":"Neutral"}"#, Expression::Neutral),
            (
                br#"{"SetFaceExpression":"Surprised"}"#,
                Expression::Surprised,
            ),
        ];
        for (payload, expected) in cases {
            assert_eq!(
                parse_espnow_payload(payload),
                Some(Command::SetFaceExpression(expected.clone()))
            );
        }
    }

    #[test]
    fn parse_espnow_invalid_utf8_returns_none() {
        assert_eq!(parse_espnow_payload(b"\xff\xfe"), None);
    }
}
