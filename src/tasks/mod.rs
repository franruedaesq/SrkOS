//! Async task implementations.
//!
//! Tasks are spawned conditionally based on the result of the boot-time
//! hardware discovery phase (see [`crate::hardware`]).

pub mod oled;
