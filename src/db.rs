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

/// One question of the interview definition (for rendering onboarding).
#[derive(Serialize, sqlx::FromRow)]
pub struct QuestionRow {
    pub code: String,
    pub version: i32,
    pub section: String,
    pub text: String,
    pub input_type: String,
    pub options: Option<Value>,
    pub feature_key: Option<String>,
    pub required: bool,
    pub evidence_citation: Option<String>,
}

/// The active interview definition, ordered by the question's numeric code (Q1…Q24).
pub async fn list_questions(pool: &PgPool) -> Result<Vec<QuestionRow>, sqlx::Error> {
    sqlx::query_as::<_, QuestionRow>(
        "SELECT code, version, section, text, input_type, options, feature_key, required,
                evidence_citation
         FROM question
         WHERE active
         ORDER BY (regexp_replace(code, '[^0-9]', '', 'g'))::int",
    )
    .fetch_all(pool)
    .await
}

/// A profile's metadata (its answers are read separately via `list_answers`).
#[derive(Serialize, sqlx::FromRow)]
pub struct ProfileRow {
    pub id: Uuid,
    pub home_location_id: Option<Uuid>,
    pub updated_at: DateTime<Utc>,
}

/// The profile row owned by an account.
pub async fn get_profile(pool: &PgPool, account_id: Uuid) -> Result<Option<ProfileRow>, sqlx::Error> {
    sqlx::query_as::<_, ProfileRow>(
        "SELECT id, home_location_id, updated_at FROM profile WHERE account_id = $1",
    )
    .bind(account_id)
    .fetch_optional(pool)
    .await
}

/// True if the error is a foreign-key violation (e.g. unknown feature_key) → map to 400/409.
pub fn is_foreign_key_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.is_foreign_key_violation())
}

/// Append an audit event inside an existing transaction (keeps mutation + audit atomic).
#[allow(clippy::too_many_arguments)]
async fn audit_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    admin_id: Uuid,
    entity: &str,
    entity_id: &str,
    action: &str,
    before: Option<&Value>,
    after: Option<&Value>,
    citation: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO audit_event (admin_id, entity, entity_id, action, before, after, citation)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(admin_id).bind(entity).bind(entity_id).bind(action).bind(before).bind(after).bind(citation)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Create a question AND its audit event atomically. Returns the new row as JSON.
/// Duplicate code → unique violation; unknown feature_key → foreign-key violation.
#[allow(clippy::too_many_arguments)]
pub async fn admin_create_question(
    pool: &PgPool,
    admin_id: Uuid,
    code: &str,
    section: &str,
    text: &str,
    input_type: &str,
    options: Option<&Value>,
    feature_key: Option<&str>,
    required: bool,
    evidence_citation: Option<&str>,
    citation: &str,
) -> Result<Value, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let after: Value = sqlx::query_scalar(
        "INSERT INTO question
             (code, version, section, text, input_type, options, feature_key, required, active, evidence_citation)
         VALUES ($1, 1, $2, $3, $4, $5, $6, $7, true, $8)
         RETURNING to_jsonb(question)",
    )
    .bind(code).bind(section).bind(text).bind(input_type).bind(options)
    .bind(feature_key).bind(required).bind(evidence_citation)
    .fetch_one(&mut *tx)
    .await?;
    audit_in_tx(&mut tx, admin_id, "question", code, "create", None, Some(&after), citation).await?;
    tx.commit().await?;
    Ok(after)
}

/// Update a question (COALESCE partial + version bump) AND its audit event atomically.
/// Returns the updated row as JSON, or None if the code is unknown (transaction rolled back).
#[allow(clippy::too_many_arguments)]
pub async fn admin_update_question(
    pool: &PgPool,
    admin_id: Uuid,
    code: &str,
    section: Option<&str>,
    text: Option<&str>,
    input_type: Option<&str>,
    options: Option<&Value>,
    required: Option<bool>,
    active: Option<bool>,
    evidence_citation: Option<&str>,
    citation: &str,
) -> Result<Option<Value>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let before: Option<Value> =
        sqlx::query_scalar("SELECT to_jsonb(question) FROM question WHERE code = $1")
            .bind(code)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(before) = before else {
        return Ok(None); // tx dropped (rolled back)
    };
    let after: Value = sqlx::query_scalar(
        "UPDATE question SET
             section = COALESCE($2, section),
             text = COALESCE($3, text),
             input_type = COALESCE($4, input_type),
             options = COALESCE($5, options),
             required = COALESCE($6, required),
             active = COALESCE($7, active),
             evidence_citation = COALESCE($8, evidence_citation),
             version = version + 1
         WHERE code = $1
         RETURNING to_jsonb(question)",
    )
    .bind(code).bind(section).bind(text).bind(input_type).bind(options)
    .bind(required).bind(active).bind(evidence_citation)
    .fetch_one(&mut *tx)
    .await?;
    audit_in_tx(&mut tx, admin_id, "question", code, "update", Some(&before), Some(&after), citation).await?;
    tx.commit().await?;
    Ok(Some(after))
}

/// True if the error is a CHECK-constraint violation (e.g. invalid role/grade) → map to 400.
pub fn is_check_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.is_check_violation())
}

/// Update a feature (COALESCE partial) AND its audit event atomically. None if the key is unknown.
#[allow(clippy::too_many_arguments)]
pub async fn admin_update_feature(
    pool: &PgPool,
    admin_id: Uuid,
    key: &str,
    name: Option<&str>,
    role: Option<&str>,
    evidence_grade: Option<&str>,
    feature_citation: Option<&str>,
    formula_note: Option<&str>,
    active: Option<bool>,
    audit_citation: &str,
) -> Result<Option<Value>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let before: Option<Value> =
        sqlx::query_scalar("SELECT to_jsonb(feature) FROM feature WHERE key = $1")
            .bind(key)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(before) = before else { return Ok(None) };
    let after: Value = sqlx::query_scalar(
        "UPDATE feature SET
             name = COALESCE($2, name),
             role = COALESCE($3, role),
             evidence_grade = COALESCE($4, evidence_grade),
             citation = COALESCE($5, citation),
             formula_note = COALESCE($6, formula_note),
             active = COALESCE($7, active)
         WHERE key = $1
         RETURNING to_jsonb(feature)",
    )
    .bind(key).bind(name).bind(role).bind(evidence_grade).bind(feature_citation).bind(formula_note).bind(active)
    .fetch_one(&mut *tx)
    .await?;
    audit_in_tx(&mut tx, admin_id, "feature", key, "update", Some(&before), Some(&after), audit_citation).await?;
    tx.commit().await?;
    Ok(Some(after))
}

/// Create a recommendation rule AND its audit event atomically. Duplicate code → unique violation;
/// unknown feature_key → foreign-key violation.
#[allow(clippy::too_many_arguments)]
pub async fn admin_create_rule(
    pool: &PgPool,
    admin_id: Uuid,
    code: &str,
    feature_key: &str,
    condition: &Value,
    message: &str,
    priority: i32,
    evidence_citation: &str,
    audit_citation: &str,
) -> Result<Value, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let after: Value = sqlx::query_scalar(
        "INSERT INTO recommendation_rule (code, feature_key, condition, message, priority, evidence_citation, active)
         VALUES ($1, $2, $3, $4, $5, $6, true)
         RETURNING to_jsonb(recommendation_rule)",
    )
    .bind(code).bind(feature_key).bind(condition).bind(message).bind(priority).bind(evidence_citation)
    .fetch_one(&mut *tx)
    .await?;
    audit_in_tx(&mut tx, admin_id, "recommendation_rule", code, "create", None, Some(&after), audit_citation).await?;
    tx.commit().await?;
    Ok(after)
}

/// Update a recommendation rule (COALESCE partial) AND its audit event atomically. None if unknown.
#[allow(clippy::too_many_arguments)]
pub async fn admin_update_rule(
    pool: &PgPool,
    admin_id: Uuid,
    code: &str,
    feature_key: Option<&str>,
    condition: Option<&Value>,
    message: Option<&str>,
    priority: Option<i32>,
    active: Option<bool>,
    evidence_citation: Option<&str>,
    audit_citation: &str,
) -> Result<Option<Value>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let before: Option<Value> =
        sqlx::query_scalar("SELECT to_jsonb(recommendation_rule) FROM recommendation_rule WHERE code = $1")
            .bind(code)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(before) = before else { return Ok(None) };
    let after: Value = sqlx::query_scalar(
        "UPDATE recommendation_rule SET
             feature_key = COALESCE($2, feature_key),
             condition = COALESCE($3, condition),
             message = COALESCE($4, message),
             priority = COALESCE($5, priority),
             active = COALESCE($6, active),
             evidence_citation = COALESCE($7, evidence_citation)
         WHERE code = $1
         RETURNING to_jsonb(recommendation_rule)",
    )
    .bind(code).bind(feature_key).bind(condition).bind(message).bind(priority).bind(active).bind(evidence_citation)
    .fetch_one(&mut *tx)
    .await?;
    audit_in_tx(&mut tx, admin_id, "recommendation_rule", code, "update", Some(&before), Some(&after), audit_citation).await?;
    tx.commit().await?;
    Ok(Some(after))
}

/// Pin a model version active (enforcing one-active) AND write a pin_model audit event, atomically.
/// None if the semver is unknown. Note: the running scorer keeps using its loaded bundle until a
/// deploy of the pinned bundle — pinning is configuration, not a code change (ADR-006).
pub async fn admin_pin_model(
    pool: &PgPool,
    admin_id: Uuid,
    semver: &str,
    audit_citation: &str,
) -> Result<Option<Value>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let before: Option<Value> =
        sqlx::query_scalar("SELECT to_jsonb(model_version) FROM model_version WHERE semver = $1")
            .bind(semver)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(before) = before else { return Ok(None) };
    // Clear other active rows first, then activate the target — keeps the one_active_model index satisfied.
    sqlx::query("UPDATE model_version SET is_active = false WHERE is_active AND semver <> $1")
        .bind(semver)
        .execute(&mut *tx)
        .await?;
    let after: Value = sqlx::query_scalar(
        "UPDATE model_version SET is_active = true WHERE semver = $1 RETURNING to_jsonb(model_version)",
    )
    .bind(semver)
    .fetch_one(&mut *tx)
    .await?;
    audit_in_tx(&mut tx, admin_id, "model_version", semver, "pin_model", Some(&before), Some(&after), audit_citation).await?;
    tx.commit().await?;
    Ok(Some(after))
}

/// Whether an account has the admin flag.
pub async fn is_admin(pool: &PgPool, account_id: Uuid) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query_scalar::<_, bool>("SELECT is_admin FROM account WHERE id = $1")
        .bind(account_id)
        .fetch_optional(pool)
        .await?
        .unwrap_or(false))
}

/// Promote/demote an account's admin flag (used by ops bootstrap + tests).
pub async fn set_admin(pool: &PgPool, account_id: Uuid, admin: bool) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE account SET is_admin = $1 WHERE id = $2")
        .bind(admin)
        .bind(account_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Append an audit event (every admin mutation writes one — evidence traceability).
#[allow(clippy::too_many_arguments)]
pub async fn insert_audit_event(
    pool: &PgPool,
    admin_id: Uuid,
    entity: &str,
    entity_id: &str,
    action: &str,
    before: Option<&Value>,
    after: Option<&Value>,
    citation: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO audit_event (admin_id, entity, entity_id, action, before, after, citation)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(admin_id)
    .bind(entity)
    .bind(entity_id)
    .bind(action)
    .bind(before)
    .bind(after)
    .bind(citation)
    .execute(pool)
    .await?;
    Ok(())
}

/// One audit-log row.
#[derive(Serialize, sqlx::FromRow)]
pub struct AuditRow {
    pub admin_id: Option<Uuid>,
    pub entity: String,
    pub entity_id: String,
    pub action: String,
    pub before: Option<Value>,
    pub after: Option<Value>,
    pub citation: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// The audit log, newest first.
pub async fn list_audit_events(pool: &PgPool, limit: i64) -> Result<Vec<AuditRow>, sqlx::Error> {
    sqlx::query_as::<_, AuditRow>(
        "SELECT admin_id, entity, entity_id, action, before, after, citation, created_at
         FROM audit_event ORDER BY created_at DESC, entity_id LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// A location with its environmental exposures (for the ENV feature + relocation compare).
#[derive(Serialize, sqlx::FromRow, Clone)]
pub struct LocationRow {
    pub name: String,
    pub country: String,
    pub pm25: Option<f64>,
    pub ndvi: Option<f64>,
    pub area_type: Option<String>,
}

/// All locations, ordered by name.
pub async fn list_locations(pool: &PgPool) -> Result<Vec<LocationRow>, sqlx::Error> {
    sqlx::query_as::<_, LocationRow>(
        "SELECT name, country, pm25::float8 AS pm25, ndvi::float8 AS ndvi, area_type
         FROM location ORDER BY name",
    )
    .fetch_all(pool)
    .await
}

/// The account's own data for GDPR export (never includes the password hash).
pub async fn account_export_json(pool: &PgPool, id: Uuid) -> Result<Option<Value>, sqlx::Error> {
    sqlx::query_scalar::<_, Value>(
        "SELECT jsonb_build_object(
                    'id', id, 'email_hash', email_hash, 'locale', locale,
                    'created_at', created_at, 'last_active_at', last_active_at)
         FROM account WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// Permanently delete an account and everything cascading from it (profile, answers, calculations,
/// scenarios). Returns the number of accounts deleted (0 or 1).
pub async fn delete_account(pool: &PgPool, id: Uuid) -> Result<u64, sqlx::Error> {
    let r = sqlx::query("DELETE FROM account WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(r.rows_affected())
}

/// The id of a location by name (+ country), for setting a profile's home location.
pub async fn location_id_by_name(
    pool: &PgPool,
    name: &str,
    country: &str,
) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar("SELECT id FROM location WHERE name = $1 AND country = $2")
        .bind(name)
        .bind(country)
        .fetch_optional(pool)
        .await
}

/// Set (or clear) a profile's home location.
pub async fn set_home_location(
    pool: &PgPool,
    account_id: Uuid,
    location_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE profile SET home_location_id = $1, updated_at = now() WHERE account_id = $2")
        .bind(location_id)
        .bind(account_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// A single location by name (+ country), for resolving a relocation target.
pub async fn location_by_name(
    pool: &PgPool,
    name: &str,
    country: &str,
) -> Result<Option<LocationRow>, sqlx::Error> {
    sqlx::query_as::<_, LocationRow>(
        "SELECT name, country, pm25::float8 AS pm25, ndvi::float8 AS ndvi, area_type
         FROM location WHERE name = $1 AND country = $2",
    )
    .bind(name)
    .bind(country)
    .fetch_optional(pool)
    .await
}

/// A reviewed study/reference (an analysed_papers/ review) backing a factor or rule.
#[derive(Serialize, sqlx::FromRow, Clone)]
pub struct Study {
    pub code: String,
    pub title: String,
    pub authors: Option<String>,
    pub year: Option<i32>,
    pub venue: Option<String>,
    pub doi: Option<String>,
    pub url: Option<String>,
    pub review_slug: Option<String>,
    pub evidence_grade: Option<String>,
}

const STUDY_COLS: &str =
    "code, title, authors, year, venue, doi, url, review_slug, evidence_grade";

/// All studies, ordered by code.
pub async fn list_studies(pool: &PgPool) -> Result<Vec<Study>, sqlx::Error> {
    sqlx::query_as::<_, Study>(&format!("SELECT {STUDY_COLS} FROM study ORDER BY code"))
        .fetch_all(pool)
        .await
}

/// Studies linked to one feature.
pub async fn studies_for_feature(pool: &PgPool, feature_key: &str) -> Result<Vec<Study>, sqlx::Error> {
    sqlx::query_as::<_, Study>(&format!(
        "SELECT {STUDY_COLS} FROM study s
         JOIN feature_study fs ON fs.study_code = s.code
         WHERE fs.feature_key = $1 ORDER BY s.code"
    ))
    .bind(feature_key)
    .fetch_all(pool)
    .await
}

/// Studies linked to one recommendation rule.
pub async fn studies_for_rule(pool: &PgPool, rule_code: &str) -> Result<Vec<Study>, sqlx::Error> {
    sqlx::query_as::<_, Study>(&format!(
        "SELECT {STUDY_COLS} FROM study s
         JOIN rule_study rs ON rs.study_code = s.code
         WHERE rs.rule_code = $1 ORDER BY s.code"
    ))
    .bind(rule_code)
    .fetch_all(pool)
    .await
}

/// A study row tagged with the feature it links to (for enriching a full why[] in one query).
#[derive(sqlx::FromRow)]
pub struct FeatureStudy {
    pub feature_key: String,
    #[sqlx(flatten)]
    pub study: Study,
}

/// Every (feature_key, study) link.
pub async fn all_feature_studies(pool: &PgPool) -> Result<Vec<FeatureStudy>, sqlx::Error> {
    sqlx::query_as::<_, FeatureStudy>(
        "SELECT fs.feature_key, s.code, s.title, s.authors, s.year, s.venue, s.doi, s.url,
                s.review_slug, s.evidence_grade
         FROM feature_study fs JOIN study s ON s.code = fs.study_code
         ORDER BY s.code",
    )
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
