# Household export format

`buywise-cli export-household` writes one JSON document holding every
household-scoped record BuyWise keeps for a household, plus the shared reference
rows those records point at. It is meant to be **readable without BuyWise**: no
internal ids are dropped, and every id a record references appears in the same
document.

```
buywise-cli export-household --db /var/lib/buywise/buywise.db \
  --household-id 00000000-0000-0000-0000-000000000001 \
  --out household.json      # omit --out to write to stdout
```

The command is read-only (it never runs migrations or changes the database) and
fails if the database file or the household does not exist, so a mistyped path or
id cannot produce a plausible-looking empty document.

## Downloading it from the app

A household member does not need an operator: the settings screen (**Your data**)
has a **Download export** button, which calls
`GET /api/household/{household_id}/export` (issue #136). The endpoint is
authenticated and requires the caller to be a member of that household; it
returns **the same document** as the CLI, as
`Content-Disposition: attachment; filename="buywise-household-<id>.json"`, so a
browser saves the file instead of rendering it. `GET /api/household` returns the
caller's `{ id, name, role }` and the app uses that id. For export, delete and
member administration access rules, see
[`docs/data-flows.md`](data-flows.md) §5.1a.

The document is built by `buywise_core::household_data::export_household`;
`crates/cli/src/commands/household.rs` and `crates/api/src/household.rs` are thin
front ends over the same function. Tests: `crates/cli/tests/household_data.rs`
(layout and links) and `crates/api/tests/household_endpoints.rs` (authorisation,
download, and the guarded delete). `EXPORT_FORMAT` and `EXPORT_FORMAT_VERSION`
are defined in `crates/core/src/household_data.rs`.

## Envelope

| Field | Type | Meaning |
| --- | --- | --- |
| `format` | string | Always `"buywise-household-export"`. |
| `format_version` | integer | `1`. Consumers must check `format` **and** `format_version`; an incompatible layout change bumps the version. |
| `exported_at` | string | Wall-clock timestamp of this export (RFC 3339 UTC). The only non-deterministic value. |
| `household` | object | `id`, `name`, `created_at`. Exactly one. |

Keys are serialised in a stable (sorted) order, but consumers must read **by key**,
not by position.

## Field types

Rows are emitted as objects keyed by their database column name, so the table
definitions in `migrations/` are the field reference. Types follow SQLite's
storage:

- `TEXT` / timestamps (RFC 3339 UTC) → JSON strings;
- `INTEGER` (ids are not integers here; this is counters, cents, `0`/`1`
  booleans, and nullable references) → JSON numbers, or `null`;
- `REAL` (item size values) → JSON numbers, or `null`;
- a column that is `NULL`, or stored in a type JSON cannot take, → `null`.

Money is always integer cents, never a float. Several columns are **JSON stored as
TEXT** and are exported verbatim as strings, not re-parsed, so nothing is lost:
`mutations.payload`, `mutations.response_json`, `list_entries.resolution_candidates_json`,
`list_entry_events.payload`, `recommendation_runs.plans_json`,
`recommendation_runs.basket_snapshot_json`, `recommendation_runs.price_evidence_json`,
`recommendation_runs.coverage_json`, `recommendation_runs.usual_profile_json`,
`savings_events.chosen_stores`, `purchase_transactions.source_payload`,
`purchase_transaction_revisions.source_payload`,
`purchase_transaction_revisions.lines_json`.

## Document layout

```
{
  "format": "buywise-household-export",
  "format_version": 1,
  "exported_at": "<RFC 3339>",
  "household":  { "id", "name", "created_at" },

  "members":    [ { "user_id", "display_name", "role", "joined_at" } ],
  "reference_data": {
    "categories":    [ { "id", "name_en", "name_es", "icon", "default_order", "is_fresh" } ],
    "subcategories": [ { "id", "category_id", "name_en", "name_es", "default_order" } ]
  },

  "catalogue":  [ {
    "id", "name_en", "name_es", "category_id", "subcategory_id",
    "default_constraint_policy", "created_at",
    "aliases":    [ { "id", "canonical_item_id", "alias", "language", "source", "created_at" } ],
    "constraint": { "id", "canonical_item_id", "brand", "allowed_brands", "excluded_brands",
                    "preferred_size_value", "preferred_size_basis", "min_size_value",
                    "min_size_basis", "max_size_value", "max_size_basis", "pack_count",
                    "form_factor", "dietary_flags", "substitution_policy", "updated_at" } | null
  } ],

  "preferences": {
    "settings":               { "convenience_threshold_2_cents", "convenience_threshold_3_cents", "updated_at" } | null,
    "stores":                 [ { "retailer", "status", "nearby", "updated_at" } ],
    "store_category_rules":   [ { "retailer", "category_id", "subcategory_id", "allowed", "updated_at" } ],
    "store_category_orders":  [ { "store", "category_id", "display_order", "updated_at" } ],
    "purchase_dismissals":    [ { "id", "canonical_item_id", "dismissed_by", "dismissed_at" } ]
  },

  "product_matches":   [ { "id", "canonical_item_id", "retailer_product_id", "status",
                           "matched_by", "created_at", "updated_at" } ],
  "retailer_products": [ { "id", "source", "retailer", "external_id", "url", "name",
                           "brand", "category", "pack_size_text", "size_value", "size_unit",
                           "pack_count", "form_factor", "dietary_flags",
                           "first_seen_at", "last_seen_at" } ],
  "match_feedback":    [ { "id", "product_match_id", "user_id", "action", "created_at" } ],

  "lists": [ {
    "id", "name", "status", "current_revision", "created_at", "archived_at",
    "entries":            [ { "id", "list_id", "canonical_item_id", "resolution_status",
                              "resolution_candidates_json", "raw_text", "display_name",
                              "added_by", "quantity", "status", "bought_by", "bought_at",
                              "effective_constraint_hash", "last_updated_revision",
                              "created_at", "updated_at" } ],
    "entry_events":       [ { "id", "entry_id", "list_id", "user_id", "event_type",
                              "payload", "created_at" } ],
    "mutations":          [ { "id", "client_mutation_id", "device_id", "user_id", "list_id",
                              "entity_type", "entity_id", "operation_type", "payload",
                              "base_revision", "created_at_client", "received_at_server",
                              "server_revision", "status", "response_json" } ],
    "recommendation_runs":[ { "id", "list_id", "run_at", "offer_snapshot_date", "plans_json",
                              "recommended_plan", "convenience_threshold_2",
                              "convenience_threshold_3", "snapshot_version",
                              "basket_snapshot_json", "price_evidence_json", "coverage_json",
                              "usual_profile_json", "snapshot_digest" } ],
    "savings_events":     [ { "id", "list_id", "recommendation_run_id", "baseline_cost_cents",
                              "recommended_cost_cents", "theoretical_minimum_cost_cents",
                              "chosen_basket_cost_cents", "chosen_plan", "chosen_stores",
                              "gross_saving_cents", "accepted_savings_cents",
                              "rejected_savings_cents", "travel_cost_cents",
                              "subscription_cost_alloc_cents", "net_saving_cents",
                              "status", "created_at" } ]
  } ],

  "purchase_events": [ { "id", "canonical_item_id", "list_id", "entry_id", "source",
                         "occurred_at", "recorded_at" } ],

  "purchase_transactions": [ {
    "id", "household_id", "retailer", "purchase_date", "source_kind",
    "source_transaction_id", "transaction_identity", "identity_kind", "fingerprint",
    "currency", "total_cents", "revision", "source_payload", "created_at", "updated_at",
    "lines":     [ { "id", "transaction_id", "line_ordinal", "original_text", "name",
                     "brand", "quantity", "size_value", "size_unit", "unit_price_cents",
                     "line_total_cents", "line_discount_cents", "deposit_cents",
                     "retailer_product_id", "canonical_item_id", "match_status",
                     "revision", "created_at" } ],
    "revisions": [ { "id", "transaction_id", "revision", "retailer", "purchase_date",
                     "currency", "total_cents", "source_payload", "lines_json",
                     "superseded_at" } ]
  } ],
  "receipt_images": [ { "id", "uploaded_by", "uploaded_at", "expires_at",
                        "content_type", "byte_size", "sha256_hex", "processing" } ]
}
```

`receipt_images` is metadata only: one entry per stored receipt photo, ordered by
upload time. `uploaded_by` is null if the uploader's account has since been
deleted. `expires_at` is the exact moment, 30 days after upload, when list and
view access stops; the daily expiry sweep then reclaims the image and row. The
retention behavior is owned by [`docs/data-flows.md`](data-flows.md) §4.4.
`content_type` is detected from the file signature, not the type the client
claimed; `processing` is the per-upload choice, `local` (see §4 there). The image
itself is not in the document: it stays on the server, and the app retrieves it
by this `id` until it expires, so a household that wants the pictures saves them
from the Receipts page before then.

## What is not in the document

- **Credentials.** Password hashes are never exported; `members` carries a member's
  `user_id`, `display_name`, role and join date only. Session tokens and CSRF
  tokens are not exported.
- **Other households.** Every array is filtered to the requested household. Shared
  reference data (`reference_data.categories`, `reference_data.subcategories`,
  `retailer_products`) is included because it is global and small, or because the
  household's own rows point at it.
- **Price history.** `product_offers`, `source_runs` and `api_usage_events` are the
  shared retail-price catalogue and aggregate counters, not household records;
  they are not exported. `retailer_products` is limited to the products this
  household's own `product_matches` reference.
- **Receipt image bytes.** Each stored photo is a file on the server, named by an
  internal `storage_key`; neither the bytes nor that key are in the document.
  `receipt_images` (see the layout above) carries the image **metadata** instead.

## Reading the links

The document is internally closed — the ids a record references resolve inside it:

| From | Field | To |
| --- | --- | --- |
| `catalogue[].category_id` / `subcategory_id` | → | `reference_data.categories[].id` / `reference_data.subcategories[].id` |
| `catalogue[].aliases[].canonical_item_id` | → | `catalogue[].id` |
| `catalogue[].constraint.canonical_item_id` | → | `catalogue[].id` |
| `lists[].entries[].canonical_item_id` | → | `catalogue[].id` (null until resolved) |
| `lists[].entries[].added_by` / `bought_by` | → | `members[].user_id` |
| `lists[].entry_events[].entry_id` | → | `lists[].entries[].id` |
| `lists[].mutations[].entity_id` | → | `lists[].entries[].id` for entry operations |
| `lists[].recommendation_runs[].id` | ← | `lists[].savings_events[].recommendation_run_id` |
| `product_matches[].canonical_item_id` | → | `catalogue[].id` |
| `product_matches[].retailer_product_id` | → | `retailer_products[].id` |
| `match_feedback[].product_match_id` | → | `product_matches[].id` |
| `purchase_events[].canonical_item_id` | → | `catalogue[].id` |
| `purchase_events[].list_id` / `entry_id` | → | `lists[].id` / `lists[].entries[].id` |
| `purchase_transactions[].lines[].transaction_id` / `revisions[].transaction_id` | → | `purchase_transactions[].id` |
| `purchase_transactions[].lines[].retailer_product_id` | → | `retailer_products[].id` (null until matched) |
| `purchase_transactions[].lines[].canonical_item_id` | → | `catalogue[].id` (null until matched) |
| `receipt_images[].uploaded_by` | → | `members[].user_id` (or `null` if the account was deleted) |
| `preferences.purchase_dismissals[].canonical_item_id` | → | `catalogue[].id` |
| `preferences.*[].category_id` / `subcategory_id` | → | `reference_data` |

All list-scoped arrays also carry `list_id`, so a flat reader can regroup them
without nesting.

## Stability

`format_version` is the contract. Adding a field is a compatible change; removing
or retyping one bumps the version. Consumers should ignore unknown fields and read
by key.
