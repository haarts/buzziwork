//! Read-only web board, served by the bot itself.
//!
//! The URL carries a secret token (`/<BOARD_TOKEN>`), so the page can sit on the
//! open internet behind a TLS reverse proxy without a login.

use crate::board::fmt_date;
use crate::db::{Db, Task};
use anyhow::Result;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{Local, NaiveDate};
use serde_json::json;
use std::sync::Arc;

const TEMPLATE: &str = include_str!("board.html");

/// Earthy project colours, readable as text on both themes.
const PROJECT_COLOURS: [&str; 8] = [
    "#2f7d7a", "#8a4fa0", "#b05a2c", "#3c6fb0", "#7a7a1e", "#b0436a", "#3f8a4a", "#6a5acd",
];

struct Web {
    db: Arc<Db>,
    token: String,
    title: String,
}

pub async fn serve(addr: &str, db: Arc<Db>, token: String, title: String) -> Result<()> {
    let state = Arc::new(Web { db, token, title });
    let app = Router::new()
        .route("/{token}", get(page))
        .route("/{token}/tasks.json", get(tasks_json))
        .route("/fonts/{name}", get(font))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    log::info!("web board on http://{addr}/<BOARD_TOKEN>");
    axum::serve(listener, app).await?;
    Ok(())
}

macro_rules! fonts {
    ($($name:literal),*) => {
        &[$(($name, include_bytes!(concat!("../static/fonts/", $name)) as &[u8])),*]
    };
}
const FONTS: &[(&str, &[u8])] = fonts!(
    "big-shoulders-display-600.woff2",
    "big-shoulders-display-800.woff2",
    "atkinson-hyperlegible-400.woff2",
    "atkinson-hyperlegible-400-italic.woff2",
    "atkinson-hyperlegible-700.woff2"
);

async fn font(Path(name): Path<String>) -> Response {
    match FONTS.iter().find(|(n, _)| *n == name) {
        Some((_, bytes)) => (
            [
                (header::CONTENT_TYPE, "font/woff2"),
                (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
            ],
            *bytes,
        )
            .into_response(),
        None => not_found(),
    }
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "Niet gevonden").into_response()
}

async fn page(State(w): State<Arc<Web>>, Path(token): Path<String>) -> Response {
    if token != w.token {
        return not_found();
    }
    match (w.db.live_tasks(7), w.db.people()) {
        (Ok(tasks), Ok(people)) => {
            let names: Vec<String> = people.into_iter().map(|p| p.name).collect();
            let now = Local::now();
            let html = render_page(&w.title, &tasks, &names, now.date_naive(), &now.format("%H:%M").to_string());
            ([(header::CACHE_CONTROL, "no-store")], Html(html)).into_response()
        }
        (Err(e), _) | (_, Err(e)) => {
            log::error!("web board: {e:#}");
            (StatusCode::INTERNAL_SERVER_ERROR, "Database niet bereikbaar").into_response()
        }
    }
}

async fn tasks_json(State(w): State<Arc<Web>>, Path(token): Path<String>) -> Response {
    if token != w.token {
        return not_found();
    }
    match w.db.live_tasks(7) {
        Ok(tasks) => Json(json!(tasks.iter().map(|t| json!({
            "id": t.id, "title": t.title, "notes": t.notes, "status": t.status,
            "project": t.project, "assignee": t.assignee, "due": t.due_date, "done_at": t.done_at,
        })).collect::<Vec<_>>()))
        .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            c => o.push(c),
        }
    }
    o
}

fn project_colour(name: &str) -> &'static str {
    // FNV-1a: stable across restarts, unlike std's RandomState.
    let mut h: u32 = 0x811c9dc5;
    for b in name.to_lowercase().bytes() {
        h = (h ^ b as u32).wrapping_mul(0x01000193);
    }
    PROJECT_COLOURS[(h as usize) % PROJECT_COLOURS.len()]
}

fn task_html(t: &Task, today: NaiveDate) -> String {
    let who = t.assignee.as_deref().unwrap_or("");
    let style = t
        .project
        .as_deref()
        .map(|p| format!(" style=\"--proj: {}\"", project_colour(p)))
        .unwrap_or_default();
    let mut meta = String::new();
    if let Some(p) = &t.project {
        meta.push_str(&format!("<span class=\"proj\">{}</span>", esc(p)));
    }
    match &t.assignee {
        Some(a) => meta.push_str(&format!("<span class=\"person\">{}</span>", esc(a))),
        None if t.status != "done" => meta.push_str("<span class=\"free\">nog vrij</span>"),
        None => {}
    }
    if let Some(d) = &t.due_date
        && let Ok(date) = NaiveDate::parse_from_str(d, "%Y-%m-%d")
    {
        let late = date < today && t.status != "done";
        let label = if date == today { "vandaag".to_string() } else { fmt_date(date) };
        meta.push_str(&format!(
            "<time datetime=\"{d}\"{}>{}{label}</time>",
            if late { " class=\"late\"" } else { "" },
            if late { "te laat: " } else { "" }
        ));
    }
    let notes = t
        .notes
        .as_deref()
        .filter(|n| !n.trim().is_empty())
        .map(|n| format!("\n      <p class=\"notes\">{}</p>", esc(n)))
        .unwrap_or_default();
    format!(
        "    <li class=\"task\" data-who=\"{}\"{style}>\n      <span class=\"num\">{}</span>\n      <div>\n      <p class=\"title\">{}</p>\n      <p class=\"meta\">{meta}</p>{notes}\n      </div>\n    </li>\n",
        esc(who),
        t.id,
        esc(&t.title)
    )
}

pub fn render_page(title: &str, tasks: &[Task], people: &[String], today: NaiveDate, updated: &str) -> String {
    let columns = [
        ("doing", "Bezig", "Niemand is ergens mee bezig."),
        ("todo", "Te doen", "Niks meer te doen."),
        ("waiting", "Wacht", "Nergens op aan het wachten."),
        ("done", "Klaar", "Deze week nog niks afgerond."),
    ];
    let mut main = String::new();
    for (status, label, empty) in columns {
        let mut ts: Vec<&Task> = tasks.iter().filter(|t| t.status == status).collect();
        if status == "done" {
            ts.sort_by(|a, b| b.done_at.cmp(&a.done_at));
        } else {
            // Due first (soonest on top), then the rest in creation order.
            ts.sort_by_key(|t| (t.due_date.is_none(), t.due_date.clone(), t.id));
        }
        main.push_str(&format!(
            "  <section class=\"col {status}\">\n  <h2>{label} <span class=\"n\">{}</span></h2>\n  <p class=\"empty\"{}>{empty}</p>\n  <ul>\n",
            ts.len(),
            if ts.is_empty() { "" } else { " hidden" }
        ));
        for t in ts {
            main.push_str(&task_html(t, today));
        }
        main.push_str("  </ul>\n  </section>\n");
    }
    let people_html: String = people
        .iter()
        .map(|p| format!("<button type=\"button\" data-who=\"{0}\" aria-pressed=\"false\">{0}</button>\n    ", esc(p)))
        .collect();
    TEMPLATE
        .replace("{{TITLE}}", &esc(title))
        .replace("{{UPDATED}}", &esc(updated))
        .replace("{{PEOPLE}}", people_html.trim_end())
        .replace("{{MAIN}}", &main)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_columns_and_escapes() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 17).unwrap();
        let tasks = vec![
            Task {
                id: 3,
                title: "Zink <links>".into(),
                notes: None,
                status: "doing".into(),
                project: Some("Dakkapel".into()),
                assignee: Some("Sem".into()),
                due_date: Some("2026-10-16".into()),
                done_at: None,
            },
            Task {
                id: 4,
                title: "Steiger".into(),
                notes: Some("bij Boels".into()),
                status: "todo".into(),
                project: None,
                assignee: None,
                due_date: None,
                done_at: None,
            },
        ];
        let html = render_page("Klussen", &tasks, &["Harm".into(), "Sem".into()], today, "17:31");
        assert!(html.contains("Zink &lt;links&gt;"));
        assert!(html.contains("<time datetime=\"2026-10-16\" class=\"late\">te laat: vr 16/10</time>"));
        assert!(html.contains("data-who=\"\""));
        assert!(html.contains("<span class=\"free\">nog vrij</span>"));
        assert!(html.contains("data-who=\"Sem\" aria-pressed"));
        assert!(html.contains("Nergens op aan het wachten.</p>"));
        assert!(!html.contains("{{"));
    }
}
