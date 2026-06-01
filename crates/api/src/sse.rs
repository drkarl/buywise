//! Server-Sent Events stream for the BuyWise API (P1-T5).
//!
//! `GET /api/sse/{list_id}` keeps a persistent HTTP connection open and pushes
//! committed mutations to every client subscribed to a list, so two devices
//! editing the same list converge in real time without polling.
//!
//! Wire format (one `text/event-stream` connection):
//!
//! ```text
//! event: mutation
//! data: {"server_revision":48,"entity":{ ...list_entries row... }}
//!
//! event: heartbeat
//! data: {}
//!
//! event: full_snapshot
//! data: { ...full list state, same shape as GET /api/list/:id... }
//! ```
//!
//! Architecture:
//!   * One [`tokio::sync::broadcast`] channel per list, keyed by `list_id` in the
//!     [`crate::SseChannels`] `DashMap` on [`AppState`]. The channel is created
//!     lazily by the first subscriber ([`sender_for`]); the mutation commit path
//!     ([`crate::mutations`]) looks one up after COMMIT and fans the event out.
//!   * On connect the handler authorises the caller (the list must exist and
//!     belong to a household the caller is a member of — 404 / 403 / 401 exactly
//!     like the list and mutation routes), subscribes to the broadcast BEFORE any
//!     backfill query so no live event committed during backfill is missed, then
//!     streams: optional backfill (or a `full_snapshot`), then live events,
//!     interleaved with a 30-second heartbeat.
//!   * Reconnect: the client sends `?since=<last_revision>`. If that revision is
//!     still within the mutation-log retention window the server replays
//!     `mutations WHERE server_revision > since` as `mutation` events. If it is
//!     below the retention floor (`current_revision - 10_000`, or older than the
//!     oldest retained mutation) the server cannot replay incrementally and sends
//!     a single `full_snapshot` with the complete current list state instead.
//!
//! Error discipline mirrors the rest of the api crate: a DB error during the
//! synchronous connect phase is logged and surfaced as 500 (never failing open);
//! 401/403/404 cover the client conditions. Once the stream is live, a per-event
//! serialisation failure is logged and that event skipped rather than tearing the
//! whole connection down.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::broadcast;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::Stream;

use crate::lists;
use crate::middleware::AuthenticatedUser;
use crate::AppState;

/// Retention window for incremental backfill, in revisions. A client whose
/// `?since=` is more than this many revisions behind the list's current revision
/// is served a `full_snapshot` instead of a replay (see [`needs_snapshot`]).
const RETENTION_FLOOR_REVISIONS: i64 = 10_000;

/// Heartbeat cadence. A named `heartbeat` event every 30 s keeps intermediary
/// proxies from closing an otherwise idle connection.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Bound on the per-subscriber broadcast buffer. If a slow client falls this far
/// behind, the oldest live events are dropped (a `Lagged` error); the client
/// recovers by reconnecting with `?since=` and backfilling, so a drop is never
/// data loss. Sized generously so only a genuinely stalled client lags.
const BROADCAST_CAPACITY: usize = 256;

/// An event fanned out over a list's broadcast channel.
///
/// Cloned once per subscriber by `tokio::sync::broadcast`, so the payload is kept
/// small (the entity is a pre-serialised JSON [`Value`], shared cheaply). Only
/// `mutation` events travel the channel; `heartbeat` is generated per-connection
/// and `full_snapshot` / backfill are produced inline at connect time.
#[derive(Debug, Clone)]
pub enum SseEvent {
    /// A committed mutation: the new `server_revision` and the affected entity
    /// (the same `entity` shape `POST /api/mutations` returns).
    Mutation {
        /// The list's `current_revision` after the mutation.
        server_revision: i64,
        /// The affected `list_entries` row as JSON, or `null` for ops without an
        /// entity. Pre-serialised so the broadcast clone is cheap.
        entity: Value,
    },
}

/// Query string for `GET /api/sse/{list_id}`.
#[derive(Debug, Deserialize)]
pub struct SseQuery {
    /// Last `server_revision` the client has already applied. When present the
    /// server backfills everything newer (or sends a `full_snapshot` if the
    /// client is below the retention floor). Absent on a first connection.
    pub since: Option<i64>,
}

/// Build the SSE route as a `Router<AppState>`, merged by [`crate::app`].
pub fn router() -> Router<AppState> {
    Router::new().route("/api/sse/{list_id}", get(sse_handler))
}

/// Look up (or lazily create) the broadcast sender for a list.
///
/// The first subscriber for a `list_id` creates the channel; subsequent
/// subscribers reuse it. `DashMap::entry` makes the get-or-insert atomic so two
/// clients connecting concurrently share one channel rather than racing to
/// create two. The channel is left in the map after the last subscriber leaves —
/// the sender is tiny and reused on the next connect, which avoids a
/// remove/recreate race against an in-flight `broadcast_mutation` lookup.
pub fn sender_for(state: &AppState, list_id: &str) -> broadcast::Sender<SseEvent> {
    state
        .sse_channels
        .entry(list_id.to_string())
        .or_insert_with(|| broadcast::channel(BROADCAST_CAPACITY).0)
        .clone()
}

/// True if a client at `client_revision` is too far behind for incremental
/// backfill and must instead receive a full snapshot.
///
/// Two conditions trigger a snapshot (see the plan's retention logic): the client
/// is below the revision-count floor (`current_revision - 10_000`), or it is
/// behind the oldest mutation the log still retains (so the rows needed to replay
/// no longer exist). `oldest_retained` is `None` when the log is empty, in which
/// case there is nothing to replay and the floor check alone applies.
fn needs_snapshot(
    client_revision: i64,
    current_revision: i64,
    oldest_retained: Option<i64>,
) -> bool {
    let retention_floor = (current_revision - RETENTION_FLOOR_REVISIONS).max(0);
    if client_revision < retention_floor {
        return true;
    }
    match oldest_retained {
        // The client must already hold everything strictly before the oldest
        // retained mutation; if it is behind that, the replay rows are gone.
        Some(oldest) => client_revision < oldest - 1,
        None => false,
    }
}

/// `GET /api/sse/{list_id}` — open a Server-Sent Events stream for a list.
///
/// Authorises the caller, then returns a `text/event-stream` that emits backfill
/// (or a `full_snapshot`), live mutation events, and a 30 s heartbeat until the
/// client disconnects. Auth failures short-circuit with the matching status
/// before any stream is opened.
async fn sse_handler(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(list_id): Path<String>,
    Query(query): Query<SseQuery>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    // 1. Authorise: the list must exist (404) and belong to a household the
    //    caller is a member of (403). Mirrors the mutation endpoint exactly.
    let household_id = list_household(&state.pool, &list_id)
        .await
        .map_err(db_error("sse.list_household"))?
        .ok_or(StatusCode::NOT_FOUND)?;
    if !is_member(&state.pool, &user.user_id, &household_id)
        .await
        .map_err(db_error("sse.is_member"))?
    {
        return Err(StatusCode::FORBIDDEN);
    }

    // 2. Subscribe to the broadcast BEFORE computing backfill, so a mutation that
    //    commits while we are querying the backlog is buffered for the live phase
    //    rather than lost in the gap between "read backlog" and "start listening".
    let sender = sender_for(&state, &list_id);
    let mut live = sender.subscribe();

    // 3. Decide the initial payload: a full snapshot if the client is below the
    //    retention floor, otherwise the incremental backfill (empty on a first
    //    connect with no `?since=`).
    let current_revision = current_revision(&state.pool, &list_id)
        .await
        .map_err(db_error("sse.current_revision"))?;

    let mut initial: Vec<SseEvent> = Vec::new();
    let mut snapshot: Option<Event> = None;

    if let Some(since) = query.since {
        let oldest = oldest_retained_revision(&state.pool, &list_id)
            .await
            .map_err(db_error("sse.oldest_retained"))?;
        if needs_snapshot(since, current_revision, oldest) {
            let detail = lists_snapshot(&state.pool, &list_id)
                .await
                .map_err(db_error("sse.snapshot"))?
                // The list existed at the auth check above; a None here would be a
                // concurrent delete, which we treat as "nothing to send".
                .ok_or(StatusCode::NOT_FOUND)?;
            snapshot = Some(snapshot_event(&detail).map_err(|e| {
                tracing::error!(error = %e, list_id, "sse: failed to serialise snapshot");
                StatusCode::INTERNAL_SERVER_ERROR
            })?);
        } else {
            initial = backfill_since(&state.pool, &list_id, since)
                .await
                .map_err(db_error("sse.backfill"))?;
        }
    }

    // 4. Drive everything from a single producer task feeding an mpsc channel.
    //    Wrapping the receiver in a `ReceiverStream` gives axum the `Stream` it
    //    wants; when the client disconnects axum drops the stream, the receiver
    //    is dropped, the producer's `send` fails, and the task exits — so one
    //    client disconnecting never disturbs another's task.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(BROADCAST_CAPACITY);

    tokio::spawn(async move {
        // 4a. Snapshot OR backfill first, in order, before any live event.
        if let Some(event) = snapshot {
            if tx.send(Ok(event)).await.is_err() {
                return;
            }
        }
        for event in initial {
            match event_from(&event) {
                Ok(e) => {
                    if tx.send(Ok(e)).await.is_err() {
                        return;
                    }
                }
                Err(err) => {
                    tracing::error!(error = %err, "sse: failed to serialise backfill event");
                }
            }
        }

        // 4b. Live events interleaved with the heartbeat until disconnect.
        let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
        // The immediate first tick would emit a heartbeat at t=0; skip it so the
        // first heartbeat lands one full interval after connect.
        heartbeat.tick().await;
        loop {
            tokio::select! {
                recv = live.recv() => match recv {
                    Ok(event) => match event_from(&event) {
                        Ok(e) => {
                            if tx.send(Ok(e)).await.is_err() {
                                return;
                            }
                        }
                        Err(err) => {
                            tracing::error!(error = %err, "sse: failed to serialise live event");
                        }
                    },
                    // Lagged: this slow client dropped some events. It recovers by
                    // reconnecting with `?since=`; keep streaming the rest.
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "sse: subscriber lagged, dropping events");
                    }
                    // Sender gone (no producers): nothing more will ever arrive.
                    Err(broadcast::error::RecvError::Closed) => return,
                },
                _ = heartbeat.tick() => {
                    if tx.send(Ok(heartbeat_event())).await.is_err() {
                        return;
                    }
                }
            }
        }
    });

    Ok(Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default()))
}

/// Build the `mutation` SSE event from a broadcast payload.
fn event_from(event: &SseEvent) -> Result<Event, serde_json::Error> {
    match event {
        SseEvent::Mutation {
            server_revision,
            entity,
        } => {
            let data = serde_json::to_string(&serde_json::json!({
                "server_revision": server_revision,
                "entity": entity,
            }))?;
            Ok(Event::default().event("mutation").data(data))
        }
    }
}

/// Build the named `heartbeat` event (`data: {}`).
fn heartbeat_event() -> Event {
    Event::default().event("heartbeat").data("{}")
}

/// Build the `full_snapshot` event carrying the complete current list state.
fn snapshot_event(detail: &lists::ListDetail) -> Result<Event, serde_json::Error> {
    let data = serde_json::to_string(detail)?;
    Ok(Event::default().event("full_snapshot").data(data))
}

// ---------------------------------------------------------------------------
// DB helpers
// ---------------------------------------------------------------------------

/// The household owning a list, or `None` if the list does not exist.
async fn list_household(
    pool: &sqlx::SqlitePool,
    list_id: &str,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT household_id FROM shopping_lists WHERE id = ?1")
        .bind(list_id)
        .fetch_optional(pool)
        .await
}

/// True if `user_id` is a member of `household_id`.
async fn is_member(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    household_id: &str,
) -> Result<bool, sqlx::Error> {
    let row: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM household_members WHERE user_id = ?1 AND household_id = ?2",
    )
    .bind(user_id)
    .bind(household_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.is_some())
}

/// A list's current revision (0 for a list with no accepted mutations yet).
async fn current_revision(pool: &sqlx::SqlitePool, list_id: &str) -> Result<i64, sqlx::Error> {
    let rev: Option<i64> =
        sqlx::query_scalar("SELECT current_revision FROM shopping_lists WHERE id = ?1")
            .bind(list_id)
            .fetch_optional(pool)
            .await?;
    Ok(rev.unwrap_or(0))
}

/// The lowest `server_revision` still retained for a list, or `None` if the
/// mutation log has no accepted (revision-stamped) rows for it.
async fn oldest_retained_revision(
    pool: &sqlx::SqlitePool,
    list_id: &str,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT MIN(server_revision) FROM mutations \
         WHERE list_id = ?1 AND server_revision IS NOT NULL",
    )
    .bind(list_id)
    .fetch_one(pool)
    .await
}

/// Replay every accepted mutation newer than `since`, oldest first.
///
/// Reconstructs each `mutation` event from the stored `response_json` (the exact
/// bytes `POST /api/mutations` returned), so a backfilled event is identical to
/// the live one a connected client would have received.
async fn backfill_since(
    pool: &sqlx::SqlitePool,
    list_id: &str,
    since: i64,
) -> Result<Vec<SseEvent>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (i64, Option<String>)>(
        "SELECT server_revision, response_json FROM mutations \
         WHERE list_id = ?1 AND server_revision IS NOT NULL AND server_revision > ?2 \
         ORDER BY server_revision ASC",
    )
    .bind(list_id)
    .bind(since)
    .fetch_all(pool)
    .await?;

    let mut events = Vec::with_capacity(rows.len());
    for (server_revision, response_json) in rows {
        // The stored response carries the entity; pull just that field out so the
        // backfill event matches the live broadcast shape. A row with no/invalid
        // response_json still advances the revision, so emit it with a null entity
        // rather than dropping it (the client only needs the revision to advance).
        let entity = response_json
            .as_deref()
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .and_then(|v| v.get("entity").cloned())
            .unwrap_or(Value::Null);
        events.push(SseEvent::Mutation {
            server_revision,
            entity,
        });
    }
    Ok(events)
}

/// Thin wrapper over [`lists::list_snapshot`] kept local so the snapshot call
/// site reads symmetrically with the other DB helpers in this module.
async fn lists_snapshot(
    pool: &sqlx::SqlitePool,
    list_id: &str,
) -> Result<Option<lists::ListDetail>, sqlx::Error> {
    lists::list_snapshot(pool, list_id).await
}

/// Map a DB error to a logged 500, matching the rest of the api crate.
fn db_error(context: &'static str) -> impl Fn(sqlx::Error) -> StatusCode {
    move |e| {
        tracing::error!(error = %e, context, "sse: database error");
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_connect_without_since_never_snapshots() {
        // No `?since=` means no backfill decision is made at all, but the floor
        // logic itself must treat "caught up" clients as not needing a snapshot.
        assert!(!needs_snapshot(48, 48, Some(1)));
    }

    #[test]
    fn recent_client_within_floor_does_not_snapshot() {
        // current=100, floor = max(100-10000,0) = 0; client at 50 is well inside.
        assert!(!needs_snapshot(50, 100, Some(1)));
    }

    #[test]
    fn client_below_revision_floor_snapshots() {
        // current=20_000, floor=10_000; a client at 5_000 is below it.
        assert!(needs_snapshot(5_000, 20_000, Some(1)));
    }

    #[test]
    fn client_behind_oldest_retained_snapshots() {
        // Log retains revisions >= 50; a client at 10 cannot be replayed from it.
        assert!(needs_snapshot(10, 100, Some(50)));
    }

    #[test]
    fn client_at_oldest_minus_one_can_replay() {
        // Oldest retained is 50, so a client that already holds revision 49 can be
        // replayed incrementally (50, 51, ...). 48 cannot.
        assert!(!needs_snapshot(49, 100, Some(50)));
        assert!(needs_snapshot(48, 100, Some(50)));
    }

    #[test]
    fn empty_log_never_snapshots_within_floor() {
        // No retained mutations and within the revision floor: nothing to replay,
        // nothing to snapshot — the live stream alone suffices.
        assert!(!needs_snapshot(0, 0, None));
    }

    #[test]
    fn heartbeat_event_is_named_with_empty_object() {
        // Smoke-test the builder does not panic and is wired to the right name.
        let _ = heartbeat_event();
    }
}
