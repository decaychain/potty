//! Host-owned saved work contexts. Live pane bindings never survive a process restart.
use crate::agent::{self, Record};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    path::PathBuf,
};

const RECENT_LIMIT: usize = 128;
const SAVED_LIMIT: usize = 512;
// Leave room for JSON escaping inside a 16 MB protocol frame.
const MAX_FILE: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Annotation {
    pub name: String,
    pub notes: String,
    pub links: Vec<String>,
    pub kept: bool,
}

impl Annotation {
    pub fn valid(&self) -> bool {
        let text = |s: &str, max| {
            s.len() <= max && !s.chars().any(|c| c.is_control() && c != '\n' && c != '\t')
        };
        text(&self.name, 120)
            && text(&self.notes, 8192)
            && self.links.len() <= 16
            && self.links.iter().all(|s| s.len() <= 2048 && link_valid(s))
    }
    pub fn normalize(&mut self) {
        self.kept |=
            !self.name.trim().is_empty() || !self.notes.trim().is_empty() || !self.links.is_empty();
    }
}

pub fn link_valid(s: &str) -> bool {
    (s.starts_with("https://") || s.starts_with("http://"))
        && !s.chars().any(|c| c.is_whitespace() || c.is_control())
        && s.split_once("://")
            .is_some_and(|(_, host)| !host.is_empty())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Context {
    pub id: String,
    pub annotation: Annotation,
    pub edited: u64,
    pub updated: u64,
    pub sessions: Vec<Record>,
}

impl Context {
    pub fn title(&self) -> &str {
        if !self.annotation.name.is_empty() {
            return &self.annotation.name;
        }
        self.sessions
            .last()
            .and_then(|r| r.briefing.as_ref())
            .map_or_else(
                || {
                    self.sessions
                        .last()
                        .and_then(|r| r.prompt.lines().next())
                        .filter(|s| !s.is_empty())
                        .unwrap_or("Agent session")
                },
                |b| &b.title,
            )
    }
    fn valid(&self) -> bool {
        !self.id.is_empty()
            && self.id.len() <= 128
            && self.annotation.valid()
            && !self.sessions.is_empty()
            && self.sessions.len() <= 16
            && self.sessions.iter().all(Record::valid)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    pub owner: String,
    pub contexts: BTreeMap<String, Context>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            version: 1,
            owner: unique_id(),
            contexts: BTreeMap::new(),
        }
    }
}

impl Snapshot {
    fn retain_latest(&mut self) {
        // Read the first prototype's multi-session format without losing annotations or IDs.
        for context in self.contexts.values_mut() {
            if let Some(last) = context.sessions.pop() {
                context.sessions.clear();
                context.sessions.push(last);
            }
        }
    }
    pub fn valid(&self) -> bool {
        self.version == 1
            && !self.owner.is_empty()
            && self.owner.len() <= 128
            && self.contexts.len() <= RECENT_LIMIT + SAVED_LIMIT
            && self.contexts.iter().all(|(id, c)| id == &c.id && c.valid())
    }
    pub fn for_record(&self, r: &Record) -> Option<&Context> {
        self.contexts
            .values()
            .find(|c| c.sessions.iter().any(|s| same_session(s, r)))
    }
}

fn same_session(a: &Record, b: &Record) -> bool {
    a.tool == b.tool && a.session == b.session && a.host == b.host
}

fn unique_id() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        "{:x}-{:x}-{:x}",
        agent::now(),
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Message {
    #[serde(rename = "potty_context_open_v1")]
    Open {
        owner: String,
        id: String,
        pane: u64,
        cols: u16,
        rows: u16,
    },
    #[serde(rename = "potty_context_refresh_v1")]
    Refresh,
    #[serde(rename = "potty_contexts_v1")]
    Snapshot { snapshot: Snapshot },
    #[serde(rename = "potty_context_edit_v1")]
    Edit {
        owner: String,
        id: String,
        expected: u64,
        annotation: Annotation,
    },
    #[serde(rename = "potty_context_forget_v1")]
    Forget {
        owner: String,
        id: String,
        expected: u64,
    },
    #[serde(rename = "potty_context_error_v1")]
    Error { message: String },
}

impl Message {
    pub fn parse(json: &str) -> Option<Self> {
        if json.len() > (MAX_FILE + 4096) as usize {
            return None;
        }
        let mut message: Self = serde_json::from_str(json).ok()?;
        let valid = match &message {
            Self::Open {
                owner,
                id,
                cols,
                rows,
                ..
            } => owner.len() <= 128 && id.len() <= 128 && *cols > 0 && *rows > 0,
            Self::Refresh => true,
            Self::Snapshot { snapshot } => snapshot.valid(),
            Self::Edit {
                owner,
                id,
                annotation,
                ..
            } => owner.len() <= 128 && id.len() <= 128 && annotation.valid(),
            Self::Error { message } => message.len() <= 1024,
            Self::Forget { owner, id, .. } => owner.len() <= 128 && id.len() <= 128,
        };
        if let Self::Snapshot { snapshot } = &mut message {
            snapshot.retain_latest();
        }
        valid.then_some(message)
    }
    pub fn json(&self) -> String {
        if let Self::Error { message } = self {
            return serde_json::to_string(&Self::Error {
                message: agent::excerpt(message, 256),
            })
            .unwrap();
        }
        serde_json::to_string(self).unwrap()
    }
}

pub struct Repository {
    path: PathBuf,
    // Ephemeral grouping only; pane numbers can be reused after a restart.
    panes: BTreeMap<(String, u64), String>,
    pub snapshot: Snapshot,
}

impl Repository {
    pub fn bind(&mut self, id: &str, origin: String, pane: u64) {
        if self.snapshot.contexts.contains_key(id) {
            self.panes.insert((origin, pane), id.to_owned());
        }
    }
    pub fn apply(&mut self, message: &Message) -> io::Result<()> {
        match message {
            Message::Edit {
                owner,
                id,
                expected,
                annotation,
            } => self.edit(owner, id, *expected, annotation.clone()),
            Message::Forget {
                owner,
                id,
                expected,
            } => self.forget(owner, id, *expected),
            Message::Refresh => self.refresh(),
            _ => Err(io::Error::other("Not a context request")),
        }
    }

    pub fn open_default() -> io::Result<Self> {
        let base = std::env::var_os("XDG_DATA_HOME")
            .filter(|p| PathBuf::from(p).is_absolute())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .ok_or_else(|| io::Error::other("No home directory for saved contexts"))?;
        Self::open(base.join("potty/contexts.json"))
    }

    pub fn open(path: PathBuf) -> io::Result<Self> {
        let mut repo = Self {
            path,
            panes: BTreeMap::new(),
            snapshot: Snapshot::default(),
        };
        repo.transaction(|_| Ok(()))?;
        Ok(repo)
    }

    pub fn refresh(&mut self) -> io::Result<()> {
        self.snapshot = self
            .read()?
            .ok_or_else(|| io::Error::other("Saved context store disappeared"))?;
        Ok(())
    }

    fn read(&self) -> io::Result<Option<Snapshot>> {
        let file = match options().read(true).open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        if file.metadata()?.len() > MAX_FILE {
            return Err(io::Error::other("Saved context file is too large"));
        }
        let mut data = String::new();
        file.take(MAX_FILE + 1).read_to_string(&mut data)?;
        let mut snapshot: Snapshot = serde_json::from_str(&data).map_err(io::Error::other)?;
        if !snapshot.valid() {
            return Err(io::Error::other("Invalid saved context file"));
        }
        snapshot.retain_latest();
        Ok(Some(snapshot))
    }

    fn transaction(
        &mut self,
        change: impl FnOnce(&mut Snapshot) -> io::Result<()>,
    ) -> io::Result<()> {
        let dir = self
            .path
            .parent()
            .ok_or_else(|| io::Error::other("Invalid context path"))?;
        std::fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let meta = std::fs::symlink_metadata(dir)?;
            if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } {
                return Err(io::Error::other("Unsafe saved context directory"));
            }
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let lock = options()
            .read(true)
            .write(true)
            .create(true)
            .open(self.path.with_extension("lock"))?;
        lock.lock()?;
        let mut snapshot = self.read()?.unwrap_or_else(|| self.snapshot.clone());
        change(&mut snapshot)?;
        let data = serde_json::to_vec(&snapshot).map_err(io::Error::other)?;
        if data.len() as u64 > MAX_FILE {
            return Err(io::Error::other("Saved context storage limit reached"));
        }
        let temp = self.path.with_extension(format!("{}.tmp", unique_id()));
        let result = (|| {
            let mut file = options().write(true).create_new(true).open(&temp)?;
            file.write_all(&data)?;
            file.sync_all()?;
            std::fs::rename(&temp, &self.path)?;
            #[cfg(unix)]
            File::open(dir)?.sync_all()?;
            Ok::<_, io::Error>(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(temp);
        }
        result?;
        self.snapshot = snapshot;
        Ok(())
    }

    pub fn observe(&mut self, record: &Record) -> io::Result<bool> {
        if !record.valid() {
            return Ok(false);
        }
        if let Some(context) = self.snapshot.for_record(record)
            && let Some(pane) = record.pane
        {
            self.panes
                .insert((record.origin.clone(), pane), context.id.clone());
        }
        // Tool lifecycle churn doesn't change the durable briefing. Save meaningful content only.
        if self.snapshot.for_record(record).is_some_and(|c| {
            c.sessions.iter().any(|s| {
                same_session(s, record)
                    && s.cwd == record.cwd
                    && s.prompt_at >= record.prompt_at
                    && s.response_at >= record.response_at
                    && s.briefing_at >= record.briefing_at
            })
        }) {
            return Ok(false);
        }
        let pane = record.pane.map(|p| (record.origin.clone(), p));
        let grouped = pane.as_ref().and_then(|p| self.panes.get(p)).cloned();
        let mut selected = String::new();
        self.transaction(|snapshot| {
            let id = snapshot
                .for_record(record)
                .map(|c| c.id.clone())
                .or_else(|| grouped.filter(|id| snapshot.contexts.contains_key(id)))
                .unwrap_or_else(unique_id);
            let context = snapshot
                .contexts
                .entry(id.clone())
                .or_insert_with(|| Context {
                    id: id.clone(),
                    annotation: Annotation::default(),
                    edited: 0,
                    updated: 0,
                    sessions: Vec::new(),
                });
            let old = context
                .sessions
                .iter()
                .position(|s| same_session(s, record));
            // A delayed old snapshot must not replace the pane's newer session.
            if old.is_none() && record.revision() < context.updated {
                selected = id;
                return Ok(());
            }
            let mut store = agent::Store::default();
            let mut saved = record.clone();
            saved.origin = "history".into();
            if let Some(index) = old {
                store.merge(context.sessions.remove(index));
            }
            store.merge(saved.clone());
            saved = store.records.remove(&saved.key()).unwrap();
            saved.pane = None;
            saved.state = agent::State::Ended;
            saved.background = None;
            saved.background_at = 0;
            context.updated = context.updated.max(record.revision());
            context.sessions.clear();
            context.sessions.push(saved);
            selected = id;
            let mut recent: Vec<_> = snapshot
                .contexts
                .values()
                .filter(|c| !c.annotation.kept)
                .map(|c| (c.updated, c.id.clone()))
                .collect();
            recent.sort();
            let remove = recent.len().saturating_sub(RECENT_LIMIT);
            for (_, id) in recent.into_iter().take(remove) {
                snapshot.contexts.remove(&id);
            }
            Ok(())
        })?;
        if let Some(pane) = pane {
            self.panes.insert(pane, selected);
        }
        Ok(true)
    }

    pub fn edit(
        &mut self,
        owner: &str,
        id: &str,
        expected: u64,
        mut annotation: Annotation,
    ) -> io::Result<()> {
        annotation.normalize();
        if !annotation.valid() {
            return Err(io::Error::other("Invalid name, notes, or HTTP(S) links"));
        }
        self.transaction(|snapshot| {
            if snapshot.owner != owner {
                return Err(io::Error::other("Context belongs to a different host"));
            }
            if annotation.kept
                && snapshot
                    .contexts
                    .values()
                    .filter(|c| c.annotation.kept)
                    .count()
                    >= SAVED_LIMIT
                && !snapshot.contexts.get(id).is_some_and(|c| c.annotation.kept)
            {
                return Err(io::Error::other("Saved context limit reached"));
            }
            let c = snapshot
                .contexts
                .get_mut(id)
                .ok_or_else(|| io::Error::other("Context no longer exists"))?;
            if c.edited != expected {
                return Err(io::Error::other(
                    "Context changed in another window. Reopen the editor before saving.",
                ));
            }
            c.annotation = annotation;
            c.edited = agent::now().max(c.edited + 1);
            Ok(())
        })
    }

    pub fn forget(&mut self, owner: &str, id: &str, expected: u64) -> io::Result<()> {
        self.transaction(|snapshot| {
            if snapshot.owner != owner {
                return Err(io::Error::other("Context belongs to a different host"));
            }
            let c = snapshot
                .contexts
                .get(id)
                .ok_or_else(|| io::Error::other("Context no longer exists"))?;
            if c.edited != expected {
                return Err(io::Error::other(
                    "Context changed in another window. Refresh before forgetting.",
                ));
            }
            snapshot.contexts.remove(id);
            Ok(())
        })
    }
}

fn options() -> OpenOptions {
    #[allow(unused_mut)]
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    options
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!("potty-context-{}", unique_id())))
        }
        fn path(&self) -> PathBuf {
            self.0.join("contexts.json")
        }
        fn repo(&self) -> Repository {
            Repository::open(self.path()).unwrap()
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn record(session: &str, pane: u64) -> Record {
        agent::hook(crate::notify::Tool::Claude, &serde_json::json!({
            "hook_event_name":"UserPromptSubmit", "session_id":session, "prompt":"Implement a dashboard", "cwd":"/tmp"
        }), "process-1".into(), "host".into(), Some(pane)).unwrap()
    }
    #[test]
    fn survives_restart_without_live_bindings_and_groups_resumed_sessions() {
        let temp = Temp::new();
        let mut repo = temp.repo();
        let first = record("a", 1);
        repo.observe(&first).unwrap();
        let id = repo.snapshot.for_record(&first).unwrap().id.clone();
        let owner = repo.snapshot.owner.clone();
        let mut second = record("b", 1);
        second.tool = crate::notify::Tool::Codex;
        repo.observe(&second).unwrap();
        assert_eq!(repo.snapshot.contexts.len(), 1);
        repo.edit(
            &owner,
            &id,
            0,
            Annotation {
                notes: "Keep this separate from agent prose".into(),
                ..Default::default()
            },
        )
        .unwrap();
        drop(repo);
        let mut repo = temp.repo();
        assert_eq!(repo.snapshot.owner, owner);
        let c = &repo.snapshot.contexts[&id];
        assert!(c.annotation.kept);
        assert_eq!(c.sessions.len(), 1);
        assert_eq!(c.sessions[0].session, "b");
        assert!(
            c.sessions.iter().all(|r| r.pane.is_none()
                && r.origin == "history"
                && r.state == agent::State::Ended)
        );
        // An unrelated session using the same numeric pane after restart is not the old context.
        let third = record("unrelated", 1);
        repo.observe(&third).unwrap();
        assert_ne!(repo.snapshot.for_record(&third).unwrap().id, id);
        let mut resumed = second.clone();
        resumed.origin = "process-2".into();
        resumed.pane = Some(7);
        assert!(!repo.observe(&resumed).unwrap());
        let mut child = record("child", 7);
        child.origin = resumed.origin.clone();
        repo.observe(&child).unwrap();
        assert_eq!(repo.snapshot.for_record(&child).unwrap().id, id);
    }

    #[test]
    fn concurrent_writers_preserve_records_and_reject_stale_annotations() {
        let temp = Temp::new();
        let mut a = temp.repo();
        let mut b = temp.repo();
        let ra = record("a", 1);
        let rb = record("b", 2);
        a.observe(&ra).unwrap();
        b.observe(&rb).unwrap();
        a.refresh().unwrap();
        assert_eq!(a.snapshot.contexts.len(), 2);
        let owner = a.snapshot.owner.clone();
        let id = a.snapshot.for_record(&ra).unwrap().id.clone();
        let notes = Annotation {
            notes: "Human decision".into(),
            ..Default::default()
        };
        a.edit(&owner, &id, 0, notes).unwrap();
        assert!(b.edit(&owner, &id, 0, Annotation::default()).is_err());
        let mut update = ra.clone();
        update.briefing = Some(agent::Briefing {
            title: "Dashboard".into(),
            summary: "Implemented".into(),
            ..Default::default()
        });
        update.briefing_at = agent::now();
        b.observe(&update).unwrap();
        a.refresh().unwrap();
        let context = &a.snapshot.contexts[&id];
        assert_eq!(context.annotation.notes, "Human decision");
        assert_eq!(
            context.sessions[0].briefing.as_ref().unwrap().summary,
            "Implemented"
        );
        assert!(
            a.edit("wrong-host", &id, context.edited, Annotation::default())
                .is_err()
        );
    }

    #[test]
    fn tool_churn_does_not_write_and_bad_storage_is_not_overwritten() {
        let temp = Temp::new();
        let mut repo = temp.repo();
        let mut r = record("a", 1);
        repo.observe(&r).unwrap();
        let before = std::fs::read(temp.path()).unwrap();
        r.state_at += 10;
        assert!(!repo.observe(&r).unwrap());
        assert_eq!(std::fs::read(temp.path()).unwrap(), before);
        std::fs::write(temp.path(), "broken").unwrap();
        r.prompt_at += 1;
        assert!(repo.observe(&r).is_err());
        assert!(Repository::open(temp.path()).is_err());
        assert_eq!(std::fs::read_to_string(temp.path()).unwrap(), "broken");
    }

    #[test]
    fn recent_eviction_never_discards_kept_contexts() {
        let temp = Temp::new();
        let mut repo = temp.repo();
        let saved = record("saved", 1000);
        repo.observe(&saved).unwrap();
        let id = repo.snapshot.for_record(&saved).unwrap().id.clone();
        let owner = repo.snapshot.owner.clone();
        repo.edit(
            &owner,
            &id,
            0,
            Annotation {
                kept: true,
                ..Default::default()
            },
        )
        .unwrap();
        for i in 0..RECENT_LIMIT + 2 {
            repo.observe(&record(&i.to_string(), i as u64)).unwrap();
        }
        assert_eq!(repo.snapshot.contexts.len(), RECENT_LIMIT + 1);
        assert!(repo.snapshot.contexts.contains_key(&id));
        let edited = repo.snapshot.contexts[&id].edited;
        assert!(repo.forget(&owner, &id, 0).is_err());
        repo.forget(&owner, &id, edited).unwrap();
        assert!(!temp.repo().snapshot.contexts.contains_key(&id));
    }

    #[test]
    fn kept_context_retains_only_latest_session_and_keeps_annotations() {
        let temp = Temp::new();
        let mut repo = temp.repo();
        let first = record("0", 1);
        repo.observe(&first).unwrap();
        let id = repo.snapshot.for_record(&first).unwrap().id.clone();
        let owner = repo.snapshot.owner.clone();
        repo.edit(
            &owner,
            &id,
            0,
            Annotation {
                kept: true,
                ..Default::default()
            },
        )
        .unwrap();
        for i in 1..17 {
            repo.observe(&record(&i.to_string(), 1)).unwrap();
        }
        assert_eq!(repo.snapshot.contexts[&id].sessions.len(), 1);
        assert_eq!(repo.snapshot.contexts[&id].sessions[0].session, "16");
        assert_eq!(repo.snapshot.contexts.len(), 1);
        assert!(repo.snapshot.contexts[&id].annotation.kept);
        repo.observe(&first).unwrap();
        assert_eq!(repo.snapshot.contexts[&id].sessions[0].session, "16");
        assert_eq!(temp.repo().snapshot.contexts[&id].sessions.len(), 1);
    }

    #[test]
    fn legacy_multi_session_contexts_keep_latest_and_annotations() {
        let temp = Temp::new();
        let mut repo = temp.repo();
        let first = record("first", 1);
        repo.observe(&first).unwrap();
        let mut legacy = repo.snapshot.clone();
        let c = legacy.contexts.values_mut().next().unwrap();
        c.sessions.push(record("latest", 1));
        c.annotation.name = "My saved work".into();
        c.annotation.notes = "Important decision".into();
        c.annotation.kept = true;
        c.edited = 7;
        let id = c.id.clone();
        std::fs::write(temp.path(), serde_json::to_vec(&legacy).unwrap()).unwrap();
        let repo = temp.repo();
        let Message::Snapshot { snapshot: wire } = Message::parse(
            &Message::Snapshot {
                snapshot: legacy.clone(),
            }
            .json(),
        )
        .unwrap() else {
            panic!()
        };
        for snapshot in [&repo.snapshot, &wire] {
            let c = &snapshot.contexts[&id];
            assert_eq!(c.sessions.len(), 1);
            assert_eq!(c.sessions[0].session, "latest");
            assert_eq!(c.annotation, legacy.contexts[&id].annotation);
            assert_eq!(c.edited, 7);
            assert_eq!(snapshot.owner, legacy.owner);
        }
    }

    #[test]
    fn wire_bounds_and_link_schemes_are_validated() {
        for link in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "https://",
            "https://host/\ncommand",
        ] {
            assert!(!link_valid(link));
        }
        assert!(link_valid("https://example.com/issue/123"));
        let bad = Message::Edit {
            owner: "owner".into(),
            id: "id".into(),
            expected: 0,
            annotation: Annotation {
                links: vec!["file:///tmp/x".into()],
                ..Default::default()
            },
        };
        assert!(Message::parse(&bad.json()).is_none());
        assert!(Message::parse(&Message::Refresh.json()).is_some());
    }

    #[test]
    #[cfg(unix)]
    fn files_are_private_and_symlinks_are_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let temp = Temp::new();
        temp.repo();
        assert_eq!(
            std::fs::metadata(temp.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&temp.0).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let target = temp.0.join("elsewhere");
        std::fs::rename(temp.path(), &target).unwrap();
        std::os::unix::fs::symlink(&target, temp.path()).unwrap();
        assert!(Repository::open(temp.path()).is_err());
    }
}
