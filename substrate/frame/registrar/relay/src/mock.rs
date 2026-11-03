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

//! Mock runtime for `pallet-registrar-relay`.
//!
//! Neither the relay chain's `paras` stack nor the parachain is modelled here: [`MockRegistrar`]
//! records what it was asked to onboard and [`RecordingSender`] records what would have been
//! reported back, both with injectable failures. Keeping the real `paras_registrar` out of this
//! crate is what lets it stay free of any Polkadot dependency; the two halves meeting for real is
//! the job of the `pallet-registrar-test` crate.

use crate::{self as pallet_registrar_relay, SendToPara};
use frame_support::{derive_impl, parameter_types, traits::ConstU32};
use registrar_primitives::{MessageToPara, ParaId, ParachainRegistrar};
use sp_runtime::BuildStorage;

pub type AccountId = u64;

pub const ALICE: AccountId = 1;
pub const PARA_A: ParaId = 2000;
pub const PARA_B: ParaId = 2001;

pub const MIN_CODE_SIZE: u32 = 9;
pub const MAX_CODE_SIZE: u32 = 1_000;
pub const MAX_HEAD_SIZE: u32 = 100;
pub const MAX_PENDING: u32 = 2;

#[frame_support::runtime]
mod test_runtime {
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
		RuntimeTask
	)]
	pub struct Test;

	#[runtime::pallet_index(0)]
	pub type System = frame_system::Pallet<Test>;

	#[runtime::pallet_index(1)]
	pub type Registrar = pallet_registrar_relay::Pallet<Test>;
}

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type Block = frame_system::mocking::MockBlockU32<Test>;
	type AccountId = AccountId;
	type Lookup = sp_runtime::traits::IdentityLookup<AccountId>;
}

parameter_types! {
	/// Paras `MockRegistrar` has onboarded, in order.
	pub static Onboarded: Vec<(ParaId, AccountId, Vec<u8>, Vec<u8>)> = Vec::new();
	/// Paras `MockRegistrar` claims the relay chain already knows.
	pub static AlreadyKnown: Vec<ParaId> = Vec::new();
	/// When true, `MockRegistrar::register` fails.
	pub static RegisterFails: bool = false;
	/// Reports handed to the transport, oldest first.
	pub static SentMessages: Vec<MessageToPara> = Vec::new();
	/// When true, the transport refuses everything.
	pub static SendFails: bool = false;
}

/// Stands in for the relay chain's `paras_registrar`.
pub struct MockRegistrar;

impl ParachainRegistrar for MockRegistrar {
	type AccountId = AccountId;

	fn check_onboarding(head_len: u32, code_len: u32) -> Result<(), ()> {
		if !(MIN_CODE_SIZE..=MAX_CODE_SIZE).contains(&code_len) || head_len > MAX_HEAD_SIZE {
			return Err(());
		}
		Ok(())
	}

	fn is_registered(para_id: ParaId) -> bool {
		AlreadyKnown::get().contains(&para_id) ||
			Onboarded::get().iter().any(|(id, ..)| *id == para_id)
	}

	fn register(
		manager: Self::AccountId,
		para_id: ParaId,
		genesis_head: Vec<u8>,
		validation_code: Vec<u8>,
	) -> sp_runtime::DispatchResult {
		if RegisterFails::get() {
			return Err(sp_runtime::DispatchError::Other("registrar refused"));
		}
		Onboarded::mutate(|v| v.push((para_id, manager, genesis_head, validation_code)));
		Ok(())
	}
}

/// A [`SendToPara`] that records instead of sending, and can be made to fail.
pub struct RecordingSender;

impl SendToPara for RecordingSender {
	fn send(message: MessageToPara) -> Result<(), ()> {
		if SendFails::get() {
			return Err(());
		}
		SentMessages::mutate(|sent| sent.push(message));
		Ok(())
	}
}

/// Take every report recorded so far, clearing the log.
pub fn take_sent() -> Vec<MessageToPara> {
	SentMessages::mutate(core::mem::take)
}

/// The origin the parachain's messages arrive under in this mock.
pub type ParaOrigin = frame_system::EnsureRoot<AccountId>;

impl pallet_registrar_relay::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type ParaOrigin = ParaOrigin;
	type SendToPara = RecordingSender;
	type Registrar = MockRegistrar;
	type MaxHeadDataSize = ConstU32<MAX_HEAD_SIZE>;
	type MaxCodeSize = ConstU32<MAX_CODE_SIZE>;
	type MaxPendingRegistrations = ConstU32<MAX_PENDING>;
	type UnsignedPriority = ConstU64<100>;
	type WeightInfo = ();
}

parameter_types! {
	pub const Unused: u32 = 0;
}

use frame_support::traits::ConstU64;

/// Fresh externalities with every recorder cleared.
pub fn new_test_ext() -> sp_io::TestExternalities {
	Onboarded::set(Vec::new());
	AlreadyKnown::set(Vec::new());
	RegisterFails::set(false);
	SentMessages::set(Vec::new());
	SendFails::set(false);

	let t = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();
	let mut ext = sp_io::TestExternalities::new(t);
	ext.execute_with(|| System::set_block_number(1));
	ext
}

/// Every event this pallet emitted, oldest first, clearing the log.
pub fn registrar_events() -> Vec<pallet_registrar_relay::Event<Test>> {
	let events = System::events()
		.into_iter()
		.filter_map(|e| match e.event {
			RuntimeEvent::Registrar(inner) => Some(inner),
			_ => None,
		})
		.collect();
	System::reset_events();
	events
}
