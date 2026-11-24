//! Content-defined chunking (FastCDC) with per-chunk SHA-256 fingerprints.
//!
//! Fixed-size chunking (what Helumi uses, evidenced by ChunkStore.chunkSize)
//! re-uploads the entire tail of a modified file. CDC cuts at content
//! boundaries so only genuinely changed chunks are re-encrypted and
//! re-uploaded. This is Lumen's core bandwidth advantage over Helumi.

use crate::error::Result;
use bytes::Bytes;
use sha2::{Digest, Sha256};

/// Smallest chunk we will ever cut.
pub const DEFAULT_MIN: usize = 256 * 1024;
/// Average chunk size target.
pub const DEFAULT_AVG: usize = 1024 * 1024;
/// Largest chunk we will ever cut.
pub const DEFAULT_MAX: usize = 4 * 1024 * 1024;
/// Key namespace holding encrypted chunk objects. Every content-addressed
/// chunk lands under this prefix; GC lists it and reclaims orphans. Kept as
/// a shared constant so storage-layout changes break compilation, not GC.
pub const CHUNK_KEY_PREFIX: &str = "chunks/";

#[derive(Debug, Clone)]
pub struct Chunk {
    pub index: u64,
    pub offset: u64,
    pub len: u64,
    /// First 16 bytes of SHA-256 over plaintext - used as dedup key.
    pub fingerprint: [u8; 16],
}

impl Chunk {
    /// Content-addressed storage key. Identical plaintext lands on the same
    /// key across every user and device - dedup and resumability for free.
    pub fn storage_key(&self) -> String {
        let fp = self.fingerprint;
        format!(
            "{}{:02x}{:02x}{:02x}{:02x}/{:016x}",
            CHUNK_KEY_PREFIX,
            fp[0],
            fp[1],
            fp[2],
            fp[3],
            u128::from_be_bytes(fp)
        )
    }
}

#[derive(Clone)]
pub struct FastCdc {
    min: usize,
    avg: usize,
    max: usize,
    mask_s: u32,
    mask_l: u32,
}

impl Default for FastCdc {
    fn default() -> Self {
        Self::new(DEFAULT_MIN, DEFAULT_AVG, DEFAULT_MAX)
    }
}

impl FastCdc {
    pub fn new(min: usize, avg: usize, max: usize) -> Self {
        assert!(min > 48 && min < avg && avg < max);
        // Gear-mask sizing per the FastCDC paper: log2 ratios keep the
        // expected chunk size near `avg` while min/max bound the tails.
        let bits_s = 32 - ((min as f64) / (avg as f64)).log2().max(1.0) as u32;
        let bits_l = 32 - ((avg as f64) / (max as f64)).log2().max(1.0) as u32 + 1;
        Self {
            min,
            avg,
            max,
            mask_s: (1u32 << bits_s.saturating_sub(1).clamp(1, 31)) - 1,
            mask_l: (1u32 << bits_l.saturating_sub(1).clamp(1, 31)) - 1,
        }
    }

    /// Split a buffer into content-defined chunks.
    pub fn split(&self, data: Bytes) -> Result<Vec<Chunk>> {
        let mut chunks = Vec::new();
        if data.is_empty() {
            return Ok(chunks);
        }
        let mut off: u64 = 0;
        while off < data.len() as u64 {
            let start = off as usize;
            let end = self.cut_at(&data[start..]).saturating_add(start);
            let plain = &data[start..end];
            let mut hasher = Sha256::new();
            hasher.update(plain);
            let mut fp = [0u8; 16];
            fp.copy_from_slice(&hasher.finalize()[..16]);
            chunks.push(Chunk {
                index: chunks.len() as u64,
                offset: off,
                len: plain.len() as u64,
                fingerprint: fp,
            });
            off = end as u64;
        }
        Ok(chunks)
    }

    /// Find the next CDC boundary in `data`. Returns absolute index of end-of-chunk.
    fn cut_at(&self, data: &[u8]) -> usize {
        let n = data.len();
        if n <= self.min {
            return n;
        }
        // Two-stage gear hash: short window uses tight mask, long uses loose.
        let mut h: u64 = 0;
        for &b in &data[..self.min] {
            h = h.wrapping_mul(31).wrapping_add(b as u64);
        }
        for (i, &b) in data[self.min..n.min(self.avg)].iter().enumerate() {
            h = h.wrapping_mul(31).wrapping_add(b as u64);
            if (h as u32) & self.mask_s == 0 {
                return self.min + i + 1;
            }
        }
        for (i, &b) in data[self.avg.min(n)..n.min(self.max)].iter().enumerate() {
            h = h.wrapping_mul(31).wrapping_add(b as u64);
            if (h as u32) & self.mask_l == 0 {
                return self.avg.min(n) + i + 1;
            }
        }
        n.min(self.max)
    }
}
