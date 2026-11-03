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

//! HOP maintenance: periodic promotion of near-expiry pool entries to permanent
//! on-chain storage and cleanup of expired entries.
//!
//! ## Architecture
//!
//! - [`HopPromoter`] — trait for promoting data on-chain (trait-object friendly).
//! - [`RuntimeApiPromoter`] — concrete implementation that calls the HOP runtime API via dynamic
//!   dispatch (see [`crate::runtime_api`]) plus [`sc_transaction_pool_api::LocalTransactionPool`].
//! - [`try_build_promoter`] — detects runtime API support at startup, returns `Some(promoter)` or
//!   logs a warning and returns `None`.
//! - [`HopMaintenanceTask`] — background task combining promotion + cleanup.

use crate::{pool::HopDataPool, runtime_api};
use sp_api::{ApiExt, CallApiAt, ProvideRuntimeApi};
use sp_blockchain::HeaderBackend;
use sp_runtime::{
	traits::Block as BlockT, AccountId32, MultiSignature, MultiSigner, SaturatedConversion,
};
use std::{marker::PhantomData, sync::Arc, time::Duration};

/// Trait for promoting HOP data to permanent on-chain storage.
///
/// Implemented as a trait object so that `HopMaintenanceTask` is not generic
/// over runtime-specific types. The concrete implementation
/// ([`RuntimeApiPromoter`]) uses the `HopRuntimeApi` runtime API.
pub trait HopPromoter: Send + Sync + 'static {
	/// Promote a blob of HOP data to permanent on-chain storage.
	///
	/// `signer`, `signature`, and `submit_timestamp` are the user's `hop_submit`-time
	/// `MultiSigner`, signature, and wall-clock timestamp (ms since unix epoch),
	/// carried into the unsigned promotion extrinsic so the runtime pallet can
	/// verify consent on-chain and bound the signature's validity window.
	fn promote(
		&self,
		data: Vec<u8>,
		signer: MultiSigner,
		signature: MultiSignature,
		submit_timestamp: u64,
	) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

	/// Whether `hash` is already stored on-chain.
	///
	/// Used by the maintenance task to confirm that a previously submitted
	/// promotion extrinsic was actually included in a block.
	fn is_promoted_on_chain(
		&self,
		hash: &[u8; 32],
	) -> Result<bool, Box<dyn std::error::Error + Send + Sync>>;
}

/// Concrete [`HopPromoter`] that calls the HOP runtime API dynamically (see
/// [`crate::runtime_api`]) to build a promotion extrinsic and submits it to
/// the local transaction pool.
pub struct RuntimeApiPromoter<Block: BlockT, C, P> {
	client: Arc<C>,
	tx_pool: Arc<P>,
	_phantom: PhantomData<Block>,
}

impl<Block, C, P> RuntimeApiPromoter<Block, C, P>
where
	Block: BlockT,
	C: HeaderBackend<Block> + CallApiAt<Block> + Send + Sync + 'static,
	P: sc_transaction_pool_api::LocalTransactionPool<Block = Block> + 'static,
{
	/// Create a new promoter.
	pub fn new(client: Arc<C>, tx_pool: Arc<P>) -> Self {
		Self { client, tx_pool, _phantom: PhantomData }
	}
}

impl<Block, C, P> HopPromoter for RuntimeApiPromoter<Block, C, P>
where
	Block: BlockT,
	C: HeaderBackend<Block> + CallApiAt<Block> + Send + Sync + 'static,
	P: sc_transaction_pool_api::LocalTransactionPool<Block = Block> + 'static,
{
	fn promote(
		&self,
		data: Vec<u8>,
		signer: MultiSigner,
		signature: MultiSignature,
		submit_timestamp: u64,
	) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
		let best_hash = self.client.info().best_hash;
		let ext = runtime_api::create_promotion_extrinsic::<Block, _>(
			&*self.client,
			best_hash,
			data,
			signer,
			signature,
			submit_timestamp,
		)?;
		self.tx_pool
			.submit_local(best_hash, ext)
			.map_err(|e| format!("submit_local failed: {:?}", e))?;
		Ok(())
	}

	fn is_promoted_on_chain(
		&self,
		hash: &[u8; 32],
	) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
		let best_hash = self.client.info().best_hash;
		Ok(runtime_api::is_promoted_on_chain::<Block, _>(&*self.client, best_hash, *hash)?)
	}
}

/// Try to build a [`HopPromoter`] by detecting the `HopRuntimeApi` runtime
/// API at the current best block.
///
/// Returns `Some(promoter)` if the runtime supports the API, or `None` with a
/// warning log if it doesn't.
pub fn try_build_promoter<Block, C, P>(
	client: &Arc<C>,
	tx_pool: &Arc<P>,
) -> Option<Arc<dyn HopPromoter>>
where
	Block: BlockT,
	C: HeaderBackend<Block> + ProvideRuntimeApi<Block> + CallApiAt<Block> + Send + Sync + 'static,
	P: sc_transaction_pool_api::LocalTransactionPool<Block = Block> + 'static,
{
	let best_hash = client.info().best_hash;
	match client
		.runtime_api()
		.has_api_with::<dyn sp_hop::HopRuntimeApi<Block, AccountId32>, _>(best_hash, |v| v >= 1)
	{
		Ok(true) => {
			tracing::info!(target: "hop", "HopRuntimeApi detected — promotion enabled");
			Some(Arc::new(RuntimeApiPromoter::new(client.clone(), tx_pool.clone())))
		},
		Ok(false) => {
			tracing::warn!(
				target: "hop",
				"HOP enabled but runtime does not support HopRuntimeApi — running cleanup only"
			);
			None
		},
		Err(e) => {
			tracing::warn!(
				target: "hop",
				error = %e,
				"Failed to check HopRuntimeApi support — running cleanup only"
			);
			None
		},
	}
}

/// Build a [`HopMaintenanceTask`] wired to the node's client and transaction pool.
///
/// Detects `HopRuntimeApi` support at startup (see [`try_build_promoter`]) and captures
/// a best-block closure over `client` so callers only need to spawn the returned
/// task on their task manager.
pub fn build_maintenance_task<Block, C, P>(
	client: &Arc<C>,
	tx_pool: &Arc<P>,
	pool: Arc<HopDataPool>,
	buffer_secs: u64,
	check_interval_secs: u64,
) -> HopMaintenanceTask
where
	Block: BlockT,
	C: HeaderBackend<Block> + ProvideRuntimeApi<Block> + CallApiAt<Block> + Send + Sync + 'static,
	P: sc_transaction_pool_api::LocalTransactionPool<Block = Block> + 'static,
{
	let promoter = try_build_promoter::<Block, _, _>(client, tx_pool);
	let best_block_client = client.clone();
	let best_block: Arc<dyn Fn() -> u32 + Send + Sync> =
		Arc::new(move || best_block_client.info().best_number.saturated_into::<u32>());
	HopMaintenanceTask::new(pool, promoter, best_block, buffer_secs, check_interval_secs)
}

/// Background task that periodically promotes near-expiry HOP pool entries to
/// permanent on-chain storage and cleans up expired entries.
pub struct HopMaintenanceTask {
	hop_pool: Arc<HopDataPool>,
	promoter: Option<Arc<dyn HopPromoter>>,
	buffer_secs: u64,
	check_interval_secs: u64,
	check_interval_blocks: u32,
	best_block: Arc<dyn Fn() -> u32 + Send + Sync>,
}

impl HopMaintenanceTask {
	/// Create a new maintenance task.
	///
	/// - `promoter`: `Some` to enable on-chain promotion, `None` for cleanup-only.
	/// - `best_block`: closure returning the current best block number.
	/// - `buffer_secs`: how many seconds before expiry to start promoting.
	/// - `check_interval_secs`: how often to run the maintenance cycle.
	pub fn new(
		hop_pool: Arc<HopDataPool>,
		promoter: Option<Arc<dyn HopPromoter>>,
		best_block: Arc<dyn Fn() -> u32 + Send + Sync>,
		buffer_secs: u64,
		check_interval_secs: u64,
	) -> Self {
		let check_interval_blocks =
			(check_interval_secs.max(1) / crate::types::HOP_BLOCK_TIME_SECS.max(1)).max(1) as u32;
		Self {
			hop_pool,
			promoter,
			buffer_secs,
			check_interval_secs,
			check_interval_blocks,
			best_block,
		}
	}

	/// Run the maintenance loop.
	pub async fn run(self) {
		loop {
			futures_timer::Delay::new(Duration::from_secs(self.check_interval_secs)).await;
			self.tick();
		}
	}

	/// Execute a single maintenance cycle: promote near-expiry entries and clean up expired ones.
	pub fn tick(&self) {
		let current_block = (self.best_block)();

		// Promote near-expiry entries one at a time to bound peak memory.
		if let Some(ref promoter) = self.promoter {
			const PROMOTION_BATCH_SIZE: usize = 10;
			let hashes =
				self.hop_pool
					.get_promotable(current_block, self.buffer_secs, PROMOTION_BATCH_SIZE);
			for hash in hashes {
				// First, ask the runtime whether this hash is already on-chain.
				// If so, the previous attempt (or a third party) already
				// landed it — flag locally and stop touching the chain.
				match promoter.is_promoted_on_chain(hash.as_fixed_bytes()) {
					Ok(true) => {
						self.hop_pool.mark_promoted(&hash);
						tracing::info!(
							target: "hop",
							hash = ?hex::encode(hash),
							"HOP entry already on-chain — flagged locally"
						);
						continue;
					},
					Ok(false) => {},
					Err(e) => {
						// Treat runtime-API failures as "unknown", which means
						// proceed with submission. Worst case we resubmit a
						// duplicate; the on-chain check will catch it next cycle.
						tracing::warn!(
							target: "hop",
							hash = ?hex::encode(hash),
							error = %e,
							"is_promoted_on_chain failed; assuming not on-chain"
						);
					},
				}

				let (data, signer, signature, submit_timestamp) =
					match self.hop_pool.get_with_auth(&hash) {
						Some(t) => t,
						None => continue,
					};
				let size = data.len();
				let result = promoter.promote(data, signer, signature, submit_timestamp);
				// Backoff on every attempt — both Ok (submitted to local pool, may
				// or may not get included) and Err (pool rejected). Without this,
				// every cycle would resubmit the same extrinsic for the entry's
				// lifetime, wasting fees and authorization budget if the runtime
				// pallet does not deduplicate.
				self.hop_pool.record_promotion_attempt(
					&hash,
					current_block,
					self.check_interval_blocks,
				);
				match result {
					Ok(()) => tracing::info!(
						target: "hop",
						hash = ?hex::encode(hash),
						size,
						"Submitted HOP promotion extrinsic; awaiting on-chain confirmation"
					),
					Err(e) => tracing::warn!(
						target: "hop",
						hash = ?hex::encode(hash),
						error = %e,
						"Failed to submit HOP promotion extrinsic; will back off"
					),
				}
			}
		}

		let freed = self.hop_pool.cleanup_expired(self.buffer_secs);
		if freed > 0 {
			tracing::info!(
				target: "hop",
				freed_bytes = freed,
				"Cleaned up expired HOP entries"
			);
		}

		self.hop_pool.metrics().record_maintenance_tick();
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		pool::HopDataPool,
		rate_limit::RateLimitConfig,
		types::{Recipient, RecipientVec, SenderId},
	};
	use sp_core::{crypto::Pair, ed25519};
	use sp_runtime::{MultiSignature, MultiSigner};
	use std::sync::Mutex;
	use tempfile::TempDir;

	const SENDER_A: SenderId = [1u8; 32];

	fn test_recipient() -> (ed25519::Pair, MultiSigner) {
		let pair = ed25519::Pair::from_seed(&[1u8; 32]);
		let signer = MultiSigner::Ed25519(pair.public());
		(pair, signer)
	}

	fn dummy_auth() -> (MultiSigner, MultiSignature) {
		let pair = ed25519::Pair::from_seed(&[7u8; 32]);
		let signer = MultiSigner::Ed25519(pair.public());
		let sig = MultiSignature::Ed25519(pair.sign(&[]));
		(signer, sig)
	}

	fn bv(v: Vec<MultiSigner>) -> RecipientVec {
		let recipients: Vec<Recipient> =
			v.into_iter().map(|signer| Recipient { signer, claimed: false }).collect();
		RecipientVec::try_from(recipients).expect("test recipient list exceeds MAX_RECIPIENTS")
	}

	fn test_pool(max_size: u64, retention_secs: u64, dir: &TempDir) -> Arc<HopDataPool> {
		Arc::new(
			HopDataPool::new(
				max_size,
				max_size,
				retention_secs,
				dir.path().to_path_buf(),
				RateLimitConfig::disabled(),
				crate::metrics::HopMetrics::disabled(),
			)
			.unwrap(),
		)
	}

	struct MockPromoter {
		calls: Mutex<Vec<Vec<u8>>>,
		should_fail: bool,
		/// Hashes that the mock claims are already stored on-chain.
		on_chain: Mutex<std::collections::HashSet<[u8; 32]>>,
	}

	impl MockPromoter {
		fn new(should_fail: bool) -> Self {
			Self {
				calls: Mutex::new(Vec::new()),
				should_fail,
				on_chain: Mutex::new(std::collections::HashSet::new()),
			}
		}

		fn call_count(&self) -> usize {
			self.calls.lock().unwrap().len()
		}

		fn calls(&self) -> Vec<Vec<u8>> {
			self.calls.lock().unwrap().clone()
		}

		/// Mark a hash as on-chain (subsequent `is_promoted_on_chain` returns `true`).
		fn set_on_chain(&self, hash: [u8; 32]) {
			self.on_chain.lock().unwrap().insert(hash);
		}
	}

	impl HopPromoter for MockPromoter {
		fn promote(
			&self,
			data: Vec<u8>,
			_signer: MultiSigner,
			_signature: MultiSignature,
			_submit_timestamp: u64,
		) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
			self.calls.lock().unwrap().push(data);
			if self.should_fail {
				Err("mock failure".into())
			} else {
				Ok(())
			}
		}

		fn is_promoted_on_chain(
			&self,
			hash: &[u8; 32],
		) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
			Ok(self.on_chain.lock().unwrap().contains(hash))
		}
	}

	#[test]
	fn tick_promotes_near_expiry_entries() {
		let dir = TempDir::new().unwrap();
		let pool = test_pool(1024 * 1024, 100, &dir);
		let (_, signer) = test_recipient();

		let hash = pool
			.insert(vec![42u8; 10], bv(vec![signer]), SENDER_A, dummy_auth().0, dummy_auth().1, 0)
			.unwrap();

		let promoter = Arc::new(MockPromoter::new(false));
		let task = HopMaintenanceTask::new(
			pool.clone(),
			Some(promoter.clone()),
			Arc::new(|| 80), // current block = 80
			180,             // buffer_secs = 180 > retention=100 → in window
			60,
		);

		task.tick();

		assert_eq!(promoter.call_count(), 1);
		assert_eq!(promoter.calls()[0], vec![42u8; 10]);

		// Entry stays in the pool (not yet confirmed on-chain) and is excluded
		// from the next promotable batch by the post-attempt backoff window.
		assert!(pool.has(&hash));
		let promotable = pool.get_promotable(80, 180, usize::MAX);
		assert!(promotable.is_empty(), "back-off should suppress immediate re-promotion");
	}

	#[test]
	fn tick_skips_promotion_when_no_promoter() {
		let dir = TempDir::new().unwrap();
		let pool = test_pool(1024 * 1024, 100, &dir);
		let (_, signer) = test_recipient();

		pool.insert(vec![42u8; 10], bv(vec![signer]), SENDER_A, dummy_auth().0, dummy_auth().1, 0)
			.unwrap();

		let task = HopMaintenanceTask::new(
			pool.clone(),
			None, // no promoter
			Arc::new(|| 80),
			180,
			60,
		);

		task.tick();

		// Entry should still be promotable (no promoter to process it).
		let promotable = pool.get_promotable(80, 180, usize::MAX);
		assert_eq!(promotable.len(), 1);
	}

	#[test]
	fn tick_does_not_mark_promoted_on_failure() {
		let dir = TempDir::new().unwrap();
		let pool = test_pool(1024 * 1024, 100, &dir);
		let (_, signer) = test_recipient();

		pool.insert(vec![42u8; 10], bv(vec![signer]), SENDER_A, dummy_auth().0, dummy_auth().1, 0)
			.unwrap();

		let promoter = Arc::new(MockPromoter::new(true)); // will fail
		let task =
			HopMaintenanceTask::new(pool.clone(), Some(promoter.clone()), Arc::new(|| 80), 180, 60);

		task.tick();

		// Promoter was called but failed.
		assert_eq!(promoter.call_count(), 1);

		// Failure schedules a back-off rather than re-marking immediately. The
		// entry isn't promotable at the failure block, but becomes promotable
		// again once the back-off (1× check_interval = 10 blocks at 6 s/block)
		// elapses — and crucially, never gets `mark_promoted`.
		assert!(pool.get_promotable(80, 180, usize::MAX).is_empty());
		assert_eq!(pool.get_promotable(95, 180, usize::MAX).len(), 1);
	}

	#[test]
	fn tick_cleans_up_expired_entries() {
		let dir = TempDir::new().unwrap();
		// retention=0 secs so entries expire immediately on the next cleanup pass.
		let pool = test_pool(1024 * 1024, 0, &dir);
		let (_, signer) = test_recipient();

		pool.insert(vec![42u8; 50], bv(vec![signer]), SENDER_A, dummy_auth().0, dummy_auth().1, 0)
			.unwrap();
		assert_eq!(pool.status().entry_count, 1);

		let task = HopMaintenanceTask::new(pool.clone(), None, Arc::new(|| 0), 5, 60);

		task.tick();

		assert_eq!(pool.status().entry_count, 0);
		assert_eq!(pool.status().total_bytes, 0);
	}

	#[test]
	fn tick_promotes_then_cleans_up_independently() {
		let dir = TempDir::new().unwrap();
		let pool = test_pool(1024 * 1024, 100, &dir);
		let (_, signer) = test_recipient();

		let hash = pool
			.insert(
				vec![1u8; 10],
				bv(vec![signer.clone()]),
				SENDER_A,
				dummy_auth().0,
				dummy_auth().1,
				0,
			)
			.unwrap();

		let promoter = Arc::new(MockPromoter::new(false));

		// First tick at block 80: promote is attempted. Entry is NOT marked promoted
		// yet because we have not confirmed inclusion on-chain.
		let block = Arc::new(Mutex::new(80u32));
		let block_clone = block.clone();
		let task = HopMaintenanceTask::new(
			pool.clone(),
			Some(promoter.clone()),
			Arc::new(move || *block_clone.lock().unwrap()),
			180,
			60,
		);

		task.tick();
		assert_eq!(promoter.call_count(), 1);
		assert_eq!(pool.status().entry_count, 1);

		// Simulate the runtime confirming inclusion: from now on, the on-chain check
		// returns true.
		promoter.set_on_chain(*hash.as_fixed_bytes());

		// Second tick: the on-chain check short-circuits to mark_promoted.
		// Promoter must not be invoked a second time.
		*block.lock().unwrap() = 100;
		task.tick();
		assert_eq!(promoter.call_count(), 1, "promoter must not be called again once on-chain");
	}

	#[test]
	fn tick_skips_promotion_when_already_on_chain() {
		let dir = TempDir::new().unwrap();
		let pool = test_pool(1024 * 1024, 100, &dir);
		let (_, signer) = test_recipient();

		let hash = pool
			.insert(vec![42u8; 10], bv(vec![signer]), SENDER_A, dummy_auth().0, dummy_auth().1, 0)
			.unwrap();

		let promoter = Arc::new(MockPromoter::new(false));
		// The entry is "already on-chain" before any tick runs.
		promoter.set_on_chain(*hash.as_fixed_bytes());

		let task =
			HopMaintenanceTask::new(pool.clone(), Some(promoter.clone()), Arc::new(|| 80), 180, 60);

		task.tick();

		assert_eq!(promoter.call_count(), 0, "promote must not be called when already on-chain");
		assert!(pool.get_promotable(80, 180, usize::MAX).is_empty());
	}

	#[test]
	fn tick_retries_unconfirmed_with_backoff() {
		let dir = TempDir::new().unwrap();
		let pool = test_pool(1024 * 1024, 100, &dir);
		let (_, signer) = test_recipient();

		pool.insert(vec![42u8; 10], bv(vec![signer]), SENDER_A, dummy_auth().0, dummy_auth().1, 0)
			.unwrap();

		let promoter = Arc::new(MockPromoter::new(false));
		let block = Arc::new(Mutex::new(80u32));
		let block_clone = block.clone();
		// check_interval_secs=60 → check_interval_blocks = 60/HOP_BLOCK_TIME_SECS.
		// With the default HOP_BLOCK_TIME_SECS=6 → check_interval_blocks = 10.
		let task = HopMaintenanceTask::new(
			pool.clone(),
			Some(promoter.clone()),
			Arc::new(move || *block_clone.lock().unwrap()),
			180,
			60,
		);

		// Tick 1 at block 80: promote (submit_local Ok). Backoff: next attempt at 80 + 10 = 90.
		task.tick();
		assert_eq!(promoter.call_count(), 1);

		// Still inside the backoff window: nothing happens.
		*block.lock().unwrap() = 85;
		task.tick();
		assert_eq!(promoter.call_count(), 1);

		// Past the first backoff: tick fires again. Backoff after attempt 2 is 2× = 20 blocks,
		// so next attempt at 90 + 20 = 110.
		*block.lock().unwrap() = 90;
		task.tick();
		assert_eq!(promoter.call_count(), 2);

		// Inside the new backoff window.
		*block.lock().unwrap() = 109;
		task.tick();
		assert_eq!(promoter.call_count(), 2);
	}
}
