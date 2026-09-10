//! The Clock of Life — Rust scoring service (binary).
//!
//! Thin wrapper: load the bundle, connect + migrate + reconcile the database, then serve. The router and
//! all logic live in the library crate so integration tests can drive them in-process.

use clock_of_life_service::{build_router, default_database_url, init_state};

#[tokio::main]
async fn main() {
    // Structured logging + request tracing (RUST_LOG controls verbosity; default info).
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tower_http=info".into()),
        )
        .init();

    let dir = std::env::var("CLOCK_BUNDLE").unwrap_or_else(|_| "bundle/model-v4.1.1".to_string());
    let database_url = default_database_url();

    let state = init_state(&dir, &database_url).await.unwrap_or_else(|e| {
        eprintln!("startup failed: {e}");
        std::process::exit(1);
    });
    println!(
        "loaded model v{} ({}) — {} country baselines; db connected, migrated, reconciled",
        state.bundle.manifest.version,
        state.bundle.manifest.algorithm,
        state.bundle.baselines.len()
    );

    let app = build_router(state);
    // Configurable so a second instance can run beside a first — reviewing a bundle change means
    // having the old and the new one answering at once.
    let addr = std::env::var("CLOCK_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("cannot bind {addr}: {e} (set CLOCK_ADDR to use another port)"));
    println!("clock-of-life-service listening on http://{addr}");
    axum::serve(listener, app).await.unwrap();
}
