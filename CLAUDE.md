# buzziwork — notes for Claude

Signal group bot for task tracking among a 3-person renovation crew (Harm + two others,
Dutch-speaking). Owner: Harm (harm@aarts.email). Read README.md for user-facing behaviour.

## Decisions (keep unless Harm says otherwise)

- **Minimal moving parts.** Rust binary spawns `signal-cli jsonRpc` as a child and speaks
  newline-delimited JSON-RPC over stdin/stdout. No Docker, no signal-cli-rest-api, no D-Bus.
- **Bot account:** preferably a numberless Signal account (Signal Login, Android beta 8.28,
  Sept 2026) with signal-cli linked as secondary device. signal-cli support for numberless
  accounts is unverified — first thing to test on real hardware. Fallback: prepaid SIM.
- **Storage:** single SQLite file (rusqlite, bundled). Schema in `src/db.rs`.
- **LLM:** OpenRouter, default `anthropic/claude-haiku-5.5`. Prompt in `src/llm.rs`.
  The model returns a JSON plan (`reply` + `actions`); we parse leniently (code fences ok).
  No structured-output API dependency on purpose (portable across models).
- **UX:** chat stays the interface. Bot is silent on chit-chat, confirms changes with one
  line per task (🆕/✏️), answers questions. Reactions: 🙋 claim, 🔨 doing, ⏸️ waiting,
  ✅ done; removing a reaction reverts.
- **Board:** a text message, edited in place on every change (Signal 24h edit window;
  repost after 9 edits), reposted daily at `BOARD_HOUR`.
- **Trello-ish model, one level deep:** project → task. No checklists/labels/priorities yet.
  Statuses: todo | doing | waiting | done | dropped.
- UI strings are Dutch.
- Example people in tests/docs are fictional (Harm, Sem, Daan).

## Data model

people(id, uuid, name) · projects(id, name, notes, status) ·
tasks(id, project_id, title, notes, status, assignee_id, due_date, position, created_by, created_at, done_at) ·
messages(signal_ts, task_id) — links any Signal message (ours or theirs) to tasks so
reactions/replies resolve · events(task_id, person_id, action, detail, at) — audit log ·
kv(key, value) — board_ts, board_edits, board_text, board_daily, group_id.

## Code map

- `src/signal.rs` — JSON-RPC client, `Messenger` trait, `parse_incoming` (envelope → `Incoming`)
- `src/llm.rs` — `Interpreter` trait, OpenRouter impl, prompt, `Plan`/`Action`
- `src/bot.rs` — message/reaction handling, applying actions, board refresh, daily loop, LLM context
- `src/board.rs` — text rendering for Signal
- `src/web.rs` + `src/board.html` — read-only web board (axum), token in path, fonts embedded from `static/fonts`
- `src/db.rs` — SQLite

`Bot<M: Messenger, L: Interpreter>` is generic so tests use fakes (see `bot.rs` tests).
Messages are handled sequentially in the main loop.

## Testing

`cargo test`. For an end-to-end smoke test without Signal, point `SIGNAL_CLI` at a script
that speaks the JSON-RPC protocol and `OPENROUTER_BASE_URL` at a local fake.

## Not yet verified against real signal-cli

- `editTimestamp` / `quoteTimestamp` / `quoteAuthor` param names in jsonRpc `send`
- exact `receive` envelope fields for reactions (`reaction.targetSentTimestamp`, `isRemove`),
  quotes (`quote.id`, `quote.text`) and mentions (`start`, `name`/`uuid`)
- edit-limit behaviour of Signal clients

## Next

1. Run against real signal-cli, fix field names.
2. Web board polish once real data flows (it exists: `src/web.rs`). Design: limewash/slate
   palette, Big Shoulders Display for numbers/headings, Atkinson Hyperlegible for text
   (readable in sunlight on site); keep that direction.
3. Reminders for due/overdue tasks in the daily board or a DM.
