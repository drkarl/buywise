//! Per-item household product preferences (`GET`/`PUT /api/items/{id}/preferences`)
//! — issue #102.
//!
//! Lets the household set, from the app and without SQL, how a canonical item is
//! allowed to be matched: the [`ConstraintPolicy`] (`any`, `brand`, `allowed_set`,
//! `own_label`, `exact`, `ask`), the brand / allowed-brands / excluded-brands rules,
//! a preferred size, and the [`SubstitutionPolicy`] (including "ask before
//! substituting"). The optimiser already reads these columns
//! (`item_constraints` + `canonical_items.default_constraint_policy`), so a save
//! here changes the very next `POST /api/optimise` run — no other code path is
//! involved.
//!
//! The same screen also lists the item's `product_matches` (accepted, suggested and
//! rejected) with the matched product's name/brand/retailer/size, so the household
//! can see acceptable alternatives and explicitly reject one. Accept/reject is
//! written through the existing `POST /api/match-feedback` path (P5-T1); this module
//! only *reads* matches, it never changes them. Rejected products stay excluded
//! because the optimiser loads candidates via `db::accepted_or_suggested_matches`.
//!
//! # Scope and household isolation
//!
//! `canonical_items` are household-scoped, so — exactly like `match_feedback.rs` —
//! the household is derived from the item itself and the caller must be a member.
//! An unknown item or one in another household is a 404, never distinguishing the
//! two, so a caller cannot probe another household's catalogue.
//!
//! # Missing data is surfaced, not silently weakened
//!
//! A policy that cannot be honoured (`brand` with no brand, `allowed_set` with an
//! empty allow-list) is rejected at the boundary with a 422, so the screen can never
//! persist a configuration the optimiser would only report as an error. The
//! response additionally carries `warnings` (stable codes) describing anything the
//! optimiser cannot evaluate, above all `no_usable_matches` when the item has no
//! accepted/suggested match at all — missing matching data is visible on the screen
//! rather than quietly letting the constraint through.
//!
//! # Contract
//!
//! `PUT` replaces the whole preference set (the screen always loads and saves the
//! complete state), so an absent/`null` field means "clear", not "leave unchanged".
//! `GET` returns the stored state plus the item's matches and warnings. CSRF is
//! enforced by the [`AuthenticatedUser`] extractor on the `PUT`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use buywise_core::db;
use buywise_core::domain::{
    ConstraintPolicy, MatchStatus, Retailer, RetailerProduct, SizeBasis, SourceConfidence,
    SubstitutionPolicy, UnitPriceBasis,
};
use buywise_core::product_matching::{rank_candidates, ItemProfile, ScoredProduct};

use crate::llm::LlmError;
use crate::middleware::AuthenticatedUser;
use crate::AppState;

/// Build the preferences routes as a `Router<AppState>`, merged by [`crate::app`].
///
/// One path, two verbs: `GET` reads the item's preferences + matches, `PUT`
/// replaces the preference set.
pub fn router() -> Router<AppState> {
    Router::new().route(
        "/api/items/{id}/preferences",
        get(get_preferences).put(put_preferences),
    )
}

// ---------------------------------------------------------------------------
// Request / response shapes
// ---------------------------------------------------------------------------

/// Body for `PUT /api/items/{id}/preferences`.
///
/// Replaces the whole preference set: a `None`/absent brand or sized field clears
/// it (the screen always sends the complete state). See the module docs.
#[derive(Debug, Deserialize)]
pub struct PutPreferencesRequest {
    /// How strictly the item constrains matching products.
    pub default_constraint_policy: ConstraintPolicy,
    /// The single brand a `brand` policy requires. Blank/whitespace is treated as
    /// unset (and a `brand` policy without a brand is a 422).
    #[serde(default)]
    pub brand: Option<String>,
    /// Brands an `allowed_set` policy accepts. Empty list clears it.
    #[serde(default)]
    pub allowed_brands: Vec<String>,
    /// Brands to reject regardless of policy. Empty list clears it.
    #[serde(default)]
    pub excluded_brands: Vec<String>,
    /// Preferred size in canonical base units (grams / millilitres / count).
    #[serde(default)]
    pub preferred_size_value: Option<f64>,
    /// Basis for `preferred_size_value` (`g`, `ml`, `each`). Required when a value
    /// is present, and must be absent when it is not.
    #[serde(default)]
    pub preferred_size_basis: Option<SizeBasis>,
    /// Whether substitutions are allowed, must be asked about, or are forbidden.
    pub substitution_policy: SubstitutionPolicy,
}

/// The item's effective constraint fields, mirroring the editable subset of
/// [`ItemConstraints`] (the optimiser-enforced columns).
#[derive(Debug, Serialize)]
pub struct PreferencesConstraints {
    /// `item_constraints.brand`.
    pub brand: Option<String>,
    /// `item_constraints.allowed_brands`, decoded (never the raw JSON).
    pub allowed_brands: Vec<String>,
    /// `item_constraints.excluded_brands`, decoded.
    pub excluded_brands: Vec<String>,
    /// `item_constraints.preferred_size_value`.
    pub preferred_size_value: Option<f64>,
    /// `item_constraints.preferred_size_basis`.
    pub preferred_size_basis: Option<SizeBasis>,
    /// `item_constraints.substitution_policy`.
    pub substitution_policy: SubstitutionPolicy,
}

/// One product match for the item, joined to its global retailer product.
#[derive(Debug, Serialize)]
pub struct MatchView {
    /// `product_matches.retailer_product_id`.
    pub retailer_product_id: String,
    /// `product_matches.status` (`accepted` | `rejected` | `suggested` | `pending`).
    pub status: MatchStatus,
    /// `product_matches.matched_by` (`user` | `system` | `llm`).
    pub matched_by: String,
    /// `retailer_products.name`.
    pub name: String,
    /// `retailer_products.brand`.
    pub brand: Option<String>,
    /// `retailer_products.retailer`.
    pub retailer: Retailer,
    /// `retailer_products.pack_size_text` (e.g. "415g"), when known.
    pub size_text: Option<String>,
}

/// One proposed retailer product a household can match to the item (issue #108),
/// carrying the product identity and the price provenance the decision is based on.
///
/// A candidate is surfaced only when it has a current offer (so every proposal is
/// priceable) and is not already accepted/suggested/rejected for the item — the
/// `matches` list owns decided products. A rejected product therefore does not
/// recur here unless its decision is deliberately undone through `match-feedback`.
#[derive(Debug, Serialize)]
pub struct CandidateView {
    /// `retailer_products.id`.
    pub retailer_product_id: String,
    /// `retailer_products.retailer`.
    pub retailer: Retailer,
    /// `retailer_products.name`.
    pub name: String,
    /// `retailer_products.brand`.
    pub brand: Option<String>,
    /// Pack size as shown on the shelf (`pack_size_text`, else composed from
    /// `size_value`/`size_unit`), or `None` when the product carries neither.
    pub size_text: Option<String>,
    /// `retailer_products.pack_count`.
    pub pack_count: Option<i32>,
    /// Deterministic relevance score (higher is a better fit). Ordering within a
    /// retailer is by this score.
    pub score: i32,
    /// Stable codes for the signals that contributed to the score: `name`,
    /// `brand`, `size`.
    pub signals: Vec<String>,
    /// Latest offer's shelf price in cents.
    pub price_cents: i64,
    /// Latest offer's loyalty price in cents, when one applies.
    pub loyalty_price_cents: Option<i64>,
    /// Loyalty scheme behind `loyalty_price_cents` (`clubcard` / `real_rewards`).
    pub loyalty_scheme: Option<String>,
    /// Unit price in micros, when the retailer publishes one.
    pub unit_price_eur_micros: Option<i64>,
    /// Basis the unit price is quoted against.
    pub unit_price_basis: Option<UnitPriceBasis>,
    /// The offer's source snapshot date (provenance).
    pub offer_snapshot_date: Option<String>,
    /// When the offer was fetched (provenance).
    pub fetched_at: String,
    /// The offer's ingest confidence (`high` / `medium` / `low`).
    pub source_confidence: SourceConfidence,
}

/// Response for `GET` and `PUT /api/items/{id}/preferences`.
#[derive(Debug, Serialize)]
pub struct PreferencesResponse {
    /// `canonical_items.id`.
    pub canonical_item_id: String,
    /// `canonical_items.name_en` (fallback display name).
    pub name_en: String,
    /// `canonical_items.name_es`.
    pub name_es: String,
    /// `canonical_items.default_constraint_policy`.
    pub default_constraint_policy: ConstraintPolicy,
    /// The item's constraint row (defaults when none exists).
    pub constraints: PreferencesConstraints,
    /// Every product match for the item, in a deterministic order.
    pub matches: Vec<MatchView>,
    /// Proposed products per retailer (issue #108), best match first within each
    /// retailer. Empty when the catalogue holds no plausible product.
    pub candidates: Vec<CandidateView>,
    /// Stable warning codes for data the optimiser cannot evaluate (see the module
    /// docs): `brand_policy_missing_brand`, `allowed_set_empty`,
    /// `no_usable_matches`.
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// Error helper
// ---------------------------------------------------------------------------

/// Map a DB error to a logged 500, mirroring `match_feedback` / `settings`.
fn db_error(context: &'static str) -> impl Fn(sqlx::Error) -> StatusCode {
    move |e| {
        tracing::error!(error = %e, context, "preferences: database error");
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `GET /api/items/{id}/preferences` — the item's preferences, matches and warnings.
///
/// 404 if the item does not exist or is in a household the caller is not a member
/// of (never distinguishing the two).
async fn get_preferences(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(canonical_item_id): Path<String>,
) -> Result<Json<PreferencesResponse>, StatusCode> {
    let pool = &state.pool;
    let household_id = authorised_household(pool, &user.user_id, &canonical_item_id).await?;
    let response = load_preferences(&state, &household_id, &canonical_item_id)
        .await
        .map_err(db_error("get_preferences.load"))?;
    Ok(Json(response))
}

/// `PUT /api/items/{id}/preferences` — replace the item's preference set.
///
/// 404 for an unknown item / non-member; 422 when the body is internally
/// inconsistent (a `brand` policy without a brand, an `allowed_set` policy with an
/// empty allow-list, a size value without a basis or vice versa, a non-positive
/// size). CSRF is enforced by the extractor. The constraints row is upserted and
/// `canonical_items.default_constraint_policy` updated in one transaction, then the
/// effective state is read back.
async fn put_preferences(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(canonical_item_id): Path<String>,
    Json(body): Json<PutPreferencesRequest>,
) -> Result<Json<PreferencesResponse>, StatusCode> {
    let pool = &state.pool;

    // Validate before any state lookup: a malformed preference set is a clean 422
    // regardless of authorization (mirrors `patch_settings` ordering).
    let validated = validate(body)?;

    let household_id = authorised_household(pool, &user.user_id, &canonical_item_id).await?;

    let now = db::now_iso();
    let mut tx = pool
        .begin()
        .await
        .map_err(db_error("put_preferences.begin"))?;

    sqlx::query(
        "INSERT INTO item_constraints \
             (id, canonical_item_id, brand, allowed_brands, excluded_brands, \
              preferred_size_value, preferred_size_basis, substitution_policy, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
         ON CONFLICT(canonical_item_id) DO UPDATE SET \
             brand = excluded.brand, \
             allowed_brands = excluded.allowed_brands, \
             excluded_brands = excluded.excluded_brands, \
             preferred_size_value = excluded.preferred_size_value, \
             preferred_size_basis = excluded.preferred_size_basis, \
             substitution_policy = excluded.substitution_policy, \
             updated_at = excluded.updated_at",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&canonical_item_id)
    .bind(validated.brand.as_deref())
    .bind(json_list(&validated.allowed_brands))
    .bind(json_list(&validated.excluded_brands))
    .bind(validated.preferred_size_value)
    .bind(validated.preferred_size_basis)
    .bind(validated.substitution_policy)
    .bind(&now)
    .execute(&mut *tx)
    .await
    .map_err(db_error("put_preferences.upsert_constraints"))?;

    sqlx::query("UPDATE canonical_items SET default_constraint_policy = ?1 WHERE id = ?2")
        .bind(validated.default_constraint_policy)
        .bind(&canonical_item_id)
        .execute(&mut *tx)
        .await
        .map_err(db_error("put_preferences.update_policy"))?;

    tx.commit()
        .await
        .map_err(db_error("put_preferences.commit"))?;

    let response = load_preferences(&state, &household_id, &canonical_item_id)
        .await
        .map_err(db_error("put_preferences.reload"))?;
    Ok(Json(response))
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// A validated, normalised `PUT` body ready to persist.
#[derive(Debug)]
struct ValidatedPreferences {
    default_constraint_policy: ConstraintPolicy,
    brand: Option<String>,
    allowed_brands: Vec<String>,
    excluded_brands: Vec<String>,
    preferred_size_value: Option<f64>,
    preferred_size_basis: Option<SizeBasis>,
    substitution_policy: SubstitutionPolicy,
}

/// Normalise and cross-check a `PUT` body, returning a 422 for any inconsistency.
///
/// Brand lists are trimmed, blanks dropped and case-insensitive duplicates
/// removed (first spelling wins) so the screen cannot store an empty entry that
/// would silently weaken a rule.
fn validate(body: PutPreferencesRequest) -> Result<ValidatedPreferences, StatusCode> {
    let brand = normalise_optional_brand(body.brand);
    let allowed_brands = if body.default_constraint_policy == ConstraintPolicy::AllowedSet {
        normalise_brand_list(body.allowed_brands)
    } else {
        Vec::new()
    };
    let excluded_brands = normalise_brand_list(body.excluded_brands);

    // A size is all-or-nothing and must be positive: the optimiser compares it
    // against a ±20% band, and a zero/negative value could never match a product.
    match (body.preferred_size_value, body.preferred_size_basis) {
        (Some(value), Some(basis)) if value.is_finite() && value > 0.0 => {
            Ok(ValidatedPreferences {
                default_constraint_policy: body.default_constraint_policy,
                brand,
                allowed_brands,
                excluded_brands,
                preferred_size_value: Some(value),
                preferred_size_basis: Some(basis),
                substitution_policy: body.substitution_policy,
            })
        }
        (None, None) => Ok(ValidatedPreferences {
            default_constraint_policy: body.default_constraint_policy,
            brand,
            allowed_brands,
            excluded_brands,
            preferred_size_value: None,
            preferred_size_basis: None,
            substitution_policy: body.substitution_policy,
        }),
        _ => Err(StatusCode::UNPROCESSABLE_ENTITY),
    }
    .and_then(|v| {
        // A policy that cannot be honoured is rejected at the boundary rather than
        // stored and later reported as a plan warning (see the module docs).
        match v.default_constraint_policy {
            ConstraintPolicy::Brand if v.brand.is_none() => Err(StatusCode::UNPROCESSABLE_ENTITY),
            ConstraintPolicy::AllowedSet if v.allowed_brands.is_empty() => {
                Err(StatusCode::UNPROCESSABLE_ENTITY)
            }
            _ => Ok(v),
        }
    })
}

/// Trim a brand to a non-empty value, or `None` when blank/absent.
fn normalise_optional_brand(brand: Option<String>) -> Option<String> {
    brand
        .map(|b| b.trim().to_string())
        .filter(|b| !b.is_empty())
}

/// Trim, drop blanks, and de-duplicate a brand list case-insensitively, keeping
/// the first spelling seen.
fn normalise_brand_list(brands: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for brand in brands {
        let brand = brand.trim();
        if brand.is_empty() {
            continue;
        }
        if out
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(brand))
        {
            continue;
        }
        out.push(brand.to_string());
    }
    out
}

/// Encode a brand list as the JSON TEXT column, or SQL `NULL` when empty so an
/// empty rule reads back as "unset" (mirrors the lenient decode on the read side).
fn json_list(brands: &[String]) -> Option<String> {
    if brands.is_empty() {
        None
    } else {
        serde_json::to_string(brands).ok()
    }
}

// ---------------------------------------------------------------------------
// DB helpers
// ---------------------------------------------------------------------------

/// Resolve the household of `canonical_item_id` and require `user_id` to be a
/// member. Returns 404 for an unknown item OR a non-member (never distinguishing
/// the two), matching `match_feedback`.
async fn authorised_household(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    canonical_item_id: &str,
) -> Result<String, StatusCode> {
    let household_id: String =
        sqlx::query_scalar("SELECT household_id FROM canonical_items WHERE id = ?1")
            .bind(canonical_item_id)
            .fetch_optional(pool)
            .await
            .map_err(db_error("preferences.canonical_household"))?
            .ok_or(StatusCode::NOT_FOUND)?;

    let member: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM household_members WHERE user_id = ?1 AND household_id = ?2",
    )
    .bind(user_id)
    .bind(&household_id)
    .fetch_optional(pool)
    .await
    .map_err(db_error("preferences.is_member"))?;

    if member.is_none() {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(household_id)
}

/// Read the item's name/policy, its constraints row (defaults when absent), its
/// matches, its proposed candidates, and derive the warning codes.
async fn load_preferences(
    state: &AppState,
    household_id: &str,
    canonical_item_id: &str,
) -> Result<PreferencesResponse, sqlx::Error> {
    let pool = &state.pool;
    let (name_en, name_es, policy) = sqlx::query_as::<_, (String, String, ConstraintPolicy)>(
        "SELECT name_en, name_es, default_constraint_policy FROM canonical_items WHERE id = ?1",
    )
    .bind(canonical_item_id)
    .fetch_one(pool)
    .await?;

    let constraints = load_constraints(pool, canonical_item_id).await?;
    let matches = load_matches(pool, household_id, canonical_item_id).await?;
    let candidates = load_candidates(
        state,
        household_id,
        canonical_item_id,
        &name_en,
        &name_es,
        &constraints,
    )
    .await?;
    let warnings = warnings(policy, &constraints, &matches);

    Ok(PreferencesResponse {
        canonical_item_id: canonical_item_id.to_string(),
        name_en,
        name_es,
        default_constraint_policy: policy,
        constraints,
        matches,
        candidates,
        warnings,
    })
}

/// Load the item's constraints row, or defaults when none exists. The JSON-list
/// columns are decoded leniently (mirrors `db::item_constraints_by_canonical_id`).
async fn load_constraints(
    pool: &sqlx::SqlitePool,
    canonical_item_id: &str,
) -> Result<PreferencesConstraints, sqlx::Error> {
    type Row = (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<f64>,
        Option<String>,
        String,
    );

    let row: Option<Row> = sqlx::query_as(
        "SELECT brand, allowed_brands, excluded_brands, preferred_size_value, \
                preferred_size_basis, substitution_policy \
         FROM item_constraints WHERE canonical_item_id = ?1",
    )
    .bind(canonical_item_id)
    .fetch_optional(pool)
    .await?;

    let Some((brand, allowed, excluded, size_value, size_basis, substitution)) = row else {
        return Ok(PreferencesConstraints {
            brand: None,
            allowed_brands: Vec::new(),
            excluded_brands: Vec::new(),
            preferred_size_value: None,
            preferred_size_basis: None,
            substitution_policy: SubstitutionPolicy::Any,
        });
    };

    Ok(PreferencesConstraints {
        brand: normalise_optional_brand(brand),
        allowed_brands: db::parse_json_string_list(allowed.as_deref()).unwrap_or_default(),
        excluded_brands: db::parse_json_string_list(excluded.as_deref()).unwrap_or_default(),
        preferred_size_value: size_value,
        preferred_size_basis: size_basis
            .as_deref()
            .and_then(|s| SizeBasis::from_db_str(s).ok()),
        substitution_policy: SubstitutionPolicy::from_db_str(&substitution)
            .unwrap_or(SubstitutionPolicy::Any),
    })
}

/// Load every `product_matches` row for the item, joined to its product. Ordered
/// by retailer then product name so the screen's list is deterministic.
async fn load_matches(
    pool: &sqlx::SqlitePool,
    household_id: &str,
    canonical_item_id: &str,
) -> Result<Vec<MatchView>, sqlx::Error> {
    let rows = sqlx::query_as::<
        _,
        (
            String,
            MatchStatus,
            String,
            String,
            Option<String>,
            Retailer,
            Option<String>,
        ),
    >(
        "SELECT pm.retailer_product_id, pm.status, pm.matched_by, rp.name, rp.brand, \
                rp.retailer, rp.pack_size_text \
         FROM product_matches pm \
         JOIN retailer_products rp ON rp.id = pm.retailer_product_id \
         WHERE pm.household_id = ?1 AND pm.canonical_item_id = ?2 \
         ORDER BY rp.retailer, rp.name",
    )
    .bind(household_id)
    .bind(canonical_item_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(retailer_product_id, status, matched_by, name, brand, retailer, size_text)| {
                MatchView {
                    retailer_product_id,
                    status,
                    matched_by,
                    name,
                    brand,
                    retailer,
                    size_text,
                }
            },
        )
        .collect())
}

/// Propose retailer products for the item (issue #108).
///
/// Loads the item's profile (names, aliases, brand/size rules), every priced
/// retailer product, ranks them with [`rank_candidates`], optionally reorders the
/// result with the LLM fallback, and attaches each candidate's latest offer as
/// price provenance. Products already decided for the item (accepted, suggested
/// or rejected) are excluded up front, so a rejected suggestion never recurs
/// unless its decision is deliberately undone.
async fn load_candidates(
    state: &AppState,
    household_id: &str,
    canonical_item_id: &str,
    name_en: &str,
    name_es: &str,
    constraints: &PreferencesConstraints,
) -> Result<Vec<CandidateView>, sqlx::Error> {
    let pool = &state.pool;

    let decided: std::collections::HashSet<String> = sqlx::query_scalar(
        "SELECT retailer_product_id FROM product_matches \
         WHERE household_id = ?1 AND canonical_item_id = ?2 \
           AND status IN ('accepted', 'suggested', 'rejected')",
    )
    .bind(household_id)
    .bind(canonical_item_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect();

    let aliases: Vec<String> = sqlx::query_scalar(
        "SELECT alias FROM item_aliases WHERE household_id = ?1 AND canonical_item_id = ?2",
    )
    .bind(household_id)
    .bind(canonical_item_id)
    .fetch_all(pool)
    .await?;

    let profile = ItemProfile {
        name_en: name_en.to_string(),
        name_es: name_es.to_string(),
        aliases,
        brand: constraints.brand.clone(),
        allowed_brands: constraints.allowed_brands.clone(),
        excluded_brands: constraints.excluded_brands.clone(),
        preferred_size: match (
            constraints.preferred_size_value,
            constraints.preferred_size_basis,
        ) {
            (Some(value), Some(basis)) => Some((value, basis)),
            _ => None,
        },
    };

    // Only products that have ever been priced can be a useful proposal (the
    // household must be able to see what it would pay).
    let products: Vec<RetailerProduct> = sqlx::query_as::<_, RetailerProduct>(
        "SELECT rp.* FROM retailer_products rp \
         WHERE EXISTS (SELECT 1 FROM product_offers po WHERE po.retailer_product_id = rp.id)",
    )
    .fetch_all(pool)
    .await?;

    let candidates: Vec<RetailerProduct> = products
        .into_iter()
        .filter(|p| !decided.contains(&p.id))
        .collect();

    let scored = rank_candidates(&profile, &candidates);
    let scored = llm_reorder(state, name_en, scored).await;

    let mut out = Vec::with_capacity(scored.len());
    for candidate in scored {
        // Latest offer, mirroring `db::latest_offer_for_product` (which returns a
        // `DbError`); queried directly here so this loader keeps the single
        // `sqlx::Error` type the handler maps to a 500.
        let offer: Option<buywise_core::domain::ProductOffer> = sqlx::query_as(
            "SELECT * FROM product_offers \
             WHERE retailer_product_id = ?1 \
             ORDER BY fetched_at DESC LIMIT 1",
        )
        .bind(&candidate.product.id)
        .fetch_optional(pool)
        .await?;
        let Some(offer) = offer else {
            continue;
        };
        let size_text = pack_size_text(&candidate.product);
        out.push(CandidateView {
            retailer_product_id: candidate.product.id,
            retailer: candidate.product.retailer,
            name: candidate.product.name,
            brand: candidate.product.brand,
            size_text,
            pack_count: candidate.product.pack_count,
            score: candidate.score,
            signals: candidate
                .signals
                .iter()
                .map(|s| s.as_str().to_string())
                .collect(),
            price_cents: offer.price_cents,
            loyalty_price_cents: offer.loyalty_price_cents,
            loyalty_scheme: offer.loyalty_scheme,
            unit_price_eur_micros: offer.unit_price_eur_micros,
            unit_price_basis: offer.unit_price_basis,
            offer_snapshot_date: offer.source_snapshot_date,
            fetched_at: offer.fetched_at,
            source_confidence: offer.source_confidence_at_ingest,
        });
    }
    Ok(out)
}

/// Human-readable pack size for a candidate: the retailer's own `pack_size_text`
/// when present, else a composed `size_value` + `size_unit`. `None` when neither
/// exists, so the UI omits the size rather than showing an empty token.
fn pack_size_text(product: &RetailerProduct) -> Option<String> {
    if let Some(text) = product.pack_size_text.as_deref() {
        if !text.trim().is_empty() {
            return Some(text.to_string());
        }
    }
    match (product.size_value, product.size_unit.as_deref()) {
        (Some(value), Some(unit)) => {
            let value = if value.fract() == 0.0 {
                format!("{}", value as i64)
            } else {
                format!("{value}")
            };
            Some(format!("{value}{unit}"))
        }
        _ => None,
    }
}

/// Reorder candidates with the LLM fallback when it is enabled (issue #108).
///
/// Best-effort and advisory: the deterministic order is always the baseline, the
/// model can only reorder the candidates it was given (never add one), and any
/// error or unparseable answer falls back silently. A call that was actually made
/// is recorded in `api_usage_events` for cost tracking, mirroring the resolution
/// cascade.
async fn llm_reorder(
    state: &AppState,
    item_description: &str,
    scored: Vec<ScoredProduct>,
) -> Vec<ScoredProduct> {
    if scored.is_empty() || !state.llm_client.is_enabled() {
        return scored;
    }
    let labels: Vec<String> = scored.iter().map(candidate_label).collect();
    match state
        .llm_client
        .rank_candidates(item_description, &labels)
        .await
    {
        Ok(order) => {
            crate::resolution::record_llm_usage(&state.pool).await;
            match order {
                Some(order) => reorder_by_labels(scored, &order),
                None => scored,
            }
        }
        // Disabled/unconfigured: nothing was attempted; keep the order silently.
        Err(LlmError::Disabled) => scored,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "preferences: LLM candidate ranking failed; using deterministic order"
            );
            scored
        }
    }
}

/// The label the model sees for a candidate, and the exact string it must echo
/// back to reorder it. `name | retailer` is stable and readable.
fn candidate_label(candidate: &ScoredProduct) -> String {
    format!(
        "{} | {}",
        candidate.product.name,
        candidate.product.retailer.as_db_str()
    )
}

/// Stable reorder: candidates the model ranked come first in the model's order,
/// every candidate it omitted keeps its deterministic relative order after them.
fn reorder_by_labels(mut scored: Vec<ScoredProduct>, order: &[String]) -> Vec<ScoredProduct> {
    let rank =
        |candidate: &ScoredProduct| order.iter().position(|l| l == &candidate_label(candidate));
    scored.sort_by(|a, b| match (rank(a), rank(b)) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    scored
}

/// Stable warning codes for data the optimiser cannot evaluate. See the module docs.
fn warnings(
    policy: ConstraintPolicy,
    constraints: &PreferencesConstraints,
    matches: &[MatchView],
) -> Vec<String> {
    let mut warnings = Vec::new();
    if policy == ConstraintPolicy::Brand && constraints.brand.is_none() {
        warnings.push("brand_policy_missing_brand".to_string());
    }
    if policy == ConstraintPolicy::AllowedSet && constraints.allowed_brands.is_empty() {
        warnings.push("allowed_set_empty".to_string());
    }
    let usable = matches
        .iter()
        .any(|m| matches!(m.status, MatchStatus::Accepted | MatchStatus::Suggested));
    if !usable {
        warnings.push("no_usable_matches".to_string());
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brand_policy_without_brand_is_rejected() {
        let body = PutPreferencesRequest {
            default_constraint_policy: ConstraintPolicy::Brand,
            brand: Some("   ".into()),
            allowed_brands: vec![],
            excluded_brands: vec![],
            preferred_size_value: None,
            preferred_size_basis: None,
            substitution_policy: SubstitutionPolicy::Any,
        };
        assert_eq!(
            validate(body).unwrap_err(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    #[test]
    fn allowed_set_without_brands_is_rejected() {
        let body = PutPreferencesRequest {
            default_constraint_policy: ConstraintPolicy::AllowedSet,
            brand: None,
            allowed_brands: vec![],
            excluded_brands: vec![],
            preferred_size_value: None,
            preferred_size_basis: None,
            substitution_policy: SubstitutionPolicy::Any,
        };
        assert_eq!(
            validate(body).unwrap_err(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    #[test]
    fn allowed_brands_are_cleared_outside_allowed_set_policy() {
        let body = PutPreferencesRequest {
            default_constraint_policy: ConstraintPolicy::Any,
            brand: None,
            allowed_brands: vec!["Branston".into()],
            excluded_brands: vec![],
            preferred_size_value: None,
            preferred_size_basis: None,
            substitution_policy: SubstitutionPolicy::Any,
        };

        assert!(validate(body).unwrap().allowed_brands.is_empty());
    }

    #[test]
    fn size_value_requires_basis() {
        let body = PutPreferencesRequest {
            default_constraint_policy: ConstraintPolicy::Any,
            brand: None,
            allowed_brands: vec![],
            excluded_brands: vec![],
            preferred_size_value: Some(415.0),
            preferred_size_basis: None,
            substitution_policy: SubstitutionPolicy::Any,
        };
        assert_eq!(
            validate(body).unwrap_err(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    #[test]
    fn brand_list_dedupes_case_insensitively_and_drops_blanks() {
        assert_eq!(
            normalise_brand_list(vec![
                " Heinz ".into(),
                "heinz".into(),
                "".into(),
                "Branston".into()
            ]),
            vec!["Heinz".to_string(), "Branston".to_string()]
        );
    }

    #[test]
    fn reorder_puts_ranked_candidates_first_and_keeps_the_rest_stable() {
        use buywise_core::domain::{Retailer, Source};

        let scored = |id: &str, retailer: Retailer, name: &str| ScoredProduct {
            product: RetailerProduct {
                id: id.into(),
                source: Source::Manual,
                retailer,
                external_id: None,
                url: None,
                name: name.into(),
                brand: None,
                category: None,
                pack_size_text: None,
                size_value: None,
                size_unit: None,
                pack_count: None,
                form_factor: None,
                dietary_flags: None,
                first_seen_at: "t".into(),
                last_seen_at: "t".into(),
            },
            score: 50,
            signals: vec![buywise_core::product_matching::MatchSignal::Name],
        };

        let a = scored("a", Retailer::Tesco, "A");
        let b = scored("b", Retailer::Aldi, "B");
        let c = scored("c", Retailer::Dunnes, "C");
        let input = vec![a.clone(), b.clone(), c.clone()];

        // Model ranks B then A, omitting C.
        let order = vec![candidate_label(&b), candidate_label(&a)];
        let out = reorder_by_labels(input, &order);
        let ids: Vec<&str> = out.iter().map(|s| s.product.id.as_str()).collect();
        assert_eq!(ids, vec!["b", "a", "c"]);
    }

    #[test]
    fn no_usable_matches_is_warned_when_only_rejected() {
        let constraints = PreferencesConstraints {
            brand: None,
            allowed_brands: vec![],
            excluded_brands: vec![],
            preferred_size_value: None,
            preferred_size_basis: None,
            substitution_policy: SubstitutionPolicy::Any,
        };
        let rejected = MatchView {
            retailer_product_id: "rp-1".into(),
            status: MatchStatus::Rejected,
            matched_by: "user".into(),
            name: "Beans".into(),
            brand: None,
            retailer: Retailer::Tesco,
            size_text: None,
        };
        assert_eq!(
            warnings(ConstraintPolicy::Any, &constraints, &[rejected]),
            vec!["no_usable_matches".to_string()]
        );
    }
}
