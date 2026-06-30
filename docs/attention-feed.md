# Attention feed — surfacing sessions that need you

> Design doc. Status: **proposed**. No code yet — this locks the contract before we build.

## The problem

Agentic CLIs (Claude Code, Codex, aider, …) spend a lot of their time *blocked on you*:
waiting for a permission grant, a plan approval, an answer to a question. When you run several
at once — across panes, tabs, and remote hosts — you lose track of which ones are waiting. You
end up babysitting, tabbing around to check "is this one done yet?".

The goal: **potty shows a single live list of every session currently waiting for you**, no
matter where it runs — a local pane, a background tab, or a Claude Code session running over SSH
inside a Zellij tab that isn't even on screen.

That last case is the whole challenge, and it's what kills the obvious approaches.

## Why watching the byte stream doesn't work

There are two channels between a program and its terminal:

1. **In-band** — the bytes the program prints. OSC notification escapes (OSC 9, OSC 777, the
   newer desktop-notification drafts) ride this channel.
2. **Out-of-band** — anything that doesn't pass through the terminal at all.

Every "detect the prompt" idea lives in-band, and in-band fails structurally on the hard case:

- **A muxer doesn't forward background panes.** Zellij (like tmux) only streams the *active*
  pane's rendered output to the outer terminal. A Claude Code session sitting on a permission
  prompt in a background Zellij tab emits **zero bytes** to potty until you switch to it. There
  is nothing to pattern-match.
- **OSC escapes don't survive muxers reliably.** tmux needs `allow-passthrough` plus DCS
  wrapping; Zellij's passthrough of arbitrary OSC from background panes is unreliable. Even when
  forwarded, an inactive pane's output may be buffered until you focus it.
- **SSH makes the content remote and opaque.** potty sees an encrypted byte pipe; the program is
  on another machine.

So the signal cannot come from the stream. It has to come from the **tool itself**, over a
**side channel** that bypasses the terminal entirely.

## Architecture

```
 Claude Code / Codex hook  ──►  potty-notify  ──►  Unix-domain socket  ──►  potty UI
        (the signal)          (adds identity)        (the transport)       (the list)
```

Four pieces, each doing one job.

### 1. The signal — tool hooks (not stream-scraping)

We let the tool tell us, using its own first-class notification hooks:

- **Claude Code** — the `Notification` hook fires precisely when Claude needs attention
  (permission prompt, idle waiting for input). It passes the helper a JSON object on **stdin**
  with `session_id`, `cwd`, `message`, `transcript_path`, `hook_event_name`. A
  `UserPromptSubmit` hook (fires when you submit a prompt) is the natural **clear** signal.
- **Codex** — the `notify` program in `~/.codex/config.toml` is still useful for
  turn-complete events, but approval prompts come through Codex lifecycle hooks:
  `PermissionRequest` raises attention and `UserPromptSubmit` clears it. `notify` passes JSON as
  an **argv** argument; hooks pass JSON on **stdin**.

No regexes, no guessing — the tool already knows it's blocked and is willing to say so.

### 2. The helper — `potty-notify` (new bin target)

A tiny Rust binary, a second `[[bin]]` in this crate (shares `serde`, cross-compiles to remote
hosts with one toolchain). Its job:

1. Read the event — stdin JSON for hooks, or `argv[1]` JSON for Codex `notify`; a `--tool` flag
   disambiguates.
2. Augment it with identity it can read from **its own environment**:
   - `hostname`
   - `cwd`, `pid`
   - `$POTTY_PANE` — set by potty for local children (exact correlation, see §4)
   - Zellij context, which Zellij *does* expose as env vars even for background panes:
     `ZELLIJ_SESSION_NAME`, `ZELLIJ_PANE_ID`
3. Normalise to the wire schema (§3) and write one JSON line to the socket at `$POTTY_NOTIFY`.
4. Exit fast and never block the tool. If `$POTTY_NOTIFY` is unset or the connect fails, exit 0
   silently — a session outside potty must behave exactly as it does today.

### 3. The transport — a Unix-domain socket potty listens on

potty owns a listener socket. The helper connects, writes one line of JSON, disconnects. On the
potty side a background thread accepts connections and forwards each event into the winit loop as
a new `UserEvent` — exactly how the PTY reader and child-exit threads already feed the loop
(`src/main.rs`: `proxy.send_event(UserEvent::Wake(id))`). The UI never does blocking I/O.

**Wire schema** (one JSON object per connection, newline-terminated):

```jsonc
{
  "v": 1,                       // schema version
  "tool": "claude" | "codex",   // source
  "event": "raise" | "clear",   // raise = now waiting; clear = no longer waiting
  "session": "abc123",          // tool session id — the dedup/identity key
  "message": "Allow edit to src/main.rs?",
  "cwd": "/home/me/project",
  "host": "devbox",             // hostname where the tool runs
  "pid": 48213,
  "potty_pane": 7,              // $POTTY_PANE if present (local only); else null
  "zellij": { "session": "work", "pane": "3" },  // present iff inside Zellij
  "ts": 1718900000             // unix seconds, stamped by the helper
}
```

**Local sessions.** potty injects two env vars into every child shell it spawns
(`spawn_terminal`, alongside the existing `cmd.env("TERM", …)` at `src/main.rs:812`):

```
POTTY_NOTIFY = /run/user/<uid>/potty/notify.sock   # the listener
POTTY_PANE   = <PaneId>                              # which pane this shell lives in
```

Because `POTTY_PANE` rides the local process tree, a local Claude Code's helper reports its exact
pane. **Local correlation is exact** — "click to focus" lands on the right pane every time.

**Built-in `potty-session` SSH (native path).** The remote daemon binds its own Unix socket and
injects that path as `$POTTY_NOTIFY` into every remote pane, alongside `$POTTY_PANE` for the daemon
pane id. A remote `potty-notify` connects to that socket; the daemon stores the note and forwards it
to the attached client as a `Notify` control frame over the existing potty protocol. No SSH
`RemoteForward`, `SendEnv`, or remote `sshd_config` changes are needed. Pending notes raised while
the client is detached are replayed on the next attach, so a long-running remote tool can still
surface after reconnect.

The crucial property is unchanged: **this path never touches the terminal byte stream.** The remote
Claude Code/Codex hook -> `potty-notify` -> remote `potty-session` socket -> potty protocol -> UI.
**Zellij is bypassed entirely**; it is irrelevant that the tab is inactive or that Zellij muxes the
output, because the notification was never in the stream Zellij controls.

**Plain/manual SSH fallback.** If you run a normal `ssh` inside a local potty shell rather than
using built-in `potty-session`, the helper on the remote still cannot see the local socket. That
path still uses SSH remote socket forwarding. `potty-notify --print-ssh-config <host>` emits:

```sshconfig
# ~/.ssh/config
Host devbox
    RemoteForward /tmp/potty-notify.sock /run/user/1000/potty/notify.sock
    SetEnv POTTY_NOTIFY=/tmp/potty-notify.sock
    SendEnv POTTY_PANE
```

```sshdconfig
# remote /etc/ssh/sshd_config (or a drop-in)
AcceptEnv POTTY_NOTIFY POTTY_PANE
StreamLocalBindUnlink yes
```

The `SendEnv POTTY_PANE` line is the **correlation trick**: potty injects `$POTTY_PANE` into every
pane's shell, `ssh` inherits it, and SSH carries it to the remote — so a note raised on the remote
reports the *local* pane that ran `ssh`, and click-to-jump lands you on that SSH pane. Without
`AcceptEnv POTTY_PANE` the remote note still appears in the feed (host, cwd, Zellij), just not
jumpable. (`StreamLocalBindUnlink yes` lets a reconnect reclaim a stale remote socket. If you can't
touch sshd_config, drop `SetEnv` and `export POTTY_NOTIFY` from the remote shell rc instead.)

Verified end-to-end through a throwaway localhost sshd: a note from the "remote" helper arrived at
the local listener carrying `pane` (forwarded) **and** `zellij: {session, pane}` (read from the
remote's own env) — i.e. the full SSH-inside-Zellij case.

**Two sessions to one host.** The fixed `/tmp/potty-notify.sock` above is fine for one session per
host at a time, but two concurrent sessions to the same host fight over that single path (the
second bind steals it; a survivor goes mute when the owner disconnects). The fix is a per-pane
remote path, which a static ssh config can't express (it can't template `$POTTY_PANE`) — so
`potty-notify --print-ssh-wrapper` emits a shell `ssh` function that forwards
`/tmp/potty-notify-$POTTY_PANE.sock` instead. Each pane gets its own remote socket, so concurrent
same-host sessions coexist (verified). Outside potty the wrapper is plain `ssh`.

### 4. The UI — the attention feed

State lives in `App` as a registry keyed by `(host, session)`:

```rust
struct Pending {
    tool: Tool,
    message: String,
    cwd: String,
    host: String,
    potty_pane: Option<PaneId>,   // Some → exact local jump
    zellij: Option<ZellijLoc>,    // shown so you know where it is remotely
    since: Instant,
}
```

- A `raise` inserts/updates (re-raise refreshes `since`). A `clear` removes.
- Surfaced three ways, cheapest to richest:
  - **Tab-bar badge** — a count of waiting sessions, and a per-tab dot when a tab owns one.
  - **A global overlay / palette** (e.g. `Alt+`\`) listing every pending session: tool · host ·
    cwd · message · age, newest first.
  - **Select to jump.** If `potty_pane` is known, focus that pane (and its tab). For remote
    sessions we focus the potty pane holding the SSH connection and *display* the Zellij
    coordinates so you can finish the last hop yourself.

## Lifecycle / clearing

A pending entry clears on the first of (all implemented):

1. An explicit `clear` event (Claude `UserPromptSubmit`; Codex next-turn-start).
2. The owning **local** pane receiving a keypress, or being jumped to from the feed — you've
   engaged with it, so it no longer needs flagging. Handled entirely inside potty.
3. A manual dismiss — the `×` on a feed row. (No time-based TTL: a prompt can legitimately wait
   indefinitely, so dismiss is explicit rather than automatic.)

When the last entry clears the overlay auto-hides; the tab-bar bell stays (the chrome latches on
for the session so content doesn't jump), and clicking it with nothing waiting dismisses the bell.

## The honest limitation: remote correlation

For a remote session we can land you on the **potty pane that holds the SSH connection**, but we
cannot reach *inside* Zellij to switch Zellij's own tab — that pane's multiplexer state isn't ours
to drive. v1 shows "zellij session `work`, pane 3" (read from env by the helper) so you know the
last hop. Auto-switching the remote Zellij tab would mean potty injecting
`zellij action go-to-tab …` back over the wire — a real **Phase 3** stretch goal, not the first
cut. Even without it, "you have 3 sessions waiting; here's each one; click to land on the right
SSH pane" is the bulk of the win.

## Security & robustness

- **Socket perms.** The listener lives under `/run/user/<uid>/potty/` (mode `0700`); the socket is
  user-only. No network exposure locally.
- **SSH forwarding is opt-in and per-host.** Only hosts you add `RemoteForward` for can reach the
  socket, and only over your authenticated SSH session. A remote host cannot reach potty unless you
  forwarded it there.
- **Treat payloads as untrusted display data.** `message`/`cwd` are rendered as plain text only —
  never interpreted, never used to build a shell command. Length-cap fields; one event per
  connection; ignore malformed JSON.
- **Fail open and quiet.** Anything wrong with the socket → the helper exits 0 and the tool is
  unaffected. The feature is strictly additive; a session outside potty behaves exactly as today.

## Setup the user writes once

The hooks can be wired automatically — `potty-notify --install-hook claude` and
`potty-notify --install-hook codex` (idempotent; merges into existing config, never clobbers).
Codex command hooks may need a one-time trust review with `/hooks` before they run.
For reference, what they write:

**Claude Code** (`~/.claude/settings.json`):

```json
{
  "hooks": {
    "Notification":      [{ "hooks": [{ "type": "command", "command": "potty-notify --tool claude" }] }],
    "UserPromptSubmit":  [{ "hooks": [{ "type": "command", "command": "potty-notify --tool claude --clear" }] }]
  }
}
```

**Codex** (`~/.codex/config.toml`):

```toml
notify = ["potty-notify", "--tool", "codex"]

[[hooks.PermissionRequest]]
matcher = "*"

[[hooks.PermissionRequest.hooks]]
type = "command"
command = "potty-notify --tool codex"
timeout = 10

[[hooks.UserPromptSubmit]]

[[hooks.UserPromptSubmit.hooks]]
type = "command"
command = "potty-notify --tool codex --clear"
timeout = 10
```

For built-in `potty-session` SSH: drop the `potty-notify` binary on the remote's `$PATH` and
install the same hooks in the remote `~/.claude` / `~/.codex`; no SSH forwarding is required.
For plain/manual `ssh`, `potty-notify --print-ssh-config <host>` still emits the fallback
`~/.ssh/config` block and remote `sshd_config` lines.

## Phasing

- **Phase 1 — local, fully robust. ✅ shipped.** Listener thread + `UserEvent` variant + per-pane
  env injection + `potty-notify` bin + the attention-feed UI (overlay, bell, jump, dismiss). Exact
  correlation; covers single-machine multi-session use.
- **Phase 2 — built-in SSH. ✅ shipped.** `potty-session` owns a remote-local notify socket,
  injects `$POTTY_NOTIFY`/`$POTTY_PANE` into panes, forwards notes over the protocol, and replays
  pending notes on reattach. The helper reports `host` + Zellij context; remote pane ids are mapped
  back to local panes for jump/clear behavior.
- **Phase 3 — plain/manual SSH fallback.** *Done:* the `ssh` wrapper
  (`potty-notify --print-ssh-wrapper`) injects a
  **per-pane** `RemoteForward`, which both removes the hand-edited ssh config *and* fixes the
  multi-session-per-host collision — each pane forwards `/tmp/potty-notify-$POTTY_PANE.sock`, so two
  live sessions to one host no longer fight over a single path (verified with concurrent sessions).
  *Still open:* OSC fallback for tools without hooks; the Zellij last-hop auto-switch
  (`zellij action go-to-tab` over the wire).

## Open questions

- **Socket path on the local side** — fixed `notify.sock` (simplest; pane comes from
  `$POTTY_PANE`) vs per-pane sockets. Leaning fixed: one listener, pane identity in the payload.
- **Overlay vs reusing the existing menu chrome** for the feed UI — TBD against the egui layer.
</content>
</invoke>
