//! The Clock of Life — Rust scoring service (library root).
//!
//! Loads a model artifact bundle, connects to PostgreSQL, reconciles the reference tables, and serves
//! estimates. State-changing calls (estimate/what-if) are persisted; history and answers are read back.
//! The binary in `main.rs` is a thin wrapper; the router is exposed here so integration tests can drive
//! it in-process.

pub mod auth;
pub mod bundle;
pub mod db;
pub mod scoring;
pub mod seed;

use std::path::Path;
use std::sync::Arc;

use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use bundle::Bundle;
use scoring::{estimate, whatif, Estimate, Profile, WhatIf, WhatIfChanges};
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
}

/// Connection string for the application database (unix socket + peer auth by default).
pub fn default_database_url() -> String {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgresql:///clock_of_life?host=/var/run/postgresql".to_string())
}

/// The JWT signing secret from `JWT_SECRET`. Falls back to an insecure dev key with a loud warning —
/// production must set `JWT_SECRET` (tokens signed with the dev key are worthless if the env is set).
pub fn jwt_secret() -> Vec<u8> {
    match std::env::var("JWT_SECRET") {
        Ok(s) if !s.is_empty() => s.into_bytes(),
        _ => {
            eprintln!("WARNING: JWT_SECRET not set — using an insecure development key. Set JWT_SECRET in production.");
            b"insecure-dev-key-do-not-use-in-production".to_vec()
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
    let seeded = seed::reconcile(&pool, &b.manifest, bundle_dir)
        .await
        .map_err(|e| format!("db reconcile: {e}"))?;
    Ok(Arc::new(AppState {
        bundle: Arc::new(b),
        pool,
        active_model_id: seeded.active_model_id,
        anon_account_id: seeded.anon_account_id,
        anon_profile_id: seeded.anon_profile_id,
        jwt_secret: jwt_secret(),
        token_ttl_secs: 7 * 24 * 3600, // 7 days
    }))
}

/// The HTTP surface. Kept separate from `init_state` so tests can build it over any state.
pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/meta", get(meta))
        .route("/api/auth/register", post(register_route))
        .route("/api/auth/login", post(login_route))
        .route("/api/estimate", post(estimate_route))
        .route("/api/whatif", post(whatif_route))
        .route("/api/calculations", get(calculations_route))
        .route("/api/answers", get(get_answers_route).post(post_answers_route))
        .with_state(state)
}

fn db_err(e: sqlx::Error) -> (StatusCode, String) {
    // Keep the detail server-side; return a generic message so internal query/schema detail never
    // reaches the client.
    eprintln!("database error: {e}");
    (StatusCode::INTERNAL_SERVER_ERROR, "internal error".to_string())
}

/// First 16 hex chars of the SHA-256 of the serialized inputs (a stable snapshot fingerprint).
fn input_hash(inputs: &serde_json::Value) -> String {
    let bytes = serde_json::to_vec(inputs).unwrap_or_default();
    let digest = Sha256::digest(&bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect::<String>()[..16].to_string()
}

async fn health(State(s): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(json!({
        "status": "ok",
        "model_version": s.bundle.manifest.version,
        "countries": s.bundle.baselines.len(),
    }))
}

/// Active model version + provenance.
async fn meta(State(s): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let mut countries: Vec<&String> = s.bundle.baselines.keys().collect();
    countries.sort();
    Json(json!({
        "model_version": s.bundle.manifest.version,
        "algorithm": s.bundle.manifest.algorithm,
        "countries": countries,
        "assumptions": [
            "statistical estimate, not a prediction or diagnosis",
            "relative risk centred on the selected country's average person",
        ],
    }))
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
) -> Result<Json<AuthResponse>, (StatusCode, String)> {
    if req.email.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "email required".to_string()));
    }
    if req.password.len() < 8 {
        return Err((StatusCode::BAD_REQUEST, "password must be at least 8 characters".to_string()));
    }
    let email_hash = auth::email_hash(&req.email);
    let password_hash = auth::hash_password(&req.password)
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "internal error".to_string()))?;
    let locale = req.locale.as_deref().unwrap_or("ro");
    let (account_id, _profile_id) = db::create_account(&s.pool, &email_hash, &password_hash, locale)
        .await
        .map_err(|e| {
            if db::is_unique_violation(&e) {
                (StatusCode::CONFLICT, "account already exists".to_string())
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
) -> Result<Json<AuthResponse>, (StatusCode, String)> {
    let email_hash = auth::email_hash(&req.email);
    let account = db::find_account_by_email_hash(&s.pool, &email_hash)
        .await
        .map_err(db_err)?;
    let unauthorized = || (StatusCode::UNAUTHORIZED, "invalid credentials".to_string());
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

#[derive(Serialize)]
struct EstimateResponse {
    #[serde(flatten)]
    estimate: Estimate,
    calculation_id: Uuid,
}

/// Answers -> Life-Clock estimate. Persists an append-only `calculation` snapshot.
async fn estimate_route(
    State(s): State<Arc<AppState>>,
    Json(profile): Json<Profile>,
) -> Result<Json<EstimateResponse>, (StatusCode, String)> {
    let est = estimate(&s.bundle, &profile).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let inputs = serde_json::to_value(&profile)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("serialize inputs: {e}")))?;
    // Per-factor attributions are a later surface ("Why?"); the v1 estimate stores an empty list.
    let attributions = json!([]);
    let id = db::insert_calculation(
        &s.pool,
        s.anon_account_id,
        s.active_model_id,
        &input_hash(&inputs),
        &inputs,
        est.estimate_years,
        est.interval[0],
        est.interval[1],
        est.reaches_age,
        est.relative_risk,
        &attributions,
    )
    .await
    .map_err(db_err)?;
    Ok(Json(EstimateResponse { estimate: est, calculation_id: id }))
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
    Json(req): Json<WhatIfRequest>,
) -> Result<Json<WhatIfResponse>, (StatusCode, String)> {
    let wi = whatif(&s.bundle, &req.base, &req.changes).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let scenario_id = if let Some(base_id) = req.base_calculation_id {
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

/// Calculation history for the current (anonymous, pre-auth) account, newest first.
async fn calculations_route(
    State(s): State<Arc<AppState>>,
) -> Result<Json<Vec<db::CalcRow>>, (StatusCode, String)> {
    let rows = db::list_calculations(&s.pool, s.anon_account_id, 50)
        .await
        .map_err(db_err)?;
    Ok(Json(rows))
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

/// Upsert the current answers for the (anonymous) profile.
async fn post_answers_route(
    State(s): State<Arc<AppState>>,
    Json(req): Json<AnswersRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    for a in &req.answers {
        db::upsert_answer(&s.pool, s.anon_profile_id, &a.question_code, &a.value)
            .await
            .map_err(|e| match e {
                sqlx::Error::RowNotFound => (
                    StatusCode::BAD_REQUEST,
                    format!("unknown question code: {}", a.question_code),
                ),
                other => db_err(other),
            })?;
    }
    Ok(Json(json!({ "saved": req.answers.len() })))
}

/// Read back the current answers for the (anonymous) profile.
async fn get_answers_route(
    State(s): State<Arc<AppState>>,
) -> Result<Json<Vec<db::AnswerRow>>, (StatusCode, String)> {
    let rows = db::list_answers(&s.pool, s.anon_profile_id)
        .await
        .map_err(db_err)?;
    Ok(Json(rows))
}
