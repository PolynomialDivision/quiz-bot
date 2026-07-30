-- Monthly leaderboard delivery state and recent-category query support.
-- Safe to re-run on every startup.

CREATE TABLE IF NOT EXISTS monthly_leaderboard_posts (
    period          TEXT PRIMARY KEY, -- YYYY-MM in the configured timezone
    transaction_id  TEXT NOT NULL,
    claimed_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    posted_at       TEXT,
    matrix_event_id TEXT
);

CREATE INDEX IF NOT EXISTS idx_questions_recent
    ON questions (asked_at DESC, id DESC);
