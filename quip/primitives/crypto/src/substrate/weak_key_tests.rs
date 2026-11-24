//! G-002 (GregoAI wp-2): a hybrid public key whose classical half is a
//! degenerate point is not a key.
//!
//! For each such point the classical signature check is satisfied by a fixed
//! value that needs no secret, for any message:
//! - sr25519 (H3, H4): the Ristretto identity, `A=0`. Schnorrkel accepts `(R,s)` when `R==s·B-k·A`,
//!   which for `A=0` is `R==s·B`, so `R=B, s=1` verifies.
//! - ed25519 (H1, H2): any small-order `A`, in any of the 14 encodings zebra accepts. zebra checks
//!   the cofactored equation `[8](s·B-k·A-R)==0`; `[8]·k·A` vanishes, so `R=0, s=0` (`R` the
//!   identity) verifies.
//!
//! The tests splice that classical half into an honest signature: the
//! post-quantum half signs `M'`, which depends only on the suite label,
//! context and message, so it stays valid. A key like this is post-quantum
//! only while it claims to be hybrid. Every Substrate verification surface
//! must refuse it.

use curve25519_dalek::{
	constants::RISTRETTO_BASEPOINT_COMPRESSED,
	edwards::{CompressedEdwardsY, EdwardsPoint},
	traits::{Identity, IsIdentity},
};
use sp_core::{
	crypto::Pair as PairT,
	proof_of_possession::{ProofOfPossessionGenerator, ProofOfPossessionVerifier},
};

use super::{ed25519_fndsa512, ed25519_mldsa44, sr25519_fndsa512, sr25519_mldsa44};

const MSG: &[u8] = b"G-002 weak classical half";
const OWNER: &[u8] = b"G-002 proof-of-possession owner";

/// The sr25519 forgery for `A = identity`: `R = B`, `s = 1`, with schnorrkel's
/// marker bit set in the last byte.
pub(crate) fn sr25519_identity_forgery() -> [u8; 64] {
	let mut sig = [0u8; 64];
	sig[..32].copy_from_slice(RISTRETTO_BASEPOINT_COMPRESSED.as_bytes());
	sig[32] = 1;
	sig[63] |= 0x80;
	sig
}

/// The ed25519 forgery for a small-order `A`: `R = identity`, `s = 0`.
pub(crate) fn ed25519_small_order_forgery() -> [u8; 64] {
	let mut sig = [0u8; 64];
	sig[0] = 1;
	sig
}

/// Every 32-byte string that decodes to a small-order Edwards point: the
/// eight torsion points `k·T` for an order-8 `T`, each with its canonical
/// `y`, the other sign bit, and `y + p` where that fits (`y < 19`). The same
/// enumeration is checked against zebra in crypto-core's `classical_key`.
pub(crate) fn small_order_ed25519_encodings() -> Vec<[u8; 32]> {
	// An order-8 point (libsodium's small-order blocklist).
	let mut t_bytes = [0u8; 32];
	let t_hex = "c7176a703d4dd84fba3c0b760d10670f2a2053fa2c39ccc64ec7fd7792ac037a";
	for (i, byte) in t_bytes.iter_mut().enumerate() {
		*byte = u8::from_str_radix(&t_hex[2 * i..2 * i + 2], 16).expect("hex");
	}
	let t = CompressedEdwardsY(t_bytes).decompress().expect("order-8 point decodes");
	let four_t = (t + t) + (t + t);
	assert!(!four_t.is_identity() && (four_t + four_t).is_identity(), "T has order 8");

	let mut point = EdwardsPoint::identity();
	let mut out = Vec::new();
	for _ in 0..8 {
		let mut y = point.compress().to_bytes();
		y[31] &= 0x7f;
		let mut ys = vec![y];
		if y[1..].iter().all(|b| *b == 0) && y[0] < 19 {
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
	assert_eq!(out.len(), 14);
	out
}

pub(crate) fn hex(bytes: &[u8]) -> String {
	bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Replaces the leading bytes of an honest public key or signature.
pub(crate) fn splice<T: AsMut<[u8]> + Clone>(honest: &T, classical: &[u8]) -> T {
	let mut out = honest.clone();
	out.as_mut()[..classical.len()].copy_from_slice(classical);
	out
}

/// The verification surfaces that accept a forgery under a public key whose
/// classical half is `weak_pk`: `Pair::verify`, `RuntimePublic::verify` (what
/// the runtime and app-crypto call) and the proof-of-possession verifier.
/// Empty means the key is refused everywhere.
pub(crate) fn accepted_forgeries<P>(
	suite: &str,
	weak_pk: [u8; 32],
	forged_classical_sig: [u8; 64],
) -> Vec<String>
where
	P: PairT + ProofOfPossessionGenerator + ProofOfPossessionVerifier,
	P::Public: AsMut<[u8]> + sp_application_crypto::RuntimePublic<Signature = P::Signature>,
	P::Signature: AsMut<[u8]> + Clone,
	P::ProofOfPossession: AsMut<[u8]> + Clone,
{
	let mut pair = P::from_seed_slice(&[0x5a; 32]).expect("32-byte seed");
	let honest_public = pair.public();
	let honest_sig = pair.sign(MSG);
	let honest_pop = pair.generate_proof_of_possession(OWNER);
	assert!(P::verify(&honest_sig, MSG, &honest_public), "{suite}: honest control");
	assert!(
		P::verify_proof_of_possession(OWNER, &honest_pop, &honest_public),
		"{suite}: honest PoP control"
	);

	let weak_public = splice(&honest_public, &weak_pk);
	let forged_sig = splice(&honest_sig, &forged_classical_sig);
	let forged_pop = splice(&honest_pop, &forged_classical_sig);
	let mut accepted = Vec::new();
	let mut check = |surface: &str, ok: bool| {
		if ok {
			accepted.push(format!("{suite} {surface} under classical public {}", hex(&weak_pk)));
		}
	};
	check("Pair::verify", P::verify(&forged_sig, MSG, &weak_public));
	check(
		"RuntimePublic::verify",
		sp_application_crypto::RuntimePublic::verify(&weak_public, &MSG, &forged_sig),
	);
	check("PoP", P::verify_proof_of_possession(OWNER, &forged_pop, &weak_public));
	accepted
}

#[test]
fn h3_refuses_the_sr25519_identity_half() {
	let accepted =
		accepted_forgeries::<sr25519_mldsa44::Pair>("H3", [0u8; 32], sr25519_identity_forgery());
	assert!(accepted.is_empty(), "forgeries accepted:\n{}", accepted.join("\n"));
}

#[test]
fn h4_refuses_the_sr25519_identity_half() {
	let accepted =
		accepted_forgeries::<sr25519_fndsa512::Pair>("H4", [0u8; 32], sr25519_identity_forgery());
	assert!(accepted.is_empty(), "forgeries accepted:\n{}", accepted.join("\n"));
}

#[test]
fn h1_refuses_every_small_order_ed25519_half() {
	let accepted: Vec<String> = small_order_ed25519_encodings()
		.into_iter()
		.flat_map(|weak| {
			accepted_forgeries::<ed25519_mldsa44::Pair>("H1", weak, ed25519_small_order_forgery())
		})
		.collect();
	assert!(accepted.is_empty(), "{} forgeries accepted:\n{}", accepted.len(), accepted.join("\n"));
}

#[test]
fn h2_refuses_every_small_order_ed25519_half() {
	let accepted: Vec<String> = small_order_ed25519_encodings()
		.into_iter()
		.flat_map(|weak| {
			accepted_forgeries::<ed25519_fndsa512::Pair>("H2", weak, ed25519_small_order_forgery())
		})
		.collect();
	assert!(accepted.is_empty(), "{} forgeries accepted:\n{}", accepted.len(), accepted.join("\n"));
}
