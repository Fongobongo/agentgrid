//! Sealed secrets: a control plane that can deliver a credential without
//! being able to read it (ADR 0028, phase 1B).
//!
//! The shape is ECIES over X25519: the sealer generates an ephemeral keypair,
//! agrees with the recipient's static public key, derives an AES-256-GCM key
//! from the shared secret with HKDF-SHA256, and ships
//! `v1.<b64(ephemeral_pub || nonce || ciphertext||tag)>`. The recipient
//! re-derives the same key from its private key.
//!
//! The property that matters is negative: the control plane holds no private
//! key, and a node's credential is stored as a sha256 hash, so nothing on the
//! CP side — a `cp.db` copy, a backup, the process memory of a compromised
//! control plane — yields the plaintext. It can still *deliver* a secret and
//! *replace* one; it cannot read one back.
//!
//! Two decisions worth stating:
//!
//! * **The entry identity is bound into the key, not only into the tag.**
//!   `aad` is `node_id \0 adapter \0 variable`, so a ciphertext sealed for
//!   `ANTHROPIC_API_KEY` on node A cannot be replayed as `GITHUB_TOKEN` on
//!   node B — the derived key would differ and the tag would not verify.
//! * **Every seal uses a fresh ephemeral key.** Re-sealing an unchanged value
//!   produces different ciphertext, so the stored blob does not leak "this
//!   secret did not change" to anyone reading the table.

use base64::{engine::general_purpose::STANDARD_NO_PAD, Engine as _};
use ring::{
    aead, hkdf,
    rand::{SecureRandom, SystemRandom},
};
use x25519_dalek::{PublicKey, StaticSecret};

/// Raw X25519 key length, in bytes.
pub const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const VERSION: &str = "v1";
/// Domain separation: this HKDF salt is what makes the derived key useless in
/// any other protocol that might agree on the same X25519 secret.
const HKDF_SALT: &[u8] = b"agentgrid-adapter-env-v1";

/// Everything that can go wrong sealing or opening a value. One variant per
/// *actionable* cause; the three ways to fail an AEAD open are deliberately
/// collapsed into [`SealError::Open`], because telling them apart is an oracle.
#[derive(Debug, thiserror::Error)]
pub enum SealError {
    #[error("system RNG failed")]
    Rng,
    #[error("not base64: {0}")]
    Base64(String),
    #[error("{0} must be {KEY_LEN} bytes")]
    KeyLength(&'static str),
    #[error("not a sealed value (missing {VERSION} prefix)")]
    NotSealed,
    #[error("sealed value is too short ({0} bytes)")]
    TooShort(usize),
    #[error("cannot open sealed value: wrong key, wrong entry, or tampered")]
    Open,
    #[error("sealed value is not UTF-8")]
    NotUtf8,
}

fn b64(bytes: &[u8]) -> String {
    STANDARD_NO_PAD.encode(bytes)
}

fn unb64(s: &str) -> Result<Vec<u8>, SealError> {
    STANDARD_NO_PAD
        .decode(s.trim())
        .map_err(|e| SealError::Base64(e.to_string()))
}

fn random_bytes<const N: usize>() -> Result<[u8; N], SealError> {
    let mut buf = [0u8; N];
    SystemRandom::new()
        .fill(&mut buf)
        .map_err(|_| SealError::Rng)?;
    Ok(buf)
}

fn parse_key<const N: usize>(s: &str, what: &'static str) -> Result<[u8; N], SealError> {
    let bytes = unb64(s)?;
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| SealError::KeyLength(what))
}

/// Generate a node keypair as `(public, private)`, both base64.
pub fn generate_keypair() -> Result<(String, String), SealError> {
    let secret = StaticSecret::from(random_bytes::<KEY_LEN>()?);
    let public = PublicKey::from(&secret);
    Ok((b64(public.as_bytes()), b64(&secret.to_bytes())))
}

/// Recompute the public half from a stored private key, so a node that lost
/// its registration can re-announce the *same* key instead of minting a new
/// one — which would strand every secret already sealed to it.
pub fn public_key_from_secret(secret_b64: &str) -> Result<String, SealError> {
    let bytes: [u8; KEY_LEN] = parse_key(secret_b64, "private key")?;
    Ok(b64(PublicKey::from(&StaticSecret::from(bytes)).as_bytes()))
}

/// The associated data that binds a blob to one entry on one node.
///
/// Both sides must build it the same way; keeping the format here rather than
/// in the control plane is what makes that possible.
pub fn entry_aad(node_id: &str, adapter: &str, key: &str) -> String {
    format!("{node_id}\u{0}{adapter}\u{0}{key}")
}

fn derive_key(shared: &[u8], aad: &str) -> Result<[u8; KEY_LEN], SealError> {
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, HKDF_SALT).extract(shared);
    // `expand` borrows both the info list and the PRK for the lifetime of the
    // returned Okm, so the list has to be a named local rather than a
    // temporary inside the call.
    let info: [&[u8]; 1] = [aad.as_bytes()];
    let okm = prk
        .expand(&info, hkdf::HKDF_SHA256)
        .map_err(|_| SealError::Rng)?;
    let mut key = [0u8; KEY_LEN];
    okm.fill(&mut key).map_err(|_| SealError::Rng)?;
    Ok(key)
}

fn aead_key(key: &[u8; KEY_LEN]) -> Result<aead::LessSafeKey, SealError> {
    aead::UnboundKey::new(&aead::AES_256_GCM, key)
        .map(aead::LessSafeKey::new)
        .map_err(|_| SealError::Rng)
}

/// Seal `plaintext` for the holder of `node_public`, bound to `aad`.
pub fn seal(node_public_b64: &str, plaintext: &str, aad: &str) -> Result<String, SealError> {
    let peer_bytes: [u8; KEY_LEN] = parse_key(node_public_b64, "public key")?;
    let eph_secret = StaticSecret::from(random_bytes::<KEY_LEN>()?);
    let eph_public = PublicKey::from(&eph_secret);
    let shared = eph_secret.diffie_hellman(&PublicKey::from(peer_bytes));
    let key = derive_key(shared.as_bytes(), aad)?;

    let nonce = random_bytes::<NONCE_LEN>()?;
    let mut buf = plaintext.as_bytes().to_vec();
    aead_key(&key)?
        .seal_in_place_append_tag(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(aad.as_bytes()),
            &mut buf,
        )
        .map_err(|_| SealError::Open)?;

    let mut out = Vec::with_capacity(KEY_LEN + NONCE_LEN + buf.len());
    out.extend_from_slice(eph_public.as_bytes());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&buf);
    Ok(format!("{VERSION}.{}", b64(&out)))
}

/// Open a blob produced by [`seal`] with the node's private key.
pub fn open(node_secret_b64: &str, sealed: &str, aad: &str) -> Result<String, SealError> {
    let body = sealed
        .strip_prefix(VERSION)
        .and_then(|s| s.strip_prefix('.'))
        .ok_or(SealError::NotSealed)?;
    let raw = unb64(body)?;
    if raw.len() < KEY_LEN + NONCE_LEN + TAG_LEN {
        return Err(SealError::TooShort(raw.len()));
    }
    let (eph, rest) = raw.split_at(KEY_LEN);
    let (nonce, ciphertext) = rest.split_at(NONCE_LEN);
    let eph_bytes: [u8; KEY_LEN] = eph
        .try_into()
        .map_err(|_| SealError::KeyLength("public key"))?;

    let secret: [u8; KEY_LEN] = parse_key(node_secret_b64, "private key")?;
    let shared = StaticSecret::from(secret).diffie_hellman(&PublicKey::from(eph_bytes));
    let key = derive_key(shared.as_bytes(), aad)?;

    let nonce_arr: [u8; NONCE_LEN] = nonce
        .try_into()
        .map_err(|_| SealError::TooShort(raw.len()))?;
    let mut buf = ciphertext.to_vec();
    let plain = aead_key(&key)?
        .open_in_place(
            aead::Nonce::assume_unique_for_key(nonce_arr),
            aead::Aad::from(aad.as_bytes()),
            &mut buf,
        )
        .map_err(|_| SealError::Open)?;
    String::from_utf8(plain.to_vec()).map_err(|_| SealError::NotUtf8)
}

#[cfg(test)]
mod tests {
    use super::*;

    const AAD: &str = "node-a\0claude\0ANTHROPIC_API_KEY";

    #[test]
    fn a_sealed_value_round_trips() {
        let (public, private) = generate_keypair().unwrap();
        let sealed = seal(&public, "sk-ant-secret", AAD).unwrap();
        assert!(sealed.starts_with("v1."), "versioned blob: {sealed}");
        assert_eq!(open(&private, &sealed, AAD).unwrap(), "sk-ant-secret");
        // The ciphertext must not contain the plaintext.
        assert!(!sealed.contains("sk-ant"));
    }

    #[test]
    fn a_second_seal_of_the_same_value_looks_different() {
        let (public, _) = generate_keypair().unwrap();
        let a = seal(&public, "same-value", AAD).unwrap();
        let b = seal(&public, "same-value", AAD).unwrap();
        assert_ne!(a, b, "fresh ephemeral key per seal");
    }

    #[test]
    fn another_nodes_key_cannot_open_it() {
        let (public_a, _private_a) = generate_keypair().unwrap();
        let (_public_b, private_b) = generate_keypair().unwrap();
        let sealed = seal(&public_a, "for-a-only", AAD).unwrap();
        assert!(matches!(
            open(&private_b, &sealed, AAD),
            Err(SealError::Open)
        ));
    }

    #[test]
    fn the_entry_identity_is_part_of_the_key() {
        let (public, private) = generate_keypair().unwrap();
        let sealed = seal(&public, "v", AAD).unwrap();
        // Same node, same value, different variable: a replayed blob must not
        // decrypt as another entry.
        let other = entry_aad("node-a", "claude", "GITHUB_TOKEN");
        assert!(open(&private, &sealed, &other).is_err());
    }

    #[test]
    fn tampering_is_detected() {
        let (public, private) = generate_keypair().unwrap();
        let sealed = seal(&public, "value", AAD).unwrap();
        let mut bytes = sealed.clone().into_bytes();
        let last = bytes.len() - 1;
        bytes[last] = if bytes[last] == b'A' { b'B' } else { b'A' };
        let broken = String::from_utf8(bytes).unwrap();
        assert!(open(&private, &broken, AAD).is_err());
    }

    #[test]
    fn garbage_is_rejected_without_panicking() {
        let (_, private) = generate_keypair().unwrap();
        for bad in ["", "v1.", "v1.!!!!", "v2.AAAA", "plain-text"] {
            assert!(open(&private, bad, AAD).is_err(), "{bad:?} must not open");
        }
        assert!(seal("not-base64", "v", AAD).is_err());
        assert!(seal("AAAA", "v", AAD).is_err(), "wrong key length");
    }

    #[test]
    fn the_public_key_can_be_recomputed_from_the_secret() {
        let (public, private) = generate_keypair().unwrap();
        assert_eq!(public_key_from_secret(&private).unwrap(), public);
    }

    #[test]
    fn aad_is_unambiguous_across_field_boundaries() {
        // Without the NUL separators "a" + "bc" and "ab" + "c" would collide.
        assert_ne!(entry_aad("node", "a", "bc"), entry_aad("node", "ab", "c"));
    }
}
