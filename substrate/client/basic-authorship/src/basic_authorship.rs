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

//! A consensus proposer for "basic" chains which use the primitive inherent-data.

// FIXME #1021 move this into sp-consensus

use codec::Encode;
use futures::{
	channel::oneshot,
	future,
	future::{Future, FutureExt},
};
use log::{debug, error, info, log_enabled, trace, warn, Level};
use prometheus_endpoint::Registry as PrometheusRegistry;
use sc_block_builder::{BlockBuilderApi, BlockBuilderBuilder};
use sc_proposer_metrics::{EndProposingReason, MetricsLink as PrometheusMetrics};
use sc_telemetry::{telemetry, TelemetryHandle, CONSENSUS_INFO};
use sc_transaction_pool_api::{InPoolTransaction, TransactionPool, TxInvalidityReportMap};
use sp_api::{ApiExt, CallApiAt, ProvideRuntimeApi};
use sp_blockchain::{ApplyExtrinsicFailed::Validity, Error::ApplyExtrinsicFailed, HeaderBackend};
use sp_consensus::{Proposal, ProposeArgs};
use sp_core::traits::SpawnNamed;
use sp_inherents::InherentData;
use sp_runtime::{
	traits::{BlakeTwo256, Block as BlockT, Hash as HashT, Header as HeaderT},
	ExtrinsicInclusionMode, Percent, SaturatedConversion,
};
use std::{pin::Pin, sync::Arc, time};

/// Default block size limit in bytes used by [`Proposer`].
///
/// Can be overwritten by [`ProposerFactory::set_default_block_size_limit`].
///
/// Be aware that there is also an upper packet size on what the networking code
/// will accept. If the block doesn't fit in such a package, it can not be
/// transferred to other nodes.
pub const DEFAULT_BLOCK_SIZE_LIMIT: usize = 4 * 1024 * 1024 + 512;

const DEFAULT_SOFT_DEADLINE_PERCENT: Percent = Percent::from_percent(50);

const LOG_TARGET: &'static str = "basic-authorship";

/// [`Proposer`] factory.
pub struct ProposerFactory<A, C> {
	spawn_handle: Box<dyn SpawnNamed>,
	/// The client instance.
	client: Arc<C>,
	/// The transaction pool.
	transaction_pool: Arc<A>,
	/// Prometheus Link,
	metrics: PrometheusMetrics,
	/// The default block size limit.
	///
	/// If no `block_size_limit` is passed to [`sp_consensus::Proposer::propose`], this block size
	/// limit will be used.
	default_block_size_limit: usize,
	/// Soft deadline percentage of hard deadline.
	///
	/// The value is used to compute soft deadline during block production.
	/// The soft deadline indicates where we should stop attempting to add transactions
	/// to the block, which exhaust resources. After soft deadline is reached,
	/// we switch to a fixed-amount mode, in which after we see `MAX_SKIPPED_TRANSACTIONS`
	/// transactions which exhaust resources, we will conclude that the block is full.
	soft_deadline_percent: Percent,
	telemetry: Option<TelemetryHandle>,
}

impl<A, C> Clone for ProposerFactory<A, C> {
	fn clone(&self) -> Self {
		Self {
			spawn_handle: self.spawn_handle.clone(),
			client: self.client.clone(),
			transaction_pool: self.transaction_pool.clone(),
			metrics: self.metrics.clone(),
			default_block_size_limit: self.default_block_size_limit,
			soft_deadline_percent: self.soft_deadline_percent,
			telemetry: self.telemetry.clone(),
		}
	}
}

impl<A, C> ProposerFactory<A, C> {
	/// Create a new proposer factory.
	pub fn new(
		spawn_handle: impl SpawnNamed + 'static,
		client: Arc<C>,
		transaction_pool: Arc<A>,
		prometheus: Option<&PrometheusRegistry>,
		telemetry: Option<TelemetryHandle>,
	) -> Self {
		ProposerFactory {
			spawn_handle: Box::new(spawn_handle),
			transaction_pool,
			metrics: PrometheusMetrics::new(prometheus),
			default_block_size_limit: DEFAULT_BLOCK_SIZE_LIMIT,
			soft_deadline_percent: DEFAULT_SOFT_DEADLINE_PERCENT,
			telemetry,
			client,
		}
	}

	/// Deprecated, use [`Self::new`] instead.
	#[deprecated(note = "Proof recording is now handled differently. Use `new` instead.")]
	pub fn with_proof_recording(
		spawn_handle: impl SpawnNamed + 'static,
		client: Arc<C>,
		transaction_pool: Arc<A>,
		prometheus: Option<&PrometheusRegistry>,
		telemetry: Option<TelemetryHandle>,
	) -> Self {
		Self::new(spawn_handle, client, transaction_pool, prometheus, telemetry)
	}

	/// Set the default block size limit in bytes.
	///
	/// The default value for the block size limit is:
	/// [`DEFAULT_BLOCK_SIZE_LIMIT`].
	///
	/// If there is no block size limit passed to [`sp_consensus::Proposer::propose`], this value
	/// will be used.
	pub fn set_default_block_size_limit(&mut self, limit: usize) {
		self.default_block_size_limit = limit;
	}

	/// Set soft deadline percentage.
	///
	/// The value is used to compute soft deadline during block production.
	/// The soft deadline indicates where we should stop attempting to add transactions
	/// to the block, which exhaust resources. After soft deadline is reached,
	/// we switch to a fixed-amount mode, in which after we see `MAX_SKIPPED_TRANSACTIONS`
	/// transactions which exhaust resources, we will conclude that the block is full.
	///
	/// Setting the value too low will significantly limit the amount of transactions
	/// we try in case they exhaust resources. Setting the value too high can
	/// potentially open a DoS vector, where many "exhaust resources" transactions
	/// are being tried with no success, hence block producer ends up creating an empty block.
	pub fn set_soft_deadline(&mut self, percent: Percent) {
		self.soft_deadline_percent = percent;
	}
}

impl<Block, C, A> ProposerFactory<A, C>
where
	A: TransactionPool<Block = Block> + 'static,
	Block: BlockT,
	C: HeaderBackend<Block> + ProvideRuntimeApi<Block> + Send + Sync + 'static,
	C::Api: ApiExt<Block> + BlockBuilderApi<Block>,
{
	fn init_with_now(
		&mut self,
		parent_header: &<Block as BlockT>::Header,
		now: Box<dyn Fn() -> time::Instant + Send + Sync>,
	) -> Proposer<Block, C, A> {
		let parent_hash = parent_header.hash();

		info!(
			"🙌 Starting consensus session on top of parent {:?} (#{})",
			parent_hash,
			parent_header.number()
		);

		let proposer = Proposer::<_, _, _> {
			spawn_handle: self.spawn_handle.clone(),
			client: self.client.clone(),
			parent_hash,
			parent_number: *parent_header.number(),
			transaction_pool: self.transaction_pool.clone(),
			now,
			metrics: self.metrics.clone(),
			default_block_size_limit: self.default_block_size_limit,
			soft_deadline_percent: self.soft_deadline_percent,
			telemetry: self.telemetry.clone(),
		};

		proposer
	}
}

impl<A, Block, C> sp_consensus::Environment<Block> for ProposerFactory<A, C>
where
	A: TransactionPool<Block = Block> + 'static,
	Block: BlockT,
	C: HeaderBackend<Block> + ProvideRuntimeApi<Block> + CallApiAt<Block> + Send + Sync + 'static,
	C::Api: ApiExt<Block> + BlockBuilderApi<Block>,
{
	type CreateProposer = future::Ready<Result<Self::Proposer, Self::Error>>;
	type Proposer = Proposer<Block, C, A>;
	type Error = sp_blockchain::Error;

	fn init(&mut self, parent_header: &<Block as BlockT>::Header) -> Self::CreateProposer {
		future::ready(Ok(self.init_with_now(parent_header, Box::new(time::Instant::now))))
	}
}

/// The proposer logic.
pub struct Proposer<Block: BlockT, C, A: TransactionPool> {
	spawn_handle: Box<dyn SpawnNamed>,
	client: Arc<C>,
	parent_hash: Block::Hash,
	parent_number: <<Block as BlockT>::Header as HeaderT>::Number,
	transaction_pool: Arc<A>,
	now: Box<dyn Fn() -> time::Instant + Send + Sync>,
	metrics: PrometheusMetrics,
	default_block_size_limit: usize,
	soft_deadline_percent: Percent,
	telemetry: Option<TelemetryHandle>,
}

impl<A, Block, C> sp_consensus::Proposer<Block> for Proposer<Block, C, A>
where
	A: TransactionPool<Block = Block> + 'static,
	Block: BlockT,
	C: HeaderBackend<Block> + ProvideRuntimeApi<Block> + CallApiAt<Block> + Send + Sync + 'static,
	C::Api: ApiExt<Block> + BlockBuilderApi<Block>,
{
	type Proposal = Pin<Box<dyn Future<Output = Result<Proposal<Block>, Self::Error>> + Send>>;
	type Error = sp_blockchain::Error;

	fn propose(self, args: ProposeArgs<Block>) -> Self::Proposal {
		Self::propose_block(self, args).boxed()
	}
}

/// If the block is full we will attempt to push at most
/// this number of transactions before quitting for real.
/// It allows us to increase block utilization.
const MAX_SKIPPED_TRANSACTIONS: usize = 8;

impl<A, Block, C> Proposer<Block, C, A>
where
	A: TransactionPool<Block = Block> + 'static,
	Block: BlockT,
	C: HeaderBackend<Block> + ProvideRuntimeApi<Block> + CallApiAt<Block> + Send + Sync + 'static,
	C::Api: ApiExt<Block> + BlockBuilderApi<Block>,
{
	/// Propose a new block.
	pub async fn propose_block(
		self,
		args: ProposeArgs<Block>,
	) -> Result<Proposal<Block>, sp_blockchain::Error> {
		let (tx, rx) = oneshot::channel();
		let spawn_handle = self.spawn_handle.clone();

		// Spawn on a new thread, because block production is a blocking operation.
		spawn_handle.spawn_blocking(
			"basic-authorship-proposer",
			None,
			async move {
				let res = self.propose_with(args).await;
				if tx.send(res).is_err() {
					trace!(
						target: LOG_TARGET,
						"Could not send block production result to proposer!"
					);
				}
			}
			.boxed(),
		);

		rx.await?.map_err(Into::into)
	}

	async fn propose_with(
		self,
		args: ProposeArgs<Block>,
	) -> Result<Proposal<Block>, sp_blockchain::Error> {
		let ProposeArgs {
			inherent_data,
			inherent_digests,
			max_duration,
			block_size_limit,
			storage_proof_recorder,
			extra_extensions,
		} = args;
		// leave some time for evaluation and block finalization (10%)
		let deadline = (self.now)() + max_duration - max_duration / 10;
		let block_timer = time::Instant::now();

		let mut block_builder = BlockBuilderBuilder::new(&*self.client)
			.on_parent_block(self.parent_hash)
			.with_parent_block_number(self.parent_number)
			.with_proof_recorder(storage_proof_recorder)
			.with_inherent_digests(inherent_digests)
			.with_extra_extensions(extra_extensions)
			.build()?;

		self.apply_inherents(&mut block_builder, inherent_data)?;

		let mode = block_builder.extrinsic_inclusion_mode();
		let end_reason = match mode {
			ExtrinsicInclusionMode::AllExtrinsics => {
				self.apply_extrinsics(&mut block_builder, deadline, block_size_limit).await?
			},
			ExtrinsicInclusionMode::OnlyInherents => EndProposingReason::TransactionForbidden,
		};
		let (block, storage_changes) = block_builder.build()?.into_inner();
		let block_took = block_timer.elapsed();

		self.print_summary(&block, end_reason, block_took, block_timer.elapsed());
		Ok(Proposal { block, storage_changes })
	}

	/// Apply all inherents to the block.
	fn apply_inherents(
		&self,
		block_builder: &mut sc_block_builder::BlockBuilder<'_, Block, C>,
		inherent_data: InherentData,
	) -> Result<(), sp_blockchain::Error> {
		let create_inherents_start = time::Instant::now();

		let inherent_identifiers = log_enabled!(target: LOG_TARGET, Level::Debug).then(|| {
			inherent_data
				.identifiers()
				.map(|id| String::from_utf8_lossy(id).to_string())
				.collect::<Vec<String>>()
		});

		let inherents = block_builder.create_inherents(inherent_data)?;
		let create_inherents_end = time::Instant::now();

		debug!(target: LOG_TARGET, "apply_inherents: Runtime provided {} inherents. Inherent identifiers present: {:?}", inherents.len(), inherent_identifiers);

		self.metrics.report(|metrics| {
			metrics.create_inherents_time.observe(
				create_inherents_end
					.saturating_duration_since(create_inherents_start)
					.as_secs_f64(),
			);
		});

		for inherent in inherents {
			match block_builder.push(inherent) {
				Err(ApplyExtrinsicFailed(Validity(e))) if e.exhausted_resources() => {
					warn!(
						target: LOG_TARGET,
						"⚠️  Dropping non-mandatory inherent from overweight block."
					)
				},
				Err(ApplyExtrinsicFailed(Validity(e))) if e.was_mandatory() => {
					error!(
						"❌️ Mandatory inherent extrinsic returned error. Block cannot be produced."
					);
					return Err(ApplyExtrinsicFailed(Validity(e)));
				},
				Err(e) => {
					warn!(
						target: LOG_TARGET,
						"❗️ Inherent extrinsic returned unexpected error: {}. Dropping.", e
					);
				},
				Ok(_) => {},
			}
		}
		Ok(())
	}

	/// Apply as many extrinsics as possible to the block.
	async fn apply_extrinsics(
		&self,
		block_builder: &mut sc_block_builder::BlockBuilder<'_, Block, C>,
		deadline: time::Instant,
		block_size_limit: Option<usize>,
	) -> Result<EndProposingReason, sp_blockchain::Error> {
		// proceed with transactions
		// We calculate soft deadline used only in case we start skipping transactions.
		let now = (self.now)();
		let left = deadline.saturating_duration_since(now);
		let left_micros: u64 = left.as_micros().saturated_into();
		let soft_deadline =
			now + time::Duration::from_micros(self.soft_deadline_percent.mul_floor(left_micros));
		let mut skipped = 0;
		let mut unqueue_invalid = TxInvalidityReportMap::new();
		let mut limit_hit_reason: Option<EndProposingReason> = None;

		let delay = deadline.saturating_duration_since((self.now)()) / 8;
		let mut pending_iterator =
			self.transaction_pool.ready_at_with_timeout(self.parent_hash, delay).await;

		let block_size_limit = block_size_limit.unwrap_or(self.default_block_size_limit);

		debug!(target: LOG_TARGET, "Attempting to push transactions from the pool at {:?}.", self.parent_hash);
		let mut transaction_pushed = false;

		let end_reason = loop {
			let pending_tx = if let Some(pending_tx) = pending_iterator.next() {
				pending_tx
			} else {
				debug!(
					target: LOG_TARGET,
					"No more transactions, proceeding with proposing."
				);

				break limit_hit_reason.unwrap_or(EndProposingReason::NoMoreTransactions);
			};

			let now = (self.now)();
			if now > deadline {
				debug!(
					target: LOG_TARGET,
					"Consensus deadline reached when pushing block transactions, \
				proceeding with proposing."
				);
				break limit_hit_reason.unwrap_or(EndProposingReason::HitDeadline);
			}

			let pending_tx_data = (**pending_tx.data()).clone();
			let pending_tx_hash = pending_tx.hash().clone();

			let block_size = block_builder.estimate_block_size();
			if block_size + pending_tx_data.encoded_size() > block_size_limit {
				pending_iterator.report_invalid(&pending_tx);
				limit_hit_reason = Some(EndProposingReason::HitBlockSizeLimit);
				if skipped < MAX_SKIPPED_TRANSACTIONS {
					skipped += 1;
					debug!(
						target: LOG_TARGET,
						"Transaction would overflow the block size limit, \
					 but will try {} more transactions before quitting.",
						MAX_SKIPPED_TRANSACTIONS - skipped,
					);
					continue;
				} else if now < soft_deadline {
					debug!(
						target: LOG_TARGET,
						"Transaction would overflow the block size limit, \
					 but we still have time before the soft deadline, so \
					 we will try a bit more."
					);
					continue;
				} else {
					debug!(
						target: LOG_TARGET,
						"Reached block size limit, proceeding with proposing."
					);
					break EndProposingReason::HitBlockSizeLimit;
				}
			}

			trace!(target: LOG_TARGET, "[{:?}] Pushing to the block.", pending_tx_hash);
			match sc_block_builder::BlockBuilder::push(block_builder, pending_tx_data) {
				Ok(()) => {
					transaction_pushed = true;
					limit_hit_reason = None;
					trace!(target: LOG_TARGET, "[{:?}] Pushed to the block.", pending_tx_hash);
				},
				Err(ApplyExtrinsicFailed(Validity(e))) if e.exhausted_resources() => {
					pending_iterator.report_invalid(&pending_tx);
					limit_hit_reason = Some(EndProposingReason::HitBlockWeightLimit);
					if skipped < MAX_SKIPPED_TRANSACTIONS {
						skipped += 1;
						debug!(target: LOG_TARGET,
							"Block seems full, but will try {} more transactions before quitting.",
							MAX_SKIPPED_TRANSACTIONS - skipped,
						);
					} else if (self.now)() < soft_deadline {
						debug!(target: LOG_TARGET,
							"Block seems full, but we still have time before the soft deadline, \
							 so we will try a bit more before quitting."
						);
					} else {
						debug!(
							target: LOG_TARGET,
							"Reached block weight limit, proceeding with proposing."
						);
						break EndProposingReason::HitBlockWeightLimit;
					}
				},
				Err(e) => {
					pending_iterator.report_invalid(&pending_tx);
					debug!(
						target: LOG_TARGET,
						"[{:?}] Invalid transaction: {} at: {}", pending_tx_hash, e, self.parent_hash
					);

					let error_to_report = match e {
						ApplyExtrinsicFailed(Validity(e)) => Some(e),
						_ => None,
					};

					unqueue_invalid.insert(pending_tx_hash, error_to_report);
				},
			}
		};

		if matches!(end_reason, EndProposingReason::HitBlockSizeLimit) && !transaction_pushed {
			warn!(
				target: LOG_TARGET,
				"Hit block size limit of `{}` without including any transaction!", block_size_limit,
			);
		}

		self.transaction_pool
			.report_invalid(Some(self.parent_hash), unqueue_invalid)
			.await;
		Ok(end_reason)
	}

	/// Prints a summary and does telemetry + metrics.
	///
	/// - `block`: The block that was build.
	/// - `end_reason`: Why did we stop producing the block?
	/// - `block_took`: How long did it took to produce the actual block?
	/// - `propose_took`: How long did the entire proposing took?
	fn print_summary(
		&self,
		block: &Block,
		end_reason: EndProposingReason,
		block_took: time::Duration,
		propose_took: time::Duration,
	) {
		let extrinsics = block.extrinsics();
		self.metrics.report(|metrics| {
			metrics.number_of_transactions.set(extrinsics.len() as u64);
			metrics.block_constructed.observe(block_took.as_secs_f64());
			metrics.report_end_proposing_reason(end_reason);
			metrics.create_block_proposal_time.observe(propose_took.as_secs_f64());
		});

		let extrinsics_summary = if extrinsics.is_empty() {
			"no extrinsics".to_string()
		} else {
			format!(
				"extrinsics ({}): [{}]",
				extrinsics.len(),
				extrinsics
					.iter()
					.map(|xt| BlakeTwo256::hash_of(xt).to_string())
					.collect::<Vec<_>>()
					.join(", ")
			)
		};

		if log::log_enabled!(log::Level::Info) {
			info!(
				"🎁 Prepared block for proposing at {} ({} ms) hash: {:?}; parent_hash: {}; end: {:?}; extrinsics_count: {}",
				block.header().number(),
				block_took.as_millis(),
				<Block as BlockT>::Hash::from(block.header().hash()),
				block.header().parent_hash(),
				end_reason,
				extrinsics.len()
			)
		} else if log::log_enabled!(log::Level::Trace) {
			trace!(
				"🎁 Prepared block for proposing at {} ({} ms) hash: {:?}; parent_hash: {}; end: {:?}; {extrinsics_summary}",
				block.header().number(),
				block_took.as_millis(),
				<Block as BlockT>::Hash::from(block.header().hash()),
				block.header().parent_hash(),
				end_reason
			);
		}

		telemetry!(
			self.telemetry;
			CONSENSUS_INFO;
			"prepared_block_for_proposing";
			"number" => ?block.header().number(),
			"hash" => ?<Block as BlockT>::Hash::from(block.header().hash()),
		);
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use futures::executor::block_on;
	use parking_lot::Mutex;
	use sc_client_api::{Backend, TrieCacheContext};
	use sc_transaction_pool::BasicPool;
	use sc_transaction_pool_api::{ChainEvent, MaintainedTransactionPool, TransactionSource};
	use sp_api::Core;
	use sp_blockchain::HeaderBackend;
	use sp_consensus::{BlockOrigin, Environment};
	use sp_runtime::{generic::BlockId, traits::NumberFor, Perbill};
	use substrate_test_runtime_client::{
		prelude::*,
		runtime::{Block as TestBlock, Extrinsic, ExtrinsicBuilder, Transfer},
		TestClientBuilder, TestClientBuilderExt,
	};

	const SOURCE: TransactionSource = TransactionSource::External;

	// Note:
	// Maximum normal extrinsic size for `substrate_test_runtime` is ~65% of max_block (refer to
	// `substrate_test_runtime::RuntimeBlockWeights` for details).
	// This extrinsic sizing allows for:
	// - one huge xts + a lot of tiny dust
	// - one huge, no medium,
	// - two medium xts
	// This is widely exploited in following tests.
	const HUGE: u32 = 649000000;
	const MEDIUM: u32 = 250000000;
	const TINY: u32 = 1000;

	fn extrinsic(nonce: u64) -> Extrinsic {
		ExtrinsicBuilder::new_fill_block(Perbill::from_parts(TINY)).nonce(nonce).build()
	}

	fn chain_event<B: BlockT>(header: B::Header) -> ChainEvent<B>
	where
		NumberFor<B>: From<u64>,
	{
		ChainEvent::NewBestBlock { hash: header.hash(), tree_route: None }
	}

	#[test]
	fn should_cease_building_block_when_deadline_is_reached() {
		// given
		let client = Arc::new(substrate_test_runtime_client::new());
		let spawner = sp_core::testing::TaskExecutor::new();
		let txpool = Arc::from(BasicPool::new_full(
			Default::default(),
			true.into(),
			None,
			spawner.clone(),
			client.clone(),
		));

		let hashof0 = client.info().genesis_hash;
		block_on(txpool.submit_at(hashof0, SOURCE, vec![extrinsic(0), extrinsic(1)])).unwrap();

		block_on(
			txpool.maintain(chain_event(
				client.expect_header(hashof0).expect("there should be header"),
			)),
		);

		let mut proposer_factory =
			ProposerFactory::new(spawner.clone(), client.clone(), txpool.clone(), None, None);

		let cell = Mutex::new((false, time::Instant::now()));
		let proposer = proposer_factory.init_with_now(
			&client.expect_header(hashof0).unwrap(),
			Box::new(move || {
				let mut value = cell.lock();
				if !value.0 {
					value.0 = true;
					return value.1;
				}
				let old = value.1;
				let new = old + time::Duration::from_secs(1);
				*value = (true, new);
				old
			}),
		);

		// when
		let deadline = time::Duration::from_secs(3);
		let block = block_on(
			proposer.propose_block(ProposeArgs { max_duration: deadline, ..Default::default() }),
		)
		.map(|r| r.block)
		.unwrap();

		// then
		// block should have some extrinsics although we have some more in the pool.
		assert_eq!(block.extrinsics().len(), 1);
		assert_eq!(txpool.ready().count(), 2);
	}

	#[test]
	fn should_not_panic_when_deadline_is_reached() {
		let client = Arc::new(substrate_test_runtime_client::new());
		let spawner = sp_core::testing::TaskExecutor::new();
		let txpool = Arc::from(BasicPool::new_full(
			Default::default(),
			true.into(),
			None,
			spawner.clone(),
			client.clone(),
		));

		let mut proposer_factory =
			ProposerFactory::new(spawner.clone(), client.clone(), txpool.clone(), None, None);

		let cell = Mutex::new((false, time::Instant::now()));
		let proposer = proposer_factory.init_with_now(
			&client.expect_header(client.info().genesis_hash).unwrap(),
			Box::new(move || {
				let mut value = cell.lock();
				if !value.0 {
					value.0 = true;
					return value.1;
				}
				let new = value.1 + time::Duration::from_secs(160);
				*value = (true, new);
				new
			}),
		);

		let deadline = time::Duration::from_secs(1);
		block_on(
			proposer.propose_block(ProposeArgs { max_duration: deadline, ..Default::default() }),
		)
		.map(|r| r.block)
		.unwrap();
	}

	#[test]
	fn proposed_storage_changes_should_match_execute_block_storage_changes() {
		let (client, backend) = TestClientBuilder::new().build_with_backend();
		let client = Arc::new(client);
		let spawner = sp_core::testing::TaskExecutor::new();
		let txpool = Arc::from(BasicPool::new_full(
			Default::default(),
			true.into(),
			None,
			spawner.clone(),
			client.clone(),
		));

		let genesis_hash = client.info().best_hash;

		block_on(txpool.submit_at(genesis_hash, SOURCE, vec![extrinsic(0)])).unwrap();

		block_on(
			txpool.maintain(chain_event(
				client
					.expect_header(client.info().genesis_hash)
					.expect("there should be header"),
			)),
		);

		let mut proposer_factory =
			ProposerFactory::new(spawner.clone(), client.clone(), txpool.clone(), None, None);

		let proposer = proposer_factory.init_with_now(
			&client.header(genesis_hash).unwrap().unwrap(),
			Box::new(move || time::Instant::now()),
		);

		let deadline = time::Duration::from_secs(9);
		let proposal = block_on(
			proposer.propose_block(ProposeArgs { max_duration: deadline, ..Default::default() }),
		)
		.unwrap();

		assert_eq!(proposal.block.extrinsics().len(), 1);

		let api = client.runtime_api();
		api.execute_block(genesis_hash, proposal.block.into()).unwrap();

		let state = backend.state_at(genesis_hash, TrieCacheContext::Untrusted).unwrap();

		let storage_changes = api.into_storage_changes(&state, genesis_hash).unwrap();

		assert_eq!(
			proposal.storage_changes.transaction_storage_root,
			storage_changes.transaction_storage_root,
		);
	}

	// This test ensures that if one transaction of a user was rejected, because for example
	// the weight limit was hit, we don't mark the other transactions of the user as invalid because
	// the nonce is not matching.
	#[test]
	fn should_not_remove_invalid_transactions_from_the_same_sender_after_one_was_invalid() {
		// given
		let client = Arc::new(substrate_test_runtime_client::new());
		let spawner = sp_core::testing::TaskExecutor::new();
		let txpool = Arc::from(BasicPool::new_full(
			Default::default(),
			true.into(),
			None,
			spawner.clone(),
			client.clone(),
		));

		let medium = |nonce| {
			ExtrinsicBuilder::new_fill_block(Perbill::from_parts(MEDIUM))
				.nonce(nonce)
				.build()
		};
		let huge = |nonce| {
			ExtrinsicBuilder::new_fill_block(Perbill::from_parts(HUGE)).nonce(nonce).build()
		};

		block_on(txpool.submit_at(
			client.info().genesis_hash,
			SOURCE,
			vec![medium(0), medium(1), huge(2), medium(3), huge(4), medium(5), medium(6)],
		))
		.unwrap();

		let mut proposer_factory =
			ProposerFactory::new(spawner.clone(), client.clone(), txpool.clone(), None, None);
		let mut propose_block = |client: &TestClient,
		                         parent_number,
		                         expected_block_extrinsics,
		                         expected_pool_transactions| {
			let hash = client.expect_block_hash_from_id(&BlockId::Number(parent_number)).unwrap();
			let proposer = proposer_factory.init_with_now(
				&client.expect_header(hash).unwrap(),
				Box::new(move || time::Instant::now()),
			);

			// when
			let deadline = time::Duration::from_secs(900);
			let block = block_on(
				proposer
					.propose_block(ProposeArgs { max_duration: deadline, ..Default::default() }),
			)
			.map(|r| r.block)
			.unwrap();

			// then
			// block should have some extrinsics although we have some more in the pool.
			assert_eq!(
				txpool.ready().count(),
				expected_pool_transactions,
				"at block: {}",
				block.header.number
			);
			assert_eq!(
				block.extrinsics().len(),
				expected_block_extrinsics,
				"at block: {}",
				block.header.number
			);

			block
		};

		let import_and_maintain = |client: Arc<TestClient>, block: TestBlock| {
			let hash = block.hash();
			block_on(client.import(BlockOrigin::Own, block)).unwrap();
			block_on(txpool.maintain(chain_event(
				client.expect_header(hash).expect("there should be header"),
			)));
		};

		block_on(
			txpool.maintain(chain_event(
				client
					.expect_header(client.info().genesis_hash)
					.expect("there should be header"),
			)),
		);
		assert_eq!(txpool.ready().count(), 7);

		// let's create one block and import it
		let block = propose_block(&client, 0, 2, 7);
		import_and_maintain(client.clone(), block.clone());
		assert_eq!(txpool.ready().count(), 5);

		// now let's make sure that we can still make some progress
		let block = propose_block(&client, 1, 1, 5);
		import_and_maintain(client.clone(), block.clone());
		assert_eq!(txpool.ready().count(), 4);

		// again let's make sure that we can still make some progress
		let block = propose_block(&client, 2, 1, 4);
		import_and_maintain(client.clone(), block.clone());
		assert_eq!(txpool.ready().count(), 3);

		// again let's make sure that we can still make some progress
		let block = propose_block(&client, 3, 1, 3);
		import_and_maintain(client.clone(), block.clone());
		assert_eq!(txpool.ready().count(), 2);

		// again let's make sure that we can still make some progress
		let block = propose_block(&client, 4, 2, 2);
		import_and_maintain(client.clone(), block.clone());
		assert_eq!(txpool.ready().count(), 0);
	}

	#[test]
	fn should_cease_building_block_when_block_limit_is_reached() {
		let client = Arc::new(substrate_test_runtime_client::new());
		let spawner = sp_core::testing::TaskExecutor::new();
		let txpool = Arc::from(BasicPool::new_full(
			Default::default(),
			true.into(),
			None,
			spawner.clone(),
			client.clone(),
		));
		let genesis_hash = client.info().genesis_hash;
		let genesis_header = client.expect_header(genesis_hash).expect("there should be header");

		let extrinsics_num = 5;
		let extrinsics = std::iter::once(
			Transfer {
				from: Sr25519Keyring::Alice.into(),
				to: Sr25519Keyring::Bob.into(),
				amount: 100,
				nonce: 0,
			}
			.into_unchecked_extrinsic(),
		)
		.chain((1..extrinsics_num as u64).map(extrinsic))
		.collect::<Vec<_>>();

		let block_limit = genesis_header.encoded_size() +
			extrinsics
				.iter()
				.take(extrinsics_num - 1)
				.map(Encode::encoded_size)
				.sum::<usize>() +
			Vec::<Extrinsic>::new().encoded_size();

		block_on(txpool.submit_at(genesis_hash, SOURCE, extrinsics.clone())).unwrap();

		block_on(txpool.maintain(chain_event(genesis_header.clone())));

		let mut proposer_factory =
			ProposerFactory::new(spawner.clone(), client.clone(), txpool.clone(), None, None);

		let proposer = block_on(proposer_factory.init(&genesis_header)).unwrap();

		// Give it enough time
		let deadline = time::Duration::from_secs(300);
		let block = block_on(proposer.propose_block(ProposeArgs {
			max_duration: deadline,
			block_size_limit: Some(block_limit),
			..Default::default()
		}))
		.map(|r| r.block)
		.unwrap();

		// Based on the block limit, one transaction shouldn't be included.
		assert_eq!(block.extrinsics().len(), extrinsics_num - 1);

		let proposer = block_on(proposer_factory.init(&genesis_header)).unwrap();

		let block = block_on(
			proposer.propose_block(ProposeArgs { max_duration: deadline, ..Default::default() }),
		)
		.map(|r| r.block)
		.unwrap();

		// Without a block limit we should include all of them
		assert_eq!(block.extrinsics().len(), extrinsics_num);

		let mut proposer_factory =
			ProposerFactory::new(spawner.clone(), client.clone(), txpool.clone(), None, None);

		let proposer = block_on(proposer_factory.init(&genesis_header)).unwrap();

		// Exact block_limit, which includes:
		// 99 (header_size) + 718 (proof@initialize_block) + 246 (one Transfer extrinsic)
		let block_limit = {
			let builder = BlockBuilderBuilder::new(&*client)
				.on_parent_block(genesis_header.hash())
				.with_parent_block_number(0)
				.with_proof_recorder(Some(Default::default()))
				.build()
				.unwrap();
			builder.estimate_block_size() + extrinsics[0].encoded_size()
		};
		let block = block_on(proposer.propose_block(ProposeArgs {
			max_duration: deadline,
			block_size_limit: Some(block_limit),
			storage_proof_recorder: Some(Default::default()),
			..Default::default()
		}))
		.map(|r| r.block)
		.unwrap();

		// The block limit was increased, but we now include the proof in the estimation of the
		// block size and thus, only the `Transfer` will fit into the block. It reads more data
		// than we have reserved in the block limit.
		assert_eq!(block.extrinsics().len(), 1);
	}

	#[test]
	fn should_keep_adding_transactions_after_exhausts_resources_before_soft_deadline() {
		// given
		let client = Arc::new(substrate_test_runtime_client::new());
		let spawner = sp_core::testing::TaskExecutor::new();
		let txpool = Arc::from(BasicPool::new_full(
			Default::default(),
			true.into(),
			None,
			spawner.clone(),
			client.clone(),
		));
		let genesis_hash = client.info().genesis_hash;

		let tiny = |nonce| {
			ExtrinsicBuilder::new_fill_block(Perbill::from_parts(TINY)).nonce(nonce).build()
		};
		let huge = |who| {
			ExtrinsicBuilder::new_fill_block(Perbill::from_parts(HUGE))
				.signer(Sr25519Keyring::numeric(who))
				.build()
		};

		block_on(
			txpool.submit_at(
				genesis_hash,
				SOURCE,
				// add 2 * MAX_SKIPPED_TRANSACTIONS that exhaust resources
				(0..MAX_SKIPPED_TRANSACTIONS * 2)
					.into_iter()
					.map(huge)
					// and some transactions that are okay.
					.chain((0..MAX_SKIPPED_TRANSACTIONS as u64).into_iter().map(tiny))
					.collect(),
			),
		)
		.unwrap();

		block_on(txpool.maintain(chain_event(
			client.expect_header(genesis_hash).expect("there should be header"),
		)));
		assert_eq!(txpool.ready().count(), MAX_SKIPPED_TRANSACTIONS * 3);

		let mut proposer_factory =
			ProposerFactory::new(spawner.clone(), client.clone(), txpool.clone(), None, None);

		let cell = Mutex::new(time::Instant::now());
		let proposer = proposer_factory.init_with_now(
			&client.expect_header(genesis_hash).unwrap(),
			Box::new(move || {
				let mut value = cell.lock();
				let old = *value;
				*value = old + time::Duration::from_secs(1);
				old
			}),
		);

		// when
		// give it enough time so that deadline is never triggered.
		let deadline = time::Duration::from_secs(900);
		let block = block_on(
			proposer.propose_block(ProposeArgs { max_duration: deadline, ..Default::default() }),
		)
		.map(|r| r.block)
		.unwrap();

		// then block should have all non-exhaust resources extrinsics (+ the first one).
		assert_eq!(block.extrinsics().len(), MAX_SKIPPED_TRANSACTIONS + 1);
	}

	#[test]
	fn should_only_skip_up_to_some_limit_after_soft_deadline() {
		// given
		let client = Arc::new(substrate_test_runtime_client::new());
		let spawner = sp_core::testing::TaskExecutor::new();
		let txpool = Arc::from(BasicPool::new_full(
			Default::default(),
			true.into(),
			None,
			spawner.clone(),
			client.clone(),
		));
		let genesis_hash = client.info().genesis_hash;

		let tiny = |who| {
			ExtrinsicBuilder::new_fill_block(Perbill::from_parts(TINY))
				.signer(Sr25519Keyring::numeric(who))
				.nonce(1)
				.build()
		};
		let huge = |who| {
			ExtrinsicBuilder::new_fill_block(Perbill::from_parts(HUGE))
				.signer(Sr25519Keyring::numeric(who))
				.build()
		};

		block_on(
			txpool.submit_at(
				genesis_hash,
				SOURCE,
				(0..MAX_SKIPPED_TRANSACTIONS + 2)
					.into_iter()
					.map(huge)
					// and some transactions that are okay.
					.chain((0..MAX_SKIPPED_TRANSACTIONS + 2).into_iter().map(tiny))
					.collect(),
			),
		)
		.unwrap();

		block_on(txpool.maintain(chain_event(
			client.expect_header(genesis_hash).expect("there should be header"),
		)));
		assert_eq!(txpool.ready().count(), MAX_SKIPPED_TRANSACTIONS * 2 + 4);

		let mut proposer_factory =
			ProposerFactory::new(spawner.clone(), client.clone(), txpool.clone(), None, None);

		let deadline = time::Duration::from_secs(600);
		let cell = Arc::new(Mutex::new((0, time::Instant::now())));
		let cell2 = cell.clone();
		let proposer = proposer_factory.init_with_now(
			&client.expect_header(genesis_hash).unwrap(),
			Box::new(move || {
				let mut value = cell.lock();
				let (called, old) = *value;
				// add time after deadline is calculated internally (hence 1)
				let increase = if called == 1 {
					// we start after the soft_deadline should have already been reached.
					deadline / 2
				} else {
					// but we make sure to never reach the actual deadline
					time::Duration::from_millis(0)
				};
				*value = (called + 1, old + increase);
				old
			}),
		);

		let block = block_on(
			proposer.propose_block(ProposeArgs { max_duration: deadline, ..Default::default() }),
		)
		.map(|r| r.block)
		.unwrap();

		// then the block should have one or two transactions. This maybe random as they are
		// processed in parallel. The same signer and consecutive nonces for huge and tiny
		// transactions guarantees that max two transactions will get to the block.
		assert!(
			(1..3).contains(&block.extrinsics().len()),
			"Block shall contain one or two extrinsics."
		);
		assert!(
			cell2.lock().0 > MAX_SKIPPED_TRANSACTIONS,
			"Not enough calls to current time, which indicates the test might have ended because of deadline, not soft deadline"
		);
	}
}
