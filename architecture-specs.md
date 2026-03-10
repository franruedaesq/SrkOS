# Architecture Specification: Bare-Metal ESP32 Companion OS

**Target Hardware:** ESP32 WROOM-32 (Xtensa Dual-Core, 240MHz, 520KB SRAM, 4MB Flash, NO PSRAM)
**Framework:** `#![no_std]`, `#![no_alloc]`, `embassy-rs`, `esp-hal`

## 1. Core System Paradigms

To run Camera, Audio, Wi-Fi, ESP-NOW, an OLED, and a Web Server concurrently on 520KB SRAM without Out-Of-Memory (OOM) panics or heap fragmentation, the team must strictly adhere to the following paradigms:

* **Zero Heap Allocation:** The `alloc` crate is strictly forbidden. The system must be `#![no_alloc]`. Dynamic dispatch (`Box`, `dyn`) and dynamically sized collections (`Vec`, `String`) are prohibited.
* **100% Static Lifetimes:** All buffers, DMA descriptors, and task futures must be statically allocated using `static_cell::StaticCell` or `static mut` wrapped in `embassy_sync` primitives.
* **Asynchronous Cooperative Multitasking:** The OS will utilize the `embassy-executor`. Context switching overhead is eliminated as all "tasks" are compiled into a single state machine. The CPU will enter `WFI` (Wait For Interrupt) during idle cycles, drastically reducing power consumption.

## 2. Memory Layout & Zero-Copy DMA Pipelines

Memory starvation is the primary risk. The team must map out the SRAM usage at compile time.

* **Network Stack (`smoltcp` & `esp-wifi`):** Allocate a strict, static pool of sockets. Define `static mut RX_BUFFER: [u8; 4096]` and `TX_BUFFER` for the TCP/IP stack.
* **Camera Pipeline (OV2640 or similar):** Processing raw RGB frames is impossible. Configure the camera driver via SCCB (I2C) to output **Hardware-Compressed JPEG** directly. Map the camera's I2S/Camera peripheral strictly to a DMA ring buffer. The CPU must never touch the pixel data. Pass the DMA buffer pointer directly to the `smoltcp` socket to stream the JPEG over Wi-Fi (Zero-Copy).
* **Display Framebuffer:** The SSD1306 0.96" OLED is 1-bit monochrome (128x64). Reserve exactly a `[u8; 1024]` static array for the `embedded-graphics` canvas. Push this to the OLED via async I2C/SPI DMA.
* **Audio Pipeline:** I2S microphone inputs must be routed via DMA into statically allocated ping-pong buffers.

## 3. Hardware Auto-Discovery (Dynamic Probing)

The OS must run identical firmware on both the integrated IdeaSpark board and a generic ESP32. This requires boot-time probing.

1. **I2C Bus Initialization:** Initialize the `I2C` peripheral on the standard pins (e.g., GPIO 21/22).
2. **The Probe Sequence:** Have the executor run an async `probe_hardware()` function before spawning main tasks. Perform a non-blocking 1-byte read request to I2C address `0x3C` (SSD1306).
3. **Conditional Task Spawning:** * If `Ok()`, the device exists. The Spawner takes ownership of the I2C bus handle and invokes `spawner.spawn(oled_task(i2c_bus)).unwrap()`.
* If `Err()`, the device is absent. The Executor skips the UI task entirely and reclaims the memory footprint of that state machine.
* Repeat this probing logic for the Camera (SCCB address) and I2S Audio chips.



## 4. Inter-Process Communication (IPC) & State Management

Tasks are isolated and cannot share mutable state directly. The team will use Embassy's `Channel` and `Signal` primitives.

```rust
// Defined globally
type SystemMutex = embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;

pub enum Command {
    SetFaceExpression(Expression),
    MoveServo(u8, u16), // Servo ID, Angle
    StreamAudio(bool),
}

// Statically allocated message bus
pub static COMMAND_BUS: Channel<SystemMutex, Command, 8> = Channel::new();

```

* **Decoupled Architecture:** The `network_task` parses an incoming HTTP request or ESP-NOW packet, drops a `Command` into the `COMMAND_BUS`, and immediately drops the connection. The `oled_task` or `servo_task` independently awaits `COMMAND_BUS.receive()` and executes the hardware state change.

## 5. The Zero-RAM Micro Web Server

To serve a Web UI without consuming SRAM:

* **Flash-Baked Assets:** Write the HTML/CSS/JS/PNG assets for the Web UI on the PC. Use Rust's `include_bytes!("ui/index.html")` macro. This hardcodes the assets into the 4MB Flash memory (`.rodata` section).
* **HTTP Routing (`picoserve`):** Use the `picoserve` crate (an async `#![no_std]` HTTP server). Route requests for `/` directly to the flash memory slice. The ESP32 will stream bytes directly from Flash to the Wi-Fi hardware buffer.
* **API Endpoints:** Create lightweight REST endpoints (e.g., `POST /api/servo`) using `serde-json-core` (a heapless JSON parser) to trigger `COMMAND_BUS` events.
* **Connection Limits:** Hardcode `MAX_CONNECTIONS = 2`. If a third client connects, the server synchronously rejects it to protect the motor control and camera DMA loops.

## 6. Connectivity Multiplexing (Wi-Fi + ESP-NOW)

To function as a standalone companion, a web-connected device, and a ROS 2 muscle node:

* **Radio Time-Slicing:** The ESP32 has one 2.4GHz radio. The `esp-wifi` crate supports concurrent Station Mode (connecting to a router) and ESP-NOW.
* **Fail-Safe Degradation:**
* *Boot:* Attempt to connect to the hardcoded/provisioned Wi-Fi SSID for 5 seconds.
* *If Success:* Initialize `smoltcp` stack, start Web Server, and listen for ESP-NOW UDP wrappers from the ESP32-S3 Brain.
* *If Timeout:* Skip `smoltcp` initialization. Drop the Wi-Fi Station task. Transition the radio exclusively into ESP-NOW listener mode. The OS operates cleanly as an offline "Muscle" node.



## 7. Recommended Crate Ecosystem for the Team

Ensure the `Cargo.toml` is strictly tailored for the bare-metal Xtensa target:

* **HAL & Runtime:** `esp-hal`, `esp-alloc` (ONLY for Wi-Fi driver internals if absolutely required by `esp-wifi`, otherwise strictly `no_alloc`), `embassy-executor`, `embassy-time`.
* **Networking:** `esp-wifi`, `smoltcp`, `picoserve`.
* **Graphics & UI:** `embedded-graphics`, `ssd1306` (configured for async I2C).
* **Serialization:** `serde-json-core`, `postcard` (for ultra-fast binary ESP-NOW packet serialization between the ESP32-S3 and this generic ESP32).

