//! The cryptography behind password locks.
//!
//! A locked collection has one random 256-bit data key. That key seals every
//! file and the manifest of the collection with XChaCha20-Poly1305. The data
//! key is itself wrapped by a key derived from the password with Argon2id, so
//! the password is never stored, and a wrong password is detected because the
//! wrapped key fails to authenticate.
//!
//! No public-key cryptography is involved. Grover's algorithm halves the
//! effective strength of a symmetric key, which leaves a 256-bit key at 128
//! bits against a quantum attacker, so the data key is not the weak point. The
//! password is, which is why it goes through a memory-hard KDF and why the
//! caller enforces a minimum length.

use crate::error::{Error, Result};
use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use std::io::{BufRead, BufReader, Read, Write};
use zeroize::{Zeroize, Zeroizing};

pub const KEY_LEN: usize = 32;
pub const SALT_LEN: usize = 16;
/// The shortest password a lock accepts. Counted in characters, not bytes.
pub const MIN_PASSWORD_CHARS: usize = 8;

const NONCE_LEN: usize = 24;
const PREFIX_LEN: usize = 16;
const TAG_LEN: usize = 16;
/// Plaintext per record. Large enough that the per-record overhead is noise,
/// small enough that a corrupted length cannot ask for a large allocation.
const CHUNK: usize = 1024 * 1024;
const MAGIC: &[u8; 4] = b"FLV1";
// Lock records are catalogue data and can be damaged or replaced. Keep their
// work factor within limits suitable for interactive unlocks.
const MAX_KDF_MEMORY_KIB: u32 = 1024 * 1024;
const MAX_KDF_ITERATIONS: u32 = 16;
const MAX_KDF_PARALLELISM: u32 = 16;

/// What a sealed stream holds. Bound into every record, so a file cannot be
/// passed off as the other kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Purpose {
    Blob = 1,
    Manifest = 2,
}

/// Argon2id cost parameters. Stored with each lock, so new locks can use a
/// stronger setting without breaking the ones already made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Kdf {
    pub memory_kib: u32,
    pub iterations: u32,
    pub parallelism: u32,
}

/// The cost new locks are made with: 64 MiB, three passes, four lanes.
pub const DEFAULT_KDF: Kdf = Kdf {
    memory_kib: 64 * 1024,
    iterations: 3,
    parallelism: 4,
};

impl Kdf {
    pub fn encode(self) -> String {
        format!(
            "argon2id:m={},t={},p={}",
            self.memory_kib, self.iterations, self.parallelism
        )
    }

    pub fn decode(text: &str) -> Result<Self> {
        let damaged = || Error::msg("the lock's key settings are damaged");
        let rest = text.strip_prefix("argon2id:").ok_or_else(damaged)?;
        let mut kdf = Self {
            memory_kib: 0,
            iterations: 0,
            parallelism: 0,
        };
        let mut seen = [false; 3];
        for part in rest.split(',') {
            let (name, value) = part.split_once('=').ok_or_else(damaged)?;
            let value: u32 = value.parse().map_err(|_| damaged())?;
            match name {
                "m" => {
                    if seen[0] {
                        return Err(damaged());
                    }
                    seen[0] = true;
                    kdf.memory_kib = value;
                }
                "t" => {
                    if seen[1] {
                        return Err(damaged());
                    }
                    seen[1] = true;
                    kdf.iterations = value;
                }
                "p" => {
                    if seen[2] {
                        return Err(damaged());
                    }
                    seen[2] = true;
                    kdf.parallelism = value;
                }
                _ => return Err(damaged()),
            }
        }
        if !seen.into_iter().all(|v| v) || !kdf.is_supported() {
            return Err(damaged());
        }
        Ok(kdf)
    }
}

impl Kdf {
    fn is_supported(self) -> bool {
        (1..=MAX_KDF_PARALLELISM).contains(&self.parallelism)
            && self.memory_kib >= 8 * self.parallelism
            && self.memory_kib <= MAX_KDF_MEMORY_KIB
            && (1..=MAX_KDF_ITERATIONS).contains(&self.iterations)
    }
}

/// A 256-bit key. Zeroed when dropped, and never printed or cloned.
pub struct Key(Zeroizing<[u8; KEY_LEN]>);

impl Key {
    pub fn random() -> Result<Self> {
        let mut bytes = Zeroizing::new([0u8; KEY_LEN]);
        random(bytes.as_mut_slice())?;
        Ok(Self(bytes))
    }

    fn cipher(&self) -> XChaCha20Poly1305 {
        // The slice is exactly KEY_LEN bytes, so construction cannot fail.
        XChaCha20Poly1305::new_from_slice(self.0.as_slice()).expect("key length")
    }
}

/// A fresh random salt for a lock's password.
pub fn new_salt() -> Result<[u8; SALT_LEN]> {
    let mut salt = [0u8; SALT_LEN];
    random(&mut salt)?;
    Ok(salt)
}

/// Stretch a password into a key-encrypting key. Slow by design.
pub fn derive(password: &str, salt: &[u8], kdf: Kdf) -> Result<Key> {
    if !kdf.is_supported() {
        return Err(Error::msg("the key settings are outside supported limits"));
    }
    let params = Params::new(
        kdf.memory_kib,
        kdf.iterations,
        kdf.parallelism,
        Some(KEY_LEN),
    )
    .map_err(|error| Error::msg(format!("the key settings are invalid: {error}")))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut out = Zeroizing::new([0u8; KEY_LEN]);
    argon
        .hash_password_into(password.as_bytes(), salt, out.as_mut_slice())
        .map_err(|error| Error::msg(format!("the password could not be processed: {error}")))?;
    Ok(Key(out))
}

/// Encrypt the data key under the password key. The result is the nonce
/// followed by the ciphertext and its tag.
pub fn wrap(kek: &Key, data_key: &Key, context: &[u8]) -> Result<Vec<u8>> {
    let mut nonce = [0u8; NONCE_LEN];
    random(&mut nonce)?;
    let sealed = kek
        .cipher()
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: data_key.0.as_slice(),
                aad: context,
            },
        )
        .map_err(|_| Error::msg("the lock key could not be wrapped"))?;
    let mut out = nonce.to_vec();
    out.extend_from_slice(&sealed);
    Ok(out)
}

/// Recover the data key. Fails with [`Error::WrongPassword`] when the password
/// key does not authenticate, which is the only way a password is checked.
pub fn unwrap(kek: &Key, wrapped: &[u8], context: &[u8]) -> Result<Key> {
    if wrapped.len() != NONCE_LEN + KEY_LEN + TAG_LEN {
        return Err(Error::msg("the lock record is damaged"));
    }
    let (nonce, sealed) = wrapped.split_at(NONCE_LEN);
    let mut plain = kek
        .cipher()
        .decrypt(
            XNonce::from_slice(nonce),
            Payload {
                msg: sealed,
                aad: context,
            },
        )
        .map_err(|_| Error::WrongPassword)?;
    let mut bytes = Zeroizing::new([0u8; KEY_LEN]);
    bytes.copy_from_slice(&plain);
    plain.zeroize();
    Ok(Key(bytes))
}

/// Encrypt `input` into `output` as a sealed stream and return the plaintext
/// length. The stream is self-describing: its header names the purpose and
/// `context`, so [`open_stream`] refuses it under any other.
///
/// Each record is authenticated together with a flag that marks the last one,
/// so a stream cut short, or with records dropped from its end, fails to open.
pub fn seal_stream(
    key: &Key,
    purpose: Purpose,
    context: &[u8],
    input: impl Read,
    mut output: impl Write,
) -> Result<u64> {
    let mut prefix = [0u8; PREFIX_LEN];
    random(&mut prefix)?;
    let header = header(purpose, context, &prefix)?;
    let cipher = key.cipher();
    output.write_all(&header)?;

    let mut reader = BufReader::with_capacity(CHUNK, input);
    let mut buffer = vec![0u8; CHUNK];
    let mut index = 0u64;
    let mut total = 0u64;
    loop {
        let read = fill(&mut reader, &mut buffer)?;
        let last = reader.fill_buf()?.is_empty();
        let flag = [u8::from(last)];
        let mut aad = header.clone();
        aad.extend_from_slice(&flag);
        let sealed = cipher
            .encrypt(
                XNonce::from_slice(&nonce(&prefix, index)),
                Payload {
                    msg: &buffer[..read],
                    aad: &aad,
                },
            )
            .map_err(|_| Error::msg("the data could not be sealed"))?;
        output.write_all(&flag)?;
        output.write_all(&(sealed.len() as u32).to_le_bytes())?;
        output.write_all(&sealed)?;
        total += read as u64;
        index += 1;
        if last {
            break;
        }
    }
    output.flush()?;
    Ok(total)
}

/// Decrypt a stream written by [`seal_stream`] into `output`, returning the
/// plaintext length. Nothing is trusted until its record authenticates, so a
/// modified record fails the whole stream.
pub fn open_stream(
    key: &Key,
    purpose: Purpose,
    context: &[u8],
    input: impl Read,
    mut output: impl Write,
) -> Result<u64> {
    let damaged = || Error::msg("the sealed data is damaged");
    let mut reader = BufReader::with_capacity(CHUNK + TAG_LEN + 8, input);

    let mut fixed = [0u8; 7];
    read_exact(&mut reader, &mut fixed)?;
    if &fixed[..4] != MAGIC || fixed[4] != purpose as u8 {
        return Err(damaged());
    }
    let context_len = u16::from_le_bytes([fixed[5], fixed[6]]) as usize;
    let mut stored_context = vec![0u8; context_len];
    read_exact(&mut reader, &mut stored_context)?;
    if stored_context != context {
        return Err(Error::msg(
            "this sealed data belongs to a different collection",
        ));
    }
    let mut prefix = [0u8; PREFIX_LEN];
    read_exact(&mut reader, &mut prefix)?;
    let mut header = Vec::with_capacity(fixed.len() + context_len + PREFIX_LEN);
    header.extend_from_slice(&fixed);
    header.extend_from_slice(&stored_context);
    header.extend_from_slice(&prefix);

    let cipher = key.cipher();
    let mut index = 0u64;
    let mut total = 0u64;
    loop {
        let mut flag = [0u8; 1];
        read_exact(&mut reader, &mut flag)?;
        let last = match flag[0] {
            0 => false,
            1 => true,
            _ => return Err(damaged()),
        };
        let mut length = [0u8; 4];
        read_exact(&mut reader, &mut length)?;
        let length = u32::from_le_bytes(length) as usize;
        if !(TAG_LEN..=CHUNK + TAG_LEN).contains(&length) {
            return Err(damaged());
        }
        let mut sealed = vec![0u8; length];
        read_exact(&mut reader, &mut sealed)?;
        let mut aad = header.clone();
        aad.push(flag[0]);
        let plain = cipher
            .decrypt(
                XNonce::from_slice(&nonce(&prefix, index)),
                Payload {
                    msg: &sealed,
                    aad: &aad,
                },
            )
            .map_err(|_| Error::msg("the sealed data is damaged or was modified"))?;
        output.write_all(&plain)?;
        total += plain.len() as u64;
        index += 1;
        if last {
            // Anything after the final record was not written by the sealer.
            if !reader.fill_buf()?.is_empty() {
                return Err(damaged());
            }
            break;
        }
    }
    output.flush()?;
    Ok(total)
}

fn header(purpose: Purpose, context: &[u8], prefix: &[u8; PREFIX_LEN]) -> Result<Vec<u8>> {
    let context_len: u16 = context
        .len()
        .try_into()
        .map_err(|_| Error::msg("the sealed data's name is too long"))?;
    let mut header = Vec::with_capacity(7 + context.len() + PREFIX_LEN);
    header.extend_from_slice(MAGIC);
    header.push(purpose as u8);
    header.extend_from_slice(&context_len.to_le_bytes());
    header.extend_from_slice(context);
    header.extend_from_slice(prefix);
    Ok(header)
}

/// The per-record nonce: the file's random prefix, then the record's index.
/// Indices never repeat within a file, and the prefix never repeats across
/// files, so no (key, nonce) pair is reused.
fn nonce(prefix: &[u8; PREFIX_LEN], index: u64) -> [u8; NONCE_LEN] {
    let mut nonce = [0u8; NONCE_LEN];
    nonce[..PREFIX_LEN].copy_from_slice(prefix);
    nonce[PREFIX_LEN..].copy_from_slice(&index.to_le_bytes());
    nonce
}

fn random(bytes: &mut [u8]) -> Result<()> {
    getrandom::fill(bytes)
        .map_err(|error| Error::msg(format!("no randomness is available: {error}")))
}

/// Read until `buffer` is full or the input ends, returning the count.
fn fill(reader: &mut impl Read, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

/// `read_exact`, but a short input is reported as damage rather than as a
/// bare I/O error, so the message says what the problem is.
fn read_exact(reader: &mut impl Read, buffer: &mut [u8]) -> Result<()> {
    reader.read_exact(buffer).map_err(|error| {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            Error::msg("the sealed data is truncated")
        } else {
            error.into()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cheap enough for tests. Real locks use [`DEFAULT_KDF`].
    const TEST_KDF: Kdf = Kdf {
        memory_kib: 8,
        iterations: 1,
        parallelism: 1,
    };

    fn seal(key: &Key, purpose: Purpose, context: &[u8], data: &[u8]) -> Vec<u8> {
        let mut sealed = Vec::new();
        seal_stream(key, purpose, context, data, &mut sealed).unwrap();
        sealed
    }

    fn open(key: &Key, purpose: Purpose, context: &[u8], sealed: &[u8]) -> Result<Vec<u8>> {
        let mut plain = Vec::new();
        open_stream(key, purpose, context, sealed, &mut plain)?;
        Ok(plain)
    }

    #[test]
    fn streams_round_trip_across_record_boundaries() {
        let key = Key::random().unwrap();
        for len in [0, 1, CHUNK - 1, CHUNK, CHUNK + 1, 3 * CHUNK + 17] {
            let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let sealed = seal(&key, Purpose::Blob, b"ctx", &data);
            assert_eq!(open(&key, Purpose::Blob, b"ctx", &sealed).unwrap(), data);
        }
    }

    #[test]
    fn a_stream_opens_only_under_its_key_purpose_and_context() {
        let key = Key::random().unwrap();
        let other = Key::random().unwrap();
        let sealed = seal(&key, Purpose::Blob, b"collection 4", b"secret");
        assert!(open(&other, Purpose::Blob, b"collection 4", &sealed).is_err());
        assert!(open(&key, Purpose::Manifest, b"collection 4", &sealed).is_err());
        assert!(open(&key, Purpose::Blob, b"collection 5", &sealed).is_err());
        assert_eq!(
            open(&key, Purpose::Blob, b"collection 4", &sealed).unwrap(),
            b"secret"
        );
    }

    #[test]
    fn any_modified_byte_fails_authentication() {
        let key = Key::random().unwrap();
        let sealed = seal(&key, Purpose::Manifest, b"m", b"the manifest body");
        for at in 0..sealed.len() {
            let mut tampered = sealed.clone();
            tampered[at] ^= 0x01;
            assert!(
                open(&key, Purpose::Manifest, b"m", &tampered).is_err(),
                "a flipped byte at {at} was accepted"
            );
        }
    }

    #[test]
    fn truncation_and_appended_data_are_rejected() {
        let key = Key::random().unwrap();
        let data = vec![7u8; 2 * CHUNK + 5];
        let sealed = seal(&key, Purpose::Blob, b"t", &data);
        for cut in [sealed.len() - 1, sealed.len() / 2, 10] {
            assert!(open(&key, Purpose::Blob, b"t", &sealed[..cut]).is_err());
        }
        let mut extended = sealed.clone();
        extended.extend_from_slice(&[0u8; 4]);
        assert!(open(&key, Purpose::Blob, b"t", &extended).is_err());
    }

    #[test]
    fn a_wrong_password_does_not_unwrap_the_data_key() {
        let salt = new_salt().unwrap();
        let data_key = Key::random().unwrap();
        let kek = derive("correct horse", &salt, TEST_KDF).unwrap();
        let wrapped = wrap(&kek, &data_key, b"lock 3").unwrap();

        let wrong = derive("battery staple", &salt, TEST_KDF).unwrap();
        assert!(matches!(
            unwrap(&wrong, &wrapped, b"lock 3"),
            Err(Error::WrongPassword)
        ));
        assert!(matches!(
            unwrap(&kek, &wrapped, b"lock 4"),
            Err(Error::WrongPassword)
        ));

        let recovered = unwrap(&kek, &wrapped, b"lock 3").unwrap();
        let sealed = seal(&data_key, Purpose::Blob, b"x", b"payload");
        assert_eq!(
            open(&recovered, Purpose::Blob, b"x", &sealed).unwrap(),
            b"payload"
        );
    }

    #[test]
    fn kdf_settings_round_trip_and_reject_damage() {
        let text = DEFAULT_KDF.encode();
        assert_eq!(Kdf::decode(&text).unwrap(), DEFAULT_KDF);
        assert!(Kdf::decode("scrypt:m=1").is_err());
        assert!(Kdf::decode("argon2id:m=0,t=3,p=4").is_err());
        assert!(Kdf::decode("argon2id:m=65536,t=3").is_err());
        for text in [
            "argon2id:m=65536,m=65536,t=3,p=4",
            "argon2id:m=4294967295,t=3,p=4",
            "argon2id:m=65536,t=4294967295,p=4",
            "argon2id:m=65536,t=3,p=4294967295",
        ] {
            assert!(Kdf::decode(text).is_err(), "accepted {text}");
        }
        assert!(
            derive(
                "password",
                &[0; SALT_LEN],
                Kdf {
                    memory_kib: 65536,
                    iterations: 3,
                    parallelism: u32::MAX,
                }
            )
            .is_err()
        );
    }
}
