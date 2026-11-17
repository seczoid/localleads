//! Passwords, sign-in sessions and MCP API keys.
//!
//! - Passwords are stored as Argon2id hashes (PHC strings with their salt).
//! - Session tokens and API keys are long random strings. Only their SHA-256
//!   hash is stored, so a copy of the database can't be used to sign in or
//!   call MCP. They're high-entropy, so a fast hash is enough.

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use rand::Rng;
use rand::distr::Alphanumeric;
use sha2::{Digest, Sha256};

/// Every API key starts with this, so it's easy to recognise in configs and
/// to spot if it leaks.
pub const API_KEY_PREFIX: &str = "ll_";
/// How many characters of a key are kept in plain text, to tell keys apart.
const SHOWN_CHARS: usize = 10;
pub const MIN_PASSWORD_CHARS: usize = 8;
/// The sign-in cookie.
pub const SESSION_COOKIE: &str = "ll_session";
pub const SESSION_DAYS: i64 = 30;
/// Environment variable `localleads mcp` reads its API key from.
pub const API_KEY_ENV: &str = "LOCALLEADS_API_KEY";

pub fn hash_password(password: &str) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|error| anyhow::anyhow!("hash password: {error}"))
}

pub fn verify_password(password: &str, stored: &str) -> bool {
    PasswordHash::new(stored)
        .is_ok_and(|hash| Argon2::default().verify_password(password.as_bytes(), &hash).is_ok())
}

fn random_text(chars: usize) -> String {
    rand::rng()
        .sample_iter(&Alphanumeric)
        .take(chars)
        .map(char::from)
        .collect()
}

/// A new sign-in session token (about 280 bits).
pub fn new_session_token() -> String {
    random_text(48)
}

/// A one-time code for creating the account on an exposed server, grouped
/// for reading off a log: `k7Pq-2mXa-9TzR-bW4e`.
pub fn new_setup_code() -> String {
    let text = random_text(16);
    text.as_bytes()
        .chunks(4)
        .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
        .collect::<Vec<_>>()
        .join("-")
}

/// A new API key and the part of it that's kept to tell keys apart.
pub fn new_api_key() -> (String, String) {
    let key = format!("{API_KEY_PREFIX}{}", random_text(40));
    let shown = key.chars().take(SHOWN_CHARS).collect();
    (key, shown)
}

/// What the database stores for a token or key.
pub fn digest(secret: &str) -> String {
    let hash = Sha256::digest(secret.trim().as_bytes());
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A trimmed, lower-cased email, or why it isn't one.
pub fn normalize_email(email: &str) -> Result<String, &'static str> {
    let email = email.trim().to_lowercase();
    let Some((local, domain)) = email.split_once('@') else {
        return Err("Enter a valid email address.");
    };
    if local.is_empty() || !domain.contains('.') || domain.starts_with('.') || domain.ends_with('.')
        || email.chars().any(char::is_whitespace) || email.len() > 254
    {
        return Err("Enter a valid email address.");
    }
    Ok(email)
}

/// Why a new password can't be used, if it can't.
pub fn check_new_password(password: &str, confirm: &str) -> Result<(), &'static str> {
    if password.chars().count() < MIN_PASSWORD_CHARS {
        return Err("Use at least 8 characters for the password.");
    }
    if password.len() > 1024 {
        return Err("That password is too long.");
    }
    if password != confirm {
        return Err("The two passwords don't match.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_verify_only_themselves() {
        let hash = hash_password("correct horse").unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(verify_password("correct horse", &hash));
        assert!(!verify_password("correct horsf", &hash));
        assert!(!verify_password("correct horse", "not a hash"));
    }

    #[test]
    fn api_keys_are_long_and_hashed() {
        let (key, shown) = new_api_key();
        assert!(key.starts_with(API_KEY_PREFIX));
        assert_eq!(key.len(), API_KEY_PREFIX.len() + 40);
        assert!(key.starts_with(&shown));
        assert_ne!(new_api_key().0, key);
        assert_eq!(digest(&key).len(), 64);
        assert_eq!(digest(&key), digest(&format!(" {key}\n")));
    }

    #[test]
    fn emails_and_passwords_are_checked() {
        assert_eq!(normalize_email("  Jo@Example.COM ").unwrap(), "jo@example.com");
        assert!(normalize_email("jo@example").is_err());
        assert!(normalize_email("jo example.com").is_err());
        assert!(check_new_password("short", "short").is_err());
        assert!(check_new_password("long enough", "long enougH").is_err());
        assert!(check_new_password("long enough", "long enough").is_ok());
    }
}
