//! SQLite storage. One file, one connection behind a mutex — plenty for three people.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use std::sync::Mutex;

const SCHEMA: &str = r#"
PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;

CREATE TABLE IF NOT EXISTS people (
    id    INTEGER PRIMARY KEY,
    uuid  TEXT NOT NULL UNIQUE,          -- Signal ACI
    name  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS projects (
    id         INTEGER PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE COLLATE NOCASE,
    notes      TEXT,
    status     TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active','done')),
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS tasks (
    id          INTEGER PRIMARY KEY,
    project_id  INTEGER REFERENCES projects(id),
    title       TEXT NOT NULL,
    notes       TEXT,
    status      TEXT NOT NULL DEFAULT 'todo'
                CHECK (status IN ('todo','doing','waiting','done','dropped')),
    assignee_id INTEGER REFERENCES people(id),
    due_date    TEXT,                     -- YYYY-MM-DD
    position    INTEGER NOT NULL DEFAULT 0,
    created_by  INTEGER REFERENCES people(id),
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    done_at     TEXT
);

-- Which Signal message is about which task (so reactions/replies find it).
CREATE TABLE IF NOT EXISTS messages (
    signal_ts INTEGER NOT NULL,
    task_id   INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    PRIMARY KEY (signal_ts, task_id)
);

CREATE TABLE IF NOT EXISTS events (
    id        INTEGER PRIMARY KEY,
    task_id   INTEGER REFERENCES tasks(id) ON DELETE SET NULL,
    person_id INTEGER REFERENCES people(id),
    action    TEXT NOT NULL,
    detail    TEXT,
    at        TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS kv (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

pub const STATUSES: [&str; 5] = ["todo", "doing", "waiting", "done", "dropped"];

#[derive(Debug, Clone, PartialEq)]
pub struct Person {
    pub id: i64,
    pub uuid: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Task {
    pub id: i64,
    pub title: String,
    pub notes: Option<String>,
    pub status: String,
    pub project: Option<String>,
    pub assignee: Option<String>,
    pub due_date: Option<String>,
    pub done_at: Option<String>,
}

/// A partial update. `Some(None)` clears a nullable field.
#[derive(Debug, Default, Clone)]
pub struct TaskPatch {
    pub title: Option<String>,
    pub notes: Option<Option<String>>,
    pub status: Option<String>,
    pub project_id: Option<Option<i64>>,
    pub assignee_id: Option<Option<i64>>,
    pub due_date: Option<Option<String>>,
}

impl TaskPatch {
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.notes.is_none()
            && self.status.is_none()
            && self.project_id.is_none()
            && self.assignee_id.is_none()
            && self.due_date.is_none()
    }
}

pub struct Db(Mutex<Connection>);

const TASK_SELECT: &str = "SELECT t.id, t.title, t.notes, t.status, p.name, pe.name, t.due_date, t.done_at
     FROM tasks t
     LEFT JOIN projects p ON p.id = t.project_id
     LEFT JOIN people pe ON pe.id = t.assignee_id";

fn row_to_task(r: &rusqlite::Row) -> rusqlite::Result<Task> {
    Ok(Task {
        id: r.get(0)?,
        title: r.get(1)?,
        notes: r.get(2)?,
        status: r.get(3)?,
        project: r.get(4)?,
        assignee: r.get(5)?,
        due_date: r.get(6)?,
        done_at: r.get(7)?,
    })
}

impl Db {
    pub fn open(path: &str) -> Result<Self> {
        let c = Connection::open(path)?;
        c.execute_batch(SCHEMA)?;
        Ok(Self(Mutex::new(c)))
    }

    fn c(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.0.lock().expect("db mutex poisoned")
    }

    // ---- people

    pub fn upsert_person(&self, uuid: &str, name: &str) -> Result<Person> {
        let c = self.c();
        c.execute(
            "INSERT INTO people (uuid, name) VALUES (?1, ?2)
             ON CONFLICT(uuid) DO UPDATE SET name = excluded.name WHERE excluded.name != '?'",
            params![uuid, name],
        )?;
        Ok(c.query_row(
            "SELECT id, uuid, name FROM people WHERE uuid = ?1",
            [uuid],
            |r| Ok(Person { id: r.get(0)?, uuid: r.get(1)?, name: r.get(2)? }),
        )?)
    }

    pub fn people(&self) -> Result<Vec<Person>> {
        let c = self.c();
        let mut st = c.prepare("SELECT id, uuid, name FROM people ORDER BY name")?;
        let v = st
            .query_map([], |r| Ok(Person { id: r.get(0)?, uuid: r.get(1)?, name: r.get(2)? }))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    /// Case-insensitive match on full name, then on first name / prefix.
    pub fn find_person(&self, name: &str) -> Result<Option<Person>> {
        let needle = name.trim().trim_start_matches('@').to_lowercase();
        if needle.is_empty() {
            return Ok(None);
        }
        let people = self.people()?;
        if let Some(p) = people.iter().find(|p| p.name.to_lowercase() == needle) {
            return Ok(Some(p.clone()));
        }
        let hits: Vec<_> = people
            .iter()
            .filter(|p| {
                let n = p.name.to_lowercase();
                n.starts_with(&needle) || n.split_whitespace().any(|w| w == needle)
            })
            .collect();
        Ok(if hits.len() == 1 { Some(hits[0].clone()) } else { None })
    }

    // ---- projects

    pub fn project_id(&self, name: &str) -> Result<i64> {
        let c = self.c();
        c.execute("INSERT OR IGNORE INTO projects (name) VALUES (?1)", [name.trim()])?;
        Ok(c.query_row("SELECT id FROM projects WHERE name = ?1", [name.trim()], |r| r.get(0))?)
    }

    pub fn set_project_notes(&self, id: i64, notes: &str) -> Result<()> {
        self.c().execute("UPDATE projects SET notes = ?2 WHERE id = ?1", params![id, notes])?;
        Ok(())
    }

    pub fn project_names(&self) -> Result<Vec<String>> {
        let c = self.c();
        let mut st = c.prepare("SELECT name FROM projects WHERE status = 'active' ORDER BY name")?;
        let v = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    // ---- tasks

    pub fn create_task(
        &self,
        title: &str,
        project_id: Option<i64>,
        assignee_id: Option<i64>,
        due_date: Option<&str>,
        notes: Option<&str>,
        created_by: Option<i64>,
    ) -> Result<i64> {
        let c = self.c();
        c.execute(
            "INSERT INTO tasks (title, project_id, assignee_id, due_date, notes, created_by, position)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, (SELECT COALESCE(MAX(position), 0) + 1 FROM tasks))",
            params![title, project_id, assignee_id, due_date, notes, created_by],
        )?;
        Ok(c.last_insert_rowid())
    }

    pub fn update_task(&self, id: i64, p: &TaskPatch) -> Result<bool> {
        let c = self.c();
        let mut n = 0;
        if let Some(v) = &p.title {
            n += c.execute("UPDATE tasks SET title = ?2 WHERE id = ?1", params![id, v])?;
        }
        if let Some(v) = &p.notes {
            n += c.execute("UPDATE tasks SET notes = ?2 WHERE id = ?1", params![id, v])?;
        }
        if let Some(v) = &p.project_id {
            n += c.execute("UPDATE tasks SET project_id = ?2 WHERE id = ?1", params![id, v])?;
        }
        if let Some(v) = &p.assignee_id {
            n += c.execute("UPDATE tasks SET assignee_id = ?2 WHERE id = ?1", params![id, v])?;
        }
        if let Some(v) = &p.due_date {
            n += c.execute("UPDATE tasks SET due_date = ?2 WHERE id = ?1", params![id, v])?;
        }
        if let Some(v) = &p.status {
            n += c.execute(
                "UPDATE tasks SET status = ?2,
                   done_at = CASE WHEN ?2 = 'done' THEN COALESCE(done_at, datetime('now')) ELSE NULL END
                 WHERE id = ?1",
                params![id, v],
            )?;
        }
        Ok(n > 0)
    }

    pub fn task(&self, id: i64) -> Result<Option<Task>> {
        let c = self.c();
        Ok(c
            .query_row(&format!("{TASK_SELECT} WHERE t.id = ?1"), [id], row_to_task)
            .optional()?)
    }

    /// Everything that is not dropped, plus done tasks from the last `done_days` days.
    pub fn live_tasks(&self, done_days: i64) -> Result<Vec<Task>> {
        let c = self.c();
        let mut st = c.prepare(&format!(
            "{TASK_SELECT}
             WHERE t.status NOT IN ('dropped','done')
                OR (t.status = 'done' AND t.done_at >= datetime('now', ?1))
             ORDER BY p.name IS NULL, p.name, t.position"
        ))?;
        let v = st
            .query_map([format!("-{done_days} days")], row_to_task)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    // ---- message ↔ task links

    pub fn link_message(&self, ts: i64, task_id: i64) -> Result<()> {
        self.c().execute(
            "INSERT OR IGNORE INTO messages (signal_ts, task_id) VALUES (?1, ?2)",
            params![ts, task_id],
        )?;
        Ok(())
    }

    pub fn tasks_for_message(&self, ts: i64) -> Result<Vec<i64>> {
        let c = self.c();
        let mut st = c.prepare("SELECT task_id FROM messages WHERE signal_ts = ?1")?;
        let v = st.query_map([ts], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    // ---- history

    pub fn log(&self, task_id: Option<i64>, person_id: Option<i64>, action: &str, detail: &str) -> Result<()> {
        self.c().execute(
            "INSERT INTO events (task_id, person_id, action, detail) VALUES (?1, ?2, ?3, ?4)",
            params![task_id, person_id, action, detail],
        )?;
        Ok(())
    }

    // ---- kv

    pub fn get(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .c()
            .query_row("SELECT value FROM kv WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        self.c().execute(
            "INSERT INTO kv (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_lifecycle() {
        let db = Db::open(":memory:").unwrap();
        let harm = db.upsert_person("u1", "Harm Aarts").unwrap();
        db.upsert_person("u2", "Sem").unwrap();
        assert_eq!(db.find_person("harm").unwrap().unwrap().id, harm.id);
        assert_eq!(db.find_person("@sem").unwrap().unwrap().name, "Sem");
        assert!(db.find_person("piet").unwrap().is_none());

        let proj = db.project_id("Dakkapel").unwrap();
        assert_eq!(db.project_id("dakkapel").unwrap(), proj);
        let id = db
            .create_task("Zink links", Some(proj), None, Some("2026-10-20"), None, Some(harm.id))
            .unwrap();
        db.link_message(123, id).unwrap();
        assert_eq!(db.tasks_for_message(123).unwrap(), vec![id]);

        let patch = TaskPatch {
            status: Some("done".into()),
            assignee_id: Some(Some(harm.id)),
            ..Default::default()
        };
        assert!(db.update_task(id, &patch).unwrap());
        let t = db.task(id).unwrap().unwrap();
        assert_eq!(t.status, "done");
        assert_eq!(t.assignee.as_deref(), Some("Harm Aarts"));
        assert_eq!(t.project.as_deref(), Some("Dakkapel"));
        assert!(t.done_at.is_some());
        assert_eq!(db.live_tasks(7).unwrap().len(), 1);

        db.update_task(id, &TaskPatch { status: Some("dropped".into()), ..Default::default() })
            .unwrap();
        assert!(db.live_tasks(7).unwrap().is_empty());
    }

    #[test]
    fn person_name_not_overwritten_by_unknown() {
        let db = Db::open(":memory:").unwrap();
        db.upsert_person("u1", "Harm").unwrap();
        assert_eq!(db.upsert_person("u1", "?").unwrap().name, "Harm");
    }
}
