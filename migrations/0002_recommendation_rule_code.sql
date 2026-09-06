-- Give recommendation_rule a stable, human-readable `code` so the startup seed can upsert rules
-- idempotently (mirrors question.code). The table is empty at this point, so NOT NULL is safe.

ALTER TABLE recommendation_rule ADD COLUMN code TEXT NOT NULL;
ALTER TABLE recommendation_rule ADD CONSTRAINT recommendation_rule_code_key UNIQUE (code);
