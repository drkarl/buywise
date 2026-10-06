//! Integration tests for the receipt-image endpoints (issue #149, roadmap #114).
//!
//! Drives the real axum router against a private in-memory database with the
//! full migrations applied, and against a real temporary receipt directory, so
//! the tests exercise the same code path as a deployment: bytes written to disk,
//! records in SQLite, and only the authenticated endpoints in between.
//!
//! Two seeded households pin the acceptance criteria: a member can upload a
//! photo, see it listed with its expiry, view the bytes and delete it; a member
//! of another household can do none of those; oversized, wrong-type and
//! over-quota uploads are refused; household delete removes the files and the
//! records; and the export lists the image metadata without the bytes.

use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use buywise_api::{app, AppState};
use buywise_core::db;
use buywise_core::receipts::ReceiptConfig;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tempfile::TempDir;
use tower::ServiceExt; // for `oneshot`

const PASSWORD: &str = "correct horse battery staple";

const HH_A: &str = "hh-a";
const HH_B: &str = "hh-b";
const HH_A_NAME: &str = "Household A";
const HH_B_NAME: &str = "Household B";

// ---------------------------------------------------------------------------
// Fixtures: minimal files with real signatures
// ---------------------------------------------------------------------------

/// A minimal file whose leading bytes carry `kind`'s signature, padded so it is
/// large enough to exercise size limits.
fn image_bytes(kind: &str, padding: usize) -> Vec<u8> {
    let mut bytes: Vec<u8> = match kind {
        "png" => vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
        "jpeg" => vec![0xFF, 0xD8, 0xFF, 0xE0],
        "webp" => b"RIFF\x00\x00\x00\x00WEBP".to_vec(),
        "heic" => {
            let mut b = vec![0, 0, 0, 0x18];
            b.extend_from_slice(b"ftypheic");
            b
        }
        other => panic!("no fixture for {other}"),
    };
    bytes.resize(bytes.len() + padding, 0x00);
    bytes
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

struct Session {
    session: String,
    csrf: String,
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A migrated in-memory database plus a private receipt directory on disk.
struct Harness {
    pool: SqlitePool,
    dir: PathBuf,
    config: ReceiptConfig,
    /// Held so the temporary directory outlives the test.
    _temp: TempDir,
}

impl Harness {
    async fn new() -> Self {
        Self::with_config(ReceiptConfig::with_dir).await
    }

    /// Build a harness whose receipt limits come from `make_config`.
    async fn with_config(make_config: impl FnOnce(PathBuf) -> ReceiptConfig) -> Self {
        let pool = db::connect("sqlite::memory:").await.expect("connect");
        db::run_migrations(&pool).await.expect("migrate");
        let temp = TempDir::new().expect("temp dir");
        let dir = temp.path().join("receipts");
        let config = make_config(dir.clone());
        Self {
            pool,
            dir,
            config,
            _temp: temp,
        }
    }

    fn state(&self) -> AppState {
        AppState::new(self.pool.clone()).with_receipt_config(self.config.clone())
    }

    fn router(&self) -> Router {
        app(self.state())
    }

    fn router_at(&self, now: chrono::DateTime<chrono::Utc>) -> Router {
        app(self.state().with_clock(move || now))
    }

    /// The file names currently in the receipt directory (empty if it was never
    /// created).
    fn stored_files(&self) -> Vec<String> {
        match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries
                .map(|entry| {
                    entry
                        .expect("dir entry")
                        .file_name()
                        .to_string_lossy()
                        .into()
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    }
}

/// Seed one household with an owner, then log them in.
async fn seed_household(
    pool: &SqlitePool,
    household_id: &str,
    household_name: &str,
    user_id: &str,
    email: &str,
) -> Session {
    sqlx::query("INSERT INTO households (id, name, created_at) VALUES (?1, ?2, ?3)")
        .bind(household_id)
        .bind(household_name)
        .bind(db::now_iso())
        .execute(pool)
        .await
        .expect("insert household");

    let hash = buywise_api::auth::hash_password_for_storage(PASSWORD).expect("hash");
    sqlx::query(
        "INSERT INTO users (id, email, password_hash, display_name, created_at) \
         VALUES (?1, ?2, ?3, ?1, ?4)",
    )
    .bind(user_id)
    .bind(email)
    .bind(&hash)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert user");

    sqlx::query(
        "INSERT INTO household_members (household_id, user_id, role, joined_at) \
         VALUES (?1, ?2, 'owner', ?3)",
    )
    .bind(household_id)
    .bind(user_id)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert membership");

    login(pool, email).await
}

/// Two households, A and B, each with one owner.
async fn seed_two_households(pool: &SqlitePool) -> (Session, Session) {
    let a = seed_household(pool, HH_A, HH_A_NAME, "user-a", "a@example.test").await;
    let b = seed_household(pool, HH_B, HH_B_NAME, "user-b", "b@example.test").await;
    (a, b)
}

async fn login(pool: &SqlitePool, email: &str) -> Session {
    let body = json!({ "email": email, "password": PASSWORD }).to_string();
    let resp = app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "login should succeed");
    Session {
        session: cookie(&resp, "session"),
        csrf: cookie(&resp, "csrf"),
    }
}

fn cookie(resp: &axum::response::Response, name: &str) -> String {
    for value in resp.headers().get_all(header::SET_COOKIE) {
        let s = value.to_str().unwrap();
        let first = s.split(';').next().unwrap();
        if let Some((k, v)) = first.split_once('=') {
            if k.trim() == name {
                return v.trim().to_string();
            }
        }
    }
    panic!("cookie {name} not set");
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

async fn list(
    harness: &Harness,
    session: &Session,
    household_id: &str,
) -> axum::response::Response {
    harness
        .router()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/household/{household_id}/receipts"))
                .header(header::COOKIE, format!("session={}", session.session))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn upload(
    harness: &Harness,
    session: &Session,
    household_id: &str,
    query: &str,
    content_type: &str,
    body: Vec<u8>,
) -> axum::response::Response {
    harness
        .router()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/household/{household_id}/receipts{query}"))
                .header(header::CONTENT_TYPE, content_type)
                .header(header::COOKIE, format!("session={}", session.session))
                .header("X-CSRF-Token", &session.csrf)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
}

/// Upload a valid PNG and return its receipt id.
async fn upload_png(harness: &Harness, session: &Session, household_id: &str) -> String {
    let resp = upload(
        harness,
        session,
        household_id,
        "?processing=local",
        "image/png",
        image_bytes("png", 64),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    json_body(resp).await["id"]
        .as_str()
        .expect("receipt id")
        .to_string()
}

async fn download(
    harness: &Harness,
    session: &Session,
    household_id: &str,
    receipt_id: &str,
) -> axum::response::Response {
    harness
        .router()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!(
                    "/api/household/{household_id}/receipts/{receipt_id}"
                ))
                .header(header::COOKIE, format!("session={}", session.session))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn delete_receipt(
    harness: &Harness,
    session: &Session,
    household_id: &str,
    receipt_id: &str,
) -> axum::response::Response {
    harness
        .router()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!(
                    "/api/household/{household_id}/receipts/{receipt_id}"
                ))
                .header(header::COOKIE, format!("session={}", session.session))
                .header("X-CSRF-Token", &session.csrf)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn export(
    harness: &Harness,
    session: &Session,
    household_id: &str,
) -> axum::response::Response {
    harness
        .router()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/household/{household_id}/export"))
                .header(header::COOKIE, format!("session={}", session.session))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn delete_household(
    harness: &Harness,
    session: &Session,
    household_id: &str,
    name: &str,
) -> axum::response::Response {
    let body = json!({ "household_name": name, "password": PASSWORD }).to_string();
    harness
        .router()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/household/{household_id}/delete"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, format!("session={}", session.session))
                .header("X-CSRF-Token", &session.csrf)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn json_body(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    if bytes.is_empty() {
        return Value::Null;
    }
    serde_json::from_slice(&bytes).unwrap()
}

async fn count_receipts(pool: &SqlitePool, household_id: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM receipt_images WHERE household_id = ?1")
        .bind(household_id)
        .fetch_one(pool)
        .await
        .expect("count")
}

/// The record's stored file, resolved through the database (never by name).
async fn stored_file(harness: &Harness, receipt_id: &str) -> PathBuf {
    let key: String = sqlx::query_scalar("SELECT storage_key FROM receipt_images WHERE id = ?1")
        .bind(receipt_id)
        .fetch_one(&harness.pool)
        .await
        .expect("storage key");
    harness.dir.join(key)
}

// ---------------------------------------------------------------------------
// Upload, list, view, delete
// ---------------------------------------------------------------------------

/// A member uploads a photo and then sees it listed with its expiry, views the
/// bytes with the private headers, and deletes it.
#[tokio::test]
async fn member_uploads_lists_views_and_deletes_a_receipt() {
    let harness = Harness::new().await;
    let (a, _b) = seed_two_households(&harness.pool).await;

    let bytes = image_bytes("png", 64);
    let resp = upload(
        &harness,
        &a,
        HH_A,
        "?processing=local",
        "image/png",
        bytes.clone(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created = json_body(resp).await;
    let id = created["id"].as_str().expect("id").to_string();
    assert_eq!(created["content_type"], "image/png");
    assert_eq!(created["byte_size"], bytes.len() as i64);
    assert_eq!(created["sha256_hex"], sha256_hex(&bytes));
    assert_eq!(created["processing"], "local");
    assert_eq!(created["uploaded_by"], "user-a");
    assert_eq!(
        created["image_url"],
        format!("/api/household/{HH_A}/receipts/{id}")
    );
    // Expiry is 30 days after upload.
    let uploaded: chrono::DateTime<chrono::Utc> =
        created["uploaded_at"].as_str().unwrap().parse().unwrap();
    let expires: chrono::DateTime<chrono::Utc> =
        created["expires_at"].as_str().unwrap().parse().unwrap();
    assert_eq!(expires - uploaded, chrono::Duration::days(30));
    // The storage key is not exposed to the app.
    assert!(created.get("storage_key").is_none());

    // The bytes are on disk under a random name, owner-only, and the key has no
    // household or user data in it.
    let path = stored_file(&harness, &id).await;
    assert_eq!(std::fs::read(&path).expect("stored file"), bytes);
    let name = path.file_name().unwrap().to_string_lossy().to_string();
    assert!(!name.contains(HH_A) && !name.contains("user-a"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "receipt files must be owner-only");
    }

    // Listed for the household with its expiry date, and never cacheable.
    let listed = list(&harness, &a, HH_A).await;
    assert_eq!(listed.status(), StatusCode::OK);
    assert_eq!(
        listed
            .headers()
            .get(header::X_CONTENT_TYPE_OPTIONS)
            .unwrap(),
        "nosniff"
    );
    assert_eq!(
        listed.headers().get(header::CACHE_CONTROL).unwrap(),
        "private, no-store"
    );
    let listed = json_body(listed).await;
    assert_eq!(listed.as_array().expect("array").len(), 1);
    assert_eq!(listed[0]["id"], created["id"]);
    assert_eq!(listed[0]["expires_at"], created["expires_at"]);

    // Viewed through the authenticated handler, with the private headers.
    let fetched = download(&harness, &a, HH_A, &id).await;
    assert_eq!(fetched.status(), StatusCode::OK);
    assert_eq!(
        fetched.headers().get(header::CONTENT_TYPE).unwrap(),
        "image/png"
    );
    assert_eq!(
        fetched
            .headers()
            .get(header::X_CONTENT_TYPE_OPTIONS)
            .unwrap(),
        "nosniff"
    );
    assert_eq!(
        fetched.headers().get(header::CACHE_CONTROL).unwrap(),
        "private, no-store"
    );
    assert_eq!(
        fetched
            .headers()
            .get(header::CONTENT_DISPOSITION)
            .unwrap()
            .to_str()
            .unwrap(),
        format!("inline; filename=\"receipt-{id}.png\"")
    );
    let body = fetched.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.as_ref(), bytes.as_slice());

    // Deleted: bytes and record both go, and a second delete is a 404.
    let deleted = delete_receipt(&harness, &a, HH_A, &id).await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    assert!(!path.exists(), "the stored file must be removed");
    assert_eq!(count_receipts(&harness.pool, HH_A).await, 0);
    assert_eq!(
        delete_receipt(&harness, &a, HH_A, &id).await.status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn expired_receipt_is_hidden_and_unavailable_before_the_sweep() {
    let harness = Harness::new().await;
    let (a, _b) = seed_two_households(&harness.pool).await;
    let id = upload_png(&harness, &a, HH_A).await;
    let path = stored_file(&harness, &id).await;
    let expired_at = "2026-10-01T10:00:00.000Z";
    sqlx::query("UPDATE receipt_images SET expires_at = ?1 WHERE id = ?2")
        .bind(expired_at)
        .bind(&id)
        .execute(&harness.pool)
        .await
        .expect("set expiry");
    let router = harness.router_at(
        chrono::DateTime::parse_from_rfc3339("2026-10-01T11:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
    );

    let listed = router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/household/{HH_A}/receipts"))
                .header(header::COOKIE, format!("session={}", a.session))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(listed.status(), StatusCode::OK);
    assert!(json_body(listed).await.as_array().unwrap().is_empty());
    assert_eq!(
        router
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!("/api/household/{HH_A}/receipts/{id}"))
                    .header(header::COOKIE, format!("session={}", a.session))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert!(
        path.exists(),
        "read-time expiry does not run the storage sweep"
    );
    assert_eq!(count_receipts(&harness.pool, HH_A).await, 1);
}

#[tokio::test]
async fn deleting_uploader_account_preserves_receipt_with_null_uploader() {
    let harness = Harness::new().await;
    let (a, _b) = seed_two_households(&harness.pool).await;
    let id = upload_png(&harness, &a, HH_A).await;

    sqlx::query("DELETE FROM sessions WHERE user_id = 'user-a'")
        .execute(&harness.pool)
        .await
        .expect("delete sessions");
    sqlx::query("DELETE FROM household_members WHERE user_id = 'user-a'")
        .execute(&harness.pool)
        .await
        .expect("delete membership");
    sqlx::query("DELETE FROM users WHERE id = 'user-a'")
        .execute(&harness.pool)
        .await
        .expect("delete uploader");

    let uploader: Option<String> =
        sqlx::query_scalar("SELECT uploaded_by FROM receipt_images WHERE id = ?1")
            .bind(&id)
            .fetch_one(&harness.pool)
            .await
            .expect("receipt uploader");
    assert_eq!(uploader, None);
    assert_eq!(count_receipts(&harness.pool, HH_A).await, 1);
}

/// Every accepted format is stored with its detected content type, regardless
/// of the client's `Content-Type`.
#[tokio::test]
async fn detected_signature_decides_the_stored_content_type() {
    let harness = Harness::new().await;
    let (a, _b) = seed_two_households(&harness.pool).await;

    for (kind, expected) in [
        ("png", "image/png"),
        ("jpeg", "image/jpeg"),
        ("webp", "image/webp"),
        ("heic", "image/heic"),
    ] {
        // A deliberately wrong client Content-Type: it is ignored.
        let resp = upload(
            &harness,
            &a,
            HH_A,
            "",
            "application/octet-stream",
            image_bytes(kind, 32),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED, "{kind}");
        let body = json_body(resp).await;
        assert_eq!(body["content_type"], expected, "{kind}");
        // The default processing choice is `local`.
        assert_eq!(body["processing"], "local", "{kind}");
    }
    assert_eq!(count_receipts(&harness.pool, HH_A).await, 4);
}

/// A text file renamed to `.jpg` is refused: the signature, not the name or the
/// client header, decides.
#[tokio::test]
async fn a_text_file_renamed_to_jpg_is_refused() {
    let harness = Harness::new().await;
    let (a, _b) = seed_two_households(&harness.pool).await;

    let resp = upload(
        &harness,
        &a,
        HH_A,
        "",
        "image/jpeg",
        b"just some text, honest".to_vec(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let body = json_body(resp).await;
    assert!(
        body["error"].as_str().unwrap().contains("JPEG, PNG, WebP"),
        "the refusal must say what is accepted: {body}"
    );
    assert_eq!(count_receipts(&harness.pool, HH_A).await, 0);
    assert!(harness.stored_files().is_empty(), "nothing may be written");
}

/// An oversize image is refused with 413 and nothing is written.
#[tokio::test]
async fn an_oversized_image_is_refused() {
    let harness = Harness::with_config(|dir| {
        ReceiptConfig::with_dir(dir).with_limits(64, 200, 500 * 1024 * 1024)
    })
    .await;
    let (a, _b) = seed_two_households(&harness.pool).await;

    let resp = upload(&harness, &a, HH_A, "", "image/png", image_bytes("png", 200)).await;
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(count_receipts(&harness.pool, HH_A).await, 0);
    assert!(harness.stored_files().is_empty());
}

/// The per-household image-count quota is enforced with a clear 409.
#[tokio::test]
async fn the_household_image_count_quota_is_enforced() {
    let harness = Harness::with_config(|dir| {
        ReceiptConfig::with_dir(dir).with_limits(10 * 1024 * 1024, 1, 500 * 1024 * 1024)
    })
    .await;
    let (a, _b) = seed_two_households(&harness.pool).await;

    upload_png(&harness, &a, HH_A).await;
    let resp = upload(&harness, &a, HH_A, "", "image/png", image_bytes("png", 8)).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = json_body(resp).await;
    assert!(
        body["error"].as_str().unwrap().contains("delete some"),
        "the refusal must say what to do: {body}"
    );
    assert_eq!(count_receipts(&harness.pool, HH_A).await, 1);
}

#[tokio::test]
async fn expired_receipts_do_not_count_toward_upload_quotas_before_the_sweep() {
    let harness = Harness::with_config(|dir| {
        ReceiptConfig::with_dir(dir).with_limits(10 * 1024 * 1024, 1, 72)
    })
    .await;
    let (a, _b) = seed_two_households(&harness.pool).await;
    let expired_id = upload_png(&harness, &a, HH_A).await;
    let expired_path = stored_file(&harness, &expired_id).await;
    let now = chrono::DateTime::parse_from_rfc3339("2026-10-01T11:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    sqlx::query("UPDATE receipt_images SET expires_at = '2026-10-01T10:00:00.000Z' WHERE id = ?1")
        .bind(&expired_id)
        .execute(&harness.pool)
        .await
        .expect("expire first receipt");

    let uploaded = harness
        .router_at(now)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/household/{HH_A}/receipts"))
                .header(header::CONTENT_TYPE, "image/png")
                .header(header::COOKIE, format!("session={}", a.session))
                .header("X-CSRF-Token", &a.csrf)
                .body(Body::from(image_bytes("png", 64)))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(uploaded.status(), StatusCode::CREATED);
    assert_eq!(count_receipts(&harness.pool, HH_A).await, 2);
    assert!(
        expired_path.exists(),
        "quota filtering does not run the sweep"
    );
}

/// The per-household total-size quota is enforced with a clear 409.
#[tokio::test]
async fn the_household_total_size_quota_is_enforced() {
    let harness = Harness::with_config(|dir| {
        ReceiptConfig::with_dir(dir).with_limits(10 * 1024 * 1024, 200, 100)
    })
    .await;
    let (a, _b) = seed_two_households(&harness.pool).await;

    upload_png(&harness, &a, HH_A).await;
    // 72 stored bytes (the PNG fixture above) plus this one must cross 100.
    let resp = upload(&harness, &a, HH_A, "", "image/png", image_bytes("png", 40)).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = json_body(resp).await;
    assert!(
        body["error"].as_str().unwrap().contains("storage is full"),
        "the refusal must say why: {body}"
    );
    assert_eq!(count_receipts(&harness.pool, HH_A).await, 1);
}

/// External processing is refused with a message that says it does not exist
/// yet — the per-upload choice is recorded policy, and no external call exists.
#[tokio::test]
async fn external_processing_is_refused_with_a_clear_message() {
    let harness = Harness::new().await;
    let (a, _b) = seed_two_households(&harness.pool).await;

    let resp = upload(
        &harness,
        &a,
        HH_A,
        "?processing=external",
        "image/png",
        image_bytes("png", 16),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    let body = json_body(resp).await;
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("External receipt processing is not available yet"),
        "got {body}"
    );
    assert_eq!(count_receipts(&harness.pool, HH_A).await, 0);

    // An unknown choice is a client error, not a silent `local`.
    let resp = upload(
        &harness,
        &a,
        HH_A,
        "?processing=sideways",
        "image/png",
        image_bytes("png", 16),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// Upload and delete are mutations: no session is 401, and a missing CSRF token
/// is 403.
#[tokio::test]
async fn upload_and_delete_require_a_session_and_csrf() {
    let harness = Harness::new().await;
    let (a, _b) = seed_two_households(&harness.pool).await;
    let id = upload_png(&harness, &a, HH_A).await;

    // No session at all.
    let resp = harness
        .router()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/household/{HH_A}/receipts"))
                .header(header::CONTENT_TYPE, "image/png")
                .body(Body::from(image_bytes("png", 8)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Session but no CSRF header.
    let resp = harness
        .router()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/household/{HH_A}/receipts"))
                .header(header::CONTENT_TYPE, "image/png")
                .header(header::COOKIE, format!("session={}", a.session))
                .body(Body::from(image_bytes("png", 8)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Delete without the CSRF header.
    let resp = harness
        .router()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/household/{HH_A}/receipts/{id}"))
                .header(header::COOKIE, format!("session={}", a.session))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(count_receipts(&harness.pool, HH_A).await, 1);
}

// ---------------------------------------------------------------------------
// Other households
// ---------------------------------------------------------------------------

/// A member of another household can neither list, view nor delete this
/// household's receipts, and a receipt id of another household reads as missing.
#[tokio::test]
async fn another_household_cannot_reach_this_households_receipts() {
    let harness = Harness::new().await;
    let (a, b) = seed_two_households(&harness.pool).await;
    let id = upload_png(&harness, &a, HH_A).await;

    assert_eq!(
        list(&harness, &b, HH_A).await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        download(&harness, &b, HH_A, &id).await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        delete_receipt(&harness, &b, HH_A, &id).await.status(),
        StatusCode::FORBIDDEN
    );
    // B's own household simply has no such receipt.
    assert_eq!(
        download(&harness, &b, HH_B, &id).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        delete_receipt(&harness, &b, HH_B, &id).await.status(),
        StatusCode::NOT_FOUND
    );

    // B's attempts changed nothing for A.
    assert_eq!(count_receipts(&harness.pool, HH_A).await, 1);
    assert_eq!(list(&harness, &a, HH_A).await.status(), StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Export and household delete
// ---------------------------------------------------------------------------

/// The export lists the household's image metadata (and never the bytes or the
/// internal storage key).
#[tokio::test]
async fn export_lists_receipt_image_metadata() {
    let harness = Harness::new().await;
    let (a, b) = seed_two_households(&harness.pool).await;
    let bytes = image_bytes("webp", 40);
    let created = upload(&harness, &a, HH_A, "", "image/webp", bytes.clone()).await;
    assert_eq!(created.status(), StatusCode::CREATED);

    let doc = json_body(export(&harness, &a, HH_A).await).await;
    let receipts = doc["receipt_images"].as_array().expect("receipt_images");
    assert_eq!(receipts.len(), 1);
    let entry = &receipts[0];
    assert_eq!(entry["content_type"], "image/webp");
    assert_eq!(entry["byte_size"], bytes.len() as i64);
    assert_eq!(entry["sha256_hex"], sha256_hex(&bytes));
    assert_eq!(entry["processing"], "local");
    assert_eq!(entry["uploaded_by"], "user-a");
    assert!(entry["uploaded_at"].is_string());
    assert!(entry["expires_at"].is_string());
    assert!(
        entry.get("storage_key").is_none(),
        "the internal file name must not be exported"
    );

    // The other household's own (empty) export has no receipts of ours.
    let other = json_body(export(&harness, &b, HH_B).await).await;
    assert!(other["receipt_images"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn household_export_omits_expired_unswept_receipts() {
    let harness = Harness::new().await;
    let (a, _b) = seed_two_households(&harness.pool).await;
    let expired_id = upload_png(&harness, &a, HH_A).await;
    let path = stored_file(&harness, &expired_id).await;
    sqlx::query("UPDATE receipt_images SET expires_at = '2026-10-01T10:00:00.000Z' WHERE id = ?1")
        .bind(&expired_id)
        .execute(&harness.pool)
        .await
        .expect("expire receipt");
    let now = chrono::DateTime::parse_from_rfc3339("2026-10-01T11:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);

    let document = harness
        .router_at(now)
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/household/{HH_A}/export"))
                .header(header::COOKIE, format!("session={}", a.session))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(document.status(), StatusCode::OK);
    let document = json_body(document).await;
    assert!(document["receipt_images"].as_array().unwrap().is_empty());
    assert_eq!(count_receipts(&harness.pool, HH_A).await, 1);
    assert!(path.exists(), "export filtering does not run the sweep");
}

/// Household delete removes every receipt file and record of that household,
/// and leaves another household's files and rows untouched.
#[tokio::test]
async fn household_delete_removes_receipt_files_and_records() {
    let harness = Harness::new().await;
    let (a, b) = seed_two_households(&harness.pool).await;

    let first = upload_png(&harness, &a, HH_A).await;
    let second = upload_png(&harness, &a, HH_A).await;
    let other = upload_png(&harness, &b, HH_B).await;
    let a_files = [
        stored_file(&harness, &first).await,
        stored_file(&harness, &second).await,
    ];
    let b_file = stored_file(&harness, &other).await;
    assert!(a_files.iter().all(|path| path.exists()));

    let resp = delete_household(&harness, &a, HH_A, HH_A_NAME).await;
    assert_eq!(resp.status(), StatusCode::OK);

    for path in &a_files {
        assert!(!path.exists(), "household A's image must be gone from disk");
    }
    assert_eq!(count_receipts(&harness.pool, HH_A).await, 0);
    // The other household's image survives, file and record.
    assert!(b_file.exists(), "household B's image must survive");
    assert_eq!(count_receipts(&harness.pool, HH_B).await, 1);
}

/// A receipt whose file was removed by hand still serves a 404 rather than
/// failing the request, and its record is still deletable through the API.
#[tokio::test]
async fn a_missing_file_reads_as_not_found_and_is_still_deletable() {
    let harness = Harness::new().await;
    let (a, _b) = seed_two_households(&harness.pool).await;
    let id = upload_png(&harness, &a, HH_A).await;

    std::fs::remove_file(stored_file(&harness, &id).await).expect("remove by hand");

    assert_eq!(
        download(&harness, &a, HH_A, &id).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        delete_receipt(&harness, &a, HH_A, &id).await.status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(count_receipts(&harness.pool, HH_A).await, 0);
}

/// Nothing under the receipt directory is reachable without authentication: the
/// API has no route that maps a stored file, and the SPA fallback 404s for a
/// missing index.
#[tokio::test]
async fn stored_files_are_not_served_from_a_static_path() {
    let harness = Harness::new().await;
    let (a, _b) = seed_two_households(&harness.pool).await;
    let id = upload_png(&harness, &a, HH_A).await;
    let key = stored_file(&harness, &id)
        .await
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();

    for path in [
        format!("/receipts/{key}"),
        format!("/api/receipts/{key}"),
        format!("/{key}"),
    ] {
        let resp = harness
            .router()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(path.clone())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // The static fallback has no built frontend in tests, so an unknown path
        // is a 404 (never a 200 with the image).
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{path}");
    }
    assert!(Path::new(&harness.dir).is_dir());
}
