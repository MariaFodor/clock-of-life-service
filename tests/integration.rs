//! Integration tests (SVC-DBT + SVC-DB5T) — exercise migrations, reconciliation, the persist/read
//! paths, and authentication + per-user isolation against a live local PostgreSQL. Point at a
//! throwaway database via `TEST_DATABASE_URL` (default
//! `postgresql:///clock_of_life_test?host=/var/run/postgresql`).
//!
//! One shared state is built once (migrate + reconcile + truncate volatile tables). Each test that
//! needs an owner registers its own unique account, so assertions are isolated and parallel-safe.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use clock_of_life_service::{build_router, init_state, AppState};
use serde_json::{json, Value};
use tokio::sync::OnceCell;
use tower::ServiceExt; // for `oneshot`

fn test_db_url() -> String {
    std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgresql:///clock_of_life_test?host=/var/run/postgresql".to_string())
}

/// One long-lived multi-thread runtime shared by every test. A `PgPool` spawns background tasks on the
/// runtime that created it, so each `#[tokio::test]` (its own short-lived runtime) would kill the shared
/// pool when it finished. Driving all tests through this single runtime keeps the pool alive throughout.
static RT: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build shared test runtime")
});

static STATE: OnceCell<Arc<AppState>> = OnceCell::const_new();
static COUNTER: AtomicU64 = AtomicU64::new(0);

async fn state() -> Arc<AppState> {
    STATE
        .get_or_init(|| async {
            let s = init_state("bundle/model-v2.0.0", &test_db_url())
                .await
                .expect("init_state (is PostgreSQL running and clock_of_life_test present?)");
            sqlx::query("TRUNCATE scenario, calculation, answer RESTART IDENTITY CASCADE")
                .execute(&s.pool)
                .await
                .expect("truncate volatile tables");
            s
        })
        .await
        .clone()
}

fn unique_email() -> String {
    format!("u{}-{}@example.com", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn post(uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn post_auth(uri: &str, body: Value, token: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().method("GET").uri(uri).body(Body::empty()).unwrap()
}

fn get_auth(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

fn patch_auth(uri: &str, body: Value, token: &str) -> Request<Body> {
    Request::builder()
        .method("PATCH")
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn delete_auth(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Drive one request through a fresh router. Centralizing the single `oneshot` monomorphization here
/// (instead of ~90 inline call-sites) keeps the test binary small and fast to compile.
async fn call(s: &Arc<AppState>, req: Request<Body>) -> axum::response::Response {
    build_router(s.clone()).oneshot(req).await.unwrap()
}

/// Register a fresh unique account and return its bearer token.
async fn register_token(s: &Arc<AppState>) -> String {
    let resp = call(s, post("/api/auth/register", json!({"email": unique_email(), "password": "password123"}))).await;
    assert_eq!(resp.status(), StatusCode::OK, "register should succeed");
    body_json(resp).await["token"].as_str().unwrap().to_string()
}

/// Register a fresh account, promote it to admin (via SQL), and return its bearer token.
async fn register_admin(s: &Arc<AppState>) -> String {
    let resp = call(s, post("/api/auth/register", json!({"email": unique_email(), "password": "password123"}))).await;
    let body = body_json(resp).await;
    let token = body["token"].as_str().unwrap().to_string();
    let account_id = body["account_id"].as_str().unwrap().to_string();
    sqlx::query("UPDATE account SET is_admin = true WHERE id = $1::uuid")
        .bind(&account_id)
        .execute(&s.pool)
        .await
        .unwrap();
    token
}

fn valid_profile() -> Value {
    json!({"country": "RO", "age": 40, "sex": "M", "smoke": 0, "pa_min": 300, "sleep": 7, "waist": 90})
}

/// DB2: reference tables are reconciled to the desired state on startup.
#[test]
fn seeds_are_reconciled() {
    RT.block_on(async {
    let s = state().await;
    let features: i64 = sqlx::query_scalar("SELECT count(*) FROM feature WHERE active")
        .fetch_one(&s.pool).await.unwrap();
    assert_eq!(features, 18, "18 features seeded");

    // Exclude questions created by the admin-mutation test (shared DB, parallel).
    let questions: i64 = sqlx::query_scalar("SELECT count(*) FROM question WHERE active AND code NOT LIKE 'Qtest%'")
        .fetch_one(&s.pool).await.unwrap();
    assert_eq!(questions, 24, "24 questions seeded");

    let active_models: i64 = sqlx::query_scalar("SELECT count(*) FROM model_version WHERE is_active")
        .fetch_one(&s.pool).await.unwrap();
    assert_eq!(active_models, 1, "exactly one active model version");

    let dangling: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM question q
         WHERE q.feature_key IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM feature f WHERE f.key = q.feature_key)",
    ).fetch_one(&s.pool).await.unwrap();
    assert_eq!(dangling, 0, "no question points at a missing feature");

    // Recommendation rules (API-02): all seeded, evidence-cited, referencing real features.
    let rules: i64 = sqlx::query_scalar("SELECT count(*) FROM recommendation_rule WHERE active AND code NOT LIKE 'Rtest%'")
        .fetch_one(&s.pool).await.unwrap();
    assert_eq!(rules, 7, "7 recommendation rules seeded");
    let uncited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM recommendation_rule WHERE evidence_citation IS NULL OR evidence_citation = ''",
    ).fetch_one(&s.pool).await.unwrap();
    assert_eq!(uncited, 0, "every rule carries an evidence citation");
    let rule_dangling: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM recommendation_rule r
         WHERE NOT EXISTS (SELECT 1 FROM feature f WHERE f.key = r.feature_key)",
    ).fetch_one(&s.pool).await.unwrap();
    assert_eq!(rule_dangling, 0, "no rule points at a missing feature");
    });
}

/// DB5b: register -> login round-trip, with the credential edge cases.
#[test]
fn auth_register_login_roundtrip() {
    RT.block_on(async {
    let s = state().await;
    let email = unique_email();
    let creds = json!({"email": email, "password": "password123"});

    let resp = call(&s, post("/api/auth/register", creds.clone())).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_json(resp).await["token"].as_str().is_some());

    // Duplicate registration -> 409.
    let resp = call(&s, post("/api/auth/register", creds.clone())).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // Too-short password -> 400.
    let resp = call(&s, post("/api/auth/register", json!({"email": unique_email(), "password": "short"}))).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Correct login -> token.
    let resp = call(&s, post("/api/auth/login", creds)).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Wrong password -> 401.
    let resp = call(&s, post("/api/auth/login", json!({"email": email, "password": "wrongpass1"}))).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Unknown user -> 401 (same as wrong password).
    let resp = call(&s, post("/api/auth/login", json!({"email": unique_email(), "password": "whatever1"}))).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    });
}

/// DB5d: personal routes reject unauthenticated callers.
#[test]
fn protected_routes_require_auth() {
    RT.block_on(async {
    let s = state().await;
    assert_eq!(call(&s, get("/api/calculations")).await.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(call(&s, get("/api/answers")).await.status(), StatusCode::UNAUTHORIZED);
    let bad_token = call(&s, get_auth("/api/calculations", "not.a.jwt")).await;
    assert_eq!(bad_token.status(), StatusCode::UNAUTHORIZED, "garbage token rejected");
    });
}

/// DB3 + DB4: an authenticated estimate persists a calculation the caller's history returns.
#[test]
fn estimate_persists_and_history_reads_back() {
    RT.block_on(async {
    let s = state().await;
    let token = register_token(&s).await;
    let resp = call(&s, post_auth("/api/estimate", valid_profile(), &token)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let est = body_json(resp).await;
    let calc_id = est["calculation_id"].as_str().expect("calculation_id returned").to_string();

    let resp = call(&s, get_auth("/api/calculations", &token)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let history = body_json(resp).await;
    let rows = history.as_array().expect("history is an array");
    let found = rows.iter().find(|r| r["id"] == calc_id).expect("new calculation is in history");
    assert_eq!(found["estimate_years"], est["estimate_years"], "persisted years round-trip");
    assert_eq!(found["input_hash"].as_str().unwrap().len(), 16, "input hash stored");
    });
}

/// API-05: /api/estimate carries a why[] breakdown + model provenance; context factors are never
/// turned into recommendations.
#[test]
fn estimate_why_and_context_not_recommended() {
    RT.block_on(async {
    let s = state().await;
    let high = json!({"country": "RO", "age": 55, "sex": "M", "smoke": 2, "pa_min": 0, "sleep": 7,
                      "waist": 115, "diabetes": true});
    let resp = call(&s, post("/api/estimate", high)).await;
    let est = body_json(resp).await;

    // model provenance block.
    assert_eq!(est["model"]["version"], "2.0.0");
    assert!(est["model"]["algorithm"].as_str().is_some());
    // why[] present, populated, each entry well-formed and sensibly signed.
    let why = est["why"].as_array().expect("why[] present");
    assert!(!why.is_empty(), "high-risk estimate has explanatory factors");
    let smoking = why.iter().find(|w| w["key"] == "smk_current").expect("smoking in why[]");
    assert!(smoking["delta_years"].as_f64().unwrap() < 0.0, "smoking costs years");
    assert_eq!(smoking["evidence"], "strong");
    assert_eq!(smoking["role"], "lever");
    for w in why {
        assert!(w["factor"].as_str().is_some() && w["citation"].as_str().is_some());
    }

    // A profile whose only deviation is a CONTEXT factor (cardiovascular history) gets no recommendation.
    let context_only = json!({"country": "RO", "age": 40, "sex": "F", "smoke": 0, "pa_min": 2000,
                              "sleep": 7, "waist": 78, "cvd_hx": true});
    let resp = call(&s, post("/api/recommendations", context_only)).await;
    let recs = body_json(resp).await;
    assert_eq!(recs.as_array().unwrap().len(), 0, "context factors are explained, never recommended");
    });
}

/// Anonymous (unauthenticated) estimate still works — try-before-signup.
#[test]
fn anonymous_estimate_still_works() {
    RT.block_on(async {
    let s = state().await;
    let resp = call(&s, post("/api/estimate", valid_profile())).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_json(resp).await["calculation_id"].as_str().is_some());
    });
}

/// DB3 + DB5d: a What-If persists a scenario when the caller owns the base calculation.
#[test]
fn whatif_persists_scenario() {
    RT.block_on(async {
    let s = state().await;
    let token = register_token(&s).await;
    let resp = call(&s, post_auth("/api/estimate", valid_profile(), &token)).await;
    let base_id = body_json(resp).await["calculation_id"].as_str().unwrap().to_string();

    let req = json!({"base": valid_profile(), "changes": {"pa_min": 2000}, "base_calculation_id": base_id});
    let resp = call(&s, post_auth("/api/whatif", req, &token)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let wi = body_json(resp).await;
    let scenario_id = wi["scenario_id"].as_str().expect("scenario persisted").to_string();
    assert!(wi["delta_years"].as_f64().unwrap() > 0.0, "more activity adds years");

    let linked: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM scenario WHERE id = $1::uuid AND base_calculation_id = $2::uuid",
    ).bind(&scenario_id).bind(&base_id).fetch_one(&s.pool).await.unwrap();
    assert_eq!(linked, 1, "scenario is linked to its base calculation");
    });
}

/// DB5d: forking someone else's calculation is forbidden; unauthenticated is unauthorized.
#[test]
fn whatif_scenario_requires_ownership() {
    RT.block_on(async {
    let s = state().await;
    let token_a = register_token(&s).await;
    let token_b = register_token(&s).await;
    let resp = call(&s, post_auth("/api/estimate", valid_profile(), &token_a)).await;
    let base_id = body_json(resp).await["calculation_id"].as_str().unwrap().to_string();

    let req = json!({"base": valid_profile(), "changes": {"pa_min": 2000}, "base_calculation_id": base_id});
    // B cannot fork A's calculation.
    let resp = call(&s, post_auth("/api/whatif", req.clone(), &token_b)).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    // Unauthenticated cannot persist a scenario.
    let resp = call(&s, post("/api/whatif", req)).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    });
}

/// API-03: recommendations are ranked by score, scoped to levers/manage, and empty for a healthy user.
#[test]
fn recommendations_rank_and_scope() {
    RT.block_on(async {
    let s = state().await;
    let high = json!({"country": "RO", "age": 55, "sex": "M", "smoke": 2, "pa_min": 0, "sleep": 9,
                      "waist": 115, "diabetes": true, "high_bp": true});
    let resp = call(&s, post("/api/recommendations", high)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let recs = body_json(resp).await;
    let arr = recs.as_array().expect("array");
    assert!(!arr.is_empty(), "high-risk profile gets recommendations");
    // Top recommendation is quitting smoking (highest impact × priority).
    assert_eq!(arr[0]["feature"], "smk_current");
    // Sorted by descending score; every recommendation is a lever or manage factor (never context).
    for pair in arr.windows(2) {
        assert!(pair[0]["score"].as_f64().unwrap() >= pair[1]["score"].as_f64().unwrap(), "sorted by score");
    }
    for r in arr {
        let role = r["role"].as_str().unwrap();
        assert!(role == "lever" || role == "manage", "only levers/manage recommended, got {role}");
        assert!(!r["evidence_citation"].as_str().unwrap().is_empty(), "recommendation is evidence-cited");
    }

    // A healthy profile triggers no rules.
    let healthy = json!({"country": "RO", "age": 40, "sex": "F", "smoke": 0, "pa_min": 2000,
                         "sleep": 7, "waist": 78});
    let resp = call(&s, post("/api/recommendations", healthy)).await;
    let recs = body_json(resp).await;
    assert_eq!(recs.as_array().unwrap().len(), 0, "healthy profile gets no recommendations");
    });
}

/// DB3: What-If without a base is a pure overlay — nothing persisted, no auth required.
#[test]
fn whatif_overlay_only_when_no_base() {
    RT.block_on(async {
    let s = state().await;
    let req = json!({"base": valid_profile(), "changes": {"smoke": 0}});
    let resp = call(&s, post("/api/whatif", req)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let wi = body_json(resp).await;
    assert!(wi.get("scenario_id").is_none(), "no scenario id without a base calculation");
    });
}

/// DB4: answers upsert (one current answer per question) and read back, for the authenticated caller.
#[test]
fn answers_upsert_and_readback() {
    RT.block_on(async {
    let s = state().await;
    let token = register_token(&s).await;
    let first = json!({"answers": [{"question_code": "Q5_smoking", "value": "Yes, currently"}]});
    assert_eq!(call(&s, post_auth("/api/answers", first, &token)).await.status(), StatusCode::OK);

    // Re-answering the same question updates in place (no duplicate row).
    let second = json!({"answers": [{"question_code": "Q5_smoking", "value": "No, never"}]});
    assert_eq!(call(&s, post_auth("/api/answers", second, &token)).await.status(), StatusCode::OK);

    let resp = call(&s, get_auth("/api/answers", &token)).await;
    let answers = body_json(resp).await;
    let q5: Vec<&Value> = answers.as_array().unwrap().iter()
        .filter(|a| a["question_code"] == "Q5_smoking").collect();
    assert_eq!(q5.len(), 1, "exactly one current answer for Q5 (upsert, not insert)");
    assert_eq!(q5[0]["value"], "No, never", "latest value wins");
    });
}

/// API-04: the interview definition is public and numerically ordered; profile is auth-scoped.
#[test]
fn questions_and_profile() {
    RT.block_on(async {
    let s = state().await;
    // Questions: public (no auth), all 24, ordered Q1..Q24 by numeric code.
    let resp = call(&s, get("/api/questions")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let qs = body_json(resp).await;
    // Exclude any questions created by the admin-mutation test (shared DB, parallel).
    let arr: Vec<&Value> = qs.as_array().unwrap().iter()
        .filter(|q| !q["code"].as_str().unwrap().starts_with("Qtest")).collect();
    assert_eq!(arr.len(), 24, "24 questions served");
    assert_eq!(arr[0]["code"], "Q1_age");
    assert_eq!(arr[9]["code"], "Q10_sedentary", "numeric order (Q10 after Q9, not after Q1)");

    // Profile: requires auth, returns the caller's saved answers.
    assert_eq!(call(&s, get("/api/profile")).await.status(), StatusCode::UNAUTHORIZED);
    let token = register_token(&s).await;
    call(&s, post_auth("/api/answers", json!({"answers": [{"question_code": "Q11_sleep", "value": "7-8"}]}), &token)).await;
    let resp = call(&s, get_auth("/api/profile", &token)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let profile = body_json(resp).await;
    assert!(profile["profile_id"].as_str().is_some());
    let answers = profile["answers"].as_array().unwrap();
    assert!(answers.iter().any(|a| a["question_code"] == "Q11_sleep" && a["value"] == "7-8"));
    });
}

/// API-10: evidence-traceability invariants — every factor/rule resolves to an openable study.
#[test]
fn every_factor_and_rule_resolves_to_a_study() {
    RT.block_on(async {
    let s = state().await;
    let feature_without_study: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM feature f WHERE f.active
           AND NOT EXISTS (SELECT 1 FROM feature_study fs WHERE fs.feature_key = f.key)",
    ).fetch_one(&s.pool).await.unwrap();
    assert_eq!(feature_without_study, 0, "every kept factor is backed by a study");

    let rule_without_study: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM recommendation_rule r WHERE r.active AND r.code NOT LIKE 'Rtest%'
           AND NOT EXISTS (SELECT 1 FROM rule_study rs WHERE rs.rule_code = r.code)",
    ).fetch_one(&s.pool).await.unwrap();
    assert_eq!(rule_without_study, 0, "every rule is backed by a study");

    // Every study is openable (a DOI or an internal review), so a citation is never a dead end.
    let unopenable: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM study
         WHERE (doi IS NULL OR doi = '') AND (review_slug IS NULL OR review_slug = '')",
    ).fetch_one(&s.pool).await.unwrap();
    assert_eq!(unopenable, 0, "every study has an openable link");

    // Link integrity: no link references a missing study.
    let dangling: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM feature_study fs
           WHERE NOT EXISTS (SELECT 1 FROM study s WHERE s.code = fs.study_code)",
    ).fetch_one(&s.pool).await.unwrap();
    assert_eq!(dangling, 0, "no feature_study link points at a missing study");
    });
}

/// API-09: why[] factors and recommendations carry openable study references inline.
#[test]
fn references_wired_into_why_and_recommendations() {
    RT.block_on(async {
    let s = state().await;
    let high = json!({"country": "RO", "age": 55, "sex": "M", "smoke": 2, "pa_min": 0, "sleep": 7,
                      "waist": 115, "diabetes": true});

    let est = body_json(call(&s, post("/api/estimate", high.clone())).await).await;
    let why = est["why"].as_array().unwrap();
    let smoking = why.iter().find(|w| w["key"] == "smk_current").expect("smoking factor");
    let refs = smoking["references"].as_array().expect("references array");
    assert!(!refs.is_empty(), "smoking factor carries a reference");
    // Reference is openable: a DOI or an internal review slug.
    assert!(refs[0]["doi"].as_str().is_some() || refs[0]["review_slug"].as_str().is_some());

    let recs = body_json(call(&s, post("/api/recommendations", high)).await).await;
    for r in recs.as_array().unwrap() {
        assert!(!r["references"].as_array().unwrap().is_empty(),
            "recommendation {} cites at least one study", r["feature"]);
    }
    });
}

/// API-15: ENV is a lever only inside relocate — it is never recommended and never a why[] factor,
/// even for a user living in a polluted location.
#[test]
fn env_is_lever_only_in_relocate() {
    RT.block_on(async {
    let s = state().await;
    // A profile in dirty air (Bucharest-like), otherwise healthy so no other rule fires.
    let dirty = json!({"country": "RO", "age": 45, "sex": "M", "smoke": 0, "pa_min": 2000, "sleep": 7,
                       "waist": 80, "pm25": 19.0, "ndvi": 0.35});

    // Recommendations never mention env (context, not a lever) — healthy lifestyle → none at all.
    let recs = body_json(call(&s, post("/api/recommendations", dirty.clone())).await).await;
    assert!(recs.as_array().unwrap().iter().all(|r| r["feature"] != "env"), "env is never recommended");

    // why[] carries no env factor (env is a context term outside the attribution set).
    let est = body_json(call(&s, post("/api/estimate", dirty.clone())).await).await;
    assert!(est["why"].as_array().unwrap().iter().all(|w| w["key"] != "env"), "env is not a why[] factor");

    // But relocation DOES act on it.
    let rel = body_json(call(&s, post("/api/relocate", json!({"base": dirty, "to": "Rural (national)"}))).await).await;
    assert!(rel["delta_years"].as_f64().unwrap() > 0.0, "moving to cleaner air adds years (env as lever)");
    });
}

/// API-20: the audit log captures before/after snapshots + citation for every admin mutation.
#[test]
fn audit_captures_before_after_and_citation() {
    RT.block_on(async {
    let s = state().await;
    let admin = register_admin(&s).await;
    let code = format!("Qtest_{}_{}", std::process::id(), COUNTER_CODE.fetch_add(1, std::sync::atomic::Ordering::Relaxed));

    // Create then update a question.
    let create = json!({"code": code, "section": "Test", "text": "Original text?", "input_type": "number", "citation": "ops: create"});
    assert_eq!(call(&s, post_auth("/api/admin/questions", create, &admin)).await.status(), StatusCode::OK);
    assert_eq!(call(&s, post_auth_put(&format!("/api/admin/questions/{code}"), json!({"text": "Edited text?", "citation": "ops: edit"}), &admin)).await.status(), StatusCode::OK);

    let audit = body_json(call(&s, get_auth("/api/admin/audit", &admin)).await).await;
    let entries = audit.as_array().unwrap();

    // Create entry: before is null, after is the row, citation present.
    let created = entries.iter().find(|e| e["entity"] == "question" && e["entity_id"] == code && e["action"] == "create").expect("create audited");
    assert!(created["before"].is_null(), "create has no before snapshot");
    assert!(created["after"].is_object(), "create captures the new row");
    assert!(!created["citation"].as_str().unwrap().is_empty());

    // Update entry: before + after both captured, and the change is visible.
    let updated = entries.iter().find(|e| e["entity"] == "question" && e["entity_id"] == code && e["action"] == "update").expect("update audited");
    assert_eq!(updated["before"]["text"], "Original text?", "before snapshot captured");
    assert_eq!(updated["after"]["text"], "Edited text?", "after snapshot captured");
    assert!(!updated["citation"].as_str().unwrap().is_empty());

    sqlx::query("DELETE FROM question WHERE code = $1").bind(&code).execute(&s.pool).await.unwrap();
    });
}

/// API-19: admin can pin a model version; one-active enforced; audited; 403/404 guarded.
#[test]
fn admin_model_pin() {
    RT.block_on(async {
    let s = state().await;
    let admin = register_admin(&s).await;
    let user = register_token(&s).await;

    // Non-admin → 403; unknown semver → 404.
    assert_eq!(call(&s, post_auth("/api/admin/model/pin", json!({"semver": "2.0.0", "citation": "c"}), &user)).await.status(), StatusCode::FORBIDDEN);
    assert_eq!(call(&s, post_auth("/api/admin/model/pin", json!({"semver": "9.9.9", "citation": "c"}), &admin)).await.status(), StatusCode::NOT_FOUND);

    // Insert a second (inactive) version, pin it, and confirm exactly one active — the new one.
    sqlx::query("INSERT INTO model_version (semver, artifact_uri, algorithm, is_active) VALUES ('2.0.0-test','test','cox_ph',false) ON CONFLICT (semver) DO NOTHING")
        .execute(&s.pool).await.unwrap();
    let resp = call(&s, post_auth("/api/admin/model/pin", json!({"semver": "2.0.0-test", "citation": "ops: activate test model"}), &admin)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let active: Vec<String> = sqlx::query_scalar("SELECT semver FROM model_version WHERE is_active").fetch_all(&s.pool).await.unwrap();
    assert_eq!(active, vec!["2.0.0-test".to_string()], "exactly one active, the newly pinned one");

    let audit = body_json(call(&s, get_auth("/api/admin/audit", &admin)).await).await;
    assert!(audit.as_array().unwrap().iter().any(|e| e["entity"] == "model_version" && e["action"] == "pin_model"));

    // Restore the real active model and remove the test row (shared DB).
    call(&s, post_auth("/api/admin/model/pin", json!({"semver": "2.0.0", "citation": "ops: restore"}), &admin)).await;
    sqlx::query("DELETE FROM model_version WHERE semver = '2.0.0-test'").execute(&s.pool).await.unwrap();
    });
}

/// API-18: admin can edit features and create/update rules; invalid values → 400; all audited.
#[test]
fn admin_feature_and_rule_mutations() {
    RT.block_on(async {
    let s = state().await;
    let admin = register_admin(&s).await;
    let user = register_token(&s).await;

    // Non-admin cannot edit a feature.
    assert_eq!(
        call(&s, post_auth_put("/api/admin/features/income", json!({"name": "x", "citation": "c"}), &user)).await.status(),
        StatusCode::FORBIDDEN);

    // Edit a feature's citation (harmless to other tests).
    let resp = call(&s, post_auth_put("/api/admin/features/income", json!({"feature_citation": "SES gradient (reviewed)", "citation": "ops: refine citation"}), &admin)).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Invalid role → 400 (CHECK violation mapped).
    assert_eq!(
        call(&s, post_auth_put("/api/admin/features/income", json!({"role": "bogus", "citation": "c"}), &admin)).await.status(),
        StatusCode::BAD_REQUEST);

    // Create a rule (unique code, never-matching condition), then update it. Unknown feature_key → 400.
    let code = format!("Rtest_{}_{}", std::process::id(), COUNTER_CODE.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    let create = json!({"code": code, "feature_key": "activity", "condition": {"field": "smoke", "op": "eq", "value": 99},
                        "message": "test rule", "priority": 1, "evidence_citation": "test", "citation": "ops: new rule"});
    assert_eq!(call(&s, post_auth("/api/admin/rules", create, &admin)).await.status(), StatusCode::OK);

    let bad_fk = json!({"code": format!("{code}_x"), "feature_key": "nope", "condition": {}, "message": "m", "evidence_citation": "e", "citation": "c"});
    assert_eq!(call(&s, post_auth("/api/admin/rules", bad_fk, &admin)).await.status(), StatusCode::BAD_REQUEST);

    let upd = call(&s, post_auth_put(&format!("/api/admin/rules/{code}"), json!({"priority": 5, "citation": "ops: bump priority"}), &admin)).await;
    assert_eq!(upd.status(), StatusCode::OK);
    assert_eq!(body_json(upd).await["priority"].as_i64().unwrap(), 5);

    // Audit recorded feature + rule mutations.
    let audit = body_json(call(&s, get_auth("/api/admin/audit", &admin)).await).await;
    let e = audit.as_array().unwrap();
    assert!(e.iter().any(|x| x["entity"] == "feature" && x["action"] == "update"));
    assert!(e.iter().any(|x| x["entity"] == "recommendation_rule" && x["action"] == "create"));

    // Clean up the created rule (shared DB).
    sqlx::query("DELETE FROM recommendation_rule WHERE code = $1").bind(&code).execute(&s.pool).await.unwrap();
    });
}

/// API-21: GDPR export returns all the caller's data and never the password hash.
#[test]
fn account_export() {
    RT.block_on(async {
    let s = state().await;
    let token = register_token(&s).await;
    call(&s, post_auth("/api/estimate", valid_profile(), &token)).await;
    call(&s, post_auth("/api/answers", json!({"answers": [{"question_code": "Q5_smoking", "value": "No, never"}]}), &token)).await;

    assert_eq!(call(&s, get("/api/account/export")).await.status(), StatusCode::UNAUTHORIZED);
    let exp = body_json(call(&s, get_auth("/api/account/export", &token)).await).await;
    assert!(exp["account"]["password_hash"].is_null(), "export never includes the password hash");
    assert!(exp["account"]["id"].as_str().is_some());
    assert!(exp["answers"].as_array().unwrap().len() >= 1);
    assert!(exp["calculations"].as_array().unwrap().len() >= 1);
    });
}

/// API-22: GDPR erasure deletes the caller's account + cascades; other accounts are untouched.
#[test]
fn account_erasure_cascades() {
    RT.block_on(async {
    let s = state().await;
    let a = register_token(&s).await;
    let b = register_token(&s).await;
    call(&s, post_auth("/api/estimate", valid_profile(), &a)).await;
    call(&s, post_auth("/api/estimate", valid_profile(), &b)).await;
    let a_id = body_json(call(&s, get_auth("/api/account/export", &a)).await).await["account"]["id"].as_str().unwrap().to_string();

    // Delete A.
    assert_eq!(call(&s, delete_auth("/api/account", &a)).await.status(), StatusCode::OK);

    // A's account + calculations are gone (cascade); A's export now 404.
    let acct: i64 = sqlx::query_scalar("SELECT count(*) FROM account WHERE id = $1::uuid").bind(&a_id).fetch_one(&s.pool).await.unwrap();
    assert_eq!(acct, 0, "account deleted");
    let calcs: i64 = sqlx::query_scalar("SELECT count(*) FROM calculation WHERE account_id = $1::uuid").bind(&a_id).fetch_one(&s.pool).await.unwrap();
    assert_eq!(calcs, 0, "calculations cascade-deleted");
    assert_eq!(call(&s, get_auth("/api/account/export", &a)).await.status(), StatusCode::NOT_FOUND);

    // B is untouched.
    let b_calcs = body_json(call(&s, get_auth("/api/calculations", &b)).await).await;
    assert!(b_calcs.as_array().unwrap().len() >= 1, "other account unaffected");
    });
}

/// API-23: right to rectification — answers, and account locale, can be corrected.
#[test]
fn rectification() {
    RT.block_on(async {
    let s = state().await;
    let token = register_token(&s).await;

    // Correct an answer (upsert) — already covered elsewhere, asserted here as part of the surface.
    call(&s, post_auth("/api/answers", json!({"answers": [{"question_code": "Q5_smoking", "value": "Yes, currently"}]}), &token)).await;
    call(&s, post_auth("/api/answers", json!({"answers": [{"question_code": "Q5_smoking", "value": "No, never"}]}), &token)).await;
    let p = body_json(call(&s, get_auth("/api/profile", &token)).await).await;
    let q5 = p["answers"].as_array().unwrap().iter().find(|a| a["question_code"] == "Q5_smoking").unwrap();
    assert_eq!(q5["value"], "No, never", "answer corrected");

    // Correct the account locale.
    assert_eq!(call(&s, patch_auth("/api/account", json!({"locale": ""}), &token)).await.status(), StatusCode::BAD_REQUEST);
    let resp = call(&s, patch_auth("/api/account", json!({"locale": "en"}), &token)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let exp = body_json(call(&s, get_auth("/api/account/export", &token)).await).await;
    assert_eq!(exp["account"]["locale"], "en", "locale corrected");

    // No auth → 401.
    assert_eq!(call(&s, patch_auth("/api/account", json!({"locale": "ro"}), "")).await.status(), StatusCode::UNAUTHORIZED);
    });
}

/// API-25: aggregates expose no individual data — only counts/means/percentiles, never ids or raw rows.
#[test]
fn aggregates_expose_no_individual_data() {
    RT.block_on(async {
    let s = state().await;
    let agg = body_json(call(&s, get("/api/aggregates")).await).await;
    // No account ids or raw input payloads anywhere in the serialized response.
    let text = agg.to_string();
    assert!(!text.contains("account_id"), "no account ids in aggregates");
    assert!(!text.contains("input_hash"), "no per-calculation identifiers in aggregates");
    // Each per-country row exposes only the safe aggregate keys.
    for c in agg["by_country"].as_array().unwrap() {
        let keys: Vec<&str> = c.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        for k in &keys {
            assert!(matches!(*k, "country" | "n" | "mean_years"), "unexpected aggregate key: {k}");
        }
    }
    });
}

/// API-24: aggregates report cohort distributions, k-gated (a group needs ≥20 to appear).
#[test]
fn aggregates_k_gating() {
    RT.block_on(async {
    let s = state().await;
    // k counts DISTINCT ACCOUNTS: seed 22 accounts each with a ZZ calc (>= k) and 3 for YY (< k).
    let seed = |country: &'static str, n: i32| {
        let pool = s.pool.clone();
        async move {
            // One fresh account per record, tagged by email_hash for cleanup.
            sqlx::query(
                "INSERT INTO account (id, email_hash, password_hash)
                 SELECT gen_random_uuid(), 'aggtest_'||$1||'_'||g, '!disabled' FROM generate_series(1,$2) g",
            ).bind(country).bind(n).execute(&pool).await.unwrap();
            sqlx::query(
                "INSERT INTO calculation (account_id, model_version_id, input_hash, inputs, estimate_years,
                     interval_low, interval_high, reaches_age, relative_risk, attributions)
                 SELECT a.id, (SELECT id FROM model_version WHERE is_active LIMIT 1),
                        'agg', jsonb_build_object('country',$1), 40, 36, 44, 80, 1.0, '[]'::jsonb
                 FROM account a WHERE a.email_hash LIKE 'aggtest_'||$1||'_%'",
            ).bind(country).execute(&pool).await.unwrap();
        }
    };
    seed("ZZ", 22).await;
    seed("YY", 3).await;

    let agg = body_json(call(&s, get("/api/aggregates")).await).await;
    assert!(agg["n"].as_i64().unwrap() >= 22, "n counts distinct accounts");
    assert!(!agg["estimate_years"].is_null(), "distribution present when cohort >= k");
    let countries: Vec<&str> = agg["by_country"].as_array().unwrap().iter()
        .filter_map(|c| c["country"].as_str()).collect();
    assert!(countries.contains(&"ZZ"), "ZZ (n>=20) is reported");
    assert!(!countries.contains(&"YY"), "YY (n<20) is suppressed");

    // Clean up seeded accounts (cascades their calculations).
    sqlx::query("DELETE FROM account WHERE email_hash LIKE 'aggtest_%'").execute(&s.pool).await.unwrap();
    });
}

/// API-30: the OpenAPI doc is generator-ready — every operation is well-formed and security refs resolve.
#[test]
fn openapi_spec_is_generator_ready() {
    RT.block_on(async {
    let s = state().await;
    let doc = body_json(call(&s, get("/api/openapi.json")).await).await;
    assert_eq!(doc["openapi"], "3.0.3");
    assert!(doc["info"]["title"].as_str().is_some() && doc["info"]["version"].as_str().is_some());
    assert!(doc["servers"].as_array().map(|a| !a.is_empty()).unwrap_or(false), "servers present");
    let has_bearer = doc["components"]["securitySchemes"]["bearerAuth"].is_object();
    assert!(has_bearer, "bearerAuth scheme defined");

    let mut op_count = 0;
    for (path, item) in doc["paths"].as_object().expect("paths") {
        for method in ["get", "post", "put", "delete", "patch"] {
            let op = &item[method];
            if op.is_null() { continue; }
            op_count += 1;
            assert!(op["summary"].as_str().is_some(), "{method} {path} has a summary");
            assert!(op["responses"]["200"].is_object(), "{method} {path} documents a 200");
            // Any operation that declares security must reference the defined bearerAuth scheme.
            if let Some(sec) = op.get("security").and_then(|v| v.as_array()) {
                assert!(sec.iter().any(|s| s.get("bearerAuth").is_some()), "{method} {path} security refs bearerAuth");
            }
        }
    }
    assert!(op_count >= 26, "all operations present (got {op_count})");
    });
}

/// API-26: /api/openapi.json is a valid OpenAPI doc listing every route (drift guard).
#[test]
fn openapi_lists_all_routes() {
    RT.block_on(async {
    let s = state().await;
    let doc = body_json(call(&s, get("/api/openapi.json")).await).await;
    assert_eq!(doc["openapi"], "3.0.3");
    assert!(doc["components"]["securitySchemes"]["bearerAuth"].is_object(), "bearer auth scheme declared");
    let paths = doc["paths"].as_object().expect("paths object");
    for p in [
        "/health", "/api/meta", "/api/openapi.json", "/api/auth/register", "/api/auth/login",
        "/api/questions", "/api/references", "/api/locations", "/api/aggregates",
        "/api/estimate", "/api/recommendations", "/api/whatif", "/api/relocate",
        "/api/calculations", "/api/answers", "/api/profile", "/api/profile/location",
        "/api/account/export", "/api/account",
        "/api/admin/audit", "/api/admin/questions", "/api/admin/questions/{code}",
        "/api/admin/features/{key}", "/api/admin/rules", "/api/admin/rules/{code}",
        "/api/admin/model/pin",
    ] {
        assert!(paths.contains_key(p), "OpenAPI spec is missing route {p}");
    }
    });
}

/// API-27: the SPA is served with a client-side-routing fallback; API routes take precedence.
#[test]
fn spa_served_with_fallback() {
    RT.block_on(async {
    let s = state().await;
    // A throwaway dist dir with an index.html.
    let dir = std::env::temp_dir().join(format!("clockspa_{}_{}", std::process::id(), COUNTER_CODE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("index.html"), "<!doctype html><title>Clock SPA</title>").unwrap();

    // A state pointing at that dist (reuses the shared pool/bundle).
    let custom = Arc::new(AppState {
        bundle: s.bundle.clone(), pool: s.pool.clone(), active_model_id: s.active_model_id,
        anon_account_id: s.anon_account_id, anon_profile_id: s.anon_profile_id,
        jwt_secret: s.jwt_secret.clone(), token_ttl_secs: s.token_ttl_secs,
        web_dist: dir.to_string_lossy().to_string(),
    });

    // A deep link (no such file) falls back to index.html.
    let resp = call(&custom, get("/dashboard/deep/link")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("Clock SPA"), "deep link serves the SPA index");

    // API routes still take precedence over the SPA fallback.
    let health = call(&custom, get("/health")).await;
    assert_eq!(health.status(), StatusCode::OK);
    assert_eq!(body_json(health).await["status"], "ok");

    let _ = std::fs::remove_dir_all(&dir);
    });
}

/// API-28: error responses are structured JSON ({"error": "..."}), not plain text.
#[test]
fn errors_are_structured_json() {
    RT.block_on(async {
    let s = state().await;
    // 401 (no auth).
    let resp = call(&s, get("/api/calculations")).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = body_json(resp).await;
    assert!(body["error"].as_str().is_some(), "401 body is JSON with an error field");

    // 400 (validation).
    let mut bad = valid_profile();
    bad["age"] = json!(5);
    let resp = call(&s, post("/api/estimate", bad)).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(body_json(resp).await["error"].as_str().is_some(), "400 body is JSON");

    // 404 (unknown relocate target).
    let resp = call(&s, post("/api/relocate", json!({"base": valid_profile(), "to": "Nowhere"}))).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(body_json(resp).await["error"].as_str().is_some(), "404 body is JSON");
    });
}

/// API-29: /health is a readiness check reporting DB + bundle status.
#[test]
fn health_readiness() {
    RT.block_on(async {
    let s = state().await;
    let resp = call(&s, get("/health")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["status"], "ok");
    assert_eq!(body["db"], "up", "readiness confirms the database is reachable");
    assert!(body["countries"].as_u64().unwrap() > 0, "bundle loaded");
    });
}

/// API-16: the admin surface is gated — 401 unauthenticated, 403 for a regular user, 200 for an admin.
#[test]
fn admin_gate() {
    RT.block_on(async {
    let s = state().await;
    assert_eq!(call(&s, get("/api/admin/audit")).await.status(), StatusCode::UNAUTHORIZED);

    let user = register_token(&s).await;
    assert_eq!(call(&s, get_auth("/api/admin/audit", &user)).await.status(), StatusCode::FORBIDDEN);

    let admin = register_admin(&s).await;
    let resp = call(&s, get_auth("/api/admin/audit", &admin)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_json(resp).await.is_array(), "audit log is a list");
    });
}

/// API-17: admin can create/update questions; version bumps; citation required; audit written.
#[test]
fn admin_question_mutations() {
    RT.block_on(async {
    let s = state().await;
    let admin = register_admin(&s).await;
    let put = |code: &str, body: Value| post_auth_put(&format!("/api/admin/questions/{code}"), body, &admin);

    // Citation is required.
    assert_eq!(call(&s, put("Q11_sleep", json!({"text": "x", "citation": ""}))).await.status(), StatusCode::BAD_REQUEST);

    // Read current version, update, confirm version bumped + change applied.
    let qs = body_json(call(&s, get("/api/questions")).await).await;
    let v0 = qs.as_array().unwrap().iter().find(|q| q["code"] == "Q11_sleep").unwrap()["version"].as_i64().unwrap();
    let resp = call(&s, put("Q11_sleep", json!({"text": "On a typical night, how many hours do you sleep? (edited)", "citation": "ops: wording tweak"}))).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["version"].as_i64().unwrap(), v0 + 1, "version bumped");

    // Unknown question → 404.
    assert_eq!(call(&s, put("Q999_nope", json!({"text": "x", "citation": "c"}))).await.status(), StatusCode::NOT_FOUND);

    // Create a new question (unique code across runs), duplicate → 409.
    let code = format!("Qtest_{}_{}", std::process::id(), COUNTER_CODE.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    let create = json!({"code": code, "section": "Test", "text": "Test question?", "input_type": "number", "citation": "ops: new item"});
    assert_eq!(call(&s, post_auth("/api/admin/questions", create.clone(), &admin)).await.status(), StatusCode::OK);
    assert_eq!(call(&s, post_auth("/api/admin/questions", create, &admin)).await.status(), StatusCode::CONFLICT);

    // The audit log records the mutations with citations.
    let audit = body_json(call(&s, get_auth("/api/admin/audit", &admin)).await).await;
    let entries = audit.as_array().unwrap();
    assert!(entries.iter().any(|e| e["entity"] == "question" && e["action"] == "update"
        && e["citation"].as_str().map(|c| !c.is_empty()).unwrap_or(false)), "update audited with citation");
    assert!(entries.iter().any(|e| e["entity"] == "question" && e["action"] == "create"), "create audited");

    // Clean up the created question so shared-DB count/order tests stay clean.
    sqlx::query("DELETE FROM question WHERE code = $1").bind(&code).execute(&s.pool).await.unwrap();
    });
}

static COUNTER_CODE: AtomicU64 = AtomicU64::new(0);

fn post_auth_put(uri: &str, body: Value, token: &str) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// API-14: relocate compares locations and explains air vs greenspace; symmetric; 404 on unknown.
#[test]
fn relocate_compares_locations() {
    RT.block_on(async {
    let s = state().await;
    let base = json!({"country": "RO", "age": 45, "sex": "M", "smoke": 0, "pa_min": 600, "sleep": 7, "waist": 90});

    let resp = call(&s, post("/api/relocate", json!({"base": base, "from": "Bucharest", "to": "Brașov"}))).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let r = body_json(resp).await;
    assert!(r["delta_years"].as_f64().unwrap() > 0.0, "cleaner+greener location adds years");
    assert!(r["breakdown"]["air_delta_years"].as_f64().is_some());
    assert!(r["breakdown"]["greenspace_delta_years"].as_f64().is_some());

    // Reverse move loses years (symmetric).
    let rev = body_json(call(&s, post("/api/relocate", json!({"base": base, "from": "Brașov", "to": "Bucharest"}))).await).await;
    assert!(rev["delta_years"].as_f64().unwrap() < 0.0, "dirtier location costs years");

    // Unknown target → 404.
    assert_eq!(
        call(&s, post("/api/relocate", json!({"base": base, "to": "Atlantis"}))).await.status(),
        StatusCode::NOT_FOUND);
    });
}

/// API-11 + API-13: locations are listed (public); a caller can set their home location.
#[test]
fn locations_and_home_location() {
    RT.block_on(async {
    let s = state().await;
    let locs = body_json(call(&s, get("/api/locations")).await).await;
    assert!(locs.as_array().unwrap().len() >= 5, "locations seeded and public");

    let token = register_token(&s).await;
    // Unknown location → 404.
    assert_eq!(
        call(&s, post_auth("/api/profile/location", json!({"name": "Atlantis"}), &token)).await.status(),
        StatusCode::NOT_FOUND);
    // Set a known location.
    let resp = call(&s, post_auth("/api/profile/location", json!({"name": "Cluj-Napoca"}), &token)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_json(resp).await["home_location_id"].as_str().is_some());
    // Profile reflects it.
    let p = body_json(call(&s, get_auth("/api/profile", &token)).await).await;
    assert!(p["home_location_id"].as_str().is_some(), "home location saved on the profile");
    // No auth → 401.
    assert_eq!(
        call(&s, post("/api/profile/location", json!({"name": "Cluj-Napoca"}))).await.status(),
        StatusCode::UNAUTHORIZED);
    });
}

/// API-08: the references endpoint returns studies, filterable by feature/rule.
#[test]
fn references_endpoint() {
    RT.block_on(async {
    let s = state().await;
    let all = body_json(call(&s, get("/api/references")).await).await;
    let arr = all.as_array().unwrap();
    assert_eq!(arr.len(), 8, "8 studies seeded");
    // Method papers carry a real DOI; every study has a code + title.
    assert!(arr.iter().any(|s| s["code"] == "cox-1972-proportional-hazards"
        && s["doi"].as_str().map(|d| d.starts_with("10.")).unwrap_or(false)));
    for st in arr {
        assert!(st["code"].as_str().is_some() && st["title"].as_str().is_some());
    }

    let act = body_json(call(&s, get("/api/references?feature=activity")).await).await;
    let codes: Vec<&str> = act.as_array().unwrap().iter().map(|s| s["code"].as_str().unwrap()).collect();
    assert!(codes.contains(&"instruments-scoring-formulas") && codes.contains(&"evidence-grades-and-alcohol"));

    let rule = body_json(call(&s, get("/api/references?rule=quit_smoking")).await).await;
    assert_eq!(rule.as_array().unwrap().len(), 1);

    let none = body_json(call(&s, get("/api/references?feature=nope")).await).await;
    assert_eq!(none.as_array().unwrap().len(), 0, "unknown feature → empty");
    });
}

/// DB4: an unknown question code is a 400, not a 500.
#[test]
fn unknown_question_code_is_bad_request() {
    RT.block_on(async {
    let s = state().await;
    let token = register_token(&s).await;
    let req = json!({"answers": [{"question_code": "Q999_nonsense", "value": 1}]});
    let resp = call(&s, post_auth("/api/answers", req, &token)).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    });
}

/// DB5d: two users are fully isolated — B never sees A's calculations or answers.
#[test]
fn two_user_isolation() {
    RT.block_on(async {
    let s = state().await;
    let token_a = register_token(&s).await;
    let token_b = register_token(&s).await;

    // A creates a calculation and an answer.
    call(&s, post_auth("/api/estimate", valid_profile(), &token_a)).await;
    call(&s, post_auth("/api/answers", json!({"answers": [{"question_code": "Q11_sleep", "value": "7-8"}]}), &token_a)).await;

    // B (fresh account) sees none of it.
    let b_hist = body_json(call(&s, get_auth("/api/calculations", &token_b)).await).await;
    assert_eq!(b_hist.as_array().unwrap().len(), 0, "B's history is empty");
    let b_answers = body_json(call(&s, get_auth("/api/answers", &token_b)).await).await;
    assert_eq!(b_answers.as_array().unwrap().len(), 0, "B's answers are empty");

    // A still sees its own.
    let a_hist = body_json(call(&s, get_auth("/api/calculations", &token_a)).await).await;
    assert!(a_hist.as_array().unwrap().len() >= 1, "A sees its own history");
    });
}

/// Validation still holds: bad input is a 400 (rejected before any insert). No auth needed.
#[test]
fn bad_input_rejected() {
    RT.block_on(async {
    let s = state().await;
    let mut bad = valid_profile();
    bad["age"] = json!(5); // below the 18 floor
    let resp = call(&s, post("/api/estimate", bad)).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    });
}
