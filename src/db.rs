//! Database layer: connection pool, migrations, and the persistence/read queries.
//!
//! Uses SQLx runtime-checked queries (not the compile-time `query!` macros) so the crate builds
//! without a live database; correctness is covered by the integration tests in `tests/`.
//! NUMERIC columns are bound from `f64` (PostgreSQL applies the assignment cast) and read back with an
//! explicit `::float8` cast, so no decimal crate is needed.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::postgres::{PgPool, PgPoolOptions};
use uuid::Uuid;

/// Embedded migrations (the `migrations/` directory), run at startup.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Open a connection pool. Accepts any URL SQLx understands, including a unix-socket form such as
/// `postgresql:///clock_of_life?host=/var/run/postgresql` (peer auth, no password).
pub async fn connect(url: &str) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new().max_connections(8).connect(url).await
}

/// Apply any pending migrations (idempotent — SQLx tracks applied versions in `_sqlx_migrations`).
pub async fn run_migrations(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    MIGRATOR.run(pool).await
}

/// True if the error is a unique-constraint violation (e.g. duplicate email_hash) → map to 409.
pub fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.is_unique_violation())
}

/// Create an account and its (empty) profile in one transaction. Returns (account_id, profile_id).
/// A duplicate `email_hash` surfaces as a unique-violation error (see `is_unique_violation`).
pub async fn create_account(
    pool: &PgPool,
    email_hash: &str,
    password_hash: &str,
    locale: &str,
) -> Result<(Uuid, Uuid), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let account_id: Uuid = sqlx::query_scalar(
        "INSERT INTO account (email_hash, password_hash, locale) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(email_hash)
    .bind(password_hash)
    .bind(locale)
    .fetch_one(&mut *tx)
    .await?;
    let profile_id: Uuid =
        sqlx::query_scalar("INSERT INTO profile (account_id) VALUES ($1) RETURNING id")
            .bind(account_id)
            .fetch_one(&mut *tx)
            .await?;
    tx.commit().await?;
    Ok((account_id, profile_id))
}

/// Look up an account by its email hash, returning (id, stored password hash) if present.
pub async fn find_account_by_email_hash(
    pool: &PgPool,
    email_hash: &str,
) -> Result<Option<(Uuid, String)>, sqlx::Error> {
    sqlx::query_as("SELECT id, password_hash FROM account WHERE email_hash = $1")
        .bind(email_hash)
        .fetch_optional(pool)
        .await
}

/// The profile id owned by an account (each account has exactly one profile).
pub async fn profile_id_for_account(
    pool: &PgPool,
    account_id: Uuid,
) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar("SELECT id FROM profile WHERE account_id = $1")
        .bind(account_id)
        .fetch_optional(pool)
        .await
}

/// One row of a user's append-only calculation history.
#[derive(Serialize, sqlx::FromRow)]
pub struct CalcRow {
    pub id: Uuid,
    pub input_hash: String,
    pub estimate_years: f64,
    pub interval_low: f64,
    pub interval_high: f64,
    pub reaches_age: f64,
    pub relative_risk: f64,
    pub inputs: Value,
    pub attributions: Value,
    pub created_at: DateTime<Utc>,
}

/// Append a calculation snapshot (the `calculation` table is append-only). Returns the new id.
#[allow(clippy::too_many_arguments)]
pub async fn insert_calculation(
    pool: &PgPool,
    account_id: Uuid,
    model_version_id: Uuid,
    input_hash: &str,
    inputs: &Value,
    estimate_years: f64,
    interval_low: f64,
    interval_high: f64,
    reaches_age: f64,
    relative_risk: f64,
    attributions: &Value,
) -> Result<Uuid, sqlx::Error> {
    sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO calculation
           (account_id, model_version_id, input_hash, inputs,
            estimate_years, interval_low, interval_high, reaches_age, relative_risk, attributions)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
         RETURNING id",
    )
    .bind(account_id)
    .bind(model_version_id)
    .bind(input_hash)
    .bind(inputs)
    .bind(estimate_years)
    .bind(interval_low)
    .bind(interval_high)
    .bind(reaches_age)
    .bind(relative_risk)
    .bind(attributions)
    .fetch_one(pool)
    .await
}

/// Most-recent-first calculation history for one account (progress timeline).
pub async fn list_calculations(
    pool: &PgPool,
    account_id: Uuid,
    limit: i64,
) -> Result<Vec<CalcRow>, sqlx::Error> {
    sqlx::query_as::<_, CalcRow>(
        "SELECT id, input_hash,
                estimate_years::float8  AS estimate_years,
                interval_low::float8    AS interval_low,
                interval_high::float8   AS interval_high,
                reaches_age::float8     AS reaches_age,
                relative_risk::float8   AS relative_risk,
                inputs, attributions, created_at
         FROM calculation
         WHERE account_id = $1
         ORDER BY created_at DESC, id DESC
         LIMIT $2",
    )
    .bind(account_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// An active recommendation rule joined to its feature's role + evidence grade.
#[derive(sqlx::FromRow)]
pub struct RuleRow {
    pub code: String,
    pub feature_key: String,
    pub condition: Value,
    pub message: String,
    pub priority: i32,
    pub evidence_citation: String,
    pub role: String,
    pub evidence_grade: Option<String>,
}

/// Active rules whose feature is a modifiable lever or a manageable condition (never context/baseline),
/// highest priority first. Context/baseline factors are explained, never turned into advice (ADR-001).
pub async fn active_recommendation_rules(pool: &PgPool) -> Result<Vec<RuleRow>, sqlx::Error> {
    sqlx::query_as::<_, RuleRow>(
        "SELECT r.code, r.feature_key, r.condition, r.message, r.priority, r.evidence_citation,
                f.role, f.evidence_grade
         FROM recommendation_rule r
         JOIN feature f ON f.key = r.feature_key
         WHERE r.active AND f.active AND f.role IN ('lever', 'manage')
         ORDER BY r.priority DESC, r.code",
    )
    .fetch_all(pool)
    .await
}

/// The account that owns a calculation, or None if the calculation id does not exist.
pub async fn calculation_owner(
    pool: &PgPool,
    calculation_id: Uuid,
) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar("SELECT account_id FROM calculation WHERE id = $1")
        .bind(calculation_id)
        .fetch_optional(pool)
        .await
}

/// Persist a What-If scenario forked from a base calculation. Returns the new scenario id.
pub async fn insert_scenario(
    pool: &PgPool,
    base_calculation_id: Uuid,
    modifications: &Value,
    result: &Value,
) -> Result<Uuid, sqlx::Error> {
    sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO scenario (base_calculation_id, modifications, result)
         VALUES ($1, $2, $3)
         RETURNING id",
    )
    .bind(base_calculation_id)
    .bind(modifications)
    .bind(result)
    .fetch_one(pool)
    .await
}

/// A stored answer, joined to its question code for read-back.
#[derive(Serialize, sqlx::FromRow)]
pub struct AnswerRow {
    pub question_code: String,
    pub question_version: i32,
    pub value: Value,
    pub created_at: DateTime<Utc>,
}

/// Upsert one answer for a profile (one current answer per question). Looks up the question by its
/// stable `code` and records the version answered. Errors if the code is unknown.
pub async fn upsert_answer(
    pool: &PgPool,
    profile_id: Uuid,
    question_code: &str,
    value: &Value,
) -> Result<(), sqlx::Error> {
    let row: Option<(Uuid, i32)> =
        sqlx::query_as("SELECT id, version FROM question WHERE code = $1 AND active")
            .bind(question_code)
            .fetch_optional(pool)
            .await?;
    let (question_id, version) = row.ok_or_else(|| sqlx::Error::RowNotFound)?;

    sqlx::query(
        "INSERT INTO answer (profile_id, question_id, question_version, value)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (profile_id, question_id)
         DO UPDATE SET value = EXCLUDED.value,
                       question_version = EXCLUDED.question_version,
                       created_at = now()",
    )
    .bind(profile_id)
    .bind(question_id)
    .bind(version)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

/// All current answers for a profile, ordered by question code.
pub async fn list_answers(pool: &PgPool, profile_id: Uuid) -> Result<Vec<AnswerRow>, sqlx::Error> {
    sqlx::query_as::<_, AnswerRow>(
        "SELECT q.code AS question_code, a.question_version, a.value, a.created_at
         FROM answer a
         JOIN question q ON q.id = a.question_id
         WHERE a.profile_id = $1
         ORDER BY q.code",
    )
    .bind(profile_id)
    .fetch_all(pool)
    .await
}
