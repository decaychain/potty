# potty

A GPU-accelerated terminal emulator in Rust — a research spike, built for fun and to
learn the stack. Wayland-native (developed on KWin), with a deliberately **visual,
pointer-driven** approach to tabs and panes rather than the keyboard-purist style of
most Rust terminals.

> Status: **prototype**. It runs a real shell, renders fast, and the chrome works —
> but it is not (yet) a daily driver. See [Roadmap](#roadmap).

## What works

- **GPU per-cell renderer** — a custom glyph atlas (cosmic-text + swash) and two instanced
  `wgpu` pipelines (cell backgrounds + foreground glyphs). Per-cell color, bold (with the
  ANSI brighten convention), reverse video, and a block cursor.
- **Real terminal core** — PTY via `portable-pty` (Unix today, ConPTY-ready for Windows),
  VT parsing and grid model via `alacritty_terminal`.
- **Visual chrome** — an `egui` overlay: tab bar (`+` / `✕`), a `☰` pane menu and per-pane
  right-click menu for splitting/closing, and an `Aa` menu to pick **font family & size**.
- **Configuration** — `~/.config/potty/potty.toml` for font and a full color scheme,
  **hot-reloaded** on save. Font family/size are also editable from the UI (and written back).
- **Keyboard** — layout-resolved text (German/US, no IME needed), an IME-commit safety net,
  Ctrl-letter, and DECCKM-aware cursor / navigation / function keys (so `mc` and friends work).

## Stack

`winit` · `wgpu` · `cosmic-text` + `swash` · `alacritty_terminal` · `portable-pty` · `egui`

## Build & run

```sh
cargo run
```

A config file is written to `~/.config/potty/potty.toml` on first run; edit it and changes
apply live.

## Roadmap

The risky parts (VT emulation, the GPU grid renderer) are done. What's left is laborious,
not uncertain:

- **One PTY + terminal per pane** — turn the placeholder panes into real shells (the menu
  already addresses panes by id, so split/close/focus stay as-is). This is the big one.
- Scrollback & mouse reporting
- Italic faces, color emoji, ligatures
- Wayland clipboard (copy/paste, primary selection)
- Damage tracking (only rebuild changed cells) for flood performance
- Windows pass (ConPTY + AltGr handling)

## License

[MIT](LICENSE) — do what you like with it.
