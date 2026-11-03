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

//! The Westend runtime. This can be compiled with `#[no_std]`, ready for Wasm.

#![cfg_attr(not(feature = "std"), no_std)]
// `#[frame_support::runtime]!` does a lot of recursion and requires us to increase the limit.
#![recursion_limit = "512"]

extern crate alloc;

use alloc::{
	collections::{btree_map::BTreeMap, vec_deque::VecDeque},
	vec,
	vec::Vec,
};
use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use frame_support::{
	derive_impl,
	dynamic_params::{dynamic_pallet_params, dynamic_params},
	genesis_builder_helper::{build_state, get_preset},
	parameter_types,
	traits::{
		fungible::HoldConsideration, ConstU32, Contains, EnsureOriginWithArg, InstanceFilter,
		KeyOwnerProofSystem, LinearStoragePrice, ProcessMessage, ProcessMessageError,
		VariantCountOf, WithdrawReasons,
	},
	weights::{ConstantMultiplier, WeightMeter},
	PalletId,
};
use frame_system::{EnsureRoot, EnsureSigned};
use pallet_grandpa::{fg_primitives, AuthorityId as GrandpaId};
use pallet_identity::legacy::IdentityInfo;
use pallet_session::historical as session_historical;
use pallet_staking_async_ah_client as ah_client;
use pallet_staking_async_rc_client as rc_client;
use pallet_transaction_payment::{FeeDetails, FungibleAdapter, RuntimeDispatchInfo};
use polkadot_primitives::{
	async_backing::Constraints, slashing, AccountId, AccountIndex, ApprovalVotingParams, Balance,
	BlockNumber, CandidateEvent, CandidateHash,
	CommittedCandidateReceiptV2 as CommittedCandidateReceipt, CoreIndex, CoreState, DisputeState,
	ExecutorParams, GroupRotationInfo, Hash, Id as ParaId, InboundDownwardMessage,
	InboundHrmpMessage, Moment, NodeFeatures, Nonce, OccupiedCoreAssumption,
	PersistedValidationData, PvfCheckStatement, ScrapedOnChainVotes, SessionInfo, Signature,
	ValidationCode, ValidationCodeHash, ValidatorId, ValidatorIndex, ValidatorSignature,
	PARACHAIN_KEY_TYPE_ID,
};
use polkadot_runtime_common::{
	assigned_slots, auctions, crowdloan, identity_migrator, impl_runtime_weights,
	impls::{ToAuthor, VersionedLocatableAsset},
	paras_registrar, paras_sudo_wrapper, prod_or_fast, slots,
	traits::OnSwap,
	BlockHashCount, SlowAdjustingFeeUpdate,
};
use polkadot_runtime_parachains::{
	configuration as parachains_configuration,
	configuration::ActiveConfigHrmpChannelSizeAndCapacityRatio,
	coretime, disputes as parachains_disputes,
	disputes::slashing as parachains_slashing,
	dmp as parachains_dmp, hrmp as parachains_hrmp, inclusion as parachains_inclusion,
	inclusion::{AggregateMessageOrigin, UmpQueueId},
	initializer as parachains_initializer, on_demand as parachains_on_demand,
	origin as parachains_origin, paras as parachains_paras,
	paras_inherent as parachains_paras_inherent, reward_points as parachains_reward_points,
	runtime_api_impl::{
		v13 as parachains_runtime_api_impl, vstaging as parachains_staging_runtime_api_impl,
	},
	scheduler as parachains_scheduler, session_info as parachains_session_info,
	shared as parachains_shared,
};
use scale_info::TypeInfo;
use sp_authority_discovery::AuthorityId as AuthorityDiscoveryId;
use sp_consensus_beefy::{
	ecdsa_crypto::{AuthorityId as BeefyId, Signature as BeefySignature},
	mmr::{BeefyDataProvider, MmrLeafVersion},
};
use sp_core::{ConstUint, OpaqueMetadata, H256};
#[cfg(any(feature = "std", test))]
pub use sp_runtime::BuildStorage;
use sp_runtime::{
	generic, impl_opaque_keys,
	traits::{
		AccountIdConversion, BlakeTwo256, Block as BlockT, ConvertInto, Get, Keccak256, OpaqueKeys,
		SaturatedConversion, Verify,
	},
	transaction_validity::{TransactionPriority, TransactionSource, TransactionValidity},
	ApplyExtrinsicResult, FixedU128, KeyTypeId, Percent,
};
use sp_staking::{EraIndex, SessionIndex};
use sp_version::RuntimeVersion;
use xcm::{
	latest::prelude::*, Version as XcmVersion, VersionedAsset, VersionedAssetId, VersionedAssets,
	VersionedLocation, VersionedXcm,
};
use xcm_runtime_apis::{
	dry_run::{CallDryRunEffects, Error as XcmDryRunApiError, XcmDryRunEffects},
	fees::Error as XcmPaymentApiError,
};

pub use frame_system::Call as SystemCall;
pub use pallet_balances::Call as BalancesCall;
pub use pallet_timestamp::Call as TimestampCall;

/// Constant values used within the runtime.
use westend_runtime_constants::{
	currency::*,
	fee::*,
	system_parachain::{
		accumulate_forward::*, coretime::TIMESLICE_PERIOD, dap::*, ASSET_HUB_ID, BROKER_ID,
	},
	time::*,
};

mod genesis_config_presets;
mod weights;
pub mod xcm_config;

// Implemented types.
mod impls;
use impls::ToParachainIdentityReaper;

// XCM configuration.
use xcm_config::XcmConfig;

#[cfg(test)]
mod tests;

impl_runtime_weights!(westend_runtime_constants);

// Make the WASM binary available.
#[cfg(feature = "std")]
include!(concat!(env!("OUT_DIR"), "/wasm_binary.rs"));

#[cfg(feature = "std")]
pub mod fast_runtime_binary {
	include!(concat!(env!("OUT_DIR"), "/fast_runtime_binary.rs"));
}

/// Runtime version (Westend).
#[sp_version::runtime_version]
pub const VERSION: RuntimeVersion = RuntimeVersion {
	spec_name: alloc::borrow::Cow::Borrowed("westend"),
	impl_name: alloc::borrow::Cow::Borrowed("parity-westend"),
	authoring_version: 2,
	spec_version: 1_025_002,
	impl_version: 0,
	apis: RUNTIME_API_VERSIONS,
	transaction_version: 27,
	system_version: 1,
};

/// The BABE epoch configuration at genesis.
pub const BABE_GENESIS_EPOCH_CONFIG: sp_consensus_babe::BabeEpochConfiguration =
	sp_consensus_babe::BabeEpochConfiguration {
		c: PRIMARY_PROBABILITY,
		allowed_slots: sp_consensus_babe::AllowedSlots::PrimaryAndSecondaryVRFSlots,
	};

/// A type to identify calls to the Identity pallet. These will be filtered to prevent invocation,
/// locking the state of the pallet and preventing further updates to identities and sub-identities.
/// The locked state will be the genesis state of a new system chain and then removed from the Relay
/// Chain.
pub struct IsIdentityCall;
impl Contains<RuntimeCall> for IsIdentityCall {
	fn contains(c: &RuntimeCall) -> bool {
		matches!(c, RuntimeCall::Identity(_))
	}
}

parameter_types! {
	pub const Version: RuntimeVersion = VERSION;
	pub const SS58Prefix: u8 = 42;
	/// Maximum length of a relay-chain block is up to 10 MiB.
	pub BlockLength: frame_system::limits::BlockLength =
		frame_system::limits::BlockLength::builder()
			.max_length(10 * 1024 * 1024)
			.modify_max_length_for_class(
				frame_support::dispatch::DispatchClass::Normal,
				|m| { *m = polkadot_runtime_common::NORMAL_DISPATCH_RATIO * *m },
			)
			.build();
}

#[derive_impl(frame_system::config_preludes::RelayChainDefaultConfig)]
impl frame_system::Config for Runtime {
	type BlockWeights = BlockWeights;
	type BlockLength = BlockLength;
	type Nonce = Nonce;
	type Hash = Hash;
	type AccountId = AccountId;
	type Block = Block;
	type BlockHashCount = BlockHashCount;
	type DbWeight = RocksDbWeight;
	type Version = Version;
	type AccountData = pallet_balances::AccountData<Balance>;
	type SystemWeightInfo = weights::frame_system::WeightInfo<Runtime>;
	type ExtensionsWeightInfo = weights::frame_system_extensions::WeightInfo<Runtime>;
	type SS58Prefix = SS58Prefix;
	type MaxConsumers = frame_support::traits::ConstU32<16>;
	type MultiBlockMigrator = MultiBlockMigrations;
	type SingleBlockMigrations = Migrations;
}

parameter_types! {
	pub MaximumSchedulerWeight: frame_support::weights::Weight = Perbill::from_percent(80) *
		BlockWeights::get().max_block;
	pub const MaxScheduledPerBlock: u32 = 50;
	pub const NoPreimagePostponement: Option<u32> = Some(10);
}

impl pallet_scheduler::Config for Runtime {
	type RuntimeOrigin = RuntimeOrigin;
	type RuntimeEvent = RuntimeEvent;
	type PalletsOrigin = OriginCaller;
	type RuntimeCall = RuntimeCall;
	type MaximumWeight = MaximumSchedulerWeight;
	type ScheduleOrigin = EnsureRoot<AccountId>;
	type MaxScheduledPerBlock = MaxScheduledPerBlock;
	type WeightInfo = weights::pallet_scheduler::WeightInfo<Runtime>;
	type OriginPrivilegeCmp = frame_support::traits::EqualPrivilegeOnly;
	type Preimages = Preimage;
	type BlockNumberProvider = System;
}

parameter_types! {
	pub const PreimageBaseDeposit: Balance = deposit(2, 64);
	pub const PreimageByteDeposit: Balance = deposit(0, 1);
	pub const PreimageHoldReason: RuntimeHoldReason = RuntimeHoldReason::Preimage(pallet_preimage::HoldReason::Preimage);
}

/// Dynamic params that can be adjusted at runtime.
#[dynamic_params(RuntimeParameters, pallet_parameters::Parameters::<Runtime>)]
pub mod dynamic_params {
	use super::*;

	/// Parameters used to calculate era payouts, see
	/// [`polkadot_runtime_common::impls::EraPayoutParams`].
	#[dynamic_pallet_params]
	#[codec(index = 0)]
	pub mod inflation {
		/// Minimum inflation rate used to calculate era payouts.
		#[codec(index = 0)]
		pub static MinInflation: Perquintill = Perquintill::from_rational(25u64, 1000u64);

		/// Maximum inflation rate used to calculate era payouts.
		#[codec(index = 1)]
		pub static MaxInflation: Perquintill = Perquintill::from_rational(10u64, 100u64);

		/// Ideal stake ratio used to calculate era payouts.
		#[codec(index = 2)]
		pub static IdealStake: Perquintill = Perquintill::from_rational(50u64, 100u64);

		/// Falloff used to calculate era payouts.
		#[codec(index = 3)]
		pub static Falloff: Perquintill = Perquintill::from_rational(50u64, 1000u64);

		/// Whether to use auction slots or not in the calculation of era payouts. If set to true,
		/// the `legacy_auction_proportion` of 60% will be used in the calculation of era payouts.
		#[codec(index = 4)]
		pub static UseAuctionSlots: bool = false;
	}
}

#[cfg(feature = "runtime-benchmarks")]
impl Default for RuntimeParameters {
	fn default() -> Self {
		RuntimeParameters::Inflation(dynamic_params::inflation::Parameters::MinInflation(
			dynamic_params::inflation::MinInflation,
			Some(Perquintill::from_rational(25u64, 1000u64)),
		))
	}
}

impl pallet_parameters::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type RuntimeParameters = RuntimeParameters;
	type AdminOrigin = DynamicParameterOrigin;
	type WeightInfo = weights::pallet_parameters::WeightInfo<Runtime>;
}

/// Defines what origin can modify which dynamic parameters.
pub struct DynamicParameterOrigin;
impl EnsureOriginWithArg<RuntimeOrigin, RuntimeParametersKey> for DynamicParameterOrigin {
	type Success = ();

	fn try_origin(
		origin: RuntimeOrigin,
		key: &RuntimeParametersKey,
	) -> Result<Self::Success, RuntimeOrigin> {
		use crate::RuntimeParametersKey::*;

		match key {
			Inflation(_) => frame_system::ensure_root(origin.clone()),
		}
		.map_err(|_| origin)
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn try_successful_origin(_key: &RuntimeParametersKey) -> Result<RuntimeOrigin, ()> {
		// Provide the origin for the parameter returned by `Default`:
		Ok(RuntimeOrigin::root())
	}
}

impl pallet_preimage::Config for Runtime {
	type WeightInfo = weights::pallet_preimage::WeightInfo<Runtime>;
	type RuntimeEvent = RuntimeEvent;
	type Currency = Balances;
	type ManagerOrigin = EnsureRoot<AccountId>;
	type Consideration = HoldConsideration<
		AccountId,
		Balances,
		PreimageHoldReason,
		LinearStoragePrice<PreimageBaseDeposit, PreimageByteDeposit, Balance>,
	>;
}

parameter_types! {
	pub const EpochDuration: u64 = prod_or_fast!(
		EPOCH_DURATION_IN_SLOTS as u64,
		2 * MINUTES as u64
	);
	pub const ExpectedBlockTime: Moment = MILLISECS_PER_BLOCK;
	pub const ReportLongevity: u64 =
		BondingDuration::get() as u64 * SessionsPerEra::get() as u64 * EpochDuration::get();
}

impl pallet_babe::Config for Runtime {
	type EpochDuration = EpochDuration;
	type ExpectedBlockTime = ExpectedBlockTime;

	// session module is the trigger
	type EpochChangeTrigger = pallet_babe::ExternalTrigger;

	type DisabledValidators = Session;

	type WeightInfo = ();

	type MaxAuthorities = MaxAuthorities;
	type MaxNominators = ConstU32<0>;

	type KeyOwnerProof = sp_session::MembershipProof;

	type EquivocationReportSystem =
		pallet_babe::EquivocationReportSystem<Self, Offences, Historical, ReportLongevity>;
}

parameter_types! {
	pub const IndexDeposit: Balance = 100 * CENTS;
}

impl pallet_indices::Config for Runtime {
	type AccountIndex = AccountIndex;
	type Currency = Balances;
	type Deposit = IndexDeposit;
	type RuntimeEvent = RuntimeEvent;
	type WeightInfo = weights::pallet_indices::WeightInfo<Runtime>;
}

parameter_types! {
	pub const ExistentialDeposit: Balance = EXISTENTIAL_DEPOSIT;
	pub const MaxLocks: u32 = 50;
	pub const MaxReserves: u32 = 50;
}

impl pallet_balances::Config for Runtime {
	type Balance = Balance;
	type DustRemoval = AccumulateForward;
	type RuntimeEvent = RuntimeEvent;
	type ExistentialDeposit = ExistentialDeposit;
	type AccountStore = System;
	type MaxLocks = MaxLocks;
	type MaxReserves = MaxReserves;
	type ReserveIdentifier = [u8; 8];
	type WeightInfo = weights::pallet_balances::WeightInfo<Runtime>;
	type RuntimeHoldReason = RuntimeHoldReason;
	type RuntimeFreezeReason = RuntimeFreezeReason;
	type FreezeIdentifier = RuntimeFreezeReason;
	type MaxFreezes = VariantCountOf<RuntimeFreezeReason>;
	type DoneSlashHandler = ();
}

parameter_types! {
	pub const BeefySetIdSessionEntries: u32 = BondingDuration::get() * SessionsPerEra::get();
}

impl pallet_beefy::Config for Runtime {
	type BeefyId = BeefyId;
	type MaxAuthorities = MaxAuthorities;
	type MaxNominators = ConstU32<0>;
	type MaxSetIdSessionEntries = BeefySetIdSessionEntries;
	type OnNewValidatorSet = BeefyMmrLeaf;
	type AncestryHelper = BeefyMmrLeaf;
	type WeightInfo = ();
	type KeyOwnerProof = sp_session::MembershipProof;
	type EquivocationReportSystem =
		pallet_beefy::EquivocationReportSystem<Self, Offences, Historical, ReportLongevity>;
}

impl pallet_mmr::Config for Runtime {
	const INDEXING_PREFIX: &'static [u8] = mmr::INDEXING_PREFIX;
	type Hashing = Keccak256;
	type OnNewRoot = pallet_beefy_mmr::DepositBeefyDigest<Runtime>;
	type LeafData = pallet_beefy_mmr::Pallet<Runtime>;
	type BlockHashProvider = pallet_mmr::DefaultBlockHashProvider<Runtime>;
	type WeightInfo = weights::pallet_mmr::WeightInfo<Runtime>;
	#[cfg(feature = "runtime-benchmarks")]
	type BenchmarkHelper = parachains_paras::benchmarking::mmr_setup::MmrSetup<Runtime>;
}

/// MMR helper types.
mod mmr {
	use super::Runtime;
	pub use pallet_mmr::primitives::*;

	pub type Leaf = <<Runtime as pallet_mmr::Config>::LeafData as LeafDataProvider>::LeafData;
	pub type Hashing = <Runtime as pallet_mmr::Config>::Hashing;
	pub type Hash = <Hashing as sp_runtime::traits::Hash>::Output;
}

parameter_types! {
	pub LeafVersion: MmrLeafVersion = MmrLeafVersion::new(0, 0);
}

/// A BEEFY data provider that merkelizes all the parachain heads at the current block
/// (sorted by their parachain id).
pub struct ParaHeadsRootProvider;
impl BeefyDataProvider<H256> for ParaHeadsRootProvider {
	fn extra_data() -> H256 {
		let para_heads: Vec<(u32, Vec<u8>)> =
			parachains_paras::Pallet::<Runtime>::sorted_para_heads();
		binary_merkle_tree::merkle_root::<mmr::Hashing, _>(
			para_heads.into_iter().map(|pair| pair.encode()),
		)
		.into()
	}
}

impl pallet_beefy_mmr::Config for Runtime {
	type LeafVersion = LeafVersion;
	type BeefyAuthorityToMerkleLeaf = pallet_beefy_mmr::BeefyEcdsaToEthereum;
	type LeafExtra = H256;
	type BeefyDataProvider = ParaHeadsRootProvider;
	type WeightInfo = weights::pallet_beefy_mmr::WeightInfo<Runtime>;
}

parameter_types! {
	pub const TransactionByteFee: Balance = 10 * MILLICENTS;
	/// This value increases the priority of `Operational` transactions by adding
	/// a "virtual tip" that's equal to the `OperationalFeeMultiplier * final_fee`.
	pub const OperationalFeeMultiplier: u8 = 5;
	/// Percentage of fees that go to the accumulation account.
	/// The remainder goes to block author. Tips always go 100% to author.
	pub const AccumulateForwardFeePercent: Percent = Percent::from_percent(100);
}

/// Fee handler that splits fees between the accumulation account and block author.
type DealWithFeesAccumulate = pallet_accumulate_and_forward::DealWithFeesSplit<
	Runtime,
	AccumulateForwardFeePercent,
	ToAuthor<Runtime>,
>;

impl pallet_transaction_payment::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type OnChargeTransaction = FungibleAdapter<Balances, DealWithFeesAccumulate>;
	type OperationalFeeMultiplier = OperationalFeeMultiplier;
	type WeightToFee = WeightToFee;
	type LengthToFee = ConstantMultiplier<Balance, TransactionByteFee>;
	type FeeMultiplierUpdate = SlowAdjustingFeeUpdate<Self>;
	type WeightInfo = weights::pallet_transaction_payment::WeightInfo<Runtime>;
}

parameter_types! {
	pub const MinimumPeriod: u64 = SLOT_DURATION / 2;
}
impl pallet_timestamp::Config for Runtime {
	type Moment = u64;
	type OnTimestampSet = Babe;
	type MinimumPeriod = MinimumPeriod;
	type WeightInfo = weights::pallet_timestamp::WeightInfo<Runtime>;
}

impl pallet_authorship::Config for Runtime {
	type FindAuthor = pallet_session::FindAccountFromAuthorIndex<Self, Babe>;
	type EventHandler = StakingAhClient;
}

parameter_types! {
	pub const Period: BlockNumber = 10 * MINUTES;
	pub const Offset: BlockNumber = 0;
}

impl_opaque_keys! {
	pub struct SessionKeys {
		pub grandpa: Grandpa,
		pub babe: Babe,
		pub para_validator: Initializer,
		pub para_assignment: ParaSessionInfo,
		pub authority_discovery: AuthorityDiscovery,
		pub beefy: Beefy,
	}
}

impl pallet_session::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type ValidatorId = AccountId;
	type ValidatorIdOf = ConvertInto;
	type ShouldEndSession = Babe;
	type NextSessionRotation = Babe;
	type SessionManager = session_historical::NoteHistoricalRoot<Self, StakingAhClient>;
	type SessionHandler = <SessionKeys as OpaqueKeys>::KeyTypeIdProviders;
	type Keys = SessionKeys;
	type DisablingStrategy = pallet_session::disabling::UpToLimitWithReEnablingDisablingStrategy;
	type WeightInfo = weights::pallet_session::WeightInfo<Runtime>;
	type Currency = Balances;
	type KeyDeposit = ();
}

impl pallet_session::historical::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type FullIdentification = sp_staking::Exposure<AccountId, Balance>;
	type FullIdentificationOf = ah_client::DefaultExposureOf<Self>;
}

#[derive(Encode, Decode)]
enum AssetHubRuntimePallets<AccountId> {
	// Audit: `StakingRcClient` in asset-hub-westend
	#[codec(index = 89)]
	RcClient(RcClientCalls<AccountId>),
}

#[derive(Encode, Decode)]
enum RcClientCalls<AccountId> {
	#[codec(index = 0)]
	RelaySessionReport(rc_client::SessionReport<AccountId>),
	#[codec(index = 1)]
	RelayNewOffencePaged(Vec<(SessionIndex, rc_client::Offence<AccountId>)>),
}

pub struct AssetHubLocation;
impl Get<Location> for AssetHubLocation {
	fn get() -> Location {
		Location::new(0, [Junction::Parachain(ASSET_HUB_ID)])
	}
}

pub struct EnsureAssetHub;
impl frame_support::traits::EnsureOrigin<RuntimeOrigin> for EnsureAssetHub {
	type Success = ();
	fn try_origin(o: RuntimeOrigin) -> Result<Self::Success, RuntimeOrigin> {
		match <RuntimeOrigin as Into<Result<parachains_origin::Origin, RuntimeOrigin>>>::into(
			o.clone(),
		) {
			Ok(parachains_origin::Origin::Parachain(id)) if id == ASSET_HUB_ID.into() => Ok(()),
			_ => Err(o),
		}
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn try_successful_origin() -> Result<RuntimeOrigin, ()> {
		Ok(RuntimeOrigin::root())
	}
}

pub struct SessionReportToXcm;
impl sp_runtime::traits::Convert<rc_client::SessionReport<AccountId>, Xcm<()>>
	for SessionReportToXcm
{
	fn convert(a: rc_client::SessionReport<AccountId>) -> Xcm<()> {
		Xcm(vec![
			Instruction::UnpaidExecution {
				weight_limit: WeightLimit::Unlimited,
				check_origin: None,
			},
			Instruction::Transact {
				origin_kind: OriginKind::Superuser,
				fallback_max_weight: None,
				call: AssetHubRuntimePallets::RcClient(RcClientCalls::RelaySessionReport(a))
					.encode()
					.into(),
			},
		])
	}
}

pub struct QueuedOffenceToXcm;
impl sp_runtime::traits::Convert<Vec<ah_client::QueuedOffenceOf<Runtime>>, Xcm<()>>
	for QueuedOffenceToXcm
{
	fn convert(offences: Vec<ah_client::QueuedOffenceOf<Runtime>>) -> Xcm<()> {
		Xcm(vec![
			Instruction::UnpaidExecution {
				weight_limit: WeightLimit::Unlimited,
				check_origin: None,
			},
			Instruction::Transact {
				origin_kind: OriginKind::Superuser,
				fallback_max_weight: None,
				call: AssetHubRuntimePallets::RcClient(RcClientCalls::RelayNewOffencePaged(
					offences,
				))
				.encode()
				.into(),
			},
		])
	}
}

pub struct StakingXcmToAssetHub;
impl ah_client::SendToAssetHub for StakingXcmToAssetHub {
	type AccountId = AccountId;

	fn relay_session_report(
		session_report: rc_client::SessionReport<Self::AccountId>,
	) -> Result<(), ()> {
		rc_client::XCMSender::<
			xcm_config::XcmRouter,
			AssetHubLocation,
			rc_client::SessionReport<AccountId>,
			SessionReportToXcm,
		>::send(session_report)
	}

	fn relay_new_offence_paged(
		offences: Vec<ah_client::QueuedOffenceOf<Runtime>>,
	) -> Result<(), ()> {
		rc_client::XCMSender::<
			xcm_config::XcmRouter,
			AssetHubLocation,
			Vec<ah_client::QueuedOffenceOf<Runtime>>,
			QueuedOffenceToXcm,
		>::send(offences)
	}
}

parameter_types! {
	// Maximum winners that can be chosen as active validators
	pub const MaxActiveValidators: u32 = 1000;
}

impl ah_client::Config for Runtime {
	type CurrencyBalance = Balance;
	type AssetHubOrigin =
		frame_support::traits::EitherOfDiverse<EnsureRoot<AccountId>, EnsureAssetHub>;
	type AdminOrigin = EnsureRoot<AccountId>;
	type SessionInterface = Session;
	type SendToAssetHub = StakingXcmToAssetHub;
	type MinimumValidatorSetSize = ConstU32<1>;
	type UnixTime = Timestamp;
	type PointsPerBlock = ConstU32<20>;
	type MaxOffenceBatchSize = ConstU32<50>;
	type Fallback = ();
	type MaximumValidatorsWithPoints = ConstU32<{ MaxActiveValidators::get() * 4 }>;
	type MaxSessionReportRetries = ConstU32<5>;
}

parameter_types! {
	pub const MaxAuthorities: u32 = 100_000;
}

impl pallet_offences::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type IdentificationTuple = session_historical::IdentificationTuple<Self>;
	type OnOffenceHandler = StakingAhClient;
}

impl pallet_authority_discovery::Config for Runtime {
	type MaxAuthorities = MaxAuthorities;
}

parameter_types! {
	pub const NposSolutionPriority: TransactionPriority = TransactionPriority::max_value() / 2;
}

parameter_types! {
	// 2 eras for unbonding (12 hours).
	pub const BondingDuration: EraIndex = 2;
	// Six sessions in an era (6 hours).
	pub const SessionsPerEra: SessionIndex = prod_or_fast!(6, 2);
	pub const MaxSetIdSessionEntries: u32 = BondingDuration::get() * SessionsPerEra::get();
}

impl pallet_grandpa::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;

	type WeightInfo = ();
	type MaxAuthorities = MaxAuthorities;
	type MaxNominators = ConstU32<0>;
	type MaxSetIdSessionEntries = MaxSetIdSessionEntries;

	type KeyOwnerProof = sp_session::MembershipProof;

	type EquivocationReportSystem =
		pallet_grandpa::EquivocationReportSystem<Self, Offences, Historical, ReportLongevity>;
}

impl frame_system::offchain::SigningTypes for Runtime {
	type Public = <Signature as Verify>::Signer;
	type Signature = Signature;
}

impl<C> frame_system::offchain::CreateTransactionBase<C> for Runtime
where
	RuntimeCall: From<C>,
{
	type RuntimeCall = RuntimeCall;
	type Extrinsic = UncheckedExtrinsic;
}

impl<LocalCall> frame_system::offchain::CreateTransaction<LocalCall> for Runtime
where
	RuntimeCall: From<LocalCall>,
{
	type Extension = TxExtension;

	fn create_transaction(call: RuntimeCall, extension: TxExtension) -> UncheckedExtrinsic {
		UncheckedExtrinsic::new_transaction(call, extension)
	}
}

/// Submits a transaction with the node's public and signature type. Adheres to the signed extension
/// format of the chain.
impl<LocalCall> frame_system::offchain::CreateSignedTransaction<LocalCall> for Runtime
where
	RuntimeCall: From<LocalCall>,
{
	fn create_signed_transaction<
		C: frame_system::offchain::AppCrypto<Self::Public, Self::Signature>,
	>(
		call: RuntimeCall,
		public: <Signature as Verify>::Signer,
		account: AccountId,
		nonce: <Runtime as frame_system::Config>::Nonce,
	) -> Option<UncheckedExtrinsic> {
		use sp_runtime::traits::StaticLookup;
		// take the biggest period possible.
		let period =
			BlockHashCount::get().checked_next_power_of_two().map(|c| c / 2).unwrap_or(2) as u64;

		let current_block = System::block_number()
			.saturated_into::<u64>()
			// The `System::block_number` is initialized with `n+1`,
			// so the actual block number is `n`.
			.saturating_sub(1);
		let tip = 0;
		let tx_ext: TxExtension = (
			frame_system::AuthorizeCall::<Runtime>::new(),
			frame_system::CheckNonZeroSender::<Runtime>::new(),
			frame_system::CheckSpecVersion::<Runtime>::new(),
			frame_system::CheckTxVersion::<Runtime>::new(),
			frame_system::CheckGenesis::<Runtime>::new(),
			frame_system::CheckMortality::<Runtime>::from(generic::Era::mortal(
				period,
				current_block,
			)),
			frame_system::CheckNonce::<Runtime>::from(nonce),
			frame_system::CheckWeight::<Runtime>::new(),
			pallet_transaction_payment::ChargeTransactionPayment::<Runtime>::from(tip),
			frame_metadata_hash_extension::CheckMetadataHash::<Runtime>::new(true),
			frame_system::WeightReclaim::<Runtime>::new(),
		)
			.into();
		let raw_payload = SignedPayload::new(call, tx_ext)
			.map_err(|e| {
				log::warn!("Unable to create signed payload: {:?}", e);
			})
			.ok()?;
		let signature = raw_payload.using_encoded(|payload| C::sign(payload, public))?;
		let (call, tx_ext, _) = raw_payload.deconstruct();
		let address = <Runtime as frame_system::Config>::Lookup::unlookup(account);
		let transaction = UncheckedExtrinsic::new_signed(call, address, signature, tx_ext);
		Some(transaction)
	}
}

impl<LocalCall> frame_system::offchain::CreateBare<LocalCall> for Runtime
where
	RuntimeCall: From<LocalCall>,
{
	fn create_bare(call: RuntimeCall) -> UncheckedExtrinsic {
		UncheckedExtrinsic::new_bare(call)
	}
}

impl<LocalCall> frame_system::offchain::CreateAuthorizedTransaction<LocalCall> for Runtime
where
	RuntimeCall: From<LocalCall>,
{
	fn create_extension() -> Self::Extension {
		(
			frame_system::AuthorizeCall::<Runtime>::new(),
			frame_system::CheckNonZeroSender::<Runtime>::new(),
			frame_system::CheckSpecVersion::<Runtime>::new(),
			frame_system::CheckTxVersion::<Runtime>::new(),
			frame_system::CheckGenesis::<Runtime>::new(),
			frame_system::CheckMortality::<Runtime>::from(generic::Era::Immortal),
			frame_system::CheckNonce::<Runtime>::from(0),
			frame_system::CheckWeight::<Runtime>::new(),
			pallet_transaction_payment::ChargeTransactionPayment::<Runtime>::from(0),
			frame_metadata_hash_extension::CheckMetadataHash::<Runtime>::new(false),
			frame_system::WeightReclaim::<Runtime>::new(),
		)
	}
}

parameter_types! {
	// Minimum 100 bytes/KSM deposited (1 CENT/byte)
	pub const BasicDeposit: Balance = 1000 * CENTS;       // 258 bytes on-chain
	pub const ByteDeposit: Balance = deposit(0, 1);
	pub const UsernameDeposit: Balance = deposit(0, 32);
	pub const SubAccountDeposit: Balance = 200 * CENTS;   // 53 bytes on-chain
	pub const MaxSubAccounts: u32 = 100;
	pub const MaxAdditionalFields: u32 = 100;
	pub const MaxRegistrars: u32 = 20;
}

impl pallet_identity::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type Currency = Balances;
	type Slashed = ();
	type BasicDeposit = BasicDeposit;
	type ByteDeposit = ByteDeposit;
	type UsernameDeposit = UsernameDeposit;
	type SubAccountDeposit = SubAccountDeposit;
	type MaxSubAccounts = MaxSubAccounts;
	type IdentityInformation = IdentityInfo<MaxAdditionalFields>;
	type MaxRegistrars = MaxRegistrars;
	type ForceOrigin = EnsureRoot<Self::AccountId>;
	type RegistrarOrigin = EnsureRoot<Self::AccountId>;
	type OffchainSignature = Signature;
	type SigningPublicKey = <Signature as Verify>::Signer;
	type UsernameAuthorityOrigin = EnsureRoot<Self::AccountId>;
	type PendingUsernameExpiration = ConstU32<{ 7 * DAYS }>;
	type UsernameGracePeriod = ConstU32<{ 30 * DAYS }>;
	type MaxSuffixLength = ConstU32<7>;
	type MaxUsernameLength = ConstU32<32>;
	#[cfg(feature = "runtime-benchmarks")]
	type BenchmarkHelper = ();
	type WeightInfo = weights::pallet_identity::WeightInfo<Runtime>;
}

impl pallet_utility::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type RuntimeCall = RuntimeCall;
	type PalletsOrigin = OriginCaller;
	type WeightInfo = weights::pallet_utility::WeightInfo<Runtime>;
}

parameter_types! {
	// One storage item; key size is 32; value is size 4+4+16+32 bytes = 56 bytes.
	pub const DepositBase: Balance = deposit(1, 88);
	// Additional storage item size of 32 bytes.
	pub const DepositFactor: Balance = deposit(0, 32);
	pub const MaxSignatories: u32 = 100;
}

impl pallet_multisig::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type RuntimeCall = RuntimeCall;
	type Currency = Balances;
	type DepositBase = DepositBase;
	type DepositFactor = DepositFactor;
	type MaxSignatories = MaxSignatories;
	type WeightInfo = weights::pallet_multisig::WeightInfo<Runtime>;
	type BlockNumberProvider = frame_system::Pallet<Runtime>;
}

parameter_types! {
	pub const MinVestedTransfer: Balance = 100 * CENTS;
	pub UnvestedFundsAllowedWithdrawReasons: WithdrawReasons =
		WithdrawReasons::except(WithdrawReasons::TRANSFER | WithdrawReasons::RESERVE);
}

impl pallet_vesting::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type Currency = Balances;
	type BlockNumberToBalance = ConvertInto;
	type MinVestedTransfer = MinVestedTransfer;
	type WeightInfo = weights::pallet_vesting::WeightInfo<Runtime>;
	type UnvestedFundsAllowedWithdrawReasons = UnvestedFundsAllowedWithdrawReasons;
	type BlockNumberProvider = System;
	const MAX_VESTING_SCHEDULES: u32 = 28;
}

impl pallet_sudo::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type RuntimeCall = RuntimeCall;
	type WeightInfo = weights::pallet_sudo::WeightInfo<Runtime>;
}

parameter_types! {
	// One storage item; key size 32, value size 8; .
	pub const ProxyDepositBase: Balance = deposit(1, 8);
	// Additional storage item size of 33 bytes.
	pub const ProxyDepositFactor: Balance = deposit(0, 33);
	pub const MaxProxies: u16 = 32;
	pub const AnnouncementDepositBase: Balance = deposit(1, 8);
	pub const AnnouncementDepositFactor: Balance = deposit(0, 66);
	pub const MaxPending: u16 = 32;
}

/// The type used to represent the kinds of proxying allowed.
#[derive(
	Copy,
	Clone,
	Eq,
	PartialEq,
	Ord,
	PartialOrd,
	Encode,
	Decode,
	DecodeWithMemTracking,
	Debug,
	MaxEncodedLen,
	TypeInfo,
)]
pub enum ProxyType {
	Any,
	NonTransfer,
	Governance,
	Staking,
	SudoBalances,
	IdentityJudgement,
	CancelProxy,
	Auction,
	// Preserve the NominationPools variants after the AH migration to keep on-chain
	// proxy entries with this type decodable.
	NominationPools,
	ParaRegistration,
}
impl Default for ProxyType {
	fn default() -> Self {
		Self::Any
	}
}
impl InstanceFilter<RuntimeCall> for ProxyType {
	fn filter(&self, c: &RuntimeCall) -> bool {
		match self {
			ProxyType::Any => true,
			ProxyType::NonTransfer => matches!(
				c,
				RuntimeCall::System(..) |
				RuntimeCall::Babe(..) |
				RuntimeCall::Timestamp(..) |
				RuntimeCall::Indices(pallet_indices::Call::claim{..}) |
				RuntimeCall::Indices(pallet_indices::Call::free{..}) |
				RuntimeCall::Indices(pallet_indices::Call::freeze{..}) |
				// Specifically omitting Indices `transfer`, `force_transfer`
				// Specifically omitting the entire Balances pallet
				RuntimeCall::Session(..) |
				RuntimeCall::Grandpa(..) |
				RuntimeCall::Utility(..) |
				RuntimeCall::Identity(..) |
				RuntimeCall::Vesting(pallet_vesting::Call::vest{..}) |
				RuntimeCall::Vesting(pallet_vesting::Call::vest_other{..}) |
				// Specifically omitting Vesting `vested_transfer`, and `force_vested_transfer`
				RuntimeCall::Scheduler(..) |
				// Specifically omitting Sudo pallet
				RuntimeCall::Proxy(..) |
				RuntimeCall::Multisig(..) |
				RuntimeCall::Registrar(paras_registrar::Call::register{..}) |
				RuntimeCall::Registrar(paras_registrar::Call::deregister{..}) |
				// Specifically omitting Registrar `swap`
				RuntimeCall::Registrar(paras_registrar::Call::reserve{..}) |
				RuntimeCall::Crowdloan(..) |
				RuntimeCall::Slots(..) |
				RuntimeCall::Auctions(..) // Specifically omitting the entire XCM Pallet
			),
			// Staking and session key management have moved to Asset Hub; this proxy is
			// no longer needed, but we keep it so that on-chain proxy entries still decode.
			ProxyType::Staking => false,
			ProxyType::NominationPools => false,
			ProxyType::SudoBalances => match c {
				RuntimeCall::Sudo(pallet_sudo::Call::sudo { call: ref x }) => {
					matches!(x.as_ref(), &RuntimeCall::Balances(..))
				},
				RuntimeCall::Utility(..) => true,
				_ => false,
			},
			// Governance has moved to AssetHub post-AHM; no calls to proxy.
			ProxyType::Governance => false,
			ProxyType::IdentityJudgement => matches!(
				c,
				RuntimeCall::Identity(pallet_identity::Call::provide_judgement { .. }) |
					RuntimeCall::Utility(..)
			),
			ProxyType::CancelProxy => {
				matches!(c, RuntimeCall::Proxy(pallet_proxy::Call::reject_announcement { .. }))
			},
			ProxyType::Auction => matches!(
				c,
				RuntimeCall::Auctions(..) |
					RuntimeCall::Crowdloan(..) |
					RuntimeCall::Registrar(..) |
					RuntimeCall::Slots(..)
			),
			ProxyType::ParaRegistration => matches!(
				c,
				RuntimeCall::Registrar(paras_registrar::Call::reserve { .. }) |
					RuntimeCall::Registrar(paras_registrar::Call::register { .. }) |
					RuntimeCall::Utility(pallet_utility::Call::batch { .. }) |
					RuntimeCall::Utility(pallet_utility::Call::batch_all { .. }) |
					RuntimeCall::Utility(pallet_utility::Call::force_batch { .. }) |
					RuntimeCall::Proxy(pallet_proxy::Call::remove_proxy { .. })
			),
		}
	}
	fn is_superset(&self, o: &Self) -> bool {
		match (self, o) {
			(x, y) if x == y => true,
			(ProxyType::Any, _) => true,
			(_, ProxyType::Any) => false,
			(ProxyType::NonTransfer, _) => true,
			_ => false,
		}
	}
}

impl pallet_proxy::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type RuntimeCall = RuntimeCall;
	type Currency = Balances;
	type ProxyType = ProxyType;
	type ProxyDepositBase = ProxyDepositBase;
	type ProxyDepositFactor = ProxyDepositFactor;
	type MaxProxies = MaxProxies;
	type WeightInfo = weights::pallet_proxy::WeightInfo<Runtime>;
	type MaxPending = MaxPending;
	type CallHasher = BlakeTwo256;
	type AnnouncementDepositBase = AnnouncementDepositBase;
	type AnnouncementDepositFactor = AnnouncementDepositFactor;
	type BlockNumberProvider = frame_system::Pallet<Runtime>;
}

impl parachains_origin::Config for Runtime {}

impl parachains_configuration::Config for Runtime {
	type WeightInfo = weights::polkadot_runtime_parachains_configuration::WeightInfo<Runtime>;
}

impl parachains_shared::Config for Runtime {
	type DisabledValidators = Session;
}

impl parachains_session_info::Config for Runtime {
	type ValidatorSet = Historical;
}

impl parachains_inclusion::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type DisputesHandler = ParasDisputes;
	type RewardValidators =
		parachains_reward_points::RewardValidatorsWithEraPoints<Runtime, StakingAhClient>;
	type MessageQueue = MessageQueue;
	type WeightInfo = weights::polkadot_runtime_parachains_inclusion::WeightInfo<Runtime>;
}

parameter_types! {
	pub const ParasUnsignedPriority: TransactionPriority = TransactionPriority::max_value();
}

impl parachains_paras::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type WeightInfo = weights::polkadot_runtime_parachains_paras::WeightInfo<Runtime>;
	type UnsignedPriority = ParasUnsignedPriority;
	type QueueFootprinter = ParaInclusion;
	type NextSessionRotation = Babe;
	type OnNewHead = ();
	type AssignCoretime = ParaScheduler;
	type Fungible = Balances;
	// Per day the cooldown is removed earlier, it should cost 1000.
	type CooldownRemovalMultiplier = ConstUint<{ 1000 * UNITS / DAYS as u128 }>;
	type AuthorizeCurrentCodeOrigin = EnsureRoot<AccountId>;
}

parameter_types! {
	/// Amount of weight that can be spent per block to service messages.
	///
	/// # WARNING
	///
	/// This is not a good value for para-chains since the `Scheduler` already uses up to 80% block weight.
	pub MessageQueueServiceWeight: Weight = Perbill::from_percent(20) * BlockWeights::get().max_block;
	pub const MessageQueueHeapSize: u32 = 128 * 1024;
	pub const MessageQueueMaxStale: u32 = 48;
}

/// Message processor to handle any messages that were enqueued into the `MessageQueue` pallet.
pub struct MessageProcessor;
impl ProcessMessage for MessageProcessor {
	type Origin = AggregateMessageOrigin;

	fn process_message(
		message: &[u8],
		origin: Self::Origin,
		meter: &mut WeightMeter,
		id: &mut [u8; 32],
	) -> Result<bool, ProcessMessageError> {
		let para = match origin {
			AggregateMessageOrigin::Ump(UmpQueueId::Para(para)) => para,
		};
		xcm_builder::ProcessXcmMessage::<
			Junction,
			xcm_executor::XcmExecutor<xcm_config::XcmConfig>,
			RuntimeCall,
		>::process_message(message, Junction::Parachain(para.into()), meter, id)
	}
}

impl pallet_message_queue::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type Size = u32;
	type HeapSize = MessageQueueHeapSize;
	type MaxStale = MessageQueueMaxStale;
	type ServiceWeight = MessageQueueServiceWeight;
	type IdleMaxServiceWeight = MessageQueueServiceWeight;
	#[cfg(not(feature = "runtime-benchmarks"))]
	type MessageProcessor = MessageProcessor;
	#[cfg(feature = "runtime-benchmarks")]
	type MessageProcessor =
		pallet_message_queue::mock_helpers::NoopMessageProcessor<AggregateMessageOrigin>;
	type QueueChangeHandler = ParaInclusion;
	type QueuePausedQuery = ();
	type WeightInfo = weights::pallet_message_queue::WeightInfo<Runtime>;
}

impl parachains_dmp::Config for Runtime {
	type WeightInfo = ();
}

parameter_types! {
	pub const HrmpChannelSizeAndCapacityWithSystemRatio: Percent = Percent::from_percent(100);
}

impl parachains_hrmp::Config for Runtime {
	type RuntimeOrigin = RuntimeOrigin;
	type RuntimeEvent = RuntimeEvent;
	type ChannelManager = EnsureRoot<AccountId>;
	type Currency = Balances;
	type DefaultChannelSizeAndCapacityWithSystem = ActiveConfigHrmpChannelSizeAndCapacityRatio<
		Runtime,
		HrmpChannelSizeAndCapacityWithSystemRatio,
	>;
	type VersionWrapper = crate::XcmPallet;
	type WeightInfo = weights::polkadot_runtime_parachains_hrmp::WeightInfo<Self>;
}

impl parachains_paras_inherent::Config for Runtime {
	type WeightInfo = weights::polkadot_runtime_parachains_paras_inherent::WeightInfo<Runtime>;
}

impl parachains_scheduler::Config for Runtime {}

parameter_types! {
	pub const BrokerId: u32 = BROKER_ID;
	pub const BrokerPalletId: PalletId = PalletId(*b"py/broke");
	pub MaxXcmTransactWeight: Weight = Weight::from_parts(200_000_000, 20_000);
}

pub struct BrokerPot;
impl Get<InteriorLocation> for BrokerPot {
	fn get() -> InteriorLocation {
		Junction::AccountId32 { network: None, id: BrokerPalletId::get().into_account_truncating() }
			.into()
	}
}

impl coretime::Config for Runtime {
	type RuntimeOrigin = RuntimeOrigin;
	type RuntimeEvent = RuntimeEvent;
	type BrokerId = BrokerId;
	type BrokerPotLocation = BrokerPot;
	type WeightInfo = weights::polkadot_runtime_parachains_coretime::WeightInfo<Runtime>;
	type SendXcm = crate::xcm_config::XcmRouter;
	type AssetTransactor = crate::xcm_config::LocalAssetTransactor;
	type AccountToLocation = xcm_builder::AliasesIntoAccountId32<
		xcm_config::ThisNetwork,
		<Runtime as frame_system::Config>::AccountId,
	>;
	type MaxXcmTransactWeight = MaxXcmTransactWeight;
}

parameter_types! {
	pub const OnDemandTrafficDefaultValue: FixedU128 = FixedU128::from_u32(1);
	// Keep 2 timeslices worth of revenue information.
	pub const MaxHistoricalRevenue: BlockNumber = 2 * TIMESLICE_PERIOD;
	pub const OnDemandPalletId: PalletId = PalletId(*b"py/ondmd");
}

impl parachains_on_demand::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type Currency = Balances;
	type TrafficDefaultValue = OnDemandTrafficDefaultValue;
	type WeightInfo = weights::polkadot_runtime_parachains_on_demand::WeightInfo<Runtime>;
	type MaxHistoricalRevenue = MaxHistoricalRevenue;
	type PalletId = OnDemandPalletId;
}

impl parachains_initializer::Config for Runtime {
	type Randomness = pallet_babe::RandomnessFromOneEpochAgo<Runtime>;
	type ForceOrigin = EnsureRoot<AccountId>;
	type WeightInfo = weights::polkadot_runtime_parachains_initializer::WeightInfo<Runtime>;
	type CoretimeOnNewSession = Coretime;
}

impl paras_sudo_wrapper::Config for Runtime {}

parameter_types! {
	pub const PermanentSlotLeasePeriodLength: u32 = 26;
	pub const TemporarySlotLeasePeriodLength: u32 = 1;
	pub const MaxTemporarySlotPerLeasePeriod: u32 = 5;
}

impl assigned_slots::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type AssignSlotOrigin = EnsureRoot<AccountId>;
	type Leaser = Slots;
	type PermanentSlotLeasePeriodLength = PermanentSlotLeasePeriodLength;
	type TemporarySlotLeasePeriodLength = TemporarySlotLeasePeriodLength;
	type MaxTemporarySlotPerLeasePeriod = MaxTemporarySlotPerLeasePeriod;
	type WeightInfo = weights::polkadot_runtime_common_assigned_slots::WeightInfo<Runtime>;
}

impl parachains_disputes::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type RewardValidators =
		parachains_reward_points::RewardValidatorsWithEraPoints<Runtime, StakingAhClient>;
	type SlashingHandler = parachains_slashing::SlashValidatorsForDisputes<ParasSlashing>;
	type WeightInfo = weights::polkadot_runtime_parachains_disputes::WeightInfo<Runtime>;
}

impl parachains_slashing::Config for Runtime {
	type KeyOwnerProofSystem = Historical;
	type KeyOwnerProof =
		<Self::KeyOwnerProofSystem as KeyOwnerProofSystem<(KeyTypeId, ValidatorId)>>::Proof;
	type KeyOwnerIdentification = <Self::KeyOwnerProofSystem as KeyOwnerProofSystem<(
		KeyTypeId,
		ValidatorId,
	)>>::IdentificationTuple;
	type HandleReports = parachains_slashing::SlashingReportHandler<
		Self::KeyOwnerIdentification,
		Offences,
		ReportLongevity,
	>;
	type WeightInfo = weights::polkadot_runtime_parachains_disputes_slashing::WeightInfo<Runtime>;
	type BenchmarkingConfig = parachains_slashing::BenchConfig<300>;
}

parameter_types! {
	pub const ParaDeposit: Balance = 2000 * CENTS;
	pub const RegistrarDataDepositPerByte: Balance = deposit(0, 1);
}

impl paras_registrar::Config for Runtime {
	type RuntimeOrigin = RuntimeOrigin;
	type RuntimeEvent = RuntimeEvent;
	type Currency = Balances;
	type OnSwap = (Crowdloan, Slots, SwapLeases);
	type ParaDeposit = ParaDeposit;
	type DataDepositPerByte = RegistrarDataDepositPerByte;
	type WeightInfo = weights::polkadot_runtime_common_paras_registrar::WeightInfo<Runtime>;
}

parameter_types! {
	pub const LeasePeriod: BlockNumber = 28 * DAYS;
}

impl slots::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type Currency = Balances;
	type Registrar = Registrar;
	type LeasePeriod = LeasePeriod;
	type LeaseOffset = ();
	type ForceOrigin = EnsureRoot<Self::AccountId>;
	type WeightInfo = weights::polkadot_runtime_common_slots::WeightInfo<Runtime>;
}

parameter_types! {
	pub const CrowdloanId: PalletId = PalletId(*b"py/cfund");
	pub const SubmissionDeposit: Balance = 100 * 100 * CENTS;
	pub const MinContribution: Balance = 100 * CENTS;
	pub const RemoveKeysLimit: u32 = 500;
	// Allow 32 bytes for an additional memo to a crowdloan.
	pub const MaxMemoLength: u8 = 32;
}

impl crowdloan::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type PalletId = CrowdloanId;
	type SubmissionDeposit = SubmissionDeposit;
	type MinContribution = MinContribution;
	type RemoveKeysLimit = RemoveKeysLimit;
	type Registrar = Registrar;
	type Auctioneer = Auctions;
	type MaxMemoLength = MaxMemoLength;
	type WeightInfo = weights::polkadot_runtime_common_crowdloan::WeightInfo<Runtime>;
}

parameter_types! {
	// The average auction is 7 days long, so this will be 70% for ending period.
	// 5 Days = 72000 Blocks @ 6 sec per block
	pub const EndingPeriod: BlockNumber = 5 * DAYS;
	// ~ 1000 samples per day -> ~ 20 blocks per sample -> 2 minute samples
	pub const SampleLength: BlockNumber = 2 * MINUTES;
}

impl auctions::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type Leaser = Slots;
	type Registrar = Registrar;
	type EndingPeriod = EndingPeriod;
	type SampleLength = SampleLength;
	type Randomness = pallet_babe::RandomnessFromOneEpochAgo<Runtime>;
	type InitiateOrigin = EnsureRoot<Self::AccountId>;
	type WeightInfo = weights::polkadot_runtime_common_auctions::WeightInfo<Runtime>;
}

impl identity_migrator::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type Reaper = EnsureSigned<AccountId>;
	type ReapIdentityHandler = ToParachainIdentityReaper<Runtime, Self::AccountId>;
	type WeightInfo = weights::polkadot_runtime_common_identity_migrator::WeightInfo<Runtime>;
}

impl pallet_root_testing::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
}

impl pallet_root_offences::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type OffenceHandler = StakingAhClient;
	type ReportOffence = Offences;
}

impl pallet_accumulate_and_forward::Config for Runtime {
	type Currency = Balances;
	type PalletId = AccumulateForwardPalletId;
	type Forwarder = xcm_builder::TeleportForwarderForAccountId32<
		xcm_config::XcmConfig,
		xcm_config::AssetHub,
		xcm_config::TokenLocation,
		DapStagingLocation,
	>;
	type TransferPeriod = ForwardPeriod;
	type MinTransferAmount = MinForwardAmount;
	type BlockNumberProvider = frame_system::Pallet<Runtime>;
	type WeightInfo = weights::pallet_accumulate_and_forward::WeightInfo<Runtime>;
}

parameter_types! {
	pub MbmServiceWeight: Weight = Perbill::from_percent(80) * BlockWeights::get().max_block;
}

impl pallet_migrations::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	#[cfg(not(feature = "runtime-benchmarks"))]
	type Migrations = (
		pallet_identity::migration::v2::LazyMigrationV1ToV2<Runtime>,
		parachains_dmp::migration::MigrateV0ToV1<Runtime>,
	);
	// Benchmarks need mocked migrations to guarantee that they succeed.
	#[cfg(feature = "runtime-benchmarks")]
	type Migrations = pallet_migrations::mock_helpers::MockedMigrations;
	type CursorMaxLen = ConstU32<65_536>;
	type IdentifierMaxLen = ConstU32<256>;
	type MigrationStatusHandler = ();
	type FailedMigrationHandler = frame_support::migrations::FreezeChainOnFailedMigration;
	type MaxServiceWeight = MbmServiceWeight;
	type WeightInfo = weights::pallet_migrations::WeightInfo<Runtime>;
}

parameter_types! {
	// The deposit configuration for the singed migration. Specially if you want to allow any signed account to do the migration (see `SignedFilter`, these deposits should be high)
	pub const MigrationSignedDepositPerItem: Balance = 1 * CENTS;
	pub const MigrationSignedDepositBase: Balance = 20 * CENTS * 100;
	pub const MigrationMaxKeyLen: u32 = 512;
}

impl pallet_asset_rate::Config for Runtime {
	type WeightInfo = weights::pallet_asset_rate::WeightInfo<Runtime>;
	type RuntimeEvent = RuntimeEvent;
	type CreateOrigin = EnsureRoot<AccountId>;
	type RemoveOrigin = EnsureRoot<AccountId>;
	type UpdateOrigin = EnsureRoot<AccountId>;
	type Currency = Balances;
	type AssetKind = VersionedLocatableAsset;
	#[cfg(feature = "runtime-benchmarks")]
	type BenchmarkHelper = polkadot_runtime_common::impls::benchmarks::AssetRateArguments;
}

// Notify `coretime` pallet when a lease swap occurs
pub struct SwapLeases;
impl OnSwap for SwapLeases {
	fn on_swap(one: ParaId, other: ParaId) {
		coretime::Pallet::<Runtime>::on_legacy_lease_swap(one, other);
	}
}

/// Minimal stub pallet preserving `RuntimeHoldReason::DelegatedStaking(StakingDelegation)`
/// encoding (pallet_index 38, codec index 0) for existing on-chain holds created by the
/// now-removed `pallet_delegated_staking`. Enables `try_decode_entire_state` to succeed
/// until all holds are migrated away. Remove once Westend storage is clean.
pub mod delegated_staking_stub {
	pub use pallet::*;

	#[frame_support::pallet]
	pub mod pallet {
		#[pallet::pallet]
		pub struct Pallet<T>(_);

		#[pallet::config]
		pub trait Config: frame_system::Config {}

		#[pallet::composite_enum]
		pub enum HoldReason {
			#[codec(index = 0)]
			StakingDelegation,
		}
	}
}

impl delegated_staking_stub::pallet::Config for Runtime {}

#[frame_support::runtime(legacy_ordering)]
mod runtime {
	#[runtime::runtime]
	#[runtime::derive(
		RuntimeCall,
		RuntimeEvent,
		RuntimeError,
		RuntimeOrigin,
		RuntimeFreezeReason,
		RuntimeHoldReason,
		RuntimeSlashReason,
		RuntimeLockId,
		RuntimeTask,
		RuntimeViewFunction
	)]
	pub struct Runtime;

	// Basic stuff; balances is uncallable initially.
	#[runtime::pallet_index(0)]
	pub type System = frame_system;

	// Babe must be before session.
	#[runtime::pallet_index(1)]
	pub type Babe = pallet_babe;

	#[runtime::pallet_index(2)]
	pub type Timestamp = pallet_timestamp;
	#[runtime::pallet_index(3)]
	pub type Indices = pallet_indices;
	#[runtime::pallet_index(4)]
	pub type Balances = pallet_balances;
	#[runtime::pallet_index(26)]
	pub type TransactionPayment = pallet_transaction_payment;
	// AccumulateForward - collects funds for periodic forwarding to DAP on AssetHub
	#[runtime::pallet_index(106)]
	pub type AccumulateForward = pallet_accumulate_and_forward;

	// Consensus support.
	// Authorship must be before session in order to note author in the correct session and era.
	#[runtime::pallet_index(5)]
	pub type Authorship = pallet_authorship;
	#[runtime::pallet_index(7)]
	pub type Offences = pallet_offences;
	#[runtime::pallet_index(27)]
	pub type Historical = session_historical;
	#[runtime::pallet_index(70)]
	pub type Parameters = pallet_parameters;

	#[runtime::pallet_index(8)]
	pub type Session = pallet_session;
	#[runtime::pallet_index(10)]
	pub type Grandpa = pallet_grandpa;
	#[runtime::pallet_index(12)]
	pub type AuthorityDiscovery = pallet_authority_discovery;

	// Utility module.
	#[runtime::pallet_index(16)]
	pub type Utility = pallet_utility;

	// Less simple identity module.
	#[runtime::pallet_index(17)]
	pub type Identity = pallet_identity;

	// Vesting. Usable initially, but removed once all vesting is finished.
	#[runtime::pallet_index(19)]
	pub type Vesting = pallet_vesting;

	// System scheduler.
	#[runtime::pallet_index(20)]
	pub type Scheduler = pallet_scheduler;

	// Preimage registrar.
	#[runtime::pallet_index(28)]
	pub type Preimage = pallet_preimage;

	// Sudo.
	#[runtime::pallet_index(21)]
	pub type Sudo = pallet_sudo;

	// Proxy module. Late addition.
	#[runtime::pallet_index(22)]
	pub type Proxy = pallet_proxy;

	// Multisig module. Late addition.
	#[runtime::pallet_index(23)]
	pub type Multisig = pallet_multisig;

	// Stub preserving RuntimeHoldReason::DelegatedStaking encoding for on-chain holds left by
	// the removed pallet_delegated_staking. Remove once Westend storage is clean.
	#[runtime::pallet_index(38)]
	pub type DelegatedStaking = delegated_staking_stub;

	// Parachains pallets. Start indices at 40 to leave room.
	#[runtime::pallet_index(41)]
	pub type ParachainsOrigin = parachains_origin;
	#[runtime::pallet_index(42)]
	pub type Configuration = parachains_configuration;
	#[runtime::pallet_index(43)]
	pub type ParasShared = parachains_shared;
	#[runtime::pallet_index(44)]
	pub type ParaInclusion = parachains_inclusion;
	#[runtime::pallet_index(45)]
	pub type ParaInherent = parachains_paras_inherent;
	#[runtime::pallet_index(46)]
	pub type ParaScheduler = parachains_scheduler;
	#[runtime::pallet_index(47)]
	pub type Paras = parachains_paras;
	#[runtime::pallet_index(48)]
	pub type Initializer = parachains_initializer;
	#[runtime::pallet_index(49)]
	pub type Dmp = parachains_dmp;
	// RIP Ump 50
	#[runtime::pallet_index(51)]
	pub type Hrmp = parachains_hrmp;
	#[runtime::pallet_index(52)]
	pub type ParaSessionInfo = parachains_session_info;
	#[runtime::pallet_index(53)]
	pub type ParasDisputes = parachains_disputes;
	#[runtime::pallet_index(54)]
	pub type ParasSlashing = parachains_slashing;
	#[runtime::pallet_index(56)]
	pub type OnDemandAssignmentProvider = parachains_on_demand;
	// RIP CoretimeAssignmentProvider 57 - Moved to scheduler::assigner_coretime submodule in PR
	// #10184 (Had no extrinsics nor events exposed)

	// Parachain Onboarding Pallets. Start indices at 60 to leave room.
	#[runtime::pallet_index(60)]
	pub type Registrar = paras_registrar;
	#[runtime::pallet_index(61)]
	pub type Slots = slots;
	#[runtime::pallet_index(62)]
	pub type ParasSudoWrapper = paras_sudo_wrapper;
	#[runtime::pallet_index(63)]
	pub type Auctions = auctions;
	#[runtime::pallet_index(64)]
	pub type Crowdloan = crowdloan;
	#[runtime::pallet_index(65)]
	pub type AssignedSlots = assigned_slots;
	#[runtime::pallet_index(66)]
	pub type Coretime = coretime;
	#[runtime::pallet_index(67)]
	pub type StakingAhClient = pallet_staking_async_ah_client;

	// Migrations pallet
	#[runtime::pallet_index(98)]
	pub type MultiBlockMigrations = pallet_migrations;

	// Pallet for sending XCM.
	#[runtime::pallet_index(99)]
	pub type XcmPallet = pallet_xcm;

	// Generalized message queue
	#[runtime::pallet_index(100)]
	pub type MessageQueue = pallet_message_queue;

	// Asset rate.
	#[runtime::pallet_index(101)]
	pub type AssetRate = pallet_asset_rate;

	// Root testing pallet.
	#[runtime::pallet_index(102)]
	pub type RootTesting = pallet_root_testing;

	// Root offences pallet
	#[runtime::pallet_index(105)]
	pub type RootOffences = pallet_root_offences;

	// BEEFY Bridges support.
	#[runtime::pallet_index(200)]
	pub type Beefy = pallet_beefy;
	// MMR leaf construction must be after session in order to have a leaf's next_auth_set
	// refer to block<N>. See issue polkadot-fellows/runtimes#160 for details.
	#[runtime::pallet_index(201)]
	pub type Mmr = pallet_mmr;
	#[runtime::pallet_index(202)]
	pub type BeefyMmrLeaf = pallet_beefy_mmr;

	// Pallet for migrating Identity to a parachain. To be removed post-migration.
	#[runtime::pallet_index(248)]
	pub type IdentityMigrator = identity_migrator;
}

/// The address format for describing accounts.
pub type Address = sp_runtime::MultiAddress<AccountId, ()>;
/// Block header type as expected by this runtime.
pub type Header = generic::Header<BlockNumber, BlakeTwo256>;
/// Block type as expected by this runtime.
pub type Block = generic::Block<Header, UncheckedExtrinsic>;
/// A Block signed with a Justification
pub type SignedBlock = generic::SignedBlock<Block>;
/// `BlockId` type as expected by this runtime.
pub type BlockId = generic::BlockId<Block>;
/// The extension to the basic transaction logic.
pub type TxExtension = (
	frame_system::AuthorizeCall<Runtime>,
	frame_system::CheckNonZeroSender<Runtime>,
	frame_system::CheckSpecVersion<Runtime>,
	frame_system::CheckTxVersion<Runtime>,
	frame_system::CheckGenesis<Runtime>,
	frame_system::CheckMortality<Runtime>,
	frame_system::CheckNonce<Runtime>,
	frame_system::CheckWeight<Runtime>,
	pallet_transaction_payment::ChargeTransactionPayment<Runtime>,
	frame_metadata_hash_extension::CheckMetadataHash<Runtime>,
	frame_system::WeightReclaim<Runtime>,
);

parameter_types! {
	/// Bounding number of agent pot accounts to be migrated in a single block.
	pub const MaxAgentsToMigrate: u32 = 300;
	pub const RecoveryPalletName: &'static str = "Recovery";
}

/// All migrations that will run on the next runtime upgrade.
///
/// This contains the combined migrations of the last 10 releases. It allows to skip runtime
/// upgrades in case governance decides to do so. THE ORDER IS IMPORTANT.
pub type Migrations = migrations::Unreleased;

/// The runtime migrations per release.
#[allow(deprecated, missing_docs)]
pub mod migrations {
	use super::*;
	use frame_support::{
		traits::{
			fungible::{Balanced, Inspect},
			tokens::{Fortitude, Precision, Preservation},
			OnRuntimeUpgrade, OnUnbalanced,
		},
		weights::Weight,
	};
	use polkadot_primitives::AccountId;
	use sp_runtime::traits::Zero;
	#[cfg(feature = "try-runtime")]
	use {
		alloc::vec::Vec,
		codec::{Decode, Encode},
		polkadot_primitives::Balance,
	};

	parameter_types! {
		pub const TreasuryPalletStr: &'static str = "Treasury";
		pub const ConvictionVotingPalletStr: &'static str = "ConvictionVoting";
		pub const ReferendaPalletStr: &'static str = "Referenda";
		pub const OriginsPalletStr: &'static str = "Origins";
		pub const WhitelistPalletStr: &'static str = "Whitelist";
		pub const StakingPalletStr: &'static str = "Staking";
		pub const ElectionProviderMultiPhasePalletStr: &'static str = "ElectionProviderMultiPhase";
		pub const VoterListPalletStr: &'static str = "VoterList";
		pub const NominationPoolsPalletStr: &'static str = "NominationPools";
		pub const FastUnstakePalletStr: &'static str = "FastUnstake";
	}

	/// Legacy treasury `PalletId` (`py/trsry`).
	const LEGACY_TREASURY_PALLET_ID: PalletId = PalletId(*b"py/trsry");
	const DRAIN_LOG_TARGET: &str = "runtime::westend::drain-legacy-treasury";

	/// One-shot migration that drains the reducible balance of the legacy
	/// `py/trsry`-derived account into the [`pallet_accumulate_and_forward`] accumulation
	/// account. The runtime's `Forwarder` then teleports the funds to AssetHub's central DAP
	/// on the next forwarding interval.
	///
	/// Runtime-local (not part of the generic ACF pallet).
	/// Idempotent: a zero reducible balance is a no-op.
	pub struct DrainLegacyTreasuryToAccumulationAccount;

	impl OnRuntimeUpgrade for DrainLegacyTreasuryToAccumulationAccount {
		fn on_runtime_upgrade() -> Weight {
			let source: AccountId = LEGACY_TREASURY_PALLET_ID.into_account_truncating();
			// No further inflows expected, but `Preserve` is used as a safeguard since this
			// migration runs on every runtime upgrade until removed. Worst case: ED stays
			// behind on a dead account.
			let amount = <Balances as Inspect<AccountId>>::reducible_balance(
				&source,
				Preservation::Preserve,
				Fortitude::Polite,
			);
			if amount.is_zero() {
				log::info!(
					target: DRAIN_LOG_TARGET,
					"nothing to withdraw (reducible balance is zero)."
				);
				return <Runtime as frame_system::Config>::DbWeight::get().reads(1);
			}

			match <Balances as Balanced<AccountId>>::withdraw(
				&source,
				amount,
				Precision::Exact,
				Preservation::Preserve,
				Fortitude::Polite,
			) {
				Ok(credit) => {
					<AccumulateForward as OnUnbalanced<_>>::on_unbalanced(credit);
					log::info!(
						target: DRAIN_LOG_TARGET,
						"swept {amount:?} to accumulation account."
					);
				},
				Err(_) => {
					frame_support::defensive!(
						"DrainLegacyTreasuryToAccumulationAccount: failed to withdraw from legacy treasury account"
					);
				},
			}

			// Distinct storage keys touched: source Account (balances + system),
			// accumulation Account (balances + system) = 4 reads and 4 writes.
			<Runtime as frame_system::Config>::DbWeight::get().reads_writes(4, 4)
		}

		#[cfg(feature = "try-runtime")]
		fn pre_upgrade() -> Result<Vec<u8>, sp_runtime::TryRuntimeError> {
			let source: AccountId = LEGACY_TREASURY_PALLET_ID.into_account_truncating();
			let legacy_pre = <Balances as Inspect<AccountId>>::reducible_balance(
				&source,
				Preservation::Preserve,
				Fortitude::Polite,
			);
			let accum_pre = <Balances as Inspect<AccountId>>::reducible_balance(
				&pallet_accumulate_and_forward::Pallet::<Runtime>::accumulation_account(),
				Preservation::Preserve,
				Fortitude::Polite,
			);
			log::info!(
				target: DRAIN_LOG_TARGET,
				"pre-upgrade legacy reducible = {legacy_pre:?}, accumulation reducible = {accum_pre:?}"
			);
			Ok((legacy_pre, accum_pre).encode())
		}

		#[cfg(feature = "try-runtime")]
		fn post_upgrade(state: Vec<u8>) -> Result<(), sp_runtime::TryRuntimeError> {
			let (legacy_pre, accum_pre): (Balance, Balance) = Decode::decode(&mut &state[..])
				.expect("pre_upgrade encoded (legacy_pre, accum_pre)");

			let source: AccountId = LEGACY_TREASURY_PALLET_ID.into_account_truncating();
			let legacy_post = <Balances as Inspect<AccountId>>::reducible_balance(
				&source,
				Preservation::Preserve,
				Fortitude::Polite,
			);
			frame_support::ensure!(
				legacy_post.is_zero(),
				"Legacy treasury reducible balance should be zero after migration"
			);

			let accum_post = <Balances as Inspect<AccountId>>::reducible_balance(
				&pallet_accumulate_and_forward::Pallet::<Runtime>::accumulation_account(),
				Preservation::Preserve,
				Fortitude::Polite,
			);
			frame_support::ensure!(
				Some(accum_post) == accum_pre.checked_add(legacy_pre),
				"Accumulation account balance should have increased by exactly the drained amount"
			);

			log::info!(
				target: DRAIN_LOG_TARGET,
				"post-upgrade OK. Legacy reducible: {legacy_post:?}, accumulation reducible: {accum_post:?}"
			);
			Ok(())
		}
	}

	/// Unreleased migrations. Add new ones here:
	pub type Unreleased = (
		// Migrate scheduler v3 -> v4 and on-demand v1 -> v2
		parachains_on_demand::migration::MigrateV1ToV2<Runtime>,
		parachains_scheduler::migration::MigrateV3ToV4<Runtime>,
		parachains_configuration::migration::v13::MigrateToV13<Runtime>,
		parachains_shared::migration::MigrateToV2<Runtime>,
		// #11705: drain residual legacy `py/trsry` balance into the ACF accumulation
		// account.
		// Idempotent. No further activity on the legacy `py/trsry`
		// account is expected. Safe to remove after the next runtime upgrade once confirmed.
		DrainLegacyTreasuryToAccumulationAccount,
		frame_support::migrations::RemovePallet<
			TreasuryPalletStr,
			<Runtime as frame_system::Config>::DbWeight,
		>,
		// OpenGov removal: clear orphaned storage for governance pallets.
		frame_support::migrations::RemovePallet<
			ConvictionVotingPalletStr,
			<Runtime as frame_system::Config>::DbWeight,
		>,
		frame_support::migrations::RemovePallet<
			ReferendaPalletStr,
			<Runtime as frame_system::Config>::DbWeight,
		>,
		frame_support::migrations::RemovePallet<
			OriginsPalletStr,
			<Runtime as frame_system::Config>::DbWeight,
		>,
		frame_support::migrations::RemovePallet<
			WhitelistPalletStr,
			<Runtime as frame_system::Config>::DbWeight,
		>,
		// Remove the Recovery pallet.
		frame_support::migrations::RemovePallet<
			RecoveryPalletName,
			<Runtime as frame_system::Config>::DbWeight,
		>,
		// Staking removal: clear orphaned storage for staking-related pallets.
		frame_support::migrations::RemovePallet<
			StakingPalletStr,
			<Runtime as frame_system::Config>::DbWeight,
		>,
		frame_support::migrations::RemovePallet<
			ElectionProviderMultiPhasePalletStr,
			<Runtime as frame_system::Config>::DbWeight,
		>,
		frame_support::migrations::RemovePallet<
			VoterListPalletStr,
			<Runtime as frame_system::Config>::DbWeight,
		>,
		frame_support::migrations::RemovePallet<
			NominationPoolsPalletStr,
			<Runtime as frame_system::Config>::DbWeight,
		>,
		frame_support::migrations::RemovePallet<
			FastUnstakePalletStr,
			<Runtime as frame_system::Config>::DbWeight,
		>,
		// permanent
		pallet_xcm::migration::MigrateToLatestXcmVersion<Runtime>,
	);
}

/// Unchecked extrinsic type as expected by this runtime.
pub type UncheckedExtrinsic =
	generic::UncheckedExtrinsic<Address, RuntimeCall, Signature, TxExtension>;
/// Unchecked signature payload type as expected by this runtime.
pub type UncheckedSignaturePayload =
	generic::UncheckedSignaturePayload<Address, Signature, TxExtension>;

/// Executive: handles dispatch to the various modules.
pub type Executive = frame_executive::Executive<
	Runtime,
	Block,
	frame_system::ChainContext<Runtime>,
	Runtime,
	AllPalletsWithSystem,
>;
/// The payload being signed in transactions.
pub type SignedPayload = generic::SignedPayload<RuntimeCall, TxExtension>;

#[cfg(feature = "runtime-benchmarks")]
mod benches {
	frame_benchmarking::define_benchmarks!(
		// Polkadot
		// NOTE: Make sure to prefix these with `runtime_common::` so
		// the that path resolves correctly in the generated file.
		[polkadot_runtime_common::assigned_slots, AssignedSlots]
		[polkadot_runtime_common::auctions, Auctions]
		[polkadot_runtime_common::crowdloan, Crowdloan]
		[polkadot_runtime_common::identity_migrator, IdentityMigrator]
		[polkadot_runtime_common::paras_registrar, Registrar]
		[polkadot_runtime_common::slots, Slots]
		[polkadot_runtime_parachains::configuration, Configuration]
		[polkadot_runtime_parachains::disputes, ParasDisputes]
		[polkadot_runtime_parachains::dmp, Dmp]
		[polkadot_runtime_parachains::hrmp, Hrmp]
		[polkadot_runtime_parachains::inclusion, ParaInclusion]
		[polkadot_runtime_parachains::initializer, Initializer]
		[polkadot_runtime_parachains::paras, Paras]
		[polkadot_runtime_parachains::paras_inherent, ParaInherent]
		[polkadot_runtime_parachains::on_demand, OnDemandAssignmentProvider]
		[polkadot_runtime_parachains::coretime, Coretime]
		// Substrate
		[pallet_balances, Balances]
		[pallet_beefy_mmr, BeefyMmrLeaf]
		[pallet_identity, Identity]
		[pallet_indices, Indices]
		[pallet_message_queue, MessageQueue]
		[pallet_migrations, MultiBlockMigrations]
		[pallet_mmr, Mmr]
		[pallet_multisig, Multisig]
		[pallet_parameters, Parameters]
		[pallet_preimage, Preimage]
		[pallet_proxy, Proxy]
		[pallet_scheduler, Scheduler]
		[pallet_sudo, Sudo]
		[frame_system, SystemBench::<Runtime>]
		[frame_system_extensions, SystemExtensionsBench::<Runtime>]
		[pallet_timestamp, Timestamp]
		[pallet_transaction_payment, TransactionPayment]
		[pallet_utility, Utility]
		[pallet_vesting, Vesting]
		[pallet_asset_rate, AssetRate]
		[pallet_accumulate_and_forward, AccumulateForward]
		// XCM
		[pallet_xcm, PalletXcmExtrinsicsBenchmark::<Runtime>]
		// NOTE: Make sure you point to the individual modules below.
		[pallet_xcm_benchmarks::fungible, XcmBalances]
		[pallet_xcm_benchmarks::generic, XcmGeneric]
	);
}

sp_api::impl_runtime_apis! {
	impl sp_api::Core<Block> for Runtime {
		fn version() -> RuntimeVersion {
			VERSION
		}

		fn execute_block(block: <Block as BlockT>::LazyBlock) {
			Executive::execute_block(block);
		}

		fn initialize_block(header: &<Block as BlockT>::Header) -> sp_runtime::ExtrinsicInclusionMode {
			Executive::initialize_block(header)
		}
	}

	impl sp_api::Metadata<Block> for Runtime {
		fn metadata() -> OpaqueMetadata {
			OpaqueMetadata::new(Runtime::metadata().into())
		}

		fn metadata_at_version(version: u32) -> Option<OpaqueMetadata> {
			Runtime::metadata_at_version(version)
		}

		fn metadata_versions() -> alloc::vec::Vec<u32> {
			Runtime::metadata_versions()
		}
	}

	impl frame_support::view_functions::runtime_api::RuntimeViewFunction<Block> for Runtime {
		fn execute_view_function(id: frame_support::view_functions::ViewFunctionId, input: Vec<u8>) -> Result<Vec<u8>, frame_support::view_functions::ViewFunctionDispatchError> {
			Runtime::execute_view_function(id, input)
		}
	}

	impl sp_block_builder::BlockBuilder<Block> for Runtime {
		fn apply_extrinsic(extrinsic: <Block as BlockT>::Extrinsic) -> ApplyExtrinsicResult {
			Executive::apply_extrinsic(extrinsic)
		}

		fn finalize_block() -> <Block as BlockT>::Header {
			Executive::finalize_block()
		}

		fn inherent_extrinsics(data: sp_inherents::InherentData) -> Vec<<Block as BlockT>::Extrinsic> {
			data.create_extrinsics()
		}

		fn check_inherents(
			block: <Block as BlockT>::LazyBlock,
			data: sp_inherents::InherentData,
		) -> sp_inherents::CheckInherentsResult {
			data.check_extrinsics(&block)
		}
	}

	impl sp_transaction_pool::runtime_api::TaggedTransactionQueue<Block> for Runtime {
		fn validate_transaction(
			source: TransactionSource,
			tx: <Block as BlockT>::Extrinsic,
			block_hash: <Block as BlockT>::Hash,
		) -> TransactionValidity {
			Executive::validate_transaction(source, tx, block_hash)
		}
	}

	impl sp_offchain::OffchainWorkerApi<Block> for Runtime {
		fn offchain_worker(header: &<Block as BlockT>::Header) {
			Executive::offchain_worker(header)
		}
	}

	#[api_version(16)]
	impl polkadot_primitives::runtime_api::ParachainHost<Block> for Runtime {
		fn validators() -> Vec<ValidatorId> {
			parachains_runtime_api_impl::validators::<Runtime>()
		}

		fn validator_groups() -> (Vec<Vec<ValidatorIndex>>, GroupRotationInfo<BlockNumber>) {
			parachains_runtime_api_impl::validator_groups::<Runtime>()
		}

		fn availability_cores() -> Vec<CoreState<Hash, BlockNumber>> {
			parachains_runtime_api_impl::availability_cores::<Runtime>()
		}

		fn persisted_validation_data(para_id: ParaId, assumption: OccupiedCoreAssumption)
			-> Option<PersistedValidationData<Hash, BlockNumber>> {
			parachains_runtime_api_impl::persisted_validation_data::<Runtime>(para_id, assumption)
		}

		fn assumed_validation_data(
			para_id: ParaId,
			expected_persisted_validation_data_hash: Hash,
		) -> Option<(PersistedValidationData<Hash, BlockNumber>, ValidationCodeHash)> {
			parachains_runtime_api_impl::assumed_validation_data::<Runtime>(
				para_id,
				expected_persisted_validation_data_hash,
			)
		}

		fn check_validation_outputs(
			para_id: ParaId,
			outputs: polkadot_primitives::CandidateCommitments,
		) -> bool {
			parachains_runtime_api_impl::check_validation_outputs::<Runtime>(para_id, outputs)
		}

		fn session_index_for_child() -> SessionIndex {
			parachains_runtime_api_impl::session_index_for_child::<Runtime>()
		}

		fn validation_code(para_id: ParaId, assumption: OccupiedCoreAssumption)
			-> Option<ValidationCode> {
			parachains_runtime_api_impl::validation_code::<Runtime>(para_id, assumption)
		}

		fn candidate_pending_availability(para_id: ParaId) -> Option<CommittedCandidateReceipt<Hash>> {
			#[allow(deprecated)]
			parachains_runtime_api_impl::candidate_pending_availability::<Runtime>(para_id)
		}

		fn candidate_events() -> Vec<CandidateEvent<Hash>> {
			parachains_runtime_api_impl::candidate_events::<Runtime, _>(|ev| {
				match ev {
					RuntimeEvent::ParaInclusion(ev) => {
						Some(ev)
					}
					_ => None,
				}
			})
		}

		fn session_info(index: SessionIndex) -> Option<SessionInfo> {
			parachains_runtime_api_impl::session_info::<Runtime>(index)
		}

		fn session_executor_params(session_index: SessionIndex) -> Option<ExecutorParams> {
			parachains_runtime_api_impl::session_executor_params::<Runtime>(session_index)
		}

		fn dmq_contents(recipient: ParaId) -> Vec<InboundDownwardMessage<BlockNumber>> {
			parachains_runtime_api_impl::dmq_contents::<Runtime>(recipient)
		}

		fn inbound_hrmp_channels_contents(
			recipient: ParaId
		) -> BTreeMap<ParaId, Vec<InboundHrmpMessage<BlockNumber>>> {
			parachains_runtime_api_impl::inbound_hrmp_channels_contents::<Runtime>(recipient)
		}

		fn validation_code_by_hash(hash: ValidationCodeHash) -> Option<ValidationCode> {
			parachains_runtime_api_impl::validation_code_by_hash::<Runtime>(hash)
		}

		fn on_chain_votes() -> Option<ScrapedOnChainVotes<Hash>> {
			parachains_runtime_api_impl::on_chain_votes::<Runtime>()
		}

		fn submit_pvf_check_statement(
			stmt: PvfCheckStatement,
			signature: ValidatorSignature,
		) {
			parachains_runtime_api_impl::submit_pvf_check_statement::<Runtime>(stmt, signature)
		}

		fn pvfs_require_precheck() -> Vec<ValidationCodeHash> {
			parachains_runtime_api_impl::pvfs_require_precheck::<Runtime>()
		}

		fn validation_code_hash(para_id: ParaId, assumption: OccupiedCoreAssumption)
			-> Option<ValidationCodeHash>
		{
			parachains_runtime_api_impl::validation_code_hash::<Runtime>(para_id, assumption)
		}

		fn disputes() -> Vec<(SessionIndex, CandidateHash, DisputeState<BlockNumber>)> {
			parachains_runtime_api_impl::get_session_disputes::<Runtime>()
		}

		fn unapplied_slashes(
		) -> Vec<(SessionIndex, CandidateHash, slashing::LegacyPendingSlashes)> {
			parachains_runtime_api_impl::unapplied_slashes::<Runtime>()
		}

		fn unapplied_slashes_v2(
		) -> Vec<(SessionIndex, CandidateHash, slashing::PendingSlashes)> {
			parachains_runtime_api_impl::unapplied_slashes_v2::<Runtime>()
		}

		fn key_ownership_proof(
			validator_id: ValidatorId,
		) -> Option<slashing::OpaqueKeyOwnershipProof> {
			use codec::Encode;

			Historical::prove((PARACHAIN_KEY_TYPE_ID, validator_id))
				.map(|p| p.encode())
				.map(slashing::OpaqueKeyOwnershipProof::new)
		}

		fn submit_report_dispute_lost(
			dispute_proof: slashing::DisputeProof,
			key_ownership_proof: slashing::OpaqueKeyOwnershipProof,
		) -> Option<()> {
			parachains_runtime_api_impl::submit_unsigned_slashing_report::<Runtime>(
				dispute_proof,
				key_ownership_proof,
			)
		}

		fn minimum_backing_votes() -> u32 {
			parachains_runtime_api_impl::minimum_backing_votes::<Runtime>()
		}

		fn para_backing_state(para_id: ParaId) -> Option<polkadot_primitives::async_backing::BackingState> {
			#[allow(deprecated)]
			parachains_runtime_api_impl::backing_state::<Runtime>(para_id)
		}

		fn async_backing_params() -> polkadot_primitives::AsyncBackingParams {
			#[allow(deprecated)]
			parachains_runtime_api_impl::async_backing_params::<Runtime>()
		}

		fn approval_voting_params() -> ApprovalVotingParams {
			parachains_runtime_api_impl::approval_voting_params::<Runtime>()
		}

		fn disabled_validators() -> Vec<ValidatorIndex> {
			parachains_runtime_api_impl::disabled_validators::<Runtime>()
		}

		fn node_features() -> NodeFeatures {
			parachains_runtime_api_impl::node_features::<Runtime>()
		}

		fn claim_queue() -> BTreeMap<CoreIndex, VecDeque<ParaId>> {
			parachains_runtime_api_impl::claim_queue::<Runtime>()
		}

		fn candidates_pending_availability(para_id: ParaId) -> Vec<CommittedCandidateReceipt<Hash>> {
			parachains_runtime_api_impl::candidates_pending_availability::<Runtime>(para_id)
		}

		fn backing_constraints(para_id: ParaId) -> Option<Constraints> {
			parachains_runtime_api_impl::backing_constraints::<Runtime>(para_id)
		}

		fn scheduling_lookahead() -> u32 {
			parachains_runtime_api_impl::scheduling_lookahead::<Runtime>()
		}

		fn validation_code_bomb_limit() -> u32 {
			parachains_runtime_api_impl::validation_code_bomb_limit::<Runtime>()
		}

		fn para_ids() -> Vec<ParaId> {
			parachains_staging_runtime_api_impl::para_ids::<Runtime>()
		}

		fn max_relay_parent_session_age() -> u32 {
			parachains_staging_runtime_api_impl::max_relay_parent_session_age::<Runtime>()
		}

		fn ancestor_relay_parent_info(
			session_index: SessionIndex,
			relay_parent: Hash,
		) -> Option<polkadot_primitives::vstaging::RelayParentInfo<Hash, BlockNumber>> {
			parachains_staging_runtime_api_impl::ancestor_relay_parent_info::<Runtime>(session_index, relay_parent)
		}
	}

	#[api_version(6)]
	impl sp_consensus_beefy::BeefyApi<Block, BeefyId> for Runtime {
		fn beefy_genesis() -> Option<BlockNumber> {
			pallet_beefy::GenesisBlock::<Runtime>::get()
		}

		fn validator_set() -> Option<sp_consensus_beefy::ValidatorSet<BeefyId>> {
			Beefy::validator_set()
		}

		fn submit_report_double_voting_unsigned_extrinsic(
			equivocation_proof: sp_consensus_beefy::DoubleVotingProof<
				BlockNumber,
				BeefyId,
				BeefySignature,
			>,
			key_owner_proof: sp_consensus_beefy::OpaqueKeyOwnershipProof,
		) -> Option<()> {
			let key_owner_proof = key_owner_proof.decode()?;

			Beefy::submit_unsigned_double_voting_report(
				equivocation_proof,
				key_owner_proof,
			)
		}

		fn submit_report_fork_voting_unsigned_extrinsic(
			equivocation_proof:
				sp_consensus_beefy::ForkVotingProof<
					<Block as BlockT>::Header,
					BeefyId,
					sp_runtime::OpaqueValue
				>,
			key_owner_proof: sp_consensus_beefy::OpaqueKeyOwnershipProof,
		) -> Option<()> {
			Beefy::submit_unsigned_fork_voting_report(
				equivocation_proof.try_into()?,
				key_owner_proof.decode()?,
			)
		}

		fn submit_report_future_block_voting_unsigned_extrinsic(
			equivocation_proof: sp_consensus_beefy::FutureBlockVotingProof<BlockNumber, BeefyId>,
			key_owner_proof: sp_consensus_beefy::OpaqueKeyOwnershipProof,
		) -> Option<()> {
			Beefy::submit_unsigned_future_block_voting_report(
				equivocation_proof,
				key_owner_proof.decode()?,
			)
		}

		fn generate_key_ownership_proof(
			_set_id: sp_consensus_beefy::ValidatorSetId,
			authority_id: BeefyId,
		) -> Option<sp_consensus_beefy::OpaqueKeyOwnershipProof> {
			use codec::Encode;

			Historical::prove((sp_consensus_beefy::KEY_TYPE, authority_id))
				.map(|p| p.encode())
				.map(sp_consensus_beefy::OpaqueKeyOwnershipProof::new)
		}
	}

	#[api_version(3)]
	impl mmr::MmrApi<Block, Hash, BlockNumber> for Runtime {
		fn mmr_root() -> Result<mmr::Hash, mmr::Error> {
			Ok(pallet_mmr::RootHash::<Runtime>::get())
		}

		fn mmr_leaf_count() -> Result<mmr::LeafIndex, mmr::Error> {
			Ok(pallet_mmr::NumberOfLeaves::<Runtime>::get())
		}

		fn generate_proof(
			block_numbers: Vec<BlockNumber>,
			best_known_block_number: Option<BlockNumber>,
		) -> Result<(Vec<mmr::EncodableOpaqueLeaf>, mmr::LeafProof<mmr::Hash>), mmr::Error> {
			Mmr::generate_proof(block_numbers, best_known_block_number).map(
				|(leaves, proof)| {
					(
						leaves
							.into_iter()
							.map(|leaf| mmr::EncodableOpaqueLeaf::from_leaf(&leaf))
							.collect(),
						proof,
					)
				},
			)
		}

		fn generate_ancestry_proof(
			prev_block_number: BlockNumber,
			best_known_block_number: Option<BlockNumber>,
		) -> Result<mmr::AncestryProof<mmr::Hash>, mmr::Error> {
			Mmr::generate_ancestry_proof(prev_block_number, best_known_block_number)
		}

		fn verify_proof(leaves: Vec<mmr::EncodableOpaqueLeaf>, proof: mmr::LeafProof<mmr::Hash>)
			-> Result<(), mmr::Error>
		{
			let leaves = leaves.into_iter().map(|leaf|
				leaf.into_opaque_leaf()
				.try_decode()
				.ok_or(mmr::Error::Verify)).collect::<Result<Vec<mmr::Leaf>, mmr::Error>>()?;
			Mmr::verify_leaves(leaves, proof)
		}

		fn verify_proof_stateless(
			root: mmr::Hash,
			leaves: Vec<mmr::EncodableOpaqueLeaf>,
			proof: mmr::LeafProof<mmr::Hash>
		) -> Result<(), mmr::Error> {
			let nodes = leaves.into_iter().map(|leaf|mmr::DataOrHash::Data(leaf.into_opaque_leaf())).collect();
			pallet_mmr::verify_leaves_proof::<mmr::Hashing, _>(root, nodes, proof)
		}
	}

	impl pallet_beefy_mmr::BeefyMmrApi<Block, Hash> for RuntimeApi {
		fn authority_set_proof() -> sp_consensus_beefy::mmr::BeefyAuthoritySet<Hash> {
			BeefyMmrLeaf::authority_set_proof()
		}

		fn next_authority_set_proof() -> sp_consensus_beefy::mmr::BeefyNextAuthoritySet<Hash> {
			BeefyMmrLeaf::next_authority_set_proof()
		}
	}

	impl fg_primitives::GrandpaApi<Block> for Runtime {
		fn grandpa_authorities() -> Vec<(GrandpaId, u64)> {
			Grandpa::grandpa_authorities()
		}

		fn current_set_id() -> fg_primitives::SetId {
			pallet_grandpa::CurrentSetId::<Runtime>::get()
		}

		fn submit_report_equivocation_unsigned_extrinsic(
			equivocation_proof: fg_primitives::EquivocationProof<
				<Block as BlockT>::Hash,
				sp_runtime::traits::NumberFor<Block>,
			>,
			key_owner_proof: fg_primitives::OpaqueKeyOwnershipProof,
		) -> Option<()> {
			let key_owner_proof = key_owner_proof.decode()?;

			Grandpa::submit_unsigned_equivocation_report(
				equivocation_proof,
				key_owner_proof,
			)
		}

		fn generate_key_ownership_proof(
			_set_id: fg_primitives::SetId,
			authority_id: fg_primitives::AuthorityId,
		) -> Option<fg_primitives::OpaqueKeyOwnershipProof> {
			use codec::Encode;

			Historical::prove((fg_primitives::KEY_TYPE, authority_id))
				.map(|p| p.encode())
				.map(fg_primitives::OpaqueKeyOwnershipProof::new)
		}
	}

	impl sp_consensus_babe::BabeApi<Block> for Runtime {
		fn configuration() -> sp_consensus_babe::BabeConfiguration {
			let epoch_config = Babe::epoch_config().unwrap_or(BABE_GENESIS_EPOCH_CONFIG);
			sp_consensus_babe::BabeConfiguration {
				slot_duration: Babe::slot_duration(),
				epoch_length: EpochDuration::get(),
				c: epoch_config.c,
				authorities: Babe::authorities().to_vec(),
				randomness: Babe::randomness(),
				allowed_slots: epoch_config.allowed_slots,
			}
		}

		fn current_epoch_start() -> sp_consensus_babe::Slot {
			Babe::current_epoch_start()
		}

		fn current_epoch() -> sp_consensus_babe::Epoch {
			Babe::current_epoch()
		}

		fn next_epoch() -> sp_consensus_babe::Epoch {
			Babe::next_epoch()
		}

		fn generate_key_ownership_proof(
			_slot: sp_consensus_babe::Slot,
			authority_id: sp_consensus_babe::AuthorityId,
		) -> Option<sp_consensus_babe::OpaqueKeyOwnershipProof> {
			use codec::Encode;

			Historical::prove((sp_consensus_babe::KEY_TYPE, authority_id))
				.map(|p| p.encode())
				.map(sp_consensus_babe::OpaqueKeyOwnershipProof::new)
		}

		fn submit_report_equivocation_unsigned_extrinsic(
			equivocation_proof: sp_consensus_babe::EquivocationProof<<Block as BlockT>::Header>,
			key_owner_proof: sp_consensus_babe::OpaqueKeyOwnershipProof,
		) -> Option<()> {
			let key_owner_proof = key_owner_proof.decode()?;

			Babe::submit_unsigned_equivocation_report(
				equivocation_proof,
				key_owner_proof,
			)
		}
	}

	impl sp_authority_discovery::AuthorityDiscoveryApi<Block> for Runtime {
		fn authorities() -> Vec<AuthorityDiscoveryId> {
			parachains_runtime_api_impl::relevant_authority_ids::<Runtime>()
		}
	}

	impl sp_session::SessionKeys<Block> for Runtime {
		fn generate_session_keys(owner: Vec<u8>, seed: Option<Vec<u8>>) -> sp_session::OpaqueGeneratedSessionKeys {
			SessionKeys::generate(&owner, seed).into()
		}

		fn decode_session_keys(
			encoded: Vec<u8>,
		) -> Option<Vec<(Vec<u8>, sp_core::crypto::KeyTypeId)>> {
			SessionKeys::decode_into_raw_public_keys(&encoded)
		}
	}

	impl frame_system_rpc_runtime_api::AccountNonceApi<Block, AccountId, Nonce> for Runtime {
		fn account_nonce(account: AccountId) -> Nonce {
			System::account_nonce(account)
		}
	}

	impl pallet_transaction_payment_rpc_runtime_api::TransactionPaymentApi<
		Block,
		Balance,
	> for Runtime {
		fn query_info(uxt: <Block as BlockT>::Extrinsic, len: u32) -> RuntimeDispatchInfo<Balance> {
			TransactionPayment::query_info(uxt, len)
		}
		fn query_fee_details(uxt: <Block as BlockT>::Extrinsic, len: u32) -> FeeDetails<Balance> {
			TransactionPayment::query_fee_details(uxt, len)
		}
		fn query_weight_to_fee(weight: Weight) -> Balance {
			TransactionPayment::weight_to_fee(weight)
		}
		fn query_length_to_fee(length: u32) -> Balance {
			TransactionPayment::length_to_fee(length)
		}
	}

	impl pallet_transaction_payment_rpc_runtime_api::TransactionPaymentCallApi<Block, Balance, RuntimeCall>
		for Runtime
	{
		fn query_call_info(call: RuntimeCall, len: u32) -> RuntimeDispatchInfo<Balance> {
			TransactionPayment::query_call_info(call, len)
		}
		fn query_call_fee_details(call: RuntimeCall, len: u32) -> FeeDetails<Balance> {
			TransactionPayment::query_call_fee_details(call, len)
		}
		fn query_weight_to_fee(weight: Weight) -> Balance {
			TransactionPayment::weight_to_fee(weight)
		}
		fn query_length_to_fee(length: u32) -> Balance {
			TransactionPayment::length_to_fee(length)
		}
	}

	impl xcm_runtime_apis::fees::XcmPaymentApi<Block> for Runtime {
		fn query_acceptable_payment_assets(xcm_version: xcm::Version) -> Result<Vec<VersionedAssetId>, XcmPaymentApiError> {
			let acceptable_assets = vec![AssetId(xcm_config::TokenLocation::get())];
			XcmPallet::query_acceptable_payment_assets(xcm_version, acceptable_assets)
		}

		fn query_weight_to_asset_fee(weight: Weight, asset: VersionedAssetId) -> Result<u128, XcmPaymentApiError> {
			type Trader = <XcmConfig as xcm_executor::Config>::Trader;
			XcmPallet::query_weight_to_asset_fee::<Trader>(weight, asset)
		}

		fn query_xcm_weight(message: VersionedXcm<()>) -> Result<Weight, XcmPaymentApiError> {
			XcmPallet::query_xcm_weight(message)
		}

		fn query_delivery_fees(destination: VersionedLocation, message: VersionedXcm<()>, asset_id: VersionedAssetId) -> Result<VersionedAssets, XcmPaymentApiError> {
			type AssetExchanger = <XcmConfig as xcm_executor::Config>::AssetExchanger;
			XcmPallet::query_delivery_fees::<AssetExchanger>(destination, message, asset_id)
		}
	}

	impl xcm_runtime_apis::dry_run::DryRunApi<Block, RuntimeCall, RuntimeEvent, OriginCaller> for Runtime {
		fn dry_run_call(origin: OriginCaller, call: RuntimeCall, result_xcms_version: XcmVersion) -> Result<CallDryRunEffects<RuntimeEvent>, XcmDryRunApiError> {
			XcmPallet::dry_run_call::<Runtime, xcm_config::XcmRouter, OriginCaller, RuntimeCall>(origin, call, result_xcms_version)
		}

		fn dry_run_xcm(origin_location: VersionedLocation, xcm: VersionedXcm<RuntimeCall>) -> Result<XcmDryRunEffects<RuntimeEvent>, XcmDryRunApiError> {
			XcmPallet::dry_run_xcm::<xcm_config::XcmRouter>(origin_location, xcm)
		}
	}

	impl xcm_runtime_apis::conversions::LocationToAccountApi<Block, AccountId> for Runtime {
		fn convert_location(location: VersionedLocation) -> Result<
			AccountId,
			xcm_runtime_apis::conversions::Error
		> {
			xcm_runtime_apis::conversions::LocationToAccountHelper::<
				AccountId,
				xcm_config::LocationConverter,
			>::convert_location(location)
		}
	}

	#[cfg(feature = "try-runtime")]
	impl frame_try_runtime::TryRuntime<Block> for Runtime {
		fn on_runtime_upgrade(checks: frame_try_runtime::UpgradeCheckSelect) -> (Weight, Weight) {
			log::info!("try-runtime::on_runtime_upgrade westend.");
			let config = frame_executive::TryRuntimeUpgradeConfig::new(checks);
			let weight = Executive::try_runtime_upgrade_with_config(config).unwrap();
			(weight, BlockWeights::get().max_block)
		}

		fn execute_block(
			block: <Block as BlockT>::LazyBlock,
			state_root_check: bool,
			signature_check: bool,
			select: frame_try_runtime::TryStateSelect,
		) -> Weight {
			// NOTE: intentional unwrap: we don't want to propagate the error backwards, and want to
			// have a backtrace here.
			Executive::try_execute_block(block, state_root_check, signature_check, select).unwrap()
		}
	}

	#[cfg(feature = "runtime-benchmarks")]
	impl frame_benchmarking::Benchmark<Block> for Runtime {
		fn benchmark_metadata(extra: bool) -> (
			Vec<frame_benchmarking::BenchmarkList>,
			Vec<frame_support::traits::StorageInfo>,
		) {
			use frame_benchmarking::BenchmarkList;
			use frame_support::traits::StorageInfoTrait;

			use pallet_xcm::benchmarking::Pallet as PalletXcmExtrinsicsBenchmark;
			use frame_system_benchmarking::Pallet as SystemBench;
			use frame_system_benchmarking::extensions::Pallet as SystemExtensionsBench;

			type XcmBalances = pallet_xcm_benchmarks::fungible::Pallet::<Runtime>;
			type XcmGeneric = pallet_xcm_benchmarks::generic::Pallet::<Runtime>;

			let mut list = Vec::<BenchmarkList>::new();
			list_benchmarks!(list, extra);

			let storage_info = AllPalletsWithSystem::storage_info();
			return (list, storage_info)
		}

		#[allow(non_local_definitions)]
		fn dispatch_benchmark(
			config: frame_benchmarking::BenchmarkConfig,
		) -> Result<
			Vec<frame_benchmarking::BenchmarkBatch>,
			alloc::string::String,
		> {
			use frame_support::traits::WhitelistedStorageKeys;
			use frame_benchmarking::{BenchmarkBatch, BenchmarkError};
			use sp_storage::TrackedStorageKey;
			use pallet_xcm::benchmarking::Pallet as PalletXcmExtrinsicsBenchmark;
			use frame_system_benchmarking::Pallet as SystemBench;
			use frame_system_benchmarking::extensions::Pallet as SystemExtensionsBench;
			use xcm_config::{AssetHub, TokenLocation};
			use alloc::boxed::Box;

			parameter_types! {
				pub ExistentialDepositAsset: Option<Asset> = Some((
					TokenLocation::get(),
					ExistentialDeposit::get()
				).into());
				pub AssetHubParaId: ParaId = westend_runtime_constants::system_parachain::ASSET_HUB_ID.into();
				pub const RandomParaId: ParaId = ParaId::new(43211234);
			}

			impl pallet_xcm::benchmarking::Config for Runtime {
				type DeliveryHelper = (
					polkadot_runtime_common::xcm_sender::ToParachainDeliveryHelper<
						xcm_config::XcmConfig,
						ExistentialDepositAsset,
						xcm_config::PriceForChildParachainDelivery,
						AssetHubParaId,
						Dmp,
					>,
					polkadot_runtime_common::xcm_sender::ToParachainDeliveryHelper<
						xcm_config::XcmConfig,
						ExistentialDepositAsset,
						xcm_config::PriceForChildParachainDelivery,
						RandomParaId,
						Dmp,
					>
				);

				fn reachable_dest() -> Option<Location> {
					Some(crate::xcm_config::AssetHub::get())
				}

				fn teleportable_asset_and_dest() -> Option<(Asset, Location)> {
					// Relay/native token can be teleported to/from AH.
					Some((
						Asset { fun: Fungible(ExistentialDeposit::get()), id: AssetId(Here.into()) },
						crate::xcm_config::AssetHub::get(),
					))
				}

				fn reserve_transferable_asset_and_dest() -> Option<(Asset, Location)> {
					None
				}

				fn set_up_complex_asset_transfer(
				) -> Option<(Assets, u32, Location, Box<dyn FnOnce()>)> {
					// Relay supports only native token, either reserve transfer it to non-system parachains,
					// or teleport it to system parachain. Use the teleport case for benchmarking as it's
					// slightly heavier.

					// Relay/native token can be teleported to/from AH.
					let native_location = Here.into();
					let dest = crate::xcm_config::AssetHub::get();
					pallet_xcm::benchmarking::helpers::native_teleport_as_asset_transfer::<Runtime>(
						native_location,
						dest
					)
				}

				// `get_assets` stays at its default: the `AssetTransactor` only handles the
				// native token, so a multi-asset worst case is not constructible here.
				fn get_asset() -> Asset {
					Asset {
						id: AssetId(Location::here()),
						fun: Fungible(ExistentialDeposit::get()),
					}
				}
				fn batch_call(calls: Vec<RuntimeCall>) -> Option<RuntimeCall> {
					Some(RuntimeCall::Utility(pallet_utility::Call::batch { calls }))
				}
			}
			impl frame_system_benchmarking::Config for Runtime {}
			impl pallet_transaction_payment::BenchmarkConfig for Runtime {}

			use xcm::latest::{
				AssetId, Fungibility::*, InteriorLocation, Junction, Junctions::*,
				Asset, Assets, Location, NetworkId, Response,
			};

			impl pallet_xcm_benchmarks::Config for Runtime {
				type XcmConfig = xcm_config::XcmConfig;
				type AccountIdConverter = xcm_config::LocationConverter;
				type DeliveryHelper = polkadot_runtime_common::xcm_sender::ToParachainDeliveryHelper<
					xcm_config::XcmConfig,
					ExistentialDepositAsset,
					xcm_config::PriceForChildParachainDelivery,
					AssetHubParaId,
					Dmp,
				>;
				fn valid_destination() -> Result<Location, BenchmarkError> {
					Ok(AssetHub::get())
				}
				fn worst_case_holding(_depositable_count: u32) -> xcm_executor::AssetsInHolding {
					use pallet_xcm_benchmarks::MockCredit;
					// Westend only knows about WND.
					let mut holding = xcm_executor::AssetsInHolding::new();
					holding.fungible.insert(
						AssetId(TokenLocation::get()),
						alloc::boxed::Box::new(MockCredit(1_000_000 * UNITS)),
					);
					holding
				}
			}

			parameter_types! {
				pub TrustedTeleporter: Option<(Location, Asset)> = Some((
					AssetHub::get(),
					Asset { fun: Fungible(1 * UNITS), id: AssetId(TokenLocation::get()) },
				));
				pub const TrustedReserve: Option<(Location, Asset)> = None;
				pub const CheckedAccount: Option<(AccountId, xcm_builder::MintLocation)> = None;
			}

			impl pallet_xcm_benchmarks::fungible::Config for Runtime {
				type TransactAsset = Balances;

				type CheckedAccount = CheckedAccount;
				type TrustedTeleporter = TrustedTeleporter;
				type TrustedReserve = TrustedReserve;

				fn get_asset() -> Asset {
					Asset {
						id: AssetId(TokenLocation::get()),
						fun: Fungible(1 * UNITS),
					}
				}
			}

			impl pallet_xcm_benchmarks::generic::Config for Runtime {
				type TransactAsset = Balances;
				type RuntimeCall = RuntimeCall;

				fn worst_case_response() -> (u64, Response) {
					(0u64, Response::Version(Default::default()))
				}

				fn worst_case_asset_exchange() -> Result<(Assets, Assets), BenchmarkError> {
					// Westend doesn't support asset exchanges
					Err(BenchmarkError::Skip)
				}

				fn universal_alias() -> Result<(Location, Junction), BenchmarkError> {
					// The XCM executor of Westend doesn't have a configured `UniversalAliases`
					Err(BenchmarkError::Skip)
				}

				fn transact_origin_and_runtime_call() -> Result<(Location, RuntimeCall), BenchmarkError> {
					Ok((AssetHub::get(), frame_system::Call::remark_with_event { remark: vec![] }.into()))
				}

				fn subscribe_origin() -> Result<Location, BenchmarkError> {
					Ok(AssetHub::get())
				}

				fn claimable_asset() -> Result<(Location, Location, Assets), BenchmarkError> {
					let origin = AssetHub::get();
					let assets: Assets = (AssetId(TokenLocation::get()), 1_000 * UNITS).into();
					let ticket = Location { parents: 0, interior: Here };
					Ok((origin, ticket, assets))
				}

				fn worst_case_for_trader() -> Result<(Asset, WeightLimit), BenchmarkError> {
					Ok((Asset {
						id: AssetId(TokenLocation::get()),
						fun: Fungible(1_000_000 * UNITS),
					}, WeightLimit::Limited(Weight::from_parts(5000, 5000))))
				}

				fn unlockable_asset() -> Result<(Location, Location, Asset), BenchmarkError> {
					// Westend doesn't support asset locking
					Err(BenchmarkError::Skip)
				}

				fn export_message_origin_and_destination(
				) -> Result<(Location, NetworkId, InteriorLocation), BenchmarkError> {
					// Westend doesn't support exporting messages
					Err(BenchmarkError::Skip)
				}

				fn alias_origin() -> Result<(Location, Location), BenchmarkError> {
					let origin = Location::new(0, [Parachain(1000)]);
					let target = Location::new(0, [Parachain(1000), AccountId32 { id: [128u8; 32], network: None }]);
					Ok((origin, target))
				}
			}

			type XcmBalances = pallet_xcm_benchmarks::fungible::Pallet::<Runtime>;
			type XcmGeneric = pallet_xcm_benchmarks::generic::Pallet::<Runtime>;

			let whitelist: Vec<TrackedStorageKey> = AllPalletsWithSystem::whitelisted_storage_keys();

			let mut batches = Vec::<BenchmarkBatch>::new();
			let params = (&config, &whitelist);

			add_benchmarks!(params, batches);

			Ok(batches)
		}
	}

	impl sp_genesis_builder::GenesisBuilder<Block> for Runtime {
		fn build_state(config: Vec<u8>) -> sp_genesis_builder::Result {
			build_state::<RuntimeGenesisConfig>(config)
		}

		fn get_preset(id: &Option<sp_genesis_builder::PresetId>) -> Option<Vec<u8>> {
			get_preset::<RuntimeGenesisConfig>(id, &genesis_config_presets::get_preset)
		}

		fn preset_names() -> Vec<sp_genesis_builder::PresetId> {
			genesis_config_presets::preset_names()
		}
	}

	impl xcm_runtime_apis::trusted_query::TrustedQueryApi<Block> for Runtime {
		fn is_trusted_reserve(asset: VersionedAsset, location: VersionedLocation) -> Result<bool, xcm_runtime_apis::trusted_query::Error> {
			XcmPallet::is_trusted_reserve(asset, location)
		}
		fn is_trusted_teleporter(asset: VersionedAsset, location: VersionedLocation) -> Result<bool, xcm_runtime_apis::trusted_query::Error> {
			XcmPallet::is_trusted_teleporter(asset, location)
		}
	}
}
