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
         ORDER BY created_at",
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
    let (id, household_id, name, status, current_revision, created_at, archived_at) = row;

    // Existing-but-forbidden is 403, NOT 404.
    if !is_member(&state.pool, &user.user_id, &household_id)
        .await
        .map_err(db_error("get_list.is_member"))?
    {
        return Err(StatusCode::FORBIDDEN);
    }

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
         ORDER BY created_at",
    )
    .bind(&id)
    .fetch_all(&state.pool)
    .await
    .map_err(db_error("get_list.entries"))?;

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

    Ok(Json(ListDetail {
        id,
        household_id,
        name,
        status,
        current_revision,
        created_at,
        archived_at,
        entries,
    }))
}

/// `PATCH /api/list/:id` — rename and/or archive a list.
///
/// 404 if missing; **403** if not a member of the owning household; 422 if the
/// body changes nothing, has an empty name, or carries an unknown status. When
/// archiving, `archived_at` is set; reactivating clears it. `current_revision`
/// is intentionally left untouched (owned by the mutation log, P1-T4). CSRF is
/// enforced by the extractor (this is a PATCH).
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
    get_list(State(state), user, Path(id)).await
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

    let mut items = Vec::with_capacity(item_rows.len());
    for (id, household_id, name_en, name_es, category_id, default_constraint_policy) in item_rows {
        let aliases = sqlx::query_as::<_, (String, String, String, String)>(
            "SELECT id, alias, language, source FROM item_aliases \
             WHERE canonical_item_id = ?1 ORDER BY alias",
        )
        .bind(&id)
        .fetch_all(&state.pool)
        .await
        .map_err(db_error("list_items.aliases"))?
        .into_iter()
        .map(|(id, alias, language, source)| AliasView {
            id,
            alias,
            language,
            source,
        })
        .collect();

        let constraints = load_constraints(&state.pool, &id)
            .await
            .map_err(db_error("list_items.constraints"))?;

        items.push(ItemView {
            id,
            household_id,
            name_en,
            name_es,
            category_id,
            default_constraint_policy,
            aliases,
            constraints,
        });
    }

    Ok(Json(items))
}

/// The columns of an `item_constraints` row, in [`ConstraintsView`] order.
type ConstraintsRow = (
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

/// Load the (at most one) constraints row for a canonical item.
async fn load_constraints(
    pool: &sqlx::SqlitePool,
    canonical_item_id: &str,
) -> Result<Option<ConstraintsView>, sqlx::Error> {
    let row = sqlx::query_as::<_, ConstraintsRow>(
        "SELECT brand, allowed_brands, excluded_brands, preferred_size_value, \
                preferred_size_basis, min_size_value, min_size_basis, max_size_value, \
                max_size_basis, pack_count, form_factor, dietary_flags, substitution_policy \
         FROM item_constraints WHERE canonical_item_id = ?1",
    )
    .bind(canonical_item_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| ConstraintsView {
        brand: r.0,
        allowed_brands: r.1,
        excluded_brands: r.2,
        preferred_size_value: r.3,
        preferred_size_basis: r.4,
        min_size_value: r.5,
        min_size_basis: r.6,
        max_size_value: r.7,
        max_size_basis: r.8,
        pack_count: r.9,
        form_factor: r.10,
        dietary_flags: r.11,
        substitution_policy: r.12,
    }))
}
