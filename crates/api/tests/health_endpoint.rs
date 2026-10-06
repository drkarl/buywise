//! Integration tests for the unauthenticated readiness probe (issue #141).
//!
//! Drives the real router (`buywise_api::app`) against an in-memory SQLite
//! database, covering the endpoint's contract:
//!   * a reachable, fully-migrated database returns `200` with a short status
//!     body and nothing else — no session cookie and no CSRF token are sent, so
//!     this also proves the route sits outside the auth/CSRF extractor;
//!   * a database whose migrations have not been applied is reported `503`;
//!   * a database that cannot be reached at all (closed pool) is reported `503`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use buywise_api::{app, AppState};
use buywise_core::db;
use http_body_util::BodyExt;
use sqlx::SqlitePool;
use tower::ServiceExt; // for `oneshot`

/// GET `/api/health` through the full app with no cookies or headers.
async fn get_health(pool: SqlitePool) -> (StatusCode, String) {
    let response = app(AppState::new(pool))
        .oneshot(
            Request::builder()
                .uri("/api/health")
                .body(Body::empty())
                .expect("build request"),
        )
        .await
        .expect("request");

    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("collect body")
        .to_bytes();
    (
        status,
        String::from_utf8(bytes.to_vec()).expect("utf-8 body"),
    )
}

/// A migrated database reports ready, and the body is only the short status —
/// the one field, no database path or migration detail an anonymous caller must
/// not see.
#[tokio::test]
async fn health_reports_ok_with_a_short_status_for_a_migrated_database() {
    let pool = db::connect("sqlite::memory:").await.expect("connect");
    db::run_migrations(&pool).await.expect("migrate");

    let (status, body) = get_health(pool).await;

    assert_eq!(status, StatusCode::OK, "body: {body}");
    let json: serde_json::Value = serde_json::from_str(&body).expect("json body");
    assert_eq!(json["status"], "ok");
    assert_eq!(
        json.as_object().expect("object body").len(),
        1,
        "body must carry only the status field: {body}"
    );
    // Odd for a reachable, migrated database to leak either of these.
    assert!(
        !body.contains("sqlite"),
        "body leaked the database URL: {body}"
    );
    assert!(!body.contains("/var/lib"), "body leaked a path: {body}");
}

/// A database the binary was built against but never migrated is not ready: the
/// `_sqlx_migrations` table is absent, so the check fails and answers `503`.
#[tokio::test]
async fn health_reports_unavailable_when_migrations_are_not_applied() {
    let pool = db::connect("sqlite::memory:").await.expect("connect");
    // Deliberately skip `db::run_migrations`.

    let (status, body) = get_health(pool).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "body: {body}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).expect("json body")["status"],
        "unavailable"
    );
}

/// A closed pool cannot answer the probe query, so the endpoint reports `503`
/// rather than a false `200`.
#[tokio::test]
async fn health_reports_unavailable_when_the_database_is_unreachable() {
    let pool = db::connect("sqlite::memory:").await.expect("connect");
    db::run_migrations(&pool).await.expect("migrate");
    pool.close().await;

    let (status, body) = get_health(pool).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "body: {body}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).expect("json body")["status"],
        "unavailable"
    );
}
