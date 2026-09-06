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

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Register a fresh unique account and return its bearer token.
async fn register_token(s: &Arc<AppState>) -> String {
    let resp = build_router(s.clone())
        .oneshot(post("/api/auth/register", json!({"email": unique_email(), "password": "password123"})))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "register should succeed");
    body_json(resp).await["token"].as_str().unwrap().to_string()
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

    let questions: i64 = sqlx::query_scalar("SELECT count(*) FROM question WHERE active")
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
    let rules: i64 = sqlx::query_scalar("SELECT count(*) FROM recommendation_rule WHERE active")
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

    let resp = build_router(s.clone()).oneshot(post("/api/auth/register", creds.clone())).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_json(resp).await["token"].as_str().is_some());

    // Duplicate registration -> 409.
    let resp = build_router(s.clone()).oneshot(post("/api/auth/register", creds.clone())).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // Too-short password -> 400.
    let resp = build_router(s.clone())
        .oneshot(post("/api/auth/register", json!({"email": unique_email(), "password": "short"}))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Correct login -> token.
    let resp = build_router(s.clone()).oneshot(post("/api/auth/login", creds)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Wrong password -> 401.
    let resp = build_router(s.clone())
        .oneshot(post("/api/auth/login", json!({"email": email, "password": "wrongpass1"}))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Unknown user -> 401 (same as wrong password).
    let resp = build_router(s.clone())
        .oneshot(post("/api/auth/login", json!({"email": unique_email(), "password": "whatever1"}))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    });
}

/// DB5d: personal routes reject unauthenticated callers.
#[test]
fn protected_routes_require_auth() {
    RT.block_on(async {
    let s = state().await;
    assert_eq!(build_router(s.clone()).oneshot(get("/api/calculations")).await.unwrap().status(), StatusCode::UNAUTHORIZED);
    assert_eq!(build_router(s.clone()).oneshot(get("/api/answers")).await.unwrap().status(), StatusCode::UNAUTHORIZED);
    let bad_token = build_router(s.clone()).oneshot(get_auth("/api/calculations", "not.a.jwt")).await.unwrap();
    assert_eq!(bad_token.status(), StatusCode::UNAUTHORIZED, "garbage token rejected");
    });
}

/// DB3 + DB4: an authenticated estimate persists a calculation the caller's history returns.
#[test]
fn estimate_persists_and_history_reads_back() {
    RT.block_on(async {
    let s = state().await;
    let token = register_token(&s).await;
    let resp = build_router(s.clone()).oneshot(post_auth("/api/estimate", valid_profile(), &token)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let est = body_json(resp).await;
    let calc_id = est["calculation_id"].as_str().expect("calculation_id returned").to_string();

    let resp = build_router(s.clone()).oneshot(get_auth("/api/calculations", &token)).await.unwrap();
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
    let resp = build_router(s.clone()).oneshot(post("/api/estimate", high)).await.unwrap();
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
    let resp = build_router(s.clone()).oneshot(post("/api/recommendations", context_only)).await.unwrap();
    let recs = body_json(resp).await;
    assert_eq!(recs.as_array().unwrap().len(), 0, "context factors are explained, never recommended");
    });
}

/// Anonymous (unauthenticated) estimate still works — try-before-signup.
#[test]
fn anonymous_estimate_still_works() {
    RT.block_on(async {
    let s = state().await;
    let resp = build_router(s.clone()).oneshot(post("/api/estimate", valid_profile())).await.unwrap();
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
    let resp = build_router(s.clone()).oneshot(post_auth("/api/estimate", valid_profile(), &token)).await.unwrap();
    let base_id = body_json(resp).await["calculation_id"].as_str().unwrap().to_string();

    let req = json!({"base": valid_profile(), "changes": {"pa_min": 2000}, "base_calculation_id": base_id});
    let resp = build_router(s.clone()).oneshot(post_auth("/api/whatif", req, &token)).await.unwrap();
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
    let resp = build_router(s.clone()).oneshot(post_auth("/api/estimate", valid_profile(), &token_a)).await.unwrap();
    let base_id = body_json(resp).await["calculation_id"].as_str().unwrap().to_string();

    let req = json!({"base": valid_profile(), "changes": {"pa_min": 2000}, "base_calculation_id": base_id});
    // B cannot fork A's calculation.
    let resp = build_router(s.clone()).oneshot(post_auth("/api/whatif", req.clone(), &token_b)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    // Unauthenticated cannot persist a scenario.
    let resp = build_router(s.clone()).oneshot(post("/api/whatif", req)).await.unwrap();
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
    let resp = build_router(s.clone()).oneshot(post("/api/recommendations", high)).await.unwrap();
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
    let resp = build_router(s.clone()).oneshot(post("/api/recommendations", healthy)).await.unwrap();
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
    let resp = build_router(s.clone()).oneshot(post("/api/whatif", req)).await.unwrap();
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
    assert_eq!(build_router(s.clone()).oneshot(post_auth("/api/answers", first, &token)).await.unwrap().status(), StatusCode::OK);

    // Re-answering the same question updates in place (no duplicate row).
    let second = json!({"answers": [{"question_code": "Q5_smoking", "value": "No, never"}]});
    assert_eq!(build_router(s.clone()).oneshot(post_auth("/api/answers", second, &token)).await.unwrap().status(), StatusCode::OK);

    let resp = build_router(s.clone()).oneshot(get_auth("/api/answers", &token)).await.unwrap();
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
    let resp = build_router(s.clone()).oneshot(get("/api/questions")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let qs = body_json(resp).await;
    let arr = qs.as_array().unwrap();
    assert_eq!(arr.len(), 24, "24 questions served");
    assert_eq!(arr[0]["code"], "Q1_age");
    assert_eq!(arr[9]["code"], "Q10_sedentary", "numeric order (Q10 after Q9, not after Q1)");

    // Profile: requires auth, returns the caller's saved answers.
    assert_eq!(build_router(s.clone()).oneshot(get("/api/profile")).await.unwrap().status(), StatusCode::UNAUTHORIZED);
    let token = register_token(&s).await;
    build_router(s.clone())
        .oneshot(post_auth("/api/answers", json!({"answers": [{"question_code": "Q11_sleep", "value": "7-8"}]}), &token))
        .await.unwrap();
    let resp = build_router(s.clone()).oneshot(get_auth("/api/profile", &token)).await.unwrap();
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
        "SELECT count(*) FROM recommendation_rule r WHERE r.active
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

    let est = body_json(build_router(s.clone()).oneshot(post("/api/estimate", high.clone())).await.unwrap()).await;
    let why = est["why"].as_array().unwrap();
    let smoking = why.iter().find(|w| w["key"] == "smk_current").expect("smoking factor");
    let refs = smoking["references"].as_array().expect("references array");
    assert!(!refs.is_empty(), "smoking factor carries a reference");
    // Reference is openable: a DOI or an internal review slug.
    assert!(refs[0]["doi"].as_str().is_some() || refs[0]["review_slug"].as_str().is_some());

    let recs = body_json(build_router(s.clone()).oneshot(post("/api/recommendations", high)).await.unwrap()).await;
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
    let recs = body_json(build_router(s.clone()).oneshot(post("/api/recommendations", dirty.clone())).await.unwrap()).await;
    assert!(recs.as_array().unwrap().iter().all(|r| r["feature"] != "env"), "env is never recommended");

    // why[] carries no env factor (env is a context term outside the attribution set).
    let est = body_json(build_router(s.clone()).oneshot(post("/api/estimate", dirty.clone())).await.unwrap()).await;
    assert!(est["why"].as_array().unwrap().iter().all(|w| w["key"] != "env"), "env is not a why[] factor");

    // But relocation DOES act on it.
    let rel = body_json(build_router(s.clone())
        .oneshot(post("/api/relocate", json!({"base": dirty, "to": "Rural (national)"}))).await.unwrap()).await;
    assert!(rel["delta_years"].as_f64().unwrap() > 0.0, "moving to cleaner air adds years (env as lever)");
    });
}

/// API-14: relocate compares locations and explains air vs greenspace; symmetric; 404 on unknown.
#[test]
fn relocate_compares_locations() {
    RT.block_on(async {
    let s = state().await;
    let base = json!({"country": "RO", "age": 45, "sex": "M", "smoke": 0, "pa_min": 600, "sleep": 7, "waist": 90});

    let resp = build_router(s.clone())
        .oneshot(post("/api/relocate", json!({"base": base, "from": "Bucharest", "to": "Brașov"}))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let r = body_json(resp).await;
    assert!(r["delta_years"].as_f64().unwrap() > 0.0, "cleaner+greener location adds years");
    assert!(r["breakdown"]["air_delta_years"].as_f64().is_some());
    assert!(r["breakdown"]["greenspace_delta_years"].as_f64().is_some());

    // Reverse move loses years (symmetric).
    let rev = body_json(build_router(s.clone())
        .oneshot(post("/api/relocate", json!({"base": base, "from": "Brașov", "to": "Bucharest"}))).await.unwrap()).await;
    assert!(rev["delta_years"].as_f64().unwrap() < 0.0, "dirtier location costs years");

    // Unknown target → 404.
    assert_eq!(
        build_router(s.clone()).oneshot(post("/api/relocate", json!({"base": base, "to": "Atlantis"}))).await.unwrap().status(),
        StatusCode::NOT_FOUND);
    });
}

/// API-11 + API-13: locations are listed (public); a caller can set their home location.
#[test]
fn locations_and_home_location() {
    RT.block_on(async {
    let s = state().await;
    let locs = body_json(build_router(s.clone()).oneshot(get("/api/locations")).await.unwrap()).await;
    assert!(locs.as_array().unwrap().len() >= 5, "locations seeded and public");

    let token = register_token(&s).await;
    // Unknown location → 404.
    assert_eq!(
        build_router(s.clone()).oneshot(post_auth("/api/profile/location", json!({"name": "Atlantis"}), &token)).await.unwrap().status(),
        StatusCode::NOT_FOUND);
    // Set a known location.
    let resp = build_router(s.clone()).oneshot(post_auth("/api/profile/location", json!({"name": "Cluj-Napoca"}), &token)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_json(resp).await["home_location_id"].as_str().is_some());
    // Profile reflects it.
    let p = body_json(build_router(s.clone()).oneshot(get_auth("/api/profile", &token)).await.unwrap()).await;
    assert!(p["home_location_id"].as_str().is_some(), "home location saved on the profile");
    // No auth → 401.
    assert_eq!(
        build_router(s.clone()).oneshot(post("/api/profile/location", json!({"name": "Cluj-Napoca"}))).await.unwrap().status(),
        StatusCode::UNAUTHORIZED);
    });
}

/// API-08: the references endpoint returns studies, filterable by feature/rule.
#[test]
fn references_endpoint() {
    RT.block_on(async {
    let s = state().await;
    let all = body_json(build_router(s.clone()).oneshot(get("/api/references")).await.unwrap()).await;
    let arr = all.as_array().unwrap();
    assert_eq!(arr.len(), 8, "8 studies seeded");
    // Method papers carry a real DOI; every study has a code + title.
    assert!(arr.iter().any(|s| s["code"] == "cox-1972-proportional-hazards"
        && s["doi"].as_str().map(|d| d.starts_with("10.")).unwrap_or(false)));
    for st in arr {
        assert!(st["code"].as_str().is_some() && st["title"].as_str().is_some());
    }

    let act = body_json(build_router(s.clone()).oneshot(get("/api/references?feature=activity")).await.unwrap()).await;
    let codes: Vec<&str> = act.as_array().unwrap().iter().map(|s| s["code"].as_str().unwrap()).collect();
    assert!(codes.contains(&"instruments-scoring-formulas") && codes.contains(&"evidence-grades-and-alcohol"));

    let rule = body_json(build_router(s.clone()).oneshot(get("/api/references?rule=quit_smoking")).await.unwrap()).await;
    assert_eq!(rule.as_array().unwrap().len(), 1);

    let none = body_json(build_router(s.clone()).oneshot(get("/api/references?feature=nope")).await.unwrap()).await;
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
    let resp = build_router(s.clone()).oneshot(post_auth("/api/answers", req, &token)).await.unwrap();
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
    build_router(s.clone()).oneshot(post_auth("/api/estimate", valid_profile(), &token_a)).await.unwrap();
    build_router(s.clone())
        .oneshot(post_auth("/api/answers", json!({"answers": [{"question_code": "Q11_sleep", "value": "7-8"}]}), &token_a))
        .await.unwrap();

    // B (fresh account) sees none of it.
    let b_hist = body_json(build_router(s.clone()).oneshot(get_auth("/api/calculations", &token_b)).await.unwrap()).await;
    assert_eq!(b_hist.as_array().unwrap().len(), 0, "B's history is empty");
    let b_answers = body_json(build_router(s.clone()).oneshot(get_auth("/api/answers", &token_b)).await.unwrap()).await;
    assert_eq!(b_answers.as_array().unwrap().len(), 0, "B's answers are empty");

    // A still sees its own.
    let a_hist = body_json(build_router(s.clone()).oneshot(get_auth("/api/calculations", &token_a)).await.unwrap()).await;
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
    let resp = build_router(s.clone()).oneshot(post("/api/estimate", bad)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    });
}
