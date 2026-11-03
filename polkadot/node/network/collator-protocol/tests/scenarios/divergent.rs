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

//! Legacy vs experimental divergences.

mod no_time_based_eviction {
	use crate::common::{
		builders::{Peer, ProtocolVersion::V1},
		contract::Effect,
		harness::CollatorSut,
		world::{activated_world, World, WorldExt as _},
	};
	use polkadot_collator_protocol::CollatorEvictionPolicy;
	use polkadot_node_network_protocol::peer_set::PeerSet;
	use polkadot_primitives::{CoreIndex, Id as ParaId};
	use std::time::Duration;

	const PARA: ParaId = ParaId::new(2000);

	// ---------------------------------------------------------------------------
	// Scenario 1: connected-but-undeclared peer
	// ---------------------------------------------------------------------------

	fn setup_undeclared<S: CollatorSut>() -> (World<S>, Peer) {
		let mut w = activated_world::<S>(&[(CoreIndex(0), PARA)]);
		let peer = w.connected_peer(PARA, V1);
		(w, peer)
	}

	#[crate::sim_test(only = "legacy")]
	fn undeclared_peer_disconnected_after_window<S: CollatorSut>() {
		let (mut w, peer) = setup_undeclared::<S>();
		w.base
			.sim
			.advance(CollatorEvictionPolicy::default().undeclared + Duration::from_millis(500));
		w.expect_disconnect(&peer);
	}

	#[crate::sim_test(only = "experimental")]
	fn undeclared_peer_kept_indefinitely<S: CollatorSut>() {
		let (mut w, peer) = setup_undeclared::<S>();
		// Advance the same distance the legacy variant uses; experimental must not evict.
		let dur = CollatorEvictionPolicy::default().undeclared + Duration::from_millis(500);
		w.expect_no_disconnect(&peer, dur);
	}

	// ---------------------------------------------------------------------------
	// Scenario 2: declared-but-inactive peer
	// ---------------------------------------------------------------------------

	fn setup_inactive<S: CollatorSut>() -> (World<S>, Peer) {
		let mut w = activated_world::<S>(&[(CoreIndex(0), PARA)]);
		let peer = w.declared_peer(PARA, V1);
		(w, peer)
	}

	#[crate::sim_test(only = "legacy")]
	fn declared_but_inactive_peer_evicted_after_window<S: CollatorSut>() {
		let (mut w, peer) = setup_inactive::<S>();
		w.base
			.sim
			.advance(CollatorEvictionPolicy::default().inactive_collator + Duration::from_secs(1));
		w.expect_disconnect(&peer);
	}

	#[crate::sim_test(only = "experimental")]
	fn declared_but_inactive_peer_kept_indefinitely<S: CollatorSut>() {
		let (mut w, peer) = setup_inactive::<S>();
		let dur = CollatorEvictionPolicy::default().inactive_collator + Duration::from_secs(1);
		w.expect_no_disconnect(&peer, dur);
	}

	// ---------------------------------------------------------------------------
	// Scenario 3: activity extends life (legacy); irrelevant on experimental
	// ---------------------------------------------------------------------------

	/// On legacy this asserts the activity-resets-timer behaviour: a peer that keeps
	/// advertising at sub-window intervals stays connected; once it falls silent, the
	/// inactive-collator window kicks in and it gets evicted.
	///
	/// On experimental there is no inactive-collator window at all (the entire concept is
	/// gone), so the "fall silent → eviction" tail has no analogue. Tested via the simpler
	/// "declared-but-inactive peer kept indefinitely" above. We document the asymmetry
	/// here rather than write a vacuous experimental variant.
	#[crate::sim_test(only = "legacy")]
	fn activity_extends_life_then_silence_evicts<S: CollatorSut>() {
		use crate::common::{
			chain::CoreSchedule,
			world::{bootstrap_world, collator_world_config},
		};

		// V1 advertisements must reference an *active leaf* (legacy explicitly rejects
		// non-leaf RPs as `ProtocolMisuse`). The original test built a linear chain of
		// three blocks and treated each as an active leaf — production `block_imported`
		// semantics no longer permit that (each child activation deactivates its parent).
		// Three sibling forks of a common non-leaf ancestor preserve the "three coexisting
		// active leaves" intent and let V1 advertisements at all three RPs land.
		let config =
			collator_world_config().with_schedule(CoreIndex(0), CoreSchedule::always(PARA));
		let mut w: World<S> = bootstrap_world::<S>(config, None);
		let common = w.new_block().register();
		let leaf_a = w.new_block().from_parent(common.hash).activate();
		let leaf_b = w.new_block().from_parent(common.hash).activate();
		let leaf_c = w.new_block().from_parent(common.hash).activate();

		let peer = w.declared_peer(PARA, V1);
		let rps = [leaf_a.hash, leaf_b.hash, leaf_c.hash];

		let inactive = CollatorEvictionPolicy::default().inactive_collator;
		let step = inactive * 2 / 3;
		for i in 0..3 {
			w.base.sim.advance(step);
			w.base.sim.send(peer.advertise(rps[i], None, None));
		}

		// After ~2× the window of continuous activity, peer must still be connected.
		w.base.sim.expect_count(
		|e| matches!(
			e,
			Effect::DisconnectPeers { peers, peer_set: PeerSet::Collation } if peers.contains(&peer.peer_id),
		),
		0,
		"DisconnectPeers targeting the actively-advertising peer (must be zero so far)",
	);

		// Fall silent — advance well past the window; peer must be disconnected.
		w.base.sim.advance(inactive + Duration::from_secs(1));
		w.expect_disconnect(&peer);
	}
}

mod reputation_priority {
	use crate::common::{
		builders::{Peer, ProtocolVersion::V2},
		harness::CollatorSut,
		world::{activated_world, World, WorldExt as _},
	};
	use polkadot_node_subsystem::OverseerSignal;
	use polkadot_primitives::{
		CandidateEvent, CoreIndex, GroupIndex, Hash, HeadData, Id as ParaId,
	};
	use std::time::Duration;

	const PARA: ParaId = ParaId::new(2000);

	/// Reputation arbitrates among carriers of the same candidate that pile up while the single
	/// fetch slot is **busy** — independently of the fetch *delay*.
	///
	/// The experimental validator postpones a 0-rep peer's fetch by a fixed delay today, but that
	/// delay is being removed (#12004 / the bounded-parallel-fetch design #11023), so we
	/// deliberately do not rely on it. The mechanism this test uses instead is the
	/// one-fetch-in-flight slot: while a fetch occupies the single claim-queue slot, further
	/// advertisements queue, and when the slot frees the validator picks the best remaining
	/// carrier by reputation. Every peer here has reputation ≥ 1, so the delay path is never taken
	/// — the prioritisation comes purely from the busy pipeline and the re-fetch-on-failure path,
	/// which keeps the test valid before and after #12004.
	///
	/// Layout: a length-1 claim queue (one fetchable slot). All three peers advertise the *same*
	/// candidate. A throwaway "first carrier" advertises first and is fetched immediately,
	/// occupying the slot; meanwhile A and B queue as co-carriers of that same candidate. The
	/// first carrier's fetch then fails with undecodable bytes (`FAILED_FETCH_SLASH` — it is a
	/// throwaway whose reputation we don't care about), which frees the slot *and* leaves the
	/// candidate un-fetched, so the validator re-fetches it from the best remaining carrier by
	/// reputation.
	///
	/// Round 1 (A=2, B=1): A wins the re-fetch. We then slash A to 0. Round 2 (A=0, B=1): B wins.
	#[crate::sim_test(only = "experimental")]
	fn busy_pipeline_arbitrates_by_reputation_then_slash_demotes<S: CollatorSut>() {
		let mut w = activated_world::<S>(&[(CoreIndex(0), PARA)]);

		fn earn_one_inclusion<S: CollatorSut>(w: &mut World<S>, peer: &Peer, leaf: Hash) {
			let cand = w
				.candidate_at(leaf)
				.para(PARA)
				.head_data(HeadData(vec![leaf.as_bytes()[0]]))
				.approved_peer(peer.peer_id)
				.build();
			{
				let mut chain = w.base.chain.lock();
				chain.set_pending_availability(PARA, vec![cand.committed()]);
				chain.set_candidate_events(
					leaf,
					vec![CandidateEvent::CandidateIncluded(
						cand.receipt.clone(),
						cand.commitments.head_data.clone(),
						CoreIndex(0),
						GroupIndex(0),
					)],
				);
				chain.set_finalized(leaf);
			}
			w.base.sim.signal(OverseerSignal::BlockFinalized(leaf, w.leaf_number()));
			w.base.sim.advance(Duration::from_millis(50));
		}

		// One contention round on a fresh, single-slot leaf. All three peers carry the *same*
		// candidate:
		//   1. `first_carrier` advertises first → fetched immediately, occupying the only slot;
		//   2. `held` (lower rep) and `winner` (higher rep) advertise the same candidate and queue;
		//   3. fail the first carrier's fetch (undecodable bytes) → the slot frees and the
		//      candidate is re-fetched from the best remaining carrier by reputation → `winner`.
		// Returns `winner`'s `RequestId` so the caller can resolve it (e.g. fail it to drive a
		// slash).
		fn contended_round<S: CollatorSut>(
			w: &mut World<S>,
			parent: Hash,
			head: u8,
			first_carrier: &Peer,
			held: &Peer,
			winner: &Peer,
		) -> crate::common::contract::RequestId {
			// Length-1 claim queue → a single fetchable slot, so one in-flight fetch blocks the
			// rest.
			let leaf = w
				.new_block()
				.from_parent(parent)
				.with_claim_queue_at(CoreIndex(0), [PARA])
				.activate()
				.hash;

			let cand = w
				.candidate_at(leaf)
				.para(PARA)
				.parent_head(HeadData(vec![head, 0]))
				.head_data(HeadData(vec![head, 1]))
				.build();
			w.outputs.insert(cand.hash(), cand.commitments.clone(), cand.pvd.clone());

			// First carrier advertises and is fetched immediately, occupying the slot; we don't
			// answer it yet.
			w.advertise_with_parent_head(first_carrier, leaf, cand.hash(), cand.parent_head_hash());
			let first_id = w.expect_fetch_to(first_carrier.peer_id);

			// With the slot busy, the other two carriers of the same candidate advertise and queue.
			w.advertise_with_parent_head(held, leaf, cand.hash(), cand.parent_head_hash());
			w.advertise_with_parent_head(winner, leaf, cand.hash(), cand.parent_head_hash());

			// While the slot is busy, no further fetch for the candidate may fire.
			w.no_fetch_for(&cand, Duration::from_millis(1000));

			// Fail the first carrier → the slot frees and the candidate is re-fetched from the best
			// remaining carrier by reputation.
			let barrier = w.recorder_barrier();
			w.respond_fetch_invalid(first_id);

			let req_id = w.expect_fetch_to(winner.peer_id);
			let (first, _) = w
				.first_fetch_after(barrier)
				.expect("the candidate must be re-fetched once the slot frees");
			assert_eq!(first, winner.peer_id, "the higher-rep carrier must win the re-fetch");
			req_id
		}

		let leaf0 = w.leaf();
		// A distinct throwaway first-carrier per round, so the per-round
		// `expect_fetch_to(first_carrier)` can never match the previous round's (still-recorded)
		// fetch.
		let carrier1 = w.declared_peer(PARA, V2);
		let carrier2 = w.declared_peer(PARA, V2);
		let peer_a = w.declared_peer(PARA, V2);
		let peer_b = w.declared_peer(PARA, V2);

		// Reputations: A = 2, B = 1, each carrier = 1 (so the carrier also fetches immediately).
		earn_one_inclusion(&mut w, &peer_a, leaf0);
		let leaf1 = w.new_block().from_parent(leaf0).activate().hash;
		earn_one_inclusion(&mut w, &peer_a, leaf1);
		let leaf2 = w.new_block().from_parent(leaf1).activate().hash;
		earn_one_inclusion(&mut w, &peer_b, leaf2);
		let leaf3 = w.new_block().from_parent(leaf2).activate().hash;
		earn_one_inclusion(&mut w, &carrier1, leaf3);
		let leaf4 = w.new_block().from_parent(leaf3).activate().hash;
		earn_one_inclusion(&mut w, &carrier2, leaf4);

		// Round 1: A (2) beats B (1) for the re-fetch.
		let fetch_id = contended_round(&mut w, leaf4, 1, &carrier1, &peer_b, &peer_a);

		// Fail A's now-in-flight fetch → `FAILED_FETCH_SLASH` saturates A's 2 to 0.
		w.respond_fetch_invalid(fetch_id);
		w.base.sim.advance(Duration::from_millis(50));

		// Round 2: A is now 0-rep, B keeps its 1 → B wins the re-fetch.
		let _ = contended_round(&mut w, leaf4, 2, &carrier2, &peer_a, &peer_b);
	}
}

mod reputation_emission {
	use crate::common::{
		builders::{Candidate, Peer, ProtocolVersion::V2},
		contract::{Effect, RepBucket},
		harness::CollatorSut,
		world::{activated_world, WorldExt as _},
	};
	use polkadot_node_subsystem_util::reputation::REPUTATION_CHANGE_INTERVAL;
	use polkadot_primitives::{
		CandidateHash, CandidateReceiptV2, CoreIndex, Hash, HeadData, Id as ParaId,
		MutateDescriptorV2, PersistedValidationData,
	};
	use polkadot_primitives_test_helpers::dummy_committed_candidate_receipt_v2;
	use std::time::Duration;

	const PARA_A: ParaId = ParaId::new(2000);
	const WRONG: ParaId = ParaId::new(69);

	fn empty_parent_pvd(relay_parent_number: u32) -> PersistedValidationData {
		PersistedValidationData {
			parent_head: HeadData(Vec::new()),
			relay_parent_number,
			relay_parent_storage_root: Hash::zero(),
			max_pov_size: 5 * 1024 * 1024,
		}
	}

	// ---------------------------------------------------------------------------
	// Scenario 1: response with mismatched candidate hash → Malicious on legacy
	// ---------------------------------------------------------------------------

	/// Shared setup for the mismatched-candidate-hash scenario. Returns the world and the
	/// peer for impl-specific assertion. The actual *spec* (no `SecondCandidate` for the bad
	/// candidate) is asserted by `response_sanity_check::response_with_mismatched_candidate
	/// _hash_rejects` in the regular regression suite.
	fn setup_mismatched_hash<S: CollatorSut>() -> (crate::common::world::World<S>, Peer) {
		let mut w = activated_world::<S>(&[(CoreIndex(0), PARA_A)]);
		let pvd = empty_parent_pvd(w.leaf_number());
		let mut actual = Candidate::for_para_at(PARA_A, w.leaf());
		actual.receipt.descriptor.set_persisted_validation_data_hash(pvd.hash());

		let peer = w.declared_peer(PARA_A, V2);
		let advertised_hash = CandidateHash(Hash::repeat_byte(0xFE));
		w.advertise_with_parent_head(&peer, w.leaf(), advertised_hash, HeadData(Vec::new()).hash());
		let request_id = w.expect_fetch_for_hash(advertised_hash);
		w.respond_fetch_v2(request_id, actual.receipt.clone(), Candidate::empty_pov());
		(w, peer)
	}

	#[crate::sim_test(only = "legacy")]
	fn mismatched_hash_emits_malicious_bus_event<S: CollatorSut>() {
		let (mut w, peer) = setup_mismatched_hash::<S>();
		w.expect_rep(&peer, RepBucket::Malicious);
	}

	#[crate::sim_test(only = "experimental")]
	fn mismatched_hash_no_bus_event<S: CollatorSut>() {
		let (mut w, peer) = setup_mismatched_hash::<S>();
		// Experimental does not emit a bus event; the rep store is updated silently.
		w.expect_no_rep(&peer, RepBucket::Malicious);
	}

	// ---------------------------------------------------------------------------
	// Scenario 2: declare twice for unneeded para → batched Performance on legacy
	// ---------------------------------------------------------------------------

	/// On legacy this exercises the `ReputationAggregator` batching: two `COST_UNNEEDED
	/// _COLLATOR` (CostMinor) hits are buffered, then flushed as one Batch after
	/// `REPUTATION_CHANGE_INTERVAL`. Experimental has no equivalent code path —
	/// `COST_UNNEEDED_COLLATOR` doesn't exist on experimental at all (see comment at
	/// `validator_side_experimental/state.rs:261-262`); the peer is just disconnected on
	/// the wrong-para Declare. Different mechanism entirely; same observable outcome
	/// (peer doesn't get to keep advertising).
	fn setup_declare_twice_unneeded<S: CollatorSut>() -> (crate::common::world::World<S>, Peer) {
		let mut w = activated_world::<S>(&[(CoreIndex(0), PARA_A)]);
		let peer = Peer::new(WRONG, V2);
		w.base.sim.send(peer.connected());
		w.base.sim.send(peer.declare());
		w.base.sim.send(peer.declare());
		(w, peer)
	}

	#[crate::sim_test(only = "legacy")]
	fn declare_twice_unneeded_emits_one_batched_rep<S: CollatorSut>() {
		let (mut w, peer) = setup_declare_twice_unneeded::<S>();

		// Buffered until the flush.
		w.base.sim.expect_count(
			|e| {
				matches!(
					e,
					Effect::Reputation { peer: p, bucket: RepBucket::Performance } if *p == peer.peer_id,
				)
			},
			0,
			"no Reputation::Performance before the aggregator flushes",
		);

		// Advance past the flush interval; aggregator dispatches one Batch.
		w.base.sim.advance(REPUTATION_CHANGE_INTERVAL + Duration::from_secs(1));
		w.base.sim.expect_count(
			|e| {
				matches!(
					e,
					Effect::Reputation { peer: p, bucket: RepBucket::Performance } if *p == peer.peer_id,
				)
			},
			1,
			"exactly one batched Reputation::Performance for the unneeded-para peer",
		);
	}

	#[crate::sim_test(only = "experimental")]
	fn declare_twice_unneeded_no_rep_event<S: CollatorSut>() {
		let (mut w, peer) = setup_declare_twice_unneeded::<S>();

		// No rep event ever fires on experimental for "wrong para" misbehaviour — that's
		// just disconnect-without-slash on this side. Advance well past any flush window
		// the legacy side would have used and confirm silence.
		w.base.sim.advance(REPUTATION_CHANGE_INTERVAL + Duration::from_secs(1));
		w.base.sim.expect_count(
			|e| {
				matches!(
					e,
					Effect::Reputation { peer: p, .. } if *p == peer.peer_id,
				)
			},
			0,
			"experimental does not slash on `wrong para Declare`; rep is bus-silent",
		);
	}

	// ---------------------------------------------------------------------------
	// Scenario 3: V2 candidate with wrong session_index → Malicious on legacy
	// ---------------------------------------------------------------------------

	/// Shared setup. Spec is "candidate rejected" (asserted in
	/// `v3_session_index_checks::v2_descriptor_with_wrong_session_index_rejects`).
	fn setup_wrong_session_index<S: CollatorSut>() -> (crate::common::world::World<S>, Peer) {
		let mut w = activated_world::<S>(&[(CoreIndex(0), PARA_A)]);
		let pvd = empty_parent_pvd(w.leaf_number());
		let mut committed = dummy_committed_candidate_receipt_v2(w.leaf());
		committed.descriptor.set_para_id(PARA_A);
		committed.descriptor.set_persisted_validation_data_hash(pvd.hash());
		committed.descriptor.set_core_index(CoreIndex(0));
		committed.descriptor.set_session_index(999);
		let receipt: CandidateReceiptV2 = committed.to_plain();
		let candidate = Candidate::from_receipt(receipt.clone());

		let peer = w.declared_peer(PARA_A, V2);
		w.advertise_with_parent_head(
			&peer,
			w.leaf(),
			candidate.hash(),
			HeadData(Vec::new()).hash(),
		);
		let request_id = w.fetch_request(&candidate);
		w.respond_fetch_v2(request_id, receipt, Candidate::empty_pov());
		(w, peer)
	}

	#[crate::sim_test(only = "legacy")]
	fn wrong_session_index_emits_malicious_bus_event<S: CollatorSut>() {
		let (mut w, peer) = setup_wrong_session_index::<S>();
		w.expect_rep(&peer, RepBucket::Malicious);
	}

	#[crate::sim_test(only = "experimental")]
	fn wrong_session_index_no_bus_event<S: CollatorSut>() {
		let (mut w, peer) = setup_wrong_session_index::<S>();
		w.expect_no_rep(&peer, RepBucket::Malicious);
	}
}

/// Per-relay-parent group rotation: the validator's owned core rotates per block, so core
/// ownership (and thus which para is fetchable) must be evaluated against each relay
/// parent's own rotation, not the active leaf's.
mod group_rotation {
	use crate::common::{
		builders::ProtocolVersion::V2,
		chain::CoreSchedule,
		harness::CollatorSut,
		world::{bootstrap_world, collator_world_config, World, WorldExt as _},
	};
	use polkadot_primitives::{CoreIndex, Id as ParaId, ValidatorIndex};

	const PARA_A: ParaId = ParaId::new(100);
	const PARA_B: ParaId = ParaId::new(600);

	/// Group rotation: our group's owned core rotates per block. With 3 cores,
	/// `group_rotation_frequency = 1`, group 0, and `core_for_group(0, 3)` at `now = N`
	/// (= block_number + 1) returning `(3 - N % 3) % 3`:
	/// - genesis (block 0, now 1): own core 2 → PARA_A
	/// - block 1 (now 2):          own core 1 → PARA_B (the active leaf)
	///
	/// The active leaf (block 1) holds genesis in its implicit view. PARA_B is advertised at
	/// the leaf (core 1) and PARA_A at the genesis ancestor (core 2) — the rotation must not
	/// orphan the ancestor's CQ slot, and a re-advertisement there must still fetch.
	#[crate::sim_test]
	fn core_rotation_accepts_candidates_for_both_cores<S: CollatorSut>() {
		let validator_groups =
			vec![vec![ValidatorIndex(0)], vec![ValidatorIndex(1)], vec![ValidatorIndex(2)]];
		let config = collator_world_config()
			.with_schedule(CoreIndex(2), CoreSchedule::always(PARA_A))
			.with_schedule(CoreIndex(1), CoreSchedule::always(PARA_B))
			.with_validator_groups(validator_groups)
			.with_group_rotation_frequency(1);
		let mut w: World<S> = bootstrap_world::<S>(config, None);
		w.new_block().activate();

		// Block 1 is the active leaf (own core 1 → PARA_B); genesis (own core 2 → PARA_A)
		// stays in its implicit view.
		let block_para_b = w.leaf(); // block 1 — own core 1 → PARA_B (the active leaf)
		let block_para_a = w.ancestors()[0]; // genesis — own core 2 → PARA_A

		let peer_a = w.declared_peer(PARA_A, V2);
		let cand_a = w.advertise(&peer_a, block_para_a, PARA_A);
		let _ = w.fetch_request(&cand_a);

		let peer_b = w.declared_peer(PARA_B, V2);
		let cand_b = w.advertise(&peer_b, block_para_b, PARA_B);
		let _ = w.fetch_request(&cand_b);

		// A second PARA_A advertisement at the genesis ancestor must still fetch — the
		// rotation's owned-core shift across blocks must not have orphaned that CQ slot.
		let peer_a2 = w.declared_peer(PARA_A, V2);
		let cand_a2 = w.advertise(&peer_a2, block_para_a, PARA_A);
		let _ = w.fetch_request(&cand_a2);
	}

	/// Per-core slot accounting: under group rotation, peer_old declares PARA_X and
	/// advertises at leaf_1 (we own core 2). After rotation we own core 1. peer_new
	/// advertises PARA_X at leaf_2 (core 1). Both cores carry exactly one PARA_X slot —
	/// per-core capacity must not be shared, so both fetch.
	///
	/// PARA_X is scheduled on cores 1 and 2; our group owns core 2 at genesis and core 1 at
	/// block 1 (per-RP rotation, `now = block_number + 1`). Both RPs sit in the active leaf's
	/// implicit view. An advertisement at each must fetch — the two cores carry independent
	/// PARA_X slots.
	#[crate::sim_test]
	fn cross_core_reservation_does_not_consume_other_cores_slots<S: CollatorSut>() {
		const PARA_X_LOCAL: ParaId = ParaId::new(100);
		const PARA_FILLER: ParaId = ParaId::new(600);
		let validator_groups =
			vec![vec![ValidatorIndex(0)], vec![ValidatorIndex(1)], vec![ValidatorIndex(2)]];
		let config = collator_world_config()
			.with_schedule(CoreIndex(1), CoreSchedule::always(PARA_X_LOCAL))
			.with_schedule(CoreIndex(2), CoreSchedule::always(PARA_X_LOCAL))
			.with_schedule(CoreIndex(0), CoreSchedule::always(PARA_FILLER))
			.with_validator_groups(validator_groups)
			.with_group_rotation_frequency(1);
		let mut w: World<S> = bootstrap_world::<S>(config, None);
		w.new_block().activate();
		// Block 1 is the active leaf (own core 1 → PARA_X); genesis (own core 2 → PARA_X)
		// stays in its implicit view. Both RPs carry PARA_X on a core we own, on different
		// cores — so the two slots are independent and both advertisements must fetch.
		let block_core1 = w.leaf(); // block 1 — own core 1 → PARA_X (the active leaf)
		let block_core2 = w.ancestors()[0]; // genesis — own core 2 → PARA_X

		let peer_old = w.declared_peer(PARA_X_LOCAL, V2);
		let cand_old = w.advertise(&peer_old, block_core2, PARA_X_LOCAL);
		let peer_new = w.declared_peer(PARA_X_LOCAL, V2);
		let cand_new = w.advertise(&peer_new, block_core1, PARA_X_LOCAL);

		let _ = w.fetch_request(&cand_old);
		let _ = w.fetch_request(&cand_new);
	}
}

/// Claim-queue capacity accounting: how many fetches a (scheduling-parent, para) is allowed
/// across the active leaves' implicit views — including ancestor relay parents, short claim
/// queues, and shared ancestors of sibling forks. The window length is bounded by the runtime
/// scheduling lookahead, and a shared ancestor's capacity is one bucket, not one-per-fork.
mod claim_queue_capacity {
	use crate::common::{
		builders::ProtocolVersion::V2,
		chain::CoreSchedule,
		contract::Effect,
		harness::CollatorSut,
		world::{bootstrap_world, collator_world_config, World, WorldExt as _},
	};
	use polkadot_primitives::{CoreIndex, HeadData, Id as ParaId};
	use std::time::Duration;

	const PARA_A: ParaId = ParaId::new(100);

	/// 3 peers advertise PARA_A at 3 different SPs on a linear path. Leaf CQ has 2 slots
	/// for PARA_A → exactly 2 fetches. >2 = over-fetch (third candidate has nowhere to
	/// land); <2 = under-fetch (a wide-window candidate stole a slot).
	#[crate::sim_test]
	fn linear_multi_sp_same_para_capacity_not_double_counted<S: CollatorSut>() {
		let config =
			collator_world_config().with_schedule(CoreIndex(0), CoreSchedule::always(PARA_A));
		let mut w: World<S> = bootstrap_world::<S>(config, None);
		for _ in 0..2 {
			w.new_block().register();
		}
		w.new_block()
			.with_claim_queue_at(CoreIndex(0), [PARA_A, ParaId::new(200), PARA_A])
			.activate();
		let leaf = w.leaf();
		let parent = w.ancestors()[0];
		let grandparent = w.ancestors()[1];

		// One distinct candidate per SP, all PARA_A.
		let peers =
			[w.declared_peer(PARA_A, V2), w.declared_peer(PARA_A, V2), w.declared_peer(PARA_A, V2)];
		let cands = [
			w.candidate_at(grandparent).para(PARA_A).head_data(HeadData(vec![1])).build(),
			w.candidate_at(parent).para(PARA_A).head_data(HeadData(vec![2])).build(),
			w.candidate_at(leaf).para(PARA_A).head_data(HeadData(vec![3])).build(),
		];
		for (peer, cand) in peers.iter().zip(cands.iter()) {
			w.advertise_with_parent_head(
				peer,
				cand.relay_parent(),
				cand.hash(),
				cand.parent_head_hash(),
			);
		}
		w.base.sim.advance(Duration::from_millis(300));
		w.base.sim.expect_count(
			|e| matches!(e, Effect::SendRequest { .. }),
			2,
			"exactly 2 fetches (leaf CQ has 2 slots for PARA_A)",
		);
	}

	/// Narrow-window SP (= older ancestor) and wide-window SP (= leaf) both advertise
	/// PARA_A. Leaf CQ `[A, other, A]` — narrow can only fill position 0; wide can fill
	/// 0 or 2. Both must fetch — wide must not steal position 0.
	#[crate::sim_test]
	fn linear_multi_sp_no_under_fetch_when_wide_and_narrow_compete<S: CollatorSut>() {
		let config =
			collator_world_config().with_schedule(CoreIndex(0), CoreSchedule::always(PARA_A));
		let mut w: World<S> = bootstrap_world::<S>(config, None);
		for _ in 0..2 {
			w.new_block().register();
		}
		w.new_block()
			.with_claim_queue_at(CoreIndex(0), [PARA_A, ParaId::new(200), PARA_A])
			.activate();
		let leaf = w.leaf();
		let grandparent = w.ancestors()[1]; // window len 1

		let peer_narrow = w.declared_peer(PARA_A, V2);
		let peer_wide = w.declared_peer(PARA_A, V2);
		let cand_narrow =
			w.candidate_at(grandparent).para(PARA_A).head_data(HeadData(vec![1])).build();
		let cand_wide = w.candidate_at(leaf).para(PARA_A).head_data(HeadData(vec![2])).build();
		w.advertise_with_parent_head(
			&peer_narrow,
			grandparent,
			cand_narrow.hash(),
			cand_narrow.parent_head_hash(),
		);
		w.advertise_with_parent_head(
			&peer_wide,
			leaf,
			cand_wide.hash(),
			cand_wide.parent_head_hash(),
		);
		w.base.sim.advance(Duration::from_millis(300));
		w.base.sim.expect_count(
			|e| matches!(e, Effect::SendRequest { .. }),
			2,
			"both narrow- and wide-window ads must fetch (no under-fetch)",
		);
	}

	/// Leaf CQ shorter than the lookahead must not reject valid ancestor advertisements.
	/// Setup: lookahead=3 (default), override leaf CQ to `[A]` (length 1). Advertise at
	/// grandparent (depth 2): position 0 maps to leaf+2 = within sp's lookahead window.
	///
	/// Both sides source the reachability window from the runtime scheduling lookahead (3)
	/// rather than the claim-queue length (1), so the depth-2 ancestor advertisement is
	/// accepted.
	#[crate::sim_test]
	fn short_claim_queue_does_not_reject_ancestor_advertisements<S: CollatorSut>() {
		let config =
			collator_world_config().with_schedule(CoreIndex(0), CoreSchedule::always(PARA_A));
		let mut w: World<S> = bootstrap_world::<S>(config, None);
		for _ in 0..2 {
			w.new_block().register();
		}
		w.new_block().with_claim_queue_at(CoreIndex(0), [PARA_A]).activate();
		let grandparent = w.ancestors()[1];
		let peer = w.declared_peer(PARA_A, V2);
		let cand = w.candidate_at(grandparent).para(PARA_A).build();
		w.advertise_with_parent_head(&peer, grandparent, cand.hash(), cand.parent_head_hash());
		let _ = w.fetch_request(&cand);
	}

	// --- Multi-fork tests ---
	//
	// Sibling forks share a common ancestor. In our framework, `build_with_ancestors_world
	// _with_config(0, ...)` produces genesis → leaf. Genesis is the common ancestor; leaf is
	// fork_a; we extend from genesis again to get fork_b. Sibling support relies on
	// `common::chain::ChainModel::extend` mixing a sibling index into the synthetic child hash, so
	// two extends from the same parent produce distinct hashes.

	const PARA_X: ParaId = ParaId::new(100);
	const PARA_Y: ParaId = ParaId::new(200);

	/// Sibling forks: fork_a schedules PARA_X (default), fork_b schedules PARA_Y. While
	/// both forks are active, both peers stay connected (assignments are the union).
	/// After dropping fork_b, peer_y must be disconnected (its para is no longer
	/// scheduled at any active leaf); peer_x stays.
	#[crate::sim_test]
	fn fork_assignments_are_union_of_leaves<S: CollatorSut>() {
		use polkadot_node_subsystem::messages::{CollatorProtocolMessage, NetworkBridgeEvent};

		let config =
			collator_world_config().with_schedule(CoreIndex(0), CoreSchedule::always(PARA_X));
		let mut w: World<S> = bootstrap_world::<S>(config, None);
		w.new_block().activate();
		let fork_a = w.leaf();
		let common = w.base.chain.lock().genesis();
		let fork_b = w
			.new_block()
			.from_parent(common)
			.with_claim_queue_at(CoreIndex(0), [PARA_Y, PARA_Y, PARA_Y])
			.activate()
			.hash;

		let peer_x = w.declared_peer(PARA_X, V2);
		let peer_y = w.declared_peer(PARA_Y, V2);

		// Both forks active → assignments are the union → neither peer disconnected.
		w.expect_no_disconnect(&peer_x, Duration::from_millis(200));
		w.expect_no_disconnect(&peer_y, Duration::from_millis(200));

		// Drop fork_b: send OurViewChange covering only fork_a.
		w.base.sim.send(CollatorProtocolMessage::NetworkBridgeUpdate(
			NetworkBridgeEvent::OurViewChange(polkadot_node_network_protocol::OurView::new(
				std::iter::once(fork_a),
				0,
			)),
		));
		let _ = fork_b;

		// peer_y disconnects (its para is no longer scheduled). peer_x stays.
		w.expect_disconnect(&peer_y);
		w.expect_no_disconnect(&peer_x, Duration::from_millis(200));
	}

	/// Capacity at a shared ancestor uses the longest-reachable window across forks: two
	/// PARA_X ads at the common ancestor must both fetch (the deeper fork gives `common` a
	/// 2-slot window). Both fetches are launched concurrently from the same scheduling
	/// parent — experimental-only behaviour: legacy serialises to one in-flight fetch per
	/// relay parent by design (`validator_side`: "there's always a single collation being
	/// fetched at any moment of time"), so it can never satisfy this. See the inverse
	/// `collation_fetching_prefer_entries_earlier_in_claim_queue` (only = "legacy").
	#[crate::sim_test(only = "experimental")]
	fn fork_capacity_uses_longest_window_across_paths<S: CollatorSut>() {
		let config =
			collator_world_config().with_schedule(CoreIndex(0), CoreSchedule::always(PARA_X));
		let mut w: World<S> = bootstrap_world::<S>(config, None);
		w.new_block()
			.with_claim_queue_at(CoreIndex(0), [PARA_X, PARA_X, PARA_X])
			.activate();
		let _fork_a = w.leaf();
		let common = w.base.chain.lock().genesis();
		// fork_b at depth 2 from common.
		let fork_b_mid = w
			.new_block()
			.from_parent(common)
			.with_claim_queue_at(CoreIndex(0), [PARA_X, PARA_X, PARA_X])
			.activate()
			.hash;
		let fork_b_tip = w
			.new_block()
			.from_parent(fork_b_mid)
			.with_claim_queue_at(CoreIndex(0), [PARA_X, PARA_X, PARA_X])
			.activate()
			.hash;
		let _ = fork_b_tip;

		let peer_a = w.declared_peer(PARA_X, V2);
		let peer_b = w.declared_peer(PARA_X, V2);
		let cand_a = w.candidate_at(common).para(PARA_X).head_data(HeadData(vec![1])).build();
		let cand_b = w.candidate_at(common).para(PARA_X).head_data(HeadData(vec![2])).build();
		w.advertise_with_parent_head(&peer_a, common, cand_a.hash(), cand_a.parent_head_hash());
		w.advertise_with_parent_head(&peer_b, common, cand_b.hash(), cand_b.parent_head_hash());
		w.base.sim.advance(Duration::from_millis(300));
		w.base.sim.expect_count(
			|e| matches!(e, Effect::SendRequest { .. }),
			2,
			"both ads at common ancestor fetch (longest-window across forks = 2)",
		);
	}

	/// Shared ancestor's capacity is one bucket across both forks, not doubled. Two sibling
	/// forks each with CQ `[X, X, X]`. 4 distinct PARA_X ads at the common ancestor must
	/// produce exactly 2 fetches (the shared `common` slot is not double-counted per fork).
	/// Both fetches fire concurrently from `common` — experimental-only: legacy serialises
	/// to one in-flight fetch per relay parent by design, so it never reaches 2.
	#[crate::sim_test(only = "experimental")]
	fn fork_shared_sp_capacity_not_double_counted<S: CollatorSut>() {
		let config =
			collator_world_config().with_schedule(CoreIndex(0), CoreSchedule::always(PARA_X));
		let mut w: World<S> = bootstrap_world::<S>(config, None);
		w.new_block()
			.with_claim_queue_at(CoreIndex(0), [PARA_X, PARA_X, PARA_X])
			.activate();
		let _fork_a = w.leaf();
		let common = w.base.chain.lock().genesis();
		let _fork_b = w
			.new_block()
			.from_parent(common)
			.with_claim_queue_at(CoreIndex(0), [PARA_X, PARA_X, PARA_X])
			.activate()
			.hash;

		let peers: Vec<_> = (0..4).map(|_| w.declared_peer(PARA_X, V2)).collect();
		let cands: Vec<_> = (0..4)
			.map(|i| w.candidate_at(common).para(PARA_X).head_data(HeadData(vec![i as u8])).build())
			.collect();
		for (peer, cand) in peers.iter().zip(cands.iter()) {
			w.advertise_with_parent_head(peer, common, cand.hash(), cand.parent_head_hash());
		}
		w.base.sim.advance(Duration::from_millis(300));
		w.base.sim.expect_count(
			|e| matches!(e, Effect::SendRequest { .. }),
			2,
			"shared ancestor capacity = 2 (not 4 — one bucket across both forks)",
		);
	}

	/// Drop a fork while a fetch is in-flight on it: the in-flight fetch must be
	/// cancelled (response sender dropped on the wire) AND peers exclusive to that
	/// fork's para must disconnect. fork_a schedules PARA_X, fork_b schedules PARA_Y.
	/// peer_y declares Y, advertises a candidate at fork_b, validator launches a
	/// fetch (we don't respond). Drop fork_b → peer_y disconnects, fetch is
	/// cancelled (we observe via no second emitted within a settle window).
	#[crate::sim_test]
	fn fork_drop_reclaims_capacity_and_disconnects_peers<S: CollatorSut>() {
		use polkadot_node_subsystem::messages::{CollatorProtocolMessage, NetworkBridgeEvent};

		let config =
			collator_world_config().with_schedule(CoreIndex(0), CoreSchedule::always(PARA_X));
		let mut w: World<S> = bootstrap_world::<S>(config, None);
		w.new_block().activate();
		let fork_a = w.leaf();
		let common = w.base.chain.lock().genesis();
		let fork_b = w
			.new_block()
			.from_parent(common)
			.with_claim_queue_at(CoreIndex(0), [PARA_Y, PARA_Y, PARA_Y])
			.activate()
			.hash;

		let peer_y = w.declared_peer(PARA_Y, V2);

		// Advertise on fork_b; validator launches a fetch — we hold the response.
		let cand_y = w.candidate_at(fork_b).para(PARA_Y).build();
		w.advertise_with_parent_head(&peer_y, fork_b, cand_y.hash(), cand_y.parent_head_hash());
		let _req_id = w.fetch_request(&cand_y);

		// Drop fork_b: send OurViewChange excluding it. The validator should:
		// - cancel the in-flight fetch (no second emitted),
		// - disconnect peer_y (its para no longer scheduled at any active leaf).
		w.base.sim.send(CollatorProtocolMessage::NetworkBridgeUpdate(
			NetworkBridgeEvent::OurViewChange(polkadot_node_network_protocol::OurView::new(
				std::iter::once(fork_a),
				0,
			)),
		));

		w.expect_disconnect(&peer_y);
		// The pending fetch must NOT be seconded — fork_b is gone, the candidate
		// can no longer land. Settle long enough that any erroneous second would
		// have fired.
		w.expect_no_second(&cand_y, Duration::from_millis(500));
	}
}

/// Reputation-based arbitration when a claim-queue slot is contended: the higher-reputation
/// collator wins the slot, regardless of where on the implicit view it advertised.
mod reputation_arbitration {
	use crate::common::{
		builders::ProtocolVersion::V2,
		contract::Effect,
		harness::CollatorSut,
		world::{bootstrap_world, collator_world_config, World, WorldExt as _},
	};
	use polkadot_node_subsystem::OverseerSignal;
	use polkadot_primitives::{CandidateEvent, CoreIndex, GroupIndex, HeadData, Id as ParaId};
	use std::time::Duration;

	const PARA_A: ParaId = ParaId::new(100);
	const PARA_OTHER: ParaId = ParaId::new(200);

	/// High-rep peer at an ancestor SP wins the single PARA_A slot over a low-rep peer at
	/// the leaf. Setup: leaf CQ `[A, other, other]` → 1 PARA_A slot. peer_low (score 0)
	/// at leaf; peer_high (score 1, ramped via finalize) at parent. Single fetch goes to
	/// peer_high.
	#[crate::sim_test(only = "experimental")]
	fn high_rep_peer_at_ancestor_wins_over_low_rep_at_leaf<S: CollatorSut>() {
		let config = collator_world_config()
			.with_schedule(CoreIndex(0), crate::common::chain::CoreSchedule::always(PARA_A));
		let mut w: World<S> = bootstrap_world::<S>(config, None);
		w.new_block().activate();
		w.new_block()
			.with_claim_queue_at(CoreIndex(0), [PARA_A, PARA_OTHER, PARA_OTHER])
			.activate();
		let leaf0 = w.leaf();
		let parent = w.ancestors()[0];

		// Ramp peer_high to score 1.
		let peer_high = w.declared_peer(PARA_A, V2);
		let cand_seed = w
			.candidate_at(leaf0)
			.para(PARA_A)
			.parent_head(HeadData(Vec::new()))
			.head_data(HeadData(vec![1]))
			.approved_peer(peer_high.peer_id)
			.build();
		w.outputs
			.insert(cand_seed.hash(), cand_seed.commitments.clone(), cand_seed.pvd.clone());
		w.full_second(&peer_high, &cand_seed);
		{
			let mut chain = w.base.chain.lock();
			chain.set_pending_availability(PARA_A, vec![cand_seed.committed()]);
			chain.set_candidate_events(
				leaf0,
				vec![CandidateEvent::CandidateIncluded(
					cand_seed.receipt.clone(),
					cand_seed.commitments.head_data.clone(),
					CoreIndex(0),
					GroupIndex(0),
				)],
			);
			chain.set_finalized(leaf0);
		}
		w.base.sim.signal(OverseerSignal::BlockFinalized(leaf0, w.leaf_number()));
		w.base.sim.advance(Duration::from_millis(50));

		// New leaf for the arbitration round; rebuild leaf-q on the new leaf too.
		let leaf1 = w
			.new_block()
			.from_parent(leaf0)
			.with_claim_queue_at(CoreIndex(0), [PARA_A, PARA_OTHER, PARA_OTHER])
			.activate()
			.hash;
		let parent_of_leaf1 = leaf0;
		let _ = parent;

		// peer_low joins fresh.
		let peer_low = w.declared_peer(PARA_A, V2);

		// Both advertise PARA_A: peer_high at the now-ancestor (leaf0), peer_low at the leaf.
		// Single PARA_A slot → arbitration kicks in.
		let cand_high = w
			.candidate_at(parent_of_leaf1)
			.para(PARA_A)
			.parent_head(cand_seed.output_head())
			.head_data(HeadData(vec![2]))
			.build();
		let cand_low = w
			.candidate_at(leaf1)
			.para(PARA_A)
			.parent_head(cand_seed.output_head())
			.head_data(HeadData(vec![3]))
			.build();
		w.advertise_with_parent_head(
			&peer_high,
			parent_of_leaf1,
			cand_high.hash(),
			cand_high.parent_head_hash(),
		);
		w.advertise_with_parent_head(
			&peer_low,
			leaf1,
			cand_low.hash(),
			cand_low.parent_head_hash(),
		);
		w.base.sim.advance(Duration::from_millis(50));

		let _ = w.expect_fetch_to(peer_high.peer_id);
		w.base.sim.expect_count(
			|e| matches!(e, Effect::SendRequest { .. }),
			1,
			"single fetch goes to high-rep ancestor peer (slot count = 1)",
		);
	}

	// TODO: port `high_rep_at_any_sp_wins_for_each_position`. Multi-position arbitration
	// where each free CQ position is filled by the rep-best reachable carrier:
	//
	// - Leaf CQ `[A, other, A]` (positions 0=A, 1=other=Y, 2=A).
	// - peer_high_x: ramped score 1, advertises A at leaf (offset 0 → positions 0, 2).
	// - peer_low_x: score 0, advertises A at grandparent (depth 2, offset 2 → position 0 only).
	// - peer_high_y: ramped score 1, advertises Y at leaf.
	//
	// Expected outcome: 3 fetches. Position 2 → peer_high_x (rep-best for A there),
	// position 1 → peer_high_y (only Y candidate), position 0 → peer_low_x (only carrier
	// reachable from grandparent — narrow-only positions don't get stolen by the rep-best
	// wide candidate).
	//
	// Blocked on having a clean way to ramp 2 peers' scores (peer_high_x and peer_high_y)
	// in a single test. The current ramp helper uses the leaf+finalize pattern; doing it
	// twice for two different peers needs either a shared chain-extension dance or a
	// `World::seed_score(peer, para, score)` shortcut. The existing single-ramp tests
	// here demonstrate that the rep machinery works; adding the more elaborate
	// multi-position arbitration is incremental.
}

/// Duplicate-fetch avoidance: the same advertised collation reaching the validator from
/// multiple peers (or across protocol versions) must be fetched once, with the other
/// carriers kept as fallbacks rather than triggering redundant concurrent fetches.
mod duplicate_fetch {
	use crate::common::{
		builders::ProtocolVersion::{V2, V3},
		contract::Effect,
		harness::CollatorSut,
		world::{activated_world, WorldExt as _},
	};
	use polkadot_node_subsystem::OverseerSignal;
	use polkadot_primitives::{CandidateEvent, CoreIndex, GroupIndex, HeadData, Id as ParaId};
	use std::time::Duration;

	const PARA: ParaId = ParaId::new(100);

	/// Two V2 peers advertise the same candidate (same hash, same offer); one fetch fires.
	/// Pre-#12004: two fetches, because `Advertisement` keys on `(offer, peer_id)`. Post-
	/// #12004: dedup keys on the offer alone, with the peer chosen by rep arbitration.
	#[crate::sim_test(bug_on = "experimental", bug_url = "github:paritytech/polkadot-sdk#12004")]
	fn v2_same_candidate_from_multiple_peers_fetched_once<S: CollatorSut>() {
		let mut w = activated_world::<S>(&[(CoreIndex(0), PARA)]);
		let leaf = w.leaf();

		let peer_a = w.declared_peer(PARA, V2);
		let peer_b = w.declared_peer(PARA, V2);
		// Same candidate (hash) advertised by both peers.
		let cand = w.candidate_at(leaf).para(PARA).build();
		w.advertise_with_parent_head(&peer_a, leaf, cand.hash(), cand.parent_head_hash());
		w.advertise_with_parent_head(&peer_b, leaf, cand.hash(), cand.parent_head_hash());

		// Settle long enough that any second concurrent fetch would have fired.
		w.base.sim.advance(Duration::from_millis(300));

		w.base.sim.expect_count(
			|e| matches!(e, Effect::SendRequest { .. }),
			1,
			"exactly one fetch for the shared V2 candidate (must NOT fire one per carrier)",
		);
	}

	/// V2 peer and V3 peer both carry the same V2-descriptor offer; one fetch fires.
	/// V3 protocol may legitimately advertise a V2 descriptor — the validator must dedup
	/// by offer (descriptor) regardless of the carrier's protocol version.
	#[crate::sim_test(bug_on = "experimental", bug_url = "github:paritytech/polkadot-sdk#12004")]
	fn cross_protocol_version_carriers_fetched_once<S: CollatorSut>() {
		let mut w = activated_world::<S>(&[(CoreIndex(0), PARA)]);
		let leaf = w.leaf();

		let peer_v2 = w.declared_peer(PARA, V2);
		let peer_v3 = w.declared_peer(PARA, V3);
		let cand = w.candidate_at(leaf).para(PARA).build();
		w.advertise_with_parent_head(&peer_v2, leaf, cand.hash(), cand.parent_head_hash());
		w.advertise_with_parent_head(&peer_v3, leaf, cand.hash(), cand.parent_head_hash());
		w.base.sim.advance(Duration::from_millis(300));
		w.base.sim.expect_count(
			|e| matches!(e, Effect::SendRequest { .. }),
			1,
			"exactly one fetch across V2 + V3 carriers (offer-keyed dedup)",
		);
	}

	/// Reputation arbitration when multiple carriers offer the same candidate. peer_high
	/// has score 1 (one past inclusion); peer_low has 0. Both advertise the same offer;
	/// the single fetch must go to peer_high.
	#[crate::sim_test(
		only = "experimental",
		bug_on = "experimental",
		bug_url = "github:paritytech/polkadot-sdk#12004"
	)]
	fn v2_co_carrier_rep_arbitration_picks_high_rep_peer<S: CollatorSut>() {
		let mut w = activated_world::<S>(&[(CoreIndex(0), PARA)]);
		let leaf0 = w.leaf();

		// Ramp peer_high to score 1 via finalize-with-included-candidate.
		let peer_high = w.declared_peer(PARA, V2);
		let cand_seed = w
			.candidate_at(leaf0)
			.para(PARA)
			.parent_head(HeadData(Vec::new()))
			.head_data(HeadData(vec![1]))
			.approved_peer(peer_high.peer_id)
			.build();
		w.outputs
			.insert(cand_seed.hash(), cand_seed.commitments.clone(), cand_seed.pvd.clone());
		w.full_second(&peer_high, &cand_seed);
		{
			let mut chain = w.base.chain.lock();
			chain.set_pending_availability(PARA, vec![cand_seed.committed()]);
			chain.set_candidate_events(
				leaf0,
				vec![CandidateEvent::CandidateIncluded(
					cand_seed.receipt.clone(),
					cand_seed.commitments.head_data.clone(),
					CoreIndex(0),
					GroupIndex(0),
				)],
			);
			chain.set_finalized(leaf0);
		}
		w.base.sim.signal(OverseerSignal::BlockFinalized(leaf0, w.leaf_number()));
		w.base.sim.advance(Duration::from_millis(50));

		// New leaf for the arbitration round.
		let leaf1 = w.new_block().from_parent(leaf0).activate().hash;
		let peer_low = w.declared_peer(PARA, V2);

		// Both carriers offer the same new candidate.
		let cand = w
			.candidate_at(leaf1)
			.para(PARA)
			.parent_head(cand_seed.output_head())
			.head_data(HeadData(vec![2]))
			.build();
		w.advertise_with_parent_head(&peer_high, leaf1, cand.hash(), cand.parent_head_hash());
		w.advertise_with_parent_head(&peer_low, leaf1, cand.hash(), cand.parent_head_hash());
		w.base.sim.advance(Duration::from_millis(50));

		// Exactly one fetch, targeted at peer_high.
		let _ = w.expect_fetch_to(peer_high.peer_id);
		w.base.sim.expect_count(
			|e| matches!(e, Effect::SendRequest { .. }),
			1,
			"exactly one fetch and it goes to the rep-best peer",
		);
	}

	/// Co-advertiser *fallback*: when several peers carry the same candidate, the validator
	/// fetches from one and *parks* the others as fallbacks — and if the in-flight fetch
	/// fails, it retries from a parked co-advertiser without waiting for re-advertisement.
	///
	/// This is the failure-path companion to `v2_same_candidate_from_multiple_peers_fetched_once`
	/// (which only checks the happy path fires once). Reviewer ask on #12004:
	/// "exercise the fallback of fetching a collation based on a duplicated advertisement from
	/// a queued peer."
	///
	/// Two V2 peers advertise the same candidate; the validator must:
	///   1. fire *exactly one* fetch and keep the duplicate parked, then
	///   2. on that fetch timing out, fire the fallback fetch to the other carrier.
	///
	/// `bug_on = "experimental"`: legacy parks the duplicate and re-fetches on timeout. Pre-
	/// #12004 experimental keys in-flight dedup on `(offer, peer_id)`, so the two carriers'
	/// `Advertisement`s differ and *both* fetches fire at once (bug 1) — there is no parked
	/// fallback, and step 1's "exactly one fetch" assertion fails. Post-#12004 the offer-only
	/// dedup makes the second carrier a true fallback and this passes on experimental too.
	#[crate::sim_test(bug_on = "experimental", bug_url = "github:paritytech/polkadot-sdk#12004")]
	fn v2_co_carrier_fallback_fetches_from_second_peer_on_failure<S: CollatorSut>() {
		let mut w = activated_world::<S>(&[(CoreIndex(0), PARA)]);
		let leaf = w.leaf();

		let peer_a = w.declared_peer(PARA, V2);
		let peer_b = w.declared_peer(PARA, V2);
		// Same candidate (hash) advertised by both peers.
		let cand = w.candidate_at(leaf).para(PARA).build();
		w.advertise_with_parent_head(&peer_a, leaf, cand.hash(), cand.parent_head_hash());
		w.advertise_with_parent_head(&peer_b, leaf, cand.hash(), cand.parent_head_hash());

		// Settle long enough that any second concurrent fetch would have fired.
		w.base.sim.advance(Duration::from_millis(300));

		// Step 1 — exactly one fetch in flight; the other carrier is parked as a fallback.
		// (This is the assertion pre-#12004 experimental fails: it double-fetches.)
		w.base.sim.expect_count(
			|e| matches!(e, Effect::SendRequest { .. }),
			1,
			"exactly one fetch for the shared V2 candidate; the duplicate must be parked",
		);

		// Identify which carrier got the (single) first fetch, so we can assert the fallback
		// targets the *other* one.
		let (first_peer, _) = w
			.first_fetch_after(0)
			.expect("exactly one fetch fired, so first_fetch_after must find it");
		let other_peer = if first_peer == peer_a.peer_id { peer_b.peer_id } else { peer_a.peer_id };

		// Step 2 — never respond. `expect_fetch_to` drives the clock to the subsystem's per-fetch
		// abandon timer, after which the parked co-advertiser must be used as the fallback, without
		// any new advertisement having arrived.
		let _ = w.expect_fetch_to(other_peer);
	}
}
