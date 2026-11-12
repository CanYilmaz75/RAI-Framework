//! Versiegelung gespeicherter Daten: Verschlüsselung im Ruhezustand und eine Hash-Kette, an
//! der sich nachträgliche Änderungen am Log erkennen lassen.

use can_core::{Bytes, Digest, StoreError};
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};

const PREFIX: &str = "enc1:";
const NONCE_BYTES: usize = 12;

/// Schlüssel für die Verschlüsselung im Ruhezustand (ChaCha20-Poly1305, 256 Bit).
#[derive(Clone)]
pub struct Cipher {
    cipher: ChaCha20Poly1305,
}

fn corrupt(message: &str) -> StoreError {
    StoreError::Corrupt(message.to_owned())
}

impl Cipher {
    /// Schlüssel aus 64 Hex-Zeichen, etwa aus `openssl rand -hex 32`.
    pub fn from_hex(key: &str) -> Result<Self, StoreError> {
        let key = key.trim();
        let invalid = || corrupt("der Schlüssel muss aus 64 Hex-Zeichen bestehen");
        if key.len() != 64 || !key.is_ascii() {
            return Err(invalid());
        }
        let bytes: Result<Vec<u8>, _> = (0..64)
            .step_by(2)
            .map(|i| u8::from_str_radix(&key[i..i + 2], 16))
            .collect();
        let bytes = bytes.map_err(|_| invalid())?;
        let cipher = ChaCha20Poly1305::new_from_slice(&bytes).map_err(|_| invalid())?;
        Ok(Self { cipher })
    }

    fn encrypt(&self, plain: &[u8]) -> Result<String, StoreError> {
        let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
        let sealed = self
            .cipher
            .encrypt(&nonce, plain)
            .map_err(|_| corrupt("Verschlüsselung fehlgeschlagen"))?;
        let mut bytes = nonce.to_vec();
        bytes.extend(sealed);
        Ok(format!("{PREFIX}{}", Bytes(bytes).to_base64()))
    }

    fn decrypt(&self, stored: &str) -> Result<Vec<u8>, StoreError> {
        let broken =
            || corrupt("verschlüsselte Daten sind beschädigt oder der Schlüssel ist falsch");
        let bytes = Bytes::from_base64(stored).ok_or_else(broken)?.0;
        if bytes.len() < NONCE_BYTES {
            return Err(broken());
        }
        let (nonce, sealed) = bytes.split_at(NONCE_BYTES);
        self.cipher
            .decrypt(Nonce::from_slice(nonce), sealed)
            .map_err(|_| broken())
    }
}

/// Versiegelt und öffnet, was ein Store ablegt. Ohne Schlüssel bleibt der Inhalt Klartext.
#[derive(Clone, Default)]
pub struct Sealer {
    cipher: Option<Cipher>,
}

impl Sealer {
    /// Versiegelung mit Verschlüsselung.
    pub fn encrypted(cipher: Cipher) -> Self {
        Self {
            cipher: Some(cipher),
        }
    }

    /// Text, wie er gespeichert wird.
    pub fn seal(&self, plain: &str) -> Result<String, StoreError> {
        match &self.cipher {
            Some(cipher) => cipher.encrypt(plain.as_bytes()),
            None => Ok(plain.to_owned()),
        }
    }

    /// Gespeicherter Text zurück in Klartext. Unverschlüsselte Altdaten bleiben lesbar.
    pub fn open(&self, stored: &str) -> Result<String, StoreError> {
        let Some(sealed) = stored.strip_prefix(PREFIX) else {
            return Ok(stored.to_owned());
        };
        let cipher = self.cipher.as_ref().ok_or_else(|| {
            corrupt("Daten sind verschlüsselt, aber es ist kein Schlüssel gesetzt")
        })?;
        String::from_utf8(cipher.decrypt(sealed)?)
            .map_err(|_| corrupt("entschlüsselte Daten sind kein Text"))
    }

    /// Bytes, wie sie gespeichert werden.
    pub fn seal_bytes(&self, plain: Vec<u8>) -> Result<Vec<u8>, StoreError> {
        match &self.cipher {
            Some(cipher) => Ok(cipher.encrypt(&plain)?.into_bytes()),
            None => Ok(plain),
        }
    }

    /// Gespeicherte Bytes zurück in Klartext.
    pub fn open_bytes(&self, stored: Vec<u8>) -> Result<Vec<u8>, StoreError> {
        match (&self.cipher, stored.strip_prefix(PREFIX.as_bytes())) {
            (Some(cipher), Some(sealed)) => cipher.decrypt(&String::from_utf8_lossy(sealed)),
            _ => Ok(stored),
        }
    }
}

/// Nächstes Glied der Hash-Kette: Prüfsumme über das vorige Glied und den gespeicherten Text.
pub fn link(previous: &str, stored: &str) -> String {
    Digest::of(format!("{previous}\n{stored}").as_bytes()).0
}

/// Prüft die Kette über `(Position, gespeicherter Text, Glied)` in Reihenfolge. Liefert die
/// erste Position, an der sie nicht stimmt.
pub fn first_broken(rows: impl IntoIterator<Item = (u64, String, Option<String>)>) -> Option<u64> {
    let mut previous = String::new();
    for (seq, stored, chain) in rows {
        let expected = link(&previous, &stored);
        if chain.as_deref() != Some(expected.as_str()) {
            return Some(seq);
        }
        previous = expected;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

    #[test]
    fn sealed_text_roundtrips_and_hides_the_content() {
        let sealer = Sealer::encrypted(Cipher::from_hex(KEY).unwrap());
        let stored = sealer.seal("geheime Notiz").unwrap();
        assert!(stored.starts_with("enc1:") && !stored.contains("geheime"));
        assert_ne!(
            stored,
            sealer.seal("geheime Notiz").unwrap(),
            "jede Versiegelung nutzt eine neue Nonce"
        );
        assert_eq!(sealer.open(&stored).unwrap(), "geheime Notiz");
        assert_eq!(sealer.open("alter Klartext").unwrap(), "alter Klartext");
        let bytes = sealer.seal_bytes(vec![0, 1, 255]).unwrap();
        assert_eq!(sealer.open_bytes(bytes).unwrap(), vec![0, 1, 255]);
    }

    #[test]
    fn wrong_key_tampering_and_missing_key_are_detected() {
        let sealer = Sealer::encrypted(Cipher::from_hex(KEY).unwrap());
        let stored = sealer.seal("geheim").unwrap();
        let other = Sealer::encrypted(Cipher::from_hex(&KEY.replace("00", "ff")).unwrap());
        assert!(matches!(other.open(&stored), Err(StoreError::Corrupt(_))));
        let mut tampered = stored.clone();
        tampered.replace_range(10..11, if &stored[10..11] == "A" { "B" } else { "A" });
        assert!(sealer.open(&tampered).is_err());
        assert!(
            Sealer::default()
                .open(&stored)
                .unwrap_err()
                .to_string()
                .contains("kein Schlüssel")
        );
        assert_eq!(Sealer::default().seal("offen").unwrap(), "offen");
    }

    #[test]
    fn bad_keys_are_rejected() {
        for key in ["", "abc", &KEY[..62], &format!("{}zz", &KEY[..62])] {
            assert!(Cipher::from_hex(key).is_err(), "{key}");
        }
    }

    #[test]
    fn chain_detects_changed_removed_and_unchained_rows() {
        let payloads = ["eins", "zwei", "drei"];
        let mut rows = Vec::new();
        let mut previous = String::new();
        for (seq, payload) in (0u64..).zip(payloads) {
            previous = link(&previous, payload);
            rows.push((seq, payload.to_owned(), Some(previous.clone())));
        }
        assert_eq!(first_broken(rows.clone()), None);
        let mut changed = rows.clone();
        changed[1].1 = "ZWEI".into();
        assert_eq!(first_broken(changed), Some(1));
        let mut removed = rows.clone();
        removed.remove(1);
        assert_eq!(first_broken(removed), Some(2));
        let mut unchained = rows;
        unchained[0].2 = None;
        assert_eq!(first_broken(unchained), Some(0));
    }
}
