//! Small AES helpers: RFC 3394 key wrap, AES-256-CBC (zero IV) and PKCS#7.

// Everything below works on 8/16-byte blocks whose lengths are validated up
// front, so slicing is bounded by construction.
#![expect(
    clippy::indexing_slicing,
    reason = "fixed-size block arithmetic with lengths validated up front"
)]

use aes::Aes256;
use aes::cipher::{Block, BlockCipherDecrypt, BlockCipherEncrypt, KeyInit};
use anyhow::{Result, anyhow, bail};
use std::io::{Read, Write};

const WRAP_IV: [u8; 8] = [0xA6; 8];
/// Streaming chunk size; must be a multiple of 16.
const CHUNK: usize = 1 << 20;

fn cipher(key: &[u8]) -> Result<Aes256> {
    Aes256::new_from_slice(key).map_err(|_| anyhow!("AES-256 key must be 32 bytes"))
}

fn to_block(bytes: &[u8]) -> Result<Block<Aes256>> {
    Block::<Aes256>::try_from(bytes).map_err(|_| anyhow!("AES block must be 16 bytes"))
}

fn wrap_step(a: [u8; 8], r: [u8; 8]) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&a);
    out[8..].copy_from_slice(&r);
    out
}

fn xor_counter(a: &[u8], t: usize) -> Option<[u8; 8]> {
    let t = u64::try_from(t).ok()?;
    Some((u64::from_be_bytes(a.try_into().ok()?) ^ t).to_be_bytes())
}

/// RFC 3394 AES key unwrap. Returns `None` if the key is invalid or the
/// integrity check fails (i.e. the wrong key was used).
#[must_use]
pub fn aes_unwrap(kek: &[u8], wrapped: &[u8]) -> Option<Vec<u8>> {
    if wrapped.len() < 24 || !wrapped.len().is_multiple_of(8) {
        return None;
    }
    let aes = cipher(kek).ok()?;
    let n = wrapped.len() / 8 - 1;
    let mut a: [u8; 8] = wrapped[..8].try_into().ok()?;
    let mut regs: Vec<[u8; 8]> = wrapped[8..]
        .chunks_exact(8)
        .filter_map(|x| x.try_into().ok())
        .collect();
    for j in (0..6).rev() {
        for i in (1..=n).rev() {
            let masked = xor_counter(&a, n * j + i)?;
            let mut blk = to_block(&wrap_step(masked, regs[i - 1])).ok()?;
            aes.decrypt_block(&mut blk);
            a.copy_from_slice(&blk[..8]);
            regs[i - 1].copy_from_slice(&blk[8..]);
        }
    }
    (a == WRAP_IV).then(|| regs.concat())
}

/// RFC 3394 AES key wrap (used by tests to build fixtures).
///
/// # Errors
/// Fails if the KEK is not 32 bytes or the key is not a multiple of 8 bytes (>= 16).
pub fn aes_wrap(kek: &[u8], key: &[u8]) -> Result<Vec<u8>> {
    if key.len() < 16 || !key.len().is_multiple_of(8) {
        bail!("key to wrap must be a multiple of 8 bytes, at least 16");
    }
    let aes = cipher(kek)?;
    let n = key.len() / 8;
    let mut a = WRAP_IV;
    let mut regs: Vec<[u8; 8]> = key
        .chunks_exact(8)
        .filter_map(|x| x.try_into().ok())
        .collect();
    for j in 0..6 {
        for i in 1..=n {
            let mut blk = to_block(&wrap_step(a, regs[i - 1]))?;
            aes.encrypt_block(&mut blk);
            a = xor_counter(&blk[..8], n * j + i).ok_or_else(|| anyhow!("bad counter"))?;
            regs[i - 1].copy_from_slice(&blk[8..]);
        }
    }
    let mut out = a.to_vec();
    out.extend(regs.concat());
    Ok(out)
}

/// Length of the data without valid PKCS#7 padding, or the full length if the
/// padding is not valid.
#[must_use]
pub fn pkcs7_unpadded_len(data: &[u8]) -> usize {
    match data.last() {
        Some(&p) if (1..=16).contains(&p) && data.len() >= usize::from(p) => {
            let start = data.len() - usize::from(p);
            if data[start..].iter().all(|&b| b == p) {
                start
            } else {
                data.len()
            }
        }
        _ => data.len(),
    }
}

/// Append PKCS#7 padding.
#[must_use]
pub fn pkcs7_pad(data: &[u8]) -> Vec<u8> {
    let pad = 16 - data.len() % 16;
    let mut v = data.to_vec();
    v.extend(std::iter::repeat_n(u8::try_from(pad).unwrap_or(16), pad));
    v
}

/// AES-256-CBC encrypt with a zero IV and PKCS#7 padding.
///
/// # Errors
/// Fails if the key is not 32 bytes.
pub fn cbc_encrypt(key: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let aes = cipher(key)?;
    let mut out = pkcs7_pad(data);
    let mut prev = [0u8; 16];
    for chunk in out.chunks_exact_mut(16) {
        for (b, p) in chunk.iter_mut().zip(prev) {
            *b ^= p;
        }
        let mut blk = to_block(chunk)?;
        aes.encrypt_block(&mut blk);
        chunk.copy_from_slice(&blk);
        prev.copy_from_slice(chunk);
    }
    Ok(out)
}

/// Streaming AES-256-CBC (zero IV) decrypt. Strips PKCS#7 padding when valid
/// and truncates to `size` if given. Returns the number of bytes written.
///
/// # Errors
/// Fails on I/O errors, a bad key, or input that is not a whole number of blocks.
pub fn cbc_decrypt_stream(
    key: &[u8],
    mut input: impl Read,
    mut output: impl Write,
    size: Option<u64>,
) -> Result<u64> {
    let aes = cipher(key)?;
    let mut prev = [0u8; 16];
    let mut written = 0u64;
    let limit = size.unwrap_or(u64::MAX);
    // The previous decrypted chunk is held back until we know it is not the last.
    let mut held: Option<Vec<u8>> = None;
    let mut emit = |buf: &[u8], output: &mut dyn Write| -> Result<()> {
        let n = usize::try_from(limit - written).map_or(buf.len(), |rem| rem.min(buf.len()));
        output.write_all(&buf[..n])?;
        written += u64::try_from(n)?;
        Ok(())
    };
    loop {
        let mut buf = vec![0u8; CHUNK];
        let mut filled = 0;
        while filled < CHUNK {
            let n = input.read(&mut buf[filled..])?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        buf.truncate(filled);
        if buf.is_empty() {
            break;
        }
        if !buf.len().is_multiple_of(16) {
            bail!("encrypted data length is not a multiple of the AES block size");
        }
        for chunk in buf.chunks_exact_mut(16) {
            let ciphertext: [u8; 16] = chunk.try_into()?;
            let mut blk = to_block(chunk)?;
            aes.decrypt_block(&mut blk);
            for (o, p) in blk.iter_mut().zip(prev) {
                *o ^= p;
            }
            chunk.copy_from_slice(&blk);
            prev = ciphertext;
        }
        if let Some(h) = held.replace(buf) {
            emit(&h, &mut output)?;
        }
    }
    if let Some(last) = held {
        let n = pkcs7_unpadded_len(&last);
        emit(&last[..n], &mut output)?;
    }
    output.flush()?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn rfc3394_256bit_kek_128bit_data() {
        let kek = unhex("000102030405060708090A0B0C0D0E0F101112131415161718191A1B1C1D1E1F");
        let data = unhex("00112233445566778899AABBCCDDEEFF");
        let wrapped = unhex("64E8C3F9CE0F5BA263E9777905818A2A93C8191E7D6E8AE7");
        assert_eq!(aes_wrap(&kek, &data).unwrap(), wrapped);
        assert_eq!(aes_unwrap(&kek, &wrapped).unwrap(), data);
    }

    #[test]
    fn unwrap_rejects_wrong_key_and_tampering() {
        let kek = [7u8; 32];
        let w = aes_wrap(&kek, &[1u8; 32]).unwrap();
        assert_eq!(aes_unwrap(&kek, &w).unwrap(), vec![1u8; 32]);
        assert!(aes_unwrap(&[8u8; 32], &w).is_none());
        let mut t = w;
        t[3] ^= 1;
        assert!(aes_unwrap(&kek, &t).is_none());
    }

    #[test]
    fn pkcs7() {
        assert_eq!(pkcs7_unpadded_len(&pkcs7_pad(b"hello")), 5);
        assert_eq!(pkcs7_unpadded_len(&pkcs7_pad(&[0u8; 16])), 16);
        let mut bad = vec![1u8; 16];
        bad[15] = 3; // claims 3 bytes of padding but they differ
        assert_eq!(pkcs7_unpadded_len(&bad), 16);
        assert_eq!(pkcs7_unpadded_len(&[17u8; 16]), 16); // pad byte > 16
        assert_eq!(pkcs7_unpadded_len(&[]), 0);
    }

    #[test]
    fn stream_roundtrip_various_sizes() {
        let key = [42u8; 32];
        for len in [
            0usize,
            1,
            15,
            16,
            17,
            CHUNK - 1,
            CHUNK,
            CHUNK + 5,
            2 * CHUNK + 33,
        ] {
            let data: Vec<u8> = (0..len)
                .map(|i| u8::try_from(i * 31 % 251).unwrap())
                .collect();
            let ct = cbc_encrypt(&key, &data).unwrap();
            let mut out = Vec::new();
            let n = cbc_decrypt_stream(&key, &ct[..], &mut out, None).unwrap();
            assert_eq!(out, data, "len {len}");
            assert_eq!(usize::try_from(n).unwrap(), len);
            let mut out = Vec::new();
            cbc_decrypt_stream(
                &key,
                &ct[..],
                &mut out,
                Some(u64::try_from(len / 2).unwrap()),
            )
            .unwrap();
            assert_eq!(out, &data[..len / 2]);
        }
    }

    #[test]
    fn stream_rejects_ragged_input() {
        let mut out = Vec::new();
        assert!(cbc_decrypt_stream(&[0u8; 32], &[0u8; 17][..], &mut out, None).is_err());
    }
}
