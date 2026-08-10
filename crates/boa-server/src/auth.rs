//! Passwords and login tokens.
//!
//! Argon2id at its default parameters, which on this hardware is about 60 ms and
//! 19 MiB per attempt. That is the point: a self-hosted server's database will end
//! up in a backup on somebody's laptop, and the only thing standing between that
//! file and everybody's password is how expensive one guess is. It also means
//! hashing must not happen on a thread that is meanwhile supposed to be forwarding
//! audio — see [`hash_password`].
//!
//! Tokens are 32 random bytes, not JWTs. A token here is only ever presented to the
//! server that issued it, which is the case where a signed self-describing token
//! buys nothing and costs the ability to revoke: a row can be deleted, a signature
//! cannot be un-signed.

use anyhow::{anyhow, Result};
use argon2::password_hash::{PasswordHash, PasswordHasher as _, PasswordVerifier as _, SaltString};
use argon2::Argon2;
use rand::RngCore as _;

/// argon2 0.5 speaks `rand_core` 0.6 and the rest of the workspace speaks `rand` 0.9,
/// which is a different `RngCore` trait with the same name. Re-exporting the one
/// argon2 wants under a distinct name is how the two coexist without either being
/// pinned back — the alternative is a wrong-trait error that reads as if `OsRng` had
/// lost a method.
use argon2::password_hash::rand_core::OsRng as SaltRng;

/// Shortest password accepted.
///
/// Twelve, and no complexity rules. Length is the only requirement that reliably
/// buys entropy; a rule demanding a digit and a capital reliably buys `Password1!`.
pub const MIN_PASSWORD: usize = 12;

/// Longest, because Argon2 hashes whatever it is given and a 10 MB "password" is a
/// denial-of-service dressed as a login.
pub const MAX_PASSWORD: usize = 1024;

/// Hash a password for storage.
///
/// Blocking and deliberately slow. Callers on the async runtime must put this on a
/// blocking thread — 60 ms on a runtime worker is 60 ms during which that worker
/// forwards no packets and answers no frames, and a handful of simultaneous logins
/// would stall the whole server.
pub fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut SaltRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|err| anyhow!("hashing: {err}"))
}

/// Check a password against a stored hash.
///
/// A malformed stored hash is a failed verification, not an error the caller has to
/// handle separately: there is nothing useful to do differently, and the alternative
/// is a code path where a corrupt row could be mistaken for a successful login.
pub fn verify_password(password: &str, stored: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(stored) else {
        log::error!("a stored password hash is not parseable; treating it as a failed login");
        return false;
    };
    Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok()
}

/// Whether a password is acceptable, and why not if it is not.
pub fn check_password_rules(password: &str) -> Result<()> {
    if password.chars().count() < MIN_PASSWORD {
        return Err(anyhow!("a password needs at least {MIN_PASSWORD} characters"));
    }
    if password.len() > MAX_PASSWORD {
        return Err(anyhow!("that password is unreasonably long"));
    }
    Ok(())
}

/// A fresh login token: 32 random bytes, URL-safe base64, no padding.
///
/// URL-safe because it travels in a query string on the attachment endpoints, where
/// an ordinary base64 `+` and `/` would need escaping and would eventually reach a
/// client that forgot to.
pub fn new_token() -> String {
    use base64::Engine as _;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Fold a login name to its canonical form.
///
/// Lowercased and trimmed, so the database's `COLLATE NOCASE` and this agree; if
/// they disagreed, a name could pass this check and then collide in the insert.
pub fn normalise_name(name: &str) -> String {
    name.trim().to_lowercase()
}

/// Whether a login name is usable.
pub fn check_name_rules(name: &str) -> Result<()> {
    let name = normalise_name(name);
    if name.chars().count() < 2 {
        return Err(anyhow!("a name needs at least two characters"));
    }
    if name.chars().count() > 32 {
        return Err(anyhow!("a name can be at most 32 characters"));
    }
    // ASCII-only, and a deliberate narrowing rather than an oversight: a login name
    // is an identifier people type at each other, and allowing the full Unicode
    // range allows two visually identical names (Cyrillic "а" against Latin "a").
    // Display names have no such restriction, which is where anybody who wants
    // their own script should put it.
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-') {
        return Err(anyhow!("names may use letters, digits, dot, dash and underscore"));
    }
    if !name.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        return Err(anyhow!("a name has to start with a letter or a digit"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_password_verifies_against_its_own_hash_and_nothing_else() {
        let hash = hash_password("correct horse battery").unwrap();
        assert!(verify_password("correct horse battery", &hash));
        assert!(!verify_password("correct horse batter", &hash));
        assert!(!verify_password("", &hash));
    }

    /// Two hashes of the same password must differ, which is what the per-hash salt
    /// is for: identical hashes would let anyone reading the database see which
    /// accounts share a password.
    #[test]
    fn the_same_password_hashes_differently_every_time() {
        let one = hash_password("correct horse battery").unwrap();
        let two = hash_password("correct horse battery").unwrap();
        assert_ne!(one, two);
        assert!(verify_password("correct horse battery", &one));
        assert!(verify_password("correct horse battery", &two));
    }

    #[test]
    fn a_corrupt_stored_hash_is_a_failed_login_not_a_crash() {
        assert!(!verify_password("anything", ""));
        assert!(!verify_password("anything", "not a phc string"));
        assert!(!verify_password("anything", "$argon2id$v=19$m=1,t=1,p=1$"));
    }

    #[test]
    fn password_rules_are_about_length_only() {
        assert!(check_password_rules("penguins are fine").is_ok());
        assert!(check_password_rules("short").is_err());
        assert!(check_password_rules(&"a".repeat(MIN_PASSWORD)).is_ok());
        assert!(check_password_rules(&"a".repeat(MIN_PASSWORD - 1)).is_err());
        assert!(check_password_rules(&"a".repeat(MAX_PASSWORD + 1)).is_err());
        // Counted in characters, not bytes, so a short password in a non-Latin
        // script is not accidentally accepted for being long in UTF-8.
        assert!(check_password_rules("äöü").is_err());
    }

    #[test]
    fn tokens_are_long_random_and_url_safe() {
        let token = new_token();
        assert!(token.len() >= 43, "32 bytes of base64");
        assert!(token.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'), "{token}");
        assert_ne!(token, new_token());
    }

    #[test]
    fn names_fold_the_way_the_database_compares_them() {
        assert_eq!(normalise_name("  AdA  "), "ada");
        assert_eq!(normalise_name("Ada"), normalise_name("ADA"));
    }

    #[test]
    fn name_rules_keep_out_the_look_alikes() {
        assert!(check_name_rules("ada").is_ok());
        assert!(check_name_rules("ada.lovelace_1-x").is_ok());
        assert!(check_name_rules("a").is_err(), "too short");
        assert!(check_name_rules(&"a".repeat(33)).is_err());
        assert!(check_name_rules(".ada").is_err(), "must start alphanumeric");
        assert!(check_name_rules("ada lovelace").is_err(), "no spaces");
        // The one that matters: this is Cyrillic "а", which would otherwise be an
        // account indistinguishable from "ada" in every list it appears in.
        assert!(check_name_rules("аda").is_err());
    }
}
