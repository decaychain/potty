<div align="center">
  <img src="assets/icon.png" alt="potty" width="128">
  <h1>potty</h1>
  <p><em>A GPU-accelerated terminal emulator in Rust, with a deliberately visual, pointer-driven take on tabs and panes.</em></p>
</div>

A research spike — built for fun and to learn the stack — that grew into something genuinely
usable. Wayland-native (developed on KWin) and ported to Windows. Where most Rust terminals are
keyboard-purist, potty leans on the **mouse**: click to focus, drag dividers to resize, right-click
for the menu.

> Status: a **personal tool**, scoped to the author's own machines (Linux + Windows). Not a
> general-purpose product — but it runs real shells, multiplexes, and behaves well.

## Features

- **GPU per-cell renderer** — custom glyph atlas (cosmic-text + swash) and two instanced `wgpu`
  pipelines (cell backgrounds + foreground glyphs). Per-cell colour, bold (with the ANSI brighten
  convention), reverse video, a block cursor, and **damage tracking** (only changed panes are
  rebuilt; a busy background tab produces zero redraws).
- **Real multiplexing** — one PTY + `alacritty_terminal` grid **per pane**. A binary split tree
  drives tabs and panes; **drag the dividers to resize**. Background tabs keep running.
- **Visual chrome** (`egui`) — a tab bar that hides itself when there's only one tab, a `☰` /
  right-click pane menu (split, close, new tab), and a floating **Font settings** window. The
  chrome is mouse-only by design.
- **Selection & clipboard** — mouse selection (drag, double-click word, triple-click line),
  `Ctrl-C`/`Ctrl-V` with terminal-correct semantics, `Ctrl-Shift-C/V`, `Ctrl/Shift-Insert`,
  primary-selection middle-click paste (Linux), and **OSC 52** (opt-in for reads). Scrollback with
  wheel + `Shift-PageUp/Down/Home/End`.
- **Mouse reporting** — SGR-1006 / X10 forwarded to apps (vim, htop, Zellij…), with `Shift` to
  bypass into local selection.
- **Per-pane titles** — from OSC 0/2; shown in the tab label and propagated to the window title.
- **Configuration** — `potty.toml` (TOML), **hot-reloaded** on save: font family/size, a separate
  chrome font size, a full colour scheme, the shell, and the OSC 52 policy.
- **Keyboard** — layout-resolved text (German/US, no IME needed) with an IME-commit safety net,
  DECCKM-aware cursor / navigation / function keys (so `mc` and ncurses apps work), and AltGr
  handling on Windows.

## Platforms

| | |
|---|---|
| **Linux** | Wayland-native, developed on **KWin**. Clipboard via `smithay-clipboard` (the app's own seat — no XWayland). Config at `~/.config/potty/potty.toml`. |
| **Windows** | MSVC build. PTY via **ConPTY**, clipboard via the Win32 API (`arboard`), default shell `cmd.exe` (override with `shell` in the config). Config at `%APPDATA%\potty\potty.toml`. |

> **Windows 10 limitation:** mouse reporting into console apps over SSH (e.g. Midnight Commander)
> does **not** work. The inbox ConPTY on Windows 10 doesn't pass mouse sequences through to/from
> console clients — the same potty code works fine on Linux, and this is expected to work on
> Windows 11's newer ConPTY. Everything else (keyboard, clipboard, AltGr, tabs/panes/resize) works.

## Build & run

Requires a recent Rust toolchain.

```sh
cargo run --release
```

On **Windows** you'll also need the **MSVC build tools** (Visual Studio Build Tools →
"Desktop development with C++") for the linker and Windows SDK.

A config file is written on first run; edit it and changes apply live.

## Stack

`winit` · `wgpu` · `cosmic-text` + `swash` · `alacritty_terminal` + `vte` · `portable-pty` ·
`egui` · `smithay-clipboard` / `arboard`

## Scope & non-goals

The narrow scope is deliberate — it's what keeps the project tractable: no IME, no broad
multi-compositor support, no exotica (sixel / kitty graphics / ligatures / hyperlinks) until
they're actually missed. Keyboard shortcuts are intentionally omitted in favour of the mouse.

## License

[MIT](LICENSE) — do what you like with it.
