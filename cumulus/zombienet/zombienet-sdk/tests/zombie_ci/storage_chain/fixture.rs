// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
pub use sp_transaction_storage_proof::HashingAlgorithm;
use std::path::{Path, PathBuf};
use zombienet_sdk::{snapshot::untar_bundle, SnapshotManifest};

use super::common::ParachainSnapshots;

pub const FIXTURE_RETENTION_PERIOD: u32 = 200;
pub const TIP_SYNC_TARGET_BLOCKS: u64 = 100;
pub const N_STORES: u32 = 30;

pub const PAYLOAD_SIZE_MIN: usize = 512 * 1024;
pub const PAYLOAD_SIZE_MAX: usize = 1536 * 1024;

pub const BUNDLE_ENV: &str = "STORAGE_CHAIN_BUNDLE";

const DEFAULT_BUNDLE_URL: &str = "https://storage.googleapis.com/zombienet-db-snaps/zombienet/storage_chain_sync/tip-sync-100-bundle.tar.gz";

const PARA_DB_ARCHIVE: &str = "parachain-db.tgz";
const RELAY_DB_ARCHIVE: &str = "relaychain-db.tgz";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotMetadata {
	pub total_blocks: u64,
	pub retention_period: u32,
	pub n_stores: u32,
	pub payload_size_min: usize,
	pub payload_size_max: usize,
	pub snapshot_height: u64,
	pub first_store_block: u64,
	pub last_store_block: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleUserData {
	pub metadata: SnapshotMetadata,
	pub para_chain_spec: serde_json::Value,
	pub relay_chain_spec: serde_json::Value,
}

pub struct ResolvedSnapshots {
	pub collator: PathBuf,
	pub relay: PathBuf,
	pub chain_spec: PathBuf,
	pub relay_chain_spec: PathBuf,
	pub metadata: SnapshotMetadata,
	_workdir: tempfile::TempDir,
}

impl ResolvedSnapshots {
	pub fn load() -> Result<Self> {
		let bundle_location =
			std::env::var(BUNDLE_ENV).unwrap_or_else(|_| DEFAULT_BUNDLE_URL.to_string());

		let workdir = tempfile::Builder::new()
			.prefix("storage-chain-bundle-")
			.tempdir()
			.context("Failed to create temp dir for bundle")?;

		let bundle_path = if is_url(&bundle_location) {
			let dst = workdir.path().join("bundle.tar.gz");
			download(&bundle_location, &dst)?;
			dst
		} else {
			PathBuf::from(&bundle_location)
		};

		let extract_dir = workdir.path().join("extracted");
		untar_bundle(&bundle_path, &extract_dir)
			.with_context(|| format!("Failed to untar bundle {}", bundle_path.display()))?;

		let manifest_path = extract_dir.join("manifest.json");
		let manifest_bytes = std::fs::read(&manifest_path)
			.with_context(|| format!("Failed to read {}", manifest_path.display()))?;
		let manifest: SnapshotManifest = serde_json::from_slice(&manifest_bytes)
			.with_context(|| format!("Failed to decode {}", manifest_path.display()))?;
		let user_data: BundleUserData = serde_json::from_value(manifest.user_data)
			.context("Failed to decode BundleUserData from manifest.user_data")?;

		let collator = extract_dir.join(PARA_DB_ARCHIVE);
		let relay = extract_dir.join(RELAY_DB_ARCHIVE);
		for (label, path) in [("collator", &collator), ("relay", &relay)] {
			anyhow::ensure!(path.is_file(), "bundle missing {label} archive at {}", path.display());
		}

		let chain_spec = workdir.path().join("para-chain-spec.json");
		let relay_chain_spec = workdir.path().join("relay-chain-spec.json");
		write_json(&chain_spec, &user_data.para_chain_spec)?;
		write_json(&relay_chain_spec, &user_data.relay_chain_spec)?;

		Ok(Self {
			collator,
			relay,
			chain_spec,
			relay_chain_spec,
			metadata: user_data.metadata,
			_workdir: workdir,
		})
	}

	pub fn as_parachain_snapshots(&self) -> ParachainSnapshots<'_> {
		ParachainSnapshots {
			collator: self.collator.to_str().expect("non-utf8 path"),
			relay: self.relay.to_str().expect("non-utf8 path"),
			chain_spec: self.chain_spec.to_str().expect("non-utf8 path"),
			relay_chain_spec: self.relay_chain_spec.to_str().expect("non-utf8 path"),
		}
	}
}

fn is_url(s: &str) -> bool {
	s.starts_with("http://") || s.starts_with("https://")
}

fn download(url: &str, dst: &Path) -> Result<()> {
	log::info!("Downloading bundle from {} -> {}", url, dst.display());
	let status = std::process::Command::new("curl")
		.args(["-fsSL", "-o"])
		.arg(dst)
		.arg(url)
		.status()
		.with_context(|| format!("Failed to spawn curl for {url}"))?;
	anyhow::ensure!(status.success(), "curl failed downloading {url}");
	Ok(())
}

fn write_json(path: &Path, value: &serde_json::Value) -> Result<()> {
	let bytes = serde_json::to_vec(value)
		.with_context(|| format!("Failed to serialise {}", path.display()))?;
	std::fs::write(path, bytes).with_context(|| format!("Failed to write {}", path.display()))?;
	Ok(())
}

pub fn algorithm(i: u32) -> HashingAlgorithm {
	match i % 2 {
		0 => HashingAlgorithm::Blake2b256,
		_ => HashingAlgorithm::Sha2_256,
	}
}

pub fn payload(i: u32) -> Vec<u8> {
	let span = (PAYLOAD_SIZE_MAX - PAYLOAD_SIZE_MIN + 1) as u32;
	let size = PAYLOAD_SIZE_MIN + (xorshift32_seeded(i.wrapping_add(0xA53C7B91)) % span) as usize;

	let mut state = i.wrapping_add(0x9E3779B9);
	let mut data = Vec::with_capacity(size);
	while data.len() < size {
		state = xorshift32(state);
		let remaining = size - data.len();
		if remaining >= 4 {
			data.extend_from_slice(&state.to_le_bytes());
		} else {
			data.extend_from_slice(&state.to_le_bytes()[..remaining]);
		}
	}
	data
}

pub fn content_hash(i: u32) -> [u8; 32] {
	algorithm(i).hash(&payload(i))
}

pub fn hash_to_cid(hash: &[u8; 32], algo: HashingAlgorithm) -> String {
	use cid::Cid;
	use multihash::Multihash;
	const RAW_CODEC: u64 = 0x55;
	let mh = Multihash::<64>::wrap(algo.multihash_code(), hash).expect("Valid multihash");
	Cid::new_v1(RAW_CODEC, mh).to_string()
}

fn xorshift32(mut x: u32) -> u32 {
	x ^= x << 13;
	x ^= x >> 17;
	x ^= x << 5;
	x
}

fn xorshift32_seeded(seed: u32) -> u32 {
	let s = if seed == 0 { 1 } else { seed };
	xorshift32(s)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn payload_is_deterministic_per_index() {
		for i in 0..50u32 {
			assert_eq!(payload(i), payload(i), "payload({i}) not deterministic");
		}
	}

	#[test]
	fn payload_sizes_within_bounds() {
		for i in 0..200u32 {
			let len = payload(i).len();
			assert!(
				(PAYLOAD_SIZE_MIN..=PAYLOAD_SIZE_MAX).contains(&len),
				"payload({i}).len()={} out of bounds [{}, {}]",
				len,
				PAYLOAD_SIZE_MIN,
				PAYLOAD_SIZE_MAX,
			);
		}
	}

	#[test]
	fn content_hashes_are_unique_for_first_n_stores() {
		use std::collections::HashSet;
		let mut seen = HashSet::new();
		for i in 0..N_STORES {
			let h = content_hash(i);
			assert!(seen.insert(h), "duplicate content hash at i={i}");
		}
	}

	#[test]
	fn algorithm_round_robin_blake_sha() {
		assert_eq!(algorithm(0), HashingAlgorithm::Blake2b256);
		assert_eq!(algorithm(1), HashingAlgorithm::Sha2_256);
		assert_eq!(algorithm(2), HashingAlgorithm::Blake2b256);
		assert_eq!(algorithm(3), HashingAlgorithm::Sha2_256);
	}
}
