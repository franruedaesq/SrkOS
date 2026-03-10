//! HTTP server logic for Phase 3: Zero-RAM Micro Server.
//!
//! All parsing and routing functions are **pure** (no I/O) and fully
//! host-testable via `cargo test`.  The actual TCP socket management and
//! Wi-Fi initialisation live in the firmware-only section of `main.rs`.
//!
//! # Connection limiting
//!
//! [`MAX_CONNECTIONS`] caps simultaneous TCP connections at 2.
//! [`ConnectionGuard`] is a RAII guard that atomically increments
//! [`ACTIVE_CONNECTIONS`] on construction and decrements it on drop.
//! Callers that cannot acquire a guard **must** respond with
//! [`RESPONSE_503`] and close the socket immediately.
//!
//! # Request routing
//!
//! | Method | Path           | Action                                   |
//! |--------|----------------|------------------------------------------|
//! | GET    | `/`            | Serve flash-embedded `index.html`        |
//! | GET    | `/index.html`  | Serve flash-embedded `index.html`        |
//! | POST   | `/api/command` | Parse JSON body → enqueue on COMMAND_BUS |
//! | other  | any            | 404 Not Found                            |
//!
//! # JSON API payload format
//!
//! The Web UI posts one of the following JSON objects to `POST /api/command`:
//!
//! ```json
//! {"SetFaceExpression": "Happy"}
//! {"MoveServo": [0, 90]}
//! {"StreamAudio": true}
//! ```

use crate::ipc::{Command, Expression};
use core::sync::atomic::{AtomicUsize, Ordering};
use heapless::String;
use serde::Deserialize;

// ── Connection limiting ────────────────────────────────────────────────────────

/// Maximum simultaneous HTTP connections allowed by the server.
///
/// Each connection occupies dedicated RX/TX socket buffers in SRAM.
/// Two connections is the safe upper bound on the ESP32's 520 KB SRAM.
pub const MAX_CONNECTIONS: usize = 2;

/// Atomic count of currently open HTTP connections.
///
/// Managed exclusively via [`ConnectionGuard`]; do not modify directly.
pub static ACTIVE_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);

/// RAII connection guard.
///
/// Acquire via [`ConnectionGuard::try_acquire`].  The guard atomically
/// increments [`ACTIVE_CONNECTIONS`] on construction and decrements it when
/// dropped, ensuring the count never drifts even if the handler panics.
pub struct ConnectionGuard;

impl ConnectionGuard {
    /// Attempt to register a new connection.
    ///
    /// Returns `Some(guard)` when `ACTIVE_CONNECTIONS < MAX_CONNECTIONS`.
    /// Returns `None` when the limit is already reached; the caller **must**
    /// send [`RESPONSE_503`] and close the socket immediately.
    pub fn try_acquire() -> Option<Self> {
        ACTIVE_CONNECTIONS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < MAX_CONNECTIONS).then_some(n + 1)
            })
            .ok()
            .map(|_| ConnectionGuard)
    }

    /// Return the current active connection count (for diagnostics / tests).
    #[inline]
    pub fn active() -> usize {
        ACTIVE_CONNECTIONS.load(Ordering::Relaxed)
    }
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        ACTIVE_CONNECTIONS.fetch_sub(1, Ordering::AcqRel);
    }
}

// ── HTTP primitives ────────────────────────────────────────────────────────────

/// HTTP method extracted from the request line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Other,
}

/// Relevant parts of a parsed HTTP/1.x request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestParts<'a> {
    /// HTTP method.
    pub method: Method,
    /// Request path, with any query string stripped.
    pub path: &'a str,
    /// Raw body bytes (empty slice for requests without a body).
    pub body: &'a [u8],
}

/// Find the byte offset of the HTTP body (the position immediately after the
/// blank line `\r\n\r\n` that separates headers from the body).
///
/// Returns `None` if the header terminator is not present in `raw`.
fn find_body_start(raw: &[u8]) -> Option<usize> {
    raw.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|pos| pos + 4)
}

/// Parse an HTTP/1.x request from raw bytes.
///
/// Returns `None` when:
/// - the input is not valid UTF-8,
/// - there is no `\r\n` terminating the request line, or
/// - the request line does not contain at least two space-delimited tokens.
///
/// Query-string parameters (everything from `?` onward) are stripped from
/// the returned path so that route matching is query-string-agnostic.
pub fn parse_request(raw: &[u8]) -> Option<RequestParts<'_>> {
    let text = core::str::from_utf8(raw).ok()?;

    // Extract the request line (ends at the first \r\n).
    let line_end = text.find("\r\n")?;
    let request_line = &text[..line_end];

    // Parse: METHOD SP request-target SP HTTP-version
    let mut tokens = request_line.splitn(3, ' ');
    let method_str = tokens.next()?;
    let raw_path = tokens.next()?;

    let method = match method_str {
        "GET" => Method::Get,
        "POST" => Method::Post,
        _ => Method::Other,
    };

    // Strip any query-string so that route matching is unambiguous.
    let path = raw_path.split('?').next().unwrap_or(raw_path);

    // The body starts immediately after the blank line that ends the headers.
    let body = find_body_start(raw).map(|off| &raw[off..]).unwrap_or(&[]);

    Some(RequestParts { method, path, body })
}

// ── API command deserialization ────────────────────────────────────────────────

/// Deserializable form of the JSON command payload posted to `/api/command`.
///
/// The Web UI sends one of:
/// - `{"SetFaceExpression": "Happy" | "Sad" | "Neutral" | "Surprised"}`
/// - `{"MoveServo": [servo_id, angle_degrees]}`  (angle 0–180)
/// - `{"StreamAudio": true | false}`
/// - `{"StreamVideo": true | false}`
#[derive(Deserialize, Debug, PartialEq)]
pub enum ApiCommand {
    SetFaceExpression(String<16>),
    MoveServo((u8, u16)),
    StreamAudio(bool),
    StreamVideo(bool),
}

/// Convert an [`ApiCommand`] into a typed system [`Command`].
///
/// Returns `None` when:
/// - `SetFaceExpression` contains an unrecognised string, or
/// - `MoveServo` angle exceeds 180° (the valid range for a standard servo).
pub fn api_command_to_command(api: ApiCommand) -> Option<Command> {
    Some(match api {
        ApiCommand::SetFaceExpression(expr) => {
            let expression = match expr.as_str() {
                "Happy" => Expression::Happy,
                "Sad" => Expression::Sad,
                "Neutral" => Expression::Neutral,
                "Surprised" => Expression::Surprised,
                _ => return None,
            };
            Command::SetFaceExpression(expression)
        }
        ApiCommand::MoveServo((id, angle)) => {
            if angle > 180 {
                return None;
            }
            Command::MoveServo(id, angle)
        }
        ApiCommand::StreamAudio(on) => Command::StreamAudio(on),
        ApiCommand::StreamVideo(on) => Command::StreamVideo(on),
    })
}

/// Deserialise a raw JSON body slice into a typed system [`Command`].
///
/// Returns `None` when:
/// - the body is not valid JSON,
/// - the top-level key does not match a known command variant, or
/// - `SetFaceExpression` names an unrecognised expression.
pub fn parse_api_body(body: &[u8]) -> Option<Command> {
    let (api_cmd, _): (ApiCommand, _) = serde_json_core::from_slice(body).ok()?;
    api_command_to_command(api_cmd)
}

// ── Pre-formatted HTTP response headers ───────────────────────────────────────

/// `200 OK` header for HTML responses.
///
/// Write this header first, then write the `INDEX_HTML` body bytes.
pub const RESPONSE_200_HTML: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n";

/// `200 OK` header for JSON API responses.
///
/// Write this header first, then write a `{}` body.
pub const RESPONSE_200_JSON: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n";

/// Complete `400 Bad Request` response (no body needed).
pub const RESPONSE_400: &[u8] = b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n";

/// Complete `404 Not Found` response (no body needed).
pub const RESPONSE_404: &[u8] = b"HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n";

/// Complete `503 Service Unavailable` response.
///
/// Returned when [`ConnectionGuard::try_acquire`] fails (limit reached).
/// The `Retry-After: 1` header advises clients to retry after 1 second.
pub const RESPONSE_503: &[u8] =
    b"HTTP/1.1 503 Service Unavailable\r\nRetry-After: 1\r\nConnection: close\r\n\r\n";

/// `200 OK` header for MJPEG video streaming responses.
///
/// Write this header first, then write JPEG frames as multipart body parts:
///
/// ```text
/// --frame\r\n
/// Content-Type: image/jpeg\r\n
/// Content-Length: <len>\r\n
/// \r\n
/// <JPEG bytes>
/// \r\n
/// ```
///
/// The frames are sourced directly from [`crate::tasks::camera::DMA_JPEG_RING`]
/// via [`crate::tasks::camera::DMA_RING_BUFFER`] zero-copy pointer — no
/// intermediate copy is made.
pub const RESPONSE_200_MJPEG: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=frame\r\nConnection: keep-alive\r\n\r\n";

/// `200 OK` header for raw PCM audio streaming responses.
///
/// Write this header first, then stream audio frames directly from
/// [`crate::tasks::audio::I2S_PING`] / [`crate::tasks::audio::I2S_PONG`]
/// via [`crate::tasks::audio::PING_PONG_STATE`] zero-copy pointer — no
/// intermediate copy is made.
///
/// The audio is 16-bit PCM, mono, little-endian at
/// [`crate::tasks::audio::I2S_SAMPLE_RATE`] Hz.
pub const RESPONSE_200_AUDIO: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: audio/l16; rate=16000; channels=1\r\nConnection: keep-alive\r\n\r\n";

// ── Route dispatch ─────────────────────────────────────────────────────────────

/// Outcome of routing a single HTTP request.
///
/// The firmware task maps each variant to the appropriate socket I/O:
///
/// | Variant              | Action                                               |
/// |----------------------|------------------------------------------------------|
/// | `ServeIndex`         | Write [`RESPONSE_200_HTML`] then `INDEX_HTML`        |
/// | `CommandAccepted(c)` | Enqueue `c` on `COMMAND_BUS`; write `RESPONSE_200_JSON` + `{}` |
/// | `ServeVideoStream`   | Write [`RESPONSE_200_MJPEG`]; stream JPEG frames via zero-copy DMA pointer |
/// | `ServeAudioStream`   | Write [`RESPONSE_200_AUDIO`]; stream PCM frames via zero-copy DMA pointer  |
/// | `BadRequest`         | Write [`RESPONSE_400`]                               |
/// | `NotFound`           | Write [`RESPONSE_404`]                               |
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteResult {
    /// Serve the flash-embedded `index.html`.
    ServeIndex,
    /// A valid command was parsed; the caller should enqueue it on `COMMAND_BUS`.
    CommandAccepted(Command),
    /// Stream MJPEG video frames from the DMA ring buffer (zero-copy).
    ServeVideoStream,
    /// Stream raw PCM audio samples from the ping-pong buffers (zero-copy).
    ServeAudioStream,
    /// The `POST /api/command` body was not valid JSON or named an unknown command.
    BadRequest,
    /// No route matched.
    NotFound,
}

/// Route a parsed HTTP request to a handler outcome (pure, no I/O).
///
/// This function is free of I/O and side-effects so it can be unit-tested on
/// the host without any Wi-Fi hardware or TCP stack.
pub fn route_request(req: &RequestParts<'_>) -> RouteResult {
    match (&req.method, req.path) {
        (Method::Get, "/") | (Method::Get, "/index.html") => RouteResult::ServeIndex,
        (Method::Get, "/api/stream/video") => RouteResult::ServeVideoStream,
        (Method::Get, "/api/stream/audio") => RouteResult::ServeAudioStream,
        (Method::Post, "/api/command") => match parse_api_body(req.body) {
            Some(cmd) => RouteResult::CommandAccepted(cmd),
            None => RouteResult::BadRequest,
        },
        _ => RouteResult::NotFound,
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{Command, Expression};

    // ── find_body_start ───────────────────────────────────────────────────────

    #[test]
    fn find_body_start_locates_double_crlf() {
        // "GET / HTTP/1.1\r\n" = 16 bytes, "Host: 192.168.4.1\r\n" = 19 bytes,
        // "\r\n" blank line at positions 35-36 → body at 37.
        let raw = b"GET / HTTP/1.1\r\nHost: 192.168.4.1\r\n\r\nbody";
        assert_eq!(find_body_start(raw), Some(37));
    }

    #[test]
    fn find_body_start_returns_none_without_terminator() {
        let raw = b"GET / HTTP/1.1\r\nHost: 192.168.4.1\r\n";
        assert_eq!(find_body_start(raw), None);
    }

    #[test]
    fn find_body_start_empty_body_after_terminator() {
        // "\r\n\r\n" at positions 14-17; body starts at 18.
        let raw = b"GET / HTTP/1.1\r\n\r\n";
        assert_eq!(find_body_start(raw), Some(18));
    }

    // ── parse_request ─────────────────────────────────────────────────────────

    #[test]
    fn parse_request_get_root() {
        let raw = b"GET / HTTP/1.1\r\nHost: 192.168.4.1\r\n\r\n";
        let req = parse_request(raw).unwrap();
        assert_eq!(req.method, Method::Get);
        assert_eq!(req.path, "/");
        assert!(req.body.is_empty());
    }

    #[test]
    fn parse_request_post_api_command_with_body() {
        let raw =
            b"POST /api/command HTTP/1.1\r\nContent-Type: application/json\r\n\r\n{\"SetFaceExpression\":\"Happy\"}";
        let req = parse_request(raw).unwrap();
        assert_eq!(req.method, Method::Post);
        assert_eq!(req.path, "/api/command");
        assert_eq!(req.body, b"{\"SetFaceExpression\":\"Happy\"}");
    }

    #[test]
    fn parse_request_strips_query_string() {
        let raw = b"GET /?debug=1 HTTP/1.1\r\n\r\n";
        let req = parse_request(raw).unwrap();
        assert_eq!(req.path, "/");
    }

    #[test]
    fn parse_request_index_html_path() {
        let raw = b"GET /index.html HTTP/1.1\r\n\r\n";
        let req = parse_request(raw).unwrap();
        assert_eq!(req.path, "/index.html");
        assert_eq!(req.method, Method::Get);
    }

    #[test]
    fn parse_request_returns_none_for_empty_input() {
        assert!(parse_request(b"").is_none());
    }

    #[test]
    fn parse_request_returns_none_for_invalid_utf8() {
        assert!(parse_request(b"\xff\xfe").is_none());
    }

    #[test]
    fn parse_request_other_method() {
        let raw = b"DELETE /foo HTTP/1.1\r\n\r\n";
        let req = parse_request(raw).unwrap();
        assert_eq!(req.method, Method::Other);
        assert_eq!(req.path, "/foo");
    }

    // ── parse_api_body ────────────────────────────────────────────────────────

    #[test]
    fn parse_api_body_set_face_happy() {
        let body = br#"{"SetFaceExpression":"Happy"}"#;
        assert_eq!(
            parse_api_body(body),
            Some(Command::SetFaceExpression(Expression::Happy))
        );
    }

    #[test]
    fn parse_api_body_set_face_sad() {
        let body = br#"{"SetFaceExpression":"Sad"}"#;
        assert_eq!(
            parse_api_body(body),
            Some(Command::SetFaceExpression(Expression::Sad))
        );
    }

    #[test]
    fn parse_api_body_set_face_neutral() {
        let body = br#"{"SetFaceExpression":"Neutral"}"#;
        assert_eq!(
            parse_api_body(body),
            Some(Command::SetFaceExpression(Expression::Neutral))
        );
    }

    #[test]
    fn parse_api_body_set_face_surprised() {
        let body = br#"{"SetFaceExpression":"Surprised"}"#;
        assert_eq!(
            parse_api_body(body),
            Some(Command::SetFaceExpression(Expression::Surprised))
        );
    }

    #[test]
    fn parse_api_body_move_servo() {
        let body = br#"{"MoveServo":[1,90]}"#;
        assert_eq!(parse_api_body(body), Some(Command::MoveServo(1, 90)));
    }

    #[test]
    fn parse_api_body_move_servo_max_angle() {
        let body = br#"{"MoveServo":[7,180]}"#;
        assert_eq!(parse_api_body(body), Some(Command::MoveServo(7, 180)));
    }

    #[test]
    fn parse_api_body_move_servo_out_of_range_angle_returns_none() {
        let body = br#"{"MoveServo":[0,181]}"#;
        assert_eq!(
            parse_api_body(body),
            None,
            "angle > 180 must be rejected to protect servo hardware"
        );
    }

    #[test]
    fn parse_api_body_stream_audio_start() {
        let body = br#"{"StreamAudio":true}"#;
        assert_eq!(parse_api_body(body), Some(Command::StreamAudio(true)));
    }

    #[test]
    fn parse_api_body_stream_audio_stop() {
        let body = br#"{"StreamAudio":false}"#;
        assert_eq!(parse_api_body(body), Some(Command::StreamAudio(false)));
    }

    #[test]
    fn parse_api_body_invalid_json_returns_none() {
        assert_eq!(parse_api_body(b"not json"), None);
    }

    #[test]
    fn parse_api_body_empty_returns_none() {
        assert_eq!(parse_api_body(b""), None);
    }

    #[test]
    fn parse_api_body_unknown_expression_returns_none() {
        let body = br#"{"SetFaceExpression":"Angry"}"#;
        assert_eq!(parse_api_body(body), None);
    }

    #[test]
    fn parse_api_body_unknown_command_key_returns_none() {
        let body = br#"{"UnknownCommand":"value"}"#;
        assert_eq!(parse_api_body(body), None);
    }

    // ── route_request ─────────────────────────────────────────────────────────

    #[test]
    fn route_get_root_serves_index() {
        let req = RequestParts {
            method: Method::Get,
            path: "/",
            body: &[],
        };
        assert_eq!(route_request(&req), RouteResult::ServeIndex);
    }

    #[test]
    fn route_get_index_html_serves_index() {
        let req = RequestParts {
            method: Method::Get,
            path: "/index.html",
            body: &[],
        };
        assert_eq!(route_request(&req), RouteResult::ServeIndex);
    }

    #[test]
    fn route_post_api_command_set_face() {
        let body = br#"{"SetFaceExpression":"Neutral"}"#;
        let req = RequestParts {
            method: Method::Post,
            path: "/api/command",
            body,
        };
        assert_eq!(
            route_request(&req),
            RouteResult::CommandAccepted(Command::SetFaceExpression(Expression::Neutral))
        );
    }

    #[test]
    fn route_post_api_command_move_servo() {
        let body = br#"{"MoveServo":[3,45]}"#;
        let req = RequestParts {
            method: Method::Post,
            path: "/api/command",
            body,
        };
        assert_eq!(
            route_request(&req),
            RouteResult::CommandAccepted(Command::MoveServo(3, 45))
        );
    }

    #[test]
    fn route_post_api_command_stream_audio() {
        let body = br#"{"StreamAudio":true}"#;
        let req = RequestParts {
            method: Method::Post,
            path: "/api/command",
            body,
        };
        assert_eq!(
            route_request(&req),
            RouteResult::CommandAccepted(Command::StreamAudio(true))
        );
    }

    #[test]
    fn route_post_api_command_invalid_json_returns_bad_request() {
        let req = RequestParts {
            method: Method::Post,
            path: "/api/command",
            body: b"bad",
        };
        assert_eq!(route_request(&req), RouteResult::BadRequest);
    }

    #[test]
    fn route_post_api_command_empty_body_returns_bad_request() {
        let req = RequestParts {
            method: Method::Post,
            path: "/api/command",
            body: &[],
        };
        assert_eq!(route_request(&req), RouteResult::BadRequest);
    }

    #[test]
    fn route_unknown_path_returns_not_found() {
        let req = RequestParts {
            method: Method::Get,
            path: "/robots.txt",
            body: &[],
        };
        assert_eq!(route_request(&req), RouteResult::NotFound);
    }

    #[test]
    fn route_post_to_root_returns_not_found() {
        let req = RequestParts {
            method: Method::Post,
            path: "/",
            body: &[],
        };
        assert_eq!(route_request(&req), RouteResult::NotFound);
    }

    // ── Full request round-trips ──────────────────────────────────────────────

    #[test]
    fn full_roundtrip_set_face_happy() {
        let raw = b"POST /api/command HTTP/1.1\r\nContent-Type: application/json\r\n\r\n{\"SetFaceExpression\":\"Happy\"}";
        let req = parse_request(raw).unwrap();
        assert_eq!(
            route_request(&req),
            RouteResult::CommandAccepted(Command::SetFaceExpression(Expression::Happy))
        );
    }

    #[test]
    fn full_roundtrip_move_servo() {
        let raw = b"POST /api/command HTTP/1.1\r\nContent-Type: application/json\r\n\r\n{\"MoveServo\":[2,135]}";
        let req = parse_request(raw).unwrap();
        assert_eq!(
            route_request(&req),
            RouteResult::CommandAccepted(Command::MoveServo(2, 135))
        );
    }

    #[test]
    fn full_roundtrip_stream_audio_start() {
        let raw = b"POST /api/command HTTP/1.1\r\nContent-Type: application/json\r\n\r\n{\"StreamAudio\":true}";
        let req = parse_request(raw).unwrap();
        assert_eq!(
            route_request(&req),
            RouteResult::CommandAccepted(Command::StreamAudio(true))
        );
    }

    #[test]
    fn full_roundtrip_stream_video_start() {
        let raw = b"POST /api/command HTTP/1.1\r\nContent-Type: application/json\r\n\r\n{\"StreamVideo\":true}";
        let req = parse_request(raw).unwrap();
        assert_eq!(
            route_request(&req),
            RouteResult::CommandAccepted(Command::StreamVideo(true))
        );
    }

    #[test]
    fn full_roundtrip_get_root() {
        let raw = b"GET / HTTP/1.1\r\nHost: 192.168.4.1\r\n\r\n";
        let req = parse_request(raw).unwrap();
        assert_eq!(route_request(&req), RouteResult::ServeIndex);
    }

    // ── Connection guard ──────────────────────────────────────────────────────

    #[test]
    fn connection_guard_allows_up_to_max() {
        ACTIVE_CONNECTIONS.store(0, Ordering::SeqCst);
        let g1 = ConnectionGuard::try_acquire();
        let g2 = ConnectionGuard::try_acquire();
        let g3 = ConnectionGuard::try_acquire();
        assert!(g1.is_some(), "first connection must be accepted");
        assert!(g2.is_some(), "second connection must be accepted");
        assert!(
            g3.is_none(),
            "third connection must be rejected (limit = 2)"
        );
        drop(g1);
        drop(g2);
        drop(g3);
    }

    #[test]
    fn connection_guard_decrements_on_drop() {
        ACTIVE_CONNECTIONS.store(0, Ordering::SeqCst);
        {
            let _g = ConnectionGuard::try_acquire().unwrap();
            assert_eq!(ConnectionGuard::active(), 1);
        }
        assert_eq!(
            ConnectionGuard::active(),
            0,
            "counter must return to 0 after drop"
        );
    }

    #[test]
    fn connection_guard_reacquires_after_release() {
        ACTIVE_CONNECTIONS.store(0, Ordering::SeqCst);
        let g1 = ConnectionGuard::try_acquire().unwrap();
        let g2 = ConnectionGuard::try_acquire().unwrap();
        assert!(
            ConnectionGuard::try_acquire().is_none(),
            "limit must be enforced"
        );
        drop(g1);
        let g3 = ConnectionGuard::try_acquire();
        assert!(
            g3.is_some(),
            "a slot must become available after releasing a guard"
        );
        drop(g2);
        drop(g3);
    }

    #[test]
    fn connection_guard_max_connections_constant_is_two() {
        assert_eq!(MAX_CONNECTIONS, 2);
    }

    // ── Response constant sanity checks ───────────────────────────────────────

    #[test]
    fn response_200_html_starts_with_http_200() {
        assert!(RESPONSE_200_HTML.starts_with(b"HTTP/1.1 200 OK"));
    }

    #[test]
    fn response_200_html_contains_html_content_type() {
        let text = core::str::from_utf8(RESPONSE_200_HTML).unwrap();
        assert!(text.contains("text/html"));
    }

    #[test]
    fn response_200_json_contains_json_content_type() {
        let text = core::str::from_utf8(RESPONSE_200_JSON).unwrap();
        assert!(text.contains("application/json"));
    }

    #[test]
    fn response_400_starts_with_http_400() {
        assert!(RESPONSE_400.starts_with(b"HTTP/1.1 400"));
    }

    #[test]
    fn response_404_starts_with_http_404() {
        assert!(RESPONSE_404.starts_with(b"HTTP/1.1 404"));
    }

    #[test]
    fn response_503_contains_retry_after() {
        let text = core::str::from_utf8(RESPONSE_503).unwrap();
        assert!(text.contains("Retry-After"));
    }

    #[test]
    fn response_200_mjpeg_starts_with_http_200() {
        assert!(RESPONSE_200_MJPEG.starts_with(b"HTTP/1.1 200 OK"));
    }

    #[test]
    fn response_200_mjpeg_contains_mjpeg_content_type() {
        let text = core::str::from_utf8(RESPONSE_200_MJPEG).unwrap();
        assert!(text.contains("multipart/x-mixed-replace"));
    }

    #[test]
    fn response_200_audio_starts_with_http_200() {
        assert!(RESPONSE_200_AUDIO.starts_with(b"HTTP/1.1 200 OK"));
    }

    #[test]
    fn response_200_audio_contains_audio_l16_content_type() {
        let text = core::str::from_utf8(RESPONSE_200_AUDIO).unwrap();
        assert!(text.contains("audio/l16"));
    }

    #[test]
    fn all_responses_terminate_with_double_crlf() {
        for resp in &[
            RESPONSE_200_HTML,
            RESPONSE_200_JSON,
            RESPONSE_200_MJPEG,
            RESPONSE_200_AUDIO,
            RESPONSE_400,
            RESPONSE_404,
            RESPONSE_503,
        ] {
            assert!(
                resp.ends_with(b"\r\n\r\n"),
                "every response header must end with \\r\\n\\r\\n"
            );
        }
    }

    // ── Streaming route dispatch ──────────────────────────────────────────────

    #[test]
    fn route_get_video_stream_serves_video_stream() {
        let req = RequestParts {
            method: Method::Get,
            path: "/api/stream/video",
            body: &[],
        };
        assert_eq!(route_request(&req), RouteResult::ServeVideoStream);
    }

    #[test]
    fn route_get_audio_stream_serves_audio_stream() {
        let req = RequestParts {
            method: Method::Get,
            path: "/api/stream/audio",
            body: &[],
        };
        assert_eq!(route_request(&req), RouteResult::ServeAudioStream);
    }

    #[test]
    fn route_post_video_stream_returns_not_found() {
        let req = RequestParts {
            method: Method::Post,
            path: "/api/stream/video",
            body: &[],
        };
        assert_eq!(route_request(&req), RouteResult::NotFound);
    }

    #[test]
    fn route_post_audio_stream_returns_not_found() {
        let req = RequestParts {
            method: Method::Post,
            path: "/api/stream/audio",
            body: &[],
        };
        assert_eq!(route_request(&req), RouteResult::NotFound);
    }

    // ── StreamVideo API command ───────────────────────────────────────────────

    #[test]
    fn parse_api_body_stream_video_start() {
        let body = br#"{"StreamVideo":true}"#;
        assert_eq!(parse_api_body(body), Some(Command::StreamVideo(true)));
    }

    #[test]
    fn parse_api_body_stream_video_stop() {
        let body = br#"{"StreamVideo":false}"#;
        assert_eq!(parse_api_body(body), Some(Command::StreamVideo(false)));
    }

    #[test]
    fn route_post_api_command_stream_video() {
        let body = br#"{"StreamVideo":true}"#;
        let req = RequestParts {
            method: Method::Post,
            path: "/api/command",
            body,
        };
        assert_eq!(
            route_request(&req),
            RouteResult::CommandAccepted(Command::StreamVideo(true))
        );
    }
}
