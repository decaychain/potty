//! Opt-in, data-only outbox. Hooks supply routing; queued data cannot choose a destination.

use potty::agent::{self, Briefing, Record};
use serde::{Deserialize, Serialize};
use std::{
    collections::hash_map::DefaultHasher,
    ffi::CString,
    fs::{DirBuilder, File, OpenOptions},
    hash::{Hash, Hasher},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

const PENDING: &str = "pending.json";
const MAX_AGE: u64 = 24 * 60 * 60 * 1_000_000_000;

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Identity {
    version: u8,
    origin: String,
    pane: Option<u64>,
    session: String,
}

impl Identity {
    fn from_record(record: &Record) -> Self {
        Self {
            version: 1,
            origin: record.origin.clone(),
            pane: record.pane,
            session: record.session.clone(),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    session: String,
    at: u64,
    briefing: Briefing,
}

pub struct Relay {
    dir: File,
    path: PathBuf,
    identity: Identity,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn private(metadata: &std::fs::Metadata) -> bool {
    metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0
}

fn directory(file: File) -> io::Result<File> {
    let metadata = file.metadata()?;
    if !metadata.is_dir() || !private(&metadata) {
        return Err(invalid(
            "relay directory must be private and owned by this user",
        ));
    }
    Ok(file)
}

fn open_directory(path: &Path) -> io::Result<File> {
    directory(
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?,
    )
}

// All children are fixed names or a hash, and are accessed relative to a pinned directory fd.
fn open_at(dir: &File, name: &str, flags: i32) -> io::Result<File> {
    let name = CString::new(name).map_err(io::Error::other)?;
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            0o600,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn regular(dir: &File, name: &str, flags: i32) -> io::Result<File> {
    let file = open_at(dir, name, flags)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || !private(&metadata)
        || metadata.nlink() != 1
        || metadata.len() > agent::MAX_WIRE as u64
    {
        return Err(invalid("unsafe or oversized relay file"));
    }
    Ok(file)
}

fn lock(dir: &File) -> io::Result<()> {
    if unsafe { libc::flock(dir.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn read_json<T: serde::de::DeserializeOwned>(dir: &File, name: &str) -> io::Result<T> {
    let file = regular(dir, name, libc::O_RDONLY)?;
    let mut bytes = Vec::new();
    file.take(agent::MAX_WIRE as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > agent::MAX_WIRE {
        return Err(invalid("relay file too large"));
    }
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

fn unlink(dir: &File, name: &str) -> io::Result<()> {
    let name = CString::new(name).map_err(io::Error::other)?;
    if unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn write_json(dir: &File, name: &str, value: &impl Serialize) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    if bytes.len() > agent::MAX_WIRE {
        return Err(invalid("relay file too large"));
    }
    // A fixed staging file bounds disk use, and rename keeps readers off partial reports.
    let mut file = regular(dir, "staging.json", libc::O_WRONLY | libc::O_CREAT)?;
    file.set_len(0)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    let from = CString::new("staging.json").unwrap();
    let to = CString::new(name).map_err(io::Error::other)?;
    if unsafe { libc::renameat(dir.as_raw_fd(), from.as_ptr(), dir.as_raw_fd(), to.as_ptr()) } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

impl Relay {
    pub fn prepare(record: &Record) -> io::Result<Self> {
        let root = std::env::temp_dir()
            .canonicalize()?
            .join(format!("potty-briefings-{}", unsafe { libc::geteuid() }));
        match DirBuilder::new().mode(0o700).create(&root) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        let root_dir = open_directory(&root)?;
        let identity = Identity::from_record(record);
        let mut hash = DefaultHasher::new();
        serde_json::to_vec(&identity)
            .map_err(io::Error::other)?
            .hash(&mut hash);
        let name = format!("{:016x}", hash.finish());
        let c_name = CString::new(name.as_str()).unwrap();
        if unsafe { libc::mkdirat(root_dir.as_raw_fd(), c_name.as_ptr(), 0o700) } != 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(error);
            }
        }
        let dir = directory(open_at(
            &root_dir,
            &name,
            libc::O_RDONLY | libc::O_DIRECTORY,
        )?)?;
        lock(&dir)?;
        match read_json::<Identity>(&dir, "identity.json") {
            Ok(existing) if existing == identity => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                write_json(&dir, "identity.json", &identity)?
            }
            _ => return Err(invalid("relay identity mismatch or corrupt metadata")),
        }
        Ok(Self {
            dir,
            path: root.join(name).join(PENDING),
            identity,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn stage(path: &Path, session: &str, briefing: Briefing) -> io::Result<()> {
        if !path.is_absolute() || path.file_name().and_then(|n| n.to_str()) != Some(PENDING) {
            return Err(invalid(
                "expected the pending.json path supplied by the session hook",
            ));
        }
        let parent = path
            .parent()
            .ok_or_else(|| invalid("missing relay directory"))?;
        let dir = open_directory(parent)?;
        lock(&dir)?;
        let identity: Identity = read_json(&dir, "identity.json")?;
        if identity.version != 1 || identity.session != session || !briefing.valid() {
            return Err(invalid("invalid briefing or relay session mismatch"));
        }
        write_json(
            &dir,
            PENDING,
            &Pending {
                session: session.to_owned(),
                at: agent::now(),
                briefing,
            },
        )
    }

    pub fn flush(
        &self,
        record: &Record,
        send: impl FnOnce(Record) -> io::Result<()>,
    ) -> io::Result<()> {
        let pending = match read_json::<Pending>(&self.dir, PENDING) {
            Ok(pending) => pending,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                let _ = unlink(&self.dir, PENDING);
                return Err(e);
            }
        };
        let now = agent::now();
        if Identity::from_record(record) != self.identity
            || pending.session != record.session
            || !pending.briefing.valid()
            || pending.at == 0
            || pending.at > now
            || now.saturating_sub(pending.at) > MAX_AGE
        {
            let _ = unlink(&self.dir, PENDING);
            return Err(invalid("invalid or stale queued briefing"));
        }
        let mut update = record.clone();
        update.state_at = 0;
        update.prompt.clear();
        update.prompt_at = 0;
        update.response.clear();
        update.response_at = 0;
        update.briefing_at = pending.at;
        update.briefing = Some(pending.briefing);
        send(update)?;
        unlink(&self.dir, PENDING)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    struct Fixture {
        record: Record,
        path: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let record = agent::hook(potty::notify::Tool::Codex,
                &serde_json::json!({"session_id":"test-session", "hook_event_name":"Stop", "last_assistant_message":"Done"}),
                format!("test-{}", agent::now()), "host".into(), Some(7)).unwrap();
            let path = Relay::prepare(&record).unwrap().path().to_owned();
            Self { record, path }
        }
        fn stage(&self) {
            Relay::stage(&self.path, &self.record.session, briefing()).unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.path.parent().unwrap());
        }
    }

    fn briefing() -> Briefing {
        Briefing {
            title: "Task".into(),
            summary: "Change tested".into(),
            next: "Review".into(),
            milestones: vec![],
        }
    }

    #[test]
    fn stages_atomically_and_only_flushes_once_with_hook_identity() {
        let f = Fixture::new();
        f.stage();
        let at = agent::now();
        let relay = Relay::prepare(&f.record).unwrap();
        relay
            .flush(&f.record, |r| {
                assert_eq!(r.key(), f.record.key());
                assert_eq!(r.pane, Some(7));
                assert_eq!(r.state_at, 0);
                assert_eq!(r.response_at, 0);
                assert!(r.response.is_empty());
                assert_eq!(r.briefing.unwrap().title, "Task");
                assert!(r.briefing_at > 0 && r.briefing_at <= at);
                Ok(())
            })
            .unwrap();
        assert!(!f.path.exists());
        relay
            .flush(&f.record, |_| panic!("duplicate delivery"))
            .unwrap();
    }

    #[test]
    fn refuses_wrong_sessions_and_isolates_panes_and_origins() {
        let f = Fixture::new();
        assert!(Relay::stage(&f.path, "other-session", briefing()).is_err());
        for change in 0..3 {
            let mut other = f.record.clone();
            match change {
                0 => other.session = "other-session".into(),
                1 => other.pane = Some(8),
                _ => other.origin = "other-socket".into(),
            }
            let relay = Relay::prepare(&other).unwrap();
            assert_ne!(relay.path(), f.path);
            let dir = relay.path().parent().unwrap().to_owned();
            relay
                .flush(&other, |_| panic!("cross-session delivery"))
                .unwrap();
            drop(relay);
            std::fs::remove_dir_all(dir).unwrap();
        }
        let mut identity: Identity = read_json(
            &open_directory(f.path.parent().unwrap()).unwrap(),
            "identity.json",
        )
        .unwrap();
        identity.session = "other-session".into();
        let dir = open_directory(f.path.parent().unwrap()).unwrap();
        write_json(&dir, "identity.json", &identity).unwrap();
        assert!(Relay::prepare(&f.record).is_err());
    }

    #[test]
    fn failed_delivery_retains_only_the_latest_report_and_locks_do_not_wait() {
        let f = Fixture::new();
        f.stage();
        let relay = Relay::prepare(&f.record).unwrap();
        assert!(Relay::prepare(&f.record).is_err());
        assert!(Relay::stage(&f.path, &f.record.session, briefing()).is_err());
        assert!(
            relay
                .flush(&f.record, |_| Err(io::Error::other("offline")))
                .is_err()
        );
        assert!(f.path.exists());
        drop(relay);
        let mut latest = briefing();
        latest.summary = "New progress".into();
        Relay::stage(&f.path, &f.record.session, latest).unwrap();
        Relay::prepare(&f.record)
            .unwrap()
            .flush(&f.record, |r| {
                assert_eq!(r.briefing.unwrap().summary, "New progress");
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn rejects_malformed_oversized_stale_and_routed_payloads() {
        let f = Fixture::new();
        for bad in [
            "{".to_owned(),
            "x".repeat(agent::MAX_WIRE + 1),
            serde_json::json!({"session":"other", "at":agent::now(), "briefing":briefing()}).to_string(),
            serde_json::json!({"session":f.record.session, "at":1, "briefing":briefing()}).to_string(),
            serde_json::json!({"session":f.record.session, "at":u64::MAX, "briefing":briefing()}).to_string(),
            serde_json::json!({"session":f.record.session, "at":agent::now(), "briefing":briefing(), "socket":"/tmp/other.sock"}).to_string(),
            serde_json::json!({"session":f.record.session, "at":agent::now(), "briefing":{"title":"", "summary":"x"}}).to_string(),
        ] {
            f.stage();
            std::fs::write(&f.path, bad).unwrap();
            assert!(Relay::prepare(&f.record).unwrap().flush(&f.record, |_| panic!("invalid publication")).is_err());
            assert!(!f.path.exists());
        }
    }

    #[test]
    fn refuses_links_nonregular_files_and_public_permissions() {
        let f = Fixture::new();
        let parent = f.path.parent().unwrap();
        let target = parent.join("target");
        std::fs::write(&target, "do not read or change").unwrap();
        symlink(&target, &f.path).unwrap();
        assert!(
            Relay::prepare(&f.record)
                .unwrap()
                .flush(&f.record, |_| panic!("symlink read"))
                .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "do not read or change"
        );
        let staging = parent.join("staging.json");
        symlink(&target, &staging).unwrap();
        assert!(Relay::stage(&f.path, &f.record.session, briefing()).is_err());
        std::fs::remove_file(&staging).unwrap();
        std::fs::hard_link(&target, &staging).unwrap();
        assert!(Relay::stage(&f.path, &f.record.session, briefing()).is_err());
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "do not read or change"
        );
        std::fs::remove_file(&staging).unwrap();
        let path = CString::new(f.path.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        assert!(
            Relay::prepare(&f.record)
                .unwrap()
                .flush(&f.record, |_| panic!("FIFO read"))
                .is_err()
        );
        f.stage();
        std::fs::set_permissions(&f.path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            Relay::prepare(&f.record)
                .unwrap()
                .flush(&f.record, |_| panic!("public file"))
                .is_err()
        );
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Relay::prepare(&f.record).is_err());
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
}
