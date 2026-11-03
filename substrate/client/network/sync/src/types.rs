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

//! Common syncing types.

use futures::Stream;
use sc_network_common::{role::Roles, types::ReputationChange};

use crate::strategy::{state_sync::StateSyncProgress, warp::WarpSyncProgress};

use sc_network_types::PeerId;
use sp_runtime::traits::{Block as BlockT, NumberFor};

use std::{fmt, pin::Pin, sync::Arc};

/// The sync status of a peer we are trying to sync with
#[derive(Debug)]
pub struct PeerInfo<Block: BlockT> {
	/// Their best block hash.
	pub best_hash: Block::Hash,
	/// Their best block number.
	pub best_number: NumberFor<Block>,
}

/// Info about a peer's known state (both full and light).
#[derive(Debug)]
pub struct ExtendedPeerInfo<B: BlockT> {
	/// Roles
	pub roles: Roles,
	/// Peer best block hash
	pub best_hash: B::Hash,
	/// Peer best block number
	pub best_number: NumberFor<B>,
}

impl<B> Clone for ExtendedPeerInfo<B>
where
	B: BlockT,
{
	fn clone(&self) -> Self {
		Self { roles: self.roles, best_hash: self.best_hash, best_number: self.best_number }
	}
}

impl<B> Copy for ExtendedPeerInfo<B> where B: BlockT {}

/// Reported sync state.
#[derive(Clone, Eq, PartialEq, Debug)]
pub enum SyncState<BlockNumber> {
	/// Initial sync is complete, keep-up sync is active.
	Idle,
	/// Actively catching up with the chain.
	Downloading { target: BlockNumber },
	/// All blocks are downloaded and are being imported.
	Importing { target: BlockNumber },
}

impl<BlockNumber> SyncState<BlockNumber> {
	/// Are we actively catching up with the chain?
	pub fn is_major_syncing(&self) -> bool {
		!matches!(self, SyncState::Idle)
	}
}

/// Syncing status and statistics.
#[derive(Debug, Clone)]
pub struct SyncStatus<Block: BlockT> {
	/// Current global sync state.
	pub state: SyncState<NumberFor<Block>>,
	/// Target sync block number.
	pub best_seen_block: Option<NumberFor<Block>>,
	/// Number of peers participating in syncing.
	pub num_peers: u32,
	/// Number of blocks queued for import
	pub queued_blocks: u32,
	/// State sync status in progress, if any.
	pub state_sync: Option<StateSyncProgress>,
	/// Warp sync in progress, if any.
	pub warp_sync: Option<WarpSyncProgress<Block>>,
}

/// A peer did not behave as expected and should be reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadPeer(pub PeerId, pub ReputationChange);

impl fmt::Display for BadPeer {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		write!(f, "Bad peer {}; Reputation change: {:?}", self.0, self.1)
	}
}

impl std::error::Error for BadPeer {}

/// Provides high-level status of syncing.
#[async_trait::async_trait]
pub trait SyncStatusProvider<Block: BlockT>: Send + Sync {
	/// Get high-level view of the syncing status.
	async fn status(&self) -> Result<SyncStatus<Block>, ()>;
}

#[async_trait::async_trait]
impl<T, Block> SyncStatusProvider<Block> for Arc<T>
where
	T: ?Sized,
	T: SyncStatusProvider<Block>,
	Block: BlockT,
{
	async fn status(&self) -> Result<SyncStatus<Block>, ()> {
		T::status(self).await
	}
}

/// Syncing-related events that other protocols can subscribe to.
pub enum SyncEvent {
	/// Peer that the syncing implementation is tracking connected.
	PeerConnected {
		/// The peer that connected.
		peer_id: PeerId,
		/// The roles the peer advertised in its handshake.
		roles: Roles,
	},

	/// Peer that the syncing implementation was tracking disconnected.
	PeerDisconnected(PeerId),
}

pub trait SyncEventStream: Send + Sync {
	/// Subscribe to syncing-related events.
	fn event_stream(&self, name: &'static str) -> Pin<Box<dyn Stream<Item = SyncEvent> + Send>>;
}

impl<T> SyncEventStream for Arc<T>
where
	T: ?Sized,
	T: SyncEventStream,
{
	fn event_stream(&self, name: &'static str) -> Pin<Box<dyn Stream<Item = SyncEvent> + Send>> {
		T::event_stream(self, name)
	}
}
