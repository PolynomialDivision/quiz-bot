-- Quiz-bot SQLite schema (applied once on startup via execute_batch)
-- All CREATE statements use IF NOT EXISTS — safe to re-run on every boot.

CREATE TABLE IF NOT EXISTS players (
    user_id       TEXT PRIMARY KEY,
    display_name  TEXT,
    first_seen_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    last_seen_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE IF NOT EXISTS rounds (
    id                         INTEGER PRIMARY KEY,
    room_id                    TEXT    NOT NULL,
    started_at                 TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    ended_at                   TEXT,
    n_questions_planned        INTEGER NOT NULL,
    n_questions_actual         INTEGER,
    triggered_by               TEXT    NOT NULL,
    config_answer_timeout      INTEGER,
    config_questions_per_round INTEGER,
    config_timezone            TEXT,
    config_category_id         INTEGER,
    config_difficulty          TEXT
);

CREATE TABLE IF NOT EXISTS questions (
    id                  INTEGER PRIMARY KEY,
    round_id            INTEGER NOT NULL REFERENCES rounds(id) ON DELETE CASCADE,
    question_num        INTEGER NOT NULL,
    matrix_event_id     TEXT,
    category            TEXT    NOT NULL,
    difficulty          TEXT    NOT NULL,
    question_text       TEXT    NOT NULL,
    choices             TEXT    NOT NULL,   -- JSON array ["a","b","c","d"]
    correct_index       INTEGER NOT NULL,
    correct_answer_text TEXT    NOT NULL,
    asked_at            TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    answer_timeout_secs INTEGER NOT NULL,
    n_answers_received  INTEGER,
    n_correct           INTEGER,
    n_wrong             INTEGER
);

CREATE TABLE IF NOT EXISTS answers (
    id             INTEGER PRIMARY KEY,
    question_id    INTEGER NOT NULL REFERENCES questions(id) ON DELETE CASCADE,
    round_id       INTEGER NOT NULL,
    user_id        TEXT    NOT NULL,
    choice_index   INTEGER NOT NULL,
    is_correct     INTEGER NOT NULL,   -- 0/1
    source         TEXT    NOT NULL DEFAULT 'unknown',
    submitted_at   TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    changed_answer INTEGER NOT NULL DEFAULT 0,   -- 0/1
    UNIQUE (question_id, user_id)
);

CREATE TABLE IF NOT EXISTS round_scores (
    round_id      INTEGER NOT NULL REFERENCES rounds(id) ON DELETE CASCADE,
    user_id       TEXT    NOT NULL,
    correct_count INTEGER NOT NULL DEFAULT 0,
    total_count   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (round_id, user_id)
);

CREATE TABLE IF NOT EXISTS bot_kv (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_answers_user_id   ON answers (user_id);
CREATE INDEX IF NOT EXISTS idx_answers_round_id  ON answers (round_id);
CREATE INDEX IF NOT EXISTS idx_answers_correct   ON answers (user_id, is_correct);
CREATE INDEX IF NOT EXISTS idx_answers_speed     ON answers (question_id, submitted_at);
CREATE INDEX IF NOT EXISTS idx_questions_round   ON questions (round_id);
CREATE INDEX IF NOT EXISTS idx_questions_text    ON questions (question_text);
CREATE INDEX IF NOT EXISTS idx_round_scores_user ON round_scores (user_id);
CREATE INDEX IF NOT EXISTS idx_rounds_started    ON rounds (started_at);
