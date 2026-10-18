//! Device identity and end-to-end payload encryption.
//!
//! Each device owns two keypairs:
//!
//! - an **Ed25519** signing keypair used to authenticate the device to the
//!   control plane (pairing challenges), and
//! - an **X25519** static keypair used for end-to-end encryption of sync
//!   payloads.
//!
//! E2E encryption is static-static ECDH (the two devices' static secrets)
//! followed by HKDF-SHA256 and ChaCha20-Poly1305. The server or relay only
//! ever sees opaque ciphertext (SRD §11).

use std::path::Path;

use chacha20poly1305::aead::{Aead, Generate, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use hkdf::Hkdf;
use sha2::Sha256;
use x25519_dalek::{PublicKey as XPublicKey, StaticSecret};

/// Length of a ChaCha20-Poly1305 nonce.
const NONCE_LEN: usize = 12;
const HKDF_INFO: &[u8] = b"syncengine-e2e-v1";

/// Errors from identity and encryption operations.
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid key or signature encoding: {0}")]
    Encoding(String),
    #[error("signature verification failed")]
    BadSignature,
    #[error("decryption failed (wrong key, tampering, or corruption)")]
    Decrypt,
    #[error("identity file is malformed: {0}")]
    Malformed(String),
}

/// A device's long-term cryptographic identity.
pub struct Identity {
    signing: SigningKey,
    encryption: StaticSecret,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct IdentityFile {
    version: u32,
    ed25519_secret: String,
    x25519_secret: String,
}

impl Identity {
    /// Generates a fresh identity from the operating-system RNG.
    pub fn generate() -> Self {
        let mut rng = rand::rng();
        Self {
            signing: SigningKey::generate(&mut rng),
            encryption: StaticSecret::random_from_rng(&mut rng),
        }
    }

    /// Rebuilds an identity from raw secret bytes.
    pub fn from_secret_bytes(ed25519: [u8; 32], x25519: [u8; 32]) -> Self {
        Self {
            signing: SigningKey::from_bytes(&ed25519),
            encryption: StaticSecret::from(x25519),
        }
    }

    /// The raw secret bytes `(ed25519, x25519)`.
    pub fn secret_bytes(&self) -> ([u8; 32], [u8; 32]) {
        (self.signing.to_bytes(), self.encryption.to_bytes())
    }

    /// Loads an identity from `path`, creating and persisting a new one when
    /// the file does not exist.
    pub fn load_or_create(path: &Path) -> Result<Self, CryptoError> {
        if path.exists() {
            Self::load(path)
        } else {
            let identity = Self::generate();
            identity.save(path)?;
            Ok(identity)
        }
    }

    /// Reads an identity from a JSON file.
    pub fn load(path: &Path) -> Result<Self, CryptoError> {
        let text = std::fs::read_to_string(path)?;
        let file: IdentityFile =
            serde_json::from_str(&text).map_err(|e| CryptoError::Malformed(e.to_string()))?;
        if file.version != 1 {
            return Err(CryptoError::Malformed(format!(
                "unsupported identity version {}",
                file.version
            )));
        }
        let ed = decode_32(&file.ed25519_secret)?;
        let x = decode_32(&file.x25519_secret)?;
        Ok(Self::from_secret_bytes(ed, x))
    }

    /// Writes the identity to `path`, restricting permissions where possible.
    pub fn save(&self, path: &Path) -> Result<(), CryptoError> {
        let (ed, x) = self.secret_bytes();
        let file = IdentityFile {
            version: 1,
            ed25519_secret: hex::encode(ed),
            x25519_secret: hex::encode(x),
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(&file)
            .map_err(|e| CryptoError::Malformed(e.to_string()))?;
        std::fs::write(path, text)?;
        restrict_permissions(path)?;
        Ok(())
    }

    /// Ed25519 public key, hex-encoded.
    pub fn ed25519_public_hex(&self) -> String {
        hex::encode(self.signing.verifying_key().to_bytes())
    }

    /// X25519 public key, hex-encoded.
    pub fn x25519_public_hex(&self) -> String {
        hex::encode(XPublicKey::from(&self.encryption).to_bytes())
    }

    /// Signs `message`, returning a hex-encoded signature.
    pub fn sign_hex(&self, message: &[u8]) -> String {
        hex::encode(self.signing.sign(message).to_bytes())
    }

    /// Verifies a hex-encoded signature over `message` for `public_key_hex`.
    pub fn verify_hex(
        public_key_hex: &str,
        message: &[u8],
        signature_hex: &str,
    ) -> Result<(), CryptoError> {
        let key_bytes = decode_32(public_key_hex)?;
        let verifying = VerifyingKey::from_bytes(&key_bytes)
            .map_err(|e| CryptoError::Encoding(e.to_string()))?;
        let sig_bytes =
            hex::decode(signature_hex).map_err(|e| CryptoError::Encoding(e.to_string()))?;
        let signature = ed25519_dalek::Signature::from_slice(&sig_bytes)
            .map_err(|e| CryptoError::Encoding(e.to_string()))?;
        verifying
            .verify(message, &signature)
            .map_err(|_| CryptoError::BadSignature)
    }

    /// Encrypts `plaintext` so only the holder of `peer_x25519_hex` can read
    /// it. Output is `nonce || ciphertext`.
    pub fn seal(&self, peer_x25519_hex: &str, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let key = self.shared_key(peer_x25519_hex)?;
        let cipher = ChaCha20Poly1305::new_from_slice(&key)
            .map_err(|_| CryptoError::Encoding("bad key length".into()))?;
        let nonce = Nonce::generate();
        let ciphertext = cipher
            .encrypt(&nonce, plaintext)
            .map_err(|_| CryptoError::Decrypt)?;
        let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        out.extend_from_slice(nonce.as_slice());
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    /// Decrypts a payload produced by [`Identity::seal`] toward this device.
    pub fn open(&self, peer_x25519_hex: &str, sealed: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if sealed.len() < NONCE_LEN {
            return Err(CryptoError::Decrypt);
        }
        let (nonce_bytes, ciphertext) = sealed.split_at(NONCE_LEN);
        let nonce = Nonce::try_from(nonce_bytes)
            .map_err(|_| CryptoError::Encoding("bad nonce length".into()))?;
        let key = self.shared_key(peer_x25519_hex)?;
        let cipher = ChaCha20Poly1305::new_from_slice(&key)
            .map_err(|_| CryptoError::Encoding("bad key length".into()))?;
        cipher
            .decrypt(&nonce, ciphertext)
            .map_err(|_| CryptoError::Decrypt)
    }

    fn shared_key(&self, peer_x25519_hex: &str) -> Result<[u8; 32], CryptoError> {
        let peer_bytes = decode_32(peer_x25519_hex)?;
        let peer_public = XPublicKey::from(peer_bytes);
        let shared = self.encryption.diffie_hellman(&peer_public);
        let hkdf = Hkdf::<Sha256>::new(None, shared.as_bytes());
        let mut key = [0u8; 32];
        hkdf.expand(HKDF_INFO, &mut key)
            .map_err(|_| CryptoError::Encoding("hkdf expand failed".into()))?;
        Ok(key)
    }
}

fn decode_32(hex_str: &str) -> Result<[u8; 32], CryptoError> {
    let bytes = hex::decode(hex_str).map_err(|e| CryptoError::Encoding(e.to_string()))?;
    if bytes.len() != 32 {
        return Err(CryptoError::Encoding(format!(
            "expected 32 bytes, got {}",
            bytes.len()
        )));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<(), CryptoError> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> Result<(), CryptoError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_verify_and_reject_tampering() {
        let id = Identity::generate();
        let msg = b"pairing-challenge-42";
        let sig = id.sign_hex(msg);
        assert!(Identity::verify_hex(&id.ed25519_public_hex(), msg, &sig).is_ok());
        assert!(matches!(
            Identity::verify_hex(&id.ed25519_public_hex(), b"other", &sig),
            Err(CryptoError::BadSignature)
        ));
        let other = Identity::generate();
        assert!(Identity::verify_hex(&other.ed25519_public_hex(), msg, &sig).is_err());
    }

    #[test]
    fn sealed_payload_round_trips_between_two_devices() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let plaintext = b"file contents that must stay private";
        let sealed = alice.seal(&bob.x25519_public_hex(), plaintext).unwrap();
        assert_ne!(&sealed[..], &plaintext[..]);

        let opened = bob.open(&alice.x25519_public_hex(), &sealed).unwrap();
        assert_eq!(opened, plaintext);
    }

    #[test]
    fn a_third_device_cannot_open_the_payload() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let eve = Identity::generate();
        let sealed = alice.seal(&bob.x25519_public_hex(), b"secret").unwrap();
        assert!(matches!(
            eve.open(&alice.x25519_public_hex(), &sealed),
            Err(CryptoError::Decrypt)
        ));
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let mut sealed = alice.seal(&bob.x25519_public_hex(), b"secret").unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0xff;
        assert!(matches!(
            bob.open(&alice.x25519_public_hex(), &sealed),
            Err(CryptoError::Decrypt)
        ));
    }

    #[test]
    fn identity_persists_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.json");
        let created = Identity::load_or_create(&path).unwrap();
        let loaded = Identity::load_or_create(&path).unwrap();
        assert_eq!(created.ed25519_public_hex(), loaded.ed25519_public_hex());
        assert_eq!(created.x25519_public_hex(), loaded.x25519_public_hex());
    }

    #[test]
    fn rejects_short_key_material() {
        assert!(decode_32("abcd").is_err());
        assert!(decode_32("zz".repeat(32).as_str()).is_err());
    }
}
