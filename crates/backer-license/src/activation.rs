//! Online activation: "a key works on 2 computers".
//!
//! The license service (turtlebox.studio) counts computers per key. When a key is entered the app sends the key and a
//! fingerprint of this computer; the service answers with an activation token, signed with the same private key that
//! signs license keys. The app checks the token offline, so a backup never needs the network.
//!
//! Token: base64url(json) + "." + base64url(Ed25519 signature over `DOMAIN` + the json bytes), no padding. The domain
//! prefix differs from the license-key one, so a signature made for one purpose never validates for the other.

use data_encoding::BASE64URL_NOPAD;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const DOMAIN: &[u8] = b"backer-activation-v1\0";
/// Where the app asks. The path after this is `/activate` or `/deactivate`.
pub const SERVER_URL: &str = "https://turtlebox.studio/api/backer/v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activation {
    pub key_id: String,
    pub machine: String,
    /// Unix seconds. The app asks for a fresh token once `refresh_after` has passed.
    pub issued_at: i64,
    pub refresh_after: i64,
    /// After this the token no longer proves anything and the key is treated as not yet activated.
    pub expires_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivationError {
    Malformed,
    BadSignature,
}

#[derive(Deserialize)]
struct Claims {
    kind: String,
    key_id: String,
    machine: String,
    issued_at: String,
    refresh_after: String,
    expires_at: String,
}

fn secs(iso: &str) -> Result<i64, ActivationError> {
    iso.parse::<jiff::Timestamp>().map(|t| t.as_second()).map_err(|_| ActivationError::Malformed)
}

pub fn verify_activation(token: &str, public_key: &[u8; 32]) -> Result<Activation, ActivationError> {
    let (p, s) = token.trim().split_once('.').ok_or(ActivationError::Malformed)?;
    let payload = BASE64URL_NOPAD.decode(p.as_bytes()).map_err(|_| ActivationError::Malformed)?;
    let sig = BASE64URL_NOPAD.decode(s.as_bytes()).map_err(|_| ActivationError::Malformed)?;
    let sig = Signature::from_slice(&sig).map_err(|_| ActivationError::Malformed)?;
    let vk = VerifyingKey::from_bytes(public_key).map_err(|_| ActivationError::BadSignature)?;
    let mut msg = DOMAIN.to_vec();
    msg.extend_from_slice(&payload);
    vk.verify(&msg, &sig).map_err(|_| ActivationError::BadSignature)?;
    let c: Claims = serde_json::from_slice(&payload).map_err(|_| ActivationError::Malformed)?;
    if c.kind != "backer-activation" {
        return Err(ActivationError::Malformed);
    }
    Ok(Activation { key_id: c.key_id, machine: c.machine, issued_at: secs(&c.issued_at)?, refresh_after: secs(&c.refresh_after)?, expires_at: secs(&c.expires_at)? })
}

/// Turns whatever identifies this computer (the OS machine id) into the fingerprint sent to the service.
/// Hashed with an app-specific prefix, so the raw machine id never leaves the computer and cannot be matched to other apps.
pub fn machine_fingerprint(raw_machine_id: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"backer-machine-v1|");
    h.update(raw_machine_id.trim().to_lowercase().as_bytes());
    data_encoding::HEXLOWER.encode(&h.finalize())
}

#[cfg(any(feature = "issue", test))]
pub mod issue {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn iso(t: i64) -> String {
        jiff::Timestamp::from_second(t).unwrap().strftime("%Y-%m-%dT%H:%M:%SZ").to_string()
    }

    /// Makes a token the way the license service does (used by tests).
    pub fn sign_activation(sk: &SigningKey, key_id: &str, machine: &str, issued_at: i64, refresh_days: i64, valid_days: i64) -> String {
        let claims = serde_json::json!({
            "kind": "backer-activation", "key_id": key_id, "machine": machine,
            "issued_at": iso(issued_at), "refresh_after": iso(issued_at + refresh_days * 86_400), "expires_at": iso(issued_at + valid_days * 86_400),
        });
        let payload = serde_json::to_vec(&claims).unwrap();
        let mut msg = DOMAIN.to_vec();
        msg.extend_from_slice(&payload);
        format!("{}.{}", BASE64URL_NOPAD.encode(&payload), BASE64URL_NOPAD.encode(&sk.sign(&msg).to_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::issue::*;
    use super::*;
    use crate::issue::{issue_key, new_signing_key};

    #[test]
    fn a_token_round_trips_and_is_bound_to_its_signer() {
        let sk = new_signing_key();
        let pk = sk.verifying_key().to_bytes();
        let t = sign_activation(&sk, "abc", &"m".repeat(64), 1_800_000_000, 7, 90);
        let a = verify_activation(&t, &pk).unwrap();
        assert_eq!((a.key_id.as_str(), a.issued_at, a.refresh_after, a.expires_at), ("abc", 1_800_000_000, 1_800_000_000 + 7 * 86_400, 1_800_000_000 + 90 * 86_400));
        let other = new_signing_key().verifying_key().to_bytes();
        assert_eq!(verify_activation(&t, &other), Err(ActivationError::BadSignature));
        // Tampering with the claims breaks the signature.
        let (p, s) = t.split_once('.').unwrap();
        let mut body = BASE64URL_NOPAD.decode(p.as_bytes()).unwrap();
        body[20] ^= 1;
        assert!(verify_activation(&format!("{}.{s}", BASE64URL_NOPAD.encode(&body)), &pk).is_err());
        assert_eq!(verify_activation("nonsense", &pk), Err(ActivationError::Malformed));
        assert_eq!(verify_activation("", &pk), Err(ActivationError::Malformed));
    }

    #[test]
    fn a_license_key_signature_is_not_an_activation_token() {
        // The same private key signs both; the domain prefixes must keep them apart.
        let sk = new_signing_key();
        let pk = sk.verifying_key().to_bytes();
        let key = issue_key(&sk, "a@b.co", 1, 1).unwrap();
        let raw = data_encoding::BASE32_NOPAD.decode(key.trim_start_matches("BACKER-").replace('-', "").as_bytes()).unwrap();
        let (payload, sig) = raw.split_at(raw.len() - 64);
        let forged = format!("{}.{}", BASE64URL_NOPAD.encode(payload), BASE64URL_NOPAD.encode(sig));
        assert!(verify_activation(&forged, &pk).is_err());
    }

    #[test]
    fn key_ids_ignore_how_the_key_is_typed_and_fingerprints_are_stable() {
        let sk = new_signing_key();
        let key = issue_key(&sk, "a@b.co", 1, 1).unwrap();
        let messy = format!("  {}\n", key.to_lowercase().replace('-', " "));
        assert_eq!(crate::key_id(&key).unwrap(), crate::key_id(&messy).unwrap());
        assert_eq!(crate::key_id(&key).unwrap().len(), 64);
        assert!(crate::key_id("hello").is_err());
        let f = machine_fingerprint("ABC-123");
        assert_eq!(f.len(), 64);
        assert_eq!(f, machine_fingerprint(" abc-123\n"));
        assert_ne!(f, machine_fingerprint("abc-124"));
    }
}
