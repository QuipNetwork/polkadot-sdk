// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
// SPDX-License-Identifier: Apache-2.0

//! V3 scheduling types for low-latency parachain block production.
//!
//! V3 candidates separate the relay parent (execution context) from the scheduling
//! parent (a recent relay chain tip used for core assignment). This enables building
//! on older relay parents while still being scheduled based on recent relay state.
//!
//! # Resubmission
//!
//! When a candidate fails to get backed in time, a different collator can resubmit
//! it with a new `scheduling_parent` (fresh relay tip) without re-executing the blocks.
//! The `relay_parent` stays the same since the execution context hasn't changed.
//!
//! For resubmission, `signed_scheduling_info` must be provided. The resubmitting
//! collator signs the core selection, proving they are the eligible author for the
//! slot derived from the `internal_scheduling_parent`.

use alloc::vec::Vec;
use codec::{Decode, Encode};
use polkadot_primitives::{ApprovedPeerId, CoreSelector, Header as RelayChainHeader, Slot};
use sp_runtime::traits::{BlakeTwo256, Hash as HashT};

/// Payload signed by a collator for resubmission.
///
/// This binds the core selection and reputation-credit peer to a specific internal
/// scheduling parent, preventing replay attacks across different scheduling contexts.
#[derive(Clone, Encode, Decode, Debug, PartialEq, Eq)]
pub struct SchedulingInfoPayload {
	/// Which core to use (indexes into the parachain's assigned cores).
	pub core_selector: CoreSelector,
	/// The claim queue offset.
	pub claim_queue_offset: u8,
	/// Peer ID to receive reputation credit for successful collation delivery.
	pub peer_id: ApprovedPeerId,
	/// The internal scheduling parent whom's slot decides the
	/// eligible block author that must sign the payload.
	pub internal_scheduling_parent: polkadot_primitives::Hash,
}

/// Signed scheduling information for candidate resubmission.
///
/// When a collator resubmits a candidate (with a newer `scheduling_parent` but same
/// `relay_parent`), they must sign the core selection to prove eligibility for the
/// slot at `internal_scheduling_parent`.
///
/// The `claim_queue_offset` is derived from the runtime's `relay_parent_offset`
/// configuration and is not part of this struct - it cannot be overridden by the
/// collator.
#[derive(Clone, Encode, Decode, Debug, PartialEq, Eq)]
pub struct SignedSchedulingInfo {
	/// The scheduling information.
	pub payload: SchedulingInfoPayload,
	/// Signature by the eligible collator over the SCALE-encoded
	/// `SchedulingInfoPayload`.
	///
	/// Stored as a fixed 64-byte blob so the verifier can decode it as either an sr25519
	/// or ed25519 signature. Both schemes produce 64-byte signatures.
	pub signature: [u8; 64],
}

impl SchedulingInfoPayload {
	/// Create a new scheduling info payload.
	pub fn new(
		core_selector: CoreSelector,
		claim_queue_offset: u8,
		peer_id: ApprovedPeerId,
		internal_scheduling_parent: polkadot_primitives::Hash,
	) -> Self {
		Self { core_selector, claim_queue_offset, peer_id, internal_scheduling_parent }
	}
}

/// V3 scheduling proof included in the POV.
///
/// Provides the ancestry from scheduling_parent back to the internal scheduling
/// parent. The PVF validates this against the relay_parent and scheduling_parent
/// from the candidate descriptor extension.
#[derive(Clone, Encode, Decode, Debug, PartialEq, Eq)]
pub struct SchedulingProof {
	/// Relay chain headers proving ancestry from scheduling_parent backward.
	///
	/// Forms a chain where each header's parent_hash equals the next header's hash.
	/// The first header's hash must equal the candidate's scheduling_parent.
	/// The last header's parent_hash is the internal scheduling parent.
	/// Length is defined by the parachain runtime config (RelayParentOffset).
	pub header_chain: Vec<RelayChainHeader>,
	/// The relay chain header at `internal_scheduling_parent`. Its hash must equal the
	/// `internal_scheduling_parent` derived from `header_chain` (the parent of the chain's
	/// last header, or `scheduling_parent` if the chain is empty).
	pub internal_scheduling_parent_header: RelayChainHeader,
	/// Signed scheduling info for core selection override.
	///
	/// - `None` with `relay_parent == internal_scheduling_parent`: Initial submission. Core
	///   selection comes from the parachain block's UMP signals.
	///
	/// - `Some` with `relay_parent == internal_scheduling_parent`: Initial submission with
	///   explicit core selection. This is optional but legal. Collators should refuse to
	///   acknowledge blocks with invalid scheduling info, so providing a signature is not required
	///   for initial submissions.
	///
	/// - `Some` with `relay_parent != internal_scheduling_parent`: Resubmission (required). The
	///   resubmitting collator signs the core selection, overriding the block's UMP signals.
	///   Signature is verified against the eligible author for the slot at
	///   `internal_scheduling_parent`.
	pub signed_scheduling_info: Option<SignedSchedulingInfo>,
}

impl SchedulingProof {
	/// Derive the scheduling parent hash.
	///
	/// Returns the hash of the first/newest header in `header_chain` if non-empty, otherwise
	/// falls back to `internal_scheduling_parent_header.hash()` (the ISP coincides with the
	/// scheduling parent when the parachain runs with `relay_parent_offset = 0`).
	pub fn scheduling_parent(&self) -> polkadot_primitives::Hash {
		self.header_chain
			.first()
			.map(BlakeTwo256::hash_of)
			.unwrap_or_else(|| self.internal_scheduling_parent_header.hash())
	}
}

/// Verifier for V3 scheduling.
///
/// Reports whether V3 scheduling is enabled for the parachain (via
/// [`Self::V3_SCHEDULING_ENABLED`]) and, when it is, verifies the [`SignedSchedulingInfo`]
/// attached to a candidate (via [`Self::verify`]).
pub trait VerifySchedulingSignature {
	/// Whether V3 scheduling validation is enabled.
	const V3_SCHEDULING_ENABLED: bool;

	/// Verifies `signed_info` against the author eligible at `relay_slot` (the slot of the
	/// internal scheduling parent)
	fn verify(signed_info: &SignedSchedulingInfo, relay_slot: Slot) -> bool;
}

/// Default no-op wiring: V3 scheduling disabled, scheduling info accepted unconditionally.
///
/// Replacing it with a real verifier should also turn V3 on.
impl VerifySchedulingSignature for () {
	const V3_SCHEDULING_ENABLED: bool = false;

	fn verify(_signed_info: &SignedSchedulingInfo, _relay_slot: Slot) -> bool {
		true
	}
}
