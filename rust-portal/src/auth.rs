//! Passwords, TOTP, session tokens and the failed-login ban.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use argon2::Argon2;
use rand::RngCore as _;
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;
use totp_rs::{Algorithm, Builder, Secret};

/// Checks a password against a stored hash.
///
/// Two formats exist:
/// - `argon2id$<salt hex>$<hash hex>`: our own, default Argon2id parameters.
/// - `guac-sha256$<salt hex>$<hash hex>`: imported from Guacamole, which stores
///   SHA-256(password + uppercase hex of the salt). It is replaced on the first
///   successful login (see [`needs_rehash`]).
pub fn verify_password(stored: &str, password: &str) -> bool {
    let Some((scheme, rest)) = stored.split_once('$') else {
        return false;
    };
    let Some((salt_hex, hash_hex)) = rest.split_once('$') else {
        return false;
    };
    let Ok(expected) = hex::decode(hash_hex) else {
        return false;
    };
    let computed: Vec<u8> = match scheme {
        "guac-sha256" => {
            let mut hasher = Sha256::new();
            hasher.update(password.as_bytes());
            hasher.update(salt_hex.to_ascii_uppercase().as_bytes());
            hasher.finalize().to_vec()
        }
        "argon2id" => {
            let Ok(salt) = hex::decode(salt_hex) else {
                return false;
            };
            let mut out = vec![0u8; expected.len().max(16)];
            if Argon2::default().hash_password_into(password.as_bytes(), &salt, &mut out).is_err() {
                return false;
            }
            out
        }
        _ => return false,
    };
    computed.len() == expected.len() && bool::from(computed.ct_eq(&expected))
}

pub fn needs_rehash(stored: &str) -> bool {
    !stored.starts_with("argon2id$")
}

pub fn hash_password(password: &str) -> anyhow::Result<String> {
    let mut salt = [0u8; 16];
    rand::rng().fill_bytes(&mut salt);
    let mut out = [0u8; 32];
    Argon2::default()
        .hash_password_into(password.as_bytes(), &salt, &mut out)
        .map_err(|e| anyhow::anyhow!("argon2: {e}"))?;
    Ok(format!("argon2id${}${}", hex::encode(salt), hex::encode(out)))
}

/// Verifies a 6-digit TOTP code (SHA-1, 30 s steps, one step of clock skew),
/// matching Guacamole's defaults. Returns the accepted time step.
pub fn verify_totp(secret_base32: &str, code: &str) -> Option<i64> {
    let secret = Secret::try_from_base32(secret_base32.trim().trim_end_matches('=')).ok()?;
    let totp = Builder::new()
        .with_algorithm(Algorithm::SHA1)
        .with_digits(6)
        .with_skew(1)
        .with_step_duration(30)
        .with_secret(secret)
        .build_noncompliant();
    let code: String = code.chars().filter(|c| c.is_ascii_digit()).collect();
    // check_current returns the matched time step (not a timestamp).
    let step = totp.check_current(&code)?;
    i64::try_from(step).ok()
}

/// A new random TOTP secret, base32 (160 bits, as authenticator apps expect).
pub fn new_totp_secret() -> String {
    let mut raw = [0u8; 20];
    rand::rng().fill_bytes(&mut raw);
    Secret::from(raw).to_base32()
}

/// The `otpauth://` link and its QR code (SVG) for an authenticator app.
pub fn totp_enrollment(issuer: &str, username: &str, secret: &str) -> (String, String) {
    let enc = |s: &str| -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
                _ => format!("%{b:02X}"),
            })
            .collect()
    };
    let uri = format!(
        "otpauth://totp/{}:{}?secret={secret}&issuer={}&algorithm=SHA1&digits=6&period=30",
        enc(issuer),
        enc(username),
        enc(issuer)
    );
    let svg = qrcode::QrCode::new(uri.as_bytes())
        .map(|code| {
            code.render::<qrcode::render::svg::Color<'_>>()
                .min_dimensions(200, 200)
                .quiet_zone(true)
                .build()
        })
        .unwrap_or_default();
    (uri, svg)
}

/// A new random session token and the hash stored in the database.
pub fn new_session_token() -> (String, String) {
    let mut raw = [0u8; 32];
    rand::rng().fill_bytes(&mut raw);
    let token = hex::encode(raw);
    let hash = token_hash(&token);
    (token, hash)
}

pub fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// Bans an address after too many failed logins, like Guacamole's ban extension.
pub struct LoginBan {
    max_failures: u32,
    window: Duration,
    failures: Mutex<HashMap<IpAddr, (u32, Instant)>>,
}

impl LoginBan {
    pub fn new(max_failures: u32, window: Duration) -> Self {
        Self {
            max_failures,
            window,
            failures: Mutex::new(HashMap::new()),
        }
    }

    pub fn is_banned(&self, ip: IpAddr) -> bool {
        let mut map = self.failures.lock().unwrap();
        match map.get(&ip) {
            Some((_, since)) if since.elapsed() > self.window => {
                map.remove(&ip);
                false
            }
            Some((count, _)) => *count >= self.max_failures,
            None => false,
        }
    }

    pub fn record_failure(&self, ip: IpAddr) {
        let mut map = self.failures.lock().unwrap();
        let entry = map.entry(ip).or_insert((0, Instant::now()));
        if entry.1.elapsed() > self.window {
            *entry = (0, Instant::now());
        }
        entry.0 += 1;
    }

    pub fn clear(&self, ip: IpAddr) {
        self.failures.lock().unwrap().remove(&ip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guacamole_hash() {
        // SHA-256("secret" + "ABCD")
        let hash = hex::encode(Sha256::digest(b"secretABCD"));
        let stored = format!("guac-sha256$abcd${hash}");
        assert!(verify_password(&stored, "secret"));
        assert!(!verify_password(&stored, "wrong"));
    }

    #[test]
    fn totp_secret_roundtrip() {
        let secret = new_totp_secret();
        assert_eq!(secret.len(), 32);
        let totp = Builder::new()
            .with_secret(Secret::try_from_base32(&secret).unwrap())
            .build_noncompliant();
        let code = totp.generate_current().to_string();
        let step = verify_totp(&secret, &code).unwrap();
        // A real time step (about 59 million in 2026), not step / 30.
        let now_step = (crate::db::now_unix() / 30) as i64;
        assert!((now_step - 1..=now_step + 1).contains(&step), "{step} vs {now_step}");
        let (uri, svg) = totp_enrollment("2FA SW", "a b", &secret);
        assert!(uri.starts_with("otpauth://totp/2FA%20SW:a%20b?secret="));
        assert!(uri.contains("&issuer=2FA%20SW&"));
        assert!(svg.contains("<svg"));
    }

    #[test]
    fn argon2_roundtrip() {
        let stored = hash_password("pw").unwrap();
        assert!(verify_password(&stored, "pw"));
        assert!(!verify_password(&stored, "px"));
        assert!(!needs_rehash(&stored));
    }
}
