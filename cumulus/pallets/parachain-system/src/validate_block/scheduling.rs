// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
// SPDX-License-Identifier: Apache-2.0

//! Scheduling validation for V3 candidates.
//!
//! Validates the header chain from scheduling_parent to internal_scheduling_parent,
//! and verifies relay_parent is at or before internal_scheduling_parent.

use alloc::vec::Vec;
use codec::{Decode, Encode};
use cumulus_primitives_core::{
	relay_chain::{ApprovedPeerId, Header as RelayChainHeader, Slot, UMPSignal, UMP_SEPARATOR},
	ClaimQueueOffset, CoreSelector, SchedulingProof, SignedSchedulingInfo,
};
use frame_support::{traits::Get, BoundedVec};
use polkadot_parachain_primitives::primitives::ValidationParamsExtension;
use sp_consensus_babe::digests::CompatibleDigestItem as BabeDigestItem;
use sp_runtime::traits::Header as HeaderT;

/// Hash type for relay chain.
pub type RelayHash = sp_core::H256;

/// Extract the relay slot from `header`'s BABE pre-digest.
///
/// The relay chain runs BABE, so the slot of a relay header lives in its BABE pre-digest.
/// Returns `None` when the header carries no BABE pre-digest.
pub(crate) fn relay_slot_from_header(header: &RelayChainHeader) -> Option<Slot> {
	header
		.digest
		.logs()
		.iter()
		.find_map(|log| BabeDigestItem::as_babe_pre_digest(log))
		.map(|pre_digest| pre_digest.slot())
}

/// Errors that can occur during scheduling validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchedulingValidationError {
	/// Header chain has wrong length.
	InvalidHeaderChainLength { expected: u32, actual: usize },
	/// Header chain does not form a valid chain.
	BrokenHeaderChain { index: usize },
	/// First header hash does not match scheduling_parent.
	SchedulingParentMismatch,
	/// relay_parent is within the header chain but not at internal_scheduling_parent.
	/// For resubmission, relay_parent must be an ancestor of internal_scheduling_parent.
	RelayParentInHeaderChain,
	/// Resubmission is missing required signed_scheduling_info.
	/// When relay_parent != internal_scheduling_parent, the resubmitting collator must
	/// sign the core selection to prove slot eligibility.
	MissingSignedSchedulingInfo,
	/// `internal_scheduling_parent_header` does not hash to the derived internal
	/// scheduling parent. Without this linkage a collator could attach an unrelated
	/// header, pointing the slot-deriving author lookup at an arbitrary slot.
	InternalSchedulingParentHeaderMismatch,
	/// `signed_scheduling_info.payload.internal_scheduling_parent` does not match the
	/// derived ISP, i.e. the signature was made for a different scheduling context.
	SignedSchedulingInfoIspMismatch,
	/// Signed `claim_queue_offset` exceeds the runtime-enforced maximum.
	ClaimQueueOffsetTooLarge { offset: u8, max: u8 },
}

/// The validated result of a V3 candidate's scheduling proof, as returned by
/// [`validate_v3_scheduling`].
pub struct ValidatedScheduling {
	/// The relay header at the ISP; its hash is already verified against the derived
	/// ISP (linkage check in [`check_scheduling`]).
	pub internal_scheduling_parent_header: RelayChainHeader,
	/// The signed scheduling info, if any. Its shape is already validated; only the
	/// signature still needs verifying by the caller.
	pub signed_scheduling_info: Option<SignedSchedulingInfo>,
}

/// Validate V3 scheduling based on runtime config and candidate extension.
///
/// Returns `None` for V1/V2 candidates, `Some(ValidatedScheduling)` for valid V3.
/// Panics on config/extension mismatches or chain-shape validation failures.
///
/// Only validates the *shape* of the proof; signature verification on
/// `signed_scheduling_info` is the caller's responsibility (see `validate_block`,
/// which invokes `PSC::SchedulingSignatureVerifier`).
pub fn validate_v3_scheduling(
	v3_enabled: bool,
	extension: &Option<ValidationParamsExtension>,
	scheduling_proof: Option<&SchedulingProof>,
	expected_header_chain_length: u32,
	max_claim_queue_offset: u8,
) -> Option<ValidatedScheduling> {
	match (v3_enabled, extension) {
		(false, None) => {
			// V3 disabled and no extension: normal V1/V2 path
			None
		},
		(false, Some(_)) => {
			// V3 disabled but extension present: this should not happen
			// The relay chain should not send V3 candidates to parachains that have not enabled it
			panic!(
				"V3 extension present but V3 scheduling is disabled. \
                Ensure collators and runtime are in sync."
			);
		},
		(true, None) => {
			// V3 enabled but no extension: candidates must be V3
			panic!(
				"V3 scheduling is enabled but no V3 extension present. \
                Collators must provide V3 candidates when V3 is enabled."
			);
		},
		(true, Some(ValidationParamsExtension::V3 { relay_parent, scheduling_parent })) => {
			// V3 enabled and extension present: validate scheduling
			let scheduling_proof = scheduling_proof
				.expect("V3 candidates require ParachainBlockData::V2 with scheduling_proof");

			match check_scheduling(
				scheduling_proof,
				*relay_parent,
				*scheduling_parent,
				expected_header_chain_length,
				max_claim_queue_offset,
			) {
				Ok(_isp) => Some(ValidatedScheduling {
					internal_scheduling_parent_header: scheduling_proof
						.internal_scheduling_parent_header
						.clone(),
					signed_scheduling_info: scheduling_proof.signed_scheduling_info.clone(),
				}),
				Err(e) => panic!("V3 scheduling validation failed: {:?}", e),
			}
		},
	}
}

/// Check the scheduling proof against the relay parent, scheduling parent, and
/// expected header chain length. Returns the derived `internal_scheduling_parent`.
///
/// Two submission shapes are valid:
/// - **Initial** (`relay_parent == ISP`): `signed_scheduling_info` is optional.
/// - **Resubmission** (`relay_parent` is an ancestor of ISP): `signed_scheduling_info` is required
///   and its `payload.internal_scheduling_parent` must match the derived ISP.
///
/// Signature verification is the caller's responsibility (see `validate_block`).
pub fn check_scheduling(
	scheduling_proof: &SchedulingProof,
	relay_parent: RelayHash,
	scheduling_parent: RelayHash,
	expected_header_chain_length: u32,
	max_claim_queue_offset: u8,
) -> Result<RelayHash, SchedulingValidationError> {
	let header_chain = &scheduling_proof.header_chain;

	// 1. Verify header chain length
	if header_chain.len() != expected_header_chain_length as usize {
		return Err(SchedulingValidationError::InvalidHeaderChainLength {
			expected: expected_header_chain_length,
			actual: header_chain.len(),
		});
	}

	// 2. Verify header chain forms a valid chain
	// First header's hash must equal scheduling_parent
	if !header_chain.is_empty() {
		let first_header_hash = header_chain[0].hash();
		if first_header_hash != scheduling_parent {
			return Err(SchedulingValidationError::SchedulingParentMismatch);
		}
	}

	// Each header's parent_hash must match the hash of the next header
	for i in 0..header_chain.len().saturating_sub(1) {
		let current_parent = header_chain[i].parent_hash();
		let next_hash = header_chain[i + 1].hash();
		if *current_parent != next_hash {
			return Err(SchedulingValidationError::BrokenHeaderChain { index: i });
		}
	}

	// 3. Derive internal_scheduling_parent: parent_hash of the oldest header, or
	// `scheduling_parent` when the chain is empty (`RelayParentOffset = 0`).
	let internal_scheduling_parent = if header_chain.is_empty() {
		scheduling_parent
	} else {
		*header_chain.last().expect("checked non-empty; qed").parent_hash()
	};

	// 4. The ISP header in the proof must hash to the derived ISP — see
	// `InternalSchedulingParentHeaderMismatch`.
	if scheduling_proof.internal_scheduling_parent_header.hash() != internal_scheduling_parent {
		return Err(SchedulingValidationError::InternalSchedulingParentHeaderMismatch);
	}

	// 5. relay_parent must NOT be inside the header chain: it either equals the ISP
	// (initial) or is an ancestor of it (resubmission), never in between.
	for header in header_chain.iter() {
		let header_hash = header.hash();
		if relay_parent == header_hash {
			return Err(SchedulingValidationError::RelayParentInHeaderChain);
		}
	}

	// 6. Resubmission (relay_parent != ISP) requires signed_scheduling_info; initial
	// submission may carry one optionally.
	if relay_parent != internal_scheduling_parent &&
		scheduling_proof.signed_scheduling_info.is_none()
	{
		return Err(SchedulingValidationError::MissingSignedSchedulingInfo);
	}

	// 7. When present, the signed payload must commit to the derived ISP and its
	// claim_queue_offset must be within the runtime cap.
	if let Some(signed_info) = &scheduling_proof.signed_scheduling_info {
		if signed_info.payload.internal_scheduling_parent != internal_scheduling_parent {
			return Err(SchedulingValidationError::SignedSchedulingInfoIspMismatch);
		}
		if signed_info.payload.claim_queue_offset > max_claim_queue_offset {
			return Err(SchedulingValidationError::ClaimQueueOffsetTooLarge {
				offset: signed_info.payload.claim_queue_offset,
				max: max_claim_queue_offset,
			});
		}
	}

	Ok(internal_scheduling_parent)
}

/// The UMP signal tail a candidate emits to the relay chain, parachain-side mirror of
/// [`polkadot_primitives::vstaging::CandidateUMPSignals`].
///
/// The relay decoder (`CandidateCommitments::ump_signals`) is the contract we build for: it
/// rejects a second occurrence of either variant (`DuplicateUMPSignal`) and any third signal
/// (`TooManyUMPSignals`), and parses only the run after the *first* `UMP_SEPARATOR`. We panic
/// rather than emit a tail it would reject — a violation here is our own runtime's bug, not
/// adversarial input.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SchedulingSignals {
	select_core: Option<(CoreSelector, ClaimQueueOffset)>,
	approved_peer: Option<ApprovedPeerId>,
}

impl SchedulingSignals {
	/// Parse the encoded `UMPSignal`s a PoV's blocks emitted after the in-block `UMP_SEPARATOR`
	/// and push the canonical scheduling tail into `upward_messages` via [`Self::emit`].
	///
	/// Panics on a repeated variant *even when values match*: the relay decoder counts
	/// occurrences, not distinct values, so a duplicate is a bug regardless. All parsing and
	/// duplicate-detection panics fire before [`Self::emit`] pushes anything, so a panic never
	/// leaves `upward_messages` half-written.
	pub fn from_block_signals<S: Get<u32>>(
		raw: &[Vec<u8>],
		upward_messages: &mut BoundedVec<Vec<u8>, S>,
	) {
		let mut signals = Self::default();
		for bytes in raw {
			// NOTE: this match is intentionally exhaustive (no `_` arm). Adding a new
			// `UMPSignal` variant must fail to compile here, forcing a deliberate decision:
			// new non-scheduling signal classes (e.g. the speculative-messaging
			// `Requires`/`Provides` commitments) must be handled explicitly — either passed
			// through untouched or routed to their own override path — and must NOT be
			// silently dropped. Such classes may also have different cardinality rules; the
			// per-variant singleton check below applies only to the scheduling signals.
			match UMPSignal::decode(&mut &bytes[..]).expect("Failed to decode `UMPSignal`") {
				UMPSignal::SelectCore(selector, offset) => {
					if signals.select_core.replace((selector, offset)).is_some() {
						panic!("Parachain emitted more than one `SelectCore` UMP signal");
					}
				},
				UMPSignal::ApprovedPeer(peer_id) => {
					if signals.approved_peer.replace(peer_id).is_some() {
						panic!("Parachain emitted more than one `ApprovedPeer` UMP signal");
					}
				},
			}
		}
		signals.emit(upward_messages);
	}

	/// Build the tail from a verified `SignedSchedulingInfo`, which wholesale replaces the
	/// block's emitted signals (the signer signed all three fields), and push it into
	/// `upward_messages` via [`Self::emit`].
	pub fn from_scheduling_info<S: Get<u32>>(
		signed_info: &SignedSchedulingInfo,
		upward_messages: &mut BoundedVec<Vec<u8>, S>,
	) {
		let payload = &signed_info.payload;
		let signals = Self {
			select_core: Some((
				payload.core_selector,
				ClaimQueueOffset(payload.claim_queue_offset),
			)),
			approved_peer: Some(payload.peer_id.clone()),
		};
		signals.emit(upward_messages);
	}

	fn is_empty(&self) -> bool {
		self.select_core.is_none() && self.approved_peer.is_none()
	}

	/// Order is `SelectCore` then `ApprovedPeer`, matching
	/// `pallet_parachain_system::send_ump_signals`. Emits nothing — not even a separator — when
	/// empty, since the relay decoder keys off the first `UMP_SEPARATOR`.
	fn emit<S: Get<u32>>(self, upward_messages: &mut BoundedVec<Vec<u8>, S>) {
		if self.is_empty() {
			return;
		}
		upward_messages
			.try_push(UMP_SEPARATOR)
			.expect("UMPSignals does not fit in UMPMessages");
		if let Some((selector, offset)) = self.select_core {
			upward_messages
				.try_push(UMPSignal::SelectCore(selector, offset).encode())
				.expect("UMPSignals does not fit in UMPMessages");
		}
		if let Some(peer_id) = self.approved_peer {
			upward_messages
				.try_push(UMPSignal::ApprovedPeer(peer_id).encode())
				.expect("UMPSignals does not fit in UMPMessages");
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use cumulus_primitives_core::{
		CoreSelector, SchedulingInfoPayload, SchedulingProof, SignedSchedulingInfo,
	};
	use rstest::rstest;
	use sp_runtime::{generic::Header, traits::BlakeTwo256};

	type RelayHeader = Header<u32, BlakeTwo256>;

	/// Claim-queue-offset cap used in tests. Matches the V3 value returned by
	/// `pallet_parachain_system::max_claim_queue_offset()`.
	const TEST_MAX_CQ_OFFSET: u8 = 2;

	/// Creates a dummy signature blob for testing (not cryptographically valid).
	fn dummy_signature() -> [u8; 64] {
		[0u8; 64]
	}

	/// Builds a `SignedSchedulingInfo` with the given core selector, ISP, and a dummy
	/// signature. `claim_queue_offset` and `peer_id` use default/zero values.
	///
	/// `check_scheduling` cross-checks `payload.internal_scheduling_parent` against the
	/// ISP derived from the proof, so callers must pass the ISP the proof points to (or
	/// a deliberately-mismatched value to exercise the rejection path).
	fn dummy_signed(core_selector: CoreSelector, isp: RelayHash) -> SignedSchedulingInfo {
		SignedSchedulingInfo {
			payload: SchedulingInfoPayload::new(core_selector, 0, Default::default(), isp),
			signature: dummy_signature(),
		}
	}

	/// Creates a chain of headers where each header's parent_hash points to the next,
	/// plus the relay header at `internal_scheduling_parent` (ISP). The ISP header's
	/// hash equals the chain's last header's `parent_hash`, or coincides with
	/// `scheduling_parent` when the chain is empty.
	///
	/// Returns the chain headers ordered newest-to-oldest (index 0 = newest =
	/// `scheduling_parent`) and the ISP header. Tests pick their own `relay_parent`:
	/// `isp_header.hash()` for initial submission, an unrelated hash for resubmission.
	fn make_header_chain(len: usize) -> (Vec<RelayHeader>, RelayHeader) {
		// Construct the ISP header first so we can derive its hash and build the chain
		// on top of it.
		let isp_header = RelayHeader::new(
			0u32,
			Default::default(),
			Default::default(),
			Default::default(),
			Default::default(),
		);

		if len == 0 {
			return (vec![], isp_header);
		}

		let mut headers = Vec::with_capacity(len);
		let mut parent_hash = isp_header.hash();

		for i in 0..len {
			let header = RelayHeader::new(
				(i + 1) as u32, // block number
				Default::default(),
				Default::default(),
				parent_hash,
				Default::default(),
			);
			parent_hash = header.hash();
			headers.push(header);
		}

		// Reverse so newest is first (matches expected ordering).
		headers.reverse();
		(headers, isp_header)
	}

	/// Build a relay header carrying a BABE secondary-plain pre-digest at `slot`.
	fn header_with_babe_slot(slot: u64) -> RelayHeader {
		use sp_consensus_babe::digests::{
			CompatibleDigestItem, PreDigest, SecondaryPlainPreDigest,
		};
		let mut digest = sp_runtime::generic::Digest::default();
		digest.push(<sp_runtime::generic::DigestItem as CompatibleDigestItem>::babe_pre_digest(
			PreDigest::SecondaryPlain(SecondaryPlainPreDigest {
				authority_index: 0,
				slot: slot.into(),
			}),
		));
		RelayHeader::new(0, Default::default(), Default::default(), Default::default(), digest)
	}

	// =========================================================================
	// relay_slot_from_header tests
	// =========================================================================

	#[test]
	fn relay_slot_from_header_reads_babe_pre_digest() {
		// The relay slot is read off the header's BABE pre-digest.
		let header = header_with_babe_slot(7);
		assert_eq!(relay_slot_from_header(&header), Some(Slot::from(7)));
	}

	#[test]
	fn relay_slot_from_header_none_without_pre_digest() {
		// A header with no BABE pre-digest yields `None`; `validate_block` turns this into a
		// candidate rejection (the relay chain always produces BABE pre-digests).
		let header = RelayHeader::new(
			0,
			Default::default(),
			Default::default(),
			Default::default(),
			Default::default(),
		);
		assert_eq!(relay_slot_from_header(&header), None);
	}

	// =========================================================================
	// Valid cases
	// =========================================================================

	#[rstest]
	#[case::len_1(1)]
	#[case::len_3(3)]
	fn valid_non_empty_header_chain(#[case] len: usize) {
		// Valid N-header chain on initial submission (`relay_parent == ISP`): validation
		// passes and `internal_scheduling_parent == relay_parent`. Length 0 is structurally
		// different (no chain headers) and lives in its own test.
		let (headers, isp_header) = make_header_chain(len);
		let scheduling_parent = headers[0].hash();
		let relay_parent = isp_header.hash();

		let proof = SchedulingProof {
			header_chain: headers,
			internal_scheduling_parent_header: isp_header,
			signed_scheduling_info: None,
		};
		let result = check_scheduling(
			&proof,
			relay_parent,
			scheduling_parent,
			len as u32,
			TEST_MAX_CQ_OFFSET,
		)
		.expect("valid chain should pass");
		assert_eq!(result, relay_parent);
	}

	#[test]
	fn valid_empty_header_chain() {
		// Empty chain (offset=0) means scheduling_parent == relay_parent and the
		// ISP header must hash to scheduling_parent.
		let (_, isp_header) = make_header_chain(0);
		let scheduling_parent = isp_header.hash();
		let relay_parent = scheduling_parent; // Must be equal for offset=0

		let proof = SchedulingProof {
			header_chain: vec![],
			internal_scheduling_parent_header: isp_header,
			signed_scheduling_info: None,
		};
		let result =
			check_scheduling(&proof, relay_parent, scheduling_parent, 0, TEST_MAX_CQ_OFFSET)
				.expect("valid empty chain should pass");
		assert_eq!(result, scheduling_parent);
	}

	// =========================================================================
	// Invalid length cases
	// =========================================================================

	#[rstest]
	#[case::too_short(2)]
	#[case::too_long(4)]
	fn reject_wrong_header_chain_length(#[case] actual: usize) {
		// Chain whose length doesn't match the expected (3) is rejected with
		// `InvalidHeaderChainLength`, both when too short and when too long.
		let (headers, isp_header) = make_header_chain(actual);
		let scheduling_parent = headers[0].hash();
		let relay_parent = isp_header.hash();

		let proof = SchedulingProof {
			header_chain: headers,
			internal_scheduling_parent_header: isp_header,
			signed_scheduling_info: None,
		};
		let result =
			check_scheduling(&proof, relay_parent, scheduling_parent, 3, TEST_MAX_CQ_OFFSET);

		assert_eq!(
			result,
			Err(SchedulingValidationError::InvalidHeaderChainLength { expected: 3, actual })
		);
	}

	// =========================================================================
	// Invalid scheduling_parent cases
	// =========================================================================

	#[test]
	fn reject_scheduling_parent_mismatch() {
		// Test: scheduling_parent must hash to the first header.
		let (headers, isp_header) = make_header_chain(3);
		let relay_parent = isp_header.hash();
		let wrong_scheduling_parent = RelayHash::repeat_byte(0xFF);

		let proof = SchedulingProof {
			header_chain: headers,
			internal_scheduling_parent_header: isp_header,
			signed_scheduling_info: None,
		};
		let result =
			check_scheduling(&proof, relay_parent, wrong_scheduling_parent, 3, TEST_MAX_CQ_OFFSET);

		assert_eq!(result, Err(SchedulingValidationError::SchedulingParentMismatch));
	}

	// =========================================================================
	// Broken header chain cases
	// =========================================================================

	#[test]
	fn reject_broken_header_chain() {
		// Test: Headers must form a valid chain via parent_hash linkage.
		let (mut headers, isp_header) = make_header_chain(3);
		let scheduling_parent = headers[0].hash();
		let relay_parent = isp_header.hash();

		// Corrupt the middle header's parent_hash to break the chain
		headers[1] = RelayHeader::new(
			99,
			Default::default(),
			Default::default(),
			RelayHash::repeat_byte(0xDE), // Wrong parent hash
			Default::default(),
		);

		let proof = SchedulingProof {
			header_chain: headers,
			internal_scheduling_parent_header: isp_header,
			signed_scheduling_info: None,
		};
		let result =
			check_scheduling(&proof, relay_parent, scheduling_parent, 3, TEST_MAX_CQ_OFFSET);

		// Chain breaks at index 0 (first header's parent doesn't match second header's hash)
		assert_eq!(result, Err(SchedulingValidationError::BrokenHeaderChain { index: 0 }));
	}

	// =========================================================================
	// relay_parent validation cases
	// =========================================================================

	#[test]
	fn reject_relay_parent_inside_header_chain() {
		// Test: relay_parent must not be one of the headers in the chain.
		// It should either equal internal_scheduling_parent or be an ancestor of it.
		let (headers, isp_header) = make_header_chain(3);
		let scheduling_parent = headers[0].hash();
		// Use the middle header's hash as relay_parent (invalid)
		let relay_parent_in_chain = headers[1].hash();

		let proof = SchedulingProof {
			header_chain: headers,
			internal_scheduling_parent_header: isp_header,
			signed_scheduling_info: None,
		};
		let result = check_scheduling(
			&proof,
			relay_parent_in_chain,
			scheduling_parent,
			3,
			TEST_MAX_CQ_OFFSET,
		);

		assert_eq!(result, Err(SchedulingValidationError::RelayParentInHeaderChain));
	}

	// =========================================================================
	// Resubmission validation cases
	// =========================================================================

	#[test]
	fn initial_submission_allows_signed_scheduling_info() {
		// Test: Initial submission (relay_parent == internal_scheduling_parent) may
		// optionally include signed_scheduling_info. This is legal because collators
		// should refuse to acknowledge blocks with invalid scheduling info anyway.
		let (headers, isp_header) = make_header_chain(3);
		let scheduling_parent = headers[0].hash();
		let relay_parent = isp_header.hash();

		let signed_info = dummy_signed(CoreSelector(0), isp_header.hash());

		let proof = SchedulingProof {
			header_chain: headers,
			internal_scheduling_parent_header: isp_header,
			signed_scheduling_info: Some(signed_info),
		};
		let result =
			check_scheduling(&proof, relay_parent, scheduling_parent, 3, TEST_MAX_CQ_OFFSET);

		// Validation passes - signed_scheduling_info is optional for initial submission
		assert!(result.is_ok());
	}

	#[test]
	fn reject_resubmission_without_signed_scheduling_info() {
		// Test: Resubmission (relay_parent != internal_scheduling_parent) requires
		// signed_scheduling_info to prove the resubmitting collator's eligibility.
		let (headers, isp_header) = make_header_chain(3);
		let scheduling_parent = headers[0].hash();
		// Use an unrelated hash as relay_parent (simulates resubmission)
		let older_relay_parent = RelayHash::repeat_byte(0xBB);

		let proof = SchedulingProof {
			header_chain: headers,
			internal_scheduling_parent_header: isp_header,
			signed_scheduling_info: None,
		};
		let result =
			check_scheduling(&proof, older_relay_parent, scheduling_parent, 3, TEST_MAX_CQ_OFFSET);

		assert_eq!(result, Err(SchedulingValidationError::MissingSignedSchedulingInfo));
	}

	#[test]
	fn valid_resubmission_with_signed_scheduling_info() {
		// Test: Resubmission with signed_scheduling_info passes validation
		// (signature verification happens separately).
		let (headers, isp_header) = make_header_chain(3);
		let scheduling_parent = headers[0].hash();
		let internal_scheduling_parent = isp_header.hash();
		// Use an unrelated hash as relay_parent (simulates resubmission where
		// relay_parent is an ancestor of internal_scheduling_parent)
		let older_relay_parent = RelayHash::repeat_byte(0xBB);

		let signed_info = dummy_signed(CoreSelector(0), internal_scheduling_parent);

		let proof = SchedulingProof {
			header_chain: headers,
			internal_scheduling_parent_header: isp_header,
			signed_scheduling_info: Some(signed_info),
		};
		let result =
			check_scheduling(&proof, older_relay_parent, scheduling_parent, 3, TEST_MAX_CQ_OFFSET);

		// Validation passes - signature verification is done separately
		let result = result.expect("valid resubmission proof");
		assert_eq!(result, internal_scheduling_parent);
	}

	// =========================================================================
	// validate_v3_scheduling tests
	// =========================================================================

	/// Helper: builds a valid V3 extension and scheduling proof for a given header chain length.
	/// Returns (extension, proof, expected_internal_scheduling_parent).
	fn make_v3_initial_submission(
		chain_len: u32,
	) -> (ValidationParamsExtension, SchedulingProof, RelayHash) {
		let (headers, isp_header) = make_header_chain(chain_len as usize);
		let relay_parent = isp_header.hash();
		let scheduling_parent = if headers.is_empty() { relay_parent } else { headers[0].hash() };

		let extension = ValidationParamsExtension::V3 { relay_parent, scheduling_parent };
		let proof = SchedulingProof {
			header_chain: headers,
			internal_scheduling_parent_header: isp_header,
			signed_scheduling_info: None,
		};
		(extension, proof, relay_parent)
	}

	#[test]
	fn v3_disabled_no_extension_returns_none() {
		let result = validate_v3_scheduling(false, &None, None, 0, TEST_MAX_CQ_OFFSET);
		assert!(result.is_none());
	}

	#[test]
	#[should_panic(expected = "V3 extension present but V3 scheduling is disabled")]
	fn v3_disabled_with_extension_panics() {
		let ext = ValidationParamsExtension::V3 {
			relay_parent: RelayHash::default(),
			scheduling_parent: RelayHash::default(),
		};
		validate_v3_scheduling(false, &Some(ext), None, 0, TEST_MAX_CQ_OFFSET);
	}

	#[test]
	#[should_panic(expected = "V3 scheduling is enabled but no V3 extension present")]
	fn v3_enabled_no_extension_panics() {
		validate_v3_scheduling(true, &None, None, 0, TEST_MAX_CQ_OFFSET);
	}

	#[rstest]
	#[case::empty(0)]
	#[case::len_3(3)]
	fn v3_enabled_valid_initial_submission(#[case] chain_len: u32) {
		let (ext, proof, expected) = make_v3_initial_submission(chain_len);
		let result =
			validate_v3_scheduling(true, &Some(ext), Some(&proof), chain_len, TEST_MAX_CQ_OFFSET)
				.expect("valid initial submission");
		assert_eq!(result.internal_scheduling_parent_header.hash(), expected);
	}

	#[test]
	#[should_panic(expected = "V3 candidates require ParachainBlockData::V2 with scheduling_proof")]
	fn v3_enabled_missing_scheduling_proof_panics() {
		let (ext, _, _) = make_v3_initial_submission(3);
		// Pass None as scheduling_proof to simulate a V0/V1 POV
		validate_v3_scheduling(true, &Some(ext), None, 3, TEST_MAX_CQ_OFFSET);
	}

	#[test]
	#[should_panic(expected = "V3 scheduling validation failed")]
	fn v3_enabled_invalid_header_chain_length_panics() {
		let (ext, proof, _) = make_v3_initial_submission(3);
		// Expect 5 headers but proof only has 3
		validate_v3_scheduling(true, &Some(ext), Some(&proof), 5, TEST_MAX_CQ_OFFSET);
	}

	#[test]
	fn v3_enabled_valid_resubmission() {
		let (headers, isp_header) = make_header_chain(3);
		let scheduling_parent = headers[0].hash();
		let internal_scheduling_parent = isp_header.hash();
		// Use an unrelated hash as relay_parent to simulate a resubmission
		let older_relay_parent = RelayHash::repeat_byte(0xBB);

		let ext =
			ValidationParamsExtension::V3 { relay_parent: older_relay_parent, scheduling_parent };
		let proof = SchedulingProof {
			header_chain: headers,
			internal_scheduling_parent_header: isp_header,
			signed_scheduling_info: Some(dummy_signed(CoreSelector(0), internal_scheduling_parent)),
		};

		let result = validate_v3_scheduling(true, &Some(ext), Some(&proof), 3, TEST_MAX_CQ_OFFSET);
		let result = result.expect("should succeed");
		assert_eq!(result.internal_scheduling_parent_header.hash(), internal_scheduling_parent);
		assert!(result.signed_scheduling_info.is_some());
	}

	#[test]
	#[should_panic(expected = "V3 scheduling validation failed")]
	fn v3_enabled_resubmission_without_signature_panics() {
		let (headers, isp_header) = make_header_chain(3);
		let scheduling_parent = headers[0].hash();
		let older_relay_parent = RelayHash::repeat_byte(0xBB);

		let ext =
			ValidationParamsExtension::V3 { relay_parent: older_relay_parent, scheduling_parent };
		let proof = SchedulingProof {
			header_chain: headers,
			internal_scheduling_parent_header: isp_header,
			signed_scheduling_info: None,
		};

		// Should panic because resubmission requires signed_scheduling_info
		validate_v3_scheduling(true, &Some(ext), Some(&proof), 3, TEST_MAX_CQ_OFFSET);
	}

	#[test]
	fn empty_chain_with_signed_info_passes_when_relay_parent_matches() {
		// With an empty chain and `relay_parent == scheduling_parent`, the candidate
		// is an initial submission. An accompanying `signed_scheduling_info` is legal
		// (collators may refuse stale info, but `check_scheduling` doesn't forbid it).
		let (_, isp_header) = make_header_chain(0);
		let scheduling_parent = isp_header.hash();
		let relay_parent = scheduling_parent;
		let proof = SchedulingProof {
			header_chain: vec![],
			internal_scheduling_parent_header: isp_header,
			signed_scheduling_info: Some(dummy_signed(CoreSelector(0), scheduling_parent)),
		};
		let result =
			check_scheduling(&proof, relay_parent, scheduling_parent, 0, TEST_MAX_CQ_OFFSET);
		assert!(result.is_ok());
	}

	#[test]
	fn empty_chain_with_mismatched_relay_parent_is_resubmission() {
		// With `RelayParentOffset = 0` the header chain is always empty, for both
		// initial submissions and resubmissions. When `relay_parent != scheduling_parent`
		// the candidate is a resubmission: `internal_scheduling_parent` falls back to
		// `scheduling_parent`, and the linkage check (against the proof's ISP header)
		// is what ultimately rejects an inconsistent proof.
		let (_, isp_header) = make_header_chain(0);
		let scheduling_parent = isp_header.hash();
		let relay_parent = RelayHash::repeat_byte(0xBB);
		let proof = SchedulingProof {
			header_chain: vec![],
			internal_scheduling_parent_header: isp_header,
			signed_scheduling_info: Some(dummy_signed(CoreSelector(0), scheduling_parent)),
		};
		let result =
			check_scheduling(&proof, relay_parent, scheduling_parent, 0, TEST_MAX_CQ_OFFSET)
				.unwrap();
		assert_eq!(result, scheduling_parent);
	}

	#[test]
	fn empty_chain_resubmission_without_signed_info_is_rejected() {
		// Empty chain + `relay_parent != scheduling_parent` is treated as a resubmission;
		// without `signed_scheduling_info` we reject as we would for any other resubmission.
		let (_, isp_header) = make_header_chain(0);
		let scheduling_parent = isp_header.hash();
		let relay_parent = RelayHash::repeat_byte(0xBB);
		let proof = SchedulingProof {
			header_chain: vec![],
			internal_scheduling_parent_header: isp_header,
			signed_scheduling_info: None,
		};
		let result =
			check_scheduling(&proof, relay_parent, scheduling_parent, 0, TEST_MAX_CQ_OFFSET);
		assert_eq!(result, Err(SchedulingValidationError::MissingSignedSchedulingInfo));
	}

	#[test]
	fn reject_unlinked_internal_scheduling_parent_header() {
		// ISP header that does not hash to the derived internal_scheduling_parent must
		// be rejected: otherwise a collator could point the verifier at an arbitrary
		// slot to satisfy the author lookup.
		let (headers, real_isp_header) = make_header_chain(3);
		let scheduling_parent = headers[0].hash();
		let relay_parent = real_isp_header.hash();
		// An unrelated header with a different block number → different hash.
		let unrelated_isp_header = RelayHeader::new(
			42u32,
			Default::default(),
			Default::default(),
			Default::default(),
			Default::default(),
		);

		let proof = SchedulingProof {
			header_chain: headers,
			internal_scheduling_parent_header: unrelated_isp_header,
			signed_scheduling_info: None,
		};
		let result =
			check_scheduling(&proof, relay_parent, scheduling_parent, 3, TEST_MAX_CQ_OFFSET);
		assert_eq!(result, Err(SchedulingValidationError::InternalSchedulingParentHeaderMismatch));
	}

	#[test]
	fn reject_signed_info_with_mismatched_isp() {
		// A signed payload whose `internal_scheduling_parent` doesn't match the ISP
		// derived from the proof must be rejected here, not just at signature-verifier
		// time. Without this, an eligible author could sign a payload claiming a stale
		// ISP and the verifier's signature check would still succeed over those bytes.
		let (headers, isp_header) = make_header_chain(3);
		let scheduling_parent = headers[0].hash();
		let older_relay_parent = RelayHash::repeat_byte(0xBB);

		// Payload commits to a different ISP than the proof carries.
		let wrong_isp = RelayHash::repeat_byte(0xCC);
		let signed_info = dummy_signed(CoreSelector(0), wrong_isp);

		let proof = SchedulingProof {
			header_chain: headers,
			internal_scheduling_parent_header: isp_header,
			signed_scheduling_info: Some(signed_info),
		};
		let result =
			check_scheduling(&proof, older_relay_parent, scheduling_parent, 3, TEST_MAX_CQ_OFFSET);
		assert_eq!(result, Err(SchedulingValidationError::SignedSchedulingInfoIspMismatch));
	}

	// =========================================================================
	// claim_queue_offset bound (step 7b) tests
	// =========================================================================

	/// Build a resubmission proof (empty chain, `relay_parent != scheduling_parent`) whose
	/// signed payload carries the given `claim_queue_offset`. Used to drive the offset-bound
	/// check in `check_scheduling`.
	fn resubmission_proof_with_offset(offset: u8) -> (SchedulingProof, RelayHash, RelayHash) {
		let (_, isp_header) = make_header_chain(0);
		let scheduling_parent = isp_header.hash();
		let relay_parent = RelayHash::repeat_byte(0xBB);
		let signed = SignedSchedulingInfo {
			payload: SchedulingInfoPayload::new(
				CoreSelector(0),
				offset,
				Default::default(),
				scheduling_parent,
			),
			signature: dummy_signature(),
		};
		let proof = SchedulingProof {
			header_chain: vec![],
			internal_scheduling_parent_header: isp_header,
			signed_scheduling_info: Some(signed),
		};
		(proof, relay_parent, scheduling_parent)
	}

	#[test]
	fn reject_resubmission_offset_exceeding_cap() {
		// A signed offset above the runtime cap is rejected: on resubmission the offset is
		// taken from the signed payload and overrides the block's emitted value, so the
		// bound must be re-applied here (the in-block check is bypassed).
		let (proof, relay_parent, scheduling_parent) =
			resubmission_proof_with_offset(TEST_MAX_CQ_OFFSET + 1);
		let result =
			check_scheduling(&proof, relay_parent, scheduling_parent, 0, TEST_MAX_CQ_OFFSET);
		assert_eq!(
			result,
			Err(SchedulingValidationError::ClaimQueueOffsetTooLarge {
				offset: TEST_MAX_CQ_OFFSET + 1,
				max: TEST_MAX_CQ_OFFSET,
			})
		);
	}

	#[test]
	fn accept_resubmission_offset_at_cap() {
		// An offset exactly at the cap is within bounds and passes.
		let (proof, relay_parent, scheduling_parent) =
			resubmission_proof_with_offset(TEST_MAX_CQ_OFFSET);
		check_scheduling(&proof, relay_parent, scheduling_parent, 0, TEST_MAX_CQ_OFFSET)
			.expect("offset at cap is valid");
	}

	// =========================================================================
	// SchedulingSignals tests
	// =========================================================================

	fn signed_with(
		core_selector: CoreSelector,
		claim_queue_offset: u8,
		peer_id: ApprovedPeerId,
	) -> SignedSchedulingInfo {
		SignedSchedulingInfo {
			payload: SchedulingInfoPayload::new(
				core_selector,
				claim_queue_offset,
				peer_id,
				Default::default(),
			),
			signature: [0u8; 64],
		}
	}

	fn peer(byte: u8) -> ApprovedPeerId {
		ApprovedPeerId::try_from(vec![byte; 4]).expect("4 bytes fits the bound; qed")
	}

	/// A `BoundedVec` with the same shape as `validate_block`'s `upward_messages`, for
	/// exercising `SchedulingSignals::emit`.
	type TestUpwardMessages = BoundedVec<Vec<u8>, frame_support::traits::ConstU32<1024>>;

	#[test]
	fn from_block_signals_roundtrips_select_core_and_approved_peer() {
		// Both signals present: parsed into the canonical tail, then emitted as
		// [SEPARATOR, SelectCore, ApprovedPeer] in that exact order.
		let raw = vec![
			UMPSignal::SelectCore(CoreSelector(7), ClaimQueueOffset(1)).encode(),
			UMPSignal::ApprovedPeer(peer(0xAA)).encode(),
		];
		let mut out = TestUpwardMessages::default();
		SchedulingSignals::from_block_signals(&raw, &mut out);
		assert_eq!(
			out.into_inner(),
			vec![
				UMP_SEPARATOR,
				UMPSignal::SelectCore(CoreSelector(7), ClaimQueueOffset(1)).encode(),
				UMPSignal::ApprovedPeer(peer(0xAA)).encode(),
			]
		);
	}

	#[test]
	fn from_block_signals_select_core_only() {
		// Block emitted only a `SelectCore`: no `ApprovedPeer` field, one signal emitted.
		let raw = vec![UMPSignal::SelectCore(CoreSelector(3), ClaimQueueOffset(0)).encode()];
		let mut out = TestUpwardMessages::default();
		SchedulingSignals::from_block_signals(&raw, &mut out);
		assert_eq!(
			out.into_inner(),
			vec![
				UMP_SEPARATOR,
				UMPSignal::SelectCore(CoreSelector(3), ClaimQueueOffset(0)).encode()
			]
		);
	}

	#[test]
	#[should_panic(expected = "more than one `SelectCore`")]
	fn from_block_signals_panics_on_duplicate_select_core_same_value() {
		// Two identical `SelectCore` signals: still an error. The relay decoder counts
		// occurrences, not distinct values, so matching duplicates would be rejected too.
		let raw = vec![
			UMPSignal::SelectCore(CoreSelector(1), ClaimQueueOffset(0)).encode(),
			UMPSignal::SelectCore(CoreSelector(1), ClaimQueueOffset(0)).encode(),
		];
		SchedulingSignals::from_block_signals(&raw, &mut TestUpwardMessages::default());
	}

	#[test]
	#[should_panic(expected = "more than one `SelectCore`")]
	fn from_block_signals_panics_on_duplicate_select_core_different_value() {
		let raw = vec![
			UMPSignal::SelectCore(CoreSelector(1), ClaimQueueOffset(0)).encode(),
			UMPSignal::SelectCore(CoreSelector(2), ClaimQueueOffset(0)).encode(),
		];
		SchedulingSignals::from_block_signals(&raw, &mut TestUpwardMessages::default());
	}

	#[test]
	#[should_panic(expected = "more than one `ApprovedPeer`")]
	fn from_block_signals_panics_on_duplicate_approved_peer() {
		let raw = vec![
			UMPSignal::ApprovedPeer(peer(0xAA)).encode(),
			UMPSignal::ApprovedPeer(peer(0xBB)).encode(),
		];
		SchedulingSignals::from_block_signals(&raw, &mut TestUpwardMessages::default());
	}

	#[test]
	fn from_block_signals_empty_emits_nothing() {
		// No signals in, nothing out — not even a separator.
		let mut out = TestUpwardMessages::default();
		SchedulingSignals::from_block_signals(&[], &mut out);
		assert!(out.is_empty());
	}

	#[test]
	fn from_scheduling_info_sources_all_fields() {
		// All three values — `core_selector`, `claim_queue_offset`, `peer_id` — are signed
		// by the resubmitting collator, so the override sources every field from the signed
		// payload. Distinct values ensure no field is sourced from the wrong place.
		let signed = signed_with(CoreSelector(7), 3, peer(0xAA));
		let mut out = TestUpwardMessages::default();
		SchedulingSignals::from_scheduling_info(&signed, &mut out);
		assert_eq!(
			out.into_inner(),
			vec![
				UMP_SEPARATOR,
				UMPSignal::SelectCore(CoreSelector(7), ClaimQueueOffset(3)).encode(),
				UMPSignal::ApprovedPeer(peer(0xAA)).encode(),
			]
		);
	}

	#[test]
	fn from_scheduling_info_emits_peer_verbatim_even_if_empty() {
		// The payload `peer_id` is a plain (non-`Option`) type → always-override. An empty
		// peer is emitted verbatim as `ApprovedPeer([])`, NOT omitted and NOT replaced by the
		// block's peer. Empty/invalid peers are the resubmitter's own reputation loss and are
		// handled gracefully downstream; the PVF forwards exactly what was signed.
		let signed = signed_with(CoreSelector(5), 1, ApprovedPeerId::default());
		let mut out = TestUpwardMessages::default();
		SchedulingSignals::from_scheduling_info(&signed, &mut out);
		assert_eq!(
			out.into_inner(),
			vec![
				UMP_SEPARATOR,
				UMPSignal::SelectCore(CoreSelector(5), ClaimQueueOffset(1)).encode(),
				UMPSignal::ApprovedPeer(ApprovedPeerId::default()).encode(),
			]
		);
	}

	#[test]
	fn from_scheduling_info_emits_even_when_block_emitted_nothing() {
		// The override is authoritative and independent of what the block emitted: a
		// resubmission always produces its tail. (At the call site this is what decouples
		// the override from the old `!upward_message_signals.is_empty()` guard.)
		let signed = signed_with(CoreSelector(0), 0, peer(0xCC));
		let mut out = TestUpwardMessages::default();
		SchedulingSignals::from_scheduling_info(&signed, &mut out);
		assert!(!out.is_empty());
	}
}
