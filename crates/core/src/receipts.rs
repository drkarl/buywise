//! Receipt images: local-only storage with 30-day expiry (issue #149,
//! roadmap #114).
//!
//! A household member photographs a receipt after a shop and keeps the image
//! privately on the household's own server for 30 days. This module owns both
//! halves of that:
//!
//! * **The bytes** live on the server filesystem, never in SQLite and never on a
//!   statically served path. Each image is written under a random [`storage key`]
//!   (a UUID plus its extension) into [`ReceiptConfig::dir`] with owner-only
//!   permissions, and is read back only through the authenticated API handler in
//!   `crates/api/src/receipts.rs`.
//! * **The record** is one `receipt_images` row (migration 020) carrying the
//!   household, uploader, upload and expiry timestamps, the *detected* content
//!   type, size, SHA-256 checksum, storage key and the per-upload processing
//!   choice.
//!
//! # Content type is decided by the bytes
//!
//! The client's `Content-Type` header and the original file name are never
//! trusted. [`detect_image`] reads the file signature and accepts only JPEG,
//! PNG, WebP and HEIC/HEIF; everything else is refused before anything is
//! written. The bytes are stored verbatim — this slice never parses or re-encodes
//! an image (so EXIF, including any location data a camera wrote, rides along
//! with the file; see `docs/KNOWN_LIMITATIONS.md`).
//!
//! # Processing choice
//!
//! [`ProcessingChoice`] records how the image is read. Only `local` is accepted
//! today; an `external` choice is refused with
//! [`ReceiptError::ExternalProcessingUnsupported`] until external processing
//! exists, and no external service is ever called from this code.
//!
//! # Quotas, expiry and deletion
//!
//! [`store_receipt`] enforces the documented limits ([`ReceiptConfig`]) — one
//! size limit per image, a count limit and a total-bytes limit per household.
//! [`expire_receipts`] is the sweep the daily systemd timer runs: it removes
//! every file whose record is past its expiry and *then* the record, tolerating a
//! file that is already gone, so running it twice changes nothing.
//! [`delete_household_receipts`] is the household-delete hook, also
//! file-before-record. The clock is a parameter ([`DateTime<Utc>`]) everywhere,
//! never read from the wall clock, so tests are deterministic.
//!
//! *[`storage key`]: [`ReceiptImage::storage_key`]

use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, SqliteConnection, SqlitePool};
use uuid::Uuid;

/// Days a receipt image is kept after upload. `expires_at = uploaded_at + 30
/// days`; the daily sweep removes the file and the record once that passes.
pub const RECEIPT_TTL_DAYS: i64 = 30;

/// Default per-image size limit: 10 MiB. A phone photo is well under this; the
/// limit exists so one upload cannot fill the disk.
pub const DEFAULT_MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;

/// Default per-household image-count limit.
pub const DEFAULT_MAX_IMAGES_PER_HOUSEHOLD: i64 = 200;

/// Default per-household total-bytes limit: 500 MiB.
pub const DEFAULT_MAX_TOTAL_BYTES_PER_HOUSEHOLD: i64 = 500 * 1024 * 1024;

/// Environment variable naming the receipt-image directory. Unset (or empty)
/// means [`default_receipt_dir`] — a `receipts/` directory next to the database.
pub const ENV_RECEIPT_DIR: &str = "RECEIPT_DIR";

/// Environment override for [`ReceiptConfig::max_image_bytes`].
pub const ENV_MAX_IMAGE_BYTES: &str = "RECEIPT_MAX_IMAGE_BYTES";

/// Environment override for [`ReceiptConfig::max_images_per_household`].
pub const ENV_MAX_IMAGES_PER_HOUSEHOLD: &str = "RECEIPT_MAX_IMAGES_PER_HOUSEHOLD";

/// Environment override for [`ReceiptConfig::max_total_bytes_per_household`].
pub const ENV_MAX_TOTAL_BYTES_PER_HOUSEHOLD: &str = "RECEIPT_MAX_TOTAL_BYTES_PER_HOUSEHOLD";

/// Sub-directory used when [`ENV_RECEIPT_DIR`] is unset: `<database dir>/receipts`.
pub const DEFAULT_RECEIPT_SUBDIR: &str = "receipts";

/// The `receipt_images` table name, shared with the household-delete path
/// (`crate::household_data`), which removes the rows and their files together.
pub const RECEIPT_IMAGES_TABLE: &str = "receipt_images";

/// `receipt_images.processing` for an image that is read on this server only.
pub const PROCESSING_LOCAL: &str = "local";

/// `receipt_images.processing` for external processing — reserved, not accepted
/// yet (see [`ProcessingChoice`]).
pub const PROCESSING_EXTERNAL: &str = "external";

/// Where receipt bytes live and how much a household may keep.
///
/// [`ReceiptConfig::from_env`] reads the documented environment overrides; an
/// unparsable limit value falls back to its default (a malformed override never
/// removes the guardrail).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptConfig {
    /// Directory holding the image files. Created on first upload with
    /// owner-only permissions.
    pub dir: PathBuf,
    /// Largest accepted image, in bytes.
    pub max_image_bytes: u64,
    /// Largest number of images one household may store.
    pub max_images_per_household: i64,
    /// Largest total size of one household's images, in bytes.
    pub max_total_bytes_per_household: i64,
}

impl Default for ReceiptConfig {
    fn default() -> Self {
        Self {
            dir: default_receipt_dir(""),
            max_image_bytes: DEFAULT_MAX_IMAGE_BYTES,
            max_images_per_household: DEFAULT_MAX_IMAGES_PER_HOUSEHOLD,
            max_total_bytes_per_household: DEFAULT_MAX_TOTAL_BYTES_PER_HOUSEHOLD,
        }
    }
}

impl ReceiptConfig {
    /// Configuration from the environment.
    ///
    /// The directory is [`ENV_RECEIPT_DIR`], or the database-relative default;
    /// the limits are the documented constants unless their environment
    /// overrides are set. The database location is read from `DATABASE_URL` so
    /// the default directory tracks the deployed database file.
    pub fn from_env() -> Self {
        let database_url = std::env::var("DATABASE_URL").unwrap_or_default();
        let dir = match env_string(ENV_RECEIPT_DIR) {
            Some(value) => PathBuf::from(value),
            None => default_receipt_dir(&database_url),
        };
        Self {
            dir,
            max_image_bytes: env_u64(ENV_MAX_IMAGE_BYTES, DEFAULT_MAX_IMAGE_BYTES),
            max_images_per_household: env_i64(
                ENV_MAX_IMAGES_PER_HOUSEHOLD,
                DEFAULT_MAX_IMAGES_PER_HOUSEHOLD,
            ),
            max_total_bytes_per_household: env_i64(
                ENV_MAX_TOTAL_BYTES_PER_HOUSEHOLD,
                DEFAULT_MAX_TOTAL_BYTES_PER_HOUSEHOLD,
            ),
        }
    }

    /// Configuration for an explicit directory with the default limits — the
    /// seam tests use to point storage at a temporary directory.
    pub fn with_dir(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            ..Self::default()
        }
    }

    /// Configuration for a CLI command that names its database file as a plain
    /// path (the `buywise-cli` convention) rather than as a URL.
    ///
    /// The directory is `dir_override`, else [`ENV_RECEIPT_DIR`], else the
    /// database-relative default ([`default_receipt_dir`]); the limits follow the
    /// same environment overrides as [`ReceiptConfig::from_env`].
    pub fn for_database(db_path: &str, dir_override: Option<&str>) -> Self {
        let dir = dir_override
            .map(PathBuf::from)
            .or_else(|| env_string(ENV_RECEIPT_DIR).map(PathBuf::from))
            .unwrap_or_else(|| default_receipt_dir(&format!("sqlite://{db_path}")));
        Self {
            dir,
            ..Self::default()
        }
    }

    /// Override the limits, keeping the directory.
    pub fn with_limits(
        mut self,
        max_image_bytes: u64,
        max_images_per_household: i64,
        max_total_bytes_per_household: i64,
    ) -> Self {
        self.max_image_bytes = max_image_bytes;
        self.max_images_per_household = max_images_per_household;
        self.max_total_bytes_per_household = max_total_bytes_per_household;
        self
    }
}

/// Unset/empty environment values read as `None`; other values are returned
/// verbatim.
fn env_string(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => Some(value.trim().to_string()),
        _ => None,
    }
}

fn env_u64(name: &str, default: u64) -> u64 {
    env_string(name)
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn env_i64(name: &str, default: i64) -> i64 {
    env_string(name)
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

/// The default receipt directory for `database_url`: a `receipts/` directory
/// beside the SQLite file, so images sit with the data they belong to and move
/// with a backup of that directory.
///
/// An in-memory (or empty/unknown) database URL has no directory to sit beside,
/// so the default is `./receipts` relative to the process working directory.
pub fn default_receipt_dir(database_url: &str) -> PathBuf {
    let spec = database_url.trim();
    let path = spec
        .strip_prefix("sqlite://")
        .or_else(|| spec.strip_prefix("sqlite:"))
        .unwrap_or(spec);
    // `sqlite://file.db?mode=rwc` — the query string is not part of the path.
    let path = path.split('?').next().unwrap_or_default();
    if path.is_empty() || path.contains(":memory:") {
        return PathBuf::from(DEFAULT_RECEIPT_SUBDIR);
    }
    match Path::new(path).parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(DEFAULT_RECEIPT_SUBDIR),
        _ => PathBuf::from(DEFAULT_RECEIPT_SUBDIR),
    }
}

// ---------------------------------------------------------------------------
// Image signatures
// ---------------------------------------------------------------------------

/// An image format accepted for a receipt photo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    /// JPEG (`image/jpeg`, `.jpg`).
    Jpeg,
    /// PNG (`image/png`, `.png`).
    Png,
    /// WebP (`image/webp`, `.webp`).
    WebP,
    /// HEIC, the ISO-BMFF brand family iPhone cameras write (`image/heic`, `.heic`).
    Heic,
    /// HEIF files that are not HEIC-branded (`image/heif`, `.heic`).
    Heif,
}

impl ImageKind {
    /// The content type stored on the record and served with the bytes.
    pub fn content_type(self) -> &'static str {
        match self {
            ImageKind::Jpeg => "image/jpeg",
            ImageKind::Png => "image/png",
            ImageKind::WebP => "image/webp",
            ImageKind::Heic => "image/heic",
            ImageKind::Heif => "image/heif",
        }
    }

    /// The file-name extension appended to the random storage key.
    pub fn extension(self) -> &'static str {
        match self {
            ImageKind::Jpeg => "jpg",
            ImageKind::Png => "png",
            ImageKind::WebP => "webp",
            ImageKind::Heic | ImageKind::Heif => "heic",
        }
    }

    /// The kind for a stored content type, or `None` for an unknown value.
    pub fn from_content_type(content_type: &str) -> Option<Self> {
        match content_type {
            "image/jpeg" => Some(ImageKind::Jpeg),
            "image/png" => Some(ImageKind::Png),
            "image/webp" => Some(ImageKind::WebP),
            "image/heic" => Some(ImageKind::Heic),
            "image/heif" => Some(ImageKind::Heif),
            _ => None,
        }
    }
}

/// Identify an image from its leading bytes — the allowlist check.
///
/// The signature, never the client's `Content-Type` or file name, decides:
///
/// * JPEG — `FF D8 FF`,
/// * PNG — the 8-byte PNG signature,
/// * WebP — `RIFF`/`WEBP`,
/// * HEIC/HEIF — an ISO-BMFF `ftyp` box whose major brand is a HEIC family brand
///   (`heic`, `heix`, `hevc`, `hevx`, `heim`, `heis`, `hevm`, `hevs`) or a
///   generic HEIF brand (`mif1`, `msf1`).
pub fn detect_image(bytes: &[u8]) -> Option<ImageKind> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(ImageKind::Jpeg);
    }
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some(ImageKind::Png);
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some(ImageKind::WebP);
    }
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        let brand = &bytes[8..12];
        match brand {
            b"heic" | b"heix" | b"hevc" | b"hevx" | b"heim" | b"heis" | b"hevm" | b"hevs" => {
                return Some(ImageKind::Heic);
            }
            b"mif1" | b"msf1" => return Some(ImageKind::Heif),
            _ => {}
        }
    }
    None
}

/// A fresh random storage key: a UUID v4 plus the extension for `kind`.
///
/// No household id, user id or original file name appears in the key, and the
/// UUID makes collisions (and therefore one image overwriting another) an
/// accident of astronomy rather than a possibility.
pub fn generate_storage_key(kind: ImageKind) -> String {
    format!("{}.{}", Uuid::new_v4().simple(), kind.extension())
}

/// True for a key this module could have produced: hex UUID, one dot, an
/// extension. A key failing this is refused rather than joined onto the receipt
/// directory, so a corrupted or tampered `storage_key` can never escape it.
fn storage_key_is_safe(key: &str) -> bool {
    if key.is_empty() || key.len() > 64 || key.starts_with('.') || key.contains("..") {
        return false;
    }
    let Some((stem, extension)) = key.split_once('.') else {
        return false;
    };
    stem.len() == 32
        && stem.chars().all(|c| c.is_ascii_hexdigit())
        && matches!(extension, "jpg" | "png" | "webp" | "heic")
}

// ---------------------------------------------------------------------------
// Filesystem
// ---------------------------------------------------------------------------

/// The absolute path of one stored image, refusing an unsafe key.
pub fn receipt_path(dir: &Path, storage_key: &str) -> Result<PathBuf, ReceiptError> {
    if !storage_key_is_safe(storage_key) {
        return Err(ReceiptError::UnsafeStorageKey(storage_key.to_string()));
    }
    Ok(dir.join(storage_key))
}

/// Create the receipt directory (and its parents) with owner-only permissions.
///
/// Idempotent: an existing directory is left as it is. Called before the first
/// write, so a deployment that has never uploaded an image needs no directory.
pub fn ensure_dir(dir: &Path) -> Result<(), ReceiptError> {
    if dir.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Write one image file owner-only. The bytes are stored verbatim: no parsing,
/// no re-encoding, no metadata rewriting.
pub fn write_file(dir: &Path, storage_key: &str, bytes: &[u8]) -> Result<(), ReceiptError> {
    let path = receipt_path(dir, storage_key)?;
    ensure_dir(dir)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    std::io::Write::write_all(&mut file, bytes)?;
    Ok(())
}

/// Read one stored image, or [`ReceiptError::FileMissing`] if the file is gone.
pub fn read_file(dir: &Path, storage_key: &str) -> Result<Vec<u8>, ReceiptError> {
    let path = receipt_path(dir, storage_key)?;
    match std::fs::read(&path) {
        Ok(bytes) => Ok(bytes),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(ReceiptError::FileMissing {
            id: String::new(),
            key: storage_key.to_string(),
        }),
        Err(err) => Err(err.into()),
    }
}

/// Remove one image file. Returns `false` when the file was already gone.
///
/// A missing file is a normal outcome, not an error: expiry and deletion must
/// still remove the record for an image an operator removed by hand.
pub fn remove_file(dir: &Path, storage_key: &str) -> Result<bool, ReceiptError> {
    let path = receipt_path(dir, storage_key)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err.into()),
    }
}

/// Remove a batch of image files; returns how many were actually removed.
///
/// Any error other than "already gone" propagates, so a caller that is deleting
/// records can abort (and keep the records) rather than silently leaving an
/// unreferenced image behind.
pub fn remove_files(dir: &Path, storage_keys: &[String]) -> Result<u64, ReceiptError> {
    let mut removed = 0;
    for key in storage_keys {
        if remove_file(dir, key)? {
            removed += 1;
        }
    }
    Ok(removed)
}

// ---------------------------------------------------------------------------
// Records
// ---------------------------------------------------------------------------

/// One `receipt_images` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, FromRow)]
pub struct ReceiptImage {
    /// `receipt_images.id` (UUID v4).
    pub id: String,
    /// Owning household.
    pub household_id: String,
    /// Uploading member (`users.id`), or `None` after their account is deleted.
    pub uploaded_by: Option<String>,
    /// Upload time (RFC 3339 UTC, millisecond precision).
    pub uploaded_at: String,
    /// `uploaded_at` + [`RECEIPT_TTL_DAYS`].
    pub expires_at: String,
    /// Detected content type (never the client's header).
    pub content_type: String,
    /// Stored size in bytes.
    pub byte_size: i64,
    /// SHA-256 of the stored bytes, hex.
    pub sha256_hex: String,
    /// Random file name under the receipt directory.
    pub storage_key: String,
    /// Processing choice taken at upload; `local` today.
    pub processing: String,
}

/// The per-upload processing choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessingChoice {
    /// Read on this household's own server. The only choice implemented.
    Local,
}

impl ProcessingChoice {
    /// Parse the `processing` parameter of an upload.
    ///
    /// `local` (the default when the parameter is absent) is accepted;
    /// `external` is refused with [`ReceiptError::ExternalProcessingUnsupported`]
    /// and anything else with [`ReceiptError::UnknownProcessing`], so a typo
    /// never silently stores a choice the household did not make.
    pub fn parse(raw: &str) -> Result<Self, ReceiptError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            PROCESSING_LOCAL => Ok(ProcessingChoice::Local),
            PROCESSING_EXTERNAL => Err(ReceiptError::ExternalProcessingUnsupported),
            other => Err(ReceiptError::UnknownProcessing(other.to_string())),
        }
    }

    /// The `receipt_images.processing` value.
    pub fn as_db_str(self) -> &'static str {
        match self {
            ProcessingChoice::Local => PROCESSING_LOCAL,
        }
    }
}

/// Errors from storing, reading, expiring or deleting receipt images.
#[derive(Debug, thiserror::Error)]
pub enum ReceiptError {
    /// The file signature is not an accepted image format.
    #[error("not a JPEG, PNG, WebP or HEIC image (the file signature does not match)")]
    UnsupportedImageType,

    /// The image is larger than the per-image limit.
    #[error("the image is {size} bytes, over the {limit}-byte limit")]
    ImageTooLarge { size: u64, limit: u64 },

    /// The household already holds the maximum number of images.
    #[error("household {household_id} already keeps {count} images (limit {limit})")]
    TooManyImages {
        household_id: String,
        count: i64,
        limit: i64,
    },

    /// The image would push the household over its total-bytes limit.
    #[error(
        "household {household_id} already keeps {stored} bytes of images \
         (limit {limit}); this {incoming}-byte image would exceed it"
    )]
    HouseholdStorageFull {
        household_id: String,
        stored: i64,
        incoming: u64,
        limit: i64,
    },

    /// The household chose external processing, which does not exist yet.
    #[error("external receipt processing is not available yet; only local processing is accepted")]
    ExternalProcessingUnsupported,

    /// The `processing` value is neither `local` nor `external`.
    #[error("unknown processing choice {0:?}; only \"local\" is accepted")]
    UnknownProcessing(String),

    /// No such receipt in this household (or no such receipt at all).
    #[error("no receipt image {id} in household {household_id}")]
    ReceiptNotFound { id: String, household_id: String },

    /// A stored key that this module would not have generated.
    #[error("refusing unsafe receipt storage key {0:?}")]
    UnsafeStorageKey(String),

    /// The record exists but its bytes are gone from disk.
    #[error("receipt {id}: file {key} is missing")]
    FileMissing { id: String, key: String },

    /// A filesystem operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// A query or transaction failed at the sqlx layer.
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
}

/// The timestamp format for `uploaded_at`/`expires_at`, matching
/// [`crate::db::now_iso`].
///
/// Both sides of the expiry comparison are written with this exact function, so
/// the `expires_at <= ?` TEXT comparison in SQL is a valid instant ordering.
pub fn iso(now: DateTime<Utc>) -> String {
    now.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Store one uploaded image: validate, enforce the quotas, write the bytes, and
/// insert the record. Returns the stored record.
///
/// Order of operations: the record is inserted inside a transaction, the bytes
/// are written, and only then does the transaction commit — so a committed row
/// always has its file. A failed write removes any partial file and rolls the
/// row back. `now` is the injected clock (see [`iso`]).
pub async fn store_receipt(
    pool: &SqlitePool,
    config: &ReceiptConfig,
    household_id: &str,
    uploaded_by: &str,
    processing: &str,
    bytes: &[u8],
    now: DateTime<Utc>,
) -> Result<ReceiptImage, ReceiptError> {
    // The processing choice is validated here, the one place a row is written,
    // so no code path can persist `external` while external processing does not
    // exist.
    let processing = ProcessingChoice::parse(processing)?;

    let size = bytes.len() as u64;
    if size > config.max_image_bytes {
        return Err(ReceiptError::ImageTooLarge {
            size,
            limit: config.max_image_bytes,
        });
    }
    let Some(kind) = detect_image(bytes) else {
        return Err(ReceiptError::UnsupportedImageType);
    };

    let mut tx = pool.begin().await?;
    let (count, stored) = usage(&mut tx, household_id, now).await?;
    if count >= config.max_images_per_household {
        return Err(ReceiptError::TooManyImages {
            household_id: household_id.to_string(),
            count,
            limit: config.max_images_per_household,
        });
    }
    if stored + size as i64 > config.max_total_bytes_per_household {
        return Err(ReceiptError::HouseholdStorageFull {
            household_id: household_id.to_string(),
            stored,
            incoming: size,
            limit: config.max_total_bytes_per_household,
        });
    }

    let receipt = ReceiptImage {
        id: Uuid::new_v4().to_string(),
        household_id: household_id.to_string(),
        uploaded_by: Some(uploaded_by.to_string()),
        uploaded_at: iso(now),
        expires_at: iso(now + Duration::days(RECEIPT_TTL_DAYS)),
        content_type: kind.content_type().to_string(),
        byte_size: size as i64,
        sha256_hex: hex::encode(Sha256::digest(bytes)),
        storage_key: generate_storage_key(kind),
        processing: processing.as_db_str().to_string(),
    };

    sqlx::query(
        "INSERT INTO receipt_images \
         (id, household_id, uploaded_by, uploaded_at, expires_at, content_type, \
          byte_size, sha256_hex, storage_key, processing) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )
    .bind(&receipt.id)
    .bind(&receipt.household_id)
    .bind(&receipt.uploaded_by)
    .bind(&receipt.uploaded_at)
    .bind(&receipt.expires_at)
    .bind(&receipt.content_type)
    .bind(receipt.byte_size)
    .bind(&receipt.sha256_hex)
    .bind(&receipt.storage_key)
    .bind(&receipt.processing)
    .execute(&mut *tx)
    .await?;

    // The row is not yet committed: an unwritable file must not survive as a
    // record pointing at nothing.
    if let Err(err) = write_file(&config.dir, &receipt.storage_key, bytes) {
        // Drop the partial write, then let the transaction roll back.
        let _ = remove_file(&config.dir, &receipt.storage_key);
        return Err(err);
    }
    tx.commit().await?;

    Ok(receipt)
}

/// A household's stored image count and total bytes.
async fn usage(
    conn: &mut SqliteConnection,
    household_id: &str,
    now: DateTime<Utc>,
) -> Result<(i64, i64), sqlx::Error> {
    let cutoff = iso(now);
    sqlx::query_as::<_, (i64, i64)>(
        "SELECT COUNT(*), COALESCE(SUM(byte_size), 0) FROM receipt_images \
         WHERE household_id = ?1 AND expires_at > ?2",
    )
    .bind(household_id)
    .bind(cutoff)
    .fetch_one(&mut *conn)
    .await
}

/// A household's receipt images, newest first.
pub async fn list_receipts(
    pool: &SqlitePool,
    household_id: &str,
    now: DateTime<Utc>,
) -> Result<Vec<ReceiptImage>, ReceiptError> {
    let cutoff = iso(now);
    let rows = sqlx::query_as::<_, ReceiptImage>(
        "SELECT id, household_id, uploaded_by, uploaded_at, expires_at, content_type, \
                byte_size, sha256_hex, storage_key, processing \
         FROM receipt_images WHERE household_id = ?1 AND expires_at > ?2 \
         ORDER BY uploaded_at DESC, id DESC",
    )
    .bind(household_id)
    .bind(cutoff)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// One receipt image of `household_id`, or `None`.
///
/// Scoped by household in the query itself: a receipt id from another household
/// is indistinguishable from a missing one, which is what the API needs.
pub async fn get_receipt(
    pool: &SqlitePool,
    household_id: &str,
    id: &str,
    now: DateTime<Utc>,
) -> Result<Option<ReceiptImage>, ReceiptError> {
    let Some(row) = lookup_receipt(pool, household_id, id).await? else {
        return Ok(None);
    };
    Ok((row.expires_at > iso(now)).then_some(row))
}

async fn lookup_receipt(
    pool: &SqlitePool,
    household_id: &str,
    id: &str,
) -> Result<Option<ReceiptImage>, ReceiptError> {
    let row = sqlx::query_as::<_, ReceiptImage>(
        "SELECT id, household_id, uploaded_by, uploaded_at, expires_at, content_type, \
                byte_size, sha256_hex, storage_key, processing \
         FROM receipt_images WHERE household_id = ?1 AND id = ?2",
    )
    .bind(household_id)
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Delete one receipt image: the file first (tolerating one already gone), then
/// the record. Returns `false` when the household has no such receipt.
pub async fn delete_receipt(
    pool: &SqlitePool,
    config: &ReceiptConfig,
    household_id: &str,
    id: &str,
) -> Result<bool, ReceiptError> {
    let Some(receipt) = lookup_receipt(pool, household_id, id).await? else {
        return Ok(false);
    };
    remove_file(&config.dir, &receipt.storage_key)?;
    sqlx::query("DELETE FROM receipt_images WHERE household_id = ?1 AND id = ?2")
        .bind(household_id)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(true)
}

/// Delete every receipt image of `household_id` from disk, for the household
/// delete path, given the rows the delete captured in its transaction.
///
/// The DB rows are removed by the caller's transaction; this removes only the
/// bytes, file-first, so a failure to unlink surfaces as an error (and the
/// caller rolls the delete back) instead of leaving an unreferenced image.
pub fn delete_household_receipt_files(
    dir: &Path,
    storage_keys: &[String],
) -> Result<u64, ReceiptError> {
    remove_files(dir, storage_keys)
}

/// What one expiry sweep removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExpiryReport {
    /// Records whose expiry had passed when the sweep ran.
    pub expired_records: u64,
    /// Records (and therefore files) removed.
    pub records_removed: u64,
    /// Files actually unlinked.
    pub files_removed: u64,
    /// Records whose file was already gone (a tolerated outcome).
    pub files_already_gone: u64,
}

/// Remove every receipt image whose `expires_at` is at or before `now`.
///
/// For each expired record the file is removed first and the record second, and
/// a file that is already gone is counted rather than treated as an error — so
/// the sweep is safe to run repeatedly and safe on a database restored from
/// backup without its images. A file-unlink failure aborts the sweep with the
/// record still in place, ready for the next run.
pub async fn expire_receipts(
    pool: &SqlitePool,
    dir: &Path,
    now: DateTime<Utc>,
) -> Result<ExpiryReport, ReceiptError> {
    let cutoff = iso(now);
    let expired = sqlx::query_as::<_, (String, String)>(
        "SELECT id, storage_key FROM receipt_images \
         WHERE expires_at <= ?1 ORDER BY expires_at, id",
    )
    .bind(&cutoff)
    .fetch_all(pool)
    .await?;

    let mut report = ExpiryReport {
        expired_records: expired.len() as u64,
        ..ExpiryReport::default()
    };
    for (id, storage_key) in expired {
        if remove_file(dir, &storage_key)? {
            report.files_removed += 1;
        } else {
            report.files_already_gone += 1;
        }
        let removed = sqlx::query("DELETE FROM receipt_images WHERE id = ?1")
            .bind(&id)
            .execute(pool)
            .await?;
        report.records_removed += removed.rows_affected();
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_each_accepted_signature_and_rejects_text() {
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0];
        jpeg.extend_from_slice(b"rest");
        assert_eq!(detect_image(&jpeg), Some(ImageKind::Jpeg));

        let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        png.extend_from_slice(b"rest");
        assert_eq!(detect_image(&png), Some(ImageKind::Png));

        let mut webp = b"RIFF\x00\x00\x00\x00WEBP".to_vec();
        webp.extend_from_slice(b"rest");
        assert_eq!(detect_image(&webp), Some(ImageKind::WebP));

        let mut heic = vec![0, 0, 0, 0x18];
        heic.extend_from_slice(b"ftypheic");
        assert_eq!(detect_image(&heic), Some(ImageKind::Heic));

        // A text file renamed to .jpg has no image signature.
        assert_eq!(detect_image(b"just some text, honest"), None);
        // A RIFF container that is not WebP (e.g. WAV) is refused.
        assert_eq!(detect_image(b"RIFF\x00\x00\x00\x00WAVE"), None);
    }

    #[test]
    fn storage_keys_are_random_and_path_safe() {
        let a = generate_storage_key(ImageKind::Jpeg);
        let b = generate_storage_key(ImageKind::Jpeg);
        assert_ne!(a, b);
        assert!(a.ends_with(".jpg"));
        assert!(storage_key_is_safe(&a));
        assert!(storage_key_is_safe(&generate_storage_key(ImageKind::Heic)));

        for bad in [
            "",
            "..",
            "../etc/passwd",
            "evil/name.jpg",
            "short.jpg",
            ".hidden.jpg",
        ] {
            assert!(!storage_key_is_safe(bad), "{bad:?} must be refused");
        }
        assert!(receipt_path(Path::new("/tmp/receipts"), "../escape").is_err());
    }

    #[test]
    fn processing_choice_accepts_local_and_refuses_external() {
        assert_eq!(
            ProcessingChoice::parse("local").expect("local"),
            ProcessingChoice::Local
        );
        assert_eq!(
            ProcessingChoice::parse(" LOCAL ").expect("trimmed"),
            ProcessingChoice::Local
        );
        assert!(matches!(
            ProcessingChoice::parse("external"),
            Err(ReceiptError::ExternalProcessingUnsupported)
        ));
        assert!(matches!(
            ProcessingChoice::parse("sideways"),
            Err(ReceiptError::UnknownProcessing(_))
        ));
    }

    #[test]
    fn default_receipt_dir_sits_beside_the_database() {
        assert_eq!(
            default_receipt_dir("sqlite:///var/lib/buywise/buywise.db"),
            PathBuf::from("/var/lib/buywise/receipts")
        );
        assert_eq!(
            default_receipt_dir("sqlite://buywise.db"),
            PathBuf::from(DEFAULT_RECEIPT_SUBDIR)
        );
        assert_eq!(
            default_receipt_dir("sqlite://file.db?mode=rwc"),
            PathBuf::from(DEFAULT_RECEIPT_SUBDIR)
        );
        assert_eq!(
            default_receipt_dir("sqlite::memory:"),
            PathBuf::from(DEFAULT_RECEIPT_SUBDIR)
        );
    }
}
