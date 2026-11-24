//! Per-chunk authenticated encryption.
//!
//! Master key (user's password-derived or account key) never touches the
//! storage backend. Each chunk gets a dedicated AES-256-GCM key derived
//! via HKDF-SHA256 from (master key, chunk fingerprint). Compromising one
//! chunk key reveals exactly that chunk and nothing else. Nonces are
//! random 96-bit per encryption, prepended to ciphertext together with
//! the chunk fingerprint for integrity binding.

use crate::error::{LumenError, Result};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;

pub const CHUNK_KEY_INFO: &[u8] = b"lumen-chunk-v1";
pub const NONCE_LEN: usize = 12;
pub const TAG_LEN: usize = 16;

pub struct ChunkCrypto {
    master: [u8; 32],
}

impl ChunkCrypto {
    pub fn new(master: [u8; 32]) -> Self {
        Self { master }
    }

    pub fn from_password(password: &str, salt: &[u8]) -> Self {
        let mut master = [0u8; 32];
        // HKDF-expand style derivation; replaces argon2 in core to keep
        // this crate pure. Password hashing lives in lumen-api-server.
        let hk = Hkdf::<Sha256>::new(Some(salt), password.as_bytes());
        hk.expand(CHUNK_KEY_INFO, &mut master)
            .expect("hkdf output length is always valid");
        Self { master }
    }

    fn chunk_key(&self, fingerprint: &[u8; 16]) -> [u8; 32] {
        let mut key = [0u8; 32];
        let hk = Hkdf::<Sha256>::new(None, &self.master);
        hk.expand(&[CHUNK_KEY_INFO, fingerprint].concat(), &mut key)
            .expect("hkdf output length is always valid");
        key
    }

    /// Encrypt plaintext with a per-chunk key bound to its fingerprint.
    /// Wire format: [nonce(12)][ciphertext+tag], fingerprint is implicit via
    /// the storage key so we do not duplicate it here.
    pub fn encrypt_chunk(&self, plaintext: &[u8], fingerprint: &[u8; 16]) -> Result<Vec<u8>> {
        use aes_gcm::aead::{Aead, Payload};
        let key = self.chunk_key(fingerprint);
        let cipher = Aes256Gcm::new_from_slice(&key)
            .map_err(|e| LumenError::Crypto(e.to_string()))?;
        let nonce_bytes: [u8; NONCE_LEN] = rand::random();
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ct = cipher
            .encrypt(nonce, Payload { msg: plaintext, aad: fingerprint })
            .map_err(|e| LumenError::Crypto(e.to_string()))?;
        let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// Decrypt chunk data, verifying both the GCM tag and AAD fingerprint.
    pub fn decrypt_chunk(&self, blob: &[u8], fingerprint: &[u8; 16]) -> Result<Vec<u8>> {
        use aes_gcm::aead::{Aead, Payload};
        if blob.len() < NONCE_LEN + TAG_LEN {
            return Err(LumenError::Integrity(hex::encode(fingerprint)));
        }
        let key = self.chunk_key(fingerprint);
        let cipher = Aes256Gcm::new_from_slice(&key)
            .map_err(|e| LumenError::Crypto(e.to_string()))?;
        let (nonce_bytes, ct) = blob.split_at(NONCE_LEN);
        let nonce = Nonce::from_slice(nonce_bytes);
        let plain = cipher
            .decrypt(nonce, Payload { msg: ct, aad: fingerprint })
            .map_err(|_| LumenError::Integrity(hex::encode(fingerprint)))?;
        Ok(plain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let crypto = ChunkCrypto::from_password("pw", b"salt-123");
        let fp = [7u8; 16];
        let pt = b"hello lumen chunk";
        let ct = crypto.encrypt_chunk(pt, &fp).unwrap();
        assert_ne!(ct[NONCE_LEN..], pt[..]);
        let back = crypto.decrypt_chunk(&ct, &fp).unwrap();
        assert_eq!(back, pt);
    }

    #[test]
    fn wrong_fingerprint_fails() {
        let crypto = ChunkCrypto::from_password("pw", b"salt-123");
        let fp = [7u8; 16];
        let other = [8u8; 16];
        let ct = crypto.encrypt_chunk(b"data", &fp).unwrap();
        assert!(crypto.decrypt_chunk(&ct, &other).is_err());
    }

    #[test]
    fn nonces_are_unique() {
        let crypto = ChunkCrypto::from_password("pw", b"salt-123");
        let fp = [1u8; 16];
        let a = crypto.encrypt_chunk(b"same", &fp).unwrap();
        let b_ = crypto.encrypt_chunk(b"same", &fp).unwrap();
        assert_ne!(a[..NONCE_LEN], b_[..NONCE_LEN]);
    }
}
