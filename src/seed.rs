//! Startup reconciliation: bring the database's reference tables to the desired state.
//!
//! Never trust-by-default (principle 12): every startup upserts the `feature` and `question` seeds and
//! pins the loaded bundle as the one active `model_version`. Idempotent — safe to run on every boot.
//!
//! A fixed pseudonymous "anonymous" account owns pre-auth calculations until real accounts land
//! (SVC-DB5); `calculation.account_id` is NOT NULL, so persistence needs an owner now.

use serde::Deserialize;
use sqlx::postgres::PgPool;
use uuid::{uuid, Uuid};

use crate::bundle::Manifest;

/// Owner of pre-auth calculations/answers. Stable UUIDs so restarts and tests agree.
pub const ANON_ACCOUNT_ID: Uuid = uuid!("00000000-0000-0000-0000-000000000001");
pub const ANON_PROFILE_ID: Uuid = uuid!("00000000-0000-0000-0000-000000000002");

const FEATURES_JSON: &str = include_str!("../seeds/features.json");
const QUESTIONS_JSON: &str = include_str!("../seeds/questions.json");
const RULES_JSON: &str = include_str!("../seeds/recommendation_rules.json");

#[derive(Deserialize)]
struct FeatureSeed {
    key: String,
    name: String,
    role: String,
    evidence_grade: String,
    citation: String,
    formula_note: String,
}

#[derive(Deserialize)]
struct RuleSeed {
    code: String,
    feature_key: String,
    condition: serde_json::Value,
    priority: i32,
    message: String,
    evidence_citation: String,
}

#[derive(Deserialize)]
struct QuestionSeed {
    code: String,
    section: String,
    text: String,
    input_type: String,
    options: Option<serde_json::Value>,
    feature_key: Option<String>,
    required: bool,
    evidence_citation: Option<String>,
}

/// Identifiers resolved during reconciliation, cached in application state.
pub struct SeedResult {
    pub active_model_id: Uuid,
    pub anon_account_id: Uuid,
    pub anon_profile_id: Uuid,
}

/// Run the full reconciliation. `artifact_uri` is where the active bundle lives (e.g. the bundle dir).
pub async fn reconcile(
    pool: &PgPool,
    manifest: &Manifest,
    artifact_uri: &str,
) -> Result<SeedResult, sqlx::Error> {
    seed_features(pool).await?;
    seed_questions(pool).await?;
    seed_recommendation_rules(pool).await?;
    let active_model_id = pin_model_version(pool, manifest, artifact_uri).await?;
    ensure_anonymous(pool).await?;
    Ok(SeedResult {
        active_model_id,
        anon_account_id: ANON_ACCOUNT_ID,
        anon_profile_id: ANON_PROFILE_ID,
    })
}

async fn seed_features(pool: &PgPool) -> Result<(), sqlx::Error> {
    let features: Vec<FeatureSeed> =
        serde_json::from_str(FEATURES_JSON).expect("features.json seed is valid");
    for f in &features {
        sqlx::query(
            "INSERT INTO feature (key, name, role, evidence_grade, citation, formula_note, active)
             VALUES ($1, $2, $3, $4, $5, $6, true)
             ON CONFLICT (key) DO UPDATE SET
                 name = EXCLUDED.name, role = EXCLUDED.role,
                 evidence_grade = EXCLUDED.evidence_grade, citation = EXCLUDED.citation,
                 formula_note = EXCLUDED.formula_note, active = true",
        )
        .bind(&f.key)
        .bind(&f.name)
        .bind(&f.role)
        .bind(&f.evidence_grade)
        .bind(&f.citation)
        .bind(&f.formula_note)
        .execute(pool)
        .await?;
    }
    Ok(())
}

async fn seed_questions(pool: &PgPool) -> Result<(), sqlx::Error> {
    let questions: Vec<QuestionSeed> =
        serde_json::from_str(QUESTIONS_JSON).expect("questions.json seed is valid");
    for q in &questions {
        sqlx::query(
            "INSERT INTO question
                 (code, version, section, text, input_type, options, feature_key, required, active, evidence_citation)
             VALUES ($1, 1, $2, $3, $4, $5, $6, $7, true, $8)
             ON CONFLICT (code) DO UPDATE SET
                 section = EXCLUDED.section, text = EXCLUDED.text,
                 input_type = EXCLUDED.input_type, options = EXCLUDED.options,
                 feature_key = EXCLUDED.feature_key, required = EXCLUDED.required,
                 active = true, evidence_citation = EXCLUDED.evidence_citation",
        )
        .bind(&q.code)
        .bind(&q.section)
        .bind(&q.text)
        .bind(&q.input_type)
        .bind(&q.options)
        .bind(&q.feature_key)
        .bind(q.required)
        .bind(&q.evidence_citation)
        .execute(pool)
        .await?;
    }
    Ok(())
}

async fn seed_recommendation_rules(pool: &PgPool) -> Result<(), sqlx::Error> {
    let rules: Vec<RuleSeed> =
        serde_json::from_str(RULES_JSON).expect("recommendation_rules.json seed is valid");
    for r in &rules {
        sqlx::query(
            "INSERT INTO recommendation_rule
                 (code, feature_key, condition, message, priority, evidence_citation, active)
             VALUES ($1, $2, $3, $4, $5, $6, true)
             ON CONFLICT (code) DO UPDATE SET
                 feature_key = EXCLUDED.feature_key, condition = EXCLUDED.condition,
                 message = EXCLUDED.message, priority = EXCLUDED.priority,
                 evidence_citation = EXCLUDED.evidence_citation, active = true",
        )
        .bind(&r.code)
        .bind(&r.feature_key)
        .bind(&r.condition)
        .bind(&r.message)
        .bind(r.priority)
        .bind(&r.evidence_citation)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Upsert the loaded bundle as a `model_version` and make it the single active one.
async fn pin_model_version(
    pool: &PgPool,
    manifest: &Manifest,
    artifact_uri: &str,
) -> Result<Uuid, sqlx::Error> {
    let reference_population = manifest
        .reference_population
        .clone()
        .unwrap_or_else(|| "RO_2024".to_string());
    let data_as_of = manifest
        .data_as_of
        .as_deref()
        .and_then(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());

    let mut tx = pool.begin().await?;
    // Insert (or refresh provenance of) this version, initially without forcing active.
    sqlx::query(
        "INSERT INTO model_version
             (semver, artifact_uri, algorithm, reference_population, data_as_of, is_active)
         VALUES ($1, $2, $3, $4, $5, false)
         ON CONFLICT (semver) DO UPDATE SET
             artifact_uri = EXCLUDED.artifact_uri, algorithm = EXCLUDED.algorithm,
             reference_population = EXCLUDED.reference_population, data_as_of = EXCLUDED.data_as_of",
    )
    .bind(&manifest.version)
    .bind(artifact_uri)
    .bind(&manifest.algorithm)
    .bind(&reference_population)
    .bind(data_as_of)
    .execute(&mut *tx)
    .await?;
    // Enforce a single active model (partial unique index `one_active_model`): clear others, set ours.
    sqlx::query("UPDATE model_version SET is_active = false WHERE semver <> $1 AND is_active")
        .bind(&manifest.version)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE model_version SET is_active = true WHERE semver = $1")
        .bind(&manifest.version)
        .execute(&mut *tx)
        .await?;
    let id: Uuid = sqlx::query_scalar("SELECT id FROM model_version WHERE semver = $1")
        .bind(&manifest.version)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(id)
}

async fn ensure_anonymous(pool: &PgPool) -> Result<(), sqlx::Error> {
    // password_hash is a non-argon2 sentinel, so this account can never be logged into.
    sqlx::query(
        "INSERT INTO account (id, email_hash, password_hash, locale)
         VALUES ($1, 'anonymous', '!disabled', 'ro')
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(ANON_ACCOUNT_ID)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO profile (id, account_id) VALUES ($1, $2) ON CONFLICT (id) DO NOTHING",
    )
    .bind(ANON_PROFILE_ID)
    .bind(ANON_ACCOUNT_ID)
    .execute(pool)
    .await?;
    Ok(())
}
