//! Transport: one SSH stdio channel, multiplexed.
//!
//! Nothing is bound outside the pipe SSH already owns — no port, no forwarded socket — because the
//! link is a live grant of one session's permissions to another machine and anything listening is
//! a second way in.

use std::path::Path;
use std::process::Stdio;
use std::task::Poll;

use anyhow::{anyhow, bail, Context, Result};
use futures::future::{self, Either};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};
use tracing::debug;

/// Written by both ends before anything else.
///
/// A login shell on the far end that prints a banner would otherwise corrupt the first bytes of the
/// multiplexer, which surfaces as an unreadable protocol rather than as the misconfiguration it is.
const PREAMBLE: &[u8] = b"cc-link/1\n";

/// Version of the control protocol spoken over the link.
pub const CONTROL_PROTOCOL: u32 = 1;

/// What a client wants from the far end.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "intent", rename_all = "snake_case")]
pub enum Intent {
    /// Enumerate the sessions available there.
    List,
    /// Mirror one of them, named because the far end never picks for itself.
    Attach {
        /// Name, pid or identifier of the session to export.
        session: String,
    },
}

/// Messages on the control stream.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Control {
    /// Opening message, carrying enough to refuse a link that should not exist.
    Hello {
        /// Control protocol the sender speaks.
        protocol: u32,
        /// Platform the sender runs on.
        platform: String,
        /// Domain the sender's processes live in.
        pid_domain: String,
        /// Home directory of the account the sender runs as.
        home: String,
        /// Sender's wall clock, so timestamps can be translated.
        clock_ms: u64,
        /// What the sender wants.
        intent: Intent,
        /// Record of the session the sender exports, absent when only listing.
        export: Option<Value>,
    },
    /// Sessions available on the far end.
    Sessions {
        /// Their records.
        sessions: Vec<Value>,
    },
    /// Far end accepted, and exports this session.
    Ready {
        /// Record of the exported session.
        export: Value,
        /// Far end's wall clock.
        clock_ms: u64,
    },
    /// Exported session's record changed.
    Update {
        /// Its current record.
        record: Value,
    },
    /// Link is ending.
    Bye {
        /// Why.
        reason: String,
    },
    /// Link is refused.
    Refused {
        /// Why.
        reason: String,
    },
}

/// A multiplexed stream carrying one relayed connection.
pub type Stream = tokio_util::compat::Compat<yamux::Stream>;

/// Handle on a running multiplexer.
pub struct Mux {
    open: mpsc::Sender<oneshot::Sender<Result<Stream>>>,
    inbound: mpsc::Receiver<Stream>,
}

impl Mux {
    /// Open a stream to the far end.
    pub async fn open(&self) -> Result<Stream> {
        let (tx, rx) = oneshot::channel();
        self.open
            .send(tx)
            .await
            .map_err(|_| anyhow!("the link is closed"))?;
        rx.await
            .map_err(|_| anyhow!("the link is closed"))?
    }

    /// Wait for the far end to open a stream. `None` once the link is closed.
    pub async fn accept(&mut self) -> Option<Stream> {
        self.inbound
            .recv()
            .await
    }
}

/// Speak the preamble, then run a multiplexer over the channel.
///
/// One task owns the connection: outbound streams are opened by polling the same object that
/// accepts inbound ones, so there is nowhere else they can come from.
pub async fn start<T>(mut io: T, client: bool) -> Result<Mux>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    io.write_all(PREAMBLE)
        .await
        .context("writing preamble")?;
    io.flush()
        .await?;
    expect_preamble(&mut io).await?;

    let mode = if client {
        yamux::Mode::Client
    } else {
        yamux::Mode::Server
    };
    let mut conn = yamux::Connection::new(io.compat(), yamux::Config::default(), mode);
    let (open_tx, mut open_rx) = mpsc::channel::<oneshot::Sender<Result<Stream>>>(16);
    let (inbound_tx, inbound_rx) = mpsc::channel::<Stream>(16);

    tokio::spawn(async move {
        let mut pending: Option<oneshot::Sender<Result<Stream>>> = None;
        loop {
            let event = tokio::select! {
                biased;
                request = open_rx.recv(), if pending.is_none() => {
                    match request {
                        Some(request) => { pending = Some(request); continue }
                        None => break,
                    }
                }
                event = future::poll_fn(|cx| {
                    if pending.is_some() {
                        if let Poll::Ready(stream) = std::pin::Pin::new(&mut conn).poll_new_outbound(cx) {
                            return Poll::Ready(Either::Left(stream));
                        }
                    }
                    std::pin::Pin::new(&mut conn).poll_next_inbound(cx).map(Either::Right)
                }) => event,
            };
            match event {
                Either::Left(outbound) => {
                    let reply = pending
                        .take()
                        .expect("an outbound stream nobody asked for");
                    let _ = reply.send(
                        outbound
                            .map(FuturesAsyncReadCompatExt::compat)
                            .map_err(|e| anyhow!("opening a stream: {e}")),
                    );
                }
                Either::Right(Some(Ok(stream))) => {
                    if inbound_tx
                        .send(stream.compat())
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Either::Right(Some(Err(e))) => {
                    debug!(error = %e, "link closed");
                    break;
                }
                Either::Right(None) => break,
            }
        }
    });

    Ok(Mux {
        open: open_tx,
        inbound: inbound_rx,
    })
}

/// Read the far end's preamble, naming anything it wrote ahead of it.
async fn expect_preamble<T: AsyncRead + Unpin>(io: &mut T) -> Result<()> {
    let mut seen = Vec::new();
    let mut byte = [0u8; 1];
    while seen.len() < PREAMBLE.len() + 256 {
        let read = io
            .read(&mut byte)
            .await
            .context("reading preamble")?;
        if read == 0 {
            bail!("the far end closed the link before identifying itself");
        }
        seen.push(byte[0]);
        if seen.ends_with(PREAMBLE) {
            let extra = seen.len() - PREAMBLE.len();
            if extra > 0 {
                bail!("the remote shell wrote {extra} bytes before cc-link started; a login shell that prints on startup will corrupt the link");
            }
            return Ok(());
        }
    }
    bail!("the far end did not identify itself as cc-link")
}

/// Send one control message.
pub async fn send<W: AsyncWrite + Unpin>(writer: &mut W, message: &Control) -> Result<()> {
    let mut line = serde_json::to_vec(message)?;
    line.push(b'\n');
    writer
        .write_all(&line)
        .await
        .context("writing control")?;
    writer
        .flush()
        .await?;
    Ok(())
}

/// Read one control message.
pub async fn recv<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Control> {
    let mut line = String::new();
    let read =
        tokio::io::AsyncBufReadExt::read_line(&mut tokio::io::BufReader::new(reader), &mut line)
            .await
            .context("reading control")?;
    if read == 0 {
        bail!("the link closed");
    }
    serde_json::from_str(&line).with_context(|| format!("parsing control message {line:?}"))
}

/// Start the far end over SSH.
///
/// The child gets its own process group so a signal aimed at the caller's shell does not take down
/// half of a link, and a death signal so it cannot outlive a relay that was killed outright.
pub fn ssh_command(host: &str, remote_bin: &str, options: &[String]) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("ssh");
    command
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ServerAliveInterval=15")
        .arg("-o")
        .arg("ServerAliveCountMax=3");
    for option in options {
        command
            .arg("-o")
            .arg(option);
    }
    command
        .arg(host)
        .arg(remote_bin)
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    unsafe {
        command.pre_exec(|| {
            nix::unistd::setsid().ok();
            nix::sys::prctl::set_pdeathsig(Some(nix::sys::signal::Signal::SIGTERM))
                .map_err(std::io::Error::from)?;
            Ok(())
        });
    }
    command
}

/// Run an arbitrary command as the transport instead of SSH, for tests that need both ends in one
/// process tree.
pub fn transport_command(command_line: &str) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("sh");
    command
        .arg("-c")
        .arg(command_line)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    command
}

/// Whether two ends share a machine, which would make them share one registry.
pub fn same_machine(
    local_domain: &str,
    local_home: &Path,
    remote_domain: &str,
    remote_home: &str,
) -> bool {
    local_domain == remote_domain && local_home.to_string_lossy() == remote_home
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_noisy_remote_shell_is_named_rather_than_left_as_a_parse_error() {
        let mut input: &[u8] = b"Welcome to host\ncc-link/1\n";
        let err = expect_preamble(&mut input)
            .await
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("16 bytes"));
    }

    #[tokio::test]
    async fn a_clean_preamble_is_accepted() {
        let mut input: &[u8] = b"cc-link/1\n";
        expect_preamble(&mut input)
            .await
            .unwrap();
    }

    #[test]
    fn a_link_that_comes_back_to_the_same_machine_is_recognised() {
        assert!(same_machine(
            "linux:a:pid:[1]",
            Path::new("/home/x"),
            "linux:a:pid:[1]",
            "/home/x"
        ));
        assert!(!same_machine(
            "linux:a:pid:[1]",
            Path::new("/home/x"),
            "linux:b:pid:[1]",
            "/home/x"
        ));
    }

    #[test]
    fn control_messages_round_trip() {
        let hello = Control::Hello {
            protocol: CONTROL_PROTOCOL,
            platform: "linux".into(),
            pid_domain: "linux:a:pid:[1]".into(),
            home: "/home/x".into(),
            clock_ms: 1,
            intent: Intent::Attach {
                session: "claude-code-zz".into(),
            },
            export: None,
        };
        let text = serde_json::to_string(&hello).unwrap();
        assert!(matches!(
            serde_json::from_str::<Control>(&text).unwrap(),
            Control::Hello { protocol: 1, .. }
        ));
    }
}
