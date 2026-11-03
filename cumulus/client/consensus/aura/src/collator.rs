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

//! The core collator logic for Aura - slot claiming, block proposing, and collation
//! packaging.
//!
//! The [`Collator`] struct exposed here is meant to be a component of higher-level logic
//! which actually manages the control flow of the collator - which slots to claim, how
//! many collations to build, when to work, etc.
//!
//! This module also exposes some standalone functions for common operations when building
//! aura-based collators.

use crate::collators::RelayParentData;
use codec::Codec;
use cumulus_client_collator::service::ServiceInterface as CollatorServiceInterface;
use cumulus_client_consensus_common::{
	self as consensus_common, ParachainBlockImportMarker, ParachainCandidate,
};
use cumulus_client_parachain_inherent::{ParachainInherentData, ParachainInherentDataProvider};
use cumulus_primitives_core::{
	relay_chain::Hash as PHash, DigestItem, ParachainBlockData, PersistedValidationData,
	RelayProofRequest,
};
use cumulus_relay_chain_interface::RelayChainInterface;
use futures::prelude::*;
use polkadot_node_primitives::{Collation, MaybeCompressedPoV};
use polkadot_primitives::{Header as PHeader, Id as ParaId};
use sc_client_api::BackendTransaction;
use sc_consensus::{BlockImport, BlockImportParams, ForkChoiceStrategy, StateAction};
use sc_consensus_aura::standalone as aura_internal;
use sc_network_types::PeerId;
use sp_api::{ApiExt, ProofRecorder, ProvideRuntimeApi, StorageProof};
use sp_application_crypto::AppPublic;
use sp_consensus::{BlockOrigin, Environment, ProposeArgs, Proposer};
use sp_consensus_aura::{AuraApi, Slot, SlotDuration};
use sp_core::crypto::Pair;
use sp_externalities::Extensions;
use sp_inherents::{CreateInherentDataProviders, InherentData, InherentDataProvider};
use sp_keystore::KeystorePtr;
use sp_runtime::{
	generic::Digest,
	traits::{Block as BlockT, HashingFor, Header as HeaderT, Member},
};
use sp_state_machine::StorageChanges;
use sp_timestamp::Timestamp;
use sp_trie::proof_size_extension::ProofSizeExt;
use std::{error::Error, time::Duration};

/// Parameters for instantiating a [`Collator`].
pub struct Params<BI, CIDP, RClient, PF, CS> {
	/// A builder for inherent data builders.
	pub create_inherent_data_providers: CIDP,
	/// The block import handle.
	pub block_import: BI,
	/// An interface to the relay-chain client.
	pub relay_client: RClient,
	/// The keystore handle used for accessing parachain key material.
	pub keystore: KeystorePtr,
	/// The collator network peer id.
	pub collator_peer_id: PeerId,
	/// The identifier of the parachain within the relay-chain.
	pub para_id: ParaId,
	/// The proposer used for building blocks.
	pub proposer: PF,
	/// The collator service used for bundling proposals into collations and announcing
	/// to the network.
	pub collator_service: CS,
}

/// Parameters for [`Collator::build_block_and_import`].
pub struct BuildBlockAndImportParams<'a, Block: BlockT, P: Pair> {
	/// The parent header to build on top of.
	pub parent_header: &'a Block::Header,
	/// The slot claim for this block.
	pub slot_claim: &'a SlotClaim<P::Public>,
	/// Additional pre-digest items to include.
	pub additional_pre_digest: Vec<DigestItem>,
	/// Parachain-specific inherent data.
	pub parachain_inherent_data: ParachainInherentData,
	/// Other inherent data (timestamp, etc.).
	pub extra_inherent_data: InherentData,
	/// Maximum duration to spend on block proposal.
	pub proposal_duration: Duration,
	/// Maximum PoV size in bytes.
	pub max_pov_size: usize,
	/// Optional [`ProofRecorder`] to use.
	///
	/// If not set, one will be initialized internally and [`ProofSizeExt`] will be
	/// registered.
	pub storage_proof_recorder: Option<ProofRecorder<Block>>,
	/// Extra extensions to forward to the block production.
	pub extra_extensions: Extensions,
}

/// Result of [`Collator::build_block_and_import`].
pub struct BuiltBlock<Block: BlockT> {
	/// The block that was built.
	pub block: Block,
	/// The proof that was recorded while building the block.
	pub proof: StorageProof,
	/// The transaction resulting from building the block.
	///
	/// This contains all the state changes.
	pub backend_transaction: BackendTransaction<HashingFor<Block>>,
}

impl<Block: BlockT> From<BuiltBlock<Block>> for ParachainCandidate<Block> {
	fn from(built: BuiltBlock<Block>) -> Self {
		Self { block: built.block, proof: built.proof }
	}
}

/// A utility struct for writing collation logic that makes use of Aura entirely
/// or in part. See module docs for more details.
pub struct Collator<Block, P, BI, CIDP, RClient, PF, CS> {
	create_inherent_data_providers: CIDP,
	block_import: BI,
	relay_client: RClient,
	keystore: KeystorePtr,
	para_id: ParaId,
	proposer: PF,
	collator_service: CS,
	_marker: std::marker::PhantomData<(Block, Box<dyn Fn(P) + Send + Sync + 'static>)>,
}

impl<Block, P, BI, CIDP, RClient, PF, CS> Collator<Block, P, BI, CIDP, RClient, PF, CS>
where
	Block: BlockT,
	RClient: RelayChainInterface,
	CIDP: CreateInherentDataProviders<Block, ()> + 'static,
	BI: BlockImport<Block> + ParachainBlockImportMarker + Send + Sync + 'static,
	PF: Environment<Block>,
	CS: CollatorServiceInterface<Block>,
	P: Pair,
	P::Public: AppPublic + Member,
	P::Signature: TryFrom<Vec<u8>> + Member + Codec,
{
	/// Instantiate a new instance of the `Aura` manager.
	pub fn new(params: Params<BI, CIDP, RClient, PF, CS>) -> Self {
		Collator {
			create_inherent_data_providers: params.create_inherent_data_providers,
			block_import: params.block_import,
			relay_client: params.relay_client,
			keystore: params.keystore,
			para_id: params.para_id,
			proposer: params.proposer,
			collator_service: params.collator_service,
			_marker: std::marker::PhantomData,
		}
	}

	/// Explicitly creates the inherent data for parachain block authoring and overrides
	/// the timestamp inherent data with the one provided, if any. Additionally, allows to specify
	/// relay parent descendants that can be used to prevent authoring at the tip of the relay
	/// chain.
	pub async fn create_inherent_data_with_rp_offset(
		&self,
		relay_parent: PHash,
		validation_data: &PersistedValidationData,
		parent_hash: Block::Hash,
		timestamp: impl Into<Option<Timestamp>>,
		relay_parent_descendants: Option<RelayParentData>,
		relay_proof_request: RelayProofRequest,
		collator_peer_id: PeerId,
	) -> Result<(ParachainInherentData, InherentData), Box<dyn Error + Send + Sync + 'static>> {
		let paras_inherent_data = ParachainInherentDataProvider::create_at(
			relay_parent,
			&self.relay_client,
			validation_data,
			self.para_id,
			relay_parent_descendants
				.map(RelayParentData::into_inherent_descendant_list)
				.unwrap_or_default(),
			relay_proof_request,
			collator_peer_id,
		)
		.await;

		let paras_inherent_data = match paras_inherent_data {
			Some(p) => p,
			None => {
				return Err(
					format!("Could not create paras inherent data at {:?}", relay_parent).into()
				)
			},
		};

		let mut other_inherent_data = self
			.create_inherent_data_providers
			.create_inherent_data_providers(parent_hash, ())
			.map_err(|e| e as Box<dyn Error + Send + Sync + 'static>)
			.await?
			.create_inherent_data()
			.await
			.map_err(Box::new)?;

		if let Some(timestamp) = timestamp.into() {
			other_inherent_data.replace_data(sp_timestamp::INHERENT_IDENTIFIER, &timestamp);
		}

		Ok((paras_inherent_data, other_inherent_data))
	}

	/// Explicitly creates the inherent data for parachain block authoring and overrides
	/// the timestamp inherent data with the one provided, if any.
	pub async fn create_inherent_data(
		&self,
		relay_parent: PHash,
		validation_data: &PersistedValidationData,
		parent_hash: Block::Hash,
		timestamp: impl Into<Option<Timestamp>>,
		relay_proof_request: RelayProofRequest,
		collator_peer_id: PeerId,
	) -> Result<(ParachainInherentData, InherentData), Box<dyn Error + Send + Sync + 'static>> {
		self.create_inherent_data_with_rp_offset(
			relay_parent,
			validation_data,
			parent_hash,
			timestamp,
			None,
			relay_proof_request,
			collator_peer_id,
		)
		.await
	}

	/// Build and import a parachain block using the given parameters.
	pub async fn build_block_and_import(
		&mut self,
		params: BuildBlockAndImportParams<'_, Block, P>,
	) -> Result<Option<BuiltBlock<Block>>, Box<dyn Error + Send + 'static>> {
		let Some((built_block, import_block)) = self.build_block(params).await? else {
			return Ok(None);
		};

		self.import_block(import_block).await?;

		Ok(Some(built_block))
	}

	/// Build a parachain block using the given parameters.
	pub async fn build_block(
		&mut self,
		mut params: BuildBlockAndImportParams<'_, Block, P>,
	) -> Result<
		Option<(BuiltBlock<Block>, BlockImportParams<Block>)>,
		Box<dyn Error + Send + 'static>,
	> {
		let mut digest = params.additional_pre_digest;
		digest.push(params.slot_claim.pre_digest.clone());

		// Create the proposer using the factory
		let proposer = self
			.proposer
			.init(&params.parent_header)
			.await
			.map_err(|e| Box::new(e) as Box<dyn Error + Send>)?;

		// Prepare inherent data - merge parachain inherent data with other inherent data
		let mut inherent_data_combined = params.extra_inherent_data;
		params
			.parachain_inherent_data
			.provide_inherent_data(&mut inherent_data_combined)
			.await
			.map_err(|e| Box::new(e) as Box<dyn Error + Send>)?;

		let recorder_passed = params.storage_proof_recorder.is_some();
		let storage_proof_recorder = params.storage_proof_recorder.unwrap_or_default();
		let proof_size_ext_registered =
			params.extra_extensions.is_registered(ProofSizeExt::type_id());

		if !proof_size_ext_registered {
			params
				.extra_extensions
				.register(ProofSizeExt::new(storage_proof_recorder.clone()));
		} else if !recorder_passed {
			return Err(
				Box::from("`ProofSizeExt` registered, but no `storage_proof_recorder` provided. This is a bug.")
					as Box<dyn Error + Send + Sync>
			);
		}

		// Create proposal arguments
		let propose_args = ProposeArgs {
			inherent_data: inherent_data_combined,
			inherent_digests: Digest { logs: digest },
			max_duration: params.proposal_duration,
			block_size_limit: Some(params.max_pov_size),
			extra_extensions: params.extra_extensions,
			storage_proof_recorder: Some(storage_proof_recorder.clone()),
		};

		// Propose the block
		let proposal = proposer
			.propose(propose_args)
			.await
			.map_err(|e| Box::new(e) as Box<dyn Error + Send>)?;

		let sealed_importable = seal::<_, P>(
			proposal.block,
			proposal.storage_changes,
			&params.slot_claim.author_pub,
			&self.keystore,
		)
		.map_err(|e| e as Box<dyn Error + Send>)?;

		let block = Block::new(
			sealed_importable.post_header(),
			sealed_importable
				.body
				.as_ref()
				.expect("body always created with this `propose` fn; qed")
				.clone(),
		);

		let Some(backend_transaction) = sealed_importable
			.state_action
			.as_storage_changes()
			.map(|c| c.transaction.clone())
		else {
			tracing::error!(target: crate::LOG_TARGET, "Building a block should return storage changes!");

			return Ok(None);
		};

		let proof = storage_proof_recorder.drain_storage_proof();

		Ok(Some((BuiltBlock { block, proof, backend_transaction }, sealed_importable)))
	}

	/// Import the given `import_block`.
	pub async fn import_block(
		&mut self,
		import_block: BlockImportParams<Block>,
	) -> Result<(), Box<dyn Error + Send + 'static>> {
		self.block_import
			.import_block(import_block)
			.map_err(|e| Box::new(e) as Box<dyn Error + Send>)
			.await
			.map(drop)
	}

	/// Propose, seal, import a block and packaging it into a collation.
	///
	/// Provide the slot to build at as well as any other necessary pre-digest logs,
	/// the inherent data, and the proposal duration and PoV size limits.
	///
	/// The Aura pre-digest should not be explicitly provided and is set internally.
	///
	/// This does not announce the collation to the parachain network or the relay chain.
	pub async fn collate(
		&mut self,
		parent_header: &Block::Header,
		slot_claim: &SlotClaim<P::Public>,
		additional_pre_digest: impl Into<Option<Vec<DigestItem>>>,
		inherent_data: (ParachainInherentData, InherentData),
		proposal_duration: Duration,
		max_pov_size: usize,
		scheduling_proof: Option<cumulus_primitives_core::SchedulingProof>,
	) -> Result<Option<(Collation, ParachainBlockData<Block>)>, Box<dyn Error + Send + 'static>> {
		let maybe_candidate = self
			.build_block_and_import(BuildBlockAndImportParams {
				parent_header,
				slot_claim,
				additional_pre_digest: additional_pre_digest.into().unwrap_or_default(),
				parachain_inherent_data: inherent_data.0,
				extra_inherent_data: inherent_data.1,
				proposal_duration,
				max_pov_size,
				storage_proof_recorder: None,
				extra_extensions: Default::default(),
			})
			.await?;

		let Some(candidate) = maybe_candidate else { return Ok(None) };

		let hash = candidate.block.header().hash();
		if let Some((collation, block_data)) = self.collator_service.build_collation(
			parent_header,
			hash,
			candidate.into(),
			scheduling_proof,
		) {
			block_data.log_size_info();

			if let MaybeCompressedPoV::Compressed(ref pov) = collation.proof_of_validity {
				tracing::info!(
					target: crate::LOG_TARGET,
					"Compressed PoV size: {}kb",
					pov.block_data.0.len() as f64 / 1024f64,
				);
			}

			Ok(Some((collation, block_data)))
		} else {
			Err(Box::<dyn Error + Send + Sync>::from("Unable to produce collation"))
		}
	}

	/// Get the underlying collator service.
	pub fn collator_service(&self) -> &CS {
		&self.collator_service
	}
}

/// A claim on an Aura slot.
pub struct SlotClaim<Pub> {
	author_pub: Pub,
	pre_digest: DigestItem,
	slot: Slot,
	timestamp: Timestamp,
}

impl<Pub> SlotClaim<Pub> {
	/// Create a slot-claim from the given author public key, slot, and timestamp.
	///
	/// This does not check whether the author actually owns the slot or the timestamp
	/// falls within the slot.
	pub fn unchecked<P>(author_pub: Pub, slot: Slot, timestamp: Timestamp) -> Self
	where
		P: Pair<Public = Pub>,
		P::Public: Codec,
		P::Signature: Codec,
	{
		SlotClaim { author_pub, timestamp, pre_digest: aura_internal::pre_digest::<P>(slot), slot }
	}

	/// Get the author's public key.
	pub fn author_pub(&self) -> &Pub {
		&self.author_pub
	}

	/// Get the Aura pre-digest for this slot.
	pub fn pre_digest(&self) -> &DigestItem {
		&self.pre_digest
	}

	/// Get the slot assigned to this claim.
	pub fn slot(&self) -> Slot {
		self.slot
	}

	/// Get the timestamp corresponding to the relay-chain slot this claim was
	/// generated against.
	pub fn timestamp(&self) -> Timestamp {
		self.timestamp
	}
}

/// Attempt to claim a slot derived from the given relay-parent header's slot.
pub async fn claim_slot<B, C, P>(
	client: &C,
	parent_hash: B::Hash,
	relay_parent_header: &PHeader,
	slot_duration: SlotDuration,
	relay_chain_slot_duration: Duration,
	keystore: &KeystorePtr,
) -> Result<Option<SlotClaim<P::Public>>, Box<dyn Error>>
where
	B: BlockT,
	C: ProvideRuntimeApi<B> + Send + Sync + 'static,
	C::Api: AuraApi<B, P::Public>,
	P: Pair,
	P::Public: Codec,
	P::Signature: Codec,
{
	let mut runtime_api = client.runtime_api();
	runtime_api.set_call_context(sp_core::traits::CallContext::Onchain { import: false });
	let authorities = runtime_api.authorities(parent_hash).map_err(Box::new)?;

	// Determine the current slot and timestamp based on the relay-parent's.
	let (slot_now, timestamp) = match consensus_common::get_relay_slot_and_timestamp(
		relay_parent_header,
		relay_chain_slot_duration,
	) {
		Some((r_s, t)) => {
			let our_slot = Slot::from_timestamp(t, slot_duration);
			tracing::debug!(
				target: crate::LOG_TARGET,
				relay_slot = ?r_s,
				para_slot = ?our_slot,
				timestamp = ?t,
				?slot_duration,
				?relay_chain_slot_duration,
				"Adjusted relay-chain slot to parachain slot"
			);
			(our_slot, t)
		},
		None => return Ok(None),
	};

	// Try to claim the slot locally.
	let author_pub = {
		let res = aura_internal::claim_slot::<P>(slot_now, &authorities, keystore).await;
		match res {
			Some(p) => p,
			None => return Ok(None),
		}
	};

	Ok(Some(SlotClaim::unchecked::<P>(author_pub, slot_now, timestamp)))
}

/// Seal a block with a signature in the header.
pub fn seal<B: BlockT, P>(
	pre_sealed: B,
	storage_changes: StorageChanges<HashingFor<B>>,
	author_pub: &P::Public,
	keystore: &KeystorePtr,
) -> Result<BlockImportParams<B>, Box<dyn Error + Send + Sync + 'static>>
where
	P: Pair,
	P::Signature: Codec + TryFrom<Vec<u8>>,
	P::Public: AppPublic,
{
	let (pre_header, body) = pre_sealed.deconstruct();
	let pre_hash = pre_header.hash();
	let block_number = *pre_header.number();

	// seal the block.
	let block_import_params = {
		let seal_digest =
			aura_internal::seal::<_, P>(&pre_hash, &author_pub, keystore).map_err(Box::new)?;
		let mut block_import_params = BlockImportParams::new(BlockOrigin::Own, pre_header);
		block_import_params.post_digests.push(seal_digest);
		block_import_params.body = Some(body);
		block_import_params.state_action =
			StateAction::ApplyChanges(sc_consensus::StorageChanges::Changes(storage_changes));
		block_import_params.fork_choice = Some(ForkChoiceStrategy::LongestChain);
		block_import_params
	};
	let post_hash = block_import_params.post_hash();

	tracing::info!(
		target: crate::LOG_TARGET,
		"🔖 Pre-sealed block for proposal at {}. Hash now {:?}, previously {:?}.",
		block_number,
		post_hash,
		pre_hash,
	);

	Ok(block_import_params)
}
