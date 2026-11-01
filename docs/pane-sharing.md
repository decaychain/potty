# Sharing a pane with an agent

> Status: **Phase 1 implemented, experimental.** Tested on Linux (unit tests, and an end-to-end
> test driving the real MCP server against a real share endpoint). Windows and macOS compile paths
> exist but are not yet verified on those systems. Sections marked *(later)* describe intended
> direction, not implemented behavior.

## Why

A pane often holds a session that is expensive to recreate: SSH through several jump hosts, a
second factor in the middle, `sudo -i`, a container shell. When troubleshooting there, asking an
agent for help usually means either copying output back and forth or teaching the agent to reach
the same system on its own, which can be the hardest part of the problem.

Pane sharing lends the *already established* pane to an agent running elsewhere (typically
Claude Code or Codex in another Potty pane). The agent can see the pane, type into it, and read
the output back, while the user watches and can take the pane back at any moment.

## User flow

1. Right-click the pane → **Share with agent**. The share ID is copied to the clipboard right
   away. A new share can type; one menu item covers the usual case, and the badge switches it to
   read-only when wanted.
2. A badge appears at the pane's top-right corner and the pane border turns amber. Hovering the
   badge reveals **Allow typing** / **Make read-only**, **Copy ID**, and **×** to stop sharing.
   The same actions are in the pane's right-click menu.
3. In the agent's pane: "help me with X in Potty pane `psh-…`" (paste the ID).
4. The agent uses the Potty MCP tools with that ID. The badge shows "Agent reading" or
   "Agent typing" while it works.
5. **×**, *Stop sharing* in the menu, or closing the pane ends the share immediately. The ID is
   then worthless; sharing again issues a new one.

## Approval model

Approval is **per share, enforced by Potty**, not per keystroke by the agent tool:

- Agent tools can't vary their permission prompts per Potty share, and confirming every send makes
  the feature too tedious to use. The installer therefore allows the Potty MCP tools in Claude
  Code (see *Setup*), and Potty decides what each request may do.
- Each share has a mode: **read-only** (snapshot, read, wait) or **can type** (also send). The
  mode can be changed at any time from the badge.
- *(later)* A **confirm** mode: Potty shows the pending input in the badge with
  *Allow / Always for this share / Deny*.

Pane content is shared as-is, secrets on screen included, and the agent can read the pane's
existing scrollback, not only output produced after sharing.

## Security

- The share ID is a capability: `psh-<instance>-<secret>`, where `<secret>` is 128 random bits.
  Requests without a live ID are refused. The ID dies with the share, the pane, or the window.
- The endpoint is local to the user: a Unix socket in a `0700` runtime directory on Linux and
  macOS, a named pipe on Windows (the default pipe DACL denies write access to other users, and
  every request also needs the secret).
- Terminal output is **untrusted input** to the agent. A log line can say "ignore previous
  instructions". The MCP server instructions tell the agent to treat pane text as data.

## Architecture

```
agent (Claude Code / Codex)
  │  MCP over stdio — spawned by the agent, outside its shell sandbox
  ▼
potty-notify --mcp
  │  one JSON request/response per connection
  ▼
share endpoint of the Potty instance named in the ID   (src/share.rs: wire + client)
  │  reads: lock the pane's grid directly               (src/share_host.rs: server)
  │  writes: UserEvent::ShareInput → main loop → write_pty
  ▼
pane (local PTY, ConPTY, or potty-session remote pane)
```

- **Why MCP**: an agent's shell sandbox blocks Unix sockets (the reason the overview briefings
  need a file relay). An MCP stdio server is started by the agent itself, outside that sandbox,
  and its calls are typed, synchronous tools, which fits "send, then wait for output".
- **Why a separate endpoint**: the notify socket is fire-and-forget and Unix-only. Sharing needs
  request/response on Linux, macOS and Windows. The endpoint uses the `interprocess` crate: Unix
  sockets and Windows named pipes behind one API.
- **Routing**: the ID embeds the owning instance, so the MCP server connects to exactly one
  endpoint without enumerating instances (named pipes can't be listed like a directory).
- **One code path for every pane kind**: the GUI keeps a grid for local and remote panes alike,
  and writes go through the same `write_pty` as the keyboard.

## Agent tools (MCP)

| Tool | Mode | Purpose |
|---|---|---|
| `pane_snapshot` | any | Visible screen as text, cursor position, alternate-screen flag, size. |
| `pane_read` | any | Output since a cursor (logical lines, wrapped rows joined). |
| `pane_send` | can type | Literal text plus named keys (`Enter`, `C-c`, `Up`, `F5`, …). |
| `pane_run` | can type | Run one command line at a shell prompt; returns its output and exit status. |
| `pane_wait` | any | Block until a pattern appears, output goes quiet, or a timeout. |

### Read cursors

`pane_read` and `pane_wait` return a cursor; pass it as `since` to get only newer output. A
cursor names a buffer row plus a fingerprint of the rows just above it. When the scrollback is
full, old rows fall off and row numbers shift; the fingerprint relocates the cursor. If it can't
be found (screen cleared, history trimmed past it, pane reflowed after a resize), the response
says `gap: yes` and returns the newest output instead. The last line (the cursor's row, often the
prompt) can repeat on the next read because it may still be changing.

### Detecting the end of a command

There is no end-of-stream marker on a terminal. `pane_run` solves it for shell prompts in one of
two ways.

**Shell integration (OSC 133).** When the pane's shell marks its prompts (README, *Shell
integration*), `pane_run` types the command unchanged and waits for the shell's command-end mark
`D`, which also carries the exit status. The output is exactly the text between the
output-start mark `C` and `D`. Each pane notes the cursor position at every mark while its bytes
are parsed, so the positions are exact. The integration is trusted only at a prompt that follows a
`D`, after the shell has sent a `C` at least once:

- After `ssh`, `sudo -i` or another shell without integration, the last mark is that command's
  `C`, with no `D`: the marker below is used. A remote host gets the marker-free path only if the
  remote shell has the snippet.
- A shell that inherited a marked `PS1` (`sh` started from an integrated bash, `sudo -E`) prints
  `A`/`B` but never `C`/`D`. Its prompt follows a `C`, not a `D`, so it isn't trusted either.

**End marker.** Otherwise `pane_run` types the command with an end marker appended and waits for
the marker's output,
`__PTY_<nonce>_<status>__`. The nonce is random per run, and the typed form never contains the
marker string (quotes or carets split it), so the shell's echo of the line can't match. The
output is the text between the echoed line and the marker.

| `shell` | Appended to the command |
|---|---|
| `posix` (default; sh, bash, zsh, dash, ksh) | `; printf '\n__PTY_%s_%s__\n' '<nonce>' "$?"` |
| `fish` | the same with `$status` |
| `powershell` | saves `$?` / `$LASTEXITCODE`, then `Write-Output` of the marker |
| `cmd` | `& call echo __PTY_^<nonce>_%^ERRORLEVEL%__` (`call` reads ERRORLEVEL after the command) |

Limits: one command line (no newlines, no trailing comment, which would swallow the marker). The
marker is visible in the pane and in shell history. A command that waits for input times out;
`pane_run` then says so, and the agent continues with `pane_snapshot`, `pane_send`, `pane_wait`.
POSIX markers and the OSC 133 path are tested against real interactive bash and zsh in a PTY,
including a non-integrated subshell inside an integrated one; the fish, PowerShell and cmd
markers are not yet tested on those shells.

For anything else (REPLs, prompts, TUIs) the agent uses `pane_send` plus `pane_wait` on a pattern
(such as the prompt) or on `quiet_ms`.


## Setup

```sh
potty-notify --install-share claude   # registers the MCP server and allows its tools
potty-notify --install-share codex    # adds [mcp_servers.potty] to config.toml
```

Claude: the installer runs `claude mcp add --scope user potty -- <helper> --mcp` and adds
`mcp__potty` to `permissions.allow` in `settings.json`. Without the `claude` command on `PATH`
it prints the command to run instead. Codex: the installer adds `[mcp_servers.potty]` with a
tool timeout long enough for `pane_wait`; Codex's own approval policy is left unchanged.

## Phases

1. **Foundation** *(implemented)*: cross-platform endpoint, ID, badge with mode / copy / stop,
   MCP server with snapshot / read / send / wait, installer.
2. **Comfort**: `pane_run` with markers and OSC 133 *(implemented)*; *(later)* input log in the
   badge, confirm mode. Dropped: "Share with → [agent session]" from the overview. The request
   has to be typed into the agent anyway, and switching panes costs nothing.
3. **Unification** *(later)*: move the notify listener onto the same cross-platform transport, so
   the session overview also works on Windows.
