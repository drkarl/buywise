//! BuyWise HTTP API server entry point (axum 0.8).
//!
//! Thin binary over the [`buywise_api`] library: it connects the SQLite pool,
//! runs migrations, and serves [`buywise_api::app`]. All routing and handler
//! logic lives in the library so it is reachable from integration tests.

use std::net::SocketAddr;

use anyhow::Context;
use buywise_api::{app, AppState};
use buywise_core::db;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let database_url =
        std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite://buywise.db".to_string());
    let pool = db::connect(&database_url)
        .await
        .with_context(|| format!("connect to {database_url}"))?;
    db::run_migrations(&pool).await.context("run migrations")?;
    db::assert_json1(&pool).await.context("assert JSON1")?;

    let state = AppState::new(pool);
    let app = app(state);

    let addr: SocketAddr = std::env::var("BIND_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:8080".to_string())
        .parse()
        .context("parse BIND_ADDR")?;

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    tracing::info!("BuyWise API listening on {addr}");
    axum::serve(listener, app).await.context("serve")?;
    Ok(())
}
