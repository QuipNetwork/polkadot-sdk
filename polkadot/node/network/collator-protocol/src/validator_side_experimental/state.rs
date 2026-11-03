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

use crate::{
	validator_side::notify_collation_seconded,
	validator_side_experimental::{
		collation_manager::{AdvertisementError, CollationManager},
		common::{
			CanSecond, CollationFetchResponse, PeerInfo, PeerState, ProspectiveCandidate,
			TryAcceptOutcome, INVALID_COLLATION_SLASH,
		},
		error::{Error, FatalResult},
		peer_manager::{Backend, PersistentDb},
		Metrics, PeerManager,
	},
	validator_side_metrics::TimedHandler,
	LOG_TARGET,
};
use fatality::Split;
use futures::{channel::oneshot, stream::FusedStream};
use polkadot_node_network_protocol::{peer_set::CollationVersion, OurView, PeerId};
use polkadot_node_primitives::{SignedFullStatement, Statement};
use polkadot_node_subsystem::{
	messages::{
		CandidateBackingMessage, IfDisconnected, KnownOutputHeads, NetworkBridgeTxMessage,
		ProspectiveParachainsMessage,
	},
	CollatorProtocolSenderTrait,
};
use polkadot_node_subsystem_util::{request_session_index_for_child, runtime::recv_runtime};
use polkadot_primitives::{
	BlockNumber, CandidateDescriptorVersion, CandidateReceiptV2 as CandidateReceipt, Hash,
	Id as ParaId,
};
use std::time::Duration;

/// All state relevant for the validator side of the protocol lives here.
pub struct State<B> {
	peer_manager: PeerManager<B>,
	collation_manager: CollationManager,
	metrics: Metrics,
}

impl<B: Backend> State<B> {
	/// Instantiate a new subsystem `State`.
	pub fn new(
		peer_manager: PeerManager<B>,
		collation_manager: CollationManager,
		metrics: Metrics,
	) -> Self {
		Self { peer_manager, collation_manager, metrics }
	}

	/// Access the metrics.
	pub fn metrics(&self) -> &Metrics {
		&self.metrics
	}

	/// Publish the current size of the in-memory connected peers store.
	pub fn note_in_memory_connected_peers(&self) {
		self.metrics
			.note_in_memory_connected_peers(self.peer_manager.connected_peer_count());
	}

	/// Handle a new peer connection.
	pub async fn handle_peer_connected<Sender: CollatorProtocolSenderTrait>(
		&mut self,
		sender: &mut Sender,
		peer_id: PeerId,
		version: CollationVersion,
	) {
		let _timer = self.metrics.time_handler(TimedHandler::PeerConnected);

		let outcome = self
			.peer_manager
			.try_accept_connection(
				sender,
				peer_id,
				PeerInfo { version, state: PeerState::Connected },
			)
			.await;
		match outcome {
			TryAcceptOutcome::Added => {
				gum::trace!(
					target: LOG_TARGET,
					?peer_id,
					?version,
					"Peer connected",
				);
			},
			TryAcceptOutcome::Replaced(other_peers) => {
				gum::trace!(
					target: LOG_TARGET,
					?peer_id,
					?version,
					?other_peers,
					"Peer connected and replaced the connection slots of other peers",
				);
				self.collation_manager.remove_peers(other_peers.iter());
			},
			TryAcceptOutcome::Rejected => {
				gum::debug!(
					target: LOG_TARGET,
					?peer_id,
					?version,
					"Peer connection was rejected. Going to disconnect",
				);
			},
		}

		self.metrics.note_collator_peer_count(self.peer_manager.connected_peer_count());
	}

	/// Handle a peer disconnection.
	pub async fn handle_peer_disconnected(&mut self, peer_id: PeerId) {
		let _timer = self.metrics.time_handler(TimedHandler::PeerDisconnected);

		gum::trace!(
			target: LOG_TARGET,
			?peer_id,
			"Peer disconnected",
		);

		self.peer_manager.disconnected(&peer_id);

		self.collation_manager.remove_peer(&peer_id);

		self.metrics.note_collator_peer_count(self.peer_manager.connected_peer_count());
	}

	/// Handle a peer's declaration message.
	/// V4 peers do not declare anymore.
	pub async fn handle_declare<Sender: CollatorProtocolSenderTrait>(
		&mut self,
		sender: &mut Sender,
		peer_id: PeerId,
		para_id: ParaId,
	) {
		let _timer = self.metrics.time_handler(TimedHandler::Declare);

		if !self.peer_manager.declared(sender, peer_id, para_id).await {
			self.collation_manager.remove_peer(&peer_id);
		}

		self.metrics.note_collator_peer_count(self.peer_manager.connected_peer_count());
	}

	/// Handle our view update.
	pub async fn handle_our_view_change<Sender: CollatorProtocolSenderTrait>(
		&mut self,
		sender: &mut Sender,
		new_view: OurView,
	) -> FatalResult<()> {
		let _timer = self.metrics.time_handler(TimedHandler::OurViewChange);

		gum::trace!(
			target: LOG_TARGET,
			?new_view,
			"Handling our view change",
		);
		let old_assignments = self.collation_manager.assignments();

		self.collation_manager.update_view(sender, new_view).await?;

		let new_assignments = self.collation_manager.assignments();
		gum::trace!(
			target: LOG_TARGET,
			?old_assignments,
			?new_assignments,
			"Old assignments vs new assignments",
		);

		if old_assignments != new_assignments {
			gum::debug!(
				target: LOG_TARGET,
				?old_assignments,
				?new_assignments,
				"Collator protocol assignments changed",
			);
		}

		self.metrics.note_assigned_paras(new_assignments.len());

		let maybe_disconnected_peers =
			self.peer_manager.scheduled_paras_update(sender, new_assignments).await;

		if !maybe_disconnected_peers.is_empty() {
			gum::trace!(
				target: LOG_TARGET,
				?maybe_disconnected_peers,
				"Disconnecting peers due to our view change",
			);
		}

		self.collation_manager.remove_peers(maybe_disconnected_peers.iter());
		self.metrics.note_collator_peer_count(self.peer_manager.connected_peer_count());

		Ok(())
	}

	/// Handle a finalized block notification.
	pub async fn handle_finalized_block<Sender: CollatorProtocolSenderTrait>(
		&mut self,
		sender: &mut Sender,
		hash: Hash,
		number: BlockNumber,
	) -> FatalResult<()> {
		let _timer = self.metrics.time_handler(TimedHandler::FinalizedBlock);

		gum::trace!(
			target: LOG_TARGET,
			?hash,
			number,
			"Processing new block finality notification",
		);

		// Process reputation bumps
		if let Err(err) = self
			.peer_manager
			.update_reputations_on_new_finalized_block(sender, (hash, number))
			.await
		{
			err.split()?.log();
		}

		// Refresh the per-para reputation score distribution metric
		self.metrics.note_score_distribution(&self.peer_manager.score_distribution());

		// Process potential changes in the registered paras set.
		let session_index = match recv_runtime(request_session_index_for_child(hash, sender).await)
			.await
			.map_err(Error::Runtime)
		{
			Ok(session_index) => session_index,
			Err(err) => {
				err.split()?.log();
				return Ok(());
			},
		};

		self.peer_manager.prune_registered_paras(sender, session_index, hash).await;

		Ok(())
	}

	/// Handle a new advertisement.
	pub async fn handle_advertisement<Sender: CollatorProtocolSenderTrait>(
		&mut self,
		sender: &mut Sender,
		peer_id: PeerId,
		scheduling_parent: Hash,
		entries: Vec<ProspectiveCandidate>,
		descriptor_version: Option<CandidateDescriptorVersion>,
		// Some for V4 self declaring ad.
		advertised_para_id: Option<ParaId>,
	) {
		let advertisement_log = if advertised_para_id.is_some() {
			"Received a segment advertisement"
		} else {
			"Received advertisement"
		};
		let _timer = self.metrics.time_handler(TimedHandler::Advertisement);

		gum::debug!(
			target: LOG_TARGET,
			?scheduling_parent,
			?peer_id,
			advertisement_log,
		);

		if let Some(para_id) = advertised_para_id {
			if entries.is_empty() {
				gum::debug!(
					target: LOG_TARGET,
					?scheduling_parent,
					?peer_id,
					?para_id,
					"Received an empty segment advertisement",
				);
				self.metrics.on_advertisement_rejected_malformed_segment(&para_id);
				return;
			}
			// A zero-len cycle is impossible for an honest block.
			if entries.iter().any(|prospective_candidate| {
				Some(prospective_candidate.parent_head_data_hash()) ==
					prospective_candidate.output_head_data_hash()
			}) {
				gum::debug!(
					target: LOG_TARGET,
					?scheduling_parent,
					?peer_id,
					?para_id,
					"Received a segment advertisement with a zero-length cycle",
				);
				self.metrics.on_advertisement_rejected_malformed_segment(&para_id);
				return;
			}

			// V4 has no `Declare`: a peer's first advertisement carries its para and binds it.
			// Until then a V4 peer holds a reserved slot on every scheduled para; binding here
			// releases the slots it held on all the other paras.
			if !self.peer_manager.declared(sender, peer_id, para_id).await {
				self.collation_manager.remove_peer(&peer_id);
				return;
			}
		}

		let Some(PeerInfo { state, .. }) = self.peer_manager.peer_info(&peer_id) else {
			self.metrics.on_advertisement_rejected_unconnected_peer();
			gum::warn!(
				target: LOG_TARGET,
				?scheduling_parent,
				?peer_id,
				"Received an advertisement from an unconnected peer"
			);
			return;
		};

		// Advertised without being declared. Not a big waste of our time, so ignore it.
		let PeerState::Collating(para_id) = state else {
			self.metrics.on_advertisement_rejected_undeclared_peer();
			gum::debug!(
				target: LOG_TARGET,
				?scheduling_parent,
				?peer_id,
				"Received advertisement for undeclared peer",
			);
			return;
		};

		// We have a result here, but it's not worth affecting reputations because advertisements
		// are cheap.
		// Note: `try_accept_segment` involves two other subsystems, so it's not super cheap,
		// actually, but cheap enough.
		match self
			.collation_manager
			.try_accept_segment(
				sender,
				peer_id,
				*para_id,
				scheduling_parent,
				descriptor_version,
				entries,
			)
			.await
		{
			Err(err) => {
				match err {
					AdvertisementError::Duplicate => {
						self.metrics.on_advertisement_rejected_duplicate(para_id)
					},
					AdvertisementError::OutOfOurView => {
						self.metrics.on_advertisement_rejected_out_of_view(para_id)
					},
					AdvertisementError::PeerLimitReached => {
						self.metrics.on_advertisement_rejected_peer_limit_reached(para_id)
					},
					AdvertisementError::BlockedByBacking => {
						self.metrics.on_advertisement_rejected_blocked_by_backing(para_id)
					},
					AdvertisementError::V1AdvertisementForImplicitParent => {
						self.metrics.on_advertisement_rejected_v1_for_implicit_parent(para_id)
					},
					AdvertisementError::SchedulingParentNotValid => {
						self.metrics.on_advertisement_rejected_scheduling_parent_invalid(para_id)
					},
					AdvertisementError::MixedClaimShapes => {
						self.metrics.on_advertisement_rejected_mixed_claim_shapes(para_id)
					},
				}
				gum::debug!(
					target: LOG_TARGET,
					?scheduling_parent,
					?peer_id,
					?para_id,
					?err,
					"Advertisement rejected",
				);
			},
			Ok(()) => {
				self.metrics.on_advertisement_accepted(para_id);
				gum::debug!(
					target: LOG_TARGET,
					?scheduling_parent,
					?peer_id,
					?para_id,
					"Advertisement accepted",
				);
			},
		}
	}

	pub fn collation_response_stream(
		&mut self,
	) -> &mut impl FusedStream<Item = CollationFetchResponse> {
		self.collation_manager.response_stream()
	}

	pub async fn handle_fetched_collation<Sender: CollatorProtocolSenderTrait>(
		&mut self,
		sender: &mut Sender,
		res: CollationFetchResponse,
	) {
		let _timer = self.metrics.time_handler(TimedHandler::FetchedCollation);

		let fetch_result = res.1.is_ok();
		let advertisement = res.0;

		if let Err(err) = &res.1 {
			gum::debug!(
				target: LOG_TARGET,
				?advertisement,
				"Collation fetch attempt failed: {}",
				err
			);
		} else {
			gum::debug!(
				target: LOG_TARGET,
				?advertisement,
				"Collation fetch attempt succeeded",
			);
		}

		let collation_version = self.peer_manager.get_peer_protocol_version(&advertisement.peer_id);
		let can_second = self.collation_manager.note_fetched(sender, res, collation_version).await;

		// To be consistent with the old implementation, if the fetch is successful we count the
		// request as successful, despite we might not be able to second it.
		let collation_request_metrics_result = if fetch_result { Ok(()) } else { Err(()) };
		match can_second {
			CanSecond::Yes(candidate_receipt, pov, pvd) => {
				let para_id = candidate_receipt.descriptor.para_id();
				sender
					.send_message(CandidateBackingMessage::Second {
						scheduling_parent: candidate_receipt.descriptor().scheduling_parent(),
						candidate: candidate_receipt,
						pvd,
						pov,
					})
					.await;

				self.metrics.on_collation_seconded(&para_id);

				gum::debug!(
					target: LOG_TARGET,
					?advertisement,
					"Started seconding"
				);
			},
			CanSecond::No(maybe_slash, reject_info) => {
				gum::debug!(
					target: LOG_TARGET,
					?maybe_slash,
					?reject_info,
					"Cannot second collation",
				);

				if let Some(slash) = maybe_slash {
					self.metrics.on_slash_failed_fetch(&reject_info.para_id);
					self.peer_manager
						.slash_reputation(&reject_info.peer_id, &reject_info.para_id, slash)
						.await;
				}

				self.collation_manager.release_slot(
					&reject_info.scheduling_parent,
					reject_info.para_id,
					reject_info.maybe_candidate_hash.as_ref(),
					reject_info.maybe_output_head_hash,
				);
			},
			CanSecond::BlockedOnParent(parent_hash, reject_info) => {
				self.metrics.on_collation_blocked_on_parent(&reject_info.para_id);
				gum::debug!(
					target: LOG_TARGET,
					?parent_hash,
					?reject_info,
					"Collation blocked on parent, waiting for parent to be validated",
				);
			},
		};

		self.metrics.on_request(collation_request_metrics_result);
	}

	pub async fn handle_invalid_collation(
		&mut self,
		receipt: CandidateReceipt,
		scheduling_parent: Hash,
	) {
		let _timer = self.metrics.time_handler(TimedHandler::InvalidCollation);

		let candidate_hash = receipt.hash();

		gum::debug!(
			target: LOG_TARGET,
			para_id = ?receipt.descriptor.para_id(),
			?scheduling_parent,
			?candidate_hash,
			"Invalid collation",
		);

		let maybe_peer_id = self.collation_manager.release_slot(
			&scheduling_parent,
			receipt.descriptor.para_id(),
			Some(&candidate_hash),
			Some(receipt.descriptor.para_head()),
		);

		let Some(peer_id) = maybe_peer_id else {
			gum::warn!(
				target: LOG_TARGET,
				para_id = ?receipt.descriptor.para_id(),
				?scheduling_parent,
				?candidate_hash,
				"Could not find the peer id of the invalid collation",
			);
			return;
		};

		gum::debug!(
			target: LOG_TARGET,
			?scheduling_parent,
			?candidate_hash,
			?peer_id,
			"Invalid collation reported, slashing peer reputation",
		);

		self.metrics.on_slash_invalid_collation(&receipt.descriptor.para_id());
		self.peer_manager
			.slash_reputation(&peer_id, &receipt.descriptor.para_id(), INVALID_COLLATION_SLASH)
			.await;
	}

	pub async fn handle_seconded_collation<Sender: CollatorProtocolSenderTrait>(
		&mut self,
		sender: &mut Sender,
		statement: SignedFullStatement,
		scheduling_parent: Hash,
	) {
		let _timer = self.metrics.time_handler(TimedHandler::Seconded);

		let receipt = match statement.payload() {
			Statement::Seconded(receipt) => receipt,
			Statement::Valid(_) => {
				gum::warn!(
					target: LOG_TARGET,
					?statement,
					"Seconded message received with a `Valid` statement",
				);
				return;
			},
		};

		let candidate_hash = receipt.hash();
		let para_id = receipt.descriptor.para_id();

		gum::debug!(
			target: LOG_TARGET,
			?para_id,
			?scheduling_parent,
			?candidate_hash,
			"Collation seconded",
		);

		let (peer_id, unblocked_collations) = self
			.collation_manager
			.note_seconded(
				sender,
				&scheduling_parent,
				&para_id,
				&candidate_hash,
				receipt.descriptor.para_head(),
			)
			.await;

		match peer_id {
			Some(peer_id) => match self.peer_manager.peer_info(&peer_id) {
				Some(PeerInfo { version, .. }) => {
					gum::debug!(
						target: LOG_TARGET,
						?para_id,
						?scheduling_parent,
						?candidate_hash,
						?peer_id,
						"Notifying collator about seconded collation",
					);
					notify_collation_seconded(
						sender,
						peer_id,
						*version,
						scheduling_parent,
						statement,
					)
					.await;
				},
				// We know who fetched it, but they disconnected before we could ack the second.
				None => {
					gum::trace!(
						target: LOG_TARGET,
						?para_id,
						?scheduling_parent,
						?candidate_hash,
						?peer_id,
						"Not notifying collator about seconded collation: peer no longer connected",
					);
				},
			},
			// No tracked fetcher for this candidate (e.g. its slot was already released).
			None => {
				gum::trace!(
					target: LOG_TARGET,
					?para_id,
					?scheduling_parent,
					?candidate_hash,
					"Not notifying any collator about seconded collation: fetcher unknown",
				);
			},
		}

		if !unblocked_collations.is_empty() {
			gum::debug!(
				target: LOG_TARGET,
				?scheduling_parent,
				?candidate_hash,
				?para_id,
				"Seconded candidate unblocked {} collations",
				unblocked_collations.len(),
			);

			self.try_second_unblocked_collations(sender, unblocked_collations).await;
		}
	}

	pub fn mark_replan(&mut self) {
		self.collation_manager.mark_replan()
	}

	#[cfg(test)]
	pub fn take_replan(&mut self) -> bool {
		self.collation_manager.take_replan()
	}

	/// Runs a planner pass if a launch-enabling mutation happened since the last one.
	/// Outer `None`: no pass ran. Inner value: the pass's fetch-delay, as before.
	pub async fn maybe_replan<Sender: CollatorProtocolSenderTrait>(
		&mut self,
		sender: &mut Sender,
	) -> Option<Option<Duration>> {
		if !self.collation_manager.take_replan() {
			return None;
		}
		let paras: Vec<ParaId> = self.collation_manager.assignments().into_iter().collect();
		if paras.is_empty() {
			return None;
		}
		let (tx, rx) = oneshot::channel();
		sender
			.send_message(ProspectiveParachainsMessage::GetKnownOutputHeads(paras, tx))
			.await;
		let pp_known = match rx.await {
			Ok(known) => known,
			Err(_) => {
				gum::warn!(
					target: LOG_TARGET,
					"GetKnownOutputHeads responder dropped; skipping planner pass",
				);
				return None;
			},
		};
		Some(self.try_launch_new_fetch_requests(sender, &pp_known).await)
	}

	pub async fn try_launch_new_fetch_requests<Sender: CollatorProtocolSenderTrait>(
		&mut self,
		sender: &mut Sender,
		pp_known: &KnownOutputHeads,
	) -> Option<Duration> {
		let _timer = self.metrics.time_handler(TimedHandler::LaunchFetchRequests);

		let peer_manager = &self.peer_manager;
		let connected_rep_query_fn = move |peer_id: &PeerId, para_id: &ParaId| {
			peer_manager.connected_peer_score(peer_id, para_id)
		};
		let max_reps = self
			.peer_manager
			.max_scores_for_paras(self.collation_manager.assignments())
			.await;

		let metrics = &self.metrics;
		let create_timer_fn = || metrics.time_collation_request_duration();

		let (requests, maybe_delay) = self.collation_manager.try_make_new_fetch_requests(
			connected_rep_query_fn,
			max_reps,
			pp_known,
			create_timer_fn,
		);

		if !requests.is_empty() {
			gum::debug!(
				target: LOG_TARGET,
				?requests,
				"Sending {} collation fetch requests",
				requests.len()
			);

			sender
				.send_message(NetworkBridgeTxMessage::SendRequests(
					requests,
					IfDisconnected::ImmediateError,
				))
				.await;
		}

		maybe_delay
	}

	async fn try_second_unblocked_collations<Sender: CollatorProtocolSenderTrait>(
		&mut self,
		sender: &mut Sender,
		unblocked_collations: Vec<CanSecond>,
	) {
		for can_second_unblocked in unblocked_collations {
			match can_second_unblocked {
				CanSecond::Yes(candidate_receipt, pov, pvd) => {
					let candidate_hash = candidate_receipt.hash();
					let para_id = candidate_receipt.descriptor.para_id();
					let scheduling_parent = candidate_receipt.descriptor().scheduling_parent();

					sender
						.send_message(CandidateBackingMessage::Second {
							scheduling_parent,
							candidate: candidate_receipt,
							pvd,
							pov,
						})
						.await;

					self.metrics.on_collation_seconded(&para_id);

					gum::debug!(
						target: LOG_TARGET,
						?scheduling_parent,
						?candidate_hash,
						?para_id,
						"Started seconding unblocked collation"
					);
				},
				CanSecond::No(maybe_slash, reject_info) => {
					gum::debug!(
						target: LOG_TARGET,
						scheduling_parent = ?reject_info.scheduling_parent,
						maybe_candidate_hash = ?reject_info.maybe_candidate_hash,
						para_id = ?reject_info.para_id,
						"Cannot second unblocked collation"
					);

					if let Some(slash) = maybe_slash {
						self.metrics.on_slash_failed_fetch(&reject_info.para_id);
						self.peer_manager
							.slash_reputation(&reject_info.peer_id, &reject_info.para_id, slash)
							.await;
					}

					self.collation_manager.release_slot(
						&reject_info.scheduling_parent,
						reject_info.para_id,
						reject_info.maybe_candidate_hash.as_ref(),
						reject_info.maybe_output_head_hash,
					);
				},
				CanSecond::BlockedOnParent(parent, reject_info) => {
					gum::warn!(
						target: LOG_TARGET,
						scheduling_parent = ?reject_info.scheduling_parent,
						maybe_candidate_hash = ?reject_info.maybe_candidate_hash,
						?parent,
						para_id = ?reject_info.para_id,
						"Cannot second unblocked collation even though its parent was just seconded"
					);

					self.collation_manager.release_slot(
						&reject_info.scheduling_parent,
						reject_info.para_id,
						reject_info.maybe_candidate_hash.as_ref(),
						reject_info.maybe_output_head_hash,
					);
				},
			}
		}
	}

	#[cfg(test)]
	pub fn connected_peers(&self) -> std::collections::BTreeSet<PeerId> {
		self.peer_manager.connected_peers()
	}

	#[cfg(test)]
	pub fn advertisements(&self) -> std::collections::BTreeSet<super::common::Advertisement> {
		self.collation_manager.advertisements()
	}

	#[cfg(test)]
	pub fn segments(
		&self,
	) -> std::collections::BTreeSet<(Hash, PeerId, Vec<super::common::ProspectiveCandidate>)> {
		self.collation_manager.segments()
	}

	#[cfg(test)]
	pub async fn processed_finalized_block_number(&self) -> Option<BlockNumber> {
		self.peer_manager.processed_finalized_block_number().await
	}
}

// Specific implementation for PersistentDb to support disk persistence.
impl State<PersistentDb> {
	/// Persist the reputation database to disk asynchronously (fire-and-forget).
	/// Called on periodic timer.
	pub fn background_persist_reputations(&mut self) {
		self.peer_manager.persist_to_disk_async();
	}

	/// Persist the reputation database to disk and wait for completion.
	/// Called on graceful shutdown.
	pub async fn persist_reputations(&mut self) {
		self.peer_manager.persist_and_wait().await;
	}
}
