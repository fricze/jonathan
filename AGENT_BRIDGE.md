# egui agent bridge — jonathan setup

Two pieces that talk to each other through a tiny file-based protocol in
`~/.egui_screenshot/` (or `$EGUI_SCREENSHOT_DIR` if you set it):

- **`src/screenshot_bridge.rs`** (defines `AgentBridge`) — wired into
  `MyApp` (see `main.rs` / `app.rs`). Polls for requests each frame and,
  when found, either asks egui to render a screenshot, or injects synthetic
  mouse/keyboard events directly into egui's own event queue.
- **`egui_screenshot_mcp_server.py`** (repo root) — an MCP server exposing
  tools for screenshots plus mouse/keyboard control.

Because everything happens inside the app's own render loop, the MCP tools
have no way to see or control anything except this app's own frame — no
other windows, no desktop, no OS-level input, nothing.

This copy differs from the upstream bridge in two ways this app needed:
modifier support (`command`/`shift`/`alt`/`ctrl`) on clicks and key
presses, and a `paste_text` tool that fires `egui::Event::Paste` — the
event `handle_clipboard_paste` in `new_table.rs` actually listens for.
Nearly every shortcut in this app (Cmd+S save, Cmd+Z/Cmd+Shift+Z
undo/redo, Cmd+F filter focus, Cmd+click/Shift+click cell selection,
Cmd+Backspace/Cmd+Enter row insert/delete) needs a modifier held, so the
unmodified upstream bridge couldn't drive most of the app.

## Available tools

- `take_app_screenshot()` — returns a PNG of the app's current frame
- `mouse_move(x, y)`
- `mouse_click(x, y, button="primary"|"secondary"|"middle", command=False, shift=False, alt=False, ctrl=False)`
- `mouse_down(x, y, button, ...modifiers)` / `mouse_up(x, y, button, ...modifiers)` — for drags
- `scroll(dx, dy)`
- `key_press(key, command=False, shift=False, alt=False, ctrl=False)` — letters,
  digits, arrows, enter, escape, tab, backspace, space, insert, delete,
  home, end, pageup, pagedown, f1–f12
- `type_text(text)` — types a whole string character-by-character into the
  focused text field
- `paste_text(text)` — simulates a clipboard paste (`egui::Event::Paste`)
  into whatever has focus; use this for the CSV multi-cell paste feature,
  not `type_text`

`command` means this app's own primary shortcut modifier — Cmd on macOS,
Ctrl elsewhere — matching what `i.modifiers.command` checks throughout the
codebase.

**Coordinates are in egui's logical points** — the same space the UI
layout code uses — not raw screenshot pixel coordinates. If the display
has a scale factor other than 1.0, a screenshot's pixel dimensions won't
match click coordinates 1:1; divide pixel coordinates by the scale factor
first.

**Clicking into a text field before typing/pasting**: `type_text` and
`paste_text` act on whatever currently has focus. Send `mouse_click` on
the target first so egui gives it focus.

## Setup

Already wired into this repo — `src/screenshot_bridge.rs` is a module,
`MyApp.agent_bridge` is polled every frame in `app.rs::update_inner`, and
`Cargo.toml` has the `image`/`serde`(derive)/`serde_json` dependencies it
needs. Nothing to change to use it locally.

```bash
pip install "mcp>=2"
```

Quick smoke test (run `cargo run` first, in another terminal):

```bash
python3 egui_screenshot_mcp_server.py
```

This starts the server on stdio, which is how MCP clients talk to it — you
won't see obvious output, that's expected. Use the MCP inspector if you
want to poke it interactively:

```bash
npx @modelcontextprotocol/inspector python3 egui_screenshot_mcp_server.py
```

### Register it with Claude Code

```bash
claude mcp add egui-screenshot -- python3 /absolute/path/to/jonathan/egui_screenshot_mcp_server.py
```

## Usage

With the app running (`cargo run`), the agent can loop: screenshot →
decide → click/type/paste → screenshot again, all scoped to that one
window. A typical sequence: `take_app_screenshot`, `mouse_click(x, y)` on
a cell seen in the screenshot, `key_press("z", command=True)` to test
undo, `take_app_screenshot` again to confirm the result.

## Notes

- **Multiple app instances**: set a distinct `EGUI_SCREENSHOT_DIR` per
  instance (and matching MCP config entries) if more than one needs to run
  at once.
- **Timeout**: defaults to 10s (`EGUI_SCREENSHOT_TIMEOUT` env var). If the
  app isn't running or isn't calling `poll()`, the tool call fails with a
  clear timeout error rather than hanging.
- **Headless/CI**: since this doesn't touch the window manager or OS input
  at all, it works the same over SSH, in a container, or with the window
  occluded/unfocused — unlike OS-level screenshot and input tools.
- **Click/key timing**: `AgentBridge` deliberately applies one action per
  frame (so a click's press and release land on separate frames, matching
  what egui widgets expect). A `mouse_click` or `key_press` therefore takes
  a couple of frames to complete — invisible in practice.
- **Key coverage**: `key_press` supports a conservative, version-stable
  subset of `egui::Key` — letters, digits, arrows, and common
  editing/navigation keys, no punctuation. Extend `key_from_name` in
  `screenshot_bridge.rs` if a test needs one that's missing.
