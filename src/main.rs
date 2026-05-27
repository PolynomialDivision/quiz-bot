use std::{collections::HashSet, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use matrix_sdk::{
    Client, Room, RoomState,
    config::SyncSettings,
    ruma::{
        OwnedEventId, OwnedRoomId, OwnedServerName, OwnedUserId, RoomOrAliasId,
        api::client::filter::FilterDefinition,
        events::{
            key::verification::request::ToDeviceKeyVerificationRequestEvent,
            reaction::OriginalSyncReactionEvent,
            relation::Thread,
            room::{
                member::StrippedRoomMemberEvent,
                message::{
                    MessageType, OriginalSyncRoomMessageEvent,
                    Relation, RoomMessageEventContent,
                },
            },
        },
    },
};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

mod commands;
mod config;
mod db;
mod explainer;
mod fetcher;
mod format;
mod quiz;
mod scheduler;
mod state;

use config::Config;
use state::State;

/// Build a thread reply.
/// `root`     — the thread root event (m.thread event_id).
/// `reply_to` — the specific event being quoted (m.in_reply_to).
///              Pass `ev.event_id` so the reply quotes the command message,
///              not the thread root.
fn thread_reply(
    text:     &str,
    root:     OwnedEventId,
    reply_to: OwnedEventId,
) -> RoomMessageEventContent {
    let mut content = format::mentionify(text);
    content.relates_to = Some(Relation::Thread(Thread::reply(root, reply_to)));
    content
}

#[derive(Clone)]
pub struct BotContext {
    pub state:       Arc<Mutex<State>>,
    pub state_path:  PathBuf,
    pub config:      Arc<Config>,
    pub admin_users: HashSet<OwnedUserId>,
    pub room_id:     OwnedRoomId,
    pub active_quiz: Arc<Mutex<Option<quiz::ActiveQuiz>>>,
    pub client:      Client,
    pub db:          Arc<db::Db>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "quiz_bot=info,matrix_sdk=warn".parse().unwrap()),
        )
        .init();

    let config_path = std::env::args()
        .find(|a| a.ends_with(".toml"))
        .unwrap_or_else(|| "config.toml".to_owned());
    let config: Config = toml::from_str(
        &std::fs::read_to_string(&config_path)
            .with_context(|| format!("Reading config {config_path}"))?,
    )
    .context("Parsing config")?;
    let config = Arc::new(config);

    let store_path = PathBuf::from(
        std::env::var("STORE_PATH").unwrap_or_else(|_| "store".to_owned()),
    );
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

    let admin_users: HashSet<OwnedUserId> = config.security.admin_users
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect();

    let allowed_inviters: HashSet<String> = config.security.allowed_inviters
        .iter()
        .cloned()
        .collect();

    let room_id: OwnedRoomId = config.schedule.room_id
        .parse()
        .context("Invalid room_id in [schedule]")?;

    let (client, bot_user_id) = mxbot_common::session::build_and_restore(
        &config.matrix,
        &store_path,
        config.security.encryption_strategy.clone().into(),
    )
    .await?;

    let ctx = BotContext {
        state,
        state_path,
        config:      Arc::clone(&config),
        admin_users,
        room_id:     room_id.clone(),
        active_quiz: Arc::new(Mutex::new(None)),
        client:      client.clone(),
        db,
    };

    // ── Invite handler ────────────────────────────────────────────────────────
    client.add_event_handler({
        let allowed_inviters = allowed_inviters.clone();
        let bot_user_id      = bot_user_id.clone();
        move |ev: StrippedRoomMemberEvent, room: Room, client: Client| {
            let allowed_inviters = allowed_inviters.clone();
            let bot_user_id      = bot_user_id.clone();
            async move {
                if ev.state_key != bot_user_id { return; }
                if !allowed_inviters.is_empty()
                    && !allowed_inviters.contains(ev.sender.as_str())
                {
                    warn!("Rejecting invite from {}", ev.sender);
                    room.leave().await.ok();
                    return;
                }
                let room_id = room.room_id().to_owned();
                let mut via: Vec<OwnedServerName> = vec![ev.sender.server_name().to_owned()];
                if let Some(s) = room_id.server_name() {
                    let s = s.to_owned();
                    if !via.contains(&s) { via.push(s); }
                }
                if let Ok(roa) = RoomOrAliasId::parse(room_id.as_str()) {
                    if let Err(e) = client.join_room_by_id_or_alias(&roa, &via).await {
                        warn!("Join failed: {e}");
                    }
                }
            }
        }
    });

    // ── Message / command handler ─────────────────────────────────────────────
    client.add_event_handler({
        let ctx         = ctx.clone();
        let bot_user_id = bot_user_id.clone();
        move |ev: OriginalSyncRoomMessageEvent, room: Room, client: Client| {
            let ctx         = ctx.clone();
            let bot_user_id = bot_user_id.clone();
            async move {
                if ev.sender == bot_user_id           { return; }
                if room.state() != RoomState::Joined  { return; }
                if room.room_id() != ctx.room_id      { return; }

                let MessageType::Text(ref text) = ev.content.msgtype else { return; };
                let body = text.body.trim();
                if !body.starts_with('!') { return; }

                let thread_root = match &ev.content.relates_to {
                    Some(Relation::Thread(t)) => t.event_id.clone(),
                    _                         => ev.event_id.clone(),
                };

                // Quiz answer shorthand: !a / !b / !c / !d
                let answer_index: Option<u8> = match body.to_lowercase().as_str() {
                    "!a" => Some(0),
                    "!b" => Some(1),
                    "!c" => Some(2),
                    "!d" => Some(3),
                    _    => None,
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
                match commands::handle(&ctx, &ev.sender, body).await {
                    Ok(Some(reply)) => {
                        if let Some(r) = client.get_room(&ctx.room_id) {
                            r.send(thread_reply(&reply, thread_root, ev.event_id.clone())).await.ok();
                        }
                    }
                    Err(e) if e.to_string() == "__not_admin__" => {
                        if let Some(r) = client.get_room(&ctx.room_id) {
                            r.send(thread_reply(
                                "❌ This command requires admin privileges.",
                                thread_root,
                                ev.event_id.clone(),
                            )).await.ok();
                        }
                    }
                    Ok(None) => {}
                    Err(e)   => error!("Command error: {e}"),
                }
            }
        }
    });

    // ── Reaction handler — quiz answers ───────────────────────────────────────
    client.add_event_handler({
        let ctx         = ctx.clone();
        let bot_user_id = bot_user_id.clone();
        move |ev: OriginalSyncReactionEvent, room: Room, _client: Client| {
            let ctx         = ctx.clone();
            let bot_user_id = bot_user_id.clone();
            async move {
                if ev.sender == bot_user_id          { return; }
                if room.state() != RoomState::Joined { return; }
                if room.room_id() != ctx.room_id     { return; }

                let choice_index = match ev.content.relates_to.key.as_str() {
                    "🇦" => 0u8, "🇧" => 1, "🇨" => 2, "🇩" => 3, _ => return,
                };

                let reacted_to = ev.content.relates_to.event_id.as_str().to_owned();
                let user       = ev.sender.as_str().to_owned();

                let mut aq = ctx.active_quiz.lock().await;
                if let Some(quiz) = aq.as_mut() {
                    if quiz.event_id.as_str() == reacted_to {
                        quiz.record_answer(user, choice_index, "reaction");
                    }
                }
            }
        }
    });

    // ── Verification handler ──────────────────────────────────────────────────
    client.add_event_handler({
        let reset_allowed: Arc<Mutex<HashSet<OwnedUserId>>> =
            Arc::new(Mutex::new(HashSet::new()));
        move |ev: ToDeviceKeyVerificationRequestEvent, client: Client| {
            let reset = Arc::clone(&reset_allowed);
            async move {
                if let Some(req) = client
                    .encryption()
                    .get_verification_request(&ev.sender, &ev.content.transaction_id)
                    .await
                {
                    tokio::spawn(mxbot_common::verify::handle_verification_request(
                        client, reset, req,
                    ));
                }
            }
        }
    });

    // ── Initial sync ──────────────────────────────────────────────────────────
    let filter = FilterDefinition::with_lazy_loading();
    client
        .sync_once(SyncSettings::default().filter(filter.into()))
        .await
        .context("Initial sync failed")?;
    info!("Initial sync complete");

    tokio::spawn(scheduler::run(ctx, client.clone()));

    loop {
        match client.sync(SyncSettings::default()).await {
            Ok(()) => warn!("Sync loop exited — reconnecting"),
            Err(e) => {
                warn!("Sync error: {e} — reconnecting in 5s");
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            }
        }
    }
}
