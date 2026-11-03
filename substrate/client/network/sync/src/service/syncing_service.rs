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

use crate::types::{ExtendedPeerInfo, SyncEvent, SyncEventStream, SyncStatus, SyncStatusProvider};

use futures::{channel::oneshot, Stream};
use sc_network_types::PeerId;

use sc_consensus::{
	BlockImportError, BlockImportStatus, JustificationImportResult, JustificationSyncLink, Link,
};
use sc_network::{NetworkBlock, NetworkSyncForkRequest};
use sc_utils::mpsc::{tracing_unbounded, TracingUnboundedSender};
use sp_runtime::traits::{Block as BlockT, NumberFor};

use std::{
	collections::HashSet,
	pin::Pin,
	sync::{
		atomic::{AtomicBool, AtomicUsize, Ordering},
		Arc,
	},
};

/// Commands send to `SyncingEngine`
pub enum ToServiceCommand<B: BlockT> {
	SetSyncForkRequest(Vec<PeerId>, B::Hash, NumberFor<B>),
	RequestJustification(B::Hash, NumberFor<B>),
	ClearJustificationRequests,
	BlocksProcessed(
		usize,
		usize,
		Vec<(Result<BlockImportStatus<NumberFor<B>>, BlockImportError>, B::Hash)>,
	),
	JustificationImported(PeerId, B::Hash, NumberFor<B>, JustificationImportResult),
	AnnounceBlock(B::Hash, Option<Vec<u8>>),
	NewBestBlockImported(B::Hash, NumberFor<B>),
	EventStream(TracingUnboundedSender<SyncEvent>),
	Status(oneshot::Sender<SyncStatus<B>>),
	NumActivePeers(oneshot::Sender<usize>),
	NumDownloadedBlocks(oneshot::Sender<usize>),
	NumSyncRequests(oneshot::Sender<usize>),
	PeersInfo(oneshot::Sender<Vec<(PeerId, ExtendedPeerInfo<B>)>>),
	OnBlockFinalized(B::Hash, B::Header),
	/// Dynamically update the no-slot peer set. Does not modify the static no-slot peer set.
	SetNoSlotPeers(HashSet<PeerId>),
	// Status {
	// 	pending_response: oneshot::Sender<SyncStatus<B>>,
	// },
}

/// Handle for communicating with `SyncingEngine` asynchronously
#[derive(Clone)]
pub struct SyncingService<B: BlockT> {
	tx: TracingUnboundedSender<ToServiceCommand<B>>,
	/// Number of peers we're connected to.
	num_connected: Arc<AtomicUsize>,
	/// Are we actively catching up with the chain?
	is_major_syncing: Arc<AtomicBool>,
}

impl<B: BlockT> SyncingService<B> {
	/// Create new handle
	pub fn new(
		tx: TracingUnboundedSender<ToServiceCommand<B>>,
		num_connected: Arc<AtomicUsize>,
		is_major_syncing: Arc<AtomicBool>,
	) -> Self {
		Self { tx, num_connected, is_major_syncing }
	}

	/// Get the number of peers known to `SyncingEngine` (both full and light).
	pub fn num_connected_peers(&self) -> usize {
		self.num_connected.load(Ordering::Relaxed)
	}

	/// Get the number of active peers.
	pub async fn num_active_peers(&self) -> Result<usize, oneshot::Canceled> {
		let (tx, rx) = oneshot::channel();
		let _ = self.tx.unbounded_send(ToServiceCommand::NumActivePeers(tx));

		rx.await
	}

	/// Get the number of downloaded blocks.
	pub async fn num_downloaded_blocks(&self) -> Result<usize, oneshot::Canceled> {
		let (tx, rx) = oneshot::channel();
		let _ = self.tx.unbounded_send(ToServiceCommand::NumDownloadedBlocks(tx));

		rx.await
	}

	/// Get the number of sync requests.
	pub async fn num_sync_requests(&self) -> Result<usize, oneshot::Canceled> {
		let (tx, rx) = oneshot::channel();
		let _ = self.tx.unbounded_send(ToServiceCommand::NumSyncRequests(tx));

		rx.await
	}

	/// Get peer information.
	pub async fn peers_info(
		&self,
	) -> Result<Vec<(PeerId, ExtendedPeerInfo<B>)>, oneshot::Canceled> {
		let (tx, rx) = oneshot::channel();
		let _ = self.tx.unbounded_send(ToServiceCommand::PeersInfo(tx));

		rx.await
	}

	/// Notify the `SyncingEngine` that a block has been finalized.
	pub fn on_block_finalized(&self, hash: B::Hash, header: B::Header) {
		let _ = self.tx.unbounded_send(ToServiceCommand::OnBlockFinalized(hash, header));
	}

	/// Replace the dynamic set of no-slot peer set.
	///
	/// The engine maintains a per-role inbound-peer counter on top of the underlying network's
	/// reserved-peer handling (since <https://github.com/paritytech/substrate/pull/14603>) to ensure the
	/// `--in-peers` budget is enforced specifically for full peers in default notification set.
	/// A peer in either the static (config-time) or dynamic (this)
	/// no-slot set bypasses that counter, exactly mirroring what reserved-peer handling does at
	/// the network layer.
	///
	/// Each call replaces the dynamic set. The engine reconciles against its previous dynamic set.
	/// Peers added are promoted (taken off the inbound budget). Peers removed are demoted
	/// (counted again, or disconnected if doing so would now exceed `--in-peers`).
	///
	/// Callers that maintain a dynamic reserved-peer relationship (e.g. parachain
	/// collator-discovery) must call this in lockstep with `set_reserved_peers` using the same
	/// peer set, so the two layers stay in sync.
	pub fn set_no_slot_peers(&self, peers: HashSet<PeerId>) {
		let _ = self.tx.unbounded_send(ToServiceCommand::SetNoSlotPeers(peers));
	}

	/// Get sync status
	///
	/// Returns an error if `SyncingEngine` has terminated.
	pub async fn status(&self) -> Result<SyncStatus<B>, oneshot::Canceled> {
		let (tx, rx) = oneshot::channel();
		let _ = self.tx.unbounded_send(ToServiceCommand::Status(tx));

		rx.await
	}
}

impl<B: BlockT + 'static> NetworkSyncForkRequest<B::Hash, NumberFor<B>> for SyncingService<B> {
	/// Configure an explicit fork sync request.
	///
	/// Note that this function should not be used for recent blocks.
	/// Sync should be able to download all the recent forks normally.
	/// `set_sync_fork_request` should only be used if external code detects that there's
	/// a stale fork missing.
	///
	/// Passing empty `peers` set effectively removes the sync request.
	fn set_sync_fork_request(&self, peers: Vec<PeerId>, hash: B::Hash, number: NumberFor<B>) {
		let _ = self
			.tx
			.unbounded_send(ToServiceCommand::SetSyncForkRequest(peers, hash, number));
	}
}

impl<B: BlockT> JustificationSyncLink<B> for SyncingService<B> {
	/// Request a justification for the given block from the network.
	///
	/// On success, the justification will be passed to the import queue that was part at
	/// initialization as part of the configuration.
	fn request_justification(&self, hash: &B::Hash, number: NumberFor<B>) {
		let _ = self.tx.unbounded_send(ToServiceCommand::RequestJustification(*hash, number));
	}

	fn clear_justification_requests(&self) {
		let _ = self.tx.unbounded_send(ToServiceCommand::ClearJustificationRequests);
	}
}

#[async_trait::async_trait]
impl<B: BlockT> SyncStatusProvider<B> for SyncingService<B> {
	/// Get high-level view of the syncing status.
	async fn status(&self) -> Result<SyncStatus<B>, ()> {
		let (rtx, rrx) = oneshot::channel();

		let _ = self.tx.unbounded_send(ToServiceCommand::Status(rtx));
		rrx.await.map_err(|_| ())
	}
}

impl<B: BlockT> Link<B> for SyncingService<B> {
	fn blocks_processed(
		&self,
		imported: usize,
		count: usize,
		results: Vec<(Result<BlockImportStatus<NumberFor<B>>, BlockImportError>, B::Hash)>,
	) {
		let _ = self
			.tx
			.unbounded_send(ToServiceCommand::BlocksProcessed(imported, count, results));
	}

	fn justification_imported(
		&self,
		who: PeerId,
		hash: &B::Hash,
		number: NumberFor<B>,
		import_result: JustificationImportResult,
	) {
		let _ = self.tx.unbounded_send(ToServiceCommand::JustificationImported(
			who,
			*hash,
			number,
			import_result,
		));
	}

	fn request_justification(&self, hash: &B::Hash, number: NumberFor<B>) {
		let _ = self.tx.unbounded_send(ToServiceCommand::RequestJustification(*hash, number));
	}
}

impl<B: BlockT> SyncEventStream for SyncingService<B> {
	/// Get syncing event stream.
	fn event_stream(&self, name: &'static str) -> Pin<Box<dyn Stream<Item = SyncEvent> + Send>> {
		let (tx, rx) = tracing_unbounded(name, 100_000);
		let _ = self.tx.unbounded_send(ToServiceCommand::EventStream(tx));
		Box::pin(rx)
	}
}

impl<B: BlockT> NetworkBlock<B::Hash, NumberFor<B>> for SyncingService<B> {
	fn announce_block(&self, hash: B::Hash, data: Option<Vec<u8>>) {
		let _ = self.tx.unbounded_send(ToServiceCommand::AnnounceBlock(hash, data));
	}

	fn new_best_block_imported(&self, hash: B::Hash, number: NumberFor<B>) {
		let _ = self.tx.unbounded_send(ToServiceCommand::NewBestBlockImported(hash, number));
	}
}

impl<B: BlockT> sp_consensus::SyncOracle for SyncingService<B> {
	fn is_major_syncing(&self) -> bool {
		self.is_major_syncing.load(Ordering::Relaxed)
	}

	fn is_offline(&self) -> bool {
		self.num_connected.load(Ordering::Relaxed) == 0
	}
}
