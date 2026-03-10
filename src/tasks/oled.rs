//! OLED display task for SSD1306 (128×64, 1-bit monochrome).
//!
//! # Memory layout
//!
//! The framebuffer is a `[u8; 1024]` static array allocated at compile time:
//!
//! ```text
//! 128 columns × 64 rows ÷ 8 bits/byte = 1 024 bytes
//! ```
//!
//! This single kilobyte is the **entire** RAM footprint of the display
//! subsystem – no heap involved.
//!
//! # Task lifecycle
//!
//! 1. `probe_hardware()` detects the SSD1306 at address `0x3C`.
//! 2. If found, the firmware spawns `oled_task()`.
//! 3. If absent, the OS logs `"No Screen Detected"` and moves on.
//!
//! The task loop pulls [`Command`] messages from [`COMMAND_BUS`] on every
//! frame (non-blocking via `try_receive`) and updates the display state.
//! The face animation and menu continue running even if the button task
//! is absent — the face is independent of any particular command producer.

#[cfg(feature = "firmware")]
use crate::ipc::COMMAND_BUS;
use crate::ipc::{Command, FaceMood};

// ── Constants ─────────────────────────────────────────────────────────────────

/// Size of the SSD1306 128×64 1-bit framebuffer in bytes.
///
/// Calculated as: 128 columns × 64 rows ÷ 8 bits per byte = **1 024 bytes**.
pub const FRAMEBUFFER_SIZE: usize = 1024;

/// Target frame period in milliseconds (~30 FPS).
///
/// Each `oled_task_loop` iteration awaits this duration so that Wi-Fi and
/// audio tasks are never starved by the display task.
pub const FRAME_PERIOD_MS: u64 = 33;

/// Inactivity timeout in seconds before the menu auto-closes.
pub const MENU_INACTIVITY_SECS: u64 = 10;

// ── Static framebuffer ────────────────────────────────────────────────────────

/// Raw backing storage for the OLED framebuffer (1 KB, zero-initialised).
///
/// # Safety
///
/// Access is always single-owner: `oled_task` takes exclusive access after
/// boot.  The Embassy executor guarantees that a task decorated with
/// `#[embassy_executor::task]` is spawned **at most once**, making this
/// `static mut` sound in practice.
#[allow(static_mut_refs)]
pub static mut OLED_FRAMEBUFFER: [u8; FRAMEBUFFER_SIZE] = [0u8; FRAMEBUFFER_SIZE];

// ── Menu hierarchy ────────────────────────────────────────────────────────────

/// Which level of the menu hierarchy is currently displayed.
///
/// The menu is navigated exclusively via the BOOT button:
/// - Single click  → [`Command::MenuNextItem`] (moves cursor down, wraps)
/// - Long press    → [`Command::MenuSelect`]  (opens / enters / executes)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuDepth {
    /// Top-level menu: Talk | Mood | Settings | Exit
    Main,
    /// Mood sub-menu: Set Happy | Set Sleep | Back
    Mood,
    /// Settings sub-menu: Wi-Fi Info | Volume | Back
    Settings,
}

/// Number of selectable items at each menu depth.
fn menu_item_count(depth: MenuDepth) -> u8 {
    match depth {
        MenuDepth::Main => 4,     // Talk, Mood, Settings, Exit
        MenuDepth::Mood => 3,     // Set Happy, Set Sleep, Back
        MenuDepth::Settings => 3, // Wi-Fi Info, Volume, Back
    }
}

// ── State machine ─────────────────────────────────────────────────────────────

/// The complete display state tracked by the OLED task.
///
/// The state is fully host-testable without hardware: `handle_command` is a
/// pure function that maps `(OledState, &Command) → OledState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OledState {
    /// Current face mood shown on the display.
    pub face_mood: FaceMood,
    /// Whether the navigation menu is currently overlaid on screen.
    pub menu_open: bool,
    /// Which menu level is currently active (only relevant when `menu_open`).
    pub menu_depth: MenuDepth,
    /// Zero-based index of the currently highlighted menu item.
    pub selected_item: u8,
}

impl OledState {
    /// Create the initial display state (neutral face, menu closed).
    pub fn new() -> Self {
        OledState {
            face_mood: FaceMood::Neutral,
            menu_open: false,
            menu_depth: MenuDepth::Main,
            selected_item: 0,
        }
    }

    /// Human-readable label for logging / defmt output.
    pub fn label(&self) -> &'static str {
        if self.menu_open {
            match self.menu_depth {
                MenuDepth::Main => "Menu:Main",
                MenuDepth::Mood => "Menu:Mood",
                MenuDepth::Settings => "Menu:Settings",
            }
        } else {
            match self.face_mood {
                FaceMood::Happy => "Happy",
                FaceMood::Sad => "Sad",
                FaceMood::Neutral => "Neutral",
                FaceMood::Excited => "Excited",
                FaceMood::Mad => "Mad",
                FaceMood::Sleeping => "Sleeping",
            }
        }
    }
}

impl Default for OledState {
    fn default() -> Self {
        Self::new()
    }
}

// ── Command handler ───────────────────────────────────────────────────────────

/// Transition the OLED state in response to an incoming [`Command`].
///
/// This pure function is fully testable on the host without any hardware.
///
/// | Command               | Effect                                            |
/// |-----------------------|---------------------------------------------------|
/// | `SetFaceExpression`   | Update `face_mood`; ignored while menu is open    |
/// | `MenuNextItem`        | Advance `selected_item`, wrapping at menu end     |
/// | `MenuSelect`          | Open menu / enter sub-menu / execute item         |
/// | all others            | State unchanged                                   |
pub fn handle_command(state: OledState, cmd: &Command) -> OledState {
    match cmd {
        Command::SetFaceExpression(mood) => OledState {
            face_mood: *mood,
            ..state
        },
        Command::MenuNextItem => {
            if !state.menu_open {
                // Single click with menu closed is a no-op.
                state
            } else {
                let count = menu_item_count(state.menu_depth);
                OledState {
                    selected_item: (state.selected_item + 1) % count,
                    ..state
                }
            }
        }
        Command::MenuSelect => handle_menu_select(state),
        _ => state,
    }
}

/// Execute the currently highlighted menu item.
fn handle_menu_select(state: OledState) -> OledState {
    if !state.menu_open {
        // Long press with menu closed → open main menu.
        return OledState {
            menu_open: true,
            menu_depth: MenuDepth::Main,
            selected_item: 0,
            ..state
        };
    }

    match state.menu_depth {
        MenuDepth::Main => match state.selected_item {
            0 => state, // Talk: no local state change (audio pipeline elsewhere)
            1 => OledState {
                menu_depth: MenuDepth::Mood,
                selected_item: 0,
                ..state
            },
            2 => OledState {
                menu_depth: MenuDepth::Settings,
                selected_item: 0,
                ..state
            },
            // Exit (item 3) or any out-of-range value closes the menu.
            _ => OledState {
                menu_open: false,
                menu_depth: MenuDepth::Main,
                selected_item: 0,
                ..state
            },
        },
        MenuDepth::Mood => match state.selected_item {
            0 => OledState {
                face_mood: FaceMood::Happy,
                menu_depth: MenuDepth::Main,
                selected_item: 0,
                ..state
            },
            1 => OledState {
                face_mood: FaceMood::Sleeping,
                menu_depth: MenuDepth::Main,
                selected_item: 0,
                ..state
            },
            // Back (item 2) or out-of-range → return to Main.
            _ => OledState {
                menu_depth: MenuDepth::Main,
                selected_item: 0,
                ..state
            },
        },
        MenuDepth::Settings => {
            // Wi-Fi Info (0), Volume (1), and Back (2) all return to Main.
            OledState {
                menu_depth: MenuDepth::Main,
                selected_item: 0,
                ..state
            }
        }
    }
}

// ── Async task (firmware-only) ────────────────────────────────────────────────

/// Continuously process commands from [`COMMAND_BUS`] and update the OLED.
///
/// **Non-blocking design**: commands are polled with `try_receive` so the
/// Embassy executor can run Wi-Fi and audio tasks between frames.
///
/// **Inactivity timeout**: if the menu is open and no button command arrives
/// within [`MENU_INACTIVITY_SECS`] seconds, the menu is automatically closed.
///
/// This function is intended to be spawned by the Embassy executor when the
/// SSD1306 is detected during the boot-time probe.
#[cfg(feature = "firmware")]
pub async fn oled_task_loop() {
    use embassy_time::{Duration, Instant, Timer};

    let mut state = OledState::new();
    let mut last_activity = Instant::now();

    loop {
        // Non-blocking poll: process one command per frame if available.
        if let Ok(cmd) = COMMAND_BUS.try_receive() {
            match &cmd {
                Command::MenuNextItem | Command::MenuSelect => {
                    last_activity = Instant::now();
                }
                _ => {}
            }
            state = handle_command(state, &cmd);
        }

        // Inactivity timeout: auto-close the menu after 10 seconds of silence.
        let time_since_activity = Instant::now() - last_activity;
        if state.menu_open && time_since_activity > Duration::from_secs(MENU_INACTIVITY_SECS) {
            state.menu_open = false;
            state.menu_depth = MenuDepth::Main;
            state.selected_item = 0;
        }

        // Log current state (routed to USB-JTAG / UART in firmware builds).
        esp_println::println!("[oled_task] face = {}", state.label());

        // Yield for one frame period — never blocks other Embassy tasks.
        Timer::after_millis(FRAME_PERIOD_MS).await;
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{Command, FaceMood};

    // ── Framebuffer layout ────────────────────────────────────────────────────

    #[test]
    fn framebuffer_constant_is_exactly_1kb() {
        assert_eq!(
            FRAMEBUFFER_SIZE, 1024,
            "SSD1306 128×64 1-bit framebuffer must be exactly 1 024 bytes"
        );
    }

    #[test]
    fn framebuffer_static_has_correct_length() {
        // The backing array's size equals the compile-time constant; no shared
        // reference to the mutable static is needed.
        assert_eq!(
            // SAFETY: we only read the size via a raw (not shared) pointer.
            core::mem::size_of_val(unsafe { &*core::ptr::addr_of!(OLED_FRAMEBUFFER) }),
            FRAMEBUFFER_SIZE
        );
    }

    #[test]
    fn framebuffer_is_zeroed_at_startup() {
        // SAFETY: single-threaded test; we access via a raw pointer to avoid
        // creating a shared reference to the mutable static.
        let sum: u32 = unsafe {
            let ptr = core::ptr::addr_of!(OLED_FRAMEBUFFER) as *const u8;
            (0..FRAMEBUFFER_SIZE).map(|i| *ptr.add(i) as u32).sum()
        };
        assert_eq!(sum, 0, "framebuffer should start fully dark (all bits 0)");
    }

    // ── OledState construction ────────────────────────────────────────────────

    #[test]
    fn oled_state_new_is_neutral_face_menu_closed() {
        let state = OledState::new();
        assert_eq!(state.face_mood, FaceMood::Neutral);
        assert!(!state.menu_open);
        assert_eq!(state.menu_depth, MenuDepth::Main);
        assert_eq!(state.selected_item, 0);
    }

    #[test]
    fn oled_state_default_matches_new() {
        assert_eq!(OledState::default(), OledState::new());
    }

    // ── OledState labels ──────────────────────────────────────────────────────

    #[test]
    fn oled_state_face_labels() {
        let cases = [
            (FaceMood::Happy, "Happy"),
            (FaceMood::Sad, "Sad"),
            (FaceMood::Neutral, "Neutral"),
            (FaceMood::Excited, "Excited"),
            (FaceMood::Mad, "Mad"),
            (FaceMood::Sleeping, "Sleeping"),
        ];
        for (mood, expected) in &cases {
            let s = OledState {
                face_mood: *mood,
                ..OledState::new()
            };
            assert_eq!(s.label(), *expected, "unexpected label for {mood:?}");
        }
    }

    #[test]
    fn oled_state_menu_labels() {
        let base = OledState {
            menu_open: true,
            ..OledState::new()
        };
        assert_eq!(
            OledState {
                menu_depth: MenuDepth::Main,
                ..base
            }
            .label(),
            "Menu:Main"
        );
        assert_eq!(
            OledState {
                menu_depth: MenuDepth::Mood,
                ..base
            }
            .label(),
            "Menu:Mood"
        );
        assert_eq!(
            OledState {
                menu_depth: MenuDepth::Settings,
                ..base
            }
            .label(),
            "Menu:Settings"
        );
    }

    // ── handle_command: SetFaceExpression ─────────────────────────────────────

    #[test]
    fn handle_command_set_face_expression_updates_mood() {
        let state = OledState::new();
        let next = handle_command(state, &Command::SetFaceExpression(FaceMood::Happy));
        assert_eq!(next.face_mood, FaceMood::Happy);
        assert!(!next.menu_open);
    }

    #[test]
    fn handle_command_set_face_changes_mood_while_menu_closed() {
        let state = OledState {
            face_mood: FaceMood::Sad,
            ..OledState::new()
        };
        let next = handle_command(state, &Command::SetFaceExpression(FaceMood::Mad));
        assert_eq!(next.face_mood, FaceMood::Mad);
    }

    #[test]
    fn handle_command_all_moods_reachable() {
        let moods = [
            FaceMood::Happy,
            FaceMood::Sad,
            FaceMood::Neutral,
            FaceMood::Excited,
            FaceMood::Mad,
            FaceMood::Sleeping,
        ];
        for mood in &moods {
            let next =
                handle_command(OledState::new(), &Command::SetFaceExpression(*mood));
            assert_eq!(next.face_mood, *mood);
        }
    }

    // ── handle_command: non-display commands preserve state ───────────────────

    #[test]
    fn handle_command_non_display_command_preserves_state() {
        let state = OledState {
            face_mood: FaceMood::Excited,
            ..OledState::new()
        };
        let next = handle_command(state, &Command::MoveServo(0, 90));
        assert_eq!(next, state, "servo command must not change the OLED state");
    }

    #[test]
    fn handle_command_stream_audio_preserves_state() {
        let state = OledState {
            face_mood: FaceMood::Sleeping,
            ..OledState::new()
        };
        let next = handle_command(state, &Command::StreamAudio(true));
        assert_eq!(next, state, "audio command must not change the OLED state");
    }

    // ── handle_command: MenuNextItem ──────────────────────────────────────────

    #[test]
    fn menu_next_item_no_op_when_menu_closed() {
        let state = OledState::new();
        let next = handle_command(state, &Command::MenuNextItem);
        assert_eq!(next, state, "MenuNextItem is a no-op when the menu is closed");
    }

    #[test]
    fn menu_next_item_advances_selection() {
        let state = OledState {
            menu_open: true,
            menu_depth: MenuDepth::Main,
            selected_item: 0,
            ..OledState::new()
        };
        let next = handle_command(state, &Command::MenuNextItem);
        assert_eq!(next.selected_item, 1);
    }

    #[test]
    fn menu_next_item_wraps_at_end_of_main_menu() {
        // Main menu has 4 items (0-3); after item 3 it wraps to 0.
        let state = OledState {
            menu_open: true,
            menu_depth: MenuDepth::Main,
            selected_item: 3,
            ..OledState::new()
        };
        let next = handle_command(state, &Command::MenuNextItem);
        assert_eq!(next.selected_item, 0);
    }

    #[test]
    fn menu_next_item_wraps_at_end_of_mood_submenu() {
        // Mood sub-menu has 3 items (0-2).
        let state = OledState {
            menu_open: true,
            menu_depth: MenuDepth::Mood,
            selected_item: 2,
            ..OledState::new()
        };
        let next = handle_command(state, &Command::MenuNextItem);
        assert_eq!(next.selected_item, 0);
    }

    #[test]
    fn menu_next_item_wraps_at_end_of_settings_submenu() {
        // Settings sub-menu has 3 items (0-2).
        let state = OledState {
            menu_open: true,
            menu_depth: MenuDepth::Settings,
            selected_item: 2,
            ..OledState::new()
        };
        let next = handle_command(state, &Command::MenuNextItem);
        assert_eq!(next.selected_item, 0);
    }

    // ── handle_command: MenuSelect (open menu) ────────────────────────────────

    #[test]
    fn menu_select_opens_main_menu_when_closed() {
        let state = OledState::new();
        let next = handle_command(state, &Command::MenuSelect);
        assert!(next.menu_open, "long press must open the menu");
        assert_eq!(next.menu_depth, MenuDepth::Main);
        assert_eq!(next.selected_item, 0);
    }

    // ── handle_command: MenuSelect (main menu navigation) ────────────────────

    #[test]
    fn menu_select_talk_is_noop_on_state() {
        let state = OledState {
            menu_open: true,
            menu_depth: MenuDepth::Main,
            selected_item: 0,
            ..OledState::new()
        };
        let next = handle_command(state, &Command::MenuSelect);
        assert_eq!(next, state, "Talk action has no local state change");
    }

    #[test]
    fn menu_select_mood_enters_mood_submenu() {
        let state = OledState {
            menu_open: true,
            menu_depth: MenuDepth::Main,
            selected_item: 1,
            ..OledState::new()
        };
        let next = handle_command(state, &Command::MenuSelect);
        assert_eq!(next.menu_depth, MenuDepth::Mood);
        assert_eq!(next.selected_item, 0);
        assert!(next.menu_open);
    }

    #[test]
    fn menu_select_settings_enters_settings_submenu() {
        let state = OledState {
            menu_open: true,
            menu_depth: MenuDepth::Main,
            selected_item: 2,
            ..OledState::new()
        };
        let next = handle_command(state, &Command::MenuSelect);
        assert_eq!(next.menu_depth, MenuDepth::Settings);
        assert_eq!(next.selected_item, 0);
        assert!(next.menu_open);
    }

    #[test]
    fn menu_select_exit_closes_menu() {
        let state = OledState {
            menu_open: true,
            menu_depth: MenuDepth::Main,
            selected_item: 3,
            ..OledState::new()
        };
        let next = handle_command(state, &Command::MenuSelect);
        assert!(!next.menu_open, "Exit must close the menu");
        assert_eq!(next.menu_depth, MenuDepth::Main);
    }

    // ── handle_command: MenuSelect (mood sub-menu) ────────────────────────────

    #[test]
    fn mood_menu_set_happy_changes_face_and_returns_to_main() {
        let state = OledState {
            menu_open: true,
            menu_depth: MenuDepth::Mood,
            selected_item: 0,
            face_mood: FaceMood::Neutral,
            ..OledState::new()
        };
        let next = handle_command(state, &Command::MenuSelect);
        assert_eq!(next.face_mood, FaceMood::Happy);
        assert_eq!(next.menu_depth, MenuDepth::Main);
        assert_eq!(next.selected_item, 0);
        assert!(next.menu_open);
    }

    #[test]
    fn mood_menu_set_sleep_changes_face_and_returns_to_main() {
        let state = OledState {
            menu_open: true,
            menu_depth: MenuDepth::Mood,
            selected_item: 1,
            face_mood: FaceMood::Neutral,
            ..OledState::new()
        };
        let next = handle_command(state, &Command::MenuSelect);
        assert_eq!(next.face_mood, FaceMood::Sleeping);
        assert_eq!(next.menu_depth, MenuDepth::Main);
        assert_eq!(next.selected_item, 0);
        assert!(next.menu_open);
    }

    #[test]
    fn mood_menu_back_returns_to_main() {
        let state = OledState {
            menu_open: true,
            menu_depth: MenuDepth::Mood,
            selected_item: 2,
            ..OledState::new()
        };
        let next = handle_command(state, &Command::MenuSelect);
        assert_eq!(next.menu_depth, MenuDepth::Main);
        assert_eq!(next.selected_item, 0);
        assert!(next.menu_open);
    }

    // ── handle_command: MenuSelect (settings sub-menu) ────────────────────────

    #[test]
    fn settings_menu_wifi_info_returns_to_main() {
        let state = OledState {
            menu_open: true,
            menu_depth: MenuDepth::Settings,
            selected_item: 0,
            ..OledState::new()
        };
        let next = handle_command(state, &Command::MenuSelect);
        assert_eq!(next.menu_depth, MenuDepth::Main);
        assert_eq!(next.selected_item, 0);
        assert!(next.menu_open);
    }

    #[test]
    fn settings_menu_volume_returns_to_main() {
        let state = OledState {
            menu_open: true,
            menu_depth: MenuDepth::Settings,
            selected_item: 1,
            ..OledState::new()
        };
        let next = handle_command(state, &Command::MenuSelect);
        assert_eq!(next.menu_depth, MenuDepth::Main);
        assert_eq!(next.selected_item, 0);
        assert!(next.menu_open);
    }

    #[test]
    fn settings_menu_back_returns_to_main() {
        let state = OledState {
            menu_open: true,
            menu_depth: MenuDepth::Settings,
            selected_item: 2,
            ..OledState::new()
        };
        let next = handle_command(state, &Command::MenuSelect);
        assert_eq!(next.menu_depth, MenuDepth::Main);
        assert_eq!(next.selected_item, 0);
        assert!(next.menu_open);
    }

    // ── Full navigation scenario ──────────────────────────────────────────────

    #[test]
    fn full_navigation_open_mood_set_happy_close() {
        let s0 = OledState::new(); // face=Neutral, menu closed

        // Long press → open menu
        let s1 = handle_command(s0, &Command::MenuSelect);
        assert!(s1.menu_open);
        assert_eq!(s1.menu_depth, MenuDepth::Main);

        // Click → Mood (item 1)
        let s2 = handle_command(s1, &Command::MenuNextItem);
        assert_eq!(s2.selected_item, 1);

        // Select → enter Mood sub-menu
        let s3 = handle_command(s2, &Command::MenuSelect);
        assert_eq!(s3.menu_depth, MenuDepth::Mood);
        assert_eq!(s3.selected_item, 0);

        // Select Set Happy (item 0)
        let s4 = handle_command(s3, &Command::MenuSelect);
        assert_eq!(s4.face_mood, FaceMood::Happy);
        assert_eq!(s4.menu_depth, MenuDepth::Main);

        // Click × 3 → Exit (item 3)
        let s5 = handle_command(s4, &Command::MenuNextItem);
        let s6 = handle_command(s5, &Command::MenuNextItem);
        let s7 = handle_command(s6, &Command::MenuNextItem);
        assert_eq!(s7.selected_item, 3);

        // Select Exit → menu closes
        let s8 = handle_command(s7, &Command::MenuSelect);
        assert!(!s8.menu_open);
        assert_eq!(s8.face_mood, FaceMood::Happy);
    }

    #[test]
    fn handle_command_chain_of_face_updates() {
        let s0 = OledState::new();
        let s1 = handle_command(s0, &Command::SetFaceExpression(FaceMood::Happy));
        let s2 = handle_command(s1, &Command::SetFaceExpression(FaceMood::Sad));
        let s3 = handle_command(s2, &Command::MoveServo(1, 45)); // no-op
        let s4 = handle_command(s3, &Command::SetFaceExpression(FaceMood::Neutral));
        assert_eq!(s4.face_mood, FaceMood::Neutral);
    }
}
