//! Base64 für Bilddaten: Anbieter-APIs und MCP übertragen Bilder so.

use crate::message::Bytes;

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

impl Bytes {
    /// Standard-Base64 mit Auffüllung.
    pub fn to_base64(&self) -> String {
        let mut out = String::with_capacity(self.0.len().div_ceil(3) * 4);
        for chunk in self.0.chunks(3) {
            let block = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let bits = u32::from(block[0]) << 16 | u32::from(block[1]) << 8 | u32::from(block[2]);
            for position in 0..4 {
                if position <= chunk.len() {
                    out.push(char::from(
                        ALPHABET[(bits >> (18 - 6 * position) & 63) as usize],
                    ));
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    /// Liest Standard-Base64; Leerraum wird übersprungen. `None` bei ungültigen Daten.
    pub fn from_base64(text: &str) -> Option<Self> {
        let mut out = Vec::with_capacity(text.len() / 4 * 3);
        let (mut bits, mut count) = (0u32, 0u8);
        let symbols = text.bytes().filter(|byte| !byte.is_ascii_whitespace());
        for byte in symbols.take_while(|byte| *byte != b'=') {
            let value = ALPHABET.iter().position(|symbol| *symbol == byte)?;
            bits = bits << 6 | value as u32;
            count += 1;
            if count == 4 {
                out.extend_from_slice(&bits.to_be_bytes()[1..]);
                (bits, count) = (0, 0);
            }
        }
        match count {
            0 => {}
            2 => out.push((bits >> 4) as u8),
            3 => out.extend_from_slice(&((bits >> 2) as u16).to_be_bytes()),
            _ => return None,
        }
        Some(Self(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors_roundtrip() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(Bytes(plain.as_bytes().to_vec()).to_base64(), encoded);
            assert_eq!(
                Bytes::from_base64(encoded),
                Some(Bytes(plain.as_bytes().to_vec()))
            );
        }
    }

    #[test]
    fn all_byte_values_roundtrip() {
        let bytes = Bytes((0..=255).collect());
        assert_eq!(Bytes::from_base64(&bytes.to_base64()), Some(bytes));
    }

    #[test]
    fn whitespace_is_skipped_and_garbage_rejected() {
        assert_eq!(
            Bytes::from_base64("Zm9v\nYmFy "),
            Some(Bytes(b"foobar".to_vec()))
        );
        assert_eq!(Bytes::from_base64("Zm9*"), None);
        assert_eq!(Bytes::from_base64("Z"), None);
    }
}
