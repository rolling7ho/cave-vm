//! Binary data in messages, carried as standard base64 with padding.

use std::fmt;

use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize, Serializer};

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Bytes that travel as a base64 string in a `data` field.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Data(pub Vec<u8>);

impl fmt::Debug for Data {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Data({} bytes)", self.0.len())
    }
}

impl From<Vec<u8>> for Data {
    fn from(bytes: Vec<u8>) -> Data {
        Data(bytes)
    }
}

impl From<&[u8]> for Data {
    fn from(bytes: &[u8]) -> Data {
        Data(bytes.to_vec())
    }
}

impl Serialize for Data {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for Data {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Data, D::Error> {
        struct Base64;
        impl Visitor<'_> for Base64 {
            type Value = Data;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a base64 string")
            }
            fn visit_str<E: de::Error>(self, s: &str) -> Result<Data, E> {
                decode(s)
                    .map(Data)
                    .ok_or_else(|| E::custom("invalid base64"))
            }
        }
        deserializer.deserialize_str(Base64)
    }
}

pub(crate) fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

pub(crate) fn decode(s: &str) -> Option<Vec<u8>> {
    let s = s.as_bytes();
    if !s.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let last = s.len() / 4;
    for (index, quad) in s.chunks(4).enumerate() {
        let pad = quad.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && index + 1 != last) {
            return None;
        }
        let mut n = 0u32;
        for &c in &quad[..4 - pad] {
            n = n << 6 | u32::from(value(c)?);
        }
        n <<= 6 * pad as u32;
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        // Bits that padding drops must be zero, so each string has one
        // decoding.
        if bytes[3 - pad..].iter().any(|&b| b != 0) {
            return None;
        }
        out.extend_from_slice(&bytes[..3 - pad]);
    }
    Some(out)
}

fn value(c: u8) -> Option<u8> {
    Some(match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_4648_vectors() {
        let vectors = [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ];
        for (plain, encoded) in vectors {
            assert_eq!(encode(plain.as_bytes()), encoded);
            assert_eq!(decode(encoded).unwrap(), plain.as_bytes());
        }
    }

    #[test]
    fn every_byte_round_trips() {
        let bytes: Vec<u8> = (0..=255).cycle().take(1000).collect();
        assert_eq!(decode(&encode(&bytes)).unwrap(), bytes);
    }

    #[test]
    fn malformed_strings_are_refused() {
        for bad in [
            "Zg=", "Zg", "Z===", "Zg==Zg==", "Zh==", "Zm9v!A==", "Zm9v\n",
        ] {
            assert_eq!(decode(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn serializes_as_a_string() {
        let json = serde_json::to_string(&Data(b"hello\n".to_vec())).unwrap();
        assert_eq!(json, "\"aGVsbG8K\"");
        let back: Data = serde_json::from_str(&json).unwrap();
        assert_eq!(back.0, b"hello\n");
        assert!(serde_json::from_str::<Data>("\"%%%%\"").is_err());
    }
}
