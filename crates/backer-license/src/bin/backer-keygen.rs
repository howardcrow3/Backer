//! The seller's tool. Never shipped with the app.
//!
//!   backer-keygen init <private-key-file>            make a key pair; prints the public key to put in the app
//!   backer-keygen issue <private-key-file> <email>   make a license key by hand (gifts, support, testing)
//!   backer-keygen check <public-key-hex> <key>       check a key

use backer_license::{issue, verify_with, EDITION_STANDARD};
use ed25519_dalek::SigningKey;
use std::{env, fs, process::exit};

fn today() -> u32 {
    let s = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    (s / 86_400) as u32
}

fn load(path: &str) -> SigningKey {
    let hex = fs::read_to_string(path).unwrap_or_else(|e| die(&format!("can't read {path}: {e}")));
    let bytes = data_encoding::HEXLOWER_PERMISSIVE.decode(hex.trim().as_bytes()).unwrap_or_else(|_| die("private key file is not hex"));
    SigningKey::from_bytes(&bytes.try_into().unwrap_or_else(|_| die("private key must be 32 bytes")))
}

fn die(m: &str) -> ! {
    eprintln!("{m}");
    exit(1)
}

fn main() {
    let a: Vec<String> = env::args().collect();
    match a.get(1).map(String::as_str) {
        Some("init") if a.len() == 3 => {
            if fs::metadata(&a[2]).is_ok() {
                die("that file already exists; refusing to overwrite a private key");
            }
            let sk = issue::new_signing_key();
            fs::write(&a[2], data_encoding::HEXLOWER.encode(&sk.to_bytes())).unwrap_or_else(|e| die(&e.to_string()));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&a[2], fs::Permissions::from_mode(0o600));
            }
            println!("Private key written to {} (keep it secret and backed up; never commit it).", a[2]);
            println!("Public key (safe to share, goes into the app):\n{}", data_encoding::HEXLOWER.encode(&sk.verifying_key().to_bytes()));
        }
        Some("issue") if a.len() == 4 => {
            let key = issue::issue_key(&load(&a[2]), &a[3], 1, today()).unwrap_or_else(|e| die(&e.to_string()));
            println!("{key}");
        }
        Some("check") if a.len() == 4 => {
            let pk: [u8; 32] = data_encoding::HEXLOWER_PERMISSIVE.decode(a[2].trim().as_bytes()).ok().and_then(|v| v.try_into().ok()).unwrap_or_else(|| die("public key must be 64 hex characters"));
            match verify_with(&a[3], &pk) {
                Ok(l) => println!("valid: {} (edition {}, covers version {}.x, issued day {})", l.email, l.edition, l.major, l.issued_day),
                Err(e) => die(&e.to_string()),
            }
        }
        _ => die("usage: backer-keygen init <private-key-file> | issue <private-key-file> <email> | check <public-key-hex> <key>"),
    }
    let _ = EDITION_STANDARD;
}
