//! List-management routes for the BuyWise API (P1-T3).
//!
//! Five endpoints, all behind the [`AuthenticatedUser`] extractor, mounted by
//! [`router`] (see `crate::app` for where the prefixes are nested):
//!
//!   * `GET   /api/lists`     — every ACTIVE list in the user's household(s).
//!   * `POST  /api/lists`     — create a new list (CSRF-protected; it is a POST
//!     behind [`AuthenticatedUser`]).
//!   * `GET   /api/list/:id`  — full list state: list metadata, a top-level
//!     `current_revision`, and all ACTIVE `list_entries`.
//!   * `PATCH /api/list/:id`  — rename or archive a list (CSRF-protected).
//!   * `GET   /api/items`     — the full canonical catalogue (items + aliases +
//!     constraints) for client-side resolution / category assignment.
//!
//! Authorization model (see also `migrations/003_phase1_schema.sql`):
//!   * A user belongs to household(s) via `household_members`. Collection
//!     endpoints (`GET /api/lists`, `POST /api/lists`, `GET /api/items`) are
//!     scoped to the user's household(s).
//!   * For a single list (`GET`/`PATCH /api/list/:id`) the row is looked up
//!     first: a missing list is **404**, an existing list the user is not a
//!     member of the owning household for is **403** (an authorization failure,
//!     deliberately distinguished from "not found"). The same scope applies to
//!     the entries returned with the list.
//!
//! Error discipline mirrors `auth.rs`: a database error is logged via
//! `tracing::error!` and surfaced as 500 (never failing open); 401/403/404/422
//! are returned for the corresponding client conditions.
//!
//! Revision note: `current_revision` is owned by the offline-sync mutation log
//! (P1-T4) and is **not** touched here — `PATCH` only renames/archives.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::middleware::AuthenticatedUser;
use crate::AppState;

/// Build the list-management routes as a `Router<AppState>`.
///
/// `GET /api/lists` and `GET /api/list/:id` are different path shapes, so the
/// `/api/lists`, `/api/list/:id`, and `/api/items` groups are registered
/// individually and merged by the caller (`crate::app`).
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/lists", get(list_lists).post(create_list))
        .route("/api/list/{id}", get(get_list).patch(patch_list))
        .route("/api/items", get(list_items))
}

// ---------------------------------------------------------------------------
// Response / request shapes
// ---------------------------------------------------------------------------

/// One list as returned in the `GET /api/lists` collection.
#[derive(Debug, Serialize)]
pub struct ListSummary {
    /// `shopping_lists.id`.
    pub id: String,
    /// `shopping_lists.household_id`.
    pub household_id: String,
    /// `shopping_lists.name`.
    pub name: String,
    /// `shopping_lists.status` (`active` here — the collection is active-only).
    pub status: String,
    /// `shopping_lists.current_revision`.
    pub current_revision: i64,
    /// `shopping_lists.created_at` (ISO-8601 UTC).
    pub created_at: String,
}

/// A single list entry as returned inside [`ListDetail`].
#[derive(Debug, Serialize)]
pub struct EntryView {
    /// `list_entries.id`.
    pub id: String,
    /// `list_entries.canonical_item_id` (NULL until resolved).
    pub canonical_item_id: Option<String>,
    /// `list_entries.resolution_status` (`unresolved`|`resolved`|`ambiguous`).
    pub resolution_status: String,
    /// `list_entries.resolution_candidates_json` (raw JSON; ambiguous only).
    pub resolution_candidates_json: Option<String>,
    /// `list_entries.raw_text`.
    pub raw_text: String,
    /// `list_entries.display_name`.
    pub display_name: String,
    /// `list_entries.added_by`.
    pub added_by: String,
    /// `list_entries.quantity`.
    pub quantity: i64,
    /// `list_entries.status` (`active` here — detail returns active-only).
    pub status: String,
    /// `list_entries.effective_constraint_hash`.
    pub effective_constraint_hash: String,
    /// `list_entries.last_updated_revision`.
    pub last_updated_revision: i64,
    /// `list_entries.created_at` (ISO-8601 UTC).
    pub created_at: String,
    /// `list_entries.updated_at` (ISO-8601 UTC).
    pub updated_at: String,
}

/// Full state of a single list: metadata, a top-level `current_revision`, and
/// all ACTIVE entries.
#[derive(Debug, Serialize)]
pub struct ListDetail {
    /// `shopping_lists.id`.
    pub id: String,
    /// `shopping_lists.household_id`.
    pub household_id: String,
    /// `shopping_lists.name`.
    pub name: String,
    /// `shopping_lists.status`.
    pub status: String,
    /// `shopping_lists.current_revision`. Top-level per the contract so a client
    /// can read it without descending into the entries.
    pub current_revision: i64,
    /// `shopping_lists.created_at` (ISO-8601 UTC).
    pub created_at: String,
    /// `shopping_lists.archived_at` (set only for archived lists).
    pub archived_at: Option<String>,
    /// All ACTIVE list entries (`status = 'active'`).
    pub entries: Vec<EntryView>,
}

/// Body for `POST /api/lists`.
///
/// `household_id` is required and explicitly validated against the caller's
/// membership (a user may belong to several households, so the target cannot be
/// safely inferred). A non-member household yields 403.
#[derive(Debug, Deserialize)]
pub struct CreateListRequest {
    /// Name for the new list (must be non-empty after trimming).
    pub name: String,
    /// Household the list belongs to; must be one the caller is a member of.
    pub household_id: String,
}

/// Body for `PATCH /api/list/:id`. Both fields are optional; at least one must
/// be present. `status`, when given, must be `active` or `archived`.
#[derive(Debug, Deserialize)]
pub struct PatchListRequest {
    /// New name (non-empty after trimming) when present.
    pub name: Option<String>,
    /// New status (`active` | `archived`) when present.
    pub status: Option<String>,
}

/// One canonical item with its aliases and (optional) constraints, returned by
/// `GET /api/items`.
#[derive(Debug, Serialize)]
pub struct ItemView {
    /// `canonical_items.id`.
    pub id: String,
    /// `canonical_items.household_id`.
    pub household_id: String,
    /// `canonical_items.name_en`.
    pub name_en: String,
    /// `canonical_items.name_es`.
    pub name_es: String,
    /// `canonical_items.category_id`.
    pub category_id: String,
    /// `canonical_items.default_constraint_policy`.
    pub default_constraint_policy: String,
    /// All aliases for this item.
    pub aliases: Vec<AliasView>,
    /// The single constraints row for this item, if any.
    pub constraints: Option<ConstraintsView>,
}

/// One alias for a canonical item.
#[derive(Debug, Serialize)]
pub struct AliasView {
    /// `item_aliases.id`.
    pub id: String,
    /// `item_aliases.alias`.
    pub alias: String,
    /// `item_aliases.language` (`en` | `es`).
    pub language: String,
    /// `item_aliases.source` (`manual` | `accepted_match` | `llm`).
    pub source: String,
}

/// The (at most one) constraints row for a canonical item.
#[derive(Debug, Serialize)]
pub struct ConstraintsView {
    /// `item_constraints.brand`.
    pub brand: Option<String>,
    /// `item_constraints.allowed_brands` (raw stored value).
    pub allowed_brands: Option<String>,
    /// `item_constraints.excluded_brands` (raw stored value).
    pub excluded_brands: Option<String>,
    /// `item_constraints.preferred_size_value`.
    pub preferred_size_value: Option<f64>,
    /// `item_constraints.preferred_size_basis` (`g` | `ml` | `each`).
    pub preferred_size_basis: Option<String>,
    /// `item_constraints.min_size_value`.
    pub min_size_value: Option<f64>,
    /// `item_constraints.min_size_basis`.
    pub min_size_basis: Option<String>,
    /// `item_constraints.max_size_value`.
    pub max_size_value: Option<f64>,
    /// `item_constraints.max_size_basis`.
    pub max_size_basis: Option<String>,
    /// `item_constraints.pack_count`.
    pub pack_count: Option<i64>,
    /// `item_constraints.form_factor`.
    pub form_factor: Option<String>,
    /// `item_constraints.dietary_flags` (raw stored value).
    pub dietary_flags: Option<String>,
    /// `item_constraints.substitution_policy` (`any` | `ask` | `strict`).
    pub substitution_policy: String,
}

// ---------------------------------------------------------------------------
// Authorization helpers
// ---------------------------------------------------------------------------

/// True if `user_id` is a member of `household_id` (via `household_members`).
///
/// A DB error is propagated so callers fail closed (500) rather than treating a
/// transient failure as "not a member".
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

/// Map a DB error to a logged 500. Used by every handler so a transient failure
/// never fails open.
fn db_error(context: &'static str) -> impl Fn(sqlx::Error) -> StatusCode {
    move |e| {
        tracing::error!(error = %e, context, "lists: database error");
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `GET /api/lists` — all ACTIVE lists across the caller's household(s).
async fn list_lists(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Vec<ListSummary>>, StatusCode> {
    let rows = sqlx::query_as::<_, (String, String, String, String, i64, String)>(
        "SELECT id, household_id, name, status, current_revision, created_at \
         FROM shopping_lists \
         WHERE status = 'active' \
           AND household_id IN \
               (SELECT household_id FROM household_members WHERE user_id = ?1) \
         ORDER BY created_at, id",
    )
    .bind(&user.user_id)
    .fetch_all(&state.pool)
    .await
    .map_err(db_error("list_lists"))?;

    let lists = rows
        .into_iter()
        .map(
            |(id, household_id, name, status, current_revision, created_at)| ListSummary {
                id,
                household_id,
                name,
                status,
                current_revision,
                created_at,
            },
        )
        .collect();
    Ok(Json(lists))
}

/// `POST /api/lists` — create a new ACTIVE list (revision 0) in a household the
/// caller is a member of.
///
/// 422 for an empty name; 403 if the caller is not a member of the requested
/// household. CSRF is enforced by the extractor (this is a POST).
async fn create_list(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<CreateListRequest>,
) -> Result<(StatusCode, Json<ListDetail>), StatusCode> {
    let name = body.name.trim();
    if name.is_empty() {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    // A list may only be created in a household the caller belongs to.
    if !is_member(&state.pool, &user.user_id, &body.household_id)
        .await
        .map_err(db_error("create_list.is_member"))?
    {
        return Err(StatusCode::FORBIDDEN);
    }

    let id = uuid::Uuid::new_v4().to_string();
    let created_at = buywise_core::db::now_iso();

    sqlx::query(
        "INSERT INTO shopping_lists \
             (id, household_id, name, status, current_revision, created_at, archived_at) \
         VALUES (?1, ?2, ?3, 'active', 0, ?4, NULL)",
    )
    .bind(&id)
    .bind(&body.household_id)
    .bind(name)
    .bind(&created_at)
    .execute(&state.pool)
    .await
    .map_err(db_error("create_list.insert"))?;

    let detail = ListDetail {
        id,
        household_id: body.household_id,
        name: name.to_string(),
        status: "active".to_string(),
        current_revision: 0,
        created_at,
        archived_at: None,
        entries: Vec::new(),
    };
    Ok((StatusCode::CREATED, Json(detail)))
}

/// A `shopping_lists` row as needed for authorization + detail rendering.
type ListRow = (String, String, String, String, i64, String, Option<String>);

/// Load a list by id, or `None` if no such row exists.
async fn load_list(pool: &sqlx::SqlitePool, id: &str) -> Result<Option<ListRow>, sqlx::Error> {
    sqlx::query_as::<_, ListRow>(
        "SELECT id, household_id, name, status, current_revision, created_at, archived_at \
         FROM shopping_lists WHERE id = ?1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// Render a [`ListDetail`] from an already-loaded `shopping_lists` row plus a
/// single query for its ACTIVE entries.
///
/// Shared by [`get_list`] and [`patch_list`] so neither re-runs [`load_list`]
/// nor a redundant [`is_member`] check after the row is already in hand.
async fn load_detail(pool: &sqlx::SqlitePool, row: ListRow) -> Result<ListDetail, sqlx::Error> {
    let (id, household_id, name, status, current_revision, created_at, archived_at) = row;

    let entry_rows = sqlx::query_as::<
        _,
        (
            String,
            Option<String>,
            String,
            Option<String>,
            String,
            String,
            String,
            i64,
            String,
            String,
            i64,
            String,
            String,
        ),
    >(
        "SELECT id, canonical_item_id, resolution_status, resolution_candidates_json, \
                raw_text, display_name, added_by, quantity, status, \
                effective_constraint_hash, last_updated_revision, created_at, updated_at \
         FROM list_entries \
         WHERE list_id = ?1 AND status = 'active' \
         ORDER BY created_at, id",
    )
    .bind(&id)
    .fetch_all(pool)
    .await?;

    let entries = entry_rows
        .into_iter()
        .map(|r| EntryView {
            id: r.0,
            canonical_item_id: r.1,
            resolution_status: r.2,
            resolution_candidates_json: r.3,
            raw_text: r.4,
            display_name: r.5,
            added_by: r.6,
            quantity: r.7,
            status: r.8,
            effective_constraint_hash: r.9,
            last_updated_revision: r.10,
            created_at: r.11,
            updated_at: r.12,
        })
        .collect();

    Ok(ListDetail {
        id,
        household_id,
        name,
        status,
        current_revision,
        created_at,
        archived_at,
        entries,
    })
}

/// `GET /api/list/:id` — full list state with a top-level `current_revision`
/// and all ACTIVE entries.
///
/// 404 if the list does not exist; **403** if it exists but belongs to a
/// household the caller is not a member of (deliberately distinct from 404).
async fn get_list(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<String>,
) -> Result<Json<ListDetail>, StatusCode> {
    let row = load_list(&state.pool, &id)
        .await
        .map_err(db_error("get_list.load"))?
        .ok_or(StatusCode::NOT_FOUND)?;

    // Existing-but-forbidden is 403, NOT 404.
    if !is_member(&state.pool, &user.user_id, &row.1)
        .await
        .map_err(db_error("get_list.is_member"))?
    {
        return Err(StatusCode::FORBIDDEN);
    }

    let detail = load_detail(&state.pool, row)
        .await
        .map_err(db_error("get_list.entries"))?;
    Ok(Json(detail))
}

/// `PATCH /api/list/:id` — rename and/or archive a list.
///
/// 404 if missing; **403** if not a member of the owning household; 422 if the
/// body changes nothing, has an empty name, or carries an unknown status. When
/// archiving, `archived_at` is set; reactivating clears it. `current_revision`
/// is intentionally left untouched (owned by the mutation log, P1-T4). CSRF is
/// enforced by the extractor (this is a PATCH).
///
/// Ordering note: body validation runs **before** the list lookup, so a
/// malformed body on a missing/forbidden list returns 422 ahead of 404/403.
/// This is intentional — input is validated at the boundary first.
async fn patch_list(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<String>,
    Json(body): Json<PatchListRequest>,
) -> Result<Json<ListDetail>, StatusCode> {
    // Validate the body before any state lookup so a malformed request is a
    // clean 422 regardless of authorization.
    if body.name.is_none() && body.status.is_none() {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    if let Some(name) = &body.name {
        if name.trim().is_empty() {
            return Err(StatusCode::UNPROCESSABLE_ENTITY);
        }
    }
    if let Some(status) = &body.status {
        if status != "active" && status != "archived" {
            return Err(StatusCode::UNPROCESSABLE_ENTITY);
        }
    }

    let row = load_list(&state.pool, &id)
        .await
        .map_err(db_error("patch_list.load"))?
        .ok_or(StatusCode::NOT_FOUND)?;
    let household_id = row.1.clone();

    if !is_member(&state.pool, &user.user_id, &household_id)
        .await
        .map_err(db_error("patch_list.is_member"))?
    {
        return Err(StatusCode::FORBIDDEN);
    }

    // Compute the effective new values, starting from the current row.
    let new_name = body.name.as_ref().map(|n| n.trim().to_string());
    let new_status = body.status.clone();

    // archived_at follows status when status changes: set on archive, cleared on
    // (re)activate. If status is unchanged, archived_at is left as-is.
    match new_status.as_deref() {
        Some("archived") => {
            let now = buywise_core::db::now_iso();
            sqlx::query(
                "UPDATE shopping_lists \
                 SET name = COALESCE(?2, name), status = 'archived', archived_at = ?3 \
                 WHERE id = ?1",
            )
            .bind(&id)
            .bind(&new_name)
            .bind(&now)
            .execute(&state.pool)
            .await
            .map_err(db_error("patch_list.archive"))?;
        }
        Some("active") => {
            sqlx::query(
                "UPDATE shopping_lists \
                 SET name = COALESCE(?2, name), status = 'active', archived_at = NULL \
                 WHERE id = ?1",
            )
            .bind(&id)
            .bind(&new_name)
            .execute(&state.pool)
            .await
            .map_err(db_error("patch_list.activate"))?;
        }
        _ => {
            // Name-only change.
            sqlx::query("UPDATE shopping_lists SET name = COALESCE(?2, name) WHERE id = ?1")
                .bind(&id)
                .bind(&new_name)
                .execute(&state.pool)
                .await
                .map_err(db_error("patch_list.rename"))?;
        }
    }

    // Re-read so the response reflects the persisted state (including entries).
    // Membership was already verified above, so render directly from the
    // updated row rather than re-running get_list (which would repeat the
    // load_list + is_member queries).
    let updated = load_list(&state.pool, &id)
        .await
        .map_err(db_error("patch_list.reload"))?
        .ok_or(StatusCode::NOT_FOUND)?;
    let detail = load_detail(&state.pool, updated)
        .await
        .map_err(db_error("patch_list.entries"))?;
    Ok(Json(detail))
}

/// `GET /api/items` — the full canonical catalogue for the caller's
/// household(s): each item with its aliases and (optional) constraints.
async fn list_items(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Vec<ItemView>>, StatusCode> {
    let item_rows = sqlx::query_as::<_, (String, String, String, String, String, String)>(
        "SELECT id, household_id, name_en, name_es, category_id, default_constraint_policy \
         FROM canonical_items \
         WHERE household_id IN \
               (SELECT household_id FROM household_members WHERE user_id = ?1) \
         ORDER BY name_en",
    )
    .bind(&user.user_id)
    .fetch_all(&state.pool)
    .await
    .map_err(db_error("list_items.items"))?;

    if item_rows.is_empty() {
        return Ok(Json(Vec::new()));
    }

    // Batch all aliases and constraints for the scoped item set in a small,
    // constant number of queries (no per-item N+1). Both helpers chunk their
    // `IN (...)` lists to stay under SQLite's bound-parameter limit.
    let item_ids: Vec<String> = item_rows.iter().map(|r| r.0.clone()).collect();

    let mut aliases_by_item = load_aliases_for_items(&state.pool, &user.user_id, &item_ids)
        .await
        .map_err(db_error("list_items.aliases"))?;
    let mut constraints_by_item = load_constraints_for_items(&state.pool, &item_ids)
        .await
        .map_err(db_error("list_items.constraints"))?;

    let items = item_rows
        .into_iter()
        .map(
            |(id, household_id, name_en, name_es, category_id, default_constraint_policy)| {
                let aliases = aliases_by_item.remove(&id).unwrap_or_default();
                let constraints = constraints_by_item.remove(&id);
                ItemView {
                    id,
                    household_id,
                    name_en,
                    name_es,
                    category_id,
                    default_constraint_policy,
                    aliases,
                    constraints,
                }
            },
        )
        .collect();

    Ok(Json(items))
}

/// Max bound parameters per chunked `IN (...)` query. SQLite's compile-time
/// limit is 999 (pre-3.32) / 32766 (3.32+); 900 is comfortably below either and
/// leaves headroom for the per-chunk household-scope parameter.
const IN_CHUNK_SIZE: usize = 900;

/// Build all aliases for the given canonical item ids, grouped by
/// `canonical_item_id`.
///
/// Defense in depth: aliases are additionally constrained to the caller's
/// household set (`item_aliases.household_id`) so a mis-pointed alias from
/// another household cannot leak through the `canonical_item_id` join.
async fn load_aliases_for_items(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    item_ids: &[String],
) -> Result<std::collections::HashMap<String, Vec<AliasView>>, sqlx::Error> {
    let mut grouped: std::collections::HashMap<String, Vec<AliasView>> =
        std::collections::HashMap::new();

    for chunk in item_ids.chunks(IN_CHUNK_SIZE) {
        let placeholders = sql_placeholders(chunk.len());
        let sql = format!(
            "SELECT canonical_item_id, id, alias, language, source FROM item_aliases \
             WHERE canonical_item_id IN ({placeholders}) \
               AND household_id IN \
                   (SELECT household_id FROM household_members WHERE user_id = ?) \
             ORDER BY alias"
        );
        let mut query = sqlx::query_as::<_, (String, String, String, String, String)>(&sql);
        for id in chunk {
            query = query.bind(id);
        }
        query = query.bind(user_id);

        for (canonical_item_id, id, alias, language, source) in query.fetch_all(pool).await? {
            grouped
                .entry(canonical_item_id)
                .or_default()
                .push(AliasView {
                    id,
                    alias,
                    language,
                    source,
                });
        }
    }

    Ok(grouped)
}

/// Build the (at most one) constraints row for each canonical item id, keyed by
/// `canonical_item_id`.
async fn load_constraints_for_items(
    pool: &sqlx::SqlitePool,
    item_ids: &[String],
) -> Result<std::collections::HashMap<String, ConstraintsView>, sqlx::Error> {
    let mut by_item: std::collections::HashMap<String, ConstraintsView> =
        std::collections::HashMap::new();

    for chunk in item_ids.chunks(IN_CHUNK_SIZE) {
        let placeholders = sql_placeholders(chunk.len());
        let sql = format!(
            "SELECT canonical_item_id, brand, allowed_brands, excluded_brands, \
                    preferred_size_value, preferred_size_basis, min_size_value, min_size_basis, \
                    max_size_value, max_size_basis, pack_count, form_factor, dietary_flags, \
                    substitution_policy \
             FROM item_constraints WHERE canonical_item_id IN ({placeholders})"
        );
        let mut query = sqlx::query_as::<_, KeyedConstraintsRow>(&sql);
        for id in chunk {
            query = query.bind(id);
        }

        for r in query.fetch_all(pool).await? {
            by_item.insert(
                r.0,
                ConstraintsView {
                    brand: r.1,
                    allowed_brands: r.2,
                    excluded_brands: r.3,
                    preferred_size_value: r.4,
                    preferred_size_basis: r.5,
                    min_size_value: r.6,
                    min_size_basis: r.7,
                    max_size_value: r.8,
                    max_size_basis: r.9,
                    pack_count: r.10,
                    form_factor: r.11,
                    dietary_flags: r.12,
                    substitution_policy: r.13,
                },
            );
        }
    }

    Ok(by_item)
}

/// Build `n` comma-separated `?` placeholders for an `IN (...)` clause.
fn sql_placeholders(n: usize) -> String {
    let mut s = String::with_capacity(n.saturating_mul(2));
    for i in 0..n {
        if i > 0 {
            s.push(',');
        }
        s.push('?');
    }
    s
}

/// An `item_constraints` row keyed by its `canonical_item_id` (first column),
/// followed by the columns in [`ConstraintsView`] order. Flat (not nested) so
/// sqlx can decode it via `FromRow`.
type KeyedConstraintsRow = (
    String,         // canonical_item_id
    Option<String>, // brand
    Option<String>, // allowed_brands
    Option<String>, // excluded_brands
    Option<f64>,    // preferred_size_value
    Option<String>, // preferred_size_basis
    Option<f64>,    // min_size_value
    Option<String>, // min_size_basis
    Option<f64>,    // max_size_value
    Option<String>, // max_size_basis
    Option<i64>,    // pack_count
    Option<String>, // form_factor
    Option<String>, // dietary_flags
    String,         // substitution_policy
);
