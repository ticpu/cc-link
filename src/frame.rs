//! The line proxy.
//!
//! Messages are newline-delimited JSON, and each one carries the sender's socket path as the
//! address to reply to. That address only exists on the machine that sent it, while the receiver
//! accepts any socket sitting in its own socket directory — a path that is identical on both
//! machines for the same account — so an unrewritten address is accepted and points replies at a
//! local socket that is absent or belongs to an unrelated session.

use std::path::Path;

use anyhow::{bail, Context, Result};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::warn;

/// Longest line to hand a receiver.
///
/// The real limit is a constant of the receiver's build with no compatibility promise; this is a
/// conservative floor. It matters because rewriting can lengthen a line — the two socket paths
/// differ by however many digits their pids have — so a message that fits before the rewrite can
/// stop fitting after it.
pub const MAX_LINE_BYTES: usize = 512 * 1024;

/// Prefix an address carries before the socket path.
const ADDRESS_SCHEME: &str = "uds:";

/// Field naming the socket a reply should go to.
const REPLY_ADDRESS_FIELD: &str = "from";

/// Replace the reply address in one message with the relay's own socket on this side.
///
/// Identifiers are untouched: delivery receipts refer back to them, and rewriting one breaks a
/// receipt silently.
pub fn rewrite_reply_address(line: &str, socket: &Path) -> Result<String> {
    let mut value: Value = serde_json::from_str(line).context("message is not JSON")?;
    let Some(obj) = value.as_object_mut() else {
        bail!("message is not a JSON object");
    };
    if let Some(address) = obj
        .get(REPLY_ADDRESS_FIELD)
        .and_then(Value::as_str)
    {
        if !address.starts_with(ADDRESS_SCHEME) {
            warn!(
                address,
                "reply address is not a socket address; leaving it alone"
            );
        } else {
            let rewritten = format!("{ADDRESS_SCHEME}{}", socket.display());
            obj.insert(REPLY_ADDRESS_FIELD.into(), Value::from(rewritten));
        }
    }
    let out = serde_json::to_string(&value)?;
    if out.len() > MAX_LINE_BYTES {
        bail!(
            "message is {} bytes after rewriting the reply address, over the {MAX_LINE_BYTES} byte limit",
            out.len()
        );
    }
    Ok(out)
}

/// Copy messages from one side of a relayed connection to the other, rewriting each reply address.
///
/// A message that does not end in a newline before the sender closes is still a message, so the
/// trailing partial line is forwarded rather than discarded.
pub async fn relay_lines<R, W>(reader: R, mut writer: W, socket: &Path) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut lines = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        let read = lines
            .read_line(&mut line)
            .await
            .context("reading a message")?;
        if read == 0 {
            break;
        }
        let trimmed = line.trim_end_matches('\n');
        if trimmed.is_empty() {
            continue;
        }
        let rewritten = rewrite_reply_address(trimmed, socket)?;
        writer
            .write_all(rewritten.as_bytes())
            .await?;
        writer
            .write_all(b"\n")
            .await?;
        writer
            .flush()
            .await?;
    }
    writer
        .shutdown()
        .await
        .context("closing the write side")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn the_reply_address_is_replaced_and_identifiers_are_not() {
        let line = r#"{"type":"user","msg_id":"abc","orig_msg_id":"def","from":"uds:/run/user/1000/cc-socks/222.sock","text":"hi"}"#;
        let out = rewrite_reply_address(line, &PathBuf::from("/run/user/1000/cc-socks/333.sock"))
            .unwrap();
        let value: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["from"], "uds:/run/user/1000/cc-socks/333.sock");
        assert_eq!(value["msg_id"], "abc");
        assert_eq!(value["orig_msg_id"], "def");
        assert_eq!(value["text"], "hi");
    }

    #[test]
    fn a_message_without_a_reply_address_passes_through() {
        let line = r#"{"type":"control","action":"rename"}"#;
        let out = rewrite_reply_address(line, &PathBuf::from("/run/user/1000/cc-socks/333.sock"))
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&out).unwrap()["action"],
            "rename"
        );
    }

    #[test]
    fn a_rewrite_that_pushes_a_message_over_the_limit_is_refused() {
        let filler = "x".repeat(MAX_LINE_BYTES - 60);
        let line = format!(r#"{{"from":"uds:/a","text":"{filler}"}}"#);
        assert!(line.len() <= MAX_LINE_BYTES);
        let long_socket =
            PathBuf::from(format!("/run/user/1000/cc-socks/{}.sock", "9".repeat(200)));
        let err = rewrite_reply_address(&line, &long_socket).unwrap_err();
        assert!(err
            .to_string()
            .contains("over the"));
    }

    #[test]
    fn a_message_that_is_not_json_is_an_error_rather_than_forwarded_blind() {
        assert!(rewrite_reply_address("not json", &PathBuf::from("/s")).is_err());
    }

    #[tokio::test]
    async fn a_trailing_message_without_a_newline_is_still_forwarded() {
        let input = r#"{"from":"uds:/a","msg_id":"1"}"#.as_bytes();
        let mut output = Vec::new();
        relay_lines(
            input,
            &mut output,
            &PathBuf::from("/run/user/1000/cc-socks/333.sock"),
        )
        .await
        .unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.ends_with('\n'));
        let value: Value = serde_json::from_str(text.trim_end()).unwrap();
        assert_eq!(value["from"], "uds:/run/user/1000/cc-socks/333.sock");
        assert_eq!(value["msg_id"], "1");
    }
}
