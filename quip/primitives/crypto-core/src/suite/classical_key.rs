//! Validity of the classical half of a hybrid public key (GregoAI wp-2 G-002).
//!
//! The component libraries accept some public keys that are not keys: the
//! classical signature check passes for them with a fixed value that needs
//! no secret, for every message. A hybrid key with such a half is
//! post-quantum only while claiming to be hybrid, so the suites refuse it
//! here, at parse time, which every verification surface goes through.

use curve25519_dalek::ristretto::RistrettoPoint;
use curve25519_dalek::traits::Identity;

/// An sr25519 public key is valid when it decodes as a Ristretto point and is
/// not the identity.
///
/// Ristretto is a prime-order group, so the identity is its only degenerate
/// point, and it has exactly one encoding (32 zero bytes): decoding rejects
/// every non-canonical encoding. Schnorrkel accepts `(R, s)` when
/// `R == s·B - k·A`; with `A` the identity that is `R == s·B`, which
/// `R = B, s = 1` satisfies for any message.
pub(crate) fn sr25519_public_is_valid(bytes: &[u8]) -> bool {
	schnorrkel::PublicKey::from_bytes(bytes)
		.is_ok_and(|public| *public.as_point() != RistrettoPoint::identity())
}

#[cfg(test)]
mod tests {
	use super::*;
	use curve25519_dalek::ristretto::CompressedRistretto;

	#[test]
	fn the_ristretto_identity_is_the_32_zero_bytes_and_decodes() {
		assert_eq!(RistrettoPoint::identity().compress().to_bytes(), [0u8; 32]);
		assert_eq!(
			CompressedRistretto([0u8; 32]).decompress(),
			Some(RistrettoPoint::identity())
		);
		assert!(
			schnorrkel::PublicKey::from_bytes(&[0u8; 32]).is_ok(),
			"schnorrkel accepts the identity, so the check below is needed"
		);
	}

	#[test]
	fn sr25519_rejects_the_identity_and_accepts_a_real_key() {
		assert!(!sr25519_public_is_valid(&[0u8; 32]));
		let real = schnorrkel::MiniSecretKey::from_bytes(&[7u8; 32])
			.expect("32 bytes")
			.expand_to_public(schnorrkel::ExpansionMode::Ed25519);
		assert!(sr25519_public_is_valid(&real.to_bytes()));
	}

	#[test]
	fn sr25519_still_rejects_what_schnorrkel_rejects() {
		// A non-canonical Ristretto encoding: the field element p - 1 + 2 = p + 1.
		let mut bad = [0xffu8; 32];
		bad[0] = 0xee;
		bad[31] = 0x7f;
		assert!(schnorrkel::PublicKey::from_bytes(&bad).is_err());
		assert!(!sr25519_public_is_valid(&bad));
		assert!(!sr25519_public_is_valid(&[0u8; 31]));
	}
}
