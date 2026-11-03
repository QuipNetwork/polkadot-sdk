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

//! Substrate blockchain trait

use codec::{Decode, Encode};
use parking_lot::RwLock;
use sp_core::H256;
use sp_runtime::{
	generic::BlockId,
	traits::{Block as BlockT, Header as HeaderT, NumberFor, Zero},
	Justifications,
};
use std::collections::{btree_set::BTreeSet, HashMap, VecDeque};
use tracing::{debug, warn};

use crate::{
	error::{Error, Result},
	header_metadata::HeaderMetadata,
	tree_route, CachedHeaderMetadata,
};

/// Blockchain database header backend. Does not perform any validation.
pub trait HeaderBackend<Block: BlockT>: Send + Sync {
	/// Get block header. Returns `None` if block is not found.
	fn header(&self, hash: Block::Hash) -> Result<Option<Block::Header>>;
	/// Get blockchain info.
	fn info(&self) -> Info<Block>;
	/// Get block status.
	fn status(&self, hash: Block::Hash) -> Result<BlockStatus>;
	/// Get block number by hash. Returns `None` if the header is not in the chain.
	fn number(
		&self,
		hash: Block::Hash,
	) -> Result<Option<<<Block as BlockT>::Header as HeaderT>::Number>>;
	/// Get block hash by number. Returns `None` if the header is not in the chain.
	fn hash(&self, number: NumberFor<Block>) -> Result<Option<Block::Hash>>;

	/// Convert an arbitrary block ID into a block hash.
	fn block_hash_from_id(&self, id: &BlockId<Block>) -> Result<Option<Block::Hash>> {
		match *id {
			BlockId::Hash(h) => Ok(Some(h)),
			BlockId::Number(n) => self.hash(n),
		}
	}

	/// Convert an arbitrary block ID into a block hash.
	fn block_number_from_id(&self, id: &BlockId<Block>) -> Result<Option<NumberFor<Block>>> {
		match *id {
			BlockId::Hash(h) => self.number(h),
			BlockId::Number(n) => Ok(Some(n)),
		}
	}

	/// Get block header. Returns `UnknownBlock` error if block is not found.
	fn expect_header(&self, hash: Block::Hash) -> Result<Block::Header> {
		self.header(hash)?
			.ok_or_else(|| Error::UnknownBlock(format!("Expect header: {}", hash)))
	}

	/// Convert an arbitrary block ID into a block number. Returns `UnknownBlock` error if block is
	/// not found.
	fn expect_block_number_from_id(&self, id: &BlockId<Block>) -> Result<NumberFor<Block>> {
		self.block_number_from_id(id).and_then(|n| {
			n.ok_or_else(|| Error::UnknownBlock(format!("Expect block number from id: {}", id)))
		})
	}

	/// Convert an arbitrary block ID into a block hash. Returns `UnknownBlock` error if block is
	/// not found.
	fn expect_block_hash_from_id(&self, id: &BlockId<Block>) -> Result<Block::Hash> {
		self.block_hash_from_id(id).and_then(|h| {
			h.ok_or_else(|| Error::UnknownBlock(format!("Expect block hash from id: {}", id)))
		})
	}
}

/// Handles stale forks.
pub trait ForkBackend<Block: BlockT>:
	HeaderMetadata<Block> + HeaderBackend<Block> + Send + Sync
{
	/// Returns block hashes for provided fork heads. It skips the fork if when blocks are missing
	/// (e.g. warp-sync) and internal `tree_route` function fails.
	///
	/// Example:
	///  G --- A1 --- A2 --- A3 --- A4           ( < fork1 )
	///                       \-----C4 --- C5    ( < fork2 )
	/// We finalize A3 and call expand_fork(C5). Result = (C5,C4).
	fn expand_forks(
		&self,
		fork_heads: &[Block::Hash],
	) -> std::result::Result<BTreeSet<Block::Hash>, Error> {
		let mut expanded_forks = BTreeSet::new();
		for fork_head in fork_heads {
			match tree_route(self, *fork_head, self.info().finalized_hash) {
				Ok(tree_route) => {
					for block in tree_route.retracted() {
						expanded_forks.insert(block.hash);
					}
					continue;
				},
				Err(_) => {
					// There are cases when blocks are missing (e.g. warp-sync).
				},
			}
		}

		Ok(expanded_forks)
	}
}

impl<Block, T> ForkBackend<Block> for T
where
	Block: BlockT,
	T: HeaderMetadata<Block> + HeaderBackend<Block> + Send + Sync,
{
}

struct MinimalBlockMetadata<Block: BlockT> {
	number: NumberFor<Block>,
	hash: Block::Hash,
	parent: Block::Hash,
}

impl<Block> Clone for MinimalBlockMetadata<Block>
where
	Block: BlockT,
{
	fn clone(&self) -> Self {
		Self { number: self.number, hash: self.hash, parent: self.parent }
	}
}

impl<Block> Copy for MinimalBlockMetadata<Block> where Block: BlockT {}

impl<Block> From<&CachedHeaderMetadata<Block>> for MinimalBlockMetadata<Block>
where
	Block: BlockT,
{
	fn from(value: &CachedHeaderMetadata<Block>) -> Self {
		Self { number: value.number, hash: value.hash, parent: value.parent }
	}
}

/// Blockchain database backend. Does not perform any validation.
pub trait Backend<Block: BlockT>:
	HeaderBackend<Block> + HeaderMetadata<Block, Error = Error>
{
	/// Get block body. Returns `None` if block is not found.
	fn body(&self, hash: Block::Hash) -> Result<Option<Vec<<Block as BlockT>::Extrinsic>>>;
	/// Get block justifications. Returns `None` if no justification exists.
	fn justifications(&self, hash: Block::Hash) -> Result<Option<Justifications>>;
	/// Get last finalized block hash.
	fn last_finalized(&self) -> Result<Block::Hash>;

	/// Returns hashes of all blocks that are leaves of the block tree.
	/// in other words, that have no children, are chain heads.
	/// Results must be ordered best (longest, highest) chain first.
	fn leaves(&self) -> Result<Vec<Block::Hash>>;

	/// Return hashes of all blocks that are children of the block with `parent_hash`.
	fn children(&self, parent_hash: Block::Hash) -> Result<Vec<Block::Hash>>;

	/// Get the most recent block hash of the longest chain that contains
	/// a block with the given `base_hash`.
	///
	/// The search space is always limited to blocks which are in the finalized
	/// chain or descendants of it.
	///
	/// Returns `Ok(None)` if `base_hash` is not found in search space.
	// TODO: document time complexity of this, see [#1444](https://github.com/paritytech/substrate/issues/1444)
	fn longest_containing(
		&self,
		base_hash: Block::Hash,
		import_lock: &RwLock<()>,
	) -> Result<Option<Block::Hash>> {
		let Some(base_header) = self.header(base_hash)? else { return Ok(None) };

		let leaves = {
			// ensure no blocks are imported during this code block.
			// an import could trigger a reorg which could change the canonical chain.
			// we depend on the canonical chain staying the same during this code block.
			let _import_guard = import_lock.read();
			let info = self.info();
			if info.finalized_number > *base_header.number() {
				// `base_header` is on a dead fork.
				return Ok(None);
			}
			self.leaves()?
		};

		// for each chain. longest chain first. shortest last
		for leaf_hash in leaves {
			let mut current_hash = leaf_hash;
			// go backwards through the chain (via parent links)
			loop {
				if current_hash == base_hash {
					return Ok(Some(leaf_hash));
				}

				let current_header = self
					.header(current_hash)?
					.ok_or_else(|| Error::MissingHeader(current_hash.to_string()))?;

				// stop search in this chain once we go below the target's block number
				if current_header.number() < base_header.number() {
					break;
				}

				current_hash = *current_header.parent_hash();
			}
		}

		// header may be on a dead fork -- the only leaves that are considered are
		// those which can still be finalized.
		//
		// FIXME #1558 only issue this warning when not on a dead fork
		warn!(
			target: crate::LOG_TARGET,
			"Block {:?} exists in chain but not found when following all leaves backwards",
			base_hash,
		);

		Ok(None)
	}

	/// Get single indexed transaction by content hash (BLAKE2b-256).
	/// Note that this will only fetch transactions that are indexed
	/// by the runtime with `storage_index_transaction`.
	fn indexed_transaction(&self, hash: H256) -> Result<Option<Vec<u8>>>;

	/// Check if indexed transaction exists given its BLAKE2b-256 hash.
	fn has_indexed_transaction(&self, hash: H256) -> Result<bool> {
		Ok(self.indexed_transaction(hash)?.is_some())
	}

	/// Get the BLAKE2b-256 hashes of all indexed transactions in a block, including renewed
	/// transactions.
	///
	/// Note that this will only fetch transactions that are indexed by the runtime with
	/// `storage_index_transaction`.
	fn block_indexed_hashes(&self, hash: Block::Hash) -> Result<Option<Vec<H256>>>;

	/// Get all indexed transactions for a block, including renewed transactions.
	///
	/// Note that this will only fetch transactions that are indexed by the runtime with
	/// `storage_index_transaction`.
	fn block_indexed_body(&self, hash: Block::Hash) -> Result<Option<Vec<Vec<u8>>>>;

	/// Returns all leaves that will be displaced after the block finalization.
	fn displaced_leaves_after_finalizing(
		&self,
		finalized_block_hash: Block::Hash,
		finalized_block_number: NumberFor<Block>,
		finalized_block_parent_hash: Block::Hash,
	) -> std::result::Result<DisplacedLeavesAfterFinalization<Block>, Error> {
		// There are no forks at genesis.
		if finalized_block_number.is_zero() {
			return Ok(DisplacedLeavesAfterFinalization::default());
		}

		let leaves = self.leaves()?;

		let now = std::time::Instant::now();
		debug!(
			target: crate::LOG_TARGET,
			?leaves,
			?finalized_block_hash,
			?finalized_block_number,
			"Checking for displaced leaves after finalization."
		);

		// Store hashes of finalized blocks for quick checking later, the last block is the
		// finalized one
		let mut finalized_chain = VecDeque::new();
		finalized_chain.push_front(MinimalBlockMetadata {
			number: finalized_block_number,
			hash: finalized_block_hash,
			parent: finalized_block_parent_hash,
		});

		// Local cache is a performance optimization in case of finalized block deep below the
		// tip of the chain with a lot of leaves above finalized block
		let mut local_cache = HashMap::<Block::Hash, MinimalBlockMetadata<Block>>::new();

		let mut result = DisplacedLeavesAfterFinalization {
			displaced_leaves: Vec::with_capacity(leaves.len()),
			displaced_blocks: Vec::with_capacity(leaves.len()),
		};

		let mut displaced_blocks_candidates = Vec::new();

		let genesis_hash = self.info().genesis_hash;

		for leaf_hash in leaves {
			let mut current_header_metadata =
				MinimalBlockMetadata::from(&self.header_metadata(leaf_hash).map_err(|err| {
					debug!(
						target: crate::LOG_TARGET,
						?leaf_hash,
						?err,
						elapsed = ?now.elapsed(),
						"Failed to fetch leaf header.",
					);
					err
				})?);
			let leaf_number = current_header_metadata.number;

			// The genesis block is part of the canonical chain.
			if leaf_hash == genesis_hash {
				result.displaced_leaves.push((leaf_number, leaf_hash));
				debug!(
					target: crate::LOG_TARGET,
					?leaf_hash,
					elapsed = ?now.elapsed(),
					"Added genesis leaf to displaced leaves."
				);
				continue;
			}

			debug!(
				target: crate::LOG_TARGET,
				?leaf_number,
				?leaf_hash,
				elapsed = ?now.elapsed(),
				"Handle displaced leaf.",
			);

			// Collect all block hashes until the height of the finalized block
			displaced_blocks_candidates.clear();
			while current_header_metadata.number > finalized_block_number {
				displaced_blocks_candidates.push(current_header_metadata.hash);

				let parent_hash = current_header_metadata.parent;
				match local_cache.get(&parent_hash) {
					Some(metadata_header) => {
						current_header_metadata = *metadata_header;
					},
					None => {
						current_header_metadata = MinimalBlockMetadata::from(
							&self.header_metadata(parent_hash).map_err(|err| {
								debug!(
									target: crate::LOG_TARGET,
									?err,
									?parent_hash,
									?leaf_hash,
									elapsed = ?now.elapsed(),
									"Failed to fetch parent header during leaf tracking.",
								);

								err
							})?,
						);
						// Cache locally in case more branches above finalized block reference
						// the same block hash
						local_cache.insert(parent_hash, current_header_metadata);
					},
				}
			}

			// If points back to the finalized header then nothing left to do, this leaf will be
			// checked again later
			if current_header_metadata.hash == finalized_block_hash {
				debug!(
					target: crate::LOG_TARGET,
					?leaf_hash,
					elapsed = ?now.elapsed(),
					"Leaf points to the finalized header, skipping for now.",
				);

				continue;
			}

			// We reuse `displaced_blocks_candidates` to store the current metadata.
			// This block is not displaced if there is a gap in the ancestry. We
			// check for this gap later.
			displaced_blocks_candidates.push(current_header_metadata.hash);

			debug!(
				target: crate::LOG_TARGET,
				current_hash = ?current_header_metadata.hash,
				current_num = ?current_header_metadata.number,
				?finalized_block_number,
				elapsed = ?now.elapsed(),
				"Looking for path from finalized block number to current leaf number"
			);

			// Collect the rest of the displaced blocks of leaf branch
			for distance_from_finalized in 1_u32.. {
				// Find block at `distance_from_finalized` from finalized block
				let (finalized_chain_block_number, finalized_chain_block_hash) =
					match finalized_chain.iter().rev().nth(distance_from_finalized as usize) {
						Some(header) => (header.number, header.hash),
						None => {
							let to_fetch = finalized_chain.front().expect("Not empty; qed");
							let metadata = match self.header_metadata(to_fetch.parent) {
								Ok(metadata) => metadata,
								Err(Error::UnknownBlock(_)) => {
									debug!(
										target: crate::LOG_TARGET,
										distance_from_finalized,
										hash = ?to_fetch.parent,
										number = ?to_fetch.number,
										elapsed = ?now.elapsed(),
										"Tried to fetch unknown block, block ancestry has gaps."
									);
									break;
								},
								Err(err) => {
									debug!(
										target: crate::LOG_TARGET,
										hash = ?to_fetch.parent,
										number = ?to_fetch.number,
										?err,
										elapsed = ?now.elapsed(),
										"Failed to fetch header for parent hash.",
									);
									return Err(err);
								},
							};
							let metadata = MinimalBlockMetadata::from(&metadata);
							let result = (metadata.number, metadata.hash);
							finalized_chain.push_front(metadata);
							result
						},
					};

				if current_header_metadata.hash == finalized_chain_block_hash {
					// Found the block on the finalized chain, nothing left to do
					result.displaced_leaves.push((leaf_number, leaf_hash));

					debug!(
						target: crate::LOG_TARGET,
						?leaf_hash,
						elapsed = ?now.elapsed(),
						"Leaf is ancestor of finalized block."
					);
					break;
				}

				if current_header_metadata.number <= finalized_chain_block_number {
					// Skip more blocks until we get all blocks on finalized chain until the height
					// of the parent block
					continue;
				}

				let parent_hash = current_header_metadata.parent;
				if finalized_chain_block_hash == parent_hash {
					// Reached finalized chain, nothing left to do
					result.displaced_blocks.extend(displaced_blocks_candidates.drain(..));
					result.displaced_leaves.push((leaf_number, leaf_hash));

					debug!(
						target: crate::LOG_TARGET,
						?leaf_hash,
						elapsed = ?now.elapsed(),
						"Found displaced leaf."
					);
					break;
				}

				// Store displaced block and look deeper for block on finalized chain
				debug!(
					target: crate::LOG_TARGET,
					?parent_hash,
					elapsed = ?now.elapsed(),
					"Found displaced block. Looking further.",
				);
				displaced_blocks_candidates.push(parent_hash);
				current_header_metadata = MinimalBlockMetadata::from(
					&self.header_metadata(parent_hash).map_err(|err| {
						debug!(
							target: crate::LOG_TARGET,
							?err,
							?parent_hash,
							elapsed = ?now.elapsed(),
							"Failed to fetch header for parent during displaced block collection",
						);
						err
					})?,
				);
			}
		}

		// There could be duplicates shared by multiple branches, clean them up
		result.displaced_blocks.sort_unstable();
		result.displaced_blocks.dedup();

		debug!(
			target: crate::LOG_TARGET,
			%finalized_block_hash,
			?finalized_block_number,
			?result,
			elapsed = ?now.elapsed(),
			"Finished checking for displaced leaves after finalization.",
		);

		return Ok(result);
	}
}

/// Result of  [`Backend::displaced_leaves_after_finalizing`].
#[derive(Clone, Debug)]
pub struct DisplacedLeavesAfterFinalization<Block: BlockT> {
	/// A list of hashes and block numbers of displaced leaves.
	pub displaced_leaves: Vec<(NumberFor<Block>, Block::Hash)>,

	/// A list of hashes displaced blocks from all displaced leaves.
	pub displaced_blocks: Vec<Block::Hash>,
}

impl<Block: BlockT> Default for DisplacedLeavesAfterFinalization<Block> {
	fn default() -> Self {
		Self { displaced_leaves: Vec::new(), displaced_blocks: Vec::new() }
	}
}

impl<Block: BlockT> DisplacedLeavesAfterFinalization<Block> {
	/// Returns a collection of hashes for the displaced leaves.
	pub fn hashes(&self) -> impl Iterator<Item = Block::Hash> + '_ {
		self.displaced_leaves.iter().map(|(_, hash)| *hash)
	}
}

/// Represents the type of block gaps that may result from either warp sync or fast sync.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Encode, Decode)]
pub enum BlockGapType {
	/// Both the header and body are missing, as a result of warp sync.
	MissingHeaderAndBody,
	/// The block body is missing, as a result of fast sync.
	MissingBody,
}

/// Represents the block gap resulted by warp sync or fast sync.
///
/// A block gap is a range of blocks where either the bodies, or both headers and bodies are
/// missing.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Encode, Decode)]
pub struct BlockGap<N> {
	/// The starting block number of the gap (inclusive).
	pub start: N,
	/// The ending block number of the gap (inclusive).
	pub end: N,
	/// The type of gap.
	pub gap_type: BlockGapType,
}

/// Blockchain info
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct Info<Block: BlockT> {
	/// Best block hash.
	pub best_hash: Block::Hash,
	/// Best block number.
	pub best_number: <<Block as BlockT>::Header as HeaderT>::Number,
	/// Genesis block hash.
	pub genesis_hash: Block::Hash,
	/// The head of the finalized chain.
	pub finalized_hash: Block::Hash,
	/// Last finalized block number.
	pub finalized_number: <<Block as BlockT>::Header as HeaderT>::Number,
	/// Last finalized state.
	pub finalized_state: Option<(Block::Hash, <<Block as BlockT>::Header as HeaderT>::Number)>,
	/// Number of concurrent leave forks.
	pub number_leaves: usize,
	/// Missing blocks after warp sync or fast sync.
	pub block_gap: Option<BlockGap<NumberFor<Block>>>,
}

/// Block status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockStatus {
	/// Already in the blockchain.
	InChain,
	/// Not in the queue or the blockchain.
	Unknown,
}
