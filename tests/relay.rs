//! End to end over a real pair of relays, with two registries under `target/`.
//!
//! The two ends differ by home directory, which is also what keeps the same-machine refusal from
//! firing: a link between two accounts on one host is a legitimate thing to test, a link that comes
//! back to the same registry is not.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

/// One side's registry: a home, a runtime directory, and a session that is really this test.
struct Machine {
    name: String,
    home: PathBuf,
    runtime: PathBuf,
    session_socket: PathBuf,
    session_name: String,
}

impl Machine {
    fn new(name: &str) -> Result<Self> {
        // Short, because a socket path that does not fit a unix address sends the relay to its
        // /tmp fallback and the test would then exercise a path the real thing rarely takes.
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("{name}{}", std::process::id()));
        let home = root.join("home");
        let runtime = root.join("run");
        std::fs::create_dir_all(home.join(".claude/sessions"))?;
        std::fs::create_dir_all(runtime.join("cc-socks"))?;
        let pid = std::process::id();
        let session_socket = runtime.join(format!("cc-socks/{pid}.sock"));
        Ok(Self {
            name: name.to_owned(),
            home,
            runtime,
            session_socket,
            session_name: format!("session-{name}"),
        })
    }

    /// Write the record and key file for the session this end exports.
    fn publish_session(&self) -> Result<()> {
        let pid = std::process::id();
        let record = json!({
            "pid": pid,
            "sessionId": format!("session-id-{}", self.name),
            "cwd": self.home.to_string_lossy(),
            "startedAt": now_ms(),
            "procStart": proc_start(pid)?,
            "version": "2.1.258",
            "peerProtocol": 1,
            "peerFeatures": ["notify_idle", "artifact_yield"],
            "kind": "interactive",
            "pidDomain": pid_domain()?,
            "messagingSocketPath": self.session_socket.to_string_lossy(),
            "name": self.session_name,
            "status": "idle",
            "updatedAt": now_ms(),
        });
        std::fs::write(
            self.sessions_dir()
                .join(format!("{pid}.json")),
            serde_json::to_vec_pretty(&record)?,
        )?;
        let key = json!({
            "peerToken": format!("token-for-{}", self.name),
            "procStart": proc_start(pid)?,
            "pidDomain": pid_domain()?,
        });
        std::fs::write(
            self.sessions_dir()
                .join(format!(
                    "{pid}.{}.key",
                    sha256_hex(
                        &self
                            .session_socket
                            .to_string_lossy()
                    )
                )),
            serde_json::to_vec_pretty(&key)?,
        )?;
        Ok(())
    }

    /// Rewrite the exported session's record the way Claude Code does: whole, not in place.
    fn set_status(&self, status: &str) -> Result<()> {
        let pid = std::process::id();
        let path = self
            .sessions_dir()
            .join(format!("{pid}.json"));
        let mut record: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
        record["status"] = json!(status);
        record["statusUpdatedAt"] = json!(now_ms());
        record["updatedAt"] = json!(now_ms());
        let temporary = path.with_extension("json.new");
        std::fs::write(&temporary, serde_json::to_vec_pretty(&record)?)?;
        std::fs::rename(&temporary, &path)?;
        Ok(())
    }

    fn sessions_dir(&self) -> PathBuf {
        self.home
            .join(".claude/sessions")
    }

    /// Every mirror in this end's registry.
    fn mirrors(&self) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(self.sessions_dir())? {
            let path = entry?.path();
            if path
                .extension()
                .and_then(|e| e.to_str())
                != Some("json")
            {
                continue;
            }
            let value: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
            if value
                .get("ccLinkStub")
                .and_then(Value::as_bool)
                == Some(true)
            {
                out.push(value);
            }
        }
        Ok(out)
    }
}

/// Stand in for a Claude Code session: accept one connection and report what arrives on it.
async fn fake_session(
    socket: &Path,
) -> Result<(mpsc::Receiver<Value>, tokio::task::JoinHandle<()>)> {
    let listener = UnixListener::bind(socket)?;
    let (tx, rx) = mpsc::channel(8);
    let handle = tokio::spawn(async move {
        while let Ok((conn, _)) = listener
            .accept()
            .await
        {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(conn).lines();
                while let Ok(Some(line)) = lines
                    .next_line()
                    .await
                {
                    if let Ok(value) = serde_json::from_str::<Value>(&line) {
                        let _ = tx
                            .send(value)
                            .await;
                    }
                }
            });
        }
    });
    Ok((rx, handle))
}

/// Start a relay on `near`, whose far end runs against `far`'s registry.
fn spawn_link(near: &Machine, far: &Machine) -> Result<Child> {
    let exe = env!("CARGO_BIN_EXE_cc-link");
    let transport = format!(
        "HOME={} XDG_RUNTIME_DIR={} {exe} serve",
        far.home
            .display(),
        far.runtime
            .display()
    );
    Command::new(exe)
        .arg("connect")
        .arg(&far.name)
        .arg("--session")
        .arg(&far.session_name)
        .arg("--local-session")
        .arg(std::process::id().to_string())
        .arg("--transport-command")
        .arg(transport)
        .env("HOME", &near.home)
        .env("XDG_RUNTIME_DIR", &near.runtime)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .context("starting the relay")
}

#[tokio::test]
async fn a_message_crosses_the_link_with_its_reply_address_rewritten() -> Result<()> {
    let near = Machine::new("n")?;
    let far = Machine::new("f")?;
    near.publish_session()?;
    far.publish_session()?;

    let (_near_messages, _near_task) = fake_session(&near.session_socket).await?;
    let (mut far_messages, _far_task) = fake_session(&far.session_socket).await?;

    let mut relay = spawn_link(&near, &far)?;
    let mirror = wait_for_mirror(&near, &mut relay).await?;
    let far_mirror = wait_for_mirror(&far, &mut relay).await?;

    // Both ends publish, because the receiving side reads the sender's identity from the kernel and
    // would otherwise see a process with no record at all.
    assert_eq!(
        mirror["name"],
        json!(format!("{}~{}", far.name, far.session_name))
    );
    assert!(far_mirror["ccLinkStub"] == json!(true));

    let mirror_socket = mirror["messagingSocketPath"]
        .as_str()
        .ok_or_else(|| anyhow!("the mirror has no socket"))?;
    let mut conn = UnixStream::connect(mirror_socket).await?;
    let sent = json!({
        "type": "user",
        "msg_id": "the-identifier",
        "from": format!("uds:{}", near.session_socket.display()),
        "text": "across",
    });
    conn.write_all(format!("{sent}\n").as_bytes())
        .await?;
    conn.flush()
        .await?;

    let received = tokio::time::timeout(Duration::from_secs(10), far_messages.recv())
        .await
        .context("the far session never saw the message")?
        .ok_or_else(|| anyhow!("the far session stopped listening"))?;

    assert_eq!(received["text"], json!("across"));
    assert_eq!(received["msg_id"], json!("the-identifier"));
    let reply_to = received["from"]
        .as_str()
        .unwrap_or_default();
    assert_eq!(
        reply_to,
        format!(
            "uds:{}",
            far_mirror["messagingSocketPath"]
                .as_str()
                .unwrap_or_default()
        ),
        "a reply must go to the relay on the receiving machine, not to a path that only exists on the sending one"
    );

    // Ending the relay must take its artifacts with it: a mirror left behind is a peer someone
    // sends to.
    let pid = relay
        .id()
        .ok_or_else(|| anyhow!("the relay is gone"))?;
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGTERM,
    )?;
    relay
        .wait()
        .await?;
    assert!(
        near.mirrors()?
            .is_empty(),
        "the mirror survived the relay"
    );
    assert!(
        !Path::new(mirror_socket).exists(),
        "the socket survived the relay"
    );
    Ok(())
}

#[tokio::test]
async fn a_status_change_reaches_the_other_machine() -> Result<()> {
    let near = Machine::new("sn")?;
    let far = Machine::new("sf")?;
    near.publish_session()?;
    far.publish_session()?;
    let (_near_messages, _near_task) = fake_session(&near.session_socket).await?;
    let (_far_messages, _far_task) = fake_session(&far.session_socket).await?;

    let mut relay = spawn_link(&near, &far)?;
    wait_for_mirror(&near, &mut relay).await?;

    far.set_status("compacting")?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let mirror = near
            .mirrors()?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("the mirror vanished"))?;
        if mirror["status"] == json!("compacting") {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("the mirror still reports {}", mirror["status"]);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Wait for a relay to publish its mirror, failing loudly if the relay dies first.
async fn wait_for_mirror(machine: &Machine, relay: &mut Child) -> Result<Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(mirror) = machine
            .mirrors()?
            .into_iter()
            .next()
        {
            return Ok(mirror);
        }
        if let Some(status) = relay.try_wait()? {
            bail!("the relay exited with {status} before publishing a mirror");
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "no mirror appeared in {}",
                machine
                    .sessions_dir()
                    .display()
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn proc_start(pid: u32) -> Result<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let tail = &stat[stat
        .rfind(')')
        .ok_or_else(|| anyhow!("no comm field"))?
        + 1..];
    Ok(tail
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| anyhow!("no start time"))?
        .to_owned())
}

fn pid_domain() -> Result<String> {
    let machine_id = std::fs::read_to_string("/etc/machine-id")?
        .trim()
        .to_owned();
    let ns = std::fs::read_link("/proc/self/ns/pid")?
        .to_string_lossy()
        .into_owned();
    Ok(format!("linux:{machine_id}:{ns}"))
}

fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(input.as_bytes()))
}
