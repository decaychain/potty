---
name: potty-briefing
description: Publish a concise task-level briefing before every final response when a Potty hook supplies a reporting command, including on discussion-only and unchanged turns. Retain the goal, current progress, and next action or wait across turns. Do not use outside a configured Potty session.
---

# Potty Briefing

Maintain a short task-level briefing for a user returning to several agent sessions.
It must answer: what is this work for, where does it stand, and what happens next?
This is a maintained task record, not a summary of the latest reply or tool call.

Keep the visible briefing scannable in a few seconds: a short task title, one short
sentence of progress, and one short next action or wait. Aim for at most 60 words
total across title, summary, and next. Do not use paragraphs, bullet lists, tool
logs, or repeated context in these fields. Milestones are optional; prefer zero to
three short phrases and omit detail already conveyed by the summary. The byte
limits below are transport ceilings, not a target length.

Use the exact reporting command supplied by the current session hook. Do not guess
a session ID or reuse one from another session. Send a single JSON object on stdin:

```json
{"title":"PTT-128: New Dashboard Screen", "summary":"Merge request updated with the overview feature.", "next":"Waiting for CI before merge review.", "milestones":[]}
```

When the supplied command includes `--briefing-file`, keep that argument and path
unchanged. The helper queues validated JSON for the installed hook to forward; it
does not connect to a socket from the sandbox. Success means queued, not confirmed
delivery. Do not try a direct socket command or request broader permissions instead.

Title: at most 120 bytes. Summary: at most 1200 bytes. Next: at most 600 bytes.
Milestones: at most five strings, each at most 300 bytes. All fields are plain text.
Each report replaces the previous briefing; milestones are a compact retained list,
not an append-only log. Quote JSON safely using a literal stdin heredoc or a JSON
serializer. Never interpolate untrusted text into shell commands.

Keep the title anchored to the overall goal and known ticket/reference, if any. A
follow-up such as "fix the lint", "push it", or "yes" normally advances that same
goal; it does not rename the task. Change the title when the user changes the goal.
Summary should retain the important accumulated outcome and current state across
turns, not list every edit. Next should name the next action, external wait, or
user decision. A CI wait is not a request for user approval. Do not invent ticket
IDs, merge requests, CI results, or background monitoring; report only what is known.

Before every final response in a configured Potty session, publish one concise
briefing using the supplied command, including on discussion-only and unchanged
turns. Keep an unchanged task's briefing stable rather than inventing progress.
You may also publish at meaningful milestones during long-running work; avoid
repetitive tool-level updates. Retain relevant earlier milestones instead of
overwriting the history with the latest turn. Use the context already available,
not additional investigation solely for this report. State uncertainty and
distinguish implemented, tested, and deployed. A stopped turn does not imply a
completed task.

Use the supplied command as part of the normal workflow when permitted. If
publication fails, is unavailable, or requires additional permission, skip it and
answer normally. Never ask for extra permissions or retry a failed publication.
Do not announce routine briefing publication in the final response.

Do not include secrets, credentials, private prompt excerpts, or raw command output.
Do not read transcripts or invoke another model to prepare this report. Subagents
must not update the parent session's briefing. This skill grants no additional
authority to execute, change files, or contact services.
