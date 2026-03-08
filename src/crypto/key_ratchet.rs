//! HKDF-based per-sender key ratchet with generation cache.
//!
//! Each sender in a DAVE session has a key ratchet initialized with a
//! base secret exported from the MLS group. The ratchet derives a unique
//! AES-128 key for each generation using HKDF-Expand.

use std::collections::HashMap;

use hkdf::Hkdf;
use sha2::Sha256;

use crate::error::SigilError;
use crate::types::KEY_LENGTH;

/// Per-sender key ratchet for DAVE frame encryption.
///
/// Generation 0 uses the base secret directly. Subsequent generations
/// are derived via `HKDF-Expand(previous_key, "sigil-ratchet-{gen}", 16)`.
///
/// Old generations can be erased once they are no longer needed, and
/// attempting to retrieve an erased generation returns an error.
pub struct KeyRatchet {
    /// The original base secret (generation 0 key).
    base_secret: [u8; KEY_LENGTH],
    /// The highest generation that has been derived so far.
    current_generation: u32,
    /// Cached keys by generation number.
    cache: HashMap<u32, [u8; KEY_LENGTH]>,
}

impl KeyRatchet {
    /// Create a new key ratchet with the given base secret.
    ///
    /// Generation 0 is immediately available and equals the base secret.
    pub fn new(base_secret: [u8; KEY_LENGTH]) -> Self {
        let mut cache = HashMap::new();
        cache.insert(0, base_secret);
        Self {
            base_secret,
            current_generation: 0,
            cache,
        }
    }

    /// Retrieve the key for a given generation, ratcheting forward as needed.
    ///
    /// # Errors
    ///
    /// Returns [`SigilError::GenerationErased`] if the requested generation
    /// has been erased via [`erase_before`](Self::erase_before).
    pub fn get(&mut self, generation: u32) -> Result<[u8; KEY_LENGTH], SigilError> {
        // If already cached, return it
        if let Some(&key) = self.cache.get(&generation) {
            return Ok(key);
        }

        // If the generation is below anything we still have, it was erased
        let min_cached = self.cache.keys().copied().min().unwrap_or(0);
        if generation < min_cached {
            return Err(SigilError::GenerationErased {
                generation,
                current: self.current_generation,
            });
        }

        // Ratchet forward from current_generation to the requested generation
        let mut g = self.current_generation;
        while g < generation {
            let prev_key = self
                .cache
                .get(&g)
                .copied()
                .ok_or(SigilError::GenerationErased {
                    generation: g,
                    current: self.current_generation,
                })?;

            g += 1;
            let next_key = Self::derive_next(&prev_key, g)?;
            self.cache.insert(g, next_key);
        }

        self.current_generation = generation;
        self.cache
            .get(&generation)
            .copied()
            .ok_or(SigilError::GenerationErased {
                generation,
                current: self.current_generation,
            })
    }

    /// Erase all cached keys for generations strictly less than `min_generation`.
    ///
    /// Once erased, those generations can no longer be retrieved.
    pub fn erase_before(&mut self, min_generation: u32) {
        self.cache.retain(|&g, _| g >= min_generation);
    }

    /// Returns the highest generation that has been derived.
    pub fn current_generation(&self) -> u32 {
        self.current_generation
    }

    /// Returns a reference to the original base secret (generation 0 key).
    pub fn base_secret(&self) -> &[u8; KEY_LENGTH] {
        &self.base_secret
    }

    /// Reset the ratchet back to generation 0, clearing all cached keys
    /// and restoring the base secret as the only cached key.
    pub fn reset(&mut self) {
        self.cache.clear();
        self.cache.insert(0, self.base_secret);
        self.current_generation = 0;
    }

    /// Derive the next generation key via HKDF.
    ///
    /// Uses the previous key as the IKM and `"sigil-ratchet-{gen}"` as the info.
    fn derive_next(
        prev_key: &[u8; KEY_LENGTH],
        generation: u32,
    ) -> Result<[u8; KEY_LENGTH], SigilError> {
        let info = format!("sigil-ratchet-{}", generation);
        // Use Hkdf::new (Extract + Expand) rather than from_prk, because
        // from_prk requires the PRK to be at least hash-length (32 bytes)
        // but our AES-128 key is only 16 bytes.
        let hk = Hkdf::<Sha256>::new(None, prev_key);

        let mut okm = [0u8; KEY_LENGTH];
        hk.expand(info.as_bytes(), &mut okm)
            .map_err(|e| SigilError::Mls(format!("HKDF-Expand error: {}", e)))?;

        Ok(okm)
    }
}
