//! Authentication primitives: password hashing (argon2), bearer tokens (JWT HS256), and the
//! deterministic email lookup hash. No database or HTTP here — just pure, unit-testable helpers.
//!
//! Privacy (ADR-002): the account is keyed by `email_hash` (a one-way lookup key), never the raw email;
//! the email is used only to derive that key at register/login time and is not stored.

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Hash a password with argon2id (random per-password salt). Returns the PHC-format string to store.
pub fn hash_password(password: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| format!("hash: {e}"))
}

/// Verify a password against a stored PHC hash. False on any parse/verify failure (never panics).
pub fn verify_password(password: &str, phc: &str) -> bool {
    match PasswordHash::new(phc) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// Normalize an email the same way for both hashes: trim, lowercase.
fn normalize(email: &str) -> String {
    email.trim().to_lowercase()
}

/// The account's unique lookup key: HMAC-SHA256 of the normalized email under a server-side pepper.
///
/// Was a bare `sha256(email)`. That is deterministic, which is what a login lookup needs — and also
/// what an attacker holding a dumped table needs. Emails carry almost no entropy: a few hundred
/// million candidates from any breach corpus, hashed once each, reverses the column. The product
/// tells people "we store only a hashed form of your email", and an unsalted digest of a
/// low-entropy input does not make that sentence true.
///
/// A per-row salt is not available to us — the lookup has to be computable from the email alone at
/// login time, and the raw email is never stored (ADR-002). A pepper is the construction that fits:
/// one secret, held by the service and never in the database, so the digest stays deterministic for
/// lookup while an offline attack on a stolen table has nothing to grind against.
pub fn email_hash(email: &str, pepper: &[u8]) -> String {
    let mut mac = <Hmac<Sha256>>::new_from_slice(pepper).expect("HMAC accepts any key length");
    mac.update(normalize(email).as_bytes());
    mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

/// The pre-pepper lookup key, kept ONLY so existing accounts can still log in.
///
/// There is no backfill available: the peppered key cannot be derived from the old one, and the raw
/// email that would let us compute it was never stored. So the migration is lazy — a legacy row is
/// recognised on the next successful login and rewritten to the peppered key then. This function is
/// what recognises it, and it can be deleted once no legacy rows remain.
pub fn email_hash_legacy(email: &str) -> String {
    let digest = Sha256::digest(normalize(email).as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// JWT claims: subject is the account id, `exp` is the Unix expiry.
#[derive(Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub exp: usize,
}

/// Issue a signed HS256 bearer token for an account, valid for `ttl_secs`.
pub fn issue_token(account_id: Uuid, secret: &[u8], ttl_secs: i64) -> Result<String, String> {
    let exp = (chrono::Utc::now().timestamp() + ttl_secs).max(0) as usize;
    let claims = Claims { sub: account_id.to_string(), exp };
    encode(&Header::default(), &claims, &EncodingKey::from_secret(secret)).map_err(|e| format!("jwt encode: {e}"))
}

/// Verify a bearer token's signature + expiry and return the account id it names.
pub fn verify_token(token: &str, secret: &[u8]) -> Result<Uuid, String> {
    let data = decode::<Claims>(token, &DecodingKey::from_secret(secret), &Validation::default())
        .map_err(|e| format!("jwt verify: {e}"))?;
    Uuid::parse_str(&data.claims.sub).map_err(|e| format!("bad subject: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_roundtrip() {
        let phc = hash_password("correct horse battery staple").unwrap();
        assert!(verify_password("correct horse battery staple", &phc));
        assert!(!verify_password("wrong password", &phc));
        assert_ne!(phc, "correct horse battery staple", "stored value is a hash, not the password");
    }

    #[test]
    fn sentinel_hash_never_verifies() {
        // The anonymous account's '!disabled' password_hash must never authenticate anyone.
        assert!(!verify_password("", "!disabled"));
        assert!(!verify_password("!disabled", "!disabled"));
    }

    #[test]
    fn email_hash_is_normalized_and_deterministic() {
        let p = b"pepper";
        assert_eq!(email_hash("  Alice@Example.COM ", p), email_hash("alice@example.com", p));
        assert_ne!(email_hash("a@b.com", p), email_hash("c@d.com", p));
        assert_eq!(email_hash("a@b.com", p).len(), 64, "sha256 hex");
    }

    #[test]
    fn the_pepper_is_what_makes_the_hash_unguessable() {
        // The defect: a dumped `account` table plus any breach corpus reverses an unsalted sha256 of
        // an email, because emails carry almost no entropy. These two assertions are the fix — the
        // digest is no longer computable from the address alone, and it is not the old one.
        assert_ne!(
            email_hash("alice@example.com", b"pepper-a"),
            email_hash("alice@example.com", b"pepper-b"),
            "the same address under different peppers must not collide"
        );
        assert_ne!(
            email_hash("alice@example.com", b"pepper-a"),
            email_hash_legacy("alice@example.com"),
            "and must not reproduce the value an attacker can compute unaided"
        );
    }

    #[test]
    fn the_legacy_hash_still_recognises_an_old_row() {
        // It has to keep working exactly as it did, or every account created before the pepper is
        // locked out — there is no backfill, because the raw email was never stored.
        assert_eq!(email_hash_legacy("  Alice@Example.COM "), email_hash_legacy("alice@example.com"));
        assert_eq!(email_hash_legacy("a@b.com").len(), 64);
    }

    #[test]
    fn token_roundtrip_and_rejects_tampering() {
        let secret = b"test-secret";
        let id = Uuid::new_v4();
        let token = issue_token(id, secret, 3600).unwrap();
        assert_eq!(verify_token(&token, secret).unwrap(), id);
        assert!(verify_token(&token, b"other-secret").is_err(), "wrong signing key rejected");
        assert!(verify_token("not.a.token", secret).is_err());
    }

    #[test]
    fn expired_token_rejected() {
        let secret = b"test-secret";
        // Expire well past jsonwebtoken's default 60s clock-skew leeway.
        let token = issue_token(Uuid::new_v4(), secret, -120).unwrap();
        assert!(verify_token(&token, secret).is_err(), "expired token rejected");
    }
}
