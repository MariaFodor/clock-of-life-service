//! The Clock of Life — Rust scoring service (library root).
//!
//! Loads a model artifact bundle, connects to PostgreSQL, reconciles the reference tables, and serves
//! estimates. State-changing calls (estimate/what-if) are persisted; history and answers are read back.
//! The binary in `main.rs` is a thin wrapper; the router is exposed here so integration tests can drive
//! it in-process.

pub mod auth;
pub mod bundle;
pub mod db;
pub mod openapi;
pub mod scoring;
pub mod seed;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use axum::{
    extract::{Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
    Json, Router,
};

/// A structured API error rendered as `{"error": "..."}` JSON with its status code.
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self { status, message: message.into() }
    }
}

impl From<(StatusCode, String)> for ApiError {
    fn from((status, message): (StatusCode, String)) -> Self {
        Self { status, message }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use bundle::Bundle;
use scoring::{
    attributions, estimate, eval_condition, whatif, Attribution, Estimate, Profile, WhatIf,
    WhatIfChanges,
};
use sqlx::postgres::PgPool;

/// Shared application state: the loaded model, the DB pool, and the ids resolved at reconciliation.
pub struct AppState {
    pub bundle: Arc<Bundle>,
    pub pool: PgPool,
    pub active_model_id: Uuid,
    pub anon_account_id: Uuid,
    pub anon_profile_id: Uuid,
    /// HS256 signing key for bearer tokens.
    pub jwt_secret: Vec<u8>,
    /// Bearer-token lifetime in seconds.
    pub token_ttl_secs: i64,
    /// Directory of the built SPA to serve (client-side routing falls back to its index.html).
    pub web_dist: String,
    /// The atlas, serialized once at startup from the bundle's own life tables. A pure function of
    /// the artifact: no database, no per-caller variation, so it is built here rather than per
    /// request. Held as the finished BYTES, not a Value — the validator below is a digest of exactly
    /// this string, so serving it verbatim makes "the ETag matches the body" true by construction
    /// rather than true by inspection, and drops a deep clone plus a re-serialization per request.
    pub atlas: Arc<String>,
    /// Validator for the atlas body. Strong, because it is a digest of the exact bytes served.
    pub atlas_etag: String,
    /// ISO3 -> the pre-serialized settlement list for that country, with its own validator.
    ///
    /// Pre-serialized per country for the same reason the atlas is: the picker asks for one country and
    /// serializing 3,522 settlements to return 60 of them is work done on every request. Derived from
    /// the BUNDLE rather than from the `location` table, so what the picker shows and what the scorer
    /// prices cannot drift — this product has already shipped one map that disagreed with its own clock
    /// by 3.6 years because a second copy of the data existed.
    pub places: HashMap<String, (Arc<String>, String)>,
    /// Pre-serialized measured-settlement points for the map's air layer, with its own validator.
    pub environment: Arc<String>,
    pub environment_etag: String,
}

/// Connection string for the application database (unix socket + peer auth by default).
pub fn default_database_url() -> String {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgresql:///clock_of_life?host=/var/run/postgresql".to_string())
}

/// The JWT signing secret from `JWT_SECRET`. Fail closed (like the bundle checksum gate): without a
/// real secret anyone could forge any account's token — incl. admin — so the service refuses to start
/// unless the operator explicitly opts into the insecure development key with CLOCK_DEV_INSECURE_JWT=1
/// (REVIEW-2026-09-09 S5).
pub fn jwt_secret() -> Result<Vec<u8>, String> {
    match std::env::var("JWT_SECRET") {
        Ok(s) if !s.is_empty() => Ok(s.into_bytes()),
        _ => {
            if std::env::var("CLOCK_DEV_INSECURE_JWT").as_deref() == Ok("1") {
                eprintln!(
                    "WARNING: JWT_SECRET not set — serving with the INSECURE development key because \
                     CLOCK_DEV_INSECURE_JWT=1. Anyone can forge tokens. Never use in production."
                );
                Ok(b"insecure-dev-key-do-not-use-in-production".to_vec())
            } else {
                Err("JWT_SECRET is not set — refusing to start. Set JWT_SECRET, or export \
                     CLOCK_DEV_INSECURE_JWT=1 to accept an insecure development key."
                    .into())
            }
        }
    }
}

/// Load the bundle, connect, migrate, and reconcile — producing ready-to-serve state.
pub async fn init_state(bundle_dir: &str, database_url: &str) -> Result<Arc<AppState>, String> {
    let b = Bundle::load(Path::new(bundle_dir)).map_err(|e| format!("bundle load: {e}"))?;
    let pool = db::connect(database_url)
        .await
        .map_err(|e| format!("db connect: {e}"))?;
    db::run_migrations(&pool)
        .await
        .map_err(|e| format!("db migrate: {e}"))?;
    let seeded = seed::reconcile(&pool, &b.manifest, bundle_dir, &b)
        .await
        .map_err(|e| format!("db reconcile: {e}"))?;
    let atlas = Arc::new(build_atlas(&b).to_string());
    // A 16-hex digest of the body itself, not of the version: a bundle rebuilt at the same version
    // would otherwise serve a stale cached map.
    let atlas_etag = format!("\"atlas-{}\"", bundle::checksum16_of(&atlas));
    let places = build_places(&b);
    let environment = Arc::new(build_environment(&b).to_string());
    let environment_etag = format!("\"env-{}\"", bundle::checksum16_of(&environment));
    Ok(Arc::new(AppState {
        bundle: Arc::new(b),
        pool,
        active_model_id: seeded.active_model_id,
        anon_account_id: seeded.anon_account_id,
        anon_profile_id: seeded.anon_profile_id,
        jwt_secret: jwt_secret().map_err(|e| format!("auth config: {e}"))?,
        token_ttl_secs: 7 * 24 * 3600, // 7 days
        web_dist: std::env::var("WEB_DIST").unwrap_or_else(|_| "web-dist".to_string()),
        atlas,
        atlas_etag,
        places,
        environment,
        environment_etag,
    }))
}

/// The HTTP surface. Kept separate from `init_state` so tests can build it over any state.
pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/openapi.json", get(openapi_route))
        .route("/api/meta", get(meta))
        .route("/api/atlas", get(atlas_route))
        .route("/api/questions", get(questions_route))
        .route("/api/ontology", get(ontology_route))
        .route("/api/references", get(references_route))
        .route("/api/locations", get(locations_route))
        .route("/api/aggregates", get(aggregates_route))
        .route("/api/auth/register", post(register_route))
        .route("/api/auth/login", post(login_route))
        .route("/api/estimate", post(estimate_route))
        .route("/api/recommendations", post(recommendations_route))
        .route("/api/relocate", post(relocate_route))
        .route("/api/places/:iso3", get(places_route))
        .route("/api/atlas/environment", get(environment_route))
        .route("/api/whatif", post(whatif_route))
        .route("/api/calculations", get(calculations_route))
        .route("/api/profile", get(profile_route))
        .route("/api/profile/location", post(set_location_route))
        .route("/api/account/export", get(account_export_route))
        .route("/api/account", delete(account_delete_route).patch(account_update_route))
        .route("/api/answers", get(get_answers_route).post(post_answers_route))
        .route("/api/admin/audit", get(audit_route))
        .route("/api/admin/questions", post(admin_create_question))
        .route("/api/admin/questions/:code", put(admin_update_question))
        .route("/api/admin/features/:key", put(admin_update_feature))
        .route("/api/admin/rules", post(admin_create_rule))
        .route("/api/admin/rules/:code", put(admin_update_rule))
        .route("/api/admin/model/pin", post(admin_pin_model))
        // Serve the built SPA for any non-API path; unknown deep links fall back to index.html.
        .fallback_service(
            tower_http::services::ServeDir::new(&state.web_dist)
                .fallback(tower_http::services::ServeFile::new(format!("{}/index.html", state.web_dist))),
        )
        // Per-request tracing spans (method, path, status, latency) via the `tracing` subscriber.
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

/// Extractor for an authenticated caller: a valid `Authorization: Bearer <jwt>` yields the account id.
pub struct Auth(pub Uuid);

#[axum::async_trait]
impl axum::extract::FromRequestParts<Arc<AppState>> for Auth {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let unauth = |m: &str| ApiError::new(StatusCode::UNAUTHORIZED, m);
        let header = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| unauth("missing bearer token"))?;
        let token = header
            .strip_prefix("Bearer ")
            .ok_or_else(|| unauth("malformed authorization header"))?;
        let id = auth::verify_token(token.trim(), &state.jwt_secret)
            .map_err(|_| unauth("invalid or expired token"))?;
        // A signature-valid token for an erased account must read as unauthenticated, not surface
        // as FK-violation 500s downstream (REVIEW-2026-09-09 S3: GDPR-erasure path).
        let exists = db::account_exists(&state.pool, id).await.map_err(db_err)?;
        if !exists {
            return Err(unauth("account no longer exists"));
        }
        Ok(Auth(id))
    }
}

/// Like `Auth`, but `None` when no credential was offered (try-before-signup paths). A *present*
/// Authorization header must still be valid: silently downgrading an expired token to the shared
/// anonymous account would persist the caller's inputs outside their history, export, and erasure
/// (REVIEW-2026-09-09 S3).
pub struct OptionalAuth(pub Option<Uuid>);

#[axum::async_trait]
impl axum::extract::FromRequestParts<Arc<AppState>> for OptionalAuth {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        if parts.headers.get(axum::http::header::AUTHORIZATION).is_none() {
            return Ok(OptionalAuth(None));
        }
        Auth::from_request_parts(parts, state).await.map(|a| OptionalAuth(Some(a.0)))
    }
}

/// Extractor for an authenticated admin: valid bearer token AND the account's `is_admin` flag.
pub struct Admin(pub Uuid);

#[axum::async_trait]
impl axum::extract::FromRequestParts<Arc<AppState>> for Admin {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let Auth(account) = Auth::from_request_parts(parts, state).await?;
        if db::is_admin(&state.pool, account).await.map_err(db_err)? {
            Ok(Admin(account))
        } else {
            Err((StatusCode::FORBIDDEN, "admin only".to_string()).into())
        }
    }
}

fn db_err(e: sqlx::Error) -> ApiError {
    // Keep the detail server-side; return a generic message so internal query/schema detail never
    // reaches the client.
    eprintln!("database error: {e}");
    ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}

/// First 16 hex chars of the SHA-256 of the serialized inputs (a stable snapshot fingerprint).
fn input_hash(inputs: &serde_json::Value) -> String {
    let bytes = serde_json::to_vec(inputs).unwrap_or_default();
    let digest = Sha256::digest(&bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect::<String>()[..16].to_string()
}

/// The OpenAPI 3.0 contract (public) — source for the web module's generated client.
async fn openapi_route() -> Json<serde_json::Value> {
    Json(openapi::openapi_doc())
}

/// Readiness: 200 when the DB is reachable and the bundle is loaded; 503 (degraded) otherwise.
async fn health(State(s): State<Arc<AppState>>) -> (StatusCode, Json<serde_json::Value>) {
    let db_up = db::ping(&s.pool).await.is_ok();
    let status = if db_up { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    (
        status,
        Json(json!({
            "status": if db_up { "ok" } else { "degraded" },
            "db": if db_up { "up" } else { "down" },
            "model_version": s.bundle.manifest.version,
            "countries": s.bundle.baselines.len(),
        })),
    )
}

/// The whole world's life expectancy, as the model's own baselines see it.
///
/// This is not a second dataset. Every number here is `remaining_le` — the exact function the Life
/// Clock runs — evaluated on the bundle's own `qx` at age 0 and age 60 with a relative risk of 1.0.
/// The map and the clock are therefore not merely consistent: they are the same arithmetic over the
/// same table, and any future change to the integrator moves both or neither. A copy of these numbers
/// in the front end could not have that property at any price.
///
/// `scoreable` is DERIVED from the loaded baselines rather than stored, because a stored flag is a
/// thing that can disagree with reality.
fn build_atlas(b: &Bundle) -> serde_json::Value {
    // How many measured settlements each country has. Counted here rather than asked for per country,
    // because the single most important thing this map has to say about the air is WHERE THERE IS NO
    // MEASUREMENT — 152 of the 237 countries drawn — and a country that greys out silently reads as
    // clean air. That claim has to arrive with the atlas the page already loads, not behind a second
    // request that may not finish.
    let mut settlements: HashMap<&str, (usize, i32)> = HashMap::new();
    for p in &b.places {
        let e = settlements.entry(p.iso3.as_str()).or_insert((0, 0));
        e.0 += 1;
        e.1 = e.1.max(p.pm25_year);
    }

    let mut countries: Vec<serde_json::Value> = b
        .reference
        .values()
        .map(|r| {
            let by_sex = |f: &dyn Fn(&HashMap<String, f64>) -> f64| {
                let mut out = serde_json::Map::new();
                for (sex, qx) in &r.qx {
                    out.insert(sex.to_lowercase(), json!(round1(f(qx))));
                }
                serde_json::Value::Object(out)
            };
            json!({
                "iso2": r.country,
                "iso3": r.iso3,
                "name": r.name,
                "region": r.region,
                "lifetable_year": r.lifetable_year,
                "scoreable": b.baselines.contains_key(&r.country),
                "le0": by_sex(&|qx| scoring::remaining_le(qx, 0, 1.0)),
                "le60": by_sex(&|qx| scoring::remaining_le(qx, 60, 1.0)),
                "am": by_sex(&scoring::adult_mortality_15_60),
                // The environment SUMMARY, not the readings. `settlements: 0` is the honest statement
                // the map needs and is deliberately not the same as a missing key: 0 means "nobody has
                // published a PM2.5 measurement for any settlement here since 2020", which is a fact
                // about the measurement rather than about the country, and the page says it in those
                // words. `reference` is the national figure, which exists for 227 countries even where
                // no city does — so a country with no dot can still have a number.
                // SUMMARY fields only, and deliberately not the whole `env_reference`: attaching that
                // to all 237 rows took the payload from 55 KB to 124 KB, past the cap this endpoint has
                // for a reason (the five residence-area splits and up to ten city names each). The full
                // reference is already served per country by /api/places/{iso3}, where a reader who has
                // picked a country is the only one who needs it.
                "env": {
                    "settlements": r.iso3.as_deref().and_then(|i| settlements.get(i)).map_or(0, |s| s.0),
                    "latest_year": r.iso3.as_deref().and_then(|i| settlements.get(i)).map(|s| s.1),
                    "pm25": r.env_reference.as_ref().map(|e| e.pm25),
                    "pm25_year": r.env_reference.as_ref().and_then(|e| e.pm25_year),
                    "ndvi": r.env_reference.as_ref().and_then(|e| e.ndvi),
                    // How thin the greenness figure is — 22 of the 30 scoreable countries rest on one
                    // city, and a page that draws the number has to be able to say so.
                    "ndvi_cities": r.env_reference.as_ref().map(|e| e.ndvi_cities),
                },
            })
        })
        .collect();
    // Sorted so the payload — and therefore its ETag — is stable across restarts; HashMap iteration
    // order is not.
    countries.sort_by(|a, b| a["iso2"].as_str().cmp(&b["iso2"].as_str()));
    json!({
        "model_version": b.manifest.version,
        "derived_by": "remaining_le(qx, age, rr=1.0) — the same integrator the Life Clock uses",
        "sources": b.manifest.sources,
        "countries": countries,
    })
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// GET /api/atlas — population life expectancy for every country the bundle carries a table for.
///
/// Public, and NOT k-anonymised. `/api/aggregates` gates at k=20 because it summarises this
/// platform's own users' calculations, where a small cell can re-identify a person. This summarises a
/// published national life table for an entire country's population, out of a static artifact, with
/// no database read: there is no individual in the numerator. Gating it would suppress small
/// COUNTRIES — San Marino, Tuvalu — on a privacy ground that does not exist, turning a published UN
/// figure into "no data".
///
/// The load-bearing proof of that is not the test but the type: `build_atlas` takes `&Bundle` and
/// nothing else, and runs in `init_state` before a router exists, so it CANNOT read user data. The
/// test's anonymous-equals-authenticated assertion earns its keep for a different reason — a body
/// that varied by caller, served `public` with no `Vary`, would be a shared-cache poisoning bug.
async fn atlas_route(State(s): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if headers
        .get("if-none-match")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == s.atlas_etag))
    {
        // Cache-Control is echoed on the 304 as well: a revalidating cache should not have to
        // remember the freshness it was given the first time.
        return (
            StatusCode::NOT_MODIFIED,
            [
                ("etag", s.atlas_etag.clone()),
                ("cache-control", "public, max-age=86400".to_string()),
            ],
        )
            .into_response();
    }
    (
        [
            ("etag", s.atlas_etag.clone()),
            // A day: these numbers change when a bundle ships, and the ETag catches that sooner.
            ("cache-control", "public, max-age=86400".to_string()),
            ("content-type", "application/json".to_string()),
        ],
        (*s.atlas).clone(),
    )
        .into_response()
}

/// Every measured settlement as a drawable point, for the map's air layer.
///
/// Separate from `/api/atlas` and loaded only when a reader switches the layer on: 3,521 coordinate
/// pairs are an order of magnitude more bytes than the whole country table, and most readers never ask
/// for them. Kept to the fields a dot and its hover need, which is why there is no greenness here — a
/// value that is usually the COUNTRY's figure cannot be drawn as a property of a point, and the country
/// layer already carries it.
fn build_environment(b: &Bundle) -> serde_json::Value {
    let mut points: Vec<serde_json::Value> = b
        .places
        .iter()
        .map(|p| {
            json!({
                "iso3": p.iso3,
                "city": p.city,
                "lat": p.lat,
                "lon": p.lon,
                "pm25": p.pm25,
                "year": p.pm25_year,
            })
        })
        .collect();
    // Sorted for a stable ETag, as the atlas is.
    points.sort_by(|a, c| {
        (a["iso3"].as_str(), a["city"].as_str()).cmp(&(c["iso3"].as_str(), c["city"].as_str()))
    });
    let measured: std::collections::HashSet<&str> =
        b.places.iter().map(|p| p.iso3.as_str()).collect();
    // Countries the atlas DRAWS and nobody has measured. Computed here so the page never has to derive
    // an absence by subtraction — the arithmetic that produces an off-by-one nobody notices.
    let mut unmeasured: Vec<&str> = b
        .reference
        .values()
        .filter_map(|r| r.iso3.as_deref())
        .filter(|iso3| !measured.contains(iso3))
        .collect();
    unmeasured.sort_unstable();
    json!({
        "model_version": b.manifest.version,
        "pollutant": "PM2.5, annual mean, µg/m³",
        // The radius each reading is being claimed to speak for. Served rather than hardcoded in the
        // page, because it is the join tolerance the greenness match also used and the two must agree.
        "speaks_for_km": 25,
        "window": [2020, 2025],
        "points": points,
        "unmeasured_iso3": unmeasured,
        "sources": b.manifest.env_sources,
        "licences": b.manifest.licences,
    })
}

/// Group the bundle's settlements by ISO3 and serialize each group once, with its own strong ETag.
///
/// Takes `&Bundle` and nothing else — the same purity argument as `build_atlas`: it runs in
/// `init_state` before a router exists, so it cannot read user data, and that is a property of the
/// signature rather than of a test.
fn build_places(b: &Bundle) -> HashMap<String, (Arc<String>, String)> {
    let mut by_country: HashMap<&str, Vec<&crate::bundle::Place>> = HashMap::new();
    for p in &b.places {
        by_country.entry(p.iso3.as_str()).or_default().push(p);
    }
    // ISO3 -> ISO2, so the response can tell a client whether a personal estimate is possible here at
    // all. Built from the bundle's own baselines rather than a second table.
    let iso3_to_iso2: HashMap<&str, &str> = b
        .reference
        .iter()
        .filter_map(|(iso2, r)| r.iso3.as_deref().map(|iso3| (iso3, iso2.as_str())))
        .collect();

    by_country
        .into_iter()
        .map(|(iso3, mut places)| {
            places.sort_by(|a, c| a.city.cmp(&c.city));
            let iso2 = iso3_to_iso2.get(iso3).copied();
            let reference = iso2
                .and_then(|i| b.reference.get(i))
                .and_then(|r| r.env_reference.as_ref());
            let body = json!({
                "iso3": iso3,
                "iso2": iso2,
                "name": iso2.and_then(|i| b.reference.get(i)).and_then(|r| r.name.clone()),
                // Whether the clock can give a PERSONAL number for someone living here. A picker that
                // offers a Nigerian city without saying this produces a dead click at the end.
                "scoreable": iso2.is_some_and(|i| b.baselines.contains_key(i)),
                // What the ENV term is centred on here, so a client can show a place's reading against
                // its own country's average rather than against nothing.
                "reference": reference,
                "places": places,
                // The coverage sentence the screen has to be able to say, computed rather than written.
                "coverage": {
                    "settlements": places.len(),
                    "with_city_greenness":
                        places.iter().filter(|p| p.ndvi_basis.as_deref() == Some("city")).count(),
                    "with_country_greenness":
                        places.iter().filter(|p| p.ndvi_basis.as_deref() == Some("country")).count(),
                    "without_greenness": places.iter().filter(|p| p.ndvi_basis.is_none()).count(),
                },
            })
            .to_string();
            let etag = format!("\"places-{}-{}\"", iso3, bundle::checksum16_of(&body));
            (iso3.to_string(), (Arc::new(body), etag))
        })
        .collect()
}

/// Every measured settlement in one country (public — powers the home-location picker).
///
/// 404 rather than an empty list when a country has no settlements: "no measurement since 2020" is a
/// fact about 152 of the 237 countries the atlas draws, and an empty array reads as a loading bug.
async fn places_route(
    State(s): State<Arc<AppState>>,
    AxumPath(iso3): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    let iso3 = iso3.to_uppercase();
    let Some((body, etag)) = s.places.get(&iso3) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": format!("no measured settlements for {iso3}"),
                "reason": "WHO has published no PM2.5 measurement for any settlement in this country \
                           since 2020. This is a gap in the measurement, not in the country.",
            })),
        )
            .into_response();
    };
    if headers
        .get("if-none-match")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag))
    {
        return (
            StatusCode::NOT_MODIFIED,
            [("etag", etag.clone()), ("cache-control", "public, max-age=86400".to_string())],
        )
            .into_response();
    }
    (
        [
            ("etag", etag.clone()),
            ("cache-control", "public, max-age=86400".to_string()),
            ("content-type", "application/json".to_string()),
        ],
        (**body).clone(),
    )
        .into_response()
}

/// Where the air has been measured, and where it has not.
async fn environment_route(State(s): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if headers
        .get("if-none-match")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == s.environment_etag))
    {
        return (
            StatusCode::NOT_MODIFIED,
            [
                ("etag", s.environment_etag.clone()),
                ("cache-control", "public, max-age=86400".to_string()),
            ],
        )
            .into_response();
    }
    (
        [
            ("etag", s.environment_etag.clone()),
            ("cache-control", "public, max-age=86400".to_string()),
            ("content-type", "application/json".to_string()),
        ],
        (*s.environment).clone(),
    )
        .into_response()
}

/// Active model version + provenance.
async fn meta(State(s): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let mut countries: Vec<&String> = s.bundle.baselines.keys().collect();
    countries.sort();
    Json(json!({
        "model_version": s.bundle.manifest.version,
        "algorithm": s.bundle.manifest.algorithm,
        "countries": countries,
        // The same list with names and ISO3 codes, so the interview's country question can be built
        // from what the model actually ships rather than from a hand-kept list in the front end. A
        // second copy of 30 country names is a second thing to update when the 31st is added — and
        // until this shipped there was no country question at all, because the profile hardcoded RO.
        //
        // `iso3` travels with it because the settlement picker keys on ISO3 (`/api/places/{iso3}`)
        // while a profile stores ISO2, and the client should not have to hold a mapping between them.
        "country_options": countries.iter().map(|iso2| {
            let r = s.bundle.reference.get(*iso2);
            json!({
                "iso2": iso2,
                "iso3": r.and_then(|r| r.iso3.clone()),
                "name": r.and_then(|r| r.name.clone()),
                // How many measured settlements this country has, so the picker can say up front
                // whether a city question will have anything in it.
                "settlements": r.and_then(|r| r.iso3.as_deref())
                    .map_or(0, |iso3| s.bundle.places.iter().filter(|p| p.iso3 == iso3).count()),
            })
        }).collect::<Vec<_>>(),
        // Codes that used to be valid and now resolve elsewhere. Without this a client holding a
        // stored `EL` finds no matching option in a picker built from `countries`, even though the
        // server still scores it — the estimate keeps working and the client cannot heal its value.
        "country_aliases": s.bundle.manifest.country_aliases,
        "assumptions": [
            "statistical estimate, not a prediction or diagnosis",
            "relative risk centred on the selected country's average person",
        ],
    }))
}

/// Cohort aggregates (public): estimate-years distribution + per-country summary, each gated at k=20 so
/// no individual is exposed. Never a claim about observed mortality (the platform never observes deaths).
async fn aggregates_route(
    State(s): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let overall = db::aggregate_overall(&s.pool).await.map_err(db_err)?;
    let distribution = if overall.n >= db::AGGREGATE_MIN_K {
        json!({"mean": overall.mean, "p10": overall.p10, "p50": overall.p50, "p90": overall.p90})
    } else {
        serde_json::Value::Null // suppressed: cohort below k
    };
    let by_country = db::aggregate_by_country(&s.pool).await.map_err(db_err)?;
    Ok(Json(json!({
        "n": overall.n,
        "min_group": db::AGGREGATE_MIN_K,
        "estimate_years": distribution,
        "by_country": by_country,
        "note": "aggregate scenario estimates only, k-anonymized; not observed mortality",
    })))
}

/// All known locations with their PM2.5 / greenspace (public — powers the location picker + compare).
async fn locations_route(
    State(s): State<Arc<AppState>>,
) -> Result<Json<Vec<db::LocationRow>>, ApiError> {
    let rows = db::list_locations(&s.pool).await.map_err(db_err)?;
    Ok(Json(rows))
}

#[derive(Deserialize)]
struct RefQuery {
    feature: Option<String>,
    rule: Option<String>,
}

/// Evidence references (public). All studies, or those linked to a `?feature=<key>` or `?rule=<code>`.
async fn references_route(
    State(s): State<Arc<AppState>>,
    Query(q): Query<RefQuery>,
) -> Result<Json<Vec<db::Study>>, ApiError> {
    let studies = match (q.feature.as_deref(), q.rule.as_deref()) {
        (Some(feature), _) => db::studies_for_feature(&s.pool, feature).await,
        (None, Some(rule)) => db::studies_for_rule(&s.pool, rule).await,
        (None, None) => db::list_studies(&s.pool).await,
    }
    .map_err(db_err)?;
    Ok(Json(studies))
}

/// The interview definition (public — the frontend renders onboarding before sign-up).
async fn questions_route(
    State(s): State<Arc<AppState>>,
) -> Result<Json<Vec<db::QuestionRow>>, ApiError> {
    let rows = db::list_questions(&s.pool).await.map_err(db_err)?;
    Ok(Json(rows))
}

/// A valid argon2 hash of a throwaway value, used to equalize login timing when no account matches
/// (so response time doesn't reveal whether an email is registered). Computed once.
static DUMMY_PW_HASH: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| auth::hash_password("timing-equalizer").unwrap_or_default());

#[derive(Deserialize)]
struct RegisterRequest {
    email: String,
    password: String,
    #[serde(default)]
    locale: Option<String>,
}

#[derive(Serialize)]
struct AuthResponse {
    token: String,
    account_id: Uuid,
}

/// Create an account (+ empty profile) and return a bearer token.
async fn register_route(
    State(s): State<Arc<AppState>>,
    Json(req): Json<RegisterRequest>,
) -> Result<Json<AuthResponse>, ApiError> {
    if req.email.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "email required".to_string()).into());
    }
    if req.password.len() < 8 {
        return Err((StatusCode::BAD_REQUEST, "password must be at least 8 characters".to_string()).into());
    }
    let email_hash = auth::email_hash(&req.email);
    let password_hash = auth::hash_password(&req.password)
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "internal error".to_string()))?;
    let locale = req.locale.as_deref().unwrap_or("ro");
    let (account_id, _profile_id) = db::create_account(&s.pool, &email_hash, &password_hash, locale)
        .await
        .map_err(|e| {
            if db::is_unique_violation(&e) {
                ApiError::new(StatusCode::CONFLICT, "account already exists")
            } else {
                db_err(e)
            }
        })?;
    let token = auth::issue_token(account_id, &s.jwt_secret, s.token_ttl_secs)
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "internal error".to_string()))?;
    Ok(Json(AuthResponse { token, account_id }))
}

#[derive(Deserialize)]
struct LoginRequest {
    email: String,
    password: String,
}

/// Verify credentials and return a bearer token. Uniform 401 whether the email or the password is wrong.
async fn login_route(
    State(s): State<Arc<AppState>>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<AuthResponse>, ApiError> {
    let email_hash = auth::email_hash(&req.email);
    let account = db::find_account_by_email_hash(&s.pool, &email_hash)
        .await
        .map_err(db_err)?;
    let unauthorized = || ApiError::new(StatusCode::UNAUTHORIZED, "invalid credentials");
    match account {
        Some((account_id, password_hash)) => {
            if !auth::verify_password(&req.password, &password_hash) {
                return Err(unauthorized());
            }
            let token = auth::issue_token(account_id, &s.jwt_secret, s.token_ttl_secs)
                .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "internal error".to_string()))?;
            Ok(Json(AuthResponse { token, account_id }))
        }
        None => {
            // Equalize timing with the password-verify path above; result ignored.
            let _ = auth::verify_password(&req.password, &DUMMY_PW_HASH);
            Err(unauthorized())
        }
    }
}

/// A "Why?" factor plus the openable studies backing it.
#[derive(Serialize)]
struct WhyEntry {
    #[serde(flatten)]
    attribution: Attribution,
    references: Vec<db::Study>,
}

#[derive(Serialize)]
struct EstimateResponse {
    #[serde(flatten)]
    estimate: Estimate,
    /// Per-factor "Why?" breakdown (total-effect year deltas + evidence + references).
    why: Vec<WhyEntry>,
    /// Provenance of the model that produced this estimate (reproducibility).
    model: serde_json::Value,
    calculation_id: Uuid,
}

/// Group all feature→study links into a lookup by feature key (one query).
async fn references_by_feature(
    pool: &sqlx::postgres::PgPool,
) -> Result<HashMap<String, Vec<db::Study>>, ApiError> {
    let rows = db::all_feature_studies(pool).await.map_err(db_err)?;
    let mut map: HashMap<String, Vec<db::Study>> = HashMap::new();
    for row in rows {
        map.entry(row.feature_key).or_default().push(row.study);
    }
    Ok(map)
}

/// Answers -> Life-Clock estimate. Persists an append-only `calculation` snapshot to the caller's
/// account (or the shared anonymous account when unauthenticated — try-before-signup).
async fn estimate_route(
    State(s): State<Arc<AppState>>,
    OptionalAuth(account): OptionalAuth,
    Json(profile): Json<Profile>,
) -> Result<Json<EstimateResponse>, ApiError> {
    let est = estimate(&s.bundle, &profile).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let raw_why = attributions(&s.bundle, &profile).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    // Enrich each factor with its openable study references.
    let refs = references_by_feature(&s.pool).await?;
    let why: Vec<WhyEntry> = raw_why
        .into_iter()
        .map(|a| {
            let references = refs.get(&a.key).cloned().unwrap_or_default();
            WhyEntry { attribution: a, references }
        })
        .collect();
    let inputs = serde_json::to_value(&profile)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("serialize inputs: {e}")))?;
    let attributions_json = serde_json::to_value(&why)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("serialize why: {e}")))?;
    let model = json!({
        "version": s.bundle.manifest.version,
        "algorithm": s.bundle.manifest.algorithm,
        "reference_population": s.bundle.manifest.reference_population,
    });
    let owner = account.unwrap_or(s.anon_account_id);
    let id = db::insert_calculation(
        &s.pool,
        owner,
        s.active_model_id,
        &input_hash(&inputs),
        &inputs,
        est.estimate_years,
        est.interval[0],
        est.interval[1],
        est.reaches_age,
        est.relative_risk,
        &attributions_json,
    )
    .await
    .map_err(db_err)?;
    Ok(Json(EstimateResponse { estimate: est, why, model, calculation_id: id }))
}

/// A prioritized, evidence-cited recommendation (levers/manage only, never context/baseline).
#[derive(Serialize)]
struct Recommendation {
    feature: String,
    message: String,
    role: String,
    priority: i32,
    evidence_grade: Option<String>,
    evidence_citation: String,
    /// Years currently at stake on this whole exposure: |feature delta + its companions|.
    impact_years: f64,
    /// Ranking score: impact_years × confidence(grade) × (priority/100).
    score: f64,
    /// Openable studies backing this recommendation.
    references: Vec<db::Study>,
}

/// Evaluate the seeded rules against a profile and return prioritized recommendations.
/// No persistence, no auth required (a pure function of the submitted profile) — try-before-signup.
async fn recommendations_route(
    State(s): State<Arc<AppState>>,
    Json(profile): Json<Profile>,
) -> Result<Json<Vec<Recommendation>>, ApiError> {
    // attributions() validates the profile and gives the per-factor years-at-stake used for ranking.
    let why = attributions(&s.bundle, &profile).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    // Signed, not absolute: the sum below adds an exposure's companions, and a companion can point
    // the other way (a smoker below the cohort's mean dose gets a POSITIVE cigs_day delta). Taking
    // |.| per factor would book that benefit as harm and overstate what quitting is worth to exactly
    // the smokers it is worth least to. Magnitude is taken once, after the exposure is whole.
    let impact: std::collections::HashMap<&str, f64> =
        why.iter().map(|a| (a.key.as_str(), a.delta_years)).collect();

    let rules = db::active_recommendation_rules(&s.pool).await.map_err(db_err)?;
    let mut recs: Vec<Recommendation> = Vec::new();
    for r in rules {
        if !eval_condition(&profile, &r.condition) {
            continue;
        }
        // A rule's impact is its whole exposure, not one indicator of it. "Quit smoking" ends both
        // being a current smoker AND the dose, so ranking it on smk_current alone dropped over a
        // year of harm — which briefly ranked quitting below getting more exercise. Companions come
        // from the ontology the model was fitted under, so this cannot drift from the fit.
        let companions: Vec<String> = s.bundle.ontology
            .get(&r.feature_key)
            .and_then(|f| f.get("companions"))
            .and_then(|c| c.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let impact_years = (impact.get(r.feature_key.as_str()).copied().unwrap_or(0.0)
            + companions.iter().filter_map(|c| impact.get(c.as_str())).sum::<f64>())
            .abs();
        let confidence = match r.evidence_grade.as_deref() {
            Some("strong") => 1.0,
            Some("moderate") => 0.6,
            Some("weak") => 0.3,
            _ => 0.3,
        };
        let score = impact_years * confidence * (r.priority as f64 / 100.0);
        let references = db::studies_for_rule(&s.pool, &r.code).await.map_err(db_err)?;
        recs.push(Recommendation {
            feature: r.feature_key,
            message: r.message,
            role: r.role,
            priority: r.priority,
            evidence_grade: r.evidence_grade,
            evidence_citation: r.evidence_citation,
            impact_years: (impact_years * 10.0).round() / 10.0,
            score: (score * 100.0).round() / 100.0,
            references,
        });
    }
    // Rank by score (impact × confidence × priority), then priority as a stable tiebreak.
    recs.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.priority.cmp(&a.priority))
    });
    Ok(Json(recs))
}

#[derive(Deserialize)]
struct RelocateRequest {
    base: Profile,
    /// Candidate location to move to (by name).
    to: String,
    /// Optional current location (by name); overrides base.pm25/ndvi. Else base's own env is the baseline.
    #[serde(default)]
    from: Option<String>,
    #[serde(default = "default_country")]
    country: String,
}

/// "Where Should I Live?" — compare remaining years at a candidate location vs the current one, and
/// explain how much of the gap is air (PM2.5) vs greenspace. Pure, no auth, no persistence. ENV is the
/// lever here (the location is the thing being changed).
async fn relocate_route(
    State(s): State<Arc<AppState>>,
    Json(req): Json<RelocateRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let round1 = |x: f64| (x * 10.0).round() / 10.0;

    // The cross-border case, refused in words rather than answered wrongly.
    //
    // `req.country` and `req.base.country` are separate fields, and nothing checked that they agreed.
    // Moving to a city in another country therefore looked up the DESTINATION's exposures and scored
    // them against the ORIGIN's life table and the ORIGIN's exposure reference — exposure arithmetic
    // wearing a national-mortality label. A German city's 8 µg/m³ was priced as a deviation from
    // Romania's 10.4 and then applied to Romanian death rates, which is not any country's answer.
    //
    // Refusing is the honest move here rather than re-basing: a real cross-border answer has to change
    // the life table too, and most destinations are not scoreable at all (30 of 237), so the feature
    // would silently work for some borders and not others.
    let alias = |c: &String| s.bundle.manifest.country_aliases.get(c).unwrap_or(c).clone();
    if alias(&req.country) != alias(&req.base.country) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "this compares places inside one country. Your profile says {} and you asked about a \
                 place in {} — moving between countries changes the national death rates the estimate \
                 is built on, not just the air, and this cannot yet account for that.",
                req.base.country, req.country
            ),
        )
            .into());
    }

    let to = db::location_by_name(&s.pool, &req.to, &req.country)
        .await
        .map_err(db_err)?
        .ok_or((StatusCode::NOT_FOUND, format!("unknown location: {} ({})", req.to, req.country)))?;

    // Baseline profile: optionally take env from a named `from` location.
    let mut base = req.base.clone();
    let from_json = if let Some(fname) = &req.from {
        let f = db::location_by_name(&s.pool, fname, &req.country)
            .await
            .map_err(db_err)?
            .ok_or((StatusCode::NOT_FOUND, format!("unknown location: {} ({})", fname, req.country)))?;
        base.pm25 = f.pm25;
        base.ndvi = f.ndvi;
        json!({"name": f.name, "pm25": f.pm25, "ndvi": f.ndvi})
    } else {
        serde_json::Value::Null
    };

    let bad = |e: String| (StatusCode::BAD_REQUEST, e);
    let current = estimate(&s.bundle, &base).map_err(bad)?.estimate_years;
    // Full move (both air + green change).
    let mut moved = base.clone();
    moved.pm25 = to.pm25;
    moved.ndvi = to.ndvi;
    let relocated = estimate(&s.bundle, &moved).map_err(bad)?.estimate_years;
    // Isolate each driver: change only air, then only greenspace.
    let mut air = base.clone();
    air.pm25 = to.pm25;
    let air_years = estimate(&s.bundle, &air).map_err(bad)?.estimate_years;
    let mut green = base.clone();
    green.ndvi = to.ndvi;
    let green_years = estimate(&s.bundle, &green).map_err(bad)?.estimate_years;

    // What could not be priced, and why — so the breakdown can say "no layer" instead of "+0.0 years".
    //
    // `0.0` was the answer to both "this makes no difference here" and "nobody has measured greenness in
    // your country", and a reader cannot tell those apart. 3,067 of the 3,522 settlements carry their
    // country's greenness rather than their own, and 29 carry none at all.
    let reference = s
        .bundle
        .manifest
        .country_aliases
        .get(&req.base.country)
        .unwrap_or(&req.base.country)
        .clone();
    let env_ref = s.bundle.baselines.get(&reference).and_then(|b| b.env_reference.as_ref());
    let (_, refused) = scoring::env_term_with_reason(to.pm25, to.ndvi, env_ref);
    let unpriced = |key: &str| {
        refused
            .iter()
            .any(|(k, r)| *k == key && *r == scoring::EnvRefused::NoReference)
    };
    let air_delta = if unpriced("pm25") {
        serde_json::Value::Null
    } else {
        json!(round1(air_years - current))
    };
    let green_delta = if unpriced("ndvi") {
        serde_json::Value::Null
    } else {
        json!(round1(green_years - current))
    };

    Ok(Json(json!({
        "from": from_json,
        "to": {
            "name": to.name, "pm25": to.pm25, "ndvi": to.ndvi,
            // Whether this settlement's greenness is its own measurement or its country's figure shown
            // here for want of one. The screen must print the difference.
            "ndvi_basis": to.ndvi_basis,
            "pm25_year": to.pm25_year, "ndvi_year": to.ndvi_year,
        },
        "reference": env_ref,
        "current_years": current,
        "relocated_years": relocated,
        "delta_years": round1(relocated - current),
        "breakdown": {
            "air_delta_years": air_delta,
            "greenspace_delta_years": green_delta,
            // Present only when something was refused, so a client cannot render an empty reason.
            "unpriced": if refused.iter().any(|(_, r)| *r == scoring::EnvRefused::NoReference) {
                json!(refused.iter()
                    .filter(|(_, r)| *r == scoring::EnvRefused::NoReference)
                    .map(|(k, _)| json!({
                        "component": k,
                        "reason": format!("no measured {k} reference for {reference} — a change \
                                           cannot be priced against an average nobody has measured"),
                    }))
                    .collect::<Vec<_>>())
            } else { serde_json::Value::Null },
        },
        "note": "statistical scenario; location exposure is ecological (assigned by area, not measured) — medium confidence, not a promise",
    })))
}

#[derive(Deserialize)]
struct WhatIfRequest {
    base: Profile,
    changes: WhatIfChanges,
    /// When present, the What-If result is persisted as a `scenario` forked from this calculation.
    #[serde(default)]
    base_calculation_id: Option<Uuid>,
}

#[derive(Serialize)]
struct WhatIfResponse {
    #[serde(flatten)]
    whatif: WhatIf,
    #[serde(skip_serializing_if = "Option::is_none")]
    scenario_id: Option<Uuid>,
}

/// Explore a lifestyle change. Overlay only, unless `base_calculation_id` asks to persist a scenario.
async fn whatif_route(
    State(s): State<Arc<AppState>>,
    OptionalAuth(account): OptionalAuth,
    Json(req): Json<WhatIfRequest>,
) -> Result<Json<WhatIfResponse>, ApiError> {
    let wi = whatif(&s.bundle, &req.base, &req.changes).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let scenario_id = if let Some(base_id) = req.base_calculation_id {
        // Persisting a scenario requires auth and that the base calculation belongs to the caller.
        let account = account.ok_or((
            StatusCode::UNAUTHORIZED,
            "authentication required to save a scenario".to_string(),
        ))?;
        match db::calculation_owner(&s.pool, base_id).await.map_err(db_err)? {
            Some(owner) if owner == account => {}
            Some(_) => return Err((StatusCode::FORBIDDEN, "not your calculation".to_string()).into()),
            None => return Err((StatusCode::NOT_FOUND, "base calculation not found".to_string()).into()),
        }
        let modifications = serde_json::to_value(&req.changes)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("serialize changes: {e}")))?;
        let result = serde_json::to_value(&wi)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("serialize result: {e}")))?;
        Some(
            db::insert_scenario(&s.pool, base_id, &modifications, &result)
                .await
                .map_err(db_err)?,
        )
    } else {
        None
    };
    Ok(Json(WhatIfResponse { whatif: wi, scenario_id }))
}

/// Calculation history for the authenticated caller, newest first.
async fn calculations_route(
    State(s): State<Arc<AppState>>,
    Auth(account): Auth,
) -> Result<Json<Vec<db::CalcRow>>, ApiError> {
    let rows = db::list_calculations(&s.pool, account, 50)
        .await
        .map_err(db_err)?;
    Ok(Json(rows))
}

/// GDPR export (auth): all of the caller's data — account (no password), profile, answers, calculations.
async fn account_export_route(
    State(s): State<Arc<AppState>>,
    Auth(account): Auth,
) -> Result<Json<serde_json::Value>, ApiError> {
    let account_json = db::account_export_json(&s.pool, account)
        .await
        .map_err(db_err)?
        .ok_or((StatusCode::NOT_FOUND, "account not found".to_string()))?;
    let profile = db::get_profile(&s.pool, account).await.map_err(db_err)?;
    let answers = match &profile {
        Some(p) => db::list_answers(&s.pool, p.id).await.map_err(db_err)?,
        None => Vec::new(),
    };
    let calculations = db::list_calculations(&s.pool, account, 10_000).await.map_err(db_err)?;
    Ok(Json(json!({
        "account": account_json,
        "profile": profile,
        "answers": answers,
        "calculations": calculations,
    })))
}

#[derive(Deserialize)]
struct AccountUpdate {
    locale: String,
}

/// Correct account-level data (auth) — right to rectification. Currently the locale.
async fn account_update_route(
    State(s): State<Arc<AppState>>,
    Auth(account): Auth,
    Json(req): Json<AccountUpdate>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if req.locale.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "locale must not be empty".to_string()).into());
    }
    let updated = db::update_account_locale(&s.pool, account, &req.locale).await.map_err(db_err)?;
    if updated == 0 {
        return Err((StatusCode::NOT_FOUND, "account not found".to_string()).into());
    }
    Ok(Json(json!({ "locale": req.locale })))
}

/// GDPR erasure (auth): permanently delete the caller's account and all data cascading from it.
/// (Full erasure; a retention/anonymization policy for reproducibility is open question A7.)
async fn account_delete_route(
    State(s): State<Arc<AppState>>,
    Auth(account): Auth,
) -> Result<Json<serde_json::Value>, ApiError> {
    let deleted = db::delete_account(&s.pool, account).await.map_err(db_err)?;
    if deleted == 0 {
        return Err((StatusCode::NOT_FOUND, "account not found".to_string()).into());
    }
    Ok(Json(json!({ "deleted": true })))
}

/// The authenticated caller's saved profile: metadata + current answers.
async fn profile_route(
    State(s): State<Arc<AppState>>,
    Auth(account): Auth,
) -> Result<Json<serde_json::Value>, ApiError> {
    let profile = db::get_profile(&s.pool, account)
        .await
        .map_err(db_err)?
        .ok_or_else(|| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "no profile for account"))?;
    let answers = db::list_answers(&s.pool, profile.id).await.map_err(db_err)?;
    Ok(Json(json!({
        "profile_id": profile.id,
        "home_location_id": profile.home_location_id,
        "updated_at": profile.updated_at,
        "answers": answers,
    })))
}

fn default_country() -> String {
    "RO".to_string()
}

#[derive(Deserialize)]
struct SetLocationRequest {
    name: String,
    #[serde(default = "default_country")]
    country: String,
}

/// Set the authenticated caller's home location (by name). 404 if the location is unknown.
async fn set_location_route(
    State(s): State<Arc<AppState>>,
    Auth(account): Auth,
    Json(req): Json<SetLocationRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let location_id = db::location_id_by_name(&s.pool, &req.name, &req.country)
        .await
        .map_err(db_err)?
        .ok_or((
            StatusCode::NOT_FOUND,
            format!("unknown location: {} ({})", req.name, req.country),
        ))?;
    db::set_home_location(&s.pool, account, location_id)
        .await
        .map_err(db_err)?;
    Ok(Json(json!({ "home_location_id": location_id })))
}

/// Admin mutations must cite their evidence (evidence-traceability); reject a blank citation.
fn require_citation(citation: &str) -> Result<(), ApiError> {
    if citation.trim().is_empty() {
        Err((StatusCode::BAD_REQUEST, "citation is required for admin changes".to_string()).into())
    } else {
        Ok(())
    }
}

fn default_true() -> bool {
    true
}

#[derive(Deserialize)]
struct QuestionCreate {
    code: String,
    section: String,
    text: String,
    input_type: String,
    #[serde(default)]
    options: Option<serde_json::Value>,
    #[serde(default)]
    feature_key: Option<String>,
    #[serde(default = "default_true")]
    required: bool,
    #[serde(default)]
    evidence_citation: Option<String>,
    /// Audit citation for this change (required).
    citation: String,
}

/// Admin-supplied codes become part of public payloads and ORDER BY expressions — keep them tame.
fn validate_code(code: &str) -> Result<(), ApiError> {
    let ok = !code.is_empty()
        && code.len() <= 64
        && code.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if ok { Ok(()) } else {
        Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "code must be 1-64 characters of letters, digits, or underscore",
        ))
    }
}

/// Create a question (admin). Writes an audit event; duplicate code → 409.
async fn admin_create_question(
    State(s): State<Arc<AppState>>,
    Admin(admin): Admin,
    Json(req): Json<QuestionCreate>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_citation(&req.citation)?;
    validate_code(&req.code)?;
    let after = db::admin_create_question(
        &s.pool, admin, &req.code, &req.section, &req.text, &req.input_type,
        req.options.as_ref(), req.feature_key.as_deref(), req.required, req.evidence_citation.as_deref(),
        &req.citation,
    )
    .await
    .map_err(|e| {
        if db::is_unique_violation(&e) {
            ApiError::new(StatusCode::CONFLICT, "question code already exists")
        } else if db::is_foreign_key_violation(&e) {
            ApiError::new(StatusCode::BAD_REQUEST, "unknown feature_key")
        } else {
            db_err(e)
        }
    })?;
    Ok(Json(after))
}

#[derive(Deserialize)]
struct QuestionUpdate {
    #[serde(default)]
    section: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    input_type: Option<String>,
    #[serde(default)]
    options: Option<serde_json::Value>,
    #[serde(default)]
    required: Option<bool>,
    #[serde(default)]
    active: Option<bool>,
    #[serde(default)]
    evidence_citation: Option<String>,
    citation: String,
}

/// Update a question (admin): COALESCE the provided fields, bump version, write an audit event.
async fn admin_update_question(
    State(s): State<Arc<AppState>>,
    Admin(admin): Admin,
    AxumPath(code): AxumPath<String>,
    Json(req): Json<QuestionUpdate>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_citation(&req.citation)?;
    let after = db::admin_update_question(
        &s.pool, admin, &code, req.section.as_deref(), req.text.as_deref(), req.input_type.as_deref(),
        req.options.as_ref(), req.required, req.active, req.evidence_citation.as_deref(), &req.citation,
    )
    .await
    .map_err(db_err)?
    .ok_or((StatusCode::NOT_FOUND, format!("unknown question: {code}")))?;
    Ok(Json(after))
}

#[derive(Deserialize)]
struct FeatureUpdate {
    #[serde(default)] name: Option<String>,
    #[serde(default)] role: Option<String>,
    #[serde(default)] evidence_grade: Option<String>,
    #[serde(default)] feature_citation: Option<String>,
    #[serde(default)] formula_note: Option<String>,
    #[serde(default)] active: Option<bool>,
    citation: String,
}

/// Update a feature (admin): COALESCE the provided fields, write an audit event.
async fn admin_update_feature(
    State(s): State<Arc<AppState>>,
    Admin(admin): Admin,
    AxumPath(key): AxumPath<String>,
    Json(req): Json<FeatureUpdate>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_citation(&req.citation)?;
    let after = db::admin_update_feature(
        &s.pool, admin, &key, req.name.as_deref(), req.role.as_deref(), req.evidence_grade.as_deref(),
        req.feature_citation.as_deref(), req.formula_note.as_deref(), req.active, &req.citation,
    )
    .await
    .map_err(|e| {
        if db::is_check_violation(&e) {
            ApiError::new(StatusCode::BAD_REQUEST, "invalid role or evidence_grade")
        } else {
            db_err(e)
        }
    })?
    .ok_or((StatusCode::NOT_FOUND, format!("unknown feature: {key}")))?;
    Ok(Json(after))
}

#[derive(Deserialize)]
struct RuleCreate {
    code: String,
    feature_key: String,
    condition: serde_json::Value,
    message: String,
    #[serde(default)] priority: i32,
    /// The rule's own required evidence citation (NOT NULL).
    evidence_citation: String,
    /// Audit citation for this change.
    citation: String,
}

/// Create a recommendation rule (admin). Duplicate code → 409; unknown feature_key → 400.
async fn admin_create_rule(
    State(s): State<Arc<AppState>>,
    Admin(admin): Admin,
    Json(req): Json<RuleCreate>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_citation(&req.citation)?;
    validate_code(&req.code)?;
    if req.evidence_citation.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "evidence_citation is required for a rule".to_string()).into());
    }
    let after = db::admin_create_rule(
        &s.pool, admin, &req.code, &req.feature_key, &req.condition, &req.message, req.priority,
        &req.evidence_citation, &req.citation,
    )
    .await
    .map_err(|e| {
        if db::is_unique_violation(&e) {
            ApiError::new(StatusCode::CONFLICT, "rule code already exists")
        } else if db::is_foreign_key_violation(&e) {
            ApiError::new(StatusCode::BAD_REQUEST, "unknown feature_key")
        } else {
            db_err(e)
        }
    })?;
    Ok(Json(after))
}

#[derive(Deserialize)]
struct RuleUpdate {
    #[serde(default)] feature_key: Option<String>,
    #[serde(default)] condition: Option<serde_json::Value>,
    #[serde(default)] message: Option<String>,
    #[serde(default)] priority: Option<i32>,
    #[serde(default)] active: Option<bool>,
    #[serde(default)] evidence_citation: Option<String>,
    citation: String,
}

/// Update a recommendation rule (admin). Unknown code → 404; unknown feature_key → 400.
async fn admin_update_rule(
    State(s): State<Arc<AppState>>,
    Admin(admin): Admin,
    AxumPath(code): AxumPath<String>,
    Json(req): Json<RuleUpdate>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_citation(&req.citation)?;
    // A rule's evidence citation must never be blanked (evidence traceability).
    if req.evidence_citation.as_deref().is_some_and(|c| c.trim().is_empty()) {
        return Err((StatusCode::BAD_REQUEST, "evidence_citation cannot be blank".to_string()).into());
    }
    let after = db::admin_update_rule(
        &s.pool, admin, &code, req.feature_key.as_deref(), req.condition.as_ref(), req.message.as_deref(),
        req.priority, req.active, req.evidence_citation.as_deref(), &req.citation,
    )
    .await
    .map_err(|e| {
        if db::is_foreign_key_violation(&e) {
            ApiError::new(StatusCode::BAD_REQUEST, "unknown feature_key")
        } else {
            db_err(e)
        }
    })?
    .ok_or((StatusCode::NOT_FOUND, format!("unknown rule: {code}")))?;
    Ok(Json(after))
}

#[derive(Deserialize)]
struct PinModelRequest {
    semver: String,
    citation: String,
}

/// Pin a model version active (admin). 404 if the semver is unknown. Writes a pin_model audit event.
async fn admin_pin_model(
    State(s): State<Arc<AppState>>,
    Admin(admin): Admin,
    Json(req): Json<PinModelRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_citation(&req.citation)?;
    let after = db::admin_pin_model(&s.pool, admin, &req.semver, &req.citation)
        .await
        .map_err(db_err)?
        .ok_or((StatusCode::NOT_FOUND, format!("unknown model version: {}", req.semver)))?;
    Ok(Json(after))
}

/// The audit log (admin only), newest first.
async fn audit_route(
    State(s): State<Arc<AppState>>,
    Admin(_admin): Admin,
) -> Result<Json<Vec<db::AuditRow>>, ApiError> {
    let rows = db::list_audit_events(&s.pool, 200).await.map_err(db_err)?;
    Ok(Json(rows))
}

/// Resolve the caller's profile id (each account has exactly one profile).
async fn caller_profile(s: &AppState, account: Uuid) -> Result<Uuid, ApiError> {
    db::profile_id_for_account(&s.pool, account)
        .await
        .map_err(db_err)?
        .ok_or_else(|| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "no profile for account"))
}

#[derive(Deserialize)]
struct AnswerInput {
    question_code: String,
    value: serde_json::Value,
}

#[derive(Deserialize)]
struct AnswersRequest {
    answers: Vec<AnswerInput>,
}

/// Upsert the current answers for the authenticated caller's profile.
async fn post_answers_route(
    State(s): State<Arc<AppState>>,
    Auth(account): Auth,
    Json(req): Json<AnswersRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let profile_id = caller_profile(&s, account).await?;
    for a in &req.answers {
        db::upsert_answer(&s.pool, profile_id, &a.question_code, &a.value)
            .await
            .map_err(|e| match e {
                sqlx::Error::RowNotFound => ApiError::new(
                    StatusCode::BAD_REQUEST,
                    format!("unknown question code: {}", a.question_code),
                ),
                other => db_err(other),
            })?;
    }
    Ok(Json(json!({ "saved": req.answers.len() })))
}

/// The model's ontology: what each factor is, how it is classified, which article backs it, and
/// the causal graph the fit was constrained by. Public, because it is the evidence behind every
/// number the product shows — the web renders the graph and the article links straight from it.
async fn ontology_route(State(s): State<Arc<AppState>>) -> Result<Json<serde_json::Value>, ApiError> {
    if s.bundle.ontology.is_null() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "this model bundle ships no ontology (pre-v3.0.0)".to_string(),
        ));
    }
    Ok(Json(s.bundle.ontology.clone()))
}

/// Read back the current answers for the authenticated caller's profile.
async fn get_answers_route(
    State(s): State<Arc<AppState>>,
    Auth(account): Auth,
) -> Result<Json<Vec<db::AnswerRow>>, ApiError> {
    let profile_id = caller_profile(&s, account).await?;
    let rows = db::list_answers(&s.pool, profile_id).await.map_err(db_err)?;
    Ok(Json(rows))
}
