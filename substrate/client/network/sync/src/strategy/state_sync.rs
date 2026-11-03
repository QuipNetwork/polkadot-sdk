// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! State sync support.

use crate::{
	schema::v1::{KeyValueStateEntry, StateEntry, StateRequest, StateResponse},
	LOG_TARGET,
};
use codec::{Decode, Encode};
use log::debug;
use sc_client_api::{CompactProof, KeyValueStates, ProofProvider};
use sc_consensus::ImportedState;
use smallvec::SmallVec;
use sp_core::storage::well_known_keys;
use sp_runtime::{
	traits::{Block as BlockT, Header, NumberFor},
	Justifications,
};
use std::{collections::HashMap, fmt, sync::Arc};

/// Generic state sync provider. Used for mocking in tests.
pub trait StateSyncProvider<B: BlockT>: Send + Sync {
	/// Validate and import a state response.
	fn import(&mut self, response: StateResponse) -> ImportResult<B>;
	/// Produce next state request.
	fn next_request(&self) -> StateRequest;
	/// Check if the state is complete.
	fn is_complete(&self) -> bool;
	/// Returns target block number.
	fn target_number(&self) -> NumberFor<B>;
	/// Returns target block hash.
	fn target_hash(&self) -> B::Hash;
	/// Returns state sync estimated progress.
	fn progress(&self) -> StateSyncProgress;
}

// Reported state sync phase.
#[derive(Clone, Eq, PartialEq, Debug)]
pub enum StateSyncPhase {
	// State download in progress.
	DownloadingState,
	// Download is complete, state is being imported.
	ImportingState,
}

impl fmt::Display for StateSyncPhase {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		match self {
			Self::DownloadingState => write!(f, "Downloading state"),
			Self::ImportingState => write!(f, "Importing state"),
		}
	}
}

/// Reported state download progress.
#[derive(Clone, Eq, PartialEq, Debug)]
pub struct StateSyncProgress {
	/// Estimated download percentage.
	pub percentage: u32,
	/// Total state size in bytes downloaded so far.
	pub size: u64,
	/// Current state sync phase.
	pub phase: StateSyncPhase,
}

/// Import state chunk result.
pub enum ImportResult<B: BlockT> {
	/// State is complete and ready for import.
	Import(B::Hash, B::Header, ImportedState<B>, Option<Vec<B::Extrinsic>>, Option<Justifications>),
	/// Continue downloading.
	Continue,
	/// Bad state chunk.
	BadResponse,
}

struct StateSyncMetadata<B: BlockT> {
	last_key: SmallVec<[Vec<u8>; 2]>,
	target_header: B::Header,
	target_body: Option<Vec<B::Extrinsic>>,
	target_justifications: Option<Justifications>,
	complete: bool,
	imported_bytes: u64,
	skip_proof: bool,
}

impl<B: BlockT> StateSyncMetadata<B> {
	fn target_hash(&self) -> B::Hash {
		self.target_header.hash()
	}

	/// Returns target block number.
	fn target_number(&self) -> NumberFor<B> {
		*self.target_header.number()
	}

	fn target_root(&self) -> B::Hash {
		*self.target_header.state_root()
	}

	fn next_request(&self) -> StateRequest {
		StateRequest {
			block: self.target_hash().encode(),
			start: self.last_key.clone().into_vec(),
			no_proof: self.skip_proof,
		}
	}

	fn progress(&self) -> StateSyncProgress {
		let cursor = *self.last_key.get(0).and_then(|last| last.get(0)).unwrap_or(&0u8);
		let percent_done = cursor as u32 * 100 / 256;
		StateSyncProgress {
			percentage: percent_done,
			size: self.imported_bytes,
			phase: if self.complete {
				StateSyncPhase::ImportingState
			} else {
				StateSyncPhase::DownloadingState
			},
		}
	}
}

/// State sync state machine.
///
/// Accumulates partial state data until it is ready to be imported.
pub struct StateSync<B: BlockT, Client> {
	metadata: StateSyncMetadata<B>,
	state: HashMap<Vec<u8>, (Vec<(Vec<u8>, Vec<u8>)>, Vec<Vec<u8>>)>,
	client: Arc<Client>,
}

impl<B, Client> StateSync<B, Client>
where
	B: BlockT,
	Client: ProofProvider<B> + Send + Sync + 'static,
{
	///  Create a new instance.
	pub fn new(
		client: Arc<Client>,
		target_header: B::Header,
		target_body: Option<Vec<B::Extrinsic>>,
		target_justifications: Option<Justifications>,
		skip_proof: bool,
	) -> Self {
		Self {
			client,
			metadata: StateSyncMetadata {
				last_key: SmallVec::default(),
				target_header,
				target_body,
				target_justifications,
				complete: false,
				imported_bytes: 0,
				skip_proof,
			},
			state: HashMap::default(),
		}
	}

	fn process_state_key_values(
		&mut self,
		state_root: Vec<u8>,
		key_values: impl IntoIterator<Item = (Vec<u8>, Vec<u8>)>,
	) {
		let is_top = state_root.is_empty();

		let entry = self.state.entry(state_root).or_default();

		if entry.0.len() > 0 && entry.1.len() > 1 {
			// Already imported child_trie with same root.
			// Warning this will not work with parallel download.
			return;
		}

		let mut child_storage_roots = Vec::new();

		for (key, value) in key_values {
			// Skip all child key root (will be recalculated on import)
			if is_top && well_known_keys::is_child_storage_key(key.as_slice()) {
				child_storage_roots.push((value, key));
			} else {
				self.metadata.imported_bytes += key.len() as u64;
				entry.0.push((key, value));
			}
		}

		for (root, storage_key) in child_storage_roots {
			self.state.entry(root).or_default().1.push(storage_key);
		}
	}

	fn process_state_verified(&mut self, values: KeyValueStates) {
		for values in values.0 {
			self.process_state_key_values(values.state_root, values.key_values);
		}
	}

	fn process_state_unverified(&mut self, response: StateResponse) -> bool {
		let mut complete = true;
		// if the trie is a child trie and one of its parent trie is empty,
		// the parent cursor stays valid.
		// Empty parent trie content only happens when all the response content
		// is part of a single child trie.
		let first_entry_is_empty =
			response.entries.first().is_some_and(|entry| entry.entries.is_empty());
		if self.metadata.last_key.len() == 2 && first_entry_is_empty {
			// Do not remove the parent trie position.
			self.metadata.last_key.pop();
		} else {
			self.metadata.last_key.clear();
		}
		for state in response.entries {
			debug!(
				target: LOG_TARGET,
				"Importing state from {:?} to {:?}",
				state.entries.last().map(|e| sp_core::hexdisplay::HexDisplay::from(&e.key)),
				state.entries.first().map(|e| sp_core::hexdisplay::HexDisplay::from(&e.key)),
			);

			if !state.complete {
				if let Some(e) = state.entries.last() {
					self.metadata.last_key.push(e.key.clone());
				}
				complete = false;
			}

			let KeyValueStateEntry { state_root, entries, complete: _ } = state;
			self.process_state_key_values(
				state_root,
				entries.into_iter().map(|StateEntry { key, value }| (key, value)),
			);
		}
		complete
	}
}

impl<B, Client> StateSyncProvider<B> for StateSync<B, Client>
where
	B: BlockT,
	Client: ProofProvider<B> + Send + Sync + 'static,
{
	///  Validate and import a state response.
	fn import(&mut self, response: StateResponse) -> ImportResult<B> {
		if self.metadata.skip_proof && response.entries.is_empty() {
			debug!(target: LOG_TARGET, "Missing state entries");
			return ImportResult::BadResponse;
		} else if !self.metadata.skip_proof && response.proof.is_empty() {
			debug!(target: LOG_TARGET, "Missing proof");
			return ImportResult::BadResponse;
		}
		let complete = if !self.metadata.skip_proof {
			debug!(target: LOG_TARGET, "Importing state from {} trie nodes", response.proof.len());
			let proof_size = response.proof.len() as u64;
			let proof = match CompactProof::decode(&mut response.proof.as_ref()) {
				Ok(proof) => proof,
				Err(e) => {
					debug!(target: LOG_TARGET, "Error decoding proof: {:?}", e);
					return ImportResult::BadResponse;
				},
			};
			let (values, completed) = match self.client.verify_range_proof(
				self.metadata.target_root(),
				proof,
				self.metadata.last_key.as_slice(),
			) {
				Err(e) => {
					debug!(
						target: LOG_TARGET,
						"StateResponse failed proof verification: {}",
						e,
					);
					return ImportResult::BadResponse;
				},
				Ok(values) => values,
			};
			debug!(target: LOG_TARGET, "Imported with {} keys", values.len());

			let complete = completed == 0;
			if !complete && !values.update_last_key(completed, &mut self.metadata.last_key) {
				debug!(target: LOG_TARGET, "Error updating key cursor, depth: {}", completed);
			};

			self.process_state_verified(values);
			self.metadata.imported_bytes += proof_size;
			complete
		} else {
			self.process_state_unverified(response)
		};
		if complete {
			self.metadata.complete = true;
			let target_hash = self.metadata.target_hash();
			ImportResult::Import(
				target_hash,
				self.metadata.target_header.clone(),
				ImportedState { block: target_hash, state: std::mem::take(&mut self.state).into() },
				self.metadata.target_body.clone(),
				self.metadata.target_justifications.clone(),
			)
		} else {
			ImportResult::Continue
		}
	}

	/// Produce next state request.
	fn next_request(&self) -> StateRequest {
		self.metadata.next_request()
	}

	/// Check if the state is complete.
	fn is_complete(&self) -> bool {
		self.metadata.complete
	}

	/// Returns target block number.
	fn target_number(&self) -> NumberFor<B> {
		self.metadata.target_number()
	}

	/// Returns target block hash.
	fn target_hash(&self) -> B::Hash {
		self.metadata.target_hash()
	}

	/// Returns state sync estimated progress.
	fn progress(&self) -> StateSyncProgress {
		self.metadata.progress()
	}
}
