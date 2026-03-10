# Product Requirements Document (PRD): Bare-Metal Async ESP32 Companion OS

## 1. Executive Summary

The objective is to build a highly optimized, dynamically adaptable, and memory-safe operating system for an ESP32 microcontroller (520KB SRAM, 4MB Flash, No PSRAM). The OS must act as a standalone desktop companion, a web-server host, and a distributed "Muscle" node (receiving commands via ESP-NOW from a master ESP32-S3).

The system will strictly use `#![no_std]`, `#![no_alloc]`, and the `embassy-rs` asynchronous framework to guarantee zero heap fragmentation and maximize power efficiency.

## 2. Core Objectives

* **Zero-Allocation Architecture:** Eliminate Out-Of-Memory (OOM) panics by statically allocating all memory buffers (Network, Camera DMA, Audio I2S, Framebuffers) at compile time.
* **Hardware Auto-Discovery:** The OS must boot, probe the I2C/I2S buses, and dynamically spawn only the tasks for the hardware it detects (e.g., SSD1306 OLED, Camera, Servos). The exact same firmware binary must run safely on a fully loaded board or a bare ESP32.
* **Zero-RAM Web Server:** Serve a functional Web UI directly from the 4MB Flash memory to a Wi-Fi client without loading assets into SRAM.
* **Asynchronous Isolation:** Use async channels for Inter-Process Communication (IPC). Tasks (like Wi-Fi and Motor Control) must never block each other.

## 3. Test-Driven Development (TDD) Strategy

Given the strict memory constraints, TDD is mandatory to ensure logic is sound before it touches the hardware.

* **Host-Side Logic Testing (`cargo test`):** State machines, JSON parsing (`serde-json-core`), routing logic, and IPC channel structures must be tested natively on the engineers' PCs. Mock the hardware interfaces to ensure the core logic handles missing hardware gracefully.
* **Hardware-in-the-Loop (HIL) Testing (`defmt-test`):** Automated tests must be executed directly on the ESP32 to verify DMA buffer alignment, I2C probe timeouts, and ESP-NOW packet latency.

---

## 4. Phased Implementation Plan

### Phase 1: Foundation & TDD Pipeline Setup

**Objective:** Establish the static memory layout, the async executor, and the testing pipeline.

* Configure the `embassy-executor` for the Xtensa architecture.
* Map out global `static` cells for the `COMMAND_BUS` (IPC channels).
* Write the first host-side unit tests to verify that messages can be passed into the channel and parsed correctly.
* **Success Criteria:** The team can run `cargo test` successfully on their PCs, and the core Embassy executor boots on the ESP32 without allocating a single byte of heap memory.

### Phase 2: Hardware Auto-Discovery & UI Task

**Objective:** Implement dynamic probing and the OLED framebuffer.

* Build the `probe_hardware()` async function to scan I2C address `0x3C` (SSD1306).
* Implement the `oled_task`. Allocate the strict 1KB static array for the framebuffer.
* Write unit tests to simulate an I2C timeout, ensuring the OS gracefully skips the `oled_task` without panicking.
* **Success Criteria:** If the screen is attached, the OS draws to it. If disconnected, the OS logs "No Screen Detected" and continues running perfectly.

### Phase 3: The Zero-RAM Micro Server & Connectivity

**Objective:** Establish Wi-Fi, ESP-NOW, and Flash-baked web serving.

* Embed the Web UI HTML/JS assets into the firmware using `include_bytes!`.
* Configure `esp-wifi` to multiplex Station Mode (Web Server) and ESP-NOW (listening for external robot commands).
* Implement a strict 2-connection limit on the HTTP server to protect SRAM.
* **Success Criteria:** A user can connect via Wi-Fi, load the UI, click a button, and the web server drops a command into the `COMMAND_BUS` to change the OLED face.

### Phase 4: Zero-Copy Media Pipelines (Camera & Audio)

**Objective:** Integrate high-bandwidth sensors without destroying the memory stack.

* Configure the Camera peripheral to output Hardware-Compressed JPEG straight into a static DMA ring buffer.
* Implement I2S microphone sampling into a static ping-pong buffer.
* Ensure that JPEG frames are passed to the Wi-Fi socket via zero-copy pointers.
* **Success Criteria:** The ESP32 streams JPEG video and audio over Wi-Fi while simultaneously updating the OLED screen and maintaining a stable heapless state.

### Phase 5: Build Verification & Release Readiness

**Objective:** Ensure the codebase is flawlessly compiled, optimized, and strictly ready for production flashing.

* **Compile-Time Verification:** The team will execute strict compilation commands. No code is merged unless `cargo build --release` completes with zero errors and zero warnings.
* **Dependency Audit:** Verify that `alloc` has not accidentally been introduced into the dependency tree.
* **Binary Size Check:** Ensure the compiled `.bin` file fits comfortably within the 4MB Flash constraints, including the baked-in Web UI assets.
* **The Final Flash Gate:** Run `cargo run --release` (which invokes `espflash` or `probe-rs` depending on your tooling) to compile, flash, and monitor the serial output in one seamless step.
* **Success Criteria:** The terminal outputs a completely clean, warning-free release build, automatically flashes the ESP32, and the serial monitor confirms all tasks have started successfully.
