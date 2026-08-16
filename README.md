# D&D Tea Rota Bot

A small Discord bot (Rust + [serenity](https://github.com/serenity-rs/serenity))
that tracks a **fixed tea rota** for your D&D sessions — a set list of people
who take turns bringing the tea. It remembers whose turn is next and advances
the rotation when a session is done.

State is stored per-server in a plain JSON file (`rota_data.json`), so it's easy
to inspect or edit by hand.

## Commands

Every command takes an optional **`public`** toggle controlling who sees the
reply. It defaults to posting in the channel for everyone, **except `/rota list`
and `/session list`, which default to private** (only you). Pass `public:false`
to hide any reply, or `public:true` to share a list.

### `/rota` — manage the rotation of people

| Command                    | What it does                                             |
| -------------------------- | -------------------------------------------------------- |
| `/rota list [public:true]` | Show the rotation order and whose turn is next (private to you unless `public:true`) |
| `/rota whose_turn`         | Say whose turn it is to bring the tea                   |
| `/rota add user:@person`   | Add someone to the end of the rotation                   |
| `/rota remove user:@person`| Remove someone from the rotation                         |
| `/rota set_next user:@person` | Jump the rotation to a specific person                |
| `/rota swap user1:@a user2:@b` | Swap two people's order in the rota and their upcoming sessions |
| `/rota clear`              | Remove everyone from the rotation                        |

### `/potsworth` — the butler

| Command                          | What it does                                                          |
| -------------------------------- | --------------------------------------------------------------------- |
| `/potsworth add master:@person`  | Assign Potsworth's master for this channel. **Can only ever be done once per channel** — after that, any attempt is met with "*@master is Potsworth's only master!*" |

## Per-channel rotas

Each **channel** has its own independent rota and session schedule — state is
keyed by channel id. Run the commands in whatever channel you like; a rota in
`#tuesday-game` is completely separate from one in `#friday-game`. The 🍵
tag-reaction also fires in any channel Potsworth is mentioned in.

### `/session` — schedule and track session dates

| Command                                   | What it does                                                        |
| ----------------------------------------- | ------------------------------------------------------------------ |
| `/session add date:YYYY/MM/DD [note:...] [skip:true]` | Schedule a session; **assigns the next person in the rota** to tea and advances the rotation. With `skip:true` the session uses no rota (split — no assignee, no turn consumed) |
| `/session list [public:true]`             | List upcoming sessions with who's on tea (private to you unless `public:true`) |
| `/session next`                           | Show the next upcoming session                                     |
| `/session history`                        | List past sessions, most recent first                              |
| `/session assign date:YYYY/MM/DD user:@person` | Change who's on tea for a session; the substitute moves to the back of the rota |
| `/session assign date:YYYY/MM/DD skip:true` | Mark an existing session as split (no rota, no assignee) |
| `/session reschedule from:YYYY/MM/DD to:YYYY/MM/DD` | Move a session to a new date, keeping its assignee and note |
| `/session remove date:YYYY/MM/DD`         | Remove a scheduled session                                         |

Scheduling sessions cycles through the rota in order, so booking the next few
dates automatically spreads tea duty across the group. Each session locks in
whoever was up next **at the time it was scheduled** (a snapshot), so later rota
changes don't retroactively reshuffle who's on tea for a booked date. Dates are
plain `YYYY/MM/DD`; past sessions drop off `/session list` automatically.

### Keeping it fair when you reassign a session

`/session assign` lets someone cover a session for the person who was on tea.
To keep turns even, it applies a **"send substitute to back"** rule and then
**rebalances the sessions that come after it**:

- The covered session is pinned to the substitute.
- The substitute is moved to the **end** of the rotation order (they've just
  taken a turn); everyone else keeps their relative place.
- **Only sessions dated after the reassigned one** are re-cycled through the new
  order from the front, in date order — so tea duty is spread evenly across the
  rest of the calendar.
- Sessions on or before the reassigned one — earlier upcoming sessions and past
  ones alike — keep their existing assignees.

The substitute must already be in the rota (add them with `/rota add` first).

### Adding the butler himself

If someone runs `/rota add` on the **bot itself**, Potsworth won't add himself —
he demurs: *"🤵🏻‍♂️ I'm the butler — I serve the tea, I don't bring it!"*.

Example — order Alice, Bob, Carol with sessions auto-assigned
`22 Aug → Alice, 29 Aug → Bob, 5 Sep → Carol, 12 Sep → Alice`. Bob covers the
22nd:

```
/session assign date:2026/08/22 user:@Bob
→ order becomes Alice, Carol, Bob
  22 Aug — Bob   (pinned cover)
  29 Aug — Alice
  5 Sep  — Carol
  12 Sep — Bob
```

## Setup

### 1. Create a Discord application & bot

1. Go to the [Discord Developer Portal](https://discord.com/developers/applications)
   and create a **New Application**.
2. Open the **Bot** tab and click **Add Bot**. Copy the **token**.
3. Under **Installation** (or the legacy **OAuth2 → URL Generator**), give the
   bot the `applications.commands` scope (and `bot` scope) and invite it to your
   server.

No privileged gateway intents are required — the bot only uses slash commands.

### 2. Configure

Copy the example env file and paste in your token:

```sh
cp .env.example .env
# then edit .env and set DISCORD_TOKEN
```

For instant slash-command registration while developing, also set
`TEST_GUILD_ID` to your server's id (global commands can take up to an hour to
appear the first time).

### 3. Run

```sh
cargo run --release
```

You should see `Connected as <bot name>` and a message confirming the commands
were registered. Then try `/rota add` in your server.

## Running with Docker

A multi-stage `Dockerfile` builds a small runtime image (Debian slim, non-root
user). State is written to `/data/rota_data.json` inside the container, so mount
a volume there to persist it.

### With docker compose (easiest)

Put your token in `.env` (see step 2), then:

```sh
docker compose up -d --build      # build & run in the background
docker compose logs -f            # watch the logs
docker compose down               # stop
```

The compose file reads `.env` for `DISCORD_TOKEN` (and optional `TEST_GUILD_ID`)
and keeps data in a named volume (`potsworth-data`).

### With plain docker

```sh
docker build -t potsworth .
docker run -d --name potsworth \
  -e DISCORD_TOKEN=your-token \
  -v potsworth-data:/data \
  --restart unless-stopped \
  potsworth
```

The data file location can be overridden with the `DATA_PATH` env var (defaults
to `/data/rota_data.json` in the image).

## How the rotation works

- People are kept in a fixed, ordered list.
- One person is always marked as **up next** (👉 in `/rota list`).
- Scheduling a session with `/session add` assigns the person who's up next and
  advances the marker to the following person, wrapping around at the end. This
  is the only thing that advances the rotation, so nobody gets skipped.
- `/rota set_next` jumps the marker to a specific person (e.g. to correct it).
- Removing someone before the current person keeps the same person "up next";
  the rotation order is otherwise preserved.
