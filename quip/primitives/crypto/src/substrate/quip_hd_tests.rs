//! The Quip Substrate profile of QUIP HD v1, end to end through the hybrid
//! `Pair` (WP2 batch 1 design note v2.2, §3-§6).
//!
//! `fixtures/quip_hd_parity.json` is pqhybridsign's golden parity file,
//! vendored unchanged from `grego/audit-wp2/quip-hd` d58d9f6
//! (`crates/pqhybridsign/tests/vectors/quip_hd_parity.json`, sha256
//! e9af023dd0df9c17a28ef7659f87c8dd3db77b68211126ac33c7e7ffff4effa2). Every
//! case whose scope or `rejected_by` names `sdk` is this layer's.

use serde_json::Value;
use sha2::{Digest, Sha256};
use sp_core::crypto::{ByteArray, DeriveJunction, Pair as PairT, DEV_PHRASE};

use super::{ed25519_fndsa512, ed25519_mldsa44, sr25519_fndsa512, sr25519_mldsa44};

const LEGAL: &str = "legal winner thank year wave sausage worth useful legal winner thank yellow";

fn parity() -> Value {
	serde_json::from_str(include_str!("fixtures/quip_hd_parity.json")).expect("parity file")
}

fn hex(bytes: &[u8]) -> String {
	bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Every `sdk` positive selects the file's seed and public key for `suite`,
/// and every `sdk` negative is refused.
fn check_parity<P: PairT<Seed = [u8; 32]>>(suite: &str)
where
	P::Public: ByteArray,
	P::Public: core::fmt::Debug,
{
	let file = parity();
	let mut positives = 0;
	for case in file["positive"].as_array().expect("positive") {
		if !case["scope"].as_array().expect("scope").iter().any(|l| l == "sdk") {
			continue;
		}
		let suri = case["suri"].as_str().expect("suri");
		let name = case["name"].as_str().expect("name");
		let key = &case["keys"][suite];
		let (pair, seed) = P::from_string_with_seed(suri, None)
			.unwrap_or_else(|e| panic!("{suite} {name}: {e:?}"));
		let seed = seed.expect("a hybrid pair always returns its seed");
		assert_eq!(hex(&seed), key["seed"].as_str().expect("seed"), "{suite} {name}");
		let public = pair.public().to_raw_vec();
		assert_eq!(
			hex(&Sha256::digest(&public)),
			key["public_sha256"].as_str().expect("public_sha256"),
			"{suite} {name}"
		);
		if let Some(full) = key["public"].as_str() {
			assert_eq!(hex(&public), full, "{suite} {name}");
		}
		// Round trip: the returned seed rebuilds the pair.
		assert_eq!(P::from_seed(&seed).public(), pair.public(), "{suite} {name}");
		positives += 1;
	}
	assert!(positives >= 20, "{suite}: {positives} sdk positives");

	let mut negatives = 0;
	for case in file["negative"].as_array().expect("negative") {
		if !case["rejected_by"].as_array().expect("rejected_by").iter().any(|l| l == "sdk") {
			continue;
		}
		let suri = case["suri"].as_str().expect("suri");
		let name = case["name"].as_str().expect("name");
		assert!(P::from_string_with_seed(suri, None).is_err(), "{suite} {name}: {suri:?}");
		negatives += 1;
	}
	assert!(negatives >= 15, "{suite}: {negatives} sdk negatives");
}

#[test]
fn the_golden_parity_file_holds_for_every_suite() {
	check_parity::<ed25519_mldsa44::Pair>("H1");
	check_parity::<ed25519_fndsa512::Pair>("H2");
	check_parity::<sr25519_mldsa44::Pair>("H3");
	check_parity::<sr25519_fndsa512::Pair>("H4");
}

fn properties<P: PairT<Seed = [u8; 32]>>(suite: &str)
where
	P::Public: core::fmt::Debug,
{
	let public = |s: &str| {
		P::from_string(s, None)
			.unwrap_or_else(|e| panic!("{suite} {s}: {e:?}"))
			.public()
	};

	// §3: `phrase` == `phrase//0` == `phrase//0//0`, and an empty `///` is no
	// password.
	let bare = public(LEGAL);
	assert_eq!(public(&format!("{LEGAL}//0")), bare, "{suite}");
	assert_eq!(public(&format!("{LEGAL}//0//0")), bare, "{suite}");
	assert_eq!(public(&format!("{LEGAL}///")), bare, "{suite}");
	assert_ne!(public(&format!("{LEGAL}///pw")), bare, "{suite}");
	assert_ne!(public("//Alice"), public("//Bob"), "{suite}");
	assert_eq!(public("//Alice"), public(&format!("{DEV_PHRASE}//Alice")), "{suite}");

	// §5 round trips: `from_phrase` and `generate_with_phrase`.
	let (pair, seed) = P::from_phrase(LEGAL, Some("pw")).expect("phrase");
	assert_eq!(P::from_seed(&seed).public(), pair.public(), "{suite}");
	assert_eq!(pair.public(), public(&format!("{LEGAL}///pw")), "{suite}");
	let (generated, phrase, seed) = P::generate_with_phrase(Some("pw"));
	assert_eq!(phrase.split(' ').count(), 12, "{suite}");
	assert_eq!(P::from_seed(&seed).public(), generated.public(), "{suite}");
	assert_eq!(
		P::from_phrase(&phrase, Some("pw")).expect("generated phrase").0.public(),
		generated.public(),
		"{suite}"
	);

	// §3: the keystore's override beats a phrase's own `///`.
	let (overridden, _) =
		P::from_string_with_seed(&format!("{LEGAL}///pw"), Some("keystore")).expect("phrase");
	assert_eq!(overridden.public(), public(&format!("{LEGAL}///keystore")), "{suite}");
	// §4: ... and does not apply to a raw seed, which keeps loading.
	let raw = format!("0x{}", "2a".repeat(32));
	let (raw_pair, _) = P::from_string_with_seed(&raw, Some("keystore")).expect("raw");
	assert_eq!(raw_pair.public(), P::from_seed(&[0x2a; 32]).public(), "{suite}");
	// §4: a raw seed without junctions is `from_seed`, unchanged.
	assert_eq!(public(&raw), P::from_seed(&[0x2a; 32]).public(), "{suite}");

	// §5, pinned: phrase junctions are QUIP HD levels; `derive` is the HKDF
	// junction. They differ, unlike upstream's equivalence.
	let (root, root_seed) = P::from_phrase(LEGAL, None).expect("phrase");
	let (derived, derived_seed) =
		root.derive([DeriveJunction::hard(1u64)].into_iter(), None).expect("derive");
	let child =
		pqhybridsign::derive::derive_hard_junction(&root_seed, DeriveJunction::hard(1u64).inner())
			.expect("junction");
	assert_eq!(derived_seed, Some(child), "{suite}: derive is the HKDF junction");
	assert_ne!(derived.public(), public(&format!("{LEGAL}//1")), "{suite}");
}

#[test]
fn the_design_properties_hold_for_every_suite() {
	properties::<ed25519_mldsa44::Pair>("H1");
	properties::<ed25519_fndsa512::Pair>("H2");
	properties::<sr25519_mldsa44::Pair>("H3");
	properties::<sr25519_fndsa512::Pair>("H4");
}

/// Design v2.1 item 2: a rejected SURI's error carries none of it, through
/// the `Pair` entry points.
#[test]
fn rejected_suris_are_not_echoed() {
	for suri in [
		format!("\"{LEGAL}\""),
		format!("{LEGAL}\n"),
		format!("{LEGAL}///Tr0ub4dor\nx"),
		format!("{LEGAL}//01"),
		format!("0X{}", "2a".repeat(32)),
	] {
		let Err(error) = sr25519_fndsa512::Pair::from_string(&suri, None) else {
			panic!("{} must be rejected", suri.len());
		};
		for text in [format!("{error}"), format!("{error:?}")] {
			for secret in ["legal", "winner", "yellow", "Tr0ub4dor", "2a2a"] {
				assert!(!text.contains(secret), "{text} shows {secret}");
			}
		}
	}
}
