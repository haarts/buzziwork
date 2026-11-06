# buzziwork

A Signal group bot that keeps track of who does what for a small crew.
You chat normally; the bot extracts tasks via an LLM, confirms them, keeps a board
message up to date, and listens to emoji reactions.

```
Harm:  Sem doet dinsdag de goot west, en ik pak het zink links van de dakkapel
bot:   🆕 ⬜ #7 Goot west · Sem · 📅 di 20/10
bot:   🆕 ⬜ #8 Zink links vervangen · 📁 Dakkapel · Harm
Sem:  (reacts 🔨 on #7)          → status doing, board edited
Sem:  goot is klaar
bot:  (reacts ✅ on Sem's message) → status done, board edited
```

**Moving parts:** one Rust binary + `signal-cli` (spawned as a child, JSON-RPC over stdio) +
one SQLite file + OpenRouter. No Docker, no REST bridge.

## In the chat

| You do | Bot does |
|---|---|
| Write naturally ("Sem, kun jij vrijdag de steiger halen?") | Creates/updates tasks, confirms with 🆕 / ✏️ |
| Say something is done, started or waiting ("#4 is klaar", "ben met de goot bezig", "wacht op de zinkwerker") | Just reacts ✅ / 🔨 / ⏸️ on your message (no text) |
| Ask ("wat moet Sem nog doen?") | Answers from the task list |
| React 🙋 on a task message | Assigns it to you |
| React 🔨 / ⏸️ / ✅ | doing / waiting / done (removing the reaction undoes it) |
| `/bord` (or `/board`) | Posts a fresh board |
| `/help` | Short help |

The bot reads every message but stays silent on chit-chat. Reactions work on the bot's
confirmation **and** on the original message that created the task.

The board is reposted daily at `BOARD_HOUR` and edited in place on every change
(Signal allows edits for 24 h; after 9 edits it reposts).

## Setup

### 1. Install on a server

`deploy/deploy.sh` (run from your machine, `HOST=racknerd` by default) syncs the source,
builds in a `rust:1-bookworm` container on the server, and installs everything for the
login user. No root:

```
~/.local/bin/{buzziwork,signal-cli}      signal-cli = native build, no Java
~/.config/buzziwork/env                  config (created once from .env.example)
~/.local/share/buzziwork/                working dir + SQLite
~/.config/systemd/user/buzziwork.service user unit; lingering is enabled so it survives logout
```

Re-run it to upgrade. The bot exits when signal-cli exits, and systemd restarts both.

### 2. A Signal account for the bot

The bot needs its own number; any phone that receives SMS (a dumb phone is fine) will do.
Registering makes signal-cli the **primary device** for that number, so don't use a
number that already has a Signal account you care about: that account would be taken over.

```sh
signal-cli -a +316… register
# usually answered with "captcha required": open
#   https://signalcaptchas.org/registration/generate.html
# solve it, copy the signalcaptcha://… link behind "Open Signal", then:
signal-cli -a +316… register --captcha 'signalcaptcha://…'
signal-cli -a +316… verify 123456        # code from the SMS (or: register --voice)
signal-cli -a +316… updateProfile --given-name Buzzibot
```

Set a registration-lock PIN later if you like (`setPin`). Numberless accounts
(Signal Login, Android beta, Sept 2026) might replace this one day; untested with signal-cli.

### 3. Put the bot in the group

Create the group in Signal, share the **group link**, then:

```sh
signal-cli -a +316… joinGroup --uri 'https://signal.group/#…'
signal-cli -a +316… listGroups   # copy the group id → GROUP_ID
```

(Or start the bot without `GROUP_ID`; it logs the id of the first group it hears from.)

### 4. Run

Fill in `~/.config/buzziwork/env`, then

```sh
systemctl --user start buzziwork
journalctl --user -u buzziwork -f
```

Locally: `cargo build --release && cp .env.example .env && set -a; . ./.env; set +a; ./target/release/buzziwork`.

## Web board

Read-only overview at `/<BOARD_TOKEN>`, served by the bot itself (same process, same
SQLite file). Four columns (Bezig / Te doen / Wacht / Klaar), project colour on each task,
filter on person or "nog vrij", refreshes itself every minute. Fonts are embedded in the
binary, so the page makes no third-party requests. There's also `/<token>/tasks.json`.

There's no login: the token in the URL is the key, so keep it out of public places.
Put it behind a TLS reverse proxy, e.g. Caddy:

```
bord.example.nl {
    reverse_proxy 127.0.0.1:8080
}
```

## Configuration (env)

| Var | Default | |
|---|---|---|
| `OPENROUTER_API_KEY` | — | required |
| `OPENROUTER_MODEL` | `anthropic/claude-haiku-5.5` | any OpenRouter chat model |
| `OPENROUTER_BASE_URL` | `https://openrouter.ai/api/v1` | |
| `SIGNAL_ACCOUNT` | unset | passed as `-a`; omit if signal-cli has one account |
| `SIGNAL_CLI` | `signal-cli` | path to the binary |
| `SIGNAL_CONFIG_DIR` | signal-cli default | passed as `--config` |
| `GROUP_ID` | unset | lock to one group (recommended) |
| `DB_PATH` | `buzziwork.db` | SQLite file |
| `BOARD_HOUR` | `7` | local hour for the daily board |
| `BOARD_LISTEN` | unset | e.g. `127.0.0.1:8080` — enables the web board |
| `BOARD_TOKEN` | — | secret path segment, required with `BOARD_LISTEN` (`openssl rand -hex 16`) |
| `BOARD_TITLE` | `Klussen` | heading on the web board |
| `BOARD_URL` | unset | link appended to the Signal board, e.g. `https://bord.example.nl/<token>` |
| `RUST_LOG` | `info` | `debug` shows raw signal-cli traffic |

## Cost

Every group message is one LLM call (~1–2k tokens in, ~100 out). With Haiku 5.5 at
$0.10/$0.50 per M tokens that's roughly €0.02 per 100 messages.

## Development

```sh
cargo test     # unit + bot-flow tests with fake Signal and fake LLM
```
