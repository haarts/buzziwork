//! Thin JSON-RPC client for `signal-cli jsonRpc` over the child's stdin/stdout.
//!
//! signal-cli writes one JSON object per line. Lines with an `id` are responses
//! to our requests; lines with `"method":"receive"` are incoming envelopes.

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, mpsc, oneshot};

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;

pub struct Signal {
    stdin: Mutex<ChildStdin>,
    pending: Pending,
    next_id: AtomicU64,
    _child: Mutex<Child>,
}

/// Options for an outgoing group message.
#[derive(Default, Debug, Clone)]
pub struct SendOpts {
    /// Quote (reply to) this message.
    pub quote: Option<(i64, String)>, // (timestamp, author uuid)
    /// Edit an earlier message of ours instead of sending a new one.
    pub edit_ts: Option<i64>,
}

/// What the bot needs from a messenger. Implemented by [`Signal`] and by test fakes.
pub trait Messenger: Send + Sync {
    /// Send to a group, return the message timestamp (its id in Signal).
    fn send_group(
        &self,
        group_id: &str,
        text: &str,
        opts: SendOpts,
    ) -> impl Future<Output = Result<i64>> + Send;
}

impl Signal {
    /// Spawn `signal-cli [-a ACCOUNT] [--config DIR] jsonRpc`.
    /// Returns the client plus a channel of raw `receive` params.
    pub fn spawn(
        cli: &str,
        account: Option<&str>,
        config_dir: Option<&str>,
    ) -> Result<(Arc<Self>, mpsc::Receiver<Value>)> {
        let mut cmd = Command::new(cli);
        if let Some(dir) = config_dir {
            cmd.arg("--config").arg(dir);
        }
        if let Some(a) = account {
            cmd.arg("-a").arg(a);
        }
        cmd.arg("jsonRpc")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let mut child = cmd.spawn().with_context(|| format!("spawning {cli}"))?;
        let stdin = child.stdin.take().context("no stdin")?;
        let stdout = child.stdout.take().context("no stdout")?;

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = mpsc::channel(256);

        let p = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(v) = serde_json::from_str::<Value>(&line) else {
                    log::warn!("signal-cli: unparseable line: {line}");
                    continue;
                };
                if let Some(id) = v.get("id").and_then(Value::as_u64) {
                    if let Some(waiter) = p.lock().await.remove(&id) {
                        let r = match v.get("error") {
                            Some(e) => Err(anyhow!("signal-cli error: {e}")),
                            None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
                        };
                        let _ = waiter.send(r);
                    }
                } else if v.get("method").and_then(Value::as_str) == Some("receive") {
                    if let Some(params) = v.get("params") {
                        // Subscription mode wraps it in `result`; default mode does not.
                        let params = params.get("result").unwrap_or(params).clone();
                        if tx.send(params).await.is_err() {
                            break;
                        }
                    }
                } else {
                    log::debug!("signal-cli: {line}");
                }
            }
            log::error!("signal-cli stdout closed");
            // Dropping tx closes the receive channel, which ends the main loop.
        });

        Ok((
            Arc::new(Self {
                stdin: Mutex::new(stdin),
                pending,
                next_id: AtomicU64::new(1),
                _child: Mutex::new(child),
            }),
            rx,
        ))
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let mut line = serde_json::to_string(&json!({
            "jsonrpc": "2.0", "method": method, "params": params, "id": id
        }))?;
        line.push('\n');
        {
            let mut stdin = self.stdin.lock().await;
            stdin.write_all(line.as_bytes()).await?;
            stdin.flush().await?;
        }
        match tokio::time::timeout(Duration::from_secs(60), rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(anyhow!("signal-cli went away")),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(anyhow!("signal-cli request {method} timed out"))
            }
        }
    }
}

impl Messenger for Signal {
    async fn send_group(&self, group_id: &str, text: &str, opts: SendOpts) -> Result<i64> {
        let mut params = json!({ "groupId": group_id, "message": text });
        if let Some((ts, author)) = opts.quote {
            params["quoteTimestamp"] = json!(ts);
            params["quoteAuthor"] = json!(author);
        }
        if let Some(ts) = opts.edit_ts {
            params["editTimestamp"] = json!(ts);
        }
        let r = self.request("send", params).await?;
        r.get("timestamp")
            .and_then(Value::as_i64)
            .ok_or_else(|| anyhow!("send returned no timestamp: {r}"))
    }
}

// ---------------------------------------------------------------- incoming

#[derive(Debug, Clone, PartialEq)]
pub struct Incoming {
    pub source_uuid: String,
    pub source_name: String,
    pub timestamp: i64,
    pub group_id: Option<String>,
    pub kind: Kind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Text {
        text: String,
        /// (timestamp, text) of the message being replied to.
        quote: Option<(i64, String)>,
    },
    Reaction {
        emoji: String,
        target_ts: i64,
        remove: bool,
    },
}

/// Turn the params of a `receive` notification into something we care about.
/// Returns None for receipts, typing indicators, sync messages, etc.
pub fn parse_incoming(params: &Value) -> Option<Incoming> {
    let env = params.get("envelope")?;
    let dm = env.get("dataMessage")?;
    let source_uuid = env
        .get("sourceUuid")
        .or_else(|| env.get("source"))?
        .as_str()?
        .to_string();
    let source_name = env
        .get("sourceName")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("?")
        .to_string();
    let timestamp = dm
        .get("timestamp")
        .or_else(|| env.get("timestamp"))?
        .as_i64()?;
    let group_id = dm
        .get("groupInfo")
        .and_then(|g| g.get("groupId"))
        .and_then(Value::as_str)
        .map(String::from);

    let kind = if let Some(r) = dm.get("reaction") {
        Kind::Reaction {
            emoji: r.get("emoji")?.as_str()?.to_string(),
            target_ts: r.get("targetSentTimestamp")?.as_i64()?,
            remove: r.get("isRemove").and_then(Value::as_bool).unwrap_or(false),
        }
    } else {
        let raw = dm.get("message").and_then(Value::as_str)?;
        let text = resolve_mentions(raw, dm.get("mentions"));
        if text.trim().is_empty() {
            return None;
        }
        let quote = dm.get("quote").and_then(|q| {
            Some((
                q.get("id")?.as_i64()?,
                q.get("text").and_then(Value::as_str).unwrap_or("").to_string(),
            ))
        });
        Kind::Text { text, quote }
    };

    Some(Incoming {
        source_uuid,
        source_name,
        timestamp,
        group_id,
        kind,
    })
}

/// Signal puts U+FFFC in the text for every @mention; swap them for `@Name`.
fn resolve_mentions(text: &str, mentions: Option<&Value>) -> String {
    let mut ms: Vec<(i64, String)> = mentions
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|m| {
                    let start = m.get("start").and_then(Value::as_i64).unwrap_or(0);
                    let name = m
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .or_else(|| m.get("number").and_then(Value::as_str))
                        .unwrap_or("someone")
                        .to_string();
                    (start, name)
                })
                .collect()
        })
        .unwrap_or_default();
    ms.sort_by_key(|m| m.0);
    let mut names = ms.into_iter().map(|m| m.1);
    text.chars()
        .map(|c| {
            if c == '\u{FFFC}' {
                format!("@{}", names.next().unwrap_or_else(|| "someone".into()))
            } else {
                c.to_string()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_group_text_with_mention_and_quote() {
        let p = json!({"envelope": {
            "sourceUuid": "u-1", "sourceName": "Harm", "timestamp": 10,
            "dataMessage": {
                "timestamp": 10,
                "message": "\u{FFFC} doe jij de goot?",
                "mentions": [{"name": "Sem", "uuid": "u-2", "start": 0, "length": 1}],
                "groupInfo": {"groupId": "G=", "type": "DELIVER"},
                "quote": {"id": 5, "author": "x", "text": "goot west"}
            }}});
        let i = parse_incoming(&p).unwrap();
        assert_eq!(i.group_id.as_deref(), Some("G="));
        assert_eq!(
            i.kind,
            Kind::Text {
                text: "@Sem doe jij de goot?".into(),
                quote: Some((5, "goot west".into()))
            }
        );
    }

    #[test]
    fn parses_reaction() {
        let p = json!({"envelope": {
            "sourceUuid": "u-1", "timestamp": 11,
            "dataMessage": {"timestamp": 11,
                "reaction": {"emoji": "✅", "targetSentTimestamp": 7, "isRemove": false},
                "groupInfo": {"groupId": "G="}}}});
        let i = parse_incoming(&p).unwrap();
        assert_eq!(
            i.kind,
            Kind::Reaction {
                emoji: "✅".into(),
                target_ts: 7,
                remove: false
            }
        );
    }

    #[test]
    fn ignores_receipts() {
        let p = json!({"envelope": {"sourceUuid": "u", "receiptMessage": {}}});
        assert!(parse_incoming(&p).is_none());
    }
}
