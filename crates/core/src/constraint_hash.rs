//! `effective_constraint_hash` computation for list entries (P1-T4).
//!
//! Every `list_entries` row carries a NOT NULL `effective_constraint_hash`. It is
//! the dedup key (together with `list_id` + `canonical_item_id`) for the partial
//! unique index `uq_list_entries_active_dedup`. Three cases, per `docs/design.md`
//! §7 and `docs/implementation-plan.md` (P1-T4):
//!
//!   * `canonical_item_id IS NULL` (entry not yet resolved)
//!     → the sentinel `unresolved:<entry_id>`. The entry id makes the value
//!     unique per row, so two unresolved adds never collide on the index and are
//!     never merged (the index predicate also excludes `canonical_item_id IS
//!     NULL`, so this is belt-and-braces).
//!   * resolved with a genuinely empty constraint set
//!     → the sentinel `default` (NOT an empty string and NOT NULL — SQLite's
//!     UNIQUE allows multiple NULLs, which would silently defeat dedup).
//!   * resolved with real constraints
//!     → the lowercase SHA-256 hex of the **sorted-key JSON** of the constraint
//!     set. Sorting keys makes the serialisation canonical so two clients (and
//!     the server) that hold the same constraints always produce the same hash.
//!
//! The hash input deliberately covers only the *semantic* constraint fields; it
//! never includes timestamps, user ids, or the entry id (those are not part of
//! "the same item with the same constraints"). The caller passes the already
//! unit-normalised constraint set; this module only canonicalises and hashes it.

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// The sentinel prefix for an unresolved entry's hash: `unresolved:<entry_id>`.
pub const UNRESOLVED_PREFIX: &str = "unresolved:";

/// The sentinel value for a resolved entry with an empty constraint set.
pub const DEFAULT_HASH: &str = "default";

/// Compute the `effective_constraint_hash` for a (possibly new) list entry.
///
/// * `canonical_item_id == None` → `unresolved:<entry_id>` (the entry is not yet
///   resolved; the row is never a dedup candidate).
/// * resolved, `constraints` empty / `None` → [`DEFAULT_HASH`] (`"default"`).
/// * resolved, `constraints` non-empty → SHA-256 hex of the sorted-key JSON.
///
/// `entry_id` is only consulted for the unresolved sentinel; it never enters the
/// hashed material so two resolved entries with identical constraints collide on
/// the dedup index (and therefore merge).
pub fn effective_constraint_hash(
    entry_id: &str,
    canonical_item_id: Option<&str>,
    constraints: Option<&Value>,
) -> String {
    if canonical_item_id.is_none() {
        return format!("{UNRESOLVED_PREFIX}{entry_id}");
    }

    match normalise_constraints(constraints) {
        // Resolved with a genuinely empty constraint set.
        None => DEFAULT_HASH.to_string(),
        Some(canonical) => hash_canonical_json(&canonical),
    }
}

/// Turn an arbitrary constraints payload into its canonical form, or `None` if it
/// carries no semantic constraints.
///
/// "Empty" means: absent, JSON `null`, an empty object, or an object whose every
/// value is itself empty (`null`, `[]`, `{}`, or `""`). Such a set hashes to the
/// `default` sentinel rather than a real digest, so an item added with no
/// meaningful constraints dedups against the same item added with none.
fn normalise_constraints(constraints: Option<&Value>) -> Option<Value> {
    let value = constraints?;
    let canonical = canonicalise(value);
    if is_semantically_empty(&canonical) {
        None
    } else {
        Some(canonical)
    }
}

/// Recursively rebuild a JSON value with every object's keys sorted, so the
/// serialised form is canonical regardless of input key order. Arrays preserve
/// order (an ordered set is itself semantic); scalars are returned unchanged.
fn canonicalise(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sorted: Map<String, Value> = Map::new();
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for key in keys {
                sorted.insert(key.clone(), canonicalise(&map[key]));
            }
            Value::Object(sorted)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonicalise).collect()),
        other => other.clone(),
    }
}

/// True if a (canonicalised) value carries no semantic constraint information.
fn is_semantically_empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.values().all(is_semantically_empty),
        // A bool or number is always meaningful.
        _ => false,
    }
}

/// Lowercase SHA-256 hex of a canonical JSON value's compact serialisation.
///
/// `serde_json::to_string` on an already-canonicalised (sorted-key) value yields
/// stable bytes, so the digest is deterministic for equal constraint sets.
fn hash_canonical_json(canonical: &Value) -> String {
    let serialised = serde_json::to_string(canonical)
        .expect("canonical JSON re-serialisation is infallible for in-memory Value");
    let digest = Sha256::digest(serialised.as_bytes());
    hex::encode(digest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unresolved_entry_uses_entry_id_sentinel() {
        let h = effective_constraint_hash("entry-123", None, None);
        assert_eq!(h, "unresolved:entry-123");

        // Constraints are ignored entirely while unresolved.
        let h2 = effective_constraint_hash("entry-123", None, Some(&json!({ "brand": "Tesco" })));
        assert_eq!(h2, "unresolved:entry-123");
    }

    #[test]
    fn unresolved_sentinel_is_unique_per_entry() {
        let a = effective_constraint_hash("entry-a", None, None);
        let b = effective_constraint_hash("entry-b", None, None);
        assert_ne!(a, b, "distinct entries must never share an unresolved hash");
    }

    #[test]
    fn resolved_no_constraints_is_default_not_empty_or_null() {
        // No constraints object at all.
        assert_eq!(
            effective_constraint_hash("entry-1", Some("item-1"), None),
            "default"
        );
        // Explicit JSON null.
        assert_eq!(
            effective_constraint_hash("entry-1", Some("item-1"), Some(&Value::Null)),
            "default"
        );
        // Empty object.
        assert_eq!(
            effective_constraint_hash("entry-1", Some("item-1"), Some(&json!({}))),
            "default"
        );
        // Object whose every field is empty.
        assert_eq!(
            effective_constraint_hash(
                "entry-1",
                Some("item-1"),
                Some(&json!({ "brand": null, "dietary": [], "pack_count": null }))
            ),
            "default"
        );
    }

    #[test]
    fn resolved_with_constraints_is_sha256_hex() {
        let h = effective_constraint_hash(
            "entry-1",
            Some("item-1"),
            Some(&json!({ "brand": "Tesco" })),
        );
        assert_eq!(h.len(), 64, "SHA-256 hex is 64 chars");
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(h, "default");
    }

    #[test]
    fn hash_is_key_order_independent() {
        let a = effective_constraint_hash(
            "entry-1",
            Some("item-1"),
            Some(&json!({ "brand": "Tesco", "pack_count": 2 })),
        );
        let b = effective_constraint_hash(
            "entry-2",
            Some("item-1"),
            Some(&json!({ "pack_count": 2, "brand": "Tesco" })),
        );
        assert_eq!(a, b, "key order must not change the hash");
    }

    #[test]
    fn different_constraints_hash_differently() {
        let milk = effective_constraint_hash("e1", Some("item-1"), Some(&json!({ "brand": null })));
        let lactose_free = effective_constraint_hash(
            "e2",
            Some("item-1"),
            Some(&json!({ "dietary": ["lactose_free"] })),
        );
        // "milk" with no real constraints is the default sentinel; "lactose-free
        // milk" carries a real constraint and hashes to a digest → two rows.
        assert_eq!(milk, "default");
        assert_ne!(lactose_free, "default");
        assert_ne!(milk, lactose_free);
    }

    #[test]
    fn empty_string_value_and_nested_empty_object_hash_to_default() {
        // Empty string value is non-semantic.
        assert_eq!(
            effective_constraint_hash("e1", Some("item-1"), Some(&json!({ "brand": "" }))),
            "default"
        );
        // Nested empty object.
        assert_eq!(
            effective_constraint_hash("e1", Some("item-1"), Some(&json!({ "size": {} }))),
            "default"
        );
        // An empty array value is likewise non-semantic.
        assert_eq!(
            effective_constraint_hash("e1", Some("item-1"), Some(&json!({ "dietary": [] }))),
            "default"
        );
    }
}
