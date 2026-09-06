//! The Clock of Life — Rust scoring service.
//!
//! v1 is a stateless scoring service: it loads a model artifact bundle and serves estimates. Database
//! persistence (accounts, calculation snapshots, admin) is a later slice, gated on PostgreSQL.

mod bundle;
mod scoring;

use std::sync::Arc;

use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde_json::json;

use bundle::Bundle;
use scoring::{estimate, Estimate, Profile};

#[tokio::main]
async fn main() {
    let dir = std::env::var("CLOCK_BUNDLE").unwrap_or_else(|_| "bundle/model-v2.0.0".to_string());
    let b = Bundle::load(std::path::Path::new(&dir)).unwrap_or_else(|e| {
        eprintln!("failed to load model bundle from {dir}: {e}");
        std::process::exit(1);
    });
    println!(
        "loaded model v{} ({}) — {} country baselines, checksums OK",
        b.manifest.version,
        b.manifest.algorithm,
        b.baselines.len()
    );
    let state = Arc::new(b);

    let app = Router::new()
        .route("/health", get(health))
        .route("/api/meta", get(meta))
        .route("/api/estimate", post(estimate_route))
        .with_state(state);

    let addr = "127.0.0.1:8080";
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    println!("clock-of-life-service listening on http://{addr}");
    axum::serve(listener, app).await.unwrap();
}

async fn health(State(b): State<Arc<Bundle>>) -> Json<serde_json::Value> {
    Json(json!({
        "status": "ok",
        "model_version": b.manifest.version,
        "countries": b.baselines.len(),
    }))
}

/// Active model version + provenance.
async fn meta(State(b): State<Arc<Bundle>>) -> Json<serde_json::Value> {
    let mut countries: Vec<&String> = b.baselines.keys().collect();
    countries.sort();
    Json(json!({
        "model_version": b.manifest.version,
        "algorithm": b.manifest.algorithm,
        "countries": countries,
        "assumptions": [
            "statistical estimate, not a prediction or diagnosis",
            "relative risk centred on the selected country's average person",
        ],
    }))
}

/// Answers -> Life-Clock estimate.
async fn estimate_route(
    State(b): State<Arc<Bundle>>,
    Json(profile): Json<Profile>,
) -> Result<Json<Estimate>, (StatusCode, String)> {
    estimate(&b, &profile)
        .map(Json)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))
}
