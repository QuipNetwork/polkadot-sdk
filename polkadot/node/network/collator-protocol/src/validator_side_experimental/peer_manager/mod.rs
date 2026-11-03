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
mod backend;
mod connected;
mod db;
mod persistence;
mod persistent_db;

use futures::channel::oneshot;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::{
	validator_side_experimental::{
		common::{
			PeerInfo, Score, TryAcceptOutcome, CONNECTED_PEERS_LIMIT, CONNECTED_PEERS_PARA_LIMIT,
			INACTIVITY_DECAY, MAX_STARTUP_ANCESTRY_LOOKBACK, VALID_INCLUDED_CANDIDATE_BUMP,
		},
		error::{Error, JfyiError, Result},
	},
	validator_side_metrics::{Metrics, ScoreBand},
	LOG_TARGET,
};
pub use backend::Backend;
use connected::ConnectedPeers;
#[cfg(test)]
pub use db::Db;
pub use persistence::PersistenceError;
pub use persistent_db::PersistentDb;
use polkadot_node_network_protocol::{
	peer_set::{CollationVersion, PeerSet},
	PeerId,
};
use polkadot_node_subsystem::{
	messages::{ChainApiMessage, NetworkBridgeTxMessage},
	CollatorProtocolSenderTrait, RuntimeApiError,
};
use polkadot_node_subsystem_util::{
	request_candidate_events, request_candidates_pending_availability, request_para_ids,
	runtime::{self, recv_runtime},
};
use polkadot_primitives::{
	BlockNumber, CandidateDescriptorVersion, CandidateEvent, CandidateHash, Hash, Id as ParaId,
	SessionIndex,
};

#[derive(Debug, PartialEq, Clone)]
pub struct ReputationUpdate {
	pub peer_id: PeerId,
	pub para_id: ParaId,
	pub value: Score,
	pub kind: ReputationUpdateKind,
}

#[derive(Debug, PartialEq, Clone)]
pub enum ReputationUpdateKind {
	Bump,
	Slash,
}

#[derive(Debug, PartialEq)]
enum DeclarationOutcome {
	Rejected,
	Switched(ParaId),
	Accepted,
}

pub struct PeerManager<B> {
	db: B,
	connected: ConnectedPeers,
	/// The `SessionIndex` of the last finalized block
	latest_finalized_session: Option<SessionIndex>,
	/// Clock used for reputation timestamps (passed to `Backend::process_bumps`).
	clock: std::sync::Arc<dyn polkadot_node_clock::Clock>,
	metrics: Metrics,
}

impl<B: Backend> PeerManager<B> {
	/// Initialize the peer manager (called on subsystem startup, after the node finished syncing to
	/// the tip of the chain).
	pub async fn startup<Sender: CollatorProtocolSenderTrait>(
		backend: B,
		sender: &mut Sender,
		scheduled_paras: BTreeSet<ParaId>,
		clock: std::sync::Arc<dyn polkadot_node_clock::Clock>,
		metrics: Metrics,
	) -> Result<Self> {
		let mut instance = Self {
			db: backend,
			connected: ConnectedPeers::new(
				scheduled_paras,
				CONNECTED_PEERS_LIMIT,
				CONNECTED_PEERS_PARA_LIMIT,
			),
			latest_finalized_session: None,
			clock,
			metrics,
		};

		let (latest_finalized_block_number, latest_finalized_block_hash) =
			get_latest_finalized_block(sender).await?;

		let processed_finalized_block_number =
			instance.db.processed_finalized_block_number().await.unwrap_or_default();

		gum::trace!(
			target: LOG_TARGET,
			scheduled_paras = ?instance.connected.scheduled_paras().collect::<Vec<_>>(),
			latest_finalized_block_number,
			?latest_finalized_block_hash,
			processed_finalized_block_number,
			"PeerManager startup"
		);

		let bumps = extract_reputation_bumps_on_new_finalized_block(
			sender,
			processed_finalized_block_number,
			(latest_finalized_block_number, latest_finalized_block_hash),
			&instance.metrics,
		)
		.await?;

		instance
			.db
			.process_bumps(
				latest_finalized_block_number,
				bumps,
				None,
				instance.clock.duration_since_epoch(),
			)
			.await;

		if latest_finalized_block_number != processed_finalized_block_number {
			gum::trace!(
				target: LOG_TARGET,
				blocks_processed = std::cmp::min(
					latest_finalized_block_number.saturating_sub(processed_finalized_block_number),
					MAX_STARTUP_ANCESTRY_LOOKBACK
				),
				"Startup lookback completed"
			);
		}

		Ok(instance)
	}

	/// Handle a new block finality notification, by updating peer reputations.
	pub async fn update_reputations_on_new_finalized_block<Sender: CollatorProtocolSenderTrait>(
		&mut self,
		sender: &mut Sender,
		(finalized_block_hash, finalized_block_number): (Hash, BlockNumber),
	) -> Result<()> {
		let processed_finalized_block_number =
			self.db.processed_finalized_block_number().await.unwrap_or_default();

		let bumps = extract_reputation_bumps_on_new_finalized_block(
			sender,
			processed_finalized_block_number,
			(finalized_block_number, finalized_block_hash),
			&self.metrics,
		)
		.await?;

		let now = self.clock.duration_since_epoch();
		let updates = self
			.db
			.process_bumps(finalized_block_number, bumps, Some(Score::new(INACTIVITY_DECAY)), now)
			.await;

		if !updates.is_empty() {
			gum::debug!(
				target: LOG_TARGET,
				finalized_block_number,
				?finalized_block_hash,
				num_updates = updates.len(),
				"Applying reputation updates on new finalized block",
			);
		}

		for update in updates {
			gum::trace!(
				target: LOG_TARGET,
				finalized_block_number,
				?update,
				"Applying reputation update",
			);
			self.connected.update_reputation(update);
		}

		Ok(())
	}

	/// Process the registered paras and cleanup all data pertaining to any unregistered paras, if
	/// any. Should be called every finalized block. Only queries the registered paras once per
	/// session since they can only change at session boundaries.
	pub async fn prune_registered_paras<Sender: CollatorProtocolSenderTrait>(
		&mut self,
		sender: &mut Sender,
		finalized_session: SessionIndex,
		finalized_hash: Hash,
	) {
		let needs_update = self
			.latest_finalized_session
			.map(|last_stored| last_stored < finalized_session)
			.unwrap_or(true);

		if !needs_update {
			return;
		}

		self.latest_finalized_session = Some(finalized_session);

		let registered_paras = match recv_runtime(
			request_para_ids(finalized_hash, finalized_session, sender).await,
		)
		.await
		{
			Ok(registered_paras) => registered_paras.into_iter().collect(),
			Err(runtime::Error::RuntimeRequest(RuntimeApiError::NotSupported { .. })) => {
				gum::warn!(
					target: LOG_TARGET,
					"Using a runtime which does not support querying the registered paras, this should not be used in production with the `--enable-experimental-collator-protocol` flag."
				);
				return;
			},
			Err(err) => {
				JfyiError::Runtime(err).log();
				return;
			},
		};

		// Tell the DB to cleanup paras that are no longer registered. No need to clean
		// up the connected peers state, since it will get automatically cleaned up
		// as the claim queue gets rid of these stale assignments.
		self.db.prune_paras(registered_paras).await;
	}

	/// Process a potential change of the scheduled paras. Returns a record of the disconnected
	/// peers.
	pub async fn scheduled_paras_update<Sender: CollatorProtocolSenderTrait>(
		&mut self,
		sender: &mut Sender,
		scheduled_paras: BTreeSet<ParaId>,
	) -> HashSet<PeerId> {
		let prev_scheduled_paras: BTreeSet<_> = self.connected.scheduled_paras().copied().collect();

		if prev_scheduled_paras == scheduled_paras {
			// Nothing to do if the scheduled paras didn't change.
			return HashSet::new();
		}

		// Recreate the connected peers based on the new schedule and try populating it again based
		// on their reputations. Disconnect any peers that couldn't be kept
		let mut new_instance =
			ConnectedPeers::new(scheduled_paras, CONNECTED_PEERS_LIMIT, CONNECTED_PEERS_PARA_LIMIT);

		std::mem::swap(&mut new_instance, &mut self.connected);
		let prev_instance = new_instance;
		let (prev_peers, cached_scores) = prev_instance.consume();

		// Build a closure that can be used to first query the in-memory past reputations of the
		// peers before reaching for the DB.

		// Borrow these for use in the closure.
		let cached_scores = &cached_scores;
		let db = &self.db;
		let reputation_query_fn = |peer_id: PeerId, para_id: ParaId| async move {
			if let Some(cached_score) =
				cached_scores.get(&para_id).and_then(|per_para| per_para.get_score(&peer_id))
			{
				cached_score
			} else {
				db.query(&peer_id, &para_id).await.unwrap_or_default()
			}
		};

		// See which of the old peers we should keep.
		let mut peers_to_disconnect = HashSet::new();
		for (peer_id, peer_info) in prev_peers {
			let outcome = self.connected.try_accept(reputation_query_fn, peer_id, peer_info).await;

			match outcome {
				TryAcceptOutcome::Rejected => {
					peers_to_disconnect.insert(peer_id);
				},
				TryAcceptOutcome::Replaced(replaced_peer_ids) => {
					peers_to_disconnect.extend(replaced_peer_ids);
				},
				TryAcceptOutcome::Added => {},
			}
		}

		// Disconnect peers that couldn't be kept.
		self.disconnect_peers(sender, peers_to_disconnect.clone().into_iter()).await;

		peers_to_disconnect
	}

	/// Process a declaration message of a peer.
	///
	/// Returns a `bool` indicating whether the operation was successful.
	pub async fn declared<Sender: CollatorProtocolSenderTrait>(
		&mut self,
		sender: &mut Sender,
		peer_id: PeerId,
		para_id: ParaId,
	) -> bool {
		if self.connected.peer_info(&peer_id).is_none() {
			return false;
		}

		let outcome = self.connected.declared(peer_id, para_id);
		match outcome {
			DeclarationOutcome::Accepted => {
				gum::debug!(
					target: LOG_TARGET,
					?para_id,
					?peer_id,
					"Peer declared",
				);
				true
			},
			DeclarationOutcome::Switched(old_para_id) => {
				gum::debug!(
					target: LOG_TARGET,
					?para_id,
					?old_para_id,
					?peer_id,
					"Peer switched collating paraid. Rejected.",
				);
				self.disconnect_peers(sender, [peer_id].into_iter()).await;
				false
			},
			DeclarationOutcome::Rejected => {
				gum::debug!(
					target: LOG_TARGET,
					?para_id,
					?peer_id,
					"Peer declared but rejected. Going to disconnect.",
				);

				self.disconnect_peers(sender, [peer_id].into_iter()).await;
				false
			},
		}
	}

	/// Slash a peer's reputation for this paraid.
	pub async fn slash_reputation(&mut self, peer_id: &PeerId, para_id: &ParaId, value: Score) {
		gum::debug!(
			target: LOG_TARGET,
			?peer_id,
			?para_id,
			?value,
			"Slashing peer's reputation",
		);

		self.db.slash(peer_id, para_id, value).await;
		self.connected.update_reputation(ReputationUpdate {
			peer_id: *peer_id,
			para_id: *para_id,
			value,
			kind: ReputationUpdateKind::Slash,
		});
	}

	/// Process a peer disconnected event coming from the network.
	pub fn disconnected(&mut self, peer_id: &PeerId) {
		self.connected.remove(peer_id);
	}

	/// A connection was made, triage it.
	pub async fn try_accept_connection<Sender: CollatorProtocolSenderTrait>(
		&mut self,
		sender: &mut Sender,
		peer_id: PeerId,
		peer_info: PeerInfo,
	) -> TryAcceptOutcome {
		let db = &self.db;
		let reputation_query_fn = |peer_id: PeerId, para_id: ParaId| async move {
			// Go straight to the DB. We only store in-memory the reputations of connected peers.
			db.query(&peer_id, &para_id).await.unwrap_or_default()
		};

		let outcome = self.connected.try_accept(reputation_query_fn, peer_id, peer_info).await;
		match outcome {
			TryAcceptOutcome::Added => TryAcceptOutcome::Added,
			TryAcceptOutcome::Replaced(other_peers) => {
				self.disconnect_peers(sender, other_peers.clone().into_iter()).await;
				TryAcceptOutcome::Replaced(other_peers)
			},
			TryAcceptOutcome::Rejected => {
				self.disconnect_peers(sender, [peer_id].into_iter()).await;
				TryAcceptOutcome::Rejected
			},
		}
	}

	/// Retrieve the score of the connected peer. We assume the peer is declared for this paraid.
	pub fn connected_peer_score(&self, peer_id: &PeerId, para_id: &ParaId) -> Option<Score> {
		self.connected.peer_score(peer_id, para_id)
	}

	/// The number of declared collators per para, grouped by score band. See
	/// [`ConnectedPeers::score_distribution`].
	pub fn score_distribution(&self) -> BTreeMap<ParaId, BTreeMap<ScoreBand, u64>> {
		self.connected.score_distribution()
	}

	/// Retrieve the peer info associated to this PeerId, if any.
	pub fn peer_info(&self, peer_id: &PeerId) -> Option<&PeerInfo> {
		self.connected.peer_info(peer_id)
	}

	/// Retrieve the max scores for the given paras.
	pub async fn max_scores_for_paras(&self, paras: BTreeSet<ParaId>) -> HashMap<ParaId, Score> {
		self.db.max_scores_for_paras(paras).await
	}

	/// Returns the number of connected peers.
	pub fn connected_peer_count(&self) -> usize {
		self.connected.len()
	}

	#[cfg(test)]
	pub fn connected_peers(&self) -> BTreeSet<PeerId> {
		self.connected.clone().consume().0.into_keys().collect()
	}

	#[cfg(test)]
	pub async fn processed_finalized_block_number(&self) -> Option<BlockNumber> {
		self.db.processed_finalized_block_number().await
	}

	async fn disconnect_peers<Sender: CollatorProtocolSenderTrait>(
		&self,
		sender: &mut Sender,
		peers: impl Iterator<Item = PeerId>,
	) {
		let peers: Vec<_> = peers.collect();
		if peers.is_empty() {
			return;
		}
		gum::trace!(
			target: LOG_TARGET,
			?peers,
			"Disconnecting peers",
		);

		sender
			.send_message(NetworkBridgeTxMessage::DisconnectPeers(peers, PeerSet::Collation))
			.await;
	}

	pub fn get_peer_protocol_version(&self, peer_id: &PeerId) -> Option<CollationVersion> {
		self.connected.get_version(peer_id)
	}
}

impl PeerManager<PersistentDb> {
	/// Persist the reputation database to disk asynchronously (fire-and-forget).
	pub fn persist_to_disk_async(&mut self) {
		self.db.persist_async(None);
	}

	/// Persist and wait for the background writer to finish the write.
	pub async fn persist_and_wait(&mut self) {
		if self.db.persist_and_wait().await.is_err() {
			gum::error!(
				target: LOG_TARGET,
				"Failed to persist reputation DB: background writer closed"
			);
		}
	}
}

async fn get_ancestors<Sender: CollatorProtocolSenderTrait>(
	sender: &mut Sender,
	k: usize,
	hash: Hash,
) -> Result<Vec<Hash>> {
	let (tx, rx) = oneshot::channel();
	sender
		.send_message(ChainApiMessage::Ancestors { hash, k, response_channel: tx })
		.await;

	Ok(rx.await.map_err(|_| Error::CanceledAncestors)??)
}

async fn get_latest_finalized_block<Sender: CollatorProtocolSenderTrait>(
	sender: &mut Sender,
) -> Result<(BlockNumber, Hash)> {
	let (tx, rx) = oneshot::channel();
	sender.send_message(ChainApiMessage::FinalizedBlockNumber(tx)).await;

	let block_number = rx.await.map_err(|_| Error::CanceledFinalizedBlockNumber)??;

	let (tx, rx) = oneshot::channel();
	sender.send_message(ChainApiMessage::FinalizedBlockHash(block_number, tx)).await;

	let block_hash = rx
		.await
		.map_err(|_| Error::CanceledFinalizedBlockHash)??
		.ok_or_else(|| Error::FinalizedBlockNotFound(block_number))?;

	Ok((block_number, block_hash))
}

async fn extract_reputation_bumps_on_new_finalized_block<Sender: CollatorProtocolSenderTrait>(
	sender: &mut Sender,
	processed_finalized_block_number: BlockNumber,
	(latest_finalized_block_number, latest_finalized_block_hash): (BlockNumber, Hash),
	metrics: &Metrics,
) -> Result<BTreeMap<ParaId, HashMap<PeerId, Score>>> {
	if latest_finalized_block_number < processed_finalized_block_number {
		// Shouldn't be possible, but in this case there is no other initialisation needed.
		gum::warn!(
			target: LOG_TARGET,
			latest_finalized_block_number,
			?latest_finalized_block_hash,
			"Peer manager stored finalized block number {} is higher than the latest finalized block.",
			processed_finalized_block_number,
		);
		return Ok(BTreeMap::new());
	}

	let ancestry_len = std::cmp::min(
		latest_finalized_block_number.saturating_sub(processed_finalized_block_number),
		MAX_STARTUP_ANCESTRY_LOOKBACK,
	);

	if ancestry_len == 0 {
		return Ok(BTreeMap::new());
	}

	let mut ancestors =
		get_ancestors(sender, ancestry_len as usize, latest_finalized_block_hash).await?;
	ancestors.reverse();
	ancestors.push(latest_finalized_block_hash);

	gum::trace!(
		target: LOG_TARGET,
		?latest_finalized_block_hash,
		processed_finalized_block_number,
		ancestors_len = ancestors.len(),
		"Processing reputation bumps for finalized relay parent {} and its {} ancestors",
		latest_finalized_block_number,
		ancestry_len
	);

	let mut v2_candidates_per_rp: HashMap<Hash, BTreeMap<ParaId, HashSet<CandidateHash>>> =
		HashMap::with_capacity(ancestors.len());

	for i in 1..ancestors.len() {
		let rp = ancestors[i];
		let parent_rp = ancestors[i - 1];

		gum::trace!(
			target: LOG_TARGET,
			relay_parent=?rp,
			"request_candidate_events"
		);

		let candidate_events = match recv_runtime(request_candidate_events(rp, sender).await).await
		{
			Ok(candidate_events) => candidate_events,
			Err(e) => {
				gum::trace!(
					target: LOG_TARGET,
					relay_parent=?rp,
					err=?e,
					"Error fetching candidate events for reputation bump extraction"
				);
				// `ancestors` are reversed so their order is from oldest to newest
				// (`get_ancestors()` returns newest -> oldest, but they are reversed after that).
				// So if this runtime call fails, the next one has got a better chance in
				// succeeding.
				continue;
			},
		};

		for event in candidate_events {
			if let CandidateEvent::CandidateIncluded(receipt, _, _, _) = event {
				// Only v2+ receipts can contain UMP signals.
				// Assuming node feature set here is fine, misinterpretations are harmless in this
				// context:
				let has_ump_signals = match receipt.descriptor.version() {
					CandidateDescriptorVersion::V1 => false,
					_ => true,
				};
				if has_ump_signals {
					v2_candidates_per_rp
						.entry(parent_rp)
						.or_default()
						.entry(receipt.descriptor.para_id())
						.or_default()
						.insert(receipt.hash());
				}
			}
		}
	}

	// This could be removed if we implemented https://github.com/paritytech/polkadot-sdk/issues/7732.
	let mut updates: BTreeMap<ParaId, HashMap<PeerId, Score>> = BTreeMap::new();
	for (rp, per_para) in v2_candidates_per_rp {
		for (para_id, included_candidates) in per_para {
			let candidates_pending_availability = match recv_runtime(
				request_candidates_pending_availability(rp, para_id, sender).await,
			)
			.await
			{
				Ok(c) => c,
				Err(err) => {
					gum::warn!(target: LOG_TARGET,
					relay_parent=?rp,
					?para_id,
					?err,
					"Failed to get candidates pending availability. Skipping score bumps for the para id",
					);
					continue;
				},
			};

			for candidate in candidates_pending_availability {
				let candidate_hash = candidate.hash();
				if included_candidates.contains(&candidate_hash) {
					match candidate.commitments.ump_signals() {
						Ok(ump_signals) => {
							if let Some(approved_peer) = ump_signals.approved_peer() {
								match PeerId::from_bytes(approved_peer) {
									Ok(peer_id) => {
										metrics.on_approved_peer_present(&para_id);
										updates
											.entry(para_id)
											.or_default()
											.entry(peer_id)
											.or_default()
											.saturating_add(VALID_INCLUDED_CANDIDATE_BUMP);
									},
									Err(err) => {
										// Collator sent an invalid peerid. It's only harming
										// itself.
										metrics.on_approved_peer_invalid(&para_id);
										gum::debug!(
											target: LOG_TARGET,
											?candidate_hash,
											"UMP signal contains invalid ApprovedPeer id: {}",
											err
										);
									},
								}
							} else {
								// v2+ candidate with no `ApprovedPeer` signal — the parachain has
								// likely not upgraded to emit it.
								metrics.on_approved_peer_absent(&para_id);
							}
						},
						Err(err) => {
							// This should never happen, as the ump signals are checked during
							// on-chain backing.
							metrics.on_approved_peer_parse_error(&para_id);
							gum::warn!(
								target: LOG_TARGET,
								?candidate_hash,
								"Failed to parse UMP signals for included candidate: {}",
								err
							);
						},
					}
				}
			}
		}
	}

	Ok(updates)
}
