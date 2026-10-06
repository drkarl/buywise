# Household data flows, external processing and retention

This document answers, for the household that owns a BuyWise deployment: **which
data lives where, what leaves the machine, who can read it, how long it is kept,
and how to get it out or delete it.**

It is derived from the code and deployment configuration at the time of writing,
not from intent. Every claim points at the file that makes it true; where a claim
about a third party (Cloudflare, a model provider) is not verifiable from this
repository it is marked as such. Roadmap #114, issue #119.

**Not a goal.** This is a private single-household deployment, not a public SaaS
compliance programme. There is deliberately no consent-management layer,
end-to-end-encrypted sync, or receipt wallet, and — importantly — **no claim that
a hosted operator cannot read server-side data.** Anyone with root on the server,
or access to the operator's R2 credentials, can read every household record in
plaintext. The point of this document is that the household can *see* that and
*control* the one optional external flow.

## 1. Where BuyWise runs

One Hetzner CAX11 (ARM64) host, one SQLite file, three systemd units
(`deploy/README.md`, `deploy/systemd/`):

| Unit | Role | Data it touches |
| --- | --- | --- |
| `buywise.service` | serves the SvelteKit PWA and the `/api/*` Axum API on one port | `/var/lib/buywise/buywise.db` (read/write) |
| `litestream.service` | continuously replicates that file to Cloudflare R2 | the same file (read) → R2 bucket `buywise-backup`, prefix `db` |
| `buywise-ingest.timer` → `buywise-ingest.service` | weekly oneshot price ingest | the same file (price tables) + outbound retailer requests |

The API and the weekly ingest are the only writers; Litestream and the API are
the only processes that read the whole database. `/var/lib/buywise` is `0750`
owned by the no-login `buywise` user, so a local unprivileged account cannot read
the database file (`deploy/setup.sh:47-49`).

## 2. Data-flow inventory

### 2.1 Device (browser) — flows that never leave the device

The frontend is a client-only SPA (SvelteKit `adapter-static`, `ssr=false`,
`frontend/src/routes/+layout.ts`). Nothing here is sent anywhere except as an
explicitly listed request in §2.5–§2.6.

| Store | Key / tables | Content | Purpose | Access | Retention |
| --- | --- | --- | --- | --- | --- |
| IndexedDB (Dexie), database `buywise` | `list_entries` | every list line: `raw_text`, `display_name`, `quantity`, `status`, `bought_by`, `bought_at`, `canonical_item_id`, `resolution_status`, revision (`frontend/src/lib/db/dexie.ts:3-31,129-133`) | offline list view and check-off | any script running on the origin; anyone with access to this browser profile | until the browser clears site data; not server-controlled |
| IndexedDB | `mutation_queue` | queued edits: idempotency id, device id, list id, operation, payload, base revision (`dexie.ts:33-45`) | offline-first sync, drained to `POST /api/mutations` | as above | until confirmed/removed |
| IndexedDB | `list_meta` | list name, `current_revision`, `last_synced_at` (`dexie.ts:47-52`) | sync cursor | as above | until site data cleared |
| IndexedDB | `categories`, `subcategories`, `store_category_orders`, `item_categories` | cached shared catalogue + this household's per-store aisle order and item→category map (`dexie.ts:59-112,143-162`) | offline aisle ordering / category grouping | as above | until site data cleared |
| localStorage | `buywise_device_id` (`frontend/src/lib/db/device.ts:8`) | per-browser UUID sent as `device_id` on every mutation | idempotency / device attribution | any script on the origin | until site data cleared |
| localStorage | `buywise:shopping:store` (`frontend/src/lib/stores/shopping.ts:115`) | last selected retailer | UI convenience | as above | until site data cleared |
| localStorage | `buywise:trip:<listId>` (`frontend/src/lib/shopping/chosenTrip.ts:33`) | the chosen optimiser plan: which products at which store for this list, plus a save timestamp | shows the trip/shopping view without re-optimising | as above | until the plan is replaced or the browser is cleared |
| Cache Storage | precache (app shell) | built JS/CSS/HTML | PWA offline shell | anyone using the browser | until the service worker updates |
| Cache Storage | `buywise-static-assets` (`frontend/vite.config.ts:71`) | same-origin images/fonts, max 60 entries / 30 days | PWA runtime cache | as above | 30 days |
| Cache Storage | `buywise-api-reads` (`vite.config.ts:128`) | **authenticated** responses of `GET /api/list/:id` and `GET /api/items` — i.e. list contents and the catalogue; max 50 entries / 1 day | offline reads | as above | 1 day, and purged on login/logout (`frontend/static/sw-purge-on-auth.js`) |
| Cookies | `session` | random session token; server stores only its SHA-256 | authentication | HttpOnly + Secure + SameSite=Strict, so not readable by page script (`crates/api/src/auth.rs:227-229`) | until logout or `sessions.expires_at` |
| Cookies | `csrf` | CSRF token | lets the SPA echo `X-CSRF-Token` on writes | **JS-readable by design**, Secure + SameSite=Strict (`auth.rs:232-235`) | until logout |

No `sessionStorage`, no analytics or telemetry SDK, no error-reporting service, no
CDN, no third-party script, no web font. Every asset is served from the same
origin. Credentials are never stored in IndexedDB or localStorage.

### 2.2 Server: the SQLite database

One file, `/var/lib/buywise/buywise.db` (override with `DATABASE_URL`). All
household state lives here. `crates/api/src/main.rs` connects, runs the migrations
in `migrations/`, and serves; the CLI and the weekly ingest open the same file.

**Household-scoped tables** (rows carry, or transitively belong to, one
`household_id`):

| Table | Content | Purpose |
| --- | --- | --- |
| `households`, `household_members`, `users`, `sessions` | household name; membership and role; email, display name, argon2id password hash; session rows (token hash + CSRF) | identity and login |
| `canonical_items`, `item_aliases`, `item_constraints` | the household's catalogue: names (EN/ES), category, learned aliases, per-item brand/size/substitution constraints | resolution and pricing rules |
| `household_settings`, `household_store_settings`, `household_store_category_rules`, `store_category_orders` | convenience thresholds, per-store active/occasional/off + nearby, per-category store rules, aisle order | preferences that shape plans |
| `shopping_lists`, `list_entries`, `list_entry_events` | lists, their lines (raw text, display name, quantity, bought state), and the per-entry audit trail | the shared list |
| `mutations` | append-only offline-sync log: idempotency keys, device id, operation, payload, and the stored response | sync, replay |
| `recommendation_runs`, `savings_events` | optimiser runs (`plans_json`) and realised/estimated savings | savings history |
| `product_matches`, `match_feedback` | accepted/rejected/suggested canonical↔retailer-product links, and accept/reject/prefer/undo feedback | price matching + learning |
| `purchase_events`, `purchase_dismissals` | one row per observed purchase (`check_off` today), and per-item "not now" dismissals | purchase suggestions |
| `purchase_transactions`, `purchase_lines`, `purchase_transaction_revisions` | imported purchase transactions with their lines (quantity, size, unit price, line total, deposit, optional product/item links) and their superseded revisions | the purchase ledger — what the household actually bought (issue #148) |

**Shared, not household-identifiable tables** (no `household_id`, or keyed by a
global id): `categories`, `subcategories`, `retailer_products`, `product_offers`,
`source_runs`, `api_usage_events`. These hold the retail price catalogue and
aggregate API counters. They are *not* deleted by the household delete command
(§5) and are not included in the household export beyond the rows the household's
own records point at.

- **Access.** The `buywise` service user and root on the host; in practice, anyone
  who can read the file can read every household record. There is no
  application-level encryption of the database at rest.
- **Retention.** Indefinite, until the household deletes it (§5) or the operator
  removes the file. Lists are archived, not expired; purchase history accumulates
  by design (`migrations/013_purchase_suggestions.sql`).

### 2.3 Routine logs (journald)

`buywise.service` sets `StandardOutput=journal`/`StandardError=journal` and the API
initialises a `tracing-subscriber` at `RUST_LOG` (default `info`)
(`crates/api/src/main.rs:16-23,48`).

What is written today — every `tracing::*` call site in `crates/api/src` was
inspected for this document:

- one startup line naming the bind address (`main.rs:48`);
- error logs per failed route/handler carrying the `sqlx`/IO error and a static
  context label (`lists.rs:356`, `mutations.rs`, `optimise.rs:320`,
  `preferences.rs:219`, `settings.rs:324`, `savings.rs:391`, `sse.rs`,
  `auth.rs:107,165,186`, `match_feedback.rs:106`, `middleware.rs:155`);
- a handful of resolution/SSE lines keyed by `entry_id` and `list_id`
  (`resolution.rs:75,200,238,318,490,775,782`, `mutations.rs:1925`);
- the weekly ingest's own unit output (`journalctl -u buywise-ingest`).

**No household content is logged.** List terms, display names, quantities, emails,
session tokens, catalogue names and model output are all absent from routine logs.
This was made true by this change: the LLM fallback's two exceptional-path log
lines (`crates/api/src/resolution.rs`) previously attached the raw list term and
the model's answer; they now carry only `entry_id`. Database error messages name
tables, columns and constraint names (e.g. `UNIQUE constraint failed:
item_aliases.household_id, ...`), never stored values.

- **Access.** Root, and any account in the `systemd-journal` group, on the host.
- **Retention.** journald's own policy (`/etc/systemd/journald.conf`:
  `SystemMaxUse`, `MaxRetentionSec`) — not configured by this repo. `journalctl
  --vacuum-time=` prunes it manually.
- **Not present:** no analytics service, no crash reporter, no metrics endpoint,
  no request-body logging.

### 2.4 Backups: Litestream → Cloudflare R2

`deploy/litestream.yml` replicates `/var/lib/buywise/buywise.db` to the
S3-compatible Cloudflare R2 bucket `buywise-backup`, prefix `db`.

- **What leaves the server:** the *whole database file*, continuously — every
  household record in §2.2 including members' emails and password hashes. This is
  the largest flow in the system and the one most worth understanding.
- **Purpose:** disaster recovery (`deploy/README.md`, "Disaster recovery").
- **Transport and authorisation:** HTTPS to
  `https://<ACCOUNT_ID>.r2.cloudflarestorage.com`, authenticated with an R2 S3
  access key. The keys are **not** in the repository: they live in
  `/etc/buywise/buywise.env` (`root:root`, mode `600`, `deploy/setup.sh:132-133`)
  and are injected into `litestream.service` by systemd's `EnvironmentFile`
  (`deploy/systemd/litestream.service`). Because they are in that process's
  environment, the `buywise` user can read them from `/proc/<litestream-pid>/environ`.
- **Encryption:** TLS in transit. **At rest there is no application-level or
  customer-managed encryption**: `age` client-side encryption is not configured
  (`deploy/litestream.yml` has no `age:` block; Litestream's `age` support defaults
  to off), so the objects rely on R2's own server-side encryption and Cloudflare's
  key custody. Confirm Cloudflare's current R2 at-rest guarantees before relying
  on them — that verification is not something this repository can make.
- **Access:** whoever holds the R2 API token (and Cloudflare account holders with
  access to the bucket).
- **Retention:** `deploy/litestream.yml` sets `retention: 168h` (7 days) on the
  R2 replica, so snapshots and WAL files are kept for 7 days, checked hourly, with
  a fresh snapshot written each time retention runs; Litestream's own default
  (what applied before this was set) would be `24h`
  ([Litestream v0.3 config reference](https://litestream.io/v0.3/reference/config)).
  Practically: the replica holds the last 7 days of database history, so a row
  deleted on the server can still exist in R2 for up to about 7 days. See §5.3.
- No automatic restore verification is configured (`validation-interval` is unset,
  hence disabled). The operator's manual restore drill is in `deploy/README.md`.

### 2.5 Retailer price requests (outbound, no household data)

The weekly ingest fetches **prices**, not household data. The request bodies and
headers contain no household identifier, list content or user data:

| Destination | Request | Payload sent |
| --- | --- | --- |
| BasketWatch (RapidAPI) | `GET https://{BASKETWATCH_API_HOST}/products?retailer=tesco\|dunnes\|supervalu\|aldi` with `X-RapidAPI-Key` / `X-RapidAPI-Host` (`crates/ingest/src/basketwatch.rs:596-600`) | the retailer name and the API key |
| Lidl direct | `GET {LIDL_FEED_URL}` (`crates/ingest/src/lidl.rs:427,551`) | the feed URL only |

What the provider learns is that this server fetched a retailer's public product
catalogue; it receives no household records. Responses are normalised into the
shared `retailer_products` / `product_offers` tables and kept as long as the
operator keeps price history (append-only, `migrations/001_phase0_schema.sql`).
Ingest is scheduled, never triggered by a household action.

### 2.6 Optional external LLM (the one household-data flow that can leave)

Two features can call an OpenAI-compatible chat-completions endpoint, both behind
one switch:

1. **Resolution fallback** (`crates/api/src/resolution.rs`): when a list term
   matches no catalogue item, the model is asked which household canonical item it
   most likely names (`crates/api/src/llm.rs`, `build_prompt`).
2. **Candidate ranking** (`crates/api/src/preferences.rs`): reorders the proposed
   retailer products for one canonical item.

| | |
| --- | --- |
| **Switch** | `LLM_FALLBACK_ENABLED` — `true` (case-insensitive) turns both on; anything else, including unset, leaves them off (`crates/api/src/llm.rs:56-73`). **Default: off.** |
| **Other config** | `LLM_API_KEY`, `LLM_MODEL` (both required for a call to happen), `LLM_BASE_URL` (default `https://api.openai.com/v1/chat/completions`), `LLM_PROVIDER` (accepted, currently unused). |
| **Leaves the server when ON** | The raw list term plus the household's canonical item names (resolution); or one item description plus the candidate product labels `name \| retailer` (ranking). |
| **Does NOT leave** | Email addresses, password hashes, session tokens, quantities, bought status, list names, savings figures, or any other household's data. A fallback that is off sends nothing at all — the gate short-circuits before any DB read or request. |
| **Where** | The endpoint named by `LLM_BASE_URL`, bearer-authenticated with `LLM_API_KEY`. |
| **Cost record** | One `api_usage_events` row with `source='llm'` per answered call — no household id and no content (`resolution.rs`, `record_llm_usage`). |
| **Failure policy** | A 15 s timeout; any error logs at `warn` and leaves the entry `unresolved`. No retries. |

The switch is documented for operators in `deploy/README.md` ("LLM resolution
fallback"). Its off-behaviour is verified by
`crates/api/tests/llm_fallback_disabled.rs`: with the fallback off, adding items,
server-side resolution and optimisation all still work, the model is called zero
times, and no usage row is written.

### 2.7 Third parties that see nothing

Same-origin only: the SPA talks exclusively to `/api/*`. There is no analytics,
telemetry, advertising, error-reporting, CDN or font provider. The only two
outbound destinations in the whole system are §2.5 (prices) and §2.6 (the
optional model).

## 3. Turning optional external processing off

**It is off unless the operator turns it on.** To be certain it is off, confirm
that `/etc/buywise/buywise.env` either omits `LLM_FALLBACK_ENABLED` or sets it to
anything other than `true`:

```bash
sudo grep -n '^LLM_' /etc/buywise/buywise.env || echo "no LLM configuration: fallback is off"
sudo systemctl restart buywise    # only needed after changing the file
```

With the fallback off, the shared list keeps working exactly as before: adding a
term resolves from the household catalogue (alias lookup then fuzzy match), an
unknown term stays `unresolved` for a member to clarify, and `/api/optimise` still
prices and recommends. That is the behaviour asserted by
`crates/api/tests/llm_fallback_disabled.rs`.

Turning it **on** is what makes the §2.6 payload leave the server, so it should be
a deliberate operator decision recorded in the env file, with `LLM_API_KEY`,
`LLM_MODEL` and (if not OpenAI) `LLM_BASE_URL` set.

## 4. Raw-receipt retention (policy for a feature that does not exist yet)

**There is no receipt ingestion, no OCR and no receipt-image storage in this
codebase today.** `purchase_events.source` already allows `'receipt'`
(`migrations/013_purchase_suggestions.sql:39-47`) so a future import can record a
purchase without double-counting, but nothing writes it. This section is therefore
*policy and a gate*, not a description of running code.

Proposed policy, for the household owner to decide before any receipt feature
ships:

1. **Upload is an explicit external-processing choice.** A receipt upload must ask
   the member, per upload, whether the image may be sent to a processing provider,
   and name the provider. No default-on, no bundled consent, no "continue implies
   consent".
2. **No community publication and no model training.** A receipt image must never
   be published to other households and must not be used to train or improve any
   model. That must be the documented default and, where the provider offers it,
   a contractual setting the operator records.
3. **Local-first processing where it is good enough.** Prefer on-device or
   server-local OCR, and offer an external provider only as an opt-in upgrade. See
   the decision below.
4. **Raw images are short-lived; derived lines are not.** Proposed: delete the raw
   image **30 days** after the receipt is accepted, keeping the extracted line
   items (which are the household's own purchase records and are exported and
   deleted like any other record). Rationale: long enough for the household to
   contest a bad extraction or re-run OCR after a fix; short enough that the
   highest-risk artefact — a photograph that can carry a name, address, card
   fragment or loyalty number — is not warehoused indefinitely. The derived lines
   are what make suggestions better; the image is not needed for that.
5. **Deletion must cover images.** When images exist, `delete-household` must
   remove them too, and the retention job must enforce the 30-day expiry even for
   households that were never deleted.

**Decisions needed** (owner): the retention period above; whether OCR is local or
external by default; and whether the image is stored on this server (then
encrypted-at-rest in §6 matters) or only held transiently during processing.

## 5. Retention and deletion

### 5.1 The commands

```
buywise-cli bootstrap-household --db /var/lib/buywise/buywise.db --name "<household>" --email <owner@example.com>
buywise-cli export-household --db /var/lib/buywise/buywise.db --household-id <uuid> [--out household.json]
buywise-cli delete-household --db /var/lib/buywise/buywise.db --household-id <uuid> --dry-run
buywise-cli delete-household --db /var/lib/buywise/buywise.db --household-id <uuid> --confirm
buywise-cli import-purchases --db /var/lib/buywise/buywise.db --household-id <uuid> --file purchases.json
```

`bootstrap-household` (issue #139) creates the very first household and its owner
on a fresh database: it creates and migrates the database file, prompts for the
owner's password without echoing it, and writes the household, the owner's
account and an `owner` membership in one transaction. It refuses once any account
exists, so it is a one-time command; the owner adds everyone else from the app
(§5.1a). See `deploy/README.md` for the deployment step.

`export-household` is read-only and writes the documented JSON layout in
[`docs/household-export.md`](household-export.md). `delete-household` refuses to
run without `--confirm`, and `--dry-run` executes the real statements inside a
transaction and rolls it back, printing exactly what a run would remove. Neither
export nor delete runs migrations or creates a missing database file.

### 5.1a The member-facing controls

A household member does not need an operator (issue #136): the settings screen
(**Your data**) offers a **Download export** button and, to owners, a **Delete
household data** action, backed by the API and the same shared implementations
(`buywise_core::household_data` and `buywise_core::members`) the CLI calls, so
the two cannot drift. Owners also get **Settings → Household members** to add and
remove members (issue #139).

| Request | Who | Purpose |
| --- | --- | --- |
| `GET /api/household` | any member | the caller's household `{ id, name, role }` |
| `GET /api/household/{household_id}/export` | any member | download the document above as an attachment |
| `POST /api/household/{household_id}/delete` | owner | guarded delete, then end the session |
| `GET /api/household/{household_id}/members` | owner | list the members with their roles |
| `POST /api/household/{household_id}/members` | owner | add a member (email, display name, initial password) |
| `DELETE /api/household/{household_id}/members/{user_id}` | owner | remove a member |

Every endpoint requires a live session and membership of the named household
(`household_members`): a member of another household is refused with `403`, so
one household can never read or delete another's data. `household_members.role`
(migration 003) is enforced: **export is available to every member**, while the
member endpoints and the household delete require the `owner` role. The delete
additionally requires the household name typed exactly and the owner's own
password re-entered; either mismatch is `422` and changes nothing. On success the
household's rows are removed by the same dependency-ordered transaction as the
CLI, the caller's session row is deleted and the `session` / `csrf` cookies are
cleared, so the client returns to the sign-in page.

Adding a member takes an email, a display name and an initial password the owner
shares out of band — it is never emailed and the server never returns it; a
duplicate email is `409`. Removing a member ends their sessions immediately (their
next request is `401`), removes their membership, and deletes their account only
if they belong to no other household; removing the household's last owner is
`409`. The very first household and owner are created on a fresh database by
`bootstrap-household` (§5.1). See
[`docs/KNOWN_LIMITATIONS.md`](KNOWN_LIMITATIONS.md) ("Household member
management").

### 5.2 What deletion removes

In one transaction, in dependency order: `list_entry_events`, `savings_events`,
`purchase_events`, `purchase_transaction_revisions`, `purchase_lines`,
`purchase_transactions`, `mutations`, `list_entries`, `recommendation_runs`,
`shopping_lists`, `match_feedback`, `product_matches`, `item_constraints`,
`item_aliases`, `purchase_dismissals`, `canonical_items`,
`household_store_category_rules`, `household_store_settings`,
`household_settings`, `store_category_orders`, `household_members`,
`households`, then `sessions` and `users` for members left with **no other
household** (`crates/core/src/household_data.rs`, `HOUSEHOLD_TABLES`). A member
who also belongs to another household keeps their account, their other membership
and their session; only the deleted household's membership goes.

Deliberately kept, because it is not household-identifiable: the shared
`categories`, `subcategories`, `retailer_products`, `product_offers`,
`source_runs` tables, and the aggregate `api_usage_events` counters (which carry
no household id and no content). A household's own aggregate API call counts
therefore survive as unattributable totals — see the decision below.

`crates/cli/tests/household_data.rs` proves the shape of this: after deleting
household A, every household-scoped table has zero rows for A, a second seeded
household's row counts are unchanged, the orphaned member is gone, and a member
shared with the other household survives.

### 5.3 Backup expiry, restore handling, and on-disk remnants

Deletion is immediate and complete **on the live server**, and it is neither
immediate nor complete in the backup (§2.4):

- The database file keeps the deleted rows' bytes in free pages and in the WAL
  until they are reused, checkpointed or vacuumed. A forensic read of the file can
  still find them. To shrink that window deliberately:

  ```bash
  sudo systemctl stop buywise litestream
  sqlite3 /var/lib/buywise/buywise.db 'PRAGMA wal_checkpoint(TRUNCATE); VACUUM;'
  sudo systemctl start buywise litestream
  ```

  (`VACUUM` rewrites the whole file and cannot run inside a transaction, hence the
  stop/restart.) This is optional and not run automatically by the CLI.
- **R2 holds up to about 7 days of history** (the explicit `retention: 168h`
  set in `deploy/litestream.yml`, §2.4). Until those frames age out, a restore of
  the replica can resurrect the deleted household. Two consequences the household
  should expect:
  - *Restore resurrects.* After a disaster restore, re-run `delete-household
    --confirm` for any household deleted before the restore point, and then
    re-apply the `VACUUM` step. The deletion is not "remembered" by an older
    replica.
  - *Forcing backup expiry.* To remove the pre-deletion frames before retention
    would: stop `litestream`, delete the `db` prefix in the `buywise-backup`
    bucket (R2 dashboard, or `aws s3 rm --recursive`/`rclone delete` with the same
    credentials), then start `litestream` again so it writes a fresh snapshot of
    the already-deleted local database. **This destroys every recovery point
    older than the purge**, so it is a deliberate operator action, not part of the
    delete command.
- Receipt images do not exist yet; when they do, they are not in the SQLite file
  and will need their own deletion and expiry (§4.5).

### 5.4 Recommended operator sequence for a household leaving

Steps 1 and 3 can also be run by a household member from the app's settings screen
(§5.1a) without an operator; the sequence below is the operator path.

1. `export-household` and store the JSON somewhere the household controls.
2. Tell the household that uploads stop when the account is gone and that members'
   clients may still hold list data in IndexedDB/localStorage until they clear site
   data on each device (the server cannot reach into a browser).
3. `delete-household --dry-run`, then `--confirm`.
4. Optionally `VACUUM` as in §5.3.
5. If the household requires that the copies in R2 go too, purge and re-seed the
   replica (§5.3) — accepting that this drops all older recovery points.
6. Re-run step 3 after any later disaster restore that predates the deletion.

## 6. Backups, permissions, encryption and recovery: review

Findings from reading `deploy/litestream.yml`, `deploy/setup.sh` and
`deploy/systemd/`. The retention and R2 token-scope rows now record the shipped
decisions (issues #136/#142); the rest describes unchanged state.

| Area | Current state | Assessment |
| --- | --- | --- |
| R2 credential storage | `LITESTREAM_ACCESS_KEY_ID` / `LITESTREAM_SECRET_ACCESS_KEY` in `/etc/buywise/buywise.env`, `root:root` `0600`; injected by systemd | Good file hygiene. The keys are in the `litestream` process environment, readable by the `buywise` user via `/proc`; consider whether Litestream should run as a distinct user. The token is bucket-scoped; see the next row and `deploy/README.md` §4.2. |
| R2 token scope | Bucket-scoped token (owner decision, issue #136); creation and verification steps in `deploy/README.md` §4.2 "Cloudflare R2 bucket and scoped token" | Bucket-scoped, read/write for the single `buywise-backup` bucket — not an account-wide token. Re-verify after any credential rotation. |
| Transport | HTTPS to the R2 endpoint | Fine, provided `skip-verify` is never set (it is not). |
| Encryption at rest | R2 server-side encryption only; no `age`, no customer key | The database (with password hashes and full purchase history) is readable by whoever holds the bucket. **Key custody is Cloudflare's.** Decide whether that is acceptable or whether `age` client-side encryption should be turned on — noting the age private key then becomes a new secret the household must hold out-of-band, without which restores are impossible. |
| Local database at rest | Unencrypted file, `0750` directory owned by `buywise`; no `UMask=`/`ProtectSystem=` hardening in the units | Acceptable for a single-tenant host; tighten with a `UMask=0077` in `buywise.service` if other local accounts exist. |
| Restore recovery | Documented manual drill in `deploy/README.md` with the writer-stop ordering; `validation-interval` unset so restores are never auto-verified | The ordering write-up is sound. There is **no rehearsal evidence in the repository**: run the drill at least once and record the outcome. Consider enabling `validation-interval` (costs egress). |
| Retention | `retention: 168h` (7 days) set explicitly in `deploy/litestream.yml` (issue #142); Litestream's default would be 24 h | Explicit and documented; makes the deletion story in §5.3 concrete. |
| Deletion vs backups | Covered in §5.3 | The gap is documented rather than papered over: deletion never promises immediate erasure from backups. |
| Recovery time | Restore is a full-file restore plus WAL replay; snapshot cadence is Litestream's default | Acceptable for one household; record the measured restore time from the drill. |

**Decisions needed** (owner): backup encryption and who holds the key; whether to
harden the service units' umask; and whether the aggregate `api_usage_events`
counters (and one household's share of them) should also be purged on deletion.
The R2 token scope and the explicit backup `retention` are decided and shipped
(bucket-scoped token, 7 days — issues #136/#142).

## 7. Decisions needed, in one place

| # | Decision | Options | Recommendation |
| --- | --- | --- | --- |
| 1 | Receipt OCR: local or external? | (a) server-local OCR, external never used; (b) external opt-in per upload; (c) external by default | (a) as the default with (b) as an explicit per-upload upgrade, per §4. |
| 2 | Raw-receipt retention | e.g. 7 / 30 / 90 days, or keep until deleted | 30 days, per §4.4, with derived lines kept as ordinary purchase records. |
| 3 | Backup encryption and key custody | R2-managed keys; or Litestream `age` with a household-held key | **Decided for now:** use provider-managed R2 keys. Revisit client-side encryption only if key custody and restore procedures are agreed. |
| 4 | R2 token scope and backup retention | account token vs bucket-scoped read/write token; default 24 h vs an explicit policy | **Decided and shipped (issues #136/#142):** bucket-scoped token; `retention: 168h` (7 days) set explicitly. |
| 5 | Do aggregate usage counters count as household data? | leave `api_usage_events` (no household id) in place; or delete the household's share | Leaving unattributable totals is harmless; revisit if per-household attribution is added. |
| 6 | Local on-disk remnant policy | document the `VACUUM` step only; or run it automatically | Keep it documented and operator-run (it briefly stops the service). |

## 8. Known gaps and follow-ups

Recorded in [`docs/KNOWN_LIMITATIONS.md`](KNOWN_LIMITATIONS.md) under "Household
data controls" and "Household member management": receipt ingestion and its
external-processing gate being policy-only, deletion not reaching backups
immediately, no receipt-image path to delete yet, member roles not being
changeable (no promote/demote), and no self-registration or invite flow.
