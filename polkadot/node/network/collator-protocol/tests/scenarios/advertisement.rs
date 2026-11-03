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

//! Advertisement handling.

mod advertise_then_fetch {
	use crate::common::{
		builders::ProtocolVersion::V2,
		harness::CollatorSut,
		world::{activated_world, WorldExt as _},
	};
	use polkadot_primitives::{CoreIndex, Id as ParaId};

	const PARA: ParaId = ParaId::new(2000);

	#[crate::sim_test]
	fn valid_advertisement_triggers_fetch<S: CollatorSut>() {
		let mut w = activated_world::<S>(&[(CoreIndex(0), PARA)]);
		let peer = w.declared_peer(PARA, V2);
		let cand = w.advertise(&peer, w.leaf(), PARA);
		let _ = w.fetch_request(&cand);
	}
}

mod advertisement_spam_protection {
	use crate::common::{
		builders::{Candidate, ProtocolVersion::V2},
		chain::CoreSchedule,
		contract::Effect,
		harness::CollatorSut,
		world::{bootstrap_world, collator_world_config, World, WorldExt as _},
	};
	use polkadot_primitives::{CoreIndex, Id as ParaId};
	use std::time::Duration;

	const PARA: ParaId = ParaId::new(2000);

	#[crate::sim_test]
	fn re_advertising_after_can_second_false_does_not_refetch<S: CollatorSut>() {
		let config =
			collator_world_config().with_schedule(CoreIndex(0), CoreSchedule::always(PARA));
		let mut w: World<S> = bootstrap_world::<S>(config, Some(false));
		w.new_block().activate();

		let candidate = Candidate::for_para_at(PARA, w.leaf());
		let peer = w.declared_peer(PARA, V2);

		// First advertisement: CanSecond=false → drop.
		w.advertise_with_parent_head(
			&peer,
			w.leaf(),
			candidate.hash(),
			polkadot_primitives::HeadData(Vec::new()).hash(),
		);
		w.base.sim.advance(Duration::from_millis(100));
		w.base.sim.expect_count(
			|e| matches!(e, Effect::SendRequest { .. }),
			0,
			"SendRequest after CanSecond=false (must be zero)",
		);

		// Duplicate advertisement → must remain dropped. Both impls agree.
		w.advertise_with_parent_head(
			&peer,
			w.leaf(),
			candidate.hash(),
			polkadot_primitives::HeadData(Vec::new()).hash(),
		);
		w.base.sim.advance(Duration::from_millis(200));
		w.base.sim.expect_count(
			|e| matches!(e, Effect::SendRequest { .. }),
			0,
			"SendRequest after duplicate advertisement (must be zero — first dropped, second too)",
		);
	}
}

mod v1_advertise_on_non_leaf {
	use crate::common::{
		builders::ProtocolVersion::V1,
		chain::CoreSchedule,
		contract::Effect,
		harness::CollatorSut,
		world::{bootstrap_world, collator_world_config, World, WorldExt as _},
	};
	use polkadot_primitives::{CoreIndex, Id as ParaId};
	use std::time::Duration;

	const PARA: ParaId = ParaId::new(2000);

	#[crate::sim_test]
	fn v1_advertisement_at_parent_of_leaf_is_rejected<S: CollatorSut>() {
		let config =
			collator_world_config().with_schedule(CoreIndex(0), CoreSchedule::always(PARA));
		let mut w: World<S> = bootstrap_world::<S>(config, None);
		for _ in 0..2 {
			w.new_block().activate();
		}
		let parent = w.ancestors()[0];

		let peer = w.declared_peer(PARA, V1);
		w.base.sim.send(peer.advertise(parent, None, None));

		// No fetch should fire for the misuse advertisement.
		w.base.sim.expect_no(
			|e| matches!(e, Effect::SendRequest { .. }),
			Duration::from_millis(300),
			"SendRequest after V1 advertisement at non-leaf (must NOT fire)",
		);
	}
}

mod ah_permissionless {
	use crate::common::{
		builders::{Candidate, Peer, ProtocolVersion::V2},
		chain::CoreSchedule,
		contract::Effect,
		harness::CollatorSut,
		impls::{set_legacy_per_test_config, LegacyValidatorConfig},
		world::{bootstrap_world, collator_world_config, World, WorldExt as _},
	};

	/// The hold-off duration we **configure the subsystem with** for these tests — passed in via
	/// `ah_world(Some(HOLD_OFF))`, which sets `ProtocolSide::Validator::collator_protocol_hold_off`
	/// (the same operator-facing knob exposed as the `--collator-protocol-hold-off` CLI flag).
	///
	/// The specific value is arbitrary: the test does not assert any particular production duration
	/// (the prod default is unpinned tuning). It asserts only that whatever the operator configures
	/// is *honored* — the first permissionless AssetHub fetch is delayed by exactly this duration —
	/// and that the hold-off is applied at most once per relay parent.
	///
	/// Must be larger than `BYPASS_WINDOW` (see below) for the once-per-RP check to be meaningful.
	const HOLD_OFF: Duration = Duration::from_millis(200);
	use polkadot_node_network_protocol::{
		peer_set::{PeerSet, MAX_AUTHORITY_INCOMING_STREAMS},
		ObservedRole,
	};
	use polkadot_node_subsystem::messages::{CollatorProtocolMessage, NetworkBridgeEvent};
	use polkadot_primitives::{CoreIndex, Id as ParaId};
	use sc_network_types::PeerId;
	use std::{collections::HashSet, time::Duration};

	/// Asset Hub's para id. The legacy validator special-cases this exact value for its
	/// permissionless connection-limit gating, which is what these tests exercise.
	const PARA_AH: ParaId = ParaId::new(1000);

	/// Build a world configured for AH permissionless tests.
	///
	/// Pass `Some(d)` to override the hold-off duration (use `Duration::from_millis(0)` to
	/// disable hold-off entirely for connection-limit tests). `None` uses the production
	/// default (`HOLD_OFF_DURATION_DEFAULT_VALUE` — 50ms under `cfg(test)`).
	fn ah_world<S: CollatorSut>(
		invulnerables: HashSet<PeerId>,
		hold_off: Option<Duration>,
	) -> crate::common::world::World<S> {
		set_legacy_per_test_config(LegacyValidatorConfig {
			invulnerables,
			collator_protocol_hold_off: hold_off,
		});
		let config =
			collator_world_config().with_schedule(CoreIndex(0), CoreSchedule::always(PARA_AH));
		let mut w: World<S> = bootstrap_world::<S>(config, None);
		w.new_block()
			.with_claim_queue_at(CoreIndex(0), [PARA_AH, PARA_AH, PARA_AH])
			.activate();
		w
	}

	/// Mirrors `permissionless_collators_are_rejected_when_connection_limit_is_hit`.
	///
	/// Scenario only exercises the legacy validator: experimental's permissionless gating
	/// uses persistent reputation rather than a static connection-limit + invulnerables set,
	/// so the same shape would pass for an unrelated reason. Filter to legacy.
	#[crate::sim_test(only = "legacy")]
	fn permissionless_collators_are_rejected_when_connection_limit_is_hit<S: CollatorSut>() {
		let invulnerable = PeerId::random();
		let invulnerables = HashSet::from_iter([invulnerable]);
		let invulnerables_len = invulnerables.len() as u32;
		let mut w = ah_world::<S>(invulnerables, Some(Duration::from_millis(0)));

		// Accept up to `connection_limit` permissionless collators.
		let connection_limit = MAX_AUTHORITY_INCOMING_STREAMS - 10 - invulnerables_len;
		for _ in 0..connection_limit {
			let peer = Peer::new(PARA_AH, V2);
			w.base.sim.send(peer.connected());
			w.base.sim.send(peer.declare());
		}

		// Connecting one more permissionless collator should be rejected (DisconnectPeers).
		let extra = PeerId::random();
		w.base.sim.send(CollatorProtocolMessage::NetworkBridgeUpdate(
			NetworkBridgeEvent::PeerConnected(
				extra,
				ObservedRole::Full,
				polkadot_node_network_protocol::peer_set::CollationVersion::V2.into(),
				None,
			),
		));
		let _ = w.base.sim.expect(
			|e| {
				matches!(
					e,
					Effect::DisconnectPeers { peers, peer_set: PeerSet::Collation } if peers.contains(&extra),
				)
			},
			Duration::from_millis(200),
			"DisconnectPeers for the over-limit permissionless collator",
		);

		// An invulnerable collator can still connect+declare. Use the well-known peer id.
		let inv_peer = Peer::new(PARA_AH, V2).with_peer_id(invulnerable);
		w.base.sim.send(inv_peer.connected());
		w.base.sim.send(inv_peer.declare());
		w.expect_no_disconnect(&inv_peer, Duration::from_millis(200));
	}

	/// Mirrors `invulnerable_collations_are_preferred_over_permissionless_ones`.
	///
	/// With one invulnerable + one permissionless peer both advertising at the same RP, the
	/// validator picks the invulnerable's collation first.
	#[crate::sim_test(only = "legacy")]
	fn invulnerable_collations_are_preferred_over_permissionless_ones<S: CollatorSut>() {
		let invulnerable = PeerId::random();
		let mut w = ah_world::<S>(HashSet::from_iter([invulnerable]), None);
		let leaf = w.leaf();

		let inv_peer = Peer::new(PARA_AH, V2).with_peer_id(invulnerable);
		let perm_peer = Peer::new(PARA_AH, V2);
		for p in [&inv_peer, &perm_peer] {
			w.base.sim.send(p.connected());
			w.base.sim.send(p.declare());
		}

		// Build two distinct candidates for the same para; advertise both at the leaf.
		let inv_cand = w
			.candidate_at(leaf)
			.para(PARA_AH)
			.parent_head(polkadot_primitives::HeadData(Vec::new()))
			.head_data(polkadot_primitives::HeadData(vec![1]))
			.build();
		let perm_cand = w
			.candidate_at(leaf)
			.para(PARA_AH)
			.parent_head(polkadot_primitives::HeadData(Vec::new()))
			.head_data(polkadot_primitives::HeadData(vec![2]))
			.build();

		let inv_parent_hash = inv_cand.parent_head_hash();
		let perm_parent_hash = perm_cand.parent_head_hash();
		let inv_msg = inv_peer.advertise(leaf, Some(inv_cand.hash()), Some(inv_parent_hash));
		let perm_msg = perm_peer.advertise(leaf, Some(perm_cand.hash()), Some(perm_parent_hash));
		w.base.sim.send(inv_msg);
		w.base.sim.send(perm_msg);

		// First fetch must be for the invulnerable's candidate.
		let (first_peer, _, first_hash) = w.expect_any_fetch();
		assert_eq!(first_peer, invulnerable, "first fetch must target the invulnerable peer");
		assert_eq!(
			first_hash,
			Some(inv_cand.hash()),
			"first fetch must be invulnerable's candidate"
		);
	}

	/// Mirrors `permissionless_are_held_off_only_once`.
	///
	/// When no invulnerable shows up, a permissionless collator's first advertisement is
	/// held off for the configured hold-off (`HOLD_OFF`) then fetched. Subsequent advertisements
	/// from the same peer at the same RP fetch immediately (the hold-off is per-RP, not
	/// per-advertisement — once the hold-off completes for an RP, the per-RP state is `Done`
	/// and further advertisements bypass the gate).
	///
	/// First step asserts the held-off latency directly: the fetch's recorded sim_t must be
	/// `>= HOLD_OFF`. Second step (chained child candidate) confirms the bypass — fetch fires
	/// within a window much shorter than the hold-off.
	#[crate::sim_test(only = "legacy")]
	fn permissionless_are_held_off_only_once<S: CollatorSut>() {
		let invulnerable = PeerId::random();
		let mut w = ah_world::<S>(HashSet::from_iter([invulnerable]), Some(HOLD_OFF));
		let leaf = w.leaf();
		let perm_peer = w.declared_peer(PARA_AH, V2);

		// First permissionless advertisement is held off; its fetch must not arrive before
		// the hold-off duration elapses.
		let cand1 = w
			.candidate_at(leaf)
			.para(PARA_AH)
			.head_data(polkadot_primitives::HeadData(vec![0]))
			.build();
		w.outputs.insert(cand1.hash(), cand1.commitments.clone(), cand1.pvd.clone());
		w.advertise_with_parent_head(&perm_peer, leaf, cand1.hash(), cand1.parent_head_hash());

		let fetch_sim_t_before = w.base.sim.now_sim_t();
		let req1 = w.fetch_request(&cand1);
		let fetch_sim_t_after = w.base.sim.now_sim_t();
		assert!(
			fetch_sim_t_after - fetch_sim_t_before >= HOLD_OFF,
			"first permissionless fetch must wait at least {:?} (held-off); waited {:?}",
			HOLD_OFF,
			fetch_sim_t_after - fetch_sim_t_before,
		);
		w.respond_fetch_v2(req1, cand1.receipt.clone(), Candidate::empty_pov());
		w.expect_second(&cand1);
		// Let the validation cycle settle so the next advertisement is processed against a
		// clean per-RP state.
		w.base.sim.advance(Duration::from_millis(200));

		// Chain a child candidate (cand1's output_head becomes cand2's parent_head). The per-RP
		// hold-off state is `Done` once cand1 completed, so cand2's fetch must fire without any
		// further hold-off.
		let cand2 = w
			.candidate_at(leaf)
			.para(PARA_AH)
			.parent_head(cand1.output_head())
			.head_data(polkadot_primitives::HeadData(vec![1]))
			.build();
		w.outputs.insert(cand2.hash(), cand2.commitments.clone(), cand2.pvd.clone());
		w.advertise_with_parent_head(&perm_peer, leaf, cand2.hash(), cand2.parent_head_hash());

		// `BYPASS_WINDOW` is the maximum simulated time we allow cand2's fetch to take. The
		// `expect` below advances the deterministic clock by up to this much, looking for the
		// fetch. It is the discriminator for "no second hold-off":
		//   - must be > 0, so the `expect` actually advances and can observe the already-armed
		//     fetch;
		//   - must be < `HOLD_OFF`, so that IF the bug were present (a second hold-off wrongly
		//     armed), cand2's fetch would be delayed by `HOLD_OFF` and not appear within this
		//     window — making the `expect` time out and the test fail.
		// The exact value is arbitrary between those bounds.
		const BYPASS_WINDOW: Duration = Duration::from_millis(50);
		assert!(BYPASS_WINDOW < HOLD_OFF, "bypass window must be shorter than the hold-off");
		let _req2 = w.base.sim.expect(
		|e| matches!(
			e,
			crate::common::contract::Effect::SendRequest { candidate_hash: Some(c), .. } if *c == cand2.hash(),
		),
		BYPASS_WINDOW,
		"cand2 fetch fires without a further hold-off (within a window shorter than HOLD_OFF)",
	);
	}
}
