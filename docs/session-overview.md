# Agent session overview

> Status: **experimental live prototype for Codex and Claude Code**. The setup and limitations
> below describe implemented behavior; the remaining proposal describes the intended direction.

## Trying live sessions

Build the current binaries, then opt in separately for each agent:

```sh
cargo build --locked --bins
./target/debug/potty-notify --install-overview codex --briefing-relay
./target/debug/potty-notify --install-overview claude
./target/debug/potty
```

The installer uses an absolute path to the helper: install it in its final location before
configuring production hooks. It adds overview hooks alongside existing notification hooks
and installs `skills/potty-briefing/SKILL.md` in the agent's configuration directory. It respects
`CODEX_HOME` and `CLAUDE_CONFIG_DIR`, preserves unrelated settings, and refuses to overwrite a
modified briefing skill. Re-running the installer upgrades an unmodified bundled skill, including
the original prototype version, and keeps a baseline for later upgrades. Nothing is installed
automatically by building or launching Potty.

The Codex command above explicitly enables a data-only file relay through trusted hooks. Without
`--briefing-relay`, publication remains direct and can fail in the agent's shell sandbox. The
installer does not change sandbox, network, or hook-trust permissions.

Start or resume the agent **inside a pane in the new Potty instance**. Restart an already-running
agent after installing hooks; Codex may require approving the hooks in `/hooks`. Open the panel
with the tab-bar Sessions button or menu item. Updates do not open the panel or steal focus.
The tab-bar button hides when no undismissed, non-ended sessions or kept contexts remain, unless
the panel is already open. Sessions stays available in the menu, including the pane's right-click menu,
so an empty panel or session history can still be opened. Waiting agents count as current sessions.
The default **Current** view excludes ended sessions. Enable **Show ended sessions** in the
panel menu to inspect or dismiss history. A new agent session can use the same pane without
being merged with its predecessor; ended history no longer offers navigation to that pane.

### Codex launch mode

Start Codex CLI inside the Potty pane without its shared background server:

```sh
codex --no-daemon
```

Codex 0.163.0 uses a shared app-server daemon by default. In the observed failure, the CLI
inherited the pane's `POTTY_NOTIFY`, `POTTY_PANE`, and `POTTY_INSTANCE`, but the daemon had been
started outside Potty and had none of them. Hooks therefore returned silently without registering
the session or supplying a reporting command; agent shell commands also lacked the variables.
Installing or trusting hooks alone does not repair this environment mismatch.

Use `--no-daemon` for each Codex session, locally or on the remote host. Do not pin a socket or
pane ID in global Codex configuration or restart the shared daemon inside one chosen pane:
different panes and Potty instances need their own routing identity. Existing sessions using the
shared daemon are unaffected by starting a separate non-daemon session.

If registration still fails, check `printenv POTTY_NOTIFY POTTY_PANE` in the launching shell and
review the overview hooks in Codex's `/hooks`. A registered row with no structured briefing is a
separate problem: inspect the explicit publisher's error for missing environment or denied socket
access rather than disabling the sandbox.

### Codex briefing relay

The `--briefing-relay` opt-in adds that flag to Codex's overview hook commands. Re-running the
installer at the same helper path updates the existing handlers instead of adding duplicates.
Review and trust the changed hooks in `/hooks`, then restart Codex with `--no-daemon`. Re-running
`--install-overview codex` without the flag switches those handlers back to direct publication.

SessionStart and UserPromptSubmit supply a reporting command with a session-specific
`--briefing-file` path. The agent runs that exact command with briefing JSON on stdin. The helper
validates and atomically queues the data without connecting to a socket or needing `POTTY_*`
variables in the sandbox. A successful exit means queued, not acknowledged by Potty.

PostToolUse and Stop hooks forward the queued report through the hook's own Potty connection;
SessionEnd and Interrupt also attempt to flush. These hooks print no relay feedback and never
request a continuation. The hook validates the session binding, schema, sizes, and timestamp,
and chooses the socket, pane, and session from its own environment and event. A file cannot
request command execution, supply an arbitrary destination, or change lifecycle status.

Each outbox holds only the latest report. Successful sends remove it; transport failures leave it
for a later hook, without model retries. Invalid reports are discarded. Nonblocking locks prevent
normal concurrent hooks from duplicating delivery. A process crash after sending but before
removal can cause a replay with the same timestamp, which the overview store merges idempotently.
No fallback attempts to open a socket from the sandbox if staging fails.

The relay is Unix-only and works on the remote host too, using the same native SSH notification
path. It needs an updated helper and opted-in remote Codex hooks, not a new daemon protocol.
If a custom sandbox denies writes to its temporary directory, queuing still fails without
requesting broader permissions.

### Overview data

Two independent sources populate a row:

- Hooks register the tool session and report working, approval requested, response ready, ended,
  or unknown. Prompt and final-response excerpts provide a fallback without transcript parsing.
- A SessionStart hook supplies a session-bound reporting command and the briefing instructions
  directly to the agent. UserPromptSubmit also supplies a compact reminder and that command,
  including for agents already running when the helper was updated. The bundled skill asks for
  a maintained task-level briefing: stable goal/ticket in the title, accumulated progress and
  current state in the summary, and the next action, external wait, or user decision. Follow-ups
  such as "push it" should not replace the task identity. Briefings should fit in three short
  lines: title, one sentence of progress, and one next action or wait, aiming for at most 60
  words total. Milestones are optional, preferably zero to three short phrases. The skill and
  prompt/start reminders ask for publication before every final response, including discussion-only
  and unchanged turns. They do not force extra agent turns.

For example: `PTT-128: New Shiny Dashboard Screen` / `Merge request updated with the overview
feature` / `Waiting for CI; review the result before merging`. The agent must know those facts;
the helper does not query JIRA or CI or infer a task from the final response. Rows labelled
**Latest response (no briefing)** or **Latest prompt (no briefing)** have received no structured
report. They remain a fallback, not a synthesized task overview. Check whether the agent invoked
`potty-notify --briefing` and whether direct publication or hook forwarding succeeded if a row
stays in this state.

**Response ready is not task completion.** A briefing never changes hook status. When another
prompt arrives without a new briefing, the old briefing remains visibly marked as older than
the prompt; expanding it also shows the latest prompt excerpt. The agent may omit reports, and
reported claims are not independently verified. Hook errors do not stop the agent, request
permissions, or force an extra model turn.

### Before-response reporting

Both providers receive an explicit instruction to publish one concise briefing before every
final response. This includes text-only discussions and unchanged turns; agents should preserve
the existing goal and state rather than inventing progress. Publication is an expected workflow
step, but failures remain best-effort: no extra permissions, retries, or investigation solely
for a report. Model compliance is not guaranteed, and missing or stale briefings remain possible.

The `Stop` hook records status and the final-response excerpt, and flushes an opted-in outbox.
It never injects feedback,
blocks stopping, or requests another model turn. The earlier Claude continuation experiment was
removed because its visible hook feedback obscured the final answer.

Upgrade/rebuild the helper at the path used by your hooks and re-run
`potty-notify --install-overview claude` (or `codex --briefing-relay`) to refresh the installed skill, then restart
the agent. There is no new hook event or daemon protocol requirement for this change. Old
`*.overview-hooks` metadata directories beside notification sockets are no longer read or written;
they can be removed once no older helpers use them.

### Publishing a briefing

The publisher accepts plain JSON on stdin, for example using the session ID supplied by the hook:

```sh
potty-notify --tool claude --briefing --session 'SESSION-ID' <<'JSON'
{"title":"SSH replay fix","summary":"Reproduced; regression test added, fix unverified.","next":"Implement the boundary fix.","milestones":["Added a failing regression test."]}
JSON
```

Wire limits are UTF-8 bytes: title 120, summary 1200, next 600, and five milestones of 300 each.
These compatibility ceilings are not writing targets; the skill requests the shorter form above.
Reports replace the previous briefing, including milestones. Invalid reports return a nonzero
exit status; automatic hooks remain silent and successful on transport or payload failures.
This first publisher uses Unix sockets, matching the existing notification integration.
For Codex's relay, use the complete command supplied by the hook, including its `--briefing-file`
argument; do not guess an outbox path or session ID.

### Overview interactions

Opening the overview hides the notification popup without clearing its entries or bell count.
New notifications do not auto-open the popup while the overview is visible; the bell still
opens it explicitly. Closing the overview does not replay suppressed popups.

Click a session row to switch to its pane; an already-expanded inactive row stays expanded.
Clicking it again while focused toggles its briefing. The focused pane's
rows are highlighted, and switching panes or reopening the panel reveals its first matching
row. Filters are respected, and ordinary updates do not pull the list away from manual scrolling.
Sessions sharing a resolved pane are adjacent, without assuming a parent/child relationship.
Prompt excerpts, including the expanded stale-briefing prompt, are limited to four wrapped lines.

Claude's [Stop hook](https://code.claude.com/docs/en/hooks#stop-input) can supply a
`background_tasks` snapshot. Potty shows a count of reported running/pending tasks, with a monitor
count when supplied, without replacing the Ready status. This includes background shell jobs;
no commands, descriptions, task output, or transcripts are retained for this indicator.
Hover for the report's age. It is refreshed at turn end, not continuously: a job completing or
being cancelled between turns may remain in the last report until the next Stop event. Missing
or malformed snapshots mean unknown, not zero, and session start/end clears the old snapshot.
Older Claude versions without this field show no indicator. No hook reinstallation is needed,
but update `potty-notify`, the GUI, and (for remote forwarding/replay of this field) `potty-session`.

### Remote sessions and retention

Native persistent SSH sessions require the new **potty-session and potty-notify on the remote**,
plus hooks installed in the remote agent's configuration. No socket forwarding is needed.
The daemon retains records while detached, replays them to attaching clients, and broadcasts
updates and dismissals to all attached clients. Dismissal hides the current revision; a genuinely
new hook or briefing update may surface it again. A locally dismissed disconnected entry is
reconciled with the daemon on reconnect. Pane navigation uses current remote-to-local routes.

Live records and dismissal watermarks are bounded to the 128 most recently updated sessions per
store and live only in memory. Remote persistence lasts for the daemon's lifetime; local
records last for the GUI's lifetime. Retention guarantees end on eviction. A missing pane or
disconnected session remains dismissible and is marked unavailable, with its last known status.
Acknowledging an attention notification does not delete a briefing, and opening a briefing's
pane does not acknowledge notifications.

### Saved contexts: first experiment

Saved contexts are enabled by default. To disable them, set this **top-level** option in
`$XDG_CONFIG_HOME/potty/potty.toml` (normally `~/.config/potty/potty.toml`), before any tables:

```toml
saved_contexts = false
```

The GUI hot-reloads this setting. Disabling it hides the history tabs and annotation controls,
closes the annotation editor, clears cached history, and stops local history reads and writes.
The live overview, agent hooks, and notifications continue working. Existing history files are
not deleted; setting it back to `true` makes them available again. Missing options default to
`true`; unreadable or malformed configuration does not silently enable persistence.

Each remote host controls its own persistence using the same option in its own configuration
file. `potty-session` reads it when the daemon starts; a local client's setting cannot disable
storage for other clients of that host. Apply the remote option before starting the daemon.
Do not stop a daemon with active work just to change the setting. When disabled remotely, live
overview updates still work, but no saved-context store is opened, replayed, or modified.

The overview has three views: **Current**, **Saved**, and **Recent**. Current keeps the live
session list; Saved contains deliberately kept work; Recent lists contexts in activity order,
including kept contexts,
including sessions whose panes have closed. Saved and Recent can be searched by name, notes,
links, host, directory, agent session ID, and briefing summary. Search only covers the local
store and remote stores received during this Potty instance's lifetime.

Expand an item to **Keep** it or **Edit notes**. The editor replaces the annotation display
inline, with a name, notes, and one HTTP(S) link per line. Saving annotations keeps the context.
User text is separate from agent briefings and cannot be overwritten by hook updates. A saved
name also labels its associated live overview rows. Long notes scroll within the expanded item;
collapsed rows remain compact. **Forget...** asks for confirmation before removing the durable
record and its annotations. It does not delete the provider's conversation or stop a process;
a new update from a still-running agent can create a fresh Recent record.

Each pane context retains only its latest agent session, replacing the previous association and
briefing without changing the context's name, notes, or links. Existing multi-session contexts
are reduced to their last session when loaded. Provider session IDs
identify resumed sessions within this host's store; socket paths and pane numbers are only
temporary navigation bindings. Historical records never claim that a process is still running
and never navigate using a pane number recovered from disk.

When a context has a live pane, clicking it focuses that pane. Otherwise **Open shell** explicitly
creates a new tab in its last recorded directory, locally or on an already-connected remote.
Missing directories produce an error rather than silently choosing a different directory.
This does not resume the agent, restore environment variables, or replay commands.
The latest agent session and its working directory appear directly above the expanded item's
action buttons; **Copy ID** and **Copy path** copy their full values, even when truncated on screen.
Expanded current items have one action row: Keep and Edit notes on the left, a dock control,
and Dismiss on the right. Editing notes temporarily replaces these actions with Save/Cancel.
Automatic agent resumption and automatic SSH connection are deliberately deferred.
A reopened shell is tracked in the current client;
once an agent publishes from it, it becomes the context's latest session.

Each host owns one per-user store, `$XDG_DATA_HOME/potty/contexts.json`, falling back to
`~/.local/share/potty/contexts.json`. Local Potty instances share the local file; remote
`potty-session` daemons use the remote user's file and replay it on reconnect, even after a
daemon restart. Remote history is not copied to local disk. Disconnected remote contexts remain
readable in memory, but cannot be edited or reopened until that host reconnects. Update the remote
**potty-session** for this feature; existing overview hooks and `potty-notify` need no changes.
Older daemons keep the live overview but do not offer context editing.
Replacing the binary does not upgrade an already-running daemon. Use a newly started daemon to
try remote contexts; do not stop a daemon that still owns work you need to preserve.

Storage uses a cross-process file lock and atomic replacement, with 0700 directories and 0600
files on Unix. An edit carries an annotation revision; conflicting edits are rejected with the
draft retained. Remote edits only show as saved after the daemon returns its updated record.
Reopening the overview or selecting a view refreshes shared storage; live mirroring of annotations
between separate local windows/remote daemons is not yet implemented. Clients attached to the
same daemon receive changes immediately.

### Session dock

The down control moves an item into a compact **Dock** at the bottom of the overview. Each
entry shows only its title, status, and restore control. Clicking the entry focuses its live
pane without undocking it. The up control restores and expands the full item. The dock stays
visible across Current/Saved/Recent and state filters; it scrolls independently and takes at
most 35% of the space below the panel controls.

Docking is independent of Keep, dismissal, agent state, and pane focus. With saved contexts
enabled, it is stored on the context's owning host, survives reconnects and restarts, and is
broadcast to clients of the same remote daemon. A context with no live agent remains in the
dock as **Saved**, without claiming its old pane still exists. Restoring it opens its details
in Recent; opening a shell remains a separate explicit action.

Persistent docking requires an updated running `potty-session` on remote hosts. Snapshots
advertise support; older daemons and sessions without saved contexts get a window-local dock,
identified by the dock control's tooltip. No unsupported persistence requests are sent.
Remote moves wait for confirmation; errors or a lost connection keep the last confirmed view.
Refreshing reconciles requests whose replies were lost. Disconnected persistent contexts must
reconnect before changing their dock state.
There is no new hook installation requirement.

Retention is bounded to 128 unkept, undocked contexts and 512 kept or docked contexts, with one
session snapshot per context and a 4 MiB file limit. Kept and docked contexts are never automatically evicted.
Limit, corruption, and write errors are shown in the panel.
Routine tool-state changes do not rewrite the file; new prompts, responses, and briefings do.

### Privacy and experiment boundaries

Installing the hooks opts into collecting up to 1600 characters from submitted prompts and
final responses, along with session IDs, host, working directory, and agent-written reports.
These can contain sensitive project information. Live overview records travel through the existing
private socket/SSH path. Context history now stores these excerpts and reports on the owning host's
disk, together with user annotations, until forgotten or evicted as described above. The optional relay also
temporarily stores queued briefing data on disk as described below. Potty does not call a
separate model service, scrape terminals, read transcripts, dump environments, or send external telemetry.
Hooks never request an additional continuation in the existing agent session.
The skill asks agents to exclude secrets, but cannot enforce semantic redaction.

The relay stores files under the hook's temporary directory, normally
`/tmp/potty-briefings-<uid>/<session-binding-hash>/` on Linux. Directories are owner-only (0700),
files owner-only (0600). Binding metadata contains socket path, pane ID, and agent session ID.
Pending/staging files contain the briefing and timestamp, not prompts or transcripts, and are
limited to 48 KiB each. Symlinks, multiply linked files, nonregular files, and unsafe permissions
are rejected. This is not an isolation boundary against other processes running as the same user.

Empty binding directories are retained so overlapping hooks keep locking the same directory.
Failed sends or interrupted writes can leave briefing data on disk. Reports older than 24 hours
are discarded on the next flush; there is no background expiry or global disk quota. Remove old
relay directories only after their sessions have stopped. A system temporary-file cleanup may
also remove them; a later prompt hook recreates a missing outbox.

This is not yet the entire proposal: cross-window mirroring of local overview rows,
explicit task-completion/review tracking, and old-daemon capability
indicators are deferred. Old daemons/clients ignore the new payload and remain usable, but do
not provide live overview support. Subagent lifecycle events with an `agent_id` are ignored;
provider events lacking subagent identity cannot be reliably separated from parent activity.
Nested multiplexers can only navigate to the containing Potty pane. Hook state is a best-effort
last report, not a liveness monitor; missing exit hooks can leave stale state.

Automated tests cover both providers' payloads, helper socket delivery, independent briefing
and status updates, silent Stop handling, validation, installer idempotence, remote detached replay, and multi-client
dismissal. They do not establish whether real models keep briefings useful. For the next trial,
run both agents through a substantive request, a text-only discussion, an unchanged acknowledgement,
a changed goal, a permission wait, and a handoff;
also detach an SSH session mid-task. Check whether the summary and next action help you choose
the right pane without opening it. Count missing, stale, and misleading briefings separately.

Relay tests cover identity separation, invalid data, links and permissions, locking, silent
flushing, and failure retention. An opt-in Linux smoke test runs the installed Codex sandbox
without making a model request, verifies that direct socket publication is denied, then checks
file queuing and hook delivery:

```sh
cargo test --test briefing_relay codex_workspace_sandbox_blocks_direct_socket_but_allows_file_relay -- --ignored
```

Hook contracts: [Codex hooks](https://learn.chatgpt.com/docs/hooks) and
[Claude Code hooks](https://code.claude.com/docs/en/hooks).

## Trying the UI preview

```sh
cargo run --bin potty -- --overview-demo
```

This explicitly enables a separate, in-memory sample dataset and opens the overview. Normal
launches use live records and leave the panel closed. The preview is labelled "Sample data" and does not change live
agent notifications, start agent processes, or connect to the sample hosts.

The preview includes six sessions, status filters, expandable briefings and milestones, dismissal,
and a reset action in the panel's overflow menu. That menu also offers **Docked / Floating** and
**Left / Right**, with docked-right as the default. These preview preferences last for the current
instance, including closing and reopening the panel. The tab-bar overview button and the Sessions
menu item toggle it. Docked mode reserves space and supports resizing; floating mode overlays the
terminal without changing its dimensions. Narrow windows use the central area and suppress input
to the hidden shell in either mode. Escape closes the floating or full-window view. Pane navigation
is disabled for samples. Display preferences are not saved across application restarts.

## Problem and goal

With roughly six agent sessions running across tabs, panes, and remote hosts, it is easy to
forget what each session was doing. Scrolling through its conversation may not recover the
context quickly. Asking the agent for a recap works, but interrupts the workflow just to decide
which session to visit next.

The [attention feed](attention-feed.md) answers **"who needs me?"**. A separate session overview
should answer **"what is going on?"** Each entry should explain the task, its current position,
the next action or decision, and where to find it.

Success test: after an hour away, can the user decide which of six sessions to visit next without
opening all six?

This does not require embedding an agent SDK or replacing an agent's TUI. Agents keep their own
interfaces and publish small briefings for Potty to present together.

## Overview panel

Use a toggleable panel with optional docking and left/right placement. Docked mode can remain open
alongside the terminal; floating mode allows a quick overview without reflowing panes. On narrower windows,
offer a full-window overview rather than squeezing both surfaces into unusable widths. Opening
the panel is explicit; incoming updates must not steal terminal focus.

The default view should make about six sessions readable together on a typical desktop window,
without expanding individual entries. Use compact rows, stable ordering, and clear emphasis for
sessions needing attention. Summary changes and status changes should not reorder the list.

Each row contains:

- A stable, editable task name.
- Agent/tool, host, and project or working directory.
- Last reported work state, with connection availability shown separately.
- A brief account of progress.
- The next action or decision, especially anything required from the user.
- Briefing age and source, so stale or inferred information is identifiable.

Example:

```text
Ubuntu clipboard support                         Waiting for you
Codex | build-server | /work/potty
Implemented X11 backend; checking selection ownership.
Needs you: approve the integration test.
Agent briefing updated 4 minutes ago
```

Selecting a row focuses its owning pane, including another Potty window where supported.
A separate expand control reveals the full briefing and recent milestones without navigating
away. Long text wraps or truncates in the compact row and remains available in the expanded view.
User-assigned names must survive subsequent agent updates.

Keep the notification feed as the lightweight interruption mechanism. Its existing jump action
still leads directly to a pane; the overview provides broader context. Opening the overview or
reading a briefing must not implicitly acknowledge every notification.

## Briefings and milestones

A briefing is a small structured record, not a transcript:

| Field | Purpose |
|---|---|
| Goal | What the session is trying to accomplish |
| Summary | What has happened and what is happening now |
| Next action | The next step or decision; whether it needs the user |
| Source and timestamp | Who supplied the briefing and how old it is |
| Recent milestones | A bounded history of meaningful progress |

Example milestones:

```text
Found Wayland-only assumption
Implemented X11 backend
Tests passed
Awaiting commit approval
```

These are reported events and outcomes, not internal reasoning. Do not create a milestone for
every text change or tool call. Agents should publish milestones deliberately; the UI should
deduplicate repeated deliveries and bound retained history.

## Information sources

### Agent-supplied briefings

A skill instructs the agent to publish a briefing when work begins and at meaningful transitions:
a change of approach, a milestone, a blocker, or completion. Avoid updates after every command.
Publishing should be quick and should not require a conversational response to the user.

Illustrative helper interface, not an existing command or a settled CLI contract:

```sh
potty-tools session update \
  --goal "Fix Ubuntu clipboard support" \
  --summary "X11 backend implemented; verifying selection ownership" \
  --next "Run integration test"
```

The implementation must resolve a stable agent-session identity, not just a pane number. Decide
whether this becomes a new executable or an extension of `potty-notify`; the example does not
imply that the existing tools package already provides a `potty-tools` executable.

Prefer a structured input option for multiline text. The helper should infer available pane,
instance, host, and directory metadata through the existing integration, keeping agent-written
arguments focused on the briefing. Missing Potty integration must not disrupt the agent's task.

### Lifecycle hooks

Use available agent hooks for explicit lifecycle signals such as turn started, approval requested,
and turn completed. Reuse the attention integration where possible, but retain separate state:

- A briefing describes work; it does not independently prove the agent is currently running.
- A notification dismissal acknowledges attention; it does not prove the task completed or an
  approval was granted.
- A completed turn is not necessarily a completed task. Label it accordingly unless task
  completion was explicitly reported.
- A quiet terminal is not evidence that an agent is idle, blocked, or finished.
- Disconnection changes visibility and reachability, not the last known execution state.

Show an unknown state when no suitable signal exists. Track briefing freshness separately from
lifecycle freshness; a new hook event must not make old prose appear newly updated. Keep event
ordering sufficient to prevent delayed or replayed updates from overwriting newer state.

### Optional model-generated briefings

Defer this until agent-supplied briefings have been tried. A later explicit "refresh briefing"
action could summarize recent captured output when no useful briefing exists. Prefer structured
conversation events when available: terminal screens can omit earlier context, overwrite output,
and hide activity inside full-screen applications or nested multiplexers.

Any inferred briefing should identify its source, capture time, and limited evidence. It must not
silently replace explicit lifecycle state. Context selection, provider configuration, cost, and
consent to send terminal content need their own design before implementation. Do not continuously
send all six terminal streams to a model by default.

## Identity, transport, and persistence

Build on the out-of-band path described in [attention-feed.md](attention-feed.md) and the native
SSH transport in [remote-sessions.md](remote-sessions.md). Briefings must not depend on whether a
pane is currently visible. Protocol changes and compatibility must be designed explicitly; do
not assume current notification messages can carry the new records unchanged.

- Identify an entry by the agent/tool session within its execution origin. Treat its current pane
  and client window as navigation locations, not the durable identity. Avoid collisions across
  hosts, daemon sessions, and local instances, and duplicate rows after reattachment.
- Retain the latest briefing and bounded milestones in `potty-session` for persistent remote
  sessions. Replay them on attach, including updates published while detached. Scope the initial
  guarantee to the daemon's lifetime; recovery across daemon or host restart is a separate choice.
- Keep the last known briefing visible while disconnected, clearly marked as unavailable for
  live updates. An old remote daemon should leave the terminal usable and make lack of overview
  support apparent.
- Mirror entries across local Potty windows using the existing instance-routing approach where
  applicable. Propagate explicit dismissals to the owning store so replay cannot resurrect them.
- Retain completed entries until explicitly dismissed, distinguishing unreviewed results from
  acknowledged ones. Focusing a row alone should not imply the task is reviewed or complete.
- If a pane no longer exists, mark the entry unavailable and allow dismissal. Do not leave an
  unremovable row or jump to a different pane that reused its number.
- For a nested multiplexer, navigation may only reach the containing Potty pane. Display any
  known inner location without promising that Potty can focus the inner agent session.

Dismissal should hide the current record, not permanently suppress a still-running session.
Define revision or acknowledgement semantics so a genuinely new update can surface again while
an old replay cannot. Local restart persistence, retention limits, and storage format remain
implementation decisions.

## First experiment

1. Define a minimal, versioned briefing record and publishing interface, with explicit identity,
   partial-update semantics, field limits, and source timestamps.
2. Provide an agent skill for publishing concise briefings and deliberate milestones. Validate it
   with at least one agent integration before generalizing provider-specific hooks.
3. Add the overview panel, stable rows, editable names, expanded milestones, and pane navigation.
4. Combine briefings with available lifecycle signals without conflating their meanings.
5. Support local windows and native persistent SSH sessions, including detached updates, replay,
   missing panes, and dismissal propagation.
6. Use the feature with roughly six real sessions and assess whether it reduces context recovery
   effort and whether agents keep their briefings useful without repeated prompting.

This experiment excludes a replacement agent conversation UI, shared-shell execution, generic
interactive Markdown/table rendering, automatic model summarization, and agent orchestration.
The briefing publisher can inform later structured-output work without requiring that broader
framework first.

## Validation and open decisions

Verify identity and ordering across duplicate delivery, reattachment, multiple clients, and
multiple agent sessions using the same pane over time. Test dismissal against the authoritative
remote store, independent notification acknowledgement, stale briefings, unknown lifecycle state,
and closed panes. Check panel readability with six entries, long task names, and narrow windows.

Treat all incoming fields as bounded, untrusted display data, never executable commands. Do not
collect transcripts, environment variables, or terminal contents automatically for the first
experiment. Briefings may still contain sensitive project details, so keep their storage within
the same user/session boundary as the existing integration.

Before implementation, resolve:

- Helper executable and input schema, including how the skill obtains the agent session ID.
- Provider hook mappings and the distinction between turn completion and task completion.
- Protocol compatibility, record ownership, ordering, and dismissal revisions.
- Local persistence and retention limits for dismissed entries and milestones.
- Panel entry point, default width, and narrow-window presentation.

The main product question remains practical: are briefings accurate and fresh enough to choose
the next session at a glance? If not, improve the reporting contract before adding more UI or
introducing model-generated summaries.
