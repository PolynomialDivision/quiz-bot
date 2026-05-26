-- Fix total_count in round_scores to reflect the actual number of questions
-- in each round, not just the questions each user happened to answer.
--
-- A user who answered 1 out of 5 questions previously had total_count = 1;
-- this corrects it to 5 so the leaderboard percentages are accurate.
--
-- Safe to re-run on every startup: the WHERE clause leaves already-correct
-- rows untouched.
UPDATE round_scores
SET total_count = (
    SELECT COALESCE(r.n_questions_actual, r.n_questions_planned)
    FROM rounds r
    WHERE r.id = round_scores.round_id
)
WHERE total_count < (
    SELECT COALESCE(r.n_questions_actual, r.n_questions_planned)
    FROM rounds r
    WHERE r.id = round_scores.round_id
);
