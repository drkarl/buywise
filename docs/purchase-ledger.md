# Purchase ledger and import format

The purchase ledger is BuyWise's record of **what the household actually
bought**: one row per purchase transaction and one per line, household-scoped,
with quantities and amounts. It is fed today by structured imports (a retailer's
own export) and, later, by reading receipts; both feed the same model
(issue [#148](https://github.com/drkarl/buywise/issues/148), roadmap #114).

It is deliberately separate from `purchase_events`: that table records only the
*check-off timing* of a canonical item and has no quantity, product or amount, so
it is not a money record. Nothing in this feature changes `purchase_events` or
the suggestion logic.

This document is the complete import-format contract: an importer for any
retailer can be written from this page alone, and
`crates/cli/tests/purchase_ledger.rs` asserts that the worked example below
parses.

## Contents

1. [Importing](#importing) — CLI and API entry points
2. [Document format](#document-format) — the versioned JSON envelope
3. [Transaction fields](#transaction-fields)
4. [Line fields](#line-fields)
5. [Identity, idempotency and revisions](#identity-idempotency-and-revisions)
6. [Errors](#errors)
7. [Limits](#limits)
8. [Worked example](#worked-example)
9. [Reading and deleting a ledger](#reading-and-deleting-a-ledger)
10. [Export and household deletion](#export-and-household-deletion)

## Importing

```
buywise-cli import-purchases --db /var/lib/buywise/buywise.db \
  --household-id 00000000-0000-0000-0000-000000000001 \
  --file tesco-2026-05.json
```

The command reads one JSON file and imports **every transaction in it in a
single database transaction**: either all are written (or found unchanged) or
none is. It does not run migrations and requires the database file to exist, so
a typo in `--db` fails loudly. The household must already exist.

The same core logic backs the authenticated API
(`crates/api/src/purchases.rs`):

| Request | Method | Who | Purpose |
| --- | --- | --- | --- |
| `/api/household/{household_id}/purchases` | `POST` | any member | import a document (the body below) |
| `/api/household/{household_id}/purchases` | `GET` | any member | list transactions (no lines) |
| `/api/household/{household_id}/purchases/{transaction_id}` | `GET` | any member | one transaction with its lines and revisions |
| `/api/household/{household_id}/purchases/{transaction_id}` | `DELETE` | any member | delete one transaction (lines and revisions go with it) |

Every endpoint requires a live session and membership of the named household: a
member of another household is refused with `403`, so one household can never
read, import into or delete another's ledger. A transaction id belonging to
another household reads and deletes as `404`.

A successful import returns the per-transaction results and the totals:

```json
{
  "results": [
    { "transaction_id": "…", "revision": 1, "status": "inserted" }
  ],
  "inserted": 1,
  "updated": 0,
  "unchanged": 0
}
```

## Document format

One JSON object. **Unknown fields are rejected at every level** — a misspelled
key is an error, never silently ignored. All values are required unless marked
optional.

| Field | Type | Required | Meaning |
| --- | --- | --- | --- |
| `format_version` | integer | yes | Must be `1` in this build. Any other value is refused. |
| `transactions` | array | yes | At least one transaction, at most 1000. |

The layout is intentionally a version number without a `format` name: the
document is delivered to a known endpoint, so the version is the whole contract.
An incompatible layout change bumps `format_version`.

## Transaction fields

| Field | Type | Required | Meaning |
| --- | --- | --- | --- |
| `retailer` | string | yes | Retailer token, e.g. `tesco`, `lidl`. Stored trimmed and lowercased. |
| `purchase_date` | string | yes | Calendar date `YYYY-MM-DD` in the shop's own timezone. |
| `total_cents` | integer | yes | Transaction total in integer cents. Never a float. |
| `lines` | array | yes | The transaction's lines (may be empty), at most 1000. |
| `source_transaction_id` | string | optional | The retailer's own transaction id. See [identity](#identity-idempotency-and-revisions). |
| `currency` | string | optional | ISO-4217 code, three uppercase letters. Defaults to `EUR`. |
| `source_payload` | any JSON | optional | The original source payload to store verbatim. Defaults to the transaction object exactly as imported. |

## Line fields

| Field | Type | Required | Meaning |
| --- | --- | --- | --- |
| `original_text` | string | yes | The line text exactly as the source printed it. Kept verbatim. |
| `line_total_cents` | integer | yes | Line total in integer cents. |
| `name` | string | optional | Product name, when the source separated one from the text. |
| `brand` | string | optional | Brand, when known. |
| `quantity` | number | optional | Quantity bought; a decimal is allowed (e.g. `0.75` kg). Must be greater than zero. |
| `size_value` | number | optional | Pack size value (non-negative). |
| `size_unit` | string | optional | Pack size unit, e.g. `l`, `g`. |
| `unit_price_cents` | integer | optional | Unit price in integer cents. |
| `line_discount_cents` | integer | optional | Discount on the line in integer cents; non-negative. Defaults to `0`. |
| `deposit_cents` | integer | optional | Deposit/recycling charge in integer cents; non-negative. Defaults to `0`. |
| `retailer_product_id` | string | optional | Link to an existing `retailer_products` row, when the source knows one. |
| `canonical_item_id` | string | optional | Link to an existing `canonical_items` row owned by the importing household. |
| `match_status` | string | optional | `unmatched` (default) or `matched`; informational and independent of the optional product ids. |

Optional fields that are absent, `null` or an empty/whitespace-only string are
stored as `NULL` rather than as a guessed value. A line with no product link is
**kept** and defaults to `match_status = "unmatched"` — lines are never dropped
or assigned a status from their product links. A supplied `retailer_product_id`
must resolve to a row in `retailer_products`; a supplied `canonical_item_id`
must resolve to a row in `canonical_items` belonging to the importing household.
A failed or cross-household link is a `422` validation error at that field
path, and no transaction in the import document is written. `match_status` is accepted
independently of either optional product id.

Money is always integer cents, never a float. `quantity` and `size_value` are the
only numbers that may be fractional.

## Identity, idempotency and revisions

Every transaction has an **identity**, which is decisive for idempotency:

- If `source_transaction_id` is supplied, that is the identity
  (`identity_kind = "source_id"`).
- Otherwise the identity is a **stable fingerprint** computed by this build
  (`identity_kind = "fingerprint"`).

The stored unique key is `(household_id, retailer, "import", identity)`.
Retailer names are trimmed and lowercased before they are used in this key, so
`Tesco` and `tesco` identify the same retailer. Source transaction IDs are
scoped to their retailer: two retailers may use the same ID without replacing
each other's purchase. Re-importing a document is therefore idempotent: the
same transaction yields **one transaction row and one set of lines**, and totals
are never summed over duplicate links.

### Fingerprint algorithm

```
fingerprint = "pl1:" + lowercase_hex(SHA-256(utf8(inputs)))
inputs      = retailer + "\n" + purchase_date + "\n" + total_cents + "\n" + join(line_texts, "\n")
```

where `retailer` is already trimmed and lowercased, `purchase_date` is the
canonical `YYYY-MM-DD`, `total_cents` is its decimal rendering, and `line_texts`
are the `original_text` values in ordinal order joined by `\n`. The `pl1:` prefix
versions the rule.

### Re-import: unchanged vs correction

For a matching identity the core compares the stored content (retailer, purchase
date, currency, total and every line field) with the imported content:

- **Identical content** → a no-op. Nothing is written; the result reports
  `unchanged` and the stored revision.
- **Different content** → a correction. Before the live row is updated, the
  previous revision is snapshotted into `purchase_transaction_revisions` (its
  header fields, source payload and lines as JSON). The live row's `revision` is
  bumped, its lines are replaced, and the result reports `updated`. The earlier
  revision is never overwritten silently.

### Correcting fields that are part of the fingerprint

When the identity is a computed fingerprint, the fingerprint *is* retailer +
date + total + line texts. Changing one of those changes the identity, so it
creates a new transaction rather than a correction. To correct across those
fields, supply `source_transaction_id`: the identity then survives a total or
date change and the import becomes a revision of the same transaction. Use it
whenever the source provides a real transaction id.

## Errors

Malformed input is refused before anything is written. The API returns `422`
with a body listing every problem, each entry naming the field path:

```json
{
  "errors": [
    "transactions[0].lines[1].line_total_cents: expected an integer",
    "transactions[0].currency: expected a three-letter ISO-4217 code"
  ]
}
```

The CLI prints the same messages and exits non-zero. Problems reported per field:
unknown field, missing required field, wrong type, empty required string,
non-`YYYY-MM-DD` date, non-integer money, negative size/discount/deposit,
non-positive quantity, an unsupported `match_status` or `format_version`, an
unknown retailer product, and a missing or other-household canonical item. Link
validation happens before any transaction is written. All input and link
problems in a document are reported with field paths, so a whole file can be
fixed at once.

## Limits

- One document: at most **1000 transactions** (`MAX_TRANSACTIONS`).
- One transaction: at most **1000 lines** (`MAX_LINES_PER_TRANSACTION`).
- HTTP bodies: at most **1 MiB** (`MAX_IMPORT_BODY_BYTES`), enforced by the
  import route.

## Worked example

<!-- purchase-ledger-example:start -->
```json
{
  "format_version": 1,
  "transactions": [
    {
      "source_transaction_id": "2026-05-01-0042",
      "retailer": "tesco",
      "purchase_date": "2026-05-01",
      "currency": "EUR",
      "total_cents": 858,
      "lines": [
        {
          "original_text": "Tesco Own Brand Milk 1L",
          "name": "Milk",
          "brand": "Tesco",
          "quantity": 2,
          "size_value": 1,
          "size_unit": "l",
          "unit_price_cents": 129,
          "line_total_cents": 258,
          "line_discount_cents": 0,
          "deposit_cents": 0,
          "match_status": "unmatched"
        },
        {
          "original_text": "Mystery Item",
          "line_total_cents": 600
        }
      ]
    },
    {
      "source_transaction_id": "2026-05-01-0042",
      "retailer": "lidl",
      "purchase_date": "2026-05-01",
      "currency": "EUR",
      "total_cents": 199,
      "lines": [
        { "original_text": "Lidl Bread", "line_total_cents": 199 }
      ]
    }
  ]
}
```
<!-- purchase-ledger-example:end -->

The Tesco and Lidl transactions may share a source ID because retailer is part
of the idempotency key. In the Tesco transaction, the mystery line supplies
only the two required fields: its product is unknown, so it is stored with
`match_status = "unmatched"` and every other field `NULL`. It stays in the
ledger; a later matching pass may link it.

## Reading and deleting a ledger

The API returns transaction summaries from `GET .../purchases` (id, retailer,
purchase date, source id and identity kind, fingerprint, currency, total,
revision, line count and timestamps) and a full transaction from
`GET .../purchases/{id}`: the summary plus its current `lines` and its
`revisions` (each with the superseded header, source payload, `lines` as JSON and
`superseded_at`). `DELETE .../purchases/{id}` removes one transaction; its lines
and revision history are removed with it.

The core also exposes `household_total_cents`, the sum of the household's live
ledger rows; the identity key means it cannot count one purchase twice.

## Export and household deletion

A household's ledger — every `purchase_transactions` row with its `lines` and
`revisions` nested — is part of the documented household export
([`docs/household-export.md`](household-export.md)) and is removed by the
household delete, so the household's own data controls cover it like every other
household-scoped record.
