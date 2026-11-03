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

use futures::channel::oneshot;
use polkadot_node_subsystem::{
	errors::ChainApiError,
	messages::{ChainApiMessage, ProspectiveParachainsMessage, RuntimeApiMessage},
	SubsystemSender,
};
use polkadot_primitives::{BlockNumber, Hash};

use std::{
	collections::{hash_map::Entry, HashMap},
	iter,
};

use crate::{
	request_session_index_for_child,
	runtime::{self, fetch_scheduling_lookahead, recv_runtime},
	LOG_TARGET,
};

// Always aim to retain 1 block before the active leaves.
const MINIMUM_RETAIN_LENGTH: BlockNumber = 2;

/// Handles the implicit view of the relay chain derived from the immediate view, which
/// is composed of active leaves, and the minimum relay-parents allowed for
/// candidates of various parachains at those leaves.
#[derive(Clone)]
pub struct View {
	leaves: HashMap<Hash, ActiveLeafPruningInfo>,
	block_info_storage: HashMap<Hash, BlockInfo>,
}

impl View {
	/// Create a new empty view.
	pub fn new() -> Self {
		Self { leaves: Default::default(), block_info_storage: Default::default() }
	}
}

impl Default for View {
	fn default() -> Self {
		Self::new()
	}
}

// Minimum relay parents implicitly relative to a particular block.
#[derive(Debug, Clone)]
struct AllowedRelayParents {
	// Ancestry, in descending order, starting from the block hash itself down
	// to and including the minimum of `minimum_relay_parents`.
	allowed_relay_parents_contiguous: Vec<Hash>,
}

impl AllowedRelayParents {
	fn allowed_relay_parents(&self) -> &[Hash] {
		&self.allowed_relay_parents_contiguous
	}
}

#[derive(Debug, Clone)]
struct ActiveLeafPruningInfo {
	// The minimum block in the same branch of the relay-chain that should be
	// preserved.
	retain_minimum: BlockNumber,
}

#[derive(Debug, Clone)]
struct BlockInfo {
	block_number: BlockNumber,
	// If this was previously an active leaf, this will be `Some`
	// and is useful for understanding the views of peers in the network
	// which may not be in perfect synchrony with our own view.
	//
	// If they are ahead of us in getting a new leaf, there's nothing we
	// can do as it's an unrecognized block hash. But if they're behind us,
	// it's useful for us to retain some information about previous leaves'
	// implicit views so we can continue to send relevant messages to them
	// until they catch up.
	maybe_allowed_relay_parents: Option<AllowedRelayParents>,
}

impl View {
	/// Get an iterator over active leaves in the view.
	pub fn leaves(&self) -> impl Iterator<Item = &Hash> {
		self.leaves.keys()
	}

	/// Check if the given block hash is an active leaf of the current view.
	pub fn contains_leaf(&self, leaf_hash: &Hash) -> bool {
		self.leaves.contains_key(leaf_hash)
	}

	/// Get the block number of a leaf in the current view.
	/// Returns `None` if leaf is not in the view.
	pub fn block_number(&self, leaf_hash: &Hash) -> Option<BlockNumber> {
		self.block_info_storage.get(leaf_hash).map(|block_info| block_info.block_number)
	}

	/// Activate a leaf in the view.
	/// This will request the minimum relay parents the leaf and will load headers in the
	/// ancestry of the leaf as needed. These are the 'implicit ancestors' of the leaf.
	///
	/// To maximize reuse of outdated leaves, it's best to activate new leaves before
	/// deactivating old ones.
	///
	/// The allowed relay parents for the relevant paras under this leaf can be
	/// queried with [`View::known_allowed_relay_parents_under`].
	///
	/// No-op for known leaves.
	pub async fn activate_leaf<Sender>(
		&mut self,
		sender: &mut Sender,
		leaf_hash: Hash,
	) -> Result<(), FetchError>
	where
		Sender: SubsystemSender<ChainApiMessage>
			+ SubsystemSender<ProspectiveParachainsMessage>
			+ SubsystemSender<RuntimeApiMessage>,
	{
		if self.leaves.contains_key(&leaf_hash) {
			return Err(FetchError::AlreadyKnown);
		}

		let res = self.fetch_fresh_leaf_and_insert_ancestry(leaf_hash, &mut *sender).await;

		match res {
			Ok(fetched) => {
				// Retain at least `MINIMUM_RETAIN_LENGTH` blocks in storage.
				// This helps to avoid Chain API calls when activating leaves in the
				// same chain.
				let retain_minimum = std::cmp::min(
					fetched.minimum_ancestor_number,
					fetched.leaf_number.saturating_sub(MINIMUM_RETAIN_LENGTH),
				);

				self.leaves.insert(leaf_hash, ActiveLeafPruningInfo { retain_minimum });

				Ok(())
			},
			Err(e) => Err(e),
		}
	}

	/// Deactivate a leaf in the view. This prunes any outdated implicit ancestors as well.
	///
	/// Returns hashes of blocks pruned from storage.
	pub fn deactivate_leaf(&mut self, leaf_hash: Hash) -> Vec<Hash> {
		let mut removed = Vec::new();

		if self.leaves.remove(&leaf_hash).is_none() {
			return removed;
		}

		// Prune everything before the minimum out of all leaves,
		// pruning absolutely everything if there are no leaves (empty view)
		//
		// Pruning by block number does leave behind orphaned forks slightly longer
		// but the memory overhead is negligible.
		{
			let minimum = self.leaves.values().map(|l| l.retain_minimum).min();

			self.block_info_storage.retain(|hash, i| {
				let keep = minimum.map_or(false, |m| i.block_number >= m);
				if !keep {
					removed.push(*hash);
				}
				keep
			});

			removed
		}
	}

	/// Get an iterator over all allowed relay-parents in the view with no particular order.
	///
	/// **Important**: not all blocks are guaranteed to be allowed for some leaves, it may
	/// happen that a block info is only kept in the view storage because of a retaining rule.
	///
	/// For getting relay-parents that are valid for parachain candidates use
	/// [`View::known_allowed_relay_parents_under`].
	pub fn all_allowed_relay_parents(&self) -> impl Iterator<Item = &Hash> {
		self.block_info_storage.keys()
	}

	/// Get the known, allowed relay-parents that are valid for parachain candidates
	/// which could be backed in a child of a given block.
	///
	/// This is expressed as a contiguous slice of relay-chain block hashes which may
	/// include the provided block hash itself.
	///
	/// `None` indicates that the block hash isn't part of the implicit view or that
	/// there are no known allowed relay parents.
	///
	/// This always returns `Some` for active leaves or for blocks that previously
	/// were active leaves.
	///
	/// This can return the empty slice, which indicates that no relay-parents are allowed
	/// at the given block hash.
	pub fn known_allowed_relay_parents_under(&self, block_hash: &Hash) -> Option<&[Hash]> {
		let block_info = self.block_info_storage.get(block_hash)?;
		block_info
			.maybe_allowed_relay_parents
			.as_ref()
			.map(|mins| mins.allowed_relay_parents())
	}

	/// Returns all paths from each leaf's oldest allowed ancestor to the leaf, for every
	/// leaf whose allowed ancestry contains `relay_parent`.
	///
	/// If `relay_parent` is not in any leaf's allowed ancestry, returns an empty `Vec`.
	pub fn paths_via_relay_parent(&self, relay_parent: &Hash) -> Vec<Vec<Hash>> {
		gum::trace!(
			target: LOG_TARGET,
			?relay_parent,
			leaves=?self.leaves,
			block_info_storage=?self.block_info_storage,
			"Finding paths via relay parent"
		);

		if !self.block_info_storage.contains_key(relay_parent) {
			return vec![];
		}

		let mut paths = Vec::new();
		for (leaf, _) in &self.leaves {
			let Some(allowed_rps) = self
				.block_info_storage
				.get(leaf)
				.and_then(|info| info.maybe_allowed_relay_parents.as_ref())
			else {
				gum::warn!(
					target: LOG_TARGET,
					?leaf,
					"Active leaf missing allowed relay parents",
				);
				continue;
			};

			// allowed_relay_parents_contiguous is in descending order: [leaf, parent, grandparent,
			// ...] Check that relay_parent is in this leaf's allowed ancestry.
			let contiguous = &allowed_rps.allowed_relay_parents_contiguous;
			if contiguous.iter().any(|h| h == relay_parent) {
				let path: Vec<Hash> = contiguous.iter().rev().copied().collect();
				paths.push(path);
			}
		}

		paths
	}

	async fn fetch_fresh_leaf_and_insert_ancestry<Sender>(
		&mut self,
		leaf_hash: Hash,
		sender: &mut Sender,
	) -> Result<FetchSummary, FetchError>
	where
		Sender: SubsystemSender<ChainApiMessage>
			+ SubsystemSender<ProspectiveParachainsMessage>
			+ SubsystemSender<RuntimeApiMessage>,
	{
		let ancestors = fetch_ancestors(leaf_hash, sender).await?;
		let ancestor_len = ancestors.len();

		let ancestry: Vec<Hash> = iter::once(leaf_hash).chain(ancestors).collect();

		let mut allowed_relay_parents =
			Some(AllowedRelayParents { allowed_relay_parents_contiguous: ancestry.clone() });

		// Ensure all ancestors up to and including `min_relay_parent` are in the
		// block storage. When views advance incrementally, everything
		// should already be present.
		for block_hash in ancestry {
			let block_info_entry = match self.block_info_storage.entry(block_hash) {
				Entry::Occupied(_) => continue,
				Entry::Vacant(e) => e,
			};

			let (tx, rx) = oneshot::channel();
			sender.send_message(ChainApiMessage::BlockHeader(block_hash, tx)).await;
			let header = match rx.await {
				Ok(Ok(Some(header))) => header,
				Ok(Ok(None)) => {
					return Err(FetchError::BlockHeaderUnavailable(
						block_hash,
						BlockHeaderUnavailableReason::Unknown,
					));
				},
				Ok(Err(e)) => {
					return Err(FetchError::BlockHeaderUnavailable(
						block_hash,
						BlockHeaderUnavailableReason::Internal(e),
					));
				},
				Err(_) => {
					return Err(FetchError::BlockHeaderUnavailable(
						block_hash,
						BlockHeaderUnavailableReason::SubsystemUnavailable,
					));
				},
			};
			block_info_entry.insert(BlockInfo {
				block_number: header.number,
				// Populate leaf node with Some:
				maybe_allowed_relay_parents: allowed_relay_parents.take(),
			});
		}

		let leaf_entry = self
			.block_info_storage
			.get(&leaf_hash)
			.expect("We just inserted this entry. qed.");

		Ok(FetchSummary {
			minimum_ancestor_number: leaf_entry.block_number.saturating_sub(ancestor_len as u32),
			leaf_number: leaf_entry.block_number,
		})
	}
}

/// Errors when fetching a leaf and associated ancestry.
#[fatality::fatality]
pub enum FetchError {
	/// Activated leaf is already present in view.
	#[error("Leaf was already known")]
	AlreadyKnown,

	/// Request to the prospective parachains subsystem failed.
	#[error("The prospective parachains subsystem was unavailable")]
	ProspectiveParachainsUnavailable,

	/// Failed to fetch the block header.
	#[error("A block header was unavailable")]
	BlockHeaderUnavailable(Hash, BlockHeaderUnavailableReason),

	/// A block header was unavailable due to a chain API error.
	#[error("A block header was unavailable due to a chain API error")]
	ChainApiError(Hash, ChainApiError),

	/// Request to the Chain API subsystem failed.
	#[error("The chain API subsystem was unavailable")]
	ChainApiUnavailable,

	/// Request to the runtime API failed.
	#[error("Runtime API error: {0}")]
	RuntimeApi(#[from] runtime::Error),
}

/// Reasons a block header might have been unavailable.
#[derive(Debug)]
pub enum BlockHeaderUnavailableReason {
	/// Block header simply unknown.
	Unknown,
	/// Internal Chain API error.
	Internal(ChainApiError),
	/// The subsystem was unavailable.
	SubsystemUnavailable,
}

struct FetchSummary {
	minimum_ancestor_number: BlockNumber,
	leaf_number: BlockNumber,
}

/// Fetches ancestor block hashes for a given leaf.
///
/// Returns up to `scheduling_lookahead - 1` ancestor block hashes in descending order (from most
/// recent to oldest), stopping early if a session boundary is encountered. This ensures all
/// returned ancestors are within the same session as the leaf.
///
/// # Returns
///
/// A vector of ancestor block hashes in descending order (excluding the leaf itself).
async fn fetch_ancestors<Sender>(
	leaf_hash: Hash,
	sender: &mut Sender,
) -> Result<Vec<Hash>, FetchError>
where
	Sender: SubsystemSender<ProspectiveParachainsMessage>
		+ SubsystemSender<RuntimeApiMessage>
		+ SubsystemSender<ChainApiMessage>,
{
	// Fetch the session of the leaf. We must make sure that we stop at the ancestor which has a
	// different session index.
	let required_session =
		recv_runtime(request_session_index_for_child(leaf_hash, sender).await).await?;

	let scheduling_lookahead =
		fetch_scheduling_lookahead(leaf_hash, required_session, sender).await?;

	// Fetch the ancestors, up to (scheduling_lookahead - 1).
	let (tx, rx) = oneshot::channel();
	sender
		.send_message(ChainApiMessage::Ancestors {
			hash: leaf_hash,
			k: scheduling_lookahead.saturating_sub(1) as usize,
			response_channel: tx,
		})
		.await;
	let mut hashes = rx
		.await
		.map_err(|_| FetchError::ChainApiUnavailable)?
		.map_err(|err| FetchError::ChainApiError(leaf_hash, err))?;

	let mut session_change_at = None;
	for (i, hash) in hashes.iter().enumerate() {
		let session = recv_runtime(request_session_index_for_child(*hash, sender).await).await?;
		// The relay chain cannot accept blocks backed from previous sessions, with
		// potentially previous validators. This is a technical limitation we need to
		// respect here.
		if session != required_session {
			session_change_at = Some(i);
			break;
		}
	}
	if let Some(session_change_at) = session_change_at {
		hashes.truncate(session_change_at);
	}
	Ok(hashes)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::TimeoutExt;
	use assert_matches::assert_matches;
	use futures::future::{join, FutureExt};
	use polkadot_node_subsystem::{messages::RuntimeApiRequest, AllMessages};
	use polkadot_node_subsystem_test_helpers::{
		make_subsystem_context, TestSubsystemContextHandle,
	};
	use polkadot_overseer::SubsystemContext;
	use polkadot_primitives::Header;
	use sp_core::testing::TaskExecutor;
	use std::time::Duration;

	const GENESIS_HASH: Hash = Hash::repeat_byte(0xFF);
	const GENESIS_NUMBER: BlockNumber = 0;

	// Chains A and B are forks of genesis.

	const CHAIN_A: &[Hash] =
		&[Hash::repeat_byte(0x01), Hash::repeat_byte(0x02), Hash::repeat_byte(0x03)];

	const CHAIN_B: &[Hash] = &[
		Hash::repeat_byte(0x04),
		Hash::repeat_byte(0x05),
		Hash::repeat_byte(0x06),
		Hash::repeat_byte(0x07),
		Hash::repeat_byte(0x08),
		Hash::repeat_byte(0x09),
	];

	type VirtualOverseer = TestSubsystemContextHandle<AllMessages>;

	const TIMEOUT: Duration = Duration::from_secs(2);

	async fn overseer_recv(virtual_overseer: &mut VirtualOverseer) -> AllMessages {
		virtual_overseer
			.recv()
			.timeout(TIMEOUT)
			.await
			.expect("overseer `recv` timed out")
	}

	fn default_header() -> Header {
		Header {
			parent_hash: Hash::zero(),
			number: 0,
			state_root: Hash::zero(),
			extrinsics_root: Hash::zero(),
			digest: Default::default(),
		}
	}

	fn get_block_header(chain: &[Hash], hash: &Hash) -> Option<Header> {
		let idx = chain.iter().position(|h| h == hash)?;
		let parent_hash = idx.checked_sub(1).map(|i| chain[i]).unwrap_or(GENESIS_HASH);
		let number =
			if *hash == GENESIS_HASH { GENESIS_NUMBER } else { GENESIS_NUMBER + idx as u32 + 1 };
		Some(Header { parent_hash, number, ..default_header() })
	}

	async fn assert_block_header_requests(
		virtual_overseer: &mut VirtualOverseer,
		chain: &[Hash],
		blocks: &[Hash],
	) {
		for block in blocks.iter().rev() {
			assert_matches!(
				overseer_recv(virtual_overseer).await,
				AllMessages::ChainApi(
					ChainApiMessage::BlockHeader(hash, tx)
				) => {
					assert_eq!(*block, hash, "unexpected block header request");
					let header = if block == &GENESIS_HASH {
						Header {
							number: GENESIS_NUMBER,
							..default_header()
						}
					} else {
						get_block_header(chain, block).expect("unknown block")
					};

					tx.send(Ok(Some(header))).unwrap();
				}
			);
		}
	}

	async fn assert_scheduling_lookahead_request(
		virtual_overseer: &mut VirtualOverseer,
		leaf: Hash,
		lookahead: u32,
	) {
		assert_matches!(
			overseer_recv(virtual_overseer).await,
			AllMessages::RuntimeApi(
				RuntimeApiMessage::Request(
					leaf_hash,
					RuntimeApiRequest::SchedulingLookahead(
						_,
						tx
					)
				)
			) => {
				assert_eq!(leaf, leaf_hash, "received unexpected leaf hash");
				tx.send(Ok(lookahead)).unwrap();
			}
		);
	}

	async fn assert_session_index_request(
		virtual_overseer: &mut VirtualOverseer,
		leaf: Hash,
		session: u32,
	) {
		assert_matches!(
			overseer_recv(virtual_overseer).await,
			AllMessages::RuntimeApi(
				RuntimeApiMessage::Request(
					leaf_hash,
					RuntimeApiRequest::SessionIndexForChild(
						tx
					)
				)
			) => {
				assert_eq!(leaf, leaf_hash, "received unexpected leaf hash");
				tx.send(Ok(session)).unwrap();
			}
		);
	}

	async fn assert_ancestors_request(
		virtual_overseer: &mut VirtualOverseer,
		leaf: Hash,
		expected_ancestor_len: u32,
		response: Vec<Hash>,
	) {
		assert_matches!(
			overseer_recv(virtual_overseer).await,
			AllMessages::ChainApi(
				ChainApiMessage::Ancestors {
					hash: leaf_hash,
					k,
					response_channel: tx
				}
			) => {
				assert_eq!(leaf, leaf_hash, "received unexpected leaf hash");
				assert_eq!(k, expected_ancestor_len as usize);

				tx.send(Ok(response)).unwrap();
			}
		);
	}

	/// Helper function to activate a leaf and handle the expected sequence of overseer requests.
	/// This encapsulates the common pattern used across multiple tests.
	///
	/// # Parameters
	/// - `view`: The view to activate the leaf in
	/// - `ctx`: The subsystem context
	/// - `ctx_handle`: The virtual overseer handle
	/// - `leaf`: The leaf hash to activate
	/// - `session`: The session index for the leaf
	/// - `scheduling_lookahead`: The scheduling lookahead value
	/// - `ancestors`: The ancestor hashes (in descending order from leaf)
	/// - `ancestor_sessions`: Session indices for each ancestor (in descending order)
	/// - `chain`: The chain to use for block header requests
	/// - `blocks_for_headers`: The blocks to fetch headers for
	async fn activate_leaf_with_overseer_requests<Ctx>(
		view: &mut View,
		ctx: &mut Ctx,
		ctx_handle: &mut VirtualOverseer,
		leaf: Hash,
		session: u32,
		scheduling_lookahead: u32,
		ancestors: Vec<Hash>,
		ancestor_sessions: Vec<u32>,
		chain: &[Hash],
		blocks_for_headers: &[Hash],
	) where
		Ctx: SubsystemContext<Message = AllMessages>,
		Ctx::Sender: SubsystemSender<ChainApiMessage>
			+ SubsystemSender<ProspectiveParachainsMessage>
			+ SubsystemSender<RuntimeApiMessage>,
	{
		let fut = view.activate_leaf(ctx.sender(), leaf).timeout(TIMEOUT).map(|res| {
			res.expect("`activate_leaf` timed out").unwrap();
		});
		let overseer_fut = async {
			// Session index for leaf
			assert_session_index_request(ctx_handle, leaf, session).await;

			// Scheduling lookahead
			assert_scheduling_lookahead_request(ctx_handle, leaf, scheduling_lookahead).await;

			// Ancestors request (returned in descending order)
			assert_ancestors_request(ctx_handle, leaf, scheduling_lookahead - 1, ancestors.clone())
				.await;

			// Session index for each ancestor (in descending order)
			for (ancestor, ancestor_session) in ancestors.iter().zip(ancestor_sessions.iter()) {
				assert_session_index_request(ctx_handle, *ancestor, *ancestor_session).await;
			}

			// Block headers for leaf and all ancestors
			assert_block_header_requests(ctx_handle, chain, blocks_for_headers).await;
		};
		join(fut, overseer_fut).await;
	}

	/// Helper function to assert that allowed relay parents match expectations.
	///
	/// # Parameters
	/// - `view`: The view to check
	/// - `leaf`: The leaf hash to check allowed relay parents for
	/// - `expected_ancestry`: The expected allowed relay parents (in descending order)
	fn assert_expected_allowed_relay_parents(view: &View, leaf: &Hash, expected_ancestry: &[Hash]) {
		let leaf_info =
			view.block_info_storage.get(leaf).expect("block must be present in storage");
		assert_matches!(
			leaf_info.maybe_allowed_relay_parents,
			Some(ref allowed_relay_parents) => {
				assert_eq!(
					allowed_relay_parents.allowed_relay_parents_contiguous,
					expected_ancestry
				);
				assert_eq!(view.known_allowed_relay_parents_under(leaf), Some(expected_ancestry));
			}
		);
	}

	/// Tests basic view construction by activating two leaves on different chain forks.
	///
	/// Verifies that:
	/// - Allowed relay parents are correctly computed based on scheduling lookahead
	/// - Only the leaf block stores allowed relay parents, not intermediate ancestors
	/// - Multiple leaves can coexist in the view
	/// - Path finding works correctly for blocks within the implicit view
	#[test]
	fn construct_fresh_view() {
		let pool = TaskExecutor::new();
		let (mut ctx, mut ctx_handle) = make_subsystem_context::<AllMessages, _>(pool);

		let mut view = View::default();

		// Activate first leaf on CHAIN_B with lookahead of 3
		const SESSION: u32 = 2;
		const SCHEDULING_LOOKAHEAD: u32 = 3;

		let leaf = CHAIN_B.last().unwrap();
		let leaf_idx = CHAIN_B.len() - 1;
		// With lookahead 3, we fetch 2 ancestors (lookahead - 1)
		let min_idx = leaf_idx - (SCHEDULING_LOOKAHEAD as usize - 1);

		futures::executor::block_on(activate_leaf_with_overseer_requests(
			&mut view,
			&mut ctx,
			&mut ctx_handle,
			*leaf,
			SESSION,
			SCHEDULING_LOOKAHEAD,
			CHAIN_B[min_idx..leaf_idx].iter().rev().copied().collect(),
			vec![SESSION; leaf_idx - min_idx],
			CHAIN_B,
			&CHAIN_B[min_idx..=leaf_idx],
		));

		// Only leaf blocks have allowed relay parents, not intermediate ancestors
		for i in min_idx..(CHAIN_B.len() - 1) {
			assert!(view.known_allowed_relay_parents_under(&CHAIN_B[i]).is_none());
		}

		// The leaf should have all blocks from min_idx to leaf as allowed relay parents
		let expected_ancestry: Vec<Hash> =
			CHAIN_B[min_idx..=leaf_idx].iter().rev().copied().collect();
		assert_expected_allowed_relay_parents(&view, leaf, &expected_ancestry);

		// Verify we have exactly one active leaf
		assert_eq!(view.leaves.len(), 1);
		assert!(view.leaves.contains_key(leaf));

		// Blocks outside the implicit view return empty paths
		assert!(view.paths_via_relay_parent(&CHAIN_B[0]).is_empty());
		assert!(view.paths_via_relay_parent(&CHAIN_A[0]).is_empty());

		// Blocks within the implicit view return the full allowed ancestry path
		// (oldest allowed ancestor to leaf), bounded by scheduling lookahead.
		assert_eq!(
			view.paths_via_relay_parent(&CHAIN_B[min_idx]),
			vec![CHAIN_B[min_idx..].to_vec()]
		);
		// Same full path — relay_parent just needs to be somewhere on it.
		assert_eq!(
			view.paths_via_relay_parent(&CHAIN_B[min_idx + 1]),
			vec![CHAIN_B[min_idx..].to_vec()]
		);
		assert_eq!(view.paths_via_relay_parent(&leaf), vec![CHAIN_B[min_idx..].to_vec()]);

		// Activate second leaf on CHAIN_A (a fork of CHAIN_B at genesis)
		const SCHEDULING_LOOKAHEAD_A: u32 = 4;
		let leaf = CHAIN_A.last().unwrap();
		let blocks = [&[GENESIS_HASH], CHAIN_A].concat();
		let leaf_idx = blocks.len() - 1;
		// With lookahead 4, we fetch 3 ancestors, starting from CHAIN_A[0]
		let min_idx_a = 1;

		futures::executor::block_on(activate_leaf_with_overseer_requests(
			&mut view,
			&mut ctx,
			&mut ctx_handle,
			*leaf,
			SESSION,
			SCHEDULING_LOOKAHEAD_A,
			blocks[min_idx_a..leaf_idx].iter().rev().copied().collect(),
			vec![SESSION; leaf_idx - min_idx_a],
			CHAIN_A,
			&blocks[min_idx_a..],
		));

		// Now we have two active leaves from different forks
		assert_eq!(view.leaves.len(), 2);

		// Second leaf has its own set of allowed relay parents
		let expected_ancestry: Vec<Hash> = blocks[min_idx_a..].iter().rev().copied().collect();
		assert_expected_allowed_relay_parents(&view, leaf, &expected_ancestry);
	}

	/// Tests view construction with different scheduling lookahead values and session boundaries.
	///
	/// Verifies that:
	/// - Views can be constructed with various scheduling lookahead values
	/// - Session boundaries are respected (ancestors in different sessions are excluded)
	/// - Path finding correctly handles session boundaries
	#[test]
	fn construct_fresh_view_with_various_lookaheads() {
		let pool = TaskExecutor::new();
		let (mut ctx, mut ctx_handle) = make_subsystem_context::<AllMessages, _>(pool);

		let mut view = View::new();

		// Activate CHAIN_B with a larger lookahead value (5)
		const SCHEDULING_LOOKAHEAD: u32 = 5;
		const MIN_RELAY_PARENT_NUMBER: u32 = 4;

		let current_session = 2;

		let leaf = CHAIN_B.last().unwrap();
		let leaf_idx = CHAIN_B.len() - 1;
		// Calculate minimum ancestor index based on absolute block number
		let min_idx = (MIN_RELAY_PARENT_NUMBER - GENESIS_NUMBER - 1) as usize;

		futures::executor::block_on(activate_leaf_with_overseer_requests(
			&mut view,
			&mut ctx,
			&mut ctx_handle,
			*leaf,
			current_session,
			SCHEDULING_LOOKAHEAD,
			CHAIN_B[min_idx..leaf_idx].iter().rev().copied().collect(),
			vec![current_session; leaf_idx - min_idx],
			CHAIN_B,
			&CHAIN_B[min_idx..=leaf_idx],
		));

		// Intermediate ancestors don't have allowed relay parents
		for i in min_idx..(CHAIN_B.len() - 1) {
			assert!(view.known_allowed_relay_parents_under(&CHAIN_B[i]).is_none());
		}

		// Leaf has expected allowed relay parents
		let expected_ancestry: Vec<Hash> =
			CHAIN_B[min_idx..=leaf_idx].iter().rev().copied().collect();
		assert_expected_allowed_relay_parents(&view, leaf, &expected_ancestry);

		// Block from different fork returns no paths
		assert!(view.paths_via_relay_parent(&CHAIN_A[0]).is_empty());
		// Block within view returns correct path
		assert_eq!(
			view.paths_via_relay_parent(&CHAIN_B[min_idx]),
			vec![CHAIN_B[min_idx..].to_vec()]
		);

		// Activate CHAIN_A where ancestors extend back to genesis (different session)
		// This tests that we stop fetching at session boundaries
		let leaf = CHAIN_A.last().unwrap();
		let blocks = [&[GENESIS_HASH], CHAIN_A].concat();
		let leaf_idx = blocks.len() - 1;

		// Ancestors are in current session, but genesis is in session 0
		// This simulates a session boundary
		let mut ancestor_sessions = vec![current_session; leaf_idx - 1];
		ancestor_sessions.push(0);

		futures::executor::block_on(activate_leaf_with_overseer_requests(
			&mut view,
			&mut ctx,
			&mut ctx_handle,
			*leaf,
			current_session,
			blocks.len() as u32 + 1,
			blocks[..leaf_idx].iter().rev().copied().collect(),
			ancestor_sessions,
			CHAIN_A,
			// Only fetch headers for CHAIN_A blocks; genesis is excluded due to session boundary
			&blocks[1..=leaf_idx],
		));

		// Two leaves active (one on each fork)
		assert_eq!(view.leaves.len(), 2);

		// Allowed relay parents only include CHAIN_A blocks (not genesis due to session boundary)
		let expected_ancestry: Vec<Hash> = CHAIN_A[..].iter().rev().copied().collect();
		assert_expected_allowed_relay_parents(&view, leaf, &expected_ancestry);

		// Genesis is not in the view because of the session boundary
		assert!(view.paths_via_relay_parent(&GENESIS_HASH).is_empty());
		// But CHAIN_A blocks are in the view
		assert_eq!(view.paths_via_relay_parent(&CHAIN_A[0]), vec![CHAIN_A.to_vec()]);
	}

	/// Tests that block info storage is reused when activating subsequent leaves.
	///
	/// Verifies that:
	/// - Block info for overlapping ancestors is cached and reused
	/// - Only new blocks fetch headers from the chain API
	/// - Previously activated leaves retain their allowed relay parents after new leaves are added
	#[test]
	fn reuse_block_info_storage() {
		let pool = TaskExecutor::new();
		let (mut ctx, mut ctx_handle) = make_subsystem_context::<AllMessages, _>(pool);

		let mut view = View::default();

		// Activate first leaf at block 3 with lookahead 3
		const SESSION: u32 = 2;
		const SCHEDULING_LOOKAHEAD_A: u32 = 3;
		let leaf_a_number = 3;
		let leaf_a = CHAIN_B[leaf_a_number - 1];
		let min_idx = leaf_a_number - (SCHEDULING_LOOKAHEAD_A as usize - 1);

		futures::executor::block_on(activate_leaf_with_overseer_requests(
			&mut view,
			&mut ctx,
			&mut ctx_handle,
			leaf_a,
			SESSION,
			SCHEDULING_LOOKAHEAD_A,
			CHAIN_B[min_idx..(leaf_a_number - 1)].iter().rev().copied().collect(),
			vec![SESSION; leaf_a_number - 1 - min_idx],
			CHAIN_B,
			&CHAIN_B[min_idx..leaf_a_number],
		));

		// Activate second leaf at block 5 with lookahead 5
		// This should reuse blocks 1-3 from storage (already fetched for leaf_a)
		const SCHEDULING_LOOKAHEAD_B: u32 = 5;
		let leaf_b_number = 5;
		let leaf_b = CHAIN_B[leaf_b_number - 1];

		futures::executor::block_on(activate_leaf_with_overseer_requests(
			&mut view,
			&mut ctx,
			&mut ctx_handle,
			leaf_b,
			SESSION,
			SCHEDULING_LOOKAHEAD_B,
			CHAIN_B[min_idx..(leaf_b_number - 1)].iter().rev().copied().collect(),
			vec![SESSION; leaf_b_number - 1 - min_idx],
			CHAIN_B,
			// Only blocks 3-4 need headers; blocks 0-2 were already fetched for leaf_a
			&CHAIN_B[leaf_a_number..leaf_b_number],
		));

		// Verify that leaf_a still has its allowed relay parents after activating leaf_b
		let expected_ancestry: Vec<Hash> =
			CHAIN_B[min_idx..leaf_a_number].iter().rev().copied().collect();
		assert_expected_allowed_relay_parents(&view, &leaf_a, &expected_ancestry);
	}

	/// Tests that outdated blocks are pruned when leaves are deactivated.
	///
	/// Verifies that:
	/// - Deactivating a non-leaf block is a no-op
	/// - Blocks are pruned when no active leaf requires them
	/// - The minimum block number across all leaves determines what gets pruned
	/// - All blocks are pruned when the last leaf is deactivated
	#[test]
	fn pruning() {
		let pool = TaskExecutor::new();
		let (mut ctx, mut ctx_handle) = make_subsystem_context::<AllMessages, _>(pool);

		let mut view = View::default();

		// Activate leaf_a (second-to-last block) with lookahead 4
		const SESSION: u32 = 2;
		const SCHEDULING_LOOKAHEAD_A: u32 = 4;
		let leaf_a = CHAIN_B.iter().rev().nth(1).unwrap();
		let leaf_a_idx = CHAIN_B.len() - 2;
		let min_a_idx = leaf_a_idx - (SCHEDULING_LOOKAHEAD_A - 1) as usize;

		futures::executor::block_on(activate_leaf_with_overseer_requests(
			&mut view,
			&mut ctx,
			&mut ctx_handle,
			*leaf_a,
			SESSION,
			SCHEDULING_LOOKAHEAD_A,
			CHAIN_B[min_a_idx..leaf_a_idx].iter().rev().copied().collect(),
			vec![SESSION; leaf_a_idx - min_a_idx],
			CHAIN_B,
			&CHAIN_B[min_a_idx..=leaf_a_idx],
		));

		// Activate leaf_b (last block) with smaller lookahead 3
		// This has a higher minimum block number than leaf_a
		const SCHEDULING_LOOKAHEAD_B: u32 = 3;
		let leaf_b = CHAIN_B.last().unwrap();
		let leaf_b_idx = CHAIN_B.len() - 1;
		let min_b_idx = leaf_b_idx - (SCHEDULING_LOOKAHEAD_B - 1) as usize;

		futures::executor::block_on(activate_leaf_with_overseer_requests(
			&mut view,
			&mut ctx,
			&mut ctx_handle,
			*leaf_b,
			SESSION,
			SCHEDULING_LOOKAHEAD_B,
			CHAIN_B[min_b_idx..leaf_b_idx].iter().rev().copied().collect(),
			vec![SESSION; leaf_b_idx - min_b_idx],
			CHAIN_B,
			&[CHAIN_B[leaf_b_idx]], // Only leaf_b needs fetching; ancestors are cached
		));

		// Deactivating a non-leaf block should be a no-op
		let block_info_len = view.block_info_storage.len();
		view.deactivate_leaf(CHAIN_B[leaf_a_idx - 1]);
		assert_eq!(block_info_len, view.block_info_storage.len());

		// Deactivate leaf_b. leaf_a requires blocks from min_a_idx onward,
		// so blocks before min_a_idx should be pruned
		view.deactivate_leaf(*leaf_b);
		for hash in CHAIN_B.iter().take(min_a_idx) {
			assert!(!view.block_info_storage.contains_key(hash));
		}
		// Blocks from min_a_idx onward (required by leaf_a) should NOT be pruned
		for hash in CHAIN_B.iter().skip(min_a_idx).take(leaf_a_idx - min_a_idx + 1) {
			assert!(view.block_info_storage.contains_key(hash));
		}

		// Deactivate the last remaining leaf - all blocks should be pruned
		view.deactivate_leaf(*leaf_a);
		assert!(view.block_info_storage.is_empty());
	}

	/// Tests view construction when the leaf is the genesis block.
	///
	/// Verifies that:
	/// - Genesis block can be activated as a leaf
	/// - No ancestors are fetched (genesis has no parent)
	/// - Genesis is included in its own allowed relay parents
	#[test]
	fn genesis_ancestry() {
		let pool = TaskExecutor::new();
		let (mut ctx, mut ctx_handle) = make_subsystem_context::<AllMessages, _>(pool);

		let mut view = View::default();

		// Activate genesis as a leaf with minimal lookahead
		const SESSION: u32 = 0;
		const SCHEDULING_LOOKAHEAD: u32 = 1;

		futures::executor::block_on(activate_leaf_with_overseer_requests(
			&mut view,
			&mut ctx,
			&mut ctx_handle,
			GENESIS_HASH,
			SESSION,
			SCHEDULING_LOOKAHEAD,
			vec![], // Genesis has no ancestors
			vec![], // No ancestor sessions
			&[GENESIS_HASH],
			&[GENESIS_HASH],
		));

		// Genesis block should have itself as the only allowed relay parent
		assert_matches!(
			view.known_allowed_relay_parents_under(&GENESIS_HASH),
			Some(hashes) if hashes == &[GENESIS_HASH]
		);
	}

	/// Tests path finding through forked chains.
	///
	/// Verifies that:
	/// - Multiple leaves on different forks can coexist
	/// - Path finding returns correct paths for blocks in each fork
	/// - Blocks outside the implicit view return empty paths
	/// - Genesis (common ancestor) is excluded due to scheduling lookahead
	#[test]
	fn path_with_fork() {
		let pool = TaskExecutor::new();
		let (mut ctx, mut ctx_handle) = make_subsystem_context::<AllMessages, _>(pool);

		let mut view = View::default();

		// Activate leaf on CHAIN_A (forks from genesis)
		const SESSION: u32 = 2;
		const SCHEDULING_LOOKAHEAD_A: u32 = 4;
		let leaf = CHAIN_A.last().unwrap();
		let blocks = [&[GENESIS_HASH], CHAIN_A].concat();
		let leaf_idx = blocks.len() - 1;

		futures::executor::block_on(activate_leaf_with_overseer_requests(
			&mut view,
			&mut ctx,
			&mut ctx_handle,
			*leaf,
			SESSION,
			SCHEDULING_LOOKAHEAD_A,
			blocks[1..leaf_idx].iter().rev().copied().collect(),
			vec![SESSION; leaf_idx - 1],
			CHAIN_A,
			&blocks[1..],
		));

		// Activate leaf on CHAIN_B (also forks from genesis)
		const SCHEDULING_LOOKAHEAD_B: u32 = 3;
		let leaf = CHAIN_B.last().unwrap();
		let leaf_idx = CHAIN_B.len() - 1;

		// With lookahead 3, minimum index is at block 3 (leaf_idx=5, so 5-2=3)
		let min_b_idx = leaf_idx - (SCHEDULING_LOOKAHEAD_B - 1) as usize;
		futures::executor::block_on(activate_leaf_with_overseer_requests(
			&mut view,
			&mut ctx,
			&mut ctx_handle,
			*leaf,
			SESSION,
			SCHEDULING_LOOKAHEAD_B,
			CHAIN_B[min_b_idx..leaf_idx].iter().rev().copied().collect(),
			vec![SESSION; leaf_idx - min_b_idx],
			CHAIN_B,
			&CHAIN_B[min_b_idx..],
		));

		// Both leaves are active
		assert_eq!(view.leaves.len(), 2);

		// Genesis is not in the view because scheduling lookahead doesn't go back that far
		let paths_to_genesis = view.paths_via_relay_parent(&GENESIS_HASH);
		assert_eq!(paths_to_genesis, Vec::<Vec<Hash>>::new());

		// CHAIN_A[1] is in the view, path is the full allowed ancestry
		let path_to_leaf_in_a = view.paths_via_relay_parent(&CHAIN_A[1]);
		let expected_path_to_leaf_in_a = vec![CHAIN_A.to_vec()];
		assert_eq!(path_to_leaf_in_a, expected_path_to_leaf_in_a);

		// CHAIN_B[4] is in the view, path is the full allowed ancestry for this leaf
		let path_to_leaf_in_b = view.paths_via_relay_parent(&CHAIN_B[4]);
		let expected_path_to_leaf_in_b = vec![CHAIN_B[3..].to_vec()];
		assert_eq!(path_to_leaf_in_b, expected_path_to_leaf_in_b);

		// Unknown block returns empty paths
		assert_eq!(view.paths_via_relay_parent(&Hash::repeat_byte(0x0A)), Vec::<Vec<Hash>>::new());
	}

	/// When a fork leaf at a lower height coexists with the main chain leaf,
	/// `block_info_storage` retains old blocks. `paths_via_relay_parent` walks through all
	/// of them, producing paths longer than `scheduling_lookahead`.
	///
	/// This causes `is_slot_available` in the collator protocol to compute `valid_len = 0` for
	/// old-but-valid scheduling parents, unconditionally rejecting advertisements.
	///
	/// See: https://github.com/paritytech/polkadot-sdk/issues/11625
	#[test]
	fn max_ancesty_len_honored() {
		let pool = TaskExecutor::new();
		let (mut ctx, mut ctx_handle) = make_subsystem_context::<AllMessages, _>(pool);

		let mut view = View::default();

		const SESSION: u32 = 2;
		const SCHEDULING_LOOKAHEAD: u32 = 3;

		// Step 1: Activate a "fork leaf" at CHAIN_B[2] (#3) with lookahead=3.
		// This represents a fork leaf that stays active while the main chain advances.
		// Fetches 2 ancestors: CHAIN_B[1] (#2), CHAIN_B[0] (#1).
		// Storage after: {B0, B1, B2}
		let fork_leaf = CHAIN_B[2];
		futures::executor::block_on(activate_leaf_with_overseer_requests(
			&mut view,
			&mut ctx,
			&mut ctx_handle,
			fork_leaf,
			SESSION,
			SCHEDULING_LOOKAHEAD,
			vec![CHAIN_B[1], CHAIN_B[0]], // ancestors in descending order
			vec![SESSION; 2],
			CHAIN_B,
			&CHAIN_B[0..=2], // headers for B0, B1, B2
		));

		assert_eq!(view.leaves.len(), 1);

		// Step 2: Activate the main chain leaf at CHAIN_B[5] (#6) with lookahead=3.
		// Fetches 2 ancestors: CHAIN_B[4] (#5), CHAIN_B[3] (#4).
		// B0, B1, B2 remain in storage because fork leaf's retain_minimum = #1.
		// Storage after: {B0, B1, B2, B3, B4, B5}
		let main_leaf = CHAIN_B[5];
		futures::executor::block_on(activate_leaf_with_overseer_requests(
			&mut view,
			&mut ctx,
			&mut ctx_handle,
			main_leaf,
			SESSION,
			SCHEDULING_LOOKAHEAD,
			vec![CHAIN_B[4], CHAIN_B[3]], // ancestors in descending order
			vec![SESSION; 2],
			CHAIN_B,
			&CHAIN_B[3..=5], // only B3, B4, B5 need headers (B0-B2 already in storage)
		));

		assert_eq!(view.leaves.len(), 2);

		// Verify all 6 blocks are in storage (old blocks retained by fork leaf).
		for hash in &CHAIN_B[..6] {
			assert!(
				view.block_info_storage.contains_key(hash),
				"Block {:?} should be in storage",
				hash
			);
		}

		// B0 is in the fork leaf's allowed ancestry {B0, B1, B2} but NOT in the
		// main leaf's allowed ancestry {B3, B4, B5}. So paths_via_relay_parent(&B0)
		// should only return a path via the fork leaf, not the main leaf.
		let paths = view.paths_via_relay_parent(&CHAIN_B[0]);

		// Expected: only 1 path — via the fork leaf [B0, B1, B2], length 3.
		// The main leaf should NOT produce a path through B0 since B0 is outside
		// its allowed ancestry.
		assert_eq!(paths.len(), 1, "B0 should only be reachable via the fork leaf");

		let path_via_fork = &paths[0];
		assert_eq!(path_via_fork.last(), Some(&fork_leaf));
		assert_eq!(path_via_fork.len(), 3);
		assert_eq!(path_via_fork, &CHAIN_B[0..=2].to_vec());

		// B3 is in the main leaf's allowed ancestry {B3, B4, B5} but NOT in the
		// fork leaf's allowed ancestry {B0, B1, B2}. Should only return one path.
		let paths = view.paths_via_relay_parent(&CHAIN_B[3]);
		assert_eq!(paths.len(), 1, "B3 should only be reachable via the main leaf");
		let path_via_main = &paths[0];
		assert_eq!(path_via_main.last(), Some(&main_leaf));
		assert_eq!(path_via_main.len(), 3);
		assert_eq!(path_via_main, &CHAIN_B[3..=5].to_vec());

		// All paths should be bounded by lookahead.
		for hash in &CHAIN_B[..6] {
			for path in view.paths_via_relay_parent(hash) {
				assert!(
					path.len() <= SCHEDULING_LOOKAHEAD as usize,
					"Path through {:?} has length {} > lookahead {}",
					hash,
					path.len(),
					SCHEDULING_LOOKAHEAD,
				);
			}
		}
	}
}
