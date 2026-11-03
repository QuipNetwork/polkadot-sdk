// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.

// Cumulus is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Cumulus is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Cumulus.  If not, see <http://www.gnu.org/licenses/>.

use crate::Client;
use codec::Encode;
use cumulus_primitives_core::{ParachainBlockData, PersistedValidationData};
use cumulus_primitives_parachain_inherent::{ParachainInherentData, INHERENT_IDENTIFIER};
use cumulus_test_relay_sproof_builder::RelayStateSproofBuilder;
use cumulus_test_runtime::{Block, GetLastTimestamp, Hash, Header};
use polkadot_primitives::{BlockNumber as PBlockNumber, Hash as PHash};
use sp_api::{ApiExt, ProofRecorder, ProofRecorderIgnoredNodes, ProvideRuntimeApi};
use sp_consensus_aura::{AuraApi, Slot};
use sp_externalities::Extensions;
use sp_runtime::{traits::Header as HeaderT, Digest, DigestItem};
use sp_trie::proof_size_extension::ProofSizeExt;

/// A struct containing a block builder and support data required to build test scenarios.
pub struct BlockBuilderAndSupportData<'a> {
	pub block_builder: sc_block_builder::BlockBuilder<'a, Block, Client>,
	pub persisted_validation_data: PersistedValidationData<PHash, PBlockNumber>,
	pub proof_recorder: ProofRecorder<Block>,
}

/// Builder for creating a block builder with customizable parameters.
pub struct BlockBuilderBuilder<'a> {
	client: &'a Client,
	at: Option<Hash>,
	validation_data: Option<PersistedValidationData<PHash, PBlockNumber>>,
	relay_sproof_builder: RelayStateSproofBuilder,
	timestamp: Option<u64>,
	ignored_nodes: Option<ProofRecorderIgnoredNodes<Block>>,
	pre_digests: Vec<DigestItem>,
}

impl<'a> BlockBuilderBuilder<'a> {
	fn new(client: &'a Client) -> Self {
		Self {
			client,
			at: None,
			validation_data: None,
			relay_sproof_builder: Default::default(),
			timestamp: None,
			ignored_nodes: None,
			pre_digests: Vec::new(),
		}
	}

	/// Set the parent block hash for the block builder.
	pub fn at(mut self, at: Hash) -> Self {
		self.at = Some(at);
		self
	}

	/// Set the validation data for the block builder.
	pub fn with_validation_data(
		mut self,
		validation_data: PersistedValidationData<PHash, PBlockNumber>,
	) -> Self {
		self.validation_data = Some(validation_data);
		self
	}

	/// Set the relay state proof builder for the block builder.
	pub fn with_relay_sproof_builder(
		mut self,
		relay_sproof_builder: RelayStateSproofBuilder,
	) -> Self {
		self.relay_sproof_builder = relay_sproof_builder;
		self
	}

	/// Set the timestamp for the block builder.
	pub fn with_timestamp(mut self, timestamp: u64) -> Self {
		self.timestamp = Some(timestamp);
		self
	}

	/// Set the ignored nodes for the proof recorder.
	pub fn with_ignored_nodes(mut self, ignored_nodes: ProofRecorderIgnoredNodes<Block>) -> Self {
		self.ignored_nodes = Some(ignored_nodes);
		self
	}

	/// Set the pre-digest items for the block builder.
	pub fn with_pre_digests(mut self, pre_digests: Vec<DigestItem>) -> Self {
		self.pre_digests = pre_digests;
		self
	}

	/// Build the block builder with the configured parameters.
	pub fn build(self) -> BlockBuilderAndSupportData<'a> {
		let at = self.at.unwrap_or_else(|| self.client.chain_info().best_hash);
		init_block_builder(
			self.client,
			at,
			self.validation_data,
			self.relay_sproof_builder,
			self.timestamp,
			self.ignored_nodes,
			Some(self.pre_digests),
		)
	}
}

/// An extension for the Cumulus test client to build a block builder.
pub trait BuildBlockBuilder {
	/// Initialize a block builder builder that can be configured and built.
	///
	/// This returns a builder that can be configured with various options like
	/// parent block hash, validation data, relay state proof builder, timestamp,
	/// ignored nodes, and pre-digests. Call `.build()` on the builder to create
	/// the actual block builder.
	///
	/// The builder will automatically create and push the inherents for you to make
	/// the block valid for the test runtime.
	fn init_block_builder_builder(&self) -> BlockBuilderBuilder<'_>;
}

fn init_block_builder(
	client: &Client,
	at: Hash,
	validation_data: Option<PersistedValidationData<PHash, PBlockNumber>>,
	mut relay_sproof_builder: RelayStateSproofBuilder,
	timestamp: Option<u64>,
	ignored_nodes: Option<ProofRecorderIgnoredNodes<Block>>,
	extra_pre_digests: Option<Vec<DigestItem>>,
) -> BlockBuilderAndSupportData<'_> {
	let mut runtime_api = client.runtime_api();
	runtime_api.set_call_context(sp_core::traits::CallContext::Onchain { import: false });
	let para_slot_duration = runtime_api.slot_duration(at).unwrap();

	let timestamp = timestamp.unwrap_or_else(|| {
		let last_timestamp =
			client.runtime_api().get_last_timestamp(at).expect("Get last timestamp");

		if last_timestamp == 0 {
			if relay_sproof_builder.current_slot != 0u64 {
				*relay_sproof_builder.current_slot * 6_000
			} else {
				std::time::SystemTime::now()
					.duration_since(std::time::SystemTime::UNIX_EPOCH)
					.expect("Time is always after UNIX_EPOCH; qed")
					.as_millis() as u64
			}
		} else {
			last_timestamp + para_slot_duration.as_millis()
		}
	});

	let slot: Slot = (timestamp / para_slot_duration.as_millis()).into();

	if relay_sproof_builder.current_slot == 0u64 {
		relay_sproof_builder.current_slot = (timestamp / 6_000).into();
	}

	let pre_digests = Digest {
		logs: extra_pre_digests
			.unwrap_or_default()
			.into_iter()
			.chain(std::iter::once(DigestItem::PreRuntime(
				sp_consensus_aura::AURA_ENGINE_ID,
				slot.encode(),
			)))
			.collect::<Vec<_>>(),
	};

	let proof_recorder =
		ProofRecorder::<Block>::with_ignored_nodes(ignored_nodes.unwrap_or_default());

	let mut extra_extensions = Extensions::default();
	extra_extensions.register(ProofSizeExt::new(proof_recorder.clone()));

	let mut block_builder = sc_block_builder::BlockBuilderBuilder::new(client)
		.on_parent_block(at)
		.fetch_parent_block_number(client)
		.unwrap()
		.with_proof_recorder(Some(proof_recorder.clone()))
		.with_inherent_digests(pre_digests)
		.with_extra_extensions(extra_extensions)
		.build()
		.expect("Creates new block builder for test runtime");

	let mut inherent_data = sp_inherents::InherentData::new();

	inherent_data
		.put_data(sp_timestamp::INHERENT_IDENTIFIER, &timestamp)
		.expect("Put timestamp failed");

	let (relay_parent_storage_root, relay_chain_state) =
		relay_sproof_builder.into_state_root_and_proof();

	let mut validation_data = validation_data.unwrap_or_default();
	validation_data.relay_parent_storage_root = relay_parent_storage_root;

	inherent_data
		.put_data(
			INHERENT_IDENTIFIER,
			&ParachainInherentData {
				validation_data: validation_data.clone(),
				relay_chain_state,
				downward_messages: Default::default(),
				horizontal_messages: Default::default(),
				relay_parent_descendants: Default::default(),
				collator_peer_id: None,
			},
		)
		.expect("Put validation function params failed");

	let inherents = block_builder.create_inherents(inherent_data).expect("Creates inherents");

	inherents
		.into_iter()
		.for_each(|ext| block_builder.push(ext).expect("Pushes inherent"));

	BlockBuilderAndSupportData {
		block_builder,
		persisted_validation_data: validation_data,
		proof_recorder,
	}
}

impl BuildBlockBuilder for Client {
	fn init_block_builder_builder(&self) -> BlockBuilderBuilder<'_> {
		BlockBuilderBuilder::new(self)
	}
}

/// Extension trait for the [`BlockBuilder`](sc_block_builder::BlockBuilder) to build directly a
/// [`ParachainBlockData`].
pub trait BuildParachainBlockData {
	/// Directly build the [`ParachainBlockData`] from the block that comes out of the block
	/// builder.
	fn build_parachain_block(self, parent_state_root: Hash) -> ParachainBlockData<Block>;
}

impl<'a> BuildParachainBlockData for sc_block_builder::BlockBuilder<'a, Block, Client> {
	fn build_parachain_block(self, parent_state_root: Hash) -> ParachainBlockData<Block> {
		let proof_recorder = self
			.proof_recorder()
			.expect("Proof recorder is always set for the test block builder; qed");
		let built_block = self.build().expect("Builds the block");

		let storage_proof = proof_recorder
			.drain_storage_proof()
			.into_compact_proof::<<Header as HeaderT>::Hashing>(parent_state_root)
			.expect("Creates the compact proof");

		ParachainBlockData::new(vec![built_block.block], storage_proof, None)
	}
}
