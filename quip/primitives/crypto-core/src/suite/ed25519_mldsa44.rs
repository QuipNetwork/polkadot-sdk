//! H1: ed25519 + ML-DSA-44 hybrid signature scheme.
//!
//! This legacy-named API now delegates key derivation, message binding, and
//! composite signing/verification to [`pqhybridsign::EdMl44`]. The library's
//! suite-separated HKDF includes the label's trailing NUL byte, so a master
//! seed derives different H1 keys than the former in-tree fork engine.

use ed25519_zebra::VerificationKey;
use pqhybridsign::{classical::Ed25519, EdMl44};
use pqhybridsign::{component::ClassicalScheme, suite::Suite};
use rand_core::CryptoRngCore;
use zeroize::Zeroizing;

use super::mldsa44::{self, Config};
use crate::{HybridSignatureScheme, Result};

/// Length in bytes of an H1 public key.
pub const HYBRID_PK_LEN: usize = mldsa44::HYBRID_PK_LEN;
/// Length in bytes of an H1 secret key.
pub const HYBRID_SK_LEN: usize = mldsa44::HYBRID_SK_LEN;
/// Length in bytes of an H1 signature.
pub const HYBRID_SIG_LEN: usize = mldsa44::HYBRID_SIG_LEN;

const _: () = {
	assert!(EdMl44::PUBLIC_KEY_LEN == HYBRID_PK_LEN);
	assert!(EdMl44::SECRET_KEY_LEN == HYBRID_SK_LEN);
	assert!(EdMl44::SIGNATURE_LEN == HYBRID_SIG_LEN);
};

/// Fixed-size H1 public key.
pub type PublicKey = mldsa44::PublicKey<Ed25519MlDsa44>;
/// H1 secret key, zeroized on drop.
pub type SecretKey = mldsa44::SecretKey<Ed25519MlDsa44>;
/// Fixed-size H1 signature.
pub type Signature = mldsa44::Signature<Ed25519MlDsa44>;

/// Zero-sized type implementing [`HybridSignatureScheme`] for H1.
pub struct Ed25519MlDsa44;

impl Config for Ed25519MlDsa44 {
	type LibrarySuite = EdMl44;

	fn classical_public_is_valid(bytes: &[u8]) -> bool {
		let Ok(bytes): core::result::Result<[u8; 32], _> = bytes.try_into() else {
			return false;
		};
		VerificationKey::try_from(bytes).is_ok()
	}

	fn classical_public_from_secret(bytes: &[u8]) -> Option<[u8; 32]> {
		let seed: &[u8; 32] = bytes.get(..32)?.try_into().ok()?;
		let mut derived_secret = Zeroizing::new([0u8; 64]);
		let mut public = [0u8; 32];
		Ed25519::derive(seed, derived_secret.as_mut(), &mut public).ok()?;
		if derived_secret.as_ref() != bytes {
			return None;
		}
		Some(public)
	}
}

impl HybridSignatureScheme for Ed25519MlDsa44 {
	type PublicKey = PublicKey;
	type SecretKey = SecretKey;
	type Signature = Signature;

	fn public_key_len() -> usize {
		HYBRID_PK_LEN
	}

	fn secret_key_len() -> usize {
		HYBRID_SK_LEN
	}

	fn signature_max_len() -> usize {
		HYBRID_SIG_LEN
	}

	fn generate(rng: &mut impl CryptoRngCore) -> (Self::SecretKey, Self::PublicKey) {
		mldsa44::generate::<Self>(rng)
	}

	fn from_seed_slice(seed: &[u8]) -> Result<(Self::SecretKey, Self::PublicKey)> {
		mldsa44::from_seed_slice::<Self>(seed)
	}

	fn public_key_from_bytes(bytes: &[u8]) -> Result<Self::PublicKey> {
		PublicKey::from_bytes(bytes)
	}

	fn secret_key_from_bytes(bytes: &[u8]) -> Result<Self::SecretKey> {
		SecretKey::from_bytes(bytes)
	}

	fn signature_from_bytes(bytes: &[u8]) -> Result<Self::Signature> {
		Signature::from_bytes(bytes)
	}

	fn public(secret: &Self::SecretKey) -> Self::PublicKey {
		mldsa44::public::<Self>(secret)
	}

	fn sign(
		secret: &Self::SecretKey,
		msg: &[u8],
		ctx: &[u8],
		rng: &mut impl CryptoRngCore,
	) -> Self::Signature {
		mldsa44::sign::<Self>(secret, msg, ctx, rng)
	}

	fn sign_deterministic(
		secret: &Self::SecretKey,
		msg: &[u8],
		ctx: &[u8],
		nonce: &[u8],
	) -> Self::Signature {
		mldsa44::sign_deterministic::<Self>(secret, msg, ctx, nonce)
	}

	fn verify(
		public: &Self::PublicKey,
		msg: &[u8],
		ctx: &[u8],
		signature: &Self::Signature,
	) -> bool {
		mldsa44::verify::<Self>(public, msg, ctx, signature)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use rand_core::OsRng;
	use serde::Deserialize;

	#[derive(Deserialize)]
	struct Vector {
		master_seed_hex: String,
		ctx_hex: String,
		msg_hex: String,
		nonce_hex: String,
		public_key_hex: String,
		signature_hex: String,
	}

	#[test]
	fn roundtrip_and_tamper_rejection() {
		let (secret, public) = Ed25519MlDsa44::generate(&mut OsRng);
		let signature = Ed25519MlDsa44::sign(&secret, b"hello quip", b"h1", &mut OsRng);
		assert!(Ed25519MlDsa44::verify(&public, b"hello quip", b"h1", &signature));

		let mut tampered = signature.to_bytes();
		tampered[100] ^= 1;
		let tampered = Signature::from_bytes(&tampered).expect("fixed-size signature");
		assert!(!Ed25519MlDsa44::verify(&public, b"hello quip", b"h1", &tampered));
	}

	#[test]
	fn deterministic_nonce_is_ignored_by_h1() {
		let (secret, _) = Ed25519MlDsa44::from_seed_slice(&[7u8; 32]).expect("seed");
		let first = Ed25519MlDsa44::sign_deterministic(&secret, b"message", b"context", b"a");
		let second = Ed25519MlDsa44::sign_deterministic(&secret, b"message", b"context", b"b");
		assert_eq!(first.as_ref(), second.as_ref());
	}

	#[test]
	fn public_key_recovers_from_serialized_secret() {
		let (secret, public) = Ed25519MlDsa44::from_seed_slice(&[9u8; 32]).expect("seed");
		let reparsed = SecretKey::from_bytes(secret.to_bytes().as_ref()).expect("secret");
		assert_eq!(Ed25519MlDsa44::public(&reparsed).as_ref(), public.as_ref());
	}

	#[test]
	fn matches_pqhybridsign_h1_golden_vector() {
		let vector: Vector = serde_json::from_str(include_str!("../../tests/vectors/h1.json"))
			.expect("valid H1 vector");
		let seed = hex::decode(vector.master_seed_hex).expect("hex seed");
		let ctx = hex::decode(vector.ctx_hex).expect("hex context");
		let msg = hex::decode(vector.msg_hex).expect("hex message");
		let nonce = hex::decode(vector.nonce_hex).expect("hex nonce");
		let (secret, public) = Ed25519MlDsa44::from_seed_slice(&seed).expect("keygen");
		let signature = Ed25519MlDsa44::sign_deterministic(&secret, &msg, &ctx, &nonce);

		assert_eq!(hex::encode(public.as_ref()), vector.public_key_hex);
		assert_eq!(hex::encode(signature.as_ref()), vector.signature_hex);
		assert!(Ed25519MlDsa44::verify(&public, &msg, &ctx, &signature));
	}
}
