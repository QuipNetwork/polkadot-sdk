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

/// An import queue which provides some equivocation resistance with lenient trait bounds.
///
/// Equivocation resistance in general is a hard problem, as different nodes in the network
/// may see equivocations in a different order, and therefore may not agree on which blocks
/// should be thrown out and which ones should be kept.
use codec::Codec;
use cumulus_client_consensus_common::ParachainBlockImportMarker;
use cumulus_primitives_core::{CumulusDigestItem, RelayBlockIdentifier};
use parking_lot::Mutex;
use polkadot_primitives::Hash as RHash;
use sc_consensus::{
	import_queue::{BasicQueue, Verifier as VerifierT},
	BlockImport, BlockImportParams, ForkChoiceStrategy,
};
use sc_consensus_aura::{standalone as aura_internal, AuthoritiesTracker};
use sc_telemetry::{telemetry, TelemetryHandle, CONSENSUS_DEBUG, CONSENSUS_TRACE};
use schnellru::{ByLength, LruMap};
use sp_api::{ApiExt, ProvideRuntimeApi};
use sp_block_builder::BlockBuilder as BlockBuilderApi;
use sp_blockchain::{HeaderBackend, HeaderMetadata};
use sp_consensus::{error::Error as ConsensusError, BlockOrigin};
use sp_consensus_aura::{AuraApi, Slot, SlotDuration};
use sp_core::crypto::Pair;
use sp_inherents::CreateInherentDataProviders;
use sp_runtime::traits::{Block as BlockT, Header as HeaderT, NumberFor};
use std::{fmt::Debug, sync::Arc};

const LRU_WINDOW: u32 = 512;
const EQUIVOCATION_LIMIT: usize = 16;

struct NaiveEquivocationDefender<N> {
	/// We distinguish blocks by `(Slot, BlockNumber, RelayParent)`.
	cache: LruMap<(u64, N, RHash), usize>,
}

impl<N: std::hash::Hash + PartialEq> Default for NaiveEquivocationDefender<N> {
	fn default() -> Self {
		NaiveEquivocationDefender { cache: LruMap::new(ByLength::new(LRU_WINDOW)) }
	}
}

impl<N: std::hash::Hash + PartialEq> NaiveEquivocationDefender<N> {
	// Returns `true` if equivocation is beyond the limit.
	fn insert_and_check(&mut self, slot: Slot, block_number: N, relay_chain_parent: RHash) -> bool {
		let val = self
			.cache
			.get_or_insert((*slot, block_number, relay_chain_parent), || 0)
			.expect("insertion with ByLength limiter always succeeds; qed");

		if *val == EQUIVOCATION_LIMIT {
			true
		} else {
			*val += 1;
			false
		}
	}
}

/// A parachain block import verifier that checks for equivocation limits within each slot.
pub struct Verifier<P: Pair, Client, Block: BlockT, CIDP> {
	client: Arc<Client>,
	create_inherent_data_providers: CIDP,
	defender: Mutex<NaiveEquivocationDefender<NumberFor<Block>>>,
	telemetry: Option<TelemetryHandle>,
	// Unused for now. Will be plugged in with a later PR.
	_authorities_tracker: AuthoritiesTracker<P, Block, Client>,
}

impl<P, Client, Block, CIDP> Verifier<P, Client, Block, CIDP>
where
	P: Pair,
	P::Signature: Codec,
	P::Public: Codec + Debug,
	Block: BlockT,
	Client: ProvideRuntimeApi<Block> + Send + Sync,
	<Client as ProvideRuntimeApi<Block>>::Api: BlockBuilderApi<Block> + AuraApi<Block, P::Public>,

	CIDP: CreateInherentDataProviders<Block, ()>,
{
	/// Creates a new Verifier instance for handling parachain block import verification in Aura
	/// consensus.
	pub fn new(
		client: Arc<Client>,
		inherent_data_provider: CIDP,
		telemetry: Option<TelemetryHandle>,
	) -> Self {
		Self {
			client: client.clone(),
			create_inherent_data_providers: inherent_data_provider,
			defender: Mutex::new(NaiveEquivocationDefender::default()),
			telemetry,
			_authorities_tracker: AuthoritiesTracker::new(client),
		}
	}
}

#[async_trait::async_trait]
impl<P, Client, Block, CIDP> VerifierT<Block> for Verifier<P, Client, Block, CIDP>
where
	P: Pair,
	P::Signature: Codec,
	P::Public: Codec + Debug,
	Block: BlockT,
	Client: HeaderBackend<Block>
		+ HeaderMetadata<Block, Error = sp_blockchain::Error>
		+ ProvideRuntimeApi<Block>
		+ Send
		+ Sync,
	<Client as ProvideRuntimeApi<Block>>::Api: BlockBuilderApi<Block> + AuraApi<Block, P::Public>,

	CIDP: CreateInherentDataProviders<Block, ()>,
{
	async fn verify(
		&self,
		mut block_params: BlockImportParams<Block>,
	) -> Result<BlockImportParams<Block>, String> {
		// Skip checks that include execution, if being told so, or when importing only state.
		//
		// This is done for example when gap syncing and it is expected that the block after the gap
		// was checked/chosen properly, e.g. by warp syncing to this block using a finality proof.
		if block_params.state_action.skip_execution_checks() || block_params.with_state() {
			block_params.fork_choice = Some(ForkChoiceStrategy::Custom(block_params.with_state()));
			return Ok(block_params);
		}

		let post_hash = block_params.header.hash();
		let parent_hash = *block_params.header.parent_hash();

		// check seal and update pre-hash/post-hash
		{
			let authorities = aura_internal::fetch_authorities(self.client.as_ref(), parent_hash)
				.map_err(|e| {
				format!("Could not fetch authorities at {:?}: {}", parent_hash, e)
			})?;

			let mut runtime_api = self.client.runtime_api();
			runtime_api.set_call_context(sp_core::traits::CallContext::Onchain { import: true });
			let slot_duration =
				runtime_api.slot_duration(parent_hash).map_err(|e| e.to_string())?;

			let slot_now = slot_now(slot_duration);
			let res = aura_internal::check_header_slot_and_seal::<Block, P>(
				slot_now,
				block_params.header,
				&authorities,
			);

			match res {
				Ok((pre_header, slot, seal_digest)) => {
					telemetry!(
						self.telemetry;
						CONSENSUS_TRACE;
						"aura.checked_and_importing";
						"pre_header" => ?pre_header,
					);

					// We need some kind of identifier for the relay parent, in the worst case we
					// take the all `0` hash.
					let relay_parent =
						match CumulusDigestItem::find_relay_block_identifier(pre_header.digest()) {
							None => Default::default(),
							Some(RelayBlockIdentifier::ByHash(h)) |
							Some(RelayBlockIdentifier::ByStorageRoot {
								storage_root: h, ..
							}) => h,
						};

					block_params.header = pre_header;
					block_params.post_digests.push(seal_digest);
					block_params.fork_choice = Some(ForkChoiceStrategy::LongestChain);
					block_params.post_hash = Some(post_hash);

					// Check for and reject egregious amounts of equivocations.
					//
					// If the `origin` is `ConsensusBroadcast`, we ignore the result of the
					// equivocation check. This `origin` is for example used by pov-recovery.
					if self.defender.lock().insert_and_check(
						slot,
						*block_params.header.number(),
						relay_parent,
					) && !matches!(block_params.origin, BlockOrigin::ConsensusBroadcast)
					{
						return Err(format!(
							"Rejecting block {:?} due to excessive equivocations at slot",
							post_hash,
						));
					}
				},
				Err(aura_internal::SealVerificationError::Deferred(hdr, slot)) => {
					telemetry!(
						self.telemetry;
						CONSENSUS_DEBUG;
						"aura.header_too_far_in_future";
						"hash" => ?post_hash,
						"a" => ?hdr,
						"b" => ?slot,
					);

					return Err(format!(
						"Rejecting block ({:?}) from future slot {:?}",
						post_hash, slot
					));
				},
				Err(e) => {
					return Err(format!(
						"Rejecting block ({:?}) with invalid seal ({:?})",
						post_hash, e
					))
				},
			}
		}

		// Check inherents.
		if let Some(body) = block_params.body.clone() {
			let block = Block::new(block_params.header.clone(), body);
			let create_inherent_data_providers = self
				.create_inherent_data_providers
				.create_inherent_data_providers(parent_hash, ())
				.await
				.map_err(|e| format!("Could not create inherent data {:?}", e))?;

			sp_block_builder::check_inherents(
				self.client.clone(),
				parent_hash,
				block,
				&create_inherent_data_providers,
			)
			.await
			.map_err(|e| format!("Error checking block inherents {:?}", e))?;
		}

		Ok(block_params)
	}
}

fn slot_now(slot_duration: SlotDuration) -> Slot {
	let timestamp = sp_timestamp::InherentDataProvider::from_system_time().timestamp();
	Slot::from_timestamp(timestamp, slot_duration)
}

/// Start an import queue for a Cumulus node which checks blocks' seals and inherent data.
///
/// Pass in only inherent data providers which don't include aura or parachain consensus inherents,
/// e.g. things like timestamp and custom inherents for the runtime.
///
/// The others are generated explicitly internally.
///
/// This should only be used for runtimes where the runtime does not check all inherents and
/// seals in `execute_block` (see <https://github.com/paritytech/cumulus/issues/2436>)
pub fn fully_verifying_import_queue<P, Client, Block: BlockT, I, CIDP>(
	client: Arc<Client>,
	block_import: I,
	create_inherent_data_providers: CIDP,
	spawner: &impl sp_core::traits::SpawnEssentialNamed,
	registry: Option<&prometheus_endpoint::Registry>,
	telemetry: Option<TelemetryHandle>,
) -> BasicQueue<Block>
where
	P: Pair + 'static,
	P::Signature: Codec,
	P::Public: Codec + Debug,
	I: BlockImport<Block, Error = ConsensusError>
		+ ParachainBlockImportMarker
		+ Send
		+ Sync
		+ 'static,
	Client: HeaderBackend<Block>
		+ HeaderMetadata<Block, Error = sp_blockchain::Error>
		+ ProvideRuntimeApi<Block>
		+ Send
		+ Sync
		+ 'static,
	<Client as ProvideRuntimeApi<Block>>::Api: BlockBuilderApi<Block> + AuraApi<Block, P::Public>,
	CIDP: CreateInherentDataProviders<Block, ()> + 'static,
{
	let verifier = Verifier::<P, _, _, _> {
		client: client.clone(),
		create_inherent_data_providers,
		defender: Mutex::new(NaiveEquivocationDefender::default()),
		telemetry,
		_authorities_tracker: AuthoritiesTracker::new(client.clone()),
	};

	BasicQueue::new(verifier, Box::new(block_import), None, spawner, registry)
}

#[cfg(test)]
mod test {
	use super::*;
	use codec::Encode;
	use cumulus_test_client::{
		runtime::Block, seal_block, BuildBlockBuilder, Client, TestClientBuilder,
		TestClientBuilderExt,
	};
	use cumulus_test_relay_sproof_builder::RelayStateSproofBuilder;
	use futures::FutureExt;
	use polkadot_primitives::{HeadData, PersistedValidationData};
	use sc_client_api::HeaderBackend;
	use sp_consensus_aura::sr25519;
	use sp_tracing::try_init_simple;
	use std::{collections::HashSet, sync::Arc};

	#[test]
	fn import_equivocated_blocks_from_recovery() {
		try_init_simple();

		let client = Arc::new(TestClientBuilder::default().build());

		let verifier = Verifier::<sr25519::AuthorityPair, Client, Block, _> {
			client: client.clone(),
			create_inherent_data_providers: |_, _| async move {
				Ok(sp_timestamp::InherentDataProvider::from_system_time())
			},
			defender: Mutex::new(NaiveEquivocationDefender::default()),
			telemetry: None,
			_authorities_tracker: AuthoritiesTracker::new(client.clone()),
		};

		let genesis = client.info().best_hash;
		let mut sproof = RelayStateSproofBuilder::default();
		sproof.included_para_head = Some(HeadData(client.header(genesis).unwrap().encode()));
		sproof.para_id = cumulus_test_client::runtime::PARACHAIN_ID.into();

		let validation_data = PersistedValidationData {
			relay_parent_number: 1,
			parent_head: client.header(genesis).unwrap().encode().into(),
			..Default::default()
		};

		let block_builder = client
			.init_block_builder_builder()
			.with_validation_data(validation_data)
			.with_relay_sproof_builder(sproof)
			.build();
		let block = block_builder.block_builder.build().unwrap();

		let mut blocks = Vec::new();
		for _ in 0..EQUIVOCATION_LIMIT + 1 {
			blocks.push(seal_block(block.block.clone(), &client))
		}

		// sr25519 should generate a different signature every time you sign something and thus, all
		// blocks get a different hash (even if they are the same block).
		assert_eq!(blocks.iter().map(|b| b.hash()).collect::<HashSet<_>>().len(), blocks.len());

		blocks.iter().take(EQUIVOCATION_LIMIT).for_each(|block| {
			let mut params =
				BlockImportParams::new(BlockOrigin::NetworkBroadcast, block.header().clone());
			params.body = Some(block.extrinsics().to_vec());
			verifier.verify(params).now_or_never().unwrap().unwrap();
		});

		// Now let's try some previously verified block and a block we have not verified yet.
		//
		// Verify should fail, because we are above the limit. However, when we change the origin to
		// `ConsensusBroadcast`, it should work.
		let extra_blocks =
			vec![blocks[EQUIVOCATION_LIMIT / 2].clone(), blocks.last().unwrap().clone()];

		extra_blocks.into_iter().for_each(|block| {
			let mut params =
				BlockImportParams::new(BlockOrigin::NetworkBroadcast, block.header().clone());
			params.body = Some(block.extrinsics().to_vec());
			assert!(verifier
				.verify(params)
				.now_or_never()
				.unwrap()
				.map(drop)
				.unwrap_err()
				.contains("excessive equivocations at slot"));

			// When it comes from `pov-recovery`, we will accept it
			let mut params =
				BlockImportParams::new(BlockOrigin::ConsensusBroadcast, block.header().clone());
			params.body = Some(block.extrinsics().to_vec());
			assert!(verifier.verify(params).now_or_never().unwrap().is_ok());
		});
	}
}
