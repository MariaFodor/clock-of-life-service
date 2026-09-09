-- ONT-01 introduced a fifth role: `marker`.
--
-- A marker predicts and explains but is never recommended — long sleep and mobility limitation are
-- signs of accumulated damage rather than things a person can usefully be told to change. The
-- shipped ontology classifies them that way, so the database has to accept it; until now the CHECK
-- constraint knew only four roles and the seeds silently disagreed with the model.
ALTER TABLE feature DROP CONSTRAINT IF EXISTS feature_role_check;
ALTER TABLE feature ADD CONSTRAINT feature_role_check
  CHECK (role IN ('lever', 'manage', 'context', 'baseline', 'marker'));
