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

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
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
        .route("/api/questions", get(questions_route))
        .route("/api/references", get(references_route))
        .route("/api/auth/register", post(register_route))
        .route("/api/auth/login", post(login_route))
        .route("/api/estimate", post(estimate_route))
        .route("/api/recommendations", post(recommendations_route))
        .route("/api/whatif", post(whatif_route))
        .route("/api/calculations", get(calculations_route))
        .route("/api/profile", get(profile_route))
        .route("/api/answers", get(get_answers_route).post(post_answers_route))
        .with_state(state)
}

/// Extractor for an authenticated caller: a valid `Authorization: Bearer <jwt>` yields the account id.
pub struct Auth(pub Uuid);

#[axum::async_trait]
impl axum::extract::FromRequestParts<Arc<AppState>> for Auth {
    type Rejection = (StatusCode, String);

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let unauth = |m: &str| (StatusCode::UNAUTHORIZED, m.to_string());
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
        Ok(Auth(id))
    }
}

/// Like `Auth`, but never rejects — `None` when no valid token is present (try-before-signup paths).
pub struct OptionalAuth(pub Option<Uuid>);

#[axum::async_trait]
impl axum::extract::FromRequestParts<Arc<AppState>> for OptionalAuth {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        Ok(OptionalAuth(Auth::from_request_parts(parts, state).await.ok().map(|a| a.0)))
    }
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

#[derive(Deserialize)]
struct RefQuery {
    feature: Option<String>,
    rule: Option<String>,
}

/// Evidence references (public). All studies, or those linked to a `?feature=<key>` or `?rule=<code>`.
async fn references_route(
    State(s): State<Arc<AppState>>,
    Query(q): Query<RefQuery>,
) -> Result<Json<Vec<db::Study>>, (StatusCode, String)> {
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
) -> Result<Json<Vec<db::QuestionRow>>, (StatusCode, String)> {
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
) -> Result<HashMap<String, Vec<db::Study>>, (StatusCode, String)> {
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
) -> Result<Json<EstimateResponse>, (StatusCode, String)> {
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
    /// Years currently at stake on this factor (|attribution delta|).
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
) -> Result<Json<Vec<Recommendation>>, (StatusCode, String)> {
    // attributions() validates the profile and gives the per-factor years-at-stake used for ranking.
    let why = attributions(&s.bundle, &profile).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let impact: std::collections::HashMap<&str, f64> =
        why.iter().map(|a| (a.key.as_str(), a.delta_years.abs())).collect();

    let rules = db::active_recommendation_rules(&s.pool).await.map_err(db_err)?;
    let mut recs: Vec<Recommendation> = Vec::new();
    for r in rules {
        if !eval_condition(&profile, &r.condition) {
            continue;
        }
        let impact_years = impact.get(r.feature_key.as_str()).copied().unwrap_or(0.0);
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
) -> Result<Json<WhatIfResponse>, (StatusCode, String)> {
    let wi = whatif(&s.bundle, &req.base, &req.changes).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let scenario_id = if let Some(base_id) = req.base_calculation_id {
        // Persisting a scenario requires auth and that the base calculation belongs to the caller.
        let account = account.ok_or((
            StatusCode::UNAUTHORIZED,
            "authentication required to save a scenario".to_string(),
        ))?;
        match db::calculation_owner(&s.pool, base_id).await.map_err(db_err)? {
            Some(owner) if owner == account => {}
            Some(_) => return Err((StatusCode::FORBIDDEN, "not your calculation".to_string())),
            None => return Err((StatusCode::NOT_FOUND, "base calculation not found".to_string())),
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
) -> Result<Json<Vec<db::CalcRow>>, (StatusCode, String)> {
    let rows = db::list_calculations(&s.pool, account, 50)
        .await
        .map_err(db_err)?;
    Ok(Json(rows))
}

/// The authenticated caller's saved profile: metadata + current answers.
async fn profile_route(
    State(s): State<Arc<AppState>>,
    Auth(account): Auth,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let profile = db::get_profile(&s.pool, account)
        .await
        .map_err(db_err)?
        .ok_or((StatusCode::INTERNAL_SERVER_ERROR, "no profile for account".to_string()))?;
    let answers = db::list_answers(&s.pool, profile.id).await.map_err(db_err)?;
    Ok(Json(json!({
        "profile_id": profile.id,
        "home_location_id": profile.home_location_id,
        "updated_at": profile.updated_at,
        "answers": answers,
    })))
}

/// Resolve the caller's profile id (each account has exactly one profile).
async fn caller_profile(s: &AppState, account: Uuid) -> Result<Uuid, (StatusCode, String)> {
    db::profile_id_for_account(&s.pool, account)
        .await
        .map_err(db_err)?
        .ok_or((StatusCode::INTERNAL_SERVER_ERROR, "no profile for account".to_string()))
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
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let profile_id = caller_profile(&s, account).await?;
    for a in &req.answers {
        db::upsert_answer(&s.pool, profile_id, &a.question_code, &a.value)
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

/// Read back the current answers for the authenticated caller's profile.
async fn get_answers_route(
    State(s): State<Arc<AppState>>,
    Auth(account): Auth,
) -> Result<Json<Vec<db::AnswerRow>>, (StatusCode, String)> {
    let profile_id = caller_profile(&s, account).await?;
    let rows = db::list_answers(&s.pool, profile_id).await.map_err(db_err)?;
    Ok(Json(rows))
}
