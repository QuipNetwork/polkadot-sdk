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

//! V4 segment scenarios: output-head-keyed fetching and same-head redundancy.
//!
//! A stored segment is ONE fetch entitlement, consumed at launch. When two collators
//! advertise byte-identical segments claiming the same output head, the second
//! collator's parked segment is the designed retry channel for the first attempt
//! failing — whether the fetch itself fails or the fetched collation is invalidated.

mod same_head_retry {
	use crate::common::{
		aux::SharedInvalidSet,
		builders::{Candidate, Peer, ProtocolVersion::V4},
		chain::CoreSchedule,
		contract::Effect,
		harness::CollatorSut,
		world::{
			bootstrap_world, bootstrap_world_with_invalid_set, collator_world_config, World,
			WorldExt as _,
		},
	};
	use polkadot_node_network_protocol::v4_collation::CandidateFingerprint;
	use polkadot_primitives::{
		node_features::FeatureIndex, CandidateCommitments, CandidateHash, CandidateReceiptV2,
		CoreIndex, Hash, HeadData, Id as ParaId, MutateDescriptorV2, PersistedValidationData,
		RELAY_CHAIN_SLOT_DURATION_MILLIS,
	};
	use polkadot_primitives_test_helpers::dummy_committed_candidate_receipt_v3;
	use sp_consensus_slots::Slot;
	use std::time::Duration;

	const PARA: ParaId = ParaId::new(2000);

	/// Wall-clock duration after which the validator's `current_slot` equals `slot`
	/// (MockClock starts at 0 = slot 0).
	fn slot_to_wall_ms(slot: u64) -> Duration {
		Duration::from_millis(slot * RELAY_CHAIN_SLOT_DURATION_MILLIS)
	}

	/// World for V4 segment scenarios.
	///
	/// * Core 0 always schedules `PARA`, so the leaf CQ is `[PARA; 3]` — the multi-position shape
	///   that keeps the same-pass exhaustion window live.
	/// * Genesis at slot 0, one registered ancestor, then the activated leaf (`leaf.slot = 2`); the
	///   wall clock is advanced to slot 3 so the leaf's slot is finished and the leaf itself is a
	///   valid V3/V4 scheduling parent.
	/// * Both candidate-receipt feature bits are on, and the ancestor is FINALIZED: backing's
	///   V3-descriptor acceptance is gated on `CandidateReceiptV3` being observed in a *finalized*
	///   block's session (monotonic `v3_ever_seen`), unlike the per-leaf `CandidateReceiptV2` bit.
	fn v4_world<S: CollatorSut>(invalid: Option<SharedInvalidSet>) -> World<S> {
		let config = collator_world_config()
			.with_schedule(CoreIndex(0), CoreSchedule::always(PARA))
			.with_genesis_slot(Slot::from(0))
			.with_v3_descriptors_enabled()
			.with_node_feature(FeatureIndex::CandidateReceiptV3);
		let mut w: World<S> = match invalid {
			Some(set) => bootstrap_world_with_invalid_set::<S>(config, None, set),
			None => bootstrap_world::<S>(config, None),
		};
		w.new_block().register();
		w.new_block().activate();
		let ancestor = w.ancestors()[0];
		w.finalize(ancestor);
		w.base.sim.advance(slot_to_wall_ms(3));
		w
	}

	/// A V4-fetchable candidate with a V3 descriptor, plus everything a scenario needs
	/// around it: consistent commitments (non-empty output head — an empty one would
	/// equal the empty parent head and make the fingerprint a zero-length cycle, which
	/// the wire handler rejects), the PVD real prospective will reproduce (empty
	/// parent head = the chain's default required parent), and the wire fingerprint.
	struct V4Candidate {
		receipt: CandidateReceiptV2,
		commitments: CandidateCommitments,
		pvd: PersistedValidationData,
		fingerprint: CandidateFingerprint,
	}

	fn v4_candidate<S: CollatorSut>(
		w: &World<S>,
		relay_parent: Hash,
		scheduling_parent: Hash,
	) -> V4Candidate {
		let parent_head = HeadData(Vec::new());
		let head_data = HeadData(vec![0xaa]);
		let pvd = PersistedValidationData {
			parent_head: parent_head.clone(),
			relay_parent_number: w.base.chain.lock().block(&relay_parent).unwrap().number,
			relay_parent_storage_root: Hash::zero(),
			max_pov_size: 5 * 1024 * 1024,
		};
		let commitments = CandidateCommitments {
			head_data: head_data.clone(),
			horizontal_messages: Default::default(),
			upward_messages: Default::default(),
			new_validation_code: None,
			processed_downward_messages: 0,
			hrmp_watermark: 0,
		};
		let mut committed = dummy_committed_candidate_receipt_v3(relay_parent, scheduling_parent);
		committed.commitments = commitments.clone();
		committed.descriptor.set_para_id(PARA);
		committed.descriptor.set_persisted_validation_data_hash(pvd.hash());
		committed.descriptor.set_core_index(CoreIndex(0));
		committed.descriptor.set_session_index(0);
		committed.descriptor.set_version(1);
		committed.descriptor.set_para_head(head_data.hash());
		let receipt = committed.to_plain();
		let fingerprint = CandidateFingerprint {
			output_head_data_hash: head_data.hash(),
			parent_head_data_hash: parent_head.hash(),
			claim_queue_offset: 0,
		};
		V4Candidate { receipt, commitments, pvd, fingerprint }
	}

	/// Wait for `Effect::SecondCandidate` carrying `hash`.
	fn expect_second_of<S: CollatorSut>(
		w: &mut World<S>,
		hash: CandidateHash,
		context: &'static str,
	) {
		let _ = w.base.sim.expect(
			|e| matches!(e, Effect::SecondCandidate { candidate_hash, .. } if *candidate_hash == hash),
			Duration::from_millis(500),
			context,
		);
	}

	/// Two V4 peers advertising the byte-identical single-entry segment `[fp]` at
	/// `scheduling_parent`, 5ms apart. Equal-score rank ties break on arrival time, so
	/// the first advertiser's segment is the deterministic first pick; 5ms stays well
	/// inside the current 6s slot (V3 scheduling-parent validity is checked per ad).
	/// V4 has no `Declare` — a peer's first segment binds it to the para.
	fn two_peers_same_segment<S: CollatorSut>(
		w: &mut World<S>,
		scheduling_parent: Hash,
		fp: &CandidateFingerprint,
	) -> (Peer, Peer) {
		let peer_a = w.connected_peer(PARA, V4);
		let peer_b = w.connected_peer(PARA, V4);
		w.advertise_segment(&peer_a, scheduling_parent, vec![fp.clone()]);
		w.base.sim.advance(Duration::from_millis(5));
		w.advertise_segment(&peer_b, scheduling_parent, vec![fp.clone()]);
		(peer_a, peer_b)
	}

	/// Baseline (green): a single V4 segment is fetched over the output-head-keyed V3
	/// request-response protocol and the candidate reaches backing. Validates the whole
	/// fixture + candidate + response tail the two red retry scenarios below depend on,
	/// so their failures can only mean one thing: the missing retry.
	#[crate::sim_test(only = "experimental")]
	fn v4_segment_fetched_and_seconded_end_to_end<S: CollatorSut>() {
		let mut w = v4_world::<S>(None);
		let leaf = w.leaf();
		let c = v4_candidate(&w, leaf, leaf);
		w.outputs.insert(c.receipt.hash(), c.commitments.clone(), c.pvd.clone());

		let peer = w.connected_peer(PARA, V4);
		w.advertise_segment(&peer, leaf, vec![c.fingerprint.clone()]);

		let (to, request_id, head) = w.expect_any_fetch_v3();
		assert_eq!(to, peer.peer_id);
		assert_eq!(head, c.fingerprint.output_head_data_hash);

		w.respond_fetch_v3(
			request_id,
			c.receipt.clone(),
			Candidate::empty_pov(),
			c.pvd.parent_head.clone(),
		);
		expect_second_of(&mut w, c.receipt.hash(), "V4-fetched candidate dispatched to backing");
		// Let the downstream pipeline (validation, import, statement, Seconded ack)
		// flush so a broken tail panics here rather than being silently dropped.
		w.base.sim.advance(Duration::from_millis(200));
	}

	/// Two collators advertise the same single-entry segment for output head A; the
	/// first fetch FAILS at the network level. The second collator's parked segment is
	/// the retry channel: a fetch of A to the other peer must fire, and the candidate
	/// seconds.
	#[crate::sim_test(only = "experimental")]
	fn same_head_fetch_failure_retries_from_second_collator<S: CollatorSut>() {
		let mut w = v4_world::<S>(None);
		let leaf = w.leaf();
		let c = v4_candidate(&w, leaf, leaf);
		w.outputs.insert(c.receipt.hash(), c.commitments.clone(), c.pvd.clone());

		let (peer_a, peer_b) = two_peers_same_segment(&mut w, leaf, &c.fingerprint);

		let (first, request_id, head) = w.expect_any_fetch_v3();
		assert_eq!(head, c.fingerprint.output_head_data_hash);
		let second = if first == peer_a.peer_id { peer_b.peer_id } else { peer_a.peer_id };

		// The same output head must not be double-launched while the fetch is pending:
		// exactly one fetch of A in flight, the redundant segment waits.
		w.no_fetch_v3_within(Duration::from_millis(100));

		w.fail_fetch(request_id);

		let retry_id = w.expect_fetch_v3_to(second);
		w.respond_fetch_v3(
			retry_id,
			c.receipt.clone(),
			Candidate::empty_pov(),
			c.pvd.parent_head.clone(),
		);
		expect_second_of(&mut w, c.receipt.hash(), "retried candidate dispatched to backing");
		w.base.sim.advance(Duration::from_millis(200));
	}

	/// Two collators advertise the same single-entry segment for output head A; the
	/// first fetch SUCCEEDS but the collation is a bad twin (same output head,
	/// different candidate) that real validation rejects — backing reports it invalid
	/// and the slot is released. The second collator's parked segment must then retry
	/// A, and the good twin seconds.
	#[crate::sim_test(only = "experimental")]
	fn same_head_invalid_collation_retries_from_second_collator<S: CollatorSut>() {
		let invalid = SharedInvalidSet::default();
		let mut w = v4_world::<S>(Some(invalid.clone()));
		let leaf = w.leaf();
		let good = v4_candidate(&w, leaf, leaf);
		w.outputs
			.insert(good.receipt.hash(), good.commitments.clone(), good.pvd.clone());

		// The bad twin claims the SAME output head (it matches the advertised
		// fingerprint) but is a different candidate — validation rejects it.
		let bad_receipt = {
			let mut receipt = good.receipt.clone();
			receipt.descriptor.set_pov_hash(Hash::repeat_byte(0x66));
			receipt
		};
		invalid.insert(bad_receipt.hash());

		let (peer_a, peer_b) = two_peers_same_segment(&mut w, leaf, &good.fingerprint);

		let (first, request_id, head) = w.expect_any_fetch_v3();
		assert_eq!(head, good.fingerprint.output_head_data_hash);
		let second = if first == peer_a.peer_id { peer_b.peer_id } else { peer_a.peer_id };

		// The winning peer serves the bad twin. Dispatch to backing is optimistic —
		// `SecondCandidate` fires for the bad twin BEFORE validation runs; real
		// backing then invalidates it via the verdict stub and reports
		// `CollatorProtocolMessage::Invalid`, which releases the slot.
		w.respond_fetch_v3(
			request_id,
			bad_receipt.clone(),
			Candidate::empty_pov(),
			good.pvd.parent_head.clone(),
		);
		expect_second_of(&mut w, bad_receipt.hash(), "bad twin optimistically dispatched");

		let retry_id = w.expect_fetch_v3_to(second);
		w.respond_fetch_v3(
			retry_id,
			good.receipt.clone(),
			Candidate::empty_pov(),
			good.pvd.parent_head.clone(),
		);
		expect_second_of(&mut w, good.receipt.hash(), "good twin seconded after the invalid one");
		w.base.sim.advance(Duration::from_millis(200));
	}
}
