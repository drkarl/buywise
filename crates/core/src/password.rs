//! Argon2id password hashing for storage in `users.password_hash`.
//!
//! Lives in `buywise-core` so both `buywise-api` (which verifies passwords at
//! login) and `buywise-cli` (the `set-password` operator tool that seeds the
//! column, issue #51) share one hashing implementation without an
//! `api ↔ cli` dependency. `buywise_api::auth` re-exports
//! [`hash_password_for_storage`] so the API's call sites keep their existing
//! path.

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use rand::rngs::OsRng;

/// Hash a plaintext password with argon2id, producing a PHC string suitable for
/// storage in `users.password_hash`.
///
/// Each call generates a fresh random salt (via the OS CSPRNG), so the same
/// password hashes to a different PHC string every time; the salt is embedded in
/// the returned string and read back during verification. Defaults to the
/// `argon2` crate's recommended argon2id parameters — the same configuration the
/// API's login path verifies against (`Argon2::default()`), so a hash produced
/// here verifies there.
pub fn hash_password_for_storage(password: &str) -> Result<String, argon2::password_hash::Error> {
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default().hash_password(password.as_bytes(), &salt)?;
    Ok(hash.to_string())
}

/// Verify a plaintext password against a stored argon2 PHC hash.
///
/// Returns `false` for any malformed stored hash or verification failure; never
/// panics and never compares plaintext directly. Mirrors the API login path so
/// tests can assert a stored hash round-trips.
pub fn verify_password(password: &str, stored_hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(stored_hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_then_verify_round_trips() {
        let hash = hash_password_for_storage("correct horse battery staple").unwrap();
        assert!(verify_password("correct horse battery staple", &hash));
        assert!(!verify_password("wrong password", &hash));
    }

    #[test]
    fn hash_is_a_parseable_argon2id_phc_string() {
        let hash = hash_password_for_storage("hunter2").unwrap();
        // The PHC string must parse and identify the argon2id variant.
        assert!(hash.starts_with("$argon2id$"), "got: {hash}");
        assert!(PasswordHash::new(&hash).is_ok(), "PHC must parse: {hash}");
    }

    #[test]
    fn each_hash_uses_a_fresh_salt() {
        let a = hash_password_for_storage("same-password").unwrap();
        let b = hash_password_for_storage("same-password").unwrap();
        assert_ne!(a, b, "a random salt must make repeated hashes differ");
    }

    #[test]
    fn verify_rejects_malformed_hash() {
        assert!(!verify_password("anything", "not-a-phc-hash"));
        assert!(!verify_password("anything", ""));
    }
}
