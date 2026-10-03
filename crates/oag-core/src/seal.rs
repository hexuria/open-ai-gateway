//! Sealing credentials at rest.
//!
//! Upstream credentials are retrievable secrets, not passwords: we have to send
//! the original bytes to the provider, so hashing is not an option and
//! authenticated encryption is. XChaCha20-Poly1305 under a key-encryption key
//! supplied by the environment.
//!
//! Plaintext OAuth access and refresh tokens in JSONB would make a database
//! backup a credential dump and a read-only SQL grant a credential grant.
//! Sealing them is this file.

use base64::Engine as _;
use chacha20poly1305::aead::{Aead, KeyInit};
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
        let nonce_bytes: [u8; 24] = {
            use rand::RngExt;
            rand::rng().random()
        };
        let nonce = XNonce::from(nonce_bytes);

        let ciphertext = self
            .cipher()
            .encrypt(&nonce, plaintext)
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
        let malformed =
            || crate::Error::Internal("stored credential nonce is malformed".to_owned());
        let nonce_bytes: [u8; 24] = sealed
            .nonce
            .as_slice()
            .try_into()
            .map_err(|_| malformed())?;
        let nonce = XNonce::try_from(&nonce_bytes[..]).map_err(|_| malformed())?;
        self.cipher()
            .decrypt(&nonce, sealed.ciphertext.as_ref())
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

    #[test]
    fn the_cipher_wipes_its_copy_of_the_key_on_drop() {
        // `cipher()` copies the KEK into every cipher it builds.
        // chacha20poly1305 0.10 wiped that copy on drop unconditionally; 0.11
        // does only with its `zeroize` feature, which the workspace manifest
        // turns on. Without the feature this does not compile.
        fn wipes_on_drop<T: ZeroizeOnDrop>() {}
        wipes_on_drop::<XChaCha20Poly1305>();
    }

    // The golden vector. Every credential row in Postgres was sealed by an
    // earlier build, and a dependency bump must not strand one. These bytes
    // were produced once, by this file on chacha20poly1305 0.10.1, and are
    // never regenerated: if they stop matching, the stored format changed.

    /// Not a real key: a repeated pattern, 32 bytes.
    const GOLDEN_KEK: [u8; 32] = *b"KEK-KEK-KEK-KEK-KEK-KEK-KEK-KEK-";
    /// Not a random nonce: a repeated pattern, 24 bytes.
    const GOLDEN_NONCE: [u8; 24] = *b"NONCE-NONCE-NONCE-NONCE-";

    const GOLDEN_SHORT: &[u8] = b"FAKE-CREDENTIAL";
    const GOLDEN_SHORT_SEALED: &[u8] = &[
        0x4c, 0xe4, 0xf4, 0xaa, 0xae, 0xf0, 0x58, 0x63, 0xbb, 0xfe, 0xba, 0xc3, 0x09, 0xb8, 0x9a,
        0x71, 0x49, 0x7e, 0x66, 0x90, 0xfc, 0x20, 0x17, 0xee, 0x8a, 0x99, 0x5f, 0xb6, 0x5b, 0x28,
        0x7b,
    ];

    /// A serialised [`SecretMaterial`], every field set, every value fake.
    const GOLDEN_MATERIAL: &str = r#"{"access_token":"FAKE-ACCESS-TOKEN","refresh_token":"FAKE-REFRESH-TOKEN","expires_at":1800000000,"version":3,"client_id":"fake-client-id","account_id":"fake-account-id"}"#;
    const GOLDEN_MATERIAL_SEALED: &[u8] = &[
        0x71, 0x87, 0xde, 0x8c, 0xe0, 0xd6, 0x79, 0x55, 0xa0, 0xcf, 0x9b, 0xfc, 0x25, 0x97, 0xf4,
        0x39, 0xd8, 0xdc, 0x3d, 0x19, 0xe2, 0x91, 0x46, 0xc6, 0xa8, 0xfe, 0x54, 0xb1, 0x30, 0xa8,
        0xea, 0x69, 0xae, 0x7f, 0x06, 0x85, 0xf2, 0x90, 0xf9, 0xbb, 0xf9, 0xd2, 0x93, 0xf8, 0x07,
        0xf9, 0x2d, 0xc7, 0x60, 0xf1, 0x1e, 0xaa, 0x91, 0x43, 0x28, 0xb8, 0x7e, 0xec, 0x05, 0x3f,
        0x3c, 0xf2, 0x7a, 0x8e, 0x94, 0x46, 0x57, 0xb4, 0x3b, 0x9a, 0xa2, 0xd8, 0xb0, 0x5c, 0x0c,
        0x55, 0x42, 0x39, 0x19, 0x53, 0x2f, 0x01, 0x43, 0x0d, 0x27, 0xa0, 0xa4, 0x5b, 0xfb, 0xf1,
        0x6e, 0x88, 0x74, 0xb4, 0xc2, 0xde, 0xfd, 0x89, 0xf6, 0xb0, 0xf1, 0x04, 0x99, 0xa7, 0xcf,
        0x40, 0x5a, 0x19, 0x18, 0xc1, 0x67, 0xe8, 0xc0, 0x9f, 0xdf, 0x23, 0x26, 0x0b, 0x45, 0x2a,
        0x04, 0x88, 0xe7, 0x30, 0x62, 0xa7, 0x79, 0x23, 0x2b, 0xa8, 0xd8, 0x8f, 0x4a, 0x1b, 0xd1,
        0x8a, 0xe6, 0xed, 0x90, 0x0c, 0xce, 0xc3, 0x53, 0x10, 0xfd, 0x86, 0x77, 0x36, 0xf5, 0xee,
        0xc5, 0x8a, 0x42, 0xf3, 0xd7, 0x46, 0xd0, 0x7a, 0x61, 0xef, 0xee, 0x0c, 0x3b, 0x5a, 0x43,
        0xeb, 0x88, 0x7b, 0x3e, 0x45, 0xb7, 0x17, 0x52, 0x3a, 0xb6, 0xc3, 0x05, 0x7c, 0xff, 0xde,
        0x09, 0x66, 0x02, 0x5c, 0xf0,
    ];

    /// What [`Kek::seal`] does, under a nonce the caller picks so the output
    /// is fixed. Test-only: a repeated nonce is what `seal` exists to prevent.
    fn seal_with_nonce(k: &Kek, plaintext: &[u8], nonce: [u8; 24]) -> Sealed {
        let ciphertext = k
            .cipher()
            .encrypt(&XNonce::from(nonce), plaintext)
            .expect("seals");
        Sealed {
            ciphertext,
            nonce: nonce.to_vec(),
        }
    }

    #[test]
    fn rows_sealed_by_an_earlier_build_open_and_reseal_byte_for_byte() {
        let k = Kek(GOLDEN_KEK);
        for (name, plaintext, ciphertext) in [
            ("short", GOLDEN_SHORT, GOLDEN_SHORT_SEALED),
            (
                "material",
                GOLDEN_MATERIAL.as_bytes(),
                GOLDEN_MATERIAL_SEALED,
            ),
        ] {
            let stored = Sealed {
                ciphertext: ciphertext.to_vec(),
                nonce: GOLDEN_NONCE.to_vec(),
            };
            // Reading: a row the earlier build wrote opens to its plaintext.
            assert_eq!(
                k.open(&stored).expect("a stored row opens"),
                plaintext,
                "{name}: the stored row opened to different bytes"
            );
            // Writing: this build seals what the earlier build did.
            assert_eq!(
                seal_with_nonce(&k, plaintext, GOLDEN_NONCE),
                stored,
                "{name}: the same key, nonce and plaintext sealed to different bytes"
            );
        }

        // And through the call production makes to read a credential row.
        let material: SecretMaterial = k
            .open_json(&Sealed {
                ciphertext: GOLDEN_MATERIAL_SEALED.to_vec(),
                nonce: GOLDEN_NONCE.to_vec(),
            })
            .expect("a stored credential opens");
        assert_eq!(material.access_token, "FAKE-ACCESS-TOKEN");
        assert_eq!(
            material.refresh_token.as_deref(),
            Some("FAKE-REFRESH-TOKEN")
        );
        assert_eq!(material.expires_at, Some(1_800_000_000));
        assert_eq!(material.version, 3);
        assert_eq!(material.client_id.as_deref(), Some("fake-client-id"));
        assert_eq!(material.account_id.as_deref(), Some("fake-account-id"));
    }
}
