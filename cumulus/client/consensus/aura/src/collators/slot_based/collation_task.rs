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

use std::path::PathBuf;

use cumulus_client_collator::service::ServiceInterface as CollatorServiceInterface;
use cumulus_relay_chain_interface::RelayChainInterface;

use polkadot_node_primitives::{MaybeCompressedPoV, SegmentCollation, SubmitSegmentParams};
use polkadot_node_subsystem::messages::CollationGenerationMessage;
use polkadot_overseer::Handle as OverseerHandle;
use polkadot_primitives::{CandidateDescriptorVersion, CollatorPair, Id as ParaId};

use cumulus_primitives_core::relay_chain::BlockId;
use futures::prelude::*;

use crate::export_pov_to_path;
use sc_utils::mpsc::TracingUnboundedReceiver;
use sp_runtime::{
	traits::{Block as BlockT, Header},
	BoundedVec,
};

use super::CollatorMessage;

const LOG_TARGET: &str = "aura::cumulus::collation_task";

/// Parameters for the collation task.
pub struct Params<Block: BlockT, RClient, CS> {
	/// A handle to the relay-chain client.
	pub relay_client: RClient,
	/// The collator key used to sign collations before submitting to validators.
	pub collator_key: CollatorPair,
	/// The para's ID.
	pub para_id: ParaId,
	/// Whether we should reinitialize the collator config (i.e. we are transitioning to aura).
	pub reinitialize: bool,
	/// Collator service interface
	pub collator_service: CS,
	/// Receiver channel for communication with the block builder task.
	pub collator_receiver: TracingUnboundedReceiver<CollatorMessage<Block>>,
	/// When set, the collator will export every produced `POV` to this folder.
	pub export_pov: Option<PathBuf>,
}

/// Asynchronously executes the collation task for a parachain.
///
/// This function initializes the collator subsystems necessary for producing and submitting
/// collations to the relay chain. It listens for new best relay chain block notifications and
/// handles collator messages. If our parachain is scheduled on a core and we have a candidate,
/// the task will build a collation and send it to the relay chain.
pub async fn run_collation_task<Block, RClient, CS>(
	Params {
		relay_client,
		collator_key,
		para_id,
		reinitialize,
		collator_service,
		mut collator_receiver,
		export_pov,
	}: Params<Block, RClient, CS>,
) where
	Block: BlockT,
	CS: CollatorServiceInterface<Block> + Send + Sync + 'static,
	RClient: RelayChainInterface + Clone + 'static,
{
	let Ok(mut overseer_handle) = relay_client.overseer_handle() else {
		tracing::error!(target: LOG_TARGET, "Failed to get overseer handle.");
		return;
	};

	cumulus_client_collator::initialize_collator_subsystems(
		&mut overseer_handle,
		collator_key,
		para_id,
		reinitialize,
	)
	.await;

	while let Some(message) = collator_receiver.next().await {
		handle_collation_message(
			message,
			&collator_service,
			&mut overseer_handle,
			relay_client.clone(),
			export_pov.clone(),
		)
		.await;
	}
}

/// Handle an incoming collation message from the block builder task.
/// This builds the collation from the [`CollatorMessage`] and submits it to
/// the collation-generation subsystem of the relay chain.
async fn handle_collation_message<Block: BlockT, RClient: RelayChainInterface + Clone + 'static>(
	message: CollatorMessage<Block>,
	collator_service: &impl CollatorServiceInterface<Block>,
	overseer_handle: &mut OverseerHandle,
	relay_client: RClient,
	export_pov: Option<PathBuf>,
) {
	let CollatorMessage {
		scheduling_proof,
		parent_header,
		blocks,
		proof,
		validation_code_hash,
		relay_parent,
		core_index,
		validation_data,
	} = message;

	// Derive scheduling_parent from the proof (the ISP header's hash is used when the
	// header chain is empty — that's the case with `relay_parent_offset = 0`).
	let scheduling_parent = scheduling_proof.as_ref().map(|p| p.scheduling_parent());
	let (collation, block_data) = match collator_service.build_multi_block_collation(
		&parent_header,
		blocks,
		proof,
		scheduling_proof,
	) {
		Some(collation) => collation,
		None => {
			tracing::warn!(target: LOG_TARGET, ?core_index, "Unable to build collation.");
			return;
		},
	};

	block_data.log_size_info();

	if let MaybeCompressedPoV::Compressed(ref pov) = collation.proof_of_validity {
		if let Some(pov_path) = export_pov {
			if let Ok(Some(relay_parent_header)) =
				relay_client.header(BlockId::Hash(relay_parent)).await
			{
				if let Some(header) = block_data.blocks().first().map(|b| b.header()) {
					export_pov_to_path::<Block>(
						pov_path.clone(),
						pov.clone(),
						header.hash(),
						*header.number(),
						parent_header.clone(),
						relay_parent_header.state_root,
						relay_parent_header.number,
						validation_data.max_pov_size,
					);
				}
			} else {
				tracing::error!(target: LOG_TARGET, "Failed to get relay parent header from hash: {relay_parent:?}");
			}
		}

		tracing::info!(
			target: LOG_TARGET,
			block_numbers = ?block_data.blocks().iter().map(|b| *b.header().number()).collect::<Vec<_>>(),
			"Compressed PoV size: {}kb",
			pov.block_data.0.len() as f64 / 1024f64,
		);
	}

	let session_index = match relay_client.session_index_for_child(relay_parent).await {
		Ok(session_index) => session_index,
		Err(err) => {
			tracing::error!(
				target: LOG_TARGET,
				?err,
				?relay_parent,
				"Failed to fetch session index."
			);
			return;
		},
	};

	tracing::debug!(
		target: LOG_TARGET,
		?core_index,
		block_numbers = ?block_data.blocks().iter().map(|b| *b.header().number()).collect::<Vec<_>>(),
		"Submitting collation for core.",
	);
	let (scheduling_parent, candidates_descriptor_version) = scheduling_parent
		.map_or((relay_parent, CandidateDescriptorVersion::V2), |parent| {
			(parent, CandidateDescriptorVersion::V3)
		});
	overseer_handle
		.send_msg(
			CollationGenerationMessage::SubmitSegment(SubmitSegmentParams {
				scheduling_parent,
				core_index,
				candidates_descriptor_version,
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
}
