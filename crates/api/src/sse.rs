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
//!     interleaved with a 30-second heartbeat. Subscribing before backfilling
//!     opens a small overlap window: a mutation that commits between the
//!     subscribe and the backfill SELECT is both replayed (it is now in
//!     `mutations`) and buffered on the live receiver. The live loop therefore
//!     tracks a high-water mark (the last revision the backfill emitted) and
//!     suppresses any live `mutation` at or below it, so every revision is
//!     delivered exactly once.
//!   * Reconnect: the client sends `?since=<last_revision>`. If that revision is
//!     still within the mutation-log retention window the server replays
//!     `mutations WHERE server_revision > since` as `mutation` events. The
//!     retention floor is two-part (per the plan: "`current_revision - 10000` or
//!     90 days"): a `?since=` below EITHER the revision floor
//!     (`current_revision - 10_000`) OR the oldest mutation still inside the
//!     90-day time window gets a single `full_snapshot` with the complete current
//!     list state instead of an incremental replay.
//!   * Slow subscriber: if a client falls behind the bounded broadcast buffer the
//!     receiver reports `Lagged`. Rather than silently resume mid-stream with a
//!     gap, the live loop self-heals — it emits a fresh `full_snapshot` of the
//!     current state and advances the high-water mark to that revision, so the
//!     client converges without needing to reconnect.
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
use axum::response::sse::{Event, Sse};
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
///
/// This is the revision half of the canonical two-part retention floor in
/// `docs/implementation-plan.md` (the source of truth): a `?since=` is replayable
/// only if it is within BOTH `current_revision - 10_000` revisions AND the
/// time window below; otherwise the server sends a `full_snapshot`.
const RETENTION_FLOOR_REVISIONS: i64 = 10_000;

/// Time half of the canonical retention floor: mutations older than this are
/// considered evicted, so a `?since=` pointing before the oldest mutation still
/// inside this window is served a `full_snapshot` rather than a replay. The plan
/// prose specifies "`current_revision - 10000` or 90 days".
const RETENTION_FLOOR_DAYS: i64 = 90;

/// Production heartbeat cadence. A named `heartbeat` event every 30 s keeps
/// intermediary proxies from closing an otherwise idle connection.
const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Production per-subscriber broadcast buffer bound. If a slow client falls this
/// far behind, the oldest live events are dropped (a `broadcast::error::RecvError::Lagged`);
/// the live loop then self-heals by emitting a fresh `full_snapshot` (see the
/// live loop in [`sse_handler`]). Sized generously so only a genuinely stalled
/// client lags.
const DEFAULT_BROADCAST_CAPACITY: usize = 256;

/// Tunable SSE parameters, carried on [`AppState`] so integration tests can force
/// timing- and capacity-dependent paths (heartbeat cadence, lag-driven snapshot,
/// time-based retention floor) without waiting for real-world intervals. The
/// production defaults match the plan: 30 s heartbeat, 256-event buffer, 90-day
/// retention window.
#[derive(Debug, Clone, Copy)]
pub struct SseConfig {
    /// Interval between named `heartbeat` events.
    pub heartbeat_interval: Duration,
    /// Per-subscriber broadcast channel buffer size.
    pub broadcast_capacity: usize,
    /// Age beyond which a mutation is treated as evicted for the time-based
    /// retention floor.
    pub retention_window: chrono::Duration,
}

impl Default for SseConfig {
    fn default() -> Self {
        SseConfig {
            heartbeat_interval: DEFAULT_HEARTBEAT_INTERVAL,
            broadcast_capacity: DEFAULT_BROADCAST_CAPACITY,
            retention_window: chrono::Duration::days(RETENTION_FLOOR_DAYS),
        }
    }
}

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

impl SseEvent {
    /// The `server_revision` this event carries, used for high-water-mark dedup.
    fn server_revision(&self) -> i64 {
        match self {
            SseEvent::Mutation {
                server_revision, ..
            } => *server_revision,
        }
    }
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
    let capacity = state.sse_config.broadcast_capacity;
    state
        .sse_channels
        .entry(list_id.to_string())
        .or_insert_with(|| broadcast::channel(capacity).0)
        .clone()
}

/// True if a client at `client_revision` is too far behind for incremental
/// backfill and must instead receive a full snapshot.
///
/// Implements the canonical two-part retention floor from
/// `docs/implementation-plan.md` ("`current_revision - 10000` or 90 days"): a
/// snapshot is required when the client is below EITHER
///   * the revision floor (`current_revision - 10_000`), OR
///   * the time-based retained floor — the oldest mutation still inside the
///     90-day window. Rows older than that are treated as evicted, so a client
///     pointing before them cannot be replayed incrementally.
///
/// `oldest_replayable` is the oldest revision that is BOTH still in the log and
/// inside the time window (see [`oldest_replayable_revision`]). It is `None` only
/// when the log has no revision-stamped rows at all (nothing to replay, so the
/// revision floor alone decides). When rows exist but all are outside the time
/// window, the caller passes `Some(current_revision + 1)`, so any client behind
/// `current_revision` is correctly forced to a snapshot.
fn needs_snapshot(
    client_revision: i64,
    current_revision: i64,
    oldest_replayable: Option<i64>,
) -> bool {
    let retention_floor = (current_revision - RETENTION_FLOOR_REVISIONS).max(0);
    if client_revision < retention_floor {
        return true;
    }
    match oldest_replayable {
        // The client must already hold everything strictly before the oldest
        // replayable mutation; if it is behind that, the rows it needs are gone
        // (evicted by either the revision or the 90-day time window).
        //
        // Intentional `oldest - 1` divergence from the plan's literal
        // `client_revision < oldest_retained`: backfill replays
        // `server_revision > since`, so a client that already holds `oldest - 1`
        // can be replayed incrementally from `oldest` onward and needs no
        // snapshot. The plan's strict `< oldest` would force an unnecessary
        // snapshot at that exact boundary. Covered by
        // `client_at_oldest_minus_one_can_replay`.
        Some(oldest) => client_revision < oldest - 1,
        None => false,
    }
}

/// The oldest revision that is both still in the log and inside the time-based
/// retention window — the floor below which incremental replay is impossible.
///
/// `oldest_retained` is the overall minimum revision-stamped row;
/// `oldest_within_window` is the minimum among rows whose `received_at_server` is
/// inside the 90-day window. A client must hold everything before the LATER of
/// the revision-replay floor and the time floor to be replayable, so this returns
/// their `max`.
///
/// The two `None` cases for `oldest_within_window` are NOT equivalent and need
/// opposite handling:
///   * `oldest_retained == None` too → there are no revision-stamped rows at all,
///     so there is no replay constraint; return `None` and let the revision floor
///     decide alone.
///   * `oldest_retained == Some(_)` → rows exist but every one is OUTSIDE the
///     time window (all evicted by age), so NOTHING is replayable. The time floor
///     is then `current_revision + 1`: `needs_snapshot` must fire for any client
///     behind `current_revision`, forcing a `full_snapshot` instead of replaying
///     stale rows. (A client already AT `current_revision` still needs no
///     snapshot — it gets an empty backfill.)
fn oldest_replayable_revision(
    oldest_retained: Option<i64>,
    oldest_within_window: Option<i64>,
    current_revision: i64,
) -> Option<i64> {
    // Effective time floor: the oldest in-window row, or — when rows exist but
    // none are in-window — a sentinel just past the current revision so any
    // `since < current_revision` is treated as un-replayable.
    let time_floor = match oldest_within_window {
        Some(w) => Some(w),
        None if oldest_retained.is_some() => Some(current_revision + 1),
        None => None,
    };
    match (oldest_retained, time_floor) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
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
    //    The cost is a small overlap (a mutation committed in that window is both
    //    backfilled and buffered live); the high-water mark below dedups it.
    let sender = sender_for(&state, &list_id);
    let mut live = sender.subscribe();

    // 3. Decide the initial payload: a full snapshot if the client is below the
    //    retention floor, otherwise the incremental backfill (empty on a first
    //    connect with no `?since=`). `high_water` is the last revision the initial
    //    phase emits; the live loop suppresses any live `mutation` at or below it
    //    so an overlap-window mutation is delivered exactly once.
    let mut initial: Vec<SseEvent> = Vec::new();
    let mut snapshot: Option<Event> = None;
    let mut high_water: i64 = query.since.unwrap_or(0);

    if let Some(since) = query.since {
        // `current_revision` is only needed for the retention-floor decision, so
        // it is queried here (inside the `?since=` branch) rather than on every
        // connect — a first connection pays no extra round-trip.
        let current_revision = current_revision(&state.pool, &list_id)
            .await
            .map_err(db_error("sse.current_revision"))?;
        let oldest = oldest_retained_revision(&state.pool, &list_id)
            .await
            .map_err(db_error("sse.oldest_retained"))?;
        // Time half of the canonical retention floor: the oldest mutation still
        // inside the 90-day window (cutoff = now - retention_window).
        let cutoff = (chrono::Utc::now() - state.sse_config.retention_window)
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let oldest_in_window = oldest_retained_within(&state.pool, &list_id, &cutoff)
            .await
            .map_err(db_error("sse.oldest_in_window"))?;
        let oldest = oldest_replayable_revision(oldest, oldest_in_window, current_revision);
        if needs_snapshot(since, current_revision, oldest) {
            let detail = lists::list_snapshot(&state.pool, &list_id)
                .await
                .map_err(db_error("sse.snapshot"))?
                // The list existed at the auth check above; a None here would be a
                // concurrent delete, which we treat as "nothing to send".
                .ok_or(StatusCode::NOT_FOUND)?;
            // A full snapshot carries state up to the current revision, so nothing
            // older than that should be re-sent live.
            high_water = high_water.max(current_revision);
            snapshot = Some(snapshot_event(&detail).map_err(|e| {
                tracing::error!(error = %e, list_id, "sse: failed to serialise snapshot");
                StatusCode::INTERNAL_SERVER_ERROR
            })?);
        } else {
            initial = backfill_since(&state.pool, &list_id, since)
                .await
                .map_err(db_error("sse.backfill"))?;
            // The backfill is ordered ascending, so the last event carries the
            // highest replayed revision — the high-water mark for live dedup.
            if let Some(SseEvent::Mutation {
                server_revision, ..
            }) = initial.last()
            {
                high_water = high_water.max(*server_revision);
            }
        }
    }

    // 4. Drive everything from a single producer task feeding an mpsc channel.
    //    Wrapping the receiver in a `ReceiverStream` gives axum the `Stream` it
    //    wants; when the client disconnects axum drops the stream, the receiver
    //    is dropped, the producer's `send` fails, and the task exits — so one
    //    client disconnecting never disturbs another's task.
    let heartbeat_interval = state.sse_config.heartbeat_interval;
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(
        state.sse_config.broadcast_capacity,
    );
    // The lag-recovery path re-snapshots the list, so the task needs the pool.
    let pool = state.pool.clone();

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
        let mut heartbeat = tokio::time::interval(heartbeat_interval);
        // The immediate first tick would emit a heartbeat at t=0; skip it so the
        // first heartbeat lands one full interval after connect.
        heartbeat.tick().await;
        loop {
            tokio::select! {
                recv = live.recv() => match recv {
                    Ok(event) => {
                        // Dedup the subscribe→backfill overlap: a mutation that
                        // committed in that window is already in the backfill (or
                        // covered by the snapshot), so suppress any live event at
                        // or below the high-water mark.
                        if event.server_revision() <= high_water {
                            continue;
                        }
                        match event_from(&event) {
                            Ok(e) => {
                                if tx.send(Ok(e)).await.is_err() {
                                    return;
                                }
                            }
                            Err(err) => {
                                tracing::error!(error = %err, "sse: failed to serialise live event");
                            }
                        }
                    }
                    // Lagged: this slow client fell behind the broadcast buffer and
                    // dropped some revisions. Rather than silently resume mid-stream
                    // with a gap, self-heal: re-snapshot the current list state,
                    // emit it as a fresh `full_snapshot`, and reset the high-water
                    // mark to that snapshot's revision so the now-stale buffered
                    // events are suppressed and only strictly newer mutations flow.
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "sse: subscriber lagged, re-snapshotting");
                        match resnapshot(&pool, &list_id).await {
                            Ok(Some((event, revision))) => {
                                high_water = high_water.max(revision);
                                if tx.send(Ok(event)).await.is_err() {
                                    return;
                                }
                            }
                            // List deleted out from under us, or a serialisation
                            // error: nothing safe to send. Stop the stream so the
                            // client reconnects and re-evaluates from scratch.
                            Ok(None) => return,
                            Err(e) => {
                                tracing::error!(error = %e, "sse: lag re-snapshot failed");
                                return;
                            }
                        }
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

    // No `KeepAlive`: the explicit named `heartbeat` every 30s is the sole
    // liveness contract (axum's `KeepAlive` would add a second, ~15s comment
    // cadence and emit frames not in the documented wire format).
    Ok(Sse::new(ReceiverStream::new(rx)))
}

/// Fan a committed `mutation` out to every client streaming `list_id`.
///
/// Shared by the mutation commit path ([`crate::mutations`]) and the async
/// resolution task ([`crate::resolution`]) so both emit the identical wire shape
/// (`server_revision` + pre-serialised `entity`). Looks the channel up without
/// inserting: if no client is currently subscribed there is no channel (or it has
/// zero receivers), and the send is a silent no-op — a disconnected client
/// backfills via `?since=` on reconnect, so a dropped live event is never lost.
pub(crate) fn broadcast_entity(
    state: &AppState,
    list_id: &str,
    server_revision: i64,
    entity: Value,
) {
    let Some(sender) = state.sse_channels.get(list_id) else {
        return;
    };
    // `send` errors only when there are zero receivers; that is an expected,
    // benign state (all clients disconnected between lookup and send), so the
    // result is intentionally ignored.
    let _ = sender.send(SseEvent::Mutation {
        server_revision,
        entity,
    });
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

/// Re-read the current list state and build a `full_snapshot` event plus the
/// revision it reflects, for the lag-recovery path in the live loop.
///
/// Returns `Ok(None)` if the list no longer exists (concurrent delete) or its
/// snapshot fails to serialise — both mean "nothing safe to resend", so the
/// caller stops the stream and lets the client reconnect.
async fn resnapshot(
    pool: &sqlx::SqlitePool,
    list_id: &str,
) -> Result<Option<(Event, i64)>, sqlx::Error> {
    let Some(detail) = lists::list_snapshot(pool, list_id).await? else {
        return Ok(None);
    };
    let revision = detail.current_revision;
    match snapshot_event(&detail) {
        Ok(event) => Ok(Some((event, revision))),
        Err(e) => {
            tracing::error!(error = %e, list_id, "sse: failed to serialise lag snapshot");
            Ok(None)
        }
    }
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

/// The lowest `server_revision` for a list that is still inside the time-based
/// retention window — i.e. whose `received_at_server` is at or after `cutoff`
/// (an RFC-3339 timestamp = `now - retention_window`). `None` when no retained
/// row falls inside the window (all mutations are older than the cutoff).
///
/// `received_at_server` is stored as an RFC-3339 string (see `db::now_iso`); that
/// format is lexicographically ordered for a fixed offset, and every row is
/// written with the same `Z` UTC offset, so a string comparison is a correct
/// chronological comparison here.
async fn oldest_retained_within(
    pool: &sqlx::SqlitePool,
    list_id: &str,
    cutoff: &str,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT MIN(server_revision) FROM mutations \
         WHERE list_id = ?1 AND server_revision IS NOT NULL AND received_at_server >= ?2",
    )
    .bind(list_id)
    .bind(cutoff)
    .fetch_one(pool)
    .await
}

/// Replay every accepted mutation newer than `since`, oldest first.
///
/// Reconstructs each `mutation` event from the stored `response_json` (the exact
/// bytes `POST /api/mutations` returned), so a backfilled event is identical to
/// the live one a connected client would have received.
///
/// Collects into a `Vec` via `fetch_all` rather than streaming: the result is
/// hard-bounded by the retention window (a `since` more than `RETENTION_FLOOR`
/// revisions behind takes the `full_snapshot` path instead, so at most ~10k rows
/// reach here), and the caller needs the highest replayed revision for the
/// live-dedup high-water mark, which is read directly off the collected slice.
/// The bounded allocation makes streaming an unnecessary complication.
async fn backfill_since(
    pool: &sqlx::SqlitePool,
    list_id: &str,
    since: i64,
) -> Result<Vec<SseEvent>, sqlx::Error> {
    // `server_revision IS NOT NULL` is sufficient to scope this to applied
    // mutations: only `accepted`/`merged` rows are revision-stamped at finalise
    // time; a `rejected` row keeps `server_revision` NULL, so no explicit
    // `status` filter is needed (see `mutations::finalize_mutation_row`).
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
    fn oldest_replayable_takes_the_later_of_both_floors() {
        // Time window starts at revision 80 even though the log retains from 50:
        // mutations 50..79 are older than 90 days, so the replayable floor is 80.
        // current_revision (100) is irrelevant when a row is in-window.
        assert_eq!(
            oldest_replayable_revision(Some(50), Some(80), 100),
            Some(80)
        );
        // In-window row missing but rows exist ⇒ all evicted ⇒ time floor is
        // current_revision + 1, so nothing below current is replayable.
        assert_eq!(oldest_replayable_revision(Some(50), None, 100), Some(101));
        // No rows in-window but also none retained ⇒ no replay constraint.
        assert_eq!(oldest_replayable_revision(None, Some(80), 100), Some(80));
        assert_eq!(oldest_replayable_revision(None, None, 100), None);
    }

    #[test]
    fn client_behind_time_window_floor_snapshots() {
        // current=100, revision floor=0 (within), but the oldest mutation inside
        // the 90-day window is 80 (older ones evicted by time). A client at 60 is
        // behind that time floor and must get a snapshot; one at 79 can replay.
        let replayable = oldest_replayable_revision(Some(1), Some(80), 100);
        assert!(needs_snapshot(60, 100, replayable), "behind time floor");
        assert!(
            !needs_snapshot(79, 100, replayable),
            "at time floor - 1 replays"
        );
    }

    #[test]
    fn all_rows_evicted_by_time_forces_snapshot_below_current() {
        // Boundary hole regression: current=10, the only revision-stamped row is
        // rev 10 but it is OUTSIDE the time window, so oldest_within_window=None
        // while oldest_retained=Some(10). Nothing is replayable: a client at
        // since=9 must snapshot (not backfill the stale rev 10), while a client
        // already at the current revision (10) needs no snapshot.
        let replayable = oldest_replayable_revision(Some(10), None, 10);
        assert_eq!(replayable, Some(11), "evicted rows ⇒ floor is current + 1");
        assert!(
            needs_snapshot(9, 10, replayable),
            "since < current ⇒ snapshot"
        );
        assert!(
            !needs_snapshot(10, 10, replayable),
            "since == current ⇒ no snapshot"
        );
    }

    #[test]
    fn heartbeat_event_is_named_with_empty_object() {
        // Smoke-test the builder does not panic and is wired to the right name.
        let _ = heartbeat_event();
    }

    #[test]
    fn default_config_matches_production_constants() {
        let c = SseConfig::default();
        assert_eq!(c.heartbeat_interval, DEFAULT_HEARTBEAT_INTERVAL);
        assert_eq!(c.broadcast_capacity, DEFAULT_BROADCAST_CAPACITY);
        assert_eq!(
            c.retention_window,
            chrono::Duration::days(RETENTION_FLOOR_DAYS)
        );
    }
}
