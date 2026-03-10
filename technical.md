# Technical Architecture Specification: Universal ESP32 Bare-Metal Companion OS

## 1. System Overview & Hardware Target

This document outlines the architecture for a highly modular, asynchronous, zero-allocation operating system designed to run on the **ESP32 WROOM-32** silicon. The compiled binary must be fully portable and adapt dynamically at runtime to different hardware configurations (e.g., an IdeaSpark board with an integrated OLED vs. a generic ESP32 wired to external servos and sensors).

**Hardware Constraints:**

* **Architecture:** Xtensa Dual-Core LX6 @ 240MHz
* **Memory:** 520 KB SRAM **(Strict Limit)**
* **Storage:** 4 MB External Flash
* **PSRAM:** NONE

## 2. Core Software Constraints

To prevent Out-Of-Memory (OOM) panics and heap fragmentation, the team must strictly adhere to the following paradigms:

* **Zero Heap Allocation:** The `alloc` crate is strictly forbidden (`#![no_alloc]`). Dynamic dispatch (`Box`, `dyn`) and dynamically sized collections (`Vec`, `String`) are prohibited.
* **Static Lifetimes:** All buffers, DMA descriptors, and task futures must be statically allocated at compile time (using `static_cell::StaticCell` or similar `embassy` macros).
* **Asynchronous Cooperative Multitasking:** The system will utilize the `embassy-executor` for the Xtensa architecture. The CPU must enter `WFI` (Wait For Interrupt) during idle cycles to minimize power consumption.

## 3. Hardware Auto-Discovery (Dynamic Probing)

The OS must run identical firmware across different physical builds. The `main` initialization sequence must perform dynamic hardware probing before spawning Embassy tasks.

1. **Bus Initialization:** Initialize I2C, I2S, and PWM peripherals.
2. **I2C Probing:** Perform non-blocking 1-byte read requests to specific I2C addresses.
* Probe `0x3C` -> If `Ok()`, allocate and spawn `face_oled_task`.
* Probe `0x3D` -> If `Ok()`, allocate and spawn `info_oled_task`.


3. **GPIO/I2S Probing:** Check configured pins for microphones, speakers, or buttons.
4. **Task Spawning:** Only spawn the Embassy tasks (`async fn`) for hardware that successfully responds. Reclaim/ignore the memory footprint of unspawned state machines.

## 4. Memory Layout & Zero-Copy Media Pipelines

Memory starvation is the primary risk. The 520 KB SRAM must be mapped strictly.

* **Display Framebuffers:** The SSD1306 0.96" OLEDs are 1-bit monochrome (128x64). Reserve exactly two `[u8; 1024]` static arrays for the `embedded-graphics` canvases. Push to OLEDs via async I2C DMA.
* **Audio Pipeline:** I2S microphone/speaker inputs must be routed via DMA into statically allocated ping-pong buffers.
* **Camera Pipeline (If detected):** Raw RGB processing is prohibited. Configure the camera (e.g., OV2640) via SCCB to output **Hardware-Compressed JPEG** directly into a DMA ring buffer. Pass the DMA buffer pointer directly to the network socket to stream the JPEG (Zero-Copy).

## 5. Inter-Process Communication (IPC)

Tasks must remain fully decoupled. Hardware abstractions (like OLED drawing or Servo moving) must not block network or timer tasks.

* Implement a globally accessible `embassy_sync::channel::Channel`.
* Define a robust `Command` enum (e.g., `UpdateFace(Mood)`, `MoveServo(Id, Angle)`, `PlayAlarm`).
* Tasks act as independent producers/consumers. (e.g., The Web Server drops an `UpdateFace` event into the channel and immediately drops the TCP connection; the `face_oled_task` reads the channel and updates the framebuffer).

## 6. Connectivity Multiplexing & Micro Web Server

The ESP32 shares a single 2.4GHz radio. The OS must time-slice or degrade gracefully between Station Mode (Wi-Fi) and ESP-NOW.

* **Zero-RAM Asset Serving:** The Web UI (HTML, CSS, JS) must be baked into the 4MB Flash memory using the `include_bytes!()` macro. It must be streamed directly from Flash to the TCP socket without being copied into SRAM.
* **HTTP Stack:** Utilize a `#![no_std]` async HTTP server (e.g., `picoserve`) running on top of `smoltcp`.
* **Connection Limits:** Hardcode `MAX_CONNECTIONS = 2` to protect motor control and media DMA loops from network floods.
* **ESP-NOW Bridge:** If provisioned, listen for binary ESP-NOW packets from the master ESP32-S3 node to trigger local servo/motor commands.

## 7. Companion Application Features

The OS must support high-level companion logic using zero-allocation primitives.

* **SNTP & Real-Time Clock:** Upon Wi-Fi connection, the `network_task` must execute an SNTP sync to set the ESP32's internal RTC.
* **Pomodoro & Alarms:** Utilize `embassy-time` (`Timer::after()`) for zero-CPU-cost async delays. When timers expire, trigger the `COMMAND_BUS`.
* **API Polling:** Create an async task that executes periodic HTTP GET requests to designated endpoints. Parse incoming JSON payloads strictly using `serde-json-core` (heapless parsing) and route text to the `info_oled_task`.
* **Mood Tracker (Physical Buttons):** Implement **Async GPIO Interrupts** (`Exti`). Tasks must await the physical pin state change (`pin.wait_for_falling_edge().await`), debounce the signal asynchronously, and fire a telemetry payload to the network task.

## 8. Required Crate Ecosystem

The `Cargo.toml` must be strictly tailored for the bare-metal Xtensa target:

* **Core:** `esp-hal`, `esp-wifi` (configured for `smoltcp`), `embassy-executor`, `embassy-time`, `embassy-sync`.
* **Networking/Server:** `smoltcp`, `picoserve`.
* **Serialization:** `serde-json-core` (JSON), `postcard` (for ultra-fast binary ESP-NOW packets).
* **UI/Graphics:** `embedded-graphics`, `ssd1306`.

## 9. Test-Driven Development (TDD) & Build Gates

* **Host-Side Logic:** State machines, `serde-json-core` parsing, and IPC channel structures must be heavily covered by `cargo test` running natively on the host PC (mocking the HAL).
* **Hardware-in-the-Loop (HIL):** Use `defmt-test` for critical DMA and I2C probing verification on the physical silicon.
* **CI/CD Build Gate:** No code is cleared for flashing unless `cargo build --release` targeting `xtensa-esp32-none-elf` completes with **zero errors and zero warnings**, and fits within the 4MB flash limit.

## 10. Dependencies to be Provided by Product Owner (PO)

The engineering team requires the following assets from the PO prior to finalizing the hardware abstraction layer:
**The ESP32 0.96'' OLED board has all the features of the traditional ESP32 Devkit V1 module,with the same exact peripheral ports,offers seamless integration with a 0.96-inch OLED display. This board uses I2C to connect to an OLED display via the SDA (D21 / GPIO21) and SCL (D22 / GPIO22) pins.**
1. **Pinout Map:** Exact GPIO assignments for the IdeaSpark built-in OLED, and the intended GPIO assignments for external Servos, Buttons, Camera (I2C/I2S/Data pins), and Microphones.
2. **Web Assets:** The compiled `ui/` directory containing the exact HTML/CSS/JS files to be flashed via `include_bytes!`.
3. **API Contract:** The exact JSON schema expected from the web UI (e.g., `{"action": "set_timer", "minutes": 25}`) and the binary struct definition for the ESP-NOW packets.
4. **Network Config:** Injection strategy for Wi-Fi SSID/Password (e.g., `.env` macro at compile time).

---
