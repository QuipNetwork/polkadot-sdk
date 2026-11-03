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

//! Stock, pure Aura collators.
//!
//! This includes the [`lookahead`] collator, which prospectively builds on parachain blocks which
//! have not yet been included in the relay chain, and also the [`slot_based`] collator.

use crate::collator::SlotClaim;
use codec::Codec;
use cumulus_client_consensus_common::{self as consensus_common, ParentSearchParams};
use cumulus_primitives_aura::{AuraUnincludedSegmentApi, Slot};
use cumulus_primitives_core::{
	relay_chain::Header as RelayHeader, BlockT, KeyToIncludeInRelayProof, RelayProofRequest,
};
use cumulus_relay_chain_interface::{OverseerHandle, RelayChainInterface};
use polkadot_node_subsystem::messages::CollatorProtocolMessage;
use polkadot_node_subsystem_util::runtime::ClaimQueueSnapshot;
use polkadot_primitives::{
	Hash as RelayHash, Id as ParaId, OccupiedCoreAssumption, ValidationCodeHash,
};
use sc_client_api::HeaderBackend;
use sc_consensus_aura::{standalone as aura_internal, AuraApi};
use sp_api::{ApiExt, ProvideRuntimeApi};
use sp_core::Pair;
use sp_keystore::KeystorePtr;
use sp_runtime::traits::Header;
use sp_timestamp::Timestamp;

pub mod lookahead;
pub mod slot_based;

// Helper to pre-connect to the backing group we got assigned to and keep the connection
// open until backing group changes or own slot ends.
struct BackingGroupConnectionHelper {
	keystore: sp_keystore::KeystorePtr,
	overseer_handle: OverseerHandle,
	our_slot: Option<Slot>,
}

impl BackingGroupConnectionHelper {
	pub fn new(keystore: sp_keystore::KeystorePtr, overseer_handle: OverseerHandle) -> Self {
		Self { keystore, overseer_handle, our_slot: None }
	}

	async fn send_subsystem_message(&mut self, message: CollatorProtocolMessage) {
		self.overseer_handle.send_msg(message, "BackingGroupConnectionHelper").await;
	}

	/// Update the current slot and initiate connections to backing groups if needed.
	pub async fn update<P>(&mut self, current_slot: Slot, authorities: &[P::Public])
	where
		P: sp_core::Pair + Send + Sync,
		P::Public: Codec,
	{
		if Some(current_slot) <= self.our_slot {
			// Current slot or next slot is ours.
			// We already sent pre-connect message, no need to proceed further.
			return;
		}

		let next_slot = current_slot + 1;
		let next_slot_is_ours =
			aura_internal::claim_slot::<P>(next_slot, authorities, &self.keystore)
				.await
				.is_some();

		if next_slot_is_ours {
			// Only send message if we were not connected. This avoids sending duplicate messages
			// when running with a single collator.
			if self.our_slot.is_none() {
				// Next slot is ours, send connect message.
				tracing::debug!(target: crate::LOG_TARGET, "Our slot {} is next, connecting to backing groups", next_slot);
				self.send_subsystem_message(CollatorProtocolMessage::ConnectToBackingGroups)
					.await;
			}
			self.our_slot = Some(next_slot);
		} else if self.our_slot.take().is_some() {
			// Next slot is not ours, send disconnect only if we had a slot before.
			tracing::debug!(target: crate::LOG_TARGET, "Current slot = {}, disconnecting from backing groups", current_slot);
			self.send_subsystem_message(CollatorProtocolMessage::DisconnectFromBackingGroups)
				.await;
		}
	}
}

/// Check the `local_validation_code_hash` against the validation code hash in the relay chain
/// state.
///
/// If the code hashes do not match, it prints a warning.
async fn check_validation_code_or_log(
	local_validation_code_hash: &ValidationCodeHash,
	para_id: ParaId,
	relay_client: &impl RelayChainInterface,
	relay_parent: RelayHash,
) {
	let state_validation_code_hash = match relay_client
		.validation_code_hash(relay_parent, para_id, OccupiedCoreAssumption::Included)
		.await
	{
		Ok(hash) => hash,
		Err(error) => {
			tracing::debug!(
				target: super::LOG_TARGET,
				%error,
				?relay_parent,
				%para_id,
				"Failed to fetch validation code hash",
			);
			return;
		},
	};

	match state_validation_code_hash {
		Some(state) => {
			if state != *local_validation_code_hash {
				tracing::warn!(
					target: super::LOG_TARGET,
					%para_id,
					?relay_parent,
					?local_validation_code_hash,
					relay_validation_code_hash = ?state,
					"Parachain code doesn't match validation code stored in the relay chain state.",
				);
			}
		},
		None => {
			tracing::warn!(
				target: super::LOG_TARGET,
				%para_id,
				?relay_parent,
				"Could not find validation code for parachain in the relay chain state.",
			);
		},
	}
}

// Returns the claim queue at the given relay parent.
async fn claim_queue_at(
	relay_parent: RelayHash,
	relay_client: &impl RelayChainInterface,
) -> ClaimQueueSnapshot {
	// Get `ClaimQueue` from runtime
	match relay_client.claim_queue(relay_parent).await {
		Ok(claim_queue) => claim_queue.into(),
		Err(error) => {
			tracing::error!(
				target: crate::LOG_TARGET,
				?error,
				?relay_parent,
				"Failed to query claim queue runtime API",
			);
			Default::default()
		},
	}
}

// Checks if we own the slot at the given block.
async fn claim_slot<Block: BlockT, Client, P>(
	para_slot: Slot,
	timestamp: Timestamp,
	parent_hash: Block::Hash,
	client: &Client,
	keystore: &KeystorePtr,
) -> Option<SlotClaim<P::Public>>
where
	Client: ProvideRuntimeApi<Block>,
	Client::Api: AuraApi<Block, P::Public> + ApiExt<Block>,
	P: Pair,
	P::Public: Codec,
	P::Signature: Codec,
{
	let mut runtime_api = client.runtime_api();
	runtime_api.set_call_context(sp_core::traits::CallContext::Onchain { import: false });
	let authorities = runtime_api.authorities(parent_hash).ok()?;
	let author_pub = aura_internal::claim_slot::<P>(para_slot, &authorities, keystore).await?;
	Some(SlotClaim::unchecked::<P>(author_pub, para_slot, timestamp))
}

// Checks if there is space in the unincluded segment.
async fn can_build_upon<Block: BlockT, Client>(
	parent_hash: Block::Hash,
	included_block: Block::Hash,
	relay_slot: Slot,
	para_slot: Slot,
	client: &Client,
) -> bool
where
	Client: ProvideRuntimeApi<Block>,
	Client::Api: AuraUnincludedSegmentApi<Block> + ApiExt<Block>,
{
	// This function is typically called when we want to build block N. At that point, the
	// unincluded segment in the runtime is unaware of the hash of block N-1. If the unincluded
	// segment in the runtime is full, but block N-1 is the included block, the unincluded segment
	// should have length 0 and we can build. Since the hash is not available to the runtime
	// however, we need this extra check here.
	if parent_hash == included_block {
		return true;
	}

	let runtime_api = client.runtime_api();
	let Some(api_version) = runtime_api
		.api_version::<dyn AuraUnincludedSegmentApi<Block>>(parent_hash)
		.ok()
		.flatten()
	else {
		return false;
	};

	let slot = if api_version > 1 { relay_slot } else { para_slot };

	runtime_api
		.can_build_upon(parent_hash, included_block, slot)
		.ok()
		.unwrap_or(false)
}

/// Use [`cumulus_client_consensus_common::find_parent_for_building`] to find the best parachain
/// block to build on.
///
/// If the best parent does not pass `filter_parent`, walks backwards through ancestors
/// until finding one that does, or reaching the included block.
async fn find_parent<Block>(
	relay_client: &impl RelayChainInterface,
	para_backend: &impl sc_client_api::Backend<Block>,
	para_id: ParaId,
	params: ParentSearchParams,
	filter_parent: impl Fn(&Block::Header) -> bool,
) -> Option<consensus_common::ParentSearchResult<Block>>
where
	Block: BlockT,
{
	let mut result = match cumulus_client_consensus_common::find_parent_for_building::<Block>(
		relay_client,
		para_backend,
		para_id,
		params.clone(),
	)
	.await
	{
		Ok(Some(result)) => result,
		Ok(None) => {
			tracing::warn!(
				target: crate::LOG_TARGET,
				?params,
				"Could not find parent to build upon.",
			);
			return None;
		},
		Err(e) => {
			tracing::error!(
				target: crate::LOG_TARGET,
				?params,
				err = ?e,
				"Could not find parent to build upon"
			);
			return None;
		},
	};

	// If the best parent doesn't pass the filter (e.g. it's a middle block in a bundle),
	// walk backwards towards the included block until we find one that does.
	// This avoids falling all the way back to the included block when there are valid
	// last-in-core ancestors closer to the chain tip.
	while !filter_parent(&result.best_parent_header) {
		let parent_hash = *result.best_parent_header.parent_hash();
		match para_backend.blockchain().header(parent_hash) {
			Ok(Some(header)) => {
				result.best_parent_header = header;
				if parent_hash == result.included_at_scheduling.hash() {
					break;
				}
			},
			_ => {
				result.best_parent_header = result.included_at_scheduling.clone();
				break;
			},
		}
	}

	Some(result)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::collators::BackingGroupConnectionHelper;
	use codec::Encode;
	use cumulus_primitives_aura::Slot;
	use cumulus_primitives_core::BlockT;
	use cumulus_relay_chain_interface::PHash;
	use cumulus_test_client::{
		runtime::{Block, Hash},
		BuildBlockBuilder, Client, DefaultTestClientBuilderExt, TestClientBuilder,
		TestClientBuilderExt,
	};
	use cumulus_test_relay_sproof_builder::RelayStateSproofBuilder;
	use futures::StreamExt;
	use polkadot_overseer::{Event, Handle};
	use polkadot_primitives::HeadData;
	use sc_consensus::{BlockImport, BlockImportParams, ForkChoiceStrategy};
	use sp_consensus::BlockOrigin;
	use sp_keystore::{Keystore, KeystorePtr};
	use std::sync::{Arc, Mutex};

	async fn import_block<I: BlockImport<Block>>(
		importer: &I,
		block: Block,
		origin: BlockOrigin,
		import_as_best: bool,
	) {
		let (header, body) = block.deconstruct();

		let mut block_import_params = BlockImportParams::new(origin, header);
		block_import_params.fork_choice = Some(ForkChoiceStrategy::Custom(import_as_best));
		block_import_params.body = Some(body);
		importer.import_block(block_import_params).await.unwrap();
	}

	fn sproof_with_parent_by_hash(client: &Client, hash: PHash) -> RelayStateSproofBuilder {
		let header = client.header(hash).ok().flatten().expect("No header for parent block");
		let included = HeadData(header.encode());
		let mut builder = RelayStateSproofBuilder::default();
		builder.para_id = cumulus_test_client::runtime::PARACHAIN_ID.into();
		builder.included_para_head = Some(included);

		builder
	}
	async fn build_and_import_block(client: &Client, included: Hash) -> Block {
		let sproof = sproof_with_parent_by_hash(client, included);

		let block_builder = client
			.init_block_builder_builder()
			.with_relay_sproof_builder(sproof)
			.build()
			.block_builder;

		let block = block_builder.build().unwrap().block;

		let origin = BlockOrigin::NetworkInitialSync;
		import_block(client, block.clone(), origin, true).await;
		block
	}

	fn set_up_components(num_authorities: usize) -> (Arc<Client>, KeystorePtr) {
		let keystore = Arc::new(sp_keystore::testing::MemoryKeystore::new()) as Arc<_>;
		for key in sp_keyring::Sr25519Keyring::iter().take(num_authorities) {
			Keystore::sr25519_generate_new(
				&*keystore,
				sp_application_crypto::key_types::AURA,
				Some(&key.to_seed()),
			)
			.expect("Can insert key into MemoryKeyStore");
		}
		(Arc::new(TestClientBuilder::new().build()), keystore)
	}

	/// This tests a special scenario where the unincluded segment in the runtime
	/// is full. We are calling `can_build_upon`, passing the last built block as the
	/// included one. In the runtime we will not find the hash of the included block in the
	/// unincluded segment. The `can_build_upon` runtime API would therefore return `false`, but
	/// we are ensuring on the node side that we are are always able to build on the included block.
	#[tokio::test]
	async fn test_can_build_upon() {
		sp_tracing::try_init_simple();

		let (client, _keystore) = set_up_components(6);

		let genesis_hash = client.chain_info().genesis_hash;
		let mut last_hash = genesis_hash;

		// Fill up the unincluded segment tracker in the runtime.
		while can_build_upon::<_, _>(
			last_hash,
			genesis_hash,
			Slot::from(u64::MAX),
			Slot::from(u64::MAX),
			&*client,
		)
		.await
		{
			let block = build_and_import_block(&client, genesis_hash).await;
			last_hash = block.header().hash();
		}

		// Blocks were built with the genesis hash set as included block.
		// We call `can_build_upon` with the last built block as the included block.
		let result = can_build_upon::<_, _>(
			last_hash,
			last_hash,
			Slot::from(u64::MAX),
			Slot::from(u64::MAX),
			&*client,
		)
		.await;
		assert!(result);
	}

	/// Helper to create a mock overseer handle and message recorder
	fn create_overseer_handle() -> (OverseerHandle, Arc<Mutex<Vec<CollatorProtocolMessage>>>) {
		let messages = Arc::new(Mutex::new(Vec::new()));
		let messages_clone = messages.clone();

		let (tx, mut rx) = polkadot_node_subsystem_util::metered::channel(100);

		// Spawn a task to receive and record overseer messages
		tokio::spawn(async move {
			while let Some(event) = rx.next().await {
				if let Event::MsgToSubsystem { msg, .. } = event {
					if let polkadot_node_subsystem::AllMessages::CollatorProtocol(cp_msg) = msg {
						messages_clone.lock().unwrap().push(cp_msg);
					}
				}
			}
		});

		(Handle::new(tx), messages)
	}

	#[tokio::test]
	async fn preconnect_when_next_slot_is_ours() {
		let (client, keystore) = set_up_components(1);
		let genesis_hash = client.chain_info().genesis_hash;
		let (overseer_handle, messages_recorder) = create_overseer_handle();

		let mut helper = BackingGroupConnectionHelper::new(keystore, overseer_handle);

		// Fetch authorities for the update call
		let authorities = client.runtime_api().authorities(genesis_hash).unwrap();

		// Update with slot 5, next slot (6) should be ours
		helper
			.update::<sp_consensus_aura::sr25519::AuthorityPair>(Slot::from(5), &authorities)
			.await;

		// Give time for message to be processed
		tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

		let messages = messages_recorder.lock().unwrap();
		assert_eq!(messages.len(), 1);
		assert!(matches!(messages[0], CollatorProtocolMessage::ConnectToBackingGroups));
		assert_eq!(helper.our_slot, Some(Slot::from(6)));
	}

	#[tokio::test]
	async fn preconnect_no_duplicate_connect_message() {
		let (client, keystore) = set_up_components(1);
		let genesis_hash = client.chain_info().genesis_hash;
		let (overseer_handle, messages_recorder) = create_overseer_handle();

		let mut helper = BackingGroupConnectionHelper::new(keystore, overseer_handle);

		// Fetch authorities for the update calls
		let authorities = client.runtime_api().authorities(genesis_hash).unwrap();

		// Update with slot 5, next slot (6) is ours
		helper
			.update::<sp_consensus_aura::sr25519::AuthorityPair>(Slot::from(5), &authorities)
			.await;

		// Give time for message to be processed
		tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
		assert_eq!(messages_recorder.lock().unwrap().len(), 1);
		messages_recorder.lock().unwrap().clear();

		// Update with slot 5 again - should not send another message
		helper
			.update::<sp_consensus_aura::sr25519::AuthorityPair>(Slot::from(5), &authorities)
			.await;
		tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
		assert_eq!(messages_recorder.lock().unwrap().len(), 0);

		// Update with slot 1 (our slot) - should not send another message
		helper
			.update::<sp_consensus_aura::sr25519::AuthorityPair>(Slot::from(6), &authorities)
			.await;
		tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
		assert_eq!(messages_recorder.lock().unwrap().len(), 0);
	}

	#[tokio::test]
	async fn preconnect_disconnect_when_slot_passes() {
		let (client, keystore) = set_up_components(1);
		let genesis_hash = client.chain_info().genesis_hash;
		let (overseer_handle, messages_recorder) = create_overseer_handle();

		let mut helper = BackingGroupConnectionHelper::new(keystore, overseer_handle);

		// Fetch authorities for the update calls
		let authorities = client.runtime_api().authorities(genesis_hash).unwrap();

		// Slot 0 -> Alice, Slot 1 -> Bob, Slot 2 -> Charlie, Slot 3 -> Dave, Slot 4 -> Eve,
		// Slot 5 -> Ferdie, Slot 6 -> Alice

		// Update with slot 5, next slot (6) is ours -> should connect
		helper
			.update::<sp_consensus_aura::sr25519::AuthorityPair>(Slot::from(5), &authorities)
			.await;
		tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
		assert_eq!(helper.our_slot, Some(Slot::from(6)));
		messages_recorder.lock().unwrap().clear();

		// Update with slot 8, next slot (9) is Charlie's -> should disconnect
		helper
			.update::<sp_consensus_aura::sr25519::AuthorityPair>(Slot::from(8), &authorities)
			.await;
		tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

		{
			let messages = messages_recorder.lock().unwrap();
			assert_eq!(messages.len(), 1, "Expected exactly one disconnect message");
			assert!(matches!(messages[0], CollatorProtocolMessage::DisconnectFromBackingGroups));
			assert_eq!(helper.our_slot, None);
		}

		messages_recorder.lock().unwrap().clear();

		// Update again with slot 8, next slot (9) is Charlie's -> should not send another
		// disconnect message
		helper
			.update::<sp_consensus_aura::sr25519::AuthorityPair>(Slot::from(8), &authorities)
			.await;
		tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

		let messages = messages_recorder.lock().unwrap();
		assert_eq!(messages.len(), 0, "Expected no messages");
		assert_eq!(helper.our_slot, None);
	}

	#[tokio::test]
	async fn preconnect_no_disconnect_without_previous_connection() {
		let (client, keystore) = set_up_components(1);
		let genesis_hash = client.chain_info().genesis_hash;
		let (overseer_handle, messages_recorder) = create_overseer_handle();

		let mut helper = BackingGroupConnectionHelper::new(keystore, overseer_handle);

		// Fetch authorities for the update call
		let authorities = client.runtime_api().authorities(genesis_hash).unwrap();

		// Slot 0 -> Alice, Slot 1 -> Bob, Slot 2 -> Charlie, Slot 3 -> Dave, Slot 4 -> Eve,
		// Slot 5 -> Ferdie

		// Update with slot 1 (Bob's slot), next slot (2) is Charlie's
		// Since we never connected before (our_slot is None), we should not send disconnect
		helper
			.update::<sp_consensus_aura::sr25519::AuthorityPair>(Slot::from(1), &authorities)
			.await;

		tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
		// Should not send any message since we never connected
		assert_eq!(messages_recorder.lock().unwrap().len(), 0);
		assert_eq!(helper.our_slot, None);
	}

	#[tokio::test]
	async fn preconnect_multiple_cycles() {
		let (client, keystore) = set_up_components(1);
		let genesis_hash = client.chain_info().genesis_hash;
		let (overseer_handle, messages_recorder) = create_overseer_handle();

		let mut helper = BackingGroupConnectionHelper::new(keystore, overseer_handle);

		// Fetch authorities for the update calls
		let authorities = client.runtime_api().authorities(genesis_hash).unwrap();

		// Slot 0 -> Alice, Slot 1 -> Bob, Slot 2 -> Charlie, Slot 3 -> Dave, Slot 4 -> Eve,
		// Slot 5 -> Ferdie, Slot 6 -> Alice, Slot 7 -> Bob, ...

		// Cycle 1: Connect at slot 5, next slot (6) is ours
		helper
			.update::<sp_consensus_aura::sr25519::AuthorityPair>(Slot::from(5), &authorities)
			.await;
		tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
		{
			let messages = messages_recorder.lock().unwrap();
			assert_eq!(messages.len(), 1);
			assert!(matches!(messages[0], CollatorProtocolMessage::ConnectToBackingGroups));
		}
		assert_eq!(helper.our_slot, Some(Slot::from(6)));
		messages_recorder.lock().unwrap().clear();

		// Cycle 1: Disconnect at slot 7, next slot (8) is Charlie's
		helper
			.update::<sp_consensus_aura::sr25519::AuthorityPair>(Slot::from(7), &authorities)
			.await;
		tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
		{
			let messages = messages_recorder.lock().unwrap();
			assert_eq!(messages.len(), 1);
			assert!(matches!(messages[0], CollatorProtocolMessage::DisconnectFromBackingGroups));
		}
		assert_eq!(helper.our_slot, None);
		messages_recorder.lock().unwrap().clear();

		// Cycle 2: Connect again at slot 11, next slot (12) is ours
		helper
			.update::<sp_consensus_aura::sr25519::AuthorityPair>(Slot::from(11), &authorities)
			.await;
		tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
		{
			let messages = messages_recorder.lock().unwrap();
			assert_eq!(messages.len(), 1);
			assert!(matches!(messages[0], CollatorProtocolMessage::ConnectToBackingGroups));
		}
		assert_eq!(helper.our_slot, Some(Slot::from(12)));
	}

	#[tokio::test]
	async fn preconnect_handles_empty_authorities() {
		let keystore = Arc::new(sp_keystore::testing::MemoryKeystore::new()) as Arc<_>;
		let (overseer_handle, messages_recorder) = create_overseer_handle();

		let mut helper = BackingGroupConnectionHelper::new(keystore, overseer_handle);

		// Pass empty authorities list
		let authorities = vec![];
		helper
			.update::<sp_consensus_aura::sr25519::AuthorityPair>(Slot::from(0), &authorities)
			.await;

		tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
		// Should not send any message if authorities list is empty
		assert_eq!(messages_recorder.lock().unwrap().len(), 0);
	}
}

/// Fetches relay chain storage proof requests from the parachain runtime.
///
/// Queries the runtime API to determine which relay chain storage keys
/// (both top-level and child trie keys) should be included in the relay chain state proof.
///
/// Falls back to an empty request if the runtime API call fails or is not implemented.
pub(crate) fn get_relay_proof_request<Block, Client>(
	client: &Client,
	parent_hash: Block::Hash,
) -> RelayProofRequest
where
	Block: BlockT,
	Client: ProvideRuntimeApi<Block>,
	Client::Api: KeyToIncludeInRelayProof<Block>,
{
	client.runtime_api().keys_to_prove(parent_hash).unwrap_or_else(|e| {
		tracing::debug!(
			target: crate::LOG_TARGET,
			error = ?e,
			"Failed to fetch relay proof requests from runtime, using empty request"
		);
		Default::default()
	})
}

/// Holds a relay parent and its descendants.
#[derive(Clone)]
pub struct RelayParentData {
	/// The relay parent block header
	relay_parent: RelayHeader,
	/// Ordered collection of descendant block headers, from oldest to newest
	descendants: Vec<RelayHeader>,
}

impl RelayParentData {
	/// Creates a new instance with the given relay parent and no descendants.
	pub fn new(relay_parent: RelayHeader) -> Self {
		Self { relay_parent, descendants: Default::default() }
	}

	/// Creates a new instance with the given relay parent and descendants.
	pub fn new_with_descendants(relay_parent: RelayHeader, descendants: Vec<RelayHeader>) -> Self {
		Self { relay_parent, descendants }
	}

	/// Returns a reference to the relay parent header.
	pub fn relay_parent(&self) -> &RelayHeader {
		&self.relay_parent
	}

	/// Takes the descendants list.
	///
	/// List is ordered from oldest to newest.
	pub fn take_descendants(&mut self) -> Vec<RelayHeader> {
		std::mem::take(&mut self.descendants)
	}

	/// Returns the number of descendants.
	#[cfg(test)]
	pub fn descendants_len(&self) -> usize {
		self.descendants.len()
	}

	/// Consumes the structure and returns a vector containing the relay parent followed by its
	/// descendants in chronological order. The resulting list should be provided to the parachain
	/// inherent data.
	pub fn into_inherent_descendant_list(self) -> Vec<RelayHeader> {
		let Self { relay_parent, mut descendants } = self;

		if descendants.is_empty() {
			return Default::default();
		}

		let mut result = vec![relay_parent];
		result.append(&mut descendants);
		result
	}
}
