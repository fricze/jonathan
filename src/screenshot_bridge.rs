//! Agent bridge for egui/eframe apps: screenshots + synthetic mouse/keyboard input.
//!
//! Lets an external process (e.g. `egui_screenshot_mcp_server.py`) drive this
//! app specifically — take screenshots, move/click the mouse, type text, and
//! press keys — via a small file-based protocol. Nothing here can see or
//! touch any other window: input is injected directly into *this* egui
//! context's event queue, and screenshots are rendered by *this* app's own
//! `ViewportCommand::Screenshot`.
//!
//! Modifier support (command/shift/alt/ctrl) and a Paste action were added
//! for jonathan specifically: nearly every shortcut in this app (save,
//! undo/redo, filter-focus, multi-select, row insert/delete) is gated on
//! Cmd or Shift, and paste is driven by `egui::Event::Paste`, not
//! `Event::Text` — the upstream bridge had neither.
//!
//! ## Usage
//!
//! ```ignore
//! struct MyApp {
//!     agent_bridge: AgentBridge,
//!     // ...your other fields
//! }
//!
//! impl eframe::App for MyApp {
//!     fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
//!         self.agent_bridge.poll(ctx);
//!
//!         egui::CentralPanel::default().show(ctx, |ui| {
//!             // ...your UI
//!         });
//!     }
//! }
//! ```

use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;

pub struct AgentBridge {
    dir: PathBuf,
    trigger_path: PathBuf,
    output_path: PathBuf,
    screenshot_error_path: PathBuf,
    actions_path: PathBuf,
    actions_done_path: PathBuf,
    actions_error_path: PathBuf,

    screenshot_pending: bool,
    action_queue: VecDeque<Action>,
}

impl AgentBridge {
    /// Uses `$EGUI_SCREENSHOT_DIR` if set, otherwise `~/.egui_screenshot`.
    /// Must match the directory the MCP server is configured to use.
    pub fn new() -> Self {
        let dir = std::env::var_os("EGUI_SCREENSHOT_DIR")
            .map(PathBuf::from)
            .or_else(|| home_dir().map(|h| h.join(".egui_screenshot")))
            .unwrap_or_else(std::env::temp_dir);

        if let Err(e) = fs::create_dir_all(&dir) {
            eprintln!("agent_bridge: failed to create {dir:?}: {e}");
        }

        Self {
            trigger_path: dir.join("trigger"),
            output_path: dir.join("output.png"),
            screenshot_error_path: dir.join("error"),
            actions_path: dir.join("actions.json"),
            actions_done_path: dir.join("actions_done"),
            actions_error_path: dir.join("actions_error"),
            dir,
            screenshot_pending: false,
            action_queue: VecDeque::new(),
        }
    }

    /// Call once per frame, at the top of `App::update()`, before building
    /// your UI.
    pub fn poll(&mut self, ctx: &egui::Context) {
        self.poll_screenshot(ctx);
        self.poll_actions(ctx);

        // Keep the app ticking even if it's otherwise idle (egui normally
        // only repaints in response to input), so requests dropped while
        // idle still get picked up promptly. Drain queued actions quickly;
        // otherwise fall back to a slower idle poll.
        let interval = if self.action_queue.is_empty() {
            Duration::from_millis(100)
        } else {
            Duration::from_millis(8)
        };
        ctx.request_repaint_after(interval);
    }

    // ---- screenshots ----------------------------------------------------

    fn poll_screenshot(&mut self, ctx: &egui::Context) {
        if !self.screenshot_pending && self.trigger_path.exists() {
            let _ = fs::remove_file(&self.trigger_path);
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(
                egui::UserData::default(),
            ));
            self.screenshot_pending = true;
        }

        if self.screenshot_pending {
            let mut captured: Option<Arc<egui::ColorImage>> = None;
            ctx.input(|i| {
                for event in &i.raw.events {
                    if let egui::Event::Screenshot { image, .. } = event {
                        captured = Some(image.clone());
                    }
                }
            });

            if let Some(color_image) = captured {
                self.write_screenshot(&color_image);
                self.screenshot_pending = false;
            }
        }
    }

    fn write_screenshot(&self, color_image: &egui::ColorImage) {
        let pixels = color_image.as_raw();
        let result = image::save_buffer(
            &self.output_path,
            pixels,
            color_image.width() as u32,
            color_image.height() as u32,
            image::ColorType::Rgba8,
        );

        if let Err(e) = result {
            eprintln!("agent_bridge: failed to save PNG: {e}");
            let _ = fs::write(&self.screenshot_error_path, e.to_string());
        }
    }

    // ---- mouse / keyboard actions ----------------------------------------

    fn poll_actions(&mut self, ctx: &egui::Context) {
        // Load a newly-dropped batch once the previous one is fully drained.
        if self.action_queue.is_empty() && self.actions_path.exists() {
            match fs::read_to_string(&self.actions_path) {
                Ok(data) => match serde_json::from_str::<Vec<Action>>(&data) {
                    Ok(actions) => self.action_queue = actions.into(),
                    Err(e) => {
                        let _ = fs::write(
                            &self.actions_error_path,
                            format!("failed to parse actions.json: {e}"),
                        );
                    }
                },
                Err(e) => {
                    let _ = fs::write(
                        &self.actions_error_path,
                        format!("failed to read actions.json: {e}"),
                    );
                }
            }
            let _ = fs::remove_file(&self.actions_path);
        }

        // Apply exactly one action per frame. This matters for mouse/key
        // down+up pairs: egui expects the press and release on separate
        // frames to register clicks and key events reliably.
        if let Some(action) = self.action_queue.pop_front() {
            if let Err(e) = self.apply_action(ctx, &action) {
                let _ = fs::write(&self.actions_error_path, e);
            }
            if self.action_queue.is_empty() {
                let _ = fs::write(&self.actions_done_path, "");
            }
        }
    }

    fn apply_action(&self, ctx: &egui::Context, action: &Action) -> Result<(), String> {
        match action {
            Action::MouseMove { x, y } => {
                push_event(ctx, egui::Event::PointerMoved(egui::pos2(*x, *y)));
            }
            Action::MouseDown { x, y, button, modifiers } => {
                push_event(ctx, egui::Event::PointerMoved(egui::pos2(*x, *y)));
                push_event(
                    ctx,
                    egui::Event::PointerButton {
                        pos: egui::pos2(*x, *y),
                        button: (*button).into(),
                        pressed: true,
                        modifiers: (*modifiers).into(),
                    },
                );
            }
            Action::MouseUp { x, y, button, modifiers } => {
                push_event(
                    ctx,
                    egui::Event::PointerButton {
                        pos: egui::pos2(*x, *y),
                        button: (*button).into(),
                        pressed: false,
                        modifiers: (*modifiers).into(),
                    },
                );
            }
            Action::Scroll { dx, dy } => {
                push_event(
                    ctx,
                    egui::Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Point,
                        delta: egui::vec2(*dx, *dy),
                        modifiers: egui::Modifiers::default(),
                    },
                );
            }
            Action::Key { key, pressed, modifiers } => {
                let egui_key = key_from_name(key)
                    .ok_or_else(|| format!("agent_bridge: unrecognized key name '{key}'"))?;
                push_event(
                    ctx,
                    egui::Event::Key {
                        key: egui_key,
                        physical_key: Some(egui_key),
                        pressed: *pressed,
                        repeat: false,
                        modifiers: (*modifiers).into(),
                    },
                );
            }
            Action::Text { text } => {
                push_event(ctx, egui::Event::Text(text.clone()));
            }
            Action::Paste { text } => {
                push_event(ctx, egui::Event::Paste(text.clone()));
            }
        }
        Ok(())
    }
}

impl Default for AgentBridge {
    fn default() -> Self {
        Self::new()
    }
}

fn push_event(ctx: &egui::Context, event: egui::Event) {
    ctx.input_mut(|i| i.events.push(event));
}

/// Minimal home-dir lookup without pulling in the `dirs` crate.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

// ---- wire protocol --------------------------------------------------------

#[derive(Deserialize, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
enum MouseButton {
    #[default]
    Primary,
    Secondary,
    Middle,
}

impl From<MouseButton> for egui::PointerButton {
    fn from(b: MouseButton) -> Self {
        match b {
            MouseButton::Primary => egui::PointerButton::Primary,
            MouseButton::Secondary => egui::PointerButton::Secondary,
            MouseButton::Middle => egui::PointerButton::Middle,
        }
    }
}

/// Wire-format modifier keys. `command` means "the app's own primary
/// shortcut modifier" -- Cmd on macOS, Ctrl elsewhere -- matching how egui's
/// own `Modifiers::command` and every `i.modifiers.command` check in this
/// codebase already behave, so the MCP tools don't need to know what
/// platform they're driving.
#[derive(Deserialize, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
struct WireModifiers {
    #[serde(default)]
    command: bool,
    #[serde(default)]
    shift: bool,
    #[serde(default)]
    alt: bool,
    #[serde(default)]
    ctrl: bool,
}

impl From<WireModifiers> for egui::Modifiers {
    fn from(m: WireModifiers) -> Self {
        egui::Modifiers {
            alt: m.alt,
            ctrl: m.ctrl,
            shift: m.shift,
            // Set both platform-specific fields so `command` reads true
            // regardless of which one this build of egui checks -- macOS
            // code (this app's target) uses mac_cmd, egui's own
            // Modifiers::command getter ORs the two together anyway.
            mac_cmd: m.command,
            command: m.command,
        }
    }
}

#[derive(Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Action {
    MouseMove {
        x: f32,
        y: f32,
    },
    MouseDown {
        x: f32,
        y: f32,
        #[serde(default)]
        button: MouseButton,
        #[serde(default)]
        modifiers: WireModifiers,
    },
    MouseUp {
        x: f32,
        y: f32,
        #[serde(default)]
        button: MouseButton,
        #[serde(default)]
        modifiers: WireModifiers,
    },
    Scroll {
        dx: f32,
        dy: f32,
    },
    Key {
        key: String,
        pressed: bool,
        #[serde(default)]
        modifiers: WireModifiers,
    },
    Text {
        text: String,
    },
    /// Simulates a clipboard paste (Cmd+V) -- distinct from `Text`, which
    /// types character-by-character into a focused widget. This app's own
    /// paste handling listens for `egui::Event::Paste` specifically.
    Paste {
        text: String,
    },
}

/// Maps a handful of well-known key names to `egui::Key`. Deliberately
/// conservative: only variants confirmed stable across recent egui
/// releases. Letters, digits, arrows, and the common editing/navigation
/// keys. Extend this if you need more (e.g. punctuation keys) — check the
/// `egui::Key` enum for your pinned egui version first.
fn key_from_name(name: &str) -> Option<egui::Key> {
    use egui::Key::*;
    Some(match name {
        "ArrowDown" | "Down" => ArrowDown,
        "ArrowLeft" | "Left" => ArrowLeft,
        "ArrowRight" | "Right" => ArrowRight,
        "ArrowUp" | "Up" => ArrowUp,
        "Escape" | "Esc" => Escape,
        "Tab" => Tab,
        "Backspace" => Backspace,
        "Enter" | "Return" => Enter,
        "Space" => Space,
        "Insert" => Insert,
        "Delete" => Delete,
        "Home" => Home,
        "End" => End,
        "PageUp" => PageUp,
        "PageDown" => PageDown,
        "0" | "Num0" => Num0,
        "1" | "Num1" => Num1,
        "2" | "Num2" => Num2,
        "3" | "Num3" => Num3,
        "4" | "Num4" => Num4,
        "5" | "Num5" => Num5,
        "6" | "Num6" => Num6,
        "7" | "Num7" => Num7,
        "8" | "Num8" => Num8,
        "9" | "Num9" => Num9,
        "A" | "a" => A,
        "B" | "b" => B,
        "C" | "c" => C,
        "D" | "d" => D,
        "E" | "e" => E,
        "F" | "f" => F,
        "G" | "g" => G,
        "H" | "h" => H,
        "I" | "i" => I,
        "J" | "j" => J,
        "K" | "k" => K,
        "L" | "l" => L,
        "M" | "m" => M,
        "N" | "n" => N,
        "O" | "o" => O,
        "P" | "p" => P,
        "Q" | "q" => Q,
        "R" | "r" => R,
        "S" | "s" => S,
        "T" | "t" => T,
        "U" | "u" => U,
        "V" | "v" => V,
        "W" | "w" => W,
        "X" | "x" => X,
        "Y" | "y" => Y,
        "Z" | "z" => Z,
        "F1" => F1,
        "F2" => F2,
        "F3" => F3,
        "F4" => F4,
        "F5" => F5,
        "F6" => F6,
        "F7" => F7,
        "F8" => F8,
        "F9" => F9,
        "F10" => F10,
        "F11" => F11,
        "F12" => F12,
        _ => return None,
    })
}
