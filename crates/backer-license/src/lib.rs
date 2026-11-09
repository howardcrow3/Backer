//! Backer license keys: a short, signed, offline-checkable string.
//!
//! A key carries the buyer's email, the edition and the major version it covers, signed with the seller's
//! Ed25519 private key. The app only holds the public key, so it can check a key with no network, and nobody
//! can make a valid key without the private key.
//!
//! Layout of the signed payload (all one byte unless noted):
//!   format (1) | edition | major | issued day (u32 BE, days since 1970-01-01) | email length | email bytes
//! The key text is `BACKER-` + base32(payload || 64-byte signature), in groups of five characters.

use data_encoding::BASE32_NOPAD;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};

pub const FORMAT: u8 = 1;
/// Edition 1 is the standard (only) edition.
pub const EDITION_STANDARD: u8 = 1;
/// The signature covers this prefix too, so a signature made for some other purpose never validates here.
const DOMAIN: &[u8] = b"backer-license-v1\0";
pub const MAX_EMAIL: usize = 80;
/// Where the "Buy" button goes. Replace with the store's checkout URL.
pub const PURCHASE_URL: &str = include_str!("../purchase_url.txt");

const PUBLIC_KEY_HEX: &str = include_str!("../public_key.hex");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct License {
    pub email: String,
    pub edition: u8,
    /// The app major version this key covers (and every earlier one).
    pub major: u8,
    /// Day the key was issued, as days since 1970-01-01.
    pub issued_day: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LicenseError {
    /// Not a Backer key at all, or damaged (a character mistyped or cut off).
    Malformed,
    /// Well formed, but not signed by us.
    BadSignature,
    /// This build was made without the real public key, so no key can be checked.
    NotConfigured,
}

impl std::fmt::Display for LicenseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Malformed => "That doesn’t look like a Backer license key. Copy the whole key from your purchase email, including the BACKER- at the start.",
            Self::BadSignature => "That key isn’t valid. Check that it was copied exactly, or contact support with your order number.",
            Self::NotConfigured => "This build of Backer can’t check license keys yet.",
        })
    }
}
impl std::error::Error for LicenseError {}

/// The public key built into this app, or None if the build still has the placeholder.
pub fn embedded_public_key() -> Option<[u8; 32]> {
    let bytes = data_encoding::HEXLOWER_PERMISSIVE.decode(PUBLIC_KEY_HEX.trim().as_bytes()).ok()?;
    let k: [u8; 32] = bytes.try_into().ok()?;
    (k != [0u8; 32]).then_some(k)
}

/// Checks a key against the key built into the app.
pub fn verify(key: &str) -> Result<License, LicenseError> {
    verify_with(key, &embedded_public_key().ok_or(LicenseError::NotConfigured)?)
}

pub fn verify_with(key: &str, public_key: &[u8; 32]) -> Result<License, LicenseError> {
    let raw = decode_text(key)?;
    if raw.len() < 8 + 64 {
        return Err(LicenseError::Malformed);
    }
    let (payload, sig) = raw.split_at(raw.len() - 64);
    let lic = parse_payload(payload)?;
    let vk = VerifyingKey::from_bytes(public_key).map_err(|_| LicenseError::NotConfigured)?;
    let sig = Signature::from_slice(sig).map_err(|_| LicenseError::Malformed)?;
    vk.verify(&signed_bytes(payload), &sig).map_err(|_| LicenseError::BadSignature)?;
    Ok(lic)
}

fn signed_bytes(payload: &[u8]) -> Vec<u8> {
    let mut m = DOMAIN.to_vec();
    m.extend_from_slice(payload);
    m
}

fn parse_payload(p: &[u8]) -> Result<License, LicenseError> {
    if p.len() < 8 || p[0] != FORMAT {
        return Err(LicenseError::Malformed);
    }
    let n = p[7] as usize;
    if n == 0 || n > MAX_EMAIL || p.len() != 8 + n {
        return Err(LicenseError::Malformed);
    }
    let email = std::str::from_utf8(&p[8..]).map_err(|_| LicenseError::Malformed)?.to_string();
    Ok(License { email, edition: p[1], major: p[2], issued_day: u32::from_be_bytes([p[3], p[4], p[5], p[6]]) })
}

/// Accepts the key as copied: with or without `BACKER-`, dashes, spaces, line breaks, any letter case.
fn decode_text(key: &str) -> Result<Vec<u8>, LicenseError> {
    let t = key.trim();
    let upper = t.to_ascii_uppercase();
    let body = upper.strip_prefix("BACKER").filter(|r| r.starts_with(['-', ' ', '\n', '\r'])).unwrap_or(&upper);
    let clean: String = body.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    BASE32_NOPAD.decode(clean.as_bytes()).map_err(|_| LicenseError::Malformed)
}

/// Builds a key from a payload and its signature (used by the issuing tool and by tests).
pub fn encode_key(payload: &[u8], signature: &[u8; 64]) -> String {
    let mut raw = payload.to_vec();
    raw.extend_from_slice(signature);
    let b32 = BASE32_NOPAD.encode(&raw);
    let groups: Vec<&str> = b32.as_bytes().chunks(5).map(|c| std::str::from_utf8(c).unwrap()).collect();
    format!("BACKER-{}", groups.join("-"))
}

pub fn make_payload(email: &str, edition: u8, major: u8, issued_day: u32) -> Result<Vec<u8>, LicenseError> {
    let email = email.trim().to_ascii_lowercase();
    if email.is_empty() || email.len() > MAX_EMAIL {
        return Err(LicenseError::Malformed);
    }
    let mut p = vec![FORMAT, edition, major];
    p.extend_from_slice(&issued_day.to_be_bytes());
    p.push(email.len() as u8);
    p.extend_from_slice(email.as_bytes());
    Ok(p)
}

#[cfg(any(feature = "issue", test))]
pub mod issue {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    pub fn new_signing_key() -> SigningKey {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).expect("system random");
        SigningKey::from_bytes(&seed)
    }

    pub fn issue_key(sk: &SigningKey, email: &str, major: u8, issued_day: u32) -> Result<String, LicenseError> {
        let payload = make_payload(email, EDITION_STANDARD, major, issued_day)?;
        let sig = sk.sign(&signed_bytes(&payload));
        Ok(encode_key(&payload, &sig.to_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::issue::*;
    use super::*;

    fn pair() -> (ed25519_dalek::SigningKey, [u8; 32]) {
        let sk = new_signing_key();
        let pk = sk.verifying_key().to_bytes();
        (sk, pk)
    }

    #[test]
    fn a_key_round_trips_and_carries_the_buyer() {
        let (sk, pk) = pair();
        let key = issue_key(&sk, "Howard@Example.com ", 1, 20400).unwrap();
        assert!(key.starts_with("BACKER-"));
        let l = verify_with(&key, &pk).unwrap();
        assert_eq!((l.email.as_str(), l.major, l.edition, l.issued_day), ("howard@example.com", 1, 1, 20400));
    }

    #[test]
    fn copy_and_paste_noise_is_forgiven() {
        let (sk, pk) = pair();
        let key = issue_key(&sk, "a@b.co", 1, 1).unwrap();
        let messy = format!("  {}\n", key.to_lowercase().replace('-', " - "));
        assert!(verify_with(&messy, &pk).is_ok());
        assert!(verify_with(key.trim_start_matches("BACKER-"), &pk).is_ok());
    }

    #[test]
    fn a_changed_character_or_another_signer_is_rejected() {
        let (sk, pk) = pair();
        let key = issue_key(&sk, "a@b.co", 1, 1).unwrap();
        // Flip one character in the middle.
        let mut chars: Vec<char> = key.chars().collect();
        let i = chars.len() / 2;
        chars[i] = if chars[i] == 'A' { 'B' } else { 'A' };
        let bad: String = chars.into_iter().collect();
        assert!(matches!(verify_with(&bad, &pk), Err(LicenseError::BadSignature | LicenseError::Malformed)));
        let (_, other_pk) = pair();
        assert_eq!(verify_with(&key, &other_pk), Err(LicenseError::BadSignature));
        assert_eq!(verify_with("hello", &pk), Err(LicenseError::Malformed));
        assert_eq!(verify_with("", &pk), Err(LicenseError::Malformed));
    }

    #[test]
    fn the_email_cannot_be_swapped_without_breaking_the_signature() {
        let (sk, pk) = pair();
        let a = issue_key(&sk, "aaaa@x.io", 1, 5).unwrap();
        let b = issue_key(&sk, "bbbb@x.io", 1, 5).unwrap();
        // Splice the payload of one with the signature of the other.
        let ra = decode_text(&a).unwrap();
        let rb = decode_text(&b).unwrap();
        let mut forged = rb[..rb.len() - 64].to_vec();
        forged.extend_from_slice(&ra[ra.len() - 64..]);
        let text = BASE32_NOPAD.encode(&forged);
        assert_eq!(verify_with(&text, &pk), Err(LicenseError::BadSignature));
    }

    #[test]
    fn a_build_without_the_real_public_key_says_so() {
        assert_eq!(embedded_public_key(), None);
        assert_eq!(verify("BACKER-AAAAA"), Err(LicenseError::NotConfigured));
    }

    #[test]
    fn emails_must_be_sensible() {
        assert!(make_payload("", 1, 1, 1).is_err());
        assert!(make_payload(&"x".repeat(81), 1, 1, 1).is_err());
    }
}
