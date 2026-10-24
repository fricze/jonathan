# `muda` pulls GTK into Linux builds (and the app doesn't use it there)

Found while trying to build in a Linux container: `cargo build` failed until
`libgtk-3-dev` was installed. The app is egui; nothing in it uses GTK directly.

- `cargo tree -i gtk` -> `gtk -> muda -> jonathan`. `muda` (native menu bar)
  is the only thing that needs GTK.
- `Cargo.toml` declares `muda` twice: unconditionally in `[dependencies]`
  (line 16) and again under `[target.'cfg(target_os = "macos")'.dependencies]`.
  The unconditional one is what drags GTK (and a pointless `muda` build) onto
  Linux and Windows.
- Only macOS uses it: `main.rs` calls `menu.init_for_nsapp()`, and the
  menu-event handling in `app.rs` is behind `#[cfg(target_os = "macos")]`.
  `src/menu.rs` and `use muda::MenuEvent` compile on every platform today.

## Possible fix (not done -- recorded only)

1. Delete the unconditional `muda` line from `[dependencies]`; keep the macOS one.
2. Gate the `muda`-using parts of `src/menu.rs` and the `MenuEvent` import in
   `app.rs` with `#[cfg(target_os = "macos")]`. Keep the "Copy as CSV" request
   flag (`request_copy_as_csv` / `take_copy_as_csv_request`) unconditional --
   `new_table.rs` reads it on every platform.
3. Verify `cargo build` and `cargo test --workspace` on Linux without GTK.

Side effect to decide on: "Copy as CSV" (Cmd+Shift+C) only exists as a macOS
menu item. Dropping `muda` elsewhere makes that permanent unless an egui
shortcut (e.g. Ctrl+Shift+C) is added.

## To build on Linux today

`apt-get install libgtk-3-dev` (plus `libxdo-dev` etc. if a build script asks).
