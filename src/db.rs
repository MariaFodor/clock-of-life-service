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
