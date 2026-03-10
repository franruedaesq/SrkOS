//! SrkOS – Bare-metal async companion OS for ESP32.
//!
//! # Crate layout
//!
//! | Module              | Description                                       |
//! |---------------------|---------------------------------------------------|
//! | [`ipc`]             | Global command bus and typed [`ipc::Command`] enum |
//! | [`hardware`]        | Boot-time I2C hardware discovery                  |
//! | [`tasks::oled`]     | SSD1306 OLED task and static framebuffer          |
//! | [`tasks::http`]     | HTTP request parsing, routing, connection limiting |
//! | [`tasks::espnow`]   | ESP-NOW frame payload parsing                     |
//!
//! # No-std / no-alloc design
//!
//! When compiled for the ESP32 target (`#[cfg(not(test))]`) this crate is
//! `no_std` and never touches the heap.  When compiled for the host
//! (`cargo test`) the std library is available so that the unit tests can
//! use `std::panic` and the `futures` executor.
#![cfg_attr(not(test), no_std)]

pub mod hardware;
pub mod ipc;
pub mod tasks;
