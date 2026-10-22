---
name: potty-briefing
description: Publish a concise session briefing to Potty's session overview when a Potty hook supplies a reporting command. Use for substantial work in a Potty pane, especially a change of goal, a meaningful result, a blocker, or a handoff. Do not use outside a configured Potty session.
---

# Potty Briefing

Keep a short, factual handoff for a user juggling several agent sessions. Reporting is
optional telemetry, not part of the task's success criteria. Never request extra
permissions, retry failures, or delay the user's work solely to publish a briefing.

Use the exact reporting command supplied by the current session hook. Do not guess
a session ID or reuse one from another session. Send a single JSON object on stdin:

```json
{"title":"Fix SSH replay", "summary":"Reproduced partial escape sequences; fix is not yet validated.", "next":"Run detach/reattach regression tests.", "milestones":["Added a regression test."]}
```

Title: at most 120 bytes. Summary: at most 1200 bytes. Next: at most 600 bytes.
Milestones: at most five strings, each at most 300 bytes. All fields are plain text.
Each report replaces the previous briefing; milestones are a compact retained list,
not an append-only log. Quote JSON safely using a literal stdin heredoc or a JSON
serializer. Never interpolate untrusted text into shell commands.

Report after understanding a substantial request, at a meaningful milestone or
blocker, and before the final response when practical. Skip trivial turns and
repetitive tool-level updates. State uncertainty, unverified tests, and the precise
decision needed. Distinguish implemented, tested, and deployed. Do not claim the
task is complete merely because a turn ended.

Do not include secrets, credentials, private prompt excerpts, or raw command output.
Do not read transcripts or invoke another model to prepare this report. Subagents
must not update the parent session's briefing. This skill grants no additional
authority to execute, change files, or contact services.
