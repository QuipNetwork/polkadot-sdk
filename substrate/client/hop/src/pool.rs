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

//! HOP data pool: in-memory index backed by sharded on-disk storage.
//!
//! ## On-disk layout
//!
//! The pool root contains two subdirectories, `blobs/` and `meta/`, each
//! sharded into 256 subdirectories named `00`–`ff` after the first byte of the
//! content hash. An entry with hash `H` is stored as:
//!
//! - `blobs/<H[0:2]>/<H>.blob` — raw payload bytes
//! - `meta/<H[0:2]>/<H>.meta` — SCALE-encoded [`HopEntryMeta`]
//!
//! ## Recovery
//!
//! On startup the pool scans every `meta/` shard, decodes each `.meta` file,
//! and rebuilds the in-memory index. `.meta` files that are corrupt, have an
//! unexpected version, or lack a sibling `.blob` are deleted. Then the
//! corresponding `blobs/` shard is scanned and any `.blob` without an entry in
//! the freshly-built index (orphan) is also deleted. Stale `.tmp.*` files left
//! by a previous crash are removed during both scans.

use crate::{
	metrics::{removal_reasons, HopMetrics},
	rate_limit::{RateLimitConfig, RateLimiter},
	types::{
		entry_accounted_size, promotion_backoff_blocks, signing_payload, HopBlockNumber,
		HopEntryMeta, HopError, HopHash, PoolStatus, RecipientVec, SenderId, HOP_ACK_CONTEXT,
		HOP_CLAIM_CONTEXT, HOP_META_VERSION, MAX_PROMOTION_ATTEMPTS,
	},
};
use codec::{Decode, Encode};
use parking_lot::{Mutex, RwLock};
use sp_core::H256;
use sp_crypto_hashing::blake2_256;
use sp_runtime::{
	traits::{IdentifyAccount, Verify},
	MultiSignature, MultiSigner,
};
use std::{
	collections::{BTreeSet, HashMap, HashSet},
	fs,
	path::{Path, PathBuf},
	process,
	sync::{
		atomic::{AtomicU64, Ordering},
		Arc,
	},
	time::{SystemTime, UNIX_EPOCH},
};

/// Per-process counter that disambiguates concurrent atomic writes targeting
/// the same final path. Two threads computing the same content hash would
/// otherwise share a `<path>.tmp` file and stomp each other's bytes.
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

const BLOBS_DIR: &str = "blobs";
const META_DIR: &str = "meta";
const BLOB_EXT: &str = "blob";
const META_EXT: &str = "meta";
/// Number of shards used for both `blobs/` and `meta/` directories (one per
/// first-byte value of the content hash: `00`–`ff`).
const SHARD_COUNT: u16 = 256;

/// HOP data pool with disk-backed blob storage and in-memory metadata index.
pub struct HopDataPool {
	/// In-memory metadata index (no blobs).
	index: Mutex<HashMap<HopHash, HopEntryMeta>>,
	/// Per-user byte usage tracked by sender id.
	///
	/// Counters live directly in the map and are charged via `charge_user`
	/// inside the read guard, so the reclamation pass in `cleanup_expired`
	/// (which holds `user_usage.write()` together with `index.lock()`) cannot
	/// interpose between a lookup and its `fetch_add`. Stale entries —
	/// counter 0 and no live index entry — are reclaimed by the same pass.
	user_usage: RwLock<HashMap<SenderId, AtomicU64>>,
	/// Maximum pool size in bytes (counts both data and per-entry metadata overhead).
	max_size: u64,
	/// Fixed hard per-user quota in bytes.
	max_user_size: u64,
	/// Current pool size in bytes (accounted size — includes metadata overhead).
	current_size: AtomicU64,
	/// Data retention period in seconds.
	retention_secs: u64,
	/// Root data directory containing blobs/ and meta/ subdirectories.
	data_dir: PathBuf,
	/// Per-account submit rate limiter.
	rate_limiter: Arc<RateLimiter>,
	/// Prometheus metrics (no-ops without a registry).
	metrics: HopMetrics,
}

impl HopDataPool {
	/// Create a new disk-backed data pool.
	///
	/// Creates shard directories under `data_dir` and rebuilds the in-memory index
	/// from existing `.meta` files on disk (recovery after restart).
	pub fn new(
		max_size: u64,
		max_user_size: u64,
		retention_secs: u64,
		data_dir: PathBuf,
		rate_limit_cfg: RateLimitConfig,
		metrics: HopMetrics,
	) -> Result<Self, HopError> {
		// Create shard directories (256 each for blobs/ and meta/).
		for i in 0..SHARD_COUNT {
			let shard = format!("{:02x}", i as u8);
			fs::create_dir_all(data_dir.join(BLOBS_DIR).join(&shard))?;
			fs::create_dir_all(data_dir.join(META_DIR).join(&shard))?;
		}

		let mut index = HashMap::new();
		let mut user_usage: HashMap<SenderId, AtomicU64> = HashMap::new();
		let mut current_size = 0u64;
		// Orphan `.blob`s are not counted: without a `.meta` they were never a
		// claimable entry.
		let mut dropped = 0u64;

		// Rebuild index from .meta files and clean orphan .blobs in a single pass.
		for i in 0..SHARD_COUNT {
			let shard = format!("{:02x}", i as u8);

			// Scan .meta files → rebuild index (removes corrupt/orphan .meta files).
			let meta_shard_dir = data_dir.join(META_DIR).join(&shard);
			if let Ok(entries) = fs::read_dir(&meta_shard_dir) {
				for entry in entries.flatten() {
					let path = entry.path();
					if path.extension().and_then(|e| e.to_str()) != Some(META_EXT) {
						if path
							.file_name()
							.and_then(|n| n.to_str())
							.map_or(false, |n| n.contains(".tmp."))
						{
							let _ = fs::remove_file(&path);
						}
						continue;
					}

					let stem = match path.file_stem().and_then(|s| s.to_str()) {
						Some(s) => s.to_string(),
						None => continue,
					};

					let Some(hash) = parse_hex_hash(&stem) else {
						tracing::warn!(target: "hop", path = ?path, "Removing .meta with invalid name");
						let _ = fs::remove_file(&path);
						dropped += 1;
						continue;
					};

					let meta_bytes = match fs::read(&path) {
						Ok(b) => b,
						Err(e) => {
							tracing::warn!(target: "hop", path = ?path, error = %e, "Removing unreadable .meta");
							let _ = fs::remove_file(&path);
							dropped += 1;
							continue;
						},
					};
					let meta = match HopEntryMeta::decode(&mut &meta_bytes[..]) {
						Ok(m) => m,
						Err(e) => {
							tracing::warn!(target: "hop", path = ?path, error = %e, "Removing corrupt .meta");
							let _ = fs::remove_file(&path);
							dropped += 1;
							continue;
						},
					};
					if meta.version != HOP_META_VERSION {
						tracing::warn!(
							target: "hop",
							path = ?path,
							version = meta.version,
							expected = HOP_META_VERSION,
							"Removing .meta with unsupported on-disk version",
						);
						let _ = fs::remove_file(&path);
						let _ = fs::remove_file(Self::entry_path(
							&data_dir, &hash, BLOBS_DIR, BLOB_EXT,
						));
						dropped += 1;
						continue;
					}

					let blob_path = Self::entry_path(&data_dir, &hash, BLOBS_DIR, BLOB_EXT);
					if !blob_path.exists() {
						tracing::warn!(target: "hop", hash = ?stem, "Removing orphan .meta (no .blob)");
						let _ = fs::remove_file(&path);
						dropped += 1;
						continue;
					}

					let accounted = entry_accounted_size(meta.size, meta.recipients.len());
					current_size += accounted;
					user_usage
						.entry(meta.sender_id)
						.or_default()
						.fetch_add(accounted, Ordering::Relaxed);
					index.insert(hash, meta);
				}
			}

			// Scan .blob files → remove orphans (blobs without corresponding .meta).
			let blob_shard_dir = data_dir.join(BLOBS_DIR).join(&shard);
			if let Ok(entries) = fs::read_dir(&blob_shard_dir) {
				for entry in entries.flatten() {
					let path = entry.path();
					if path.extension().and_then(|e| e.to_str()) != Some(BLOB_EXT) {
						if path
							.file_name()
							.and_then(|n| n.to_str())
							.map_or(false, |n| n.contains(".tmp."))
						{
							let _ = fs::remove_file(&path);
						}
						continue;
					}
					let stem = match path.file_stem().and_then(|s| s.to_str()) {
						Some(s) => s.to_string(),
						None => continue,
					};
					// Any blob without a corresponding index entry is an orphan.
					// The meta scan for this shard already populated `index`, so an
					// in-memory lookup is sufficient and avoids a syscall per blob.
					// Blobs with unparseable names have no possible index match and
					// are always removed.
					let is_orphan = match parse_hex_hash(&stem) {
						Some(hash) => !index.contains_key(&hash),
						None => true,
					};
					if is_orphan {
						tracing::warn!(target: "hop", hash = ?stem, "Removing orphan .blob (no .meta)");
						let _ = fs::remove_file(&path);
					}
				}
			}
		}

		tracing::info!(
			target: "hop",
			entries = index.len(),
			total_bytes = current_size,
			dropped,
			"Recovered HOP pool from disk"
		);

		metrics.set_pool_status(index.len() as u64, current_size, max_size);
		metrics.record_removed(removal_reasons::STARTUP_DROPPED, dropped);

		Ok(Self {
			index: Mutex::new(index),
			user_usage: RwLock::new(user_usage),
			max_size,
			max_user_size,
			current_size: AtomicU64::new(current_size),
			retention_secs,
			data_dir,
			rate_limiter: Arc::new(RateLimiter::new(rate_limit_cfg)),
			metrics,
		})
	}

	/// Metrics shared by the pool, RPC server, and maintenance task.
	pub(crate) fn metrics(&self) -> &HopMetrics {
		&self.metrics
	}

	/// Snapshot the pool size gauges after a mutation, from inside the index
	/// critical section with `entries` read from the locked index: publishing
	/// after the unlock would let an earlier writer overtake a later one and
	/// leave the gauges stale.
	fn publish_size_metrics(&self, entries: usize) {
		self.metrics
			.set_pool_size(entries as u64, self.current_size.load(Ordering::Relaxed));
	}

	/// Charge `accounted` bytes against `sender_id`'s per-user quota, creating
	/// a zero-initialized counter if absent. The read guard held across the
	/// `fetch_add` excludes the reclamation pass in `cleanup_expired` (which
	/// takes `user_usage.write()`), so the counter cannot be reclaimed
	/// between lookup and increment.
	fn charge_user(&self, sender_id: &SenderId, accounted: u64) -> Result<(), HopError> {
		// Fast path: sender already in map, a read guard is enough.
		{
			let usage = self.user_usage.read();
			if let Some(counter) = usage.get(sender_id) {
				return self.try_charge(counter, accounted);
			}
		}
		// Cold path: first insert from this sender — take the write guard.
		let mut usage = self.user_usage.write();
		let counter = usage.entry(*sender_id).or_default();
		self.try_charge(counter, accounted)
	}

	/// Atomically increment `counter` by `accounted`, rolling back on cap
	/// overflow. `saturating_add` clamps to `u64::MAX` if concurrent failing
	/// charges briefly inflate the previous value past the wrap point,
	/// ensuring overflow always falls into the "exceeds cap" branch.
	fn try_charge(&self, counter: &AtomicU64, accounted: u64) -> Result<(), HopError> {
		let previous = counter.fetch_add(accounted, Ordering::Relaxed);
		if previous.saturating_add(accounted) > self.max_user_size {
			counter.fetch_sub(accounted, Ordering::Relaxed);
			return Err(HopError::UserQuotaExceeded { used: previous, limit: self.max_user_size });
		}
		Ok(())
	}

	/// Decrement a user's usage counter. Counters are never removed by this
	/// path; reclamation happens only in the per-sender pass at the end of
	/// `cleanup_expired`.
	fn release_user_quota(&self, sender_id: &SenderId, accounted: u64) {
		if let Some(counter) = self.user_usage.read().get(sender_id) {
			saturating_release(counter, accounted);
		}
	}

	/// Path to a file within a shard subdirectory rooted at `data_dir`.
	fn entry_path(data_dir: &Path, hash: &HopHash, subdir: &str, ext: &str) -> PathBuf {
		let hex = hex::encode(hash);
		data_dir.join(subdir).join(&hex[..2]).join(format!("{}.{}", hex, ext))
	}

	/// Path to the blob file for a given hash.
	fn blob_path(&self, hash: &HopHash) -> PathBuf {
		Self::entry_path(&self.data_dir, hash, BLOBS_DIR, BLOB_EXT)
	}

	/// Path to the meta file for a given hash.
	fn meta_path(&self, hash: &HopHash) -> PathBuf {
		Self::entry_path(&self.data_dir, hash, META_DIR, META_EXT)
	}

	/// Atomically write data to a file (write to a unique .tmp path, then rename).
	///
	/// The tmp suffix encodes process id + a per-process atomic counter so two
	/// threads writing the same final path (i.e. same content-addressed hash)
	/// do not race on a shared tmp file. Removes the tmp file on failure so a
	/// failed write never leaves an orphan.
	fn write_atomic(path: &Path, data: &[u8]) -> Result<(), HopError> {
		let suffix = format!("tmp.{}.{}", process::id(), TMP_SEQ.fetch_add(1, Ordering::Relaxed));
		let tmp_path = path.with_extension(suffix);
		if let Err(e) = fs::write(&tmp_path, data) {
			let _ = fs::remove_file(&tmp_path);
			return Err(e.into());
		}
		if let Err(e) = fs::rename(&tmp_path, path) {
			let _ = fs::remove_file(&tmp_path);
			return Err(e.into());
		}
		Ok(())
	}

	/// Insert data into the pool.
	///
	/// Returns the hash of the data.
	pub fn insert(
		&self,
		data: Vec<u8>,
		recipients: RecipientVec,
		sender_id: SenderId,
		signer: MultiSigner,
		signature: MultiSignature,
		submit_timestamp: u64,
	) -> Result<HopHash, HopError> {
		if recipients.is_empty() {
			return Err(HopError::NoRecipients);
		}
		let unique: BTreeSet<&MultiSigner> = recipients.iter().map(|r| &r.signer).collect();
		if unique.len() != recipients.len() {
			return Err(HopError::DuplicateRecipient);
		}

		if data.is_empty() {
			return Err(HopError::EmptyData);
		}

		let data_len = data.len() as u64;

		// Total accounted size includes bounded per-recipient metadata overhead so
		// a submitter cannot inflate memory via large recipient lists while the
		// capacity counter only tracks `data.len()`. Charge the rate limiter the
		// same accounted size, otherwise a 1-byte payload with 256 recipients
		// would cost ~10 KiB of pool capacity while only spending 1 byte of
		// bandwidth tokens — making the bandwidth dimension non-functional for
		// fan-out-heavy entries.
		let accounted = entry_accounted_size(data_len, recipients.len());

		// Rejected requests never reserve capacity — check before any atomic bump.
		if let Err(retry_after_secs) = self.rate_limiter.check(&sender_id, accounted) {
			return Err(HopError::RateLimited { retry_after_secs });
		}

		let previous_size = self.current_size.fetch_add(accounted, Ordering::Relaxed);
		if previous_size.saturating_add(accounted) > self.max_size {
			self.current_size.fetch_sub(accounted, Ordering::Relaxed);
			return Err(HopError::PoolFull(previous_size, self.max_size));
		}

		if let Err(e) = self.charge_user(&sender_id, accounted) {
			self.current_size.fetch_sub(accounted, Ordering::Relaxed);
			return Err(e);
		}

		let hash = H256(blake2_256(&data));

		// First duplicate check (read lock only).
		{
			let index = self.index.lock();
			if index.contains_key(&hash) {
				self.release_user_quota(&sender_id, accounted);
				self.current_size.fetch_sub(accounted, Ordering::Relaxed);
				return Err(HopError::DuplicateEntry);
			}
		}

		// Blob write is outside the lock — content-addressed bytes, racers
		// produce identical output, rename is atomic.
		let blob_path = self.blob_path(&hash);
		if let Err(e) = Self::write_atomic(&blob_path, &data) {
			self.release_user_quota(&sender_id, accounted);
			self.current_size.fetch_sub(accounted, Ordering::Relaxed);
			return Err(e);
		}

		let expires_at = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.unwrap_or_default()
			.as_secs()
			.saturating_add(self.retention_secs);
		let meta = HopEntryMeta::new(
			data_len,
			expires_at,
			recipients,
			sender_id,
			signer,
			signature,
			submit_timestamp,
		);
		let meta_bytes = meta.encode();
		let meta_path = self.meta_path(&hash);

		// Meta write goes under the index lock: meta is not content-addressed
		// (sender_id, signer, signature, recipients, submit_timestamp differ
		// between submitters), so racing writers would otherwise leave the
		// loser's bytes on disk, diverging from the winner held in memory.
		{
			let mut index = self.index.lock();
			if index.contains_key(&hash) {
				tracing::debug!(
					target: "hop",
					hash = ?hex::encode(hash),
					"Duplicate insert race lost; keeping winner's files"
				);
				// Drop `index` before `release_user_quota` takes `user_usage.read()`
				// to keep the outer-to-inner lock order matching `cleanup_expired`.
				drop(index);
				self.release_user_quota(&sender_id, accounted);
				self.current_size.fetch_sub(accounted, Ordering::Relaxed);
				return Err(HopError::DuplicateEntry);
			}
			if let Err(e) = Self::write_atomic(&meta_path, &meta_bytes) {
				// Index doesn't contain this hash; remove the blob to avoid
				// leaving an orphan.
				let _ = fs::remove_file(&blob_path);
				drop(index);
				self.release_user_quota(&sender_id, accounted);
				self.current_size.fetch_sub(accounted, Ordering::Relaxed);
				return Err(e);
			}
			index.insert(hash, meta);
			self.publish_size_metrics(index.len());
		}
		self.metrics.record_inserted_bytes(accounted);

		tracing::info!(
			target: "hop",
			hash = ?hex::encode(hash),
			size = data_len,
			accounted,
			expires_at,
			"Data added to HOP pool"
		);

		Ok(hash)
	}

	/// Read a blob from disk and verify its content hash.
	///
	/// Content addressing means `blake2_256(data) == *hash` is an invariant
	/// — corruption (bit rot, partial write, local tampering) violates it.
	/// On integrity failure the caller-facing result is the same as a missing
	/// blob and the broken entry is purged so subsequent reads converge.
	fn read_and_verify_blob(&self, hash: &HopHash) -> Result<Vec<u8>, HopError> {
		let blob_path = self.blob_path(hash);
		let data = fs::read(&blob_path).map_err(|e| {
			if e.kind() == std::io::ErrorKind::NotFound {
				HopError::NotFound
			} else {
				HopError::IoError(e)
			}
		})?;
		if H256(blake2_256(&data)) != *hash {
			tracing::error!(
				target: "hop",
				hash = ?hex::encode(hash),
				size = data.len(),
				"Blob integrity check failed; purging entry"
			);
			self.purge_corrupt_entry(hash);
			return Err(HopError::NotFound);
		}
		Ok(data)
	}

	/// Remove a corrupt entry from the index and best-effort delete its files.
	/// The accounted size is released back to the pool and the user quota.
	fn purge_corrupt_entry(&self, hash: &HopHash) {
		let removed = {
			let mut index = self.index.lock();
			index.remove(hash).map(|meta| {
				let accounted = entry_accounted_size(meta.size, meta.recipients.len());
				self.current_size.fetch_sub(accounted, Ordering::Relaxed);
				self.publish_size_metrics(index.len());
				(meta.sender_id, accounted)
			})
		};
		if let Some((sender_id, accounted)) = removed {
			self.release_user_quota(&sender_id, accounted);
			self.metrics.record_removed(removal_reasons::CORRUPT, 1);
		}
		let _ = fs::remove_file(self.blob_path(hash));
		let _ = fs::remove_file(self.meta_path(hash));
	}

	/// Read and verify a blob, returning `None` for missing entries and logging
	/// any other failure. Shared by [`Self::get`] and [`Self::get_with_auth`].
	fn read_or_log(&self, hash: &HopHash) -> Option<Vec<u8>> {
		match self.read_and_verify_blob(hash) {
			Ok(data) => Some(data),
			Err(HopError::NotFound) => None,
			Err(e) => {
				tracing::error!(
					target: "hop",
					hash = ?hex::encode(hash),
					error = ?e,
					"Failed to read blob from disk"
				);
				None
			},
		}
	}

	/// Get data from the pool by content hash.
	pub fn get(&self, hash: &HopHash) -> Option<Vec<u8>> {
		{
			let index = self.index.lock();
			if !index.contains_key(hash) {
				return None;
			}
		}
		self.read_or_log(hash)
	}

	/// Get data alongside the submitter's `MultiSigner`, `hop_submit` signature,
	/// and submit timestamp.
	///
	/// Used by the promoter so the unsigned promotion extrinsic can carry the
	/// user's submit-time signature for runtime-side verification.
	pub fn get_with_auth(
		&self,
		hash: &HopHash,
	) -> Option<(Vec<u8>, MultiSigner, MultiSignature, u64)> {
		let (signer, signature, submit_timestamp) = {
			let index = self.index.lock();
			let meta = index.get(hash)?;
			(meta.signer.clone(), meta.signature.clone(), meta.submit_timestamp)
		};
		let data = self.read_or_log(hash)?;
		Some((data, signer, signature, submit_timestamp))
	}

	/// Decode `signature` and return the index of the matching recipient in
	/// `meta.recipients`. `context` is the operation's domain separator (claim
	/// / ack). Returning an index keeps a single implementation for both
	/// shared- and exclusive-borrow callers (`meta.recipients[idx]` works in
	/// either case).
	fn find_recipient_idx(
		meta: &HopEntryMeta,
		hash: &HopHash,
		signature: &[u8],
		context: &[u8],
	) -> Result<usize, HopError> {
		let multi_sig =
			MultiSignature::decode(&mut &signature[..]).map_err(|_| HopError::InvalidSignature)?;
		let payload = signing_payload(context, hash);

		meta.recipients
			.iter()
			.position(|r| multi_sig.verify(&payload[..], &r.signer.clone().into_account()))
			.ok_or(HopError::NotRecipient)
	}

	/// Claim data from the pool (read-only). Verifies the signature against recipient
	/// public keys. Returns the data if the signature matches a recipient.
	///
	/// This does NOT mark the recipient as claimed — call `ack` after receiving the data
	/// to confirm receipt.
	///
	/// Returns `AlreadyClaimed` if the recipient has already acked (data may be deleted).
	pub fn claim(&self, hash: &HopHash, signature: &[u8]) -> Result<Vec<u8>, HopError> {
		{
			let index = self.index.lock();
			let meta = index.get(hash).ok_or(HopError::NotFound)?;
			// Map NotRecipient → NotFound so callers cannot probe whether a hash
			// exists by observing different error codes.
			let idx = Self::find_recipient_idx(meta, hash, signature, HOP_CLAIM_CONTEXT)
				.map_err(|_| HopError::NotFound)?;

			// If this recipient already acked, the data may be gone.
			if meta.recipients[idx].claimed {
				return Err(HopError::AlreadyClaimed);
			}
		}
		// Read blob from disk and verify its content hash. May be gone if
		// concurrently acked and deleted, in which case we surface NotFound.
		self.read_and_verify_blob(hash)
	}

	/// Acknowledge receipt of claimed data. Marks the recipient as claimed and triggers
	/// cleanup when all recipients have acked.
	///
	/// Idempotent: acking a recipient that already acked returns `Ok(())`.
	pub fn ack(&self, hash: &HopHash, signature: &[u8]) -> Result<(), HopError> {
		// Phase 1: idempotent fast path under read lock.
		{
			let index = self.index.lock();
			let meta = index.get(hash).ok_or(HopError::NotFound)?;
			let idx = Self::find_recipient_idx(meta, hash, signature, HOP_ACK_CONTEXT)
				.map_err(|_| HopError::NotFound)?;
			if meta.recipients[idx].claimed {
				return Ok(());
			}
		}

		// Phase 2: re-run the lookup against the current meta — the entry could
		// have been removed and re-submitted with a different recipient list since Phase 1.
		let mut index = self.index.lock();
		let meta = index.get_mut(hash).ok_or(HopError::NotFound)?;
		let idx = Self::find_recipient_idx(meta, hash, signature, HOP_ACK_CONTEXT)
			.map_err(|_| HopError::NotFound)?;

		if meta.recipients[idx].claimed {
			return Ok(());
		}

		meta.recipients[idx].claimed = true;

		// If all recipients have acked, remove the entry entirely.
		if meta.recipients.iter().all(|r| r.claimed) {
			let accounted = entry_accounted_size(meta.size, meta.recipients.len());
			let sender = meta.sender_id;
			index.remove(hash);
			self.current_size.fetch_sub(accounted, Ordering::Relaxed);
			self.publish_size_metrics(index.len());
			self.release_user_quota(&sender, accounted);
			drop(index);
			self.metrics.record_removed(removal_reasons::ACKED, 1);

			// Delete files from disk (best-effort; orphans cleaned on restart).
			let _ = fs::remove_file(self.blob_path(hash));
			let _ = fs::remove_file(self.meta_path(hash));

			tracing::info!(
				target: "hop",
				hash = ?hex::encode(hash),
				"All recipients acked, data removed"
			);
		} else {
			let claimed_count = meta.recipients.iter().filter(|r| r.claimed).count();
			// Persist updated claimed state to disk.
			let meta_bytes = meta.encode();
			let meta_path = self.meta_path(hash);
			if let Err(e) = Self::write_atomic(&meta_path, &meta_bytes) {
				tracing::error!(target: "hop", hash = ?hex::encode(hash), error = %e, "Failed to persist ack state");
			}
			drop(index);

			tracing::debug!(
				target: "hop",
				hash = ?hex::encode(hash),
				claimed = claimed_count,
				"Recipient acked"
			);
		}

		Ok(())
	}

	/// Check if data exists in the pool.
	#[cfg(test)]
	pub fn has(&self, hash: &HopHash) -> bool {
		let index = self.index.lock();
		index.contains_key(hash)
	}

	/// Remove data from the pool.
	#[cfg(test)]
	pub fn remove(&self, hash: &HopHash) -> Result<(), HopError> {
		let meta = {
			let mut index = self.index.lock();
			index.remove(hash)
		};

		if let Some(meta) = meta {
			let accounted = entry_accounted_size(meta.size, meta.recipients.len());
			self.current_size.fetch_sub(accounted, Ordering::Relaxed);
			self.release_user_quota(&meta.sender_id, accounted);

			// Delete files from disk (best-effort).
			let _ = fs::remove_file(self.blob_path(hash));
			let _ = fs::remove_file(self.meta_path(hash));

			tracing::debug!(
				target: "hop",
				hash = ?hex::encode(hash),
				"Data removed from pool"
			);

			Ok(())
		} else {
			Err(HopError::NotFound)
		}
	}

	/// Get pool status.
	pub fn status(&self) -> PoolStatus {
		let index = self.index.lock();
		PoolStatus {
			entry_count: index.len(),
			total_bytes: self.current_size.load(Ordering::Relaxed),
			max_bytes: self.max_size,
		}
	}

	/// Remove expired entries and release their user quotas.
	/// Returns the total bytes freed.
	///
	/// Processes entries in bounded batches to keep the index write lock from
	/// being held across the full HashMap on huge pools. After all batches the
	/// per-sender `user_usage` map is GC'd in a single pass.
	///
	/// `promotion_buffer_secs` does not affect what is cleaned up. As in
	/// [`Self::get_promotable`] the caller owns the promotion window; here it
	/// only scopes the backlog gauge, snapshot from the phase-4 pass.
	pub fn cleanup_expired(&self, promotion_buffer_secs: u64) -> u64 {
		const CLEANUP_BATCH_SIZE: usize = 10_000;
		let mut total_freed: u64 = 0;
		let now_secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();

		loop {
			// Phase 1: Under index write lock — collect and remove up to one
			// batch of expired entries. Bounded so the lock hold scales with
			// batch size, not pool size.
			let expired: Vec<(HopHash, HopEntryMeta)> = {
				let mut index = self.index.lock();
				let expired_keys: Vec<HopHash> = index
					.iter()
					.filter(|(_, m)| now_secs >= m.expires_at)
					.map(|(h, _)| *h)
					.take(CLEANUP_BATCH_SIZE)
					.collect();

				expired_keys
					.into_iter()
					.filter_map(|hash| index.remove(&hash).map(|meta| (hash, meta)))
					.collect()
			};

			if expired.is_empty() {
				break;
			}

			// Phase 2: Update counters and batch user-quota release. Entries
			// expiring unpromoted are the data-loss case; count them separately.
			let mut freed = 0u64;
			let mut promoted = 0u64;
			let mut unpromoted = 0u64;
			for (_, meta) in &expired {
				freed =
					freed.saturating_add(entry_accounted_size(meta.size, meta.recipients.len()));
				if meta.promoted {
					promoted = promoted.saturating_add(1);
				} else {
					unpromoted = unpromoted.saturating_add(1);
				}
			}
			self.current_size.fetch_sub(freed, Ordering::Relaxed);
			total_freed = total_freed.saturating_add(freed);
			self.metrics.record_removed(removal_reasons::EXPIRED_PROMOTED, promoted);
			self.metrics.record_removed(removal_reasons::EXPIRED_UNPROMOTED, unpromoted);

			{
				let usage = self.user_usage.read();
				for (_, meta) in &expired {
					if let Some(counter) = usage.get(&meta.sender_id) {
						let accounted = entry_accounted_size(meta.size, meta.recipients.len());
						saturating_release(counter, accounted);
					}
				}
			}

			// Phase 3: Delete files from disk (best-effort, no locks held).
			for (hash, _) in &expired {
				let _ = fs::remove_file(self.blob_path(hash));
				let _ = fs::remove_file(self.meta_path(hash));
			}
		}

		// Phase 4: Reclaim per-sender counters whose owners have no live
		// entries. Holding `index.lock()` and `user_usage.write()` together
		// closes the dominant TOCTOU race (concurrent writers cannot create a
		// new index entry under our held index lock; concurrent
		// `release_user_quota` only takes `user_usage.read()` which is
		// excluded). Build a live-sender set in one index pass so retain is
		// O(senders + entries) instead of O(senders × entries). The same pass
		// counts the promotion backlog and publishes the size gauges.
		let backlog = {
			let index = self.index.lock();
			let mut usage = self.user_usage.write();
			let mut live: HashSet<&SenderId> = HashSet::new();
			let mut backlog = 0u64;
			// The window test exists only to feed a gauge.
			let count_backlog = self.metrics.is_enabled();
			for meta in index.values() {
				live.insert(&meta.sender_id);
				if count_backlog && Self::in_promotion_window(meta, now_secs, promotion_buffer_secs)
				{
					backlog = backlog.saturating_add(1);
				}
			}
			usage.retain(|sender_id, counter| {
				counter.load(Ordering::Relaxed) > 0 || live.contains(sender_id)
			});
			self.publish_size_metrics(index.len());
			backlog
		};

		// Let the rate limiter shed stale per-sender state on the same cadence.
		self.rate_limiter.evict_stale();

		self.metrics.set_promotion_backlog(backlog);

		total_freed
	}

	/// Outstanding promotion candidate: unpromoted, near expiry, attempts left.
	/// Ignores the back-off deadline — a backing-off entry is still outstanding.
	fn in_promotion_window(meta: &HopEntryMeta, now_secs: u64, buffer_secs: u64) -> bool {
		!meta.promoted &&
			now_secs.saturating_add(buffer_secs) >= meta.expires_at &&
			meta.promotion_attempts < MAX_PROMOTION_ATTEMPTS
	}

	/// Return hashes of entries within `buffer_secs` of expiry that have not yet been promoted.
	/// Returns up to `limit` hashes. Use [`Self::get`] to read blob data when needed.
	/// The maintenance task runs periodically, so remaining entries are picked up next cycle.
	pub fn get_promotable(
		&self,
		current_block: HopBlockNumber,
		buffer_secs: u64,
		limit: usize,
	) -> Vec<HopHash> {
		let now_secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
		let index = self.index.lock();
		// `take(limit)` stays lazy: this holds the index lock, so a full scan
		// would block inserts and acks. The backlog gauge is snapshot in
		// `cleanup_expired` instead.
		index
			.iter()
			.filter(|(_, meta)| {
				Self::in_promotion_window(meta, now_secs, buffer_secs) &&
					current_block >= meta.next_promotion_attempt_at
			})
			.map(|(h, _)| *h)
			.take(limit)
			.collect()
	}

	/// Mark an entry as promoted to permanent on-chain storage.
	/// Persists the updated metadata to disk.
	pub fn mark_promoted(&self, hash: &HopHash) {
		let mut index = self.index.lock();
		if let Some(meta) = index.get_mut(hash) {
			// Count the transition, not the call: this setter is idempotent.
			let newly_promoted = !meta.promoted;
			meta.promoted = true;
			let meta_bytes = meta.encode();
			let meta_path = self.meta_path(hash);
			drop(index);
			if newly_promoted {
				self.metrics.record_promotion_confirmed();
			}

			if let Err(e) = Self::write_atomic(&meta_path, &meta_bytes) {
				tracing::error!(
					target: "hop",
					hash = ?hex::encode(hash),
					error = %e,
					"Failed to persist promoted state"
				);
			}
		}
	}

	/// Record a promotion attempt: bumps the per-entry attempt counter and
	/// schedules the next eligible block via exponential back-off. The
	/// maintenance task will skip the entry until then. Once
	/// `MAX_PROMOTION_ATTEMPTS` is reached the entry is left to expire.
	///
	/// Called on **both** an `Err` from `submit_local` (the tx pool rejected
	/// us) and an `Ok` followed by a runtime check that the data is not yet
	/// on-chain (the tx was accepted into the pool but never included). The
	/// backoff schedule is identical for both cases.
	pub fn record_promotion_attempt(
		&self,
		hash: &HopHash,
		current_block: HopBlockNumber,
		check_interval_blocks: u32,
	) {
		let mut index = self.index.lock();
		if let Some(meta) = index.get_mut(hash) {
			meta.promotion_attempts = meta.promotion_attempts.saturating_add(1);
			let backoff = promotion_backoff_blocks(meta.promotion_attempts, check_interval_blocks);
			meta.next_promotion_attempt_at = current_block.saturating_add(backoff);
			let meta_bytes = meta.encode();
			let meta_path = self.meta_path(hash);
			drop(index);

			if let Err(e) = Self::write_atomic(&meta_path, &meta_bytes) {
				tracing::error!(
					target: "hop",
					hash = ?hex::encode(hash),
					error = %e,
					"Failed to persist promotion-attempt state"
				);
			}
		}
	}
}

/// Decode a 64-char hex stem into a `HopHash`. Returns `None` for any
/// non-32-byte stem (corrupt name, wrong length, non-hex chars).
fn parse_hex_hash(stem: &str) -> Option<HopHash> {
	let bytes = hex::decode(stem).ok()?;
	let arr: [u8; 32] = bytes.try_into().ok()?;
	Some(H256(arr))
}

/// Atomically subtract `accounted` from `counter`, clamped so the counter
/// cannot underflow. The CAS retry inside `fetch_update` keeps the clamp
/// value fresh — a plain `counter.fetch_sub(accounted.min(counter.load()), …)`
/// would race with concurrent releases on the same counter and could wrap
/// to near `u64::MAX`.
fn saturating_release(counter: &AtomicU64, accounted: u64) {
	let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |previous| {
		Some(previous - accounted.min(previous))
	});
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::types::{Recipient, MAX_RECIPIENTS};
	use sp_core::{crypto::Pair, ed25519, sr25519};
	use sp_runtime::MultiSigner;
	use tempfile::TempDir;

	const SENDER_A: SenderId = [1u8; 32];
	const SENDER_B: SenderId = [2u8; 32];

	/// Accounted cost of an entry with `data_size` bytes and `num_recipients` recipients.
	fn acct(data_size: u64, num_recipients: usize) -> u64 {
		entry_accounted_size(data_size, num_recipients)
	}

	fn make_pool(max_size: u64, retention_secs: u64) -> (HopDataPool, TempDir) {
		let dir = TempDir::new().unwrap();
		let pool = HopDataPool::new(
			max_size,
			max_size,
			retention_secs,
			dir.path().to_path_buf(),
			RateLimitConfig::disabled(),
			HopMetrics::disabled(),
		)
		.unwrap();
		(pool, dir)
	}

	fn make_pool_with_user_cap(
		max_size: u64,
		max_user_size: u64,
		retention_secs: u64,
	) -> (HopDataPool, TempDir) {
		let dir = TempDir::new().unwrap();
		let pool = HopDataPool::new(
			max_size,
			max_user_size,
			retention_secs,
			dir.path().to_path_buf(),
			RateLimitConfig::disabled(),
			HopMetrics::disabled(),
		)
		.unwrap();
		(pool, dir)
	}

	fn create_test_pool() -> (HopDataPool, TempDir) {
		make_pool(1024 * 1024, 100)
	}

	fn test_recipient() -> (ed25519::Pair, MultiSigner) {
		let pair = ed25519::Pair::from_seed(&[1u8; 32]);
		let signer = MultiSigner::Ed25519(pair.public());
		(pair, signer)
	}

	/// Deterministic placeholder `(MultiSigner, MultiSignature)` for tests that
	/// don't exercise submit-signature semantics. The actual values are never
	/// verified by these tests.
	fn dummy_auth() -> (MultiSigner, MultiSignature) {
		let pair = ed25519::Pair::from_seed(&[7u8; 32]);
		let signer = MultiSigner::Ed25519(pair.public());
		let sig = MultiSignature::Ed25519(pair.sign(&[]));
		(signer, sig)
	}

	fn sign_ed(pair: &ed25519::Pair, context: &[u8], hash: &HopHash) -> Vec<u8> {
		let payload = signing_payload(context, hash);
		MultiSignature::Ed25519(pair.sign(&payload)).encode()
	}

	fn sign_sr(pair: &sr25519::Pair, context: &[u8], hash: &HopHash) -> Vec<u8> {
		let payload = signing_payload(context, hash);
		MultiSignature::Sr25519(pair.sign(&payload)).encode()
	}

	fn user_usage(pool: &HopDataPool, sender: &SenderId) -> u64 {
		pool.user_usage
			.read()
			.get(sender)
			.map(|c| c.load(Ordering::Relaxed))
			.unwrap_or(0)
	}

	/// Convert a `Vec<MultiSigner>` into a `RecipientVec` (with `claimed=false` for
	/// each) for test ergonomics; panics only if a test exceeds `MAX_RECIPIENTS`.
	fn bv(v: Vec<MultiSigner>) -> RecipientVec {
		let recipients: Vec<Recipient> =
			v.into_iter().map(|signer| Recipient { signer, claimed: false }).collect();
		RecipientVec::try_from(recipients).expect("test recipient list exceeds MAX_RECIPIENTS")
	}

	#[test]
	fn metrics_track_insert_ack_and_expiry() {
		let registry = prometheus_endpoint::Registry::new();
		let dir = TempDir::new().unwrap();
		let pool = HopDataPool::new(
			1024 * 1024,
			1024 * 1024,
			0, // entries expire immediately
			dir.path().to_path_buf(),
			RateLimitConfig::disabled(),
			HopMetrics::new(Some(&registry)).unwrap(),
		)
		.unwrap();
		let (pair, signer) = test_recipient();

		// Insert + full ack removes the entry under `acked`.
		let hash = pool
			.insert(
				vec![1u8; 50],
				bv(vec![signer.clone()]),
				SENDER_A,
				dummy_auth().0,
				dummy_auth().1,
				0,
			)
			.unwrap();
		assert_eq!(pool.metrics().pool_gauges(), (1, acct(50, 1)));
		let ack = sign_ed(&pair, HOP_ACK_CONTEXT, &hash);
		pool.ack(&hash, &ack).unwrap();
		assert_eq!(pool.metrics().removed_count(removal_reasons::ACKED), 1);
		assert_eq!(pool.metrics().pool_gauges(), (0, 0));

		// An entry expiring without promotion counts as data loss.
		pool.insert(vec![2u8; 30], bv(vec![signer]), SENDER_A, dummy_auth().0, dummy_auth().1, 0)
			.unwrap();
		pool.cleanup_expired(0);
		assert_eq!(pool.metrics().removed_count(removal_reasons::EXPIRED_UNPROMOTED), 1);
		assert_eq!(pool.metrics().removed_count(removal_reasons::EXPIRED_PROMOTED), 0);
		assert_eq!(pool.metrics().pool_gauges(), (0, 0));
	}

	/// Pool with registered metrics, so counters can be read back.
	fn make_metered_pool(retention_secs: u64) -> (HopDataPool, TempDir) {
		let registry = prometheus_endpoint::Registry::new();
		let dir = TempDir::new().unwrap();
		let pool = HopDataPool::new(
			1024 * 1024,
			1024 * 1024,
			retention_secs,
			dir.path().to_path_buf(),
			RateLimitConfig::disabled(),
			HopMetrics::new(Some(&registry)).unwrap(),
		)
		.unwrap();
		(pool, dir)
	}

	#[test]
	fn backlog_gauge_counts_backing_off_entries() {
		let (pool, _dir) = make_metered_pool(/* retention = */ 100);
		let (_, signer) = test_recipient();
		let buffer = 300_u64;
		let hash = pool
			.insert(vec![1u8; 10], bv(vec![signer]), SENDER_A, dummy_auth().0, dummy_auth().1, 0)
			.unwrap();

		// Outside the window with a buffer of 0, inside it with 300s.
		pool.cleanup_expired(0);
		assert_eq!(pool.metrics().promotion_backlog(), 0);

		pool.cleanup_expired(buffer);
		assert_eq!(pool.metrics().promotion_backlog(), 1);

		// Backing off drops it from `get_promotable` but not from the backlog.
		pool.record_promotion_attempt(&hash, 60, /* check_interval_blocks = */ 10);
		assert!(pool.get_promotable(60, buffer, 10).is_empty());
		pool.cleanup_expired(buffer);
		assert_eq!(pool.metrics().promotion_backlog(), 1);

		pool.mark_promoted(&hash);
		pool.cleanup_expired(buffer);
		assert_eq!(pool.metrics().promotion_backlog(), 0);
	}

	#[test]
	fn confirmed_counter_ignores_repeated_mark_promoted() {
		let (pool, _dir) = make_metered_pool(100);
		let (_, signer) = test_recipient();
		let hash = pool
			.insert(vec![1u8; 10], bv(vec![signer]), SENDER_A, dummy_auth().0, dummy_auth().1, 0)
			.unwrap();

		pool.mark_promoted(&hash);
		pool.mark_promoted(&hash);
		assert_eq!(pool.metrics().promotions_confirmed(), 1, "only the transition counts");
	}

	#[test]
	fn test_insert_and_get() {
		let (pool, _dir) = create_test_pool();
		let (_, signer) = test_recipient();
		let data = vec![1, 2, 3, 4, 5];
		let hash = pool
			.insert(data.clone(), bv(vec![signer]), SENDER_A, dummy_auth().0, dummy_auth().1, 0)
			.unwrap();

		let retrieved = pool.get(&hash).unwrap();
		assert_eq!(data, retrieved);
	}

	#[test]
	fn test_insert_no_recipients() {
		let (pool, _dir) = create_test_pool();
		let data = vec![1, 2, 3, 4, 5];
		let result = pool.insert(data, bv(vec![]), SENDER_A, dummy_auth().0, dummy_auth().1, 0);
		assert!(matches!(result, Err(HopError::NoRecipients)));
	}

	#[test]
	fn test_duplicate_insert() {
		let (pool, _dir) = create_test_pool();
		let (_, signer) = test_recipient();
		let data = vec![1, 2, 3, 4, 5];

		pool.insert(
			data.clone(),
			bv(vec![signer.clone()]),
			SENDER_A,
			dummy_auth().0,
			dummy_auth().1,
			0,
		)
		.unwrap();
		let result =
			pool.insert(data, bv(vec![signer]), SENDER_A, dummy_auth().0, dummy_auth().1, 0);

		assert!(matches!(result, Err(HopError::DuplicateEntry)));
	}

	#[test]
	fn test_too_many_recipients_rejected_at_type_level() {
		// Construction of a `RecipientVec` with more than `MAX_RECIPIENTS` entries
		// fails at `try_from`; callers (like the RPC) turn that into a
		// `TooManyRecipients` error before reaching the pool.
		let recipients: Vec<Recipient> = (0..=MAX_RECIPIENTS as u64)
			.map(|i| {
				let mut seed = [0u8; 32];
				seed[..8].copy_from_slice(&i.to_le_bytes());
				Recipient {
					signer: MultiSigner::Ed25519(ed25519::Pair::from_seed(&seed).public()),
					claimed: false,
				}
			})
			.collect();
		assert_eq!(recipients.len(), MAX_RECIPIENTS as usize + 1);
		assert!(RecipientVec::try_from(recipients).is_err());
	}

	#[test]
	fn test_duplicate_recipient_rejected() {
		let (pool, _dir) = create_test_pool();
		let (_, signer) = test_recipient();
		let result = pool.insert(
			vec![1, 2, 3],
			bv(vec![signer.clone(), signer]),
			SENDER_A,
			dummy_auth().0,
			dummy_auth().1,
			0,
		);
		assert!(matches!(result, Err(HopError::DuplicateRecipient)));
	}

	#[test]
	fn test_pool_full() {
		// Capacity exactly holds one 60-byte entry with one recipient (60 + 40 = 100).
		let (pool, _dir) = make_pool(acct(60, 1), 100);
		let (_, signer) = test_recipient();

		let data1 = vec![0u8; 60];
		let data2 = vec![1u8; 50];

		pool.insert(data1, bv(vec![signer.clone()]), SENDER_A, dummy_auth().0, dummy_auth().1, 0)
			.unwrap();
		let result =
			pool.insert(data2, bv(vec![signer]), SENDER_A, dummy_auth().0, dummy_auth().1, 0);

		assert!(matches!(result, Err(HopError::PoolFull(_, _))));
	}

	#[test]
	fn test_remove() {
		let (pool, _dir) = create_test_pool();
		let (_, signer) = test_recipient();
		let data = vec![1, 2, 3, 4, 5];
		let hash = pool
			.insert(data, bv(vec![signer]), SENDER_A, dummy_auth().0, dummy_auth().1, 0)
			.unwrap();

		assert!(pool.has(&hash));
		pool.remove(&hash).unwrap();
		assert!(!pool.has(&hash));

		// Files should be cleaned up.
		assert!(!pool.blob_path(&hash).exists());
		assert!(!pool.meta_path(&hash).exists());
	}

	#[test]
	fn test_status() {
		let (pool, _dir) = create_test_pool();
		let (_, signer) = test_recipient();
		let data1 = vec![1, 2, 3, 4, 5];
		let data2 = vec![6, 7, 8];

		pool.insert(
			data1.clone(),
			bv(vec![signer.clone()]),
			SENDER_A,
			dummy_auth().0,
			dummy_auth().1,
			0,
		)
		.unwrap();
		pool.insert(data2.clone(), bv(vec![signer]), SENDER_A, dummy_auth().0, dummy_auth().1, 0)
			.unwrap();

		let status = pool.status();
		assert_eq!(status.entry_count, 2);
		assert_eq!(status.total_bytes, acct(data1.len() as u64, 1) + acct(data2.len() as u64, 1));
	}

	#[test]
	fn test_claim_valid_signature() {
		let (pool, _dir) = create_test_pool();
		let (pair, signer) = test_recipient();
		let data = vec![1, 2, 3, 4, 5];
		let hash = pool
			.insert(data.clone(), bv(vec![signer]), SENDER_A, dummy_auth().0, dummy_auth().1, 0)
			.unwrap();

		let claim = sign_ed(&pair, HOP_CLAIM_CONTEXT, &hash);
		let ack = sign_ed(&pair, HOP_ACK_CONTEXT, &hash);
		let result = pool.claim(&hash, &claim).unwrap();
		assert_eq!(data, result);

		// Entry still exists until ack.
		assert!(pool.has(&hash));

		pool.ack(&hash, &ack).unwrap();
		assert!(!pool.has(&hash));
	}

	#[test]
	fn test_claim_sig_rejected_on_ack() {
		// Domain separation: a claim signature cannot be replayed as an ack.
		let (pool, _dir) = create_test_pool();
		let (pair, signer) = test_recipient();
		let hash = pool
			.insert(vec![1, 2, 3], bv(vec![signer]), SENDER_A, dummy_auth().0, dummy_auth().1, 0)
			.unwrap();

		let claim = sign_ed(&pair, HOP_CLAIM_CONTEXT, &hash);
		pool.claim(&hash, &claim).unwrap();
		assert!(matches!(pool.ack(&hash, &claim), Err(HopError::NotFound)));
	}

	#[test]
	fn test_claim_invalid_signature() {
		let (pool, _dir) = create_test_pool();
		let (_, signer) = test_recipient();
		let data = vec![1, 2, 3, 4, 5];
		let hash = pool
			.insert(data, bv(vec![signer]), SENDER_A, dummy_auth().0, dummy_auth().1, 0)
			.unwrap();

		// Use invalid SCALE bytes — cannot decode as MultiSignature
		let result = pool.claim(&hash, &[0u8; 3]);
		assert!(matches!(result, Err(HopError::NotFound)));
	}

	#[test]
	fn test_claim_wrong_key() {
		let (pool, _dir) = create_test_pool();
		let (_, signer) = test_recipient();
		let hash = pool
			.insert(
				vec![1, 2, 3, 4, 5],
				bv(vec![signer]),
				SENDER_A,
				dummy_auth().0,
				dummy_auth().1,
				0,
			)
			.unwrap();

		let wrong_pair = ed25519::Pair::from_seed(&[99u8; 32]);
		let wrong_claim = sign_ed(&wrong_pair, HOP_CLAIM_CONTEXT, &hash);
		assert!(matches!(pool.claim(&hash, &wrong_claim), Err(HopError::NotFound)));
		assert!(pool.has(&hash));
	}

	#[test]
	fn test_claim_multi_recipient() {
		let (pool, _dir) = create_test_pool();
		let pair1 = ed25519::Pair::from_seed(&[1u8; 32]);
		let pair2 = ed25519::Pair::from_seed(&[2u8; 32]);
		let signer1 = MultiSigner::Ed25519(pair1.public());
		let signer2 = MultiSigner::Ed25519(pair2.public());

		let data = vec![1, 2, 3, 4, 5];
		let hash = pool
			.insert(
				data.clone(),
				bv(vec![signer1, signer2]),
				SENDER_A,
				dummy_auth().0,
				dummy_auth().1,
				0,
			)
			.unwrap();

		let claim1 = sign_ed(&pair1, HOP_CLAIM_CONTEXT, &hash);
		let ack1 = sign_ed(&pair1, HOP_ACK_CONTEXT, &hash);
		assert_eq!(data, pool.claim(&hash, &claim1).unwrap());
		pool.ack(&hash, &ack1).unwrap();
		assert!(pool.has(&hash));

		let claim2 = sign_ed(&pair2, HOP_CLAIM_CONTEXT, &hash);
		let ack2 = sign_ed(&pair2, HOP_ACK_CONTEXT, &hash);
		assert_eq!(data, pool.claim(&hash, &claim2).unwrap());
		pool.ack(&hash, &ack2).unwrap();
		assert!(!pool.has(&hash));
		assert_eq!(pool.status().total_bytes, 0);
	}

	#[test]
	fn test_claim_after_ack_returns_already_claimed() {
		let (pool, _dir) = create_test_pool();
		let (pair, signer) = test_recipient();
		let pair2 = ed25519::Pair::from_seed(&[2u8; 32]);
		let signer2 = MultiSigner::Ed25519(pair2.public());

		let hash = pool
			.insert(
				vec![1, 2, 3, 4, 5],
				bv(vec![signer, signer2]),
				SENDER_A,
				dummy_auth().0,
				dummy_auth().1,
				0,
			)
			.unwrap();

		let claim = sign_ed(&pair, HOP_CLAIM_CONTEXT, &hash);
		let ack = sign_ed(&pair, HOP_ACK_CONTEXT, &hash);
		pool.claim(&hash, &claim).unwrap();
		pool.ack(&hash, &ack).unwrap();

		// Same recipient claims again — already acked.
		assert!(matches!(pool.claim(&hash, &claim), Err(HopError::AlreadyClaimed)));
	}

	#[test]
	fn test_claim_not_found() {
		let (pool, _dir) = create_test_pool();
		let fake_hash = H256([0u8; 32]);
		let result = pool.claim(&fake_hash, &[0u8; 64]);
		assert!(matches!(result, Err(HopError::NotFound)));
	}

	#[test]
	fn test_per_user_cap_is_hard_limit() {
		// Pool big enough for multiple users; user cap sized to one 60-byte entry (+ metadata).
		let (pool, _dir) = make_pool_with_user_cap(10_000, acct(60, 1), 100);
		let (_, signer) = test_recipient();

		pool.insert(
			vec![0u8; 60],
			bv(vec![signer.clone()]),
			SENDER_A,
			dummy_auth().0,
			dummy_auth().1,
			0,
		)
		.unwrap();

		// User A is at the cap; next insert is rejected regardless of pool headroom.
		let result = pool.insert(
			vec![1u8; 10],
			bv(vec![signer.clone()]),
			SENDER_A,
			dummy_auth().0,
			dummy_auth().1,
			0,
		);
		assert!(matches!(result, Err(HopError::UserQuotaExceeded { .. })));

		// User B has their own independent cap.
		pool.insert(vec![2u8; 60], bv(vec![signer]), SENDER_B, dummy_auth().0, dummy_auth().1, 0)
			.unwrap();
	}

	#[test]
	fn test_quota_released_after_ack() {
		let (pool, _dir) = make_pool_with_user_cap(10_000, acct(100, 1), 100);
		let (pair, signer) = test_recipient();

		let hash = pool
			.insert(
				vec![0u8; 100],
				bv(vec![signer.clone()]),
				SENDER_A,
				dummy_auth().0,
				dummy_auth().1,
				0,
			)
			.unwrap();

		// At cap; next insert rejected.
		let result = pool.insert(
			vec![1u8; 10],
			bv(vec![signer.clone()]),
			SENDER_A,
			dummy_auth().0,
			dummy_auth().1,
			0,
		);
		assert!(matches!(result, Err(HopError::UserQuotaExceeded { .. })));

		let claim = sign_ed(&pair, HOP_CLAIM_CONTEXT, &hash);
		let ack = sign_ed(&pair, HOP_ACK_CONTEXT, &hash);
		pool.claim(&hash, &claim).unwrap();
		pool.ack(&hash, &ack).unwrap();

		// Quota freed — user can insert again.
		pool.insert(vec![2u8; 100], bv(vec![signer]), SENDER_A, dummy_auth().0, dummy_auth().1, 0)
			.unwrap();
	}

	#[test]
	fn test_cleanup_expired_releases_quota() {
		let (pool, _dir) = make_pool(10_000, 0);
		let (_, signer) = test_recipient();

		pool.insert(vec![0u8; 100], bv(vec![signer]), SENDER_A, dummy_auth().0, dummy_auth().1, 0)
			.unwrap();
		let charged = acct(100, 1);
		assert_eq!(user_usage(&pool, &SENDER_A), charged);

		let freed = pool.cleanup_expired(0);
		assert_eq!(freed, charged);
		assert_eq!(pool.status().total_bytes, 0);
		assert_eq!(user_usage(&pool, &SENDER_A), 0);
	}

	#[test]
	fn test_cleanup_expired_honors_wall_clock_retention() {
		// Retention is measured in real seconds, not blocks: insert with a 1 s
		// retention, sleep past it, and assert cleanup reaps the entry.
		let (pool, _dir) = make_pool(10_000, 1);
		let (_, signer) = test_recipient();

		let hash = pool
			.insert(vec![0u8; 100], bv(vec![signer]), SENDER_A, dummy_auth().0, dummy_auth().1, 0)
			.unwrap();

		// Not yet expired — cleanup must be a no-op.
		assert_eq!(
			pool.cleanup_expired(0),
			0,
			"entry should still be live before retention elapses"
		);
		assert!(pool.has(&hash));

		std::thread::sleep(std::time::Duration::from_millis(1_200));

		assert!(
			pool.cleanup_expired(0) > 0,
			"entry should be reaped once wall-clock retention elapses"
		);
		assert!(!pool.has(&hash));
	}

	#[test]
	fn test_user_counter_preserved_until_cleanup() {
		// release_user_quota does not remove the map entry — only cleanup_expired
		// reclaims stale per-sender slots. Until then the slot remains at 0 so a
		// concurrent insert would not orphan its `Arc`.
		let (pool, _dir) = create_test_pool();
		let (pair, signer) = test_recipient();

		let hash = pool
			.insert(vec![0u8; 50], bv(vec![signer]), SENDER_A, dummy_auth().0, dummy_auth().1, 0)
			.unwrap();
		assert!(pool.user_usage.read().contains_key(&SENDER_A));

		let claim = sign_ed(&pair, HOP_CLAIM_CONTEXT, &hash);
		let ack = sign_ed(&pair, HOP_ACK_CONTEXT, &hash);
		pool.claim(&hash, &claim).unwrap();
		pool.ack(&hash, &ack).unwrap();

		assert_eq!(user_usage(&pool, &SENDER_A), 0);
		assert!(pool.user_usage.read().contains_key(&SENDER_A));
	}

	#[test]
	fn test_cleanup_expired_evicts_idle_user_counters() {
		// After cleanup_expired runs and a sender has no live entries with a
		// non-zero counter, their map slot must be removed so the map cannot
		// grow unbounded across the lifetime of a long-running node.
		let (pool, _dir) = make_pool(10_000, 10);
		let (pair, signer) = test_recipient();

		let hash = pool
			.insert(vec![0u8; 50], bv(vec![signer]), SENDER_A, dummy_auth().0, dummy_auth().1, 0)
			.unwrap();
		let claim = sign_ed(&pair, HOP_CLAIM_CONTEXT, &hash);
		let ack = sign_ed(&pair, HOP_ACK_CONTEXT, &hash);
		pool.claim(&hash, &claim).unwrap();
		pool.ack(&hash, &ack).unwrap();
		assert!(pool.user_usage.read().contains_key(&SENDER_A));

		pool.cleanup_expired(0);
		assert!(!pool.user_usage.read().contains_key(&SENDER_A));
	}

	#[test]
	fn test_cleanup_expired_keeps_active_user_counters() {
		// A sender with live (non-expired) entries must keep their counter
		// even when the counter dropped to 0 between submissions — otherwise
		// concurrent in-flight inserts could orphan their `Arc`.
		let (pool, _dir) = make_pool(10_000, 100);
		let (_, signer) = test_recipient();

		pool.insert(vec![0u8; 50], bv(vec![signer]), SENDER_A, dummy_auth().0, dummy_auth().1, 0)
			.unwrap();
		// Cleanup at a block where the entry is not yet expired must not
		// reclaim the sender's slot — a concurrent insert would otherwise
		// orphan its `Arc`.
		pool.cleanup_expired(0);
		assert!(pool.user_usage.read().contains_key(&SENDER_A));
	}

	#[test]
	fn test_cleanup_expired_processes_more_than_one_batch() {
		// Cleanup batch size is 10_000 — feed it 25_000 entries that all expire,
		// confirm every entry is removed (proving the loop terminates rather
		// than leaving leftovers from the first batch).
		const BATCHES: u32 = 2;
		const PER_BATCH: u32 = 10_000 + 1; // > one batch each
		let total = BATCHES * PER_BATCH;

		let dir = TempDir::new().unwrap();
		// Pool sized for ~25k tiny entries (4 bytes each + recipient overhead).
		let entry_bytes = std::mem::size_of::<u32>() as u64;
		let pool = HopDataPool::new(
			(acct(entry_bytes, 1) * total as u64) + 1024,
			u64::MAX,
			0,
			dir.path().to_path_buf(),
			RateLimitConfig::disabled(),
			HopMetrics::disabled(),
		)
		.unwrap();
		let (_, signer) = test_recipient();

		for i in 0..total {
			let mut sender = SENDER_A;
			sender[0] = (i & 0xff) as u8;
			sender[1] = ((i >> 8) & 0xff) as u8;
			sender[2] = ((i >> 16) & 0xff) as u8;
			// Data must be unique per entry — content-addressing means equal
			// bytes hash to the same key and the second insert hits
			// DuplicateEntry. Embed `i` so each blob is distinct.
			let data = i.to_le_bytes().to_vec();
			pool.insert(data, bv(vec![signer.clone()]), sender, dummy_auth().0, dummy_auth().1, 0)
				.unwrap();
		}
		assert_eq!(pool.status().entry_count, total as usize);

		pool.cleanup_expired(0);
		assert_eq!(pool.status().entry_count, 0);
		assert_eq!(pool.status().total_bytes, 0);
		assert!(pool.user_usage.read().is_empty());
	}

	#[test]
	fn test_restart_recovery() {
		let dir = TempDir::new().unwrap();
		let (_, signer) = test_recipient();
		let expected_accounted = acct(100, 1);

		let hash;
		{
			let pool = HopDataPool::new(
				1024 * 1024,
				1024 * 1024,
				100,
				dir.path().to_path_buf(),
				RateLimitConfig::disabled(),
				HopMetrics::disabled(),
			)
			.unwrap();
			hash = pool
				.insert(
					vec![42u8; 100],
					bv(vec![signer]),
					SENDER_A,
					dummy_auth().0,
					dummy_auth().1,
					0,
				)
				.unwrap();
			assert!(pool.has(&hash));
			assert_eq!(pool.status().entry_count, 1);
			assert_eq!(pool.status().total_bytes, expected_accounted);
		}

		{
			let pool = HopDataPool::new(
				1024 * 1024,
				1024 * 1024,
				100,
				dir.path().to_path_buf(),
				RateLimitConfig::disabled(),
				HopMetrics::disabled(),
			)
			.unwrap();
			assert!(pool.has(&hash));
			assert_eq!(pool.status().entry_count, 1);
			assert_eq!(pool.status().total_bytes, expected_accounted);

			let data = pool.get(&hash).unwrap();
			assert_eq!(data, vec![42u8; 100]);
			assert_eq!(user_usage(&pool, &SENDER_A), expected_accounted);
		}
	}

	#[test]
	fn test_orphan_blob_cleanup() {
		let dir = TempDir::new().unwrap();
		{
			let _pool = HopDataPool::new(
				1024 * 1024,
				1024 * 1024,
				100,
				dir.path().to_path_buf(),
				RateLimitConfig::disabled(),
				HopMetrics::disabled(),
			)
			.unwrap();
		}

		let orphan_hash = "aa".to_string() + &"bb".repeat(15);
		let blob_path = dir.path().join("blobs").join("aa").join(format!("{}.blob", orphan_hash));
		fs::write(&blob_path, b"orphan data").unwrap();
		assert!(blob_path.exists());

		let _pool = HopDataPool::new(
			1024 * 1024,
			1024 * 1024,
			100,
			dir.path().to_path_buf(),
			RateLimitConfig::disabled(),
			HopMetrics::disabled(),
		)
		.unwrap();
		assert!(!blob_path.exists());
	}

	#[test]
	fn test_corrupt_meta_cleanup() {
		let dir = TempDir::new().unwrap();
		{
			let _pool = HopDataPool::new(
				1024 * 1024,
				1024 * 1024,
				100,
				dir.path().to_path_buf(),
				RateLimitConfig::disabled(),
				HopMetrics::disabled(),
			)
			.unwrap();
		}

		let fake_hash = "bb".to_string() + &"cc".repeat(15);
		let meta_path = dir.path().join("meta").join("bb").join(format!("{}.meta", fake_hash));
		fs::write(&meta_path, b"not valid SCALE data").unwrap();
		assert!(meta_path.exists());

		let pool = HopDataPool::new(
			1024 * 1024,
			1024 * 1024,
			100,
			dir.path().to_path_buf(),
			RateLimitConfig::disabled(),
			HopMetrics::disabled(),
		)
		.unwrap();
		assert!(!meta_path.exists());
		assert_eq!(pool.status().entry_count, 0);
	}

	#[test]
	fn test_claim_sr25519() {
		let (pool, _dir) = create_test_pool();
		let pair = sr25519::Pair::from_seed(&[3u8; 32]);
		let signer = MultiSigner::Sr25519(pair.public());

		let data = vec![10, 20, 30];
		let hash = pool
			.insert(data.clone(), bv(vec![signer]), SENDER_A, dummy_auth().0, dummy_auth().1, 0)
			.unwrap();

		let claim = sign_sr(&pair, HOP_CLAIM_CONTEXT, &hash);
		let ack = sign_sr(&pair, HOP_ACK_CONTEXT, &hash);
		assert_eq!(data, pool.claim(&hash, &claim).unwrap());
		pool.ack(&hash, &ack).unwrap();
		assert!(!pool.has(&hash));
	}

	#[test]
	fn test_claim_mixed_key_types() {
		let (pool, _dir) = create_test_pool();
		let ed_pair = ed25519::Pair::from_seed(&[4u8; 32]);
		let sr_pair = sr25519::Pair::from_seed(&[5u8; 32]);
		let ed_signer = MultiSigner::Ed25519(ed_pair.public());
		let sr_signer = MultiSigner::Sr25519(sr_pair.public());

		let data = vec![42, 43, 44];
		let hash = pool
			.insert(
				data.clone(),
				bv(vec![ed_signer, sr_signer]),
				SENDER_A,
				dummy_auth().0,
				dummy_auth().1,
				0,
			)
			.unwrap();

		let sr_claim = sign_sr(&sr_pair, HOP_CLAIM_CONTEXT, &hash);
		let sr_ack = sign_sr(&sr_pair, HOP_ACK_CONTEXT, &hash);
		assert_eq!(data, pool.claim(&hash, &sr_claim).unwrap());
		pool.ack(&hash, &sr_ack).unwrap();
		assert!(pool.has(&hash));

		let ed_claim = sign_ed(&ed_pair, HOP_CLAIM_CONTEXT, &hash);
		let ed_ack = sign_ed(&ed_pair, HOP_ACK_CONTEXT, &hash);
		assert_eq!(data, pool.claim(&hash, &ed_claim).unwrap());
		pool.ack(&hash, &ed_ack).unwrap();
		assert!(!pool.has(&hash));
	}

	#[test]
	fn test_claim_is_repeatable() {
		let (pool, _dir) = create_test_pool();
		let (pair, signer) = test_recipient();
		let data = vec![1, 2, 3, 4, 5];
		let hash = pool
			.insert(data.clone(), bv(vec![signer]), SENDER_A, dummy_auth().0, dummy_auth().1, 0)
			.unwrap();

		let claim = sign_ed(&pair, HOP_CLAIM_CONTEXT, &hash);
		assert_eq!(data, pool.claim(&hash, &claim).unwrap());
		assert_eq!(data, pool.claim(&hash, &claim).unwrap());
		assert!(pool.has(&hash));
	}

	#[test]
	fn test_ack_idempotent() {
		let (pool, _dir) = create_test_pool();
		let (pair, signer) = test_recipient();
		let pair2 = ed25519::Pair::from_seed(&[2u8; 32]);
		let signer2 = MultiSigner::Ed25519(pair2.public());

		let hash = pool
			.insert(
				vec![1, 2, 3, 4, 5],
				bv(vec![signer, signer2]),
				SENDER_A,
				dummy_auth().0,
				dummy_auth().1,
				0,
			)
			.unwrap();
		let ack = sign_ed(&pair, HOP_ACK_CONTEXT, &hash);

		pool.ack(&hash, &ack).unwrap();
		pool.ack(&hash, &ack).unwrap();
		assert!(pool.has(&hash));
	}

	#[test]
	fn test_multi_recipient_partial_ack() {
		let (pool, _dir) = create_test_pool();
		let pair1 = ed25519::Pair::from_seed(&[1u8; 32]);
		let pair2 = ed25519::Pair::from_seed(&[2u8; 32]);
		let signer1 = MultiSigner::Ed25519(pair1.public());
		let signer2 = MultiSigner::Ed25519(pair2.public());

		let data = vec![1, 2, 3, 4, 5];
		let hash = pool
			.insert(
				data.clone(),
				bv(vec![signer1, signer2]),
				SENDER_A,
				dummy_auth().0,
				dummy_auth().1,
				0,
			)
			.unwrap();

		let claim1 = sign_ed(&pair1, HOP_CLAIM_CONTEXT, &hash);
		let ack1 = sign_ed(&pair1, HOP_ACK_CONTEXT, &hash);
		let claim2 = sign_ed(&pair2, HOP_CLAIM_CONTEXT, &hash);
		let ack2 = sign_ed(&pair2, HOP_ACK_CONTEXT, &hash);

		assert_eq!(data, pool.claim(&hash, &claim1).unwrap());
		pool.ack(&hash, &ack1).unwrap();
		assert!(pool.has(&hash));

		assert_eq!(data, pool.claim(&hash, &claim2).unwrap());
		pool.ack(&hash, &ack2).unwrap();
		assert!(!pool.has(&hash));
		assert_eq!(pool.status().total_bytes, 0);
	}

	#[test]
	fn test_concurrent_inserts_respect_capacity() {
		use std::{sync::Barrier, thread};

		let (_, signer) = test_recipient();
		// Capacity for exactly 4 entries of 50 bytes (accounted = 90 each).
		let (pool, _dir) = make_pool(acct(50, 1) * 4, 100);
		let pool = Arc::new(pool);
		let barrier = Arc::new(Barrier::new(10));

		let handles: Vec<_> = (0..10u8)
			.map(|i| {
				let pool = pool.clone();
				let signer = signer.clone();
				let barrier = barrier.clone();
				thread::spawn(move || {
					barrier.wait();
					pool.insert(
						vec![i; 50],
						bv(vec![signer]),
						SENDER_A,
						dummy_auth().0,
						dummy_auth().1,
						0,
					)
				})
			})
			.collect();

		let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
		let successes = results.iter().filter(|r| r.is_ok()).count();

		assert!(successes <= 4, "Got {} successes, max should be 4", successes);
		assert!(pool.status().total_bytes <= acct(50, 1) * 4);
	}

	#[test]
	fn test_concurrent_inserts_respect_user_quota() {
		use std::{sync::Barrier, thread};

		let (_, signer) = test_recipient();
		// Per-user cap holds 3 entries of 100 bytes. Pool has plenty of room so the
		// *user* cap is what actually constrains the test.
		let per_entry = acct(100, 1);
		let (pool, _dir) = make_pool_with_user_cap(per_entry * 20, per_entry * 3, 100);
		let pool = Arc::new(pool);
		let barrier = Arc::new(Barrier::new(10));

		let handles: Vec<_> = (0..10u8)
			.map(|i| {
				let pool = pool.clone();
				let signer = signer.clone();
				let barrier = barrier.clone();
				thread::spawn(move || {
					barrier.wait();
					pool.insert(
						vec![i; 100],
						bv(vec![signer]),
						SENDER_A,
						dummy_auth().0,
						dummy_auth().1,
						0,
					)
				})
			})
			.collect();

		let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
		let successes = results.iter().filter(|r| r.is_ok()).count();

		// Hard per-user cap: at most 3 inserts may succeed regardless of concurrency.
		assert!(successes <= 3, "hard per-user cap violated: {} successes", successes);
		assert!(user_usage(&pool, &SENDER_A) <= per_entry * 3);
	}

	#[test]
	fn test_concurrent_claim_and_ack() {
		use std::{sync::Barrier, thread};

		let (pool, _dir) = create_test_pool();
		let pool = Arc::new(pool);

		let pairs: Vec<_> = (1..=5u8)
			.map(|i| {
				let pair = ed25519::Pair::from_seed(&[i; 32]);
				let signer = MultiSigner::Ed25519(pair.public());
				(pair, signer)
			})
			.collect();

		let signers: Vec<_> = pairs.iter().map(|(_, s)| s.clone()).collect();
		let data = vec![42u8; 100];
		let hash = pool
			.insert(data.clone(), bv(signers), SENDER_A, dummy_auth().0, dummy_auth().1, 0)
			.unwrap();

		let barrier = Arc::new(Barrier::new(5));

		let handles: Vec<_> = pairs
			.into_iter()
			.map(|(pair, _)| {
				let pool = pool.clone();
				let barrier = barrier.clone();
				let data = data.clone();
				thread::spawn(move || {
					barrier.wait();
					let claim = sign_ed(&pair, HOP_CLAIM_CONTEXT, &hash);
					let ack = sign_ed(&pair, HOP_ACK_CONTEXT, &hash);

					let claimed = pool.claim(&hash, &claim).unwrap();
					assert_eq!(data, claimed);
					pool.ack(&hash, &ack).unwrap();
				})
			})
			.collect();

		for h in handles {
			h.join().unwrap();
		}

		assert!(!pool.has(&hash));
		assert_eq!(pool.status().total_bytes, 0);
	}

	#[test]
	fn test_concurrent_duplicate_insert_preserves_files() {
		use std::{sync::Barrier, thread};

		// Two threads insert identical content concurrently. The race-loser must
		// not delete the winner's blob/meta files; the winning hash must remain
		// readable via claim().
		let (kp, signer) = test_recipient();
		let (pool, _dir) = make_pool(1024 * 1024, 100);
		let pool = Arc::new(pool);
		let data = vec![0xABu8; 4096];
		let barrier = Arc::new(Barrier::new(2));

		let handles: Vec<_> = (0..2)
			.map(|_| {
				let pool = pool.clone();
				let barrier = barrier.clone();
				let signer = signer.clone();
				let data = data.clone();
				thread::spawn(move || {
					barrier.wait();
					pool.insert(data, bv(vec![signer]), SENDER_A, dummy_auth().0, dummy_auth().1, 0)
				})
			})
			.collect();
		let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

		let oks: Vec<_> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
		let dupes = results.iter().filter(|r| matches!(r, Err(HopError::DuplicateEntry))).count();
		assert_eq!(oks.len(), 1, "exactly one insert must win the race");
		assert_eq!(dupes, 1, "the other must report DuplicateEntry");

		let hash = *oks[0];
		let sig = sign_ed(&kp, HOP_CLAIM_CONTEXT, &hash);
		let claimed = pool.claim(&hash, &sig).expect("claim must succeed");
		assert_eq!(claimed, data);
	}

	#[test]
	fn test_concurrent_duplicate_insert_keeps_winner_meta_on_disk() {
		use std::{sync::Barrier, thread};

		// Same content, different senders. The race-loser's meta must not end
		// up on disk; otherwise restart recovery would silently load it as
		// canonical for the entry.
		let dir = TempDir::new().unwrap();
		let pool = Arc::new(
			HopDataPool::new(
				1024 * 1024,
				1024 * 1024,
				100,
				dir.path().to_path_buf(),
				RateLimitConfig::disabled(),
				HopMetrics::disabled(),
			)
			.unwrap(),
		);

		let signer_a = MultiSigner::Ed25519(ed25519::Pair::from_seed(&[11u8; 32]).public());
		let signer_b = MultiSigner::Ed25519(ed25519::Pair::from_seed(&[22u8; 32]).public());
		let data = vec![0xCDu8; 4096];

		let barrier = Arc::new(Barrier::new(2));
		let (p1, d1, b1, s1) = (pool.clone(), data.clone(), barrier.clone(), signer_a.clone());
		let h1 = thread::spawn(move || {
			b1.wait();
			p1.insert(d1, bv(vec![s1]), SENDER_A, dummy_auth().0, dummy_auth().1, 0)
		});
		let (p2, d2, b2, s2) = (pool.clone(), data.clone(), barrier.clone(), signer_b.clone());
		let h2 = thread::spawn(move || {
			b2.wait();
			p2.insert(d2, bv(vec![s2]), SENDER_B, dummy_auth().0, dummy_auth().1, 0)
		});

		let r1 = h1.join().unwrap();
		let r2 = h2.join().unwrap();

		let (winner_hash, winner_sender) = match (&r1, &r2) {
			(Ok(h), Err(HopError::DuplicateEntry)) => (*h, SENDER_A),
			(Err(HopError::DuplicateEntry), Ok(h)) => (*h, SENDER_B),
			other => panic!("expected exactly one winner and one DuplicateEntry, got {other:?}"),
		};

		// Simulate restart: drop the pool, reopen from the same data dir so
		// recovery rebuilds the in-memory index from `.meta` files on disk.
		drop(pool);
		let pool2 = HopDataPool::new(
			1024 * 1024,
			1024 * 1024,
			100,
			dir.path().to_path_buf(),
			RateLimitConfig::disabled(),
			HopMetrics::disabled(),
		)
		.unwrap();

		let recovered_sender = pool2
			.index
			.lock()
			.get(&winner_hash)
			.expect("winner's entry must survive restart")
			.sender_id;
		assert_eq!(
			recovered_sender, winner_sender,
			"on-disk meta diverged from the winning insert; loser's meta overwrote the winner's",
		);
	}

	#[test]
	fn test_saturating_release_concurrent_no_underflow() {
		use std::{sync::Barrier, thread};

		// Many threads each release a fixed amount that sums to exactly the
		// initial counter. With a non-atomic load-then-clamp-then-fetch_sub,
		// stale clamps would let the counter wrap to ~u64::MAX.
		// `saturating_release` must keep the result clamped at 0.
		const THREADS: u64 = 32;
		const RELEASE_PER_THREAD: u64 = 7;
		let counter = Arc::new(AtomicU64::new(THREADS * RELEASE_PER_THREAD));
		let barrier = Arc::new(Barrier::new(THREADS as usize));

		let handles: Vec<_> = (0..THREADS)
			.map(|_| {
				let counter = counter.clone();
				let barrier = barrier.clone();
				thread::spawn(move || {
					barrier.wait();
					saturating_release(&counter, RELEASE_PER_THREAD);
				})
			})
			.collect();
		for h in handles {
			h.join().unwrap();
		}

		assert_eq!(counter.load(Ordering::Relaxed), 0, "counter underflowed or did not reach zero");

		// Releasing more than the remaining balance must clamp to 0, never wrap.
		saturating_release(&counter, u64::MAX);
		assert_eq!(counter.load(Ordering::Relaxed), 0);
	}

	#[test]
	fn test_get_promotable_within_buffer() {
		// retention=3600s; a freshly-inserted entry is in the promotion window only
		// if the buffer is at least as large as the time-to-expiry.
		let (pool, _dir) = make_pool(1024 * 1024, 3600);
		let (_, signer) = test_recipient();

		let hash = pool
			.insert(vec![1, 2, 3], bv(vec![signer]), SENDER_A, dummy_auth().0, dummy_auth().1, 0)
			.unwrap();

		// Small buffer (180s ≪ 3600s retention): not promotable yet.
		let promotable = pool.get_promotable(50, 180, usize::MAX);
		assert!(promotable.is_empty());

		// Large buffer (6000s > 3600s retention): within the window.
		let promotable = pool.get_promotable(0, 6000, usize::MAX);
		assert_eq!(promotable.len(), 1);
		assert_eq!(promotable[0], hash);
	}

	#[test]
	fn test_get_promotable_excludes_promoted() {
		let (pool, _dir) = make_pool(1024 * 1024, 100);
		let (_, signer) = test_recipient();

		let hash = pool
			.insert(vec![1, 2, 3], bv(vec![signer]), SENDER_A, dummy_auth().0, dummy_auth().1, 0)
			.unwrap();

		let promotable = pool.get_promotable(80, 180, usize::MAX);
		assert_eq!(promotable.len(), 1);

		pool.mark_promoted(&hash);

		let promotable = pool.get_promotable(80, 180, usize::MAX);
		assert!(promotable.is_empty());
	}

	#[test]
	fn test_mark_promoted_persists_across_restart() {
		let dir = TempDir::new().unwrap();
		let (_, signer) = test_recipient();

		let hash;
		{
			let pool = HopDataPool::new(
				1024 * 1024,
				1024 * 1024,
				100,
				dir.path().to_path_buf(),
				RateLimitConfig::disabled(),
				HopMetrics::disabled(),
			)
			.unwrap();
			hash = pool
				.insert(
					vec![42u8; 10],
					bv(vec![signer]),
					SENDER_A,
					dummy_auth().0,
					dummy_auth().1,
					0,
				)
				.unwrap();
			pool.mark_promoted(&hash);
		}

		{
			let pool = HopDataPool::new(
				1024 * 1024,
				1024 * 1024,
				100,
				dir.path().to_path_buf(),
				RateLimitConfig::disabled(),
				HopMetrics::disabled(),
			)
			.unwrap();
			let promotable = pool.get_promotable(80, 180, usize::MAX);
			assert!(promotable.is_empty(), "promoted entry should not be promotable after restart");
			assert!(pool.has(&hash), "entry should still exist");
		}
	}

	#[test]
	fn test_cleanup_expired_removes_promoted() {
		let (pool, _dir) = make_pool(1024 * 1024, 0);
		let (_, signer) = test_recipient();

		let hash = pool
			.insert(vec![1, 2, 3], bv(vec![signer]), SENDER_A, dummy_auth().0, dummy_auth().1, 0)
			.unwrap();
		pool.mark_promoted(&hash);
		assert!(pool.has(&hash));

		let freed = pool.cleanup_expired(0);
		assert!(freed > 0);
		assert!(!pool.has(&hash));
	}

	#[test]
	fn test_rate_limit_rejects_burst_overflow() {
		let dir = TempDir::new().unwrap();
		// submit_burst=2 so the 3rd request is rate-limited by submit count.
		// Bandwidth is sized comfortably above the 3-byte test payloads so the
		// rejection comes from the request bucket, not the bandwidth bucket.
		let cfg = RateLimitConfig {
			enabled: true,
			submit_rate_per_min: 60,
			submit_burst: 2,
			bandwidth_per_min: 1024 * 1024 * 60,
			bandwidth_burst: 1024 * 1024,
		};
		let pool = HopDataPool::new(
			1024 * 1024,
			1024 * 1024,
			100,
			dir.path().to_path_buf(),
			cfg,
			HopMetrics::disabled(),
		)
		.unwrap();
		let (_, signer) = test_recipient();

		pool.insert(
			vec![1, 2, 3],
			bv(vec![signer.clone()]),
			SENDER_A,
			dummy_auth().0,
			dummy_auth().1,
			0,
		)
		.unwrap();
		pool.insert(
			vec![4, 5, 6],
			bv(vec![signer.clone()]),
			SENDER_A,
			dummy_auth().0,
			dummy_auth().1,
			0,
		)
		.unwrap();
		assert!(matches!(
			pool.insert(
				vec![7, 8, 9],
				bv(vec![signer]),
				SENDER_A,
				dummy_auth().0,
				dummy_auth().1,
				0,
			),
			Err(HopError::RateLimited { .. })
		));
	}

	#[test]
	fn test_meta_version_mismatch_rejected() {
		// Persist a HopEntryMeta with version 0 (an older / future schema), then
		// boot a fresh pool over the same dir and assert the .meta is wiped and
		// not surfaced in the in-memory index.
		let dir = TempDir::new().unwrap();
		let (_, signer) = test_recipient();
		let recipients = bv(vec![signer.clone()]);
		let mut meta =
			HopEntryMeta::new(100, 0, recipients, SENDER_A, dummy_auth().0, dummy_auth().1, 0);
		meta.version = 0;

		let fake_hash = "ee".to_string() + &"ff".repeat(15);
		let meta_dir = dir.path().join("meta").join("ee");
		let blob_dir = dir.path().join("blobs").join("ee");
		fs::create_dir_all(&meta_dir).unwrap();
		fs::create_dir_all(&blob_dir).unwrap();
		let meta_path = meta_dir.join(format!("{}.meta", fake_hash));
		let blob_path = blob_dir.join(format!("{}.blob", fake_hash));
		fs::write(&meta_path, meta.encode()).unwrap();
		fs::write(&blob_path, b"x").unwrap();

		let registry = prometheus_endpoint::Registry::new();
		let pool = HopDataPool::new(
			1024 * 1024,
			1024 * 1024,
			100,
			dir.path().to_path_buf(),
			RateLimitConfig::disabled(),
			HopMetrics::new(Some(&registry)).unwrap(),
		)
		.unwrap();
		assert!(!meta_path.exists(), "stale-version .meta should be removed");
		assert!(!blob_path.exists(), "matching .blob should also be removed");
		assert_eq!(pool.status().entry_count, 0);
		assert_eq!(pool.metrics().removed_count(removal_reasons::STARTUP_DROPPED), 1);
	}

	#[test]
	fn test_promotion_backoff_skips_until_due_then_gives_up() {
		use crate::types::MAX_PROMOTION_ATTEMPTS;

		let (pool, _dir) = make_pool(1024 * 1024, /* retention = */ 100);
		let (_, signer) = test_recipient();
		let hash = pool
			.insert(vec![1u8; 100], bv(vec![signer]), SENDER_A, dummy_auth().0, dummy_auth().1, 0)
			.unwrap();

		// Inside the buffer window (>= retention=100s) so the entry is promotable
		// in principle.
		let buffer = 300_u64;
		let current = 60;
		assert_eq!(pool.get_promotable(current, buffer, 10), vec![hash]);

		// First failure schedules next attempt at current + 1× check_interval_blocks.
		let check_interval_blocks: u32 = 10;
		pool.record_promotion_attempt(&hash, current, check_interval_blocks);
		assert!(
			pool.get_promotable(current, buffer, 10).is_empty(),
			"entry should be skipped until back-off elapses"
		);
		assert_eq!(pool.get_promotable(current + 10, buffer, 10), vec![hash]);

		// Burn through the remaining attempts; once at MAX, the entry stays out
		// of the promotable set forever (regardless of how far we advance time).
		// Schedule after first failure: 1×, 2×, 4×, 8×, 16× check_interval.
		let mut now = current + 10;
		for next_attempt in 2..=MAX_PROMOTION_ATTEMPTS {
			pool.record_promotion_attempt(&hash, now, check_interval_blocks);
			let shift = (next_attempt - 1).min(5);
			let backoff = check_interval_blocks << shift;
			now += backoff;
		}
		assert!(
			pool.get_promotable(now + 10_000, buffer, 10).is_empty(),
			"entry should give up after MAX_PROMOTION_ATTEMPTS"
		);
	}
}
