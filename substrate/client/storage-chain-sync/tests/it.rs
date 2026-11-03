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

//! Integration tests for [`sc_storage_chain_sync::StorageChainBlockImport`].
//!
//! These tests drive the wrapper through its public [`BlockImport::import_block`] surface
//! against a hand-rolled mock client/runtime API and a recording inner `BlockImport` that
//! captures the `BlockImportParams` it receives.
//!
//! Scope: the attached-changes path (incoming `StorageChanges`), gap-sync, the
//! `should_intercept` short-circuits, and the block-execution path (via a dedicated
//! harness with an executable overlay-supporting runtime API mock).

use mock::{
	attached_changes_params, gap_sync_params, make_harness, params_with_origin,
	prefetched_attached, renew_op,
};
use rstest::rstest;
use sc_consensus::{BlockImport, ImportResult, StateAction};
use sp_consensus::BlockOrigin;
use sp_core::H256;
use sp_runtime::OpaqueExtrinsic;
use sp_state_machine::IndexOperation;
use sp_transaction_storage_proof::{ContentHash, HashingAlgorithm, IndexedTransactionInfo};

#[rstest]
#[case::warp_sync(BlockOrigin::WarpSync, None)]
#[case::gap_sync_without_body(BlockOrigin::GapSync, None)]
#[case::body_none(BlockOrigin::NetworkBroadcast, None)]
#[tokio::test]
async fn import_passes_through(
	#[case] origin: BlockOrigin,
	#[case] body: Option<Vec<OpaqueExtrinsic>>,
) {
	let h = make_harness();
	let params = params_with_origin(origin, 1, body);
	let result = h.wrapper.import_block(params).await.expect("import_block");
	assert!(matches!(result, ImportResult::Imported(_)));
	let captured = h.captured.lock().unwrap();
	assert_eq!(captured.len(), 1);
	assert!(!prefetched_attached(&captured[0]));
}

fn info(
	content_hash: ContentHash,
	size: u32,
	hashing: HashingAlgorithm,
	extrinsic_index: u32,
) -> IndexedTransactionInfo {
	IndexedTransactionInfo {
		content_hash,
		size,
		hashing,
		cid_codec: sc_network_bitswap::RAW_CODEC,
		extrinsic_index,
	}
}

#[tokio::test]
async fn import_attached_changes_no_renews_attaches_nothing() {
	let h = make_harness();
	let params = attached_changes_params(1, Vec::new());
	let result = h.wrapper.import_block(params).await.expect("import_block");
	assert!(matches!(result, ImportResult::Imported(_)));
	let captured = h.captured.lock().unwrap();
	assert_eq!(captured.len(), 1);
	assert!(!prefetched_attached(&captured[0]));
}

#[rstest]
#[case::blake2b(
	b"renew-blob-payload".as_slice(),
	sp_transaction_storage_proof::HashingAlgorithm::Blake2b256,
)]
#[case::sha2(
	b"sha2-renew-blob-payload".as_slice(),
	sp_transaction_storage_proof::HashingAlgorithm::Sha2_256,
)]
#[case::keccak(
	b"keccak-renew-blob-payload".as_slice(),
	sp_transaction_storage_proof::HashingAlgorithm::Keccak256,
)]
#[tokio::test]
async fn import_attached_changes_attaches_prefetched(
	#[case] bytes: &[u8],
	#[case] algorithm: sp_transaction_storage_proof::HashingAlgorithm,
) {
	let h = make_harness();
	let content_hash: ContentHash = algorithm.hash(bytes);
	h.api
		.set_indexed(1, vec![info(content_hash, bytes.len() as u32, algorithm, u32::MAX)]);
	h.network.insert(content_hash, bytes.to_vec());

	let params = attached_changes_params(1, vec![renew_op(content_hash, 0)]);
	let result = h.wrapper.import_block(params).await.expect("import_block");
	assert!(matches!(result, ImportResult::Imported(_)));

	let captured = h.captured.lock().unwrap();
	assert_eq!(captured.len(), 1);
	let prefetched = &captured[0].prefetched_indexed_transactions;
	assert!(prefetched.ops.is_empty(), "tip-block path must not synthesize ops");
	assert_eq!(prefetched.renew_payloads.len(), 1);
	let key = sp_core::H256::from(content_hash);
	assert_eq!(prefetched.renew_payloads.get(&key).map(|v| v.as_slice()), Some(bytes));
	assert!(h.api.overlay_marker_seen(), "overlay marker should have been observed by call_api_at",);
}

#[tokio::test]
async fn import_attached_changes_propagates_runtime_declared_codec_to_bitswap_cid() {
	const DAG_PB_CODEC: u64 = 0x70;

	let h = make_harness();
	let bytes = b"non-raw-codec-payload".to_vec();
	let algorithm = HashingAlgorithm::Blake2b256;
	let content_hash: ContentHash = algorithm.hash(&bytes);

	h.api.set_indexed(
		1,
		vec![IndexedTransactionInfo {
			content_hash,
			size: bytes.len() as u32,
			hashing: algorithm,
			cid_codec: DAG_PB_CODEC,
			extrinsic_index: u32::MAX,
		}],
	);
	h.network.insert(content_hash, bytes.clone());

	let params = attached_changes_params(1, vec![renew_op(content_hash, 0)]);
	let result = h.wrapper.import_block(params).await.expect("import_block");
	assert!(matches!(result, ImportResult::Imported(_)));

	let observed = h.network.observed_cids();
	assert!(
		observed.iter().any(|cid| cid.codec() == DAG_PB_CODEC &&
			cid.hash().digest() == content_hash),
		"bitswap request must carry the runtime-declared codec ({DAG_PB_CODEC:#x}); observed: {observed:?}",
	);
	assert!(
		observed.iter().all(|cid| cid.codec() != sc_network_bitswap::RAW_CODEC),
		"no request should fall back to hard-coded RAW_CODEC; observed: {observed:?}",
	);

	let captured = h.captured.lock().unwrap();
	assert_eq!(captured.len(), 1);
	let prefetched = &captured[0].prefetched_indexed_transactions;
	let key = sp_core::H256::from(content_hash);
	assert_eq!(
		prefetched.renew_payloads.get(&key).map(|v| v.as_slice()),
		Some(bytes.as_slice()),
		"non-RAW codec entries must still be successfully fetched and attached",
	);
}

#[tokio::test]
async fn import_attached_changes_skips_already_present_hash() {
	let h = make_harness();
	let bytes = b"already-on-disk".to_vec();
	let content_hash: ContentHash = HashingAlgorithm::Blake2b256.hash(&bytes);
	h.api.set_indexed(
		1,
		vec![info(content_hash, bytes.len() as u32, HashingAlgorithm::Blake2b256, u32::MAX)],
	);
	h.api.insert_indexed_transaction(content_hash, bytes);

	let params = attached_changes_params(1, vec![renew_op(content_hash, 0)]);
	let result = h.wrapper.import_block(params).await.expect("import_block");
	assert!(matches!(result, ImportResult::Imported(_)));

	let captured = h.captured.lock().unwrap();
	assert_eq!(captured.len(), 1);
	assert!(!prefetched_attached(&captured[0]));
}

#[tokio::test]
async fn import_attached_changes_errors_when_fetcher_partial() {
	let h = make_harness();
	let content_hash: ContentHash = [0x33u8; 32];
	h.api
		.set_indexed(1, vec![info(content_hash, 32, HashingAlgorithm::Blake2b256, u32::MAX)]);
	let params = attached_changes_params(1, vec![renew_op(content_hash, 0)]);
	let err = h
		.wrapper
		.import_block(params)
		.await
		.expect_err("fetcher should yield zero bytes and the wrapper should error");
	let msg = format!("{err}");
	assert!(msg.contains("could not be fetched via bitswap"), "unexpected error message: {msg}",);
	assert!(h.captured.lock().unwrap().is_empty());
}

#[tokio::test]
async fn import_block_execution_executes_once_and_indexes_on_same_overlay() {
	let bytes = b"case-b-renew-blob".to_vec();
	let h = mock::make_block_execution_harness(bytes.clone());
	let params = mock::block_execution_params(1);
	let result = h.wrapper.import_block(params).await.expect("import_block");
	assert!(matches!(result, ImportResult::Imported(_)));

	assert_eq!(h.api.execute_block_count(), 1);
	assert_eq!(h.api.indexed_transactions_count(), 1);
	assert!(h.api.overlay_marker_seen_by_indexed_transactions());

	let captured = h.captured.lock().unwrap();
	assert_eq!(captured.len(), 1);
	let changes = captured[0]
		.state_action
		.as_storage_changes()
		.expect("block-execution path forwards generated storage changes");
	assert_eq!(changes.transaction_index_changes.len(), 1);
	let sp_state_machine::IndexOperation::Renew { extrinsic, hash } =
		&changes.transaction_index_changes[0]
	else {
		panic!("expected a renew operation from the block-execution path");
	};
	assert_eq!((*extrinsic, hash.as_slice()), (0, h.content_hash.as_slice()));
	assert!(
		changes
			.main_storage_changes
			.iter()
			.any(|(key, value)| key == mock::CASE_B_MARKER_KEY &&
				value.as_deref() == Some(mock::CASE_B_MARKER_VALUE)),
		"execute_block overlay marker must be forwarded",
	);
	assert!(
		!changes
			.main_storage_changes
			.iter()
			.any(|(key, _)| key == mock::CASE_B_ROLLBACK_MARKER_KEY),
		"indexed_transactions rollback marker must not leak into forwarded changes",
	);

	let prefetched = &captured[0].prefetched_indexed_transactions;
	assert!(prefetched.ops.is_empty(), "tip-block path must not synthesize ops");
	let expected: std::collections::HashMap<sp_core::H256, Vec<u8>> =
		std::iter::once((sp_core::H256::from(h.content_hash), bytes)).collect();
	assert_eq!(prefetched.renew_payloads, expected);
}

// W13: gap-sync integration tests. Non-archive gap-sync blocks are header-only and pass through
// via the body gate; archive/body-carrying gap-sync blocks exercise the synthetic-ops path.

#[tokio::test]
async fn import_gap_sync_pure_renews_attaches_synthetic_renew_ops_and_payloads() {
	let h = make_harness();
	let finalized = H256::from([0xF1; 32]);
	h.api.set_finalized_hash(finalized);

	let bytes_a = b"renew-payload-a".to_vec();
	let bytes_b = b"renew-payload-b".to_vec();
	let hash_a = HashingAlgorithm::Blake2b256.hash(&bytes_a);
	let hash_b = HashingAlgorithm::Sha2_256.hash(&bytes_b);
	let body = vec![
		OpaqueExtrinsic::from_blob(b"renew-call-a".to_vec()),
		OpaqueExtrinsic::from_blob(b"renew-call-b".to_vec()),
	];

	h.api.set_indexed(
		1,
		vec![
			info(hash_a, bytes_a.len() as u32, HashingAlgorithm::Blake2b256, 0),
			info(hash_b, bytes_b.len() as u32, HashingAlgorithm::Sha2_256, 1),
		],
	);
	h.network.insert(hash_a, bytes_a.clone());
	h.network.insert(hash_b, bytes_b.clone());

	let params = gap_sync_params(1, Some(body));
	let result = h.wrapper.import_block(params).await.expect("gap-sync import");
	assert!(matches!(result, ImportResult::Imported(_)));

	let captured = h.captured.lock().unwrap();
	assert_eq!(captured.len(), 1);
	let prefetched = &captured[0].prefetched_indexed_transactions;
	assert_eq!(prefetched.ops.len(), 2, "two synthetic renew ops");
	for op in &prefetched.ops {
		assert!(matches!(op, IndexOperation::Renew { .. }));
	}
	assert_eq!(prefetched.renew_payloads.len(), 2);
}

#[tokio::test]
async fn import_gap_sync_pure_stores_attaches_synthetic_insert_ops_no_fetch() {
	let h = make_harness();
	h.api.set_finalized_hash(H256::from([0xF2; 32]));

	let body = vec![
		OpaqueExtrinsic::from_blob(b"store-call-a".to_vec()),
		OpaqueExtrinsic::from_blob(b"store-call-b".to_vec()),
	];
	let encoded_a = codec::Encode::encode(&body[0]);
	let encoded_b = codec::Encode::encode(&body[1]);
	let hash_a = HashingAlgorithm::Blake2b256.hash(&encoded_a);
	let hash_b = HashingAlgorithm::Keccak256.hash(&encoded_b);

	h.api.set_indexed(
		1,
		vec![
			info(hash_a, encoded_a.len() as u32, HashingAlgorithm::Blake2b256, 0),
			info(hash_b, encoded_b.len() as u32, HashingAlgorithm::Keccak256, 1),
		],
	);

	let params = gap_sync_params(1, Some(body));
	let result = h.wrapper.import_block(params).await.expect("gap-sync import");
	assert!(matches!(result, ImportResult::Imported(_)));

	let captured = h.captured.lock().unwrap();
	assert_eq!(captured.len(), 1);
	let prefetched = &captured[0].prefetched_indexed_transactions;
	assert_eq!(prefetched.ops.len(), 2);
	for op in &prefetched.ops {
		assert!(matches!(op, IndexOperation::Insert { .. }));
	}
	assert!(prefetched.renew_payloads.is_empty(), "stores need no bitswap fetch");
	assert_eq!(h.network.call_count(), 0, "fetcher must not be invoked for pure stores");
}

#[tokio::test]
async fn import_gap_sync_mixed_body_attaches_both_with_correct_split() {
	let h = make_harness();
	h.api.set_finalized_hash(H256::from([0xF3; 32]));

	let store_ext = OpaqueExtrinsic::from_blob(b"store-call-mixed".to_vec());
	let renew_bytes = b"renew-payload-mixed".to_vec();
	let body = vec![store_ext.clone(), OpaqueExtrinsic::from_blob(b"renew-call-mixed".to_vec())];
	let encoded = codec::Encode::encode(&store_ext);
	let store_hash = HashingAlgorithm::Blake2b256.hash(&encoded);
	let renew_hash = HashingAlgorithm::Sha2_256.hash(&renew_bytes);

	h.api.set_indexed(
		1,
		vec![
			info(store_hash, encoded.len() as u32, HashingAlgorithm::Blake2b256, 0),
			info(renew_hash, renew_bytes.len() as u32, HashingAlgorithm::Sha2_256, 1),
		],
	);
	h.network.insert(renew_hash, renew_bytes.clone());

	let params = gap_sync_params(1, Some(body));
	let result = h.wrapper.import_block(params).await.expect("gap-sync import");
	assert!(matches!(result, ImportResult::Imported(_)));

	let captured = h.captured.lock().unwrap();
	let prefetched = &captured[0].prefetched_indexed_transactions;
	assert_eq!(prefetched.ops.len(), 2);
	assert!(matches!(prefetched.ops[0], IndexOperation::Insert { extrinsic: 0, .. }));
	assert!(matches!(prefetched.ops[1], IndexOperation::Renew { extrinsic: 1, .. }));
	let expected: std::collections::HashMap<sp_core::H256, Vec<u8>> =
		std::iter::once((sp_core::H256::from(renew_hash), renew_bytes)).collect();
	assert_eq!(prefetched.renew_payloads, expected);
}

#[tokio::test]
async fn import_gap_sync_state_action_remains_skip() {
	let h = make_harness();
	h.api.set_finalized_hash(H256::from([0xF4; 32]));

	let body = vec![OpaqueExtrinsic::from_blob(b"renew-call".to_vec())];
	let renew_bytes = b"renew-payload".to_vec();
	let renew_hash = HashingAlgorithm::Blake2b256.hash(&renew_bytes);
	h.api.set_indexed(
		1,
		vec![info(renew_hash, renew_bytes.len() as u32, HashingAlgorithm::Blake2b256, 0)],
	);
	h.network.insert(renew_hash, renew_bytes);

	let params = gap_sync_params(1, Some(body));
	let _ = h.wrapper.import_block(params).await.expect("gap-sync import");

	let captured = h.captured.lock().unwrap();
	assert!(
		matches!(captured[0].state_action, StateAction::Skip),
		"gap-sync state_action must stay Skip after the wrapper",
	);
}

#[tokio::test]
async fn import_gap_sync_below_retention_finalized_returns_empty_passes_through() {
	let h = make_harness();
	h.api.set_finalized_hash(H256::from([0xF5; 32]));
	// No `set_indexed` -> runtime API returns Vec::new() for block N.

	let body = vec![OpaqueExtrinsic::from_blob(b"old-extrinsic".to_vec())];
	let params = gap_sync_params(1, Some(body));
	let result = h.wrapper.import_block(params).await.expect("gap-sync import");
	assert!(matches!(result, ImportResult::Imported(_)));

	let captured = h.captured.lock().unwrap();
	let prefetched = &captured[0].prefetched_indexed_transactions;
	assert!(prefetched.ops.is_empty(), "out-of-retention -> no ops");
	assert!(prefetched.renew_payloads.is_empty(), "out-of-retention -> no payloads");
	assert_eq!(h.network.call_count(), 0, "fetcher must not be invoked when API returns empty");
}

#[tokio::test]
async fn import_gap_sync_uses_finalized_hash_not_parent_hash() {
	let h = make_harness();
	let finalized = H256::from([0x99; 32]);
	h.api.set_finalized_hash(finalized);

	let renew_bytes = b"renew-state-context-probe".to_vec();
	let renew_hash = HashingAlgorithm::Blake2b256.hash(&renew_bytes);
	let body = vec![OpaqueExtrinsic::from_blob(b"renew-call".to_vec())];

	h.api.set_indexed(
		1,
		vec![info(renew_hash, renew_bytes.len() as u32, HashingAlgorithm::Blake2b256, 0)],
	);
	h.network.insert(renew_hash, renew_bytes);

	let params = gap_sync_params(1, Some(body));
	let _ = h.wrapper.import_block(params).await.expect("gap-sync import");

	let observed_state = h
		.api
		.last_indexed_transactions_state()
		.expect("API must be invoked at least once");
	assert_eq!(
		observed_state, finalized,
		"gap-sync runtime API call must use finalized_hash, NOT parent_hash",
	);
	// Parent hash of a gap_sync_params block is zero; assert that it's NOT what we saw.
	assert_ne!(observed_state, H256::zero(), "parent_hash sanity guard: must differ from probe");
}

#[tokio::test]
async fn import_gap_sync_filters_already_present_hashes() {
	let h = make_harness();
	h.api.set_finalized_hash(H256::from([0xF7; 32]));

	let bytes_present = b"already-on-disk".to_vec();
	let bytes_to_fetch = b"needs-bitswap".to_vec();
	let hash_present = HashingAlgorithm::Blake2b256.hash(&bytes_present);
	let hash_to_fetch = HashingAlgorithm::Blake2b256.hash(&bytes_to_fetch);
	let body = vec![
		OpaqueExtrinsic::from_blob(b"renew-call-present".to_vec()),
		OpaqueExtrinsic::from_blob(b"renew-call-fetch".to_vec()),
	];

	h.api.insert_indexed_transaction(hash_present, bytes_present.clone());
	h.api.set_indexed(
		1,
		vec![
			info(hash_present, bytes_present.len() as u32, HashingAlgorithm::Blake2b256, 0),
			info(hash_to_fetch, bytes_to_fetch.len() as u32, HashingAlgorithm::Blake2b256, 1),
		],
	);
	h.network.insert(hash_to_fetch, bytes_to_fetch.clone());

	let params = gap_sync_params(1, Some(body));
	let _ = h.wrapper.import_block(params).await.expect("gap-sync import");

	let captured = h.captured.lock().unwrap();
	let prefetched = &captured[0].prefetched_indexed_transactions;
	assert_eq!(prefetched.ops.len(), 2, "both entries produce ops");
	let expected: std::collections::HashMap<sp_core::H256, Vec<u8>> =
		std::iter::once((sp_core::H256::from(hash_to_fetch), bytes_to_fetch)).collect();
	assert_eq!(prefetched.renew_payloads, expected, "only the missing hash is in renew_payloads",);
}

#[tokio::test]
async fn import_gap_sync_fetcher_partial_failure_propagates_error() {
	let h = make_harness();
	h.api.set_finalized_hash(H256::from([0xF8; 32]));

	let hash_unfetchable: ContentHash = [0xDE; 32];
	let body = vec![OpaqueExtrinsic::from_blob(b"renew-call".to_vec())];
	h.api
		.set_indexed(1, vec![info(hash_unfetchable, 32, HashingAlgorithm::Blake2b256, 0)]);
	// No `h.network.insert(...)` -> fetcher returns zero bytes.

	let params = gap_sync_params(1, Some(body));
	let err = h
		.wrapper
		.import_block(params)
		.await
		.expect_err("fetcher partial failure must propagate");
	let msg = format!("{err}");
	assert!(msg.contains("could not be fetched via bitswap"), "unexpected error: {msg}");
	assert!(h.captured.lock().unwrap().is_empty(), "no inner import on fetcher error");
}

#[tokio::test]
async fn import_gap_sync_without_body_passes_through() {
	// Non-archive gap sync imports headers without bodies; those must short-circuit at
	// `should_intercept` and be forwarded unchanged.
	let h = make_harness();
	h.api.set_finalized_hash(H256::from([0xFA; 32]));

	let params = params_with_origin(BlockOrigin::GapSync, 1, None);
	let result = h.wrapper.import_block(params).await.expect("pass-through import");
	assert!(matches!(result, ImportResult::Imported(_)));

	let captured = h.captured.lock().unwrap();
	assert_eq!(captured.len(), 1);
	assert!(
		!prefetched_attached(&captured[0]),
		"header-only GapSync must short-circuit before any prefetch attach",
	);
	assert_eq!(h.api.call_api_at_count(), 0, "runtime API must not be invoked");
	assert_eq!(h.network.call_count(), 0, "fetcher must not be invoked");
	assert!(
		h.api.last_indexed_transactions_state().is_none(),
		"runtime API must not have been called at any state",
	);
}

mod mock {
	use async_trait::async_trait;
	use codec::{Decode, Encode};
	use sc_storage_chain_sync::{
		BitswapHandleSlot, BitswapRequest, IndexedTransactionFetcher, StorageChainBlockImport,
	};

	use sc_consensus::{
		BlockCheckParams, BlockImport, BlockImportParams, ImportResult, ImportedAux, StateAction,
		StorageChanges as ConsensusStorageChanges,
	};
	use sc_network_bitswap::{BitswapError, Cid as BitswapCid, FetchItem, RAW_CODEC};
	use sp_api::{ApiError, ConstructRuntimeApi};
	use sp_consensus::{BlockOrigin, Error as ConsensusError};
	use sp_core::H256;
	use sp_runtime::{
		generic,
		traits::{BlakeTwo256, Block as BlockT, Header as _},
		Digest, Justifications, OpaqueExtrinsic,
	};
	use sp_state_machine::{InMemoryBackend, IndexOperation, OverlayedChanges, StorageChanges};
	use sp_transaction_storage_proof::{ContentHash, IndexedTransactionInfo};
	use std::{
		collections::{HashMap, HashSet},
		sync::{Arc, Mutex, OnceLock},
	};
	use tokio::sync::mpsc;

	pub(super) type TestBlock = generic::Block<generic::Header<u32, BlakeTwo256>, OpaqueExtrinsic>;
	type TestHeader = generic::Header<u32, BlakeTwo256>;
	#[allow(dead_code)]
	type Block = TestBlock;

	pub(super) const OVERLAY_MARKER_KEY: &[u8] = b"storage-chain-sync-overlay-marker";
	pub(super) const OVERLAY_MARKER_VALUE: &[u8] = b"visible";
	pub(super) const CASE_B_MARKER_KEY: &[u8] = b"storage-chain-sync-case-b-marker";
	pub(super) const CASE_B_MARKER_VALUE: &[u8] = b"visible-after-execute-block";
	pub(super) const CASE_B_ROLLBACK_MARKER_KEY: &[u8] = b"storage-chain-sync-case-b-rollback";

	#[derive(Default, Clone)]
	struct MockApiInner {
		indexed_at_block_number: HashMap<u32, Vec<IndexedTransactionInfo>>,
		indexed_transactions: HashMap<H256, Vec<u8>>,
		call_api_at_count: usize,
		overlay_marker_seen: bool,
		finalized_hash: H256,
		// Recorded by `indexed_transactions` API impl. Lets gap-sync tests assert the wrapper
		// queried at `finalized_hash` rather than `parent_hash`.
		last_indexed_transactions_state: Option<H256>,
	}

	#[derive(Clone, Default)]
	pub(super) struct MockApiClient {
		inner: Arc<Mutex<MockApiInner>>,
	}

	impl MockApiClient {
		pub(super) fn set_indexed(&self, block_number: u32, infos: Vec<IndexedTransactionInfo>) {
			self.inner.lock().unwrap().indexed_at_block_number.insert(block_number, infos);
		}

		pub(super) fn insert_indexed_transaction(&self, hash: ContentHash, data: Vec<u8>) {
			self.inner.lock().unwrap().indexed_transactions.insert(H256::from(hash), data);
		}

		pub(super) fn overlay_marker_seen(&self) -> bool {
			self.inner.lock().unwrap().overlay_marker_seen
		}

		pub(super) fn set_finalized_hash(&self, hash: H256) {
			self.inner.lock().unwrap().finalized_hash = hash;
		}

		pub(super) fn last_indexed_transactions_state(&self) -> Option<H256> {
			self.inner.lock().unwrap().last_indexed_transactions_state
		}

		pub(super) fn call_api_at_count(&self) -> usize {
			self.inner.lock().unwrap().call_api_at_count
		}
	}

	impl sp_api::ProvideRuntimeApi<TestBlock> for MockApiClient {
		type Api = RuntimeApiImpl<TestBlock, MockApiClient>;

		fn runtime_api(&self) -> sp_api::ApiRef<'_, Self::Api> {
			RuntimeApi::construct_runtime_api(self)
		}
	}

	impl sp_api::CallApiAt<TestBlock> for MockApiClient {
		type StateBackend =
			sp_state_machine::InMemoryBackend<sp_runtime::traits::HashingFor<TestBlock>>;

		fn call_api_at(
			&self,
			params: sp_api::CallApiAtParams<TestBlock>,
		) -> Result<Vec<u8>, ApiError> {
			assert_eq!(
				params.function, "TransactionStorageApi_indexed_transactions",
				"unexpected runtime API function",
			);
			let (block_number,): (u32,) = Decode::decode(&mut &params.arguments[..])
				.expect("encoded indexed_transactions argument must decode");
			let overlay_marker_seen = matches!(
				params.overlayed_changes.borrow_mut().storage(OVERLAY_MARKER_KEY),
				Some(Some(value)) if value == OVERLAY_MARKER_VALUE
			);
			let mut inner = self.inner.lock().unwrap();
			inner.call_api_at_count += 1;
			inner.overlay_marker_seen |= overlay_marker_seen;
			inner.last_indexed_transactions_state = Some(params.at);
			Ok(inner
				.indexed_at_block_number
				.get(&block_number)
				.cloned()
				.unwrap_or_default()
				.encode())
		}

		fn runtime_version_at(
			&self,
			_at: <TestBlock as sp_runtime::traits::Block>::Hash,
			_call_context: sp_core::traits::CallContext,
		) -> Result<sp_version::RuntimeVersion, ApiError> {
			Ok(block_execution_runtime_version())
		}

		fn state_at(
			&self,
			_at: <TestBlock as sp_runtime::traits::Block>::Hash,
		) -> Result<Self::StateBackend, ApiError> {
			unreachable!(
				"only the block-execution path queries this; out of scope for this harness"
			)
		}

		fn initialize_extensions(
			&self,
			_at: <TestBlock as sp_runtime::traits::Block>::Hash,
			_extensions: &mut sp_externalities::Extensions,
		) -> Result<(), ApiError> {
			Ok(())
		}
	}

	impl sc_client_api::BlockBackend<TestBlock> for MockApiClient {
		fn block_body(
			&self,
			_hash: <TestBlock as BlockT>::Hash,
		) -> sp_blockchain::Result<Option<Vec<<TestBlock as BlockT>::Extrinsic>>> {
			unreachable!("StorageChainBlockImport tests only query indexed transaction presence")
		}

		fn block_indexed_body(
			&self,
			_hash: <TestBlock as BlockT>::Hash,
		) -> sp_blockchain::Result<Option<Vec<Vec<u8>>>> {
			unreachable!("StorageChainBlockImport tests only query indexed transaction presence")
		}

		fn block_indexed_hashes(
			&self,
			_hash: <TestBlock as BlockT>::Hash,
		) -> sp_blockchain::Result<Option<Vec<H256>>> {
			unreachable!("StorageChainBlockImport tests only query indexed transaction presence")
		}

		fn block(
			&self,
			_hash: <TestBlock as BlockT>::Hash,
		) -> sp_blockchain::Result<Option<generic::SignedBlock<TestBlock>>> {
			unreachable!("StorageChainBlockImport tests only query indexed transaction presence")
		}

		fn block_status(
			&self,
			_hash: <TestBlock as BlockT>::Hash,
		) -> sp_blockchain::Result<sp_consensus::BlockStatus> {
			unreachable!("StorageChainBlockImport tests only query indexed transaction presence")
		}

		fn justifications(
			&self,
			_hash: <TestBlock as BlockT>::Hash,
		) -> sp_blockchain::Result<Option<Justifications>> {
			unreachable!("StorageChainBlockImport tests only query indexed transaction presence")
		}

		fn block_hash(
			&self,
			_number: sp_runtime::traits::NumberFor<TestBlock>,
		) -> sp_blockchain::Result<Option<<TestBlock as BlockT>::Hash>> {
			unreachable!("StorageChainBlockImport tests only query indexed transaction presence")
		}

		fn indexed_transaction(&self, hash: H256) -> sp_blockchain::Result<Option<Vec<u8>>> {
			Ok(self.inner.lock().unwrap().indexed_transactions.get(&hash).cloned())
		}

		fn has_indexed_transaction(&self, hash: H256) -> sp_blockchain::Result<bool> {
			Ok(self.inner.lock().unwrap().indexed_transactions.contains_key(&hash))
		}

		fn requires_full_sync(&self) -> bool {
			false
		}
	}

	impl sp_blockchain::HeaderBackend<TestBlock> for MockApiClient {
		fn header(
			&self,
			_hash: <TestBlock as BlockT>::Hash,
		) -> sp_blockchain::Result<Option<<TestBlock as BlockT>::Header>> {
			unreachable!("wrapper only queries info().finalized_hash, not header()")
		}

		fn info(&self) -> sp_blockchain::Info<TestBlock> {
			let inner = self.inner.lock().unwrap();
			sp_blockchain::Info {
				best_hash: H256::zero(),
				best_number: 0,
				genesis_hash: H256::zero(),
				finalized_hash: inner.finalized_hash,
				finalized_number: 0,
				finalized_state: None,
				number_leaves: 0,
				block_gap: None,
			}
		}

		fn status(
			&self,
			_hash: <TestBlock as BlockT>::Hash,
		) -> sp_blockchain::Result<sp_blockchain::BlockStatus> {
			unreachable!("wrapper only queries info().finalized_hash, not status()")
		}

		fn number(
			&self,
			_hash: <TestBlock as BlockT>::Hash,
		) -> sp_blockchain::Result<Option<sp_runtime::traits::NumberFor<TestBlock>>> {
			unreachable!("wrapper only queries info().finalized_hash, not number()")
		}

		fn hash(
			&self,
			_number: sp_runtime::traits::NumberFor<TestBlock>,
		) -> sp_blockchain::Result<Option<<TestBlock as BlockT>::Hash>> {
			unreachable!("wrapper only queries info().finalized_hash, not hash()")
		}
	}

	pub(super) struct TestInner {
		captured: Arc<Mutex<Vec<BlockImportParams<TestBlock>>>>,
	}

	impl TestInner {
		fn recording() -> Self {
			Self { captured: Arc::new(Mutex::new(Vec::new())) }
		}
	}

	#[async_trait]
	impl BlockImport<TestBlock> for TestInner {
		type Error = ConsensusError;

		async fn check_block(
			&self,
			_block: BlockCheckParams<TestBlock>,
		) -> Result<ImportResult, Self::Error> {
			Ok(ImportResult::imported(true))
		}

		async fn import_block(
			&self,
			block: BlockImportParams<TestBlock>,
		) -> Result<ImportResult, Self::Error> {
			self.captured.lock().unwrap().push(block);
			Ok(ImportResult::Imported(ImportedAux::default()))
		}
	}

	#[derive(Default)]
	pub(super) struct MockBitswap {
		responses: Mutex<HashMap<ContentHash, Vec<u8>>>,
		call_count: Mutex<usize>,
		observed_cids: Mutex<Vec<BitswapCid>>,
	}

	impl MockBitswap {
		pub(super) fn insert(&self, hash: ContentHash, data: Vec<u8>) {
			self.responses.lock().unwrap().insert(hash, data);
		}

		pub(super) fn call_count(&self) -> usize {
			*self.call_count.lock().unwrap()
		}

		pub(super) fn observed_cids(&self) -> Vec<BitswapCid> {
			self.observed_cids.lock().unwrap().clone()
		}
	}

	impl BitswapRequest for MockBitswap {
		fn request_stream(
			&self,
			cids: HashSet<BitswapCid>,
		) -> Result<mpsc::Receiver<FetchItem>, BitswapError> {
			*self.call_count.lock().unwrap() += 1;
			let (tx, rx) = mpsc::channel(cids.len().max(1));
			let responses = self.responses.lock().unwrap();
			let mut observed = self.observed_cids.lock().unwrap();
			for cid in cids {
				observed.push(cid);
				let digest: Option<ContentHash> = cid.hash().digest().try_into().ok();
				if let Some(bytes) = digest.and_then(|d| responses.get(&d).cloned()) {
					tx.try_send(Ok((cid, bytes.into()))).expect("channel sized for cids.len()");
				}
			}
			Ok(rx)
		}
	}

	fn populated_bitswap_slot(mock: Arc<MockBitswap>) -> BitswapHandleSlot {
		let slot: BitswapHandleSlot = Arc::new(OnceLock::new());
		let _ = slot.set(mock);
		slot
	}

	#[allow(dead_code)]
	struct Runtime;

	sp_api::impl_runtime_apis! {
		impl sp_api::Core<Block> for Runtime {
			fn version() -> sp_version::RuntimeVersion {
				block_execution_runtime_version()
			}

			fn execute_block(_block: <Block as BlockT>::LazyBlock) {}

			fn initialize_block(_header: &<Block as BlockT>::Header) -> sp_runtime::ExtrinsicInclusionMode {
				sp_runtime::ExtrinsicInclusionMode::AllExtrinsics
			}
		}

		impl sp_transaction_storage_proof::runtime_api::TransactionStorageApi<Block> for Runtime {
			fn retention_period() -> u32 {
				0
			}

			fn indexed_transactions(_block: u32) -> Vec<IndexedTransactionInfo> {
				Vec::new()
			}
		}
	}

	fn block_execution_runtime_version() -> sp_version::RuntimeVersion {
		sp_version::RuntimeVersion {
			spec_name: "storage-chain-sync-test".into(),
			impl_name: "storage-chain-sync-test".into(),
			authoring_version: 1,
			spec_version: 1,
			impl_version: 1,
			apis: RUNTIME_API_VERSIONS,
			transaction_version: 1,
			system_version: 1,
		}
	}

	#[derive(Default)]
	struct BlockExecutionInner {
		indexed_transactions: HashMap<H256, Vec<u8>>,
		execute_block_count: usize,
		indexed_transactions_count: usize,
		overlay_marker_seen_by_indexed_transactions: bool,
	}

	pub(super) struct BlockExecutionClient {
		inner: Arc<Mutex<BlockExecutionInner>>,
		content_hash: ContentHash,
		info: IndexedTransactionInfo,
	}

	impl BlockExecutionClient {
		fn new(content_hash: ContentHash, data: Vec<u8>) -> Self {
			let info = IndexedTransactionInfo {
				content_hash,
				size: data.len() as u32,
				hashing: sp_transaction_storage_proof::HashingAlgorithm::Blake2b256,
				cid_codec: RAW_CODEC,
				extrinsic_index: u32::MAX,
			};
			Self { inner: Arc::new(Mutex::new(BlockExecutionInner::default())), content_hash, info }
		}

		pub(super) fn execute_block_count(&self) -> usize {
			self.inner.lock().unwrap().execute_block_count
		}

		pub(super) fn indexed_transactions_count(&self) -> usize {
			self.inner.lock().unwrap().indexed_transactions_count
		}

		pub(super) fn overlay_marker_seen_by_indexed_transactions(&self) -> bool {
			self.inner.lock().unwrap().overlay_marker_seen_by_indexed_transactions
		}
	}

	impl sp_api::ProvideRuntimeApi<TestBlock> for BlockExecutionClient {
		type Api = RuntimeApiImpl<TestBlock, BlockExecutionClient>;

		fn runtime_api(&self) -> sp_api::ApiRef<'_, Self::Api> {
			RuntimeApi::construct_runtime_api(self)
		}
	}

	impl sp_api::CallApiAt<TestBlock> for BlockExecutionClient {
		type StateBackend = InMemoryBackend<sp_runtime::traits::HashingFor<TestBlock>>;

		fn call_api_at(
			&self,
			params: sp_api::CallApiAtParams<TestBlock>,
		) -> Result<Vec<u8>, ApiError> {
			match params.function {
				"Core_execute_block" => {
					self.inner.lock().unwrap().execute_block_count += 1;
					let mut overlay = params.overlayed_changes.borrow_mut();
					overlay.set_storage(
						CASE_B_MARKER_KEY.to_vec(),
						Some(CASE_B_MARKER_VALUE.to_vec()),
					);
					overlay.add_transaction_index(IndexOperation::Renew {
						extrinsic: 0,
						hash: self.content_hash.to_vec(),
					});
					Ok(().encode())
				},
				"TransactionStorageApi_indexed_transactions" => {
					let overlay_marker_seen = {
						let mut overlay = params.overlayed_changes.borrow_mut();
						let seen = matches!(
							overlay.storage(CASE_B_MARKER_KEY),
							Some(Some(value)) if value == CASE_B_MARKER_VALUE
						);
						overlay.set_storage(
							CASE_B_ROLLBACK_MARKER_KEY.to_vec(),
							Some(b"must-be-rolled-back".to_vec()),
						);
						seen
					};
					let mut inner = self.inner.lock().unwrap();
					inner.indexed_transactions_count += 1;
					inner.overlay_marker_seen_by_indexed_transactions |= overlay_marker_seen;
					Ok(vec![self.info.clone()].encode())
				},
				other => panic!("unexpected runtime API function: {other}"),
			}
		}

		fn runtime_version_at(
			&self,
			_at: <TestBlock as sp_runtime::traits::Block>::Hash,
			_call_context: sp_core::traits::CallContext,
		) -> Result<sp_version::RuntimeVersion, ApiError> {
			Ok(block_execution_runtime_version())
		}

		fn state_at(
			&self,
			_at: <TestBlock as sp_runtime::traits::Block>::Hash,
		) -> Result<Self::StateBackend, ApiError> {
			Ok(InMemoryBackend::default())
		}

		fn initialize_extensions(
			&self,
			_at: <TestBlock as sp_runtime::traits::Block>::Hash,
			_extensions: &mut sp_externalities::Extensions,
		) -> Result<(), ApiError> {
			Ok(())
		}
	}

	impl sc_client_api::BlockBackend<TestBlock> for BlockExecutionClient {
		fn block_body(
			&self,
			_hash: <TestBlock as BlockT>::Hash,
		) -> sp_blockchain::Result<Option<Vec<<TestBlock as BlockT>::Extrinsic>>> {
			unreachable!("StorageChainBlockImport tests only query indexed transaction presence")
		}

		fn block_indexed_body(
			&self,
			_hash: <TestBlock as BlockT>::Hash,
		) -> sp_blockchain::Result<Option<Vec<Vec<u8>>>> {
			unreachable!("StorageChainBlockImport tests only query indexed transaction presence")
		}

		fn block_indexed_hashes(
			&self,
			_hash: <TestBlock as BlockT>::Hash,
		) -> sp_blockchain::Result<Option<Vec<H256>>> {
			unreachable!("StorageChainBlockImport tests only query indexed transaction presence")
		}

		fn block(
			&self,
			_hash: <TestBlock as BlockT>::Hash,
		) -> sp_blockchain::Result<Option<generic::SignedBlock<TestBlock>>> {
			unreachable!("StorageChainBlockImport tests only query indexed transaction presence")
		}

		fn block_status(
			&self,
			_hash: <TestBlock as BlockT>::Hash,
		) -> sp_blockchain::Result<sp_consensus::BlockStatus> {
			unreachable!("StorageChainBlockImport tests only query indexed transaction presence")
		}

		fn justifications(
			&self,
			_hash: <TestBlock as BlockT>::Hash,
		) -> sp_blockchain::Result<Option<Justifications>> {
			unreachable!("StorageChainBlockImport tests only query indexed transaction presence")
		}

		fn block_hash(
			&self,
			_number: sp_runtime::traits::NumberFor<TestBlock>,
		) -> sp_blockchain::Result<Option<<TestBlock as BlockT>::Hash>> {
			unreachable!("StorageChainBlockImport tests only query indexed transaction presence")
		}

		fn indexed_transaction(&self, hash: H256) -> sp_blockchain::Result<Option<Vec<u8>>> {
			Ok(self.inner.lock().unwrap().indexed_transactions.get(&hash).cloned())
		}

		fn has_indexed_transaction(&self, hash: H256) -> sp_blockchain::Result<bool> {
			Ok(self.inner.lock().unwrap().indexed_transactions.contains_key(&hash))
		}

		fn requires_full_sync(&self) -> bool {
			false
		}
	}

	impl sp_blockchain::HeaderBackend<TestBlock> for BlockExecutionClient {
		fn header(
			&self,
			_hash: <TestBlock as BlockT>::Hash,
		) -> sp_blockchain::Result<Option<<TestBlock as BlockT>::Header>> {
			unreachable!("block-execution wrapper does not call info().finalized_hash on this path")
		}

		fn info(&self) -> sp_blockchain::Info<TestBlock> {
			sp_blockchain::Info {
				best_hash: H256::zero(),
				best_number: 0,
				genesis_hash: H256::zero(),
				finalized_hash: H256::zero(),
				finalized_number: 0,
				finalized_state: None,
				number_leaves: 0,
				block_gap: None,
			}
		}

		fn status(
			&self,
			_hash: <TestBlock as BlockT>::Hash,
		) -> sp_blockchain::Result<sp_blockchain::BlockStatus> {
			unreachable!("block-execution wrapper does not call status()")
		}

		fn number(
			&self,
			_hash: <TestBlock as BlockT>::Hash,
		) -> sp_blockchain::Result<Option<sp_runtime::traits::NumberFor<TestBlock>>> {
			unreachable!("block-execution wrapper does not call number()")
		}

		fn hash(
			&self,
			_number: sp_runtime::traits::NumberFor<TestBlock>,
		) -> sp_blockchain::Result<Option<<TestBlock as BlockT>::Hash>> {
			unreachable!("block-execution wrapper does not call hash()")
		}
	}

	pub(super) struct Harness {
		pub(super) wrapper: StorageChainBlockImport<TestBlock, TestInner, MockApiClient>,
		pub(super) api: Arc<MockApiClient>,
		pub(super) captured: Arc<Mutex<Vec<BlockImportParams<TestBlock>>>>,
		pub(super) network: Arc<MockBitswap>,
	}

	pub(super) fn make_harness() -> Harness {
		let api = Arc::new(MockApiClient::default());
		let network: Arc<MockBitswap> = Arc::new(MockBitswap::default());
		let inner = TestInner::recording();
		let captured = inner.captured.clone();

		let fetcher = IndexedTransactionFetcher::new(populated_bitswap_slot(network.clone()));
		let wrapper = StorageChainBlockImport::new(inner, api.clone(), fetcher);

		Harness { wrapper, api, captured, network }
	}

	pub(super) struct BlockExecutionHarness {
		pub(super) wrapper: StorageChainBlockImport<TestBlock, TestInner, BlockExecutionClient>,
		pub(super) api: Arc<BlockExecutionClient>,
		pub(super) captured: Arc<Mutex<Vec<BlockImportParams<TestBlock>>>>,
		pub(super) content_hash: ContentHash,
	}

	pub(super) fn make_block_execution_harness(data: Vec<u8>) -> BlockExecutionHarness {
		let content_hash = sp_transaction_storage_proof::HashingAlgorithm::Blake2b256.hash(&data);
		let api = Arc::new(BlockExecutionClient::new(content_hash, data.clone()));
		let network: Arc<MockBitswap> = Arc::new(MockBitswap::default());
		network.insert(content_hash, data);
		let inner = TestInner::recording();
		let captured = inner.captured.clone();

		let fetcher = IndexedTransactionFetcher::new(populated_bitswap_slot(network));
		let wrapper = StorageChainBlockImport::new(inner, api.clone(), fetcher);

		BlockExecutionHarness { wrapper, api, captured, content_hash }
	}

	pub(super) fn block_execution_params(number: u32) -> BlockImportParams<TestBlock> {
		let header = TestHeader::new(
			number,
			H256::zero(),
			block_execution_state_root(),
			H256::zero(),
			Digest::default(),
		);
		let mut params = BlockImportParams::new(BlockOrigin::NetworkBroadcast, header);
		params.body = Some(vec![OpaqueExtrinsic::from_blob(b"case-b-renew-call".to_vec())]);
		params.fork_choice = Some(sc_consensus::ForkChoiceStrategy::Custom(true));
		params
	}

	fn block_execution_state_root() -> H256 {
		let backend = InMemoryBackend::<sp_runtime::traits::HashingFor<TestBlock>>::default();
		let mut overlay = OverlayedChanges::<sp_runtime::traits::HashingFor<TestBlock>>::default();
		overlay.set_storage(CASE_B_MARKER_KEY.to_vec(), Some(CASE_B_MARKER_VALUE.to_vec()));
		overlay
			.storage_root(&backend, block_execution_runtime_version().state_version())
			.0
	}

	fn test_header(number: u32, parent: H256) -> TestHeader {
		TestHeader::new(number, H256::zero(), H256::zero(), parent, Digest::default())
	}

	pub(super) fn params_with_origin(
		origin: BlockOrigin,
		number: u32,
		body: Option<Vec<OpaqueExtrinsic>>,
	) -> BlockImportParams<TestBlock> {
		let header = test_header(number, H256::zero());
		let mut params = BlockImportParams::new(origin, header);
		params.body = body;
		params.fork_choice = Some(sc_consensus::ForkChoiceStrategy::Custom(true));
		params
	}

	fn empty_storage_changes() -> StorageChanges<sp_runtime::traits::HashingFor<TestBlock>> {
		StorageChanges::default()
	}

	pub(super) fn renew_op(hash: ContentHash, extrinsic_index: u32) -> IndexOperation {
		IndexOperation::Renew { extrinsic: extrinsic_index, hash: hash.to_vec() }
	}

	pub(super) fn attached_changes_params(
		number: u32,
		renews: Vec<IndexOperation>,
	) -> BlockImportParams<TestBlock> {
		let mut params = params_with_origin(
			BlockOrigin::NetworkBroadcast,
			number,
			Some(vec![OpaqueExtrinsic::from_blob(b"renew-call".to_vec())]),
		);
		let mut changes = empty_storage_changes();
		changes
			.main_storage_changes
			.push((OVERLAY_MARKER_KEY.to_vec(), Some(OVERLAY_MARKER_VALUE.to_vec())));
		changes.transaction_index_changes = renews;
		params.state_action = StateAction::ApplyChanges(ConsensusStorageChanges::Changes(changes));
		params
	}

	pub(super) fn prefetched_attached(params: &BlockImportParams<TestBlock>) -> bool {
		let prefetched = &params.prefetched_indexed_transactions;
		!prefetched.ops.is_empty() || !prefetched.renew_payloads.is_empty()
	}

	// Builds a `BlockImportParams` with `BlockOrigin::GapSync` and a populated body.
	// State action defaults to `StateAction::Execute` per `BlockImportParams::new`, but
	// the wrapper's gap-sync dispatch ignores it; the production sync layer translates
	// `skip_execution=true` into `StateAction::Skip`, the wrapper preserves that.
	pub(super) fn gap_sync_params(
		number: u32,
		body: Option<Vec<OpaqueExtrinsic>>,
	) -> BlockImportParams<TestBlock> {
		let mut params = params_with_origin(BlockOrigin::GapSync, number, body);
		params.state_action = StateAction::Skip;
		params
	}
}
