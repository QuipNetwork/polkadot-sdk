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

#![cfg(test)]

use crate as offences;
use crate::Config;
use codec::Encode;
use frame_support::{
	derive_impl, parameter_types,
	traits::ConstU32,
	weights::{constants::RocksDbWeight, Weight},
};
use sp_runtime::{traits::IdentityLookup, BuildStorage, Perbill};
use sp_staking::{
	offence::{self, Kind, OffenceDetails},
	SessionIndex,
};

pub struct OnOffenceHandler;

parameter_types! {
	pub static OnOffencePerbill: Vec<Perbill> = Default::default();
	pub static OffenceWeight: Weight = Default::default();
}

impl<Reporter, Offender> offence::OnOffenceHandler<Reporter, Offender, Weight>
	for OnOffenceHandler
{
	fn on_offence(
		_offenders: &[OffenceDetails<Reporter, Offender>],
		slash_fraction: &[Perbill],
		_offence_session: SessionIndex,
	) -> Weight {
		OnOffencePerbill::mutate(|f| {
			*f = slash_fraction.to_vec();
		});

		OffenceWeight::get()
	}
}

pub fn with_on_offence_fractions<R, F: FnOnce(&mut Vec<Perbill>) -> R>(f: F) -> R {
	OnOffencePerbill::mutate(|fractions| f(fractions))
}

type Block = frame_system::mocking::MockBlock<Runtime>;

frame_support::construct_runtime!(
	pub enum Runtime {
		System: frame_system,
		Offences: offences,
	}
);

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Runtime {
	type Nonce = u64;
	type Lookup = IdentityLookup<Self::AccountId>;
	type Block = Block;
	type RuntimeEvent = RuntimeEvent;
	type DbWeight = RocksDbWeight;
	type MaxConsumers = ConstU32<16>;
}

impl Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type IdentificationTuple = u64;
	type OnOffenceHandler = OnOffenceHandler;
}

pub fn new_test_ext() -> sp_io::TestExternalities {
	let t = frame_system::GenesisConfig::<Runtime>::default().build_storage().unwrap();
	let mut ext = sp_io::TestExternalities::new(t);
	ext.execute_with(|| System::set_block_number(1));
	ext
}

pub const KIND: [u8; 16] = *b"test_report_1234";

/// Returns all offence details for the specific `kind` happened at the specific time slot.
pub fn offence_reports(kind: Kind, time_slot: u128) -> Vec<OffenceDetails<u64, u64>> {
	<crate::ConcurrentReportsIndex<Runtime>>::get(&kind, &time_slot.encode())
		.into_iter()
		.map(|report_id| {
			<crate::Reports<Runtime>>::get(&report_id)
				.expect("dangling report id is found in ConcurrentReportsIndex")
		})
		.collect()
}

#[derive(Clone)]
pub struct Offence {
	pub validator_set_count: u32,
	pub offenders: Vec<u64>,
	pub slot: u128,
}

impl offence::Offence<u64> for Offence {
	const ID: offence::Kind = KIND;
	type Slot = u128;

	fn offenders(&self) -> Vec<u64> {
		self.offenders.clone()
	}

	fn validator_set_count(&self) -> u32 {
		self.validator_set_count
	}

	fn slot(&self) -> u128 {
		self.slot
	}

	fn session_index(&self) -> SessionIndex {
		1
	}

	fn slash_fraction(&self, offenders_count: u32) -> Perbill {
		Perbill::from_percent(5 + offenders_count * 100 / self.validator_set_count)
	}
}
