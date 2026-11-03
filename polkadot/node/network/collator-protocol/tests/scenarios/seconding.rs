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

//! Seconding flow.

mod full_seconding {
	use crate::common::{
		builders::{Candidate, ProtocolVersion::V2},
		contract::RepBucket,
		harness::CollatorSut,
		world::{activated_world, WorldExt as _},
	};
	use polkadot_primitives::{CoreIndex, Id as ParaId};

	const PARA: ParaId = ParaId::new(2000);

	#[crate::sim_test]
	fn advertise_fetch_respond_yields_second_candidate<S: CollatorSut>() {
		let mut w = activated_world::<S>(&[(CoreIndex(0), PARA)]);
		let leaf_n = w.leaf_number();
		let candidate = Candidate::builder()
			.para(PARA)
			.relay_parent(w.leaf())
			.relay_parent_number(leaf_n)
			.build();

		let peer = w.declared_peer(PARA, V2);
		w.full_second(&peer, &candidate);

		// Sanity counterpart for `project_collator_experimental_no_invalid_reputation_event`:
		// a *valid* candidate must NOT produce a Reputation::Malicious. Pairs with the Invalid
		// scenario in `fetch_next_on_invalid` to confirm the Malicious emission is gated on
		// invalidity, not on every fetch outcome.
		w.expect_no_rep(&peer, RepBucket::Malicious);
	}
}

mod fragment_chain_seconding {
	use crate::common::{
		builders::{Candidate, ProtocolVersion::V2},
		harness::CollatorSut,
		world::{activated_world, WorldExt as _},
	};
	use polkadot_primitives::{CoreIndex, HeadData, Id as ParaId};

	const PARA_A: ParaId = ParaId::new(2000);

	#[crate::sim_test]
	fn parent_then_child_seconds_in_order<S: CollatorSut>() {
		let mut w = activated_world::<S>(&[(CoreIndex(0), PARA_A)]);
		let leaf_n = w.leaf_number();

		// Parent: parent_head=empty, output=vec![1]. Child: parent_head=vec![1], output=vec![2].
		let parent = Candidate::builder()
			.para(PARA_A)
			.relay_parent(w.leaf())
			.relay_parent_number(leaf_n)
			.parent_head(HeadData(Vec::new()))
			.head_data(HeadData(vec![1]))
			.build();
		let child = Candidate::builder()
			.para(PARA_A)
			.relay_parent(w.leaf())
			.relay_parent_number(leaf_n)
			.parent_head(parent.output_head())
			.head_data(HeadData(vec![2]))
			.build();

		let peer = w.declared_peer(PARA_A, V2);
		w.full_second(&peer, &parent);
		w.full_second(&peer, &child);
	}
}

mod second_multiple_candidates_per_relay_parent {
	use crate::common::{
		builders::{Candidate, ProtocolVersion::V2},
		harness::CollatorSut,
		world::{activated_world, WorldExt as _},
	};
	use polkadot_primitives::{CoreIndex, HeadData, Id as ParaId};
	use std::time::Duration;

	const PARA: ParaId = ParaId::new(2000);

	#[crate::sim_test]
	fn three_chained_candidates_seconded_then_fourth_rejected<S: CollatorSut>() {
		let mut w = activated_world::<S>(&[(CoreIndex(0), PARA)]);
		let leaf = w.leaf();
		let leaf_n = w.leaf_number();

		// Chain of three candidates: parent_head=[i-1] → output_head=[i] for i in 1..=3.
		let chain: Vec<Candidate> = (1..=3u8)
			.map(|i| {
				let parent_head = if i == 1 { HeadData(Vec::new()) } else { HeadData(vec![i - 1]) };
				Candidate::builder()
					.para(PARA)
					.relay_parent(leaf)
					.relay_parent_number(leaf_n)
					.parent_head(parent_head)
					.head_data(HeadData(vec![i]))
					.build()
			})
			.collect();

		let peer = w.declared_peer(PARA, V2);
		for cand in &chain {
			w.full_second(&peer, cand);
		}

		// 4th candidate at the same RP — claim slots are full. No fetch should fire.
		let extra = Candidate::builder()
			.para(PARA)
			.relay_parent(leaf)
			.relay_parent_number(leaf_n)
			.parent_head(HeadData(vec![3]))
			.head_data(HeadData(vec![4]))
			.build();
		w.advertise_with_parent_head(&peer, leaf, extra.hash(), extra.parent_head_hash());
		w.no_fetch_for(&extra, Duration::from_millis(200));
	}
}

mod child_blocked_from_seconding_by_parent {
	use crate::common::{
		builders::{Candidate, ProtocolVersion::V2},
		harness::CollatorSut,
		world::{activated_world, WorldExt as _},
	};
	use polkadot_node_subsystem::messages::CollatorProtocolMessage;
	use polkadot_primitives::{CoreIndex, HeadData, Id as ParaId};
	use std::time::Duration;

	const PARA: ParaId = ParaId::new(2000);

	#[crate::sim_test]
	fn child_advertised_first_blocks_then_unblocks_after_parent_seconds<S: CollatorSut>() {
		let mut w = activated_world::<S>(&[(CoreIndex(0), PARA)]);
		let leaf = w.leaf();

		let parent = w
			.candidate_at(leaf)
			.para(PARA)
			.parent_head(HeadData(Vec::new()))
			.head_data(HeadData(vec![1]))
			.build();
		let child = w
			.candidate_at(leaf)
			.para(PARA)
			.parent_head(HeadData(vec![1]))
			.head_data(HeadData(vec![2]))
			.build();

		let peer = w.declared_peer(PARA, V2);
		w.outputs.insert(child.hash(), child.commitments.clone(), child.pvd.clone());

		// Child first → fetched but seconding held until parent enters fragment chain.
		w.advertise_with_parent_head(&peer, leaf, child.hash(), child.parent_head_hash());
		let child_req = w.fetch_request(&child);
		w.respond_fetch_v2(child_req, child.receipt.clone(), Candidate::empty_pov());

		// Parent flows through full advertise → fetch → second.
		w.full_second(&peer, &parent);

		// Child unblocks.
		w.expect_second(&child);
	}

	/// Upstream's `valid_parent=false` variant: parent A reported Invalid before being
	/// seconded → B never seconded.
	///
	/// In upstream, backing is mocked and the test explicitly chooses whether to dispatch
	/// `send_seconded_statement` or `Invalid`. With **real** backing, validation-stub's "valid"
	/// verdict auto-seconds parent and unblocks child before `Invalid` can fire. To preserve
	/// the upstream invariant, install `CanSecondStub(false)` so backing never auto-seconds.
	#[crate::sim_test]
	fn child_remains_blocked_when_parent_reported_invalid<S: CollatorSut>() {
		use crate::common::{
			chain::CoreSchedule,
			world::{bootstrap_world, collator_world_config, World, WorldExt as _},
		};
		let config =
			collator_world_config().with_schedule(CoreIndex(0), CoreSchedule::always(PARA));
		let mut w: World<S> = bootstrap_world::<S>(config, Some(false));
		w.new_block().activate();
		let leaf = w.leaf();

		let parent = w
			.candidate_at(leaf)
			.para(PARA)
			.parent_head(HeadData(Vec::new()))
			.head_data(HeadData(vec![1]))
			.build();
		let child = w
			.candidate_at(leaf)
			.para(PARA)
			.parent_head(HeadData(vec![1]))
			.head_data(HeadData(vec![2]))
			.build();

		let peer = w.declared_peer(PARA, V2);

		// Both ads. CanSecond stub answers false → both held without backing dispatch.
		w.advertise_with_parent_head(&peer, leaf, child.hash(), child.parent_head_hash());
		w.advertise_with_parent_head(&peer, leaf, parent.hash(), parent.parent_head_hash());

		// Drive Invalid signal for parent (upstream test's `valid_parent=false`).
		w.base
			.sim
			.send(CollatorProtocolMessage::Invalid(leaf, parent.receipt.clone().into()));

		// Parent never seconded → child never seconded.
		w.base.sim.expect_no(
		|e| matches!(
			e,
			crate::common::contract::Effect::SecondCandidate { candidate_hash, .. } if candidate_hash == &child.hash()
		),
		Duration::from_millis(200),
		"SecondCandidate for child after parent reported Invalid (must NOT fire)",
	);
	}
}

mod v1_full_seconding_with_back_notification {
	use crate::common::{
		builders::{Candidate, ProtocolVersion::V1},
		contract::{Effect, WireMsgKind},
		harness::CollatorSut,
		world::{activated_world, WorldExt as _},
	};
	use polkadot_primitives::{CoreIndex, HeadData, Id as ParaId};
	use std::time::Duration;

	const PARA: ParaId = ParaId::new(2000);

	#[crate::sim_test]
	fn v1_advertise_fetch_second_and_collator_notified<S: CollatorSut>() {
		let mut w = activated_world::<S>(&[(CoreIndex(0), PARA)]);
		let leaf = w.leaf();
		let candidate = w
			.candidate_at(leaf)
			.para(PARA)
			.parent_head(HeadData(Vec::new()))
			.head_data(HeadData(vec![1]))
			.build();
		// Register outputs so validation stub returns matching commitments — fragment chain
		// would reject otherwise (validated head_data ≠ descriptor.para_head).
		w.outputs
			.insert(candidate.hash(), candidate.commitments.clone(), candidate.pvd.clone());

		let peer = w.declared_peer(PARA, V1);
		w.base.sim.send(peer.advertise(leaf, None, None));
		let (_, request_id, _) = w.expect_any_fetch();
		w.respond_fetch_v1(request_id, candidate.receipt.clone(), Candidate::empty_pov());
		w.expect_second(&candidate);

		// Let the back-notification flow through statement-distribution-noop and back to
		// collator-protocol.
		w.base.sim.advance(Duration::from_millis(100));

		let _ = w.base.sim.expect(
			|e| {
				matches!(
					e,
					Effect::SendCollation {
						peers,
						kind: WireMsgKind::CollationSeconded { .. },
					} if peers.contains(&peer.peer_id),
				)
			},
			Duration::from_millis(500),
			"Effect::SendCollation CollationSeconded targeting the collator peer",
		);
	}
}
