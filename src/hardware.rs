//! Hardware auto-discovery via I2C bus probing.
//!
//! At boot the OS calls [`probe_hardware`] before spawning any tasks.  Each
//! peripheral is probed with a non-blocking 1-byte read.  A device that does
//! not ACK is simply absent – the probe never panics and never blocks other
//! tasks indefinitely.
//!
//! # Example (firmware)
//!
//! ```rust,ignore
//! let hw = probe_hardware(&mut i2c_bus).await;
//! if hw.oled {
//!     spawner.spawn(oled_task()).unwrap();
//! } else {
//!     esp_println::println!("No Screen Detected");
//! }
//! ```
//!
//! # Testing
//!
//! The function is fully testable on the host by passing a mock [`I2c`]
//! implementation that either returns `Ok(())` or an error.

use embedded_hal_async::i2c::I2c;

// ── Well-known I2C addresses ──────────────────────────────────────────────────

/// I2C address of the SSD1306 OLED display (128×64, 1-bit monochrome).
pub const SSD1306_ADDR: u8 = 0x3C;

/// SCCB (I2C-compatible) address of the OV2640 camera module.
pub const CAMERA_SCCB_ADDR: u8 = 0x30;

/// I2C address of a generic I2S audio codec.
pub const AUDIO_CODEC_ADDR: u8 = 0x1A;

// ── Result type ───────────────────────────────────────────────────────────────

/// Describes which peripherals were detected during the boot-time I2C scan.
///
/// All fields default to `false`; only devices that ACK their probe address
/// are marked `true`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DetectedHardware {
    /// An SSD1306-compatible OLED was found at [`SSD1306_ADDR`].
    pub oled: bool,
    /// An OV2640-compatible camera was found at [`CAMERA_SCCB_ADDR`].
    pub camera: bool,
    /// An I2S audio codec was found at [`AUDIO_CODEC_ADDR`].
    pub audio: bool,
}

// ── Core probing logic ────────────────────────────────────────────────────────

/// Attempt a non-blocking 1-byte read from `address` on `i2c`.
///
/// Returns `true` when the device ACKs the request, `false` on any error
/// (NACK, bus timeout, arbitration loss, …).  This function **never panics**.
pub async fn probe_i2c_device<I: I2c>(i2c: &mut I, address: u8) -> bool {
    let mut buf = [0u8; 1];
    i2c.read(address, &mut buf).await.is_ok()
}

/// Scan the I2C bus for all known SrkOS peripherals.
///
/// The caller passes exclusive ownership of the I2C bus.  After the scan the
/// bus can be passed on to whichever task claimed it (e.g. `oled_task`).
///
/// Probing is sequential; each device gets one attempt.  If the underlying
/// HAL driver has a timeout configured, the total worst-case duration is
/// `num_devices × timeout`.
pub async fn probe_hardware<I: I2c>(i2c: &mut I) -> DetectedHardware {
    DetectedHardware {
        oled: probe_i2c_device(i2c, SSD1306_ADDR).await,
        camera: probe_i2c_device(i2c, CAMERA_SCCB_ADDR).await,
        audio: probe_i2c_device(i2c, AUDIO_CODEC_ADDR).await,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use core::convert::Infallible;
    use embedded_hal_async::i2c::{Error, ErrorKind, ErrorType, I2c, Operation};
    use futures::executor::block_on;

    // ── Mock I2C helpers ──────────────────────────────────────────────────────

    /// Opaque error type returned by the "always fail" mock.
    #[derive(Debug)]
    struct MockI2cError;

    impl Error for MockI2cError {
        fn kind(&self) -> ErrorKind {
            ErrorKind::Other
        }
    }

    /// Mock I2C bus that always succeeds – simulates a fully-populated board.
    struct AlwaysOkI2c;

    impl ErrorType for AlwaysOkI2c {
        type Error = Infallible;
    }

    impl I2c for AlwaysOkI2c {
        async fn read(&mut self, _addr: u8, _buf: &mut [u8]) -> Result<(), Infallible> {
            Ok(())
        }
        async fn write(&mut self, _addr: u8, _buf: &[u8]) -> Result<(), Infallible> {
            Ok(())
        }
        async fn write_read(
            &mut self,
            _addr: u8,
            _write: &[u8],
            _read: &mut [u8],
        ) -> Result<(), Infallible> {
            Ok(())
        }
        async fn transaction(
            &mut self,
            _addr: u8,
            _ops: &mut [Operation<'_>],
        ) -> Result<(), Infallible> {
            Ok(())
        }
    }

    /// Mock I2C bus that always fails – simulates a bare ESP32 with nothing
    /// attached.
    struct AlwaysErrI2c;

    impl ErrorType for AlwaysErrI2c {
        type Error = MockI2cError;
    }

    impl I2c for AlwaysErrI2c {
        async fn read(&mut self, _addr: u8, _buf: &mut [u8]) -> Result<(), MockI2cError> {
            Err(MockI2cError)
        }
        async fn write(&mut self, _addr: u8, _buf: &[u8]) -> Result<(), MockI2cError> {
            Err(MockI2cError)
        }
        async fn write_read(
            &mut self,
            _addr: u8,
            _write: &[u8],
            _read: &mut [u8],
        ) -> Result<(), MockI2cError> {
            Err(MockI2cError)
        }
        async fn transaction(
            &mut self,
            _addr: u8,
            _ops: &mut [Operation<'_>],
        ) -> Result<(), MockI2cError> {
            Err(MockI2cError)
        }
    }

    /// Mock I2C bus that succeeds only for the SSD1306 address – simulates a
    /// board with just the OLED attached.
    struct OledOnlyI2c;

    impl ErrorType for OledOnlyI2c {
        type Error = MockI2cError;
    }

    impl I2c for OledOnlyI2c {
        async fn read(&mut self, addr: u8, _buf: &mut [u8]) -> Result<(), MockI2cError> {
            if addr == SSD1306_ADDR {
                Ok(())
            } else {
                Err(MockI2cError)
            }
        }
        async fn write(&mut self, _addr: u8, _buf: &[u8]) -> Result<(), MockI2cError> {
            Err(MockI2cError)
        }
        async fn write_read(
            &mut self,
            _addr: u8,
            _write: &[u8],
            _read: &mut [u8],
        ) -> Result<(), MockI2cError> {
            Err(MockI2cError)
        }
        async fn transaction(
            &mut self,
            _addr: u8,
            _ops: &mut [Operation<'_>],
        ) -> Result<(), MockI2cError> {
            Err(MockI2cError)
        }
    }

    // ── probe_i2c_device ──────────────────────────────────────────────────────

    #[test]
    fn probe_returns_true_when_device_present() {
        let mut i2c = AlwaysOkI2c;
        assert!(
            block_on(probe_i2c_device(&mut i2c, SSD1306_ADDR)),
            "probe must return true when the device ACKs"
        );
    }

    #[test]
    fn probe_returns_false_when_device_absent() {
        let mut i2c = AlwaysErrI2c;
        assert!(
            !block_on(probe_i2c_device(&mut i2c, SSD1306_ADDR)),
            "probe must return false when the device NACKs (absent / timeout)"
        );
    }

    #[test]
    fn probe_does_not_panic_on_error() {
        // If this test completes without unwinding, the function is panic-free.
        let mut i2c = AlwaysErrI2c;
        let _ = block_on(probe_i2c_device(&mut i2c, 0x00));
        let _ = block_on(probe_i2c_device(&mut i2c, 0xFF));
    }

    // ── probe_hardware ────────────────────────────────────────────────────────

    #[test]
    fn probe_hardware_detects_all_peripherals() {
        let mut i2c = AlwaysOkI2c;
        let hw = block_on(probe_hardware(&mut i2c));
        assert!(hw.oled, "OLED should be detected on a fully-loaded board");
        assert!(hw.camera, "Camera should be detected on a fully-loaded board");
        assert!(hw.audio, "Audio codec should be detected on a fully-loaded board");
    }

    #[test]
    fn probe_hardware_detects_no_peripherals_on_bare_board() {
        let mut i2c = AlwaysErrI2c;
        let hw = block_on(probe_hardware(&mut i2c));
        assert!(!hw.oled, "OLED must not be detected when probe errors");
        assert!(!hw.camera, "Camera must not be detected when probe errors");
        assert!(!hw.audio, "Audio must not be detected when probe errors");
    }

    #[test]
    fn probe_hardware_bare_board_does_not_panic() {
        // Ensures the OS handles a completely empty I2C bus without panicking.
        let mut i2c = AlwaysErrI2c;
        let _hw = block_on(probe_hardware(&mut i2c));
    }

    #[test]
    fn probe_hardware_partial_detection_oled_only() {
        let mut i2c = OledOnlyI2c;
        let hw = block_on(probe_hardware(&mut i2c));
        assert!(hw.oled, "OLED should be detected");
        assert!(!hw.camera, "Camera must not be detected");
        assert!(!hw.audio, "Audio must not be detected");
    }

    #[test]
    fn detected_hardware_default_is_all_false() {
        let hw = DetectedHardware::default();
        assert!(!hw.oled);
        assert!(!hw.camera);
        assert!(!hw.audio);
    }
}
