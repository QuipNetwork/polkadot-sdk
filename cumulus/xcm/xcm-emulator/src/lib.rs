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

extern crate alloc;

pub use array_bytes;
pub use codec::{Decode, Encode, EncodeLike, MaxEncodedLen};
pub use paste;
pub use std::{
	any::type_name,
	collections::HashMap,
	error::Error,
	fmt,
	marker::PhantomData,
	ops::Deref,
	sync::{Arc, LazyLock, Mutex},
};
pub use tracing;

pub use cumulus_primitives_core::relay_chain::Slot;
pub use sp_consensus_aura::AURA_ENGINE_ID;
pub use sp_runtime::DigestItem;
// Substrate
pub use alloc::collections::vec_deque::VecDeque;
pub use core::{cell::RefCell, fmt::Debug};
pub use cumulus_primitives_core::AggregateMessageOrigin as CumulusAggregateMessageOrigin;
pub use frame_support::{
	assert_ok,
	sp_runtime::{
		traits::{Convert, Dispatchable, Header as HeaderT, Zero},
		Digest, DispatchResult,
	},
	traits::{
		EnqueueMessage, ExecuteOverweightError, Get, Hooks, OnFinalize, OnIdle, OnInitialize,
		OriginTrait, ProcessMessage, ProcessMessageError, ServiceQueues,
	},
	weights::{Weight, WeightMeter},
};
pub use frame_system::{
	limits::BlockWeights as BlockWeightsLimits, pallet_prelude::BlockNumberFor,
	Config as SystemConfig, Pallet as SystemPallet,
};
pub use pallet_balances::AccountData;
pub use pallet_message_queue;
pub use pallet_timestamp::Call as TimestampCall;
pub use sp_arithmetic::traits::Bounded;
pub use sp_core::{
	crypto::get_public_from_string_or_panic, parameter_types, sr25519, storage::Storage, Pair,
};
pub use sp_crypto_hashing::blake2_256;
pub use sp_io::TestExternalities;
pub use sp_runtime::BoundedSlice;
pub use sp_tracing;

// Cumulus
pub use cumulus_pallet_parachain_system::{
	parachain_inherent::{deconstruct_parachain_inherent_data, InboundMessagesData},
	Call as ParachainSystemCall, Config as ParachainSystemConfig, Pallet as ParachainSystemPallet,
};
pub use cumulus_primitives_core::{
	relay_chain::{BlockNumber as RelayBlockNumber, HeadData, HrmpChannelId},
	AbridgedHrmpChannel, DmpMessageHandler, ParaId, PersistedValidationData, XcmpMessageHandler,
};
pub use cumulus_primitives_parachain_inherent::ParachainInherentData;
pub use cumulus_test_relay_sproof_builder::RelayStateSproofBuilder;
pub use pallet_aura;
pub use pallet_message_queue::{Config as MessageQueueConfig, Pallet as MessageQueuePallet};
pub use parachains_common::{AccountId, Balance};
pub use polkadot_primitives;
pub use polkadot_runtime_parachains::inclusion::{AggregateMessageOrigin, UmpQueueId};

// Polkadot
pub use polkadot_parachain_primitives::primitives::RelayChainBlockNumber;
use sp_core::{crypto::AccountId32, H256};
pub use xcm::latest::prelude::{
	AccountId32 as AccountId32Junction, Ancestor, Assets, Here, Location,
	Parachain as ParachainJunction, Parent, WeightLimit, XcmHash,
};
pub use xcm_executor::traits::ConvertLocation;
use xcm_simulator::helpers::TopicIdTracker;

pub type AccountIdOf<T> = <T as frame_system::Config>::AccountId;

/// Relay chain slot duration in milliseconds (6 seconds).
/// This is used to calculate timestamps and derive parachain slots from relay chain slots.
pub const RELAY_CHAIN_SLOT_DURATION_MILLIS: u64 = 6000;

thread_local! {
	/// Downward messages, each message is: `(to_para_id, [(relay_block_number, msg)])`
	#[allow(clippy::type_complexity)]
	pub static DOWNWARD_MESSAGES: RefCell<HashMap<String, VecDeque<(u32, Vec<(RelayBlockNumber, Vec<u8>)>)>>>
		= RefCell::new(HashMap::new());
	/// Downward messages that already processed by parachains, each message is: `(to_para_id, relay_block_number, Vec<u8>)`
	#[allow(clippy::type_complexity)]
	pub static DMP_DONE: RefCell<HashMap<String, VecDeque<(u32, RelayBlockNumber, Vec<u8>)>>>
		= RefCell::new(HashMap::new());
	/// Horizontal messages, each message is: `(to_para_id, [(from_para_id, relay_block_number, msg)])`
	#[allow(clippy::type_complexity)]
	pub static HORIZONTAL_MESSAGES: RefCell<HashMap<String, VecDeque<(u32, Vec<(ParaId, RelayBlockNumber, Vec<u8>)>)>>>
		= RefCell::new(HashMap::new());
	/// Upward messages, each message is: `(from_para_id, msg)`
	pub static UPWARD_MESSAGES: RefCell<HashMap<String, VecDeque<(u32, Vec<u8>)>>> = RefCell::new(HashMap::new());
	/// Bridged messages, each message is: `BridgeMessage`
	pub static BRIDGED_MESSAGES: RefCell<HashMap<String, VecDeque<BridgeMessage>>> = RefCell::new(HashMap::new());
	/// Parachains Ids a the Network
	pub static PARA_IDS: RefCell<HashMap<String, Vec<u32>>> = RefCell::new(HashMap::new());
	/// Flag indicating if global variables have been initialized for a certain Network
	pub static INITIALIZED: RefCell<HashMap<String, bool>> = RefCell::new(HashMap::new());
	/// Most recent `HeadData` of each parachain, encoded.
	pub static LAST_HEAD: RefCell<HashMap<String, HashMap<u32, HeadData>>> = RefCell::new(HashMap::new());
}
pub trait CheckAssertion<Origin, Destination, Hops, Args>
where
	Origin: Chain + Clone,
	Destination: Chain + Clone,
	Origin::RuntimeOrigin: OriginTrait<AccountId = AccountIdOf<Origin::Runtime>> + Clone,
	Destination::RuntimeOrigin: OriginTrait<AccountId = AccountIdOf<Destination::Runtime>> + Clone,
	Hops: Clone,
	Args: Clone,
{
	fn check_assertion(test: Test<Origin, Destination, Hops, Args>);
}

#[impl_trait_for_tuples::impl_for_tuples(5)]
impl<Origin, Destination, Hops, Args> CheckAssertion<Origin, Destination, Hops, Args> for Tuple
where
	Origin: Chain + Clone,
	Destination: Chain + Clone,
	Origin::RuntimeOrigin: OriginTrait<AccountId = AccountIdOf<Origin::Runtime>> + Clone,
	Destination::RuntimeOrigin: OriginTrait<AccountId = AccountIdOf<Destination::Runtime>> + Clone,
	Hops: Clone,
	Args: Clone,
{
	fn check_assertion(test: Test<Origin, Destination, Hops, Args>) {
		for_tuples!( #(
			Tuple::check_assertion(test.clone());
		)* );
	}
}

// Implement optional inherent code to be executed
// This will be executed after on-initialize and before on-finalize
pub trait AdditionalInherentCode {
	fn on_new_block() -> DispatchResult {
		Ok(())
	}
}

impl AdditionalInherentCode for () {}

/// Customizes block production inside the emulator.
///
/// The default impl ([`AuraBlockProducer`]) covers Aura-based parachains.
/// Parachains using a different consensus (e.g. Nimbus) can supply a custom
/// implementation via the `BlockProducer:` field of `decl_test_parachains!`.
pub trait BlockProducer {
	/// Slot duration in milliseconds, used to compute the relay-to-para block
	/// ratio when advancing the network.
	fn slot_duration() -> u64;

	/// Pre-runtime digest prepended when initialising a new parachain block.
	fn pre_runtime_digest(relay_block_number: u32) -> Digest;
}

/// Default `BlockProducer` implementation for Aura-based parachains.
pub struct AuraBlockProducer<T>(PhantomData<T>);

impl<T> BlockProducer for AuraBlockProducer<T>
where
	T: pallet_aura::Config,
	u64: From<<T as pallet_timestamp::Config>::Moment>,
{
	fn slot_duration() -> u64 {
		pallet_aura::Pallet::<T>::slot_duration().into()
	}

	fn pre_runtime_digest(relay_block_number: u32) -> Digest {
		let slot_duration = Self::slot_duration();
		let aura_slot: Slot =
			(relay_block_number as u64 * RELAY_CHAIN_SLOT_DURATION_MILLIS / slot_duration).into();
		let mut digest = Digest::default();
		digest
			.logs
			.push(DigestItem::PreRuntime(AURA_ENGINE_ID, Encode::encode(&aura_slot)));
		digest
	}
}

pub trait TestExt {
	fn build_new_ext(storage: Storage) -> TestExternalities;
	fn new_ext() -> TestExternalities;
	fn move_ext_out(id: &'static str);
	fn move_ext_in(id: &'static str);
	fn reset_ext();
	fn execute_with<R>(execute: impl FnOnce() -> R) -> R;
	fn ext_wrapper<R>(func: impl FnOnce() -> R) -> R;
}

impl TestExt for () {
	fn build_new_ext(_storage: Storage) -> TestExternalities {
		TestExternalities::default()
	}
	fn new_ext() -> TestExternalities {
		TestExternalities::default()
	}
	fn move_ext_out(_id: &'static str) {}
	fn move_ext_in(_id: &'static str) {}
	fn reset_ext() {}
	fn execute_with<R>(execute: impl FnOnce() -> R) -> R {
		execute()
	}
	fn ext_wrapper<R>(func: impl FnOnce() -> R) -> R {
		func()
	}
}

pub trait Network {
	type Relay: RelayChain;
	type Bridge: Bridge;

	fn name() -> &'static str;
	fn init();
	fn reset();
	fn para_ids() -> Vec<u32>;
	fn relay_block_number() -> u32;
	fn set_relay_block_number(number: u32);
	fn process_messages();
	fn has_unprocessed_messages() -> bool;
	fn process_downward_messages();
	fn process_horizontal_messages();
	fn process_upward_messages();
	fn process_bridged_messages();
	fn hrmp_channel_parachain_inherent_data(
		para_id: u32,
		relay_parent_number: u32,
		parent_head_data: HeadData,
		relay_parent_offset: u64,
	) -> ParachainInherentData;
	fn send_horizontal_messages<I: Iterator<Item = (ParaId, RelayBlockNumber, Vec<u8>)>>(
		to_para_id: u32,
		iter: I,
	) {
		HORIZONTAL_MESSAGES.with(|b| {
			b.borrow_mut()
				.get_mut(Self::name())
				.unwrap()
				.push_back((to_para_id, iter.collect()))
		});
	}

	fn send_upward_message(from_para_id: u32, msg: Vec<u8>) {
		UPWARD_MESSAGES
			.with(|b| b.borrow_mut().get_mut(Self::name()).unwrap().push_back((from_para_id, msg)));
	}

	fn send_downward_messages(
		to_para_id: u32,
		iter: impl Iterator<Item = (RelayBlockNumber, Vec<u8>)>,
	) {
		DOWNWARD_MESSAGES.with(|b| {
			b.borrow_mut()
				.get_mut(Self::name())
				.unwrap()
				.push_back((to_para_id, iter.collect()))
		});
	}

	fn send_bridged_messages(msg: BridgeMessage) {
		BRIDGED_MESSAGES.with(|b| b.borrow_mut().get_mut(Self::name()).unwrap().push_back(msg));
	}
}

pub trait Chain: TestExt {
	type Network: Network;
	type Runtime: SystemConfig;
	type RuntimeCall: Clone + Dispatchable<RuntimeOrigin = Self::RuntimeOrigin>;
	type RuntimeOrigin;
	type RuntimeEvent;
	type System;
	type OriginCaller;

	fn account_id_of(seed: &str) -> AccountId {
		get_public_from_string_or_panic::<sr25519::Public>(seed).into()
	}

	fn account_data_of(account: AccountIdOf<Self::Runtime>) -> AccountData<Balance>;

	fn events() -> Vec<<Self as Chain>::RuntimeEvent>;

	/// Whether the local Total Issuance can be treated as authoritative.
	fn native_total_issuance_source_of_truth() -> bool;
}

pub trait RelayChain: Chain {
	type SovereignAccountOf: ConvertLocation<AccountIdOf<Self::Runtime>>;
	type MessageProcessor: ProcessMessage<Origin = ParaId> + ServiceQueues;

	fn init();

	fn child_location_of(id: ParaId) -> Location {
		(Ancestor(0), ParachainJunction(id.into())).into()
	}

	fn sovereign_account_id_of(location: Location) -> AccountIdOf<Self::Runtime> {
		Self::SovereignAccountOf::convert_location(&location).unwrap()
	}

	fn sovereign_account_id_of_child_para(id: ParaId) -> AccountIdOf<Self::Runtime> {
		Self::sovereign_account_id_of(Self::child_location_of(id))
	}
}

pub trait Parachain: Chain {
	type XcmpMessageHandler: XcmpMessageHandler;
	type LocationToAccountId: ConvertLocation<AccountIdOf<Self::Runtime>>;
	type ParachainInfo: Get<ParaId>;
	type ParachainSystem;
	type MessageProcessor: ProcessMessage + ServiceQueues;
	type AdditionalInherentCode: AdditionalInherentCode;
	type BlockProducer: BlockProducer;

	fn init();

	fn new_block();

	fn finalize_block();

	fn set_last_head();

	fn para_id() -> ParaId {
		Self::ext_wrapper(|| Self::ParachainInfo::get())
	}

	fn parent_location() -> Location {
		(Parent).into()
	}

	fn sibling_location_of(para_id: ParaId) -> Location {
		(Parent, ParachainJunction(para_id.into())).into()
	}

	fn sovereign_account_id_of(location: Location) -> AccountIdOf<Self::Runtime> {
		Self::LocationToAccountId::convert_location(&location).unwrap()
	}
}

pub trait Bridge {
	type Source: TestExt;
	type Target: TestExt;
	type Handler: BridgeMessageHandler;

	fn init();
}

impl Bridge for () {
	type Source = ();
	type Target = ();
	type Handler = ();

	fn init() {}
}

pub type BridgeLaneId = Vec<u8>;

#[derive(Clone, Default, Debug)]
pub struct BridgeMessage {
	pub lane_id: BridgeLaneId,
	pub nonce: u64,
	pub payload: Vec<u8>,
}

pub trait BridgeMessageHandler {
	fn get_source_outbound_messages() -> Vec<BridgeMessage>;

	fn dispatch_target_inbound_message(
		message: BridgeMessage,
	) -> Result<(), BridgeMessageDispatchError>;

	fn notify_source_message_delivery(lane_id: BridgeLaneId);
}

impl BridgeMessageHandler for () {
	fn get_source_outbound_messages() -> Vec<BridgeMessage> {
		Default::default()
	}

	fn dispatch_target_inbound_message(
		_message: BridgeMessage,
	) -> Result<(), BridgeMessageDispatchError> {
		Err(BridgeMessageDispatchError(Box::new("Not a bridge")))
	}

	fn notify_source_message_delivery(_lane_id: BridgeLaneId) {}
}

#[derive(Debug)]
pub struct BridgeMessageDispatchError(pub Box<dyn Debug>);

impl Error for BridgeMessageDispatchError {}

impl fmt::Display for BridgeMessageDispatchError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "{:?}", self.0)
	}
}

// Relay Chain Implementation
#[macro_export]
macro_rules! decl_test_relay_chains {
	(
		$(
			#[api_version($api_version:tt)]
			pub struct $name:ident {
				genesis = $genesis:expr,
				on_init = $on_init:expr,
				runtime = $runtime:ident,
				core = {
					SovereignAccountOf: $sovereign_acc_of:path,
				},
				pallets = {
					$($pallet_name:ident: $pallet_path:path,)*
				}
			}
		),
		+
		$(,)?
	) => {
		$(
			#[derive(Clone)]
			pub struct $name<N>($crate::PhantomData<N>);

			impl<N: $crate::Network> $crate::Chain for $name<N> {
				type Network = N;
				type Runtime = $runtime::Runtime;
				type RuntimeCall = $runtime::RuntimeCall;
				type RuntimeOrigin = $runtime::RuntimeOrigin;
				type RuntimeEvent = $runtime::RuntimeEvent;
				type System = $crate::SystemPallet::<Self::Runtime>;
				type OriginCaller = $runtime::OriginCaller;

				fn account_data_of(account: $crate::AccountIdOf<Self::Runtime>) -> $crate::AccountData<$crate::Balance> {
					<Self as $crate::TestExt>::ext_wrapper(|| $crate::SystemPallet::<Self::Runtime>::account(account).data.into())
				}

				fn events() -> Vec<<Self as $crate::Chain>::RuntimeEvent> {
					Self::System::events()
						.iter()
						.map(|record| record.event.clone())
						.collect()
				}

				fn native_total_issuance_source_of_truth() -> bool {
					false
				}
			}

			impl<N: $crate::Network> $crate::RelayChain for $name<N> {
				type SovereignAccountOf = $sovereign_acc_of;
				type MessageProcessor = $crate::DefaultRelayMessageProcessor<$name<N>>;

				fn init() {
					use $crate::TestExt;
					// Initialize the thread local variable
					$crate::paste::paste! {
						[<LOCAL_EXT_ $name:upper>].with(|v| *v.borrow_mut() = Self::build_new_ext($genesis));
					}
				}
			}

			$crate::paste::paste! {
				pub trait [<$name RelayPallet>] {
					$(
						type $pallet_name;
					)?
				}

				impl<N: $crate::Network> [<$name RelayPallet>] for $name<N> {
					$(
						type $pallet_name = $pallet_path;
					)?
				}
			}

			$crate::__impl_test_ext_for_relay_chain!($name, N, $genesis, $on_init, $api_version);
			$crate::__impl_check_assertion!($name, N);
		)+
	};
}

#[macro_export]
macro_rules! __impl_test_ext_for_relay_chain {
	// entry point: generate ext name
	($name:ident, $network:ident, $genesis:expr, $on_init:expr, $api_version:tt) => {
		$crate::paste::paste! {
			$crate::__impl_test_ext_for_relay_chain!(
				@impl $name,
				$network,
				$genesis,
				$on_init,
				[<ParachainHostV $api_version>],
				[<LOCAL_EXT_ $name:upper>],
				[<GLOBAL_EXT_ $name:upper>]
			);
		}
	};
	// impl
	(@impl $name:ident, $network:ident, $genesis:expr, $on_init:expr, $api_version:ident, $local_ext:ident, $global_ext:ident) => {
		thread_local! {
			pub static $local_ext: $crate::RefCell<$crate::TestExternalities>
				= $crate::RefCell::new($crate::TestExternalities::new($genesis));
		}

		pub static $global_ext: $crate::LazyLock<$crate::Mutex<$crate::RefCell<$crate::HashMap<String, $crate::TestExternalities>>>>
			= $crate::LazyLock::new(|| $crate::Mutex::new($crate::RefCell::new($crate::HashMap::new())));

		impl<$network: $crate::Network> $crate::TestExt for $name<$network> {
			fn build_new_ext(storage: $crate::Storage) -> $crate::TestExternalities {
				use $crate::{sp_tracing, Network, Chain, TestExternalities};

				let mut ext = TestExternalities::new(storage);

				ext.execute_with(|| {
					#[allow(clippy::no_effect)]
					$on_init;
					sp_tracing::try_init_simple();

					let mut block_number = <Self as Chain>::System::block_number();
					block_number = std::cmp::max(1, block_number);
					<Self as Chain>::System::set_block_number(block_number);
				});
				ext
			}

			fn new_ext() -> $crate::TestExternalities {
				Self::build_new_ext($genesis)
			}

			fn move_ext_out(id: &'static str) {
				use $crate::Deref;

				// Take TestExternality from thread_local
				let local_ext = $local_ext.with(|v| {
					v.take()
				});

				// Get TestExternality from LazyLock
				let global_ext_guard = $global_ext.lock().unwrap();

				// Replace TestExternality in LazyLock by TestExternality from thread_local
				global_ext_guard.deref().borrow_mut().insert(id.to_string(), local_ext);
			}

			fn move_ext_in(id: &'static str) {
				use $crate::Deref;

				let mut global_ext_unlocked = false;

				// Keep the mutex unlocked until TesExternality from LazyLock
				// has been updated
				while !global_ext_unlocked {
					// Get TesExternality from LazyLock
					let global_ext_result = $global_ext.try_lock();

					if let Ok(global_ext_guard) = global_ext_result {
						// Unlock the mutex as long as the condition is not met
						if !global_ext_guard.deref().borrow().contains_key(id) {
							drop(global_ext_guard);
						} else {
							global_ext_unlocked = true;
						}
					}
				}

				// Now that we know that TestExt has been updated, we lock its mutex
				let mut global_ext_guard = $global_ext.lock().unwrap();

				// and set TesExternality from LazyLock into TesExternality for local_thread
				let global_ext = global_ext_guard.deref();

				$local_ext.with(|v| {
					v.replace(global_ext.take().remove(id).unwrap());
				});
			}

			fn reset_ext() {
				$local_ext.with(|v| *v.borrow_mut() = Self::build_new_ext($genesis));
			}

			fn execute_with<R>(execute: impl FnOnce() -> R) -> R {
				use $crate::{Chain, Network};
				// Make sure the Network is initialized
				<$network>::init();

				// Execute
				let r = $local_ext.with(|v| {
					$crate::tracing::info!(target: "xcm::emulator::execute_with", "Executing as {}", stringify!($name));
					v.borrow_mut().execute_with(execute)
				});

				// Send messages if needed
				$local_ext.with(|v| {
					v.borrow_mut().execute_with(|| {
						use $crate::polkadot_primitives::runtime_api::runtime_decl_for_parachain_host::$api_version;

						//TODO: mark sent count & filter out sent msg
						for para_id in <$network>::para_ids() {
							// downward messages
							let downward_messages = <Self as $crate::Chain>::Runtime::dmq_contents(para_id.into())
								.into_iter()
								.map(|inbound| (inbound.sent_at, inbound.msg));
							if downward_messages.len() == 0 {
								continue;
							}
							<$network>::send_downward_messages(para_id, downward_messages.into_iter());

							// Note: no need to handle horizontal messages, as the
							// simulator directly sends them to dest (not relayed).
						}

						// log events
						Self::events().iter().for_each(|event| {
							$crate::tracing::info!(target: concat!("events::", stringify!($name)), ?event, "Event emitted");
						});

						// clean events
						<Self as Chain>::System::reset_events();
					})
				});

				<$network>::process_messages();

				r
			}

			fn ext_wrapper<R>(func: impl FnOnce() -> R) -> R {
				$local_ext.with(|v| {
					v.borrow_mut().execute_with(|| {
						func()
					})
				})
			}
		}
	};
}

// Parachain Implementation
#[macro_export]
macro_rules! decl_test_parachains {
	(
		$(
			pub struct $name:ident {
				genesis = $genesis:expr,
				on_init = $on_init:expr,
				runtime = $runtime:ident,
				core = {
					XcmpMessageHandler: $xcmp_message_handler:path,
					LocationToAccountId: $location_to_account:path,
					ParachainInfo: $parachain_info:path,
					MessageOrigin: $message_origin:path,
					$( AdditionalInherentCode: $additional_inherent_code:ty,)?
					$( native_total_supply_tracker: $total_supply_tracker:expr,)?
					$( BlockProducer: $block_producer:ty,)?
				},
				pallets = {
					$($pallet_name:ident: $pallet_path:path,)*
				}
			}
		),
		+
		$(,)?
	) => {
		$(
			#[derive(Clone)]
			pub struct $name<N>($crate::PhantomData<N>);

			impl<N: $crate::Network> $crate::Chain for $name<N> {
				type Runtime = $runtime::Runtime;
				type RuntimeCall = $runtime::RuntimeCall;
				type RuntimeOrigin = $runtime::RuntimeOrigin;
				type RuntimeEvent = $runtime::RuntimeEvent;
				type System = $crate::SystemPallet::<Self::Runtime>;
				type OriginCaller = $runtime::OriginCaller;
				type Network = N;

				fn account_data_of(account: $crate::AccountIdOf<Self::Runtime>) -> $crate::AccountData<$crate::Balance> {
					<Self as $crate::TestExt>::ext_wrapper(|| $crate::SystemPallet::<Self::Runtime>::account(account).data.into())
				}

				fn events() -> Vec<<Self as $crate::Chain>::RuntimeEvent> {
					Self::System::events()
						.iter()
						.map(|record| record.event.clone())
						.collect()
				}

				fn native_total_issuance_source_of_truth() -> bool {
					$crate::decl_test_parachains!(@inner_total_supply_tracker $($total_supply_tracker)?)
				}
			}

			impl<N: $crate::Network> $crate::Parachain for $name<N> {
				type XcmpMessageHandler = $xcmp_message_handler;
				type LocationToAccountId = $location_to_account;
				type ParachainSystem = $crate::ParachainSystemPallet<<Self as $crate::Chain>::Runtime>;
				type ParachainInfo = $parachain_info;
				type MessageProcessor = $crate::DefaultParaMessageProcessor<$name<N>, $message_origin>;
				$crate::decl_test_parachains!(@inner_additional_inherent_code $($additional_inherent_code)?);
				$crate::decl_test_parachains!(@inner_block_producer $runtime, $($block_producer)?);

				// We run an empty block during initialisation to open HRMP channels
				// and have them ready for the next block
				fn init() {
					use $crate::{Chain, TestExt};

					// Initialize the thread local variable
					$crate::paste::paste! {
						[<LOCAL_EXT_ $name:upper>].with(|v| *v.borrow_mut() = Self::build_new_ext($genesis));
					}
					// Set the last block head for later use in the next block
					Self::set_last_head();
					// Initialize a new block
					Self::new_block();
					// Finalize the new block
					Self::finalize_block();
				}

				fn new_block() {
					use $crate::{
						Dispatchable, Chain, TestExt, Zero, AdditionalInherentCode, BlockProducer,
						Parachain, RELAY_CHAIN_SLOT_DURATION_MILLIS
					};

					let para_id = Self::para_id().into();

					Self::ext_wrapper(|| {
						let slot_duration =
							<<Self as Parachain>::BlockProducer as BlockProducer>::slot_duration();

						let mut relay_block_number = N::relay_block_number();
						relay_block_number += 1;
						N::set_relay_block_number(relay_block_number);

						// Initialize a new Parachain block
						let mut block_number = <Self as Chain>::System::block_number();
						block_number += 1;
						let parent_head_data = $crate::LAST_HEAD.with(|b| b.borrow_mut()
							.get_mut(N::name())
							.expect("network not initialized?")
							.get(&para_id)
							.expect("network not initialized?")
							.clone()
						);

						// Pre-runtime digest comes from the configured `BlockProducer`
						// (Aura by default; Nimbus and friends can plug in).
						let digest = <<Self as Parachain>::BlockProducer as BlockProducer>::pre_runtime_digest(relay_block_number);
						<Self as Chain>::System::initialize(&block_number, &parent_head_data.hash(), &digest);

						// Process `on_initialize` for all pallets except `System`.
					// This must run BEFORE timestamp::set because pallet_aura's OnTimestampSet implementation
					// checks that the timestamp slot matches CurrentSlot, and CurrentSlot is updated in on_initialize.
					let _ = $runtime::AllPalletsWithoutSystem::on_initialize(block_number);

					// Process parachain inherents:

					// 1. inherent: pallet_timestamp::Call::set (we expect the parachain has `pallet_timestamp`)
					let timestamp_set: <Self as Chain>::RuntimeCall = $crate::TimestampCall::set {
						now: relay_block_number as u64 * RELAY_CHAIN_SLOT_DURATION_MILLIS,
					}.into();
					$crate::assert_ok!(
						timestamp_set.dispatch(<Self as Chain>::RuntimeOrigin::none())
					);

					// Get RelayParentOffset from the runtime
					let relay_parent_offset = <<<Self as $crate::Chain>::Runtime as $crate::ParachainSystemConfig>::RelayParentOffset as $crate::Get<u32>>::get();

					// 2. inherent: cumulus_pallet_parachain_system::Call::set_validation_data
						let data = N::hrmp_channel_parachain_inherent_data(para_id, relay_block_number, parent_head_data, relay_parent_offset as u64);
						let (data, mut downward_messages, mut horizontal_messages) =
							$crate::deconstruct_parachain_inherent_data(data);
						let inbound_messages_data = $crate::InboundMessagesData::new(
							downward_messages.into_abridged(&mut usize::MAX.clone()),
							horizontal_messages.into_abridged(&mut usize::MAX.clone()),
						);
						let set_validation_data: <Self as Chain>::RuntimeCall = $crate::ParachainSystemCall::set_validation_data {
							data,
							inbound_messages_data
						}.into();
						$crate::assert_ok!(
							set_validation_data.dispatch(<Self as Chain>::RuntimeOrigin::none())
						);

						$crate::assert_ok!(
							<Self as Parachain>::AdditionalInherentCode::on_new_block()
						);
					});
				}

				fn finalize_block() {
					use $crate::{BlockWeightsLimits, Chain, OnFinalize, OnIdle, SystemConfig, TestExt, Weight};

					Self::ext_wrapper(|| {
						let block_number = <Self as Chain>::System::block_number();

						// Process `on_idle` for all pallets.
						let weight = <Self as Chain>::System::block_weight();
						let max_weight: Weight = <<<Self as Chain>::Runtime as SystemConfig>::BlockWeights as frame_support::traits::Get<BlockWeightsLimits>>::get().max_block;
						let remaining_weight = max_weight.saturating_sub(weight.total());
						if remaining_weight.all_gt(Weight::zero()) {
							let _ = $runtime::AllPalletsWithSystem::on_idle(block_number, remaining_weight);
						}

						// Process `on_finalize` for all pallets except `System`.
						$runtime::AllPalletsWithoutSystem::on_finalize(block_number);
					});

					Self::set_last_head();
				}


				fn set_last_head() {
					use $crate::{Chain, Encode, HeadData, TestExt};

					let para_id = Self::para_id().into();

					Self::ext_wrapper(|| {
						// Store parent head data for use later.
						let created_header = <Self as Chain>::System::finalize();
						$crate::LAST_HEAD.with(|b| b.borrow_mut()
							.get_mut(N::name())
							.expect("network not initialized?")
							.insert(para_id, HeadData(created_header.encode()))
						);
					});
				}
			}

			$crate::paste::paste! {
				pub trait [<$name ParaPallet>] {
					$(
						type $pallet_name;
					)*
				}

				impl<N: $crate::Network> [<$name ParaPallet>] for $name<N> {
					$(
						type $pallet_name = $pallet_path;
					)*
				}
			}

			$crate::__impl_test_ext_for_parachain!($name, N, $genesis, $on_init);
			$crate::__impl_check_assertion!($name, N);
		)+
	};
	( @inner_additional_inherent_code $additional_inherent_code:ty ) => { type AdditionalInherentCode = $additional_inherent_code; };
	( @inner_additional_inherent_code /* none */ ) => { type AdditionalInherentCode = (); };
	( @inner_total_supply_tracker $total_supply_tracker:expr ) => { $total_supply_tracker };
	( @inner_total_supply_tracker /* none */ ) => { false };
	( @inner_block_producer $runtime:ident, $block_producer:ty ) => { type BlockProducer = $block_producer; };
	( @inner_block_producer $runtime:ident, /* none */ ) => { type BlockProducer = $crate::AuraBlockProducer<$runtime::Runtime>; };
}

#[macro_export]
macro_rules! __impl_test_ext_for_parachain {
	// entry point: generate ext name
	($name:ident, $network:ident, $genesis:expr, $on_init:expr) => {
		$crate::paste::paste! {
			$crate::__impl_test_ext_for_parachain!(@impl $name, $network, $genesis, $on_init, [<LOCAL_EXT_ $name:upper>], [<GLOBAL_EXT_ $name:upper>]);
		}
	};
	// impl
	(@impl $name:ident, $network:ident, $genesis:expr, $on_init:expr, $local_ext:ident, $global_ext:ident) => {
		thread_local! {
			pub static $local_ext: $crate::RefCell<$crate::TestExternalities>
				= $crate::RefCell::new($crate::TestExternalities::new($genesis));
		}

		pub static $global_ext: $crate::LazyLock<$crate::Mutex<$crate::RefCell<$crate::HashMap<String, $crate::TestExternalities>>>>
			= $crate::LazyLock::new(|| $crate::Mutex::new($crate::RefCell::new($crate::HashMap::new())));

		impl<$network: $crate::Network> $crate::TestExt for $name<$network> {
			fn build_new_ext(storage: $crate::Storage) -> $crate::TestExternalities {
				let mut ext = $crate::TestExternalities::new(storage);

				ext.execute_with(|| {
					#[allow(clippy::no_effect)]
					$on_init;
					$crate::sp_tracing::try_init_simple();

					let mut block_number = <Self as $crate::Chain>::System::block_number();
					block_number = std::cmp::max(1, block_number);
					<Self as $crate::Chain>::System::set_block_number(block_number);
				});
				ext
			}

			fn new_ext() -> $crate::TestExternalities {
				Self::build_new_ext($genesis)
			}

			fn move_ext_out(id: &'static str) {
				use $crate::Deref;

				// Take TestExternality from thread_local
				let local_ext = $local_ext.with(|v| {
					v.take()
				});

				// Get TestExternality from LazyLock
				let global_ext_guard = $global_ext.lock().unwrap();

				// Replace TestExternality in LazyLock by TestExternality from thread_local
				global_ext_guard.deref().borrow_mut().insert(id.to_string(), local_ext);
			}

			fn move_ext_in(id: &'static str) {
				use $crate::Deref;

				let mut global_ext_unlocked = false;

				// Keep the mutex unlocked until TesExternality from LazyLock
				// has been updated
				while !global_ext_unlocked {
					// Get TesExternality from LazyLock
					let global_ext_result = $global_ext.try_lock();

					if let Ok(global_ext_guard) = global_ext_result {
						// Unlock the mutex as long as the condition is not met
						if !global_ext_guard.deref().borrow().contains_key(id) {
							drop(global_ext_guard);
						} else {
							global_ext_unlocked = true;
						}
					}
				}

				// Now that we know that TestExt has been updated, we lock its mutex
				let mut global_ext_guard = $global_ext.lock().unwrap();

				// and set TesExternality from LazyLock into TesExternality for local_thread
				let global_ext = global_ext_guard.deref();

				$local_ext.with(|v| {
					v.replace(global_ext.take().remove(id).unwrap());
				});
			}

			fn reset_ext() {
				$local_ext.with(|v| *v.borrow_mut() = Self::build_new_ext($genesis));
			}

			fn execute_with<R>(execute: impl FnOnce() -> R) -> R {
				use $crate::{Chain, Get, Hooks, Network, Parachain, Encode};

				// Make sure the Network is initialized
				<$network>::init();

				// Initialize a new block
				Self::new_block();

				// Execute
				let r = $local_ext.with(|v| {
					$crate::tracing::info!(target: "xcm::emulator::execute_with", "Executing as {}", stringify!($name));
					v.borrow_mut().execute_with(execute)
				});

				// Finalize the block
				Self::finalize_block();

				let para_id = Self::para_id().into();

				// Send messages if needed
				$local_ext.with(|v| {
					v.borrow_mut().execute_with(|| {
						let mock_header = $crate::HeaderT::new(
							0,
							Default::default(),
							Default::default(),
							Default::default(),
							Default::default(),
						);

						let collation_info = <Self as Parachain>::ParachainSystem::collect_collation_info(&mock_header);

						// send upward messages
						let relay_block_number = <$network>::relay_block_number();
						for msg in collation_info.upward_messages.clone() {
							<$network>::send_upward_message(para_id, msg);
						}

						// send horizontal messages
						for msg in collation_info.horizontal_messages {
							<$network>::send_horizontal_messages(
								msg.recipient.into(),
								vec![(para_id.into(), relay_block_number, msg.data)].into_iter(),
							);
						}

						// get bridge messages
						type NetworkBridge<$network> = <$network as $crate::Network>::Bridge;

						let bridge_messages = <<NetworkBridge<$network> as $crate::Bridge>::Handler as $crate::BridgeMessageHandler>::get_source_outbound_messages();

						// send bridged messages
						for msg in bridge_messages {
							<$network>::send_bridged_messages(msg);
						}

						// log events
						<Self as $crate::Chain>::events().iter().for_each(|event| {
							$crate::tracing::info!(target: concat!("events::", stringify!($name)), ?event, "Event emitted");
						});

						// clean events
						<Self as $crate::Chain>::System::reset_events();
					})
				});

				// provide inbound DMP/HRMP messages through a side-channel.
				// normally this would come through the `set_validation_data`,
				// but we go around that.
				<$network>::process_messages();

				r
			}

			fn ext_wrapper<R>(func: impl FnOnce() -> R) -> R {
				$local_ext.with(|v| {
					v.borrow_mut().execute_with(|| {
						func()
					})
				})
			}
		}
	};
}

// Network Implementation
#[macro_export]
macro_rules! decl_test_networks {
	(
		$(
			pub struct $name:ident {
				relay_chain = $relay_chain:ident,
				parachains = vec![ $( $parachain:ident, )* ],
				bridge = $bridge:ty
			}
		),
		+
		$(,)?
	) => {
		$(
			#[derive(Clone)]
			pub struct $name;

			impl $crate::Network for $name {
				type Relay = $relay_chain<Self>;
				type Bridge = $bridge;

				fn name() -> &'static str {
					$crate::type_name::<Self>()
				}

				fn reset() {
					use $crate::{TestExt};

					$crate::INITIALIZED.with(|b| b.borrow_mut().remove(Self::name()));
					$crate::DOWNWARD_MESSAGES.with(|b| b.borrow_mut().remove(Self::name()));
					$crate::DMP_DONE.with(|b| b.borrow_mut().remove(Self::name()));
					$crate::UPWARD_MESSAGES.with(|b| b.borrow_mut().remove(Self::name()));
					$crate::HORIZONTAL_MESSAGES.with(|b| b.borrow_mut().remove(Self::name()));
					$crate::BRIDGED_MESSAGES.with(|b| b.borrow_mut().remove(Self::name()));
					$crate::LAST_HEAD.with(|b| b.borrow_mut().remove(Self::name()));

					<$relay_chain<Self>>::reset_ext();
					$( <$parachain<Self>>::reset_ext(); )*
				}

				fn init() {
					// If Network has not been initialized yet, it gets initialized
					if $crate::INITIALIZED.with(|b| b.borrow_mut().get(Self::name()).is_none()) {
						$crate::INITIALIZED.with(|b| b.borrow_mut().insert(Self::name().to_string(), true));
						$crate::DOWNWARD_MESSAGES.with(|b| b.borrow_mut().insert(Self::name().to_string(), $crate::VecDeque::new()));
						$crate::DMP_DONE.with(|b| b.borrow_mut().insert(Self::name().to_string(), $crate::VecDeque::new()));
						$crate::UPWARD_MESSAGES.with(|b| b.borrow_mut().insert(Self::name().to_string(), $crate::VecDeque::new()));
						$crate::HORIZONTAL_MESSAGES.with(|b| b.borrow_mut().insert(Self::name().to_string(), $crate::VecDeque::new()));
						$crate::BRIDGED_MESSAGES.with(|b| b.borrow_mut().insert(Self::name().to_string(), $crate::VecDeque::new()));
						$crate::PARA_IDS.with(|b| b.borrow_mut().insert(Self::name().to_string(), Self::para_ids()));
						$crate::LAST_HEAD.with(|b| b.borrow_mut().insert(Self::name().to_string(), $crate::HashMap::new()));

						<$relay_chain<Self> as $crate::RelayChain>::init();
						$( <$parachain<Self> as $crate::Parachain>::init(); )*
					}
				}

				fn para_ids() -> Vec<u32> {
					vec![$(
						<$parachain<Self> as $crate::Parachain>::para_id().into(),
					)*]
				}

				fn relay_block_number() -> u32 {
					<Self::Relay as $crate::TestExt>::ext_wrapper(|| {
						<Self::Relay as $crate::Chain>::System::block_number()
					})
				}

				fn set_relay_block_number(number: u32) {
					<Self::Relay as $crate::TestExt>::ext_wrapper(|| {
						<Self::Relay as $crate::Chain>::System::set_block_number(number);
					})
				}

				fn process_messages() {
					while Self::has_unprocessed_messages() {
						Self::process_upward_messages();
						Self::process_horizontal_messages();
						Self::process_downward_messages();
						Self::process_bridged_messages();
					}
				}

				fn has_unprocessed_messages() -> bool {
					$crate::DOWNWARD_MESSAGES.with(|b| !b.borrow_mut().get_mut(Self::name()).unwrap().is_empty())
					|| $crate::HORIZONTAL_MESSAGES.with(|b| !b.borrow_mut().get_mut(Self::name()).unwrap().is_empty())
					|| $crate::UPWARD_MESSAGES.with(|b| !b.borrow_mut().get_mut(Self::name()).unwrap().is_empty())
					|| $crate::BRIDGED_MESSAGES.with(|b| !b.borrow_mut().get_mut(Self::name()).unwrap().is_empty())
				}

				fn process_downward_messages() {
					use $crate::{DmpMessageHandler, Bounded, Parachain, RelayChainBlockNumber, TestExt, Encode};

					while let Some((to_para_id, messages))
						= $crate::DOWNWARD_MESSAGES.with(|b| b.borrow_mut().get_mut(Self::name()).unwrap().pop_front()) {
						$(
							let para_id: u32 = <$parachain<Self>>::para_id().into();

							if $crate::PARA_IDS.with(|b| b.borrow_mut().get_mut(Self::name()).unwrap().contains(&to_para_id)) && para_id == to_para_id {
								let mut msg_dedup: Vec<(RelayChainBlockNumber, Vec<u8>)> = Vec::new();
								for m in &messages {
									msg_dedup.push((m.0, m.1.clone()));
								}
								msg_dedup.dedup();

								let msgs = msg_dedup.clone().into_iter().filter(|m| {
									!$crate::DMP_DONE.with(|b| b.borrow().get(Self::name())
										.unwrap_or(&mut $crate::VecDeque::new())
										.contains(&(to_para_id, m.0, m.1.clone()))
									)
								}).collect::<Vec<(RelayChainBlockNumber, Vec<u8>)>>();

								use $crate::{ProcessMessage, CumulusAggregateMessageOrigin, BoundedSlice, WeightMeter};
								for (block, msg) in msgs.clone().into_iter() {
									let mut weight_meter = WeightMeter::new();
									<$parachain<Self>>::ext_wrapper(|| {
										let _ =  <$parachain<Self> as Parachain>::MessageProcessor::process_message(
											&msg[..],
											$crate::CumulusAggregateMessageOrigin::Parent.into(),
											&mut weight_meter,
											&mut msg.using_encoded($crate::blake2_256),
										);
									});
									let messages = msgs.clone().iter().map(|(block, message)| {
										(*block, $crate::array_bytes::bytes2hex("0x", message))
									}).collect::<Vec<_>>();
									$crate::tracing::info!(target: concat!("xcm::dmp::", stringify!($name)), ?to_para_id, ?messages, "Downward messages processed");
									$crate::DMP_DONE.with(|b| b.borrow_mut().get_mut(Self::name()).unwrap().push_back((to_para_id, block, msg)));
								}
							}
						)*
					}
				}

				fn process_horizontal_messages() {
					use $crate::{XcmpMessageHandler, ServiceQueues, Bounded, Parachain, TestExt};

					while let Some((to_para_id, messages))
						= $crate::HORIZONTAL_MESSAGES.with(|b| b.borrow_mut().get_mut(Self::name()).unwrap().pop_front()) {
						let iter = messages.iter().map(|(para_id, relay_block_number, message)| (*para_id, *relay_block_number, &message[..])).collect::<Vec<_>>().into_iter();
						$(
							let para_id: u32 = <$parachain<Self>>::para_id().into();

							if $crate::PARA_IDS.with(|b| b.borrow_mut().get_mut(Self::name()).unwrap().contains(&to_para_id)) && para_id == to_para_id {
								<$parachain<Self>>::ext_wrapper(|| {
									<$parachain<Self> as Parachain>::XcmpMessageHandler::handle_xcmp_messages(iter.clone(), $crate::Weight::MAX);
									// Nudge the MQ pallet to process immediately instead of in the next block.
									let _ =  <$parachain<Self> as Parachain>::MessageProcessor::service_queues($crate::Weight::MAX);
								});
								let messages = messages.clone().iter().map(|(para_id, relay_block_number, message)| {
									(*para_id, *relay_block_number, $crate::array_bytes::bytes2hex("0x", message))
								}).collect::<Vec<_>>();
								$crate::tracing::info!(target: concat!("xcm::hrmp::", stringify!($name)), ?to_para_id, ?messages, "Horizontal messages processed");
							}
						)*
					}
				}

				fn process_upward_messages() {
					use $crate::{Encode, ProcessMessage, TestExt, WeightMeter};

					while let Some((from_para_id, msg)) = $crate::UPWARD_MESSAGES.with(|b| b.borrow_mut().get_mut(Self::name()).unwrap().pop_front()) {
						let mut weight_meter = WeightMeter::new();
						<$relay_chain<Self>>::ext_wrapper(|| {
							let _ =  <$relay_chain<Self> as $crate::RelayChain>::MessageProcessor::process_message(
								&msg[..],
								from_para_id.into(),
								&mut weight_meter,
								&mut msg.using_encoded($crate::blake2_256),
							);
						});
						let message = $crate::array_bytes::bytes2hex("0x", msg.clone());
						$crate::tracing::info!(target: concat!("xcm::ump::", stringify!($name)), ?from_para_id, ?message, "Upward message processed");
					}
				}

				fn process_bridged_messages() {
					use $crate::{Bridge, BridgeMessageHandler, TestExt};
					// Make sure both, including the target `Network` are initialized
					<Self::Bridge as Bridge>::init();

					while let Some(msg) = $crate::BRIDGED_MESSAGES.with(|b| b.borrow_mut().get_mut(Self::name()).unwrap().pop_front()) {
						let dispatch_result = <<Self::Bridge as Bridge>::Target as TestExt>::ext_wrapper(|| {
							<<Self::Bridge as Bridge>::Handler as BridgeMessageHandler>::dispatch_target_inbound_message(msg.clone())
						});

						match dispatch_result {
							Err(e) => panic!("Error {:?} processing bridged message: {:?}", e, msg),
							Ok(()) => {
								<<Self::Bridge as Bridge>::Source as TestExt>::ext_wrapper(|| {
									<<Self::Bridge as Bridge>::Handler as BridgeMessageHandler>::notify_source_message_delivery(msg.lane_id.clone());
								});
								$crate::tracing::info!(target: concat!("bridge::", stringify!($name)), ?msg, "Bridged message processed");
							}
						}
					}
				}

				fn hrmp_channel_parachain_inherent_data(
					para_id: u32,
					relay_parent_number: u32,
					parent_head_data: $crate::HeadData,
					relay_parent_offset: u64,
				) -> $crate::ParachainInherentData {
					let mut sproof = $crate::RelayStateSproofBuilder::default();
					sproof.para_id = para_id.into();
					sproof.current_slot = $crate::polkadot_primitives::Slot::from(relay_parent_number as u64);
					sproof.host_config.max_upward_message_size = 1024 * 1024;
					sproof.num_authorities = relay_parent_offset + 1;

					// egress channel
					let e_index = sproof.hrmp_egress_channel_index.get_or_insert_with(Vec::new);
					for recipient_para_id in $crate::PARA_IDS.with(|b| b.borrow_mut().get_mut(Self::name()).unwrap().clone()) {
						let recipient_para_id = $crate::ParaId::from(recipient_para_id);
						if let Err(idx) = e_index.binary_search(&recipient_para_id) {
							e_index.insert(idx, recipient_para_id);
						}

						sproof.included_para_head = parent_head_data.clone().into();

						sproof
							.hrmp_channels
							.entry($crate::HrmpChannelId {
								sender: sproof.para_id,
								recipient: recipient_para_id,
							})
							.or_insert_with(|| $crate::AbridgedHrmpChannel {
								max_capacity: 1024,
								max_total_size: 1024 * 1024,
								max_message_size: 1024 * 1024,
								msg_count: 0,
								total_size: 0,
								mqc_head: Option::None,
							});
					}

					let (relay_storage_root, proof, relay_parent_descendants) =
						sproof.into_state_root_proof_and_descendants(relay_parent_offset);

					$crate::ParachainInherentData {
						validation_data: $crate::PersistedValidationData {
							parent_head: parent_head_data.clone(),
							relay_parent_number,
							relay_parent_storage_root: relay_storage_root,
							max_pov_size: Default::default(),
						},
						relay_chain_state: proof,
						downward_messages: Default::default(),
						horizontal_messages: Default::default(),
						relay_parent_descendants,
						collator_peer_id: None,
					}
				}
			}

			$crate::paste::paste! {
				pub type [<$relay_chain Relay>] = $relay_chain<$name>;
			}

			$(
				$crate::paste::paste! {
					pub type [<$parachain Para>] = $parachain<$name>;
				}
			)*
		)+
	};
}

#[macro_export]
macro_rules! decl_test_bridges {
	(
		$(
			pub struct $name:ident {
				source = $source:ident,
				target = $target:ident,
				handler = $handler:ident
			}
		),
		+
		$(,)?
	) => {
		$(
			#[derive(Debug)]
			pub struct $name;

			impl $crate::Bridge for $name {
				type Source = $source;
				type Target = $target;
				type Handler = $handler;

				fn init() {
					use $crate::{Network, Parachain};
					// Make sure source and target `Network` have been initialized
					<$source as Chain>::Network::init();
					<$target as Chain>::Network::init();
				}
			}
		)+
	};
}

#[macro_export]
macro_rules! __impl_check_assertion {
	($chain:ident, $network:ident) => {
		impl<$network, Origin, Destination, Hops, Args>
			$crate::CheckAssertion<Origin, Destination, Hops, Args> for $chain<$network>
		where
			$network: $crate::Network,
			Origin: $crate::Chain + Clone,
			Destination: $crate::Chain + Clone,
			Origin::RuntimeOrigin:
				$crate::OriginTrait<AccountId = $crate::AccountIdOf<Origin::Runtime>> + Clone,
			Destination::RuntimeOrigin:
				$crate::OriginTrait<AccountId = $crate::AccountIdOf<Destination::Runtime>> + Clone,
			Hops: Clone,
			Args: Clone,
		{
			fn check_assertion(test: $crate::Test<Origin, Destination, Hops, Args>) {
				use $crate::{Dispatchable, TestExt};

				let chain_name = std::any::type_name::<$chain<$network>>();

				<$chain<$network>>::execute_with(|| {
					if let Some(dispatchable) = test.hops_dispatchable.get(chain_name) {
						$crate::assert_ok!(dispatchable(test.clone()));
					}
					if let Some(call) = test.hops_calls.get(chain_name) {
						$crate::assert_ok!(
							match call.clone().dispatch(test.signed_origin.clone()) {
								// We get rid of `post_info`.
								Ok(_) => Ok(()),
								Err(error_with_post_info) => Err(error_with_post_info.error),
							}
						);
					}
					if let Some(assertion) = test.hops_assertion.get(chain_name) {
						assertion(test);
					}
				});
			}
		}
	};
}

#[macro_export]
macro_rules! assert_expected_events {
    ( $chain:ident, vec![$( $event_pat:pat => { $($attr:ident : $condition:expr, )* }, )*] ) => {
		let mut messages: Vec<String> = Vec::new();
		let mut events = <$chain as $crate::Chain>::events();

		// For each event pattern, we try to find a matching event.
		$(
			// We'll store a string representation of the first partially matching event.
			let mut failure_message: Option<String> = None;
			let mut event_received = false;
			for index in 0..events.len() {
				let event = &events[index];
				match event {
					$event_pat => {
						let mut event_meets_conditions = true;
						let mut conditions_message: Vec<String> = Vec::new();
						event_received = true;

						$(
							if !$condition {
								conditions_message.push(
									format!(
										" - The attribute {} = {:?} did not meet the condition {}\n",
										stringify!($attr),
										$attr,
										stringify!($condition)
									)
								);
							}
							event_meets_conditions &= $condition;
						)*

						if failure_message.is_none() && !conditions_message.is_empty() {
							// Record the failure message.
							failure_message = Some(format!(
								"\n\n{}::\x1b[31m{}\x1b[0m was received but some of its attributes did not meet the conditions.\n\
								 Actual event:\n{:#?}\n\
								 Failures:\n{}",
								stringify!($chain),
								stringify!($event_pat),
								event,
								conditions_message.concat()
							));
						}

						if event_meets_conditions {
							// Found an event where all conditions hold.
							failure_message = None;
							events.remove(index);
							break;
						}
					},
					_ => {}
				}
			}

			if !event_received || failure_message.is_some() {
				// No event matching the pattern was found.
				messages.push(
					format!(
						"\n\n{}::\x1b[31m{}\x1b[0m was never received. All events:\n{:#?}",
						stringify!($chain),
						stringify!($event_pat),
						<$chain as $crate::Chain>::events(),
					)
				);
			}
		)*

		if !messages.is_empty() {
			// Log all events (since they won't be logged after the panic).
			<$chain as $crate::Chain>::events().iter().for_each(|event| {
				$crate::tracing::info!(target: concat!("events::", stringify!($chain)), ?event, "Event emitted");
			});
			panic!("{}", messages.concat())
		}
	}
}

#[macro_export]
macro_rules! bx {
	($e:expr) => {
		Box::new($e)
	};
}

#[macro_export]
macro_rules! decl_test_sender_receiver_accounts_parameter_types {
	( $( $chain:ident { sender: $sender:expr, receiver: $receiver:expr }),+ ) => {
		$crate::paste::paste! {
			$crate::parameter_types! {
				$(
					pub [<$chain Sender>]: $crate::AccountId = <$chain as $crate::Chain>::account_id_of($sender);
					pub [<$chain Receiver>]: $crate::AccountId = <$chain as $crate::Chain>::account_id_of($receiver);
				)+
			}
		}
	};
}

pub struct DefaultParaMessageProcessor<T, M>(PhantomData<(T, M)>);

// Process HRMP messages from sibling paraids
impl<T, M> ProcessMessage for DefaultParaMessageProcessor<T, M>
where
	M: codec::FullCodec
		+ MaxEncodedLen
		+ Clone
		+ Eq
		+ PartialEq
		+ frame_support::pallet_prelude::TypeInfo
		+ Debug,
	T: Parachain,
	T::Runtime: MessageQueueConfig,
	<<T::Runtime as MessageQueueConfig>::MessageProcessor as ProcessMessage>::Origin: PartialEq<M>,
	MessageQueuePallet<T::Runtime>: EnqueueMessage<M> + ServiceQueues,
{
	type Origin = M;

	fn process_message(
		msg: &[u8],
		orig: Self::Origin,
		_meter: &mut WeightMeter,
		_id: &mut XcmHash,
	) -> Result<bool, ProcessMessageError> {
		MessageQueuePallet::<T::Runtime>::enqueue_message(
			msg.try_into().expect("Message too long"),
			orig.clone(),
		);
		MessageQueuePallet::<T::Runtime>::service_queues(Weight::MAX);

		Ok(true)
	}
}

impl<T, M> ServiceQueues for DefaultParaMessageProcessor<T, M>
where
	M: MaxEncodedLen,
	T: Parachain,
	T::Runtime: MessageQueueConfig,
	<<T::Runtime as MessageQueueConfig>::MessageProcessor as ProcessMessage>::Origin: PartialEq<M>,
	MessageQueuePallet<T::Runtime>: EnqueueMessage<M> + ServiceQueues,
{
	type OverweightMessageAddress = ();

	fn service_queues(weight_limit: Weight) -> Weight {
		MessageQueuePallet::<T::Runtime>::service_queues(weight_limit)
	}

	fn execute_overweight(
		_weight_limit: Weight,
		_address: Self::OverweightMessageAddress,
	) -> Result<Weight, ExecuteOverweightError> {
		unimplemented!()
	}
}

pub type MessageOriginFor<T> =
	<<<T as Chain>::Runtime as MessageQueueConfig>::MessageProcessor as ProcessMessage>::Origin;

pub struct DefaultRelayMessageProcessor<T>(PhantomData<T>);

// Process UMP messages on the relay
impl<T> ProcessMessage for DefaultRelayMessageProcessor<T>
where
	T: RelayChain,
	T::Runtime: MessageQueueConfig,
	MessageOriginFor<T>: From<AggregateMessageOrigin>,
	MessageQueuePallet<T::Runtime>: EnqueueMessage<MessageOriginFor<T>> + ServiceQueues,
{
	type Origin = ParaId;

	fn process_message(
		msg: &[u8],
		para: Self::Origin,
		_meter: &mut WeightMeter,
		_id: &mut XcmHash,
	) -> Result<bool, ProcessMessageError> {
		MessageQueuePallet::<T::Runtime>::enqueue_message(
			msg.try_into().expect("Message too long"),
			AggregateMessageOrigin::Ump(UmpQueueId::Para(para)).into(),
		);
		MessageQueuePallet::<T::Runtime>::service_queues(Weight::MAX);

		Ok(true)
	}
}

impl<T> ServiceQueues for DefaultRelayMessageProcessor<T>
where
	T: RelayChain,
	T::Runtime: MessageQueueConfig,
	MessageOriginFor<T>: From<AggregateMessageOrigin>,
	MessageQueuePallet<T::Runtime>: EnqueueMessage<MessageOriginFor<T>> + ServiceQueues,
{
	type OverweightMessageAddress = ();

	fn service_queues(weight_limit: Weight) -> Weight {
		MessageQueuePallet::<T::Runtime>::service_queues(weight_limit)
	}

	fn execute_overweight(
		_weight_limit: Weight,
		_address: Self::OverweightMessageAddress,
	) -> Result<Weight, ExecuteOverweightError> {
		unimplemented!()
	}
}

/// Struct that keeps account's id and balance
#[derive(Clone)]
pub struct TestAccount<R: Chain> {
	pub account_id: AccountIdOf<R::Runtime>,
	pub balance: Balance,
}

/// Default `Args` provided by xcm-emulator to be stored in a `Test` instance
#[derive(Clone)]
pub struct TestArgs<AssetId = u32> {
	pub dest: Location,
	pub beneficiary: Location,
	pub amount: Balance,
	pub assets: Assets,
	pub asset_id: Option<AssetId>,
	pub fee_asset_item: u32,
	pub weight_limit: WeightLimit,
}

impl<AssetId> TestArgs<AssetId> {
	/// Returns a [`TestArgs`] instance to be used for the Relay Chain across integration tests.
	pub fn new_relay(dest: Location, beneficiary_id: AccountId32, amount: Balance) -> Self {
		Self {
			dest,
			beneficiary: AccountId32Junction { network: None, id: beneficiary_id.into() }.into(),
			amount,
			assets: (Here, amount).into(),
			asset_id: None,
			fee_asset_item: 0,
			weight_limit: WeightLimit::Unlimited,
		}
	}

	/// Returns a [`TestArgs`] instance to be used for parachains across integration tests.
	pub fn new_para(
		dest: Location,
		beneficiary_id: AccountId32,
		amount: Balance,
		assets: Assets,
		asset_id: Option<AssetId>,
		fee_asset_item: u32,
	) -> Self {
		Self {
			dest,
			beneficiary: AccountId32Junction { network: None, id: beneficiary_id.into() }.into(),
			amount,
			assets,
			asset_id,
			fee_asset_item,
			weight_limit: WeightLimit::Unlimited,
		}
	}
}

/// Auxiliar struct to help creating a new `Test` instance
pub struct TestContext<T, Origin: Chain, Destination: Chain> {
	pub sender: AccountIdOf<Origin::Runtime>,
	pub receiver: AccountIdOf<Destination::Runtime>,
	pub args: T,
}

/// Struct that helps with tests where either dispatchables or assertions need
/// to be reused. The struct keeps the test's arguments of your choice in the generic `Args`.
/// These arguments can be easily reused and shared between the assertion functions
/// and dispatchable functions, which are also stored in `Test`.
/// `Origin` corresponds to the chain where the XCM interaction starts with an initial execution.
/// `Destination` corresponds to the last chain where an effect of the initial execution is expected
/// to happen. `Hops` refer to all the ordered intermediary chains an initial XCM execution can
/// provoke some effect on.
#[derive(Clone)]
pub struct Test<Origin, Destination, Hops = (), Args = TestArgs>
where
	Origin: Chain + Clone,
	Destination: Chain + Clone,
	Origin::RuntimeOrigin: OriginTrait<AccountId = AccountIdOf<Origin::Runtime>> + Clone,
	Destination::RuntimeOrigin: OriginTrait<AccountId = AccountIdOf<Destination::Runtime>> + Clone,
	Hops: Clone,
{
	pub sender: TestAccount<Origin>,
	pub receiver: TestAccount<Destination>,
	pub signed_origin: Origin::RuntimeOrigin,
	pub root_origin: Origin::RuntimeOrigin,
	pub hops_assertion: HashMap<String, fn(Self)>,
	pub hops_dispatchable: HashMap<String, fn(Self) -> DispatchResult>,
	pub hops_calls: HashMap<String, Origin::RuntimeCall>,
	pub args: Args,
	pub topic_id_tracker: Arc<Mutex<TopicIdTracker>>,
	_marker: PhantomData<(Destination, Hops)>,
}

/// `Test` implementation.
impl<Origin, Destination, Hops, Args> Test<Origin, Destination, Hops, Args>
where
	Args: Clone,
	Origin: Chain + Clone,
	Destination: Chain + Clone,
	Origin::RuntimeOrigin: OriginTrait<AccountId = AccountIdOf<Origin::Runtime>> + Clone,
	Destination::RuntimeOrigin: OriginTrait<AccountId = AccountIdOf<Destination::Runtime>> + Clone,
	Hops: Clone,
{
	/// Asserts that a single unique topic ID exists across all chains.
	pub fn assert_unique_topic_id(&self) {
		self.topic_id_tracker.lock().unwrap().assert_unique();
	}
	/// Inserts a topic ID for a specific chain and asserts it remains globally unique.
	pub fn insert_unique_topic_id(&mut self, chain: &str, id: H256) {
		self.topic_id_tracker.lock().unwrap().insert_and_assert_unique(chain, id);
	}
}

impl<Origin, Destination, Hops, Args> Test<Origin, Destination, Hops, Args>
where
	Args: Clone,
	Origin: Chain + Clone + CheckAssertion<Origin, Destination, Hops, Args>,
	Destination: Chain + Clone + CheckAssertion<Origin, Destination, Hops, Args>,
	Origin::RuntimeOrigin: OriginTrait<AccountId = AccountIdOf<Origin::Runtime>> + Clone,
	Destination::RuntimeOrigin: OriginTrait<AccountId = AccountIdOf<Destination::Runtime>> + Clone,
	Hops: Clone + CheckAssertion<Origin, Destination, Hops, Args>,
{
	/// Creates a new `Test` instance.
	pub fn new(test_args: TestContext<Args, Origin, Destination>) -> Self {
		Test {
			sender: TestAccount {
				account_id: test_args.sender.clone(),
				balance: Origin::account_data_of(test_args.sender.clone()).free,
			},
			receiver: TestAccount {
				account_id: test_args.receiver.clone(),
				balance: Destination::account_data_of(test_args.receiver.clone()).free,
			},
			signed_origin: <Origin as Chain>::RuntimeOrigin::signed(test_args.sender),
			root_origin: <Origin as Chain>::RuntimeOrigin::root(),
			hops_assertion: Default::default(),
			hops_dispatchable: Default::default(),
			hops_calls: Default::default(),
			args: test_args.args,
			topic_id_tracker: Arc::new(Mutex::new(TopicIdTracker::new())),
			_marker: Default::default(),
		}
	}
	/// Stores an assertion in a particular Chain
	pub fn set_assertion<Hop>(&mut self, assertion: fn(Self)) {
		let chain_name = std::any::type_name::<Hop>();
		self.hops_assertion.insert(chain_name.to_string(), assertion);
	}
	/// Stores a dispatchable in a particular Chain
	pub fn set_dispatchable<Hop>(&mut self, dispatchable: fn(Self) -> DispatchResult) {
		let chain_name = std::any::type_name::<Hop>();
		self.hops_dispatchable.insert(chain_name.to_string(), dispatchable);
	}
	/// Stores a call in a particular Chain, this will later be dispatched.
	pub fn set_call(&mut self, call: Origin::RuntimeCall) {
		let chain_name = std::any::type_name::<Origin>();
		self.hops_calls.insert(chain_name.to_string(), call);
	}
	/// Executes all dispatchables and assertions in order from `Origin` to `Destination`
	pub fn assert(&mut self) {
		Origin::check_assertion(self.clone());
		Hops::check_assertion(self.clone());
		Destination::check_assertion(self.clone());
		Self::update_balances(self);
	}
	/// Updates sender and receiver balances
	fn update_balances(&mut self) {
		self.sender.balance = Origin::account_data_of(self.sender.account_id.clone()).free;
		self.receiver.balance = Destination::account_data_of(self.receiver.account_id.clone()).free;
	}
}

pub mod helpers {
	use super::*;

	pub fn within_threshold(threshold: u64, expected_value: u64, current_value: u64) -> bool {
		let margin = (current_value * threshold) / 100;
		let lower_limit = expected_value.checked_sub(margin).unwrap_or(u64::MIN);
		let upper_limit = expected_value.checked_add(margin).unwrap_or(u64::MAX);

		current_value >= lower_limit && current_value <= upper_limit
	}

	pub fn weight_within_threshold(
		(threshold_time, threshold_size): (u64, u64),
		expected_weight: Weight,
		weight: Weight,
	) -> bool {
		let ref_time_within =
			within_threshold(threshold_time, expected_weight.ref_time(), weight.ref_time());
		let proof_size_within =
			within_threshold(threshold_size, expected_weight.proof_size(), weight.proof_size());

		ref_time_within && proof_size_within
	}
}
