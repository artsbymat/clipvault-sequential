# Clipvault Sequential

A small sequential clipboard queue for Wayland. Copy several text or image
items, then paste them from newest to oldest with `Ctrl+V` or
`Ctrl+Shift+V`. Each successful paste advances the queue automatically.

## Features

- Text and PNG/JPEG previews
- Color previews for HEX, RGB, RGBA, HSL, HSLA, HSV, and CSS color values
- Sequential paste queue
- Pause and resume clipboard capture
- Drag-and-drop reordering, arrow buttons, and reverse queue controls
- Delete individual items or clear the entire queue
- Session-only storage; nothing is written to a database
- Native Wayland window without compositor-specific code
- Low memory footprint with zero-copy buffer sharing, texture thumbnailing, and aggressive heap trimming

## Requirements

- Linux with a Wayland compositor
- Rust 1.88 or newer
- [`wl-clipboard`](https://github.com/bugaevc/wl-clipboard), providing
  `wl-copy` and `wl-paste`
- Read permission for the keyboard devices under `/dev/input/event*`

The application shows an error at the bottom of the window when it cannot read
an evdev keyboard. Configure access through your distribution's input group or
a suitably restricted udev rule. Direct evdev access can observe keyboard
events system-wide, so do not grant broader device permissions than necessary.

## Build

```sh
cargo build --release --locked
```

The resulting executable is:

```text
target/release/clipvault-sequential
```

Install it into Cargo's binary directory if desired:

```sh
cargo install --path . --locked
```

## Run

```sh
clipvault-sequential
```

The application must remain open while sequential-paste mode is in use. Close
the window to stop clipboard capture and keyboard monitoring.

## Hyprland setup

The following example uses the Lua configuration format introduced in
Hyprland 0.55. It starts the regular [Clipvault](https://github.com/rolv-apneseth/clipvault) history watcher with Hyprland,
opens Clipvault Sequential on `SUPER+ALT+V`, and gives the GUI a compact
floating, pinned, top-of-stack layout.

Add this to your Hyprland Lua configuration:

The startup command assumes the regular [`clipvault`](https://github.com/rolv-apneseth/clipvault) CLI is installed
separately and available in `PATH`.

```lua
-- Keep the regular Clipvault history updated in the background.
hl.on("hyprland.start", function()
    hl.dispatch(hl.dsp.exec_cmd("wl-paste --watch clipvault store"))
end)

-- Open sequential-paste mode only when it is needed.
hl.bind("SUPER + ALT + V", hl.dsp.exec_cmd("clipvault-sequential"))

-- Match the title set before the Wayland window is mapped. Pinning only works
-- for floating windows, so both effects are set in the same rule.
hl.window_rule({
    name = "clipvault-sequential",
    match = {
        initial_title = "^Clipvault Sequential$",
    },
    float = true,
    pin = true,
    size = { 440, 620 },
    center = true,
    persistent_size = true,
})

-- Keep the window above other normal windows after it has been initialized.
hl.on("window.open", function(window)
    if window ~= nil and window.title == "Clipvault Sequential" then
        hl.dispatch(hl.dsp.window.alter_zorder({
            window = window,
            mode = "top",
        }))
    end
end)
```

The rule provides:

- `SUPER + ALT + V`: opens Clipvault Sequential on demand
- `wl-paste --watch clipvault store`: keeps regular clipboard history running
- `float = true`: keeps the application out of the tiled layout
- `size = { 440, 620 }`: opens it as a compact panel
- `pin = true`: displays it across workspaces on the same monitor
- `alter_zorder({ mode = "top" })`: places it above normal windows
- `persistent_size = true`: remembers manual floating-window resizing


## Usage

1. Press `SUPER+ALT+V` to open Clipvault Sequential.
2. Copy several items.
3. Focus the destination application.
4. Press `Ctrl+V` or `Ctrl+Shift+V` repeatedly.
5. Close Clipvault Sequential when the sequence is finished.

The first paste uses the newest queued item. The active item is removed only
after the next clipboard value has been accepted successfully.

## Acknowledgements

- [Clipvault](https://github.com/rolv-apneseth/clipvault)

