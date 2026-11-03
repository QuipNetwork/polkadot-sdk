// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! Mocks for all the traits.

use crate::{
	configuration, coretime, disputes,
	disputes::slashing as disputes_slashing,
	dmp, hrmp,
	inclusion::{self, AggregateMessageOrigin, UmpQueueId},
	initializer, on_demand, origin, paras,
	paras::ParaKind,
	paras_inherent, scheduler, session_info, shared, ParaId,
};
use frame_support::pallet_prelude::*;

use codec::Decode;
use frame_support::{
	assert_ok, derive_impl,
	dispatch::GetDispatchInfo,
	parameter_types,
	traits::{
		Currency, ProcessMessage, ProcessMessageError, ValidatorSet, ValidatorSetWithIdentification,
	},
	weights::{Weight, WeightMeter},
	PalletId,
};
use frame_support_test::TestRandomness;
use frame_system::{limits, EnsureRoot};
use polkadot_primitives::{
	slashing::DisputesTimeSlot, AuthorityDiscoveryId, Balance, BlockNumber, CandidateHash,
	DisputeOffenceKind, Moment, SessionIndex, UpwardMessage, ValidationCode, ValidatorId,
	ValidatorIndex, PARACHAIN_KEY_TYPE_ID,
};
use sp_core::{crypto::KeyTypeId, ConstU32, H256};
use sp_io::TestExternalities;
use sp_runtime::{
	traits::{AccountIdConversion, BlakeTwo256, IdentityLookup},
	transaction_validity::TransactionPriority,
	BuildStorage, FixedU128, Perbill, Permill,
};
use sp_staking::offence::OffenceError;
use std::{
	cell::RefCell,
	collections::{btree_map::BTreeMap, HashMap},
};
use xcm::{
	prelude::XcmVersion,
	v5::{Assets, InteriorLocation, Location, SendError, SendResult, SendXcm, Xcm, XcmHash},
	IntoVersion, VersionedXcm, WrapVersion,
};

type UncheckedExtrinsic = frame_system::mocking::MockUncheckedExtrinsic<Test>;
type Block = frame_system::mocking::MockBlockU32<Test>;

frame_support::construct_runtime!(
	pub enum Test
	{
		System: frame_system,
		Balances: pallet_balances,
		MessageQueue: pallet_message_queue,
		Paras: paras,
		Configuration: configuration,
		ParasShared: shared,
		ParaInclusion: inclusion,
		ParaInherent: paras_inherent,
		Scheduler: scheduler,
		OnDemand: on_demand,
		Coretime: coretime,
		Initializer: initializer,
		Dmp: dmp,
		Hrmp: hrmp,
		ParachainsOrigin: origin,
		SessionInfo: session_info,
		Disputes: disputes,
		Slashing: disputes_slashing,
		Babe: pallet_babe,
	}
);

impl<C> frame_system::offchain::CreateTransactionBase<C> for Test
where
	RuntimeCall: From<C>,
{
	type Extrinsic = UncheckedExtrinsic;
	type RuntimeCall = RuntimeCall;
}

impl<C> frame_system::offchain::CreateBare<C> for Test
where
	RuntimeCall: From<C>,
{
	fn create_bare(call: Self::RuntimeCall) -> Self::Extrinsic {
		UncheckedExtrinsic::new_bare(call)
	}
}

parameter_types! {
	pub static BlockWeights: frame_system::limits::BlockWeights =
		frame_system::limits::BlockWeights::simple_max(
			Weight::from_parts(4 * 1024 * 1024, u64::MAX),
		);
	pub static BlockLength: limits::BlockLength = limits::BlockLength::builder()
		.max_length(u32::MAX)
		.modify_max_length_for_class(frame_support::dispatch::DispatchClass::Normal, |m| {
			*m = Perbill::from_percent(75) * u32::MAX
		})
		.build();
}

pub type AccountId = u64;

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type BaseCallFilter = frame_support::traits::Everything;
	type BlockWeights = BlockWeights;
	type BlockLength = BlockLength;
	type DbWeight = ();
	type RuntimeOrigin = RuntimeOrigin;
	type RuntimeCall = RuntimeCall;
	type Nonce = u64;
	type Hash = H256;
	type Hashing = BlakeTwo256;
	type AccountId = u64;
	type Lookup = IdentityLookup<u64>;
	type Block = Block;
	type RuntimeEvent = RuntimeEvent;
	type Version = ();
	type PalletInfo = PalletInfo;
	type AccountData = pallet_balances::AccountData<u128>;
	type OnNewAccount = ();
	type OnKilledAccount = ();
	type SystemWeightInfo = ();
	type SS58Prefix = ();
	type OnSetCode = ();
	type MaxConsumers = ConstU32<16>;
}

parameter_types! {
	pub static ExistentialDeposit: u64 = 1;
}

#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig)]
impl pallet_balances::Config for Test {
	type Balance = Balance;
	type ExistentialDeposit = ExistentialDeposit;
	type AccountStore = System;
}

parameter_types! {
	pub const EpochDuration: u64 = 10;
	pub const ExpectedBlockTime: Moment = 6_000;
	pub const ReportLongevity: u64 = 10;
	pub const MaxAuthorities: u32 = 100_000;
}

impl pallet_babe::Config for Test {
	type EpochDuration = EpochDuration;
	type ExpectedBlockTime = ExpectedBlockTime;

	// session module is the trigger
	type EpochChangeTrigger = pallet_babe::ExternalTrigger;
	type DisabledValidators = ();
	type WeightInfo = ();
	type MaxAuthorities = MaxAuthorities;
	type MaxNominators = ConstU32<0>;
	type KeyOwnerProof = sp_core::Void;
	type EquivocationReportSystem = ();
}

parameter_types! {
	pub const MinimumPeriod: Moment = 6_000 / 2;
}

impl pallet_timestamp::Config for Test {
	type Moment = Moment;
	type OnTimestampSet = ();
	type MinimumPeriod = MinimumPeriod;
	type WeightInfo = ();
}

impl crate::initializer::Config for Test {
	type Randomness = TestRandomness<Self>;
	type ForceOrigin = frame_system::EnsureRoot<u64>;
	type WeightInfo = ();
	type CoretimeOnNewSession = Coretime;
}

impl crate::configuration::Config for Test {
	type WeightInfo = crate::configuration::TestWeightInfo;
}

pub struct MockDisabledValidators {}
impl frame_support::traits::DisabledValidators for MockDisabledValidators {
	/// Returns true if the given validator is disabled.
	fn is_disabled(index: u32) -> bool {
		disabled_validators().iter().any(|v| *v == index)
	}

	/// Returns a hardcoded list (`DISABLED_VALIDATORS`) of disabled validators
	fn disabled_validators() -> Vec<u32> {
		disabled_validators()
	}
}

impl crate::shared::Config for Test {
	type DisabledValidators = MockDisabledValidators;
}

impl origin::Config for Test {}

parameter_types! {
	pub const ParasUnsignedPriority: TransactionPriority = TransactionPriority::max_value();
}

/// A very dumb implementation of `EstimateNextSessionRotation`. At the moment of writing, this
/// is more to satisfy type requirements rather than to test anything.
pub struct TestNextSessionRotation;

impl frame_support::traits::EstimateNextSessionRotation<u32> for TestNextSessionRotation {
	fn average_session_length() -> u32 {
		10
	}

	fn estimate_current_session_progress(_now: u32) -> (Option<Permill>, Weight) {
		(None, Weight::zero())
	}

	fn estimate_next_session_rotation(_now: u32) -> (Option<u32>, Weight) {
		(None, Weight::zero())
	}
}

impl crate::paras::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type WeightInfo = crate::paras::TestWeightInfo;
	type UnsignedPriority = ParasUnsignedPriority;
	type QueueFootprinter = ParaInclusion;
	type NextSessionRotation = TestNextSessionRotation;
	type OnNewHead = ();
	type AssignCoretime = ();
	type Fungible = Balances;
	type CooldownRemovalMultiplier = ConstUint<1>;
	type AuthorizeCurrentCodeOrigin = EnsureRoot<AccountId>;
}

impl crate::dmp::Config for Test {
	type WeightInfo = ();
}

parameter_types! {
	pub const DefaultChannelSizeAndCapacityWithSystem: (u32, u32) = (4, 1);
}

thread_local! {
	pub static VERSION_WRAPPER: RefCell<BTreeMap<Location, Option<XcmVersion>>> = RefCell::new(BTreeMap::new());
}
/// Mock implementation of the [`WrapVersion`] trait which wraps XCM only for known/stored XCM
/// versions in the `VERSION_WRAPPER`.
pub struct TestUsesOnlyStoredVersionWrapper;
impl WrapVersion for TestUsesOnlyStoredVersionWrapper {
	fn wrap_version<RuntimeCall: Decode + GetDispatchInfo>(
		dest: &Location,
		xcm: impl Into<VersionedXcm<RuntimeCall>>,
	) -> Result<VersionedXcm<RuntimeCall>, ()> {
		match VERSION_WRAPPER.with(|r| r.borrow().get(dest).map_or(None, |v| *v)) {
			Some(v) => xcm.into().into_version(v),
			None => Err(()),
		}
	}
}
impl TestUsesOnlyStoredVersionWrapper {
	pub fn set_version(location: Location, version: Option<XcmVersion>) {
		VERSION_WRAPPER.with(|r| {
			let _ = r.borrow_mut().entry(location).and_modify(|v| *v = version).or_insert(version);
		});
	}
}

impl crate::hrmp::Config for Test {
	type RuntimeOrigin = RuntimeOrigin;
	type RuntimeEvent = RuntimeEvent;
	type ChannelManager = frame_system::EnsureRoot<u64>;
	type Currency = pallet_balances::Pallet<Test>;
	type DefaultChannelSizeAndCapacityWithSystem = DefaultChannelSizeAndCapacityWithSystem;
	type VersionWrapper = TestUsesOnlyStoredVersionWrapper;
	type WeightInfo = crate::hrmp::TestWeightInfo;
}

impl crate::disputes::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type RewardValidators = Self;
	type SlashingHandler = Self;
	type WeightInfo = crate::disputes::TestWeightInfo;
}

thread_local! {
	pub static REWARD_VALIDATORS: RefCell<Vec<(SessionIndex, Vec<ValidatorIndex>)>> = RefCell::new(Vec::new());
	pub static PUNISH_VALIDATORS_FOR: RefCell<Vec<(SessionIndex, Vec<ValidatorIndex>)>> = RefCell::new(Vec::new());
	pub static PUNISH_VALIDATORS_AGAINST: RefCell<Vec<(SessionIndex, Vec<ValidatorIndex>)>> = RefCell::new(Vec::new());
	pub static PUNISH_BACKERS_FOR: RefCell<Vec<(SessionIndex, Vec<ValidatorIndex>)>> = RefCell::new(Vec::new());
}

impl crate::disputes::RewardValidators for Test {
	fn reward_dispute_statement(
		session: SessionIndex,
		validators: impl IntoIterator<Item = ValidatorIndex>,
	) {
		REWARD_VALIDATORS.with(|r| r.borrow_mut().push((session, validators.into_iter().collect())))
	}
}

impl crate::disputes::SlashingHandler<BlockNumber> for Test {
	fn punish_for_invalid(
		session: SessionIndex,
		_: CandidateHash,
		losers: impl IntoIterator<Item = ValidatorIndex>,
		backers: impl IntoIterator<Item = ValidatorIndex>,
	) {
		PUNISH_VALIDATORS_FOR
			.with(|r| r.borrow_mut().push((session, losers.into_iter().collect())));
		PUNISH_BACKERS_FOR.with(|r| r.borrow_mut().push((session, backers.into_iter().collect())));
	}

	fn punish_against_valid(
		session: SessionIndex,
		_: CandidateHash,
		losers: impl IntoIterator<Item = ValidatorIndex>,
		_backers: impl IntoIterator<Item = ValidatorIndex>,
	) {
		PUNISH_VALIDATORS_AGAINST
			.with(|r| r.borrow_mut().push((session, losers.into_iter().collect())))
	}

	fn initializer_initialize(_now: BlockNumber) -> Weight {
		Weight::zero()
	}

	fn initializer_finalize() {}

	fn initializer_on_new_session(_: SessionIndex) {}
}

parameter_types! {
	pub const SlashingReportLongevity: u64 = 100;
}

thread_local! {
	pub static MOCK_KEY_OWNERSHIP_PROOFS:
		RefCell<HashMap<(KeyTypeId, ValidatorId, MockKeyOwnerProof), AccountId>>
		= RefCell::new(HashMap::new());
	pub static MOCK_REPORTED_OFFENCES:
		RefCell<Vec<(DisputesTimeSlot, DisputeOffenceKind, Vec<(AccountId, ())>)>>
		= RefCell::new(Vec::new());
	pub static MOCK_KNOWN_OFFENCES: RefCell<Vec<(DisputesTimeSlot, AccountId)>>
		= RefCell::new(Vec::new());
	pub static MOCK_REPORT_OFFENCE_RESULT: RefCell<MockReportResult>
		= const { RefCell::new(MockReportResult::Ok) };
}

#[derive(Clone, Copy, Debug)]
pub enum MockReportResult {
	Ok,
	DuplicateReport,
	#[allow(dead_code)]
	Other(u8),
}

impl MockReportResult {
	fn as_offence_result(self) -> Result<(), OffenceError> {
		match self {
			MockReportResult::Ok => Ok(()),
			MockReportResult::DuplicateReport => Err(OffenceError::DuplicateReport),
			MockReportResult::Other(e) => Err(OffenceError::Other(e)),
		}
	}
}

#[derive(
	Clone,
	PartialEq,
	Eq,
	Hash,
	Debug,
	codec::Encode,
	codec::Decode,
	codec::DecodeWithMemTracking,
	scale_info::TypeInfo,
)]
pub struct MockKeyOwnerProof {
	pub session: SessionIndex,
	pub validator_count: u32,
	pub tag: u32,
}

impl sp_session::GetSessionNumber for MockKeyOwnerProof {
	fn session(&self) -> SessionIndex {
		self.session
	}
}

impl sp_session::GetValidatorCount for MockKeyOwnerProof {
	fn validator_count(&self) -> sp_session::ValidatorCount {
		self.validator_count
	}
}

pub struct MockKeyOwnerProofSystem;

impl frame_support::traits::KeyOwnerProofSystem<(KeyTypeId, ValidatorId)>
	for MockKeyOwnerProofSystem
{
	type Proof = MockKeyOwnerProof;
	type IdentificationTuple = (AccountId, ());

	fn prove(_key: (KeyTypeId, ValidatorId)) -> Option<Self::Proof> {
		None
	}

	fn check_proof(
		key: (KeyTypeId, ValidatorId),
		proof: Self::Proof,
	) -> Option<Self::IdentificationTuple> {
		MOCK_KEY_OWNERSHIP_PROOFS.with(|m| {
			m.borrow()
				.get(&(key.0, key.1, proof))
				.cloned()
				.map(|account_id| (account_id, ()))
		})
	}
}

pub fn register_mock_key_owner_proof(
	validator_id: ValidatorId,
	proof: MockKeyOwnerProof,
	account_id: AccountId,
) {
	MOCK_KEY_OWNERSHIP_PROOFS.with(|m| {
		m.borrow_mut().insert((PARACHAIN_KEY_TYPE_ID, validator_id, proof), account_id);
	});
}

pub fn mock_reported_offences() -> Vec<(DisputesTimeSlot, DisputeOffenceKind, Vec<(AccountId, ())>)>
{
	MOCK_REPORTED_OFFENCES.with(|r| r.borrow().clone())
}

pub fn set_mock_known_offence(time_slot: DisputesTimeSlot, account_id: AccountId) {
	MOCK_KNOWN_OFFENCES.with(|r| r.borrow_mut().push((time_slot, account_id)));
}

pub fn set_mock_report_offence_result(result: MockReportResult) {
	MOCK_REPORT_OFFENCE_RESULT.with(|r| *r.borrow_mut() = result);
}

pub struct MockHandleReports;

impl crate::disputes::slashing::HandleReports<Test> for MockHandleReports {
	type ReportLongevity = SlashingReportLongevity;

	fn report_offence(
		offence: crate::disputes::slashing::SlashingOffence<(AccountId, ())>,
	) -> Result<(), OffenceError> {
		let result = MOCK_REPORT_OFFENCE_RESULT.with(|r| *r.borrow()).as_offence_result();
		if result.is_ok() {
			MOCK_REPORTED_OFFENCES.with(|r| {
				r.borrow_mut()
					.push((offence.time_slot.clone(), offence.kind, offence.offenders));
			});
		}
		result
	}

	fn is_known_offence(offenders: &[(AccountId, ())], time_slot: &DisputesTimeSlot) -> bool {
		MOCK_KNOWN_OFFENCES.with(|r| {
			let known = r.borrow();
			offenders.iter().any(|(account, _)| {
				known.iter().any(|(slot, acc)| slot == time_slot && acc == account)
			})
		})
	}

	fn submit_unsigned_slashing_report(
		_dispute_proof: polkadot_primitives::slashing::DisputeProof,
		_key_owner_proof: <Test as crate::disputes::slashing::Config>::KeyOwnerProof,
	) -> Result<(), sp_runtime::TryRuntimeError> {
		Ok(())
	}
}

impl crate::disputes::slashing::Config for Test {
	type KeyOwnerProof = MockKeyOwnerProof;
	type KeyOwnerIdentification = (AccountId, ());
	type KeyOwnerProofSystem = MockKeyOwnerProofSystem;
	type HandleReports = MockHandleReports;
	type WeightInfo = crate::disputes::slashing::TestWeightInfo;
	type BenchmarkingConfig = crate::disputes::slashing::BenchConfig<200>;
}

impl crate::scheduler::Config for Test {}

pub struct TestMessageQueueWeight;
impl pallet_message_queue::WeightInfo for TestMessageQueueWeight {
	fn ready_ring_knit() -> Weight {
		Weight::zero()
	}
	fn ready_ring_unknit() -> Weight {
		Weight::zero()
	}
	fn service_queue_base() -> Weight {
		Weight::zero()
	}
	fn service_page_base_completion() -> Weight {
		Weight::zero()
	}
	fn service_page_base_no_completion() -> Weight {
		Weight::zero()
	}
	fn service_page_item() -> Weight {
		Weight::zero()
	}
	fn set_service_head() -> Weight {
		Weight::zero()
	}
	fn bump_service_head() -> Weight {
		Weight::zero()
	}
	fn reap_page() -> Weight {
		Weight::zero()
	}
	fn execute_overweight_page_removed() -> Weight {
		Weight::zero()
	}
	fn execute_overweight_page_updated() -> Weight {
		Weight::zero()
	}
}
parameter_types! {
	pub const MessageQueueServiceWeight: Weight = Weight::from_all(500);
}

pub type MessageQueueSize = u32;

impl pallet_message_queue::Config for Test {
	type Size = MessageQueueSize;
	type RuntimeEvent = RuntimeEvent;
	type WeightInfo = TestMessageQueueWeight;
	type MessageProcessor = TestProcessMessage;
	type QueueChangeHandler = ParaInclusion;
	type QueuePausedQuery = ();
	type HeapSize = ConstU32<65536>;
	type MaxStale = ConstU32<8>;
	type ServiceWeight = MessageQueueServiceWeight;
	type IdleMaxServiceWeight = ();
}

parameter_types! {
	pub const OnDemandTrafficDefaultValue: FixedU128 = FixedU128::from_u32(1);
	// Production chains should keep this numbar around twice the
	// defined Timeslice for Coretime.
	pub const MaxHistoricalRevenue: BlockNumber = 2 * 5;
	pub const OnDemandPalletId: PalletId = PalletId(*b"py/ondmd");
}

impl on_demand::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type Currency = Balances;
	type TrafficDefaultValue = OnDemandTrafficDefaultValue;
	type WeightInfo = crate::on_demand::TestWeightInfo;
	type MaxHistoricalRevenue = MaxHistoricalRevenue;
	type PalletId = OnDemandPalletId;
}

parameter_types! {
	pub const BrokerId: u32 = 10u32;
	pub MaxXcmTransactWeight: Weight = Weight::from_parts(10_000_000, 10_000);
}

pub struct BrokerPot;
impl Get<InteriorLocation> for BrokerPot {
	fn get() -> InteriorLocation {
		unimplemented!()
	}
}

impl coretime::Config for Test {
	type RuntimeOrigin = RuntimeOrigin;
	type RuntimeEvent = RuntimeEvent;
	type BrokerId = BrokerId;
	type WeightInfo = crate::coretime::TestWeightInfo;
	type SendXcm = DummyXcmSender;
	type MaxXcmTransactWeight = MaxXcmTransactWeight;
	type BrokerPotLocation = BrokerPot;
	type AssetTransactor = ();
	type AccountToLocation = ();
}

pub struct DummyXcmSender;
impl SendXcm for DummyXcmSender {
	type Ticket = ();
	fn validate(_: &mut Option<Location>, _: &mut Option<Xcm<()>>) -> SendResult<Self::Ticket> {
		Ok(((), Assets::new()))
	}

	/// Actually carry out the delivery operation for a previously validated message sending.
	fn deliver(_ticket: Self::Ticket) -> Result<XcmHash, SendError> {
		Ok([0u8; 32])
	}
}

pub struct InclusionWeightInfo;

impl crate::inclusion::WeightInfo for InclusionWeightInfo {
	fn enact_candidate(_u: u32, _h: u32, _c: u32) -> Weight {
		Weight::from_parts(1024 * 1024, 0)
	}
}

impl crate::inclusion::Config for Test {
	type WeightInfo = InclusionWeightInfo;
	type RuntimeEvent = RuntimeEvent;
	type DisputesHandler = Disputes;
	type RewardValidators = TestRewardValidators;
	type MessageQueue = MessageQueue;
}

impl crate::paras_inherent::Config for Test {
	type WeightInfo = crate::paras_inherent::TestWeightInfo;
}

pub struct MockValidatorSet;

impl ValidatorSet<AccountId> for MockValidatorSet {
	type ValidatorId = AccountId;
	type ValidatorIdOf = ValidatorIdOf;
	fn session_index() -> SessionIndex {
		MOCK_CURRENT_SESSION.with(|s| *s.borrow())
	}
	fn validators() -> Vec<Self::ValidatorId> {
		Vec::new()
	}
}

thread_local! {
	pub static MOCK_CURRENT_SESSION: RefCell<SessionIndex> = const { RefCell::new(0) };
}

pub fn set_mock_current_session(session: SessionIndex) {
	MOCK_CURRENT_SESSION.with(|s| *s.borrow_mut() = session);
}

impl ValidatorSetWithIdentification<AccountId> for MockValidatorSet {
	type Identification = ();
	type IdentificationOf = FoolIdentificationOf;
}

pub struct FoolIdentificationOf;
impl sp_runtime::traits::Convert<AccountId, Option<()>> for FoolIdentificationOf {
	fn convert(_: AccountId) -> Option<()> {
		Some(())
	}
}

pub struct ValidatorIdOf;
impl sp_runtime::traits::Convert<AccountId, Option<AccountId>> for ValidatorIdOf {
	fn convert(a: AccountId) -> Option<AccountId> {
		Some(a)
	}
}

impl crate::session_info::Config for Test {
	type ValidatorSet = MockValidatorSet;
}

thread_local! {
	pub static DISCOVERY_AUTHORITIES: RefCell<Vec<AuthorityDiscoveryId>> = RefCell::new(Vec::new());
}

pub fn discovery_authorities() -> Vec<AuthorityDiscoveryId> {
	DISCOVERY_AUTHORITIES.with(|r| r.borrow().clone())
}

pub fn set_discovery_authorities(new: Vec<AuthorityDiscoveryId>) {
	DISCOVERY_AUTHORITIES.with(|r| *r.borrow_mut() = new);
}

impl crate::session_info::AuthorityDiscoveryConfig for Test {
	fn authorities() -> Vec<AuthorityDiscoveryId> {
		discovery_authorities()
	}
}

thread_local! {
	pub static BACKING_REWARDS: RefCell<HashMap<ValidatorIndex, usize>>
		= RefCell::new(HashMap::new());

	pub static AVAILABILITY_REWARDS: RefCell<HashMap<ValidatorIndex, usize>>
		= RefCell::new(HashMap::new());

	pub static DISABLED_VALIDATORS: RefCell<Vec<u32>> = RefCell::new(vec![]);
}

pub fn backing_rewards() -> HashMap<ValidatorIndex, usize> {
	BACKING_REWARDS.with(|r| r.borrow().clone())
}

pub fn availability_rewards() -> HashMap<ValidatorIndex, usize> {
	AVAILABILITY_REWARDS.with(|r| r.borrow().clone())
}

pub fn disabled_validators() -> Vec<u32> {
	DISABLED_VALIDATORS.with(|r| r.borrow().clone())
}

parameter_types! {
	pub static Processed: Vec<(ParaId, UpwardMessage)> = vec![];
}

/// An implementation of a UMP sink that just records which messages were processed.
///
/// A message's weight is defined by the first 4 bytes of its data, which we decode into a
/// `u32`.
pub struct TestProcessMessage;
impl ProcessMessage for TestProcessMessage {
	type Origin = AggregateMessageOrigin;

	fn process_message(
		message: &[u8],
		origin: AggregateMessageOrigin,
		meter: &mut WeightMeter,
		_id: &mut [u8; 32],
	) -> Result<bool, ProcessMessageError> {
		let para = match origin {
			AggregateMessageOrigin::Ump(UmpQueueId::Para(p)) => p,
		};

		let required = match u32::decode(&mut &message[..]) {
			Ok(w) => Weight::from_parts(w as u64, w as u64),
			Err(_) => return Err(ProcessMessageError::Corrupt), // same as the real `ProcessMessage`
		};
		if meter.try_consume(required).is_err() {
			return Err(ProcessMessageError::Overweight(required));
		}

		let mut processed = Processed::get();
		processed.push((para, message.to_vec()));
		Processed::set(processed);
		Ok(true)
	}
}

pub struct TestRewardValidators;

impl inclusion::RewardValidators for TestRewardValidators {
	fn reward_backing(v: impl IntoIterator<Item = ValidatorIndex>) {
		BACKING_REWARDS.with(|r| {
			let mut r = r.borrow_mut();
			for i in v {
				*r.entry(i).or_insert(0) += 1;
			}
		})
	}
	fn reward_bitfields(v: impl IntoIterator<Item = ValidatorIndex>) {
		AVAILABILITY_REWARDS.with(|r| {
			let mut r = r.borrow_mut();
			for i in v {
				*r.entry(i).or_insert(0) += 1;
			}
		})
	}
}

/// Create a new set of test externalities.
pub fn new_test_ext(state: MockGenesisConfig) -> TestExternalities {
	use sp_keystore::{testing::MemoryKeystore, KeystoreExt, KeystorePtr};
	use std::sync::Arc;

	sp_tracing::try_init_simple();

	BACKING_REWARDS.with(|r| r.borrow_mut().clear());
	AVAILABILITY_REWARDS.with(|r| r.borrow_mut().clear());

	let mut t = state.system.build_storage().unwrap();
	state.configuration.assimilate_storage(&mut t).unwrap();
	state.paras.assimilate_storage(&mut t).unwrap();

	let mut ext: TestExternalities = t.into();
	ext.register_extension(KeystoreExt(Arc::new(MemoryKeystore::new()) as KeystorePtr));

	ext
}

#[derive(Default)]
pub struct MockGenesisConfig {
	pub system: frame_system::GenesisConfig<Test>,
	pub configuration: crate::configuration::GenesisConfig<Test>,
	pub paras: crate::paras::GenesisConfig<Test>,
}

pub fn assert_last_event(generic_event: RuntimeEvent) {
	let events = frame_system::Pallet::<Test>::events();
	let system_event: <Test as frame_system::Config>::RuntimeEvent = generic_event.into();
	// compare to the last event record
	let frame_system::EventRecord { event, .. } = &events[events.len() - 1];
	assert_eq!(event, &system_event);
}

pub fn assert_last_events<E>(generic_events: E)
where
	E: DoubleEndedIterator<Item = RuntimeEvent> + ExactSizeIterator,
{
	for (i, (got, want)) in frame_system::Pallet::<Test>::events()
		.into_iter()
		.rev()
		.map(|e| e.event)
		.zip(generic_events.rev().map(<Test as frame_system::Config>::RuntimeEvent::from))
		.rev()
		.enumerate()
	{
		assert_eq!((i, got), (i, want));
	}
}

pub(crate) fn register_parachain_with_balance(id: ParaId, balance: Balance) {
	let validation_code: ValidationCode = vec![1].into();
	assert_ok!(Paras::schedule_para_initialize(
		id,
		crate::paras::ParaGenesisArgs {
			para_kind: ParaKind::Parachain,
			genesis_head: vec![1].into(),
			validation_code: validation_code.clone(),
		},
	));

	assert_ok!(Paras::add_trusted_validation_code(RuntimeOrigin::root(), validation_code));
	<Test as crate::hrmp::Config>::Currency::make_free_balance_be(
		&id.into_account_truncating(),
		balance,
	);
}

pub(crate) fn register_parachain(id: ParaId) {
	register_parachain_with_balance(id, 1000);
}

pub(crate) fn deregister_parachain(id: ParaId) {
	assert_ok!(Paras::schedule_para_cleanup(id));
}

/// Calls `schedule_para_cleanup` in a new storage transactions, since it assumes rollback on error.
pub(crate) fn try_deregister_parachain(id: ParaId) -> crate::DispatchResult {
	frame_support::storage::transactional::with_storage_layer(|| Paras::schedule_para_cleanup(id))
}

pub(crate) fn set_disabled_validators(disabled: Vec<u32>) {
	DISABLED_VALIDATORS.with(|d| *d.borrow_mut() = disabled)
}
