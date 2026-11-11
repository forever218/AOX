//! Room passphrase encryption: AES-256-GCM, key from passphrase plus room id via PBKDF2.
//! Server sees only enc:v1:<base64(nonce||ciphertext)> as a plain string; the passphrase never goes online.
//! Text only, no forward secrecy; PBKDF2 slows weak-passphrase cracking; room id as salt gives per-room keys.

use aes_gcm::aead::rand_core::RngCore;
use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use pbkdf2::pbkdf2_hmac;
use sha2::Sha256;

/// The prefix is the version: failed or unknown-version strings show as-is, never swallowed as ciphertext.
pub const PREFIX: &str = "enc:v1:";

const ITERATIONS: u32 = 100_000;
const NONCE_LEN: usize = 12;

pub fn is_encrypted(text: &str) -> bool {
    text.starts_with(PREFIX)
}

pub fn derive_key(passphrase: &str, room: &str) -> [u8; 32] {
    let mut key = [0u8; 32];
    pbkdf2_hmac::<Sha256>(passphrase.as_bytes(), room.as_bytes(), ITERATIONS, &mut key);
    key
}

pub fn encrypt(key: &[u8; 32], plain: &str) -> String {
    let cipher = Aes256Gcm::new(key.into());
    let mut nonce = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce);
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce), plain.as_bytes())
        .expect("AES-GCM encrypt cannot fail for valid sizes");
    let mut raw = Vec::with_capacity(NONCE_LEN + ct.len());
    raw.extend_from_slice(&nonce);
    raw.extend_from_slice(&ct);
    format!("{PREFIX}{}", STANDARD.encode(raw))
}

/// Return None on failure: wrong passphrase, tampering and unknown format are deliberately indistinguishable.
pub fn decrypt(key: &[u8; 32], text: &str) -> Option<String> {
    let b64 = text.strip_prefix(PREFIX)?;
    let raw = STANDARD.decode(b64).ok()?;
    if raw.len() <= NONCE_LEN {
        return None;
    }
    let (nonce, ct) = raw.split_at(NONCE_LEN);
    let cipher = Aes256Gcm::new(key.into());
    let plain = cipher.decrypt(Nonce::from_slice(nonce), ct).ok()?;
    String::from_utf8(plain).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let key = derive_key("正确的口令 horse battery", "room-1");
        let ct = encrypt(&key, "你好，世界 **markdown** 🙂");
        assert!(is_encrypted(&ct));
        assert_eq!(decrypt(&key, &ct).as_deref(), Some("你好，世界 **markdown** 🙂"));
    }

    #[test]
    fn same_passphrase_different_room_gives_different_key() {
        let a = derive_key("same", "room-a");
        let b = derive_key("same", "room-b");
        assert_ne!(a, b);
        let ct = encrypt(&a, "secret");
        assert_eq!(decrypt(&b, &ct), None);
    }

    #[test]
    fn wrong_passphrase_fails_cleanly() {
        let ct = encrypt(&derive_key("right", "r"), "hello");
        assert_eq!(decrypt(&derive_key("wrong", "r"), &ct), None);
    }

    #[test]
    fn tampering_is_detected() {
        let key = derive_key("k", "r");
        let ct = encrypt(&key, "hello");
        let mut raw = STANDARD.decode(ct.strip_prefix(PREFIX).unwrap()).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 0x01;
        let bad = format!("{PREFIX}{}", STANDARD.encode(raw));
        assert_eq!(decrypt(&key, &bad), None);
    }


    /// Dart's crypto_check.dart vector: decrypting pins KDF params, nonce/tag and base64; any change fails this test.
    #[test]
    fn decrypts_a_dart_produced_vector() {
        let key = derive_key("correct horse battery staple", "room-test-1");
        let ct = "enc:v1:hhhXgw8SNVOFkcJkdIAgcHO3oauN0E0po1M+FKlMbjbPx6esgyQx3ypJEsfu8wvpW+vJHaoGnmF0L06/ydYpkg0=";
        assert_eq!(
            decrypt(&key, ct).as_deref(),
            Some("跨语言测试 **hello** 你好 🙂")
        );
    }

    #[test]
    fn plaintext_passes_through_untouched() {
        assert!(!is_encrypted("普通消息"));
        assert_eq!(decrypt(&derive_key("k", "r"), "普通消息"), None);
    }
}
