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

//! Test helpers for the dmp module tests.

use super::*;
use crate::mock::{new_test_ext, Dmp, MockGenesisConfig, Paras, System, Test};
use polkadot_primitives::BlockNumber;
use sp_io::TestExternalities;

/// Number of pages the lazy-delete path is supposed to clear per call.
///
/// Hardcoded so the tests act as an independent oracle and don't silently track
/// changes to the production constant.
pub(crate) const EXPECTED_LAZY_DELETE_PAGES: u64 = 3;

pub(crate) fn run_to_block(to: BlockNumber, new_session: Option<Vec<BlockNumber>>) {
	while System::block_number() < to {
		let b = System::block_number();
		Paras::initializer_finalize(b);
		Dmp::initializer_finalize();
		if new_session.as_ref().map_or(false, |v| v.contains(&(b + 1))) {
			Dmp::initializer_on_new_session(&Default::default(), &Vec::new());
		}
		System::on_finalize(b);

		System::on_initialize(b + 1);
		System::set_block_number(b + 1);

		Paras::initializer_finalize(b + 1);
		Dmp::initializer_initialize(b + 1);
	}
}

pub(crate) fn default_genesis_config() -> MockGenesisConfig {
	MockGenesisConfig {
		configuration: crate::configuration::GenesisConfig {
			config: crate::configuration::HostConfiguration {
				max_downward_message_size: 1024,
				..Default::default()
			},
		},
		..Default::default()
	}
}

pub(crate) fn queue_downward_message(
	para_id: ParaId,
	msg: DownwardMessage,
) -> Result<(), QueueDownwardMessageError> {
	Dmp::queue_downward_message(
		&configuration::ActiveConfig::<crate::mock::Test>::get(),
		para_id,
		msg,
	)
}

pub(crate) fn register_paras(paras: &[ParaId]) {
	paras.iter().for_each(|p| {
		Dmp::make_parachain_reachable(*p);
	});
}

/// All `PageIndex` keys currently in storage for the given para, sorted ascending.
pub(crate) fn pages_in_storage(para: ParaId) -> Vec<PageIndex> {
	let mut out: Vec<_> =
		DownwardMessageQueuePages::<crate::mock::Test>::iter_key_prefix(&para).collect();
	out.sort();
	out
}

/// Run `f` against `ext`, asserting the dmp queue invariants both before and
/// after — same shape as `TestExternalities::execute_with` but with always-on
/// `integrity_test` checks.
pub(crate) fn execute_with_try_state<R>(ext: &mut TestExternalities, f: impl FnOnce() -> R) -> R {
	ext.execute_with(|| {
		InboundDownwardQueue::<Test>::try_state();
		let r = f();
		InboundDownwardQueue::<Test>::try_state();
		r
	})
}

/// Single-shot variant: build a fresh `TestExternalities` from `state` and
/// run `f` inside it with always-on integrity checks. Drop-in replacement for
/// `new_test_ext(state).execute_with(...)`.
pub(crate) fn new_test_ext_integrity<R>(state: MockGenesisConfig, f: impl FnOnce() -> R) -> R {
	let mut ext = new_test_ext(state);
	execute_with_try_state(&mut ext, f)
}
