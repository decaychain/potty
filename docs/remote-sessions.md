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
- **potty-session is opt-in.** The connect dialog has a "Use potty-session (persistent multiplexing)"
  checkbox, **off by default** — most hosts don't run `potty-session`. Off → a plain SSH shell
  (`remote::shell_session`): a client-side coordinator speaks the same wire protocol to the GUI but
  backs each pane with its own SSH channel (PTY + shell) on the shared session, so panes/splits/tabs
  work unchanged — just no persistence (closing potty drops the shells). On → exec `potty-session`
  (the daemon path, with detach/reattach/layout). "Detach session" only shows for the latter.
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

## Wire protocol (v2)

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
| `Welcome { version, client }` | S→C | ack; `client` = this client's daemon-assigned id (v2) |
| `Focus { owner }` | S→C | which client id drives layout/sizes now; 0 = nobody (v2) |
| `Open { pane, cols, rows, cwd_from? }` | C→S | start a shell pane of this size, optionally inheriting cwd from another daemon pane |
| `Opened { pane }` | S→C | pane is live |
| `Resize { pane, cols, rows }` | C→S from the owner; S→C mirrors it to the other clients (v2) |
| `Close { pane }` | C→S | kill a pane's shell |
| `Exited { pane }` | S→C | a pane's shell exited |
| `Restore { pane }` | S→C | adopt an existing pane (attach burst, or another client opened it) |
| `LayoutTree { json }` | C→S from the owner; S→C on attach and mirrored live (v2) |
| `Notify { json }` | S→C | attention-feed note passthrough |
| `Ready` | S→C | end of the attach restore burst |

Pane ids are assigned by the client (`next_remote_id` is seeded past restored ids). Terminal I/O
is `Data` frames.

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
   - *Step 3b-ii (done):* the connect flow. Right-click / ☰ → "Connect to host…" opens a dialog
     (`[user@]host[:port]`); connecting uses the agent + `~/.ssh/id_*` + `~/.ssh/known_hosts` and
     the `remote_command` config (default `potty-session`). The text-prompt dialog (passphrase,
     keyboard-interactive, password) reuses the same bridge; keyboard routes to egui while a dialog
     field is open. Remote tabs are host-badged. The `MaxAuthTries` exhaustion now stops the ladder
     and reports a clear, actionable error instead of "Channel send error". Verified visually:
     connect dialog and host-badged tab. *Still env-driven `maybe_test_connect`/`POTTY_TEST_*` kept
     as a scripting/testing aid; `potty attach`, auto-reattach, and bootstrapping remain later.*
   - *Step 3c (done):* multi-pane-per-connection. Splitting a remote pane, or "New tab" from one,
     creates another shell on the *same* connection (clones its `outbound`) rather than a local
     PTY. `potty-session` already multiplexed many panes; this is the client side. Verified
     visually: a split remote tab showing two remote shells side by side.
   - *Known gap:* a busy ssh-agent can exhaust the server's `MaxAuthTries` before the ladder reaches
     a working method (seen as "Channel send error"). The ladder should cap/triage agent identities
     or handle the disconnect.
4. **Persistence.**
   - *Step 4a (done):* daemonization. `potty-session` is now two roles — a short-lived **attach**
     relay (what ssh execs: a byte pipe between the SSH channel and the daemon's Unix socket) and a
     detached **daemon** (`--daemon`, own process group) that owns the PTYs and survives client
     disconnects. The attach starts the daemon if absent and exits on disconnect, leaving it; the
     daemon idle-exits when it has no panes and no client. `POTTY_SESSION_NODAEMON=1` keeps the
     old inline mode for the transport tests. Verified: a shell's process survives the client
     disconnecting (`tests/remote_persist.rs`).
   - *Step 4b (done):* reattach — on a new client's Hello the daemon replays `Restore{pane}` + the
     buffered screen + `Ready`; the client adopts each pane, keyed by `(ConnId, remote_id)`.
   - *Step 4b-fix (done):* connection teardown. Closing the last remote pane now actually closes the
     SSH connection. `connect_and_exec` hands the sole outbound `Sender` to the UI; when it and the
     per-pane clones all drop (last pane gone), the writer flushes any queued `Close` then signals
     channel **EOF**, so the remote relay exits and the daemon — now with no panes and no client —
     idle-exits. Previously the connection thread blocked forever, leaving the daemon attached: it
     never exited *and*, being single-client, it blocked the next connect (which "did nothing").
     Tested: the daemon idle-exits given the `Close`-then-EOF the client now emits
     (`tests/remote_persist.rs::daemon_exits_after_last_pane_closed`).
   - *Step 4c (done):* layout persistence. The client serializes its tab/pane tree (a `proto::Layout`
     with daemon pane ids at the leaves) and pushes it to the daemon (`LayoutTree`) whenever it
     changes, after the handshake (`ready` gate avoids clobbering mid-restore). The daemon stores it
     opaquely and replays it before `Ready`; the client rebuilds the original tabs/splits instead of
     one-tab-per-pane. A pane that died while detached collapses to its surviving split sibling;
     panes the layout doesn't cover fall back to their own tab. Verified: the pushed split layout
     round-trips verbatim through disconnect/reattach (`tests/remote_persist.rs`).
5. **Multi-client attach (done).** Any number of potty instances can attach to the same daemon at
   once (protocol v2) — attaching no longer evicts the previous client. The daemon keeps a client
   list, broadcasts pane output to every client that has been told about the pane (`Restore`/
   `Opened` announcements always precede a pane's `Data` on a given client's stream, carrying the
   replay buffer and the pane's current size), and mirrors layout changes live.
   - **Focus follows input.** Exactly one client (the *focus owner*) drives the layout and pane
     sizes at a time; there is no permission system — any client that types (`Data`), opens, or
     closes a pane simply becomes the owner (`Focus{owner}` broadcast). `Resize`/`LayoutTree`
     from non-owners are dropped *without* flipping focus: they're machine-generated echoes, and
     honoring them would start a resize war between differently-sized windows. When the owner
     detaches, `owner = 0` until the next input claims it.
   - **Followers mirror.** A non-owner client stops fitting remote panes to its own window
     (`fit_terminal` is suspended for that connection) and stops pushing layouts; instead it
     conforms its grids to the daemon's `Resize` broadcasts (the pane rect scissors any overflow)
     and rebuilds its tabs in place from mirrored `LayoutTree` pushes — adopting live-announced
     panes and locally dropping panes the owner closed (never sending `Close` back, which would
     kill shells and steal focus). Typing into the session is all it takes to become the owner
     again; the next redraw then re-fits every pane to the local window.
   - **Compat.** A v2 client against a v1 daemon degrades to the old single-client behavior:
     `Welcome` carries no client id (serde-defaults to 0), no `Focus` ever arrives, and
     `client_id == focus_owner == 0` means "we're the controller". Plain-SSH shells fabricate
     `Welcome{client:1}` + `Focus{owner:1}` for the same reason. A v1 *client* against a v2
     daemon, however, fails on the unknown `Focus` variant — upgrade potty and potty-tools
     together.
   - Verified in `tests/remote_persist.rs`: a second attach joins without evicting the first
     (output broadcast to both); focus flips on input and gates layout/resize (a follower's push
     is dropped, the owner's is mirrored); a pane opened by one client is announced to the other,
     which then sees its output.
   - *Known gap:* pane ids are still client-assigned (each client seeds `next_remote_id` past
     every id it has seen), so two clients opening panes *simultaneously* can collide — the daemon
     ignores the second `Open` and that client's pane stays dead. Unlikely with one human driving
     focus-follows-input; the fix, if it ever bites, is daemon-assigned ids or a per-client id
     namespace.
6. **Later:** auto-reattach, bootstrapping, ProxyJump, agent forwarding, Windows `potty attach` IPC.
