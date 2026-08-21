//! H3: sr25519 + ML-DSA-44 hybrid signature scheme.
//!
//! This legacy-named API now delegates key derivation, message binding, and
//! composite signing/verification to [`pqhybridsign::H3`]. The library's
//! suite-separated HKDF includes the label's trailing NUL byte, so a master
//! seed derives different H3 keys than the former in-tree fork engine.

use pqhybridsign::{pq::MlDsa44, H3};
use pqhybridsign::{component::PqScheme, suite::Suite};
use rand_core::CryptoRngCore;
use zeroize::Zeroize;

use super::mldsa44::{self, Config};
use crate::{HybridSignatureScheme, Result};

/// Length in bytes of an H3 public key.
pub const HYBRID_PK_LEN: usize = mldsa44::HYBRID_PK_LEN;
/// Length in bytes of an H3 secret key.
pub const HYBRID_SK_LEN: usize = mldsa44::HYBRID_SK_LEN;
/// Length in bytes of an H3 signature.
pub const HYBRID_SIG_LEN: usize = mldsa44::HYBRID_SIG_LEN;
/// Length in bytes of H3's ML-DSA-44 public-key component.
#[doc(hidden)]
pub const ML_DSA_PUBLIC_KEY_LEN: usize = MlDsa44::PUBLIC_KEY_LEN;
/// Length in bytes of H3's ML-DSA-44 signature component.
#[doc(hidden)]
pub const ML_DSA_SIGNATURE_LEN: usize = MlDsa44::SIGNATURE_LEN;

const _: () = {
	assert!(H3::PUBLIC_KEY_LEN == HYBRID_PK_LEN);
	assert!(H3::SECRET_KEY_LEN == HYBRID_SK_LEN);
	assert!(H3::SIGNATURE_LEN == HYBRID_SIG_LEN);
};

/// Fixed-size H3 public key.
pub type PublicKey = mldsa44::PublicKey<Sr25519MlDsa44>;
/// H3 secret key, zeroized on drop.
pub type SecretKey = mldsa44::SecretKey<Sr25519MlDsa44>;
/// Fixed-size H3 signature.
pub type Signature = mldsa44::Signature<Sr25519MlDsa44>;

/// Zero-sized type implementing [`HybridSignatureScheme`] for H3.
pub struct Sr25519MlDsa44;

impl Config for Sr25519MlDsa44 {
	type LibrarySuite = H3;

	fn classical_public_is_valid(bytes: &[u8]) -> bool {
		schnorrkel::PublicKey::from_bytes(bytes).is_ok()
	}

	fn classical_public_from_secret(bytes: &[u8]) -> Option<[u8; 32]> {
		let mut secret = schnorrkel::SecretKey::from_bytes(bytes).ok()?;
		let public = secret.to_public().to_bytes();
		secret.zeroize();
		Some(public)
	}
}

impl HybridSignatureScheme for Sr25519MlDsa44 {
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

/// Signs the H3 VRF binding message with only the ML-DSA-44 component.
///
/// Kept for the legacy H3 BABE wrapper, whose proof format carries the native
/// sr25519 VRF proof separately and therefore must not emit a full H3 signature.
#[doc(hidden)]
pub fn sign_ml_dsa_component(secret: &SecretKey, message: &[u8]) -> [u8; ML_DSA_SIGNATURE_LEN] {
	let (_, pq_secret) = secret.split_components();
	let mut signature = [0u8; ML_DSA_SIGNATURE_LEN];
	MlDsa44::sign_deterministic(pq_secret, message, b"", &mut signature)
		.expect("stored H3 key and exact ML-DSA-44 signature buffer cannot fail");
	signature
}

/// Verifies the ML-DSA-44 component of an H3 VRF binding proof.
#[doc(hidden)]
pub fn verify_ml_dsa_component(public: &[u8], message: &[u8], signature: &[u8]) -> bool {
	bool::from(MlDsa44::verify(public, message, signature))
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
		let (secret, public) = Sr25519MlDsa44::generate(&mut OsRng);
		let signature = Sr25519MlDsa44::sign(&secret, b"hello quip", b"h3", &mut OsRng);
		assert!(Sr25519MlDsa44::verify(&public, b"hello quip", b"h3", &signature));

		let mut tampered = signature.to_bytes();
		tampered[100] ^= 1;
		let tampered = Signature::from_bytes(&tampered).expect("fixed-size signature");
		assert!(!Sr25519MlDsa44::verify(&public, b"hello quip", b"h3", &tampered));
	}

	#[test]
	fn deterministic_nonce_is_bound_by_h3() {
		let (secret, _) = Sr25519MlDsa44::from_seed_slice(&[7u8; 32]).expect("seed");
		let first = Sr25519MlDsa44::sign_deterministic(&secret, b"message", b"context", b"a");
		let second = Sr25519MlDsa44::sign_deterministic(&secret, b"message", b"context", b"b");
		assert_ne!(first.as_ref(), second.as_ref());
	}

	#[test]
	fn public_key_recovers_from_serialized_secret() {
		let (secret, public) = Sr25519MlDsa44::from_seed_slice(&[9u8; 32]).expect("seed");
		let reparsed = SecretKey::from_bytes(secret.to_bytes().as_ref()).expect("secret");
		assert_eq!(Sr25519MlDsa44::public(&reparsed).as_ref(), public.as_ref());
	}

	#[test]
	fn matches_pqhybridsign_h3_golden_vector() {
		let vector: Vector = serde_json::from_str(include_str!("../../tests/vectors/h3.json"))
			.expect("valid H3 vector");
		let seed = hex::decode(vector.master_seed_hex).expect("hex seed");
		let ctx = hex::decode(vector.ctx_hex).expect("hex context");
		let msg = hex::decode(vector.msg_hex).expect("hex message");
		let nonce = hex::decode(vector.nonce_hex).expect("hex nonce");
		let (secret, public) = Sr25519MlDsa44::from_seed_slice(&seed).expect("keygen");
		let signature = Sr25519MlDsa44::sign_deterministic(&secret, &msg, &ctx, &nonce);

		assert_eq!(hex::encode(public.as_ref()), vector.public_key_hex);
		assert_eq!(hex::encode(signature.as_ref()), vector.signature_hex);
		assert!(Sr25519MlDsa44::verify(&public, &msg, &ctx, &signature));
	}
}
