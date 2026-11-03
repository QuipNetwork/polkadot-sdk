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

#![cfg_attr(not(feature = "std"), no_std)]

// Make the WASM binary available.
#[cfg(feature = "std")]
include!(concat!(env!("OUT_DIR"), "/wasm_binary.rs"));

extern crate alloc;

use alloc::{vec, vec::Vec};
use currency::*;
use frame_support::weights::{
	constants::{BlockExecutionWeight, ExtrinsicBaseWeight, WEIGHT_REF_TIME_PER_SECOND},
	Weight,
};
use frame_system::limits::BlockWeights;
use pallet_revive::{
	evm::{
		fees::{BlockRatioFee, Info as FeeInfo},
		runtime::EthExtra,
	},
	AccountId32Mapper,
};
use pallet_transaction_payment::{ConstFeeMultiplier, FeeDetails, Multiplier, RuntimeDispatchInfo};
use polkadot_sdk::{
	polkadot_sdk_frame::{
		deps::sp_genesis_builder,
		runtime::{apis, prelude::*},
		traits::Block as BlockT,
	},
	*,
};
use sp_weights::ConstantMultiplier;

pub use polkadot_sdk::{
	parachains_common::{AccountId, Balance, BlockNumber, Hash, Header, Nonce, Signature},
	polkadot_sdk_frame::runtime::types_common::OpaqueBlock,
};

pub mod currency {
	use super::Balance;
	pub const DOLLARS: Balance = 1_000_000_000_000;
	pub const CENTS: Balance = DOLLARS / 100;
	pub const MILLICENTS: Balance = CENTS / 1_000;
}

/// Provides getters for genesis configuration presets.
pub mod genesis_config_presets {
	use super::*;
	use crate::{
		currency::DOLLARS, sp_keyring::Sr25519Keyring, Balance, BalancesConfig, ReviveConfig,
		RuntimeGenesisConfig, SudoConfig,
	};

	use alloc::{vec, vec::Vec};
	use pallet_revive::AddressMapper;
	use serde_json::Value;

	pub const ENDOWMENT: Balance = 10_000_000_000_001 * DOLLARS;

	fn well_known_accounts() -> Vec<AccountId> {
		Sr25519Keyring::well_known()
			.map(|k| k.to_account_id())
			.chain([
				// subxt_signer::eth::dev::alith()
				array_bytes::hex_n_into_unchecked(
					"f24ff3a9cf04c71dbc94d0b566f7a27b94566caceeeeeeeeeeeeeeeeeeeeeeee",
				),
				// subxt_signer::eth::dev::baltathar()
				array_bytes::hex_n_into_unchecked(
					"3cd0a705a2dc65e5b1e1205896baa2be8a07c6e0eeeeeeeeeeeeeeeeeeeeeeee",
				),
				// subxt_signer::eth::dev::charleth()
				array_bytes::hex_n_into_unchecked(
					"798d4ba9baf0064ec19eb4f0a1a45785ae9d6dfceeeeeeeeeeeeeeeeeeeeeeee",
				),
				// subxt_signer::eth::dev::dorothy()
				array_bytes::hex_n_into_unchecked(
					"773539d4ac0e786233d90a233654ccee26a613d9eeeeeeeeeeeeeeeeeeeeeeee",
				),
				// subxt_signer::eth::dev::ethan()
				array_bytes::hex_n_into_unchecked(
					"ff64d3f6efe2317ee2807d223a0bdc4c0c49dfdbeeeeeeeeeeeeeeeeeeeeeeee",
				),
			])
			.collect::<Vec<_>>()
	}

	/// Returns a development genesis config preset.
	pub fn development_config_genesis() -> Value {
		let endowed_accounts = well_known_accounts();
		frame_support::build_struct_json_patch!(RuntimeGenesisConfig {
			balances: BalancesConfig {
				balances: endowed_accounts
					.iter()
					.cloned()
					.map(|id| (id, ENDOWMENT))
					.collect::<Vec<_>>(),
			},
			sudo: SudoConfig { key: Some(Sr25519Keyring::Alice.to_account_id()) },
			revive: ReviveConfig {
				mapped_accounts: endowed_accounts
					.iter()
					.filter(|x| !<Runtime as pallet_revive::Config>::AddressMapper::is_mapped(x))
					.cloned()
					.collect(),
			},
		})
	}

	/// Get the set of the available genesis config presets.
	pub fn get_preset(id: &PresetId) -> Option<Vec<u8>> {
		let patch = match id.as_ref() {
			sp_genesis_builder::DEV_RUNTIME_PRESET => development_config_genesis(),
			_ => return None,
		};
		Some(
			serde_json::to_string(&patch)
				.expect("serialization to json is expected to work. qed.")
				.into_bytes(),
		)
	}

	/// List of supported presets.
	pub fn preset_names() -> Vec<PresetId> {
		vec![PresetId::from(sp_genesis_builder::DEV_RUNTIME_PRESET)]
	}
}

/// The runtime version.
#[runtime_version]
pub const VERSION: RuntimeVersion = RuntimeVersion {
	spec_name: alloc::borrow::Cow::Borrowed("revive-dev-runtime"),
	impl_name: alloc::borrow::Cow::Borrowed("revive-dev-runtime"),
	authoring_version: 1,
	spec_version: 0,
	impl_version: 1,
	apis: RUNTIME_API_VERSIONS,
	transaction_version: 1,
	system_version: 1,
};

/// The address format for describing accounts.
pub type Address = sp_runtime::MultiAddress<AccountId, ()>;
/// Block type as expected by this runtime.
pub type Block = sp_runtime::generic::Block<Header, UncheckedExtrinsic>;
/// The transaction extensions that are added to the runtime.
type TxExtension = (
	// Checks that the sender is not the zero address.
	frame_system::CheckNonZeroSender<Runtime>,
	// Checks that the runtime version is correct.
	frame_system::CheckSpecVersion<Runtime>,
	// Checks that the transaction version is correct.
	frame_system::CheckTxVersion<Runtime>,
	// Checks that the genesis hash is correct.
	frame_system::CheckGenesis<Runtime>,
	// Checks that the era is valid.
	frame_system::CheckEra<Runtime>,
	// Checks that the nonce is valid.
	frame_system::CheckNonce<Runtime>,
	// Checks that the weight is valid.
	frame_system::CheckWeight<Runtime>,
	// Ensures that the sender has enough funds to pay for the transaction
	// and deducts the fee from the sender's account.
	pallet_transaction_payment::ChargeTransactionPayment<Runtime>,
	// Needs to be done after all extensions that rely on a signed origin.
	pallet_revive::evm::tx_extension::SetOrigin<Runtime>,
	// Reclaim the unused weight from the block using post dispatch information.
	// It must be last in the pipeline in order to catch the refund in previous transaction
	// extensions
	frame_system::WeightReclaim<Runtime>,
);

/// Default extensions applied to Ethereum transactions.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct EthExtraImpl;

impl EthExtra for EthExtraImpl {
	type Config = Runtime;
	type ExtensionV0 = TxExtension;
	type ExtensionOtherVersions = sp_runtime::traits::InvalidVersion;

	fn get_eth_extension(nonce: u32, tip: Balance) -> Self::ExtensionV0 {
		(
			frame_system::CheckNonZeroSender::<Runtime>::new(),
			frame_system::CheckSpecVersion::<Runtime>::new(),
			frame_system::CheckTxVersion::<Runtime>::new(),
			frame_system::CheckGenesis::<Runtime>::new(),
			frame_system::CheckMortality::from(sp_runtime::generic::Era::Immortal),
			frame_system::CheckNonce::<Runtime>::from(nonce),
			frame_system::CheckWeight::<Runtime>::new(),
			pallet_transaction_payment::ChargeTransactionPayment::<Runtime>::from(tip),
			pallet_revive::evm::tx_extension::SetOrigin::<Runtime>::new_from_eth_transaction(),
			frame_system::WeightReclaim::<Runtime>::new(),
		)
	}
}

pub type UncheckedExtrinsic =
	pallet_revive::evm::runtime::UncheckedExtrinsic<Address, Signature, EthExtraImpl>;

type Executive = frame_executive::Executive<
	Runtime,
	Block,
	frame_system::ChainContext<Runtime>,
	Runtime,
	AllPalletsWithSystem,
>;

// Composes the runtime by adding all the used pallets and deriving necessary types.
#[frame_construct_runtime]
mod runtime {
	/// The main runtime type.
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

	/// Mandatory system pallet that should always be included in a FRAME runtime.
	#[runtime::pallet_index(0)]
	pub type System = frame_system::Pallet<Runtime>;

	/// Provides a way for consensus systems to set and check the onchain time.
	#[runtime::pallet_index(1)]
	pub type Timestamp = pallet_timestamp::Pallet<Runtime>;

	/// Provides the ability to keep track of balances.
	#[runtime::pallet_index(2)]
	pub type Balances = pallet_balances::Pallet<Runtime>;

	/// Provides a way to execute privileged functions.
	#[runtime::pallet_index(3)]
	pub type Sudo = pallet_sudo::Pallet<Runtime>;

	/// Provides the ability to charge for extrinsic execution.
	#[runtime::pallet_index(4)]
	pub type TransactionPayment = pallet_transaction_payment::Pallet<Runtime>;

	/// Provides the ability to execute Smart Contracts.
	#[runtime::pallet_index(5)]
	pub type Revive = pallet_revive::Pallet<Runtime>;
}

/// We assume that ~10% of the block weight is consumed by `on_initialize` handlers.
/// This is used to limit the maximal weight of a single extrinsic.
const AVERAGE_ON_INITIALIZE_RATIO: Perbill = Perbill::from_percent(10);
/// We allow `Normal` extrinsics to fill up the block up to 75%, the rest can be used
/// by  Operational  extrinsics.
const NORMAL_DISPATCH_RATIO: Perbill = Perbill::from_percent(75);
/// We allow for 2 seconds of compute with a 6 second average block time, with maximum proof size.
const MAXIMUM_BLOCK_WEIGHT: Weight = Weight::from_parts(
	WEIGHT_REF_TIME_PER_SECOND.saturating_mul(2),
	polkadot_primitives::MAX_POV_SIZE as u64,
);

parameter_types! {
	pub const Version: RuntimeVersion = VERSION;
	pub RuntimeBlockWeights: BlockWeights = BlockWeights::builder()
		.base_block(BlockExecutionWeight::get())
		.for_class(DispatchClass::all(), |weights| {
			weights.base_extrinsic = ExtrinsicBaseWeight::get();
		})
		.for_class(DispatchClass::Normal, |weights| {
			weights.max_total = Some(NORMAL_DISPATCH_RATIO * MAXIMUM_BLOCK_WEIGHT);
		})
		.for_class(DispatchClass::Operational, |weights| {
			weights.max_total = Some(MAXIMUM_BLOCK_WEIGHT);
			// Operational transactions have some extra reserved space, so that they
			// are included even if block reached `MAXIMUM_BLOCK_WEIGHT`.
			weights.reserved = Some(
				MAXIMUM_BLOCK_WEIGHT - NORMAL_DISPATCH_RATIO * MAXIMUM_BLOCK_WEIGHT
			);
		})
		.avg_block_initialization(AVERAGE_ON_INITIALIZE_RATIO)
		.build_or_panic();
}

/// Implements the types required for the system pallet.
#[derive_impl(frame_system::config_preludes::SolochainDefaultConfig)]
impl frame_system::Config for Runtime {
	type Block = Block;
	type BlockWeights = RuntimeBlockWeights;
	type Version = Version;
	type AccountId = AccountId;
	type Hash = Hash;
	type Nonce = Nonce;
	type AccountData = pallet_balances::AccountData<<Runtime as pallet_balances::Config>::Balance>;
	type OnNewAccount = pallet_revive::AutoMapper<Runtime>;
	type OnKilledAccount = pallet_revive::AutoMapper<Runtime>;
}

parameter_types! {
	pub const ExistentialDeposit: Balance = CENTS;
}

// Implements the types required for the balances pallet.
#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig)]
impl pallet_balances::Config for Runtime {
	type AccountStore = System;
	type Balance = Balance;
	type ExistentialDeposit = ExistentialDeposit;
	type RuntimeFreezeReason = RuntimeFreezeReason;
	type FreezeIdentifier = RuntimeFreezeReason;
	type MaxFreezes = frame_support::traits::VariantCountOf<RuntimeFreezeReason>;
}

// Implements the types required for the sudo pallet.
#[derive_impl(pallet_sudo::config_preludes::TestDefaultConfig)]
impl pallet_sudo::Config for Runtime {}

// Implements the types required for the sudo pallet.
#[derive_impl(pallet_timestamp::config_preludes::TestDefaultConfig)]
impl pallet_timestamp::Config for Runtime {}

parameter_types! {
	pub const TransactionByteFee: Balance = 10 * MILLICENTS;
	pub FeeMultiplier: Multiplier = Multiplier::one();
}

// Implements the types required for the transaction payment pallet.
#[derive_impl(pallet_transaction_payment::config_preludes::TestDefaultConfig)]
impl pallet_transaction_payment::Config for Runtime {
	type OnChargeTransaction = pallet_transaction_payment::FungibleAdapter<Balances, ()>;
	type WeightToFee = BlockRatioFee<1, 1, Self, Balance>;
	type LengthToFee = ConstantMultiplier<Balance, TransactionByteFee>;
	type FeeMultiplierUpdate = ConstFeeMultiplier<FeeMultiplier>;
}

parameter_types! {
	pub CodeHashLockupDepositPercent: Perbill = Perbill::from_percent(30);
}

#[derive_impl(pallet_revive::config_preludes::TestDefaultConfig)]
impl pallet_revive::Config for Runtime {
	type AddressMapper = AccountId32Mapper<Self>;
	type ChainId = ConstU64<420_420_420>;
	type CodeHashLockupDepositPercent = CodeHashLockupDepositPercent;
	type Balance = Balance;
	type Currency = Balances;
	type NativeToEthRatio = ConstU32<1_000_000>;
	type UploadOrigin = EnsureSigned<Self::AccountId>;
	type InstantiateOrigin = EnsureSigned<Self::AccountId>;
	type Time = Timestamp;
	type FeeInfo = FeeInfo<Address, Signature, EthExtraImpl>;
	type DebugEnabled = ConstBool<true>;
	type AutoMap = ConstBool<true>;
	type GasScale = ConstU32<50000>;
}

pallet_revive::impl_runtime_apis_plus_revive_traits!(
	Runtime,
	Revive,
	Executive,
	EthExtraImpl,

	impl apis::Core<Block> for Runtime {
		fn version() -> RuntimeVersion {
			VERSION
		}

		fn execute_block(block: <Block as BlockT>::LazyBlock) {
			Executive::execute_block(block)
		}

		fn initialize_block(header: &Header) -> ExtrinsicInclusionMode {
			Executive::initialize_block(header)
		}
	}

	impl apis::Metadata<Block> for Runtime {
		fn metadata() -> OpaqueMetadata {
			OpaqueMetadata::new(Runtime::metadata().into())
		}

		fn metadata_at_version(version: u32) -> Option<OpaqueMetadata> {
			Runtime::metadata_at_version(version)
		}

		fn metadata_versions() -> Vec<u32> {
			Runtime::metadata_versions()
		}
	}

	impl apis::BlockBuilder<Block> for Runtime {
		fn apply_extrinsic(extrinsic: ExtrinsicFor<Runtime>) -> ApplyExtrinsicResult {
			Executive::apply_extrinsic(extrinsic)
		}

		fn finalize_block() -> HeaderFor<Runtime> {
			Executive::finalize_block()
		}

		fn inherent_extrinsics(data: InherentData) -> Vec<ExtrinsicFor<Runtime>> {
			data.create_extrinsics()
		}

		fn check_inherents(
			block: <Block as BlockT>::LazyBlock,
			data: InherentData,
		) -> CheckInherentsResult {
			data.check_extrinsics(&block)
		}
	}

	impl apis::TaggedTransactionQueue<Block> for Runtime {
		fn validate_transaction(
			source: TransactionSource,
			tx: ExtrinsicFor<Runtime>,
			block_hash: <Runtime as frame_system::Config>::Hash,
		) -> TransactionValidity {
			Executive::validate_transaction(source, tx, block_hash)
		}
	}

	impl apis::OffchainWorkerApi<Block> for Runtime {
		fn offchain_worker(header: &HeaderFor<Runtime>) {
			Executive::offchain_worker(header)
		}
	}

	impl apis::SessionKeys<Block> for Runtime {
		fn generate_session_keys(_owner: Vec<u8>, _seed: Option<Vec<u8>>) -> apis::OpaqueGeneratedSessionKeys {
		Default::default()
		}


		fn decode_session_keys(
			_encoded: Vec<u8>,
		) -> Option<Vec<(Vec<u8>, apis::KeyTypeId)>> {
			Default::default()
		}
	}

	impl apis::AccountNonceApi<Block, AccountId, Nonce> for Runtime {
		fn account_nonce(account: AccountId) -> Nonce {
			System::account_nonce(account)
		}
	}

	impl pallet_transaction_payment_rpc_runtime_api::TransactionPaymentApi<
		Block,
		Balance,
	> for Runtime {
		fn query_info(uxt: ExtrinsicFor<Runtime>, len: u32) -> RuntimeDispatchInfo<Balance> {
			TransactionPayment::query_info(uxt, len)
		}
		fn query_fee_details(uxt: ExtrinsicFor<Runtime>, len: u32) -> FeeDetails<Balance> {
			TransactionPayment::query_fee_details(uxt, len)
		}
		fn query_weight_to_fee(weight: Weight) -> Balance {
			TransactionPayment::weight_to_fee(weight)
		}
		fn query_length_to_fee(length: u32) -> Balance {
			TransactionPayment::length_to_fee(length)
		}
	}

	impl apis::GenesisBuilder<Block> for Runtime {
		fn build_state(config: Vec<u8>) -> sp_genesis_builder::Result {
			build_state::<RuntimeGenesisConfig>(config)
		}

		fn get_preset(id: &Option<PresetId>) -> Option<Vec<u8>> {
			get_preset::<RuntimeGenesisConfig>(id, self::genesis_config_presets::get_preset)
		}

		fn preset_names() -> Vec<PresetId> {
			self::genesis_config_presets::preset_names()
		}
	}
);
