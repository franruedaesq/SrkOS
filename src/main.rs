//! SrkOS firmware entry point -- ESP32 Xtensa (240 MHz, 520 KB SRAM, 4 MB Flash).
//!
//! # Building the firmware
//!
//! This binary requires the `firmware` feature **and** the Xtensa toolchain.
//! Before building, add the platform crates to Cargo.toml:
//!
//! ```toml
//! # [dependencies] -- add these alongside the existing entries
//! esp-hal         = { version = "1.0", features = ["esp32"] }
//! esp-hal-embassy = { version = "0.9", default-features = false, features = ["esp32", "executors"] }
//! esp-backtrace   = { version = "0.18", features = ["esp32", "panic-handler", "println"] }
//! esp-println     = { version = "0.16", features = ["esp32"] }
//! ```
//!
//! Then build and flash:
//!
//! ```sh
//! cargo build --target xtensa-esp32-none-elf --features firmware --release
//! cargo run  --target xtensa-esp32-none-elf --features firmware --release
//! ```
//!
//! # Boot sequence
//!
//! 1. Initialise peripherals and the Embassy time driver (Timer Group 0).
//! 2. Probe the I2C bus (GPIO 21 SDA / GPIO 22 SCL) for known peripherals.
//! 3. Conditionally spawn hardware tasks for each detected device.
//! 4. Enter the idle loop -- Embassy puts the CPU in `WFI` between events.
//!
//! # Memory guarantees
//!
//! * `#![no_alloc]` -- the heap is never touched.
//! * All buffers (RX/TX network, OLED framebuffer) are statically allocated.
//! * The Embassy cooperative executor eliminates context-switching overhead.

#![no_std]
#![no_main]

// NOTE: The imports below require the platform crates listed in the doc above.
// They compile only when targeting xtensa-esp32-none-elf with --features firmware.
use embassy_executor::Spawner;
use embassy_time::Timer;
use esp_backtrace as _;
use esp_hal::i2c::master::I2c;
use esp_hal::timer::timg::TimerGroup;

use srkos::hardware::probe_hardware;
use srkos::ipc::COMMAND_BUS;

// ---- Static network buffers -------------------------------------------------
// Reserved at compile time; the heap is never touched at runtime.

/// TCP/IP receive buffer for the `smoltcp` network stack (Phase 3).
static mut RX_BUFFER: [u8; 4096] = [0u8; 4096];

/// TCP/IP transmit buffer for the `smoltcp` network stack (Phase 3).
static mut TX_BUFFER: [u8; 4096] = [0u8; 4096];

/// Flash-baked Web UI -- embedded from Flash (.rodata), zero SRAM cost.
static INDEX_HTML: &[u8] = include_bytes!("../ui/index.html");

// ---- Embassy entry point ----------------------------------------------------

/// Embassy main task.
///
/// The `#[esp_hal_embassy::main]` attribute sets up the Xtensa interrupt
/// executor and calls `esp_hal_embassy::init` automatically.
#[esp_hal_embassy::main]
async fn main(spawner: Spawner) {
    // Initialise all ESP32 peripherals.
    let peripherals = esp_hal::init(esp_hal::Config::default());

    // Embassy time driver: must be initialised before any `Timer::after_*`.
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_hal_embassy::init(timg0.timer0);

    // I2C bus on GPIO 21 (SDA) / GPIO 22 (SCL) using the esp-hal 1.0 builder.
    let mut i2c = I2c::new(peripherals.I2C0)
        .with_sda(peripherals.GPIO21)
        .with_scl(peripherals.GPIO22)
        .into_async();

    // ---- Hardware auto-discovery -------------------------------------------
    let hw = probe_hardware(&mut i2c).await;

    if hw.oled {
        esp_println::println!("[SrkOS] SSD1306 OLED detected -- spawning oled_task");
        spawner.spawn(oled_task()).unwrap();
    } else {
        esp_println::println!("[SrkOS] No Screen Detected -- skipping oled_task");
    }

    if hw.camera {
        esp_println::println!("[SrkOS] Camera (OV2640) detected");
        // Phase 4: spawner.spawn(camera_task()).unwrap();
    }

    if hw.audio {
        esp_println::println!("[SrkOS] Audio codec detected");
        // Phase 4: spawner.spawn(audio_task()).unwrap();
    }

    esp_println::println!(
        "[SrkOS] Boot complete. Web UI: {} bytes in Flash.",
        INDEX_HTML.len()
    );

    // Idle loop -- Embassy puts the CPU into WFI between timer wakeups.
    loop {
        Timer::after_secs(60).await;
    }
}

// ---- Tasks ------------------------------------------------------------------

/// OLED display task.
///
/// Awaits commands from [`COMMAND_BUS`] and drives the SSD1306 state machine.
/// Spawned only when the SSD1306 is detected during the boot-time I2C probe.
///
/// Delegates to [`srkos::tasks::oled::oled_task_loop`] so the loop logic is
/// defined once and shared between the firmware and its host-side tests.
#[embassy_executor::task]
async fn oled_task() {
    srkos::tasks::oled::oled_task_loop().await;
}
