use std::{collections::HashSet, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use mxbot_common::{
    admin::Dispatch,
    matrix_sdk::{
        self,
        deserialized_responses::EncryptionInfo,
        ruma::{
            events::{
                reaction::OriginalSyncReactionEvent,
                room::message::{MessageType, OriginalSyncRoomMessageEvent},
            },
            OwnedRoomId, OwnedUserId,
        },
        Client, Room, RoomState,
    },
    send::{in_thread, thread_root},
    Bot,
};
use tokio::sync::Mutex;
use tracing::{error, info};

mod commands;
mod config;
mod db;
mod explainer;
mod fetcher;
mod format;
mod groq;
mod leaderboard;
mod quiz;
mod scheduler;
mod state;
mod triviaqa;

use config::Config;
use state::State;

#[derive(Clone)]
pub struct BotContext {
    pub state: Arc<Mutex<State>>,
    pub state_path: PathBuf,
    pub config: Arc<Config>,
    pub admin_users: HashSet<OwnedUserId>,
    pub room_id: OwnedRoomId,
    pub active_quiz: Arc<Mutex<Option<quiz::ActiveQuiz>>>,
    pub quiz_run_lock: Arc<Mutex<()>>,
    pub client: Client,
    pub db: Arc<db::Db>,
}

/// Run one `!command` and build the reply, or `None` when there is nothing
/// to say. Command replies (leaderboards, speed stats, …) embed raw mxids
/// for any player they mention — resolve them to display names via the same
/// mention pipeline the round score uses.
async fn command_reply(
    ctx: &BotContext,
    sender: &OwnedUserId,
    body: &str,
) -> Option<matrix_sdk::ruma::events::room::message::RoomMessageEventContent> {
    match commands::handle(ctx, sender, body).await {
        Ok(Some(reply)) => {
            let names = ctx.db.player_display_names().await.unwrap_or_default();
            Some(format::mentionify_with_names(&reply, &names))
        }
        Err(e) if e.to_string() == "__not_admin__" => Some(format::mentionify(
            "❌ This command requires admin privileges.",
        )),
        Ok(None) => None,
        Err(e) => {
            error!("Command error: {e}");
            None
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    mxbot_common::logging::init("quiz_bot");

    let config: Config =
        mxbot_common::config::load_toml(&mxbot_common::config::config_path_from_args())?;
    config
        .schedule
        .timezone
        .parse::<chrono_tz::Tz>()
        .with_context(|| format!("Invalid schedule timezone {:?}", config.schedule.timezone))?;
    let config = Arc::new(config);

    let store_path = mxbot_common::config::store_path_from_env();
    tokio::fs::create_dir_all(&store_path).await?;

    // ── Database (SQLite, lives in store dir) ────────────────────────────────
    let db = db::Db::open(&store_path.join("quiz.db")).await?;
    db.migrate().await?;
    let db = Arc::new(db);

    // ── State (operational, non-analytics data) ───────────────────────────────
    let state_path = store_path.join("state.json");
    let mut st = State::load(&state_path).await?;
    if st.created_at.is_none() {
        st.created_at = Some(chrono::Utc::now());
        st.save(&state_path).await?;
    }
    let state = Arc::new(Mutex::new(st));

    let room_id =
        mxbot_common::rooms::parse_room_id("[schedule] room_id", &config.schedule.room_id)?;

    let bot = Bot::builder("quiz-bot", env!("CARGO_PKG_VERSION"))
        .store_path(&store_path)
        .admin_help(
            "!startquiz · !schedulequiz · !cancelquiz · !prefetch · !resetstats · !catconfig",
        )
        .start(&config.matrix, &config.security)
        .await?;
    let client = bot.client.clone();
    let bot_user_id = bot.user_id.clone();

    let ctx = BotContext {
        state,
        state_path,
        config: Arc::clone(&config),
        admin_users: bot.admins().clone(),
        room_id: room_id.clone(),
        active_quiz: Arc::new(Mutex::new(None)),
        quiz_run_lock: Arc::new(Mutex::new(())),
        client: client.clone(),
        db,
    };

    // ── Message / command handler ─────────────────────────────────────────────
    client.add_event_handler({
        let ctx = ctx.clone();
        let bot = bot.clone();
        move |ev: OriginalSyncRoomMessageEvent,
              room: Room,
              client: Client,
              encryption: Option<EncryptionInfo>| {
            let ctx = ctx.clone();
            let bot = bot.clone();
            async move {
                if ev.sender == bot.user_id || room.state() != RoomState::Joined {
                    return;
                }
                match bot.admin.handle(&room, &ev, encryption.as_ref()).await {
                    Dispatch::Handled => return,
                    Dispatch::AdminDm => {
                        // Admin commands sent privately are answered privately.
                        let MessageType::Text(text) = &ev.content.msgtype else {
                            return;
                        };
                        if let Some(reply) = command_reply(&ctx, &ev.sender, text.body.trim()).await
                        {
                            room.send(reply).await.ok();
                        }
                        return;
                    }
                    Dispatch::Continue => {}
                }
                if room.room_id() != ctx.room_id {
                    return;
                }

                let MessageType::Text(ref text) = ev.content.msgtype else {
                    return;
                };
                let body = text.body.trim();
                if !body.starts_with('!') {
                    return;
                }

                // Quiz answer shorthand: !a / !b / !c / !d
                let answer_index: Option<u8> = match body.to_lowercase().as_str() {
                    "!a" => Some(0),
                    "!b" => Some(1),
                    "!c" => Some(2),
                    "!d" => Some(3),
                    _ => None,
                };
                if let Some(choice_index) = answer_index {
                    let user = ev.sender.as_str().to_owned();
                    let mut aq = ctx.active_quiz.lock().await;
                    if let Some(quiz) = aq.as_mut() {
                        quiz.record_answer(user, choice_index, "text");
                    }
                    return;
                }

                // Regular commands.
                if let Some(reply) = command_reply(&ctx, &ev.sender, body).await {
                    if let Some(r) = client.get_room(&ctx.room_id) {
                        r.send(in_thread(reply, thread_root(&ev), ev.event_id.clone()))
                            .await
                            .ok();
                    }
                }
            }
        }
    });

    // ── Reaction handler — quiz answers ───────────────────────────────────────
    client.add_event_handler({
        let ctx = ctx.clone();
        let bot_user_id = bot_user_id.clone();
        move |ev: OriginalSyncReactionEvent, room: Room, _client: Client| {
            let ctx = ctx.clone();
            let bot_user_id = bot_user_id.clone();
            async move {
                // Routine, high-volume, and uninteresting: the bot's own
                // tap-to-answer reactions and anything outside the quiz
                // room. Not logged — logging every one would drown out the
                // reactions that actually matter below.
                if ev.sender == bot_user_id {
                    return;
                }
                if room.state() != RoomState::Joined {
                    return;
                }
                if room.room_id() != ctx.room_id {
                    return;
                }

                let key = ev.content.relates_to.key.as_str();
                let choice_index = match key {
                    "🇦" => 0u8,
                    "🇧" => 1,
                    "🇨" => 2,
                    "🇩" => 3,
                    // Not an answer reaction (e.g. 👍 on some other message) —
                    // this is the overwhelming majority of room reactions, so
                    // stays silent too.
                    _ => return,
                };

                let reacted_to = ev.content.relates_to.event_id.clone();
                let sender = ev.sender.as_str().to_owned();

                // From here on, a real answer-shaped reaction was received —
                // every outcome is logged so a lost answer can be traced.
                let mut aq = ctx.active_quiz.lock().await;
                let result =
                    quiz::apply_reaction(&mut aq, &reacted_to, sender.clone(), choice_index);
                match result {
                    quiz::ReactionResult::Accepted(quiz::AnswerOutcome::New) => info!(
                        reaction_event_id = %ev.event_id,
                        reacted_to = %reacted_to,
                        sender = %sender,
                        key,
                        received_at = %ev.origin_server_ts.get(),
                        "Reaction accepted"
                    ),
                    quiz::ReactionResult::Accepted(quiz::AnswerOutcome::Replaced { previous }) => {
                        info!(
                            reaction_event_id = %ev.event_id,
                            reacted_to = %reacted_to,
                            sender = %sender,
                            key,
                            received_at = %ev.origin_server_ts.get(),
                            previous_choice = previous,
                            "Reaction accepted — replaced previous answer"
                        )
                    }
                    quiz::ReactionResult::Accepted(quiz::AnswerOutcome::Unchanged) => info!(
                        reaction_event_id = %ev.event_id,
                        reacted_to = %reacted_to,
                        sender = %sender,
                        key,
                        "Reaction ignored — duplicate of the sender's existing answer"
                    ),
                    quiz::ReactionResult::WrongQuestion => info!(
                        reaction_event_id = %ev.event_id,
                        reacted_to = %reacted_to,
                        sender = %sender,
                        key,
                        "Reaction ignored — targets a different (stale) question"
                    ),
                    quiz::ReactionResult::NoActiveQuestion => info!(
                        reaction_event_id = %ev.event_id,
                        reacted_to = %reacted_to,
                        sender = %sender,
                        key,
                        "Reaction ignored — no quiz question is currently active"
                    ),
                }
            }
        }
    });

    // ── Initial sync ──────────────────────────────────────────────────────────
    bot.initial_sync().await;
    info!("Initial sync complete");

    tokio::spawn(triviaqa::ensure_ingested(ctx.clone()));
    tokio::spawn(scheduler::run(ctx, client.clone()));

    bot.sync_forever().await
}
