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

//! Classify outgoing `AllMessages` into [`Effect`] (asserted) or [`Query`] (mock-answered).
//!
//! A single `AllMessages` can correspond to several entries in the observation stream — most
//! commonly because a single `NetworkBridgeTxMessage::SendRequests` carries multiple `Requests`,
//! or `SendCollationMessages` carries multiple peer-batches. The classifier returns a flat
//! `Vec<Classified>` accordingly; callers iterate.
//!
//! The outer `AllMessages` enum is large and includes families the collator-protocol does not
//! interact with at all (`ApprovalDistribution`, `BitfieldDistribution`, ...). The outer match
//! therefore keeps a wildcard arm that panics with a contract-violation message; that's the
//! intended failure mode for genuinely undeclared egress.
//!
//! Inner matches on small, stable enums (`NetworkBridgeTxMessage`, `Requests`,
//! `ReportPeerMessage`) are **exhaustive** — when an upstream variant is added, the build
//! breaks on the next compile and we make a conscious decision rather than discovering it via
//! a panicked test.
//!
//! See `polkadot/node/network/collator-protocol/src/...` for the production emission sites
//! that ground each variant.

use crate::{
	contract::{
		effect::{AdvertisementSummary, Effect, ReqKind, WireMsgKind},
		query::Query,
		reputation::RepBucket,
	},
	harness::pending_fetches::PendingFetches,
};
use polkadot_node_network_protocol::{
	request_response::Requests, v1 as protocol_v1, v2 as protocol_v2, v3_collation as protocol_v3,
	v4_collation as protocol_v4, CollationProtocols,
};
use polkadot_node_subsystem::messages::{
	AllMessages, CandidateBackingMessage, NetworkBridgeTxMessage, ReportPeerMessage,
};
use std::collections::BTreeSet;

/// Result of classifying a single outgoing `AllMessages`. One message may classify into
/// multiple entries (see module docs); callers iterate the returned `Vec`.
#[derive(Debug)]
pub enum Classified {
	/// The message is an observable effect — record it.
	Effect(Effect),
	/// The message is an information-gathering query — forward to responder.
	Query(Query),
}

/// Peek at an outgoing `AllMessages` without consuming it and emit any [`Effect`] descriptions
/// implied by the message.
///
/// Used by the router for **dual-delivery** messages: outbound messages that are *both* an
/// observable Effect tests assert on *and* an input to a real auxiliary subsystem (e.g.
/// `CandidateBackingMessage::Second{...}`). The router records these effects manually when an
/// aux slot accepts the message; otherwise the regular [`classify`] path runs and emits them
/// in the consume direction.
///
/// Returns an empty `Vec` for variants that have no Effect interpretation (queries, plain
/// aux-only messages).
pub fn peek_effects(msg: &AllMessages) -> Vec<Effect> {
	match msg {
		AllMessages::CandidateBacking(CandidateBackingMessage::Second {
			scheduling_parent,
			candidate,
			..
		}) => vec![Effect::SecondCandidate {
			scheduling_parent: *scheduling_parent,
			candidate_hash: candidate.hash(),
			para: candidate.descriptor.para_id(),
		}],
		_ => Vec::new(),
	}
}

/// Walk an outgoing `AllMessages`, classify it. Panics on undeclared egress (a contract
/// violation, not a test bug).
///
/// `pending` is the side table where outgoing-fetch response senders are parked: any
/// `NetworkBridgeTxMessage::SendRequests` produces an `Effect::SendRequest` with a fresh
/// [`RequestId`] and the embedded `oneshot::Sender` is moved into `pending` so tests can
/// later resolve it via `Sim::respond_fetch`.
///
/// [`RequestId`]: crate::contract::RequestId
pub fn classify(msg: AllMessages, pending: &mut PendingFetches) -> Vec<Classified> {
	match msg {
		AllMessages::CandidateBacking(inner) => from_candidate_backing(inner),
		AllMessages::NetworkBridgeTx(inner) => from_network_bridge_tx(inner, pending),
		AllMessages::RuntimeApi(inner) => vec![Classified::Query(Query::Runtime(inner))],
		AllMessages::ChainApi(inner) => vec![Classified::Query(Query::ChainApi(inner))],
		// A prospective-parachains message is a query family some configs answer (e.g. a
		// `QueryScript` with a `.prospective(..)` handler, or prospective-parachains itself
		// under test). In the collator config it is instead consumed by a real
		// prospective-parachains aux slot, so it should never reach `classify` — the router
		// only falls through to here when no slot claimed the message. If it does reach here
		// in a collator scenario, the responder chain has no prospective handler and the tail
		// `PanicResponder` will surface it by name: the fix is to register the prospective aux
		// slot, not to script the query.
		AllMessages::ProspectiveParachains(inner) => {
			vec![Classified::Query(Query::Prospective(inner))]
		},
		other => panic!(
			"collator-protocol emitted undeclared egress: {:?}\n\
			 If this is a legitimate effect, add a variant to `Effect` and a classifier arm.",
			other
		),
	}
}

fn from_candidate_backing(msg: CandidateBackingMessage) -> Vec<Classified> {
	match msg {
		CandidateBackingMessage::Second { scheduling_parent, candidate, pvd: _, pov: _ } => {
			vec![Classified::Effect(Effect::SecondCandidate {
				scheduling_parent,
				candidate_hash: candidate.hash(),
				para: candidate.descriptor.para_id(),
			})]
		},
		msg @ CandidateBackingMessage::CanSecond(..) => {
			vec![Classified::Query(Query::CanSecond(msg))]
		},
		// Other CandidateBackingMessage variants (`GetBackableCandidates`, `Statement`) are not
		// emitted by the collator-protocol.
		other => panic!(
			"collator-protocol emitted unexpected CandidateBackingMessage variant: {:?}",
			other
		),
	}
}

fn from_network_bridge_tx(
	msg: NetworkBridgeTxMessage,
	pending: &mut PendingFetches,
) -> Vec<Classified> {
	match msg {
		NetworkBridgeTxMessage::ReportPeer(report) => from_report_peer(report),
		NetworkBridgeTxMessage::DisconnectPeers(peers, peer_set) => {
			vec![Classified::Effect(Effect::DisconnectPeers {
				peers: peers.into_iter().collect::<BTreeSet<_>>(),
				peer_set,
			})]
		},
		NetworkBridgeTxMessage::ConnectToValidators { validator_ids, peer_set, failed: _ } => {
			vec![Classified::Effect(Effect::ConnectValidators {
				validator_ids: validator_ids.into_iter().collect::<BTreeSet<_>>(),
				peer_set,
			})]
		},
		NetworkBridgeTxMessage::SendCollationMessage(peers, proto) => {
			let kind = wire_kind_from_collation_protocol(&proto);
			vec![Classified::Effect(Effect::SendCollation { peers, kind })]
		},
		NetworkBridgeTxMessage::SendCollationMessages(batches) => batches
			.into_iter()
			.map(|(peers, proto)| {
				let kind = wire_kind_from_collation_protocol(&proto);
				Classified::Effect(Effect::SendCollation { peers, kind })
			})
			.collect(),
		NetworkBridgeTxMessage::SendRequests(requests, _) => {
			requests.into_iter().map(|r| classify_request(r, pending)).collect()
		},

		// The collator-protocol never sends validation-protocol messages or connect/extend
		// resolved-validators commands. If that changes, we want to know at compile time, not
		// at panic time.
		NetworkBridgeTxMessage::SendValidationMessage(..) |
		NetworkBridgeTxMessage::SendValidationMessages(..) |
		NetworkBridgeTxMessage::ConnectToResolvedValidators { .. } |
		NetworkBridgeTxMessage::AddToResolvedValidators { .. } => {
			panic!("collator-protocol emitted unexpected NetworkBridgeTxMessage variant: {:?}", msg)
		},
	}
}

fn from_report_peer(msg: ReportPeerMessage) -> Vec<Classified> {
	match msg {
		ReportPeerMessage::Single(peer, change) => vec![Classified::Effect(Effect::Reputation {
			peer,
			bucket: RepBucket::from_raw(&change),
		})],
		ReportPeerMessage::Batch(map) => map
			.into_iter()
			// A net-zero accumulated magnitude is a no-op (offsetting cost + benefit) — drop
			// it rather than emit a spurious `Reputation` effect. Non-zero magnitudes bucket
			// via the same logic as the single-report path.
			.filter_map(|(peer, magnitude)| {
				RepBucket::from_magnitude(magnitude)
					.map(|bucket| Classified::Effect(Effect::Reputation { peer, bucket }))
			})
			.collect(),
	}
}

fn classify_request(req: Requests, pending: &mut PendingFetches) -> Classified {
	match req {
		Requests::CollationFetchingV1(out) => {
			let to = recipient_to_peer_id(&out.peer);
			let request_id = pending.register(out.pending_response);
			Classified::Effect(Effect::SendRequest {
				request_id,
				to,
				kind: ReqKind::CollationFetchingV1,
				candidate_hash: None,
			})
		},
		Requests::CollationFetchingV2(out) => {
			let to = recipient_to_peer_id(&out.peer);
			let candidate_hash = out.payload.candidate_hash;
			let request_id = pending.register(out.pending_response);
			Classified::Effect(Effect::SendRequest {
				request_id,
				to,
				kind: ReqKind::CollationFetchingV2,
				candidate_hash: Some(candidate_hash),
			})
		},
		Requests::CollationFetchingV3(out) => {
			let to = recipient_to_peer_id(&out.peer);
			let output_head_data_hash = out.payload.output_head_data_hash;
			let request_id = pending.register(out.pending_response);
			Classified::Effect(Effect::SendRequestV3 {
				request_id,
				to,
				kind: ReqKind::CollationFetchingV3,
				output_head_data_hash,
			})
		},

		// Other request kinds are emitted by other subsystems, never by collator-protocol.
		// Exhaustive arms here mean a new variant on the upstream `Requests` enum breaks the
		// build until we've consciously routed it.
		Requests::ChunkFetching(_) |
		Requests::PoVFetchingV1(_) |
		Requests::AvailableDataFetchingV1(_) |
		Requests::DisputeSendingV1(_) |
		Requests::AttestedCandidateV2(_) => {
			panic!("collator-protocol emitted unexpected request kind: {:?}", req)
		},
	}
}

fn wire_kind_from_collation_protocol(
	proto: &polkadot_node_network_protocol::VersionedCollationProtocol,
) -> WireMsgKind {
	use polkadot_node_network_protocol::{
		v1::CollationProtocol as V1, v2::CollationProtocol as V2,
		v3_collation::CollationProtocol as V3,
	};
	match proto {
		CollationProtocols::V1(V1::CollatorProtocol(msg)) => match msg {
			protocol_v1::CollatorProtocolMessage::Declare(_, para, _) => {
				WireMsgKind::Declare { para: *para }
			},
			protocol_v1::CollatorProtocolMessage::AdvertiseCollation(rp) => {
				WireMsgKind::Advertise {
					summary: AdvertisementSummary {
						scheduling_parent: *rp,
						candidate_hash: None,
						parent_head_hash: None,
						output_head_hash: None,
					},
				}
			},
			protocol_v1::CollatorProtocolMessage::CollationSeconded(rp, _) => {
				WireMsgKind::CollationSeconded { relay_parent: *rp }
			},
		},
		CollationProtocols::V2(V2::CollatorProtocol(msg)) => match msg {
			protocol_v2::CollatorProtocolMessage::Declare(_, para, _) => {
				WireMsgKind::Declare { para: *para }
			},
			protocol_v2::CollatorProtocolMessage::AdvertiseCollation {
				scheduling_parent,
				candidate_hash,
				parent_head_data_hash,
			} => WireMsgKind::Advertise {
				summary: AdvertisementSummary {
					scheduling_parent: *scheduling_parent,
					candidate_hash: Some(*candidate_hash),
					parent_head_hash: Some(*parent_head_data_hash),
					output_head_hash: None,
				},
			},
			protocol_v2::CollatorProtocolMessage::CollationSeconded(rp, _) => {
				WireMsgKind::CollationSeconded { relay_parent: *rp }
			},
		},
		CollationProtocols::V3(V3::CollatorProtocol(msg)) => match msg {
			protocol_v3::CollatorProtocolMessage::Declare(_, para, _) => {
				WireMsgKind::Declare { para: *para }
			},
			protocol_v3::CollatorProtocolMessage::AdvertiseCollation {
				scheduling_parent,
				candidate_hash,
				parent_head_data_hash,
				..
			} => WireMsgKind::Advertise {
				summary: AdvertisementSummary {
					scheduling_parent: *scheduling_parent,
					candidate_hash: Some(*candidate_hash),
					parent_head_hash: Some(*parent_head_data_hash),
					output_head_hash: None,
				},
			},
			protocol_v3::CollatorProtocolMessage::CollationSeconded(rp, _) => {
				WireMsgKind::CollationSeconded { relay_parent: *rp }
			},
		},
		CollationProtocols::V4(protocol_v4::AdvertiseSegment {
			scheduling_parent,
			candidates,
			..
		}) => {
			let tip =
				candidates.last().expect("subsystem never emits an empty segment advertisement");
			WireMsgKind::Advertise {
				summary: AdvertisementSummary {
					scheduling_parent: *scheduling_parent,
					candidate_hash: None,
					parent_head_hash: Some(tip.parent_head_data_hash),
					output_head_hash: Some(tip.output_head_data_hash),
				},
			}
		},
	}
}

fn recipient_to_peer_id(
	recipient: &polkadot_node_network_protocol::request_response::outgoing::Recipient,
) -> sc_network_types::PeerId {
	use polkadot_node_network_protocol::request_response::outgoing::Recipient;
	match recipient {
		Recipient::Peer(p) => *p,
		Recipient::Authority(_) => {
			panic!("collator-protocol fetches always target a PeerId, not an AuthorityDiscoveryId")
		},
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use polkadot_node_network_protocol::peer_set::PeerSet;
	use polkadot_node_subsystem::messages::ChainApiMessage;
	use polkadot_primitives::Hash;

	fn one(c: Vec<Classified>) -> Classified {
		assert_eq!(c.len(), 1, "expected exactly one classified entry");
		c.into_iter().next().unwrap()
	}

	#[test]
	fn report_peer_single_classifies_as_reputation_effect() {
		let peer = sc_network_types::PeerId::random();
		let change = polkadot_node_network_protocol::ReputationChange::new(i32::MIN, "bad");
		let msg = AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(
			ReportPeerMessage::Single(peer, change),
		));
		match one(classify(msg, &mut PendingFetches::new())) {
			Classified::Effect(Effect::Reputation { peer: p, bucket }) => {
				assert_eq!(p, peer);
				assert_eq!(bucket, RepBucket::Malicious);
			},
			other => panic!("unexpected classification: {:?}", other),
		}
	}

	#[test]
	fn report_peer_batch_emits_one_effect_per_peer() {
		let p1 = sc_network_types::PeerId::random();
		let p2 = sc_network_types::PeerId::random();
		let p3 = sc_network_types::PeerId::random();
		let mut map = std::collections::HashMap::new();
		map.insert(p1, -100i32);
		map.insert(p2, 100i32);
		map.insert(p3, i32::MIN);
		let msg = AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(
			ReportPeerMessage::Batch(map),
		));
		let out = classify(msg, &mut PendingFetches::new());
		assert_eq!(out.len(), 3);
		// Bucket per peer is preserved.
		let buckets: Vec<RepBucket> = out
			.into_iter()
			.map(|c| match c {
				Classified::Effect(Effect::Reputation { bucket, .. }) => bucket,
				other => panic!("expected Reputation effect, got {:?}", other),
			})
			.collect();
		assert!(buckets.contains(&RepBucket::Performance));
		assert!(buckets.contains(&RepBucket::Benefit));
		assert!(buckets.contains(&RepBucket::Malicious));
	}

	#[test]
	fn disconnect_peers_classifies_as_disconnect_effect() {
		let peer_a = sc_network_types::PeerId::random();
		let peer_b = sc_network_types::PeerId::random();
		let msg = AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::DisconnectPeers(
			vec![peer_a, peer_b],
			PeerSet::Collation,
		));
		match one(classify(msg, &mut PendingFetches::new())) {
			Classified::Effect(Effect::DisconnectPeers { peers, peer_set }) => {
				assert_eq!(peers.len(), 2);
				assert_eq!(peer_set, PeerSet::Collation);
			},
			other => panic!("unexpected classification: {:?}", other),
		}
	}

	#[test]
	fn chain_api_classifies_as_query() {
		let (tx, _rx) = futures::channel::oneshot::channel();
		let msg = AllMessages::ChainApi(ChainApiMessage::FinalizedBlockNumber(tx));
		match one(classify(msg, &mut PendingFetches::new())) {
			Classified::Query(Query::ChainApi(_)) => {},
			other => panic!("unexpected classification: {:?}", other),
		}
		let _ = Hash::default();
	}
}
