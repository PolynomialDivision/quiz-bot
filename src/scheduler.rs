use chrono::Timelike;
use chrono_tz::Tz;
use matrix_sdk::Client;
use tracing::{error, info, warn};

use crate::{BotContext, config::ScheduleConfig};

/// Background task: wake up every 60 seconds and check whether it's time to
/// fire any configured quiz slot.
pub async fn run(ctx: BotContext, client: Client) {
    info!("Quiz scheduler started");
    loop {
        if let Err(e) = tick(&ctx, &client).await {
            error!("Scheduler error: {e}");
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
    }
}

async fn tick(ctx: &BotContext, client: &Client) -> anyhow::Result<()> {
    let tz: Tz  = ctx.config.schedule.timezone.parse().unwrap_or(chrono_tz::UTC);
    let local_now  = chrono::Utc::now().with_timezone(&tz);
    let local_date = local_now.date_naive();
    let now_hour   = local_now.hour();
    let now_minute = local_now.minute();
    let offset     = ctx.config.schedule.reminder_before_secs as i64;

    for time_str in &ctx.config.schedule.quiz_times {
        let (qh, qm) = match ScheduleConfig::parse_quiz_time(time_str) {
            Some(t) => t,
            None => {
                warn!("Invalid quiz_times entry {:?} — skipping", time_str);
                continue;
            }
        };

        // Fire this many seconds before the quiz so the reminder lands on time.
        let quiz_secs = (qh * 3600 + qm * 60) as i64;
        let fire_secs = (quiz_secs - offset).rem_euclid(86400);
        let fire_hour = (fire_secs / 3600) as u32;
        let fire_min  = ((fire_secs % 3600) / 60) as u32;

        if now_hour != fire_hour || now_minute != fire_min {
            continue;
        }

        // Already fired this slot today?
        {
            let state = ctx.state.lock().await;
            if state.last_quiz_dates.get(time_str.as_str()) == Some(&local_date) {
                continue;
            }
        }

        // Another quiz round already running?
        {
            let aq = ctx.active_quiz.lock().await;
            if aq.is_some() {
                warn!(
                    "Scheduler: fire time for slot {time_str} \
                     but a quiz is already in progress — skipping"
                );
                continue;
            }
        }

        info!(
            "Scheduled quiz firing for slot {time_str} \
             (fire at {fire_hour}:{fire_min:02}, quiz at {qh}:{qm:02})",
        );
        let ctx2    = ctx.clone();
        let client2 = client.clone();
        let slot    = time_str.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::quiz::start_quiz(ctx2, client2, false, Some(slot)).await {
                error!("Quiz error: {e}");
            }
        });
    }

    Ok(())
}
