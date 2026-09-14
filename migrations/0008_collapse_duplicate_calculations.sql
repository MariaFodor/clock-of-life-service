-- Collapse the duplicate history rows that existed before the service stopped creating them.
--
-- Re-scoring an unchanged profile used to append a row per click, and a React ref was the only thing
-- discouraging it — so it died on every page reload. An owner reported six identical rows on My
-- Progress: same estimate, same day, "±0.0 yr since first", under a sentence promising the history
-- shows how the estimate moves AS THE ANSWERS CHANGE. `insert_calculation` now collapses a repeat of
-- whatever sits at the top of an account's history, under a per-account advisory lock. This removes
-- what accumulated before that.
--
-- A row is dropped only when it repeats the row immediately before it in its own account's history on
-- input_hash, model_version_id, estimate_years, relative_risk AND attributions. So:
--   * A -> A -> A collapses to one row.
--   * A -> B -> A keeps three: the reader changed something and changed it back, which the history
--     should show, and which the live rule also keeps.
--   * Same answers scored differently (a bundle or scoring change) keeps both, because the numbers
--     differ — the same reason the live CTE matches on them.
-- Anything this deletes is a row the current service would never have written.
--
-- THE MATCH IS STRICTER THAN THE LIVE RULE, on `attributions`, and that asymmetry is deliberate. The
-- live CTE matches on four columns; this matches on five. They are not the same act: the live rule
-- DECLINES TO WRITE a new evidence snapshot and leaves the old one intact, while a DELETE here would
-- DESTROY one already on disk. `attributions` is not a function of the other four — `estimate_route`
-- enriches scoring's why[] with study references read live from `study`/`feature_study`, which
-- `seed_studies` rebuilds on every boot, and `model_version_id` comes from `manifest.version`, so a
-- changed citation moves the snapshot without moving anything else. Measured on a development
-- database: 35 groups identical on the other four columns carried more than one distinct
-- `attributions` — one walks 1806 -> 2253 bytes at identical numbers as its references grow. Nothing
-- renders this today (the web types it `unknown`), but it is what /api/account/export hands a person
-- who asks what the model said about them, and it is the evidence half of a product built on
-- citations. A row carrying a snapshot no other row carries is not a duplicate.
--
-- THE FIRST OF EACH RUN SURVIVES, not the last, matching `insert_calculation`: it returns the id of
-- the existing top row rather than inserting, so the row that persists is the earliest of the run.
-- Keeping the last would move every collapsed calculation's created_at forward and rewrite when the
-- reader actually calculated.
--
-- SCENARIOS ARE REPOINTED BEFORE ANYTHING IS DELETED. `scenario.base_calculation_id` is
-- ON DELETE CASCADE (0001_init.sql), so deleting a duplicate would silently take any What-If saved
-- against it. A scenario built on a row that was a duplicate of the one before it is equally a
-- scenario on the survivor: identical inputs, identical model version, identical numbers.
--
-- This is the one DELETE against a table 0001_init.sql marks APPEND-ONLY. The marker is a convention
-- rather than a constraint, and the rows removed here are ones the append was never meant to make.

CREATE TEMP TABLE collapse_map AS
WITH ordered AS (
    SELECT
        id,
        account_id,
        created_at,
        input_hash,
        model_version_id,
        estimate_years,
        relative_risk,
        LAG(input_hash)       OVER w AS prev_hash,
        LAG(model_version_id) OVER w AS prev_model,
        LAG(estimate_years)   OVER w AS prev_years,
        LAG(relative_risk)    OVER w AS prev_rr,
        attributions,
        LAG(attributions)     OVER w AS prev_attr
    FROM calculation
    WINDOW w AS (PARTITION BY account_id ORDER BY created_at, id)
),
-- 1 where a row differs from its predecessor (a new run begins), 0 where it repeats it.
marked AS (
    SELECT
        id, account_id, created_at,
        CASE
            WHEN prev_hash  IS NOT DISTINCT FROM input_hash
             AND prev_model IS NOT DISTINCT FROM model_version_id
             AND prev_years IS NOT DISTINCT FROM estimate_years
             AND prev_rr    IS NOT DISTINCT FROM relative_risk
             AND prev_attr  IS NOT DISTINCT FROM attributions
            THEN 0 ELSE 1
        END AS starts_run
    FROM ordered
),
-- A running total of those flags numbers the runs within each account.
grouped AS (
    SELECT
        id, account_id, created_at,
        SUM(starts_run) OVER (PARTITION BY account_id ORDER BY created_at, id) AS run_id
    FROM marked
),
keepers AS (
    SELECT DISTINCT ON (account_id, run_id) account_id, run_id, id AS keep_id
    FROM grouped
    ORDER BY account_id, run_id, created_at, id
)
SELECT g.id AS drop_id, k.keep_id
FROM grouped g
JOIN keepers k ON k.account_id = g.account_id AND k.run_id = g.run_id
WHERE g.id <> k.keep_id;

-- Lock the doomed rows before touching anything. A concurrent INSERT into `scenario` takes FOR KEY
-- SHARE on its parent calculation, which conflicts with FOR UPDATE — so a What-If saved against a
-- doomed row DURING this migration now waits here and then fails its foreign key, instead of being
-- silently cascaded away after the repoint has already run. The window is real: migrations run at
-- startup, so a new replica executes this while the old one is still serving /api/whatif. A visible
-- error for one save beats a scenario that vanishes without trace.
SELECT id FROM calculation WHERE id IN (SELECT drop_id FROM collapse_map) FOR UPDATE;

-- Repoint. A scenario whose base is about to go would otherwise cascade away with it.
UPDATE scenario s
SET base_calculation_id = m.keep_id
FROM collapse_map m
WHERE s.base_calculation_id = m.drop_id;

DELETE FROM calculation c
USING collapse_map m
WHERE c.id = m.drop_id;

DO $$
DECLARE
    dropped BIGINT;
BEGIN
    SELECT count(*) INTO dropped FROM collapse_map;
    IF dropped > 0 THEN
        RAISE NOTICE 'collapsed % duplicate calculation row(s) that re-scored an unchanged profile', dropped;
    END IF;
END $$;

DROP TABLE collapse_map;
