// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

use anyhow::anyhow;
use codec::{Decode, Encode};
use cumulus_primitives_core::{BlockBundleInfo, CoreInfo, CumulusDigestItem, RelayBlockIdentifier};
use futures::stream::StreamExt;
use polkadot_primitives::{BlakeTwo256, CandidateReceiptV2, HashT, Id as ParaId};
use std::{cmp::max, collections::HashMap, ops::Range, sync::Arc};
use tokio::{
	join,
	time::{sleep, Duration},
};
use zombienet_sdk::{
	subxt::{
		self,
		blocks::Block,
		config::{polkadot::PolkadotExtrinsicParamsBuilder, substrate::DigestItem},
		dynamic::Value,
		events::Events,
		ext::scale_value::value,
		metadata::Metadata,
		tx::{signer::Signer, DynamicPayload, SubmittableTransaction, TxStatus},
		utils::H256,
		Config, OnlineClient, PolkadotConfig,
	},
	LocalFileSystem, Network,
};

/// Specifies which block should occupy a full core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockToCheck {
	/// The exact block hash provided should occupy a full core.
	Exact(H256),
	/// Wait for the next first bundle block.
	NextFirstBundleBlock(H256),
}

// Maximum number of blocks to wait for a session change.
// If it does not arrive for whatever reason, we should not wait forever.
const WAIT_MAX_BLOCKS_FOR_SESSION: u32 = 50;

// Maximum time to wait for PVF preparation to conclude on a validator before
// starting throughput measurement. PVF preparation is a one-off ~20s wasm
// compile per validator that contends for CPU. The clock starts at the session
// change, but the first parachain candidate (which triggers PVF preparation)
// can be delayed by several minutes of collator warm-up, which we must account for.
const PVF_PREPARE_TIMEOUT_SECS: u64 = 300;

/// Format a `sp_runtime::DispatchError` using runtime metadata for human-readable output.
///
/// For module errors this resolves the pallet index and error index to their names
/// (e.g. `ParachainSystem::TooBig`) instead of showing raw bytes.
fn format_dispatch_error(err: &sp_runtime::DispatchError, metadata: &Metadata) -> String {
	match err {
		sp_runtime::DispatchError::Module(module_err) => {
			let pallet = metadata.pallet_by_index(module_err.index);
			let pallet_name = pallet.as_ref().map(|p| p.name()).unwrap_or("UnknownPallet");
			let error_name = pallet
				.and_then(|p| p.error_variant_by_index(module_err.error[0]))
				.map(|v| v.name.as_str())
				.unwrap_or("UnknownError");
			format!("{pallet_name}::{error_name}")
		},
		other => format!("{other:?}"),
	}
}

/// Find an event in subxt `Events` and attempt to decode the fields of the event.
fn find_event_and_decode_fields<T: Decode>(
	events: &Events<PolkadotConfig>,
	pallet: &str,
	variant: &str,
) -> Result<Vec<T>, anyhow::Error> {
	let mut result = vec![];
	for event in events.iter() {
		let event = event?;
		if event.pallet_name() == pallet && event.variant_name() == variant {
			result.push(T::decode(&mut &event.field_bytes()[..])?);
		}
	}
	Ok(result)
}
/// Returns `true` if the `block` is a session change.
async fn is_session_change(
	block: &Block<PolkadotConfig, OnlineClient<PolkadotConfig>>,
) -> Result<bool, anyhow::Error> {
	let events = block.events().await?;
	Ok(events.iter().any(|event| {
		event.as_ref().is_ok_and(|event| {
			event.pallet_name() == "Session" && event.variant_name() == "NewSession"
		})
	}))
}

// Helper function for asserting the throughput of parachains, after the first session change.
//
// The throughput is measured as total number of backed candidates in a window of `stop_after` relay
// chain blocks. The counting window starts from the relay chain block after the first one that
// contains a backed candidate for a tracked para. Relay chain blocks with session changes are
// generally ignored, but it is ensured that no blocks are build on top of these relay blocks.
//
// For tests where PVF preparation timing affects throughput (e.g. elastic scaling, runtime
// upgrades), call [`wait_for_pvf_prepare`] before this helper to ensure all validators have
// finished preparing the relevant PVFs.
pub async fn assert_para_throughput(
	relay_client: &OnlineClient<PolkadotConfig>,
	stop_after: u32,
	expected_candidate_ranges: impl Into<HashMap<ParaId, Range<u32>>>,
	expected_number_of_blocks: impl Into<HashMap<ParaId, (OnlineClient<PolkadotConfig>, Range<u32>)>>,
) -> Result<(), anyhow::Error> {
	let ranges = expected_candidate_ranges.into();
	let expected_number_of_blocks = expected_number_of_blocks.into();

	let candidate_count =
		collect_para_throughput(relay_client, stop_after, ranges, |_| Ok(true)).await?;

	assert_expected_number_of_blocks(candidate_count, expected_number_of_blocks).await
}

/// Like [`assert_para_throughput`], but accepts a closure to validate each backed candidate
/// receipt.
///
/// The closure receives each [`CandidateReceiptV2`] and should return:
/// - `Ok(true)` to count the candidate,
/// - `Ok(false)` to skip it,
/// - `Err(e)` to fail immediately.
///
/// Only receipts for para IDs present in `expected_candidate_ranges` are passed to the closure.
pub async fn assert_para_throughput_with<F>(
	relay_client: &OnlineClient<PolkadotConfig>,
	stop_after: u32,
	expected_candidate_ranges: impl Into<HashMap<ParaId, Range<u32>>>,
	validate: F,
) -> Result<(), anyhow::Error>
where
	F: Fn(&CandidateReceiptV2<H256>) -> Result<bool, anyhow::Error>,
{
	collect_para_throughput(relay_client, stop_after, expected_candidate_ranges, validate)
		.await
		.map(|_| ())
}

/// Waits until every relaychain validator in `network` reports
/// `polkadot_pvf_prepare_concluded >= min_total_prepares`.
///
/// Use this before [`assert_para_throughput`] in tests where PVF preparation timing demonstrably
/// affects throughput — typically elastic-scaling tests (high core count, validators bound on
/// PVF-compile CPU) and tests that measure throughput across a parachain runtime upgrade (which
/// triggers re-preparation of the new PVF).
///
/// `min_total_prepares` is the absolute minimum value the metric must reach, in cumulative
/// concluded prepare jobs since validator startup. For one round of preparation (one PVF per
/// tracked parachain), pass `tracked_paras as u32`. For a measurement after a runtime upgrade,
/// pass `2 * tracked_paras as u32`. Caller is responsible for tracking the round — we
/// deliberately do not read a baseline from the metric and add a delta, because validators
/// may already have started or finished preparing a new PVF before the baseline read, which
/// makes the delta racy.
pub async fn wait_for_pvf_prepare(
	network: &Network<LocalFileSystem>,
	min_total_prepares: u32,
) -> Result<(), anyhow::Error> {
	let validators = network.relaychain().nodes();
	let target = min_total_prepares as f64;
	log::info!(
		"Waiting for PVF preparation to conclude on {} validator(s) (target {} concluded job(s) per validator).",
		validators.len(),
		target,
	);
	for node in &validators {
		let node_name = node.name();
		log::info!("Waiting for {node_name} PVF prep (target={target})...");
		node.wait_metric_with_timeout(
			"polkadot_pvf_prepare_concluded",
			|c| c >= target,
			PVF_PREPARE_TIMEOUT_SECS,
		)
		.await
		.map_err(|e| anyhow!("{node_name}: PVF prepare did not conclude within timeout: {e}"))?;
	}
	log::info!(
		"All {} validator(s) have prepared PVF artifacts (target {})",
		validators.len(),
		target,
	);
	Ok(())
}

async fn collect_para_throughput<F>(
	relay_client: &OnlineClient<PolkadotConfig>,
	stop_after: u32,
	expected_candidate_ranges: impl Into<HashMap<ParaId, Range<u32>>>,
	validate: F,
) -> Result<HashMap<ParaId, Vec<CandidateReceiptV2<H256>>>, anyhow::Error>
where
	F: Fn(&CandidateReceiptV2<H256>) -> Result<bool, anyhow::Error>,
{
	let mut blocks_sub = relay_client.blocks().subscribe_finalized().await?;
	let mut candidate_count: HashMap<ParaId, Vec<CandidateReceiptV2<H256>>> = HashMap::new();
	let mut current_block_count = 0;

	let expected_candidate_ranges = expected_candidate_ranges.into();
	let valid_para_ids: Vec<ParaId> = expected_candidate_ranges.keys().cloned().collect();

	log::info!(
		"Asserting parachain throughput for para_ids: {:?}. Wait for the first session change",
		valid_para_ids
	);
	// Wait for the first session, block production on the parachain will start after that.
	wait_for_first_session_change(&mut blocks_sub).await?;
	log::info!(
		"First session change detected. Waiting for backed candidates from all tracked paras before counting."
	);

	let mut paras_seen = std::collections::HashSet::new();
	while let Some(block) = blocks_sub.next().await {
		let block = block?;
		log::debug!("Finalized relay chain block {}", block.number());

		// Do not count blocks with session changes, no backed blocks there.
		if is_session_change(&block).await? {
			continue;
		}

		let events = block.events().await?;
		let receipts = find_event_and_decode_fields::<CandidateReceiptV2<H256>>(
			&events,
			"ParaInclusion",
			"CandidateBacked",
		)?;

		// Skip relay chain blocks until every tracked para has had at least one backed candidate.
		// This avoids counting the initial warm-up period where the backing pipeline (PVF
		// compilation, first collation) hasn't reached steady state yet.
		for receipt in &receipts {
			let para_id = receipt.descriptor.para_id();
			if valid_para_ids.contains(&para_id) {
				paras_seen.insert(para_id);
			}
		}
		if paras_seen.len() != valid_para_ids.len() {
			log::info!(
				"Not all tracked paras have produced candidates by relay block {}. \
				Not counting blocks yet.",
				block.number()
			);
			continue;
		}

		current_block_count += 1;

		for receipt in receipts {
			let para_id = receipt.descriptor.para_id();
			log::debug!("Block backed for para_id {para_id}");

			if !valid_para_ids.contains(&para_id) {
				continue;
			}

			if !validate(&receipt)? {
				continue;
			}

			candidate_count.entry(para_id).or_default().push(receipt);
		}

		if current_block_count == stop_after {
			break;
		}
	}

	log::info!(
		"Reached {stop_after} finalized relay chain blocks that contain backed candidates. The per-parachain distribution is: {:#?}",
		candidate_count.iter().map(|(para_id, receipts)| format!("{para_id} has {} backed candidates", receipts.len())).collect::<Vec<_>>()
	);

	for (para_id, expected_candidate_range) in expected_candidate_ranges {
		let actual = candidate_count
			.get(&para_id)
			.ok_or_else(|| anyhow!("ParaId {para_id} did not have any backed candidates"))?
			.len() as u32;

		if !expected_candidate_range.contains(&actual) {
			return Err(anyhow!(
				"ParaId {para_id}: candidate count {actual} not within expected range {expected_candidate_range:?}"
			));
		}
	}

	Ok(candidate_count)
}

async fn assert_expected_number_of_blocks(
	candidate_count: HashMap<ParaId, Vec<CandidateReceiptV2<H256>>>,
	expected_number_of_blocks: HashMap<ParaId, (OnlineClient<PolkadotConfig>, Range<u32>)>,
) -> Result<(), anyhow::Error> {
	for (para_id, (para_client, expected_number_of_blocks)) in expected_number_of_blocks {
		let receipts = candidate_count
			.get(&para_id)
			.ok_or_else(|| anyhow!("ParaId did not have any backed candidates"))?;

		let mut num_blocks = 0;

		for receipt in receipts {
			// We "abuse" the fact that the parachain is using `BlakeTwo256` as hash and thus, the
			// `para_head` hash and the hash of the `header` should be equal.
			let mut next_para_block_hash = receipt.descriptor().para_head();

			let mut relay_identifier = None;
			let mut core_info = None;

			loop {
				let block: Block<PolkadotConfig, OnlineClient<PolkadotConfig>> =
					para_client.blocks().at(next_para_block_hash).await?;

				// Genesis block is not part of a candidate :)
				if block.number() == 0 {
					break;
				}

				let ri = find_relay_block_identifier(&block)?;
				let ci = find_core_info(&block)?;

				// If the core changes or the relay identifier, we found all blocks for the
				// candidate.
				if *relay_identifier.get_or_insert(ri.clone()) != ri ||
					*core_info.get_or_insert(ci.clone()) != ci
				{
					break;
				}

				num_blocks += 1;
				next_para_block_hash = block.header().parent_hash;
			}
		}

		if !expected_number_of_blocks.contains(&num_blocks) {
			return Err(anyhow!(
				"Block number count {num_blocks} not within range {expected_number_of_blocks:?}",
			));
		}
	}

	Ok(())
}

/// Returns [`CoreInfo`] for the given parachain block.
pub fn find_core_info(
	block: &Block<PolkadotConfig, OnlineClient<PolkadotConfig>>,
) -> Result<CoreInfo, anyhow::Error> {
	let substrate_digest =
		sp_runtime::generic::Digest::decode(&mut &block.header().digest.encode()[..])
			.expect("`subxt::Digest` and `substrate::Digest` should encode and decode; qed");

	CumulusDigestItem::find_core_info(&substrate_digest)
		.ok_or_else(|| anyhow!("Failed to find `CoreInfo` digest"))
}

/// Returns [`RelayBlockIdentifier`] for the given parachain block.
fn find_relay_block_identifier(
	block: &Block<PolkadotConfig, OnlineClient<PolkadotConfig>>,
) -> Result<RelayBlockIdentifier, anyhow::Error> {
	let substrate_digest =
		sp_runtime::generic::Digest::decode(&mut &block.header().digest.encode()[..])
			.expect("`subxt::Digest` and `substrate::Digest` should encode and decode; qed");

	CumulusDigestItem::find_relay_block_identifier(&substrate_digest)
		.ok_or_else(|| anyhow!("Failed to find `RelayBlockIdentifier` digest"))
}

/// Wait for the first block with a session change.
///
/// The session change is detected by inspecting the events in the block.
pub async fn wait_for_first_session_change(
	blocks_sub: &mut zombienet_sdk::subxt::backend::StreamOfResults<
		Block<PolkadotConfig, OnlineClient<PolkadotConfig>>,
	>,
) -> Result<(), anyhow::Error> {
	wait_for_nth_session_change(blocks_sub, 1).await
}

/// Wait for the first block with the Nth session change.
///
/// The session change is detected by inspecting the events in the block.
pub async fn wait_for_nth_session_change(
	blocks_sub: &mut zombienet_sdk::subxt::backend::StreamOfResults<
		Block<PolkadotConfig, OnlineClient<PolkadotConfig>>,
	>,
	mut sessions_to_wait: u32,
) -> Result<(), anyhow::Error> {
	let mut waited_block_num = 0;
	while let Some(block) = blocks_sub.next().await {
		let block = block?;
		log::debug!("Finalized relay chain block {}", block.number());

		if is_session_change(&block).await? {
			sessions_to_wait -= 1;
			if sessions_to_wait == 0 {
				return Ok(());
			}

			waited_block_num = 0;
		} else {
			if waited_block_num >= WAIT_MAX_BLOCKS_FOR_SESSION {
				return Err(anyhow::format_err!("Waited for {WAIT_MAX_BLOCKS_FOR_SESSION}, a new session should have been arrived by now."));
			}

			waited_block_num += 1;
		}
	}
	Ok(())
}

// Helper function that asserts the maximum finality lag.
pub async fn assert_finality_lag(
	client: &OnlineClient<PolkadotConfig>,
	maximum_lag: u32,
) -> Result<(), anyhow::Error> {
	let mut best_stream = client.blocks().subscribe_best().await?;
	let mut fut_stream = client.blocks().subscribe_finalized().await?;
	let (Some(Ok(best)), Some(Ok(finalized))) = join!(best_stream.next(), fut_stream.next()) else {
		return Err(anyhow::format_err!("Unable to fetch best an finalized block!"));
	};
	let finality_lag = best.number() - finalized.number();

	log::info!(
		"Finality lagged by {finality_lag} blocks, maximum expected was {maximum_lag} blocks"
	);

	assert!(finality_lag <= maximum_lag, "Expected finality to lag by a maximum of {maximum_lag} blocks, but was lagging by {finality_lag} blocks.");
	Ok(())
}

/// Assert that finality has not stalled.
pub async fn assert_blocks_are_being_finalized(
	client: &OnlineClient<PolkadotConfig>,
) -> Result<(), anyhow::Error> {
	let sleep_duration = Duration::from_secs(12);
	let mut finalized_blocks = client.blocks().subscribe_finalized().await?;
	let first_measurement = finalized_blocks
		.next()
		.await
		.ok_or(anyhow::anyhow!("Can't get finalized block from stream"))??
		.number();
	sleep(sleep_duration).await;
	let second_measurement = finalized_blocks
		.next()
		.await
		.ok_or(anyhow::anyhow!("Can't get finalized block from stream"))??
		.number();

	log::info!(
		"Finalized {} blocks within {sleep_duration:?}",
		second_measurement - first_measurement
	);
	assert!(second_measurement > first_measurement);

	Ok(())
}

/// Checks if the given `RelayBlockIdentifier` matches a relay chain header.
fn identifier_matches_header(
	identifier: &RelayBlockIdentifier,
	header: &<PolkadotConfig as Config>::Header,
) -> bool {
	match identifier {
		RelayBlockIdentifier::ByHash(hash) => {
			let header_hash = BlakeTwo256::hash(&header.encode());
			header_hash == *hash
		},
		RelayBlockIdentifier::ByStorageRoot { storage_root, .. } => {
			header.state_root == *storage_root
		},
	}
}

/// Asserts that parachain blocks have the correct relay parent offset. This also checks that the
/// relay chain descendants do not contain any session changes.
///
/// # Arguments
///
/// * `relay_client` - Client connected to a relay chain node
/// * `para_client` - Client connected to a parachain node
/// * `offset` - Expected minimum offset between relay parent and highest seen relay block
/// * `block_limit` - Number of parachain blocks to verify before completing
pub async fn assert_relay_parent_offset(
	relay_client: &OnlineClient<PolkadotConfig>,
	para_client: &OnlineClient<PolkadotConfig>,
	offset: u32,
	block_limit: u32,
) -> Result<(), anyhow::Error> {
	let mut relay_block_stream = relay_client.blocks().subscribe_all().await?;

	// First parachain header #0 does not contain relay block identifier digest item.
	let mut para_block_stream = para_client.blocks().subscribe_all().await?.skip(1);
	let mut highest_relay_block_seen = 0;
	let mut num_para_blocks_seen = 0;
	let mut forbidden_parents = Vec::new();
	let mut seen_relay_parents = HashMap::new();
	loop {
		tokio::select! {
			Some(Ok(relay_block)) = relay_block_stream.next() => {
				highest_relay_block_seen = max(relay_block.number(), highest_relay_block_seen);
				if highest_relay_block_seen > 15 && num_para_blocks_seen == 0 {
					return Err(anyhow!("No parachain blocks produced!"))
				}
				// When a relay chain block contains a session change, parachains shall not build on
				// any ancestor of that block, if the session change block is part of the descendants.
				// Example:
				// RC Chain: A -> B -> C -> D*
				// "*" denotes session change
				// In this scenario, parachains with an offset of 2 should never build on relay chain
				// blocks B or C. Both of them would include the session change block D* in their
				// descendants, and we know that the candidate would span a session boundary.
				if is_session_change(&relay_block).await? {
					log::debug!("RC block #{} contains session change, adding {offset} parents to forbidden list.", relay_block.number());
					let mut current_hash = relay_block.header().parent_hash;
					for _ in 0..offset {
						let block = relay_client.blocks().at(current_hash).await.map_err(|_| anyhow!("Unable to fetch RC header."))?;
						forbidden_parents.push(block.header().clone());
						current_hash = block.header().parent_hash;
					}
				}
			},
			Some(Ok(para_block)) = para_block_stream.next() => {
				let relay_block_identifier = find_relay_block_identifier(&para_block)?;

				let relay_parent_number = match &relay_block_identifier {
					RelayBlockIdentifier::ByHash(block_hash) => relay_client.blocks().at(*block_hash).await?.number(),
					RelayBlockIdentifier::ByStorageRoot { block_number, .. } => *block_number,
				};

				let para_block_number = para_block.number();
				seen_relay_parents.insert(relay_block_identifier.clone(), para_block);
				log::debug!("Parachain block #{para_block_number} was built on relay parent #{relay_parent_number}, highest seen was {highest_relay_block_seen}");
				assert!(
					highest_relay_block_seen < offset ||
					relay_parent_number <= highest_relay_block_seen.saturating_sub(offset),
					"Relay parent is not at the correct offset! relay_parent: #{relay_parent_number} highest_seen_relay_block: #{highest_relay_block_seen}",
				);
				// As per explanation above, we need to check that no parachain blocks are built
				// on the forbidden parents.
				for forbidden in &forbidden_parents {
					for (identifier, para_block) in &seen_relay_parents {
						if identifier_matches_header(identifier, forbidden) {
							panic!(
								"Parachain block {} was built on forbidden relay parent with session change descendants ({:?})",
								para_block.hash(),
								identifier
							);
						}
					}
				}
				num_para_blocks_seen += 1;
				if num_para_blocks_seen >= block_limit {
					log::info!("Successfully verified relay parent offset of {offset} for {num_para_blocks_seen} parachain blocks.");
					break;
				}
			}
		}
	}

	Ok(())
}

/// Submits the given `call` as signed transaction and waits for its successful finalization.
///
/// The transaction is sent as immortal transaction.
pub async fn submit_extrinsic_and_wait_for_finalization_success<S: Signer<PolkadotConfig>>(
	client: &OnlineClient<PolkadotConfig>,
	call: &DynamicPayload,
	signer: &S,
) -> Result<H256, anyhow::Error> {
	let extensions = PolkadotExtrinsicParamsBuilder::new().immortal().build();

	log::info!("Submitting transaction...");

	let tx = client.tx().create_signed(call, signer, extensions).await?;

	submit_tx_and_wait_for_finalization(tx).await
}

/// Submits the given `call` as unsigned transaction and waits for it successful finalization.
pub async fn submit_unsigned_extrinsic_and_wait_for_finalization_success(
	client: &OnlineClient<PolkadotConfig>,
	call: &DynamicPayload,
) -> Result<H256, anyhow::Error> {
	let tx = client.tx().create_unsigned(call)?;

	submit_tx_and_wait_for_finalization(tx).await
}

/// Submit the given transaction and wait for its finalization.
async fn submit_tx_and_wait_for_finalization(
	tx: SubmittableTransaction<PolkadotConfig, OnlineClient<PolkadotConfig>>,
) -> Result<H256, anyhow::Error> {
	log::info!("Submitting transaction: {:?}", tx.hash());

	let mut tx = tx.submit_and_watch().await?;

	while let Some(status) = tx.next().await.transpose()? {
		match status {
			TxStatus::InBestBlock(tx_in_block) => {
				tx_in_block.wait_for_success().await?;
				log::info!("[Best] In block: {:#?}", tx_in_block.block_hash());
			},
			TxStatus::InFinalizedBlock(ref tx_in_block) => {
				tx_in_block.wait_for_success().await?;
				log::info!("[Finalized] In block: {:#?}", tx_in_block.block_hash());
				return Ok(tx_in_block.block_hash());
			},
			TxStatus::Error { message } |
			TxStatus::Invalid { message } |
			TxStatus::Dropped { message } => {
				return Err(anyhow!("Error submitting tx: {message}"));
			},
			_ => continue,
		}
	}

	Err(anyhow!("Transaction event stream ended without reaching the finalized state"))
}

/// Submits the given `call` as transaction and waits `timeout_secs` for it successful finalization.
///
/// If the transaction does not reach the finalized state in `timeout_secs` an error is returned.
/// The transaction is send as immortal transaction.
pub async fn submit_extrinsic_and_wait_for_finalization_success_with_timeout<
	S: Signer<PolkadotConfig>,
>(
	client: &OnlineClient<PolkadotConfig>,
	call: &DynamicPayload,
	signer: &S,
	timeout_secs: impl Into<u64>,
) -> Result<(), anyhow::Error> {
	let secs = timeout_secs.into();
	let res = tokio::time::timeout(
		Duration::from_secs(secs),
		submit_extrinsic_and_wait_for_finalization_success(client, call, signer),
	)
	.await;

	match res {
		Ok(Ok(_)) => Ok(()),
		Ok(Err(e)) => Err(anyhow!("Error waiting for metric: {}", e)),
		// timeout
		Err(_) => Err(anyhow!("Timeout ({secs}), waiting for extrinsic finalization")),
	}
}

/// Asserts that the given `para_id` is registered at the relay chain.
pub async fn assert_para_is_registered(
	relay_client: &OnlineClient<PolkadotConfig>,
	para_id: ParaId,
	blocks_to_wait: u32,
) -> Result<(), anyhow::Error> {
	let mut blocks_sub = relay_client.blocks().subscribe_all().await?;
	let para_id: u32 = para_id.into();

	let keys: Vec<Value> = vec![];
	let query = subxt::dynamic::storage("Paras", "Parachains", keys);

	let mut blocks_cnt = 0;
	while let Some(block) = blocks_sub.next().await {
		let block = block?;
		log::debug!("Relay block #{}, checking if para_id {para_id} is registered", block.number(),);
		let parachains = block.storage().fetch(&query).await?;

		let parachains: Vec<u32> = match parachains {
			Some(parachains) => parachains.as_type()?,
			None => vec![],
		};

		log::debug!("Registered para_ids: {:?}", parachains);

		if parachains.iter().any(|p| para_id.eq(p)) {
			log::debug!("para_id {para_id} registered");
			return Ok(());
		}
		if blocks_cnt >= blocks_to_wait {
			return Err(anyhow!(
				"Parachain {para_id} not registered within {blocks_to_wait} blocks"
			));
		}
		blocks_cnt += 1;
	}

	Err(anyhow!("No more blocks to check"))
}

/// Returns [`BlockBundleInfo`] for the given parachain block.
fn find_block_bundle_info(
	block: &Block<PolkadotConfig, OnlineClient<PolkadotConfig>>,
) -> Result<BlockBundleInfo, anyhow::Error> {
	let substrate_digest =
		sp_runtime::generic::Digest::decode(&mut &block.header().digest.encode()[..])
			.expect("`subxt::Digest` and `substrate::Digest` should encode and decode; qed");

	CumulusDigestItem::find_block_bundle_info(&substrate_digest)
		.ok_or_else(|| anyhow!("Failed to find `BlockBundleInfo` digest"))
}

/// Validates that the given block is a "special" block in the core.
///
/// If `is_only_block_in_core` is true, it checks if the given block is the first block in the core
/// and the only one. If this is `false`, it only checks if the block is the last block in the core.
async fn ensure_is_block_in_core_impl(
	para_client: &OnlineClient<PolkadotConfig>,
	block_hash: H256,
	is_only_block_in_core: bool,
) -> Result<(), anyhow::Error> {
	let blocks = para_client.blocks();
	let block = blocks.at(block_hash).await?;
	let block_core_info = find_core_info(&block)?;

	if is_only_block_in_core {
		let parent = blocks.at(block.header().parent_hash).await?;

		// Genesis is for sure on a different core :)
		if parent.number() != 0 {
			let parent_core_info = find_core_info(&parent)?;

			if parent_core_info == block_core_info {
				return Err(anyhow::anyhow!(
					"Not first block ({}) in core, at least the parent block is on the same core.",
					block.header().number
				));
			}
		}
	}

	let next_block = loop {
		// Start with the latest best block.
		let mut current_block = Arc::new(blocks.subscribe_best().await?.next().await.unwrap()?);

		let mut next_block = None;

		while current_block.hash() != block_hash {
			next_block = Some(current_block.clone());
			current_block = Arc::new(blocks.at(current_block.header().parent_hash).await?);

			if current_block.number() == 0 {
				return Err(anyhow::anyhow!(
					"Did not found block while going backwards from the best block"
				));
			}
		}

		// It possible that the first block we got is the same as the transaction got finalized.
		// So, we just retry again until we found some more blocks.
		if let Some(next_block) = next_block {
			break next_block;
		}
	};

	let next_block_core_info = find_core_info(&next_block)?;

	if next_block_core_info == block_core_info {
		return Err(anyhow::anyhow!(
			"Not {} block ({}) in core, at least the following block is on the same core.",
			if is_only_block_in_core { "first" } else { "last" },
			block.header().number
		));
	}

	Ok(())
}

/// Checks if the specified block occupies a full core.
pub async fn ensure_is_only_block_in_core(
	para_client: &OnlineClient<PolkadotConfig>,
	block_to_check: BlockToCheck,
) -> Result<(), anyhow::Error> {
	let blocks = para_client.blocks();

	match block_to_check {
		BlockToCheck::Exact(block_hash) => {
			ensure_is_block_in_core_impl(para_client, block_hash, true).await
		},
		BlockToCheck::NextFirstBundleBlock(start_block_hash) => {
			let start_block = blocks.at(start_block_hash).await?;

			let mut best_block_stream = blocks.subscribe_best().await?;

			let mut next_first_bundle_block = None;
			while let Some(mut block) = best_block_stream.next().await.transpose()? {
				while block.number() > start_block.number() {
					if find_block_bundle_info(&block)?.index == 0 {
						next_first_bundle_block = Some(block.hash());
					}

					block = blocks.at(block.header().parent_hash).await?;
				}

				if next_first_bundle_block.is_some() {
					break;
				}
			}

			if let Some(block) = next_first_bundle_block {
				ensure_is_block_in_core_impl(para_client, block, true).await
			} else {
				Err(anyhow!("Could not find the next bundle after {}", start_block.number()))
			}
		},
	}
}

/// Checks if the specified block is the last block in a core.
///
/// Also ensures that the last block is NOT the first block.
pub async fn ensure_is_last_block_in_core(
	para_client: &OnlineClient<PolkadotConfig>,
	block_to_check: H256,
) -> Result<(), anyhow::Error> {
	ensure_is_block_in_core_impl(para_client, block_to_check, false).await?;

	let blocks = para_client.blocks();
	let block = blocks.at(block_to_check).await?;
	let bundle_info = find_block_bundle_info(&block)?;

	// Above we ensure it is the last block in the core and now we want to ensure it isn't the first
	// block.
	if bundle_info.index == 0 {
		Err(anyhow!("`{block_to_check:?}` is the first block of a core and not the last"))
	} else {
		Ok(())
	}
}

/// Assigns the given `cores` to the given `para_id`.
///
/// Zombienet by default adds extra core for each registered parachain additionally to the one
/// requested by `num_cores`. It then assigns the parachains to the extra cores allocated at the
/// end. So, the passed core indices should be counted from zero.
///
/// # Example
///
/// Genesis patch:
/// ```json
/// "configuration": {
///   "config": {
///     "scheduler_params": {
///       "num_cores": 2,
///     }
///   }
/// }
/// ```
///
/// Runs the relay chain with `2` cores and we also add two parachains.
/// To assign these extra `2` cores, the call would look like this:
///
/// ```ignore
/// assign_cores(&relay_node, PARA_ID, vec![0, 1])
/// ```
///
/// The cores `2` and `3` are assigned to the parachains by Zombienet.
pub async fn assign_cores(
	client: &OnlineClient<PolkadotConfig>,
	para_id: u32,
	cores: Vec<u32>,
) -> Result<(), anyhow::Error> {
	log::info!("Assigning {:?} cores to parachain {}", cores, para_id);

	let assign_cores_call =
		create_assign_core_call(&cores.into_iter().map(|core| (core, para_id)).collect::<Vec<_>>());

	let res = submit_extrinsic_and_wait_for_finalization_success_with_timeout(
		client,
		&assign_cores_call,
		&zombienet_sdk::subxt_signer::sr25519::dev::alice(),
		60u64,
	)
	.await;
	assert!(res.is_ok(), "Extrinsic failed to finalize: {:?}", res.unwrap_err());
	log::info!("Cores assigned to the parachain");

	Ok(())
}

fn create_assign_core_call(core_and_para: &[(u32, u32)]) -> DynamicPayload {
	let mut assign_cores = vec![];
	for (core, para_id) in core_and_para.iter() {
		assign_cores.push(value! {
			Coretime(assign_core { core : *core, begin: 0, assignment: ((Task(*para_id), 57600)), end_hint: None() })
		});
	}

	zombienet_sdk::subxt::tx::dynamic(
		"Sudo",
		"sudo",
		vec![value! {
			Utility(batch { calls: assign_cores })
		}],
	)
}

/// Creates a runtime upgrade call using `Sudo::sudo(System::set_code_without_checks)`.
///
/// The `wasm_binary` should be the WASM runtime binary to upgrade to.
pub fn create_runtime_upgrade_call(wasm: &[u8]) -> DynamicPayload {
	zombienet_sdk::subxt::tx::dynamic(
		"Sudo",
		"sudo_unchecked_weight",
		vec![
			value! {
				System(set_code { code: Value::from_bytes(wasm) })
			},
			value! {
				{
					ref_time: 1u64,
					proof_size: 1u64
				}
			},
		],
	)
}

/// Submit a runtime upgrade via sudo and verify it was scheduled.
///
/// This submits a `Sudo::sudo_unchecked_weight(System::set_code(wasm))` extrinsic,
/// waits for finalization, then checks the `Sudid` event to verify the inner dispatch
/// succeeded. Returns the hash of the finalized block containing the upgrade extrinsic.
pub async fn submit_sudo_runtime_upgrade<S: Signer<PolkadotConfig>>(
	client: &OnlineClient<PolkadotConfig>,
	wasm: &[u8],
	signer: &S,
) -> Result<H256, anyhow::Error> {
	log::info!("Submitting sudo runtime upgrade, wasm size: {} bytes", wasm.len());
	let call = create_runtime_upgrade_call(wasm);
	let block_hash =
		submit_extrinsic_and_wait_for_finalization_success(client, &call, signer).await?;

	// Verify the inner sudo dispatch succeeded by checking the Sudid event.
	// sudo_unchecked_weight always returns Ok at the extrinsic level, even if the
	// inner call fails — the actual result is only in the Sudid event.
	let block = client.blocks().at(block_hash).await?;
	let events = block.events().await?;
	let sudid_results: Vec<Result<(), sp_runtime::DispatchError>> =
		find_event_and_decode_fields(&events, "Sudo", "Sudid")?;

	match sudid_results.first() {
		Some(Ok(())) => {
			log::info!("Sudo runtime upgrade dispatched successfully in block {block_hash:?}")
		},
		Some(Err(e)) => {
			return Err(anyhow!(
				"Sudo runtime upgrade inner dispatch failed in block {block_hash:?}: {}",
				format_dispatch_error(e, &client.metadata()),
			))
		},
		None => return Err(anyhow!("Sudid event not found in block {block_hash:?}")),
	}

	Ok(block_hash)
}

/// Wait until a runtime upgrade has happened.
///
/// This checks all finalized blocks until it finds a block that sets the
/// `RuntimeEnvironmentUpdated` digest.
///
/// Returns the hash of the block at which the runtime upgrade was applied.
pub async fn wait_for_runtime_upgrade(
	client: &OnlineClient<PolkadotConfig>,
) -> Result<H256, anyhow::Error> {
	let mut finalized_blocks = client.blocks().subscribe_finalized().await?;

	while let Some(Ok(block)) = finalized_blocks.next().await {
		if block
			.header()
			.digest
			.logs
			.iter()
			.any(|d| matches!(d, DigestItem::RuntimeEnvironmentUpdated))
		{
			log::info!("Runtime upgraded in block {:?}", block.hash());

			return Ok(block.hash());
		}
	}

	Err(anyhow!("Did not find a runtime upgrade"))
}

/// Poll a node's WebSocket endpoint until its subxt metadata reports the given pallet,
/// returning a fresh `OnlineClient` against that metadata, or fail on timeout.
///
/// After a runtime upgrade that introduces a new pallet, subxt's cached metadata can lag
/// the on-chain state until a new client is constructed against a block executed under the
/// upgraded runtime.
pub async fn wait_for_pallet_in_metadata(
	ws_url: &str,
	pallet_name: &str,
	timeout: Duration,
	poll_interval: Duration,
) -> Result<OnlineClient<PolkadotConfig>, anyhow::Error> {
	let deadline = std::time::Instant::now() + timeout;
	loop {
		if std::time::Instant::now() >= deadline {
			return Err(anyhow!(
				"metadata at {ws_url} never reflected pallet `{pallet_name}` within {timeout:?}",
			));
		}
		sleep(poll_interval).await;
		let candidate = OnlineClient::<PolkadotConfig>::from_url(ws_url).await?;
		if candidate.metadata().pallet_by_name(pallet_name).is_some() {
			return Ok(candidate);
		}
		log::debug!("`{pallet_name}` not in metadata yet, retrying");
	}
}
