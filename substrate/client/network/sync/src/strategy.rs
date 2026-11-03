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

//! [`SyncingStrategy`] defines an interface [`crate::engine::SyncingEngine`] uses as a specific
//! syncing algorithm.
//!
//! A few different strategies are provided by Substrate out of the box with custom strategies
//! possible too.

pub mod chain_sync;
mod disconnected_peers;
pub mod polkadot;
pub mod state;
pub mod state_sync;
pub mod warp;

use crate::{
	pending_responses::ResponseFuture,
	service::network::NetworkServiceHandle,
	types::{BadPeer, SyncStatus},
};
use sc_consensus::{BlockImportError, BlockImportStatus, IncomingBlock};
use sc_network::ProtocolName;
use sc_network_common::sync::message::BlockAnnounce;
use sc_network_types::PeerId;
use sp_blockchain::Error as ClientError;
use sp_consensus::BlockOrigin;
use sp_runtime::{
	traits::{Block as BlockT, NumberFor},
	Justifications,
};
use std::any::Any;

/// Syncing strategy for syncing engine to use
pub trait SyncingStrategy<B: BlockT>: Send
where
	B: BlockT,
{
	/// Notify syncing state machine that a new sync peer has connected.
	fn add_peer(&mut self, peer_id: PeerId, best_hash: B::Hash, best_number: NumberFor<B>);

	/// Notify that a sync peer has disconnected.
	fn remove_peer(&mut self, peer_id: &PeerId);

	/// Submit a validated block announcement.
	///
	/// Returns new best hash & best number of the peer if they are updated.
	#[must_use]
	fn on_validated_block_announce(
		&mut self,
		is_best: bool,
		peer_id: PeerId,
		announce: &BlockAnnounce<B::Header>,
	) -> Option<(B::Hash, NumberFor<B>)>;

	/// Configure an explicit fork sync request in case external code has detected that there is a
	/// stale fork missing.
	///
	/// Note that this function should not be used for recent blocks.
	/// Sync should be able to download all the recent forks normally.
	///
	/// Passing empty `peers` set effectively removes the sync request.
	fn set_sync_fork_request(&mut self, peers: Vec<PeerId>, hash: &B::Hash, number: NumberFor<B>);

	/// Request extra justification.
	fn request_justification(&mut self, hash: &B::Hash, number: NumberFor<B>);

	/// Clear extra justification requests.
	fn clear_justification_requests(&mut self);

	/// Report a justification import (successful or not).
	fn on_justification_import(&mut self, hash: B::Hash, number: NumberFor<B>, success: bool);

	/// Process generic response.
	///
	/// Strategy has to create opaque response and should be to downcast it back into concrete type
	/// internally. Failure to downcast is an implementation bug.
	fn on_generic_response(
		&mut self,
		peer_id: &PeerId,
		key: StrategyKey,
		protocol_name: ProtocolName,
		response: Box<dyn Any + Send>,
	);

	/// A batch of blocks that have been processed, with or without errors.
	///
	/// Call this when a batch of blocks that have been processed by the import queue, with or
	/// without errors.
	fn on_blocks_processed(
		&mut self,
		imported: usize,
		count: usize,
		results: Vec<(Result<BlockImportStatus<NumberFor<B>>, BlockImportError>, B::Hash)>,
	);

	/// Notify a syncing strategy that a block has been finalized.
	fn on_block_finalized(&mut self, hash: &B::Hash, number: NumberFor<B>);

	/// Inform sync about a new best imported block.
	fn update_chain_info(&mut self, best_hash: &B::Hash, best_number: NumberFor<B>);

	// Are we in major sync mode?
	fn is_major_syncing(&self) -> bool;

	/// Get the number of peers known to the syncing strategy.
	fn num_peers(&self) -> usize;

	/// Returns the current sync status.
	fn status(&self) -> SyncStatus<B>;

	/// Get the total number of downloaded blocks.
	fn num_downloaded_blocks(&self) -> usize;

	/// Get an estimate of the number of parallel sync requests.
	fn num_sync_requests(&self) -> usize;

	/// Get actions that should be performed by the owner on the strategy's behalf
	#[must_use]
	fn actions(
		&mut self,
		// TODO: Consider making this internal property of the strategy
		network_service: &NetworkServiceHandle,
	) -> Result<Vec<SyncingAction<B>>, ClientError>;
}

/// The key identifying a specific strategy for responses routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StrategyKey(&'static str);

impl StrategyKey {
	/// Instantiate opaque strategy key.
	pub const fn new(key: &'static str) -> Self {
		Self(key)
	}
}

pub enum SyncingAction<B: BlockT> {
	/// Start a request to a peer. Any previous request with the same peer and strategy key
	/// must have completed or been explicitly canceled.
	StartRequest { peer_id: PeerId, key: StrategyKey, request: ResponseFuture },
	/// Drop a pending response. The strategy must release the canceled request's bookkeeping
	/// before scheduling a replacement and emit this action before its `StartRequest`.
	CancelRequest { peer_id: PeerId, key: StrategyKey },
	/// Peer misbehaved. Disconnect, report it and cancel any requests to it.
	DropPeer(BadPeer),
	/// Import blocks.
	ImportBlocks { origin: BlockOrigin, blocks: Vec<IncomingBlock<B>> },
	/// Import justifications.
	ImportJustifications {
		peer_id: PeerId,
		hash: B::Hash,
		number: NumberFor<B>,
		justifications: Justifications,
	},
	/// Strategy finished. Nothing to do, this is handled by `PolkadotSyncingStrategy`.
	Finished,
}

// Note: Ideally we can deduce this information with #[derive(derive_more::Debug)].
// However, we'd need a bump to the latest version 2 of the crate.
impl<B> std::fmt::Debug for SyncingAction<B>
where
	B: BlockT,
{
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match &self {
			Self::StartRequest { peer_id, key, .. } => {
				write!(f, "StartRequest {{ peer_id: {:?}, key: {:?} }}", peer_id, key)
			},
			Self::CancelRequest { peer_id, key } => {
				write!(f, "CancelRequest {{ peer_id: {:?}, key: {:?} }}", peer_id, key)
			},
			Self::DropPeer(peer) => write!(f, "DropPeer({:?})", peer),
			Self::ImportBlocks { blocks, .. } => write!(f, "ImportBlocks({:?})", blocks),
			Self::ImportJustifications { hash, number, .. } => {
				write!(f, "ImportJustifications({:?}, {:?})", hash, number)
			},
			Self::Finished => write!(f, "Finished"),
		}
	}
}

impl<B: BlockT> SyncingAction<B> {
	/// Returns `true` if the syncing action has completed.
	pub fn is_finished(&self) -> bool {
		matches!(self, SyncingAction::Finished)
	}

	#[cfg(test)]
	pub(crate) fn name(&self) -> &'static str {
		match self {
			Self::StartRequest { .. } => "StartRequest",
			Self::CancelRequest { .. } => "CancelRequest",
			Self::DropPeer(_) => "DropPeer",
			Self::ImportBlocks { .. } => "ImportBlocks",
			Self::ImportJustifications { .. } => "ImportJustifications",
			Self::Finished => "Finished",
		}
	}
}
