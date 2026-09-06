# clock-of-life-service

The Clock of Life **service module** (Rust / Axum) — loads a model artifact bundle and serves longevity
estimates, backed by PostgreSQL. It persists an append-only history of calculations and What-If
scenarios and reconciles its reference tables (features, questions, active model version) on startup.

## Requirements
- Rust 1.98+, a running **PostgreSQL** with a database the service can connect to.
- Default connection: `postgresql:///clock_of_life?host=/var/run/postgresql` (unix socket, peer auth).
  Override with the `DATABASE_URL` env var. The database must exist; tables are created by the
  migrations at startup.

```bash
createdb clock_of_life          # once
cargo run                       # connects, migrates, reconciles, serves http://127.0.0.1:8080
curl localhost:8080/health
```

## Test
Integration tests run against a throwaway database (they migrate + reconcile it):
```bash
createdb clock_of_life_test     # once; override with TEST_DATABASE_URL
cargo test
```

## Layout
- `src/` — Axum app: `lib.rs` (router + handlers), `scoring.rs` (scoring engine), `bundle.rs` (artifact
  loader), `db.rs` (pool, migrations, queries), `seed.rs` (startup reconciliation), `main.rs` (binary).
- `migrations/` — SQLx migrations (the full 11-table schema).
- `seeds/` — desired-state `feature` and `question` reference data seeded on startup.
- `bundle/model-v2.0.0/` — vendored model artifact (coefficients, per-country baselines, evidence)
  loaded at startup; produced by `clock-of-life-model`.

## Endpoints
- `GET /health` — liveness
- `GET /api/meta` — active model version + provenance
- `POST /api/estimate` — answers → Life Clock (years, interval, relative risk); persists a `calculation`,
  returns its `calculation_id`
- `POST /api/whatif` — lifestyle-change overlay; persists a `scenario` when `base_calculation_id` is given
- `GET /api/calculations` — calculation history (most recent first)
- `GET` / `POST /api/answers` — read / upsert questionnaire answers (one current answer per question)

## Status
Persist-and-read-back service. Scoring is Cox, country-aware (30 Eurostat baselines); age/sex resolve
against the national life table, lifestyle/pathology against the fitted coefficients. **Pre-auth:** all
calculations/answers share a single pseudonymous *anonymous* account until accounts + auth land
(SVC-DB5), which will introduce per-user ownership and isolation.
