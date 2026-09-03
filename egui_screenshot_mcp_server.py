#!/usr/bin/env python3
"""
MCP server that lets an AI agent see and control a *specific* running
egui/eframe app: screenshots plus mouse and keyboard input.

How it works
------------
Instead of grabbing whatever window happens to be on screen or driving the
OS-level mouse/keyboard (which would work on any window, not just yours),
this server writes small request files. Your egui app polls for them every
frame (see screenshot_bridge.rs / AgentBridge) and, when it finds one,
either renders a screenshot via `ViewportCommand::Screenshot` or injects
synthetic `egui::Event`s (pointer moves, clicks, key presses, text, paste)
directly into its own event queue. Because all of this happens *inside*
your app's own render loop, there is no way for this tool to ever see or
control any other window, application, or desktop content.

Coordinates are in egui's logical points — the same coordinate space your
UI code lays out in — not raw screenshot pixels. If your display has a
scale factor other than 1.0, pixel coordinates from a screenshot need to be
divided by that scale factor before being used as click coordinates.

Modifiers (command/shift/alt/ctrl): `command` means this app's own primary
shortcut modifier — Cmd on macOS, Ctrl elsewhere — matching what
`i.modifiers.command` checks in egui code, so callers don't need to know
which platform the target app is running on.

Setup
-----
    pip install "mcp>=2"

Run standalone (for testing with the MCP inspector):
    python3 egui_screenshot_mcp_server.py

Wire into an MCP client (e.g. Claude Code) — see README.md for the JSON config.
"""

import json
import os
import time
from pathlib import Path
from typing import Literal

from mcp.server.mcpserver import MCPServer, Image

# Where the app and this server rendezvous. Override with env vars if you
# want multiple app instances / multiple bridges running at once.
BRIDGE_DIR = Path(
    os.environ.get("EGUI_SCREENSHOT_DIR", Path.home() / ".egui_screenshot")
)
TRIGGER_FILE = BRIDGE_DIR / "trigger"
OUTPUT_FILE = BRIDGE_DIR / "output.png"
ERROR_FILE = BRIDGE_DIR / "error"

ACTIONS_FILE = BRIDGE_DIR / "actions.json"
ACTIONS_DONE_FILE = BRIDGE_DIR / "actions_done"
ACTIONS_ERROR_FILE = BRIDGE_DIR / "actions_error"

TIMEOUT_SECONDS = float(os.environ.get("EGUI_SCREENSHOT_TIMEOUT", "10"))
POLL_INTERVAL_SECONDS = 0.05

MouseButtonName = Literal["primary", "secondary", "middle"]

server = MCPServer(
    name="egui-screenshot",
    instructions=(
        "Sees and controls exactly one running egui/eframe application — the "
        "one wired up with screenshot_bridge.rs (AgentBridge). These tools "
        "cannot see or control any other window, application, or desktop "
        "content. Coordinates are in egui logical points, matching the "
        "app's own UI layout coordinates — not raw screenshot pixel "
        "coordinates if the display is scaled. `command` on mouse/key tools "
        "means this app's own primary shortcut modifier (Cmd on macOS, Ctrl "
        "elsewhere) — use it for shortcuts like save/undo/redo. If a call "
        "times out, the app is either not running or not calling "
        "AgentBridge::poll(ctx) every frame."
    ),
)


def _modifiers(command: bool, shift: bool, alt: bool, ctrl: bool) -> dict:
    return {"command": command, "shift": shift, "alt": alt, "ctrl": ctrl}


def _run_actions(actions: list[dict]) -> None:
    """Write an action batch and block until the app has applied all of it."""
    BRIDGE_DIR.mkdir(parents=True, exist_ok=True)
    ACTIONS_DONE_FILE.unlink(missing_ok=True)
    ACTIONS_ERROR_FILE.unlink(missing_ok=True)

    ACTIONS_FILE.write_text(json.dumps(actions))

    deadline = time.time() + TIMEOUT_SECONDS
    while time.time() < deadline:
        if ACTIONS_ERROR_FILE.exists():
            message = ACTIONS_ERROR_FILE.read_text(errors="replace").strip()
            ACTIONS_ERROR_FILE.unlink(missing_ok=True)
            raise RuntimeError(f"App reported an input error: {message}")
        if ACTIONS_DONE_FILE.exists():
            ACTIONS_DONE_FILE.unlink(missing_ok=True)
            return
        time.sleep(POLL_INTERVAL_SECONDS)

    raise TimeoutError(
        f"Input action did not complete within {TIMEOUT_SECONDS}s. "
        "Is the app running, and is AgentBridge::poll(ctx) being called "
        "every frame inside your App::update()?"
    )


@server.tool()
def take_app_screenshot() -> Image:
    """Capture a screenshot of the running egui application.

    Requires the app to be running with the screenshot_bridge module active.
    Returns a PNG image of exactly what the app is currently rendering.
    """
    BRIDGE_DIR.mkdir(parents=True, exist_ok=True)

    # Clean up any stale state from a previous call.
    for f in (OUTPUT_FILE, ERROR_FILE):
        f.unlink(missing_ok=True)

    TRIGGER_FILE.touch()

    deadline = time.time() + TIMEOUT_SECONDS
    while time.time() < deadline:
        if ERROR_FILE.exists():
            message = ERROR_FILE.read_text(errors="replace").strip()
            ERROR_FILE.unlink(missing_ok=True)
            raise RuntimeError(f"App reported a screenshot error: {message}")
        if OUTPUT_FILE.exists():
            break
        time.sleep(POLL_INTERVAL_SECONDS)
    else:
        TRIGGER_FILE.unlink(missing_ok=True)
        raise TimeoutError(
            f"No screenshot appeared within {TIMEOUT_SECONDS}s. "
            "Is the app running, and is ScreenshotBridge::poll(ctx) being "
            "called every frame inside your App::update()?"
        )

    # Small settle delay in case the writer is still flushing.
    time.sleep(0.05)
    data = OUTPUT_FILE.read_bytes()
    OUTPUT_FILE.unlink(missing_ok=True)

    return Image(data=data, format="png")


@server.tool()
def mouse_move(x: float, y: float) -> str:
    """Move the mouse pointer to (x, y) in the app's logical UI coordinates."""
    _run_actions([{"type": "mouse_move", "x": x, "y": y}])
    return f"Moved pointer to ({x}, {y})."


@server.tool()
def mouse_click(
    x: float,
    y: float,
    button: MouseButtonName = "primary",
    command: bool = False,
    shift: bool = False,
    alt: bool = False,
    ctrl: bool = False,
) -> str:
    """Move to (x, y) and click. Use this for buttons, checkboxes, menu items, etc.

    Set command/shift/alt/ctrl for modifier-held clicks (e.g. shift-click to
    extend a selection, command-click to toggle one cell into a selection).
    """
    modifiers = _modifiers(command, shift, alt, ctrl)
    _run_actions(
        [
            {"type": "mouse_move", "x": x, "y": y},
            {"type": "mouse_down", "x": x, "y": y, "button": button, "modifiers": modifiers},
            {"type": "mouse_up", "x": x, "y": y, "button": button, "modifiers": modifiers},
        ]
    )
    held = ",".join(k for k, v in modifiers.items() if v) or "none"
    return f"Clicked ({button}) at ({x}, {y}) with modifiers: {held}."


@server.tool()
def mouse_down(
    x: float,
    y: float,
    button: MouseButtonName = "primary",
    command: bool = False,
    shift: bool = False,
    alt: bool = False,
    ctrl: bool = False,
) -> str:
    """Press and hold a mouse button at (x, y). Pair with mouse_up to drag."""
    modifiers = _modifiers(command, shift, alt, ctrl)
    _run_actions([{"type": "mouse_down", "x": x, "y": y, "button": button, "modifiers": modifiers}])
    return f"Pressed ({button}) at ({x}, {y})."


@server.tool()
def mouse_up(
    x: float,
    y: float,
    button: MouseButtonName = "primary",
    command: bool = False,
    shift: bool = False,
    alt: bool = False,
    ctrl: bool = False,
) -> str:
    """Release a mouse button at (x, y)."""
    modifiers = _modifiers(command, shift, alt, ctrl)
    _run_actions([{"type": "mouse_up", "x": x, "y": y, "button": button, "modifiers": modifiers}])
    return f"Released ({button}) at ({x}, {y})."


@server.tool()
def scroll(dx: float = 0.0, dy: float = 0.0) -> str:
    """Scroll the content under the pointer. Positive dy scrolls down, positive dx scrolls right."""
    _run_actions([{"type": "scroll", "dx": dx, "dy": dy}])
    return f"Scrolled by ({dx}, {dy})."


@server.tool()
def key_press(
    key: str,
    command: bool = False,
    shift: bool = False,
    alt: bool = False,
    ctrl: bool = False,
) -> str:
    """Press and release a single key, optionally with modifiers held.

    Supports letters (a-z), digits (0-9), arrow keys (up/down/left/right),
    and: enter, escape, tab, backspace, space, insert, delete, home, end,
    pageup, pagedown, f1-f12. For typing sentences/words, use type_text
    instead — it's far more efficient and handles all characters. Use
    command=True for shortcuts (e.g. key_press("s", command=True) for
    save, key_press("z", command=True) for undo,
    key_press("z", command=True, shift=True) for redo).
    """
    modifiers = _modifiers(command, shift, alt, ctrl)
    _run_actions(
        [
            {"type": "key", "key": key, "pressed": True, "modifiers": modifiers},
            {"type": "key", "key": key, "pressed": False, "modifiers": modifiers},
        ]
    )
    held = ",".join(k for k, v in modifiers.items() if v) or "none"
    return f"Pressed key '{key}' with modifiers: {held}."


@server.tool()
def type_text(text: str) -> str:
    """Type a string of text into whatever text field currently has focus.

    Click the target text field first with mouse_click so it has focus.
    Types character-by-character, as if the user typed it. For simulating a
    clipboard paste instead, use paste_text.
    """
    _run_actions([{"type": "text", "text": text}])
    return f"Typed {len(text)} characters."


@server.tool()
def paste_text(text: str) -> str:
    """Simulate pasting `text` (Cmd/Ctrl+V) into whatever currently has focus.

    Distinct from type_text: some apps handle paste differently from typed
    input (e.g. treating pasted text as a structured block — rows/columns of
    a spreadsheet-style paste — rather than character-by-character entry).
    Click the target first with mouse_click so it has focus.
    """
    _run_actions([{"type": "paste", "text": text}])
    return f"Pasted {len(text)} characters."


if __name__ == "__main__":
    server.run(transport="stdio")
