//! Junctions on a hybrid pair are pqhybridsign's HKDF junction for every
//! suite (design note v2 §4); sr25519 and ed25519 hard derivation are gone
//! (GregoAI wp-2 G-003).

use sp_core::crypto::{DeriveError, DeriveJunction, Pair as PairT};

use super::{ed25519_fndsa512, ed25519_mldsa44, sr25519_fndsa512, sr25519_mldsa44};

const SEED: [u8; 32] = [0x2a; 32];

/// The seed pqhybridsign's HKDF junction gives for `path` from `seed`.
fn hkdf_path(seed: [u8; 32], path: &[DeriveJunction]) -> [u8; 32] {
	path.iter().fold(seed, |parent, junction| {
		pqhybridsign::derive::derive_hard_junction(&parent, junction.inner()).expect("junction")
	})
}

fn check<P>(suite: &str)
where
	P: PairT<Seed = [u8; 32]>,
	P::Public: core::fmt::Debug,
{
	let root = P::from_seed(&SEED);
	for path in [
		vec![DeriveJunction::hard(1u64)],
		vec![DeriveJunction::hard("Alice"), DeriveJunction::hard(7u64)],
	] {
		let (child, child_seed) = root.derive(path.clone().into_iter(), None).expect("derive");
		let want = hkdf_path(SEED, &path);
		assert_eq!(child_seed, Some(want), "{suite}");
		assert_eq!(child.public(), P::from_seed(&want).public(), "{suite}");
	}
	// A soft junction is refused, anywhere in the path.
	for path in [
		vec![DeriveJunction::soft(1u64)],
		vec![DeriveJunction::hard(1u64), DeriveJunction::soft("x")],
	] {
		assert!(
			matches!(root.derive(path.into_iter(), None), Err(DeriveError::SoftKeyInPath)),
			"{suite}"
		);
	}
	// A raw seed with junctions goes through the same derivation.
	let raw = format!("0x{}//1", "2a".repeat(32));
	let (from_suri, _) = P::from_string_with_seed(&raw, None).expect("raw seed SURI");
	assert_eq!(
		from_suri.public(),
		P::from_seed(&hkdf_path(SEED, &[DeriveJunction::hard(1u64)])).public(),
		"{suite}"
	);
}

#[test]
fn every_suite_derives_hard_junctions_with_the_hkdf_junction() {
	check::<ed25519_mldsa44::Pair>("H1");
	check::<ed25519_fndsa512::Pair>("H2");
	check::<sr25519_mldsa44::Pair>("H3");
	check::<sr25519_fndsa512::Pair>("H4");
}
