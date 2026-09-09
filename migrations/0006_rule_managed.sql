-- Distinguish seed-owned rules from admin-authored ones.
--
-- Reconciliation has to deactivate a rule that disappears from the seed — that is what "reconcile
-- against desired state" means, and without it a withdrawn rule keeps firing for ever. But it must
-- only do that to rows the seed owns: an admin who creates a rule through /api/admin/rules would
-- otherwise watch it silently die at the next restart, with its creation audited and its removal
-- not. Ownership is now recorded, instead of being guessed from a name prefix that only happened to
-- match what the test suite calls its fixtures.
ALTER TABLE recommendation_rule
  ADD COLUMN IF NOT EXISTS managed BOOLEAN NOT NULL DEFAULT false;

-- Existing rows carrying a seed code are seed-owned; anything else predates this column and is
-- treated as admin-authored, which is the safe direction to be wrong in.
UPDATE recommendation_rule SET managed = true
 WHERE code IN ('quit_smoking','increase_activity','manage_hypertension','manage_diabetes',
                'reduce_waist','manage_respiratory','reduce_alcohol','improve_diet',
                'reduce_sitting','manage_stress','review_long_sleep');
