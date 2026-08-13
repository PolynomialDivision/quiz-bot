-- Supports normalization-aware, cooldown-based question reuse (see
-- Db::migrate_normalized_text_column, Db::question_recently_asked).
-- The column itself is added and backfilled in Rust (SQLite has no
-- portable ADD COLUMN IF NOT EXISTS, and backfill needs punctuation
-- stripping that isn't practical in pure SQL). This file only adds the
-- index, which is safe to (re-)create once the column exists.
CREATE INDEX IF NOT EXISTS idx_questions_normalized ON questions (normalized_text);
