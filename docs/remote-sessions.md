# Remote sessions — potty as its own multiplexer

> Design doc. Status: **in progress** (spike). Supersedes the SSH-socket-forwarding approach in
> [attention-feed.md](attention-feed.md) for the *remote* case: once potty owns the remote session,
> the attention feed is just "a potty pane wants attention" — no forwarded sockets, no ssh wrappers.

## Goal

Make potty multiplex remote sessions itself, replacing Zellij-over-SSH. A remote host appears as
ordinary potty tabs/panes, backed by a small `potty-session` daemon on the host. Programs keep
running across disconnects; reconnecting re-attaches to the same session.

## Decisions (settled)

- **Transport: `russh`** (pure-Rust SSH), not spawning the system `ssh`. potty owns the SSH stack,
  so auth, host-key, and prompting are native dialogs that work *identically on Windows and Linux*
  — sidestepping `ssh.exe`'s missing `SSH_ASKPASS`/`ControlMaster`. We lose `~/.ssh/config`
  niceties (we parse the subset we need ourselves; `ProxyJump`/multi-hop later).
- **Auth: agent-first, controlled fallback.** Order: ssh-agent → explicit key files (passphrase
  dialog if encrypted) → keyboard-interactive / password (dialog). potty drives the method order,
  so a locked/empty agent or expired key *falls through to a dialog* instead of hanging. Agent
  transport is `$SSH_AUTH_SOCK` (Unix) or the `\\.\pipe\openssh-ssh-agent` named pipe (Windows) —
  the agent protocol is identical; only the stream differs. **Agent forwarding: deferred.**
- **Scope:** Windows is a **client** only. Windows *remotes* are out of scope — plain `ssh` to
  them, no persistence. So `potty-session` targets Unix (standard double-fork daemon, Unix-socket
  client↔daemon IPC). The Windows client just speaks the protocol over the russh channel.
- **UI: flatten.** Remote tabs/panes are first-class entries in potty's single tab bar, badged by
  host (iTerm2-with-tmux model), not a nested sub-tab-bar.
- **Entry points:** the `+` tab button / right-click menu ("Connect to host…"), and a
  `potty attach <host>` convenience that signals the running potty over local IPC. Plain `ssh`
  stays as-is for Windows/throwaway logins.
- **Deferred:** auto-reattach on launch; bootstrapping `potty-session` onto a remote (scp-on-first-
  connect); `ProxyJump`; agent forwarding; the local-IPC `potty attach` verb on Windows (named
  pipe). The visual connect path needs none of these.

## Architecture

```
 potty (GUI client)                          remote host
 ┌───────────────────────┐   russh exec     ┌──────────────────────────┐
 │ renderer (wgpu/egui)  │  ────────────►   │ potty-session            │
 │ workspace (tabs/panes)│   one SSH chan   │  owns PTYs, multiplexes   │
 │ russh client + auth   │  ◄────────────   │  them over the channel    │
 └───────────────────────┘   wire protocol  └──────────────────────────┘
```

- potty connects to the host's **sshd** via russh, authenticates, opens one channel, and **execs
  `potty-session`** on it (no PTY on this channel — it must stay 8-bit clean for the protocol).
- `potty-session` allocates the real PTYs for the shells and **multiplexes** their byte streams
  over that single channel using the wire protocol below.
- Locally, a remote pane is fed bytes exactly like a local pane: the renderer doesn't care whether
  a grid's bytes come from a local PTY reader thread or a `Data` frame off the channel.

**Code factoring:** the protocol lives in the lib crate (`potty::proto`), shared by the `potty`
client and the `potty-session` server — same pattern as `potty::notify`. Over time the PTY/grid
backend factors into `potty-core` so `potty-session` is "potty's backend, headless".

## Wire protocol (v1)

One byte stream, length-prefixed frames. Two frame kinds so terminal bytes avoid encoding
overhead while control stays small and debuggable:

```
frame  = [u32 len big-endian][payload]          // len = payload byte count
payload= [u8 tag][...]
  tag 1 = Control : JSON of the Control enum (below)
  tag 2 = Data    : [u64 pane little-endian][raw bytes]   // server→client = output, client→server = input
```

`Control` (JSON, internally tagged on `"t"`):

| message | dir | meaning |
|---|---|---|
| `Hello { version }` | C→S | first frame; negotiate version |
| `Welcome { version }` | S→C | ack |
| `Open { pane, cols, rows }` | C→S | start a shell pane of this size |
| `Opened { pane }` | S→C | pane is live |
| `Resize { pane, cols, rows }` | C→S | resize a pane |
| `Close { pane }` | C→S | kill a pane's shell |
| `Exited { pane }` | S→C | a pane's shell exited |

Pane ids are assigned by the client. Terminal I/O is `Data` frames. This is the **spike** surface;
persistence (reattach repaint), the tab/pane *tree* (vs. flat panes), titles, bell, and the
attention-feed passthrough are added once the spike proves the round trip and feel.

## Lifecycle (target, beyond the spike)

- **Attach:** `russh exec potty-session attach`. First time → daemon starts with one shell.
  Reconnect → daemon already running (one per user per host), replays each pane's current screen so
  the fresh client repaints. The daemon holds the authoritative tree + grids.
- **Detach:** client disconnects (channel EOF) → daemon keeps PTYs alive.
- The client↔daemon split on the remote (short-lived exec process ↔ persistent daemon) is a
  remote-local Unix socket — never forwarded, so no Windows-cross-machine concern.

## Phasing

1. **Spike (now):** `potty::proto` + `potty-session` owning PTYs over stdin/stdout, multiplexed,
   tested with a pipe harness. No SSH, no persistence, no GUI yet.
2. **russh client:** connect + auth + exec, pump the channel.
   - *Step 1 (done):* connect, **publickey** auth, exec `potty-session`, bidirectional frame pump
     (`src/remote.rs`). Host key accepted blindly. Tested over a throwaway localhost sshd
     (`tests/remote_ssh.rs`): a shell command run on the "remote" round-trips its output as frames.
   - *Step 2 (done):* the auth ladder — agent → key files (passphrase) → keyboard-interactive →
     password — plus host-key verification against known_hosts. Interactive bits go through the
     `Authenticator` trait (the GUI implements it with dialogs; step 3 bridges the sync calls to
     the UI thread). Tested: publickey round trip, host-key rejection aborts connect, and
     **ssh-agent** auth (agent started + key added in-test). Windows agent uses Pageant/named-pipe
     (compiled, not yet E2E-tested); keyboard-interactive/password are wired but need PAM/root to
     test, so they're not covered E2E.
3. **GUI wiring.**
   - *Step 3a (done):* the async bridge + remote panes. A tokio runtime on its own threads hosts
     the russh client; frames cross back to the winit loop as `UserEvent`s. `Terminal` gained a
     `Backend` (Local PTY vs Remote), so input/resize route to a `Data`/`Resize` frame and a remote
     pane's bytes feed its own vte `Processor`. A remote session opens as a new tab (flatten model).
     Verified visually: potty auto-connected to a throwaway sshd, exec'd `potty-session`, and a
     remote `echo` round-tripped and rendered in a remote tab. *Driven by `$POTTY_TEST_*` spike
     scaffolding (`maybe_test_connect`/`SpikeAuth`) — temporary, replaced by the connect flow below.*
   - *Step 3b-i (done):* the auth-dialog bridge. Each connection runs on its own thread (a
     current-thread runtime), so a prompt can block *that* connection while the UI keeps rendering.
     `GuiAuth` implements `Authenticator` by sending an `AuthPrompt` over the event loop and
     blocking on a reply channel; the UI shows the dialog and the answer unblocks the thread. The
     host-key approval dialog (host + fingerprint + Unknown/Changed) is wired and verified visually.
   - *Step 3b-ii (next):* the connect flow — `+`/right-click "Connect to host…", `potty attach`,
     host badges on tabs; the text-prompt dialog (passphrase, keyboard-interactive, password)
     reusing the bridge; and removing the `$POTTY_TEST_*` scaffolding. Also the `MaxAuthTries` fix.
   - *Known gap:* a busy ssh-agent can exhaust the server's `MaxAuthTries` before the ladder reaches
     a working method (seen as "Channel send error"). The ladder should cap/triage agent identities
     or handle the disconnect.
4. **Persistence:** daemonize + reattach-repaint; the pane/tab tree server-side.
5. **Later:** auto-reattach, bootstrapping, ProxyJump, agent forwarding, Windows `potty attach` IPC.
