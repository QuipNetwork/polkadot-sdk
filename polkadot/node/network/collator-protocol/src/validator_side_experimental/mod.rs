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

mod collation_manager;
mod common;
mod error;
mod peer_manager;
mod state;
#[cfg(test)]
mod tests;

use crate::{
	validator_side_experimental::{
		common::MIN_FETCH_TIMER_DELAY,
		peer_manager::{Backend, PeerManager, PersistentDb},
	},
	LOG_TARGET,
};
use collation_manager::CollationManager;
use common::{ProspectiveCandidate, MAX_STORED_SCORES_PER_PARA};
use error::{log_error, FatalError, FatalResult, Result};
use futures::{future::Fuse, select, FutureExt, StreamExt};
use polkadot_node_clock::Clock;
use polkadot_node_network_protocol::{
	self as net_protocol,
	peer_set::PeerSet,
	v1 as protocol_v1, v2 as protocol_v2, v3_collation as protocol_v3,
	v4_collation::{self as protocol_v4},
	CollationProtocols, PeerId,
};
use polkadot_node_subsystem::{
	messages::{CollatorProtocolMessage, NetworkBridgeEvent, NetworkBridgeTxMessage},
	overseer, ActivatedLeaf, CollatorProtocolSenderTrait, FromOrchestra, OverseerSignal,
};
use polkadot_node_subsystem_util::database::Database;
use sp_keystore::KeystorePtr;
use std::{future, future::Future, pin::Pin, sync::Arc, time::Duration};

#[cfg(test)]
use peer_manager::Db;

use state::State;

pub use crate::validator_side_metrics::Metrics;

/// Default interval for persisting the reputation database to disk (in seconds).
const DEFAULT_PERSIST_INTERVAL_SECS: u64 = 600;

/// Configuration for the reputation db.
#[derive(Debug, Clone, Copy)]
pub struct ReputationConfig {
	/// The data column in the store to use for reputation data.
	pub col_reputation_data: u32,
	/// How often to persist the reputation database to disk.
	/// If None, defaults to DEFAULT_PERSIST_INTERVAL_SECS seconds.
	pub persist_interval: Option<Duration>,
}

/// The main run loop.
#[overseer::contextbounds(CollatorProtocol, prefix = self::overseer)]
pub(crate) async fn run<Context>(
	mut ctx: Context,
	keystore: KeystorePtr,
	metrics: Metrics,
	db: Arc<dyn Database>,
	reputation_config: ReputationConfig,
	clock: Arc<dyn Clock>,
) -> FatalResult<()> {
	let persist_interval = reputation_config
		.persist_interval
		.unwrap_or(Duration::from_secs(DEFAULT_PERSIST_INTERVAL_SECS));
	gum::info!(
		LOG_TARGET,
		persist_interval_secs = persist_interval.as_secs(),
		"Running experimental collator protocol"
	);
	if let Some(state) =
		initialize(&mut ctx, keystore, metrics, db, reputation_config, clock.clone()).await?
	{
		run_inner(ctx, state, persist_interval, clock).await?;
	}

	Ok(())
}

#[overseer::contextbounds(CollatorProtocol, prefix = self::overseer)]
async fn initialize<Context>(
	ctx: &mut Context,
	keystore: KeystorePtr,
	metrics: Metrics,
	db: Arc<dyn Database>,
	reputation_config: ReputationConfig,
	clock: Arc<dyn Clock>,
) -> FatalResult<Option<State<PersistentDb>>> {
	loop {
		let first_leaf = match wait_for_first_leaf(ctx).await? {
			Some(activated_leaf) => {
				gum::debug!(
					target: LOG_TARGET,
					number = activated_leaf.number,
					hash = ?activated_leaf.hash,
					"Got the first active leaf notification, trying to initialize subsystem."
				);
				activated_leaf
			},
			None => return Ok(None),
		};

		let collation_manager =
			CollationManager::new(ctx.sender(), keystore.clone(), first_leaf, clock.clone())
				.await?;

		let scheduled_paras = collation_manager.assignments();

		gum::debug!(
			target: LOG_TARGET,
			?scheduled_paras,
			"Collator protocol initial assignments",
		);

		metrics.note_assigned_paras(scheduled_paras.len());

		// Create PersistentDb with disk persistence
		let (backend, task) = match PersistentDb::new(
			db.clone(),
			reputation_config,
			MAX_STORED_SCORES_PER_PARA,
		)
		.await
		{
			Ok(result) => result,
			Err(e) => {
				gum::error!(
					target: LOG_TARGET,
					error = ?e,
					"Failed to initialize persistent reputation DB"
				);
				return Err(FatalError::ReputationDbInit(e));
			},
		};

		// Background task for async writes
		ctx.spawn_blocking("collator-reputation-persistence-task", task)
			.map_err(|e| FatalError::SpawnTask(e.to_string()))?;

		gum::trace!(target: LOG_TARGET, "Spawned background reputation persistence task");

		match PeerManager::startup(
			backend,
			ctx.sender(),
			scheduled_paras.into_iter().collect(),
			clock.clone(),
			metrics.clone(),
		)
		.await
		{
			Ok(peer_manager) => {
				return Ok(Some(State::new(peer_manager, collation_manager, metrics)))
			},
			Err(err) => {
				log_error(Err(err))?;
				continue;
			},
		}
	}
}

/// Wait for `ActiveLeavesUpdate`, returns `None` if `Conclude` signal came first.
#[overseer::contextbounds(CollatorProtocol, prefix = self::overseer)]
async fn wait_for_first_leaf<Context>(ctx: &mut Context) -> FatalResult<Option<ActivatedLeaf>> {
	loop {
		match ctx.recv().await.map_err(FatalError::SubsystemReceive)? {
			FromOrchestra::Signal(OverseerSignal::Conclude) => return Ok(None),
			FromOrchestra::Signal(OverseerSignal::ActiveLeaves(update)) => {
				if let Some(activated) = update.activated {
					return Ok(Some(activated));
				}
			},
			FromOrchestra::Signal(OverseerSignal::BlockFinalized(_, _)) => {},
			FromOrchestra::Communication { msg } => {
				// Disconnect peers that connect before the subsystem is initialized.
				// They will reconnect later when we're ready.
				match msg {
					CollatorProtocolMessage::NetworkBridgeUpdate(
						NetworkBridgeEvent::PeerConnected(peer_id, ..),
					) => {
						gum::debug!(
							target: LOG_TARGET,
							?peer_id,
							"Disconnecting peer that connected before subsystem initialization",
						);
						ctx.send_message(NetworkBridgeTxMessage::DisconnectPeers(
							vec![peer_id],
							PeerSet::Collation,
						))
						.await;
					},
					CollatorProtocolMessage::NetworkBridgeUpdate(
						NetworkBridgeEvent::PeerMessage(peer_id, ..),
					) => {
						gum::debug!(
							target: LOG_TARGET,
							?peer_id,
							"Disconnecting peer that sent message before subsystem initialization",
						);
						ctx.send_message(NetworkBridgeTxMessage::DisconnectPeers(
							vec![peer_id],
							PeerSet::Collation,
						))
						.await;
					},
					msg => {
						gum::trace!(
							target: LOG_TARGET,
							?msg,
							"Received msg before first active leaves update, dropping.",
						);
					},
				}
			},
		}
	}
}

fn create_timer(
	clock: &dyn Clock,
	maybe_delay: Option<Duration>,
) -> Fuse<Pin<Box<dyn Future<Output = ()> + Send>>> {
	let timer: Pin<Box<dyn Future<Output = ()> + Send>> = match maybe_delay {
		Some(delay) => clock.delay(delay),
		None => Box::pin(future::pending::<()>()),
	};

	timer.fuse()
}

/// Create the persistence timer that fires after the given interval.
fn create_persistence_timer(
	clock: &dyn Clock,
	interval: Duration,
) -> Fuse<Pin<Box<dyn Future<Output = ()> + Send>>> {
	clock.delay(interval).fuse()
}

#[overseer::contextbounds(CollatorProtocol, prefix = self::overseer)]
async fn run_inner<Context>(
	mut ctx: Context,
	mut state: State<PersistentDb>,
	persist_interval: Duration,
	clock: Arc<dyn Clock>,
) -> FatalResult<()> {
	let mut timer = create_timer(&*clock, None);
	let mut persistence_timer = create_persistence_timer(&*clock, persist_interval);

	loop {
		select! {
			// Calling `fuse()` here is useless, because the termination state of the resulting
			// fused future is discarded after each iteration. But we need to do it in order to
			// make the compiler happy.
			// However, we actually need `ctx.recv()` to poll for received messages/signals during
			// each loop iteration, so the resulting behavior is the desired one.
			res = ctx.recv().fuse() => {
				match res {
					Ok(FromOrchestra::Communication { msg }) => {
						gum::trace!(target: LOG_TARGET, msg = ?msg, "received a message");
						process_msg(
							ctx.sender(),
							&mut state,
							msg,
						).await;
					}
					Ok(FromOrchestra::Signal(OverseerSignal::Conclude)) | Err(_) => {
						// Persist to disk before shutdown
						state.persist_reputations().await;
						break
					},
					Ok(FromOrchestra::Signal(OverseerSignal::BlockFinalized(hash, number))) => {
						state.handle_finalized_block(ctx.sender(), hash, number).await?;
					},
					Ok(FromOrchestra::Signal(_)) => continue,
				}
			},
			resp = state.collation_response_stream().select_next_some() => {
				state.handle_fetched_collation(ctx.sender(), resp).await;
			},
			_ = &mut timer => {
				// If the timer expires, we only need to trigger the advertisement fetching logic.
				state.mark_replan();
			},
			_ = &mut persistence_timer => {
				// Periodic persistence - write reputation DB to disk
				state.background_persist_reputations();
				// Reset the timer for the next interval
				persistence_timer = create_persistence_timer(&*clock, persist_interval);
			},
		}

		// Refresh the in-memory connected peers. Done once per loop iteration, so that
		// it covers every event source above and cannot be missed by a handler that forgets to
		// update it.
		state.note_in_memory_connected_peers();

		// Trigger the advertisement fetching logic only when something happened that
		// can make a new fetch possible: every such mutation sets the replan flag.
		if let Some(maybe_delay) = state.maybe_replan(ctx.sender()).await {
			timer = create_timer(
				&*clock,
				maybe_delay.map(|delay| std::cmp::max(delay, MIN_FETCH_TIMER_DELAY)),
			);
		}
	}

	Ok(())
}

/// The main message receiver switch.
async fn process_msg<Sender: CollatorProtocolSenderTrait>(
	sender: &mut Sender,
	state: &mut State<PersistentDb>,
	msg: CollatorProtocolMessage,
) {
	use CollatorProtocolMessage::*;

	let _timer = state.metrics().time_process_msg();

	match msg {
		CollateOn(id) => {
			gum::warn!(
				target: LOG_TARGET,
				para_id = %id,
				"CollateOn message is not expected on the validator side of the protocol",
			);
		},
		DistributeSegment { .. } => {
			gum::warn!(
				target: LOG_TARGET,
				"DistributeSegment message is not expected on the validator side of the protocol",
			);
		},
		NetworkBridgeUpdate(event) => {
			if let Err(e) = handle_network_msg(sender, state, event).await {
				gum::warn!(
					target: LOG_TARGET,
					err = ?e,
					"Failed to handle incoming network message",
				);
			}
		},
		Seconded(parent, stmt) => {
			state.handle_seconded_collation(sender, stmt, parent).await;
		},
		Invalid(parent, candidate_receipt) => {
			state.handle_invalid_collation(candidate_receipt, parent).await;
		},
		ConnectToBackingGroups => {
			gum::warn!(
				target: LOG_TARGET,
				"ConnectToBackingGroups message is not expected on the validator side of the protocol",
			);
		},
		DisconnectFromBackingGroups => {
			gum::warn!(
				target: LOG_TARGET,
				"DisconnectFromBackingGroups message is not expected on the validator side of the protocol",
			);
		},
	}
}

/// Bridge event switch.
async fn handle_network_msg<Sender: CollatorProtocolSenderTrait>(
	sender: &mut Sender,
	state: &mut State<PersistentDb>,
	bridge_message: NetworkBridgeEvent<net_protocol::CollatorProtocolMessage>,
) -> Result<()> {
	use NetworkBridgeEvent::*;

	match bridge_message {
		PeerConnected(peer_id, observed_role, protocol_version, _) => {
			let version = match protocol_version.try_into() {
				Ok(version) => version,
				Err(err) => {
					// Network bridge is expected to handle this.
					gum::error!(
						target: LOG_TARGET,
						?peer_id,
						?observed_role,
						?err,
						"Unsupported protocol version"
					);
					return Ok(());
				},
			};
			state.handle_peer_connected(sender, peer_id, version).await;
		},
		PeerDisconnected(peer_id) => {
			state.handle_peer_disconnected(peer_id).await;
		},
		NewGossipTopology { .. } => {
			// impossible!
		},
		PeerViewChange(_, _) => {
			// We don't really care about a peer's view.
		},
		OurViewChange(view) => {
			state.handle_our_view_change(sender, view).await?;
		},
		PeerMessage(remote, msg) => {
			process_incoming_peer_message(sender, state, remote, msg).await;
		},
		UpdatedAuthorityIds { .. } => {
			// The validator side doesn't deal with `AuthorityDiscoveryId`s.
		},
	}

	Ok(())
}

async fn process_incoming_peer_message<Sender, B>(
	sender: &mut Sender,
	state: &mut State<B>,
	origin: PeerId,
	msg: CollationProtocols<
		protocol_v1::CollatorProtocolMessage,
		protocol_v2::CollatorProtocolMessage,
		protocol_v3::CollatorProtocolMessage,
		protocol_v4::AdvertiseSegment,
	>,
) where
	Sender: CollatorProtocolSenderTrait,
	B: Backend,
{
	use protocol_v1::CollatorProtocolMessage as V1;
	use protocol_v2::CollatorProtocolMessage as V2;
	use protocol_v3::CollatorProtocolMessage as V3;
	use protocol_v4::AdvertiseSegment as V4;

	match msg {
		CollationProtocols::V1(V1::Declare(_collator_id, para_id, _signature)) |
		CollationProtocols::V2(V2::Declare(_collator_id, para_id, _signature)) |
		CollationProtocols::V3(V3::Declare(_collator_id, para_id, _signature)) => {
			state.handle_declare(sender, origin, para_id).await;
		},
		CollationProtocols::V1(V1::CollationSeconded(..)) |
		CollationProtocols::V2(V2::CollationSeconded(..)) |
		CollationProtocols::V3(V3::CollationSeconded(..)) => {
			gum::warn!(
				target: LOG_TARGET,
				peer_id = ?origin,
				"Unexpected `CollationSeconded` message",
			);
		},
		CollationProtocols::V1(V1::AdvertiseCollation(relay_parent)) => {
			state
				.handle_advertisement(sender, origin, relay_parent, vec![], None, None)
				.await;
		},
		CollationProtocols::V2(V2::AdvertiseCollation {
			scheduling_parent,
			candidate_hash,
			parent_head_data_hash,
			..
		}) => {
			state
				.handle_advertisement(
					sender,
					origin,
					scheduling_parent,
					vec![ProspectiveCandidate::ByHash { candidate_hash, parent_head_data_hash }],
					None,
					None,
				)
				.await;
		},
		CollationProtocols::V3(V3::AdvertiseCollation {
			scheduling_parent,
			candidate_hash,
			parent_head_data_hash,
			candidate_descriptor_version,
			..
		}) => {
			state
				.handle_advertisement(
					sender,
					origin,
					scheduling_parent,
					vec![ProspectiveCandidate::ByHash { candidate_hash, parent_head_data_hash }],
					Some(candidate_descriptor_version),
					None,
				)
				.await;
		},
		CollationProtocols::V4(V4 {
			scheduling_parent,
			para_id,
			candidates_descriptor_version,
			candidates,
		}) => {
			let entries = candidates
				.iter()
				.map(|candidate_fingerprint| ProspectiveCandidate::ByOutputHead {
					output_head_data_hash: candidate_fingerprint.output_head_data_hash,
					parent_head_data_hash: candidate_fingerprint.parent_head_data_hash,
				})
				.collect();
			state
				.handle_advertisement(
					sender,
					origin,
					scheduling_parent,
					entries,
					Some(candidates_descriptor_version),
					Some(para_id),
				)
				.await;
		},
	}
}
