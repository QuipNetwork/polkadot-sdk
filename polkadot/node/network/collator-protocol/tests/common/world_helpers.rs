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

//! Fluent test surface on top of [`crate::common::world::World`].
//!
//! Compresses common scenario boilerplate. The "advertise → fetch fires" assertion goes
//! from
//!
//! ```ignore
//! world.base.sim.send(peer.connected());
//! world.base.sim.send(peer.declare());
//! world.base.sim.send(peer.advertise(rp, Some(c.hash()), Some(parent_head_hash)));
//! let send_request = world.base.sim.expect(
//!     |e| matches!(e, Effect::SendRequest { .. } if /* ... */),
//!     Duration::from_millis(500),
//!     "...",
//! );
//! let request_id = send_request.request_id().expect("...");
//! ```
//!
//! down to
//!
//! ```ignore
//! let peer = w.declared_peer(para, V2);
//! let cand = w.advertise(&peer, w.leaf(), para);
//! let _ = w.fetch_request(&cand);
//! ```
//!
//! Scenarios that need the raw API still have `world.base.sim.send(...)` /
//! `world.base.sim.expect(...)` available — these helpers are additive, not gating.

use crate::common::{
	builders::{Candidate, CandidateBuilder, Peer, ProtocolVersion},
	contract::{Effect, RepBucket, ReqKind, RequestId},
	harness::CollatorSut,
	world::World,
};
use codec::Encode;
use polkadot_node_network_protocol::{
	request_response::{v1 as protocol_v1, v2 as protocol_v2, v3 as protocol_v3},
	v4_collation::CandidateFingerprint,
};
use polkadot_node_primitives::PoV;
use polkadot_primitives::{
	CandidateDescriptorVersion, CandidateHash, CandidateReceiptV2, Hash, HeadData, Id as ParaId,
};
use sc_network::{ProtocolName, RequestFailure};
use std::time::Duration;

/// Default budget for "happy-path" expectations (advertise → fetch → respond → second).
/// Long enough to cover the few internal future yields the real backing pipeline needs.
const HAPPY_PATH_TIMEOUT: Duration = Duration::from_millis(500);

/// Default budget for negative assertions (`expect_no_*`). Short — we want fast feedback if
/// the effect *does* fire and don't want to wait the full happy-path budget for nothing.
const NEGATIVE_TIMEOUT: Duration = Duration::from_millis(100);

impl<S: CollatorSut> World<S> {
	/// Open a [`CandidateBuilder`] pre-loaded with `relay_parent` and the matching
	/// `relay_parent_number` looked up from [`SharedChain`]. Tests just supply
	/// `para`/`parent_head`/`head_data`; getting the relay-parent number wrong is the
	/// most common cause of "Persisted validation data hash doesn't match" rejections in
	/// real prospective.
	pub fn candidate_at(&self, relay_parent: Hash) -> CandidateBuilder {
		let n = self
			.base
			.chain
			.lock()
			.block(&relay_parent)
			.unwrap_or_else(|| {
				panic!(
					"World::candidate_at: relay_parent {:?} not found in chain. Build via \
					 `world.leaf()` / `world.ancestors()` so the chain knows about it.",
					relay_parent,
				)
			})
			.number;
		Candidate::builder().relay_parent(relay_parent).relay_parent_number(n)
	}

	/// Connect a peer and immediately have it `Declare` for `para` over `version`. Returns
	/// the [`Peer`] for further use (advertise, expect-rep, etc.).
	pub fn declared_peer(&mut self, para: ParaId, version: ProtocolVersion) -> Peer {
		let peer = self.connected_peer(para, version);
		self.base.sim.send(peer.declare());
		peer
	}

	/// Connect a peer without declaring. Useful for bad-signature tests, undeclared-eviction
	/// tests, and any other scenario that wants to drive the connect/declare boundary by
	/// hand.
	pub fn connected_peer(&mut self, para: ParaId, version: ProtocolVersion) -> Peer {
		let peer = Peer::new(para, version);
		self.base.sim.send(peer.connected());
		peer
	}

	/// Build a fresh candidate for `para` at `relay_parent`, send a V2 advertisement for it
	/// from `peer` (parent_head_data = empty), and return the candidate. The advertisement
	/// uses `Hash::default()` as parent-head-data hash unless the scenario specifies one
	/// via [`Self::advertise_with_parent_head`].
	///
	/// V1 peers get a V1 advertisement (no candidate hash on the wire); the returned
	/// `Candidate` is still the constructed receipt so test code can use its hash for
	/// later assertions.
	pub fn advertise(&mut self, peer: &Peer, relay_parent: Hash, para: ParaId) -> Candidate {
		let candidate = Candidate::for_para_at(para, relay_parent);
		let parent_head_hash = HeadData(Vec::new()).hash();
		self.base.sim.send(peer.advertise(
			relay_parent,
			Some(candidate.hash()),
			Some(parent_head_hash),
		));
		candidate
	}

	/// Variant of [`Self::advertise`] that lets the caller specify the parent-head-data hash
	/// on the wire. The returned candidate is unchanged — use [`Candidate::for_para_at`]
	/// directly if a custom candidate shape is needed.
	pub fn advertise_with_parent_head(
		&mut self,
		peer: &Peer,
		relay_parent: Hash,
		candidate_hash: CandidateHash,
		parent_head_hash: Hash,
	) {
		self.base.sim.send(peer.advertise(
			relay_parent,
			Some(candidate_hash),
			Some(parent_head_hash),
		));
	}

	/// Send a V3 advertisement with full control of `scheduling_parent`, `relay_parent`,
	/// and `descriptor_version`. The `peer` must be V3.
	pub fn advertise_v3(
		&mut self,
		peer: &Peer,
		scheduling_parent: Hash,
		relay_parent: Hash,
		candidate_hash: CandidateHash,
		parent_head_hash: Hash,
		descriptor_version: polkadot_primitives::CandidateDescriptorVersion,
	) {
		self.base.sim.send(peer.advertise_v3(
			scheduling_parent,
			relay_parent,
			candidate_hash,
			parent_head_hash,
			descriptor_version,
		));
	}

	/// Send a V4 `AdvertiseSegment` from `peer` at `scheduling_parent`, carrying V3
	/// candidate descriptors (the only descriptor version V4 segments use today). The
	/// `peer` must be V4. A V4 peer's first segment doubles as its declaration. For a
	/// different descriptor version, call [`Peer::advertise_segment`] directly.
	pub fn advertise_segment(
		&mut self,
		peer: &Peer,
		scheduling_parent: Hash,
		fingerprints: Vec<CandidateFingerprint>,
	) {
		self.base.sim.send(peer.advertise_segment(
			scheduling_parent,
			CandidateDescriptorVersion::V3,
			fingerprints,
		));
	}

	/// Snapshot the recorder's current entry count. Use as a barrier with
	/// [`Self::first_fetch_after`] to find the first fetch fired *after* a particular
	/// scenario step. Sim time alone doesn't separate events recorded inside a single
	/// `drain` cycle (they all carry the same `sim_t`); the entry index does.
	pub fn recorder_barrier(&self) -> usize {
		self.base.sim.recorder().entries().len()
	}

	/// Find the first `Effect::SendRequest CollationFetchingV{1,2}` recorded at or after
	/// `barrier` (a recorder-entry index from [`Self::recorder_barrier`]). Returns
	/// `None` if none has been recorded yet — it does not block.
	pub fn first_fetch_after(
		&self,
		barrier: usize,
	) -> Option<(sc_network_types::PeerId, Option<polkadot_primitives::CandidateHash>)> {
		self.base.sim.recorder().entries().iter().skip(barrier).find_map(|o| {
			let crate::common::harness::Observation::Effect(s) = o;
			if let Effect::SendRequest {
				kind: ReqKind::CollationFetchingV1 | ReqKind::CollationFetchingV2,
				to,
				candidate_hash,
				..
			} = &s.value
			{
				Some((*to, *candidate_hash))
			} else {
				None
			}
		})
	}

	/// Wait for `Effect::SendRequest CollationFetchingV2` matching `candidate_hash`. Use
	/// when the test asserts on a known candidate hash without holding a `Candidate`
	/// (e.g. mismatched-hash sanity scenarios).
	pub fn expect_fetch_for_hash(
		&mut self,
		candidate_hash: polkadot_primitives::CandidateHash,
	) -> RequestId {
		let send_request = self.base.sim.expect(
			|e| {
				matches!(
					e,
					Effect::SendRequest {
						kind: ReqKind::CollationFetchingV2,
						candidate_hash: Some(c),
						..
					} if *c == candidate_hash,
				)
			},
			HAPPY_PATH_TIMEOUT,
			"Effect::SendRequest CollationFetchingV2 for the specified hash",
		);
		send_request.request_id().expect("SendRequest carries a RequestId")
	}

	/// Wait for `Effect::SendRequest CollationFetchingV{1,2}` targeting `peer`. Use when
	/// the candidate hash is unknown (the test only cares about which peer was picked).
	pub fn expect_fetch_to(&mut self, peer: sc_network_types::PeerId) -> RequestId {
		let send_request = self.base.sim.expect(
			|e| {
				matches!(
					e,
					Effect::SendRequest {
						kind: ReqKind::CollationFetchingV1 | ReqKind::CollationFetchingV2,
						to,
						..
					} if *to == peer,
				)
			},
			HAPPY_PATH_TIMEOUT,
			"Effect::SendRequest CollationFetching to the specified peer",
		);
		send_request.request_id().expect("SendRequest carries a RequestId")
	}

	/// Wait for the next outbound `Effect::SendRequest CollationFetchingV{1,2}` of any kind
	/// from any peer. Returns `(peer_id, request_id, candidate_hash_if_any)` for the
	/// caller to react to.
	///
	/// Use this when the test doesn't yet know which peer the validator will pick — e.g.
	/// in fairness scenarios with multiple advertising peers. For known-candidate
	/// matching see [`Self::fetch_request`].
	pub fn expect_any_fetch(
		&mut self,
	) -> (sc_network_types::PeerId, RequestId, Option<polkadot_primitives::CandidateHash>) {
		let send_request = self.base.sim.expect(
			|e| {
				matches!(
					e,
					Effect::SendRequest {
						kind: ReqKind::CollationFetchingV1 | ReqKind::CollationFetchingV2,
						..
					}
				)
			},
			HAPPY_PATH_TIMEOUT,
			"Effect::SendRequest CollationFetching from any peer",
		);
		match send_request {
			Effect::SendRequest { to, request_id, candidate_hash, .. } => {
				(to, request_id, candidate_hash)
			},
			_ => unreachable!("filter ensures SendRequest"),
		}
	}

	/// Wait for `Effect::SendRequest CollationFetchingV{1,2}` whose candidate hash matches
	/// `candidate`. Returns the [`RequestId`] so the test can later
	/// [`Self::respond_fetch_collation`] or drop it on the floor.
	pub fn fetch_request(&mut self, candidate: &Candidate) -> RequestId {
		let send_request = self.base.sim.expect(
			|e| match e {
				Effect::SendRequest {
					kind: ReqKind::CollationFetchingV1,
					candidate_hash: None,
					..
				} => true,
				Effect::SendRequest {
					kind: ReqKind::CollationFetchingV2,
					candidate_hash: Some(c),
					..
				} if *c == candidate.hash() => true,
				_ => false,
			},
			HAPPY_PATH_TIMEOUT,
			"Effect::SendRequest CollationFetching for the advertised candidate",
		);
		send_request.request_id().expect("SendRequest carries a RequestId")
	}

	/// Assert that **no** fetch fires for `candidate` within `within`.
	pub fn no_fetch_for(&mut self, candidate: &Candidate, within: Duration) {
		self.base.sim.expect_no(
			|e| {
				matches!(
					e,
					Effect::SendRequest { candidate_hash: Some(c), .. } if *c == candidate.hash(),
				)
			},
			within,
			"SendRequest for the candidate (must NOT fire)",
		);
	}

	/// Assert that **no** fetch of any kind fires within `within`. Useful when a scenario's
	/// invariant is "advertisement was rejected; nothing happened downstream."
	pub fn no_fetch_within(&mut self, within: Duration) {
		self.base.sim.expect_no(
			|e| matches!(e, Effect::SendRequest { .. }),
			within,
			"any SendRequest (must NOT fire)",
		);
	}

	/// Wait for the next V4-resolved fetch — `Effect::SendRequestV3`, the
	/// output-head-keyed `CollationFetchingV3` request — from any peer. Returns
	/// `(peer_id, request_id, output_head_data_hash)`.
	///
	/// Use when the test doesn't know which peer the validator will pick (equal-score
	/// peers tie-break nondeterministically). Note: the V1/V2 helpers
	/// (`expect_fetch_to`, `no_fetch_within`, ...) match `Effect::SendRequest` only and
	/// do NOT see V4 fetches.
	pub fn expect_any_fetch_v3(&mut self) -> (sc_network_types::PeerId, RequestId, Hash) {
		let send_request = self.base.sim.expect(
			|e| matches!(e, Effect::SendRequestV3 { .. }),
			HAPPY_PATH_TIMEOUT,
			"Effect::SendRequestV3 (output-head-keyed collation fetch) from any peer",
		);
		match send_request {
			Effect::SendRequestV3 { to, request_id, output_head_data_hash, .. } => {
				(to, request_id, output_head_data_hash)
			},
			_ => unreachable!("filter ensures SendRequestV3"),
		}
	}

	/// Wait for `Effect::SendRequestV3` targeting `peer`. Returns the [`RequestId`] for
	/// [`Self::respond_fetch_v3`] / [`Self::fail_fetch`].
	pub fn expect_fetch_v3_to(&mut self, peer: sc_network_types::PeerId) -> RequestId {
		let send_request = self.base.sim.expect(
			|e| matches!(e, Effect::SendRequestV3 { to, .. } if *to == peer),
			HAPPY_PATH_TIMEOUT,
			"Effect::SendRequestV3 to the specified peer",
		);
		send_request.request_id().expect("SendRequestV3 carries a RequestId")
	}

	/// Assert that **no** V4-resolved fetch (`Effect::SendRequestV3`) fires within
	/// `within`.
	pub fn no_fetch_v3_within(&mut self, within: Duration) {
		self.base.sim.expect_no(
			|e| matches!(e, Effect::SendRequestV3 { .. }),
			within,
			"any SendRequestV3 (must NOT fire)",
		);
	}

	/// Encode a V2 `CollationFetchingResponse::Collation(receipt, pov)` and resolve the
	/// pending fetch identified by `request_id`.
	pub fn respond_fetch_v2(
		&mut self,
		request_id: RequestId,
		receipt: CandidateReceiptV2,
		pov: PoV,
	) {
		let response = protocol_v2::CollationFetchingResponse::Collation(receipt, pov);
		self.base
			.sim
			.respond_fetch(request_id, Ok((response.encode(), ProtocolName::from(""))));
	}

	/// Resolve the pending fetch with bytes that cannot be decoded as a
	/// `CollationFetchingResponse`. The subsystem's fetch future fails with `InvalidResponse`,
	/// which the experimental validator treats as a slashable failed fetch
	/// (`FAILED_FETCH_SLASH`). Use to drive reputation slashes from the network layer.
	pub fn respond_fetch_invalid(&mut self, request_id: RequestId) {
		self.base
			.sim
			.respond_fetch(request_id, Ok((vec![0xff; 8], ProtocolName::from(""))));
	}

	/// Encode a V3 request-response `CollationFetchingResponse::Collation { .. }` — the
	/// response format for output-head-keyed V4 fetches; `parent_head_data` is
	/// mandatory there — and resolve the pending fetch identified by `request_id`.
	pub fn respond_fetch_v3(
		&mut self,
		request_id: RequestId,
		receipt: CandidateReceiptV2,
		pov: PoV,
		parent_head_data: HeadData,
	) {
		let response =
			protocol_v3::CollationFetchingResponse::Collation { receipt, pov, parent_head_data };
		self.base
			.sim
			.respond_fetch(request_id, Ok((response.encode(), ProtocolName::from(""))));
	}

	/// Resolve the pending fetch with a network-level failure (`NotConnected`). The
	/// subsystem sees `RequestError::NetworkError` — a fetch failure that is not
	/// attributed to the collator (no slash) — making this the plain "the first fetch
	/// failed" stimulus for retry scenarios.
	pub fn fail_fetch(&mut self, request_id: RequestId) {
		self.base.sim.respond_fetch(request_id, Err(RequestFailure::NotConnected));
	}

	/// V1 variant of [`Self::respond_fetch_v2`].
	pub fn respond_fetch_v1(
		&mut self,
		request_id: RequestId,
		receipt: CandidateReceiptV2,
		pov: PoV,
	) {
		let response = protocol_v1::CollationFetchingResponse::Collation(receipt.into(), pov);
		self.base
			.sim
			.respond_fetch(request_id, Ok((response.encode(), ProtocolName::from(""))));
	}

	/// Encode a V2 `CollationFetchingResponse::CollationWithParentHeadData { ... }` and
	/// resolve the fetch. `parent_head_data` is sent as-is — passing a value whose hash
	/// differs from the advertised parent-head hash is the canonical sanity-check failure
	/// scenario.
	pub fn respond_fetch_v2_with_parent_head(
		&mut self,
		request_id: RequestId,
		receipt: CandidateReceiptV2,
		pov: PoV,
		parent_head_data: HeadData,
	) {
		let response = protocol_v2::CollationFetchingResponse::CollationWithParentHeadData {
			receipt,
			pov,
			parent_head_data,
		};
		self.base
			.sim
			.respond_fetch(request_id, Ok((response.encode(), ProtocolName::from(""))));
	}

	/// Run a full advertise → fetch → respond → second cycle for `candidate` over V2.
	/// Returns once the validator has emitted `Effect::SecondCandidate` for this hash.
	///
	/// Use in fragment-chain scenarios that need to second N candidates back-to-back. Build
	/// each candidate via [`Candidate::builder`] threading `parent_head` and `head_data`
	/// (the previous candidate's `output_head` becomes the next's `parent_head`), then call
	/// this helper for each in order.
	///
	/// # Panics
	///
	/// On expectation failures (no fetch, no second), the underlying `Sim::expect` calls
	/// dump the timeline and panic — same dev-ex as a hand-rolled scenario.
	pub fn full_second(&mut self, peer: &Peer, candidate: &Candidate) {
		self.outputs
			.insert(candidate.hash(), candidate.commitments.clone(), candidate.pvd.clone());
		self.advertise_with_parent_head(
			peer,
			candidate.relay_parent(),
			candidate.hash(),
			candidate.parent_head_hash(),
		);
		let request_id = self.fetch_request(candidate);
		self.respond_fetch_v2(request_id, candidate.receipt.clone(), Candidate::empty_pov());
		self.expect_second(candidate);
		// `Effect::SecondCandidate` is observed at the moment collator-protocol dispatches
		// `CandidateBackingMessage::Second` to backing — *before* backing has run validation
		// and forwarded `IntroduceSecondedCandidate` to prospective. Subsequent calls in a
		// fragment chain need prospective to have absorbed this candidate, so we let the
		// downstream pipeline flush before returning.
		self.base.sim.advance(Duration::from_millis(200));
	}

	/// Wait for `Effect::SecondCandidate` whose candidate hash equals `candidate`'s.
	pub fn expect_second(&mut self, candidate: &Candidate) {
		let _ = self.base.sim.expect(
			|e| {
				matches!(
					e,
					Effect::SecondCandidate { candidate_hash, .. } if candidate_hash == &candidate.hash()
				)
			},
			HAPPY_PATH_TIMEOUT,
			"Effect::SecondCandidate for the candidate",
		);
	}

	/// Assert that **no** `Effect::SecondCandidate` for `candidate` fires within `within`.
	///
	/// Use as the impl-agnostic "candidate was rejected" observable when the test no longer
	/// asserts the (impl-specific) reputation effect. Both impls agree on the negative
	/// observation; only the *signal* of rejection differs.
	pub fn expect_no_second(&mut self, candidate: &Candidate, within: Duration) {
		self.base.sim.expect_no(
			|e| {
				matches!(
					e,
					Effect::SecondCandidate { candidate_hash, .. } if candidate_hash == &candidate.hash()
				)
			},
			within,
			"Effect::SecondCandidate for the candidate (must NOT fire — candidate rejected)",
		);
	}

	/// Wait for `Effect::Reputation { peer, bucket }` matching `peer` and `bucket`.
	pub fn expect_rep(&mut self, peer: &Peer, bucket: RepBucket) {
		self.expect_rep_id(peer.peer_id, bucket)
	}

	/// Variant of [`Self::expect_rep`] that takes a `PeerId` directly. Useful when the
	/// scenario obtained the id from an effect rather than from a `Peer` builder.
	pub fn expect_rep_id(&mut self, peer_id: sc_network_types::PeerId, bucket: RepBucket) {
		let _ = self.base.sim.expect(
			|e| {
				matches!(
					e,
					Effect::Reputation { peer: p, bucket: b } if *p == peer_id && *b == bucket,
				)
			},
			HAPPY_PATH_TIMEOUT,
			"Effect::Reputation for peer (matching bucket)",
		);
	}

	/// Assert that **no** `Effect::Reputation` for `peer` with the given `bucket` fires
	/// within [`NEGATIVE_TIMEOUT`].
	pub fn expect_no_rep(&mut self, peer: &Peer, bucket: RepBucket) {
		self.base.sim.expect_no(
			|e| {
				matches!(
					e,
					Effect::Reputation { peer: p, bucket: b } if *p == peer.peer_id && *b == bucket,
				)
			},
			NEGATIVE_TIMEOUT,
			"Effect::Reputation for peer (must NOT fire with this bucket)",
		);
	}

	/// Wait for `Effect::DisconnectPeers` on the Collation peer-set carrying `peer`.
	pub fn expect_disconnect(&mut self, peer: &Peer) {
		use polkadot_node_network_protocol::peer_set::PeerSet;
		let _ = self.base.sim.expect(
			|e| {
				matches!(
					e,
					Effect::DisconnectPeers { peers, peer_set: PeerSet::Collation }
						if peers.contains(&peer.peer_id),
				)
			},
			HAPPY_PATH_TIMEOUT,
			"Effect::DisconnectPeers for peer on the Collation peer-set",
		);
	}

	/// Assert that **no** `Effect::DisconnectPeers` carrying `peer` fires within `within`.
	pub fn expect_no_disconnect(&mut self, peer: &Peer, within: Duration) {
		use polkadot_node_network_protocol::peer_set::PeerSet;
		self.base.sim.expect_no(
			|e| {
				matches!(
					e,
					Effect::DisconnectPeers { peers, peer_set: PeerSet::Collation }
						if peers.contains(&peer.peer_id),
				)
			},
			within,
			"Effect::DisconnectPeers for peer (must NOT fire)",
		);
	}
}
