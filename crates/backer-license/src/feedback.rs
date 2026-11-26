//! Signing for the High Turtle feedback service (`https://feedback.turtlebox.studio/v1/feedback`).
//!
//! Each request carries `x-ht-app`, `x-ht-timestamp` (unix seconds) and `x-ht-signature`: hex
//! HMAC-SHA256 with this app's key over `"<timestamp>.<raw body>"`. The key is compiled in from the
//! `FEEDBACK_KEY` build variable (a GitHub Actions secret), never committed. See
//! howardcrow3/highturtle `feedback/README.md` for the service side.

use sha2::{Digest, Sha256};

pub const FEEDBACK_URL: &str = "https://feedback.turtlebox.studio/v1/feedback";
pub const APP_ID: &str = "backer";

/// Plain HMAC-SHA256 (RFC 2104), hex encoded.
pub fn hmac_sha256_hex(key: &[u8], msg: &[u8]) -> String {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.iter().map(|b| b ^ 0x36).collect::<Vec<u8>>());
    inner.update(msg);
    let mut outer = Sha256::new();
    outer.update(k.iter().map(|b| b ^ 0x5c).collect::<Vec<u8>>());
    outer.update(inner.finalize());
    data_encoding::HEXLOWER.encode(&outer.finalize())
}

/// The signature header value for one request. `key_hex` is the app key as stored by the service.
pub fn sign(key_hex: &str, timestamp: i64, body: &str) -> Option<String> {
    let key = data_encoding::HEXLOWER_PERMISSIVE.decode(key_hex.trim().as_bytes()).ok()?;
    Some(hmac_sha256_hex(&key, format!("{timestamp}.{body}").as_bytes()))
}

/// What the user typed, checked the same way the service checks it. Returns the JSON body to send.
pub fn body(rating: Option<u8>, message: &str, category: Option<&str>, email: Option<&str>, app_version: &str, platform: &str) -> Result<String, String> {
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
    if let Some(e) = email.map(str::trim).filter(|e| !e.is_empty()) {
        v["email"] = e.into();
    }
    Ok(v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc4231() {
        // RFC 4231 test case 2.
        assert_eq!(hmac_sha256_hex(b"Jefe", b"what do ya want for nothing?"), "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
        // Test case 6: a key longer than the block size.
        assert_eq!(
            hmac_sha256_hex(&[0xaa; 131], b"Test Using Larger Than Block-Size Key - Hash Key First"),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn signature_matches_the_service() {
        // Same inputs as the service's JS signer: HMAC(hex-decoded key, "ts.body").
        let key = "ab".repeat(32);
        let sig = sign(&key, 1_795_694_400, r#"{"app":"backer","rating":5}"#).unwrap();
        // Value computed by the service's own signer (feedback/src/core.mjs signBody) for these inputs.
        assert_eq!(sig, "d2523b2c438bb71b1a763eaf45b3eeea2041afaf961bdf15c202c045455d54df");
        assert!(sign("not hex", 1, "{}").is_none());
    }

    #[test]
    fn body_rules() {
        assert!(body(None, "  ", None, None, "1.0", "macos").is_err());
        assert!(body(Some(0), "", None, None, "1.0", "macos").is_err());
        assert!(body(Some(6), "", None, None, "1.0", "macos").is_err());
        assert!(body(None, &"x".repeat(5001), None, None, "1.0", "macos").is_err());
        let v: serde_json::Value = serde_json::from_str(&body(Some(4), " Add a schedule ", Some("feature"), Some(" "), "1.0", "macos").unwrap()).unwrap();
        assert_eq!(v, serde_json::json!({"app":"backer","rating":4,"message":"Add a schedule","category":"feature","appVersion":"1.0","platform":"macos"}));
        let v: serde_json::Value = serde_json::from_str(&body(None, "hi", Some("bogus"), Some("a@b.co"), "1.0", "linux").unwrap()).unwrap();
        assert_eq!(v["email"], "a@b.co");
        assert!(v.get("category").is_none());
    }
}
