// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023 Snowfork <hello@snowfork.com>
use super::*;
use frame_support::ensure;
use snowbridge_beacon_primitives::ExecutionProof;

use alloy_primitives::Log as AlloyLog;
use snowbridge_beacon_primitives::merkle_proof::{generalized_index_length, subtree_index};
use snowbridge_verification_primitives::{
	receipt::verify_receipt_proof,
	VerificationError::{self, *},
	Verifier, *,
};

impl<T: Config> Verifier for Pallet<T> {
	/// Verify a message by verifying the existence of the corresponding
	/// Ethereum log in a block. Returns the log if successful. The execution header containing
	/// the log is sent with the message. The beacon header containing the execution header
	/// is also sent with the message, to check if the header is an ancestor of a finalized
	/// header.
	fn verify(event_log: &Log, proof: &Proof) -> Result<(), VerificationError> {
		// Refuse to verify any Ethereum-side proof while the beacon light client is halted.
		// Governance halts the light client when it suspects a compromise (e.g. sync committee
		// takeover), at which point any signed headers/receipts must be treated as untrusted.
		// Covers every Verifier consumer, including `inbound_queue_v2::submit` and
		// `outbound_queue_v2::submit_delivery_receipt` (which would otherwise still drain
		// pending relayer rewards while the bridge is halted).
		ensure!(!Self::operating_mode().is_halted(), VerificationError::Halted);

		Self::verify_execution_proof(&proof.execution_proof)
			.map_err(|e| InvalidExecutionProof(e.into()))?;

		Self::verify_receipt_inclusion(
			proof.execution_proof.execution_header.receipts_root(),
			event_log.tx_index,
			&proof.receipt_proof,
			event_log,
		)?;

		Ok(())
	}
}

impl<T: Config> Pallet<T> {
	/// Verifies that the receipt encoded in `proof.data` is included in the block given by
	/// `proof.block_hash`.
	pub fn verify_receipt_inclusion(
		receipts_root: H256,
		tx_index: u64,
		receipt_proof: &[Vec<u8>],
		log: &Log,
	) -> Result<(), VerificationError> {
		let receipt =
			verify_receipt_proof(receipts_root, tx_index, receipt_proof).ok_or(InvalidProof)?;
		if !receipt.logs().iter().any(|l| Self::check_log_match(log, l)) {
			tracing::error!(
				target: "ethereum-client",
				"💫 Event log not found in receipt for transaction",
			);
			return Err(LogNotFound);
		}
		Ok(())
	}

	fn check_log_match(log: &Log, receipt_log: &AlloyLog) -> bool {
		let equal = receipt_log.data.data.0 == log.data &&
			receipt_log.address.0 == log.address.0 &&
			receipt_log.topics().len() == log.topics.len();
		if !equal {
			return false;
		}
		for (_, (topic1, topic2)) in receipt_log.topics().iter().zip(log.topics.iter()).enumerate()
		{
			if topic1.0 != topic2.0 {
				return false;
			}
		}
		true
	}

	/// Validates an execution header with ancestry_proof against a finalized checkpoint on
	/// chain.The beacon header containing the execution header is sent, plus the execution header,
	/// along with a proof that the execution header is rooted in the beacon header body.
	pub(crate) fn verify_execution_proof(execution_proof: &ExecutionProof) -> DispatchResult {
		let latest_finalized_state =
			FinalizedBeaconState::<T>::get(LatestFinalizedBlockRoot::<T>::get())
				.ok_or(Error::<T>::NotBootstrapped)?;
		// Checks that the header is an ancestor of a finalized header, using slot number.
		ensure!(
			execution_proof.header.slot <= latest_finalized_state.slot,
			Error::<T>::HeaderNotFinalized
		);

		let beacon_block_root: H256 = execution_proof
			.header
			.hash_tree_root()
			.map_err(|_| Error::<T>::HeaderHashTreeRootFailed)?;

		match &execution_proof.ancestry_proof {
			Some(proof) => {
				Self::verify_ancestry_proof(
					beacon_block_root,
					execution_proof.header.slot,
					&proof.header_branch,
					proof.finalized_block_root,
				)?;
			},
			None => {
				// If the ancestry proof is not provided, we expect this beacon header to be a
				// finalized beacon header. We need to check that the header hash matches the
				// finalized header root at the expected slot.
				let state = <FinalizedBeaconState<T>>::get(beacon_block_root)
					.ok_or(Error::<T>::ExpectedFinalizedHeaderNotStored)?;
				if execution_proof.header.slot != state.slot {
					return Err(Error::<T>::ExpectedFinalizedHeaderNotStored.into());
				}
			},
		}

		// Gets the hash tree root of the execution header, in preparation for the execution
		// header proof (used to check that the execution header is rooted in the beacon
		// header body.
		let execution_header_root: H256 = execution_proof
			.execution_header
			.hash_tree_root()
			.map_err(|_| Error::<T>::BlockBodyHashTreeRootFailed)?;

		let execution_header_gindex = Self::execution_header_gindex();
		ensure!(
			verify_merkle_branch(
				execution_header_root,
				&execution_proof.execution_branch,
				subtree_index(execution_header_gindex),
				generalized_index_length(execution_header_gindex),
				execution_proof.header.body_root
			),
			Error::<T>::InvalidExecutionHeaderProof
		);
		Ok(())
	}

	/// Verify that `block_root` is an ancestor of `finalized_block_root` Used to prove that
	/// an execution header is an ancestor of a finalized header (i.e. the blocks are
	/// on the same chain).
	fn verify_ancestry_proof(
		block_root: H256,
		block_slot: u64,
		block_root_proof: &[H256],
		finalized_block_root: H256,
	) -> DispatchResult {
		let state = <FinalizedBeaconState<T>>::get(finalized_block_root)
			.ok_or(Error::<T>::ExpectedFinalizedHeaderNotStored)?;

		ensure!(block_slot < state.slot, Error::<T>::HeaderNotFinalized);

		let index_in_array = block_slot % (SLOTS_PER_HISTORICAL_ROOT as u64);
		let leaf_index = (SLOTS_PER_HISTORICAL_ROOT as u64) + index_in_array;

		ensure!(
			verify_merkle_branch(
				block_root,
				block_root_proof,
				leaf_index as usize,
				config::BLOCK_ROOT_AT_INDEX_DEPTH,
				state.block_roots_root
			),
			Error::<T>::InvalidAncestryMerkleProof
		);

		Ok(())
	}
}
