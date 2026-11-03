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

//! Smoke tests for `polkadot-prospective-parachains-test-sim`.
//!
//! These validate the harness wiring against a second tenant — they don't yet exercise
//! prospective-parachains' protocol behaviour. Real protocol scenarios land alongside
//! domain expertise about which behaviours matter most to lock in.

use crate::common::ProspectiveParachains;
use polkadot_subsystem_test_sim::{
	contract::Query,
	harness::{AnswerQuery, Sim, SimConfig},
};

struct PanicResponder;
impl AnswerQuery for PanicResponder {
	fn answer(&mut self, query: Query) {
		panic!("unexpected query in smoke test: {:?}", query);
	}
}

#[test]
fn prospective_parachains_starts_and_concludes() {
	// No `OverseerSignal::ActiveLeaves` is sent → prospective fires no Runtime/ChainApi
	// queries; the panic responder stays untouched. `finish` sends `Conclude`; the
	// subsystem drops its main loop cleanly.
	let sim = Sim::<ProspectiveParachains>::start(SimConfig::default(), PanicResponder);
	let recorder = sim.finish();
	assert_eq!(recorder.len(), 0);
}
