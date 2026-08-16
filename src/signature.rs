//! Whether a release was published by whoever holds the signing key.
//!
//! The checksum sidecar an update already checks answers a different question:
//! it catches a download that arrived wrong, and nothing else, because it comes
//! from the same place as the binary. Anything able to replace one can replace
//! the other. What a signature adds is that the pair was made by somebody with
//! the private key, which is not on the release host.
//!
//! What is signed is the sidecar rather than the binary. It is 65 bytes, so
//! there is no prehashing question to get wrong, and the chain is the ordinary
//! one: the signature says the sidecar is ours, and the sidecar says the binary
//! is the one the sidecar was written for. Both are checked or neither counts.
//!
//! Ed25519 over the sidecar's exact bytes, with the signature published hex
//! encoded beside it. Deliberately something `openssl` can produce and check,
//! so a release can be verified by hand without this program:
//!
//! ```text
//! curl -fsSLO https://github.com/rayfish/manymux/releases/latest/download/mm-linux-x86_64.sha256
//! curl -fsSLO https://github.com/rayfish/manymux/releases/latest/download/mm-linux-x86_64.sha256.sig
//! xxd -r -p mm-linux-x86_64.sha256.sig > sig.bin
//! openssl pkeyutl -verify -pubin -inkey release.pem -rawin \
//!     -in mm-linux-x86_64.sha256 -sigfile sig.bin
//! ```
//!
//! Verified in this process rather than by shelling out, which is what the rest
//! of [`crate::update`] does. There is no tool worth shelling out to: minisign
//! is on almost no machines, and the `openssl` macOS ships is a LibreSSL that
//! cannot do `-rawin` at all. A check that is skipped on most machines is not a
//! check, and the point of signing is the machines you did not think about.

use anyhow::{Result, bail};
use ed25519_dalek::{Signature, VerifyingKey};

/// The key releases are signed with, as the 32 raw bytes hex encoded.
///
/// Empty while there is no key yet, which switches the checking off rather than
/// failing every update: a build that refused everything because nobody had
/// generated a key would be a build nobody could update away from. Set this and
/// signing starts mattering, without any other change.
pub const RELEASE_KEY: &str = "";

/// What checking a release signature can conclude.
#[derive(Debug, PartialEq, Eq)]
pub enum Signed {
    /// The signature is the key's, over these exact bytes.
    By,
    /// Nothing to check against: no key compiled in, so no opinion.
    Unchecked,
    /// A key is compiled in and the release carries no signature. Worth saying
    /// out loud, and not worth refusing an update over while releases from
    /// before signing existed are still the newest thing published.
    Unsigned,
}

/// Check `signature` against `signed`, or say why there is no answer.
///
/// A signature that is present and wrong is an error, always. The two soft
/// answers are both about there being nothing to compare.
pub fn check(signed: &[u8], signature: Option<&str>, key: &str) -> Result<Signed> {
    if key.is_empty() {
        return Ok(Signed::Unchecked);
    }
    let key = verifying_key(key)?;
    let Some(signature) = signature else {
        return Ok(Signed::Unsigned);
    };
    let bytes: [u8; 64] = unhex(signature)
        .and_then(|raw| raw.try_into().ok())
        .ok_or_else(|| anyhow::anyhow!("a signature is 64 bytes hex encoded"))?;
    // Strict, which rejects the small-order public keys and non-canonical
    // encodings that make a signature verify under more than one key. Nothing
    // here needs the permissive one, and a release is exactly the place where
    // "verifies under some key" must not be mistaken for "verifies under ours".
    key.verify_strict(signed, &Signature::from_bytes(&bytes))
        .map_err(|e| anyhow::anyhow!("the signature is not this key's: {e}"))?;
    Ok(Signed::By)
}

fn verifying_key(key: &str) -> Result<VerifyingKey> {
    let bytes: [u8; 32] = unhex(key)
        .and_then(|raw| raw.try_into().ok())
        .ok_or_else(|| anyhow::anyhow!("the release key is 32 bytes hex encoded"))?;
    match VerifyingKey::from_bytes(&bytes) {
        Ok(key) => Ok(key),
        Err(e) => bail!("the release key is not a usable one: {e}"),
    }
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    if !text.len().is_multiple_of(2) {
        return None;
    }
    text.as_bytes()
        .chunks(2)
        .map(|pair| {
            let digits = std::str::from_utf8(pair).ok()?;
            u8::from_str_radix(digits, 16).ok()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key and a signature made by `openssl pkeyutl -sign -rawin`, so what
    /// this agrees with is another implementation rather than itself. The
    /// message is a checksum sidecar exactly as a release publishes one,
    /// trailing newline included.
    const KEY: &str = "8c070fe3eb9ec167a9870c5268031ff0178c2a3ef17a25ccef0ac0e6339ed099";
    const SIDECAR: &[u8] =
        b"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  mm-linux-x86_64\n";
    const SIGNATURE: &str = "43a28484fdefe995ababc4fa48837f2aa1de4ff73bb491a98c0520e0a7c5fff1\
                             7db72cd29d4cb22a14b6b647d958207ebaf50d6a3bef3ead4c6a75c1319b860f";

    #[test]
    fn a_signature_openssl_made_verifies() {
        assert_eq!(check(SIDECAR, Some(SIGNATURE), KEY).unwrap(), Signed::By);
    }

    /// The whole point: a sidecar somebody changed no longer matches, so the
    /// checksum it names cannot be swapped for one covering a different binary.
    #[test]
    fn a_sidecar_that_was_edited_does_not_verify() {
        let mut tampered = SIDECAR.to_vec();
        tampered[0] = b'f';
        let refused = check(&tampered, Some(SIGNATURE), KEY);
        assert!(refused.is_err(), "{refused:?}");
    }

    #[test]
    fn another_keys_signature_does_not_verify() {
        // The same signature offered against a different valid key.
        let other = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";
        let refused = check(SIDECAR, Some(SIGNATURE), other);
        assert!(refused.is_err(), "{refused:?}");
    }

    #[test]
    fn a_signature_that_is_not_one_is_refused_rather_than_ignored() {
        assert!(check(SIDECAR, Some("nonsense"), KEY).is_err());
        assert!(check(SIDECAR, Some(""), KEY).is_err());
        // Right length, wrong bytes.
        assert!(check(SIDECAR, Some(&"ab".repeat(64)), KEY).is_err());
    }

    /// The two ways there is nothing to compare, which are not failures.
    #[test]
    fn nothing_to_check_against_is_not_a_refusal() {
        assert_eq!(
            check(SIDECAR, Some(SIGNATURE), "").unwrap(),
            Signed::Unchecked,
            "no key compiled in, so no opinion"
        );
        assert_eq!(
            check(SIDECAR, None, KEY).unwrap(),
            Signed::Unsigned,
            "a release published before signing existed"
        );
    }

    #[test]
    fn a_key_that_is_not_a_key_is_a_failure_rather_than_a_shrug() {
        assert!(check(SIDECAR, Some(SIGNATURE), "abcd").is_err());
        assert!(check(SIDECAR, Some(SIGNATURE), &"zz".repeat(32)).is_err());
    }
}
