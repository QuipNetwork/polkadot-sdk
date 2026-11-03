// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
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

use super::{transaction_extension::DynamicMaxBlockWeight, *};
use crate::{self as parachain_system, MessagingStateSnapshot, PreviousCoreCount};
use codec::Compact;
use cumulus_primitives_core::{
	BlockBundleInfo, ClaimQueueOffset, CoreInfo, CoreSelector, CumulusDigestItem,
};
use frame_support::{
	construct_runtime, derive_impl,
	dispatch::DispatchClass,
	migrations::MultiStepMigrator,
	parameter_types,
	traits::PreInherents,
	weights::{
		constants::{BlockExecutionWeight, ExtrinsicBaseWeight},
		Weight,
	},
};
use frame_system::{limits::BlockWeights, CheckWeight};
use polkadot_primitives::PersistedValidationData;
use sp_core::ConstU32;
use sp_io;
use sp_runtime::{
	generic::{self, UncheckedExtrinsic},
	testing::UintAuthorityId,
	BuildStorage, Perbill,
};

const AVERAGE_ON_INITIALIZE_RATIO: Perbill = Perbill::from_percent(1);

/// A simple call, which one doesn't matter.
pub const CALL: &RuntimeCall =
	&RuntimeCall::System(frame_system::Call::set_heap_pages { pages: 0u64 });

pub type ExtrinsicOnlyOperational = UncheckedExtrinsic<
	UintAuthorityId,
	only_operational_runtime::RuntimeCall,
	UintAuthorityId,
	DynamicMaxBlockWeight<
		RuntimeOnlyOperational,
		CheckWeight<RuntimeOnlyOperational>,
		ConstU32<TARGET_BLOCK_RATE>,
		10,
		false,
	>,
>;

pub type Extrinsic = UncheckedExtrinsic<
	UintAuthorityId,
	RuntimeCall,
	UintAuthorityId,
	DynamicMaxBlockWeight<Runtime, CheckWeight<Runtime>, ConstU32<TARGET_BLOCK_RATE>>,
>;

pub type Block =
	generic::Block<generic::Header<u64, <Runtime as frame_system::Config>::Hashing>, Extrinsic>;

pub type BlockOnlyOperational = generic::Block<
	generic::Header<u64, <Runtime as frame_system::Config>::Hashing>,
	ExtrinsicOnlyOperational,
>;

pub const TARGET_BLOCK_RATE: u32 = 12;
pub const NORMAL_DISPATCH_RATIO: Perbill = Perbill::from_percent(75);

#[docify::export(tx_extension_setup)]
pub type TxExtension = DynamicMaxBlockWeight<
	Runtime,
	// Here you need to set the other extensions that are required by your runtime...
	(
		frame_system::AuthorizeCall<Runtime>,
		frame_system::CheckNonZeroSender<Runtime>,
		frame_system::CheckSpecVersion<Runtime>,
		frame_system::CheckGenesis<Runtime>,
		frame_system::CheckEra<Runtime>,
		frame_system::CheckNonce<Runtime>,
		frame_system::CheckWeight<Runtime>,
	),
	ConstU32<TARGET_BLOCK_RATE>,
>;

#[allow(dead_code)]
type NotDeadCode = TxExtension;

#[docify::export_content(max_block_weight_setup)]
mod max_block_weight_setup {
	use super::*;

	type MaximumBlockWeight = MaxParachainBlockWeight<Runtime, ConstU32<TARGET_BLOCK_RATE>>;

	parameter_types! {
		pub RuntimeBlockWeights: BlockWeights = BlockWeights::builder()
			.base_block(BlockExecutionWeight::get())
			.for_class(DispatchClass::all(), |weights| {
				weights.base_extrinsic = ExtrinsicBaseWeight::get();
			})
			.for_class(DispatchClass::Normal, |weights| {
				weights.max_total = Some(MaximumBlockWeight::get() * NORMAL_DISPATCH_RATIO);
			})
			.for_class(DispatchClass::Operational, |weights| {
				weights.max_total = Some(MaximumBlockWeight::get());
			})
			.avg_block_initialization(AVERAGE_ON_INITIALIZE_RATIO)
			.build_or_panic();
	}
}

#[frame_support::pallet(dev_mode)]
pub mod test_pallet {
	use super::*;
	use frame_support::{
		dispatch::DispatchClass,
		pallet_prelude::*,
		weights::{constants::WEIGHT_REF_TIME_PER_SECOND, WeightMeter},
	};
	use frame_system::pallet_prelude::*;

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config + crate::Config {}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// A heavy call with Normal dispatch class that consumes significant weight.
		#[pallet::weight((Weight::from_parts(WEIGHT_REF_TIME_PER_SECOND, 1024 * 1024), DispatchClass::Normal))]
		pub fn heavy_call_normal(_: OriginFor<T>) -> DispatchResult {
			Ok(())
		}

		/// A heavy call with Operational dispatch class that consumes significant weight.
		#[pallet::weight((Weight::from_parts(WEIGHT_REF_TIME_PER_SECOND, 1024 * 1024), DispatchClass::Operational))]
		pub fn heavy_call_operational(_: OriginFor<T>) -> DispatchResult {
			Ok(())
		}

		/// A heavy call with Operational dispatch class that consumes significant weight.
		#[pallet::weight((Weight::from_parts(WEIGHT_REF_TIME_PER_SECOND, 1024 * 1024), DispatchClass::Mandatory))]
		pub fn heavy_call_mandatory(_: OriginFor<T>) -> DispatchResult {
			Ok(())
		}

		#[pallet::weight((*_weight, DispatchClass::Normal))]
		pub fn use_weight(_: OriginFor<T>, _weight: Weight) -> DispatchResult {
			Ok(())
		}
	}

	#[pallet::inherent]
	impl<T: Config> ProvideInherent for Pallet<T> {
		type Call = Call<T>;
		type Error = sp_inherents::MakeFatalError<()>;
		const INHERENT_IDENTIFIER: InherentIdentifier = *b"testtest";

		fn create_inherent(_data: &InherentData) -> Option<Self::Call> {
			None
		}

		fn is_inherent(call: &Self::Call) -> bool {
			matches!(call, Call::heavy_call_mandatory {})
		}
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn on_idle(_block: BlockNumberFor<T>, limit: Weight) -> Weight {
			if let Some(max) = OnIdleMaxLeftWeight::get() {
				assert!(limit.all_lte(max));
			}

			Weight::zero()
		}

		fn on_poll(_n: BlockNumberFor<T>, weight: &mut WeightMeter) {
			if let Some(max) = OnPollMaxLeftWeight::get() {
				assert!(weight.remaining().all_lte(max));
			}
		}
	}

	#[allow(deprecated)]
	#[pallet::validate_unsigned]
	impl<T: Config> sp_runtime::traits::ValidateUnsigned for Pallet<T> {
		type Call = Call<T>;
		fn validate_unsigned(_source: TransactionSource, call: &Self::Call) -> TransactionValidity {
			Ok(ValidTransaction {
				priority: u64::max_value(),
				requires: Vec::new(),
				provides: vec![call.encode()],
				longevity: TransactionLongevity::max_value(),
				propagate: true,
			})
		}
	}
}

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
#[docify::export(pre_inherents_setup)]
impl frame_system::Config for Runtime {
	// Setup the block weight.
	type BlockWeights = max_block_weight_setup::RuntimeBlockWeights;
	// Set the `PreInherents` hook.
	type PreInherents = DynamicMaxBlockWeightHooks<Self, ConstU32<TARGET_BLOCK_RATE>>;

	// Just required to make it compile, but not that important for this example here.
	type Block = Block;
	type OnSetCode = crate::ParachainSetCode<Self>;
	type AccountId = u64;
	type Lookup = UintAuthorityId;
	// Rest of the types are omitted here.
}

impl crate::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type OnSystemEvent = ();
	type SelfParaId = ();
	type OutboundXcmpMessageSource = ();
	type DmpQueue = ();
	type ReservedDmpWeight = ();
	type XcmpMessageHandler = ();
	type ReservedXcmpWeight = ();
	type CheckAssociatedRelayNumber = crate::RelayNumberStrictlyIncreases;
	type WeightInfo = ();
	type ConsensusHook = crate::ExpectParentIncluded;
	type RelayParentOffset = ();
	type SchedulingSignatureVerifier = ();
}

impl test_pallet::Config for Runtime {}

construct_runtime!(
	pub enum Runtime {
		System: frame_system,
		ParachainSystem: parachain_system,
		TestPallet: test_pallet,
	}
);

parameter_types! {
	pub static MbmOngoing: bool = false;
	pub static OnIdleMaxLeftWeight: Option<Weight> = None;
	pub static OnPollMaxLeftWeight: Option<Weight> = None;
}

pub struct Migrator;

impl MultiStepMigrator for Migrator {
	fn ongoing() -> bool {
		MbmOngoing::get()
	}

	fn step() -> Weight {
		Weight::zero()
	}
}

pub mod only_operational_runtime {
	use super::{BlockOnlyOperational, Migrator};
	use crate::block_weight::DynamicMaxBlockWeightHooks;
	use frame_support::{construct_runtime, derive_impl};
	use sp_core::ConstU32;
	use sp_runtime::testing::UintAuthorityId;

	#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
	impl frame_system::Config for RuntimeOnlyOperational {
		type BlockWeights = super::max_block_weight_setup::RuntimeBlockWeights;
		type PreInherents =
			DynamicMaxBlockWeightHooks<Self, ConstU32<{ super::TARGET_BLOCK_RATE }>>;
		type Block = BlockOnlyOperational;
		type OnSetCode = crate::ParachainSetCode<Self>;
		type AccountId = u64;
		type Lookup = UintAuthorityId;
		type MultiBlockMigrator = Migrator;
	}

	impl crate::Config for RuntimeOnlyOperational {
		type RuntimeEvent = RuntimeEvent;
		type OnSystemEvent = ();
		type SelfParaId = ();
		type OutboundXcmpMessageSource = ();
		type DmpQueue = ();
		type ReservedDmpWeight = ();
		type XcmpMessageHandler = ();
		type ReservedXcmpWeight = ();
		type CheckAssociatedRelayNumber = crate::RelayNumberStrictlyIncreases;
		type WeightInfo = ();
		type ConsensusHook = crate::ExpectParentIncluded;
		type RelayParentOffset = ();
		type SchedulingSignatureVerifier = ();
	}

	impl super::test_pallet::Config for RuntimeOnlyOperational {}

	construct_runtime!(
		pub enum RuntimeOnlyOperational {
			System: frame_system,
			ParachainSystem: super::parachain_system,
			TestPallet: super::test_pallet,
		}
	);
}

pub use only_operational_runtime::{
	RuntimeCall as RuntimeCallOnlyOperational, RuntimeOnlyOperational,
};

/// Executive: handles dispatch to the various modules.
pub type Executive = frame_executive::Executive<
	Runtime,
	Block,
	frame_system::ChainContext<Runtime>,
	Runtime,
	AllPalletsWithSystem,
>;

/// Executive configured to only accept operational transaction to go over the limit.
pub type ExecutiveOnlyOperational = frame_executive::Executive<
	RuntimeOnlyOperational,
	BlockOnlyOperational,
	frame_system::ChainContext<RuntimeOnlyOperational>,
	RuntimeOnlyOperational,
	only_operational_runtime::AllPalletsWithSystem,
>;

/// Builder for test externalities
pub struct TestExtBuilder {
	num_cores: Option<u16>,
	bundle_index: Option<u8>,
	bundle_is_last: bool,
	previous_core_count: Option<u16>,
}

impl Default for TestExtBuilder {
	fn default() -> Self {
		sp_tracing::try_init_simple();

		Self {
			num_cores: None,
			bundle_index: None,
			bundle_is_last: false,
			previous_core_count: None,
		}
	}
}

impl TestExtBuilder {
	/// Create a new builder
	pub fn new() -> Self {
		Self::default()
	}

	/// Set the number of cores
	pub fn number_of_cores(mut self, num_cores: u16) -> Self {
		self.num_cores = Some(num_cores);
		self
	}

	/// Set the `PreviousCoreCount` storage value.
	pub fn previous_core_count(mut self, previous_core_count: u16) -> Self {
		self.previous_core_count = Some(previous_core_count);
		self
	}

	/// Set this as the first block in the core (bundle index = 0)
	pub fn first_block_in_core(mut self, is_first: bool) -> Self {
		if is_first {
			self.bundle_index = Some(0);
		} else if self.bundle_index.is_none() {
			// If not first and no bundle index set, default to index 1
			self.bundle_index = Some(1);
		}
		self
	}

	/// Build the test externalities
	pub fn build(self) -> sp_io::TestExternalities {
		let storage = frame_system::GenesisConfig::<Runtime>::default().build_storage().unwrap();
		let mut ext = sp_io::TestExternalities::from(storage);

		ext.execute_with(|| {
			// Add core info if specified
			if let Some(num_cores) = self.num_cores {
				let core_info = CoreInfo {
					selector: CoreSelector(0),
					claim_queue_offset: ClaimQueueOffset(0),
					number_of_cores: Compact(num_cores),
				};
				let digest = CumulusDigestItem::CoreInfo(core_info).to_digest_item();
				frame_system::Pallet::<Runtime>::deposit_log(digest);
			}

			// Add bundle info if specified
			if let Some(bundle_index) = self.bundle_index {
				let bundle_info =
					BlockBundleInfo { index: bundle_index, is_last: self.bundle_is_last };
				let digest = CumulusDigestItem::BlockBundleInfo(bundle_info).to_digest_item();
				frame_system::Pallet::<Runtime>::deposit_log(digest);
			}

			if let Some(previous_core_count) = self.previous_core_count {
				PreviousCoreCount::<Runtime>::put(Compact(previous_core_count));
			}
		});

		ext
	}
}

/// Helper to check if UseFullCore digest was deposited
pub fn has_use_full_core_digest() -> bool {
	let digest = frame_system::Pallet::<Runtime>::digest();
	CumulusDigestItem::contains_use_full_core(&digest)
}

/// Helper to register weight as consumed (simulating on_initialize)
pub fn register_weight(weight: Weight, class: DispatchClass) {
	frame_system::Pallet::<Runtime>::register_extra_weight_unchecked(weight, class);
}

/// Emulates what happes after `initialize_block` finished.
pub fn initialize_block_finished() {
	System::set_block_consumed_resources(Weight::zero(), 0);
	System::note_finished_initialize();
	<Runtime as frame_system::Config>::PreInherents::pre_inherents();
	System::note_inherents_applied();
}

/// Fakes the call to `set_validation_data`.
pub fn fake_set_validation_data() {
	crate::ValidationData::<Runtime>::put(PersistedValidationData::default());
	crate::HostConfiguration::<Runtime>::put(cumulus_primitives_core::AbridgedHostConfiguration {
		max_code_size: 2 * 1024 * 1024,
		max_head_data_size: 1024 * 1024,
		max_upward_queue_count: 8,
		max_upward_queue_size: 1024,
		max_upward_message_size: 256,
		max_upward_message_num_per_candidate: 5,
		hrmp_max_message_num_per_candidate: 5,
		validation_upgrade_cooldown: 6,
		validation_upgrade_delay: 6,
		async_backing_params: cumulus_primitives_core::relay_chain::AsyncBackingParams {
			allowed_ancestry_len: 0,
			max_candidate_depth: 0,
		},
	});
	crate::RelevantMessagingState::<Runtime>::put(MessagingStateSnapshot {
		dmq_mqc_head: Default::default(),
		relay_dispatch_queue_remaining_capacity: Default::default(),
		ingress_channels: Vec::new(),
		egress_channels: Vec::new(),
	});
}
