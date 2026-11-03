// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Configuration types for remote externalities.

use codec::{Compact, Decode, Encode};
use sp_runtime::{traits::Block as BlockT, StateVersion};
use std::{
	fs,
	path::{Path, PathBuf},
};

use crate::Result;

pub(crate) const DEFAULT_WS_ENDPOINT: &str = "wss://try-runtime.polkadot.io:443";
pub(crate) type SnapshotVersion = Compact<u16>;
pub(crate) const SNAPSHOT_VERSION: SnapshotVersion = Compact(4);

/// The execution mode.
#[derive(Clone)]
pub enum Mode<H> {
	/// Online. Potentially writes to a snapshot file.
	Online(OnlineConfig<H>),
	/// Offline. Uses a state snapshot file and needs not any client config.
	Offline(OfflineConfig),
	/// Prefer using a snapshot file if it exists, else use a remote server.
	OfflineOrElseOnline(OfflineConfig, OnlineConfig<H>),
}

impl<H> Default for Mode<H> {
	fn default() -> Self {
		Mode::Online(OnlineConfig::default())
	}
}

/// Configuration of the offline execution.
///
/// A state snapshot config must be present.
#[derive(Clone)]
pub struct OfflineConfig {
	/// The configuration of the state snapshot file to use. It must be present.
	pub state_snapshot: SnapshotConfig,
}

/// Configuration of the online execution.
///
/// A state snapshot config may be present and will be written to in that case.
#[derive(Clone)]
pub struct OnlineConfig<H> {
	/// The block hash at which to get the runtime state. Will be latest finalized head if not
	/// provided.
	pub at: Option<H>,
	/// An optional state snapshot file to WRITE to, not for reading. Not written if set to `None`.
	pub state_snapshot: Option<SnapshotConfig>,
	/// The pallets to scrape. These values are hashed and added to `hashed_prefix`.
	pub pallets: Vec<String>,
	/// Transport URIs. Can be a single URI or multiple for load distribution.
	pub transport_uris: Vec<String>,
	/// Lookout for child-keys, and scrape them as well if set to true.
	pub child_trie: bool,
	/// Storage entry key prefixes to be injected into the externalities. The *hashed* prefix must
	/// be given.
	pub hashed_prefixes: Vec<Vec<u8>>,
	/// Storage entry keys to be injected into the externalities. The *hashed* key must be given.
	pub hashed_keys: Vec<Vec<u8>>,
	/// Disable verifying the downloaded storage root against the block header's state root.
	///
	/// The check fails `build` when the scraped state is incomplete or corrupted. Set this to
	/// bypass it when deliberately overwriting the state version. Use with care.
	pub disable_root_check: bool,
}

impl<H: Clone> OnlineConfig<H> {
	pub(crate) fn at_expected(&self) -> H {
		self.at.clone().expect("block at must be initialized; qed")
	}
}

impl<H> Default for OnlineConfig<H> {
	fn default() -> Self {
		Self {
			transport_uris: vec![DEFAULT_WS_ENDPOINT.to_owned()],
			child_trie: true,
			at: None,
			state_snapshot: None,
			pallets: Default::default(),
			hashed_keys: Default::default(),
			hashed_prefixes: Default::default(),
			disable_root_check: false,
		}
	}
}

impl<H> From<String> for OnlineConfig<H> {
	fn from(uri: String) -> Self {
		Self { transport_uris: vec![uri], ..Default::default() }
	}
}

/// Configuration of the state snapshot.
#[derive(Clone)]
pub struct SnapshotConfig {
	/// The path to the snapshot file.
	pub path: PathBuf,
}

impl SnapshotConfig {
	pub fn new<P: Into<PathBuf>>(path: P) -> Self {
		Self { path: path.into() }
	}
}

impl From<String> for SnapshotConfig {
	fn from(s: String) -> Self {
		Self::new(s)
	}
}

impl Default for SnapshotConfig {
	fn default() -> Self {
		Self { path: Path::new("SNAPSHOT").into() }
	}
}

/// The snapshot that we store on disk.
#[derive(Decode, Encode)]
pub(crate) struct Snapshot<B: BlockT> {
	snapshot_version: SnapshotVersion,
	pub(crate) state_version: StateVersion,
	pub(crate) raw_storage: Vec<(Vec<u8>, (Vec<u8>, i32))>,
	pub(crate) storage_root: B::Hash,
	pub(crate) header: B::Header,
}

impl<B: BlockT> Snapshot<B> {
	pub(crate) fn new(
		state_version: StateVersion,
		raw_storage: Vec<(Vec<u8>, (Vec<u8>, i32))>,
		storage_root: B::Hash,
		header: B::Header,
	) -> Self {
		Self {
			snapshot_version: SNAPSHOT_VERSION,
			state_version,
			raw_storage,
			storage_root,
			header,
		}
	}

	pub(crate) fn load(path: &PathBuf) -> Result<Snapshot<B>> {
		let bytes = fs::read(path).map_err(|_| "fs::read failed.")?;
		// The first item in the SCALE encoded struct bytes is the snapshot version. We decode and
		// check that first, before proceeding to decode the rest of the snapshot.
		let snapshot_version = SnapshotVersion::decode(&mut &*bytes)
			.map_err(|_| "Failed to decode snapshot version")?;

		if snapshot_version != SNAPSHOT_VERSION {
			return Err("Unsupported snapshot version detected. Please create a new snapshot.");
		}

		Decode::decode(&mut &*bytes).map_err(|_| "Decode failed")
	}
}
