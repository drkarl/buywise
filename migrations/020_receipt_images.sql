-- ============================================================================
-- Migration 020: Receipt images (issue #149, roadmap #114)
-- ============================================================================
-- Policy this implements (docs/data-flows.md §4): a household member may keep a
-- photograph of a receipt privately on the household's own server for 30 days.
-- `docs/data-flows.md` previously described this as policy only because no
-- receipt-image storage existed; this migration is the first storage slice.
--
-- One row per stored image. The BYTES ARE NOT IN SQLITE — they live on the
-- server filesystem under `receipt_images.storage_key` in the directory named by
-- the `RECEIPT_DIR` configuration (default: a `receipts/` directory next to the
-- database file), and are served only through an authenticated handler.
--
-- Conventions (match 003_phase1_schema.sql):
--   * Primary keys are TEXT (here a UUID v4 minted by the uploader).
--   * Timestamps are TEXT (ISO-8601 UTC).
--
-- Columns:
--   * `household_id`  — the owning tenant; every read and write is scoped by it.
--   * `uploaded_by`   — the member who uploaded the image; null if their account is deleted.
--   * `uploaded_at`   — when the upload was accepted (wall clock, RFC 3339 ms).
--   * `expires_at`    — `uploaded_at` + 30 days. Indexed: the expiry sweep
--     (`buywise-cli expire-receipts`, daily systemd timer) selects on it.
--   * `content_type`  — the type DETECTED FROM THE FILE SIGNATURE at upload
--     (never the client's header): one of image/jpeg, image/png, image/webp,
--     image/heic, image/heif.
--   * `byte_size`     — image length in bytes, for the per-household total-size
--     quota (and the count quota is a COUNT(*) over this table).
--   * `sha256_hex`    — SHA-256 of the stored bytes, so a restored or copied file
--     can be checked against its record.
--   * `storage_key`   — the random file name under the receipt directory: a UUID
--     plus the extension implied by `content_type`, with no household, user or
--     original-filename data in it. UNIQUE so two records can never alias one
--     file, and never used as a path served directly.
--   * `processing`    — the per-upload processing choice. Only `'local'` is
--     accepted today (the image is read on this server only); an external choice
--     is refused by the API with a clear message until external processing
--     exists, so the column records the choice that was actually taken.
--
-- Additive only / no rebuild: one new table plus its indexes. Nothing existing is
-- altered, so every earlier table and row stays readable.
-- ============================================================================

PRAGMA foreign_keys = ON;

CREATE TABLE receipt_images (
    id           TEXT PRIMARY KEY,
    household_id TEXT NOT NULL REFERENCES households(id),
    uploaded_by  TEXT REFERENCES users(id) ON DELETE SET NULL,
    uploaded_at  TEXT NOT NULL,
    expires_at   TEXT NOT NULL,
    content_type TEXT NOT NULL,
    byte_size    INTEGER NOT NULL,
    sha256_hex   TEXT NOT NULL,
    storage_key  TEXT NOT NULL UNIQUE,
    processing   TEXT NOT NULL DEFAULT 'local',
    CHECK (processing IN ('local', 'external')),
    CHECK (byte_size >= 0)
);

-- The list/limits/indexed by household, newest first.
CREATE INDEX idx_receipt_images_household ON receipt_images(household_id, uploaded_at DESC);

-- The expiry sweep: `WHERE expires_at <= ?` ordered oldest-first.
CREATE INDEX idx_receipt_images_expires_at ON receipt_images(expires_at);
