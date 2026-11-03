// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// Cumulus is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Cumulus is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Cumulus. If not, see <https://www.gnu.org/licenses/>.

//! Proof size recording utilities.

use codec::{Decode, Encode};
use sc_client_api::{
	backend::AuxStore,
	client::{AuxDataOperations, FinalityNotification, PreCommitActions},
};
use sp_blockchain::{Error as ClientError, Result as ClientResult};
use sp_runtime::traits::Block as BlockT;
use sp_trie::proof_size_extension::RecordedProofSizeEstimations;
use std::sync::Arc;

const PROOF_SIZE_RECORDING_VERSION: &[u8] = b"cumulus_proof_size_recording_version";
const PROOF_SIZE_RECORDING_CURRENT_VERSION: u32 = 1;

/// The aux storage key used to store the proof size recordings for the given block hash.
fn proof_size_recording_key<H: Encode>(block_hash: H) -> Vec<u8> {
	(b"cumulus_proof_size_recording", block_hash).encode()
}

fn load_decode<B, T>(backend: &B, key: &[u8]) -> ClientResult<Option<T>>
where
	B: AuxStore,
	T: Decode,
{
	let corrupt = |e: codec::Error| {
		ClientError::Backend(format!("Proof size recording DB is corrupted. Decode error: {}", e))
	};
	match backend.get_aux(key)? {
		None => Ok(None),
		Some(t) => T::decode(&mut &t[..]).map(Some).map_err(corrupt),
	}
}

/// Prepare aux storage key-value pairs for persisting proof size recordings.
///
/// Returns the key-value pairs that need to be written to the aux storage.
pub fn prepare_proof_size_recording_aux_data<H: Encode>(
	block_hash: H,
	recordings: Vec<u32>,
) -> impl Iterator<Item = (Vec<u8>, Vec<u8>)> {
	let current_version = PROOF_SIZE_RECORDING_CURRENT_VERSION.encode();
	let key = proof_size_recording_key(block_hash);
	let recordings = recordings.encode();

	[(key, recordings), (PROOF_SIZE_RECORDING_VERSION.to_vec(), current_version)].into_iter()
}

/// Load the proof size recordings associated with a block.
pub fn load_proof_size_recording<H: Encode, B: AuxStore>(
	backend: &B,
	block_hash: H,
) -> ClientResult<Option<RecordedProofSizeEstimations>> {
	let version = load_decode::<_, u32>(backend, PROOF_SIZE_RECORDING_VERSION)?;

	match version {
		None => Ok(None),
		Some(PROOF_SIZE_RECORDING_CURRENT_VERSION) => {
			load_decode::<_, Vec<u32>>(backend, proof_size_recording_key(block_hash).as_slice())
				.map(|recordings| recordings.map(Into::into))
		},
		Some(other) => Err(ClientError::Backend(format!(
			"Unsupported proof size recording DB version: {:?}",
			other
		))),
	}
}

/// Cleanup auxiliary storage for finalized blocks.
///
/// This function removes proof size recordings for blocks that are no longer needed
/// after finalization. It processes the finalized blocks and their stale heads to
/// determine which recordings can be safely removed.
fn aux_storage_cleanup<Block>(notification: &FinalityNotification<Block>) -> AuxDataOperations
where
	Block: BlockT,
{
	// Convert the hashes to deletion operations
	notification
		.stale_blocks
		.iter()
		.map(|b| (proof_size_recording_key(b.hash), None))
		.collect()
}

/// Register a finality action for cleaning up proof size recordings.
///
/// This should be called during consensus initialization to automatically clean up
/// proof size recordings when blocks are finalized.
pub fn register_proof_size_recording_cleanup<C, Block>(client: Arc<C>)
where
	C: PreCommitActions<Block> + 'static,
	Block: BlockT,
{
	let on_finality = move |notification: &FinalityNotification<Block>| -> AuxDataOperations {
		aux_storage_cleanup(notification)
	};

	client.register_finality_action(Box::new(on_finality));
}
