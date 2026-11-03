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

//! Test utilities.

use parking_lot::Mutex;
use sc_client_api::{
	execution_extensions::ExecutionExtensions, BlockBackend, BlockImportNotification,
	BlockchainEvents, CallExecutor, ChildInfo, ExecutorProvider, FinalityNotification,
	FinalityNotifications, FinalizeSummary, ImportNotifications, KeysIter, MerkleValue, PairsIter,
	StaleBlock, StorageData, StorageEventStream, StorageKey, StorageProvider,
};
use sc_utils::mpsc::{tracing_unbounded, TracingUnboundedSender};
use sp_api::{CallApiAt, CallApiAtParams, CallContext};
use sp_blockchain::{BlockStatus, CachedHeaderMetadata, HeaderBackend, HeaderMetadata, Info};
use sp_consensus::BlockOrigin;
use sp_runtime::{
	generic::SignedBlock,
	traits::{Block as BlockT, Header as HeaderT, NumberFor},
	Justifications,
};
use sp_version::RuntimeVersion;
use std::sync::Arc;
use substrate_test_runtime::{Block, Hash, Header, H256};

/// A mock client used for testing.
pub struct ChainHeadMockClient<Client> {
	client: Arc<Client>,
	import_sinks: Mutex<Vec<TracingUnboundedSender<BlockImportNotification<Block>>>>,
	finality_sinks: Mutex<Vec<TracingUnboundedSender<FinalityNotification<Block>>>>,
	best_block: Mutex<Option<(H256, u64)>>,
}

impl<Client> ChainHeadMockClient<Client> {
	/// Create a new mock client.
	pub fn new(client: Arc<Client>) -> Self {
		ChainHeadMockClient {
			client,
			import_sinks: Default::default(),
			finality_sinks: Default::default(),
			best_block: Default::default(),
		}
	}

	/// Trigger the import stram from a header.
	pub async fn trigger_import_stream(&self, header: Header) {
		// Ensure the client called the `import_notification_stream`.
		while self.import_sinks.lock().is_empty() {
			tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
		}

		// Build the notification.
		let (sink, _stream) = tracing_unbounded("test_sink", 100_000);
		let notification =
			BlockImportNotification::new(header.hash(), BlockOrigin::Own, header, true, None, sink);

		for sink in self.import_sinks.lock().iter_mut() {
			let _ = sink.unbounded_send(notification.clone());
		}
	}

	/// Trigger the import stram from a header and a list of stale heads.
	pub async fn trigger_finality_stream(&self, header: Header, stale_blocks: Vec<Hash>) {
		// Ensure the client called the `finality_notification_stream`.
		while self.finality_sinks.lock().is_empty() {
			tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
		}

		// Build the notification.
		let (sink, _stream) = tracing_unbounded("test_sink", 100_000);
		let summary = FinalizeSummary {
			header: header.clone(),
			finalized: vec![header.hash()],
			stale_blocks: stale_blocks
				.into_iter()
				.map(|h| StaleBlock { hash: h, is_head: false })
				.collect(),
		};
		let notification = FinalityNotification::from_summary(summary, sink);

		for sink in self.finality_sinks.lock().iter_mut() {
			let _ = sink.unbounded_send(notification.clone());
		}
	}

	/// Set the best block hash and number that is reported by the `info` method.
	pub fn set_best_block(&self, hash: H256, number: u64) {
		*self.best_block.lock() = Some((hash, number));
	}
}

// ChainHead calls `import_notification_stream` and `finality_notification_stream` in order to
// subscribe to block events.
impl<Client> BlockchainEvents<Block> for ChainHeadMockClient<Client> {
	fn import_notification_stream(&self) -> ImportNotifications<Block> {
		let (sink, stream) = tracing_unbounded("import_notification_stream", 1024);
		self.import_sinks.lock().push(sink);
		stream
	}

	fn every_import_notification_stream(&self) -> ImportNotifications<Block> {
		unimplemented!()
	}

	fn finality_notification_stream(&self) -> FinalityNotifications<Block> {
		let (sink, stream) = tracing_unbounded("finality_notification_stream", 1024);
		self.finality_sinks.lock().push(sink);
		stream
	}

	fn storage_changes_notification_stream(
		&self,
		_filter_keys: Option<&[StorageKey]>,
		_child_filter_keys: Option<&[(StorageKey, Option<Vec<StorageKey>>)]>,
	) -> sp_blockchain::Result<StorageEventStream<Hash>> {
		unimplemented!()
	}
}

// The following implementations are imposed by the `chainHead` trait bounds.

impl<Block: BlockT, E: CallExecutor<Block>, Client: ExecutorProvider<Block, Executor = E>>
	ExecutorProvider<Block> for ChainHeadMockClient<Client>
{
	type Executor = <Client as ExecutorProvider<Block>>::Executor;

	fn executor(&self) -> &Self::Executor {
		self.client.executor()
	}

	fn execution_extensions(&self) -> &ExecutionExtensions<Block> {
		self.client.execution_extensions()
	}
}

impl<
		BE: sc_client_api::backend::Backend<Block> + Send + Sync + 'static,
		Block: BlockT,
		Client: StorageProvider<Block, BE>,
	> StorageProvider<Block, BE> for ChainHeadMockClient<Client>
{
	fn storage(
		&self,
		hash: Block::Hash,
		key: &StorageKey,
	) -> sp_blockchain::Result<Option<StorageData>> {
		self.client.storage(hash, key)
	}

	fn storage_hash(
		&self,
		hash: Block::Hash,
		key: &StorageKey,
	) -> sp_blockchain::Result<Option<Block::Hash>> {
		self.client.storage_hash(hash, key)
	}

	fn storage_keys(
		&self,
		hash: Block::Hash,
		prefix: Option<&StorageKey>,
		start_key: Option<&StorageKey>,
	) -> sp_blockchain::Result<KeysIter<BE::State, Block>> {
		self.client.storage_keys(hash, prefix, start_key)
	}

	fn storage_pairs(
		&self,
		hash: <Block as BlockT>::Hash,
		prefix: Option<&StorageKey>,
		start_key: Option<&StorageKey>,
	) -> sp_blockchain::Result<PairsIter<BE::State, Block>> {
		self.client.storage_pairs(hash, prefix, start_key)
	}

	fn child_storage(
		&self,
		hash: Block::Hash,
		child_info: &ChildInfo,
		key: &StorageKey,
	) -> sp_blockchain::Result<Option<StorageData>> {
		self.client.child_storage(hash, child_info, key)
	}

	fn child_storage_keys(
		&self,
		hash: Block::Hash,
		child_info: ChildInfo,
		prefix: Option<&StorageKey>,
		start_key: Option<&StorageKey>,
	) -> sp_blockchain::Result<KeysIter<BE::State, Block>> {
		self.client.child_storage_keys(hash, child_info, prefix, start_key)
	}

	fn child_storage_hash(
		&self,
		hash: Block::Hash,
		child_info: &ChildInfo,
		key: &StorageKey,
	) -> sp_blockchain::Result<Option<Block::Hash>> {
		self.client.child_storage_hash(hash, child_info, key)
	}

	fn closest_merkle_value(
		&self,
		hash: Block::Hash,
		key: &StorageKey,
	) -> sp_blockchain::Result<Option<MerkleValue<Block::Hash>>> {
		self.client.closest_merkle_value(hash, key)
	}

	fn child_closest_merkle_value(
		&self,
		hash: Block::Hash,
		child_info: &ChildInfo,
		key: &StorageKey,
	) -> sp_blockchain::Result<Option<MerkleValue<Block::Hash>>> {
		self.client.child_closest_merkle_value(hash, child_info, key)
	}
}

impl<Block: BlockT, Client: CallApiAt<Block>> CallApiAt<Block> for ChainHeadMockClient<Client> {
	type StateBackend = <Client as CallApiAt<Block>>::StateBackend;

	fn call_api_at(&self, params: CallApiAtParams<Block>) -> Result<Vec<u8>, sp_api::ApiError> {
		self.client.call_api_at(params)
	}

	fn runtime_version_at(
		&self,
		hash: Block::Hash,
		call_context: CallContext,
	) -> Result<RuntimeVersion, sp_api::ApiError> {
		self.client.runtime_version_at(hash, call_context)
	}

	fn state_at(&self, at: Block::Hash) -> Result<Self::StateBackend, sp_api::ApiError> {
		self.client.state_at(at)
	}

	fn initialize_extensions(
		&self,
		at: <Block as BlockT>::Hash,
		extensions: &mut sp_externalities::Extensions,
	) -> Result<(), sp_api::ApiError> {
		self.client.initialize_extensions(at, extensions)
	}
}

impl<Block: BlockT, Client: BlockBackend<Block>> BlockBackend<Block>
	for ChainHeadMockClient<Client>
{
	fn block_body(
		&self,
		hash: Block::Hash,
	) -> sp_blockchain::Result<Option<Vec<<Block as BlockT>::Extrinsic>>> {
		self.client.block_body(hash)
	}

	fn block(&self, hash: Block::Hash) -> sp_blockchain::Result<Option<SignedBlock<Block>>> {
		self.client.block(hash)
	}

	fn block_status(&self, hash: Block::Hash) -> sp_blockchain::Result<sp_consensus::BlockStatus> {
		self.client.block_status(hash)
	}

	fn justifications(&self, hash: Block::Hash) -> sp_blockchain::Result<Option<Justifications>> {
		self.client.justifications(hash)
	}

	fn block_hash(&self, number: NumberFor<Block>) -> sp_blockchain::Result<Option<Block::Hash>> {
		self.client.block_hash(number)
	}

	fn indexed_transaction(&self, hash: sp_core::H256) -> sp_blockchain::Result<Option<Vec<u8>>> {
		self.client.indexed_transaction(hash)
	}

	fn has_indexed_transaction(&self, hash: sp_core::H256) -> sp_blockchain::Result<bool> {
		self.client.has_indexed_transaction(hash)
	}

	fn block_indexed_body(&self, hash: Block::Hash) -> sp_blockchain::Result<Option<Vec<Vec<u8>>>> {
		self.client.block_indexed_body(hash)
	}

	fn block_indexed_hashes(&self, hash: Block::Hash) -> sp_blockchain::Result<Option<Vec<H256>>> {
		self.client.block_indexed_hashes(hash)
	}

	fn requires_full_sync(&self) -> bool {
		self.client.requires_full_sync()
	}
}

impl<Block: BlockT, Client: HeaderMetadata<Block> + Send + Sync> HeaderMetadata<Block>
	for ChainHeadMockClient<Client>
{
	type Error = <Client as HeaderMetadata<Block>>::Error;

	fn header_metadata(
		&self,
		hash: Block::Hash,
	) -> Result<CachedHeaderMetadata<Block>, Self::Error> {
		self.client.header_metadata(hash)
	}

	fn insert_header_metadata(
		&self,
		hash: Block::Hash,
		header_metadata: CachedHeaderMetadata<Block>,
	) {
		self.client.insert_header_metadata(hash, header_metadata)
	}

	fn remove_header_metadata(&self, hash: Block::Hash) {
		self.client.remove_header_metadata(hash)
	}
}

impl<Block: BlockT<Hash = H256>, Client: HeaderBackend<Block> + Send + Sync> HeaderBackend<Block>
	for ChainHeadMockClient<Client>
where
	<<Block as sp_runtime::traits::Block>::Header as HeaderT>::Number: From<u64>,
{
	fn header(
		&self,
		hash: Block::Hash,
	) -> sp_blockchain::Result<Option<<Block as BlockT>::Header>> {
		self.client.header(hash)
	}

	fn info(&self) -> Info<Block> {
		let mut info = self.client.info();

		if let Some((block_hash, block_num)) = self.best_block.lock().take() {
			info.best_hash = block_hash;
			info.best_number = block_num.into();
		}

		info
	}

	fn status(&self, hash: Block::Hash) -> sc_client_api::blockchain::Result<BlockStatus> {
		self.client.status(hash)
	}

	fn number(
		&self,
		hash: Block::Hash,
	) -> sc_client_api::blockchain::Result<Option<<<Block as BlockT>::Header as HeaderT>::Number>>
	{
		self.client.number(hash)
	}

	fn hash(
		&self,
		number: <<Block as BlockT>::Header as HeaderT>::Number,
	) -> sp_blockchain::Result<Option<Block::Hash>> {
		self.client.hash(number)
	}
}
