//! The glue: incoming Signal events → LLM plan / reactions → DB → replies + board.

use crate::board;
use crate::db::{Db, Person, STATUSES, TaskPatch};
use crate::llm::{Action, Interpreter, Plan};
use crate::signal::{Incoming, Kind, Messenger, SendOpts};
use anyhow::Result;
use chrono::{Local, NaiveDate, Timelike};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

const RECENT_LINES: usize = 10;
const BOARD_EDIT_WINDOW_MS: i64 = 23 * 3600 * 1000; // Signal allows edits for 24h
const BOARD_MAX_EDITS: u32 = 9; // stay under Signal's edit limit, then repost

pub struct Bot<M, L> {
    pub db: Arc<Db>,
    messenger: Arc<M>,
    llm: L,
    group_id: Option<String>,
    web_url: Option<String>,
    recent: Mutex<VecDeque<String>>,
    board_lock: tokio::sync::Mutex<()>,
}

enum Change {
    Created(i64),
    Updated(i64),
}

fn today() -> NaiveDate {
    Local::now().date_naive()
}

/// "none"/"" → clear (Some(None)); anything else → set.
fn clearable(v: &Option<String>) -> Option<Option<String>> {
    v.as_ref().map(|s| {
        let t = s.trim();
        if t.is_empty() || t.eq_ignore_ascii_case("none") || t.eq_ignore_ascii_case("null") {
            None
        } else {
            Some(t.to_string())
        }
    })
}

impl<M: Messenger, L: Interpreter> Bot<M, L> {
    pub fn new(db: Arc<Db>, messenger: Arc<M>, llm: L, group_id: Option<String>, web_url: Option<String>) -> Self {
        Self {
            db,
            messenger,
            llm,
            group_id,
            web_url,
            recent: Mutex::new(VecDeque::new()),
            board_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// The group we work in: configured, or the last one we heard from.
    fn group(&self) -> Option<String> {
        self.group_id.clone().or_else(|| self.db.get("group_id").ok().flatten())
    }

    pub async fn handle(&self, inc: Incoming) -> Result<()> {
        let Some(gid) = inc.group_id.clone() else {
            log::info!("ignoring direct message from {} ({})", inc.source_name, inc.source_uuid);
            return Ok(());
        };
        match &self.group_id {
            Some(want) if want != &gid => {
                log::debug!("ignoring message in other group {gid}");
                return Ok(());
            }
            None => log::warn!("GROUP_ID not set; accepting group {gid} — set GROUP_ID={gid} to lock it"),
            _ => {}
        }
        self.db.set("group_id", &gid)?;
        let person = self.db.upsert_person(&inc.source_uuid, &inc.source_name)?;

        match inc.kind {
            Kind::Reaction { emoji, target_ts, remove } => {
                self.on_reaction(&gid, &person, &emoji, target_ts, remove).await
            }
            Kind::Text { text, quote } => self.on_text(&gid, &person, inc.timestamp, &text, quote).await,
        }
    }

    // ------------------------------------------------------------ text

    async fn on_text(
        &self,
        gid: &str,
        who: &Person,
        ts: i64,
        text: &str,
        quote: Option<(i64, String)>,
    ) -> Result<()> {
        let cmd = text.trim().to_lowercase();
        if cmd == "/board" || cmd == "/bord" {
            return self.refresh_board(gid, true).await;
        }
        if cmd == "/help" {
            let help = "Praat gewoon: \"Sem doet dinsdag de goot west\" of \"#4 is klaar\".\n\
                        Reageer op een taakbericht met 🙋 (ik doe 'm), 🔨 (bezig), ⏸️ (wacht), ✅ (klaar).\n\
                        /bord — nieuw overzicht";
            self.messenger.send_group(gid, help, SendOpts::default()).await?;
            return Ok(());
        }

        let context = self.context(who, quote.as_ref())?;
        let line = format!("{}: {}", who.name, text);
        {
            let mut r = self.recent.lock().unwrap();
            r.push_back(line.clone());
            while r.len() > RECENT_LINES {
                r.pop_front();
            }
        }

        let plan = match self.llm.interpret(&context, &line).await {
            Ok(p) => p,
            Err(e) => {
                log::error!("LLM failed: {e:#}");
                return Ok(());
            }
        };
        if plan.actions.is_empty() && plan.reply.is_none() {
            return Ok(());
        }
        log::info!("plan for {:?}: {:?}", line, plan);
        self.db.log(None, Some(who.id), "message", text)?;
        self.execute(gid, who, ts, plan).await
    }

    async fn execute(&self, gid: &str, who: &Person, msg_ts: i64, plan: Plan) -> Result<()> {
        let mut changes = Vec::new();
        let mut problems = Vec::new();
        let mut show_board = false;

        for action in plan.actions {
            match self.apply(who, &action, &mut problems) {
                Ok(Some(c)) => changes.push(c),
                Ok(None) => {}
                Err(e) => problems.push(format!("{e:#}")),
            }
            if action == Action::ShowBoard {
                show_board = true;
            }
        }

        let quote = || SendOpts { quote: Some((msg_ts, who.uuid.clone())), ..Default::default() };
        for c in &changes {
            let (prefix, id) = match c {
                Change::Created(id) => ("🆕", *id),
                Change::Updated(id) => ("✏️", *id),
            };
            let Some(t) = self.db.task(id)? else { continue };
            let msg = format!("{prefix} {}", board::task_line(&t, today(), true));
            let sent = self.messenger.send_group(gid, &msg, quote()).await?;
            self.db.link_message(sent, id)?;
            self.db.link_message(msg_ts, id)?;
        }

        let mut reply = plan.reply.unwrap_or_default();
        if !problems.is_empty() {
            if !reply.is_empty() {
                reply.push('\n');
            }
            reply.push_str(&format!("⚠️ {}", problems.join("\n⚠️ ")));
        }
        if !reply.trim().is_empty() {
            self.messenger.send_group(gid, reply.trim(), quote()).await?;
        }

        if show_board {
            self.refresh_board(gid, true).await?;
        } else if !changes.is_empty() {
            self.refresh_board(gid, false).await?;
        }
        Ok(())
    }

    fn resolve_assignee(&self, name: &str, problems: &mut Vec<String>) -> Result<Option<i64>> {
        match self.db.find_person(name)? {
            Some(p) => Ok(Some(p.id)),
            None => {
                problems.push(format!("Ik ken \"{name}\" niet (nog nooit iets gezegd in de groep?)"));
                Ok(None)
            }
        }
    }

    fn valid_date(&self, d: &str, problems: &mut Vec<String>) -> Option<String> {
        match NaiveDate::parse_from_str(d, "%Y-%m-%d") {
            Ok(_) => Some(d.to_string()),
            Err(_) => {
                problems.push(format!("Datum \"{d}\" snap ik niet"));
                None
            }
        }
    }

    fn apply(&self, who: &Person, action: &Action, problems: &mut Vec<String>) -> Result<Option<Change>> {
        match action {
            Action::CreateProject { name, notes } => {
                let id = self.db.project_id(name)?;
                if let Some(n) = notes {
                    self.db.set_project_notes(id, n)?;
                }
                self.db.log(None, Some(who.id), "create_project", name)?;
                Ok(None)
            }
            Action::ShowBoard => Ok(None),
            Action::CreateTask { title, project, assignee, due, notes, status } => {
                let project_id = clearable(project).flatten().map(|p| self.db.project_id(&p)).transpose()?;
                let assignee_id = match clearable(assignee).flatten() {
                    Some(a) => self.resolve_assignee(&a, problems)?,
                    None => None,
                };
                let due = clearable(due).flatten().and_then(|d| self.valid_date(&d, problems));
                let id = self.db.create_task(
                    title.trim(),
                    project_id,
                    assignee_id,
                    due.as_deref(),
                    notes.as_deref(),
                    Some(who.id),
                )?;
                if let Some(s) = status.as_deref().filter(|s| *s != "todo" && STATUSES.contains(s)) {
                    self.db.update_task(id, &TaskPatch { status: Some(s.into()), ..Default::default() })?;
                }
                self.db.log(Some(id), Some(who.id), "create", &format!("{action:?}"))?;
                Ok(Some(Change::Created(id)))
            }
            Action::UpdateTask { id, title, project, assignee, due, notes, status } => {
                if self.db.task(*id)?.is_none() {
                    problems.push(format!("Taak #{id} bestaat niet"));
                    return Ok(None);
                }
                let mut p = TaskPatch {
                    title: title.clone().filter(|t| !t.trim().is_empty()),
                    notes: clearable(notes),
                    ..Default::default()
                };
                if let Some(s) = status {
                    if STATUSES.contains(&s.as_str()) {
                        p.status = Some(s.clone());
                    } else {
                        problems.push(format!("Onbekende status \"{s}\""));
                    }
                }
                if let Some(proj) = clearable(project) {
                    p.project_id = Some(proj.map(|n| self.db.project_id(&n)).transpose()?);
                }
                if let Some(a) = clearable(assignee) {
                    match a {
                        None => p.assignee_id = Some(None),
                        Some(name) => {
                            if let Some(pid) = self.resolve_assignee(&name, problems)? {
                                p.assignee_id = Some(Some(pid));
                            }
                        }
                    }
                }
                if let Some(d) = clearable(due) {
                    match d {
                        None => p.due_date = Some(None),
                        Some(d) => {
                            if let Some(d) = self.valid_date(&d, problems) {
                                p.due_date = Some(Some(d));
                            }
                        }
                    }
                }
                if p.is_empty() {
                    return Ok(None);
                }
                self.db.update_task(*id, &p)?;
                self.db.log(Some(*id), Some(who.id), "update", &format!("{action:?}"))?;
                Ok(Some(Change::Updated(*id)))
            }
        }
    }

    // ------------------------------------------------------------ reactions

    async fn on_reaction(&self, gid: &str, who: &Person, emoji: &str, target_ts: i64, remove: bool) -> Result<()> {
        let tasks = self.db.tasks_for_message(target_ts)?;
        if tasks.is_empty() {
            return Ok(());
        }
        let first = emoji.chars().next().unwrap_or(' ');
        let status_for = match first {
            '🔨' => Some("doing"),
            '⏸' => Some("waiting"),
            '✅' | '✔' => Some("done"),
            _ => None,
        };
        let mut changed = false;
        for id in tasks {
            let Some(t) = self.db.task(id)? else { continue };
            let mut p = TaskPatch::default();
            if first == '🙋' {
                if !remove {
                    p.assignee_id = Some(Some(who.id));
                } else if t.assignee.as_deref() == Some(who.name.as_str()) {
                    p.assignee_id = Some(None);
                }
            } else if let Some(s) = status_for {
                if !remove {
                    p.status = Some(s.into());
                    if t.assignee.is_none() && s != "waiting" {
                        // Whoever starts or finishes it evidently did it.
                        p.assignee_id = Some(Some(who.id));
                    }
                } else if t.status == s {
                    p.status = Some(if s == "done" { "doing" } else { "todo" }.into());
                }
            }
            if !p.is_empty() {
                self.db.update_task(id, &p)?;
                self.db.log(Some(id), Some(who.id), "reaction", &format!("{emoji} remove={remove}"))?;
                changed = true;
            }
        }
        if changed {
            self.refresh_board(gid, false).await?;
        }
        Ok(())
    }

    // ------------------------------------------------------------ board

    /// Edit the current board message, or post a new one when forced / too old / too often edited.
    pub async fn refresh_board(&self, gid: &str, force_new: bool) -> Result<()> {
        let _guard = self.board_lock.lock().await;
        let text = board::render(&self.db.live_tasks(7)?, today(), self.web_url.as_deref());
        let now = chrono::Utc::now().timestamp_millis();
        let board_ts: Option<i64> = self.db.get("board_ts")?.and_then(|s| s.parse().ok());
        let edits: u32 = self.db.get("board_edits")?.and_then(|s| s.parse().ok()).unwrap_or(0);

        if !force_new
            && let Some(ts) = board_ts
            && now - ts < BOARD_EDIT_WINDOW_MS
            && edits < BOARD_MAX_EDITS
        {
            if self.db.get("board_text")?.as_deref() == Some(text.as_str()) {
                return Ok(());
            }
            let opts = SendOpts { edit_ts: Some(ts), ..Default::default() };
            match self.messenger.send_group(gid, &text, opts).await {
                Ok(_) => {
                    self.db.set("board_edits", &(edits + 1).to_string())?;
                    self.db.set("board_text", &text)?;
                    return Ok(());
                }
                Err(e) => log::warn!("board edit failed, reposting: {e:#}"),
            }
        }
        let ts = self.messenger.send_group(gid, &text, SendOpts::default()).await?;
        self.db.set("board_ts", &ts.to_string())?;
        self.db.set("board_edits", "0")?;
        self.db.set("board_text", &text)?;
        Ok(())
    }

    /// Post a fresh board once a day at `hour` (local time).
    pub async fn daily_loop(self: Arc<Self>, hour: u32) {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            let now = Local::now();
            let date = now.date_naive().to_string();
            if now.hour() < hour || self.db.get("board_daily").ok().flatten().as_deref() == Some(date.as_str()) {
                continue;
            }
            let Some(gid) = self.group() else { continue };
            match self.refresh_board(&gid, true).await {
                Ok(()) => {
                    let _ = self.db.set("board_daily", &date);
                }
                Err(e) => log::error!("daily board failed: {e:#}"),
            }
        }
    }

    // ------------------------------------------------------------ LLM context

    fn context(&self, who: &Person, quote: Option<&(i64, String)>) -> Result<String> {
        let d = today();
        let mut s = format!("Today: {} ({})\nSender: {}\n", d, d.format("%A"), who.name);
        let people: Vec<String> = self.db.people()?.into_iter().map(|p| p.name).collect();
        s.push_str(&format!("People: {}\n", people.join(", ")));
        s.push_str(&format!("Projects: {}\n", self.db.project_names()?.join(", ")));
        s.push_str("Tasks:\n");
        let tasks = self.db.live_tasks(3)?;
        if tasks.is_empty() {
            s.push_str("(none)\n");
        }
        for t in tasks {
            s.push_str(&format!("#{} [{}] {}", t.id, t.status, t.title));
            for (k, v) in [("project", &t.project), ("assignee", &t.assignee), ("due", &t.due_date), ("notes", &t.notes)] {
                if let Some(v) = v {
                    s.push_str(&format!(" | {k}: {v}"));
                }
            }
            s.push('\n');
        }
        let recent = self.recent.lock().unwrap();
        if !recent.is_empty() {
            s.push_str("Recent chat (oldest first):\n");
            for l in recent.iter() {
                s.push_str(l);
                s.push('\n');
            }
        }
        if let Some((qts, qtext)) = quote {
            let ids = self.db.tasks_for_message(*qts)?;
            s.push_str(&format!("The new message is a reply to: \"{qtext}\""));
            if !ids.is_empty() {
                let ids: Vec<String> = ids.iter().map(|i| format!("#{i}")).collect();
                s.push_str(&format!(" (that message is about task {})", ids.join(", ")));
            }
            s.push('\n');
        }
        Ok(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::parse_plan;
    use std::sync::atomic::{AtomicI64, Ordering};

    /// Hands out realistic timestamps: T0, T0+1, ...
    struct FakeSignal {
        sent: Mutex<Vec<(String, SendOpts)>>,
        ts: AtomicI64,
    }
    impl Default for FakeSignal {
        fn default() -> Self {
            Self { sent: Mutex::default(), ts: AtomicI64::new(chrono::Utc::now().timestamp_millis()) }
        }
    }
    impl Messenger for FakeSignal {
        async fn send_group(&self, _g: &str, text: &str, opts: SendOpts) -> Result<i64> {
            self.sent.lock().unwrap().push((text.to_string(), opts));
            Ok(self.ts.fetch_add(1, Ordering::Relaxed))
        }
    }

    /// Returns canned plans in order.
    struct FakeLlm(Mutex<VecDeque<&'static str>>);
    impl Interpreter for FakeLlm {
        async fn interpret(&self, _c: &str, _m: &str) -> Result<Plan> {
            parse_plan(self.0.lock().unwrap().pop_front().unwrap_or(r#"{"actions":[]}"#))
        }
    }

    fn text(uuid: &str, name: &str, ts: i64, t: &str) -> Incoming {
        Incoming {
            source_uuid: uuid.into(),
            source_name: name.into(),
            timestamp: ts,
            group_id: Some("G".into()),
            kind: Kind::Text { text: t.into(), quote: None },
        }
    }

    #[tokio::test]
    async fn create_then_react() {
        let sig = Arc::new(FakeSignal::default());
        let llm = FakeLlm(Mutex::new(VecDeque::from([
            r#"{"actions":[]}"#,
            r#"{"actions":[{"op":"create_task","title":"Goot west","project":"Dakkapel","assignee":"Sem","due":"2026-10-20"}]}"#,
        ])));
        let bot = Bot::new(Arc::new(Db::open(":memory:").unwrap()), sig.clone(), llm, Some("G".into()), None);

        bot.handle(text("u2", "Sem", 1, "hoi")).await.unwrap();
        assert!(sig.sent.lock().unwrap().is_empty(), "chit-chat stays silent");

        bot.handle(text("u1", "Harm", 2, "Sem doet dinsdag de goot west")).await.unwrap();
        let (confirm_ts, board_ts) = {
            let first = bot.db.tasks_for_message(2).unwrap();
            assert_eq!(first, vec![1]);
            let b: i64 = bot.db.get("board_ts").unwrap().unwrap().parse().unwrap();
            (b - 1, b)
        };
        {
            let sent = sig.sent.lock().unwrap();
            assert_eq!(sent.len(), 2, "{sent:?}"); // confirmation + new board
            assert!(sent[0].0.starts_with("🆕 ⬜ #1 Goot west · 📁 Dakkapel · Sem · 📅 di 20/10"));
            assert_eq!(sent[0].1.quote, Some((2, "u1".into())));
            assert!(sent[1].0.starts_with("📋 Bord"));
        }

        // Harm reacts ✅ on the bot's confirmation → done, board edited.
        bot.handle(Incoming {
            kind: Kind::Reaction { emoji: "✅".into(), target_ts: confirm_ts, remove: false },
            ..text("u1", "Harm", 3, "")
        })
        .await
        .unwrap();
        assert_eq!(bot.db.task(1).unwrap().unwrap().status, "done");
        let sent = sig.sent.lock().unwrap();
        assert_eq!(sent.len(), 3);
        assert_eq!(sent[2].1.edit_ts, Some(board_ts), "board is edited in place");
        assert!(sent[2].0.contains("✅ 1 klaar"));
    }

    #[tokio::test]
    async fn claim_with_raised_hand_and_unknown_person() {
        let sig = Arc::new(FakeSignal::default());
        let llm = FakeLlm(Mutex::new(VecDeque::from([
            r#"{"actions":[{"op":"create_task","title":"Steiger huren","assignee":"Piet"}]}"#,
        ])));
        let bot = Bot::new(Arc::new(Db::open(":memory:").unwrap()), sig.clone(), llm, Some("G".into()), None);
        bot.handle(text("u1", "Harm", 5, "Piet regelt de steiger")).await.unwrap();
        {
            let sent = sig.sent.lock().unwrap();
            assert!(sent.iter().any(|s| s.0.contains("⚠️ Ik ken \"Piet\" niet")), "{sent:?}");
        }
        // Sem claims it by reacting 🙋‍♂️ on Harm's original message.
        bot.handle(Incoming {
            kind: Kind::Reaction { emoji: "🙋‍♂️".into(), target_ts: 5, remove: false },
            ..text("u2", "Sem", 6, "")
        })
        .await
        .unwrap();
        assert_eq!(bot.db.task(1).unwrap().unwrap().assignee.as_deref(), Some("Sem"));
    }

    #[tokio::test]
    async fn other_groups_and_dms_ignored() {
        let sig = Arc::new(FakeSignal::default());
        let bot = Bot::new(Arc::new(Db::open(":memory:").unwrap()), sig.clone(), FakeLlm(Mutex::new(VecDeque::new())), Some("G".into()), None);
        let mut m = text("u1", "Harm", 1, "/bord");
        m.group_id = Some("OTHER".into());
        bot.handle(m.clone()).await.unwrap();
        m.group_id = None;
        bot.handle(m).await.unwrap();
        assert!(sig.sent.lock().unwrap().is_empty());
    }
}
