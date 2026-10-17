//! Plain-text rendering of tasks and the board message.

use crate::db::Task;
use chrono::{Datelike, NaiveDate};

const WEEKDAYS: [&str; 7] = ["ma", "di", "wo", "do", "vr", "za", "zo"];

pub fn status_emoji(s: &str) -> &'static str {
    match s {
        "doing" => "🔨",
        "waiting" => "⏸️",
        "done" => "✅",
        "dropped" => "🗑️",
        _ => "⬜",
    }
}

/// "di 20/10", with a warning sign when it's in the past and the task is open.
pub fn fmt_due(due: &str, status: &str, today: NaiveDate) -> String {
    match NaiveDate::parse_from_str(due, "%Y-%m-%d") {
        Ok(d) => {
            let late = d < today && status != "done" && status != "dropped";
            format!(
                "{}📅 {} {}/{}",
                if late { "⚠️" } else { "" },
                WEEKDAYS[d.weekday().num_days_from_monday() as usize],
                d.day(),
                d.month()
            )
        }
        Err(_) => format!("📅 {due}"),
    }
}

/// One line: `🔨 #3 Zink links · Noah · 📅 di 20/10`.
pub fn task_line(t: &Task, today: NaiveDate, with_project: bool) -> String {
    let mut s = format!("{} #{} {}", status_emoji(&t.status), t.id, t.title);
    if with_project && let Some(p) = &t.project {
        s.push_str(&format!(" · 📁 {p}"));
    }
    if let Some(a) = &t.assignee {
        s.push_str(&format!(" · {a}"));
    }
    if let Some(d) = &t.due_date {
        s.push_str(&format!(" · {}", fmt_due(d, &t.status, today)));
    }
    s
}

fn status_rank(s: &str) -> u8 {
    match s {
        "doing" => 0,
        "todo" => 1,
        "waiting" => 2,
        _ => 3,
    }
}

/// The pinned-ish overview message. `tasks` should come from `Db::live_tasks`.
pub fn render(tasks: &[Task], today: NaiveDate, web_url: Option<&str>) -> String {
    let mut out = format!(
        "📋 Bord — {} {}/{}\n",
        WEEKDAYS[today.weekday().num_days_from_monday() as usize],
        today.day(),
        today.month()
    );
    let open: Vec<&Task> = tasks.iter().filter(|t| t.status != "done").collect();
    let done = tasks.len() - open.len();

    if open.is_empty() {
        out.push_str("\nNiks open. 🎉\n");
    }

    // Group by project, keeping the DB order (projects alphabetical, loose tasks last).
    let mut groups: Vec<(Option<&str>, Vec<&Task>)> = Vec::new();
    for t in open {
        let key = t.project.as_deref();
        match groups.iter_mut().find(|g| g.0 == key) {
            Some(g) => g.1.push(t),
            None => groups.push((key, vec![t])),
        }
    }
    for (proj, mut ts) in groups {
        ts.sort_by_key(|t| status_rank(&t.status));
        out.push_str(&format!("\n📁 {}\n", proj.unwrap_or("Los")));
        for t in ts {
            out.push_str(&task_line(t, today, false));
            out.push('\n');
        }
    }
    if done > 0 {
        out.push_str(&format!("\n✅ {done} klaar afgelopen week\n"));
    }
    out.push_str("\n🙋 = ik doe 'm · 🔨 = bezig · ⏸️ = wacht · ✅ = klaar");
    if let Some(u) = web_url {
        out.push_str(&format!("\n🌐 {u}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(id: i64, status: &str, project: Option<&str>) -> Task {
        Task {
            id,
            title: format!("taak {id}"),
            notes: None,
            status: status.into(),
            project: project.map(String::from),
            assignee: Some("Noah".into()),
            due_date: Some("2026-10-16".into()),
            done_at: None,
        }
    }

    #[test]
    fn renders_grouped_board() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 17).unwrap();
        let tasks = vec![
            t(1, "todo", Some("Dakkapel")),
            t(2, "doing", Some("Dakkapel")),
            t(3, "done", Some("Dakkapel")),
            t(4, "waiting", None),
        ];
        let b = render(&tasks, today, None);
        assert!(b.starts_with("📋 Bord — za 17/10"));
        let dak = b.find("📁 Dakkapel").unwrap();
        let doing = b.find("#2").unwrap();
        let todo = b.find("#1").unwrap();
        assert!(dak < doing && doing < todo, "doing sorts before todo:\n{b}");
        assert!(b.contains("📁 Los\n⏸️ #4"));
        assert!(b.contains("⚠️📅 vr 16/10"));
        assert!(b.contains("✅ 1 klaar"));
        assert!(!b.contains("#3"));
    }
}
