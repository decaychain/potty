//! Best-effort session telemetry. Independent clocks keep hook state and agent prose separate.
use crate::notify::Tool;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MAX_WIRE: usize = 48 * 1024;
pub const MAX_RECORDS: usize = 128;

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Working,
    NeedsYou,
    ResponseReady,
    Ended,
    Unknown,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Briefing {
    pub title: String,
    pub summary: String,
    #[serde(default)]
    pub next: String,
    #[serde(default)]
    pub milestones: Vec<String>,
}

impl Briefing {
    pub fn valid(&self) -> bool {
        valid_text(&self.title, 120)
            && !self.title.trim().is_empty()
            && valid_text(&self.summary, 1200)
            && !self.summary.trim().is_empty()
            && valid_text(&self.next, 600)
            && self.milestones.len() <= 5
            && self.milestones.iter().all(|s| valid_text(s, 300))
    }
}

fn valid_text(s: &str, max: usize) -> bool {
    s.len() <= max && !s.chars().any(|c| c.is_control() && c != '\n' && c != '\t')
}

pub fn excerpt(s: &str, max: usize) -> String {
    s.chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .take(max)
        .collect()
}

/// A turn-end snapshot, not a process liveness check. No commands or task output are retained.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Background {
    pub tasks: u32,
    pub monitors: u32,
}

impl Background {
    fn from_hook(payload: &serde_json::Value) -> Option<Self> {
        let tasks = payload.get("background_tasks")?.as_array()?;
        if tasks.len() > 1024 {
            return None;
        }
        let mut ids = std::collections::HashSet::new();
        let mut snapshot = Self {
            tasks: 0,
            monitors: 0,
        };
        for task in tasks {
            let id = task.get("id")?.as_str()?;
            let kind = task.get("type")?.as_str()?;
            let status = task.get("status")?.as_str()?;
            if id.is_empty() {
                return None;
            }
            if matches!(status, "running" | "pending") && ids.insert(id) {
                snapshot.tasks += 1;
                snapshot.monitors += u32::from(kind == "monitor");
            }
        }
        Some(snapshot)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub origin: String,
    pub tool: Tool,
    pub session: String,
    pub host: String,
    pub cwd: String,
    pub pane: Option<u64>,
    pub state: State,
    pub state_at: u64,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub prompt_at: u64,
    #[serde(default)]
    pub response: String,
    #[serde(default)]
    pub response_at: u64,
    pub briefing: Option<Briefing>,
    #[serde(default)]
    pub briefing_at: u64,
    #[serde(default)]
    pub background: Option<Background>,
    #[serde(default)]
    pub background_at: u64,
    #[serde(default)]
    pub dismissed: u64,
}

impl Record {
    pub fn key(&self) -> String {
        serde_json::to_string(&(&self.origin, &self.host, self.tool, &self.session)).unwrap()
    }
    pub fn revision(&self) -> u64 {
        self.state_at
            .max(self.prompt_at)
            .max(self.response_at)
            .max(self.briefing_at)
            .max(self.background_at)
    }
    pub fn visible(&self) -> bool {
        self.revision() > self.dismissed
    }
    pub fn valid(&self) -> bool {
        !self.origin.is_empty()
            && valid_text(&self.origin, 1024)
            && !self.session.is_empty()
            && valid_text(&self.session, 256)
            && valid_text(&self.host, 256)
            && valid_text(&self.cwd, 4096)
            && valid_text(&self.prompt, 8192)
            && valid_text(&self.response, 8192)
            && self.briefing.as_ref().is_none_or(Briefing::valid)
            && self
                .background
                .as_ref()
                .is_none_or(|b| b.monitors <= b.tasks && b.tasks <= 1024)
            && self.revision() > 0
    }
}

// Travels inside the existing opaque Notify frame. Old clients ignore this JSON payload.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "record")]
pub enum Message {
    #[serde(rename = "potty_session_overview_v1")]
    Record(Record),
}

impl Message {
    pub fn parse(json: &str) -> Option<Record> {
        if json.len() > MAX_WIRE {
            return None;
        }
        let Self::Record(record) = serde_json::from_str(json).ok()?;
        record.valid().then_some(record)
    }
    pub fn json(record: Record) -> String {
        serde_json::to_string(&Self::Record(record)).unwrap()
    }
}

#[derive(Default)]
pub struct Store {
    pub records: BTreeMap<String, Record>,
}

impl Store {
    pub fn merge(&mut self, incoming: Record) {
        if !incoming.valid() {
            return;
        }
        let key = incoming.key();
        if let Some(old) = self.records.get_mut(&key) {
            if incoming.state_at > old.state_at {
                old.state = incoming.state;
                old.state_at = incoming.state_at;
                old.pane = incoming.pane;
                old.cwd = incoming.cwd.clone();
                old.host = incoming.host.clone();
            }
            if incoming.prompt_at > old.prompt_at {
                old.prompt = incoming.prompt;
                old.prompt_at = incoming.prompt_at;
            }
            if incoming.response_at > old.response_at {
                old.response = incoming.response;
                old.response_at = incoming.response_at;
            }
            if incoming.briefing_at > old.briefing_at {
                old.briefing = incoming.briefing;
                old.briefing_at = incoming.briefing_at;
            }
            if incoming.background_at > old.background_at {
                old.background = incoming.background;
                old.background_at = incoming.background_at;
            }
            old.dismissed = old.dismissed.max(incoming.dismissed);
        } else {
            self.records.insert(key, incoming);
        }
        while self.records.len() > MAX_RECORDS {
            let key = self
                .records
                .iter()
                .min_by_key(|(_, r)| r.revision())
                .unwrap()
                .0
                .clone();
            self.records.remove(&key);
        }
    }
}

/// Normalize only documented root-session events. Missing identities never create fake sessions.
pub fn hook(
    tool: Tool,
    payload: &serde_json::Value,
    origin: String,
    host: String,
    pane: Option<u64>,
) -> Option<Record> {
    if tool == Tool::Other || payload.get("agent_id").is_some_and(|v| !v.is_null()) {
        return None;
    }
    let event = payload.get("hook_event_name")?.as_str()?;
    let state = match event {
        "SessionStart" => State::Unknown,
        "UserPromptSubmit" | "PreToolUse" | "PostToolUse" => State::Working,
        "PermissionRequest" => State::NeedsYou,
        "Stop" => State::ResponseReady,
        "SessionEnd" => State::Ended,
        "Interrupt" | "StopFailure" => State::Unknown,
        _ => return None,
    };
    let ts = now();
    let mut record = Record {
        origin,
        tool,
        session: payload.get("session_id")?.as_str()?.to_owned(),
        host,
        cwd: payload
            .get("cwd")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned(),
        pane,
        state,
        state_at: ts,
        prompt: String::new(),
        prompt_at: 0,
        response: String::new(),
        response_at: 0,
        briefing: None,
        briefing_at: 0,
        background: None,
        background_at: 0,
        dismissed: 0,
    };
    if event == "UserPromptSubmit" {
        record.prompt = excerpt(
            payload
                .get("prompt")
                .and_then(|v| v.as_str())
                .unwrap_or_default(),
            1600,
        );
        record.prompt_at = ts;
    }
    if event == "Stop" {
        record.response = excerpt(
            payload
                .get("last_assistant_message")
                .and_then(|v| v.as_str())
                .unwrap_or_default(),
            1600,
        );
        record.response_at = ts;
    }
    if tool == Tool::Claude && matches!(event, "Stop" | "SessionStart" | "SessionEnd") {
        record.background_at = ts;
        if event == "Stop" {
            record.background = Background::from_hook(payload);
        }
    }
    record.valid().then_some(record)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_snapshots_are_independent_and_missing_is_not_zero() {
        let stop = |extra: serde_json::Value| {
            let mut payload = serde_json::json!({"hook_event_name":"Stop", "session_id":"a"});
            payload
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            hook(
                Tool::Claude,
                &payload,
                "origin".into(),
                "host".into(),
                Some(1),
            )
            .unwrap()
        };
        let running = stop(serde_json::json!({"background_tasks":[
            {"id":"shell", "type":"shell", "status":"running", "command":"sensitive"},
            {"id":"watch", "type":"monitor", "status":"pending"},
            {"id":"watch", "type":"monitor", "status":"pending"},
            {"id":"done", "type":"shell", "status":"completed"}
        ]}));
        assert_eq!(running.state, State::ResponseReady);
        assert_eq!(
            running.background,
            Some(Background {
                tasks: 2,
                monitors: 1
            })
        );
        let wire = Message::json(running.clone());
        assert!(!wire.contains("sensitive"));
        assert_eq!(
            Message::parse(&wire).unwrap().background,
            running.background
        );
        let mut store = Store::default();
        store.merge(running.clone());
        let mut working = running.clone();
        working.state = State::Working;
        working.state_at += 1;
        working.background = None;
        working.background_at = 0;
        store.merge(working);
        assert_eq!(store.records[&running.key()].background, running.background);
        let empty = stop(serde_json::json!({"background_tasks":[]}));
        assert_eq!(
            empty.background,
            Some(Background {
                tasks: 0,
                monitors: 0
            })
        );
        store.merge(empty.clone());
        store.merge(running.clone());
        assert_eq!(store.records[&running.key()].background, empty.background);
        for extra in [
            serde_json::json!({}),
            serde_json::json!({"background_tasks":[{}]}),
        ] {
            let unknown = stop(extra);
            store.merge(unknown.clone());
            assert!(store.records[&unknown.key()].background.is_none());
            assert!(unknown.background_at > 0);
        }
        for event in ["SessionStart", "SessionEnd"] {
            let reset = hook(
                Tool::Claude,
                &serde_json::json!({"hook_event_name":event, "session_id":"a"}),
                "origin".into(),
                "host".into(),
                Some(1),
            )
            .unwrap();
            assert!(reset.background.is_none());
            assert!(reset.background_at > running.background_at);
        }
        let mut legacy = serde_json::to_value(running).unwrap();
        legacy.as_object_mut().unwrap().remove("background");
        legacy.as_object_mut().unwrap().remove("background_at");
        let legacy: Record = serde_json::from_value(legacy).unwrap();
        assert!(legacy.valid());
        assert!(legacy.background.is_none());
        assert_eq!(legacy.background_at, 0);
    }
    fn sample() -> Record {
        hook(Tool::Codex, &serde_json::json!({"hook_event_name":"UserPromptSubmit", "session_id":"a", "prompt":"Fix it"}), "origin".into(), "host".into(), Some(1)).unwrap()
    }
    #[test]
    fn replay_and_late_hooks_preserve_briefings_and_dismissals() {
        let mut store = Store::default();
        let original = sample();
        let mut report = original.clone();
        report.briefing = Some(Briefing {
            title: "Fix".into(),
            summary: "Testing".into(),
            ..Default::default()
        });
        report.briefing_at = original.state_at + 1;
        store.merge(report.clone());
        let mut dismissed = report.clone();
        dismissed.dismissed = dismissed.revision();
        store.merge(dismissed);
        store.merge(original.clone());
        store.merge(report);
        assert!(!store.records[&original.key()].visible());
        let mut next = original.clone();
        next.state_at += 2;
        store.merge(next);
        let result = &store.records[&original.key()];
        assert!(result.visible());
        assert!(result.briefing.is_some());
    }
    #[test]
    fn both_tools_stop_means_response_not_task_completion() {
        for tool in [Tool::Codex, Tool::Claude] {
            let payload = serde_json::json!({"session_id":"s", "hook_event_name":"Stop", "last_assistant_message":"Need your decision"});
            let record = hook(tool, &payload, "o".into(), "h".into(), None).unwrap();
            assert_eq!(record.state, State::ResponseReady);
            assert_eq!(record.response, "Need your decision");
            assert!(Message::parse(&Message::json(record)).is_some());
        }
        assert!(
            hook(
                Tool::Codex,
                &serde_json::json!({"hook_event_name":"Stop"}),
                "o".into(),
                "h".into(),
                None
            )
            .is_none()
        );
    }
    #[test]
    fn rejects_malformed_briefings() {
        let mut r = sample();
        r.briefing = Some(Briefing {
            title: "\u{1b}[31m".into(),
            summary: "x".into(),
            ..Default::default()
        });
        assert!(Message::parse(&Message::json(r)).is_none());
    }

    #[test]
    fn identity_is_scoped_and_retention_bounded() {
        let record = sample();
        let mut other = record.clone();
        other.tool = Tool::Claude;
        assert_ne!(record.key(), other.key());
        other = record.clone();
        other.origin.push('2');
        assert_ne!(record.key(), other.key());
        other = record.clone();
        other.host.push('2');
        assert_ne!(record.key(), other.key());
        let mut store = Store::default();
        for i in 0..MAX_RECORDS + 10 {
            let mut r = record.clone();
            r.session = i.to_string();
            r.state_at += i as u64;
            store.merge(r);
        }
        assert_eq!(store.records.len(), MAX_RECORDS);
        assert!(!store.records.values().any(|r| r.session == "0"));
    }

    #[test]
    fn hooks_ignore_identified_subagents_and_map_lifecycle_conservatively() {
        for tool in [Tool::Claude, Tool::Codex] {
            for (event, state) in [
                ("PermissionRequest", State::NeedsYou),
                ("PreToolUse", State::Working),
                ("PostToolUse", State::Working),
                ("SessionEnd", State::Ended),
                ("Interrupt", State::Unknown),
                ("StopFailure", State::Unknown),
            ] {
                let mut payload = serde_json::json!({"hook_event_name":event, "session_id":"root"});
                let record = hook(tool, &payload, "o".into(), "h".into(), Some(1)).unwrap();
                assert_eq!(record.state, state);
                payload["agent_id"] = "child".into();
                assert!(hook(tool, &payload, "o".into(), "h".into(), Some(1)).is_none());
            }
        }
    }
}
