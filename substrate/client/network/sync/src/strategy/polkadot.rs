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

//! [`PolkadotSyncingStrategy`] is a proxy between [`crate::engine::SyncingEngine`]
//! and specific syncing algorithms.

use crate::{
	block_relay_protocol::BlockDownloader,
	block_request_handler::MAX_BLOCKS_IN_RESPONSE,
	service::network::NetworkServiceHandle,
	strategy::{
		chain_sync::{ChainSync, ChainSyncMode, GapSyncBodyPolicyProvider},
		state::StateStrategy,
		warp::{WarpSync, WarpSyncConfig},
		StrategyKey, SyncingAction, SyncingStrategy,
	},
	types::SyncStatus,
	LOG_TARGET,
};
use log::{debug, error, info, warn};
use prometheus_endpoint::Registry;
use sc_client_api::{BlockBackend, ProofProvider};
use sc_consensus::{BlockImportError, BlockImportStatus};
use sc_network::ProtocolName;
use sc_network_common::sync::{message::BlockAnnounce, SyncMode};
use sc_network_types::PeerId;
use sp_blockchain::{Error as ClientError, HeaderBackend, HeaderMetadata};
use sp_runtime::traits::{Block as BlockT, Header, NumberFor};
use std::{any::Any, collections::HashMap, sync::Arc};

/// Corresponding `ChainSync` mode.
fn chain_sync_mode(sync_mode: SyncMode) -> ChainSyncMode {
	match sync_mode {
		SyncMode::Full => ChainSyncMode::Full,
		SyncMode::LightState { skip_proofs, storage_chain_mode } => {
			ChainSyncMode::LightState { skip_proofs, storage_chain_mode }
		},
		SyncMode::Warp => ChainSyncMode::Full,
	}
}

/// Syncing configuration containing data for [`PolkadotSyncingStrategy`].
#[derive(Clone)]
pub struct PolkadotSyncingStrategyConfig<Block>
where
	Block: BlockT,
{
	/// Syncing mode.
	pub mode: SyncMode,
	/// The number of parallel downloads to guard against slow peers.
	pub max_parallel_downloads: u32,
	/// Maximum number of blocks to request.
	pub max_blocks_per_request: u32,
	/// Number of peers that need to be connected before warp sync is started.
	pub min_peers_to_start_warp_sync: Option<usize>,
	/// Prometheus metrics registry.
	pub metrics_registry: Option<Registry>,
	/// Protocol name used to send out state requests
	pub state_request_protocol_name: ProtocolName,
	/// Block downloader
	pub block_downloader: Arc<dyn BlockDownloader<Block>>,
	/// Resolves the gap sync body policy when a `ChainSync` instance is created.
	pub gap_sync_body_policy: GapSyncBodyPolicyProvider,
}

/// Proxy to specific syncing strategies used in Polkadot.
pub struct PolkadotSyncingStrategy<B: BlockT, Client> {
	/// Initial syncing configuration.
	config: PolkadotSyncingStrategyConfig<B>,
	/// Client used by syncing strategies.
	client: Arc<Client>,
	/// Warp strategy.
	warp: Option<WarpSync<B>>,
	/// State strategy.
	state: Option<StateStrategy<B>>,
	/// `ChainSync` strategy.`
	chain_sync: Option<ChainSync<B, Client>>,
	/// Connected peers and their best blocks used to seed a new strategy when switching to it in
	/// `PolkadotSyncingStrategy::proceed_to_next`.
	peer_best_blocks: HashMap<PeerId, (B::Hash, NumberFor<B>)>,
}

impl<B: BlockT, Client> SyncingStrategy<B> for PolkadotSyncingStrategy<B, Client>
where
	B: BlockT,
	Client: HeaderBackend<B>
		+ BlockBackend<B>
		+ HeaderMetadata<B, Error = sp_blockchain::Error>
		+ ProofProvider<B>
		+ Send
		+ Sync
		+ 'static,
{
	fn add_peer(&mut self, peer_id: PeerId, best_hash: B::Hash, best_number: NumberFor<B>) {
		self.peer_best_blocks.insert(peer_id, (best_hash, best_number));

		self.warp.as_mut().map(|s| s.add_peer(peer_id, best_hash, best_number));
		self.state.as_mut().map(|s| s.add_peer(peer_id, best_hash, best_number));
		self.chain_sync.as_mut().map(|s| s.add_peer(peer_id, best_hash, best_number));
	}

	fn remove_peer(&mut self, peer_id: &PeerId) {
		self.warp.as_mut().map(|s| s.remove_peer(peer_id));
		self.state.as_mut().map(|s| s.remove_peer(peer_id));
		self.chain_sync.as_mut().map(|s| s.remove_peer(peer_id));

		self.peer_best_blocks.remove(peer_id);
	}

	fn on_validated_block_announce(
		&mut self,
		is_best: bool,
		peer_id: PeerId,
		announce: &BlockAnnounce<B::Header>,
	) -> Option<(B::Hash, NumberFor<B>)> {
		let new_best = if let Some(ref mut warp) = self.warp {
			warp.on_validated_block_announce(is_best, peer_id, announce)
		} else if let Some(ref mut state) = self.state {
			state.on_validated_block_announce(is_best, peer_id, announce)
		} else if let Some(ref mut chain_sync) = self.chain_sync {
			chain_sync.on_validated_block_announce(is_best, peer_id, announce)
		} else {
			error!(target: LOG_TARGET, "No syncing strategy is active.");
			debug_assert!(false);
			Some((announce.header.hash(), *announce.header.number()))
		};

		if let Some(new_best) = new_best {
			if let Some(best) = self.peer_best_blocks.get_mut(&peer_id) {
				*best = new_best;
			} else {
				debug!(
					target: LOG_TARGET,
					"Cannot update `peer_best_blocks` as peer {peer_id} is not known to `Strategy` \
					 (already disconnected?)",
				);
			}
		}

		new_best
	}

	fn set_sync_fork_request(&mut self, peers: Vec<PeerId>, hash: &B::Hash, number: NumberFor<B>) {
		// Fork requests are only handled by `ChainSync`.
		if let Some(ref mut chain_sync) = self.chain_sync {
			chain_sync.set_sync_fork_request(peers.clone(), hash, number);
		}
	}

	fn request_justification(&mut self, hash: &B::Hash, number: NumberFor<B>) {
		// Justifications can only be requested via `ChainSync`.
		if let Some(ref mut chain_sync) = self.chain_sync {
			chain_sync.request_justification(hash, number);
		}
	}

	fn clear_justification_requests(&mut self) {
		// Justification requests can only be cleared by `ChainSync`.
		if let Some(ref mut chain_sync) = self.chain_sync {
			chain_sync.clear_justification_requests();
		}
	}

	fn on_justification_import(&mut self, hash: B::Hash, number: NumberFor<B>, success: bool) {
		// Only `ChainSync` is interested in justification import.
		if let Some(ref mut chain_sync) = self.chain_sync {
			chain_sync.on_justification_import(hash, number, success);
		}
	}

	fn on_generic_response(
		&mut self,
		peer_id: &PeerId,
		key: StrategyKey,
		protocol_name: ProtocolName,
		response: Box<dyn Any + Send>,
	) {
		match key {
			StateStrategy::<B>::STRATEGY_KEY => {
				if let Some(state) = &mut self.state {
					let Ok(response) = response.downcast::<Vec<u8>>() else {
						warn!(target: LOG_TARGET, "Failed to downcast state response");
						debug_assert!(false);
						return;
					};

					state.on_state_response(peer_id, *response);
				} else if let Some(chain_sync) = &mut self.chain_sync {
					chain_sync.on_generic_response(peer_id, key, protocol_name, response);
				} else {
					error!(
						target: LOG_TARGET,
						"`on_generic_response()` called with unexpected key {key:?} \
						 or corresponding strategy is not active.",
					);
					debug_assert!(false);
				}
			},
			WarpSync::<B>::STRATEGY_KEY => {
				if let Some(warp) = &mut self.warp {
					warp.on_generic_response(peer_id, protocol_name, response);
				} else {
					error!(
						target: LOG_TARGET,
						"`on_generic_response()` called with unexpected key {key:?} \
						 or warp strategy is not active",
					);
					debug_assert!(false);
				}
			},
			ChainSync::<B, Client>::STRATEGY_KEY => {
				if let Some(chain_sync) = &mut self.chain_sync {
					chain_sync.on_generic_response(peer_id, key, protocol_name, response);
				} else {
					error!(
						target: LOG_TARGET,
						"`on_generic_response()` called with unexpected key {key:?} \
						 or corresponding strategy is not active.",
					);
					debug_assert!(false);
				}
			},
			key => {
				warn!(
					target: LOG_TARGET,
					"Unexpected generic response strategy key {key:?}, protocol {protocol_name}",
				);
				debug_assert!(false);
			},
		}
	}

	fn on_blocks_processed(
		&mut self,
		imported: usize,
		count: usize,
		results: Vec<(Result<BlockImportStatus<NumberFor<B>>, BlockImportError>, B::Hash)>,
	) {
		// Only `StateStrategy` and `ChainSync` are interested in block processing notifications.
		if let Some(ref mut state) = self.state {
			state.on_blocks_processed(imported, count, results);
		} else if let Some(ref mut chain_sync) = self.chain_sync {
			chain_sync.on_blocks_processed(imported, count, results);
		}
	}

	fn on_block_finalized(&mut self, hash: &B::Hash, number: NumberFor<B>) {
		// Only `ChainSync` is interested in block finalization notifications.
		if let Some(ref mut chain_sync) = self.chain_sync {
			chain_sync.on_block_finalized(hash, number);
		}
	}

	fn update_chain_info(&mut self, best_hash: &B::Hash, best_number: NumberFor<B>) {
		// This is relevant to `ChainSync` only.
		if let Some(ref mut chain_sync) = self.chain_sync {
			chain_sync.update_chain_info(best_hash, best_number);
		}
	}

	fn is_major_syncing(&self) -> bool {
		self.warp.is_some() ||
			self.state.is_some() ||
			match self.chain_sync {
				Some(ref s) => s.status().state.is_major_syncing(),
				None => unreachable!("At least one syncing strategy is active; qed"),
			}
	}

	fn num_peers(&self) -> usize {
		self.peer_best_blocks.len()
	}

	fn status(&self) -> SyncStatus<B> {
		// This function presumes that strategies are executed serially and must be refactored
		// once we have parallel strategies.
		if let Some(ref warp) = self.warp {
			warp.status()
		} else if let Some(ref state) = self.state {
			state.status()
		} else if let Some(ref chain_sync) = self.chain_sync {
			chain_sync.status()
		} else {
			unreachable!("At least one syncing strategy is always active; qed")
		}
	}

	fn num_downloaded_blocks(&self) -> usize {
		self.chain_sync
			.as_ref()
			.map_or(0, |chain_sync| chain_sync.num_downloaded_blocks())
	}

	fn num_sync_requests(&self) -> usize {
		self.chain_sync.as_ref().map_or(0, |chain_sync| chain_sync.num_sync_requests())
	}

	fn actions(
		&mut self,
		network_service: &NetworkServiceHandle,
	) -> Result<Vec<SyncingAction<B>>, ClientError> {
		// This function presumes that strategies are executed serially and must be refactored once
		// we have parallel strategies.
		let actions: Vec<_> = if let Some(ref mut warp) = self.warp {
			warp.actions(network_service).map(Into::into).collect()
		} else if let Some(ref mut state) = self.state {
			state.actions(network_service).map(Into::into).collect()
		} else if let Some(ref mut chain_sync) = self.chain_sync {
			chain_sync.actions(network_service)?
		} else {
			unreachable!("At least one syncing strategy is always active; qed")
		};

		if actions.iter().any(SyncingAction::is_finished) {
			self.proceed_to_next()?;
		}

		Ok(actions)
	}
}

impl<B: BlockT, Client> PolkadotSyncingStrategy<B, Client>
where
	B: BlockT,
	Client: HeaderBackend<B>
		+ BlockBackend<B>
		+ HeaderMetadata<B, Error = sp_blockchain::Error>
		+ ProofProvider<B>
		+ Send
		+ Sync
		+ 'static,
{
	/// Initialize a new syncing strategy.
	pub fn new(
		mut config: PolkadotSyncingStrategyConfig<B>,
		client: Arc<Client>,
		warp_sync_config: Option<WarpSyncConfig<B>>,
		warp_sync_protocol_name: Option<ProtocolName>,
	) -> Result<Self, ClientError> {
		if config.max_blocks_per_request > MAX_BLOCKS_IN_RESPONSE as u32 {
			info!(
				target: LOG_TARGET,
				"clamping maximum blocks per request to {MAX_BLOCKS_IN_RESPONSE}",
			);
			config.max_blocks_per_request = MAX_BLOCKS_IN_RESPONSE as u32;
		}

		if let SyncMode::Warp = config.mode {
			let warp_sync_config = warp_sync_config
				.expect("Warp sync configuration must be supplied in warp sync mode.");
			let warp_sync = WarpSync::new(
				client.clone(),
				warp_sync_config,
				warp_sync_protocol_name,
				config.block_downloader.clone(),
				config.min_peers_to_start_warp_sync,
			);
			Ok(Self {
				config,
				client,
				warp: Some(warp_sync),
				state: None,
				chain_sync: None,
				peer_best_blocks: Default::default(),
			})
		} else {
			let chain_sync = ChainSync::new(
				chain_sync_mode(config.mode),
				client.clone(),
				config.max_parallel_downloads,
				config.max_blocks_per_request,
				config.state_request_protocol_name.clone(),
				config.block_downloader.clone(),
				(config.gap_sync_body_policy)()?,
				config.metrics_registry.as_ref(),
				std::iter::empty(),
			)?;
			Ok(Self {
				config,
				client,
				warp: None,
				state: None,
				chain_sync: Some(chain_sync),
				peer_best_blocks: Default::default(),
			})
		}
	}

	/// Proceed with the next strategy if the active one finished.
	pub fn proceed_to_next(&mut self) -> Result<(), ClientError> {
		// The strategies are switched as `WarpSync` -> `StateStrategy` -> `ChainSync`.
		if let Some(ref mut warp) = self.warp {
			match warp.take_result() {
				Some(res) => {
					info!(
						target: LOG_TARGET,
						"Warp sync is complete, continuing with state sync."
					);
					let state_sync = StateStrategy::new(
						self.client.clone(),
						res.target_header,
						res.target_body,
						res.target_justifications,
						false,
						self.peer_best_blocks
							.iter()
							.map(|(peer_id, (_, best_number))| (*peer_id, *best_number)),
						self.config.state_request_protocol_name.clone(),
					);

					self.warp = None;
					self.state = Some(state_sync);
					Ok(())
				},
				None => {
					error!(
						target: LOG_TARGET,
						"Warp sync failed. Continuing with full sync."
					);
					let chain_sync = match ChainSync::new(
						chain_sync_mode(self.config.mode),
						self.client.clone(),
						self.config.max_parallel_downloads,
						self.config.max_blocks_per_request,
						self.config.state_request_protocol_name.clone(),
						self.config.block_downloader.clone(),
						(self.config.gap_sync_body_policy)()?,
						self.config.metrics_registry.as_ref(),
						self.peer_best_blocks.iter().map(|(peer_id, (best_hash, best_number))| {
							(*peer_id, *best_hash, *best_number)
						}),
					) {
						Ok(chain_sync) => chain_sync,
						Err(e) => {
							error!(target: LOG_TARGET, "Failed to start `ChainSync`.");
							return Err(e);
						},
					};

					self.warp = None;
					self.chain_sync = Some(chain_sync);
					Ok(())
				},
			}
		} else if let Some(state) = &self.state {
			if state.is_succeeded() {
				info!(target: LOG_TARGET, "State sync is complete, continuing with block sync.");
			} else {
				error!(target: LOG_TARGET, "State sync failed. Falling back to full sync.");
			}
			let chain_sync = match ChainSync::new(
				chain_sync_mode(self.config.mode),
				self.client.clone(),
				self.config.max_parallel_downloads,
				self.config.max_blocks_per_request,
				self.config.state_request_protocol_name.clone(),
				self.config.block_downloader.clone(),
				(self.config.gap_sync_body_policy)()?,
				self.config.metrics_registry.as_ref(),
				self.peer_best_blocks.iter().map(|(peer_id, (best_hash, best_number))| {
					(*peer_id, *best_hash, *best_number)
				}),
			) {
				Ok(chain_sync) => chain_sync,
				Err(e) => {
					error!(target: LOG_TARGET, "Failed to start `ChainSync`.");
					return Err(e);
				},
			};

			self.state = None;
			self.chain_sync = Some(chain_sync);
			Ok(())
		} else {
			unreachable!("Only warp & state strategies can finish; qed")
		}
	}
}
