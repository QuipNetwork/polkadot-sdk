// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// Cumulus is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Cumulus is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Cumulus. If not, see <https://www.gnu.org/licenses/>.

//! A collator for Aura that looks ahead of the most recently included parachain block
//! when determining what to build upon.
//!
//! This collator also builds additional blocks when the maximum backlog is not saturated.
//! The size of the backlog is determined by invoking a runtime API. If that runtime API
//! is not supported, this assumes a maximum backlog size of 1.
//!
//! This takes more advantage of asynchronous backing, though not complete advantage.
//! When the backlog is not saturated, this approach lets the backlog temporarily 'catch up'
//! with periods of higher throughput. When the backlog is saturated, we typically
//! fall back to the limited cadence of a single parachain block per relay-chain block.
//!
//! Despite this, the fact that there is a backlog at all allows us to spend more time
//! building the block, as there is some buffer before it can get posted to the relay-chain.
//! The main limitation is block propagation time - i.e. the new blocks created by an author
//! must be propagated to the next author before their turn.

use codec::{Codec, Encode};
use cumulus_client_collator::service::ServiceInterface as CollatorServiceInterface;
use cumulus_client_consensus_common::{
	self as consensus_common, ParachainBlockImportMarker, ParentSearchParams,
};
use cumulus_primitives_aura::AuraUnincludedSegmentApi;
use cumulus_primitives_core::{
	CollectCollationInfo, KeyToIncludeInRelayProof, PersistedValidationData,
};
use cumulus_relay_chain_interface::RelayChainInterface;
use polkadot_node_primitives::{SegmentCollation, SubmitSegmentParams};
use polkadot_node_subsystem::messages::CollationGenerationMessage;
use polkadot_overseer::Handle as OverseerHandle;
use polkadot_primitives::{
	CandidateDescriptorVersion, CollatorPair, Id as ParaId, OccupiedCoreAssumption,
};
use sp_consensus::Environment;

use crate::{
	collator as collator_util,
	collators::{claim_queue_at, BackingGroupConnectionHelper},
	export_pov_to_path,
};
use futures::prelude::*;
use sc_client_api::{backend::AuxStore, BlockBackend, BlockOf};
use sc_consensus::BlockImport;
use sc_network_types::PeerId;
use sp_api::{ApiExt, ProvideRuntimeApi};
use sp_application_crypto::AppPublic;
use sp_blockchain::HeaderBackend;
use sp_consensus_aura::{AuraApi, Slot};
use sp_core::crypto::Pair;
use sp_inherents::CreateInherentDataProviders;
use sp_keystore::KeystorePtr;
use sp_runtime::{
	traits::{Block as BlockT, Header as HeaderT, Member},
	BoundedVec, Saturating,
};
use sp_timestamp::Timestamp;
use std::{path::PathBuf, sync::Arc, time::Duration};

/// Parameters for [`run`].
pub struct Params<BI, CIDP, Client, Backend, RClient, CHP, ProposerFactory, CS> {
	/// Inherent data providers. Only non-consensus inherent data should be provided, i.e.
	/// the timestamp, slot, and paras inherents should be omitted, as they are set by this
	/// collator.
	pub create_inherent_data_providers: CIDP,
	/// Used to actually import blocks.
	pub block_import: BI,
	/// The underlying para client.
	pub para_client: Arc<Client>,
	/// The para client's backend, used to access the database.
	pub para_backend: Arc<Backend>,
	/// A handle to the relay-chain client.
	pub relay_client: RClient,
	/// A validation code hash provider, used to get the current validation code hash.
	pub code_hash_provider: CHP,
	/// The underlying keystore, which should contain Aura consensus keys.
	pub keystore: KeystorePtr,
	/// The collator key used to sign collations before submitting to validators.
	pub collator_key: CollatorPair,
	/// The collator network peer id.
	pub collator_peer_id: PeerId,
	/// The para's ID.
	pub para_id: ParaId,
	/// A handle to the relay-chain client's "Overseer" or task orchestrator.
	pub overseer_handle: OverseerHandle,
	/// The length of slots in the relay chain.
	pub relay_chain_slot_duration: Duration,
	/// The proposer for building blocks.
	pub proposer: ProposerFactory,
	/// The generic collator service used to plug into this consensus engine.
	pub collator_service: CS,
	/// The amount of time to spend authoring each block.
	pub authoring_duration: Duration,
	/// Whether we should reinitialize the collator config (i.e. we are transitioning to aura).
	pub reinitialize: bool,
	/// The maximum percentage of the maximum PoV size that the collator can use.
	/// It will be removed once <https://github.com/paritytech/polkadot-sdk/issues/6020> is fixed.
	pub max_pov_percentage: Option<u32>,
}

/// Get the current parachain slot from a given block hash.
///
/// Returns the parachain slot, relay chain slot, and timestamp.
fn get_parachain_slot<Block, Client, P>(
	para_client: &Client,
	block_hash: Block::Hash,
	relay_parent_header: &polkadot_primitives::Header,
	relay_chain_slot_duration: Duration,
) -> Option<(Slot, Slot, Timestamp)>
where
	Block: BlockT,
	Client: ProvideRuntimeApi<Block>,
	Client::Api: AuraApi<Block, P>,
	P: Codec,
{
	let slot_duration =
		match sc_consensus_aura::standalone::slot_duration_at(para_client, block_hash) {
			Ok(sd) => sd,
			Err(err) => {
				tracing::error!(target: crate::LOG_TARGET, ?err, "Failed to acquire parachain slot duration");
				return None;
			},
		};

	tracing::debug!(target: crate::LOG_TARGET, ?slot_duration, ?block_hash, "Parachain slot duration acquired");

	let (relay_slot, timestamp) = consensus_common::get_relay_slot_and_timestamp(
		relay_parent_header,
		relay_chain_slot_duration,
	)?;

	let slot_now = Slot::from_timestamp(timestamp, slot_duration);

	tracing::debug!(
		target: crate::LOG_TARGET,
		?relay_slot,
		para_slot = ?slot_now,
		?timestamp,
		?slot_duration,
		?relay_chain_slot_duration,
		"Adjusted relay-chain slot to parachain slot"
	);

	Some((slot_now, relay_slot, timestamp))
}

/// Run async-backing-friendly Aura.
pub fn run<Block, P, BI, CIDP, Client, Backend, RClient, CHP, Proposer, CS>(
	params: Params<BI, CIDP, Client, Backend, RClient, CHP, Proposer, CS>,
) -> impl Future<Output = ()> + Send + 'static
where
	Block: BlockT,
	Client: ProvideRuntimeApi<Block>
		+ BlockOf
		+ AuxStore
		+ HeaderBackend<Block>
		+ BlockBackend<Block>
		+ Send
		+ Sync
		+ 'static,
	Client::Api: AuraApi<Block, P::Public>
		+ CollectCollationInfo<Block>
		+ AuraUnincludedSegmentApi<Block>
		+ KeyToIncludeInRelayProof<Block>,
	Backend: sc_client_api::Backend<Block> + 'static,
	RClient: RelayChainInterface + Clone + 'static,
	CIDP: CreateInherentDataProviders<Block, ()> + 'static,
	CIDP::InherentDataProviders: Send,
	BI: BlockImport<Block> + ParachainBlockImportMarker + Send + Sync + 'static,
	Proposer: Environment<Block> + Clone + Send + Sync + 'static,
	CS: CollatorServiceInterface<Block> + Send + Sync + 'static,
	CHP: consensus_common::ValidationCodeHashProvider<Block::Hash> + Send + 'static,
	P: Pair + Send + Sync + 'static,
	P::Public: AppPublic + Member + Codec,
	P::Signature: TryFrom<Vec<u8>> + Member + Codec,
{
	run_with_export::<_, P, _, _, _, _, _, _, _, _>(ParamsWithExport { params, export_pov: None })
}

/// Parameters for [`run_with_export`].
pub struct ParamsWithExport<BI, CIDP, Client, Backend, RClient, CHP, Proposer, CS> {
	/// The parameters.
	pub params: Params<BI, CIDP, Client, Backend, RClient, CHP, Proposer, CS>,

	/// When set, the collator will export every produced `POV` to this folder.
	pub export_pov: Option<PathBuf>,
}

/// Run async-backing-friendly Aura.
///
/// This is exactly the same as [`run`], but it supports the optional export of each produced `POV`
/// to the file system.
pub fn run_with_export<Block, P, BI, CIDP, Client, Backend, RClient, CHP, Proposer, CS>(
	ParamsWithExport { mut params, export_pov }: ParamsWithExport<
		BI,
		CIDP,
		Client,
		Backend,
		RClient,
		CHP,
		Proposer,
		CS,
	>,
) -> impl Future<Output = ()> + Send + 'static
where
	Block: BlockT,
	Client: ProvideRuntimeApi<Block>
		+ BlockOf
		+ AuxStore
		+ HeaderBackend<Block>
		+ BlockBackend<Block>
		+ Send
		+ Sync
		+ 'static,
	Client::Api: AuraApi<Block, P::Public>
		+ CollectCollationInfo<Block>
		+ AuraUnincludedSegmentApi<Block>
		+ KeyToIncludeInRelayProof<Block>,
	Backend: sc_client_api::Backend<Block> + 'static,
	RClient: RelayChainInterface + Clone + 'static,
	CIDP: CreateInherentDataProviders<Block, ()> + 'static,
	CIDP::InherentDataProviders: Send,
	BI: BlockImport<Block> + ParachainBlockImportMarker + Send + Sync + 'static,
	Proposer: Environment<Block> + Clone + Send + Sync + 'static,
	CS: CollatorServiceInterface<Block> + Send + Sync + 'static,
	CHP: consensus_common::ValidationCodeHashProvider<Block::Hash> + Send + 'static,
	P: Pair + Send + Sync + 'static,
	P::Public: AppPublic + Member + Codec,
	P::Signature: TryFrom<Vec<u8>> + Member + Codec,
{
	async move {
		cumulus_client_collator::initialize_collator_subsystems(
			&mut params.overseer_handle,
			params.collator_key,
			params.para_id,
			params.reinitialize,
		)
		.await;

		let mut import_notifications = match params.relay_client.import_notification_stream().await
		{
			Ok(s) => s,
			Err(err) => {
				tracing::error!(
					target: crate::LOG_TARGET,
					?err,
					"Failed to initialize consensus: no relay chain import notification stream"
				);

				return;
			},
		};

		let mut collator = {
			let params = collator_util::Params {
				create_inherent_data_providers: params.create_inherent_data_providers,
				block_import: params.block_import,
				relay_client: params.relay_client.clone(),
				keystore: params.keystore.clone(),
				collator_peer_id: params.collator_peer_id,
				para_id: params.para_id,
				proposer: params.proposer,
				collator_service: params.collator_service,
			};

			collator_util::Collator::<Block, P, _, _, _, _, _>::new(params)
		};

		let mut connection_helper = BackingGroupConnectionHelper::new(
			params.keystore.clone(),
			params.overseer_handle.clone(),
		);

		while let Some(relay_parent_header) = import_notifications.next().await {
			let relay_parent = relay_parent_header.hash();

			let Some(core_index) = claim_queue_at(relay_parent, &mut params.relay_client)
				.await
				.iter_claims_at_depth_for_para(0, params.para_id)
				.next()
			else {
				tracing::trace!(
					target: crate::LOG_TARGET,
					?relay_parent,
					?params.para_id,
					"Para is not scheduled on any core, skipping import notification",
				);

				continue;
			};

			let max_pov_size = match params
				.relay_client
				.persisted_validation_data(
					relay_parent,
					params.para_id,
					OccupiedCoreAssumption::Included,
				)
				.await
			{
				Ok(None) => continue,
				Ok(Some(pvd)) => pvd.max_pov_size,
				Err(err) => {
					tracing::error!(target: crate::LOG_TARGET, ?err, "Failed to gather information from relay-client");
					continue;
				},
			};

			let session_index =
				match params.relay_client.session_index_for_child(relay_parent).await {
					Ok(session_index) => session_index,
					Err(err) => {
						tracing::error!(
							target: crate::LOG_TARGET,
							?err,
							?relay_parent,
							"Failed to fetch session index."
						);
						continue;
					},
				};

			let parent_search_result = match crate::collators::find_parent(
				&params.relay_client,
				&*params.para_backend,
				params.para_id,
				ParentSearchParams::V2 { scheduling_parent: relay_parent },
				|_| true,
			)
			.await
			{
				Some(result) => result,
				None => continue,
			};

			let included_header = &parent_search_result.included_at_scheduling;
			let para_client = &*params.para_client;
			let keystore = &params.keystore;
			let included_block_hash = included_header.hash();

			// Note that the authorities can change at any block, so we need to re-claim our slot
			// on every relay parent.
			let parent_hash = parent_search_result.best_parent_header.hash();
			let parent_header = parent_search_result.best_parent_header;
			// Distance from included block to best parent.
			let initial_parent_depth =
				(*parent_header.number()).saturating_sub(*included_header.number());
			let overseer_handle = &mut params.overseer_handle;

			// Do not try to build upon an unknown, pruned or bad block
			if !collator.collator_service().check_block_status(parent_hash, &parent_header) {
				continue;
			}

			// Trigger pre-connect to backing groups if necessary.
			if let Some((slot_now, _relay_slot, _timestamp)) = get_parachain_slot::<_, _, P::Public>(
				para_client,
				parent_hash,
				&relay_parent_header,
				params.relay_chain_slot_duration,
			) {
				let mut runtime_api = para_client.runtime_api();
				runtime_api
					.set_call_context(sp_core::traits::CallContext::Onchain { import: false });
				if let Ok(authorities) = runtime_api.authorities(parent_hash) {
					connection_helper.update::<P>(slot_now, &authorities).await;
				}
			}

			let Some((slot_now, relay_slot, timestamp)) = get_parachain_slot::<_, _, P::Public>(
				para_client,
				parent_hash,
				&relay_parent_header,
				params.relay_chain_slot_duration,
			) else {
				continue;
			};

			let Some(slot_claim) = super::claim_slot::<_, _, P>(
				slot_now,
				timestamp,
				parent_hash,
				para_client,
				&keystore,
			)
			.await
			else {
				continue;
			};

			if !super::can_build_upon::<_, _>(
				parent_hash,
				included_block_hash,
				relay_slot,
				slot_now,
				para_client,
			)
			.await
			{
				continue;
			}

			tracing::debug!(
				target: crate::LOG_TARGET,
				?relay_parent,
				unincluded_segment_len = ?initial_parent_depth,
				"Slot claimed. Building"
			);

			let validation_data = PersistedValidationData {
				parent_head: parent_header.encode().into(),
				relay_parent_number: *relay_parent_header.number(),
				relay_parent_storage_root: *relay_parent_header.state_root(),
				max_pov_size,
			};

			let relay_proof_request =
				super::get_relay_proof_request(&*params.para_client, parent_hash);

			let (parachain_inherent_data, other_inherent_data) = match collator
				.create_inherent_data(
					relay_parent,
					&validation_data,
					parent_hash,
					slot_claim.timestamp(),
					relay_proof_request,
					params.collator_peer_id,
				)
				.await
			{
				Err(err) => {
					tracing::error!(target: crate::LOG_TARGET, ?err);
					continue;
				},
				Ok(x) => x,
			};

			let Some(validation_code_hash) = params.code_hash_provider.code_hash_at(parent_hash)
			else {
				tracing::error!(target: crate::LOG_TARGET, ?parent_hash, "Could not fetch validation code hash");
				continue;
			};

			super::check_validation_code_or_log(
				&validation_code_hash,
				params.para_id,
				&params.relay_client,
				relay_parent,
			)
			.await;

			let allowed_pov_size = if let Some(max_pov_percentage) = params.max_pov_percentage {
				validation_data.max_pov_size * max_pov_percentage / 100
			} else {
				// Set the block limit to 85% of the maximum PoV size.
				//
				// Once https://github.com/paritytech/polkadot-sdk/issues/6020 issue is
				// fixed, the reservation should be removed.
				validation_data.max_pov_size * 85 / 100
			} as usize;

			let collation_result = collator
				.collate(
					&parent_header,
					&slot_claim,
					None,
					(parachain_inherent_data, other_inherent_data),
					params.authoring_duration,
					allowed_pov_size,
					None,
				)
				.await;

			match collation_result {
				Ok(Some((collation, block_data))) => {
					let Some(new_block_header) =
						block_data.blocks().first().map(|b| b.header().clone())
					else {
						tracing::error!(target: crate::LOG_TARGET,  "Produced PoV doesn't contain any blocks");
						continue;
					};

					let new_block_hash = new_block_header.hash();

					// Here we are assuming that the import logic protects against equivocations
					// and provides sybil-resistance, as it should.
					collator.collator_service().announce_block(new_block_hash, None);

					if let Some(ref export_pov) = export_pov {
						export_pov_to_path::<Block>(
							export_pov.clone(),
							collation.proof_of_validity.clone().into_compressed(),
							new_block_hash,
							*new_block_header.number(),
							parent_header.clone(),
							*relay_parent_header.state_root(),
							*relay_parent_header.number(),
							validation_data.max_pov_size,
						);
					}

					// Send a submit-collation message to the collation generation subsystem,
					// which then distributes this to validators.
					//
					// Here we are assuming that the leaf is imported, as we've gotten an
					// import notification.
					overseer_handle
						.send_msg(
							CollationGenerationMessage::SubmitSegment(SubmitSegmentParams {
								scheduling_parent: relay_parent,
								core_index,
								candidates_descriptor_version: CandidateDescriptorVersion::V2,
								collations: BoundedVec::try_from(vec![SegmentCollation {
									relay_parent,
									collation,
									validation_code_hash,
									session_index,
									validation_data,
								}])
								.expect("One element segment should fit;qed!"),
							}),
							"SubmitSegment",
						)
						.await;
				},
				Ok(None) => {
					tracing::debug!(target: crate::LOG_TARGET, "No block proposal");
					continue;
				},
				Err(err) => {
					tracing::error!(target: crate::LOG_TARGET, ?err);
					continue;
				},
			}
		}
	}
}
