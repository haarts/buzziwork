//! Turns a chat message into a plan of task operations via OpenRouter.

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Default, Deserialize, PartialEq)]
pub struct Plan {
    /// Text to post in the group, if any. Null for most messages.
    #[serde(default)]
    pub reply: Option<String>,
    #[serde(default)]
    pub actions: Vec<Action>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Action {
    CreateTask {
        title: String,
        #[serde(default)]
        project: Option<String>,
        #[serde(default)]
        assignee: Option<String>,
        #[serde(default)]
        due: Option<String>,
        #[serde(default)]
        notes: Option<String>,
        #[serde(default)]
        status: Option<String>,
    },
    UpdateTask {
        id: i64,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        project: Option<String>,
        #[serde(default)]
        assignee: Option<String>,
        #[serde(default)]
        due: Option<String>,
        #[serde(default)]
        notes: Option<String>,
        #[serde(default)]
        status: Option<String>,
    },
    CreateProject {
        name: String,
        #[serde(default)]
        notes: Option<String>,
    },
    ShowBoard,
}

pub trait Interpreter: Send + Sync {
    fn interpret(&self, context: &str, message: &str) -> impl Future<Output = Result<Plan>> + Send;
}

pub const SYSTEM_PROMPT: &str = r#"You are the task-keeping bot in a Signal group of a small renovation crew (they mostly write Dutch, sometimes English).
You read every message. Most messages are ordinary chat: for those return {"reply": null, "actions": []}.
Only act when a message clearly creates, assigns, reschedules, completes or asks about work.

Return ONLY a JSON object, no prose, no code fences:
{
  "reply": string or null,
  "actions": [ ...zero or more of:
    {"op":"create_task","title":"...","project":"...|null","assignee":"<person name>|null","due":"YYYY-MM-DD|null","notes":"...|null","status":"todo|doing|waiting|done|null"},
    {"op":"update_task","id":<task id>,"title"?:"...","project"?:"...","assignee"?:"<name>|none","due"?:"YYYY-MM-DD|none","notes"?:"...","status"?:"todo|doing|waiting|done|dropped"},
    {"op":"create_project","name":"...","notes":"...|null"},
    {"op":"show_board"}
  ]
}

Rules:
- Task titles: short imperative, in the language of the message (e.g. "Zink dakkapel links vervangen").
- "ik"/"I"/"me" means the sender. Only use people names from the people list; unknown person -> leave assignee null and mention it in reply.
- Resolve relative dates ("dinsdag", "morgen", "volgende week") against today's date given below. Never invent a date that was not said.
- Reuse an existing project name when it fits (case-insensitive, synonyms like dakkapel/dormer). Only put a project on a task when it clearly belongs to one.
- To change an existing task use update_task with its #id from the task list. Omit fields you do not change. "dropped" = cancelled.
- If several tasks are mentioned in one message, emit several actions.
- Set "reply" only when someone asks a question (e.g. "wat moet Sem nog doen?", "wat staat er voor morgen?") — answer briefly from the task list — or when you need to flag a problem (ambiguous task, unknown person). Do NOT reply just to confirm; the bot confirms changes itself.
- Reply in the language of the message. Keep it short; plain text, no markdown."#;

pub struct OpenRouter {
    http: reqwest::Client,
    base_url: String,
    key: String,
    model: String,
}

impl OpenRouter {
    pub fn new(base_url: String, key: String, model: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url,
            key,
            model,
        }
    }
}

impl Interpreter for OpenRouter {
    async fn interpret(&self, context: &str, message: &str) -> Result<Plan> {
        let body = json!({
            "model": self.model,
            "temperature": 0,
            "max_tokens": 1500,
            "messages": [
                {"role": "system", "content": SYSTEM_PROMPT},
                {"role": "user", "content": format!("{context}\n\n=== NEW MESSAGE ===\n{message}")},
            ],
        });
        let resp = self
            .http
            .post(format!("{}/chat/completions", self.base_url.trim_end_matches('/')))
            .bearer_auth(&self.key)
            .header("X-Title", "buzziwork")
            .json(&body)
            .timeout(std::time::Duration::from_secs(60))
            .send()
            .await
            .context("calling OpenRouter")?;
        let status = resp.status();
        let v: Value = resp.json().await.context("OpenRouter response not JSON")?;
        if !status.is_success() {
            return Err(anyhow!("OpenRouter {status}: {v}"));
        }
        let content = v["choices"][0]["message"]["content"]
            .as_str()
            .ok_or_else(|| anyhow!("no content in OpenRouter response: {v}"))?;
        parse_plan(content)
    }
}

/// Lenient: tolerates code fences or chatter around the JSON object.
pub fn parse_plan(s: &str) -> Result<Plan> {
    let start = s.find('{').ok_or_else(|| anyhow!("no JSON in model output: {s}"))?;
    let end = s.rfind('}').ok_or_else(|| anyhow!("no JSON in model output: {s}"))?;
    let plan: Plan = serde_json::from_str(&s[start..=end])
        .with_context(|| format!("bad plan JSON: {s}"))?;
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fenced_plan() {
        let s = "```json\n{\"reply\":null,\"actions\":[{\"op\":\"create_task\",\"title\":\"Goot west\",\"assignee\":\"Sem\",\"due\":\"2026-10-20\"},{\"op\":\"update_task\",\"id\":3,\"status\":\"done\"},{\"op\":\"show_board\"}]}\n```";
        let p = parse_plan(s).unwrap();
        assert_eq!(p.actions.len(), 3);
        assert!(matches!(&p.actions[0], Action::CreateTask { title, assignee: Some(a), .. } if title == "Goot west" && a == "Sem"));
        assert!(matches!(&p.actions[1], Action::UpdateTask { id: 3, status: Some(s), .. } if s == "done"));
        assert_eq!(p.actions[2], Action::ShowBoard);
    }

    #[test]
    fn empty_plan() {
        assert_eq!(parse_plan(r#"{"reply":null,"actions":[]}"#).unwrap(), Plan::default());
    }
}
