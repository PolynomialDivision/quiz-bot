-- Local corpus of TriviaQA question/answer pairs, ingested once from a
-- configured dataset URL (see src/triviaqa.rs) rather than re-downloaded
-- on every startup. Distractors (wrong answers) are generated on demand by
-- the LLM and cached here so a question that later cycles back in (after
-- its reuse cooldown expires — see questions.normalized_text) doesn't pay
-- for a fresh LLM call.
--
-- Deliberately separate from `questions` (the "what has this bot actually
-- asked" history table, shared with OpenTDB): this table is a static
-- reference corpus, not gameplay history, so `!resetstats` must not touch
-- it.
-- `category_group` and `difficulty` reuse the bot's existing OpenTDB-derived
-- vocabulary (see `fetcher::CATEGORY_GROUPS` and OpenTDB's own
-- easy/medium/hard) rather than a TriviaQA-specific scheme, so TriviaQA
-- questions plug into the same category filtering / difficulty selection /
-- round diversity logic as OpenTDB ones. Both are classified by the LLM
-- (same call that generates distractors) the first time a row is sampled,
-- and cached here — NULL until then.
CREATE TABLE IF NOT EXISTS triviaqa_pool (
    id                       INTEGER PRIMARY KEY,
    question_text            TEXT    NOT NULL UNIQUE,
    normalized_text          TEXT    NOT NULL,
    correct_answer           TEXT    NOT NULL,
    aliases                  TEXT    NOT NULL DEFAULT '[]', -- JSON array
    category_group           TEXT,                          -- one of fetcher::CATEGORY_GROUPS; NULL until classified
    difficulty               TEXT,                          -- "easy" | "medium" | "hard"; NULL until classified
    distractors              TEXT,                          -- JSON array; NULL until generated
    distractors_generated_at TEXT,
    imported_at              TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_triviaqa_pool_normalized ON triviaqa_pool (normalized_text);
