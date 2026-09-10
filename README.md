# clock-of-life-service

The Clock of Life **service module** (Rust / Axum) — loads a model artifact bundle and serves longevity
estimates, backed by PostgreSQL. It persists an append-only history of calculations and What-If
scenarios and reconciles its reference tables (features, questions, active model version) on startup.

## Requirements
- Rust 1.98+, a running **PostgreSQL** with a database the service can connect to.
- Default connection: `postgresql:///clock_of_life?host=/var/run/postgresql` (unix socket, peer auth).
  Override with the `DATABASE_URL` env var. The database must exist; tables are created by the
  migrations at startup.
- `JWT_SECRET` signs bearer tokens and is **required**: without it the service refuses to start
  (fail closed). For local development only, `CLOCK_DEV_INSECURE_JWT=1` opts into an insecure
  well-known key (loud startup warning).

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
- `bundle/model-v4.0.0/` — vendored model artifact (coefficients, per-country baselines, evidence)
  loaded at startup; produced by `clock-of-life-model`.

## Endpoints
- `GET /health` — readiness (DB ping + bundle); 503 when the DB is unreachable
- `GET /api/openapi.json` — OpenAPI 3.0 contract (source for the web client) *(public)*
- `GET /api/meta` — active model version + provenance
- `GET /api/questions` — the 24-question interview definition *(public)*
- `GET /api/ontology` — the model's ontology: each factor's role, the causal graph, and a verified article link. Public.
- `GET /api/references` — evidence studies (openable DOIs / reviews); `?feature=<key>` or `?rule=<code>` *(public)*
- `GET /api/locations` — locations with PM2.5 / greenspace *(public)*
- `POST /api/auth/register` — create an account (`email` + `password` ≥ 8); returns a bearer token
- `POST /api/auth/login` — verify credentials; returns a bearer token
- `POST /api/estimate` — answers → Life Clock **+ `why[]` (per-factor deltas + references) + `model` provenance**;
  persists a `calculation` to the caller (or the anonymous account when unauthenticated)
- `POST /api/recommendations` — prioritized, evidence-cited advice (levers/manage only) *(pure)*
- `POST /api/whatif` — lifestyle-change overlay; persists a `scenario` when `base_calculation_id` is
  given (requires auth + ownership of that calculation)
- `POST /api/relocate` — "Where Should I Live?": compare a candidate location, with air/greenspace breakdown *(pure)*
- `GET /api/calculations` — the caller's calculation history *(auth required)*
- `GET` / `POST /api/answers` — read / upsert the caller's questionnaire answers *(auth required)*
- `GET /api/profile` — the caller's saved profile + answers; `POST /api/profile/location` sets the home location *(auth required)*
- `GET /api/aggregates` — k-anonymized cohort distributions (gated at 20 distinct accounts) *(public)*
- `GET /api/account/export` — GDPR export of all the caller's data; `DELETE /api/account` erases it; `PATCH /api/account` corrects it *(auth required)*
- `GET /api/admin/audit` and `admin/*` mutations (questions/features/rules, model-pin) — every change writes an `audit_event` *(admin only)*

Any non-API path serves the built SPA (`WEB_DIST`, client-side-routing fallback to `index.html`).
Authenticated requests send `Authorization: Bearer <token>`. Errors are JSON `{"error": "..."}`.

## Status
**Full launch-shaped API** over a persist-and-read-back core. Scoring is Cox, country-aware (30 Eurostat
baselines) with a location ENV term (PM2.5 + greenspace, RES-04); estimates carry a per-factor "Why?"
breakdown and prioritized recommendations, each linked to openable studies (DOIs / `analysed_papers`
reviews). Accounts are pseudonymous (argon2, JWT, email one-way-hashed — ADR-002) with per-account
isolation, GDPR export/erasure/correction, an admin surface with mandatory-citation audit trail, and
k-anonymized aggregates. Platform: OpenAPI contract, SPA serving, structured JSON errors, request
tracing, readiness health. **Data caveat:** the Romanian PM2.5/NDVI location values are illustrative
placeholders (RES-04) pending real sourced layers. **Model residuals (MH-\*)** still gate launch-grade
*numbers* (survey weights, coefficient tuning) — tracked in the model module.
