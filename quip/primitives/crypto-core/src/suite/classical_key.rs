//! Validity of the classical half of a hybrid public key (GregoAI wp-2 G-002).
//!
//! The component libraries accept some public keys that are not keys: the
//! classical signature check passes for them with a fixed value that needs
//! no secret, for every message. A hybrid key with such a half is
//! post-quantum only while claiming to be hybrid, so the suites refuse it
//! here, at parse time, which every verification surface goes through.

use curve25519_dalek::{edwards::CompressedEdwardsY, ristretto::RistrettoPoint, traits::Identity};

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

/// An ed25519 public key is valid when zebra accepts it and it is not a
/// small-order point, in any of its encodings.
///
/// zebra follows ZIP-215: it decodes non-canonical `y` (`y + p` for
/// `y < 19`) and the sign bit on `x = 0`, and it checks the cofactored
/// equation `[8](s·B - k·A - R) == 0`. For `A` in the torsion subgroup
/// `[8]·k·A` vanishes, so `R = identity, s = 0` verifies for any message.
/// `decompress` reduces `y` the same way zebra does, so `is_small_order` sees
/// every encoding zebra accepts.
pub(crate) fn ed25519_public_is_valid(bytes: &[u8]) -> bool {
	let Ok(bytes): core::result::Result<[u8; 32], _> = bytes.try_into() else {
		return false;
	};
	ed25519_zebra::VerificationKey::try_from(bytes).is_ok() &&
		CompressedEdwardsY(bytes)
			.decompress()
			.is_some_and(|point| !point.is_small_order())
}

#[cfg(test)]
mod tests {
	use super::*;
	use curve25519_dalek::{
		edwards::EdwardsPoint, ristretto::CompressedRistretto, traits::IsIdentity,
	};

	/// Every 32-byte string that decodes to a small-order Edwards point.
	///
	/// The torsion subgroup is cyclic of order 8, so it is `{k·T | k = 0..8}`
	/// for a point `T` of order 8. Each of its points has the canonical
	/// encoding, the same `y` with the other sign bit (which decodes for
	/// `x = 0` too), and `y + p` when that fits in 255 bits (only `y < 19`).
	fn small_order_ed25519_encodings() -> std::vec::Vec<[u8; 32]> {
		// An order-8 point (libsodium's small-order blocklist).
		let mut t_bytes = [0u8; 32];
		hex::decode_to_slice(
			"c7176a703d4dd84fba3c0b760d10670f2a2053fa2c39ccc64ec7fd7792ac037a",
			&mut t_bytes,
		)
		.expect("hex");
		let t = CompressedEdwardsY(t_bytes).decompress().expect("order-8 point decodes");
		let four_t = (t + t) + (t + t);
		assert!(!four_t.is_identity(), "T has order 8, not 4");
		assert!((four_t + four_t).is_identity(), "T has order 8");

		let mut point = EdwardsPoint::identity();
		let mut out = std::vec::Vec::new();
		for _ in 0..8 {
			let mut y = point.compress().to_bytes();
			y[31] &= 0x7f;
			let mut ys = std::vec![y];
			if y[1..].iter().all(|b| *b == 0) && y[0] < 19 {
				// y + p = y + 2^255 - 19, little-endian.
				let mut plus_p = [0xffu8; 32];
				plus_p[0] = 0xed + y[0];
				plus_p[31] = 0x7f;
				ys.push(plus_p);
			}
			for y in ys {
				for sign in [0u8, 0x80] {
					let mut enc = y;
					enc[31] |= sign;
					let small =
						CompressedEdwardsY(enc).decompress().is_some_and(|p| p.is_small_order());
					if small && !out.contains(&enc) {
						out.push(enc);
					}
				}
			}
			point += t;
		}
		out
	}

	#[test]
	fn zebra_accepts_all_fourteen_small_order_encodings_and_the_check_refuses_them() {
		let encodings = small_order_ed25519_encodings();
		assert_eq!(encodings.len(), 14);
		for enc in encodings {
			assert!(
				ed25519_zebra::VerificationKey::try_from(enc).is_ok(),
				"zebra accepts {}, so this is a real gap",
				hex::encode(enc)
			);
			assert!(!ed25519_public_is_valid(&enc), "{} is small-order", hex::encode(enc));
		}
	}

	#[test]
	fn ed25519_accepts_a_real_key_and_rejects_what_zebra_rejects() {
		let real = ed25519_zebra::SigningKey::from([7u8; 32]);
		let real: [u8; 32] = ed25519_zebra::VerificationKey::from(&real).into();
		assert!(ed25519_public_is_valid(&real));
		// The first small `y` that is not on the curve: zebra rejects it.
		let off_curve = (2u8..=255)
			.map(|y| {
				let mut enc = [0u8; 32];
				enc[0] = y;
				enc
			})
			.find(|enc| ed25519_zebra::VerificationKey::try_from(*enc).is_err())
			.expect("some small y is off the curve");
		assert!(!ed25519_public_is_valid(&off_curve));
		assert!(!ed25519_public_is_valid(&[0u8; 31]));
	}

	#[test]
	fn the_ristretto_identity_is_the_32_zero_bytes_and_decodes() {
		assert_eq!(RistrettoPoint::identity().compress().to_bytes(), [0u8; 32]);
		assert_eq!(CompressedRistretto([0u8; 32]).decompress(), Some(RistrettoPoint::identity()));
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
