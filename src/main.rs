//! A Discord bot that tracks a fixed tea rota for D&D sessions.
//!
//! It exposes a single `/rota` slash command with subcommands to manage a
//! rotation of who brings the tea each session. State is stored per-guild in
//! a JSON file on disk.

mod store;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{Local, NaiveDate};
use serenity::all::*;
use serenity::async_trait;

use store::{
    AddSessionError, AssignError, AssignOutcome, Member, RescheduleError, Rota, Session, Store,
};

/// Default location for the JSON state (relative to the working directory).
/// Override with the `DATA_PATH` environment variable — handy for pointing at a
/// mounted volume in Docker.
const DATA_PATH: &str = "rota_data.json";

struct Handler {
    store: Arc<Mutex<Store>>,
    /// The bot's own user id, learned on `ready` (0 until then).
    bot_id: AtomicU64,
}

impl Handler {
    /// Turn a slash-command interaction into a reply, applying any state changes
    /// and persisting them. Returns the reply text and whether it should be
    /// ephemeral (only visible to the invoking user).
    fn handle(&self, command: &CommandInteraction) -> (String, bool) {
        let Some(guild_id) = command.guild_id else {
            return (
                "The tea rota only works inside a server, not in DMs.".to_string(),
                false,
            );
        };

        let Some(sub) = command.data.options.first() else {
            return ("Unknown command.".to_string(), false);
        };

        let CommandDataOptionValue::SubCommand(opts) = &sub.value else {
            return ("Unknown command.".to_string(), false);
        };

        let mut store = self.store.lock().unwrap();
        let rota = store.rota_mut(guild_id.get());

        // Once bound to the master's channel, Potsworth only works there.
        if let Some(ch) = rota.master_channel() {
            if command.channel_id.get() != ch {
                return (
                    format!("🤵🏻‍♂️ I only attend to my duties in <#{ch}>."),
                    true,
                );
            }
        }

        let bot_id = self.bot_id.load(Ordering::Relaxed);
        let reply = match command.data.name.as_str() {
            "rota" => handle_rota(rota, &sub.name, opts, command, bot_id),
            "session" => handle_session(rota, &sub.name, opts, command),
            "potsworth" => handle_potsworth(rota, &sub.name, opts, command, bot_id),
            other => format!("Unknown command `{other}`."),
        };

        if let Err(e) = store.save() {
            eprintln!("Failed to save rota data: {e}");
        }

        (reply, false)
    }
}

/// Handle the `/rota` subcommands.
fn handle_rota(
    rota: &mut Rota,
    sub: &str,
    opts: &[CommandDataOption],
    command: &CommandInteraction,
    bot_id: u64,
) -> String {
    match sub {
        "list" => {
            if rota.members.is_empty() {
                "The rota is empty. Add people with `/rota add`.".to_string()
            } else {
                let mut lines = String::from("**🍵 Tea rota**\n");
                for (i, m) in rota.members.iter().enumerate() {
                    let marker = if i == rota.current { "👉 " } else { "   " };
                    lines.push_str(&format!("{marker}{}. {}\n", i + 1, m.name));
                }
                if let Some(cur) = rota.current_member() {
                    lines.push_str(&format!("\nUp next: {} 🍵", cur.mention()));
                }
                lines
            }
        }

        "whose_turn" => match rota.current_member() {
            Some(m) => format!("It's {}'s turn to bring the tea! 🍵", m.mention()),
            None => "The rota is empty. Add people with `/rota add`.".to_string(),
        },

        "add" => match user_option(opts, "user", command) {
            // Potsworth won't add himself to the rota.
            Some((id, _)) if bot_id != 0 && id == bot_id => {
                "🤵🏻‍♂️ I'm the butler — I serve the tea, I don't bring it!".to_string()
            }
            Some((id, name)) => {
                if rota.add(Member { id, name: name.clone() }) {
                    format!("Added **{name}** to the rota.")
                } else {
                    format!("**{name}** is already in the rota.")
                }
            }
            None => "You need to specify a user.".to_string(),
        },

        "remove" => match user_option(opts, "user", command) {
            Some((id, _)) => match rota.remove(id) {
                Some(m) => format!("Removed **{}** from the rota.", m.name),
                None => "That person isn't in the rota.".to_string(),
            },
            None => "You need to specify a user.".to_string(),
        },

        "next" => match rota.advance() {
            Some(m) => format!(
                "Thanks to whoever brought tea last time! 🎲\nUp next: {} 🍵",
                m.mention()
            ),
            None => "The rota is empty. Add people with `/rota add`.".to_string(),
        },

        "set_next" => match user_option(opts, "user", command) {
            Some((id, name)) => {
                if rota.set_current(id) {
                    format!("Set **{name}** as up next. 🍵")
                } else {
                    format!("**{name}** isn't in the rota — add them first.")
                }
            }
            None => "You need to specify a user.".to_string(),
        },

        "clear" => {
            rota.members.clear();
            rota.current = 0;
            "Cleared the rota.".to_string()
        }

        other => format!("Unknown subcommand `{other}`."),
    }
}

/// A session's tea duty as a plain name, or a note that it's skipped.
fn session_name(s: &Session) -> String {
    match &s.assignee {
        Some(m) => m.name.clone(),
        None => "— split (no rota)".to_string(),
    }
}

/// A session's tea duty as a mention, or a note that it's skipped.
fn session_mention(s: &Session) -> String {
    match &s.assignee {
        Some(m) => format!("{} is on tea. 🍵", m.mention()),
        None => "no rota this session — the group splits. 🍵".to_string(),
    }
}

/// Handle the `/session` subcommands.
fn handle_session(
    rota: &mut Rota,
    sub: &str,
    opts: &[CommandDataOption],
    command: &CommandInteraction,
) -> String {
    match sub {
        "add" => {
            let Some(raw) = string_option(opts, "date") else {
                return "You need to specify a date.".to_string();
            };
            let Some(date) = parse_date(&raw) else {
                return format!("`{raw}` isn't a valid date. Use YYYY/MM/DD, e.g. 2026/08/22.");
            };
            let note = string_option(opts, "note").filter(|s| !s.trim().is_empty());
            let skip = bool_option(opts, "skip");
            match rota.add_session(iso(date), note, skip) {
                Ok(Some(assignee)) => format!(
                    "📅 Scheduled **{}** — {} is on tea. 🍵",
                    pretty(date),
                    assignee.mention()
                ),
                Ok(None) => format!(
                    "📅 Scheduled **{}** — no rota this session (the group splits). 🍵",
                    pretty(date)
                ),
                Err(AddSessionError::NoMembers) => {
                    "The rota is empty — add people with `/rota add` before scheduling sessions \
                     (or use `skip:true` for a split session)."
                        .to_string()
                }
                Err(AddSessionError::DuplicateDate) => {
                    format!("There's already a session on {}.", pretty(date))
                }
            }
        }

        "list" => {
            let today = iso(Local::now().date_naive());
            let upcoming: Vec<_> = rota.upcoming(&today).collect();
            if upcoming.is_empty() {
                "No upcoming sessions. Schedule one with `/session add`.".to_string()
            } else {
                let mut out = String::from("**📅 Upcoming sessions**\n");
                for s in upcoming {
                    out.push_str(&format!("• {} — {}", pretty_iso(&s.date), session_name(s)));
                    if let Some(note) = &s.note {
                        out.push_str(&format!(" ({note})"));
                    }
                    out.push('\n');
                }
                out
            }
        }

        "history" => {
            let today = iso(Local::now().date_naive());
            let mut past: Vec<_> = rota.past(&today).collect();
            past.reverse(); // most recent first
            if past.is_empty() {
                "No past sessions yet.".to_string()
            } else {
                let total = past.len();
                let mut out = String::from("**📜 Past sessions**\n");
                for s in past.iter().take(20) {
                    out.push_str(&format!("• {} — {}", pretty_iso(&s.date), session_name(s)));
                    if let Some(note) = &s.note {
                        out.push_str(&format!(" ({note})"));
                    }
                    out.push('\n');
                }
                if total > 20 {
                    out.push_str(&format!("…and {} earlier.", total - 20));
                }
                out
            }
        }

        "next" => {
            let today = iso(Local::now().date_naive());
            match rota.upcoming(&today).next() {
                Some(s) => {
                    let note = s
                        .note
                        .as_ref()
                        .map(|n| format!("\n> {n}"))
                        .unwrap_or_default();
                    format!(
                        "Next session: **{}** — {}{note}",
                        pretty_iso(&s.date),
                        session_mention(s)
                    )
                }
                None => "No upcoming sessions. Schedule one with `/session add`.".to_string(),
            }
        }

        "remove" => {
            let Some(raw) = string_option(opts, "date") else {
                return "You need to specify a date.".to_string();
            };
            let Some(date) = parse_date(&raw) else {
                return format!("`{raw}` isn't a valid date. Use YYYY/MM/DD, e.g. 2026/08/22.");
            };
            match rota.remove_session(&iso(date)) {
                Some(s) => format!("Removed the session on {}.", pretty_iso(&s.date)),
                None => format!("No session found on {}.", pretty(date)),
            }
        }

        "reschedule" => {
            let Some(from_raw) = string_option(opts, "from") else {
                return "You need to specify the current date (`from`).".to_string();
            };
            let Some(from) = parse_date(&from_raw) else {
                return format!(
                    "`{from_raw}` isn't a valid date. Use YYYY/MM/DD, e.g. 2026/08/22."
                );
            };
            let Some(to_raw) = string_option(opts, "to") else {
                return "You need to specify the new date (`to`).".to_string();
            };
            let Some(to) = parse_date(&to_raw) else {
                return format!("`{to_raw}` isn't a valid date. Use YYYY/MM/DD, e.g. 2026/08/29.");
            };
            match rota.reschedule_session(&iso(from), iso(to)) {
                Ok(s) => format!(
                    "📅 Moved the session from {} to **{}** — {}",
                    pretty(from),
                    pretty(to),
                    session_mention(&s)
                ),
                Err(RescheduleError::NotFound) => format!("No session found on {}.", pretty(from)),
                Err(RescheduleError::DuplicateDate) => {
                    format!("There's already a session on {}.", pretty(to))
                }
            }
        }

        "assign" => {
            let Some(raw) = string_option(opts, "date") else {
                return "You need to specify a date.".to_string();
            };
            let Some(date) = parse_date(&raw) else {
                return format!("`{raw}` isn't a valid date. Use YYYY/MM/DD, e.g. 2026/08/22.");
            };

            // `skip:true` marks the session as split (no rota), no user needed.
            if bool_option(opts, "skip") {
                return if rota.skip_session(&iso(date)) {
                    format!(
                        "📅 **{}** is now a split session — no rota. 🍵",
                        pretty(date)
                    )
                } else {
                    format!("No session found on {}.", pretty(date))
                };
            }

            let Some((id, name)) = user_option(opts, "user", command) else {
                return "You need to specify a user (or set `skip:true` for a split session)."
                    .to_string();
            };
            let today = iso(Local::now().date_naive());
            match rota.assign_session(&iso(date), id, &today) {
                Ok(AssignOutcome::Reassigned { old, new }) => {
                    let was = match old {
                        Some(m) => m.name,
                        None => "split, no rota".to_string(),
                    };
                    let mut msg = format!(
                        "🔁 {} is now on tea for **{}** (was {was}).\n**Rebalanced upcoming rota:**\n",
                        new.mention(),
                        pretty(date),
                    );
                    for s in rota.upcoming(&today) {
                        msg.push_str(&format!("• {} — {}\n", pretty_iso(&s.date), session_name(s)));
                    }
                    msg
                }
                Ok(AssignOutcome::Unchanged(m)) => {
                    format!("**{}** is already on tea for {}.", m.name, pretty(date))
                }
                Err(AssignError::SessionNotFound) => {
                    format!("No session found on {}.", pretty(date))
                }
                Err(AssignError::NotAMember) => {
                    format!("**{name}** isn't in the rota — add them with `/rota add` first.")
                }
            }
        }

        other => format!("Unknown subcommand `{other}`."),
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        println!("Connected as {}", ready.user.name);
        self.bot_id.store(ready.user.id.get(), Ordering::Relaxed);

        let commands = vec![
            build_rota_command(),
            build_session_command(),
            build_potsworth_command(),
        ];

        // If TEST_GUILD_ID is set, register instantly to that one guild
        // (global commands can take up to an hour to appear).
        match std::env::var("TEST_GUILD_ID").ok().and_then(|s| s.parse::<u64>().ok()) {
            Some(gid) => {
                match GuildId::new(gid).set_commands(&ctx.http, commands).await {
                    Ok(_) => println!("Registered guild commands for {gid}."),
                    Err(e) => eprintln!("Failed to register guild commands: {e}"),
                }
                // Clear any lingering global commands so they don't show up as
                // duplicates alongside the guild ones.
                match Command::set_global_commands(&ctx.http, vec![]).await {
                    Ok(_) => println!("Cleared global commands."),
                    Err(e) => eprintln!("Failed to clear global commands: {e}"),
                }
            }
            None => match Command::set_global_commands(&ctx.http, commands).await {
                Ok(_) => println!("Registered global commands (may take up to an hour to appear)."),
                Err(e) => eprintln!("Failed to register global commands: {e}"),
            },
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        if let Interaction::Command(command) = interaction {
            if !matches!(command.data.name.as_str(), "rota" | "session" | "potsworth") {
                return;
            }
            let (content, ephemeral) = self.handle(&command);
            let message = CreateInteractionResponseMessage::new()
                .content(content)
                .ephemeral(ephemeral);
            let response = CreateInteractionResponse::Message(message);
            if let Err(e) = command.create_response(&ctx.http, response).await {
                eprintln!("Failed to respond to interaction: {e}");
            }
        }
    }

    async fn message(&self, ctx: Context, msg: Message) {
        let bot_id = self.bot_id.load(Ordering::Relaxed);
        // Not ready yet, or the bot's own message — leave it be.
        if bot_id == 0 || msg.author.id.get() == bot_id {
            return;
        }
        // Only in a server, and only if Potsworth is actually tagged.
        let Some(guild_id) = msg.guild_id else {
            return;
        };
        if !msg.mentions.iter().any(|u| u.id.get() == bot_id) {
            return;
        }
        // Once bound to the master's channel, only react there.
        {
            let store = self.store.lock().unwrap();
            if let Some(ch) = store.rota(guild_id.get()).and_then(|r| r.master_channel()) {
                if msg.channel_id.get() != ch {
                    return;
                }
            }
        }
        // React with a cup of green tea whenever Potsworth is tagged (this also
        // covers replies that ping him).
        if let Err(e) = msg
            .react(&ctx.http, ReactionType::Unicode("🍵".to_string()))
            .await
        {
            eprintln!("Failed to add reaction: {e}");
        }
    }
}

/// Handle the `/potsworth` subcommands.
fn handle_potsworth(
    rota: &mut Rota,
    sub: &str,
    opts: &[CommandDataOption],
    command: &CommandInteraction,
    bot_id: u64,
) -> String {
    match sub {
        "add" => {
            let Some((id, name)) = user_option(opts, "master", command) else {
                return "You need to specify a user.".to_string();
            };
            // Once a master is set for this server, that's final.
            if let Some(existing) = rota.master() {
                return format!("{} is Potsworth's only master!", existing.mention());
            }
            if bot_id != 0 && id == bot_id {
                return "🤵🏻‍♂️ A butler cannot serve himself, I'm afraid.".to_string();
            }
            let channel = command.channel_id.get();
            let _ = rota.set_master(Member { id, name }, channel);
            format!(
                "At your service. <@{id}> is now my one and only master. \
                 I shall attend to my duties here in <#{channel}>. 🤵🏻‍♂️☕"
            )
        }

        other => format!("Unknown subcommand `{other}`."),
    }
}

/// Pull a `User`-typed option out of a subcommand's options, returning the
/// user's id and best display name.
fn user_option(
    opts: &[CommandDataOption],
    name: &str,
    command: &CommandInteraction,
) -> Option<(u64, String)> {
    let uid = opts.iter().find(|o| o.name == name).and_then(|o| match o.value {
        CommandDataOptionValue::User(uid) => Some(uid),
        _ => None,
    })?;

    let display = command
        .data
        .resolved
        .users
        .get(&uid)
        .map(|u| u.global_name.clone().unwrap_or_else(|| u.name.clone()))
        .unwrap_or_else(|| format!("User {}", uid.get()));

    Some((uid.get(), display))
}

/// Pull a `String`-typed option out of a subcommand's options.
fn string_option(opts: &[CommandDataOption], name: &str) -> Option<String> {
    opts.iter().find(|o| o.name == name).and_then(|o| match &o.value {
        CommandDataOptionValue::String(s) => Some(s.clone()),
        _ => None,
    })
}

/// Pull a `Boolean`-typed option, defaulting to `false` when absent.
fn bool_option(opts: &[CommandDataOption], name: &str) -> bool {
    opts.iter().find(|o| o.name == name).is_some_and(|o| {
        matches!(o.value, CommandDataOptionValue::Boolean(true))
    })
}

/// Parse a user-supplied `YYYY/MM/DD` date, tolerating surrounding whitespace.
fn parse_date(raw: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(raw.trim(), "%Y/%m/%d").ok()
}

/// Canonical `YYYY/MM/DD` string for a date, used as the stored key.
fn iso(date: NaiveDate) -> String {
    date.format("%Y/%m/%d").to_string()
}

/// Human-friendly rendering, e.g. `Sat 22 Aug 2026`.
fn pretty(date: NaiveDate) -> String {
    date.format("%a %d %b %Y").to_string()
}

/// Human-friendly rendering from a stored ISO string, falling back to the raw
/// text if it somehow doesn't parse.
fn pretty_iso(date: &str) -> String {
    parse_date(date).map(pretty).unwrap_or_else(|| date.to_string())
}

/// Build the `/rota` command with all of its subcommands.
fn build_rota_command() -> CreateCommand {
    let user_opt = |desc: &str| {
        CreateCommandOption::new(CommandOptionType::User, "user", desc).required(true)
    };

    CreateCommand::new("rota")
        .description("Manage the D&D tea rota")
        .add_option(CreateCommandOption::new(
            CommandOptionType::SubCommand,
            "list",
            "Show the rota order and whose turn is next",
        ))
        .add_option(CreateCommandOption::new(
            CommandOptionType::SubCommand,
            "whose_turn",
            "Show whose turn it is to bring tea",
        ))
        .add_option(
            CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "add",
                "Add a person to the rota",
            )
            .add_sub_option(user_opt("Person to add")),
        )
        .add_option(
            CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "remove",
                "Remove a person from the rota",
            )
            .add_sub_option(user_opt("Person to remove")),
        )
        .add_option(CreateCommandOption::new(
            CommandOptionType::SubCommand,
            "next",
            "Mark this session done and advance to the next person",
        ))
        .add_option(
            CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "set_next",
                "Set who is up next in the rota",
            )
            .add_sub_option(user_opt("Person who is up next")),
        )
        .add_option(CreateCommandOption::new(
            CommandOptionType::SubCommand,
            "clear",
            "Remove everyone from the rota",
        ))
}

/// Build the `/session` command with all of its subcommands.
fn build_session_command() -> CreateCommand {
    let date_opt = || {
        CreateCommandOption::new(CommandOptionType::String, "date", "Date as YYYY/MM/DD")
            .required(true)
    };

    CreateCommand::new("session")
        .description("Schedule and track D&D session dates")
        .add_option(
            CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "add",
                "Schedule a session; assigns the next person in the rota to tea",
            )
            .add_sub_option(date_opt())
            .add_sub_option(CreateCommandOption::new(
                CommandOptionType::String,
                "note",
                "Optional note (e.g. chapter or location)",
            ))
            .add_sub_option(CreateCommandOption::new(
                CommandOptionType::Boolean,
                "skip",
                "Split / no rota: no one is on tea and no turn is used",
            )),
        )
        .add_option(CreateCommandOption::new(
            CommandOptionType::SubCommand,
            "list",
            "List upcoming sessions and who's on tea",
        ))
        .add_option(CreateCommandOption::new(
            CommandOptionType::SubCommand,
            "next",
            "Show the next upcoming session",
        ))
        .add_option(CreateCommandOption::new(
            CommandOptionType::SubCommand,
            "history",
            "List past sessions (most recent first)",
        ))
        .add_option(
            CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "remove",
                "Remove a scheduled session by date",
            )
            .add_sub_option(date_opt()),
        )
        .add_option(
            CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "reschedule",
                "Move a scheduled session to a new date",
            )
            .add_sub_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "from",
                    "Current date of the session (YYYY/MM/DD)",
                )
                .required(true),
            )
            .add_sub_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "to",
                    "New date for the session (YYYY/MM/DD)",
                )
                .required(true),
            ),
        )
        .add_option(
            CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "assign",
                "Change who's on tea for a session (moves them to the back of the rota)",
            )
            .add_sub_option(date_opt())
            .add_sub_option(CreateCommandOption::new(
                CommandOptionType::User,
                "user",
                "Person to put on tea for that session",
            ))
            .add_sub_option(CreateCommandOption::new(
                CommandOptionType::Boolean,
                "skip",
                "Split / no rota this session instead of assigning someone",
            )),
        )
}

/// Build the `/potsworth` command.
fn build_potsworth_command() -> CreateCommand {
    CreateCommand::new("potsworth")
        .description("Potsworth, at your service")
        .add_option(
            CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "add",
                "Assign Potsworth's master — can only ever be done once per server",
            )
            .add_sub_option(
                CreateCommandOption::new(
                    CommandOptionType::User,
                    "master",
                    "The user Potsworth will serve",
                )
                .required(true),
            ),
        )
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    let token = std::env::var("DISCORD_TOKEN")
        .expect("Set the DISCORD_TOKEN environment variable (see .env.example).");

    let data_path = std::env::var("DATA_PATH").unwrap_or_else(|_| DATA_PATH.to_string());
    let store = Store::load(&data_path);

    let handler = Handler {
        store: Arc::new(Mutex::new(store)),
        bot_id: AtomicU64::new(0),
    };

    // GUILD_MESSAGES (non-privileged) lets us receive message events so we can
    // react when Potsworth is tagged. Detecting mentions needs no MESSAGE_CONTENT
    // intent — the mentions list is always delivered.
    let intents = GatewayIntents::GUILD_MESSAGES;

    let mut client = Client::builder(&token, intents)
        .event_handler(handler)
        .await
        .expect("Failed to create Discord client");

    if let Err(e) = client.start().await {
        eprintln!("Client error: {e}");
    }
}
