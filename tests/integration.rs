//! Integration tests (SVC-DBT) — exercise migrations, reconciliation, and the persist/read paths
//! against a live local PostgreSQL. Point at a throwaway database via `TEST_DATABASE_URL`
//! (default `postgresql:///clock_of_life_test?host=/var/run/postgresql`).
//!
//! One shared state is built once (migrate + reconcile + truncate volatile tables); every assertion is
//! scoped to ids it creates, so the tests are safe to run in parallel against the shared database.

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

fn post(uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().method("GET").uri(uri).body(Body::empty()).unwrap()
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
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

    // Every question's feature_key (when set) must reference a seeded feature (FK integrity).
    let dangling: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM question q
         WHERE q.feature_key IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM feature f WHERE f.key = q.feature_key)",
    ).fetch_one(&s.pool).await.unwrap();
    assert_eq!(dangling, 0, "no question points at a missing feature");

    // The anonymous owner exists (pre-auth persistence target).
    let acct: i64 = sqlx::query_scalar("SELECT count(*) FROM account WHERE id = $1")
        .bind(s.anon_account_id).fetch_one(&s.pool).await.unwrap();
    assert_eq!(acct, 1, "anonymous account seeded");
    });
}

/// DB3 + DB4: an estimate persists a calculation that the history read path returns.
#[test]
fn estimate_persists_and_history_reads_back() {
    RT.block_on(async {
    let s = state().await;
    let resp = build_router(s.clone()).oneshot(post("/api/estimate", valid_profile())).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let est = body_json(resp).await;
    let calc_id = est["calculation_id"].as_str().expect("calculation_id returned").to_string();
    assert!(est["estimate_years"].as_f64().unwrap() > 0.0);

    let resp = build_router(s.clone()).oneshot(get("/api/calculations")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let history = body_json(resp).await;
    let rows = history.as_array().expect("history is an array");
    let found = rows.iter().find(|r| r["id"] == calc_id).expect("new calculation is in history");
    assert_eq!(found["estimate_years"], est["estimate_years"], "persisted years round-trip");
    assert_eq!(found["input_hash"].as_str().unwrap().len(), 16, "input hash stored");
    });
}

/// DB3: a What-If persists a scenario when a base calculation is named.
#[test]
fn whatif_persists_scenario() {
    RT.block_on(async {
    let s = state().await;
    // First create a base calculation.
    let resp = build_router(s.clone()).oneshot(post("/api/estimate", valid_profile())).await.unwrap();
    let base_id = body_json(resp).await["calculation_id"].as_str().unwrap().to_string();

    let req = json!({
        "base": valid_profile(),
        "changes": {"pa_min": 2000},
        "base_calculation_id": base_id,
    });
    let resp = build_router(s.clone()).oneshot(post("/api/whatif", req)).await.unwrap();
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

/// DB3: What-If without a base is a pure overlay — nothing persisted.
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

/// DB4: answers upsert (one current answer per question) and read back.
#[test]
fn answers_upsert_and_readback() {
    RT.block_on(async {
    let s = state().await;
    let first = json!({"answers": [{"question_code": "Q5_smoking", "value": "Yes, currently"}]});
    let resp = build_router(s.clone()).oneshot(post("/api/answers", first)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Re-answering the same question updates in place (no duplicate row).
    let second = json!({"answers": [{"question_code": "Q5_smoking", "value": "No, never"}]});
    let resp = build_router(s.clone()).oneshot(post("/api/answers", second)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = build_router(s.clone()).oneshot(get("/api/answers")).await.unwrap();
    let answers = body_json(resp).await;
    let q5: Vec<&Value> = answers.as_array().unwrap().iter()
        .filter(|a| a["question_code"] == "Q5_smoking").collect();
    assert_eq!(q5.len(), 1, "exactly one current answer for Q5 (upsert, not insert)");
    assert_eq!(q5[0]["value"], "No, never", "latest value wins");
    });
}

/// DB4: an unknown question code is a 400, not a 500.
#[test]
fn unknown_question_code_is_bad_request() {
    RT.block_on(async {
    let s = state().await;
    let req = json!({"answers": [{"question_code": "Q999_nonsense", "value": 1}]});
    let resp = build_router(s.clone()).oneshot(post("/api/answers", req)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    });
}

/// Validation still holds through the persistence layer: bad input is a 400 (rejected before any
/// insert, so nothing is stored — the rejection happens in `validate()`, ahead of persistence).
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
