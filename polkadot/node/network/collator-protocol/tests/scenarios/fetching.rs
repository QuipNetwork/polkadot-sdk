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

//! Fetch lifecycle.

mod fetches_next_collation {
	use crate::common::{
		builders::ProtocolVersion::V1,
		contract::{Effect, ReqKind},
		harness::CollatorSut,
		world::{activated_world, WorldExt as _},
	};
	use polkadot_primitives::{CoreIndex, Id as ParaId};

	const PARA: ParaId = ParaId::new(2000);

	#[crate::sim_test]
	fn stalled_fetch_falls_back_to_next_peer_after_timeout<S: CollatorSut>() {
		let mut w = activated_world::<S>(&[(CoreIndex(0), PARA)]);
		let leaf = w.leaf();

		let peers =
			[w.declared_peer(PARA, V1), w.declared_peer(PARA, V1), w.declared_peer(PARA, V1)];
		for p in &peers {
			w.base.sim.send(p.advertise(leaf, None, None));
		}

		// First fetch fires (which peer is unspecified).
		let (_first_peer, _, _) = w.expect_any_fetch();

		// Don't respond. `expect_at_least_after` drives the clock to the subsystem's per-fetch
		// abandon timer, after which ≥1 follow-up fetch must fire.
		let barrier = w.base.sim.now_sim_t();
		w.base.sim.expect_at_least_after(
			barrier,
			|e| matches!(e, Effect::SendRequest { kind: ReqKind::CollationFetchingV1, .. }),
			1,
			"a follow-up SendRequest fires after the first peer's deadline",
		);
	}
}

mod fetch_next_on_invalid {
	use crate::common::{
		builders::{Candidate, ProtocolVersion::V1},
		harness::CollatorSut,
		world::{activated_world, WorldExt as _},
	};
	use polkadot_node_subsystem::messages::CollatorProtocolMessage;
	use polkadot_primitives::{CoreIndex, Id as ParaId};

	const PARA: ParaId = ParaId::new(2000);

	#[crate::sim_test]
	fn invalid_signal_fetches_next<S: CollatorSut>() {
		let mut w = activated_world::<S>(&[(CoreIndex(0), PARA)]);
		let leaf = w.leaf();
		let candidate = w.candidate_at(leaf).para(PARA).build();

		let peer_b = w.declared_peer(PARA, V1);
		let peer_c = w.declared_peer(PARA, V1);
		w.base.sim.send(peer_b.advertise(leaf, None, None));
		w.base.sim.send(peer_c.advertise(leaf, None, None));

		// One fetch fires (whichever peer wins the queue).
		let (first_peer, request_id, _) = w.expect_any_fetch();
		let other_peer = if first_peer == peer_b.peer_id { peer_c.peer_id } else { peer_b.peer_id };

		w.respond_fetch_v1(request_id, candidate.receipt.clone(), Candidate::empty_pov());
		w.expect_second(&candidate);

		// Invalid signal → next fetch fires for the other peer. Rep emission (if any) is
		// covered by the divergent suite.
		w.base
			.sim
			.send(CollatorProtocolMessage::Invalid(leaf, candidate.receipt.clone().into()));
		let _ = w.expect_fetch_to(other_peer);
	}
}

mod fetch_timeout {
	use crate::common::{
		builders::{Candidate, ProtocolVersion::V2},
		harness::CollatorSut,
		world::{activated_world, WorldExt as _},
	};
	use polkadot_primitives::{CoreIndex, Id as ParaId};

	const PARA: ParaId = ParaId::new(2000);

	#[crate::sim_test]
	fn fetch_timeout_advances_to_next_peer<S: CollatorSut>() {
		let mut w = activated_world::<S>(&[(CoreIndex(0), PARA)]);
		let leaf = w.leaf();

		let candidate = Candidate::for_para_at(PARA, leaf);
		let head_hash = candidate.receipt.descriptor.para_head();

		let peer_a = w.declared_peer(PARA, V2);
		let peer_b = w.declared_peer(PARA, V2);
		for peer in [&peer_a, &peer_b] {
			w.base.sim.send(peer.advertise(leaf, Some(candidate.hash()), Some(head_hash)));
		}

		let (first_peer, _, _) = w.expect_any_fetch();
		let other_peer = if first_peer == peer_a.peer_id { peer_b.peer_id } else { peer_a.peer_id };

		// Don't respond. `expect_fetch_to` drives the clock to the subsystem's per-fetch abandon
		// timer, after which the fetch falls back to the other peer.
		let _ = w.expect_fetch_to(other_peer);
	}
}

mod single_fetch_per_relay_parent {
	use crate::common::{
		builders::ProtocolVersion::V1,
		contract::{Effect, ReqKind},
		harness::CollatorSut,
		world::{activated_world, WorldExt as _},
	};
	use polkadot_primitives::{CoreIndex, Id as ParaId};

	const PARA: ParaId = ParaId::new(2000);

	#[crate::sim_test]
	fn one_fetch_per_relay_parent_until_seconded<S: CollatorSut>() {
		let mut w = activated_world::<S>(&[(CoreIndex(0), PARA)]);
		let leaf = w.leaf();

		let peer_b = w.declared_peer(PARA, V1);
		let peer_c = w.declared_peer(PARA, V1);
		w.base.sim.send(peer_b.advertise(leaf, None, None));
		w.base.sim.send(peer_c.advertise(leaf, None, None));

		let _ = w.expect_any_fetch();

		w.base.sim.expect_count(
			|e| matches!(e, Effect::SendRequest { kind: ReqKind::CollationFetchingV1, .. }),
			1,
			"SendRequest while one fetch is in flight (no second concurrent fetch allowed)",
		);
	}
}

mod fair_collation_fetches {
	use crate::common::{
		builders::{Candidate, ProtocolVersion::V2},
		chain::CoreSchedule,
		harness::CollatorSut,
		world::{bootstrap_world, collator_world_config, World, WorldExt as _},
	};
	use polkadot_primitives::{CandidateHash, CoreIndex, Hash, HeadData, Id as ParaId};
	use std::time::Duration;

	const PARA_A: ParaId = ParaId::new(2000);
	const PARA_B: ParaId = ParaId::new(2001);

	fn shared_core_world<S: CollatorSut>() -> crate::common::world::World<S> {
		let config =
			collator_world_config().with_schedule(CoreIndex(0), CoreSchedule::always(PARA_A));
		let mut w: World<S> = bootstrap_world::<S>(config, None);
		w.new_block()
			.with_claim_queue_at(CoreIndex(0), [PARA_B, PARA_A, PARA_A])
			.activate();
		w
	}

	#[crate::sim_test]
	fn shared_core_fills_per_para_lookahead_then_rejects_more<S: CollatorSut>() {
		let mut w = shared_core_world::<S>();
		let leaf = w.leaf();
		let leaf_n = w.leaf_number();

		// Para A: chain of 2 candidates.
		let a1 = Candidate::builder()
			.para(PARA_A)
			.relay_parent(leaf)
			.relay_parent_number(leaf_n)
			.parent_head(HeadData(Vec::new()))
			.head_data(HeadData(vec![1]))
			.build();
		let a2 = Candidate::builder()
			.para(PARA_A)
			.relay_parent(leaf)
			.relay_parent_number(leaf_n)
			.parent_head(a1.output_head())
			.head_data(HeadData(vec![2]))
			.build();

		// Para B: single candidate.
		let b1 = Candidate::builder()
			.para(PARA_B)
			.relay_parent(leaf)
			.relay_parent_number(leaf_n)
			.parent_head(HeadData(Vec::new()))
			.head_data(HeadData(vec![10]))
			.build();

		let peer_a = w.declared_peer(PARA_A, V2);
		let peer_b = w.declared_peer(PARA_B, V2);

		w.full_second(&peer_a, &a1);
		w.full_second(&peer_a, &a2);
		w.full_second(&peer_b, &b1);

		// 4th advertisement on either para must NOT trigger any fetch.
		let extra_a = CandidateHash(Hash::repeat_byte(0xAA));
		w.advertise_with_parent_head(&peer_a, leaf, extra_a, Hash::zero());
		let extra_b = CandidateHash(Hash::repeat_byte(0xBB));
		w.advertise_with_parent_head(&peer_b, leaf, extra_b, Hash::zero());

		w.no_fetch_within(Duration::from_millis(200));
	}

	/// Para B advertised after para A still gets fetched — earlier claim-queue entry wins.
	/// CQ=[B,A,A]: B occupies position 0 (earliest), A at 1+2. Even when peer A starts queue
	/// first, peer B's advertisement triggers a fetch for B once B's slot opens.
	#[crate::sim_test]
	fn shared_core_para_b_can_fetch_alongside_para_a<S: CollatorSut>() {
		let mut w = shared_core_world::<S>();
		let leaf = w.leaf();
		let leaf_n = w.leaf_number();

		let a1 = Candidate::builder()
			.para(PARA_A)
			.relay_parent(leaf)
			.relay_parent_number(leaf_n)
			.parent_head(HeadData(Vec::new()))
			.head_data(HeadData(vec![1]))
			.build();
		let b1 = Candidate::builder()
			.para(PARA_B)
			.relay_parent(leaf)
			.relay_parent_number(leaf_n)
			.parent_head(HeadData(Vec::new()))
			.head_data(HeadData(vec![10]))
			.build();

		let peer_a = w.declared_peer(PARA_A, V2);
		let peer_b = w.declared_peer(PARA_B, V2);

		w.full_second(&peer_a, &a1);
		w.full_second(&peer_b, &b1);
	}

	/// 4th advertisement for para A on a shared-core CQ where para A holds 2 slots is
	/// silently rejected — claim slots full for A. (Also exercised by the headline test
	/// `shared_core_fills_per_para_lookahead_then_rejects_more`.)
	#[crate::sim_test]
	fn shared_core_third_para_a_advertisement_silently_dropped<S: CollatorSut>() {
		let mut w = shared_core_world::<S>();
		let leaf = w.leaf();
		let leaf_n = w.leaf_number();

		let a1 = Candidate::builder()
			.para(PARA_A)
			.relay_parent(leaf)
			.relay_parent_number(leaf_n)
			.parent_head(HeadData(Vec::new()))
			.head_data(HeadData(vec![1]))
			.build();
		let a2 = Candidate::builder()
			.para(PARA_A)
			.relay_parent(leaf)
			.relay_parent_number(leaf_n)
			.parent_head(a1.output_head())
			.head_data(HeadData(vec![2]))
			.build();

		let peer_a = w.declared_peer(PARA_A, V2);
		w.full_second(&peer_a, &a1);
		w.full_second(&peer_a, &a2);

		let extra_hash = CandidateHash(Hash::repeat_byte(0xCC));
		w.advertise_with_parent_head(&peer_a, leaf, extra_hash, Hash::zero());
		w.no_fetch_within(Duration::from_millis(200));
	}

	/// On-demand under-scheduled core: the claim queue (`[A]`, length 1) is shorter than the
	/// scheduling lookahead (3). Para A holds exactly one slot. After A seconds its one candidate,
	/// a second advertisement for A at the same relay parent must be rejected — the slot is full
	/// and the positions within the lookahead beyond the claim queue are unscheduled, available to
	/// no para.
	///
	/// Guards against treating those padding positions as free: sourcing the window length from the
	/// runtime lookahead (rather than the claim-queue length) means the window now extends past the
	/// claim queue, so the padding must be marked occupied, not available — otherwise A would be
	/// admitted into a phantom slot.
	#[crate::sim_test]
	fn under_scheduled_core_rejects_beyond_claim_queue<S: CollatorSut>() {
		let config =
			collator_world_config().with_schedule(CoreIndex(0), CoreSchedule::always(PARA_A));
		let mut w: World<S> = bootstrap_world::<S>(config, None);
		w.new_block().with_claim_queue_at(CoreIndex(0), [PARA_A]).activate();
		let leaf = w.leaf();
		let leaf_n = w.leaf_number();

		let a1 = Candidate::builder()
			.para(PARA_A)
			.relay_parent(leaf)
			.relay_parent_number(leaf_n)
			.parent_head(HeadData(Vec::new()))
			.head_data(HeadData(vec![1]))
			.build();

		let peer_a = w.declared_peer(PARA_A, V2);
		w.full_second(&peer_a, &a1);

		// A's one slot is now consumed; a second advertisement must not fetch.
		let extra_hash = CandidateHash(Hash::repeat_byte(0xCC));
		w.advertise_with_parent_head(&peer_a, leaf, extra_hash, Hash::zero());
		w.no_fetch_within(Duration::from_millis(200));
	}
}

mod response_sanity_check {
	use crate::common::{
		builders::{Candidate, ProtocolVersion::V2},
		harness::CollatorSut,
		world::{activated_world, WorldExt as _},
	};
	use std::time::Duration;

	/// Window for the "candidate not seconded" assertion. Long enough that a working impl
	/// would have already dispatched the bad-response detection and short enough not to
	/// stall the suite; both impls land well under this.
	const NO_SECOND_WINDOW: Duration = Duration::from_millis(500);
	use polkadot_primitives::{
		CandidateHash, CandidateReceiptV2, CoreIndex, Hash, HeadData, Id as ParaId,
		MutateDescriptorV2, PersistedValidationData,
	};
	use polkadot_primitives_test_helpers::{
		dummy_committed_candidate_receipt_v2, dummy_committed_candidate_receipt_v3,
	};

	const PARA_A: ParaId = ParaId::new(2000);

	/// PVD whose `parent_head` is empty (the framework's default fixture). All four sanity
	/// scenarios pin `persisted_validation_data_hash` to this shape so real backing's PVD
	/// lookup proceeds; the rejection happens later, in the response-side check.
	fn empty_parent_pvd(relay_parent_number: u32) -> PersistedValidationData {
		PersistedValidationData {
			parent_head: HeadData(Vec::new()),
			relay_parent_number,
			relay_parent_storage_root: Hash::zero(),
			max_pov_size: 5 * 1024 * 1024,
		}
	}

	/// Build a `CandidateReceiptV2` (with the supplied closure) wrapped in a [`Candidate`]
	/// for advertise/fetch convenience. Used by the V2/V3 invalid-descriptor scenarios.
	fn build_descriptor_with<F>(
		w: &crate::common::world::World<impl CollatorSut>,
		mut f: F,
	) -> (CandidateReceiptV2, Candidate)
	where
		F: FnMut(&mut polkadot_primitives::CommittedCandidateReceiptV2),
	{
		let pvd = empty_parent_pvd(w.leaf_number());
		let mut committed = dummy_committed_candidate_receipt_v2(w.leaf());
		committed.descriptor.set_para_id(PARA_A);
		committed.descriptor.set_persisted_validation_data_hash(pvd.hash());
		committed.descriptor.set_core_index(CoreIndex(0));
		committed.descriptor.set_session_index(0);
		f(&mut committed);
		let receipt: CandidateReceiptV2 = committed.to_plain();
		let candidate = Candidate::from_receipt(receipt.clone());
		(receipt, candidate)
	}

	#[crate::sim_test]
	fn response_with_mismatched_candidate_hash_rejects<S: CollatorSut>() {
		let mut w = activated_world::<S>(&[(CoreIndex(0), PARA_A)]);
		let pvd = empty_parent_pvd(w.leaf_number());
		let mut actual = Candidate::for_para_at(PARA_A, w.leaf());
		actual.receipt.descriptor.set_persisted_validation_data_hash(pvd.hash());

		let peer = w.declared_peer(PARA_A, V2);

		// Advertise with a hash that is NOT the actual fetched candidate's hash.
		let advertised_hash = CandidateHash(Hash::repeat_byte(0xFE));
		assert_ne!(advertised_hash, actual.hash(), "advertised hash must differ from actual");
		w.advertise_with_parent_head(&peer, w.leaf(), advertised_hash, HeadData(Vec::new()).hash());

		let request_id = w.expect_fetch_for_hash(advertised_hash);
		w.respond_fetch_v2(request_id, actual.receipt.clone(), Candidate::empty_pov());
		w.expect_no_second(&actual, NO_SECOND_WINDOW);
	}

	#[crate::sim_test]
	fn response_with_wrong_parent_head_data_rejects<S: CollatorSut>() {
		let mut w = activated_world::<S>(&[(CoreIndex(0), PARA_A)]);
		let pvd = empty_parent_pvd(w.leaf_number());
		let mut candidate = Candidate::for_para_at(PARA_A, w.leaf());
		candidate.receipt.descriptor.set_persisted_validation_data_hash(pvd.hash());

		let peer = w.declared_peer(PARA_A, V2);

		let advertised_parent_head_hash = HeadData(Vec::new()).hash();
		w.advertise_with_parent_head(
			&peer,
			w.leaf(),
			candidate.hash(),
			advertised_parent_head_hash,
		);
		let request_id = w.fetch_request(&candidate);

		let wrong_parent_head = HeadData(vec![0xDE, 0xAD, 0xBE, 0xEF]);
		assert_ne!(wrong_parent_head.hash(), advertised_parent_head_hash);
		w.respond_fetch_v2_with_parent_head(
			request_id,
			candidate.receipt.clone(),
			Candidate::empty_pov(),
			wrong_parent_head,
		);

		w.expect_no_second(&candidate, NO_SECOND_WINDOW);
	}

	#[crate::sim_test]
	fn v2_descriptor_with_invalid_core_index_rejects<S: CollatorSut>() {
		let mut w = activated_world::<S>(&[(CoreIndex(0), PARA_A)]);
		// Para is assigned to core 0; out-of-range core 10 → rejected.
		let (receipt, candidate) = build_descriptor_with(&w, |c| {
			c.descriptor.set_core_index(CoreIndex(10));
		});
		let peer = w.declared_peer(PARA_A, V2);
		w.advertise_with_parent_head(
			&peer,
			w.leaf(),
			candidate.hash(),
			HeadData(Vec::new()).hash(),
		);
		let request_id = w.fetch_request(&candidate);
		w.respond_fetch_v2(request_id, receipt, Candidate::empty_pov());
		w.expect_no_second(&candidate, NO_SECOND_WINDOW);
	}

	/// Mirrors the second arm of upstream `invalid_v2_descriptor`: core_index=0 is fine but
	/// the descriptor's session_index is wrong → rejected. (Distinct from
	/// `v3_session_index_checks::v2_descriptor_with_wrong_session_index_rejects` only by
	/// which leg of the upstream rstest it tracks; both probe the same gate.)
	#[crate::sim_test]
	fn v2_descriptor_with_invalid_session_index_rejects<S: CollatorSut>() {
		let mut w = activated_world::<S>(&[(CoreIndex(0), PARA_A)]);
		let (receipt, candidate) = build_descriptor_with(&w, |c| {
			c.descriptor.set_session_index(10); // chain has session 0
		});
		let peer = w.declared_peer(PARA_A, V2);
		w.advertise_with_parent_head(
			&peer,
			w.leaf(),
			candidate.hash(),
			HeadData(Vec::new()).hash(),
		);
		let request_id = w.fetch_request(&candidate);
		w.respond_fetch_v2(request_id, receipt, Candidate::empty_pov());
		w.expect_no_second(&candidate, NO_SECOND_WINDOW);
	}

	#[crate::sim_test]
	fn v3_candidate_via_v2_protocol_rejects<S: CollatorSut>() {
		v3_descriptor_rejected_on_wrong_protocol_helper::<S>(
			ProtocolKind::V2,
			// crafted_unknown
			false,
		);
	}

	#[crate::sim_test]
	fn v3_candidate_via_v1_protocol_rejects<S: CollatorSut>() {
		v3_descriptor_rejected_on_wrong_protocol_helper::<S>(
			ProtocolKind::V1,
			// crafted_unknown
			false,
		);
	}

	#[crate::sim_test]
	fn crafted_unknown_descriptor_via_v2_protocol_rejects<S: CollatorSut>() {
		v3_descriptor_rejected_on_wrong_protocol_helper::<S>(
			ProtocolKind::V2,
			// crafted_unknown
			true,
		);
	}

	#[crate::sim_test]
	fn crafted_unknown_descriptor_via_v1_protocol_rejects<S: CollatorSut>() {
		v3_descriptor_rejected_on_wrong_protocol_helper::<S>(
			ProtocolKind::V1,
			// crafted_unknown
			true,
		);
	}

	#[derive(Clone, Copy)]
	enum ProtocolKind {
		V1,
		V2,
	}

	/// Helper for the 4-case rstest above. Builds a V3 (or crafted-unknown via
	/// `set_version(2)`) candidate, advertises over V1 or V2, responds with the matching
	/// fetch flavour. Validator must reject (no `SecondCandidate`) in all cases.
	fn v3_descriptor_rejected_on_wrong_protocol_helper<S: CollatorSut>(
		wire: ProtocolKind,
		crafted_unknown: bool,
	) {
		use crate::common::builders::ProtocolVersion;
		let mut w = activated_world::<S>(&[(CoreIndex(0), PARA_A)]);
		let pvd = empty_parent_pvd(w.leaf_number());

		let mut committed = dummy_committed_candidate_receipt_v3(w.leaf(), w.leaf());
		committed.descriptor.set_para_id(PARA_A);
		committed.descriptor.set_persisted_validation_data_hash(pvd.hash());
		committed.descriptor.set_core_index(CoreIndex(0));
		committed.descriptor.set_session_index(0);
		if crafted_unknown {
			// version=0 → V2, version=1 → V3, anything else → Unknown.
			committed.descriptor.set_version(2);
		}
		let receipt: CandidateReceiptV2 = committed.to_plain();
		let candidate = Candidate::from_receipt(receipt.clone());

		let proto = match wire {
			ProtocolKind::V1 => ProtocolVersion::V1,
			ProtocolKind::V2 => ProtocolVersion::V2,
		};
		let peer = w.declared_peer(PARA_A, proto);
		let leaf = w.leaf();
		match wire {
			ProtocolKind::V1 => {
				// V1 advertisement carries no candidate_hash on the wire.
				w.base.sim.send(peer.advertise(leaf, None, None));
				let (_, request_id, _) = w.expect_any_fetch();
				w.respond_fetch_v1(request_id, receipt, Candidate::empty_pov());
			},
			ProtocolKind::V2 => {
				w.advertise_with_parent_head(
					&peer,
					leaf,
					candidate.hash(),
					HeadData(Vec::new()).hash(),
				);
				let request_id = w.fetch_request(&candidate);
				w.respond_fetch_v2(request_id, receipt, Candidate::empty_pov());
			},
		}
		w.expect_no_second(&candidate, NO_SECOND_WINDOW);
	}
}

mod collation_fetching_considers_advertisements_from_the_whole_view {
	use crate::common::{
		builders::ProtocolVersion::V2,
		chain::CoreSchedule,
		harness::CollatorSut,
		world::{bootstrap_world, collator_world_config, World, WorldExt as _},
	};
	use polkadot_primitives::{CoreIndex, HeadData, Id as ParaId};
	use std::time::Duration;

	const PARA_A: ParaId = ParaId::new(2000);
	const PARA_B: ParaId = ParaId::new(2001);

	/// KNOWN BUG (experimental): seconded count from the prior leaf is not preserved when
	/// extending into the new leaf's implicit view. Experimental fires a fetch for a
	/// candidate that should be CQ-blocked. See
	/// `memory:project_collator_experimental_seconded_count_lost_across_view`.
	#[crate::sim_test]
	fn seconded_per_para_counted_across_whole_view<S: CollatorSut>() {
		let config =
			collator_world_config().with_schedule(CoreIndex(0), CoreSchedule::always(PARA_A));
		let mut w: World<S> = bootstrap_world::<S>(config, None);
		w.new_block()
			.with_claim_queue_at(CoreIndex(0), [PARA_B, PARA_A, PARA_A])
			.activate();
		let leaf0 = w.leaf();

		// Second 1× A and 1× B at leaf0.
		let a1 = w
			.candidate_at(leaf0)
			.para(PARA_A)
			.parent_head(HeadData(Vec::new()))
			.head_data(HeadData(vec![1]))
			.build();
		let b1 = w
			.candidate_at(leaf0)
			.para(PARA_B)
			.parent_head(HeadData(Vec::new()))
			.head_data(HeadData(vec![10]))
			.build();

		let peer_a = w.declared_peer(PARA_A, V2);
		let peer_b = w.declared_peer(PARA_B, V2);

		w.full_second(&peer_a, &a1);
		w.full_second(&peer_b, &b1);

		// Activate a child of leaf0; previously seconded candidates remain in scope via the
		// new leaf's implicit view. New leaf inherits same CQ shape [B,A,A].
		let leaf1 = w
			.new_block()
			.from_parent(leaf0)
			.with_claim_queue_at(CoreIndex(0), [PARA_B, PARA_A, PARA_A])
			.activate()
			.hash;

		// Advertise another A at leaf1 — this lands in CQ position 1 or 2; A still has 1
		// remaining slot (3 total in CQ minus 1 already counted).
		let a2 = w
			.candidate_at(leaf1)
			.para(PARA_A)
			.parent_head(a1.output_head())
			.head_data(HeadData(vec![2]))
			.build();
		w.full_second(&peer_a, &a2);

		// 4th A: claim queue full for A (2 already seconded). Reject.
		let a3 = w
			.candidate_at(leaf1)
			.para(PARA_A)
			.parent_head(a2.output_head())
			.head_data(HeadData(vec![3]))
			.build();
		w.advertise_with_parent_head(&peer_a, leaf1, a3.hash(), a3.parent_head_hash());
		w.no_fetch_for(&a3, Duration::from_millis(150));

		// 2nd B: B was at CQ pos 0 (1 slot). Already 1 seconded → reject.
		let b2 = w
			.candidate_at(leaf1)
			.para(PARA_B)
			.parent_head(b1.output_head())
			.head_data(HeadData(vec![11]))
			.build();
		w.advertise_with_parent_head(&peer_b, leaf1, b2.hash(), b2.parent_head_hash());
		w.no_fetch_for(&b2, Duration::from_millis(50));
	}
}

mod collation_fetching_fairness_handles_old_claims {
	use crate::common::{
		builders::ProtocolVersion::V2,
		chain::CoreSchedule,
		harness::CollatorSut,
		world::{bootstrap_world, collator_world_config, World, WorldExt as _},
	};
	use polkadot_primitives::{CoreIndex, HeadData, Id as ParaId};
	use std::time::Duration;

	const PARA_A: ParaId = ParaId::new(2000);
	const PARA_B: ParaId = ParaId::new(2001);

	#[crate::sim_test]
	fn old_claims_age_out_only_on_view_shift<S: CollatorSut>() {
		// Initial leaf with CQ=[A,B,A].
		let config =
			collator_world_config().with_schedule(CoreIndex(0), CoreSchedule::always(PARA_A));
		let mut w: World<S> = bootstrap_world::<S>(config, None);
		w.new_block()
			.with_claim_queue_at(CoreIndex(0), [PARA_A, PARA_B, PARA_A])
			.activate();
		let leaf2 = w.leaf();

		let peer_a = w.declared_peer(PARA_A, V2);
		let peer_b = w.declared_peer(PARA_B, V2);

		// Second 2A + 1B at leaf2 — fills the queue.
		let a1 = w
			.candidate_at(leaf2)
			.para(PARA_A)
			.parent_head(HeadData(Vec::new()))
			.head_data(HeadData(vec![1]))
			.build();
		let a2 = w
			.candidate_at(leaf2)
			.para(PARA_A)
			.parent_head(a1.output_head())
			.head_data(HeadData(vec![2]))
			.build();
		let b1 = w
			.candidate_at(leaf2)
			.para(PARA_B)
			.parent_head(HeadData(Vec::new()))
			.head_data(HeadData(vec![10]))
			.build();

		w.full_second(&peer_a, &a1);
		w.full_second(&peer_a, &a2);
		w.full_second(&peer_b, &b1);

		// Activate leaf3 (child of leaf2) with CQ=[B,A,B]. With A=2 already seconded and
		// only A=1 in this CQ, A's claim is full; B=1 already → CQ has 2 B slots, 1 free.
		let leaf3 = w
			.new_block()
			.from_parent(leaf2)
			.with_claim_queue_at(CoreIndex(0), [PARA_B, PARA_A, PARA_B])
			.activate()
			.hash;

		// Per upstream, no new ads should fetch at leaf3 — across the view {leaf2, leaf3} the
		// total seconded count for each para already meets/exceeds the CQ's per-para count.
		let extra_a_at_3 = w
			.candidate_at(leaf3)
			.para(PARA_A)
			.parent_head(a2.output_head())
			.head_data(HeadData(vec![20]))
			.build();
		w.advertise_with_parent_head(
			&peer_a,
			leaf3,
			extra_a_at_3.hash(),
			extra_a_at_3.parent_head_hash(),
		);
		w.no_fetch_for(&extra_a_at_3, Duration::from_millis(150));

		// Now activate leaf4 (child of leaf3) with CQ=[A,B,A]. Per upstream, leaf2 ages out
		// of allowed ancestry (depth > allowed_ancestry_len=2) → its seconded count drops.
		// With leaf2 out, only leaf3+leaf4 ancestry counts — fresh budget for B and A.
		let leaf4 = w
			.new_block()
			.from_parent(leaf3)
			.with_claim_queue_at(CoreIndex(0), [PARA_A, PARA_B, PARA_A])
			.activate()
			.hash;

		let b2 = w
			.candidate_at(leaf4)
			.para(PARA_B)
			.parent_head(b1.output_head())
			.head_data(HeadData(vec![11]))
			.build();
		let a3 = w
			.candidate_at(leaf4)
			.para(PARA_A)
			.parent_head(a2.output_head())
			.head_data(HeadData(vec![3]))
			.build();
		w.full_second(&peer_b, &b2);
		w.full_second(&peer_a, &a3);

		// Now CQ at leaf4 satisfied — further ads ignored.
		let extra_a = w
			.candidate_at(leaf4)
			.para(PARA_A)
			.parent_head(a3.output_head())
			.head_data(HeadData(vec![30]))
			.build();
		w.advertise_with_parent_head(&peer_a, leaf4, extra_a.hash(), extra_a.parent_head_hash());
		w.no_fetch_for(&extra_a, Duration::from_millis(150));
	}
}

mod collation_fetching_prefer_entries_earlier_in_claim_queue {
	use crate::common::{
		builders::ProtocolVersion::V2,
		chain::CoreSchedule,
		contract::{Effect, ReqKind},
		harness::CollatorSut,
		world::{bootstrap_world, collator_world_config, World, WorldExt as _},
	};
	use polkadot_primitives::{CoreIndex, HeadData, Id as ParaId};
	use std::time::Duration;

	const PARA_A: ParaId = ParaId::new(2000);
	const PARA_B: ParaId = ParaId::new(2001);

	/// Legacy-only: experimental fetches one collation per CQ slot in parallel by
	/// design (#11023 / PR #12004's prdoc), not one fetch per RP. The "earlier-CQ-
	/// position wins" invariant only applies on legacy where per-RP serialization
	/// queues advertisements behind a single in-flight fetch. Reclassified from
	/// `bug_on = "experimental"` to `only = "legacy"` after investigation —
	/// see `memory:project_collator_experimental_concurrent_fetches_violation`.
	#[crate::sim_test(only = "legacy")]
	fn collation_fetching_prefer_entries_earlier_in_claim_queue<S: CollatorSut>() {
		let config =
			collator_world_config().with_schedule(CoreIndex(0), CoreSchedule::always(PARA_A));
		let mut w: World<S> = bootstrap_world::<S>(config, None);
		w.new_block()
			.with_claim_queue_at(CoreIndex(0), [PARA_B, PARA_A, PARA_A])
			.activate();
		let leaf = w.leaf();

		let a1 = w
			.candidate_at(leaf)
			.para(PARA_A)
			.parent_head(HeadData(Vec::new()))
			.head_data(HeadData(vec![1]))
			.build();
		let a2 = w
			.candidate_at(leaf)
			.para(PARA_A)
			.parent_head(a1.output_head())
			.head_data(HeadData(vec![2]))
			.build();
		let b1 = w
			.candidate_at(leaf)
			.para(PARA_B)
			.parent_head(HeadData(Vec::new()))
			.head_data(HeadData(vec![10]))
			.build();

		let peer_a = w.declared_peer(PARA_A, V2);
		let peer_b = w.declared_peer(PARA_B, V2);

		// A1 fetched first.
		w.outputs.insert(a1.hash(), a1.commitments.clone(), a1.pvd.clone());
		w.outputs.insert(a2.hash(), a2.commitments.clone(), a2.pvd.clone());
		w.outputs.insert(b1.hash(), b1.commitments.clone(), b1.pvd.clone());

		w.advertise_with_parent_head(&peer_a, leaf, a1.hash(), a1.parent_head_hash());
		let a1_req = w.fetch_request(&a1);

		// Queue A2 + B1 while A1 is in flight; expect no fetches to fire.
		w.advertise_with_parent_head(&peer_a, leaf, a2.hash(), a2.parent_head_hash());
		w.advertise_with_parent_head(&peer_b, leaf, b1.hash(), b1.parent_head_hash());
		// One fetch in flight.
		w.base.sim.expect_count(
			|e| matches!(e, Effect::SendRequest { kind: ReqKind::CollationFetchingV2, .. }),
			1,
			"exactly 1 fetch in flight while A1 is being fetched",
		);

		// Resolve A1, then assert the next fetch fired (after this point in the recorder) is
		// for B1, not A2 — earlier CQ position wins.
		let barrier = w.recorder_barrier();
		w.respond_fetch_v2(
			a1_req,
			a1.receipt.clone(),
			crate::common::builders::Candidate::empty_pov(),
		);
		w.expect_second(&a1);
		w.base.sim.advance(Duration::from_millis(50));

		let next = w.first_fetch_after(barrier).expect("a fetch fires after A1 seconding");
		assert_eq!(
			next.1,
			Some(b1.hash()),
			"first fetch after A1 must be B1 (CQ position 0), not A2",
		);
	}
}
