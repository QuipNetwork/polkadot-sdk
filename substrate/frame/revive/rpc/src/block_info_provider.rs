// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
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

use crate::{
	ClientError, LOG_TARGET,
	client::{SubscriptionType, SubstrateBlock, SubstrateBlockNumber},
	subxt_client::SrcChainConfig,
};
use jsonrpsee::core::async_trait;
use sp_core::H256;
use std::sync::Arc;
use subxt::{
	OnlineClient, config::RpcConfigFor, error::OnlineClientAtBlockError,
	rpcs::methods::LegacyRpcMethods,
};
use tokio::sync::RwLock;

/// BlockInfoProvider cache and retrieves information about blocks.
#[async_trait]
pub trait BlockInfoProvider: Send + Sync {
	/// Update the latest block or the latest finalized block, depending on `subscription_type`,
	/// ignoring a block that is not a valid new head.
	async fn update_latest(&self, block: Arc<SubstrateBlock>, subscription_type: SubscriptionType);

	/// Return the latest finalized block.
	async fn latest_finalized_block(&self) -> Arc<SubstrateBlock>;

	/// Return the latest block.
	async fn latest_block(&self) -> Arc<SubstrateBlock>;

	/// Return the latest block number
	async fn latest_block_number(&self) -> SubstrateBlockNumber {
		self.latest_block().await.block_number()
	}

	/// Get block by block_number.
	async fn block_by_number(
		&self,
		block_number: SubstrateBlockNumber,
	) -> Result<Option<Arc<SubstrateBlock>>, ClientError>;

	/// Get block by block hash.
	async fn block_by_hash(&self, hash: &H256) -> Result<Option<Arc<SubstrateBlock>>, ClientError>;
}

/// Provides information about blocks.
#[derive(Clone)]
pub struct SubxtBlockInfoProvider {
	/// The latest block.
	latest_block: Arc<RwLock<Arc<SubstrateBlock>>>,

	/// The latest finalized block.
	latest_finalized_block: Arc<RwLock<Arc<SubstrateBlock>>>,

	/// The rpc client, used to fetch blocks not in the cache.
	rpc: LegacyRpcMethods<RpcConfigFor<SrcChainConfig>>,

	/// The api client, used to fetch blocks not in the cache.
	api: OnlineClient<SrcChainConfig>,
}

impl SubxtBlockInfoProvider {
	pub async fn new(
		api: OnlineClient<SrcChainConfig>,
		rpc: LegacyRpcMethods<RpcConfigFor<SrcChainConfig>>,
	) -> Result<Self, ClientError> {
		let latest_finalized_block = Arc::new(api.at_current_block().await?);
		let best_hash = rpc.chain_get_block_hash(None).await?.ok_or(ClientError::BlockNotFound)?;
		let latest_block = Arc::new(api.at_block(best_hash).await?);
		Ok(Self {
			api,
			rpc,
			latest_block: Arc::new(RwLock::new(latest_block)),
			latest_finalized_block: Arc::new(RwLock::new(latest_finalized_block)),
		})
	}

	/// Whether `number` still resolves to `hash` on chain.
	async fn is_canonical(&self, number: SubstrateBlockNumber, hash: H256) -> bool {
		match self.rpc.chain_get_block_hash(Some(number.into())).await {
			Ok(canonical) => canonical == Some(hash),
			Err(err) => {
				log::debug!(target: LOG_TARGET,
					"Failed to check if block #{number} ({hash:?}) is canonical, keeping it as the latest block: {err:?}");
				true
			},
		}
	}

	/// Update the latest finalized block, and the latest block when it is no longer ahead of it.
	async fn update_finalized(&self, block: Arc<SubstrateBlock>) {
		// The finalized block only ever increases.
		let mut finalized = self.latest_finalized_block.write().await;
		if block.block_number() >= finalized.block_number() {
			*finalized = block;
		}
		let finalized_block = finalized.clone();
		drop(finalized);

		// A finalized block is on the best chain, so the best block is never behind it.
		let mut best = self.latest_block.write().await;
		if finalized_block.block_number() >= best.block_number() &&
			finalized_block.block_hash() != best.block_hash()
		{
			log::debug!(target: LOG_TARGET,
				"Advancing the latest block #{} ({:?}) to the finalized block #{} ({:?}): it is no longer ahead",
				best.block_number(),
				best.block_hash(),
				finalized_block.block_number(),
				finalized_block.block_hash());
			*best = finalized_block;
		}
	}

	/// Update the latest block, ignoring a replay of a block at or below the cached one.
	async fn update_best(&self, block: Arc<SubstrateBlock>) {
		let is_same_or_above = |other: &SubstrateBlock| {
			block.block_number() > other.block_number() || block.block_hash() == other.block_hash()
		};

		let mut best = self.latest_block.write().await;
		if is_same_or_above(&best) {
			*best = block;
			return;
		}

		let (best_number, best_hash) = (best.block_number(), best.block_hash());
		drop(best);

		// The chain's best block never falls behind the finalized block.
		let finalized = self.latest_finalized_block.read().await.clone();
		if !is_same_or_above(&finalized) {
			log::debug!(target: LOG_TARGET,
				"Ignoring best block #{} ({:?}): it is neither the finalized block #{} ({:?}) nor above it",
				block.block_number(),
				block.block_hash(),
				finalized.block_number(),
				finalized.block_hash());
			return;
		}

		// A lower block is a replay, unless the stored best block is no longer canonical.
		if self.is_canonical(best_number, best_hash).await {
			return;
		}

		let mut best = self.latest_block.write().await;
		if best.block_hash() != best_hash {
			debug_assert!(false, "the latest block must have a single writer");
			log::warn!(target: LOG_TARGET,
				"Ignoring best block #{} ({:?}): the latest block was concurrently replaced with #{} ({:?})",
				block.block_number(),
				block.block_hash(),
				best.block_number(),
				best.block_hash());
			return;
		}

		log::trace!(target: LOG_TARGET,
			"Moving the latest block back from #{best_number} ({best_hash:?}) to #{} ({:?}): the chain no longer lists it",
			block.block_number(),
			block.block_hash());
		*best = block;
	}
}

#[async_trait]
impl BlockInfoProvider for SubxtBlockInfoProvider {
	async fn update_latest(&self, block: Arc<SubstrateBlock>, subscription_type: SubscriptionType) {
		match subscription_type {
			SubscriptionType::FinalizedBlocks => self.update_finalized(block).await,
			SubscriptionType::BestBlocks => self.update_best(block).await,
		}
	}

	async fn latest_block(&self) -> Arc<SubstrateBlock> {
		self.latest_block.read().await.clone()
	}

	async fn latest_finalized_block(&self) -> Arc<SubstrateBlock> {
		self.latest_finalized_block.read().await.clone()
	}

	async fn block_by_number(
		&self,
		block_number: SubstrateBlockNumber,
	) -> Result<Option<Arc<SubstrateBlock>>, ClientError> {
		let latest = self.latest_block().await;
		if block_number == latest.block_number() {
			return Ok(Some(latest));
		}

		let latest_finalized = self.latest_finalized_block().await;
		if block_number == latest_finalized.block_number() {
			return Ok(Some(latest_finalized));
		}

		let Some(hash) = self.rpc.chain_get_block_hash(Some(block_number.into())).await? else {
			return Ok(None);
		};

		match self.api.at_block(hash).await {
			Ok(block) => Ok(Some(Arc::new(block))),
			Err(
				OnlineClientAtBlockError::BlockHeaderNotFound { .. } |
				OnlineClientAtBlockError::BlockNotFound { .. },
			) => Ok(None),
			Err(err) => Err(err.into()),
		}
	}

	async fn block_by_hash(&self, hash: &H256) -> Result<Option<Arc<SubstrateBlock>>, ClientError> {
		let latest = self.latest_block().await;
		if hash == &latest.block_hash() {
			return Ok(Some(latest));
		}

		let latest_finalized = self.latest_finalized_block().await;
		if hash == &latest_finalized.block_hash() {
			return Ok(Some(latest_finalized));
		}

		match self.api.at_block(*hash).await {
			Ok(block) => Ok(Some(Arc::new(block))),
			Err(
				OnlineClientAtBlockError::BlockHeaderNotFound { .. } |
				OnlineClientAtBlockError::BlockNotFound { .. },
			) => {
				log::trace!(target: LOG_TARGET, "block_by_hash: block {hash:?} not found");
				Ok(None)
			},
			Err(err) => {
				log::trace!(target: LOG_TARGET, "block_by_hash: failed to fetch block {hash:?}: {err:?}");
				Err(err.into())
			},
		}
	}
}

#[cfg(test)]
pub mod test {
	use super::*;
	use crate::BlockInfo;
	use codec::Decode;
	use std::sync::{
		Mutex,
		atomic::{AtomicBool, AtomicUsize, Ordering},
	};
	use subxt::{
		backend::LegacyBackend,
		config::{
			polkadot::PolkadotConfigBuilder,
			substrate::{SpecVersionForRange, SubstrateHeader},
		},
		metadata::Metadata,
		rpcs::{
			Error as RpcError, RpcClient, UserError,
			client::{MockRpcClient, mock_rpc_client::Json},
		},
	};

	/// A Noop BlockInfoProvider used to test [`crate::ReceiptProvider`].
	pub struct MockBlockInfoProvider;

	pub struct MockBlockInfo {
		pub number: SubstrateBlockNumber,
		pub hash: H256,
	}

	impl BlockInfo for MockBlockInfo {
		fn hash(&self) -> H256 {
			self.hash
		}
		fn number(&self) -> SubstrateBlockNumber {
			self.number
		}
	}

	#[async_trait]
	impl BlockInfoProvider for MockBlockInfoProvider {
		async fn update_latest(
			&self,
			_block: Arc<SubstrateBlock>,
			_subscription_type: SubscriptionType,
		) {
		}

		async fn latest_finalized_block(&self) -> Arc<SubstrateBlock> {
			unimplemented!()
		}

		async fn latest_block(&self) -> Arc<SubstrateBlock> {
			unimplemented!()
		}

		async fn latest_block_number(&self) -> SubstrateBlockNumber {
			2u64
		}

		async fn block_by_number(
			&self,
			_block_number: SubstrateBlockNumber,
		) -> Result<Option<Arc<SubstrateBlock>>, ClientError> {
			Ok(None)
		}

		async fn block_by_hash(
			&self,
			_hash: &H256,
		) -> Result<Option<Arc<SubstrateBlock>>, ClientError> {
			Ok(None)
		}
	}

	/// A config carrying the generated runtime metadata for every block.
	pub(crate) fn chain_config() -> SrcChainConfig {
		let metadata_bytes: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/revive_chain.scale"));
		let metadata = Metadata::decode(&mut &metadata_bytes[..]).unwrap();
		PolkadotConfigBuilder::new()
			.set_metadata_for_spec_versions(std::iter::once((0u32, metadata.into())))
			.set_spec_version_for_block_ranges(std::iter::once(SpecVersionForRange {
				block_range: 0..u64::MAX,
				spec_version: 0,
				transaction_version: 0,
			}))
			.build()
	}

	/// A block at the given block number, on one of two branches.
	#[derive(Clone, Copy)]
	enum MockBlockId {
		MainBranch(u64),
		SideBranch(u64),
	}

	impl MockBlockId {
		/// Offsets the block number in the hash byte; keeps the genesis hash non-zero and
		/// the two branches in disjoint ranges.
		const MAIN_BRANCH_OFFSET: u8 = 0x01;
		const SIDE_BRANCH_OFFSET: u8 = 0xa0;
		/// The highest block number both offsets encode without leaving their range.
		const MAX_BLOCK_NUMBER: u64 = (u8::MAX - Self::SIDE_BRANCH_OFFSET) as u64;

		/// The hash of this block: its block number as the repeated byte, shifted by the
		/// variant's offset.
		fn hash(self) -> H256 {
			let (offset, number) = match self {
				MockBlockId::MainBranch(number) => (Self::MAIN_BRANCH_OFFSET, number),
				MockBlockId::SideBranch(number) => (Self::SIDE_BRANCH_OFFSET, number),
			};
			assert!(
				number <= Self::MAX_BLOCK_NUMBER,
				"a mock block number must not exceed {}",
				Self::MAX_BLOCK_NUMBER
			);
			H256::repeat_byte(offset + number as u8)
		}

		fn number(self) -> u64 {
			match self {
				MockBlockId::MainBranch(number) | MockBlockId::SideBranch(number) => number,
			}
		}

		/// Recover the block embedded in a hash, so a header can be derived for any
		/// `MockBlockId` hash without a table of known blocks.
		fn from_hash(hash: H256) -> Option<MockBlockId> {
			let bytes = hash.as_fixed_bytes();
			let byte = bytes[0];
			if bytes.iter().any(|other| other != &byte) {
				return None;
			}
			if let Some(number) = byte.checked_sub(Self::SIDE_BRANCH_OFFSET) {
				Some(MockBlockId::SideBranch(number.into()))
			} else if let Some(number) = byte.checked_sub(Self::MAIN_BRANCH_OFFSET) {
				Some(MockBlockId::MainBranch(number.into()))
			} else {
				None
			}
		}
	}

	const _: () = assert!(
		MockBlockId::MAIN_BRANCH_OFFSET as u64 + MockBlockId::MAX_BLOCK_NUMBER <
			MockBlockId::SIDE_BRANCH_OFFSET as u64,
		"a main-branch hash must stay below the side-branch range"
	);

	/// Decode the JSON-RPC params into the handler's parameter tuple.
	fn decode_params<ParamsTuple: serde::de::DeserializeOwned>(
		params: Option<Box<serde_json::value::RawValue>>,
	) -> ParamsTuple {
		let raw = params.expect("legacy RPC methods always send params");
		serde_json::from_str(raw.get()).expect("params decode into the parameter tuple")
	}

	/// Build the incoming block that tests pass to `update_latest`.
	async fn block_at(
		api: &OnlineClient<SrcChainConfig>,
		block: MockBlockId,
	) -> Arc<SubstrateBlock> {
		Arc::new(api.at_block(block.hash()).await.unwrap())
	}

	/// The heads of the mocked chain.
	#[derive(Clone)]
	struct MockChainHeads {
		/// The chain's best block.
		best_block: Arc<Mutex<MockBlockId>>,
		/// The chain's finalized block.
		finalized_block: MockBlockId,
		/// The number of `chain_getBlockHash` calls received.
		block_hash_lookup_count: Arc<AtomicUsize>,
		/// Fail block hash lookups while set.
		fail_block_hash_lookups: Arc<AtomicBool>,
		/// Answer best block requests with `null` while set.
		report_no_best_block: Arc<AtomicBool>,
	}

	impl Default for MockChainHeads {
		fn default() -> Self {
			const INITIAL_BEST_BLOCK_NUMBER: u64 = 7;
			const INITIAL_FINALIZED_BLOCK_NUMBER: u64 = 5;
			Self {
				best_block: Arc::new(Mutex::new(MockBlockId::MainBranch(
					INITIAL_BEST_BLOCK_NUMBER,
				))),
				finalized_block: MockBlockId::MainBranch(INITIAL_FINALIZED_BLOCK_NUMBER),
				block_hash_lookup_count: Arc::default(),
				fail_block_hash_lookups: Arc::default(),
				report_no_best_block: Arc::default(),
			}
		}
	}

	impl MockChainHeads {
		/// The chain imports `block` as its new best and notifies `provider`.
		async fn import_best(
			&self,
			provider: &SubxtBlockInfoProvider,
			api: &OnlineClient<SrcChainConfig>,
			block: MockBlockId,
		) {
			*self.best_block.lock().unwrap() = block;
			provider
				.update_latest(block_at(api, block).await, SubscriptionType::BestBlocks)
				.await;
		}

		/// Build the clients that talk to this mocked chain.
		async fn clients(
			&self,
		) -> (OnlineClient<SrcChainConfig>, LegacyRpcMethods<RpcConfigFor<SrcChainConfig>>) {
			let config = chain_config();

			let mock = MockRpcClient::builder()
				.method_handler("chain_getBlockHash", {
					let chain_heads = self.clone();
					move |params: Option<Box<serde_json::value::RawValue>>| {
						let (number,): (Option<u64>,) = decode_params(params);
						chain_heads.block_hash_lookup_count.fetch_add(1, Ordering::SeqCst);
						let response = if chain_heads.fail_block_hash_lookups.load(Ordering::SeqCst)
						{
							Err(RpcError::User(UserError {
								code: -32000,
								message: "scripted failure".into(),
								data: None,
							}))
						} else {
							match number {
								None => {
									if chain_heads.report_no_best_block.load(Ordering::SeqCst) {
										Ok(None)
									} else {
										Ok(Some(chain_heads.best_block.lock().unwrap().hash()))
									}
								},
								Some(number) => {
									let best_block = *chain_heads.best_block.lock().unwrap();
									Ok(if number > best_block.number() {
										None
									} else if number == best_block.number() {
										Some(best_block.hash())
									} else {
										Some(MockBlockId::MainBranch(number).hash())
									})
								},
							}
						};
						async move { response.map(Json) }
					}
				})
				.method_handler("chain_getFinalizedHead", {
					let finalized_hash = self.finalized_block.hash();
					move |_params: Option<Box<serde_json::value::RawValue>>| async move {
						Json(finalized_hash)
					}
				})
				.method_handler(
					"chain_getHeader",
					|params: Option<Box<serde_json::value::RawValue>>| {
						let (hash,): (H256,) = decode_params(params);
						let header = MockBlockId::from_hash(hash).map(|block| SubstrateHeader {
							parent_hash: H256::zero(),
							number: block.number(),
							state_root: H256::zero(),
							extrinsics_root: H256::zero(),
							digest: Default::default(),
						});
						async move { Json(header) }
					},
				)
				.build();

			let rpc_client = RpcClient::new(mock);
			let backend = LegacyBackend::<SrcChainConfig>::builder().build(rpc_client.clone());
			let api =
				OnlineClient::<SrcChainConfig>::from_backend_with_config(config, Arc::new(backend))
					.await
					.unwrap();
			let rpc = LegacyRpcMethods::<RpcConfigFor<SrcChainConfig>>::new(rpc_client);
			(api, rpc)
		}

		/// Build a `SubxtBlockInfoProvider` backed by this mocked chain, and the client
		/// used to construct the blocks the tests pass to `update_latest`.
		async fn provider(&self) -> (SubxtBlockInfoProvider, OnlineClient<SrcChainConfig>) {
			let (api, rpc) = self.clients().await;
			let provider = SubxtBlockInfoProvider::new(api.clone(), rpc).await.unwrap();
			// Setup queries end here: tests count only their own block hash lookups.
			self.block_hash_lookup_count.store(0, Ordering::SeqCst);
			(provider, api)
		}
	}

	#[tokio::test]
	async fn construction_seeds_the_latest_and_finalized_blocks() {
		let chain_heads = MockChainHeads::default();
		let (provider, _api) = chain_heads.provider().await;

		assert_eq!(
			provider.latest_block().await.block_hash(),
			chain_heads.best_block.lock().unwrap().hash(),
			"the latest block starts at the chain's best block"
		);
		assert_eq!(
			provider.latest_finalized_block().await.block_hash(),
			chain_heads.finalized_block.hash(),
			"the latest finalized block starts at the chain's finalized block"
		);
	}

	#[tokio::test]
	async fn construction_fails_without_a_best_block() {
		let chain_heads = MockChainHeads::default();
		let (api, rpc) = chain_heads.clients().await;

		chain_heads.report_no_best_block.store(true, Ordering::SeqCst);
		assert!(
			matches!(SubxtBlockInfoProvider::new(api, rpc).await, Err(ClientError::BlockNotFound)),
			"construction fails when the chain reports no best block"
		);
	}

	#[tokio::test]
	async fn best_block_updates_follow_the_chain_head() {
		let chain_heads = MockChainHeads::default();
		let (provider, api) = chain_heads.provider().await;
		let best = chain_heads.best_block.lock().unwrap().number();

		chain_heads
			.import_best(&provider, &api, MockBlockId::MainBranch(best + 1))
			.await;
		assert_eq!(
			provider.latest_block().await.block_hash(),
			MockBlockId::MainBranch(best + 1).hash(),
			"a higher block becomes the latest block"
		);
		assert_eq!(
			chain_heads.block_hash_lookup_count.load(Ordering::SeqCst),
			0,
			"no block hash lookup for a higher block"
		);

		chain_heads
			.import_best(&provider, &api, MockBlockId::SideBranch(best + 1))
			.await;
		assert_eq!(
			provider.latest_block().await.block_hash(),
			MockBlockId::SideBranch(best + 1).hash(),
			"a same-number block from a side branch replaces the latest block"
		);
		assert_eq!(
			chain_heads.block_hash_lookup_count.load(Ordering::SeqCst),
			1,
			"one block hash lookup to accept a same-number block from another branch"
		);

		chain_heads
			.import_best(&provider, &api, MockBlockId::SideBranch(best + 2))
			.await;
		assert_eq!(
			provider.latest_block().await.block_hash(),
			MockBlockId::SideBranch(best + 2).hash(),
			"a higher side-branch block becomes the latest block"
		);
		assert_eq!(
			chain_heads.block_hash_lookup_count.load(Ordering::SeqCst),
			1,
			"no block hash lookup for a higher side-branch block"
		);

		let latest = provider.latest_block().await;
		provider
			.update_latest(
				block_at(&api, MockBlockId::SideBranch(best + 2)).await,
				SubscriptionType::BestBlocks,
			)
			.await;
		assert!(
			!Arc::ptr_eq(&latest, &provider.latest_block().await),
			"a repeat of the latest block replaces the cached one"
		);
		assert_eq!(
			chain_heads.block_hash_lookup_count.load(Ordering::SeqCst),
			1,
			"no block hash lookup for a repeat of the latest block"
		);
	}

	#[tokio::test]
	async fn reorgs_are_followed_and_replays_are_ignored() {
		let chain_heads = MockChainHeads::default();
		let (provider, api) = chain_heads.provider().await;
		let best = chain_heads.best_block.lock().unwrap().number();
		let finalized = chain_heads.finalized_block.number();

		chain_heads
			.import_best(&provider, &api, MockBlockId::SideBranch(best - 1))
			.await;
		assert_eq!(
			provider.latest_block().await.block_hash(),
			MockBlockId::SideBranch(best - 1).hash(),
			"a lower block is accepted when the chain ends below the stored latest block"
		);
		assert_eq!(
			chain_heads.block_hash_lookup_count.load(Ordering::SeqCst),
			1,
			"one block hash lookup for the accepted lower block"
		);

		// The chain's best block moves back to the main branch without a notification.
		*chain_heads.best_block.lock().unwrap() = MockBlockId::MainBranch(best);

		provider
			.update_latest(
				block_at(&api, MockBlockId::MainBranch(finalized)).await,
				SubscriptionType::BestBlocks,
			)
			.await;
		assert_eq!(
			provider.latest_block().await.block_hash(),
			MockBlockId::MainBranch(finalized).hash(),
			"an old block is accepted when the chain no longer lists the stored latest block"
		);
		assert_eq!(
			chain_heads.block_hash_lookup_count.load(Ordering::SeqCst),
			2,
			"one block hash lookup for each accepted lower block"
		);

		// The chain's next notification carries its best block.
		chain_heads.import_best(&provider, &api, MockBlockId::MainBranch(best)).await;
		assert_eq!(
			provider.latest_block().await.block_hash(),
			MockBlockId::MainBranch(best).hash(),
			"the chain's best block becomes the latest block"
		);
		assert_eq!(
			chain_heads.block_hash_lookup_count.load(Ordering::SeqCst),
			2,
			"no block hash lookup for the chain's best block"
		);

		provider
			.update_latest(
				block_at(&api, MockBlockId::MainBranch(best - 1)).await,
				SubscriptionType::BestBlocks,
			)
			.await;
		assert_eq!(
			provider.latest_block().await.block_hash(),
			MockBlockId::MainBranch(best).hash(),
			"an old block is ignored while the chain still lists the stored latest block"
		);
		assert_eq!(
			chain_heads.block_hash_lookup_count.load(Ordering::SeqCst),
			3,
			"one more block hash lookup to ignore an old block"
		);

		provider
			.update_latest(
				block_at(&api, MockBlockId::SideBranch(finalized - 1)).await,
				SubscriptionType::BestBlocks,
			)
			.await;
		assert_eq!(
			provider.latest_block().await.block_hash(),
			MockBlockId::MainBranch(best).hash(),
			"a block below the finalized block is ignored"
		);
		assert_eq!(
			chain_heads.block_hash_lookup_count.load(Ordering::SeqCst),
			3,
			"no block hash lookup for a block below the finalized block"
		);

		provider
			.update_latest(
				block_at(&api, MockBlockId::SideBranch(finalized)).await,
				SubscriptionType::BestBlocks,
			)
			.await;
		assert_eq!(
			provider.latest_block().await.block_hash(),
			MockBlockId::MainBranch(best).hash(),
			"a block from another branch at the finalized block's number is ignored"
		);
		assert_eq!(
			chain_heads.block_hash_lookup_count.load(Ordering::SeqCst),
			3,
			"no block hash lookup for a block conflicting with the finalized block"
		);

		provider
			.update_latest(
				block_at(&api, MockBlockId::SideBranch(best)).await,
				SubscriptionType::BestBlocks,
			)
			.await;
		assert_eq!(
			provider.latest_block().await.block_hash(),
			MockBlockId::MainBranch(best).hash(),
			"a same-number block from another branch is ignored while the chain still lists the \
			 stored latest block"
		);
		assert_eq!(
			chain_heads.block_hash_lookup_count.load(Ordering::SeqCst),
			4,
			"one more block hash lookup to ignore a same-number block"
		);
	}

	#[tokio::test]
	async fn failed_block_hash_lookups_keep_the_stored_best_block() {
		let chain_heads = MockChainHeads::default();
		let (provider, api) = chain_heads.provider().await;
		let best = chain_heads.best_block.lock().unwrap().number();
		let finalized = chain_heads.finalized_block.number();

		chain_heads.fail_block_hash_lookups.store(true, Ordering::SeqCst);
		provider
			.update_latest(
				block_at(&api, MockBlockId::MainBranch(finalized)).await,
				SubscriptionType::BestBlocks,
			)
			.await;
		assert_eq!(
			provider.latest_block().await.block_hash(),
			MockBlockId::MainBranch(best).hash(),
			"an old block is ignored while the block hash lookup fails"
		);
		assert_eq!(
			chain_heads.block_hash_lookup_count.load(Ordering::SeqCst),
			1,
			"one block hash lookup for the ignored old block"
		);

		chain_heads.fail_block_hash_lookups.store(false, Ordering::SeqCst);
		chain_heads
			.import_best(&provider, &api, MockBlockId::SideBranch(best - 1))
			.await;
		assert_eq!(
			provider.latest_block().await.block_hash(),
			MockBlockId::SideBranch(best - 1).hash(),
			"a lower block is accepted once the block hash lookup succeeds"
		);
		assert_eq!(
			chain_heads.block_hash_lookup_count.load(Ordering::SeqCst),
			2,
			"one more block hash lookup for the accepted lower block"
		);
	}

	#[tokio::test]
	async fn finalized_blocks_never_move_backwards() {
		let chain_heads = MockChainHeads::default();
		let (provider, api) = chain_heads.provider().await;
		let best = chain_heads.best_block.lock().unwrap().number();
		let finalized = chain_heads.finalized_block.number();

		provider
			.update_latest(
				block_at(&api, MockBlockId::MainBranch(finalized - 1)).await,
				SubscriptionType::FinalizedBlocks,
			)
			.await;
		assert_eq!(
			provider.latest_finalized_block().await.block_hash(),
			MockBlockId::MainBranch(finalized).hash(),
			"a lower finalized block is ignored"
		);

		provider
			.update_latest(
				block_at(&api, MockBlockId::MainBranch(finalized + 1)).await,
				SubscriptionType::FinalizedBlocks,
			)
			.await;
		assert_eq!(
			provider.latest_finalized_block().await.block_hash(),
			MockBlockId::MainBranch(finalized + 1).hash(),
			"a higher finalized block becomes the latest finalized block"
		);
		assert_eq!(
			provider.latest_block().await.block_hash(),
			MockBlockId::MainBranch(best).hash(),
			"the latest block stays put while it is ahead of the finalized block"
		);

		provider
			.update_latest(
				block_at(&api, MockBlockId::MainBranch(best + 1)).await,
				SubscriptionType::FinalizedBlocks,
			)
			.await;
		assert_eq!(
			provider.latest_finalized_block().await.block_hash(),
			MockBlockId::MainBranch(best + 1).hash(),
			"a higher finalized block becomes the latest finalized block"
		);
		assert_eq!(
			provider.latest_block().await.block_hash(),
			MockBlockId::MainBranch(best + 1).hash(),
			"the latest block follows a finalized block ahead of it"
		);

		chain_heads
			.import_best(&provider, &api, MockBlockId::SideBranch(best + 2))
			.await;
		provider
			.update_latest(
				block_at(&api, MockBlockId::MainBranch(best + 2)).await,
				SubscriptionType::FinalizedBlocks,
			)
			.await;
		assert_eq!(
			provider.latest_block().await.block_hash(),
			MockBlockId::MainBranch(best + 2).hash(),
			"a finalized block replaces a same-numbered latest block from another branch"
		);

		let latest = provider.latest_block().await;
		provider
			.update_latest(
				block_at(&api, MockBlockId::MainBranch(best + 2)).await,
				SubscriptionType::FinalizedBlocks,
			)
			.await;
		assert!(
			Arc::ptr_eq(&latest, &provider.latest_block().await),
			"the latest block is unchanged when is already the finalized block"
		);
	}

	#[tokio::test]
	async fn stale_finalized_blocks_still_pull_up_the_latest_block() {
		let inverted_best_block_number = MockChainHeads::default().finalized_block.number() - 2;
		let chain_heads = MockChainHeads {
			best_block: Arc::new(Mutex::new(MockBlockId::MainBranch(inverted_best_block_number))),
			..MockChainHeads::default()
		};
		let (provider, api) = chain_heads.provider().await;
		let finalized = chain_heads.finalized_block.number();

		assert!(
			provider.latest_finalized_block().await.block_number() >
				provider.latest_block().await.block_number(),
			"the latest block starts below the latest finalized block"
		);

		provider
			.update_latest(
				block_at(&api, MockBlockId::MainBranch(finalized - 1)).await,
				SubscriptionType::FinalizedBlocks,
			)
			.await;
		assert_eq!(
			provider.latest_finalized_block().await.block_hash(),
			MockBlockId::MainBranch(finalized).hash(),
			"a lower finalized block is ignored"
		);
		assert_eq!(
			provider.latest_block().await.block_hash(),
			MockBlockId::MainBranch(finalized).hash(),
			"the latest block is pulled up to the latest finalized block, not to the stale one"
		);
	}

	#[tokio::test]
	async fn block_by_number_uses_the_cache_before_querying_the_chain() {
		let chain_heads = MockChainHeads::default();
		let (provider, _api) = chain_heads.provider().await;
		let best = chain_heads.best_block.lock().unwrap().number();
		let finalized = chain_heads.finalized_block.number();

		let block = provider.block_by_number(best).await.unwrap().unwrap();
		assert_eq!(
			block.block_hash(),
			MockBlockId::MainBranch(best).hash(),
			"the latest block is returned for its number"
		);
		let block = provider.block_by_number(finalized).await.unwrap().unwrap();
		assert_eq!(
			block.block_hash(),
			MockBlockId::MainBranch(finalized).hash(),
			"the latest finalized block is returned for its number"
		);
		assert_eq!(
			chain_heads.block_hash_lookup_count.load(Ordering::SeqCst),
			0,
			"no block hash lookup for the cached blocks"
		);

		let block = provider.block_by_number(best - 1).await.unwrap().unwrap();
		assert_eq!(
			block.block_hash(),
			MockBlockId::MainBranch(best - 1).hash(),
			"an uncached block is fetched from the chain"
		);
		assert_eq!(
			chain_heads.block_hash_lookup_count.load(Ordering::SeqCst),
			1,
			"one block hash lookup for an uncached block number"
		);

		assert!(
			provider.block_by_number(best + 1).await.unwrap().is_none(),
			"no block exists above the chain's best block"
		);
		assert_eq!(
			chain_heads.block_hash_lookup_count.load(Ordering::SeqCst),
			2,
			"one more block hash lookup for a number above the chain's best block"
		);

		chain_heads.fail_block_hash_lookups.store(true, Ordering::SeqCst);
		assert!(
			provider.block_by_number(best - 1).await.is_err(),
			"a block hash lookup failure is returned as an error"
		);
	}

	#[tokio::test]
	async fn block_by_hash_uses_the_cache_before_querying_the_chain() {
		let chain_heads = MockChainHeads::default();
		let (provider, _api) = chain_heads.provider().await;
		let best = chain_heads.best_block.lock().unwrap().number();
		let finalized = chain_heads.finalized_block.number();

		let block = provider
			.block_by_hash(&MockBlockId::MainBranch(best).hash())
			.await
			.unwrap()
			.unwrap();
		assert_eq!(block.block_number(), best, "the latest block is returned for its hash");

		let block = provider
			.block_by_hash(&MockBlockId::MainBranch(finalized).hash())
			.await
			.unwrap()
			.unwrap();
		assert_eq!(
			block.block_number(),
			finalized,
			"the latest finalized block is returned for its hash"
		);

		let block = provider
			.block_by_hash(&MockBlockId::SideBranch(best).hash())
			.await
			.unwrap()
			.unwrap();
		assert_eq!(
			block.block_hash(),
			MockBlockId::SideBranch(best).hash(),
			"an uncached block is fetched from the chain"
		);
		assert_eq!(block.block_number(), best, "the fetched block's number comes from its header");

		assert!(
			provider.block_by_hash(&H256::zero()).await.unwrap().is_none(),
			"no block exists for an unknown hash"
		);

		assert_eq!(
			chain_heads.block_hash_lookup_count.load(Ordering::SeqCst),
			0,
			"fetching by hash never asks the chain for a block hash"
		);
	}
}
