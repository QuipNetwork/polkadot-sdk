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

//! Test utilities

use crate::{self as pallet_babe, Config, CurrentSlot};
use codec::Encode;
use frame_election_provider_support::{
	bounds::{ElectionBounds, ElectionBoundsBuilder},
	onchain, SequentialPhragmen,
};
use frame_support::{
	derive_impl, parameter_types,
	traits::{ConstU128, ConstU32, ConstU64, OnInitialize},
};
use pallet_session::historical as pallet_session_historical;
use quip_crypto_primitives::HybridVrf;
use sp_consensus_babe::{AuthorityId, AuthorityPair, Randomness, Slot, VrfSignature};
use sp_core::{
	crypto::{Pair, VrfSecret},
	ConstBool, U256,
};
use sp_io;
use sp_runtime::{
	curve::PiecewiseLinear,
	impl_opaque_keys,
	testing::{Digest, DigestItem, Header, TestXt},
	traits::{Header as _, OpaqueKeys},
	BuildStorage, DispatchError, Perbill,
};
use sp_staking::{EraIndex, SessionIndex};

type DummyValidatorId = u64;

type Block = frame_system::mocking::MockBlock<Test>;

frame_support::construct_runtime!(
	pub enum Test
	{
		System: frame_system,
		Authorship: pallet_authorship,
		Balances: pallet_balances,
		Historical: pallet_session_historical,
		Offences: pallet_offences,
		Babe: pallet_babe,
		Staking: pallet_staking,
		Session: pallet_session,
		Timestamp: pallet_timestamp,
	}
);

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type Block = Block;
	type AccountData = pallet_balances::AccountData<u128>;
}

impl<C> frame_system::offchain::CreateTransactionBase<C> for Test
where
	RuntimeCall: From<C>,
{
	type RuntimeCall = RuntimeCall;
	type Extrinsic = TestXt<RuntimeCall, ()>;
}

impl<C> frame_system::offchain::CreateBare<C> for Test
where
	RuntimeCall: From<C>,
{
	fn create_bare(call: Self::RuntimeCall) -> Self::Extrinsic {
		TestXt::new_bare(call)
	}
}

impl_opaque_keys! {
	pub struct MockSessionKeys {
		pub babe_authority: super::Pallet<Test>,
	}
}

impl pallet_session::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type ValidatorId = <Self as frame_system::Config>::AccountId;
	type ValidatorIdOf = sp_runtime::traits::ConvertInto;
	type ShouldEndSession = Babe;
	type NextSessionRotation = Babe;
	type SessionManager = pallet_session::historical::NoteHistoricalRoot<Self, Staking>;
	type SessionHandler = <MockSessionKeys as OpaqueKeys>::KeyTypeIdProviders;
	type Keys = MockSessionKeys;
	type DisablingStrategy = ();
	type WeightInfo = ();
	type Currency = Balances;
	type KeyDeposit = ();
}

impl pallet_session::historical::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type FullIdentification = ();
	type FullIdentificationOf = pallet_staking::UnitIdentificationOf<Self>;
}

impl pallet_authorship::Config for Test {
	type FindAuthor = pallet_session::FindAccountFromAuthorIndex<Self, Babe>;
	type EventHandler = ();
}

impl pallet_timestamp::Config for Test {
	type Moment = u64;
	type OnTimestampSet = Babe;
	type MinimumPeriod = ConstU64<1>;
	type WeightInfo = ();
}

type Balance = u128;
#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig)]
impl pallet_balances::Config for Test {
	type Balance = Balance;
	type ExistentialDeposit = ConstU128<1>;
	type AccountStore = System;
}

pallet_staking_reward_curve::build! {
	const REWARD_CURVE: PiecewiseLinear<'static> = curve!(
		min_inflation: 0_025_000u64,
		max_inflation: 0_100_000,
		ideal_stake: 0_500_000,
		falloff: 0_050_000,
		max_piece_count: 40,
		test_precision: 0_005_000,
	);
}

parameter_types! {
	pub const SessionsPerEra: SessionIndex = 3;
	pub const BondingDuration: EraIndex = 3;
	pub const RewardCurve: &'static PiecewiseLinear<'static> = &REWARD_CURVE;
	pub static ElectionsBounds: ElectionBounds = ElectionBoundsBuilder::default().build();
}

pub struct OnChainSeqPhragmen;
impl onchain::Config for OnChainSeqPhragmen {
	type System = Test;
	type Solver = SequentialPhragmen<DummyValidatorId, Perbill>;
	type DataProvider = Staking;
	type WeightInfo = ();
	type MaxWinnersPerPage = ConstU32<100>;
	type MaxBackersPerWinner = ConstU32<100>;
	type Sort = ConstBool<true>;
	type Bounds = ElectionsBounds;
}

#[derive_impl(pallet_staking::config_preludes::TestDefaultConfig)]
impl pallet_staking::Config for Test {
	type OldCurrency = Balances;
	type Currency = Balances;
	type SessionsPerEra = SessionsPerEra;
	type BondingDuration = BondingDuration;
	type AdminOrigin = frame_system::EnsureRoot<Self::AccountId>;
	type SessionInterface = Self;
	type UnixTime = pallet_timestamp::Pallet<Test>;
	type EraPayout = pallet_staking::ConvertCurve<RewardCurve>;
	type NextNewSession = Session;
	type ElectionProvider = onchain::OnChainExecution<OnChainSeqPhragmen>;
	type GenesisElectionProvider = Self::ElectionProvider;
	type VoterList = pallet_staking::UseNominatorsAndValidatorsMap<Self>;
	type TargetList = pallet_staking::UseValidatorsMap<Self>;
}

impl pallet_offences::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type IdentificationTuple = pallet_session::historical::IdentificationTuple<Self>;
	type OnOffenceHandler = Staking;
}

parameter_types! {
	pub const EpochDuration: u64 = 3;
	pub const ReportLongevity: u64 =
		BondingDuration::get() as u64 * SessionsPerEra::get() as u64 * EpochDuration::get();
}

impl Config for Test {
	type EpochDuration = EpochDuration;
	type ExpectedBlockTime = ConstU64<1>;
	type EpochChangeTrigger = crate::ExternalTrigger;
	type DisabledValidators = Session;
	type WeightInfo = ();
	type MaxAuthorities = ConstU32<10>;
	type MaxNominators = ConstU32<100>;
	type KeyOwnerProof = sp_session::MembershipProof;
	type EquivocationReportSystem =
		super::EquivocationReportSystem<Self, Offences, Historical, ReportLongevity>;
}

pub fn go_to_block(n: u64, s: u64) {
	use frame_support::traits::OnFinalize;

	Babe::on_finalize(System::block_number());
	Session::on_finalize(System::block_number());
	Staking::on_finalize(System::block_number());

	let parent_hash = if System::block_number() > 1 {
		let hdr = System::finalize();
		hdr.hash()
	} else {
		System::parent_hash()
	};

	let pre_digest = make_secondary_plain_pre_digest(0, s.into());

	System::reset_events();
	System::initialize(&n, &parent_hash, &pre_digest);

	Babe::on_initialize(n);
	Session::on_initialize(n);
	Staking::on_initialize(n);
}

/// Slots will grow accordingly to blocks
pub fn progress_to_block(n: u64) {
	let mut slot = u64::from(CurrentSlot::<Test>::get()) + 1;
	for i in System::block_number() + 1..=n {
		go_to_block(i, slot);
		slot += 1;
	}
}

/// Progress to the first block at the given session
pub fn start_session(session_index: SessionIndex) {
	let missing = (session_index - Session::current_index()) * 3;
	progress_to_block(System::block_number() + missing as u64 + 1);
	assert_eq!(Session::current_index(), session_index);
}

/// Progress to the first block at the given era
pub fn start_era(era_index: EraIndex) {
	start_session((era_index * 3).into());
	assert_eq!(pallet_staking::CurrentEra::<Test>::get(), Some(era_index));
}

pub fn make_primary_pre_digest(
	authority_index: sp_consensus_babe::AuthorityIndex,
	slot: sp_consensus_babe::Slot,
	vrf_signature: VrfSignature,
) -> Digest {
	let digest_data = sp_consensus_babe::digests::PreDigest::Primary(
		sp_consensus_babe::digests::PrimaryPreDigest { authority_index, slot, vrf_signature },
	);
	let log = DigestItem::PreRuntime(sp_consensus_babe::BABE_ENGINE_ID, digest_data.encode());
	Digest { logs: vec![log] }
}

pub fn make_secondary_plain_pre_digest(
	authority_index: sp_consensus_babe::AuthorityIndex,
	slot: sp_consensus_babe::Slot,
) -> Digest {
	let digest_data = sp_consensus_babe::digests::PreDigest::SecondaryPlain(
		sp_consensus_babe::digests::SecondaryPlainPreDigest { authority_index, slot },
	);
	let log = DigestItem::PreRuntime(sp_consensus_babe::BABE_ENGINE_ID, digest_data.encode());
	Digest { logs: vec![log] }
}

pub fn make_secondary_vrf_pre_digest(
	authority_index: sp_consensus_babe::AuthorityIndex,
	slot: sp_consensus_babe::Slot,
	vrf_signature: VrfSignature,
) -> Digest {
	let digest_data = sp_consensus_babe::digests::PreDigest::SecondaryVRF(
		sp_consensus_babe::digests::SecondaryVRFPreDigest { authority_index, slot, vrf_signature },
	);
	let log = DigestItem::PreRuntime(sp_consensus_babe::BABE_ENGINE_ID, digest_data.encode());
	Digest { logs: vec![log] }
}

pub fn make_vrf_signature_and_randomness(
	slot: Slot,
	pair: &sp_consensus_babe::AuthorityPair,
) -> (VrfSignature, Randomness) {
	let transcript =
		sp_consensus_babe::make_vrf_transcript(&pallet_babe::Randomness::<Test>::get(), slot, 0);

	let randomness =
		pair.as_ref().make_bytes(sp_consensus_babe::RANDOMNESS_VRF_CONTEXT, &transcript);

	let signature = VrfSecret::vrf_sign(pair.as_ref(), &transcript.into());

	(signature, randomness)
}

pub fn new_test_ext(authorities_len: usize) -> sp_io::TestExternalities {
	new_test_ext_with_pairs(authorities_len).1
}

pub fn build_and_execute(authorities_len: usize, test: impl FnOnce()) {
	new_test_ext(authorities_len).execute_with(|| {
		test();
		Babe::do_try_state().expect("All invariants must hold after a test");
	})
}

pub fn build_and_execute_with_pairs(authorities_len: usize, test: impl FnOnce(Vec<AuthorityPair>)) {
	let (pairs, mut ext) = new_test_ext_with_pairs(authorities_len);
	ext.execute_with(|| {
		test(pairs);
		Babe::do_try_state().expect("All invariants must hold after a test");
	})
}

pub fn new_test_ext_with_pairs(
	authorities_len: usize,
) -> (Vec<AuthorityPair>, sp_io::TestExternalities) {
	let pairs = (0..authorities_len)
		.map(|i| AuthorityPair::from_seed(&U256::from(i).to_little_endian()))
		.collect::<Vec<_>>();

	let public = pairs.iter().map(|p| p.public()).collect();

	(pairs, new_test_ext_raw_authorities(public))
}

pub fn new_test_ext_raw_authorities(authorities: Vec<AuthorityId>) -> sp_io::TestExternalities {
	sp_tracing::try_init_simple();
	let mut t = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();

	let balances: Vec<_> = (0..authorities.len()).map(|i| (i as u64, 10_000_000)).collect();

	pallet_balances::GenesisConfig::<Test> { balances, ..Default::default() }
		.assimilate_storage(&mut t)
		.unwrap();

	// stashes are the index.
	let session_keys: Vec<_> = authorities
		.iter()
		.enumerate()
		.map(|(i, k)| {
			(i as u64, i as u64, MockSessionKeys { babe_authority: AuthorityId::from(k.clone()) })
		})
		.collect();

	// NOTE: this will initialize the babe authorities
	// through OneSessionHandler::on_genesis_session
	pallet_session::GenesisConfig::<Test> { keys: session_keys, ..Default::default() }
		.assimilate_storage(&mut t)
		.unwrap();

	// controllers are same as stash
	let stakers: Vec<_> = (0..authorities.len())
		.map(|i| (i as u64, i as u64, 10_000, pallet_staking::StakerStatus::<u64>::Validator))
		.collect();

	let staking_config = pallet_staking::GenesisConfig::<Test> {
		stakers,
		validator_count: 8,
		force_era: pallet_staking::Forcing::ForceNew,
		minimum_validator_count: 0,
		invulnerables: vec![],
		..Default::default()
	};

	staking_config.assimilate_storage(&mut t).unwrap();

	pallet_babe::GenesisConfig::<Test> {
		epoch_config: sp_consensus_babe::BabeEpochConfiguration {
			c: (1, 4),
			allowed_slots: sp_consensus_babe::AllowedSlots::PrimaryAndSecondaryPlainSlots,
		},
		..Default::default()
	}
	.assimilate_storage(&mut t)
	.unwrap();

	t.into()
}

/// Creates an equivocation at the current block, by generating two headers.
pub fn generate_equivocation_proof(
	offender_authority_index: u32,
	offender_authority_pair: &AuthorityPair,
	slot: Slot,
) -> sp_consensus_babe::EquivocationProof<Header> {
	use sp_consensus_babe::digests::CompatibleDigestItem;

	let current_block = System::block_number();
	let current_slot = CurrentSlot::<Test>::get();

	let make_header = || {
		// We don't want to change any state, so we build the headers in a transaction and revert it
		// afterward.
		frame_support::storage::with_transaction(|| {
			let parent_hash = System::parent_hash();
			let pre_digest = make_secondary_plain_pre_digest(offender_authority_index, slot);
			System::reset_events();
			System::set_block_number(System::block_number() - 1);
			System::initialize(&current_block, &parent_hash, &pre_digest);
			System::set_block_number(current_block);
			Timestamp::set_timestamp(*current_slot * Babe::slot_duration());
			let header = System::finalize();

			sp_runtime::TransactionOutcome::Rollback(Ok::<_, DispatchError>(header))
		})
		.unwrap()
	};

	// Sign the header prehash and sign it, adding it to the block as the seal
	// digest item
	let seal_header = |header: &mut Header| {
		let prehash = header.hash();
		let seal = <DigestItem as CompatibleDigestItem>::babe_seal(
			offender_authority_pair.sign(prehash.as_ref()),
		);
		header.digest_mut().push(seal);
	};

	// Generate two headers at the current block
	let mut h1 = make_header();
	let mut h2 = make_header();
	// Hybrid signatures are deterministic: signing the same header twice does
	// not create an equivocation. Make the second pre-seal header distinct.
	h2.digest_mut().push(DigestItem::Other(b"second equivocation header".to_vec()));

	seal_header(&mut h1);
	seal_header(&mut h2);

	let proof = sp_consensus_babe::EquivocationProof {
		slot,
		offender: offender_authority_pair.public(),
		first_header: h1,
		second_header: h2,
	};
	assert!(sp_consensus_babe::check_equivocation_proof(proof.clone()));
	proof
}
