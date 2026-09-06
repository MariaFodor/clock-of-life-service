# clock-of-life-service

The Clock of Life **service module** (Rust / Axum) — loads a model artifact bundle and serves longevity
estimates. v1 is a **stateless scoring service**; database persistence (accounts, calculation snapshots,
admin) is a later slice, gated on PostgreSQL.

## Run
```bash
cargo run          # serves http://127.0.0.1:8080
curl localhost:8080/health
```

## Layout
- `src/` — Axum app (routes, scoring engine, bundle loader)
- `bundle/model-v2.0.0/` — vendored model artifact (coefficients, per-country baselines, evidence) that
  the service loads at startup; produced by `clock-of-life-model`.

## Endpoints (building out, one per commit)
- `GET /health` — liveness ✅
- `GET /api/meta` — active model version + provenance
- `POST /api/estimate` — answers → Life Clock (years, interval, why)
- `POST /api/whatif` — non-persisted overlay for a lifestyle change

## Status
v1 in progress — stateless Cox scoring. Model bundle is country-aware (30 Eurostat baselines);
age/sex resolve against the national life table, lifestyle/pathology against the fitted coefficients.
