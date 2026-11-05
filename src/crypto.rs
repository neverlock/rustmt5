//! AES-CBC (zero IV, PKCS7) cipher (port of `pymt5.crypto`).

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};

use crate::constants::INITIAL_KEY_OBFUSCATED;
use crate::error::{Error, Result};
use crate::helpers::{hex_to_bytes, obfuscation_decode};

#[derive(Clone)]
pub struct AesCipher {
    key: Vec<u8>,
}

impl std::fmt::Debug for AesCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AesCipher(<{} byte key>)", self.key.len())
    }
}

impl AesCipher {
    pub fn new(key: &[u8]) -> Result<Self> {
        if !matches!(key.len(), 16 | 24 | 32) {
            return Err(Error::protocol(format!("invalid AES key length: {}", key.len())));
        }
        Ok(Self { key: key.to_vec() })
    }
    pub fn key(&self) -> &[u8] { &self.key }

    pub fn encrypt(&self, data: &[u8]) -> Vec<u8> {
        let iv = [0u8; 16];
        match self.key.len() {
            16 => cbc::Encryptor::<aes::Aes128>::new_from_slices(&self.key, &iv).unwrap().encrypt_padded_vec_mut::<Pkcs7>(data),
            24 => cbc::Encryptor::<aes::Aes192>::new_from_slices(&self.key, &iv).unwrap().encrypt_padded_vec_mut::<Pkcs7>(data),
            _ => cbc::Encryptor::<aes::Aes256>::new_from_slices(&self.key, &iv).unwrap().encrypt_padded_vec_mut::<Pkcs7>(data),
        }
    }

    pub fn decrypt(&self, data: &[u8]) -> Result<Vec<u8>> {
        let iv = [0u8; 16];
        let bad = |_| Error::protocol("AES decrypt failed (bad padding/length)");
        match self.key.len() {
            16 => cbc::Decryptor::<aes::Aes128>::new_from_slices(&self.key, &iv).unwrap().decrypt_padded_vec_mut::<Pkcs7>(data).map_err(bad),
            24 => cbc::Decryptor::<aes::Aes192>::new_from_slices(&self.key, &iv).unwrap().decrypt_padded_vec_mut::<Pkcs7>(data).map_err(bad),
            _ => cbc::Decryptor::<aes::Aes256>::new_from_slices(&self.key, &iv).unwrap().decrypt_padded_vec_mut::<Pkcs7>(data).map_err(bad),
        }
    }
}

pub fn initial_key_bytes() -> Vec<u8> {
    hex_to_bytes(&obfuscation_decode(INITIAL_KEY_OBFUSCATED)).expect("built-in key is valid hex")
}

pub fn initial_cipher() -> AesCipher {
    AesCipher::new(&initial_key_bytes()).expect("built-in key has valid length")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn initial_key_is_32_bytes() { assert_eq!(initial_key_bytes().len(), 32); }
    #[test]
    fn roundtrip_all_key_sizes() {
        for n in [16usize, 24, 32] {
            let c = AesCipher::new(&vec![7u8; n]).unwrap();
            for len in [0usize, 1, 15, 16, 17, 100] {
                let data = vec![0xAB; len];
                let enc = c.encrypt(&data);
                assert_eq!(enc.len() % 16, 0);
                assert_eq!(c.decrypt(&enc).unwrap(), data);
            }
        }
    }
    #[test]
    fn invalid_key_rejected() { assert!(AesCipher::new(&[0u8; 10]).is_err()); }
    #[test]
    fn bootstrap_size_matches_browser_capture() {
        // 2-byte prefix + 2-byte cmd + 64-byte token = 68 -> 80 encrypted -> 88 framed
        let inner = crate::protocol::build_command(0, &[0u8; 64]);
        assert_eq!(inner.len(), 68);
        let enc = initial_cipher().encrypt(&inner);
        assert_eq!(enc.len(), 80);
        assert_eq!(crate::protocol::pack_outer(&enc).len(), 88);
    }
}
