//! iOS backup keybag parsing and class-key unlocking.

use crate::crypto::aes_unwrap;
use anyhow::{Result, bail};
use std::collections::HashMap;

#[derive(Debug, Default, Clone)]
/// One class-key record from the keybag.
pub struct ClassKeyEntry {
    /// Protection class.
    pub class: u32,
    /// Wrap flags (2 = passcode-wrapped).
    pub wrap: u32,
    /// Wrapped class key.
    pub wpky: Vec<u8>,
}

#[derive(Debug, Default, Clone)]
/// Parsed keybag header and class keys.
pub struct Keybag {
    /// Outer PBKDF2 salt (SALT).
    pub salt: Vec<u8>,
    /// Outer PBKDF2 iterations (ITER).
    pub iter: u32,
    /// Inner PBKDF2 salt (DPSL).
    pub dpsl: Vec<u8>,
    /// Inner PBKDF2 iterations (DPIC).
    pub dpic: u32,
    /// Class-key records.
    pub class_keys: Vec<ClassKeyEntry>,
}

fn be_u32(v: &[u8]) -> u32 {
    v.get(..4)
        .and_then(|b| b.try_into().ok())
        .map_or(0, u32::from_be_bytes)
}

impl Keybag {
    /// Parse the TLV keybag: 4-byte tag, 4-byte big-endian length, value.
    /// The first `UUID` belongs to the keybag header; each later `UUID`
    /// starts a new class-key record.
    pub fn parse(data: &[u8]) -> Result<Self> {
        let mut kb = Self::default();
        let mut uuids = 0;
        let mut rest = data;
        while !rest.is_empty() {
            let (Some(tag), Some(len_bytes)) = (rest.get(..4), rest.get(4..8)) else {
                bail!("truncated keybag header");
            };
            let len = usize::try_from(be_u32(len_bytes))?;
            let Some(val) = rest.get(8..8 + len) else {
                bail!(
                    "truncated keybag value for tag {}",
                    String::from_utf8_lossy(tag)
                );
            };
            rest = rest.get(8 + len..).unwrap_or_default();
            if tag == b"UUID" {
                uuids += 1;
                if uuids > 1 {
                    kb.class_keys.push(ClassKeyEntry::default());
                }
                continue;
            }
            match kb.class_keys.last_mut() {
                Some(ck) => match tag {
                    b"CLAS" => ck.class = be_u32(val),
                    b"WRAP" => ck.wrap = be_u32(val),
                    b"WPKY" => ck.wpky = val.to_vec(),
                    _ => {}
                },
                None => match tag {
                    b"SALT" => kb.salt = val.to_vec(),
                    b"ITER" => kb.iter = be_u32(val),
                    b"DPSL" => kb.dpsl = val.to_vec(),
                    b"DPIC" => kb.dpic = be_u32(val),
                    _ => {}
                },
            }
        }
        Ok(kb)
    }

    /// PBKDF2-SHA1(PBKDF2-SHA256(password, DPSL, DPIC), SALT, ITER).
    pub fn passcode_key(&self, password: &str) -> Result<[u8; 32]> {
        if self.iter == 0 || self.dpic == 0 || self.salt.is_empty() || self.dpsl.is_empty() {
            bail!("keybag is missing key-derivation parameters (SALT/ITER/DPSL/DPIC)");
        }
        let mut first = [0u8; 32];
        pbkdf2::pbkdf2_hmac::<sha2::Sha256>(password.as_bytes(), &self.dpsl, self.dpic, &mut first);
        let mut key = [0u8; 32];
        pbkdf2::pbkdf2_hmac::<sha1::Sha1>(&first, &self.salt, self.iter, &mut key);
        Ok(key)
    }

    /// Unlock all passcode-wrapped class keys. Fails if none can be unwrapped,
    /// which means the password is wrong.
    pub fn unlock(&self, password: &str) -> Result<ClassKeys> {
        let pk = self.passcode_key(password)?;
        let mut keys = HashMap::new();
        for ck in self.class_keys.iter().filter(|c| c.wrap & 2 != 0) {
            if let Some(k) = aes_unwrap(&pk, &ck.wpky) {
                keys.insert(ck.class, k);
            }
        }
        if keys.is_empty() {
            bail!("incorrect backup password (could not unlock the keybag)");
        }
        Ok(ClassKeys(keys))
    }
}

/// Unwrapped class keys by protection class.
#[derive(Debug)]
pub struct ClassKeys(HashMap<u32, Vec<u8>>);

impl ClassKeys {
    /// Unwrap a `class (LE u32) || wrapped key` blob (`ManifestKey` / `EncryptionKey`).
    pub fn unwrap_blob(&self, blob: &[u8]) -> Result<Vec<u8>> {
        let (Some(class), Some(wrapped)) = (blob.get(..4), blob.get(4..)) else {
            bail!("wrapped key blob too short ({} bytes)", blob.len());
        };
        let class = class.try_into().map(u32::from_le_bytes)?;
        let Some(class_key) = self.0.get(&class) else {
            bail!("no unlocked class key for protection class {class}");
        };
        aes_unwrap(class_key, wrapped)
            .ok_or_else(|| anyhow::anyhow!("failed to unwrap file key with class {class} key"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tlv(tag: &[u8], val: &[u8]) -> Vec<u8> {
        let mut v = tag.to_vec();
        v.extend(u32::try_from(val.len()).unwrap().to_be_bytes());
        v.extend(val);
        v
    }

    #[test]
    fn parses_header_and_class_keys() {
        let mut d = Vec::new();
        d.extend(tlv(b"VERS", &4u32.to_be_bytes()));
        d.extend(tlv(b"TYPE", &1u32.to_be_bytes()));
        d.extend(tlv(b"UUID", &[1; 16]));
        d.extend(tlv(b"WRAP", &1u32.to_be_bytes()));
        d.extend(tlv(b"SALT", &[5; 20]));
        d.extend(tlv(b"ITER", &1000u32.to_be_bytes()));
        d.extend(tlv(b"DPIC", &77u32.to_be_bytes()));
        d.extend(tlv(b"DPSL", &[6; 20]));
        for class in [3u32, 4] {
            d.extend(tlv(b"UUID", &[u8::try_from(class).unwrap(); 16]));
            d.extend(tlv(b"CLAS", &class.to_be_bytes()));
            d.extend(tlv(b"WRAP", &3u32.to_be_bytes()));
            d.extend(tlv(b"KTYP", &0u32.to_be_bytes()));
            d.extend(tlv(b"WPKY", &[u8::try_from(class).unwrap(); 40]));
        }
        let kb = Keybag::parse(&d).unwrap();
        assert_eq!(kb.iter, 1000);
        assert_eq!(kb.dpic, 77);
        assert_eq!(kb.salt, vec![5; 20]);
        assert_eq!(kb.dpsl, vec![6; 20]);
        assert_eq!(kb.class_keys.len(), 2);
        assert_eq!(kb.class_keys[1].class, 4);
        assert_eq!(kb.class_keys[1].wrap, 3);
        assert_eq!(kb.class_keys[1].wpky, vec![4; 40]);
    }

    #[test]
    fn truncated_keybag_is_error() {
        let mut d = tlv(b"SALT", &[1; 20]);
        d.truncate(d.len() - 3);
        assert!(Keybag::parse(&d).is_err());
    }
}
