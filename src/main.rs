//! The Clock of Life — Rust scoring service.
//!
//! v1 is a stateless scoring service: it loads a model artifact bundle and serves estimates. Database
//! persistence (accounts, calculation snapshots, admin) is a later slice, gated on PostgreSQL.

use axum::{routing::get, Json, Router};
use serde_json::json;

#[tokio::main]
async fn main() {
    let app = Router::new().route("/health", get(health));

    let addr = "127.0.0.1:8080";
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    println!("clock-of-life-service listening on http://{addr}");
    axum::serve(listener, app).await.unwrap();
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok", "service": "clock-of-life-service" }))
}
