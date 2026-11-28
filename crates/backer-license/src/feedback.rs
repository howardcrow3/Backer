//! Signing for the High Turtle feedback service (`https://feedback.turtlebox.studio/v1/feedback`,
//! see docs/FEEDBACK_API.md).
//!
//! There is no secret in the app. Each install makes its own Ed25519 key the first time it sends
//! feedback and keeps it in the OS keychain (the app shell does that). Every request carries
//! `x-ht-install` (the public key, hex), `x-ht-timestamp` (unix seconds) and `x-ht-signature` (hex
//! Ed25519 signature over [`signing_input`]).

use ed25519_dalek::{Signer, SigningKey};

pub const FEEDBACK_URL: &str = "https://feedback.turtlebox.studio/v1/feedback";
pub const APP_ID: &str = "backer";

fn hex(bytes: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(bytes)
}

/// The exact text that is signed: app id, install id, timestamp and the raw body, so a signature
/// cannot be reused for another app, another install, another moment or another message.
pub fn signing_input(install: &str, timestamp: i64, body: &str) -> String {
    format!("{APP_ID}.{install}.{timestamp}.{body}")
}

/// `(install id, signature)` for one request, both hex.
pub fn sign(key: &SigningKey, timestamp: i64, body: &str) -> (String, String) {
    let install = hex(key.verifying_key().as_bytes());
    let sig = key.sign(signing_input(&install, timestamp, body).as_bytes());
    (install, hex(&sig.to_bytes()))
}

/// A fresh install key. If the system random source fails (not expected) a time based seed is
/// used rather than losing feedback; it only identifies the install to the service.
pub fn new_key() -> SigningKey {
    let mut seed = [0u8; 32];
    if getrandom::fill(&mut seed).is_err() {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        seed[..16].copy_from_slice(&t.to_le_bytes());
    }
    SigningKey::from_bytes(&seed)
}

/// Hex form of a key's 32-byte seed, for the keychain.
pub fn key_to_hex(key: &SigningKey) -> String {
    hex(&key.to_bytes())
}

/// A key from its saved hex seed; `None` for anything that is not exactly 64 hex characters.
pub fn key_from_hex(s: &str) -> Option<SigningKey> {
    let seed: [u8; 32] = data_encoding::HEXLOWER_PERMISSIVE.decode(s.trim().as_bytes()).ok()?.try_into().ok()?;
    Some(SigningKey::from_bytes(&seed))
}

/// A sensible-looking email address, matching the service's own check: one `@`, no spaces, a dotted domain
/// of letters, digits and hyphens, and a top-level domain of at least 2 letters.
pub fn email_ok(e: &str) -> bool {
    let Some((local, domain)) = e.split_once('@') else { return false };
    let bad_local = |c: char| c.is_whitespace() || c.is_control() || "@\"<>()[],;:\\".contains(c);
    let labels: Vec<&str> = domain.split('.').collect();
    let label_ok = |l: &&str| !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-') && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    let tld = labels.last().copied().unwrap_or("");
    e.len() <= 254
        && !local.is_empty()
        && local.len() <= 64
        && !local.chars().any(bad_local)
        && !local.starts_with('.')
        && !local.ends_with('.')
        && !local.contains("..")
        && labels.len() >= 2
        && labels.iter().all(label_ok)
        && (2..=63).contains(&tld.len())
        && tld.chars().all(|c| c.is_ascii_alphabetic())
}

/// What the user typed, checked the same way the service checks it. Returns the JSON body to send.
/// With `anonymous`, the email is left out entirely and the service is told not to keep one.
pub fn body(rating: Option<u8>, message: &str, category: Option<&str>, email: Option<&str>, anonymous: bool, app_version: &str, platform: &str) -> Result<String, String> {
    let message = message.trim();
    if let Some(r) = rating {
        if !(1..=5).contains(&r) {
            return Err("Choose from 1 to 5 stars.".into());
        }
    }
    if rating.is_none() && message.is_empty() {
        return Err("Give a star rating or write a few words.".into());
    }
    if message.chars().count() > 5000 {
        return Err("Please keep it under 5,000 characters.".into());
    }
    let mut v = serde_json::json!({ "app": APP_ID, "appVersion": app_version, "platform": platform });
    if let Some(r) = rating {
        v["rating"] = r.into();
    }
    if !message.is_empty() {
        v["message"] = message.into();
    }
    if let Some(c) = category.filter(|c| ["feature", "issue", "praise", "other"].contains(c)) {
        v["category"] = c.into();
    }
    if anonymous {
        v["anonymous"] = true.into();
    } else if let Some(e) = email.map(str::trim).filter(|e| !e.is_empty()) {
        if !email_ok(e) {
            return Err("That email address doesn’t look right. Fix it, clear it, or tick Send anonymously.".into());
        }
        v["email"] = e.into();
    }
    Ok(v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    #[test]
    fn a_request_is_signed_by_the_install_key_and_verifies() {
        use ed25519_dalek::{Signature, Verifier};
        let k = key(7);
        let body = r#"{"app":"backer","rating":5}"#;
        let (install, sig) = sign(&k, 1_795_694_400, body);
        // The install id is the public key, so the service needs no shared secret to check it.
        assert_eq!(install, hex(k.verifying_key().as_bytes()));
        let sig_bytes: [u8; 64] = data_encoding::HEXLOWER.decode(sig.as_bytes()).unwrap().try_into().unwrap();
        let sig = Signature::from_bytes(&sig_bytes);
        let msg = signing_input(&install, 1_795_694_400, body);
        assert_eq!(msg, format!("backer.{install}.1795694400.{body}"));
        assert!(k.verifying_key().verify(msg.as_bytes(), &sig).is_ok());
        // Any change to the body, time or install breaks it.
        for other in [
            signing_input(&install, 1_795_694_401, body),
            signing_input(&install, 1_795_694_400, r#"{"app":"backer","rating":1}"#),
            signing_input("other", 1_795_694_400, body),
        ] {
            assert!(k.verifying_key().verify(other.as_bytes(), &sig).is_err());
        }
    }

    #[test]
    fn installs_have_different_ids_and_signing_is_deterministic() {
        let (a, sa) = sign(&key(1), 10, "{}");
        let (b, _) = sign(&key(2), 10, "{}");
        assert_ne!(a, b);
        assert_eq!(sign(&key(1), 10, "{}"), (a, sa));
        assert_ne!(new_key().verifying_key().as_bytes(), new_key().verifying_key().as_bytes());
    }

    #[test]
    fn saved_keys_round_trip_and_junk_is_ignored() {
        let k = key(9);
        assert_eq!(key_from_hex(&key_to_hex(&k)).unwrap().to_bytes(), k.to_bytes());
        assert!(key_from_hex("nope").is_none());
        assert!(key_from_hex(&"zz".repeat(32)).is_none());
        assert!(key_from_hex(&"ab".repeat(31)).is_none());
    }

    #[test]
    fn body_rules() {
        assert!(body(None, "  ", None, None, false, "1.0", "macos").is_err());
        assert!(body(Some(0), "", None, None, false, "1.0", "macos").is_err());
        assert!(body(Some(6), "", None, None, false, "1.0", "macos").is_err());
        assert!(body(None, &"x".repeat(5001), None, None, false, "1.0", "macos").is_err());
        let v: serde_json::Value = serde_json::from_str(&body(Some(4), " Add a schedule ", Some("feature"), Some(" "), false, "1.0", "macos").unwrap()).unwrap();
        assert_eq!(v, serde_json::json!({"app":"backer","rating":4,"message":"Add a schedule","category":"feature","appVersion":"1.0","platform":"macos"}));
        let v: serde_json::Value = serde_json::from_str(&body(None, "hi", Some("bogus"), Some("a@b.co"), false, "1.0", "linux").unwrap()).unwrap();
        assert_eq!(v["email"], "a@b.co");
        assert!(v.get("category").is_none());
    }

    #[test]
    fn email_and_anonymous() {
        for good in ["a@b.co", "first.last+tag@mail.example.com", "x_y-z@sub-domain.example.io"] {
            assert!(email_ok(good), "{good}");
        }
        for bad in ["nope", "a@b", "a@b.c", "a b@c.co", "a@b..co", ".a@b.co", "a..b@c.co", "a@-b.co", "<a@b.co>", "a@b@c.co", "a@b.c0"] {
            assert!(!email_ok(bad), "{bad}");
        }
        assert!(body(None, "hi", None, Some("nope"), false, "1.0", "macos").is_err());
        // Anonymous: the address isn't checked or sent, and the service is told so.
        let v: serde_json::Value = serde_json::from_str(&body(None, "hi", None, Some("me@example.com"), true, "1.0", "macos").unwrap()).unwrap();
        assert!(v.get("email").is_none());
        assert_eq!(v["anonymous"], true);
        assert!(body(None, "hi", None, Some("nope"), true, "1.0", "macos").is_ok());
        // Not anonymous: no flag at all, the same body older builds sent.
        let v: serde_json::Value = serde_json::from_str(&body(None, "hi", None, Some("me@example.com"), false, "1.0", "macos").unwrap()).unwrap();
        assert_eq!(v["email"], "me@example.com");
        assert!(v.get("anonymous").is_none());
    }
}
