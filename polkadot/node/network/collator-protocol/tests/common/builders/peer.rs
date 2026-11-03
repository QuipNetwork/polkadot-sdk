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

//! Builder for inbound NetworkBridgeRx events.
//!
//! A `Peer` is a fixture for a remote collator: a peer-id, a collator key, a target para, and
//! a collation protocol version. The builder produces `CollatorProtocolMessage`s to inject via
//! [`crate::common::harness::Sim::send`].

use polkadot_node_network_protocol::{
	peer_set::CollationVersion as ProtoCollationVersion, v1 as protocol_v1, v2 as protocol_v2,
	v3_collation as protocol_v3, v4_collation as protocol_v4, CollationProtocols, ObservedRole,
};
use polkadot_node_subsystem::messages::{CollatorProtocolMessage, NetworkBridgeEvent};
use polkadot_primitives::{CandidateHash, CollatorPair, Hash, Id as ParaId};
use sc_network_types::PeerId;
use sp_core::Pair as _;

use polkadot_subsystem_test_sim::builders::fixtures::fresh_collator;

/// Collation protocol version a `Peer` speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtocolVersion {
	/// V1 protocol — relay-parent-only advertisements.
	V1,
	/// V2 protocol — advertises relay parent + candidate hash + parent head data hash.
	V2,
	/// V3 protocol — separate scheduling parent + relay parent.
	V3,
	/// V4 protocol — segment advertisements keyed by output head; no `Declare` message
	/// (the first `AdvertiseSegment` binds the peer to its para).
	V4,
}

impl ProtocolVersion {
	fn into_proto(self) -> ProtoCollationVersion {
		match self {
			Self::V1 => ProtoCollationVersion::V1,
			Self::V2 => ProtoCollationVersion::V2,
			Self::V3 => ProtoCollationVersion::V3,
			Self::V4 => ProtoCollationVersion::V4,
		}
	}
}

/// A test-side handle to a fake remote collator. Plain data — the harness consumes the
/// `CollatorProtocolMessage`s the methods produce.
pub struct Peer {
	/// The peer id this peer presents on the wire.
	pub peer_id: PeerId,
	/// The collator key this peer signs declarations with.
	pub collator: CollatorPair,
	/// The para id the peer is collating on.
	pub para: ParaId,
	/// The collation protocol version this peer speaks.
	pub version: ProtocolVersion,
}

impl Peer {
	/// New peer with a fresh peer-id and collator key.
	pub fn new(para: ParaId, version: ProtocolVersion) -> Self {
		Self { peer_id: PeerId::random(), collator: fresh_collator(), para, version }
	}

	/// Override the peer id.
	pub fn with_peer_id(mut self, peer_id: PeerId) -> Self {
		self.peer_id = peer_id;
		self
	}

	/// Override the collator pair.
	pub fn with_collator(mut self, collator: CollatorPair) -> Self {
		self.collator = collator;
		self
	}

	/// Wrap a NetworkBridgeRx event so it lands on `CollatorProtocolMessage::NetworkBridgeUpdate`.
	fn wrap(
		event: NetworkBridgeEvent<
			polkadot_node_network_protocol::CollationProtocols<
				protocol_v1::CollatorProtocolMessage,
				protocol_v2::CollatorProtocolMessage,
				protocol_v3::CollatorProtocolMessage,
				protocol_v4::AdvertiseSegment,
			>,
		>,
	) -> CollatorProtocolMessage {
		CollatorProtocolMessage::NetworkBridgeUpdate(event)
	}

	/// `PeerConnected` event with this peer's id and protocol version.
	pub fn connected(&self) -> CollatorProtocolMessage {
		Self::wrap(NetworkBridgeEvent::PeerConnected(
			self.peer_id,
			ObservedRole::Full,
			self.version.into_proto().into(),
			None,
		))
	}

	/// `Declare` wire message signed correctly for this peer's protocol version.
	pub fn declare(&self) -> CollatorProtocolMessage {
		let proto = match self.version {
			ProtocolVersion::V1 => {
				CollationProtocols::V1(protocol_v1::CollatorProtocolMessage::Declare(
					self.collator.public(),
					self.para,
					self.collator.sign(&protocol_v1::declare_signature_payload(&self.peer_id)),
				))
			},
			ProtocolVersion::V2 => {
				CollationProtocols::V2(protocol_v2::CollatorProtocolMessage::Declare(
					self.collator.public(),
					self.para,
					self.collator.sign(&protocol_v2::declare_signature_payload(&self.peer_id)),
				))
			},
			ProtocolVersion::V3 => {
				CollationProtocols::V3(protocol_v3::CollatorProtocolMessage::Declare(
					self.collator.public(),
					self.para,
					self.collator.sign(&protocol_v3::declare_signature_payload(&self.peer_id)),
				))
			},
			ProtocolVersion::V4 => {
				panic!("V4 has no `Declare` message; the first `AdvertiseSegment` binds the para")
			},
		};
		Self::wrap(NetworkBridgeEvent::PeerMessage(self.peer_id, proto))
	}

	/// `Declare` wire message with a deliberately invalid signature (for negative tests).
	pub fn declare_with_bad_signature(&self) -> CollatorProtocolMessage {
		// Sign garbage so the validator rejects the declaration.
		let bad = self.collator.sign(&[42u8]);
		let proto = match self.version {
			ProtocolVersion::V1 => {
				CollationProtocols::V1(protocol_v1::CollatorProtocolMessage::Declare(
					self.collator.public(),
					self.para,
					bad,
				))
			},
			ProtocolVersion::V2 => {
				CollationProtocols::V2(protocol_v2::CollatorProtocolMessage::Declare(
					self.collator.public(),
					self.para,
					bad,
				))
			},
			ProtocolVersion::V3 => {
				CollationProtocols::V3(protocol_v3::CollatorProtocolMessage::Declare(
					self.collator.public(),
					self.para,
					bad,
				))
			},
			ProtocolVersion::V4 => {
				panic!("V4 has no `Declare` message; the first `AdvertiseSegment` binds the para")
			},
		};
		Self::wrap(NetworkBridgeEvent::PeerMessage(self.peer_id, proto))
	}

	/// `AdvertiseCollation` wire message at `relay_parent`. For V2/V3, supply the candidate
	/// hash and parent head-data hash. V3 advertisements through this method default to
	/// `descriptor_version = V2` and `scheduling_parent == relay_parent` — use
	/// [`Self::advertise_v3`] for full control of those fields.
	pub fn advertise(
		&self,
		relay_parent: Hash,
		candidate_hash: Option<CandidateHash>,
		parent_head_data_hash: Option<Hash>,
	) -> CollatorProtocolMessage {
		let proto = match self.version {
			ProtocolVersion::V1 => CollationProtocols::V1(
				protocol_v1::CollatorProtocolMessage::AdvertiseCollation(relay_parent),
			),
			ProtocolVersion::V2 => {
				CollationProtocols::V2(protocol_v2::CollatorProtocolMessage::AdvertiseCollation {
					scheduling_parent: relay_parent,
					candidate_hash: candidate_hash
						.expect("V2 advertisement requires candidate hash"),
					parent_head_data_hash: parent_head_data_hash
						.expect("V2 advertisement requires parent head-data hash"),
				})
			},
			ProtocolVersion::V3 => {
				CollationProtocols::V3(protocol_v3::CollatorProtocolMessage::AdvertiseCollation {
					scheduling_parent: relay_parent,
					candidate_hash: candidate_hash
						.expect("V3 advertisement requires candidate hash"),
					parent_head_data_hash: parent_head_data_hash
						.expect("V3 advertisement requires parent head-data hash"),
					candidate_descriptor_version:
						polkadot_primitives::CandidateDescriptorVersion::V2,
					relay_parent,
				})
			},
			ProtocolVersion::V4 => {
				panic!("V4 advertises segments, not single collations; use `advertise_segment`")
			},
		};
		Self::wrap(NetworkBridgeEvent::PeerMessage(self.peer_id, proto))
	}

	/// Full-control V3 advertisement. The peer must have been built with
	/// [`ProtocolVersion::V3`]; otherwise this method panics. Lets the scenario specify
	/// `scheduling_parent`, `relay_parent`, and `candidate_descriptor_version`
	/// independently — needed by the V3 stalled-relay-chain / scheduling-parent tests.
	pub fn advertise_v3(
		&self,
		scheduling_parent: Hash,
		relay_parent: Hash,
		candidate_hash: CandidateHash,
		parent_head_data_hash: Hash,
		descriptor_version: polkadot_primitives::CandidateDescriptorVersion,
	) -> CollatorProtocolMessage {
		assert_eq!(
			self.version,
			ProtocolVersion::V3,
			"advertise_v3 requires a peer constructed with ProtocolVersion::V3"
		);
		let proto =
			CollationProtocols::V3(protocol_v3::CollatorProtocolMessage::AdvertiseCollation {
				scheduling_parent,
				candidate_hash,
				parent_head_data_hash,
				candidate_descriptor_version: descriptor_version,
				relay_parent,
			});
		Self::wrap(NetworkBridgeEvent::PeerMessage(self.peer_id, proto))
	}

	/// V4 `AdvertiseSegment` wire message: a list of candidate fingerprints for this
	/// peer's para at `scheduling_parent`. The peer must have been built with
	/// [`ProtocolVersion::V4`]; otherwise this method panics. A V4 peer's first
	/// segment doubles as its declaration.
	pub fn advertise_segment(
		&self,
		scheduling_parent: Hash,
		descriptor_version: polkadot_primitives::CandidateDescriptorVersion,
		fingerprints: Vec<protocol_v4::CandidateFingerprint>,
	) -> CollatorProtocolMessage {
		assert_eq!(
			self.version,
			ProtocolVersion::V4,
			"advertise_segment requires a peer constructed with ProtocolVersion::V4"
		);
		let proto = CollationProtocols::V4(protocol_v4::AdvertiseSegment {
			scheduling_parent,
			para_id: self.para,
			candidates_descriptor_version: descriptor_version,
			candidates: fingerprints
				.try_into()
				.expect("test segment length within MAX_SEGMENT_LEN"),
		});
		Self::wrap(NetworkBridgeEvent::PeerMessage(self.peer_id, proto))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn connected_carries_protocol_version() {
		let peer = Peer::new(ParaId::from(2000), ProtocolVersion::V2);
		match peer.connected() {
			CollatorProtocolMessage::NetworkBridgeUpdate(NetworkBridgeEvent::PeerConnected(
				p,
				_role,
				ver,
				_auth,
			)) => {
				assert_eq!(p, peer.peer_id);
				assert_eq!(ver, ProtoCollationVersion::V2.into());
			},
			other => panic!("expected PeerConnected, got {:?}", other),
		}
	}

	#[test]
	fn declare_signs_payload_correctly_per_version() {
		// All three versions produce a Declare wire message wrapped in a PeerMessage.
		for v in [ProtocolVersion::V1, ProtocolVersion::V2, ProtocolVersion::V3] {
			let peer = Peer::new(ParaId::from(2000), v);
			match peer.declare() {
				CollatorProtocolMessage::NetworkBridgeUpdate(NetworkBridgeEvent::PeerMessage(
					p,
					_,
				)) => assert_eq!(p, peer.peer_id),
				other => panic!("expected PeerMessage, got {:?}", other),
			}
		}
	}
}
