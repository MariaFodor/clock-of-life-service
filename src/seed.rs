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
const STUDIES_JSON: &str = include_str!("../seeds/studies.json");

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
struct StudySeed {
    code: String,
    title: String,
    authors: Option<String>,
    year: Option<i32>,
    venue: Option<String>,
    doi: Option<String>,
    url: Option<String>,
    review_slug: Option<String>,
    evidence_grade: Option<String>,
}

#[derive(Deserialize)]
struct FeatureLink {
    feature_key: String,
    study_code: String,
}

#[derive(Deserialize)]
struct RuleLink {
    rule_code: String,
    study_code: String,
}

#[derive(Deserialize)]
struct StudySeedFile {
    studies: Vec<StudySeed>,
    feature_studies: Vec<FeatureLink>,
    rule_studies: Vec<RuleLink>,
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
    bundle: &crate::bundle::Bundle,
) -> Result<SeedResult, sqlx::Error> {
    seed_features(pool).await?;
    seed_questions(pool).await?;
    seed_recommendation_rules(pool).await?;
    seed_studies(pool).await?;
    seed_locations(pool, bundle).await?;
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
                 (code, feature_key, condition, message, priority, evidence_citation, active, managed)
             VALUES ($1, $2, $3, $4, $5, $6, true, true)
             ON CONFLICT (code) DO UPDATE SET
                 feature_key = EXCLUDED.feature_key, condition = EXCLUDED.condition,
                 message = EXCLUDED.message, priority = EXCLUDED.priority,
                 evidence_citation = EXCLUDED.evidence_citation, active = true, managed = true",
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

    // Reconciliation means matching desired state in BOTH directions. Upserting only meant a rule
    // removed from the seed stayed active for ever — which is how `review_long_sleep` kept being
    // recommended after ONT-01 demoted long sleep to a marker that must never be advised.
    // Only rows the seed OWNS are withdrawn. An admin-authored rule is not ours to retire:
    // deactivating it on restart would be a silent, unaudited deletion of someone else's work.
    // Ownership is a column, not a name prefix — the earlier `Rtest%` carve-out matched only what
    // the test suite happens to call its fixtures, so the suite could not have caught the bug.
    let keep: Vec<String> = rules.iter().map(|r| r.code.clone()).collect();
    // The withdrawal and its audit row commit together: a crash between them would leave exactly
    // the unaudited deactivation this change exists to prevent.
    let mut tx = pool.begin().await?;
    let withdrawn: Vec<String> = sqlx::query_scalar(
        "UPDATE recommendation_rule SET active = false
         WHERE active AND managed AND code <> ALL($1)
         RETURNING code",
    )
    .bind(&keep)
    .fetch_all(&mut *tx)
    .await?;
    for code in &withdrawn {
        sqlx::query(
            "INSERT INTO audit_event (admin_id, entity, entity_id, action, citation)
             VALUES (NULL, 'recommendation_rule', $1, 'deactivate',
                     'startup reconciliation: rule removed from the seed')",
        )
        .bind(code)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    for code in &withdrawn {
        // Every decision the system makes on its own leaves a trace naming what it did and why.
        eprintln!("[reconcile] withdrew seed rule '{code}': no longer in the desired state");
    }
    
    Ok(())
}

/// Seed the study/reference store and rebuild its link tables from the desired state.
async fn seed_studies(pool: &PgPool) -> Result<(), sqlx::Error> {
    let data: StudySeedFile =
        serde_json::from_str(STUDIES_JSON).expect("studies.json seed is valid");
    for s in &data.studies {
        sqlx::query(
            "INSERT INTO study (code, title, authors, year, venue, doi, url, review_slug, evidence_grade)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (code) DO UPDATE SET
                 title = EXCLUDED.title, authors = EXCLUDED.authors, year = EXCLUDED.year,
                 venue = EXCLUDED.venue, doi = EXCLUDED.doi, url = EXCLUDED.url,
                 review_slug = EXCLUDED.review_slug, evidence_grade = EXCLUDED.evidence_grade",
        )
        .bind(&s.code).bind(&s.title).bind(&s.authors).bind(s.year).bind(&s.venue)
        .bind(&s.doi).bind(&s.url).bind(&s.review_slug).bind(&s.evidence_grade)
        .execute(pool)
        .await?;
    }
    // Link tables are fully derived from the seed — rebuild them so removals are reflected.
    sqlx::query("DELETE FROM feature_study").execute(pool).await?;
    sqlx::query("DELETE FROM rule_study").execute(pool).await?;
    for l in &data.feature_studies {
        sqlx::query(
            "INSERT INTO feature_study (feature_key, study_code) VALUES ($1, $2)
             ON CONFLICT DO NOTHING",
        )
        .bind(&l.feature_key).bind(&l.study_code)
        .execute(pool)
        .await?;
    }
    for l in &data.rule_studies {
        sqlx::query(
            "INSERT INTO rule_study (rule_code, study_code) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        )
        .bind(&l.rule_code).bind(&l.study_code)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Seed the location table (PM2.5 / greenspace per place) for the ENV feature + relocation compare.
/// Values are illustrative placeholders (RES-04): real RO layers must replace them before launch.
/// The measured settlements from the bundle, replacing the seven invented Romanian rows.
///
/// Batched with UNNEST rather than looped: 3,522 rows at one round-trip each was the shape that made
/// the old seven-row seed look cheap, and a cold boot would spend minutes in the driver.
///
/// Refuses an empty `places` list rather than succeeding. A pre-v4.1.0 bundle has no settlements, and
/// "seeded nothing" would leave whatever the database already held — which, in any database that has
/// ever booted, is the invented rows this exists to delete.
async fn seed_locations(pool: &PgPool, bundle: &crate::bundle::Bundle) -> Result<(), sqlx::Error> {
    if bundle.places.is_empty() {
        // Not a silent skip: the seeder's whole job here is to make the fakes unreachable.
        return Err(sqlx::Error::Protocol(
            "bundle carries no places.json — refusing to seed locations, because leaving the \
             previous (illustrative) rows in place is worse than failing to boot".into(),
        ));
    }

    // places.json keys countries by ISO3; `location.country` has always held ISO2, and stored profiles
    // and deployed clients use it. The map comes from the bundle's own baselines rather than a table
    // here, so there is one source for the pairing.
    let iso3_to_iso2: std::collections::HashMap<&str, &str> = bundle
        .reference
        .iter()
        .filter_map(|(iso2, b)| b.iso3.as_deref().map(|iso3| (iso3, iso2.as_str())))
        .collect();

    let src = "WHO Ambient Air Quality Database v8.0; greenness: Stowell et al. 2023 (CC0)";
    let mut names: Vec<String> = Vec::new();
    let mut countries: Vec<String> = Vec::new();
    let mut iso3s: Vec<String> = Vec::new();
    let mut pm25: Vec<f64> = Vec::new();
    let mut ndvi: Vec<Option<f64>> = Vec::new();
    let mut areas: Vec<String> = Vec::new();
    let mut as_ofs: Vec<chrono::NaiveDate> = Vec::new();
    let mut lats: Vec<f64> = Vec::new();
    let mut lons: Vec<f64> = Vec::new();
    let mut pops: Vec<Option<i64>> = Vec::new();
    let mut pm_years: Vec<i32> = Vec::new();
    let mut stations: Vec<Option<i32>> = Vec::new();
    let mut nd_years: Vec<Option<i32>> = Vec::new();
    let mut bases: Vec<Option<String>> = Vec::new();
    let mut skipped = 0usize;

    for p in &bundle.places {
        let Some(iso2) = iso3_to_iso2.get(p.iso3.as_str()) else {
            // A settlement in a country this bundle has no life table for. The model gate refuses to
            // emit one, so this should be unreachable; counted rather than ignored so a future
            // mismatch is visible in the log instead of being a quietly shorter list.
            skipped += 1;
            continue;
        };
        // `as_of` is the year the reading was taken, not the day it was loaded. The column is a DATE,
        // so it becomes 1 January of the measurement year — a date that is honest about its precision
        // in the only way the column allows.
        let Some(as_of) = chrono::NaiveDate::from_ymd_opt(p.pm25_year, 1, 1) else {
            skipped += 1;
            continue;
        };
        names.push(p.city.clone());
        countries.push((*iso2).to_string());
        iso3s.push(p.iso3.clone());
        pm25.push(p.pm25);
        ndvi.push(p.ndvi);
        // WHO measures settlements, not administrative areas, and does not classify them. 'city' here
        // is the shape of the measurement, not a claim about population size — the country-level
        // urban/rural/city/town splits live in the baseline's env_reference, where WHO does classify.
        areas.push("city".to_string());
        as_ofs.push(as_of);
        lats.push(p.lat);
        lons.push(p.lon);
        pops.push(p.population);
        pm_years.push(p.pm25_year);
        stations.push(p.pm25_stations);
        nd_years.push(p.ndvi_year);
        bases.push(p.ndvi_basis.clone());
    }
    if skipped > 0 {
        eprintln!("[seed] {skipped} settlement(s) skipped: no ISO2 for their country in this bundle");
    }

    sqlx::query(
        "INSERT INTO location
             (name, country, iso3, pm25, ndvi, area_type, as_of, lat, lon, population,
              pm25_year, pm25_stations, ndvi_year, ndvi_basis, source)
         SELECT * FROM UNNEST(
             $1::text[], $2::text[], $3::text[], $4::numeric[], $5::numeric[], $6::text[],
             $7::date[], $8::float8[], $9::float8[], $10::bigint[], $11::int[], $12::int[],
             $13::int[], $14::text[]
         ) AS t(name, country, iso3, pm25, ndvi, area_type, as_of, lat, lon, population,
                pm25_year, pm25_stations, ndvi_year, ndvi_basis), (SELECT $15::text) AS s(source)
         ON CONFLICT (name, country) DO UPDATE SET
             iso3 = EXCLUDED.iso3, pm25 = EXCLUDED.pm25, ndvi = EXCLUDED.ndvi,
             area_type = EXCLUDED.area_type, as_of = EXCLUDED.as_of,
             lat = EXCLUDED.lat, lon = EXCLUDED.lon, population = EXCLUDED.population,
             pm25_year = EXCLUDED.pm25_year, pm25_stations = EXCLUDED.pm25_stations,
             ndvi_year = EXCLUDED.ndvi_year, ndvi_basis = EXCLUDED.ndvi_basis,
             source = EXCLUDED.source",
    )
    .bind(&names)
    .bind(&countries)
    .bind(&iso3s)
    .bind(&pm25)
    .bind(&ndvi)
    .bind(&areas)
    .bind(&as_ofs)
    .bind(&lats)
    .bind(&lons)
    .bind(&pops)
    .bind(&pm_years)
    .bind(&stations)
    .bind(&nd_years)
    .bind(&bases)
    .bind(src)
    .execute(pool)
    .await?;
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
