//! Byte/string helpers (port of `pymt5.helpers`).

use sha1::{Digest, Sha1};

pub fn hex_to_bytes(value: &str) -> crate::Result<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return Err(crate::Error::validation("odd-length hex string"));
    }
    (0..value.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(value.get(i..i + 2).unwrap_or("zz"), 16).map_err(|_| crate::Error::validation("invalid hex")))
        .collect()
}

pub fn bytes_to_hex(value: &[u8]) -> String {
    value.iter().map(|b| format!("{b:02x}")).collect()
}

/// Decode the obfuscated initial key string.
pub fn obfuscation_decode(value: &str) -> String {
    value
        .chars()
        .map(|c| match c as u32 {
            28 => '&',
            23 => '!',
            code => char::from_u32(code.wrapping_sub(1)).unwrap_or('\u{FFFD}'),
        })
        .collect()
}

pub fn random_command_prefix() -> [u8; 2] {
    rand::random()
}

/// latin-1 encode, dropping unencodable chars, truncated to `limit` bytes.
pub fn truncate_ascii(value: &str, limit: usize) -> Vec<u8> {
    value.chars().filter(|c| (*c as u32) < 256).map(|c| c as u32 as u8).take(limit).collect()
}

pub fn pad_ascii(value: &str, size: usize) -> Vec<u8> {
    let mut raw = truncate_ascii(value, size);
    raw.resize(size, 0);
    raw
}

/// Encode as UTF-16LE into a fixed-size zero padded buffer.
pub fn encode_utf16le(value: &str, size: usize) -> Vec<u8> {
    let mut enc: Vec<u8> = value.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    enc.truncate(size);
    enc.resize(size, 0);
    enc
}

/// Decode a UTF-16LE buffer, stopping at the first NUL code unit.
pub fn decode_utf16le(data: &[u8]) -> String {
    let units: Vec<u16> = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .take_while(|u| *u != 0)
        .collect();
    char::decode_utf16(units).map(|r| r.unwrap_or('\u{FFFD}')).collect()
}

pub fn pad_bytes(value: &[u8], size: usize) -> Vec<u8> {
    let mut v: Vec<u8> = value.iter().copied().take(size).collect();
    v.resize(size, 0);
    v
}

pub fn strip_fixed_string(value: &[u8]) -> String {
    let end = value.iter().position(|b| *b == 0).unwrap_or(value.len());
    value[..end].iter().map(|b| *b as char).collect()
}

/// 16-byte client id: first 16 bytes of SHA-1 over the joined parameters.
pub fn build_client_id(platform: &str, device_pixel_ratio: &str, language: &str, screen: &str, uniq: Option<&str>) -> [u8; 16] {
    let uniq_value = match uniq {
        Some(u) if !u.is_empty() => u.to_string(),
        _ => {
            let r: [u8; 3] = rand::random();
            (((r[0] as u32) << 16) | ((r[1] as u32) << 8) | r[2] as u32).to_string()
        }
    };
    let source = [platform, device_pixel_ratio, language, screen, &uniq_value].join(";");
    let digest = Sha1::digest(source.as_bytes());
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        assert_eq!(hex_to_bytes("00ff10").unwrap(), vec![0, 255, 16]);
        assert_eq!(bytes_to_hex(&[0, 255, 16]), "00ff10");
        assert!(hex_to_bytes("abc").is_err());
    }
    #[test]
    fn utf16_roundtrip() {
        let e = encode_utf16le("NyCh-i4r", 64);
        assert_eq!(e.len(), 64);
        assert_eq!(&e[..4], b"N\0y\0");
        assert_eq!(decode_utf16le(&e), "NyCh-i4r");
        assert_eq!(decode_utf16le(&encode_utf16le("สวัสดี", 64)), "สวัสดี");
        assert_eq!(encode_utf16le("", 8), vec![0; 8]);
        assert_eq!(encode_utf16le("ABCDEF", 4), b"A\0B\0".to_vec());
    }
    #[test]
    fn ascii_helpers() {
        assert_eq!(pad_ascii("ab", 4), b"ab\0\0");
        assert_eq!(truncate_ascii("abcdef", 3), b"abc");
        assert_eq!(strip_fixed_string(b"abc\0def"), "abc");
        assert_eq!(pad_bytes(b"abcdef", 3), b"abc");
    }
    #[test]
    fn client_id() {
        let a = build_client_id("p", "1", "en", "0x0", Some("u"));
        assert_eq!(a, build_client_id("p", "1", "en", "0x0", Some("u")));
        assert_ne!(a, build_client_id("q", "1", "en", "0x0", Some("u")));
    }
}
