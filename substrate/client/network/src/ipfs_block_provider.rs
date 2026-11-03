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

use core::marker::PhantomData;
use futures::{stream::FusedStream, Stream, StreamExt};
use log::{debug, warn};
use sc_client_api::{BlockBackend, BlockchainEvents, FinalityNotifications};
use sp_core::H256;
use sp_runtime::traits::{BlakeTwo256, Block, Hash, Header, NumberFor, Saturating, Zero};
use std::{
	collections::{hash_map::Entry, HashMap, VecDeque},
	pin::Pin,
	sync::Arc,
	task::{Context, Poll},
};

/// Log target for this file.
const LOG_TARGET: &str = "sub-libp2p::ipfs";

/// Multihash wiith allocated size of 32 bytes for hash value.
type Multihash = cid::multihash::Multihash<32>;

/// A change to the blocks available from a [`BlockProvider`].
pub enum Change {
	/// The block with the given hash is now available.
	Added(Multihash),
	/// The block with the given hash is no longer available.
	Removed(Multihash),
}

/// Provides blocks to be served over IPFS. Requires `Send` and `Sync` so we can write `Arc<dyn
/// BlockProvider>` instead of `Arc<dyn BlockProvider + Send + Sync>`.
pub trait BlockProvider: Send + Sync {
	/// Returns `true` if we have the block with the given hash.
	fn have(&self, multihash: &Multihash) -> bool;

	/// Returns the block with the given hash if possible, otherwise returns `None`.
	fn get(&self, multihash: &Multihash) -> Option<Vec<u8>>;

	/// Returns a stream of changes to the available blocks. All blocks which are available at the
	/// point of calling will be reported as additions. Note that the stream may not be perfectly
	/// synchronised with [`have`](Self::have)/[`get`](Self::get).
	fn changes(&self) -> Pin<Box<dyn Stream<Item = Change> + Send>>;
}

/// Implemented for hasher types such as [`BlakeTwo256`], providing the corresponding Multihash
/// code.
trait HasMultihashCode {
	/// The Multihash code for the hasher.
	const MULTIHASH_CODE: u64;
}

impl HasMultihashCode for BlakeTwo256 {
	const MULTIHASH_CODE: u64 = 0xb220;
}

fn try_from_multihash<H: Hash + HasMultihashCode>(multihash: &Multihash) -> Option<H::Output> {
	if multihash.code() != H::MULTIHASH_CODE {
		return None;
	}
	let mut hash = H::Output::default();
	let src = multihash.digest();
	let dst = hash.as_mut();
	if src.len() != dst.len() {
		return None;
	}
	dst.copy_from_slice(src);
	Some(hash)
}

fn to_multihash<H: Hash + HasMultihashCode>(hash: &H::Output) -> Multihash {
	Multihash::wrap(H::MULTIHASH_CODE, hash.as_ref()).expect("Hash size is fixed and small enough")
}

/// A block containing indexed transactions.
struct IndexedBlock<B: Block> {
	number: NumberFor<B>,
	/// BLAKE2b-256 hashes of the indexed transactions.
	// TODO: index transactions using multihash and not bare hash value, and use `Multihash`
	// here instead of `H256` interpreted as BLAKE2b-256.
	transaction_hashes: Vec<H256>,
}

struct IndexedTransactionChanges<B: Block, C> {
	client: Arc<C>,
	/// Number of finalized blocks kept by the client.
	num_blocks_kept: u32,
	finality_notifications: FinalityNotifications<B>,
	/// The number of the last finalized block, _plus one_.
	finalized_to: NumberFor<B>,
	/// Finalized blocks with indexed transactions. Old blocks are at the front, new blocks at the
	/// back. Transaction hashes and blocks are popped as they are reported as removed.
	blocks: VecDeque<IndexedBlock<B>>,
	/// The number of the last fetched block, _plus one_. Blocks are added to
	/// [`blocks`](Self::blocks) as they are fetched, but only if they contain indexed
	/// transactions.
	fetched_to: NumberFor<B>,
	/// The number of indexed transactions in the last fetched block which have been handled, or
	/// `None` if all have been handled. Additional blocks will not be fetched if this is `Some`.
	added_to: Option<usize>,
	/// The BLAKE2b-256 hashes of all available transactions are present in this map. The `u32` is
	/// the number of transactions with the hash, minus one. This is used to deduplicate
	/// added/removed reports.
	extra_refs: HashMap<H256, u32>,
}

impl<B, C> IndexedTransactionChanges<B, C>
where
	B: Block,
	C: BlockchainEvents<B>,
{
	fn new(client: Arc<C>, num_blocks_kept: u32) -> Self {
		let finality_notifications = client.finality_notification_stream();
		Self {
			client,
			num_blocks_kept,
			finality_notifications,
			finalized_to: Zero::zero(),
			blocks: VecDeque::new(),
			fetched_to: Zero::zero(),
			added_to: None,
			extra_refs: HashMap::new(),
		}
	}
}

impl<B: Block, C> Unpin for IndexedTransactionChanges<B, C> {}

impl<B, C> Stream for IndexedTransactionChanges<B, C>
where
	B: Block,
	C: BlockBackend<B>,
{
	type Item = Change;

	fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
		let this = self.get_mut();

		// Update finalized_to
		if !this.finality_notifications.is_terminated() {
			while let Poll::Ready(Some(notification)) =
				this.finality_notifications.poll_next_unpin(cx)
			{
				this.finalized_to = *notification.header.number() + 1u32.into();
			}
		}

		// Handle (assumed) pruned blocks
		let pruned_to = this.finalized_to.saturating_sub(this.num_blocks_kept.into());
		this.fetched_to = this.fetched_to.max(pruned_to); // Don't try to fetch pruned blocks!
		while let (only_block, Some(block)) = (this.blocks.len() == 1, this.blocks.front_mut()) {
			if block.number >= pruned_to {
				break; // Not pruned
			}

			// Discard any transaction hashes that we didn't even add yet
			if let (true, Some(added_to)) = (only_block, this.added_to) {
				block.transaction_hashes.truncate(added_to);
				this.added_to = None;
			}

			while let Some(hash) = block.transaction_hashes.pop() {
				match this.extra_refs.entry(hash) {
					Entry::Occupied(mut entry) => match entry.get().checked_sub(1) {
						Some(extra_refs) => {
							entry.insert(extra_refs);
						},
						None => {
							entry.remove();
							// TODO: don't assume hash is BLAKE2b, use `Multihash` instead.
							return Poll::Ready(Some(Change::Removed(
								to_multihash::<BlakeTwo256>(&hash),
							)));
						},
					},
					// This should not be possible!
					Entry::Vacant(_) => warn!("Pruned transaction hash {hash} not found"),
				}
			}

			this.blocks.pop_front();
		}

		// Handle finalized blocks
		loop {
			// Fetch finalized blocks
			while this.added_to.is_none() && (this.fetched_to < this.finalized_to) {
				let hashes = this.client.block_hash(this.fetched_to).and_then(|hash| {
					let hash = hash.ok_or_else(|| {
						sp_blockchain::Error::UnknownBlock(format!(
							"Hash of block {} not found",
							this.fetched_to,
						))
					})?;
					this.client.block_indexed_hashes(hash)
				});
				match hashes {
					Ok(Some(hashes)) if !hashes.is_empty() => {
						this.blocks.push_back(IndexedBlock {
							number: this.fetched_to,
							transaction_hashes: hashes,
						});
						this.added_to = Some(0);
					},
					Ok(_) => (),
					Err(err) => debug!("Error fetching block {}: {err}", this.fetched_to),
				}
				this.fetched_to += 1u32.into();
			}

			// Add from last fetched block
			while let Some(added_to) = &mut this.added_to {
				let block = this.blocks.back().expect(
					"added_to only set to Some after pushing a block, \
					set to None before popping last block",
				);
				let hash = block.transaction_hashes[*added_to];
				*added_to += 1;
				if *added_to == block.transaction_hashes.len() {
					this.added_to = None;
				}

				match this.extra_refs.entry(hash) {
					Entry::Occupied(mut entry) => *entry.get_mut() += 1,
					Entry::Vacant(entry) => {
						entry.insert(0);
						// TODO: don't assume hash is BLAKE2b, use `Multihash` instead.
						return Poll::Ready(Some(Change::Added(to_multihash::<BlakeTwo256>(
							&hash,
						))));
					},
				}
			}

			// Fully handled last fetched block. Loop if there are more blocks to fetch, otherwise
			// nothing to do.
			debug_assert!(this.fetched_to <= this.finalized_to);
			if this.fetched_to == this.finalized_to {
				return Poll::Pending;
			}
		}
	}
}

/// Implements [`BlockProvider`], providing access to indexed transactions in the wrapped client.
/// Note that it isn't possible to just implement [`BlockProvider`] on types implementing
/// [`BlockBackend`] because `BlockBackend` is generic over the (chain) block type.
pub struct IndexedTransactions<B, C> {
	client: Arc<C>,
	num_blocks_kept: u32,
	phantom: PhantomData<B>,
}

impl<B, C> IndexedTransactions<B, C> {
	/// Create a new `IndexedTransactions` wrapper over the given client. The client is assumed to
	/// keep `num_blocks_kept` finalized blocks.
	pub fn new(client: Arc<C>, num_blocks_kept: u32) -> Self {
		Self { client, num_blocks_kept, phantom: PhantomData }
	}
}

impl<B, C> BlockProvider for IndexedTransactions<B, C>
where
	B: Block,
	C: BlockchainEvents<B> + BlockBackend<B> + Send + Sync + 'static,
{
	fn have(&self, multihash: &Multihash) -> bool {
		let Some(hash) = try_from_multihash::<BlakeTwo256>(multihash) else { return false };
		match self.client.has_indexed_transaction(hash) {
			Ok(have) => have,
			Err(err) => {
				debug!(target: LOG_TARGET, "Error checking for block {hash:?}: {err}");
				false
			},
		}
	}

	fn get(&self, multihash: &Multihash) -> Option<Vec<u8>> {
		let Some(hash) = try_from_multihash::<BlakeTwo256>(multihash) else { return None };
		match self.client.indexed_transaction(hash) {
			Ok(block) => block,
			Err(err) => {
				debug!(target: LOG_TARGET, "Error getting block {hash:?}: {err}");
				None
			},
		}
	}

	fn changes(&self) -> Pin<Box<dyn Stream<Item = Change> + Send>> {
		Box::pin(IndexedTransactionChanges::new(self.client.clone(), self.num_blocks_kept))
	}
}
