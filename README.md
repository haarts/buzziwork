# buzziwork

A Signal group bot that keeps track of who does what for a small crew.
You chat normally; the bot extracts tasks via an LLM, confirms them, keeps a board
message up to date, and listens to emoji reactions.

```
Harm:  Sem doet dinsdag de goot west, en ik pak het zink links van de dakkapel
bot:   🆕 ⬜ #7 Goot west · Sem · 📅 di 20/10
bot:   🆕 ⬜ #8 Zink links vervangen · 📁 Dakkapel · Harm
Sem:  (reacts 🔨 on #7)          → status doing, board edited
```

**Moving parts:** one Rust binary + `signal-cli` (spawned as a child, JSON-RPC over stdio) +
one SQLite file + OpenRouter. No Docker, no REST bridge.

## In the chat

| You do | Bot does |
|---|---|
| Write naturally ("#4 is klaar", "Sem, kun jij vrijdag de steiger halen?") | Creates/updates tasks, confirms with 🆕 / ✏️ |
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

### 1. A Signal account for the bot

**Option A — no phone number (Signal Login, Android beta, since Sept 2026).**
Needs an Android device *with Google Play services* (payment goes through Play, ~$3 one-off).

1. Install the Signal Android beta (8.28+) on a spare phone, tap *Register Without Phone Number*, pay.
2. Store the **Account ID** and **Recovery Key** in your password manager — there is no recovery.
3. Set a profile name ("Buzzibot") and a **username** — without one nobody can reach the account.
4. Link signal-cli as a secondary device (below). Keep the phone around as primary device;
   linked devices drop off if the primary is gone for a long time.

> ⚠️ Untested: whether signal-cli handles numberless accounts. Linking should work since
> linked devices "behave the same", but signal-cli identifies accounts by number in places.
> If `-a` needs a value, try leaving `SIGNAL_ACCOUNT` unset (single-account mode) first.

**Option B — a cheap prepaid SIM.** `signal-cli -a +31… register` / `verify`. Boring, known to work.

### 2. signal-cli

```sh
# needs Java 21+, or grab the -Linux-native build
VERSION=0.14.2   # check https://github.com/AsamK/signal-cli/releases
curl -LO https://github.com/AsamK/signal-cli/releases/download/v$VERSION/signal-cli-$VERSION.tar.gz
sudo tar xf signal-cli-$VERSION.tar.gz -C /opt && sudo ln -sf /opt/signal-cli-$VERSION/bin/signal-cli /usr/local/bin/

# link as secondary device: prints a sgnl:// URI — turn it into a QR and scan it
# on the bot phone (Settings → Linked devices)
signal-cli link -n buzziwork | tee >(head -1 | qrencode -t utf8)

signal-cli listAccounts            # note the account identifier
signal-cli -a ACCOUNT receive      # once, to sync
```

### 3. Put the bot in the group

Create the group in Signal, share the **group link**, then:

```sh
signal-cli -a ACCOUNT joinGroup --uri 'https://signal.group/#…'
signal-cli -a ACCOUNT listGroups   # copy the group id → GROUP_ID
```

(Or start the bot without `GROUP_ID`; it logs the id of the first group it hears from.)

### 4. Run

```sh
cargo build --release
cp .env.example .env && $EDITOR .env
set -a; . ./.env; set +a; ./target/release/buzziwork
```

For keeps: `deploy/buzziwork.service` (systemd). The bot exits when signal-cli exits, and
systemd restarts both.

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
