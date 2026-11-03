// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::vec;

use codec::Encode;
use frame_support::{
	construct_runtime, derive_impl, parameter_types,
	traits::{ConstU32, ConstU64},
};
use sp_consensus_beefy::mmr::MmrLeafVersion;
use sp_io::TestExternalities;
use sp_runtime::{
	impl_opaque_keys,
	traits::{ConvertInto, Keccak256, OpaqueKeys},
	BuildStorage,
};
use sp_state_machine::BasicExternalities;

use crate as pallet_beefy_mmr;

pub use sp_consensus_beefy::{
	ecdsa_crypto::{AuthorityId as BeefyId, Pair as BeefyPair},
	mmr::BeefyDataProvider,
	ConsensusLog, BEEFY_ENGINE_ID,
};
use sp_core::{
	offchain::{testing::TestOffchainExt, OffchainDbExt, OffchainWorkerExt},
	Pair,
};

impl_opaque_keys! {
	pub struct MockSessionKeys {
		pub dummy: pallet_beefy::Pallet<Test>,
	}
}

type Block = frame_system::mocking::MockBlock<Test>;

construct_runtime!(
	pub enum Test
	{
		System: frame_system,
		Session: pallet_session,
		Balances: pallet_balances,
		Mmr: pallet_mmr,
		Beefy: pallet_beefy,
		BeefyMmr: pallet_beefy_mmr,
	}
);
#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type AccountData = pallet_balances::AccountData<u64>;
	type Block = Block;
}

#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig)]
impl pallet_balances::Config for Test {
	type AccountStore = System;
}

impl pallet_session::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type ValidatorId = u64;
	type ValidatorIdOf = ConvertInto;
	type ShouldEndSession = pallet_session::PeriodicSessions<ConstU64<1>, ConstU64<0>>;
	type NextSessionRotation = pallet_session::PeriodicSessions<ConstU64<1>, ConstU64<0>>;
	type SessionManager = MockSessionManager;
	type SessionHandler = <MockSessionKeys as OpaqueKeys>::KeyTypeIdProviders;
	type Keys = MockSessionKeys;
	type DisablingStrategy = ();
	type WeightInfo = ();
	type Currency = Balances;
	type KeyDeposit = ();
}

pub type MmrLeaf = sp_consensus_beefy::mmr::MmrLeaf<
	frame_system::pallet_prelude::BlockNumberFor<Test>,
	<Test as frame_system::Config>::Hash,
	crate::MerkleRootOf<Test>,
	Vec<u8>,
>;

impl pallet_mmr::Config for Test {
	const INDEXING_PREFIX: &'static [u8] = b"mmr";

	type Hashing = Keccak256;

	type LeafData = BeefyMmr;

	type OnNewRoot = pallet_beefy_mmr::DepositBeefyDigest<Test>;

	type BlockHashProvider = pallet_mmr::DefaultBlockHashProvider<Test>;

	type WeightInfo = ();

	#[cfg(feature = "runtime-benchmarks")]
	type BenchmarkHelper = ();
}

impl pallet_beefy::Config for Test {
	type BeefyId = BeefyId;
	type MaxAuthorities = ConstU32<100>;
	type MaxNominators = ConstU32<1000>;
	type MaxSetIdSessionEntries = ConstU64<100>;
	type OnNewValidatorSet = BeefyMmr;
	type AncestryHelper = BeefyMmr;
	type WeightInfo = ();
	type KeyOwnerProof = sp_core::Void;
	type EquivocationReportSystem = ();
}

parameter_types! {
	pub LeafVersion: MmrLeafVersion = MmrLeafVersion::new(1, 5);
}

impl pallet_beefy_mmr::Config for Test {
	type LeafVersion = LeafVersion;

	type BeefyAuthorityToMerkleLeaf = pallet_beefy_mmr::BeefyEcdsaToEthereum;

	type LeafExtra = Vec<u8>;

	type BeefyDataProvider = DummyDataProvider;
	type WeightInfo = ();
}

pub struct DummyDataProvider;
impl BeefyDataProvider<Vec<u8>> for DummyDataProvider {
	fn extra_data() -> Vec<u8> {
		let mut col = vec![(15, vec![1, 2, 3]), (5, vec![4, 5, 6])];
		col.sort();
		binary_merkle_tree::merkle_root::<<Test as pallet_mmr::Config>::Hashing, _>(
			col.into_iter().map(|pair| pair.encode()),
		)
		.as_ref()
		.to_vec()
	}
}

pub struct MockSessionManager;
impl pallet_session::SessionManager<u64> for MockSessionManager {
	fn end_session(_: sp_staking::SessionIndex) {}
	fn start_session(_: sp_staking::SessionIndex) {}
	fn new_session(idx: sp_staking::SessionIndex) -> Option<Vec<u64>> {
		if idx == 0 || idx == 1 {
			Some(vec![1, 2])
		} else if idx == 2 {
			Some(vec![3, 4])
		} else {
			None
		}
	}
}

// Note, that we can't use `UintAuthorityId` here. Reason is that the implementation
// of `to_public_key()` assumes, that a public key is 32 bytes long. This is true for
// ed25519 and sr25519 but *not* for ecdsa. A compressed ecdsa public key is 33 bytes,
// with the first one containing information to reconstruct the uncompressed key.
//
// We derive deterministic *valid* ECDSA keys from a per-id seed so that
// `BeefyEcdsaToEthereum` exercises its happy path (it errors on malformed keys and
// falls back to the all-zeros sentinel; using fabricated bytes would silently hide
// that behavior in tests).
pub fn mock_beefy_id(id: u8) -> BeefyId {
	let mut seed = [0u8; 32];
	seed[0] = id;
	BeefyPair::from_seed(&seed).public()
}

pub fn mock_authorities(vec: Vec<u8>) -> Vec<(u64, BeefyId)> {
	vec.into_iter().map(|id| ((id as u64), mock_beefy_id(id))).collect()
}

pub fn new_test_ext(ids: Vec<u8>) -> TestExternalities {
	new_test_ext_raw_authorities(mock_authorities(ids))
}

pub fn new_test_ext_raw_authorities(authorities: Vec<(u64, BeefyId)>) -> TestExternalities {
	let mut t = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();

	let session_keys: Vec<_> = authorities
		.iter()
		.enumerate()
		.map(|(_, id)| (id.0 as u64, id.0 as u64, MockSessionKeys { dummy: id.1.clone() }))
		.collect();

	BasicExternalities::execute_with_storage(&mut t, || {
		for (ref id, ..) in &session_keys {
			frame_system::Pallet::<Test>::inc_providers(id);
		}
	});

	pallet_session::GenesisConfig::<Test> { keys: session_keys, ..Default::default() }
		.assimilate_storage(&mut t)
		.unwrap();

	let mut ext: TestExternalities = t.into();
	let (offchain, _offchain_state) = TestOffchainExt::with_offchain_db(ext.offchain_db());
	ext.register_extension(OffchainDbExt::new(offchain.clone()));
	ext.register_extension(OffchainWorkerExt::new(offchain));

	ext
}

pub fn build_and_execute(ids: Vec<u8>, test: impl FnOnce()) {
	new_test_ext(ids).execute_with(|| {
		test();
		crate::Pallet::<Test>::do_try_state().expect("All invariants must hold after a test");
	})
}
