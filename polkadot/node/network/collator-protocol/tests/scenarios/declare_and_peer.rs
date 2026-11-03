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

//! Declare validation, peer-disconnect, view-change.

mod bad_signature {
	use crate::common::{
		builders::ProtocolVersion::V1, contract::RepBucket, harness::CollatorSut,
		world::activated_world,
	};
	use polkadot_primitives::{CoreIndex, Id as ParaId};

	const PARA: ParaId = ParaId::new(2000);

	/// Legacy-only, and intentionally so. The `Declare` signature is self-signed: the peer
	/// supplies both the collator public key and a signature over its own peer id by that
	/// key. The validator has no allowlist of known collator keys to check the public key
	/// against (the invulnerables set is keyed by `PeerId`, not collator key), so a valid
	/// signature proves only "I hold the private key for a key I just generated" — it
	/// authenticates nothing. Legacy nonetheless verifies it and slashes a bad signature as
	/// `Malicious`; experimental correctly skips the pointless check. Intended divergence,
	/// not a bug — the signature should be dropped from the message entirely (v4 protocol).
	#[crate::sim_test(only = "legacy")]
	fn declare_with_bad_signature_yields_malicious_reputation<S: CollatorSut>() {
		let mut w = activated_world::<S>(&[(CoreIndex(0), PARA)]);
		let peer = w.connected_peer(PARA, V1);
		w.base.sim.send(peer.declare_with_bad_signature());
		w.expect_rep(&peer, RepBucket::Malicious);
	}

	/// Legacy sanity counterpart: a *valid* declare must NOT trip the malicious bucket,
	/// ruling out "any declare in this setup yields `Reputation::Malicious`" as a false
	/// positive for the bad-signature test above. Legacy-only for the same reason — only
	/// legacy verifies the (meaningless) `Declare` signature at all.
	#[crate::sim_test(only = "legacy")]
	fn declare_with_valid_signature_does_not_get_malicious_reputation<S: CollatorSut>() {
		let mut w = activated_world::<S>(&[(CoreIndex(0), PARA)]);
		let _peer = w.declared_peer(PARA, V1);
		w.expect_no_rep(&_peer, RepBucket::Malicious);
	}
}

mod disconnect_if_wrong_declare {
	use crate::common::{
		builders::ProtocolVersion::V1, harness::CollatorSut, world::activated_world,
	};
	use polkadot_primitives::{CoreIndex, Id as ParaId};
	use std::time::Duration;

	const SCHEDULED: ParaId = ParaId::new(2000);
	const WRONG: ParaId = ParaId::new(3000);

	#[crate::sim_test]
	fn peer_disconnected_after_declaring_for_wrong_para<S: CollatorSut>() {
		let mut w = activated_world::<S>(&[(CoreIndex(0), SCHEDULED)]);
		let peer = w.declared_peer(WRONG, V1);
		w.expect_disconnect(&peer);
	}

	#[crate::sim_test]
	fn peer_with_correct_declare_is_not_disconnected<S: CollatorSut>() {
		let mut w = activated_world::<S>(&[(CoreIndex(0), SCHEDULED)]);
		let peer = w.declared_peer(SCHEDULED, V1);
		w.expect_no_disconnect(&peer, Duration::from_millis(200));
	}
}

mod malicious_para {
	use crate::common::{
		builders::ProtocolVersion::V2, harness::CollatorSut, world::activated_world,
	};
	use polkadot_primitives::{CoreIndex, Id as ParaId};

	const SCHEDULED: ParaId = ParaId::new(2000);
	const UNSCHEDULED: ParaId = ParaId::new(3000);

	#[crate::sim_test]
	fn declare_for_unscheduled_para_disconnects_peer<S: CollatorSut>() {
		let mut w = activated_world::<S>(&[(CoreIndex(0), SCHEDULED)]);
		let peer = w.declared_peer(UNSCHEDULED, V2);
		w.expect_disconnect(&peer);
	}
}

mod unneeded_para {
	use crate::common::{
		builders::ProtocolVersion::V1, harness::CollatorSut, world::activated_world,
	};
	use polkadot_primitives::Id as ParaId;

	#[crate::sim_test]
	fn declare_for_unneeded_para_disconnects_peer<S: CollatorSut>() {
		let mut w = activated_world::<S>(&[]); // empty schedule = nothing scheduled
		let peer = w.declared_peer(ParaId::from(2000), V1);
		w.expect_disconnect(&peer);
	}
}

mod peer_disconnect_clears_queue {
	use crate::common::{
		builders::{Candidate, ProtocolVersion::V1},
		contract::Effect,
		harness::CollatorSut,
		world::{activated_world, WorldExt as _},
	};
	use polkadot_node_subsystem::messages::{CollatorProtocolMessage, NetworkBridgeEvent};
	use polkadot_primitives::{CoreIndex, Id as ParaId};
	use std::time::Duration;

	const PARA: ParaId = ParaId::new(2000);

	#[crate::sim_test]
	fn disconnect_clears_queued_advertisement<S: CollatorSut>() {
		let mut w = activated_world::<S>(&[(CoreIndex(0), PARA)]);
		let leaf = w.leaf();
		let leaf_n = w.leaf_number();

		// Build a candidate consistent with the framework's empty-parent-head PVD.
		let candidate = Candidate::builder()
			.para(PARA)
			.relay_parent(leaf)
			.relay_parent_number(leaf_n)
			.build();

		let peer_a = w.declared_peer(PARA, V1);
		let peer_b = w.declared_peer(PARA, V1);

		// peer_a advertises; first fetch fires for peer_a (only declared peer with an ad).
		w.base.sim.send(peer_a.advertise(leaf, None, None));
		let request_id = w.fetch_request(&candidate);

		// peer_b queues behind peer_a, then disconnects.
		w.base.sim.send(peer_b.advertise(leaf, None, None));
		w.base.sim.send(CollatorProtocolMessage::NetworkBridgeUpdate(
			NetworkBridgeEvent::PeerDisconnected(peer_b.peer_id),
		));

		// peer_a's fetch resolves valid → seconding emits.
		w.respond_fetch_v1(request_id, candidate.receipt.clone(), Candidate::empty_pov());
		w.expect_second(&candidate);

		// No fetch ever targets peer_b.
		w.base.sim.expect_no(
			|e| matches!(e, Effect::SendRequest { to, .. } if *to == peer_b.peer_id),
			Duration::from_millis(100),
			"SendRequest targeting peer_b after peer_b disconnected its advertisement",
		);
	}
}

mod view_change_disconnects {
	use crate::common::{
		builders::ProtocolVersion::V1, harness::CollatorSut, world::activated_world,
	};
	use polkadot_node_network_protocol::OurView;
	use polkadot_node_subsystem::messages::{CollatorProtocolMessage, NetworkBridgeEvent};
	use polkadot_primitives::{CoreIndex, Id as ParaId};
	use std::time::Duration;

	const PARA: ParaId = ParaId::new(2000);

	#[crate::sim_test]
	fn empty_view_disconnects_declared_peer<S: CollatorSut>() {
		let mut w = activated_world::<S>(&[(CoreIndex(0), PARA)]);
		let peer = w.declared_peer(PARA, V1);

		w.base.sim.send(CollatorProtocolMessage::NetworkBridgeUpdate(
			NetworkBridgeEvent::OurViewChange(OurView::new(std::iter::empty(), 0)),
		));

		w.expect_disconnect(&peer);
	}

	#[crate::sim_test]
	fn declared_peer_stays_connected_when_view_unchanged<S: CollatorSut>() {
		let mut w = activated_world::<S>(&[(CoreIndex(0), PARA)]);
		let peer = w.declared_peer(PARA, V1);
		w.expect_no_disconnect(&peer, Duration::from_millis(200));
	}
}
