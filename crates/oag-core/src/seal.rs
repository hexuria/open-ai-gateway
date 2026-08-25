//! Sealing credentials at rest.
//!
//! Upstream credentials are retrievable secrets, not passwords: we have to send
//! the original bytes to the provider, so hashing is not an option and
//! authenticated encryption is. XChaCha20-Poly1305 under a key-encryption key
//! supplied by the environment.
//!
//! sub2api stores OAuth access and refresh tokens as plaintext JSONB, which
//! makes a database backup a credential dump and a read-only SQL grant a
//! credential grant. The cost of not doing that is this file.

use base64::Engine as _;
use chacha20poly1305::aead::{Aead, KeyInit, OsRng, rand_core::RngCore};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use zeroize::{Zeroize, ZeroizeOnDrop};

/// The key-encryption key.
///
/// Loaded once at boot from `security.credential_kek` and held for the process
/// lifetime. Zeroized on drop; the `Debug` impl is hand-written so it cannot
/// reach a log through a `tracing` field.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct Kek([u8; 32]);

impl std::fmt::Debug for Kek {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Kek(<redacted>)")
    }
}

/// Ciphertext and the nonce it was produced under.
///
/// Stored as two columns rather than one concatenated blob so that a future
/// key rotation can rewrite ciphertext without re-parsing a packed format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sealed {
    pub ciphertext: Vec<u8>,
    pub nonce: Vec<u8>,
}

impl Kek {
    /// Parse a base64-encoded 32-byte key.
    pub fn from_base64(encoded: &str) -> crate::Result<Self> {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(encoded.trim())
            .map_err(|_| {
                crate::Error::Config(
                    "security.credential_kek is not valid base64; \
                     generate one with `openssl rand -base64 32`"
                        .to_owned(),
                )
            })?;
        let bytes: [u8; 32] = raw.as_slice().try_into().map_err(|_| {
            crate::Error::Config(format!(
                "security.credential_kek must decode to exactly 32 bytes, got {}",
                raw.len()
            ))
        })?;
        Ok(Self(bytes))
    }

    fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new((&self.0).into())
    }

    /// Encrypt.
    ///
    /// A fresh random nonce per call. XChaCha's 192-bit nonce is what makes
    /// random generation safe here — with a 96-bit nonce, random selection has
    /// a birthday bound close enough to matter for a table that gets rewritten
    /// on every token refresh.
    pub fn seal(&self, plaintext: &[u8]) -> crate::Result<Sealed> {
        let mut nonce_bytes = [0u8; 24];
        OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = XNonce::from_slice(&nonce_bytes);

        let ciphertext = self
            .cipher()
            .encrypt(nonce, plaintext)
            .map_err(|_| crate::Error::Internal("sealing credential failed".to_owned()))?;

        Ok(Sealed {
            ciphertext,
            nonce: nonce_bytes.to_vec(),
        })
    }

    /// Decrypt.
    ///
    /// Fails on any tampering, because the tag is checked. The error message
    /// deliberately says nothing about which part failed.
    pub fn open(&self, sealed: &Sealed) -> crate::Result<Vec<u8>> {
        let nonce_bytes: [u8; 24] = sealed.nonce.as_slice().try_into().map_err(|_| {
            crate::Error::Internal("stored credential nonce is malformed".to_owned())
        })?;
        self.cipher()
            .decrypt(XNonce::from_slice(&nonce_bytes), sealed.ciphertext.as_ref())
            .map_err(|_| {
                crate::Error::Internal(
                    "could not open sealed credential: wrong key, or the row was tampered with"
                        .to_owned(),
                )
            })
    }

    /// Seal a serialisable value as JSON.
    pub fn seal_json<T: serde::Serialize>(&self, value: &T) -> crate::Result<Sealed> {
        let mut json = serde_json::to_vec(value)?;
        let out = self.seal(&json);
        // The plaintext JSON held the secret; do not leave it in a freed page.
        json.zeroize();
        out
    }

    /// Open and deserialise.
    pub fn open_json<T: serde::de::DeserializeOwned>(&self, sealed: &Sealed) -> crate::Result<T> {
        let mut plain = self.open(sealed)?;
        let value = serde_json::from_slice(&plain);
        plain.zeroize();
        Ok(value?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential::SecretMaterial;

    fn kek() -> Kek {
        Kek::from_base64("MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=").expect("valid kek")
    }

    #[test]
    fn round_trips() {
        let k = kek();
        let sealed = k.seal(b"FAKE-CREDENTIAL-FOR-TESTS").expect("seals");
        assert_eq!(
            k.open(&sealed).expect("opens"),
            b"FAKE-CREDENTIAL-FOR-TESTS"
        );
    }

    #[test]
    fn ciphertext_does_not_contain_the_plaintext() {
        let sealed = kek().seal(b"FAKE-CREDENTIAL-FOR-TESTS").expect("seals");
        let haystack = String::from_utf8_lossy(&sealed.ciphertext);
        assert!(!haystack.contains("FAKE-CREDENTIAL"));
    }

    #[test]
    fn the_same_plaintext_seals_differently_every_time() {
        // A deterministic nonce would let anyone with read access to the table
        // tell which accounts share a credential.
        let k = kek();
        let a = k.seal(b"same").expect("seals");
        let b = k.seal(b"same").expect("seals");
        assert_ne!(a.ciphertext, b.ciphertext);
        assert_ne!(a.nonce, b.nonce);
    }

    #[test]
    fn a_tampered_ciphertext_will_not_open() {
        let k = kek();
        let mut sealed = k.seal(b"secret").expect("seals");
        sealed.ciphertext[0] ^= 0xff;
        assert!(k.open(&sealed).is_err());
    }

    #[test]
    fn a_tampered_nonce_will_not_open() {
        let k = kek();
        let mut sealed = k.seal(b"secret").expect("seals");
        sealed.nonce[0] ^= 0xff;
        assert!(k.open(&sealed).is_err());
    }

    #[test]
    fn the_wrong_key_will_not_open() {
        let sealed = kek().seal(b"secret").expect("seals");
        let other =
            Kek::from_base64("ZmZmZmZmZmZmZmZmZmZmZmZmZmZmZmZmZmZmZmZmZmY=").expect("valid kek");
        assert!(other.open(&sealed).is_err());
    }

    #[test]
    fn a_short_key_is_rejected_at_load() {
        // Better to refuse at boot than to run with a key that is not 32 bytes.
        assert!(Kek::from_base64("c2hvcnQ=").is_err());
        assert!(Kek::from_base64("not base64 at all !!!").is_err());
    }

    #[test]
    fn credential_material_round_trips_as_json() {
        let k = kek();
        let cred = SecretMaterial {
            access_token: "FAKE-CREDENTIAL-FOR-TESTS".to_owned(),
            refresh_token: Some("refresh-abc".to_owned()),
            expires_at: Some(1_800_000_000),
            version: 7,
            client_id: None,
            account_id: None,
        };
        let sealed = k.seal_json(&cred).expect("seals");
        let back: SecretMaterial = k.open_json(&sealed).expect("opens");
        assert_eq!(back.access_token, cred.access_token);
        assert_eq!(back.refresh_token, cred.refresh_token);
        assert_eq!(back.version, 7);
    }

    #[test]
    fn debug_never_prints_the_key() {
        assert_eq!(format!("{:?}", kek()), "Kek(<redacted>)");
    }
}
