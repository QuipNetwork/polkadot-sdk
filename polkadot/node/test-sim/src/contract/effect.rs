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

//! `Effect` enum: the protocol-relevant outputs of the collator-protocol subsystem.
//!
//! Tests assert on [`Effect`] values exclusively. Each variant carries semantic content (peer
//! ids, candidate hashes, paraids, message kinds), never raw wire bytes, so refactors to wire
//! formats do not break tests.

use crate::contract::reputation::RepBucket;
use polkadot_node_network_protocol::peer_set::PeerSet;
use polkadot_primitives::{AuthorityDiscoveryId, CandidateHash, Hash, Id as ParaId};
use sc_network_types::PeerId;
use std::collections::BTreeSet;

/// Opaque identifier assigned by the harness when the subsystem fires an outgoing request.
/// Tests pass it to `Sim::respond_fetch` to deliver a response into the oneshot the subsystem
/// is awaiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RequestId(pub u64);

/// A single observable output of the collator-protocol subsystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
	/// Subsystem instructed `CandidateBacking` to second a fetched candidate.
	SecondCandidate {
		/// Scheduling parent (validator group context).
		scheduling_parent: Hash,
		/// Candidate identity.
		candidate_hash: CandidateHash,
		/// Para the candidate belongs to.
		para: ParaId,
	},
	/// Subsystem advertised a candidate to a set of peers.
	SendAdvertisement {
		/// Recipients.
		peers: Vec<PeerId>,
		/// Semantic summary of the advertisement.
		advertisement: AdvertisementSummary,
	},
	/// Subsystem sent a wire-level collation message (Declare, CollationSeconded, ...).
	SendCollation {
		/// Recipients.
		peers: Vec<PeerId>,
		/// What kind of collation message was sent.
		kind: WireMsgKind,
	},
	/// Subsystem fired an outgoing request.
	SendRequest {
		/// Opaque identifier the harness assigned to this request. Tests use this with
		/// `Sim::respond_fetch(request_id, payload)` to deliver a response into the oneshot
		/// the subsystem is awaiting.
		request_id: RequestId,
		/// Target peer.
		to: PeerId,
		/// What kind of request.
		kind: ReqKind,
		/// Candidate the request is about, if applicable.
		candidate_hash: Option<CandidateHash>,
	},
	/// Subsystem fired an outgoing request.
	SendRequestV3 {
		/// Opaque identifier the harness assigned to this request. Tests use this with
		/// `Sim::respond_fetch(request_id, payload)` to deliver a response into the oneshot
		/// the subsystem is awaiting.
		request_id: RequestId,
		/// Target peer.
		to: PeerId,
		/// What kind of request.
		kind: ReqKind,
		/// Output head data hash of the candidate.
		output_head_data_hash: Hash,
	},
	/// Subsystem requested a peer reputation change.
	Reputation {
		/// Affected peer.
		peer: PeerId,
		/// Bucketed severity (Malicious / Performance / Benefit).
		bucket: RepBucket,
	},
	/// Subsystem instructed the network bridge to maintain connections to a validator set.
	ConnectValidators {
		/// Validator authority-discovery ids to connect to.
		validator_ids: BTreeSet<AuthorityDiscoveryId>,
		/// Which peer-set the request applies to.
		peer_set: PeerSet,
	},
	/// Subsystem instructed the network bridge to disconnect peers.
	DisconnectPeers {
		/// Peers to disconnect.
		peers: BTreeSet<PeerId>,
		/// Which peer-set the disconnect applies to.
		peer_set: PeerSet,
	},
	/// Subsystem replied to a previously-received request (e.g. responding to a
	/// `CollationFetchingRequest`).
	RequestResponseSent {
		/// Identifier of the request being responded to.
		request_id: u64,
		/// Kind of response sent.
		kind: RespKind,
	},
}

/// Semantic summary of an advertisement carried over the wire. Drops protocol-version
/// discriminants and raw byte encoding — tests assert against these fields, not wire format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvertisementSummary {
	/// Scheduling parent (relay parent for V1, possibly different from the claim's relay parent
	/// for V2/V3/V4).
	pub scheduling_parent: Hash,
	/// Candidate hash, when the advertisement format includes one (V2/V3). `None` for V1 and
	/// for V4, whose fingerprints deliberately carry no candidate hash.
	pub candidate_hash: Option<CandidateHash>,
	/// Hash of the parent head data (V2/V3, and the V4 tip fingerprint).
	pub parent_head_hash: Option<Hash>,
	/// Output head hash of the claimed parachain block — the claim identity of a V4
	/// fingerprint (tip summarized). `None` for V1/V2/V3.
	pub output_head_hash: Option<Hash>,
}

/// Coarse-grained kind of an outbound collation wire message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireMsgKind {
	/// `Declare(collator_id, para_id, signature)`.
	Declare {
		/// Para the collator is declaring on.
		para: ParaId,
	},
	/// `AdvertiseCollation(...)` — semantic summary attached.
	Advertise {
		/// Semantic content of the advertisement.
		summary: AdvertisementSummary,
	},
	/// `CollationSeconded(relay_parent, statement)`.
	CollationSeconded {
		/// Relay parent the seconded statement is anchored at.
		relay_parent: Hash,
	},
}

/// Coarse-grained kind of an outbound request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReqKind {
	/// `CollationFetchingRequest` v1 (no candidate hash).
	CollationFetchingV1,
	/// `CollationFetchingRequest` v2 (with candidate hash).
	CollationFetchingV2,
	/// `CollationFetchingRequest` v3 (no candidate hash, with output head data hash)
	CollationFetchingV3,
}

/// Coarse-grained kind of an outbound response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RespKind {
	/// Successful collation fetch response.
	CollationFetchingOk,
	/// Error / refusal response.
	Error,
}

/// Helper: convenience for assertions on the seconding effect.
impl Effect {
	/// Returns `Some(candidate_hash)` if this is a `SecondCandidate` for the given hash, else
	/// `None`. Matches the per-Effect convenience matchers described in the design doc.
	pub fn is_second_of(&self, expected: &CandidateHash) -> bool {
		matches!(self, Effect::SecondCandidate { candidate_hash, .. } if candidate_hash == expected)
	}

	/// Returns `Some(candidate_hash)` if this is a `SendRequest` for a `CollationFetching*` of
	/// the given candidate. Useful when the test does not care which protocol-version is used.
	pub fn is_fetch_for(&self, expected: &CandidateHash) -> bool {
		matches!(
			self,
			Effect::SendRequest {
				kind: ReqKind::CollationFetchingV1 | ReqKind::CollationFetchingV2,
				candidate_hash: Some(c),
				..
			} if c == expected
		)
	}

	/// Returns `Some(request_id)` if this is a `SendRequest` or `SendRequestV3`. Useful for
	/// chaining `Sim::expect(...)` with `Sim::respond_fetch(request_id, payload)`.
	pub fn request_id(&self) -> Option<RequestId> {
		match self {
			Effect::SendRequest { request_id, .. } | Effect::SendRequestV3 { request_id, .. } => {
				Some(*request_id)
			},
			_ => None,
		}
	}
}
