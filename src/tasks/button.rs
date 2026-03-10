//! BOOT button task — reads GPIO 0 and sends logical navigation commands.
//!
//! # Hardware
//!
//! The ESP32's BOOT button is wired to **GPIO 0** with an internal pull-up
//! resistor.  It reads HIGH when idle and goes LOW while pressed.
//!
//! # Button logic
//!
//! | Gesture                     | Threshold     | Command sent         |
//! |-----------------------------|---------------|----------------------|
//! | Single click (held < 500 ms) | [`CLICK_MAX_MS`] | [`Command::MenuNextItem`] |
//! | Long press  (held ≥ 2000 ms) | [`LONG_PRESS_MS`] | [`Command::MenuSelect`]   |
//! | Medium hold (500–1999 ms)   | —             | Ignored (no command) |
//!
//! After each detected gesture a [`DEBOUNCE_MS`]-millisecond async delay
//! prevents phantom re-triggers without blocking other Embassy tasks.
//!
//! # Separation of concerns
//!
//! This task only reads electrical edges and sends logical commands.  It has
//! no knowledge of the menu state — that belongs entirely to the OLED task.
//! If this task crashes, the OLED task continues animating the face.
//!
//! # Firmware-only
//!
//! All code in this module is gated on the `firmware` feature because it
//! depends on `esp-hal` GPIO types that are not available on the host.

// ── Timing constants ──────────────────────────────────────────────────────────

/// Maximum press duration in milliseconds that counts as a single click.
pub const CLICK_MAX_MS: u64 = 500;

/// Minimum press duration in milliseconds that counts as a long press.
pub const LONG_PRESS_MS: u64 = 2000;

/// Async debounce delay in milliseconds after each detected gesture.
pub const DEBOUNCE_MS: u64 = 50;

/// GPIO number of the ESP32 BOOT button.
pub const BOOT_BUTTON_GPIO: u8 = 0;

// ── Async task (firmware-only) ────────────────────────────────────────────────

/// Await button edges on GPIO 0, classify gestures, and publish commands.
///
/// Call this function from the `button_task` Embassy task wrapper in
/// `main.rs`.  The GPIO pin should be initialised with [`Pull::Up`] before
/// being passed here.
///
/// ```rust,ignore
/// use esp_hal::gpio::{Input, Pull};
/// let boot = Input::new(peripherals.GPIO0, Pull::Up);
/// spawner.spawn(button_task(boot)).unwrap();
/// ```
#[cfg(feature = "firmware")]
pub async fn button_task_loop(mut boot_pin: esp_hal::gpio::Input<'static>) {
    use embassy_time::{Duration, Instant, Timer};

    use crate::ipc::{Command, COMMAND_BUS};

    loop {
        // Wait for the button to be pressed (active-low: HIGH → LOW edge).
        boot_pin.wait_for_falling_edge().await;
        let press_start = Instant::now();

        // Wait for the button to be released (LOW → HIGH edge).
        boot_pin.wait_for_rising_edge().await;
        let held_ms = (Instant::now() - press_start).as_millis();

        if held_ms < CLICK_MAX_MS {
            // Short click (< CLICK_MAX_MS): move to the next menu item.
            COMMAND_BUS.send(Command::MenuNextItem).await;
        } else if held_ms >= LONG_PRESS_MS {
            // Long press (≥ LONG_PRESS_MS): open the menu / select highlighted item.
            COMMAND_BUS.send(Command::MenuSelect).await;
        }
        // Presses in the range [CLICK_MAX_MS, LONG_PRESS_MS) are intentionally
        // ignored — they are ambiguous gestures that should not trigger any action.

        // Async debounce — yields to other tasks, never blocks.
        Timer::after_millis(DEBOUNCE_MS).await;
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timing_constants_are_consistent() {
        assert!(
            CLICK_MAX_MS < LONG_PRESS_MS,
            "click threshold must be shorter than long-press threshold"
        );
        assert!(
            DEBOUNCE_MS < CLICK_MAX_MS,
            "debounce must be shorter than click threshold to avoid masking clicks"
        );
    }

    #[test]
    fn click_max_ms_is_500() {
        assert_eq!(CLICK_MAX_MS, 500);
    }

    #[test]
    fn long_press_ms_is_2000() {
        assert_eq!(LONG_PRESS_MS, 2000);
    }

    #[test]
    fn debounce_ms_is_50() {
        assert_eq!(DEBOUNCE_MS, 50);
    }

    #[test]
    fn boot_button_gpio_is_zero() {
        assert_eq!(BOOT_BUTTON_GPIO, 0);
    }
}
