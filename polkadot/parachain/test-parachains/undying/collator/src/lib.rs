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

//! Collator for the `Undying` test parachain.

use codec::{Decode, Encode};
use futures::StreamExt;
use futures_timer::Delay;
use polkadot_cli::ProvideRuntimeApi;
use polkadot_node_primitives::{
	maybe_compress_pov, AvailableData, Collation, CollationResult, CollatorFn, MaybeCompressedPoV,
	PoV, UpwardMessages,
};
use polkadot_node_subsystem::messages::{CollatorProtocolMessage, Segment, SegmentEntry};
use polkadot_primitives::{
	CandidateCommitments, CandidateDescriptorV2, CandidateReceiptV2, ClaimQueueOffset, CollatorId,
	CollatorPair, CoreIndex, Hash, Id as ParaId, OccupiedCoreAssumption,
	DEFAULT_CLAIM_QUEUE_OFFSET,
};
use polkadot_service::{Handle, NewFull, ParachainHost};
use sc_client_api::client::BlockchainEvents;
use sp_core::Pair;

use std::{
	collections::HashMap,
	sync::{Arc, Mutex},
	time::Duration,
};
use test_parachain_undying::{
	execute, hash_state, BlockData, GraveyardState, HeadData, StateMismatch,
};

pub const LOG_TARGET: &str = "parachain::undying-collator";

/// Default PoV size which also drives state size.
const DEFAULT_POV_SIZE: usize = 1000;
/// Default PVF time complexity - 1 signature per block.
const DEFAULT_PVF_COMPLEXITY: u32 = 1;

/// Calculates the head and state for the block with the given `number`.
fn calculate_head_and_state_for_number(
	number: u64,
	graveyard_size: usize,
	pvf_complexity: u32,
	experimental_send_approved_peer: bool,
) -> Result<(HeadData, GraveyardState), StateMismatch> {
	let index = 0u64;
	let mut graveyard = vec![0u8; graveyard_size * graveyard_size];
	let zombies = 0;
	let seal = [0u8; 32];
	let core_selector_number = 0;

	// Ensure a larger compressed PoV.
	graveyard.iter_mut().enumerate().for_each(|(i, grave)| {
		*grave = i as u8;
	});

	let mut state = GraveyardState { index, graveyard, zombies, seal, core_selector_number };
	let mut head =
		HeadData { number: 0, parent_hash: Hash::default().into(), post_state: hash_state(&state) };

	while head.number < number {
		let block = BlockData {
			state,
			tombstones: 1_000,
			iterations: pvf_complexity,
			experimental_send_approved_peer,
		};
		let (new_head, new_state, _) = execute(head.hash(), head.clone(), block)?;
		head = new_head;
		state = new_state;
	}

	Ok((head, state))
}

/// The state of the undying parachain.
struct State {
	// We need to keep these around until the including relay chain blocks are finalized.
	// This is because disputes can trigger reverts up to last finalized block, so we
	// want that state to collate on older relay chain heads.
	head_to_state: HashMap<Arc<HeadData>, GraveyardState>,
	number_to_head: HashMap<u64, Arc<HeadData>>,
	/// Block number of the best block.
	best_block: u64,
	/// PVF time complexity.
	pvf_complexity: u32,
	/// Defines the state size (Vec<u8>). Our PoV includes the entire state so this value will
	/// drive the PoV size.
	/// Important note: block execution heavily clones this state, so something like 300.000 is
	/// the max value here, otherwise we'll get OOM during wasm execution.
	/// TODO: Implement a static state, and use `ballast` to inflate the PoV size. This way
	/// we can just discard the `ballast` before processing the block.
	graveyard_size: usize,
	experimental_send_approved_peer: bool,
}

impl State {
	/// Init the genesis state.
	fn genesis(
		graveyard_size: usize,
		pvf_complexity: u32,
		experimental_send_approved_peer: bool,
	) -> Self {
		let index = 0u64;
		let mut graveyard = vec![0u8; graveyard_size * graveyard_size];
		let zombies = 0;
		let seal = [0u8; 32];
		let core_selector_number = 0;

		// Ensure a larger compressed PoV.
		graveyard.iter_mut().enumerate().for_each(|(i, grave)| {
			*grave = i as u8;
		});

		let state = GraveyardState { index, graveyard, zombies, seal, core_selector_number };

		let head_data =
			HeadData { number: 0, parent_hash: Default::default(), post_state: hash_state(&state) };
		let head_data = Arc::new(head_data);

		Self {
			head_to_state: vec![(head_data.clone(), state.clone())].into_iter().collect(),
			number_to_head: vec![(0, head_data)].into_iter().collect(),
			best_block: 0,
			pvf_complexity,
			graveyard_size,
			experimental_send_approved_peer,
		}
	}

	/// Advance the state and produce a new block based on the given `parent_head`.
	///
	/// Returns the new [`BlockData`] and the new [`HeadData`].
	fn advance(
		&mut self,
		parent_head: HeadData,
	) -> Result<(BlockData, HeadData, UpwardMessages), StateMismatch> {
		self.best_block = parent_head.number;

		let state = if let Some(state) = self
			.number_to_head
			.get(&self.best_block)
			.and_then(|head_data| self.head_to_state.get(head_data).cloned())
		{
			state
		} else {
			let (_, state) = calculate_head_and_state_for_number(
				parent_head.number,
				self.graveyard_size,
				self.pvf_complexity,
				self.experimental_send_approved_peer,
			)?;
			state
		};

		// Start with prev state and transaction to execute (place 1000 tombstones).
		let block = BlockData {
			state,
			tombstones: 1000,
			iterations: self.pvf_complexity,
			experimental_send_approved_peer: self.experimental_send_approved_peer,
		};

		let (new_head, new_state, upward_messages) =
			execute(parent_head.hash(), parent_head, block.clone())?;

		let new_head_arc = Arc::new(new_head.clone());

		self.head_to_state.insert(new_head_arc.clone(), new_state);
		self.number_to_head.insert(new_head.number, new_head_arc);

		Ok((block, new_head, upward_messages))
	}
}

/// The collator of the undying parachain.
pub struct Collator {
	state: Arc<Mutex<State>>,
	key: CollatorPair,
}

impl Default for Collator {
	fn default() -> Self {
		Self::new(DEFAULT_POV_SIZE, DEFAULT_PVF_COMPLEXITY, false)
	}
}

impl Collator {
	/// Create a new collator instance with the state initialized from genesis and `pov_size`
	/// parameter. The same parameter needs to be passed when exporting the genesis state.
	pub fn new(
		pov_size: usize,
		pvf_complexity: u32,
		experimental_send_approved_peer: bool,
	) -> Self {
		let graveyard_size = ((pov_size / std::mem::size_of::<u8>()) as f64).sqrt().ceil() as usize;

		log::info!(
			target: LOG_TARGET,
			"PoV target size: {} bytes. Graveyard size: ({} x {})",
			pov_size,
			graveyard_size,
			graveyard_size,
		);

		log::info!(
			target: LOG_TARGET,
			"PVF time complexity: {}",
			pvf_complexity,
		);

		Self {
			state: Arc::new(Mutex::new(State::genesis(
				graveyard_size,
				pvf_complexity,
				experimental_send_approved_peer,
			))),
			key: CollatorPair::generate().0,
		}
	}

	/// Get the SCALE encoded genesis head of the parachain.
	pub fn genesis_head(&self) -> Vec<u8> {
		self.state
			.lock()
			.unwrap()
			.number_to_head
			.get(&0)
			.expect("Genesis header exists")
			.encode()
	}

	/// Get the validation code of the undying parachain.
	pub fn validation_code(&self) -> &[u8] {
		test_parachain_undying::wasm_binary_unwrap()
	}

	/// Get the collator key.
	pub fn collator_key(&self) -> CollatorPair {
		self.key.clone()
	}

	/// Get the collator id.
	pub fn collator_id(&self) -> CollatorId {
		self.key.public()
	}

	/// Create the collation function.
	///
	/// This collation function can be plugged into the overseer to generate collations for the
	/// undying parachain.
	pub fn create_collation_function(
		&self,
		_spawner: impl SpawnNamed + Clone + 'static,
	) -> CollatorFn {
		use futures::FutureExt as _;

		let state = self.state.clone();

		Box::new(move |relay_parent, validation_data| {
			let parent = match HeadData::decode(&mut &validation_data.parent_head.0[..]) {
				Err(err) => {
					log::error!(
						target: LOG_TARGET,
						"Requested to build on top of malformed head-data: {:?}",
						err,
					);
					return futures::future::ready(None).boxed();
				},
				Ok(p) => p,
			};

			let (block_data, head_data, upward_messages) =
				match state.lock().unwrap().advance(parent.clone()) {
					Err(err) => {
						log::error!(
							target: LOG_TARGET,
							"Unable to build on top of {:?}: {:?}",
							parent,
							err,
						);
						return futures::future::ready(None).boxed();
					},
					Ok(x) => x,
				};

			log::info!(
				target: LOG_TARGET,
				"created a new collation on relay-parent({}): {:?}",
				relay_parent,
				head_data,
			);

			// The pov is the actually the initial state and the transactions.
			let pov = PoV { block_data: block_data.encode().into() };

			let collation = Collation {
				upward_messages,
				horizontal_messages: Default::default(),
				new_validation_code: None,
				head_data: head_data.encode().into(),
				proof_of_validity: MaybeCompressedPoV::Raw(pov.clone()),
				processed_downward_messages: 0,
				hrmp_watermark: validation_data.relay_parent_number,
			};

			log::info!(
				target: LOG_TARGET,
				"Raw PoV size for collation: {} bytes",
				pov.block_data.0.len(),
			);
			let compressed_pov = maybe_compress_pov(pov);

			log::info!(
				target: LOG_TARGET,
				"Compressed PoV size for collation: {} bytes",
				compressed_pov.block_data.0.len(),
			);

			async move { Some(CollationResult { collation }) }.boxed()
		})
	}

	/// Wait until `blocks` are built and enacted.
	pub async fn wait_for_blocks(&self, blocks: u64) {
		let start_block = self.state.lock().unwrap().best_block;
		loop {
			Delay::new(Duration::from_secs(1)).await;

			let current_block = self.state.lock().unwrap().best_block;

			if start_block + blocks <= current_block {
				return;
			}
		}
	}

	pub fn send_same_collations_to_all_assigned_cores(
		&self,
		full_node: &NewFull,
		mut overseer_handle: Handle,
		para_id: ParaId,
	) {
		let client = full_node.client.clone();

		let collation_function =
			self.create_collation_function(full_node.task_manager.spawn_handle());

		full_node
			.task_manager
			.spawn_handle()
			.spawn("malus-undying-collator", None, async move {
				// Subscribe to relay chain block import notifications. In each iteration, build a
				// collation in response to a block import notification and submits it to all cores
				// assigned to the parachain.
				let mut import_notifications = client.import_notification_stream();

				while let Some(notification) = import_notifications.next().await {
					let relay_parent = notification.hash;

					// Get the list of cores assigned to the parachain.
					let claim_queue = match client.runtime_api().claim_queue(relay_parent) {
						Ok(claim_queue) => claim_queue,
						Err(error) => {
							log::error!(
								target: LOG_TARGET,
								"Failed to query claim queue runtime API: {error:?}",
							);
							continue;
						},
					};

					let claim_queue_offset = ClaimQueueOffset(DEFAULT_CLAIM_QUEUE_OFFSET);

					let scheduled_cores: Vec<CoreIndex> = claim_queue
						.iter()
						.filter_map(move |(core_index, paras)| {
							paras.get(claim_queue_offset.0 as usize).and_then(|core_para_id| {
								(core_para_id == &para_id).then_some(*core_index)
							})
						})
						.collect();

					if scheduled_cores.is_empty() {
						log::info!(
							target: LOG_TARGET,
							"Scheduled cores is empty.",
						);
						continue;
					}

					if scheduled_cores.len() == 1 {
						log::info!(
							target: LOG_TARGET,
							"Malus collator configured with duplicate collations, but only 1 core assigned. \
							Collator will not do anything malicious.",
						);
					}

					// Fetch validation data for the collation.
					let validation_data = match client.runtime_api().persisted_validation_data(
						relay_parent,
						para_id,
						OccupiedCoreAssumption::Included,
					) {
						Ok(Some(validation_data)) => validation_data,
						Ok(None) => {
							log::info!(
								target: LOG_TARGET,
								"Persisted validation data is None.",
							);
							continue;
						},
						Err(error) => {
							log::error!(
								target: LOG_TARGET,
								"Failed to query persisted validation data runtime API: {error:?}",
							);
							continue;
						},
					};

					// Generate the collation.
					let collation =
						match collation_function(relay_parent, &validation_data).await {
							Some(collation) => collation,
							None => {
								log::info!(
									target: LOG_TARGET,
									"Collation result is None.",
								);
								continue;
							},
						}
						.collation;

					// Fetch the validation code hash.
					let validation_code_hash = match client.runtime_api().validation_code_hash(
						relay_parent,
						para_id,
						OccupiedCoreAssumption::Included,
					) {
						Ok(Some(validation_code_hash)) => validation_code_hash,
						Ok(None) => {
							log::info!(
								target: LOG_TARGET,
								"Validation code hash is None.",
							);
							continue;
						},
						Err(error) => {
							log::error!(
								target: LOG_TARGET,
								"Failed to query validation code hash runtime API: {error:?}",
							);
							continue;
						},
					};

					// Fetch the session index.
					let session_index =
						match client.runtime_api().session_index_for_child(relay_parent) {
							Ok(session_index) => session_index,
							Err(error) => {
								log::error!(
									target: LOG_TARGET,
									"Failed to query session index for child runtime API: {error:?}",
								);
								continue;
							},
						};

					let persisted_validation_data_hash = validation_data.hash();
					let parent_head_data = validation_data.parent_head.clone();

					// Apply compression to the block data.
					let pov = {
						let pov = collation.proof_of_validity.into_compressed();
						let encoded_size = pov.encoded_size();
						let max_pov_size = validation_data.max_pov_size as usize;

						// As long as `POV_BOMB_LIMIT` is at least `max_pov_size`, this ensures
						// that honest collators never produce a PoV which is uncompressed.
						//
						// As such, honest collators never produce an uncompressed PoV which starts
						// with a compression magic number, which would lead validators to
						// reject the collation.
						if encoded_size > max_pov_size {
							log::error!(
								target: LOG_TARGET,
								"PoV size {encoded_size} exceeded maximum size of {max_pov_size}",
							);
							continue;
						}

						pov
					};

					let pov_hash = pov.hash();

					// Fetch the session info.
					let session_info =
						match client.runtime_api().session_info(relay_parent, session_index) {
							Ok(Some(session_info)) => session_info,
							Ok(None) => {
								log::info!(
									target: LOG_TARGET,
									"Session info is None.",
								);
								continue;
							},
							Err(error) => {
								log::error!(
									target: LOG_TARGET,
									"Failed to query session info runtime API: {error:?}",
								);
								continue;
							},
						};

					let n_validators = session_info.validators.len();

					let available_data =
						AvailableData { validation_data, pov: Arc::new(pov.clone()) };
					let chunks = match polkadot_erasure_coding::obtain_chunks_v1(
						n_validators,
						&available_data,
					) {
						Ok(chunks) => chunks,
						Err(error) => {
							log::error!(
								target: LOG_TARGET,
								"Failed to obtain chunks v1: {error:?}",
							);
							continue;
						},
					};
					let erasure_root = polkadot_erasure_coding::branches(&chunks).root();

					let commitments = CandidateCommitments {
						upward_messages: collation.upward_messages,
						horizontal_messages: collation.horizontal_messages,
						new_validation_code: collation.new_validation_code,
						head_data: collation.head_data,
						processed_downward_messages: collation.processed_downward_messages,
						hrmp_watermark: collation.hrmp_watermark,
					};

					// Submit the same collation to all assigned cores.
					for core_index in &scheduled_cores {
						let candidate_receipt = CandidateReceiptV2 {
							descriptor: CandidateDescriptorV2::new(
								para_id,
								relay_parent,
								*core_index,
								session_index,
								persisted_validation_data_hash,
								pov_hash,
								erasure_root,
								commitments.head_data.hash(),
								validation_code_hash,
							),
							commitments_hash: commitments.hash(),
						};

						// We cannot use SubmitCollation here because it includes an additional
						// check for the core index by calling `parse_ump_signals`. This check
						// enforces that the parachain always selects the correct core by comparing
						// the descriptor and commitments core indexes. To bypass this check, we are
						// simulating the behavior of SubmitCollation while skipping ump signals
						// validation.
						overseer_handle
							.send_msg(
								CollatorProtocolMessage::DistributeSegment {
									core_index: *core_index,
									para_id,
									segment: Segment::V2(SegmentEntry {
										relay_parent,
										session_index,
										validation_code_hash,
										persisted_validation_data_hash,
										erasure_root,
										commitments_hash: candidate_receipt.commitments_hash,
										output_head_data_hash: candidate_receipt
											.descriptor()
											.para_head(),
										pov: pov.clone(),
										parent_head_data: parent_head_data.clone(),
									}),
								},
								"Collator",
							)
							.await;
					}
				}
			});
	}
}

use sp_core::traits::SpawnNamed;

#[cfg(test)]
mod tests {
	use super::*;
	use futures::executor::block_on;
	use polkadot_parachain_primitives::primitives::{ValidationParams, ValidationResult};
	use polkadot_primitives::{Hash, PersistedValidationData};

	#[test]
	fn collator_works() {
		let spawner = sp_core::testing::TaskExecutor::new();
		let collator = Collator::new(1_000, 1, false);
		let collation_function = collator.create_collation_function(spawner);

		for i in 0..5 {
			let parent_head =
				collator.state.lock().unwrap().number_to_head.get(&i).unwrap().clone();

			let validation_data = PersistedValidationData {
				parent_head: parent_head.encode().into(),
				..Default::default()
			};

			let collation =
				block_on(collation_function(Default::default(), &validation_data)).unwrap();
			validate_collation(&collator, (*parent_head).clone(), collation.collation);
		}
	}

	fn validate_collation(collator: &Collator, parent_head: HeadData, collation: Collation) {
		use polkadot_node_core_pvf::testing::validate_candidate;

		let block_data = match collation.proof_of_validity {
			MaybeCompressedPoV::Raw(pov) => pov.block_data,
			MaybeCompressedPoV::Compressed(_) => panic!("Only works with uncompressed povs"),
		};

		let ret_buf = validate_candidate(
			collator.validation_code(),
			&ValidationParams {
				parent_head: parent_head.encode().into(),
				block_data,
				relay_parent_number: 1,
				relay_parent_storage_root: Hash::zero(),
			}
			.encode(),
		)
		.unwrap();
		let ret = ValidationResult::decode(&mut &ret_buf[..]).unwrap();

		let new_head = HeadData::decode(&mut &ret.head_data.0[..]).unwrap();
		assert_eq!(
			**collator
				.state
				.lock()
				.unwrap()
				.number_to_head
				.get(&(parent_head.number + 1))
				.unwrap(),
			new_head
		);
	}

	#[test]
	fn advance_to_state_when_parent_head_is_missing() {
		let collator = Collator::new(1_000, 1, false);
		let graveyard_size = collator.state.lock().unwrap().graveyard_size;

		let mut head = calculate_head_and_state_for_number(10, graveyard_size, 1, false).unwrap().0;

		for i in 1..10 {
			head = collator.state.lock().unwrap().advance(head).unwrap().1;
			assert_eq!(10 + i, head.number);
		}

		let collator = Collator::new(1_000, 1, false);
		let mut second_head = collator
			.state
			.lock()
			.unwrap()
			.number_to_head
			.get(&0)
			.cloned()
			.unwrap()
			.as_ref()
			.clone();

		for _ in 1..20 {
			second_head = collator.state.lock().unwrap().advance(second_head.clone()).unwrap().1;
		}

		assert_eq!(second_head, head);
	}
}
