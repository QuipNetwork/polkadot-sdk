// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Parity Bridges Common.

// Parity Bridges Common is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity Bridges Common is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity Bridges Common.  If not, see <http://www.gnu.org/licenses/>.

#![cfg(test)]

use crate as pallet_bridge_relayers;

use bp_header_chain::ChainWithGrandpa;
use bp_messages::{
	target_chain::{DispatchMessage, MessageDispatch},
	ChainWithMessages, HashedLaneId, LaneIdType, MessageNonce,
};
use bp_parachains::SingleParaStoredHeaderDataBuilder;
use bp_relayers::{
	PayRewardFromAccount, PaymentProcedure, RewardsAccountOwner, RewardsAccountParams,
};
use bp_runtime::{messages::MessageDispatchResult, Chain, ChainId, Parachain};
use codec::Encode;
use frame_support::{
	derive_impl, parameter_types,
	traits::fungible::Mutate,
	weights::{ConstantMultiplier, IdentityFee, RuntimeDbWeight, Weight},
};
use pallet_transaction_payment::Multiplier;
use sp_core::H256;
use sp_runtime::{
	traits::{BlakeTwo256, ConstU32, ConstU64, ConstU8},
	BuildStorage, FixedPointNumber, Perquintill, StateVersion,
};

/// Account identifier at `ThisChain`.
pub type ThisChainAccountId = u64;
/// Balance at `ThisChain`.
pub type ThisChainBalance = u64;
/// Block number at `ThisChain`.
pub type ThisChainBlockNumber = u32;
/// Hash at `ThisChain`.
pub type ThisChainHash = H256;
/// Hasher at `ThisChain`.
pub type ThisChainHasher = BlakeTwo256;
/// Header of `ThisChain`.
pub type ThisChainHeader = sp_runtime::generic::Header<ThisChainBlockNumber, ThisChainHasher>;
/// Block of `ThisChain`.
pub type ThisChainBlock = frame_system::mocking::MockBlockU32<TestRuntime>;

/// Account identifier at the `BridgedChain`.
pub type BridgedChainAccountId = u128;
/// Balance at the `BridgedChain`.
pub type BridgedChainBalance = u128;
/// Block number at the `BridgedChain`.
pub type BridgedChainBlockNumber = u32;
/// Hash at the `BridgedChain`.
pub type BridgedChainHash = H256;
/// Hasher at the `BridgedChain`.
pub type BridgedChainHasher = BlakeTwo256;
/// Header of the `BridgedChain`.
pub type BridgedChainHeader =
	sp_runtime::generic::Header<BridgedChainBlockNumber, BridgedChainHasher>;

/// Bridged chain id used in tests.
pub const TEST_BRIDGED_CHAIN_ID: ChainId = *b"brdg";
/// Maximal extrinsic size at the `BridgedChain`.
pub const BRIDGED_CHAIN_MAX_EXTRINSIC_SIZE: u32 = 1024;

/// Lane identifier type used for tests.
pub type TestLaneIdType = HashedLaneId;
/// Lane that we're using in tests.
pub fn test_lane_id() -> TestLaneIdType {
	TestLaneIdType::try_new(1, 2).unwrap()
}
/// Reward measurement type.
pub type RewardBalance = u64;

/// Underlying chain of `ThisChain`.
pub struct ThisUnderlyingChain;

impl Chain for ThisUnderlyingChain {
	const ID: ChainId = *b"tuch";

	type BlockNumber = ThisChainBlockNumber;
	type Hash = ThisChainHash;
	type Hasher = ThisChainHasher;
	type Header = ThisChainHeader;
	type AccountId = ThisChainAccountId;
	type Balance = ThisChainBalance;
	type Nonce = u32;
	type Signature = sp_runtime::MultiSignature;

	const STATE_VERSION: StateVersion = StateVersion::V1;

	fn max_extrinsic_size() -> u32 {
		BRIDGED_CHAIN_MAX_EXTRINSIC_SIZE
	}

	fn max_extrinsic_weight() -> Weight {
		Weight::zero()
	}
}

impl ChainWithMessages for ThisUnderlyingChain {
	const WITH_CHAIN_MESSAGES_PALLET_NAME: &'static str = "";

	const MAX_UNREWARDED_RELAYERS_IN_CONFIRMATION_TX: MessageNonce = 16;
	const MAX_UNCONFIRMED_MESSAGES_IN_CONFIRMATION_TX: MessageNonce = 1000;
}

/// Underlying chain of `BridgedChain`.
pub struct BridgedUnderlyingParachain;

impl Chain for BridgedUnderlyingParachain {
	const ID: ChainId = TEST_BRIDGED_CHAIN_ID;

	type BlockNumber = BridgedChainBlockNumber;
	type Hash = BridgedChainHash;
	type Hasher = BridgedChainHasher;
	type Header = BridgedChainHeader;
	type AccountId = BridgedChainAccountId;
	type Balance = BridgedChainBalance;
	type Nonce = u32;
	type Signature = sp_runtime::MultiSignature;

	const STATE_VERSION: StateVersion = StateVersion::V1;

	fn max_extrinsic_size() -> u32 {
		BRIDGED_CHAIN_MAX_EXTRINSIC_SIZE
	}
	fn max_extrinsic_weight() -> Weight {
		Weight::zero()
	}
}

impl ChainWithGrandpa for BridgedUnderlyingParachain {
	const WITH_CHAIN_GRANDPA_PALLET_NAME: &'static str = "";
	const MAX_AUTHORITIES_COUNT: u32 = 16;
	const REASONABLE_HEADERS_IN_JUSTIFICATION_ANCESTRY: u32 = 8;
	const MAX_MANDATORY_HEADER_SIZE: u32 = 256;
	const AVERAGE_HEADER_SIZE: u32 = 64;
}

impl ChainWithMessages for BridgedUnderlyingParachain {
	const WITH_CHAIN_MESSAGES_PALLET_NAME: &'static str = "";
	const MAX_UNREWARDED_RELAYERS_IN_CONFIRMATION_TX: MessageNonce = 16;
	const MAX_UNCONFIRMED_MESSAGES_IN_CONFIRMATION_TX: MessageNonce = 1000;
}

impl Parachain for BridgedUnderlyingParachain {
	const PARACHAIN_ID: u32 = 42;
	const MAX_HEADER_SIZE: u32 = 1_024;
}

pub type TestStakeAndSlash = pallet_bridge_relayers::StakeAndSlashNamed<
	ThisChainAccountId,
	ThisChainBlockNumber,
	Balances,
	ReserveId,
	Stake,
	Lease,
>;

frame_support::construct_runtime! {
	pub enum TestRuntime
	{
		System: frame_system,
		Utility: pallet_utility,
		Balances: pallet_balances,
		TransactionPayment: pallet_transaction_payment,
		BridgeRelayers: pallet_bridge_relayers,
		BridgeGrandpa: pallet_bridge_grandpa,
		BridgeParachains: pallet_bridge_parachains,
		BridgeMessages: pallet_bridge_messages,
	}
}

parameter_types! {
	pub const BridgedParasPalletName: &'static str = "Paras";
	pub const DbWeight: RuntimeDbWeight = RuntimeDbWeight { read: 1, write: 2 };
	pub const ExistentialDeposit: ThisChainBalance = 1;
	pub const ReserveId: [u8; 8] = *b"brdgrlrs";
	pub const Stake: ThisChainBalance = 1_000;
	pub const Lease: ThisChainBlockNumber = 8;
	pub const TargetBlockFullness: Perquintill = Perquintill::from_percent(25);
	pub const TransactionBaseFee: ThisChainBalance = 0;
	pub const TransactionByteFee: ThisChainBalance = 1;
	pub AdjustmentVariable: Multiplier = Multiplier::saturating_from_rational(3, 100_000);
	pub MinimumMultiplier: Multiplier = Multiplier::saturating_from_rational(1, 1_000_000u128);
	pub MaximumMultiplier: Multiplier = sp_runtime::traits::Bounded::max_value();
}

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for TestRuntime {
	type Block = ThisChainBlock;
	// TODO: remove when https://github.com/paritytech/polkadot-sdk/pull/4543 merged
	type BlockHashCount = ConstU32<10>;
	type AccountData = pallet_balances::AccountData<ThisChainBalance>;
	type DbWeight = DbWeight;
}

#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig)]
impl pallet_balances::Config for TestRuntime {
	type ReserveIdentifier = [u8; 8];
	type AccountStore = System;
}

impl pallet_utility::Config for TestRuntime {
	type RuntimeEvent = RuntimeEvent;
	type RuntimeCall = RuntimeCall;
	type PalletsOrigin = OriginCaller;
	type WeightInfo = ();
}

#[derive_impl(pallet_transaction_payment::config_preludes::TestDefaultConfig)]
impl pallet_transaction_payment::Config for TestRuntime {
	type OnChargeTransaction = pallet_transaction_payment::FungibleAdapter<Balances, ()>;
	type OperationalFeeMultiplier = ConstU8<5>;
	type WeightToFee = IdentityFee<ThisChainBalance>;
	type LengthToFee = ConstantMultiplier<ThisChainBalance, TransactionByteFee>;
	type FeeMultiplierUpdate = pallet_transaction_payment::TargetedFeeAdjustment<
		TestRuntime,
		TargetBlockFullness,
		AdjustmentVariable,
		MinimumMultiplier,
		MaximumMultiplier,
	>;
	type RuntimeEvent = RuntimeEvent;
}

impl pallet_bridge_grandpa::Config for TestRuntime {
	type RuntimeEvent = RuntimeEvent;
	type BridgedChain = BridgedUnderlyingParachain;
	type MaxFreeHeadersPerBlock = ConstU32<4>;
	type FreeHeadersInterval = ConstU32<1_024>;
	type HeadersToKeep = ConstU32<8>;
	type WeightInfo = pallet_bridge_grandpa::weights::BridgeWeight<TestRuntime>;
}

impl pallet_bridge_parachains::Config for TestRuntime {
	type RuntimeEvent = RuntimeEvent;
	type BridgesGrandpaPalletInstance = ();
	type ParasPalletName = BridgedParasPalletName;
	type ParaStoredHeaderDataBuilder =
		SingleParaStoredHeaderDataBuilder<BridgedUnderlyingParachain>;
	type HeadsToKeep = ConstU32<8>;
	type MaxParaHeadDataSize = ConstU32<1024>;
	type WeightInfo = pallet_bridge_parachains::weights::BridgeWeight<TestRuntime>;
	type OnNewHead = ();
}

impl pallet_bridge_messages::Config for TestRuntime {
	type RuntimeEvent = RuntimeEvent;
	type WeightInfo = pallet_bridge_messages::weights::BridgeWeight<TestRuntime>;

	type OutboundPayload = Vec<u8>;
	type InboundPayload = Vec<u8>;
	type LaneId = TestLaneIdType;

	type DeliveryPayments = ();
	type DeliveryConfirmationPayments = pallet_bridge_relayers::DeliveryConfirmationPaymentsAdapter<
		TestRuntime,
		(),
		(),
		ConstU64<100_000>,
	>;
	type OnMessagesDelivered = ();

	type MessageDispatch = DummyMessageDispatch;
	type ThisChain = ThisUnderlyingChain;
	type BridgedChain = BridgedUnderlyingParachain;
	type BridgedHeaderChain = BridgeGrandpa;
}

impl pallet_bridge_relayers::Config for TestRuntime {
	type RuntimeEvent = RuntimeEvent;
	type RewardBalance = RewardBalance;
	type Reward = RewardsAccountParams<pallet_bridge_messages::LaneIdOf<TestRuntime, ()>>;
	type PaymentProcedure = TestPaymentProcedure;
	type StakeAndSlash = TestStakeAndSlash;
	type Balance = ThisChainBalance;
	type WeightInfo = ();
}

#[cfg(feature = "runtime-benchmarks")]
impl pallet_bridge_relayers::benchmarking::Config for TestRuntime {
	fn bench_reward() -> Self::Reward {
		RewardsAccountParams::new(
			TestLaneIdType::default(),
			*b"test",
			RewardsAccountOwner::ThisChain,
		)
	}

	fn prepare_rewards_account(
		_relayer: &ThisChainAccountId,
		account_params: RewardsAccountParams<TestLaneIdType>,
		reward: Self::RewardBalance,
	) -> Option<(RewardsAccountParams<TestLaneIdType>, ThisChainAccountId)> {
		let rewards_account = PayRewardFromAccount::<
			Balances,
			ThisChainAccountId,
			TestLaneIdType,
			RewardBalance,
		>::rewards_account(account_params);
		Self::deposit_account(rewards_account, reward.into());

		Some((account_params, REGULAR_RELAYER2))
	}

	fn deposit_account(account: Self::AccountId, balance: Self::Balance) {
		frame_support::assert_ok!(Balances::mint_into(
			&account,
			balance.saturating_add(ExistentialDeposit::get())
		));
	}
}

/// Regular relayer that may receive rewards.
pub const REGULAR_RELAYER: ThisChainAccountId = 1;
/// Regular relayer that may receive rewards.
pub const REGULAR_RELAYER2: ThisChainAccountId = 3;

/// Relayer that can't receive rewards.
pub const FAILING_RELAYER: ThisChainAccountId = 2;

/// Relayer that is able to register.
pub const REGISTER_RELAYER: ThisChainAccountId = 42;

/// Payment procedure that rejects payments to the `FAILING_RELAYER`.
pub struct TestPaymentProcedure;

impl TestPaymentProcedure {
	pub fn rewards_account(params: RewardsAccountParams<TestLaneIdType>) -> ThisChainAccountId {
		PayRewardFromAccount::<(), ThisChainAccountId, TestLaneIdType, RewardBalance>::rewards_account(
			params,
		)
	}
}

impl PaymentProcedure<ThisChainAccountId, RewardsAccountParams<TestLaneIdType>, RewardBalance>
	for TestPaymentProcedure
{
	type Error = ();
	type Beneficiary = ThisChainAccountId;

	fn pay_reward(
		relayer: &ThisChainAccountId,
		_reward_kind: RewardsAccountParams<TestLaneIdType>,
		_reward: RewardBalance,
		_beneficiary: Self::Beneficiary,
	) -> Result<(), Self::Error> {
		match *relayer {
			FAILING_RELAYER => Err(()),
			_ => Ok(()),
		}
	}
}

/// Dummy message dispatcher.
pub struct DummyMessageDispatch;

impl DummyMessageDispatch {
	pub fn deactivate(lane: TestLaneIdType) {
		frame_support::storage::unhashed::put(&(b"inactive", lane).encode()[..], &false);
	}
}

impl MessageDispatch for DummyMessageDispatch {
	type DispatchPayload = Vec<u8>;
	type DispatchLevelResult = ();
	type LaneId = TestLaneIdType;

	fn is_active(lane: Self::LaneId) -> bool {
		frame_support::storage::unhashed::take::<bool>(&(b"inactive", lane).encode()[..]) !=
			Some(false)
	}

	fn dispatch_weight(
		_message: &mut DispatchMessage<Self::DispatchPayload, Self::LaneId>,
	) -> Weight {
		Weight::zero()
	}

	fn dispatch(
		_: DispatchMessage<Self::DispatchPayload, Self::LaneId>,
	) -> MessageDispatchResult<Self::DispatchLevelResult> {
		MessageDispatchResult { unspent_weight: Weight::zero(), dispatch_level_result: () }
	}
}

/// Reward account params that we are using in tests.
pub fn test_reward_account_param() -> RewardsAccountParams<TestLaneIdType> {
	RewardsAccountParams::new(
		TestLaneIdType::try_new(1, 2).unwrap(),
		*b"test",
		RewardsAccountOwner::ThisChain,
	)
}

/// Return test externalities to use in tests.
pub fn new_test_ext() -> sp_io::TestExternalities {
	let t = frame_system::GenesisConfig::<TestRuntime>::default().build_storage().unwrap();
	sp_io::TestExternalities::new(t)
}

/// Run pallet test.
pub fn run_test<T>(test: impl FnOnce() -> T) -> T {
	new_test_ext().execute_with(|| {
		Balances::mint_into(&REGISTER_RELAYER, ExistentialDeposit::get() + 10 * Stake::get())
			.unwrap();

		test()
	})
}
