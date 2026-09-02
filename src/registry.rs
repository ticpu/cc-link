//! The Claude Code session registry: discovery, and synthesis of the records a relay publishes.
//!
//! Every path comes from the environment, so a test can point `HOME` and `XDG_RUNTIME_DIR` at a
//! scratch tree and exercise publication without touching the real registry.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

/// Marker cc-link writes into the records it publishes, so a mirror is recognisable as one.
pub const STUB_MARKER: &str = "ccLinkStub";
/// Remote host a mirrored record was published for.
pub const STUB_HOST: &str = "ccLinkHost";

/// Namespace for the synthetic identifier a mirrored session is given.
const SESSION_ID_NAMESPACE: uuid::Uuid = uuid::uuid!("6f9d4a1c-2d3b-4f8e-9a71-0c5d8e2b7a44");

/// Longest `sun_path` a unix socket address can hold, minus the terminator.
const SUN_PATH_MAX: usize = 103;

/// Fields whose value describes the process behind a record rather than the session it fronts.
/// A mirrored record takes these from the relay, never from the peer.
const IDENTITY_FIELDS: &[&str] = &[
    "pid",
    "procStart",
    "pidDomain",
    "messagingSocketPath",
    "version",
    "peerProtocol",
    "sessionId",
    "name",
    "peerFeatures",
];

/// Filesystem locations the registry lives in.
#[derive(Clone, Debug)]
pub struct Paths {
    home: PathBuf,
    runtime: PathBuf,
    uid: u32,
}

impl Paths {
    /// Resolve from the environment.
    pub fn from_env() -> Result<Self> {
        let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
        let uid = nix::unistd::Uid::current().as_raw();
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("/run/user/{uid}")));
        Ok(Self {
            home: PathBuf::from(home),
            runtime,
            uid,
        })
    }

    /// Directory holding session records and their key files.
    pub fn sessions_dir(&self) -> PathBuf {
        self.home
            .join(".claude/sessions")
    }

    /// Directory holding session sockets. Falls back to the `/tmp` form only when the resulting
    /// socket path would not fit in a unix socket address, which is the condition Claude Code uses.
    pub fn sock_dir(&self) -> PathBuf {
        let primary = self
            .runtime
            .join("cc-socks");
        let longest = primary.join(format!("{}.sock", u32::MAX));
        if longest
            .as_os_str()
            .len()
            > SUN_PATH_MAX
        {
            PathBuf::from(format!("/tmp/cc-socks-{}", self.uid))
        } else {
            primary
        }
    }

    /// Socket a session with this pid binds.
    pub fn socket_path(&self, pid: u32) -> PathBuf {
        self.sock_dir()
            .join(format!("{pid}.sock"))
    }

    /// Record for a pid.
    pub fn record_path(&self, pid: u32) -> PathBuf {
        self.sessions_dir()
            .join(format!("{pid}.json"))
    }

    /// Key file for a pid, named after the socket path it belongs to.
    pub fn key_path(&self, pid: u32, socket_path: &Path) -> PathBuf {
        let digest = Sha256::digest(
            socket_path
                .as_os_str()
                .as_encoded_bytes(),
        );
        self.sessions_dir()
            .join(format!("{pid}.{}.key", hex::encode(digest)))
    }
}

/// A parsed session record. The whole document is kept: synthesis overlays onto it rather than
/// rebuilding from a field list.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record(pub Value);

impl Record {
    /// Process the record belongs to.
    pub fn pid(&self) -> Option<u32> {
        self.0
            .get("pid")?
            .as_u64()
            .map(|v| v as u32)
    }

    /// Process start time, as the kernel reports it.
    pub fn proc_start(&self) -> Option<String> {
        self.0
            .get("procStart")?
            .as_str()
            .map(str::to_owned)
    }

    /// Domain the process lives in: machine plus pid namespace.
    pub fn pid_domain(&self) -> Option<String> {
        self.0
            .get("pidDomain")?
            .as_str()
            .map(str::to_owned)
    }

    /// Display name.
    pub fn name(&self) -> Option<String> {
        self.0
            .get("name")?
            .as_str()
            .map(str::to_owned)
    }

    /// Socket the session listens on.
    pub fn socket_path(&self) -> Option<PathBuf> {
        self.0
            .get("messagingSocketPath")?
            .as_str()
            .map(PathBuf::from)
    }

    /// Whether this record was published by a relay rather than by a session.
    pub fn is_stub(&self) -> bool {
        self.0
            .get(STUB_MARKER)
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// Capabilities the session advertises.
    pub fn features(&self) -> BTreeSet<String> {
        self.0
            .get("peerFeatures")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Domain string for this machine and pid namespace.
pub fn local_pid_domain() -> Result<String> {
    let machine_id = fs::read_to_string("/etc/machine-id")
        .context("reading /etc/machine-id")?
        .trim()
        .to_owned();
    let ns = fs::read_link("/proc/self/ns/pid")
        .context("reading /proc/self/ns/pid")?
        .to_string_lossy()
        .into_owned();
    Ok(format!("linux:{machine_id}:{ns}"))
}

/// Start time of a process, field 22 of its stat line.
///
/// The field is located from the last `)` rather than by splitting the whole line: a process name
/// may itself contain spaces and parentheses, which would shift every index before it.
pub fn proc_start(pid: u32) -> Result<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))
        .with_context(|| format!("reading /proc/{pid}/stat"))?;
    let tail = stat
        .rfind(')')
        .map(|i| &stat[i + 1..])
        .ok_or_else(|| anyhow!("/proc/{pid}/stat has no comm field"))?;
    tail.split_whitespace()
        .nth(19)
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("/proc/{pid}/stat is too short to hold a start time"))
}

/// Whether a pid exists.
pub fn pid_alive(pid: u32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok()
}

/// Wall clock in milliseconds.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before the unix epoch")
        .as_millis() as u64
}

/// Records that pass the same discovery filter Claude Code applies: a pid-named file, a live
/// process, a matching start time, and — when the record carries one — a matching domain.
///
/// Records that fail are skipped and never removed: sweeping is the harness's job, and a record we
/// did not publish is not ours to delete.
pub fn list_live(paths: &Paths) -> Result<Vec<Record>> {
    let dir = paths.sessions_dir();
    let domain = local_pid_domain()?;
    let mut out = Vec::new();
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(stem) = name.strip_suffix(".json") else {
            continue;
        };
        let Ok(pid) = stem.parse::<u32>() else {
            continue;
        };
        if !pid_alive(pid) {
            continue;
        }
        let text = match fs::read_to_string(entry.path()) {
            Ok(text) => text,
            Err(e) => {
                warn!(path = %entry.path().display(), error = %e, "skipping unreadable record");
                continue;
            }
        };
        let value: Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(e) => {
                warn!(path = %entry.path().display(), error = %e, "skipping unparsable record");
                continue;
            }
        };
        let record = Record(value);
        if record.pid() != Some(pid) {
            debug!(pid, "skipping record whose pid disagrees with its filename");
            continue;
        }
        match (record.proc_start(), proc_start(pid).ok()) {
            (Some(recorded), Some(actual)) if recorded == actual => {}
            _ => {
                debug!(
                    pid,
                    "skipping record whose start time does not match the process"
                );
                continue;
            }
        }
        if let Some(recorded) = record.pid_domain() {
            if recorded != domain {
                debug!(pid, "skipping record from another domain");
                continue;
            }
        }
        out.push(record);
    }
    Ok(out)
}

/// Pick the session a link will export.
///
/// This is the only place that judgement is made. A mirror is never exportable: relaying through
/// one makes the grant transitive while neither outer end can see the far machine in any listing.
/// The marker in the record decides it; the process name is a weaker hint, since it is truncated by
/// the kernel and any process can set its own.
pub fn resolve_exportable_session(paths: &Paths, selector: Option<&str>) -> Result<Record> {
    let mut candidates = Vec::new();
    for record in list_live(paths)? {
        let pid = record
            .pid()
            .unwrap_or_default();
        if record.is_stub() {
            debug!(pid, "not exportable: already a mirror of a remote session");
            continue;
        }
        if comm(pid).as_deref() == Some(env!("CARGO_BIN_NAME")) {
            debug!(pid, "not exportable: process is a relay");
            continue;
        }
        if let Some(selector) = selector {
            let matches = record
                .name()
                .as_deref()
                == Some(selector)
                || pid.to_string() == selector
                || record
                    .0
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .is_some_and(|id| id == selector);
            if !matches {
                continue;
            }
        }
        candidates.push(record);
    }
    match candidates.len() {
        0 => match selector {
            Some(selector) => bail!("no live session matches {selector:?}"),
            None => bail!("no live session to export; cc-link mirrors a session, it is not one"),
        },
        1 => {
            let record = candidates.remove(0);
            debug!(
                pid = record.pid(),
                name = record.name(),
                "exporting session"
            );
            Ok(record)
        }
        n => {
            let names: Vec<String> = candidates
                .iter()
                .map(|r| {
                    format!(
                        "{} ({})",
                        r.name()
                            .unwrap_or_else(|| "unnamed".into()),
                        r.pid()
                            .unwrap_or_default()
                    )
                })
                .collect();
            bail!(
                "{n} live sessions to choose from, name one: {}",
                names.join(", ")
            )
        }
    }
}

/// Process name as the kernel reports it, truncated to its own limit.
fn comm(pid: u32) -> Option<String> {
    fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| {
            s.trim()
                .to_owned()
        })
}

/// What a relay knows about its own end of a link when it publishes a mirror.
#[derive(Clone, Debug)]
pub struct LocalIdentity {
    /// Relay process backing the mirrored record.
    pub pid: u32,
    /// Its start time.
    pub proc_start: String,
    /// Its domain.
    pub pid_domain: String,
    /// Socket it listens on.
    pub socket_path: PathBuf,
    /// Host the mirrored session lives on, used to prefix its name.
    pub host: String,
    /// Milliseconds to add to the peer's timestamps to express them on this clock.
    pub clock_offset_ms: i64,
}

/// Build the record a relay publishes for a remote session.
///
/// The template is a live local record, so the document keeps whatever shape this build of Claude
/// Code writes. The peer's values fill the fields that describe a session; the relay's own fill the
/// fields that describe a process. Anything the local build does not write is dropped rather than
/// carried across, and anything the peer does not send keeps the template's value.
pub fn synth_record(template: &Record, peer: &Record, local: &LocalIdentity) -> Result<Value> {
    let template_obj = template
        .0
        .as_object()
        .ok_or_else(|| anyhow!("template record is not an object"))?;
    let peer_obj = peer
        .0
        .as_object()
        .ok_or_else(|| anyhow!("peer record is not an object"))?;

    let dropped: Vec<&str> = peer_obj
        .keys()
        .filter(|k| !template_obj.contains_key(*k))
        .map(String::as_str)
        .collect();
    if !dropped.is_empty() {
        warn!(fields = ?dropped, "peer record carries fields this build does not write; dropping");
    }

    let mut out = Map::new();
    for (key, template_value) in template_obj {
        if IDENTITY_FIELDS.contains(&key.as_str()) {
            out.insert(key.clone(), template_value.clone());
            continue;
        }
        let value = match peer_obj.get(key) {
            Some(peer_value) => translate_timestamp(key, peer_value, local.clock_offset_ms),
            None => template_value.clone(),
        };
        out.insert(key.clone(), value);
    }

    out.insert(
        "pid".into(),
        local
            .pid
            .into(),
    );
    out.insert(
        "procStart".into(),
        local
            .proc_start
            .clone()
            .into(),
    );
    out.insert(
        "pidDomain".into(),
        local
            .pid_domain
            .clone()
            .into(),
    );
    out.insert(
        "messagingSocketPath".into(),
        local
            .socket_path
            .to_string_lossy()
            .into_owned()
            .into(),
    );
    out.insert("sessionId".into(), mirrored_session_id(peer).into());
    out.insert("name".into(), mirrored_name(peer, &local.host).into());
    out.insert(
        "peerFeatures".into(),
        Value::Array(
            shared_features(template, peer)
                .into_iter()
                .map(Value::from)
                .collect(),
        ),
    );
    out.insert(STUB_MARKER.into(), Value::Bool(true));
    out.insert(
        STUB_HOST.into(),
        local
            .host
            .clone()
            .into(),
    );

    Ok(Value::Object(out))
}

/// A timestamp from the peer is expressed on the peer's clock; bring it onto ours, and never let it
/// land in the future.
fn translate_timestamp(key: &str, value: &Value, offset_ms: i64) -> Value {
    let (Some(ms), true) = (
        value.as_u64(),
        key.ends_with("At") || key.ends_with("Since"),
    ) else {
        return value.clone();
    };
    let shifted = (ms as i64)
        .saturating_add(offset_ms)
        .max(0) as u64;
    Value::from(shifted.min(now_ms()))
}

/// Identifier for a mirrored session: stable across reconnects, distinct from the remote session's
/// own and from any local one, so two machines mirroring each other cannot collide.
fn mirrored_session_id(peer: &Record) -> String {
    let seed = format!(
        "{}/{}",
        peer.pid_domain()
            .unwrap_or_default(),
        peer.0
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
    );
    uuid::Uuid::new_v5(&SESSION_ID_NAMESPACE, seed.as_bytes()).to_string()
}

/// Host-prefixed name. The separator cannot be a path component, and the prefix is what keeps a
/// mirrored session from being mistaken for a local one.
fn mirrored_name(peer: &Record, host: &str) -> String {
    let name = peer
        .name()
        .unwrap_or_else(|| "unnamed".into());
    format!("{host}~{name}")
}

/// Capabilities both ends can serve, sanitized to what a reader will accept.
///
/// A capability advertised but not honoured sends a sender down a path that silently does nothing,
/// so a mirror claims only what the remote session and this build both offer.
fn shared_features(template: &Record, peer: &Record) -> Vec<String> {
    let local = template.features();
    peer.features()
        .into_iter()
        .filter(|f| local.contains(f))
        .filter(|f| {
            !f.is_empty()
                && f.len() <= 32
                && f.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
        .take(16)
        .collect()
}

/// Build the key file that accompanies a mirrored record.
///
/// Every field is the relay's own. The token exists so a session can recognise a process it spawned
/// itself; nothing else reads it here, so it is generated locally and the remote session's real
/// token never crosses the link.
pub fn synth_key(template_key: &Value, local: &LocalIdentity) -> Result<Value> {
    let template_obj = template_key
        .as_object()
        .ok_or_else(|| anyhow!("template key file is not an object"))?;
    let mut out = Map::new();
    for key in template_obj.keys() {
        let value = match key.as_str() {
            "peerToken" => Value::from(random_token()),
            "procStart" => Value::from(
                local
                    .proc_start
                    .clone(),
            ),
            "pidDomain" => Value::from(
                local
                    .pid_domain
                    .clone(),
            ),
            other => {
                warn!(
                    field = other,
                    "key file carries a field cc-link cannot fill; dropping"
                );
                continue;
            }
        };
        out.insert(key.clone(), value);
    }
    Ok(Value::Object(out))
}

/// 32 random bytes, hex encoded.
fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Read the key file belonging to a record, to use as a template.
pub fn read_key(paths: &Paths, record: &Record) -> Result<Value> {
    let pid = record
        .pid()
        .ok_or_else(|| anyhow!("record carries no pid"))?;
    let socket = record
        .socket_path()
        .ok_or_else(|| anyhow!("record carries no socket path"))?;
    let path = paths.key_path(pid, &socket);
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Write a mirrored record and its key file.
pub fn publish(paths: &Paths, local: &LocalIdentity, record: &Value, key: &Value) -> Result<()> {
    let dir = paths.sessions_dir();
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("setting mode on {}", dir.display()))?;

    let record_path = paths.record_path(local.pid);
    fs::write(&record_path, serde_json::to_vec_pretty(record)?)
        .with_context(|| format!("writing {}", record_path.display()))?;

    let key_path = paths.key_path(local.pid, &local.socket_path);
    fs::write(&key_path, serde_json::to_vec_pretty(key)?)
        .with_context(|| format!("writing {}", key_path.display()))?;
    fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("setting mode on {}", key_path.display()))?;
    Ok(())
}

/// Remove a mirrored record, its key file and its socket.
///
/// Each artifact is checked to be ours before it goes: a pid is reused, and removing another
/// session's record would take a live peer out of every listing.
pub fn unpublish(paths: &Paths, local: &LocalIdentity) -> Result<()> {
    let record_path = paths.record_path(local.pid);
    match fs::read_to_string(&record_path) {
        Ok(text) => {
            let ours = serde_json::from_str::<Value>(&text)
                .map(Record)
                .map(|r| {
                    r.pid() == Some(local.pid)
                        && r.proc_start()
                            .as_deref()
                            == Some(&local.proc_start)
                })
                .unwrap_or(false);
            if ours {
                remove(&record_path)?;
            } else {
                warn!(path = %record_path.display(), "leaving a record that is not ours");
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            warn!(path = %record_path.display(), error = %e, "could not read record before removing it")
        }
    }
    remove(&paths.key_path(local.pid, &local.socket_path))?;
    remove(&local.socket_path)?;
    Ok(())
}

fn remove(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

/// Move a mirrored record's activity timestamp forward.
///
/// Listings drop a peer whose timestamp has gone stale, so a link carrying no traffic would
/// otherwise vanish while still live.
pub fn touch_activity(paths: &Paths, local: &LocalIdentity) -> Result<()> {
    let path = paths.record_path(local.pid);
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let mut value: Value = serde_json::from_str(&text)?;
    let obj = value
        .as_object_mut()
        .ok_or_else(|| anyhow!("{} is not an object", path.display()))?;
    obj.insert("updatedAt".into(), now_ms().into());
    fs::write(&path, serde_json::to_vec_pretty(&value)?)
        .with_context(|| format!("writing {}", path.display()))
}

/// Confirm the session a relay fronts is still the one it was pointed at.
///
/// A pid outlives nothing: the kernel reuses it, and a new session binding the same socket would
/// otherwise be handed frames meant for a session that has exited.
pub fn validate_target(paths: &Paths, expected: &Record) -> Result<Record> {
    let pid = expected
        .pid()
        .ok_or_else(|| anyhow!("target record carries no pid"))?;
    let path = paths.record_path(pid);
    let text = fs::read_to_string(&path)
        .with_context(|| format!("target session {pid} is gone ({})", path.display()))?;
    let current = Record(serde_json::from_str(&text)?);
    if current.proc_start() != expected.proc_start() {
        bail!("pid {pid} has been reused by another process");
    }
    if current.pid_domain() != expected.pid_domain() {
        bail!("pid {pid} now belongs to another domain");
    }
    Ok(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn template() -> Record {
        Record(json!({
            "pid": 111,
            "sessionId": "local-uuid",
            "cwd": "/home/local/project",
            "startedAt": 1_000_u64,
            "procStart": "111111",
            "version": "2.1.258",
            "peerProtocol": 1,
            "peerFeatures": ["notify_idle", "artifact_yield"],
            "kind": "interactive",
            "pidDomain": "linux:local:pid:[1]",
            "messagingSocketPath": "/run/user/1000/cc-socks/111.sock",
            "name": "claude-code-aa",
            "status": "idle",
            "updatedAt": 2_000_u64,
        }))
    }

    fn peer() -> Record {
        Record(json!({
            "pid": 222,
            "sessionId": "remote-uuid",
            "cwd": "/home/remote/other",
            "startedAt": 5_000_u64,
            "procStart": "222222",
            "version": "2.1.239",
            "peerProtocol": 9,
            "peerFeatures": ["notify_idle", "BAD-FEATURE", "unknown_here"],
            "kind": "interactive",
            "pidDomain": "linux:remote:pid:[1]",
            "messagingSocketPath": "/run/user/1000/cc-socks/222.sock",
            "name": "claude-code-zz",
            "status": "shell",
            "updatedAt": 6_000_u64,
            "fieldFromAnotherBuild": true,
        }))
    }

    fn identity() -> LocalIdentity {
        LocalIdentity {
            pid: 333,
            proc_start: "333333".into(),
            pid_domain: "linux:local:pid:[1]".into(),
            socket_path: PathBuf::from("/run/user/1000/cc-socks/333.sock"),
            host: "p4".into(),
            clock_offset_ms: 0,
        }
    }

    #[test]
    fn identity_fields_are_local_and_descriptive_fields_are_the_peers() {
        let out = synth_record(&template(), &peer(), &identity()).unwrap();
        assert_eq!(out["pid"], 333);
        assert_eq!(out["procStart"], "333333");
        assert_eq!(out["pidDomain"], "linux:local:pid:[1]");
        assert_eq!(
            out["messagingSocketPath"],
            "/run/user/1000/cc-socks/333.sock"
        );
        assert_eq!(out["version"], "2.1.258");
        assert_eq!(out["peerProtocol"], 1);
        assert_eq!(out["status"], "shell");
        assert_eq!(out["cwd"], "/home/remote/other");
        assert_eq!(out["name"], "p4~claude-code-zz");
        assert_eq!(out[STUB_MARKER], true);
        assert_eq!(out[STUB_HOST], "p4");
    }

    #[test]
    fn fields_this_build_does_not_write_are_dropped() {
        let out = synth_record(&template(), &peer(), &identity()).unwrap();
        assert!(out
            .get("fieldFromAnotherBuild")
            .is_none());
    }

    #[test]
    fn features_are_intersected_and_sanitized() {
        let out = synth_record(&template(), &peer(), &identity()).unwrap();
        assert_eq!(out["peerFeatures"], json!(["notify_idle"]));
    }

    #[test]
    fn the_mirrored_identifier_is_neither_ends_own() {
        let out = synth_record(&template(), &peer(), &identity()).unwrap();
        assert_ne!(out["sessionId"], "remote-uuid");
        assert_ne!(out["sessionId"], "local-uuid");
        let again = synth_record(&template(), &peer(), &identity()).unwrap();
        assert_eq!(out["sessionId"], again["sessionId"]);
    }

    #[test]
    fn peer_timestamps_are_brought_onto_the_local_clock() {
        let mut local = identity();
        local.clock_offset_ms = -3_000;
        let out = synth_record(&template(), &peer(), &local).unwrap();
        assert_eq!(out["startedAt"], 2_000_u64);
        assert!(
            out["updatedAt"]
                .as_u64()
                .unwrap()
                <= now_ms()
        );
    }

    #[test]
    fn a_mirror_is_never_exportable() {
        let mut mirror = template();
        mirror.0[STUB_MARKER] = json!(true);
        assert!(mirror.is_stub());
        assert!(!template().is_stub());
    }

    #[test]
    fn key_files_carry_nothing_from_the_template() {
        let template_key = json!({
            "peerToken": "the-template-session-token",
            "procStart": "111111",
            "pidDomain": "linux:local:pid:[1]",
        });
        let out = synth_key(&template_key, &identity()).unwrap();
        assert_ne!(out["peerToken"], "the-template-session-token");
        assert_eq!(
            out["peerToken"]
                .as_str()
                .unwrap()
                .len(),
            64
        );
        assert_eq!(out["procStart"], "333333");
    }

    #[test]
    fn a_start_time_survives_a_process_name_containing_a_paren() {
        let stat = "42 (weird ) name) S 1 42 42 0 -1 4194304 0 0 0 0 0 0 0 0 20 0 1 0 987654 0 0";
        let tail = &stat[stat
            .rfind(')')
            .unwrap()
            + 1..];
        assert_eq!(
            tail.split_whitespace()
                .nth(19),
            Some("987654")
        );
    }

    #[test]
    fn the_key_filename_hashes_the_socket_path() {
        let paths = Paths {
            home: PathBuf::from("/home/x"),
            runtime: PathBuf::from("/run/user/1000"),
            uid: 1000,
        };
        let socket = PathBuf::from("/run/user/1000/cc-socks/1751283.sock");
        let expected =
            "1751283.f277a78061ef90b211f6b4c5ae77a323c7dcf36953afedb4e49f48d5fbc6ef57.key";
        assert_eq!(
            paths
                .key_path(1751283, &socket)
                .file_name()
                .unwrap(),
            expected
        );
    }
}
