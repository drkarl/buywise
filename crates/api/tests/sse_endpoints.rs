//! Integration tests for the P1-T5 SSE stream (`GET /api/sse/{list_id}`, issue #16).
//!
//! Unlike the other api integration tests (which drive the router via `oneshot`),
//! SSE responses never complete — the body is an open `text/event-stream`. These
//! tests therefore bind a real TCP server backed by a SHARED-CACHE in-memory
//! SQLite database (so the served app and the test's direct seeding/inserts see
//! the same rows) and read the streaming response incrementally with `reqwest`,
//! parsing SSE frames out of the byte stream under short timeouts.
//!
//! Coverage (all acceptance criteria + testing expectations):
//!   * two clients on the same list both receive a `mutation` event after a POST;
//!   * a client reconnecting with `?since=2` against 5 stored mutations receives
//!     exactly the 3 with `server_revision > 2`, as `mutation` events;
//!   * a client whose `?since=` is below the retention floor receives a single
//!     `full_snapshot` event instead of a replay;
//!   * disconnecting one client leaves the other's stream working.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use buywise_api::{app, AppState};
use buywise_core::db;
use futures_util::StreamExt;
use sqlx::SqlitePool;

const PASSWORD: &str = "correct horse battery staple";

/// A parsed SSE frame: its `event:` name and `data:` payload.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Frame {
    event: String,
    data: String,
}

/// Incrementally accumulates bytes from a streaming response and yields complete
/// SSE frames (blocks separated by a blank line).
struct SseReader {
    stream:
        std::pin::Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
    buf: String,
}

impl SseReader {
    fn new(resp: reqwest::Response) -> Self {
        SseReader {
            stream: Box::pin(resp.bytes_stream()),
            buf: String::new(),
        }
    }

    /// Read the next complete frame, or `None` if the connection closed. Bounded
    /// by `timeout` so a missing event fails the test rather than hanging.
    async fn next_frame(&mut self, timeout: Duration) -> Option<Frame> {
        loop {
            if let Some(frame) = self.take_frame() {
                return Some(frame);
            }
            let chunk = match tokio::time::timeout(timeout, self.stream.next()).await {
                Ok(Some(Ok(bytes))) => bytes,
                // Stream ended or errored, or we timed out waiting for more bytes.
                _ => return self.take_frame(),
            };
            self.buf.push_str(&String::from_utf8_lossy(&chunk));
        }
    }

    /// Pull one complete frame out of the buffer if a blank-line terminator is
    /// present. Skips axum keep-alive comment frames (lines beginning with `:`).
    fn take_frame(&mut self) -> Option<Frame> {
        loop {
            let end = self.buf.find("\n\n")?;
            let block: String = self.buf.drain(..end + 2).collect();
            let mut event = String::new();
            let mut data = String::new();
            let mut is_comment = true;
            for line in block.lines() {
                if line.is_empty() {
                    continue;
                }
                is_comment = false;
                if let Some(rest) = line.strip_prefix("event:") {
                    event = rest.trim().to_string();
                } else if let Some(rest) = line.strip_prefix("data:") {
                    data = rest.trim().to_string();
                } else if line.starts_with(':') {
                    is_comment = true;
                }
            }
            if is_comment {
                // A keep-alive comment block; skip it and look for the next frame.
                continue;
            }
            return Some(Frame { event, data });
        }
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A shared-cache in-memory pool so the spawned server and the test share rows.
async fn setup_shared_pool() -> SqlitePool {
    let name = format!("buywise_sse_{}", uuid::Uuid::new_v4().simple());
    let url = format!("file:{name}?mode=memory&cache=shared");
    let pool = db::connect(&url).await.expect("connect shared");
    db::run_migrations(&pool).await.expect("migrate");
    pool
}

async fn seed_household(pool: &SqlitePool, id: &str, name: &str) {
    sqlx::query("INSERT INTO households (id, name, created_at) VALUES (?1, ?2, ?3)")
        .bind(id)
        .bind(name)
        .bind(db::now_iso())
        .execute(pool)
        .await
        .expect("insert household");
}

async fn seed_user(pool: &SqlitePool, id: &str, email: &str, household_id: &str) {
    let hash = buywise_api::auth::hash_password_for_storage(PASSWORD).expect("hash");
    sqlx::query(
        "INSERT INTO users (id, email, password_hash, display_name, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(id)
    .bind(email)
    .bind(&hash)
    .bind(id)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert user");

    sqlx::query(
        "INSERT INTO household_members (household_id, user_id, role, joined_at) \
         VALUES (?1, ?2, 'owner', ?3)",
    )
    .bind(household_id)
    .bind(id)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert membership");
}

async fn seed_list(pool: &SqlitePool, id: &str, household_id: &str, current_revision: i64) {
    sqlx::query(
        "INSERT INTO shopping_lists \
             (id, household_id, name, status, current_revision, created_at) \
         VALUES (?1, ?2, 'A list', 'active', ?3, ?4)",
    )
    .bind(id)
    .bind(household_id)
    .bind(current_revision)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert list");
}

/// Insert a fully-accepted `mutations` row with a stored `response_json` so the
/// backfill path can replay it. The response shape mirrors `MutationResponse`.
async fn seed_mutation(pool: &SqlitePool, list_id: &str, server_revision: i64) {
    let entity = serde_json::json!({
        "id": format!("entry-{server_revision}"),
        "list_id": list_id,
        "status": "active",
        "display_name": format!("Item {server_revision}"),
    });
    let response = serde_json::json!({
        "client_mutation_id": format!("cm-{server_revision}"),
        "status": "accepted",
        "server_revision": server_revision,
        "entity": entity,
    });
    sqlx::query(
        "INSERT INTO mutations \
             (id, client_mutation_id, device_id, user_id, list_id, entity_type, entity_id, \
              operation_type, payload, base_revision, created_at_client, received_at_server, \
              server_revision, status, response_json) \
         VALUES (?1, ?2, 'device-1', 'user-a', ?3, 'list_entry', ?4, 'add_item', '{}', 0, \
                 ?5, ?5, ?6, 'accepted', ?7)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(format!("cm-{server_revision}"))
    .bind(list_id)
    .bind(format!("entry-{server_revision}"))
    .bind(db::now_iso())
    .bind(server_revision)
    .bind(response.to_string())
    .execute(pool)
    .await
    .expect("insert mutation");
}

/// Spawn the real router on an ephemeral TCP port; returns its base URL.
async fn spawn_server(pool: SqlitePool) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr: SocketAddr = listener.local_addr().expect("local_addr");
    let app = app(AppState::new(pool));
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    format!("http://{addr}")
}

/// A reqwest client preconfigured with the session + CSRF cookies for a user.
struct Client {
    http: reqwest::Client,
    base: String,
    cookies: String,
}

impl Client {
    /// Log in over HTTP and capture the `session` + `csrf` cookies.
    async fn login(base: &str, email: &str) -> Client {
        let http = reqwest::Client::builder().build().expect("build client");
        let resp = http
            .post(format!("{base}/api/auth/login"))
            .json(&serde_json::json!({ "email": email, "password": PASSWORD }))
            .send()
            .await
            .expect("login send");
        assert_eq!(resp.status(), reqwest::StatusCode::OK, "login ok");

        let mut jar: HashMap<String, String> = HashMap::new();
        for value in resp.headers().get_all(reqwest::header::SET_COOKIE) {
            let s = value.to_str().unwrap();
            let first = s.split(';').next().unwrap();
            if let Some((k, v)) = first.split_once('=') {
                jar.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
        let session = jar.get("session").expect("session cookie").clone();
        let csrf = jar.get("csrf").expect("csrf cookie").clone();
        Client {
            http,
            base: base.to_string(),
            cookies: format!("session={session}; csrf={csrf}"),
        }
    }

    /// Open an SSE stream for a list (optionally with `?since=`).
    async fn open_sse(&self, list_id: &str, since: Option<i64>) -> reqwest::Response {
        let mut url = format!("{}/api/sse/{list_id}", self.base);
        if let Some(s) = since {
            url.push_str(&format!("?since={s}"));
        }
        let resp = self
            .http
            .get(url)
            .header(reqwest::header::COOKIE, &self.cookies)
            .send()
            .await
            .expect("sse send");
        assert_eq!(resp.status(), reqwest::StatusCode::OK, "sse 200");
        resp
    }

    /// POST a mutation (CSRF header echoed from the captured csrf cookie).
    async fn post_mutation(&self, body: serde_json::Value) -> reqwest::StatusCode {
        let csrf = self.cookies.split("csrf=").nth(1).unwrap().to_string();
        let resp = self
            .http
            .post(format!("{}/api/mutations", self.base))
            .header(reqwest::header::COOKIE, &self.cookies)
            .header("X-CSRF-Token", csrf)
            .json(&body)
            .send()
            .await
            .expect("mutation send");
        resp.status()
    }
}

fn add_item_envelope(cm: &str, list_id: &str, entry_id: &str) -> serde_json::Value {
    serde_json::json!({
        "client_mutation_id": cm,
        "device_id": "device-1",
        "list_id": list_id,
        "base_revision": 0,
        "entity_type": "list_entry",
        "entity_id": entry_id,
        "operation_type": "add_item",
        "payload": { "raw_text": "Milk", "display_name": "Milk" },
        "created_at_client": "2026-05-31T00:00:00.000Z",
    })
}

const SHORT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Two clients on the same list both receive the `mutation` event after one POST,
/// and disconnecting one does not stop the other from receiving a later event.
#[tokio::test]
async fn two_clients_both_receive_mutation_event() {
    let pool = setup_shared_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a", 0).await;
    let base = spawn_server(pool.clone()).await;

    let client = Client::login(&base, "a@example.test").await;

    let mut a = SseReader::new(client.open_sse("list-a", None).await);
    let mut b = SseReader::new(client.open_sse("list-a", None).await);

    // Give both subscribers a moment to register on the broadcast channel before
    // the mutation commits (the broadcast only reaches current subscribers).
    tokio::time::sleep(Duration::from_millis(200)).await;

    let status = client
        .post_mutation(add_item_envelope("cm-1", "list-a", "entry-1"))
        .await;
    assert_eq!(status, reqwest::StatusCode::CREATED, "mutation accepted");

    let fa = a.next_frame(SHORT).await.expect("client A frame");
    let fb = b.next_frame(SHORT).await.expect("client B frame");
    assert_eq!(fa.event, "mutation", "A got a mutation event");
    assert_eq!(fb.event, "mutation", "B got a mutation event");

    let va: serde_json::Value = serde_json::from_str(&fa.data).expect("A data json");
    assert_eq!(va["server_revision"], 1, "A sees revision 1");
    assert_eq!(va["entity"]["id"], "entry-1", "A sees the entity");

    // Disconnect A by dropping its reader; B must still receive the next event.
    drop(a);
    let status = client
        .post_mutation(add_item_envelope("cm-2", "list-a", "entry-2"))
        .await;
    assert_eq!(
        status,
        reqwest::StatusCode::CREATED,
        "second mutation accepted"
    );

    let fb2 = b.next_frame(SHORT).await.expect("client B second frame");
    assert_eq!(fb2.event, "mutation");
    let vb2: serde_json::Value = serde_json::from_str(&fb2.data).expect("B data json");
    assert_eq!(vb2["server_revision"], 2, "B still streaming after A left");
}

/// Reconnecting with `?since=2` against 5 stored mutations replays exactly the 3
/// with `server_revision > 2`, oldest first, as `mutation` events.
#[tokio::test]
async fn since_backfills_only_newer_mutations() {
    let pool = setup_shared_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a", 5).await;
    for rev in 1..=5 {
        seed_mutation(&pool, "list-a", rev).await;
    }
    let base = spawn_server(pool.clone()).await;
    let client = Client::login(&base, "a@example.test").await;

    let mut r = SseReader::new(client.open_sse("list-a", Some(2)).await);

    let mut revisions = Vec::new();
    for _ in 0..3 {
        let frame = r.next_frame(SHORT).await.expect("backfill frame");
        assert_eq!(frame.event, "mutation", "backfill frames are mutations");
        let v: serde_json::Value = serde_json::from_str(&frame.data).expect("json");
        revisions.push(v["server_revision"].as_i64().expect("revision"));
    }
    assert_eq!(
        revisions,
        vec![3, 4, 5],
        "only server_revision > 2, in order"
    );
}

/// Reconnecting with `?since == current_revision` yields no backfill: the client
/// is already caught up, so the first frame it sees is a later live mutation
/// (revision current + 1), never a replay of an already-held revision.
#[tokio::test]
async fn since_equal_to_current_revision_backfills_nothing() {
    let pool = setup_shared_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a", 3).await;
    for rev in 1..=3 {
        seed_mutation(&pool, "list-a", rev).await;
    }
    let base = spawn_server(pool.clone()).await;
    let client = Client::login(&base, "a@example.test").await;

    // since == current_revision (3): nothing newer to replay.
    let mut r = SseReader::new(client.open_sse("list-a", Some(3)).await);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let status = client
        .post_mutation(add_item_envelope("cm-live", "list-a", "entry-live"))
        .await;
    assert_eq!(
        status,
        reqwest::StatusCode::CREATED,
        "live mutation accepted"
    );

    // The very first frame must be the live revision 4 — proving zero backfill
    // frames for already-held revisions 1..=3 preceded it.
    let frame = r.next_frame(SHORT).await.expect("first frame");
    assert_eq!(frame.event, "mutation");
    let v: serde_json::Value = serde_json::from_str(&frame.data).expect("json");
    assert_eq!(
        v["server_revision"], 4,
        "first frame is the live revision 4"
    );
}

/// Regression for the subscribe→backfill overlap (review finding #1): after a
/// `?since=` backfill, revisions already replayed are never re-emitted on the
/// live channel — the high-water mark suppresses them, so the next live frame is
/// strictly newer than the last backfilled revision.
#[tokio::test]
async fn backfilled_revisions_are_not_re_emitted_live() {
    let pool = setup_shared_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a", 5).await;
    for rev in 1..=5 {
        seed_mutation(&pool, "list-a", rev).await;
    }
    let base = spawn_server(pool.clone()).await;
    let client = Client::login(&base, "a@example.test").await;

    // Backfill 3, 4, 5 (high-water mark = 5).
    let mut r = SseReader::new(client.open_sse("list-a", Some(2)).await);
    for expected in [3, 4, 5] {
        let frame = r.next_frame(SHORT).await.expect("backfill frame");
        let v: serde_json::Value = serde_json::from_str(&frame.data).expect("json");
        assert_eq!(v["server_revision"], expected, "backfill in order");
    }

    // A new live mutation lands at revision 6; the next frame must be exactly 6,
    // never a duplicate of an already-backfilled 3/4/5.
    let status = client
        .post_mutation(add_item_envelope("cm-live", "list-a", "entry-live"))
        .await;
    assert_eq!(
        status,
        reqwest::StatusCode::CREATED,
        "live mutation accepted"
    );

    let frame = r.next_frame(SHORT).await.expect("live frame");
    assert_eq!(frame.event, "mutation");
    let v: serde_json::Value = serde_json::from_str(&frame.data).expect("json");
    assert_eq!(
        v["server_revision"], 6,
        "next live frame is strictly past the high-water mark"
    );
}

/// Reconnecting with a `?since=` below the retention floor yields a single
/// `full_snapshot` event with the complete list state, not an incremental replay.
#[tokio::test]
async fn since_below_retention_floor_yields_full_snapshot() {
    let pool = setup_shared_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    // current_revision far above the floor window so since=5 is below the floor
    // (floor = current - 10_000 = 15_000).
    seed_list(&pool, "list-a", "hh-a", 25_000).await;
    seed_mutation(&pool, "list-a", 20_000).await;
    let base = spawn_server(pool.clone()).await;
    let client = Client::login(&base, "a@example.test").await;

    let mut r = SseReader::new(client.open_sse("list-a", Some(5)).await);

    let frame = r.next_frame(SHORT).await.expect("snapshot frame");
    assert_eq!(frame.event, "full_snapshot", "below floor → full_snapshot");
    let v: serde_json::Value = serde_json::from_str(&frame.data).expect("snapshot json");
    assert_eq!(v["id"], "list-a", "snapshot carries the list state");
    assert_eq!(
        v["current_revision"], 25_000,
        "snapshot has current revision"
    );
}

/// A non-member is forbidden from streaming another household's list (403), and a
/// missing list is 404 — auth mirrors the mutation endpoint.
#[tokio::test]
async fn sse_enforces_authorization() {
    let pool = setup_shared_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_household(&pool, "hh-b", "Household B").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_user(&pool, "user-b", "b@example.test", "hh-b").await;
    seed_list(&pool, "list-a", "hh-a", 0).await;
    let base = spawn_server(pool.clone()).await;

    let outsider = Client::login(&base, "b@example.test").await;

    // Foreign household → 403.
    let resp = outsider
        .http
        .get(format!("{base}/api/sse/list-a"))
        .header(reqwest::header::COOKIE, &outsider.cookies)
        .send()
        .await
        .expect("send");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::FORBIDDEN,
        "foreign list 403"
    );

    // Missing list → 404.
    let resp = outsider
        .http
        .get(format!("{base}/api/sse/does-not-exist"))
        .header(reqwest::header::COOKIE, &outsider.cookies)
        .send()
        .await
        .expect("send");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::NOT_FOUND,
        "missing list 404"
    );

    // Unauthenticated → 401.
    let resp = reqwest::Client::new()
        .get(format!("{base}/api/sse/list-a"))
        .send()
        .await
        .expect("send");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "no session 401"
    );
}
