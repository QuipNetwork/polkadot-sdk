//! H4: sr25519 + FN-DSA-512 hybrid signature scheme.

use pqhybridsign::H4;
use pqhybridsign::suite::DeltaSuite;
use rand_core::CryptoRngCore;
use zeroize::Zeroize;

use super::fndsa512::{self, Config};
use crate::{HybridSignatureScheme, Result};

/// Length in bytes of an H4 public key.
pub const HYBRID_PK_LEN: usize = fndsa512::HYBRID_PK_LEN;
/// Length in bytes of an H4 secret key.
pub const HYBRID_SK_LEN: usize = fndsa512::HYBRID_SK_LEN;
/// Maximum length in bytes of an H4 signature.
pub const HYBRID_SIG_LEN: usize = fndsa512::HYBRID_SIG_LEN;

const _: () = {
	assert!(H4::PUBLIC_KEY_LEN == HYBRID_PK_LEN);
	assert!(H4::SECRET_KEY_LEN == HYBRID_SK_LEN);
	assert!(H4::MAX_SIGNATURE_LEN == HYBRID_SIG_LEN);
};

/// Fixed-size H4 public key.
pub type PublicKey = fndsa512::PublicKey<Sr25519FnDsa512>;
/// H4 secret key, zeroized on drop.
pub type SecretKey = fndsa512::SecretKey<Sr25519FnDsa512>;
/// Fixed-size, zero-padded H4 signature.
pub type Signature = fndsa512::Signature<Sr25519FnDsa512>;

/// Zero-sized type implementing [`HybridSignatureScheme`] for H4.
pub struct Sr25519FnDsa512;

impl Config for Sr25519FnDsa512 {
	type LibrarySuite = H4;

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

impl HybridSignatureScheme for Sr25519FnDsa512 {
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
		fndsa512::generate::<Self>(rng)
	}

	fn from_seed_slice(seed: &[u8]) -> Result<(Self::SecretKey, Self::PublicKey)> {
		fndsa512::from_seed_slice::<Self>(seed)
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
		fndsa512::public::<Self>(secret)
	}

	fn sign(
		secret: &Self::SecretKey,
		msg: &[u8],
		ctx: &[u8],
		rng: &mut impl CryptoRngCore,
	) -> Self::Signature {
		fndsa512::sign::<Self>(secret, msg, ctx, rng)
	}

	fn sign_deterministic(
		secret: &Self::SecretKey,
		msg: &[u8],
		ctx: &[u8],
		nonce: &[u8],
	) -> Self::Signature {
		fndsa512::sign_deterministic::<Self>(secret, msg, ctx, nonce)
	}

	fn verify(
		public: &Self::PublicKey,
		msg: &[u8],
		ctx: &[u8],
		signature: &Self::Signature,
	) -> bool {
		fndsa512::verify::<Self>(public, msg, ctx, signature)
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
		let (secret, public) = Sr25519FnDsa512::generate(&mut OsRng);
		let signature = Sr25519FnDsa512::sign(&secret, b"hello quip", b"h4", &mut OsRng);
		assert!(Sr25519FnDsa512::verify(&public, b"hello quip", b"h4", &signature));

		let mut tampered = signature.to_bytes();
		tampered[100] ^= 1;
		let tampered = Signature::from_bytes(&tampered).expect("fixed-size signature");
		assert!(!Sr25519FnDsa512::verify(&public, b"hello quip", b"h4", &tampered));
	}

	#[test]
	fn deterministic_signing_is_reproducible() {
		let (secret, _) = Sr25519FnDsa512::from_seed_slice(&[7u8; 32]).expect("seed");
		let first = Sr25519FnDsa512::sign_deterministic(&secret, b"message", b"context", b"nonce");
		let second = Sr25519FnDsa512::sign_deterministic(&secret, b"message", b"context", b"nonce");
		assert_eq!(first.as_ref(), second.as_ref());
	}

	#[test]
	fn public_key_recovers_from_serialized_secret() {
		let (secret, public) = Sr25519FnDsa512::from_seed_slice(&[9u8; 32]).expect("seed");
		let reparsed = SecretKey::from_bytes(secret.to_bytes().as_ref()).expect("secret");
		assert_eq!(Sr25519FnDsa512::public(&reparsed).as_ref(), public.as_ref());
	}

	#[test]
	fn matches_pqhybridsign_h4_golden_vector() {
		let vector: Vector = serde_json::from_str(include_str!("../../tests/vectors/h4.json"))
			.expect("valid H4 vector");
		let seed = hex::decode(vector.master_seed_hex).expect("hex seed");
		let ctx = hex::decode(vector.ctx_hex).expect("hex context");
		let msg = hex::decode(vector.msg_hex).expect("hex message");
		let nonce = hex::decode(vector.nonce_hex).expect("hex nonce");
		let (secret, public) = Sr25519FnDsa512::from_seed_slice(&seed).expect("keygen");
		let signature = Sr25519FnDsa512::sign_deterministic(&secret, &msg, &ctx, &nonce);

		assert_eq!(hex::encode(public.as_ref()), vector.public_key_hex);
		assert_eq!(hex::encode(signature.wire_bytes()), vector.signature_hex);
		assert!(Sr25519FnDsa512::verify(&public, &msg, &ctx, &signature));
	}
}
