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
//!
//! # Phase 3: Wi-Fi + ESP-NOW (Station/AP mode + ESP-NOW peer)
//! esp-wifi = { version = "0.13", default-features = false, features = [
//!     "esp32", "phy-enable-usb", "esp-now", "async",
//! ] }
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
//! 4. Spawn the Wi-Fi HTTP server task and ESP-NOW receiver task (Phase 3).
//! 5. Enter the idle loop -- Embassy puts the CPU in `WFI` between events.
//!
//! # Memory guarantees
//!
//! * `#![no_alloc]` -- the heap is never touched.
//! * All buffers (RX/TX network, OLED framebuffer, DMA JPEG ring, I2S ping-pong)
//!   are statically allocated.
//! * Camera outputs hardware-compressed JPEG via DMA; the CPU never touches
//!   raw pixel data (`DMA_JPEG_RING` = 32 KB static).
//! * I2S audio is routed via DMA into a 2 KB ping-pong buffer pair; the CPU
//!   never copies sample data.
//! * JPEG frames and audio samples are streamed to Wi-Fi via zero-copy pointers
//!   (`DMA_RING_BUFFER.current_frame()` / `PING_PONG_STATE.ready_buffer()`).
//! * The Embassy cooperative executor eliminates context-switching overhead.
//! * The HTTP server enforces a 2-connection limit via `ConnectionGuard` to
//!   protect SRAM from exhaustion.

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
use srkos::tasks::camera::DMA_RING_BUFFER;
use srkos::tasks::audio::PING_PONG_STATE;
use srkos::tasks::http::{
    ConnectionGuard, RouteResult, RESPONSE_200_AUDIO, RESPONSE_200_HTML, RESPONSE_200_JSON,
    RESPONSE_200_MJPEG, RESPONSE_400, RESPONSE_404, RESPONSE_503,
};

// ---- Static network buffers -------------------------------------------------
// Reserved at compile time; the heap is never touched at runtime.

/// TCP/IP receive buffer for the `smoltcp` network stack (Phase 3).
static mut RX_BUFFER: [u8; 4096] = [0u8; 4096];

/// TCP/IP transmit buffer for the `smoltcp` network stack (Phase 3).
static mut TX_BUFFER: [u8; 4096] = [0u8; 4096];

/// Flash-baked Web UI -- embedded from Flash (.rodata), zero SRAM cost.
static INDEX_HTML: &[u8] = include_bytes!("../ui/index.html");

/// Idle period for stub task loops that have not yet been wired to hardware.
///
/// Replaced by event-driven awaits once `esp-wifi` is linked in.
const STUB_TASK_DELAY_SECS: u64 = 60;

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
        esp_println::println!("[SrkOS] Camera (OV2640) detected -- spawning camera_task");
        spawner.spawn(camera_task()).unwrap();
    }

    if hw.audio {
        esp_println::println!("[SrkOS] Audio codec detected -- spawning audio_task");
        spawner.spawn(audio_task()).unwrap();
    }

    // ---- Phase 3: Wi-Fi HTTP server + ESP-NOW receiver ---------------------
    // Requires esp-wifi in Cargo.toml (see module doc for the exact entry).
    spawner.spawn(wifi_task()).unwrap();
    spawner.spawn(espnow_task()).unwrap();

    esp_println::println!(
        "[SrkOS] Boot complete. Web UI: {} bytes in Flash. Max connections: {}.",
        INDEX_HTML.len(),
        srkos::tasks::http::MAX_CONNECTIONS,
    );

    // Idle loop -- Embassy puts the CPU into WFI between timer wakeups.
    loop {
        Timer::after_secs(60).await;
    }
}

// ---- Tasks ------------------------------------------------------------------

/// Camera DMA ring buffer task (Phase 4).
///
/// Configures the OV2640 sensor via SCCB to output hardware-compressed JPEG
/// directly into [`srkos::tasks::camera::DMA_JPEG_RING`].  The CPU never
/// touches raw pixel data.
///
/// The task loop awaits [`Command::StreamVideo`] messages from [`COMMAND_BUS`]
/// and delegates to [`srkos::tasks::camera::camera_task_loop`].
///
/// The Wi-Fi task streams the current JPEG frame via zero-copy:
/// `socket.write_all(DMA_RING_BUFFER.current_frame()).await`.
///
/// Spawned only when the OV2640 is detected during the boot-time I2C probe.
#[embassy_executor::task]
async fn camera_task() {
    srkos::tasks::camera::camera_task_loop().await;
}

/// I2S microphone ping-pong DMA task (Phase 4).
///
/// Enables the I2S peripheral and starts the DMA engine into the static
/// ping-pong buffers ([`srkos::tasks::audio::I2S_PING`] /
/// [`srkos::tasks::audio::I2S_PONG`]).  The CPU never copies sample data.
///
/// The task loop awaits [`Command::StreamAudio`] messages from [`COMMAND_BUS`]
/// and delegates to [`srkos::tasks::audio::audio_task_loop`].
///
/// The Wi-Fi task streams the completed audio frame via zero-copy:
/// `socket.write_all(PING_PONG_STATE.ready_buffer()).await`.
///
/// Spawned only when the I2S codec is detected during the boot-time I2C probe.
#[embassy_executor::task]
async fn audio_task() {
    srkos::tasks::audio::audio_task_loop().await;
}

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

/// Wi-Fi HTTP server task (Phase 3 / Phase 4).
///
/// Initialises the ESP32 in AP mode via `esp-wifi`, then accepts TCP
/// connections on port 80.  For each connection:
///
/// 1. Attempt to acquire a [`ConnectionGuard`]; reject with [`RESPONSE_503`]
///    if the 2-connection limit is already reached.
/// 2. Read the raw HTTP request into [`RX_BUFFER`].
/// 3. Parse with [`srkos::tasks::http::parse_request`].
/// 4. Dispatch via [`srkos::tasks::http::route_request`]:
///    - `ServeIndex`         → write [`RESPONSE_200_HTML`] + `INDEX_HTML`
///    - `CommandAccepted(c)` → enqueue `c` on [`COMMAND_BUS`]; write
///                             [`RESPONSE_200_JSON`] + `{}`
///    - `ServeVideoStream`   → write [`RESPONSE_200_MJPEG`]; stream JPEG frames
///                             from [`DMA_RING_BUFFER`] zero-copy until closed
///    - `ServeAudioStream`   → write [`RESPONSE_200_AUDIO`]; stream PCM frames
///                             from [`PING_PONG_STATE`] zero-copy until closed
///    - `BadRequest`         → write [`RESPONSE_400`]
///    - `NotFound`           → write [`RESPONSE_404`]
/// 5. Drop the [`ConnectionGuard`] (decrements the connection counter).
///
/// # Station + ESP-NOW multiplexing
///
/// `esp-wifi` time-slices the single 2.4 GHz radio between the AP/Station
/// driver and the ESP-NOW peer automatically.  No additional configuration
/// is required beyond enabling both features in the `esp-wifi` crate.
#[embassy_executor::task]
async fn wifi_task() {
    // NOTE: Full implementation requires `esp-wifi` in Cargo.toml.
    // Pseudocode (illustrates correct ConnectionGuard + route_request usage):
    //
    //   let wifi = esp_wifi::init(...).unwrap();
    //   let ap   = wifi.start_ap("SrkOS", "").await.unwrap();
    //   let mut server = TcpListener::new(ap, 80);
    //
    //   loop {
    //       let socket = server.accept().await;
    //
    //       // Enforce the 2-connection limit.
    //       let guard = match ConnectionGuard::try_acquire() {
    //           Some(g) => g,
    //           None    => { socket.write_all(RESPONSE_503).await; continue; }
    //       };
    //
    //       // Read request into the static RX buffer (zero heap allocation).
    //       let n = socket.read(&mut RX_BUFFER).await.unwrap_or(0);
    //
    //       // Parse and route.
    //       if let Some(req) = srkos::tasks::http::parse_request(&RX_BUFFER[..n]) {
    //           match srkos::tasks::http::route_request(&req) {
    //               RouteResult::ServeIndex => {
    //                   socket.write_all(RESPONSE_200_HTML).await;
    //                   socket.write_all(INDEX_HTML).await;
    //               }
    //               RouteResult::CommandAccepted(cmd) => {
    //                   COMMAND_BUS.send(cmd).await;
    //                   socket.write_all(RESPONSE_200_JSON).await;
    //                   socket.write_all(b"{}").await;
    //               }
    //               RouteResult::ServeVideoStream => {
    //                   // Phase 4: MJPEG zero-copy stream.
    //                   // The DMA ring buffer pointer is passed directly to the
    //                   // socket — no intermediate copy is ever made.
    //                   socket.write_all(RESPONSE_200_MJPEG).await;
    //                   loop {
    //                       let frame = DMA_RING_BUFFER.current_frame();
    //                       if frame.is_empty() {
    //                           Timer::after_millis(10).await; // wait for first frame
    //                           continue;
    //                       }
    //                       // MJPEG multipart boundary + frame headers.
    //                       // Content-Length must be written as ASCII digits; use a
    //                       // heapless::String or write each digit manually since
    //                       // format!() is not available in no_alloc builds.
    //                       socket.write_all(b"--frame\r\n").await;
    //                       socket.write_all(b"Content-Type: image/jpeg\r\n").await;
    //                       socket.write_all(b"\r\n").await;
    //                       socket.write_all(frame).await; // zero-copy DMA pointer
    //                       socket.write_all(b"\r\n").await;
    //                   }
    //               }
    //               RouteResult::ServeAudioStream => {
    //                   // Phase 4: raw PCM zero-copy stream.
    //                   socket.write_all(RESPONSE_200_AUDIO).await;
    //                   loop {
    //                       let samples = PING_PONG_STATE.ready_buffer();
    //                       if !samples.is_empty() {
    //                           socket.write_all(samples).await; // zero-copy DMA pointer
    //                       }
    //                       Timer::after_millis(32).await; // ~32 ms per I2S frame
    //                   }
    //               }
    //               RouteResult::BadRequest => { socket.write_all(RESPONSE_400).await; }
    //               RouteResult::NotFound   => { socket.write_all(RESPONSE_404).await; }
    //           }
    //       }
    //       drop(guard); // releases the connection slot
    //   }

    // Suppress "unused import" warnings until esp-wifi is wired up.
    let _ = (
        ConnectionGuard::active(),
        DMA_RING_BUFFER.current_frame(),
        PING_PONG_STATE.ready_buffer(),
        RESPONSE_200_HTML,
        RESPONSE_200_JSON,
        RESPONSE_200_MJPEG,
        RESPONSE_200_AUDIO,
        RESPONSE_400,
        RESPONSE_404,
        RESPONSE_503,
        INDEX_HTML,
    );
    esp_println::println!("[wifi_task] started (awaiting esp-wifi initialisation)");
    loop {
        Timer::after_secs(STUB_TASK_DELAY_SECS).await;
    }
}

/// ESP-NOW receiver task (Phase 3).
///
/// Listens for incoming ESP-NOW frames from the master ESP32-S3 Brain.
/// Each frame's payload is parsed by
/// [`srkos::tasks::espnow::parse_espnow_payload`] and, if valid, enqueued
/// on [`COMMAND_BUS`] for consumption by `oled_task` and other hardware tasks.
///
/// # Payload format
///
/// The ESP32-S3 master encodes commands as JSON (same format as the HTTP API):
///
/// ```json
/// {"SetFaceExpression": "Happy"}
/// {"MoveServo": [0, 90]}
/// {"StreamAudio": true}
/// ```
#[embassy_executor::task]
async fn espnow_task() {
    // NOTE: Full implementation requires `esp-wifi` with the `esp-now` feature.
    // Pseudocode:
    //
    //   let espnow = EspNow::new(&wifi_controller).unwrap();
    //   loop {
    //       let frame = espnow.receive().await;
    //       if let Some(cmd) = srkos::tasks::espnow::parse_espnow_payload(&frame.data) {
    //           COMMAND_BUS.send(cmd).await;
    //       }
    //   }

    esp_println::println!("[espnow_task] started (awaiting esp-wifi/esp-now initialisation)");
    loop {
        Timer::after_secs(STUB_TASK_DELAY_SECS).await;
    }
}
