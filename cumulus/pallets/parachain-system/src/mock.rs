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

//! Test setup and helpers.

#![cfg(test)]

use super::*;

use alloc::collections::vec_deque::VecDeque;
use codec::Encode;
use core::num::NonZeroU32;
use cumulus_primitives_core::{
	relay_chain::BlockNumber as RelayBlockNumber, AggregateMessageOrigin, InboundDownwardMessage,
	InboundHrmpMessage, PersistedValidationData,
};
use cumulus_test_relay_sproof_builder::RelayStateSproofBuilder;
use frame_support::{
	derive_impl,
	inherent::{InherentData, ProvideInherent},
	parameter_types,
	traits::{
		OnFinalize, OnInitialize, ProcessMessage, ProcessMessageError, UnfilteredDispatchable,
	},
	weights::{Weight, WeightMeter},
};
use frame_system::{limits::BlockWeights, pallet_prelude::BlockNumberFor, RawOrigin};
use sp_runtime::{traits::BlakeTwo256, BuildStorage};
use sp_version::RuntimeVersion;
use std::cell::RefCell;

use crate as parachain_system;
use crate::consensus_hook::UnincludedSegmentCapacity;

type Block = frame_system::mocking::MockBlock<Test>;

frame_support::construct_runtime!(
	pub enum Test {
		System: frame_system,
		ParachainSystem: parachain_system,
		MessageQueue: pallet_message_queue,
	}
);

parameter_types! {
	pub Version: RuntimeVersion = RuntimeVersion {
		spec_name: alloc::borrow::Cow::Borrowed("test"),
		impl_name: alloc::borrow::Cow::Borrowed("system-test"),
		authoring_version: 1,
		spec_version: 1,
		impl_version: 1,
		apis: sp_version::create_apis_vec!([]),
		transaction_version: 1,
		system_version: 1,
	};
	pub RuntimeBlockWeights: BlockWeights =
		BlockWeights::simple_max(Weight::from_parts(1024, 6 * 1024 * 1024));
	pub const ParachainId: ParaId = ParaId::new(200);
	pub const ReservedXcmpWeight: Weight = Weight::zero();
	pub const ReservedDmpWeight: Weight = Weight::zero();
}

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type BlockWeights = RuntimeBlockWeights;
	type Block = Block;
	type Version = Version;
	type OnSetCode = ParachainSetCode<Self>;
}

parameter_types! {
	pub const RelayOrigin: AggregateMessageOrigin = AggregateMessageOrigin::Parent;
}

impl Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type OnSystemEvent = ();
	type SelfParaId = ParachainId;
	type OutboundXcmpMessageSource = FromThreadLocal;
	type DmpQueue = frame_support::traits::EnqueueWithOrigin<MessageQueue, RelayOrigin>;
	type ReservedDmpWeight = ReservedDmpWeight;
	type XcmpMessageHandler = SaveIntoThreadLocal;
	type ReservedXcmpWeight = ReservedXcmpWeight;
	type CheckAssociatedRelayNumber = AnyRelayNumber;
	type ConsensusHook = TestConsensusHook;
	type WeightInfo = ();
	type RelayParentOffset = MockRelayParentOffset;
	type SchedulingSignatureVerifier = ();
}

std::thread_local! {
	pub static CONSENSUS_HOOK: RefCell<Box<dyn Fn(&RelayChainStateProof) -> (Weight, UnincludedSegmentCapacity)>>
		= RefCell::new(Box::new(|_| (Weight::zero(), NonZeroU32::new(1).unwrap().into())));
}

pub struct TestConsensusHook;

impl ConsensusHook for TestConsensusHook {
	fn on_state_proof(s: &RelayChainStateProof) -> (Weight, UnincludedSegmentCapacity) {
		CONSENSUS_HOOK.with(|f| f.borrow_mut()(s))
	}
}

parameter_types! {
	pub const MaxWeight: Weight = Weight::MAX;
	/// Settable so tests can exercise the non-zero case; `ConstU32` is fixed at compile time.
	pub static MockRelayParentOffset: u32 = 0;
}

impl pallet_message_queue::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	// NOTE that normally for benchmarking we should use the No-OP message processor, but in this
	// case its a mocked runtime and will only be used to generate insecure default weights.
	type MessageProcessor = SaveIntoThreadLocal;
	type Size = u32;
	type QueueChangeHandler = ();
	type QueuePausedQuery = ();
	type HeapSize = sp_core::ConstU32<{ 103 * 1024 }>;
	type MaxStale = sp_core::ConstU32<8>;
	type ServiceWeight = MaxWeight;
	type IdleMaxServiceWeight = ();
	type WeightInfo = ();
}

/// A `XcmpMessageSource` that takes messages from thread-local.
pub struct FromThreadLocal;

/// A `MessageProcessor` that stores all messages in thread-local.
pub struct SaveIntoThreadLocal;

std::thread_local! {
	pub static HANDLED_DMP_MESSAGES: RefCell<Vec<Vec<u8>>> = RefCell::new(Vec::new());
	pub static HANDLED_XCMP_MESSAGES: RefCell<Vec<(ParaId, relay_chain::BlockNumber, Vec<u8>)>> = RefCell::new(Vec::new());
	pub static SENT_MESSAGES: RefCell<Vec<(ParaId, Vec<u8>)>> = RefCell::new(Vec::new());
}

pub fn send_message(dest: ParaId, message: Vec<u8>) {
	SENT_MESSAGES.with(|m| m.borrow_mut().push((dest, message)));
}

impl XcmpMessageSource for FromThreadLocal {
	fn take_outbound_messages(
		maximum_channels: usize,
		excluded_recipients: &[ParaId],
	) -> Vec<(ParaId, Vec<u8>)> {
		let mut ids =
			std::collections::BTreeSet::<ParaId>::from_iter(excluded_recipients.iter().copied());
		let mut taken_messages = 0;
		let mut taken_bytes = 0;
		let mut result = Vec::new();
		SENT_MESSAGES.with(|ms| {
			ms.borrow_mut().retain(|m| {
				let status = <Pallet<Test> as GetChannelInfo>::get_channel_status(m.0);
				let (max_size_now, max_size_ever) = match status {
					ChannelStatus::Ready(now, ever) => (now, ever),
					ChannelStatus::Closed => return false, // drop message
					ChannelStatus::Full => return true,    // keep message queued.
				};

				let msg_len = m.1.len();

				if !ids.contains(&m.0) &&
					taken_messages < maximum_channels &&
					msg_len <= max_size_ever &&
					taken_bytes + msg_len <= max_size_now
				{
					ids.insert(m.0);
					taken_messages += 1;
					taken_bytes += msg_len;
					result.push(m.clone());
					false
				} else {
					true
				}
			})
		});
		result
	}
}

impl ProcessMessage for SaveIntoThreadLocal {
	type Origin = AggregateMessageOrigin;

	fn process_message(
		message: &[u8],
		origin: Self::Origin,
		_meter: &mut WeightMeter,
		_id: &mut [u8; 32],
	) -> Result<bool, ProcessMessageError> {
		assert_eq!(origin, Self::Origin::Parent);

		HANDLED_DMP_MESSAGES.with(|m| {
			m.borrow_mut().push(message.to_vec());
			Weight::zero()
		});
		Ok(true)
	}
}

impl XcmpMessageHandler for SaveIntoThreadLocal {
	fn handle_xcmp_messages<'a, I: Iterator<Item = (ParaId, RelayBlockNumber, &'a [u8])>>(
		iter: I,
		_max_weight: Weight,
	) -> Weight {
		HANDLED_XCMP_MESSAGES.with(|m| {
			for (sender, sent_at, message) in iter {
				m.borrow_mut().push((sender, sent_at, message.to_vec()));
			}
			Weight::zero()
		})
	}
}

// This function basically just builds a genesis storage key/value store according to
// our desired mockup.
pub fn new_test_ext() -> sp_io::TestExternalities {
	HANDLED_DMP_MESSAGES.with(|m| m.borrow_mut().clear());
	HANDLED_XCMP_MESSAGES.with(|m| m.borrow_mut().clear());

	frame_system::GenesisConfig::<Test>::default().build_storage().unwrap().into()
}

#[allow(dead_code)]
pub fn mk_dmp(sent_at: u8, size: usize) -> InboundDownwardMessage {
	InboundDownwardMessage { sent_at: sent_at as u32, msg: vec![sent_at; size] }
}

pub fn mk_hrmp(sent_at: u8, size: usize) -> InboundHrmpMessage {
	InboundHrmpMessage { sent_at: sent_at as u32, data: vec![sent_at; size] }
}

pub struct ReadRuntimeVersion(pub Vec<u8>);

impl sp_core::traits::ReadRuntimeVersion for ReadRuntimeVersion {
	fn read_runtime_version(
		&self,
		_wasm_code: &[u8],
		_ext: &mut dyn sp_externalities::Externalities,
	) -> Result<Vec<u8>, String> {
		Ok(self.0.clone())
	}
}

pub fn wasm_ext() -> sp_io::TestExternalities {
	let version = RuntimeVersion {
		spec_name: "test".into(),
		spec_version: 2,
		impl_version: 1,
		..Default::default()
	};

	let mut ext = new_test_ext();
	ext.register_extension(sp_core::traits::ReadRuntimeVersionExt::new(ReadRuntimeVersion(
		version.encode(),
	)));
	ext
}

pub struct BlockTest {
	n: BlockNumberFor<Test>,
	within_block: Box<dyn Fn()>,
	after_block: Option<Box<dyn Fn()>>,
}

/// BlockTests exist to test blocks with some setup: we have to assume that
/// `validate_block` will mutate and check storage in certain predictable
/// ways, for example, and we want to always ensure that tests are executed
/// in the context of some particular block number.
#[derive(Default)]
pub struct BlockTests {
	tests: Vec<BlockTest>,
	without_externalities: bool,
	pending_upgrade: Option<RelayChainBlockNumber>,
	ran: bool,
	relay_sproof_builder_hook:
		Option<Box<dyn Fn(&BlockTests, RelayChainBlockNumber, &mut RelayStateSproofBuilder)>>,
	inherent_data_hook:
		Option<Box<dyn Fn(&BlockTests, RelayChainBlockNumber, &mut ParachainInherentData)>>,
	inclusion_delay: Option<usize>,
	relay_block_number: Option<Box<dyn Fn(&BlockNumberFor<Test>) -> RelayChainBlockNumber>>,

	included_para_head: Option<relay_chain::HeadData>,
	pending_blocks: VecDeque<relay_chain::HeadData>,
	pre_inherent_digests: Vec<sp_runtime::DigestItem>,
}

impl BlockTests {
	pub fn new() -> BlockTests {
		// `parameter_types!` statics are thread-local and outlive the test that set them, and a
		// `should_panic` test never runs cleanup, so reset here rather than at the end of a test.
		MockRelayParentOffset::set(0);
		Default::default()
	}

	pub fn new_without_externalities() -> BlockTests {
		let mut tests = BlockTests::new();
		tests.without_externalities = true;
		tests
	}

	pub fn add_raw(mut self, test: BlockTest) -> Self {
		self.tests.push(test);
		self
	}

	pub fn add<F>(self, n: BlockNumberFor<Test>, within_block: F) -> Self
	where
		F: 'static + Fn(),
	{
		self.add_raw(BlockTest { n, within_block: Box::new(within_block), after_block: None })
	}

	pub fn add_with_post_test<F1, F2>(
		self,
		n: BlockNumberFor<Test>,
		within_block: F1,
		after_block: F2,
	) -> Self
	where
		F1: 'static + Fn(),
		F2: 'static + Fn(),
	{
		self.add_raw(BlockTest {
			n,
			within_block: Box::new(within_block),
			after_block: Some(Box::new(after_block)),
		})
	}

	pub fn with_relay_sproof_builder<F>(mut self, f: F) -> Self
	where
		F: 'static + Fn(&BlockTests, RelayChainBlockNumber, &mut RelayStateSproofBuilder),
	{
		self.relay_sproof_builder_hook = Some(Box::new(f));
		self
	}

	pub fn with_relay_block_number<F>(mut self, f: F) -> Self
	where
		F: 'static + Fn(&BlockNumberFor<Test>) -> RelayChainBlockNumber,
	{
		self.relay_block_number = Some(Box::new(f));
		self
	}

	pub fn with_inherent_data<F>(mut self, f: F) -> Self
	where
		F: 'static + Fn(&BlockTests, RelayChainBlockNumber, &mut ParachainInherentData),
	{
		self.inherent_data_hook = Some(Box::new(f));
		self
	}

	pub fn with_pre_inherent_digests(mut self, digests: Vec<sp_runtime::DigestItem>) -> Self {
		self.pre_inherent_digests = digests;
		self
	}

	/// Sets `Config::RelayParentOffset` for this test. Reset to 0 by [`BlockTests::new`].
	pub fn with_relay_parent_offset(self, offset: u32) -> Self {
		MockRelayParentOffset::set(offset);
		self
	}

	pub fn with_inclusion_delay(mut self, inclusion_delay: usize) -> Self {
		self.inclusion_delay.replace(inclusion_delay);
		self
	}

	pub fn run(&mut self) {
		wasm_ext().execute_with(|| {
			self.run_without_ext();
		});
	}

	pub fn run_without_ext(&mut self) {
		self.ran = true;

		let mut parent_head_data = {
			let header = HeaderFor::<Test>::new_from_number(0);
			relay_chain::HeadData(header.encode())
		};

		self.included_para_head = Some(parent_head_data.clone());

		for BlockTest { n, within_block, after_block } in self.tests.iter() {
			let relay_parent_number = self
				.relay_block_number
				.as_ref()
				.map(|f| f(n))
				.unwrap_or(*n as RelayChainBlockNumber);
			// clear pending updates, as applicable
			if let Some(upgrade_block) = self.pending_upgrade {
				if n >= &upgrade_block.into() {
					self.pending_upgrade = None;
				}
			}

			// begin initialization
			let parent_hash = BlakeTwo256::hash(&parent_head_data.0);
			System::reset_events();
			System::initialize(n, &parent_hash, &Default::default());

			// now mess with the storage the way validate_block does
			let mut sproof_builder = RelayStateSproofBuilder::default();
			sproof_builder.included_para_head = self
				.included_para_head
				.clone()
				.unwrap_or_else(|| parent_head_data.clone())
				.into();
			if let Some(ref hook) = self.relay_sproof_builder_hook {
				hook(self, relay_parent_number, &mut sproof_builder);
			}
			let (relay_parent_storage_root, relay_chain_state) =
				sproof_builder.into_state_root_and_proof();
			let vfp = PersistedValidationData {
				relay_parent_number,
				relay_parent_storage_root,
				..Default::default()
			};

			<ValidationData<Test>>::put(&vfp);
			NewValidationCode::<Test>::kill();

			// It is insufficient to push the validation function params
			// to storage; they must also be included in the inherent data.
			let inherent_data = {
				let mut inherent_data = InherentData::default();
				let mut system_inherent_data = ParachainInherentData {
					validation_data: vfp.clone(),
					relay_chain_state,
					downward_messages: Default::default(),
					horizontal_messages: Default::default(),
					relay_parent_descendants: Default::default(),
					collator_peer_id: None,
				};
				if let Some(ref hook) = self.inherent_data_hook {
					hook(self, relay_parent_number, &mut system_inherent_data);
				}
				inherent_data
					.put_data(
						cumulus_primitives_parachain_inherent::INHERENT_IDENTIFIER,
						&system_inherent_data,
					)
					.expect("failed to put VFP inherent");
				inherent_data
			};

			// execute the block
			for digest in &self.pre_inherent_digests {
				System::deposit_log(digest.clone());
			}
			ParachainSystem::on_initialize(*n);
			ParachainSystem::create_inherent(&inherent_data)
				.expect("got an inherent")
				.dispatch_bypass_filter(RawOrigin::None.into())
				.expect("dispatch succeeded");
			MessageQueue::on_initialize(*n);
			within_block();
			MessageQueue::on_finalize(*n);
			ParachainSystem::on_finalize(*n);

			// did block execution set new validation code?
			if NewValidationCode::<Test>::exists() && self.pending_upgrade.is_some() {
				panic!("attempted to set validation code while upgrade was pending");
			}

			// clean up
			System::maybe_apply_pending_code_upgrade();
			let header = System::finalize();
			let head_data = relay_chain::HeadData(header.encode());
			parent_head_data = head_data.clone();
			match self.inclusion_delay {
				Some(delay) if delay > 0 => {
					self.pending_blocks.push_back(head_data);
					if self.pending_blocks.len() > delay {
						let included = self.pending_blocks.pop_front().unwrap();

						self.included_para_head.replace(included);
					}
				},
				_ => {
					self.included_para_head.replace(head_data);
				},
			}

			if let Some(after_block) = after_block {
				after_block();
			}
		}
	}
}

impl Drop for BlockTests {
	fn drop(&mut self) {
		if !self.ran {
			if self.without_externalities {
				self.run_without_ext();
			} else {
				self.run();
			}
		}
	}
}
