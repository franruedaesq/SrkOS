//! Async task implementations.
//!
//! Tasks are spawned conditionally based on the result of the boot-time
//! hardware discovery phase (see [`crate::hardware`]).
//!
//! | Module        | Description                                             |
//! |---------------|---------------------------------------------------------|
//! | [`oled`]      | SSD1306 OLED state machine and framebuffer              |
//! | [`http`]      | HTTP request parsing, routing, and connection limiting  |
//! | [`espnow`]    | ESP-NOW frame payload parsing                           |

pub mod espnow;
pub mod http;
pub mod oled;
