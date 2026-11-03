// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! The actual implementation of the validate block functionality.

use super::{scheduling, trie_cache, trie_recorder, MemoryOptimizedValidationParams};
use alloc::vec::Vec;
use codec::Encode;
use cumulus_primitives_core::{
	relay_chain::{
		BlockNumber as RNumber, Hash as RHash, Header as RelayChainHeader, MAX_HEAD_DATA_SIZE,
		UMP_SEPARATOR,
	},
	CumulusDigestItem, ParachainBlockData, PersistedValidationData, SignedSchedulingInfo,
	VerifySchedulingSignature,
};
use frame_support::{
	traits::{ExecuteBlock, Get, IsSubType},
	BoundedVec,
};
use polkadot_parachain_primitives::primitives::{HeadData, ValidationResult};
use sp_core::storage::{well_known_keys, ChildInfo, StateVersion};
use sp_externalities::{set_and_run_with_externalities, Externalities};
use sp_io::{hashing::blake2_128, KillStorageResult};
use sp_runtime::traits::{
	Block as BlockT, ExtrinsicCall, Hash as HashT, HashingFor, Header as HeaderT, LazyBlock,
};
use sp_state_machine::OverlayedChanges;
use sp_trie::{HashDBT, ProofSizeProvider, EMPTY_PREFIX};
use trie_recorder::{SeenNodes, SizeOnlyRecorderProvider};

type Ext<'a, Block, Backend> = sp_state_machine::Ext<'a, HashingFor<Block>, Backend>;

fn with_externalities<F: FnOnce(&mut dyn Externalities) -> R, R>(f: F) -> R {
	sp_externalities::with_externalities(f).expect("Environmental externalities not set.")
}

// Recorder instance to be used during this validate_block call.
environmental::environmental!(recorder: trait ProofSizeProvider);

/// Validate the given parachain block.
///
/// This function is doing roughly the following:
///
/// 1. We decode the [`ParachainBlockData`] from the `block_data` in `params`.
///
/// 2. We are doing some security checks like checking that the `parent_head` in `params`
/// is the parent of the block we are going to check. We also ensure that the `set_validation_data`
/// inherent is present in the block and that the validation data matches the values in `params`.
///
/// 3. We construct the sparse in-memory database from the storage proof inside the block data and
/// then ensure that the storage root matches the storage root in the `parent_head`.
///
/// 4. We replace all the storage related host functions with functions inside the wasm blob.
/// This means instead of calling into the host, we will stay inside the wasm execution. This is
/// very important as the relay chain validator hasn't the state required to verify the block. But
/// we have the in-memory database that contains all the values from the state of the parachain
/// that we require to verify the block.
///
/// 5. The last step is to execute the entire block in the machinery we just have setup. Executing
/// the blocks include running all transactions in the block against our in-memory database and
/// ensuring that the final storage root matches the storage root in the header of the block. In the
/// end we return back the [`ValidationResult`] with all the required information for the validator.
#[doc(hidden)]
pub fn validate_block<B: BlockT, E: ExecuteBlock<B>, PSC: crate::Config>(
	MemoryOptimizedValidationParams {
		block_data,
		parent_head: parachain_head,
		relay_parent_number,
		relay_parent_storage_root,
		extension,
	}: MemoryOptimizedValidationParams,
) -> ValidationResult
where
	B::Extrinsic: ExtrinsicCall,
	<B::Extrinsic as ExtrinsicCall>::Call: IsSubType<crate::Call<PSC>>,
{
	// Decode block data first - we need it for both scheduling validation and block execution
	let block_data = codec::decode_from_bytes::<ParachainBlockData<B::LazyBlock>>(block_data)
		.expect("Invalid parachain block data");

	let _guard = (
		// Replace storage calls with our own implementations
		sp_io::storage::host_read.replace_implementation(host_storage_read),
		sp_io::storage::host_set.replace_implementation(host_storage_set),
		sp_io::storage::host_get.replace_implementation(host_storage_get),
		sp_io::storage::host_exists.replace_implementation(host_storage_exists),
		sp_io::storage::host_clear.replace_implementation(host_storage_clear),
		sp_io::storage::host_root.replace_implementation(host_storage_root),
		sp_io::storage::host_clear_prefix.replace_implementation(host_storage_clear_prefix),
		sp_io::storage::host_append.replace_implementation(host_storage_append),
		sp_io::storage::host_next_key.replace_implementation(host_storage_next_key),
		sp_io::storage::host_start_transaction
			.replace_implementation(host_storage_start_transaction),
		sp_io::storage::host_rollback_transaction
			.replace_implementation(host_storage_rollback_transaction),
		sp_io::storage::host_commit_transaction
			.replace_implementation(host_storage_commit_transaction),
		sp_io::default_child_storage::host_get
			.replace_implementation(host_default_child_storage_get),
		sp_io::default_child_storage::host_read
			.replace_implementation(host_default_child_storage_read),
		sp_io::default_child_storage::host_set
			.replace_implementation(host_default_child_storage_set),
		sp_io::default_child_storage::host_clear
			.replace_implementation(host_default_child_storage_clear),
		sp_io::default_child_storage::host_storage_kill
			.replace_implementation(host_default_child_storage_storage_kill),
		sp_io::default_child_storage::host_exists
			.replace_implementation(host_default_child_storage_exists),
		sp_io::default_child_storage::host_clear_prefix
			.replace_implementation(host_default_child_storage_clear_prefix),
		sp_io::default_child_storage::host_root
			.replace_implementation(host_default_child_storage_root),
		sp_io::default_child_storage::host_next_key
			.replace_implementation(host_default_child_storage_next_key),
		sp_io::offchain_index::host_set.replace_implementation(host_offchain_index_set),
		sp_io::offchain_index::host_clear.replace_implementation(host_offchain_index_clear),
		cumulus_primitives_proof_size_hostfunction::storage_proof_size::host_storage_proof_size
			.replace_implementation(host_storage_proof_size),
		#[cfg(feature = "transaction-index")]
		sp_io::transaction_index::host_index.replace_implementation(host_transaction_index_index),
		#[cfg(feature = "transaction-index")]
		sp_io::transaction_index::host_renew.replace_implementation(host_transaction_index_renew),
	);

	// V3 scheduling validation (chain-shape only). Signature verification of
	// `signed_scheduling_info` happens here at the call site so the verifier wiring
	// stays out of the pure shape check.
	let validated_scheduling = scheduling::validate_v3_scheduling(
		PSC::SchedulingSignatureVerifier::V3_SCHEDULING_ENABLED,
		&extension.0,
		block_data.scheduling_proof(),
		PSC::RelayParentOffset::get(),
		crate::Pallet::<PSC>::max_claim_queue_offset(),
	);

	// The override inputs (signed payload + the ISP header), present whenever the proof carried a
	// `signed_scheduling_info`. The signature is verified later, inside the externalities scope
	// below, since it needs to read `Authorities::<T>` from parachain state.
	let scheduling_override_inputs: Option<(SignedSchedulingInfo, RelayChainHeader)> =
		validated_scheduling.and_then(|validated| {
			validated
				.signed_scheduling_info
				.map(|signed_info| (signed_info, validated.internal_scheduling_parent_header))
		});

	// Initialize hashmaps randomness.
	sp_trie::add_extra_randomness(build_seed_from_head_data::<B>(
		&block_data,
		relay_parent_storage_root,
	));

	let mut parent_header =
		codec::decode_from_bytes::<B::Header>(parachain_head.clone()).expect("Invalid parent head");

	let (blocks, proof) = block_data.into_inner();

	verify_blocks_form_chain::<B>(&blocks, &parent_header);

	let mut processed_downward_messages = 0;
	let mut upward_messages = BoundedVec::default();
	let mut upward_message_signals = Vec::<Vec<_>>::new();
	let mut horizontal_messages = BoundedVec::default();
	let mut hrmp_watermark = Default::default();
	let mut head_data = None;
	let mut new_validation_code = None;
	let num_blocks = blocks.len();

	// Create the db
	let mut db = match proof.to_memory_db(parent_header.state_root()) {
		Ok((db, _)) => db,
		Err(_) => panic!("Compact proof decoding failure."),
	};

	core::mem::drop(proof);

	let cache_provider = trie_cache::CacheProvider::new();
	let seen_nodes = SeenNodes::<HashingFor<B>>::default();

	// Verify the V3 scheduling signature override. Only set up the backend and externalities
	// when there's actually an override to check.
	if let Some((signed_info, isp_header)) = scheduling_override_inputs.as_ref() {
		let relay_slot = scheduling::relay_slot_from_header(isp_header).expect(
			"internal_scheduling_parent header must carry a BABE pre-digest; \
			 the relay chain runs BABE; qed",
		);

		let parent_backend: sp_state_machine::TrieBackend<
			_,
			HashingFor<B>,
			_,
			SizeOnlyRecorderProvider<HashingFor<B>>,
		> = sp_state_machine::TrieBackendBuilder::new_with_cache(
			&db,
			*parent_header.state_root(),
			&cache_provider,
		)
		.build();
		run_with_externalities_and_recorder::<B, _, _>(
			&parent_backend,
			&mut Default::default(),
			&mut Default::default(),
			|| {
				if !PSC::SchedulingSignatureVerifier::verify(signed_info, relay_slot) {
					panic!(
						"V3 scheduling validation failed: invalid \
						 signed_scheduling_info (ISP: {:?})",
						isp_header.hash(),
					);
				}
			},
		);
	}

	for (block_index, mut block) in blocks.into_iter().enumerate() {
		// We use the storage root of the `parent_head` to ensure that it is the correct root.
		// This is already being done above while creating the in-memory db, but let's be paranoid!!
		let backend = sp_state_machine::TrieBackendBuilder::new_with_cache(
			&db,
			*parent_header.state_root(),
			&cache_provider,
		)
		.build();

		// Each node only contributes once to the total size of the storage proof. So, we keep track
		// of them inside `seen_nodes` to always return the correct proof size.
		let mut execute_recorder = SizeOnlyRecorderProvider::with_seen_nodes(seen_nodes.clone());
		// `backend` with the `execute_recorder`. As the `execute_recorder`, this should only be
		// used for `execute_block`.
		let execute_backend = sp_state_machine::TrieBackendBuilder::wrap(&backend)
			.with_recorder(execute_recorder.clone())
			.build();

		let mut overlay = OverlayedChanges::default();

		parent_header = block.header().clone();

		run_with_externalities_and_recorder::<B, _, _>(
			&backend,
			&mut Default::default(),
			&mut Default::default(),
			|| {
				E::verify_and_remove_seal(&mut block);
			},
		);

		run_with_externalities_and_recorder::<B, _, _>(
			&execute_backend,
			// Here is the only place where we want to use the recorder.
			// We want to ensure that we not accidentally read something from the proof, that
			// was not yet read and thus, alter the proof size. Otherwise, we end up with
			// mismatches in later blocks.
			&mut execute_recorder,
			&mut overlay,
			|| {
				E::execute_verified_block(block);
			},
		);

		let code_upgrade_detected =
			if <PSC as frame_system::Config>::Version::get().system_version >= 3 {
				overlay.storage(well_known_keys::PENDING_CODE).is_some()
			} else {
				overlay.storage(well_known_keys::CODE).is_some()
			};
		if code_upgrade_detected && num_blocks > 1 {
			panic!(
				"When applying a runtime upgrade, only one block per PoV is allowed. Received {num_blocks}."
			)
		}
		run_with_externalities_and_recorder::<B, _, _>(
			&backend,
			&mut Default::default(),
			// We are only reading here, but need to know what the old block has written. Thus, we
			// are passing here the overlay.
			&mut overlay,
			|| {
				// Ensure the validation data is correct.
				validate_validation_data(
					crate::ValidationData::<PSC>::get()
						.expect("`ValidationData` must be set after executing a block; qed"),
					&parachain_head,
					relay_parent_number,
					relay_parent_storage_root,
				);

				new_validation_code =
					new_validation_code.take().or(crate::NewValidationCode::<PSC>::get());

				let mut found_separator = false;
				crate::UpwardMessages::<PSC>::get()
					.into_iter()
					.filter_map(|m| {
						// Filter out the `UMP_SEPARATOR` and the `UMPSignals`.
						if m == UMP_SEPARATOR {
							found_separator = true;
							None
						} else if found_separator {
							upward_message_signals.push(m);
							None
						} else {
							// No signal or separator
							Some(m)
						}
					})
					.for_each(|m| {
						upward_messages.try_push(m).expect(
							"Number of upward messages should not be greater than `MAX_UPWARD_MESSAGE_NUM`",
						)
					});

				processed_downward_messages += crate::ProcessedDownwardMessages::<PSC>::get();
				horizontal_messages
					.try_extend(crate::HrmpOutboundMessages::<PSC>::get().into_iter())
					.expect(
						"Number of horizontal messages should not be greater than `MAX_HORIZONTAL_MESSAGE_NUM`",
					);
				hrmp_watermark = crate::HrmpWatermark::<PSC>::get();

				if block_index + 1 == num_blocks {
					head_data = Some(
						crate::CustomValidationHeadData::<PSC>::get()
							.map_or_else(|| HeadData(parent_header.encode()), HeadData),
					);
				}
			},
		);

		if block_index + 1 != num_blocks {
			let mut changes = overlay
				.drain_storage_changes(
					&backend,
					<PSC as frame_system::Config>::Version::get().state_version(),
				)
				.expect("Failed to get drain storage changes from the overlay.");

			drop(backend);

			// We just forward the changes directly to our db.
			changes.transaction.drain().into_iter().for_each(|(_, (value, count))| {
				// We only care about inserts and not deletes.
				if count > 0 {
					db.insert(EMPTY_PREFIX, &value);

					let hash = HashingFor::<B>::hash(&value);
					seen_nodes.borrow_mut().insert(hash);
				}
			});
		}
	}

	// A `signed_scheduling_info` overrides the block's emitted signals wholesale — they
	// are ignored, not merged.
	match scheduling_override_inputs.as_ref() {
		Some((signed_info, _)) => {
			scheduling::SchedulingSignals::from_scheduling_info(signed_info, &mut upward_messages)
		},
		None => scheduling::SchedulingSignals::from_block_signals(
			&upward_message_signals,
			&mut upward_messages,
		),
	}

	horizontal_messages.sort_by(|a, b| a.recipient.cmp(&b.recipient));

	ValidationResult {
		head_data: head_data.expect("HeadData not set"),
		new_validation_code: new_validation_code.map(Into::into),
		upward_messages,
		processed_downward_messages,
		horizontal_messages,
		hrmp_watermark,
	}
}

/// Validates the given [`PersistedValidationData`] against the data from the relay chain.
fn validate_validation_data(
	validation_data: PersistedValidationData,
	parent_header: &[u8],
	relay_parent_number: RNumber,
	relay_parent_storage_root: RHash,
) {
	assert_eq!(parent_header, &validation_data.parent_head.0, "Parent head doesn't match");
	assert_eq!(
		relay_parent_number, validation_data.relay_parent_number,
		"Relay parent number doesn't match",
	);
	assert_eq!(
		relay_parent_storage_root, validation_data.relay_parent_storage_root,
		"Relay parent storage root doesn't match",
	);
}

fn verify_blocks_form_chain<B: BlockT>(blocks: &[B::LazyBlock], parent_header: &B::Header) {
	let num_blocks = blocks.len();

	// Check first block's parent matches the given parent_header
	assert_eq!(
		*blocks
			.first()
			.expect("BlockData should have at least one block")
			.header()
			.parent_hash(),
		parent_header.hash(),
		"Parachain head needs to be the parent of the first block"
	);

	let mut first_block_has_bundle_info: Option<bool> = None;

	blocks.iter().enumerate().fold(
		parent_header.hash(),
		|expected_parent, (block_index, block)| {
			// Check chain validity
			assert_eq!(
				expected_parent,
				*block.header().parent_hash(),
				"Not a valid chain of blocks :(; {:?} not a parent of {:?}?",
				array_bytes::bytes2hex("0x", expected_parent.as_ref()),
				array_bytes::bytes2hex("0x", block.header().parent_hash().as_ref()),
			);

			let encoded_header_size = block.header().encoded_size();
			assert!(
				encoded_header_size <= MAX_HEAD_DATA_SIZE as usize,
				"Header size {encoded_header_size} exceeds MAX_HEAD_DATA_SIZE {MAX_HEAD_DATA_SIZE}",
			);

			// Validate BlockBundleInfo consistency
			let bundle_info = CumulusDigestItem::find_block_bundle_info(block.header().digest());
			match (first_block_has_bundle_info, &bundle_info) {
				(None, info) => {
					first_block_has_bundle_info = Some(info.is_some());
				},
				(Some(true), None) => {
					panic!("All blocks in a bundled PoV must include `BlockBundleInfo`");
				},
				(Some(false), _) => {
					panic!("A PoV without `BlockBundleInfo` may only contain a single block");
				},
				_ => {},
			}

			if let Some(ref info) = bundle_info {
				assert_eq!(
					info.index as usize, block_index,
					"BlockBundleInfo index mismatch: expected {block_index}, got {}",
					info.index
				);

				if block_index + 1 < num_blocks {
					assert!(
						!CumulusDigestItem::is_last_block_in_core(block.header().digest()).unwrap_or(false),
						"Intermediate block at index {block_index} is marked as last block in core, \
						but more blocks follow in the PoV",
					);
				} else if !CumulusDigestItem::is_last_block_in_core(block.header().digest())
					.unwrap_or(true)
				{
					panic!(
						"Last block in PoV must include the digest that marks it as the last block in the core"
					);
				}
			}

			block.header().hash()
		},
	);
}

/// Build a seed from the head data of the parachain block.
///
/// Uses both the relay parent storage root and the hash of the blocks
/// in the block data, to make sure the seed changes every block and that
/// the user cannot find about it ahead of time.
fn build_seed_from_head_data<B: BlockT>(
	block_data: &ParachainBlockData<B::LazyBlock>,
	relay_parent_storage_root: crate::relay_chain::Hash,
) -> [u8; 16] {
	let mut bytes_to_hash = Vec::with_capacity(
		block_data.blocks().len() * size_of::<B::Hash>() + size_of::<crate::relay_chain::Hash>(),
	);

	bytes_to_hash.extend_from_slice(relay_parent_storage_root.as_ref());
	block_data.blocks().iter().for_each(|block| {
		bytes_to_hash.extend_from_slice(block.header().hash().as_ref());
	});

	blake2_128(&bytes_to_hash)
}

/// Run the given closure with the externalities and recorder set.
fn run_with_externalities_and_recorder<Block: BlockT, R, F: FnOnce() -> R>(
	backend: &impl sp_state_machine::Backend<HashingFor<Block>>,
	recorder: &mut SizeOnlyRecorderProvider<HashingFor<Block>>,
	overlay: &mut OverlayedChanges<HashingFor<Block>>,
	execute: F,
) -> R {
	let mut ext = Ext::<Block, _>::new(overlay, backend);

	recorder::using(recorder, || set_and_run_with_externalities(&mut ext, || execute()))
}

fn host_storage_read(key: &[u8], value_out: &mut [u8], value_offset: u32) -> Option<u32> {
	match with_externalities(|ext| ext.storage(key)) {
		Some(value) => {
			let value_offset = value_offset as usize;
			let data = &value[value_offset.min(value.len())..];
			let written = core::cmp::min(data.len(), value_out.len());
			value_out[..written].copy_from_slice(&data[..written]);
			Some(value.len() as u32)
		},
		None => None,
	}
}

fn host_storage_set(key: &[u8], value: &[u8]) {
	with_externalities(|ext| ext.place_storage(key.to_vec(), Some(value.to_vec())))
}

fn host_storage_get(key: &[u8]) -> Option<bytes::Bytes> {
	with_externalities(|ext| ext.storage(key).map(|value| value.into()))
}

fn host_storage_exists(key: &[u8]) -> bool {
	with_externalities(|ext| ext.exists_storage(key))
}

fn host_storage_clear(key: &[u8]) {
	with_externalities(|ext| ext.place_storage(key.to_vec(), None))
}

fn host_storage_proof_size() -> u64 {
	recorder::with(|rec| rec.estimate_encoded_size()).expect("Recorder is always set; qed") as _
}

fn host_storage_root(version: StateVersion) -> Vec<u8> {
	with_externalities(|ext| ext.storage_root(version))
}

fn host_storage_clear_prefix(prefix: &[u8], limit: Option<u32>) -> KillStorageResult {
	with_externalities(|ext| ext.clear_prefix(prefix, limit, None).into())
}

fn host_storage_append(key: &[u8], value: Vec<u8>) {
	with_externalities(|ext| ext.storage_append(key.to_vec(), value))
}

fn host_storage_next_key(key: &[u8]) -> Option<Vec<u8>> {
	with_externalities(|ext| ext.next_storage_key(key))
}

fn host_storage_start_transaction() {
	with_externalities(|ext| ext.storage_start_transaction())
}

fn host_storage_rollback_transaction() {
	with_externalities(|ext| ext.storage_rollback_transaction().ok())
		.expect("No open transaction that can be rolled back.");
}

fn host_storage_commit_transaction() {
	with_externalities(|ext| ext.storage_commit_transaction().ok())
		.expect("No open transaction that can be committed.");
}

fn host_default_child_storage_get(storage_key: &[u8], key: &[u8]) -> Option<Vec<u8>> {
	let child_info = ChildInfo::new_default(storage_key);
	with_externalities(|ext| ext.child_storage(&child_info, key))
}

fn host_default_child_storage_read(
	storage_key: &[u8],
	key: &[u8],
	value_out: &mut [u8],
	value_offset: u32,
) -> Option<u32> {
	let child_info = ChildInfo::new_default(storage_key);
	match with_externalities(|ext| ext.child_storage(&child_info, key)) {
		Some(value) => {
			let value_offset = value_offset as usize;
			let data = &value[value_offset.min(value.len())..];
			let written = core::cmp::min(data.len(), value_out.len());
			value_out[..written].copy_from_slice(&data[..written]);
			Some(value.len() as u32)
		},
		None => None,
	}
}

fn host_default_child_storage_set(storage_key: &[u8], key: &[u8], value: &[u8]) {
	let child_info = ChildInfo::new_default(storage_key);
	with_externalities(|ext| {
		ext.place_child_storage(&child_info, key.to_vec(), Some(value.to_vec()))
	})
}

fn host_default_child_storage_clear(storage_key: &[u8], key: &[u8]) {
	let child_info = ChildInfo::new_default(storage_key);
	with_externalities(|ext| ext.place_child_storage(&child_info, key.to_vec(), None))
}

fn host_default_child_storage_storage_kill(
	storage_key: &[u8],
	limit: Option<u32>,
) -> KillStorageResult {
	let child_info = ChildInfo::new_default(storage_key);
	with_externalities(|ext| ext.kill_child_storage(&child_info, limit, None).into())
}

fn host_default_child_storage_exists(storage_key: &[u8], key: &[u8]) -> bool {
	let child_info = ChildInfo::new_default(storage_key);
	with_externalities(|ext| ext.exists_child_storage(&child_info, key))
}

fn host_default_child_storage_clear_prefix(
	storage_key: &[u8],
	prefix: &[u8],
	limit: Option<u32>,
) -> KillStorageResult {
	let child_info = ChildInfo::new_default(storage_key);
	with_externalities(|ext| ext.clear_child_prefix(&child_info, prefix, limit, None).into())
}

fn host_default_child_storage_root(storage_key: &[u8], version: StateVersion) -> Vec<u8> {
	let child_info = ChildInfo::new_default(storage_key);
	with_externalities(|ext| ext.child_storage_root(&child_info, version))
}

fn host_default_child_storage_next_key(storage_key: &[u8], key: &[u8]) -> Option<Vec<u8>> {
	let child_info = ChildInfo::new_default(storage_key);
	with_externalities(|ext| ext.next_child_storage_key(&child_info, key))
}

fn host_offchain_index_set(_key: &[u8], _value: &[u8]) {}

fn host_offchain_index_clear(_key: &[u8]) {}

/// Parachain validation does not require maintaining a transaction index,
/// and indexing transactions does **not** contribute to the parachain state.
/// However, the host environment still expects this function to exist,
/// so we provide a no-op implementation.
#[cfg(feature = "transaction-index")]
fn host_transaction_index_index(_extrinsic: u32, _size: u32, _context_hash: [u8; 32]) {
	// No-op host function used during parachain validation.
}

/// Parachain validation does not require maintaining a transaction index,
/// and indexing transactions does **not** contribute to the parachain state.
/// However, the host environment still expects this function to exist,
/// so we provide a no-op implementation.
#[cfg(feature = "transaction-index")]
fn host_transaction_index_renew(_extrinsic: u32, _context_hash: [u8; 32]) {
	// No-op host function used during parachain validation.
}
