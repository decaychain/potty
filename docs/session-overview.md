# Agent session overview

> Proposal. Status: **UI preview with mock data**. Records the intended experience and a bounded
> first experiment; helper syntax, wire format, and storage details remain open.

## Trying the UI preview

```sh
cargo run --bin potty -- --overview-demo
```

This explicitly enables a separate, in-memory sample dataset and opens the overview. Normal
launches do not show it. The panel is labelled "Sample data" and does not read or change live
agent notifications, start agent processes, or connect to the sample hosts.

The preview includes six sessions, status filters, expandable briefings and milestones, dismissal,
and a reset action in the panel's overflow menu. That menu also offers **Docked / Floating** and
**Left / Right**, with docked-right as the default. These preview preferences last for the current
instance, including closing and reopening the panel. The tab-bar overview button and the Sessions
menu item toggle it. Docked mode reserves space and supports resizing; floating mode overlays the
terminal without changing its dimensions. Narrow windows use the central area and suppress input
to the hidden shell in either mode. Escape closes the floating or full-window view. Pane navigation
is disabled for samples; editable names, live updates, and persistence are not implemented yet.

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
