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

//! # Remote Externalities
//!
//! An equivalent of `sp_io::TestExternalities` that can load its state from a remote substrate
//! based chain, or a local state snapshot file.

mod client;
mod config;
mod key_range;
mod logging;
mod parallel;

pub use config::{Mode, OfflineConfig, OnlineConfig, SnapshotConfig};

use client::{with_timeout, Client, ConnectionManager, RPC_TIMEOUT};
use codec::Encode;
use config::Snapshot;
#[cfg(all(test, feature = "remote-test"))]
use config::DEFAULT_WS_ENDPOINT;
use indicatif::{ProgressBar, ProgressStyle};
use jsonrpsee::core::params::ArrayParams;
use log::*;
use parallel::{run_workers, ProcessResult, RetryAction};
use serde::de::DeserializeOwned;
use sp_core::{
	hexdisplay::HexDisplay,
	storage::{
		well_known_keys::{is_default_child_storage_key, DEFAULT_CHILD_STORAGE_KEY_PREFIX},
		ChildInfo, ChildType, PrefixedStorageKey, StorageData, StorageKey,
	},
};
use sp_runtime::{
	traits::{Block as BlockT, HashingFor, Header as HeaderT},
	StateVersion,
};
use sp_state_machine::TestExternalities;
use std::{
	collections::{BTreeSet, VecDeque},
	future::Future,
	ops::{Deref, DerefMut},
	sync::{
		atomic::{AtomicUsize, Ordering},
		Arc, Mutex,
	},
	time::Duration,
};
use substrate_rpc_client::{rpc_params, BatchRequestBuilder, ChainApi, ClientT, StateApi};

use crate::key_range::{initialize_work_queue, subdivide_remaining_range};

type Result<T, E = &'static str> = std::result::Result<T, E>;

type KeyValue = (StorageKey, StorageData);
type TopKeyValues = Vec<KeyValue>;
type ChildKeyValues = Vec<(ChildInfo, Vec<KeyValue>)>;

const LOG_TARGET: &str = "remote-ext";

/// Whether a stringified RPC error reports that the provider does not have the requested block.
///
/// Such a provider is lagging or pruning and should be dropped rather than retried (see the
/// `UnknownBlock` error in `sp_blockchain`).
fn is_unknown_block_error(error: &str) -> bool {
	error.contains("UnknownBlock")
}

/// How to handle the worker's client after a failed RPC: drop a provider that lacks the block,
/// otherwise reconnect and retry.
fn retry_action(error: &str) -> RetryAction {
	if is_unknown_block_error(error) {
		RetryAction::Remove
	} else {
		RetryAction::Recreate
	}
}

/// An externalities that acts exactly the same as [`sp_io::TestExternalities`] but has a few extra
/// bits and pieces to it, and can be loaded remotely.
pub struct RemoteExternalities<B: BlockT> {
	/// The inner externalities.
	pub inner_ext: TestExternalities<HashingFor<B>>,
	/// The block header which we created this externalities env.
	pub header: B::Header,
}

impl<B: BlockT> Deref for RemoteExternalities<B> {
	type Target = TestExternalities<HashingFor<B>>;
	fn deref(&self) -> &Self::Target {
		&self.inner_ext
	}
}

impl<B: BlockT> DerefMut for RemoteExternalities<B> {
	fn deref_mut(&mut self) -> &mut Self::Target {
		&mut self.inner_ext
	}
}

/// Builder for [`RemoteExternalities`].
#[derive(Clone)]
pub struct Builder<B: BlockT> {
	/// Custom key-pairs to be injected into the final externalities. The *hashed* keys and values
	/// must be given.
	hashed_key_values: Vec<KeyValue>,
	/// The keys that will be excluded from the final externality. The *hashed* key must be given.
	hashed_blacklist: Vec<Vec<u8>>,
	/// Connectivity mode, online or offline.
	mode: Mode<B::Hash>,
	/// If provided, overwrite the state version with this. Otherwise, the state_version of the
	/// remote node is used. All cache files also store their state version.
	///
	/// Overwrite only with care.
	overwrite_state_version: Option<StateVersion>,
	/// Connection manager for RPC clients (initialized during `init_remote_client`).
	conn_manager: Option<ConnectionManager>,
}

impl<B: BlockT> Default for Builder<B> {
	fn default() -> Self {
		Self {
			mode: Default::default(),
			hashed_key_values: Default::default(),
			hashed_blacklist: Default::default(),
			overwrite_state_version: None,
			conn_manager: None,
		}
	}
}

// Mode methods
impl<B: BlockT> Builder<B> {
	fn as_online(&self) -> &OnlineConfig<B::Hash> {
		match &self.mode {
			Mode::Online(config) => config,
			Mode::OfflineOrElseOnline(_, config) => config,
			_ => panic!("Unexpected mode: Online"),
		}
	}

	fn as_online_mut(&mut self) -> &mut OnlineConfig<B::Hash> {
		match &mut self.mode {
			Mode::Online(config) => config,
			Mode::OfflineOrElseOnline(_, config) => config,
			_ => panic!("Unexpected mode: Online"),
		}
	}

	fn conn_manager(&self) -> Result<&ConnectionManager> {
		self.conn_manager.as_ref().ok_or("connection manager must be initialized; qed")
	}

	/// Whether the configured scrape covers the entire top trie.
	///
	/// Only a complete scrape yields a storage root that matches the block header's state root.
	/// This is signalled by the empty prefix being queued for download (see
	/// `init_remote_client`); partial scrapes (specific pallets/keys) never match.
	fn is_complete_scrape(&self) -> bool {
		self.as_online().hashed_prefixes.iter().any(|p| p.is_empty())
	}
}

// RPC methods
impl<B: BlockT> Builder<B>
where
	B::Hash: DeserializeOwned,
	B::Header: DeserializeOwned,
{
	const PARALLEL_REQUESTS_PER_CLIENT: usize = 4;

	async fn parallel_requests(&self) -> usize {
		let cm = self.conn_manager().expect("connection manager must be initialized; qed");
		cm.num_clients().await * Self::PARALLEL_REQUESTS_PER_CLIENT
	}

	/// Execute an RPC call on any available client. Tries each client until one succeeds.
	///
	/// Starts with a random client to distribute load across clients.
	async fn with_any_client<T, E, F, Fut>(&self, op_name: &'static str, f: F) -> Result<T, ()>
	where
		F: Fn(Client) -> Fut,
		Fut: Future<Output = std::result::Result<T, E>>,
		E: std::fmt::Debug,
	{
		let conn_manager = self.conn_manager().map_err(|_| ())?;
		let num_clients = conn_manager.num_clients().await;
		let start_offset: usize = rand::random();
		for j in 0..num_clients {
			let i = (start_offset + j) % num_clients;
			let client = conn_manager.get(i).await;
			let result = with_timeout(f(client), RPC_TIMEOUT).await;
			match result {
				Ok(Ok(value)) => return Ok(value),
				Ok(Err(e)) => {
					debug!(target: LOG_TARGET, "Client {i}: {op_name} RPC error: {e:?}");
				},
				Err(()) => {
					debug!(target: LOG_TARGET, "Client {i}: {op_name} timeout");
				},
			}
		}
		Err(())
	}

	/// Get a single storage value. Tries each client until one succeeds.
	async fn rpc_get_storage(
		&self,
		key: StorageKey,
		maybe_at: Option<B::Hash>,
	) -> Result<Option<StorageData>> {
		trace!(target: LOG_TARGET, "rpc: get_storage");
		self.with_any_client("get_storage", move |client| {
			let key = key.clone();
			async move { client.storage(key, maybe_at).await }
		})
		.await
		.map_err(|_| "rpc get_storage failed on all clients")
	}

	/// Fetch the state version from the runtime. Tries each client until one succeeds.
	async fn fetch_state_version(&self) -> Result<StateVersion> {
		let conn_manager = self.conn_manager()?;

		let num_clients = conn_manager.num_clients().await;
		for i in 0..num_clients {
			let client = conn_manager.get(i).await;
			let result = with_timeout(
				StateApi::<B::Hash>::runtime_version(client.ws_client.as_ref(), None),
				RPC_TIMEOUT,
			)
			.await;

			match result {
				Ok(Ok(version)) => return Ok(version.state_version()),
				Ok(Err(e)) => {
					debug!(target: LOG_TARGET, "Client {i}: runtime_version RPC error: {e:?}");
				},
				Err(()) => {
					debug!(target: LOG_TARGET, "Client {i}: runtime_version timeout");
				},
			}
		}

		Err("rpc runtime_version failed on all clients")
	}

	/// Get the latest finalized head. Tries each client until one succeeds.
	async fn rpc_get_head(&self) -> Result<B::Hash> {
		trace!(target: LOG_TARGET, "rpc: finalized_head");
		self.with_any_client("finalized_head", |client| async move {
			ChainApi::<(), _, B::Header, ()>::finalized_head(&*client).await
		})
		.await
		.map_err(|_| "rpc finalized_head failed on all clients")
	}

	/// Get keys with `prefix` at `block` using parallel workers.
	async fn rpc_get_keys_parallel(
		&self,
		prefix: &StorageKey,
		block: B::Hash,
		parallel: usize,
	) -> Result<Vec<StorageKey>> {
		let work_queue = initialize_work_queue(&[prefix.clone()]);
		let initial_ranges = work_queue.lock().unwrap().len();
		info!(target: LOG_TARGET, "🔧 Initialized work queue with {initial_ranges} ranges");

		let conn_manager = self.conn_manager()?;
		info!(target: LOG_TARGET, "🌐 Using {} RPC provider(s)", conn_manager.num_clients().await);
		info!(target: LOG_TARGET, "🚀 Spawning {parallel} parallel workers for key fetching");

		let all_keys: Arc<Mutex<BTreeSet<StorageKey>>> = Arc::new(Mutex::new(BTreeSet::new()));
		let last_logged_milestone = Arc::new(AtomicUsize::new(0));
		let initial_work = work_queue.lock().unwrap().drain(..).collect();
		let all_keys_for_result = all_keys.clone();

		run_workers(initial_work, conn_manager, parallel, move |worker_index, range, client| {
			let all_keys = all_keys.clone();
			let last_logged_milestone = last_logged_milestone.clone();

			async move {
				trace!(
					target: LOG_TARGET,
					"Worker {worker_index}: fetching keys starting at {:?} (page_size: {})",
					HexDisplay::from(&range.start_key.0),
					range.page_size
				);

				let rpc_result = with_timeout(
					client.storage_keys_paged(
						Some(range.prefix.clone()),
						range.page_size,
						Some(range.start_key.clone()),
						Some(block),
					),
					RPC_TIMEOUT,
				)
				.await;

				let page = match rpc_result {
					Ok(Ok(p)) => p,
					Ok(Err(e)) => {
						debug!(target: LOG_TARGET, "Worker {worker_index}: RPC error: {e:?}");
						return ProcessResult::Retry {
							work: range.with_halved_page_size(),
							sleep_duration: Duration::from_secs(15),
							action: retry_action(&format!("{e:?}")),
						};
					},
					Err(()) => {
						debug!(target: LOG_TARGET, "Worker {worker_index}: timeout");
						return ProcessResult::Retry {
							work: range.with_halved_page_size(),
							sleep_duration: Duration::from_secs(5),
							action: RetryAction::Recreate,
						};
					},
				};

				// Filter keys and determine if this was a full batch
				let (page, is_full_batch) = range.filter_keys(page);
				let last_two_keys = if page.len() >= 2 {
					Some((page[page.len() - 2].clone(), page[page.len() - 1].clone()))
				} else {
					None
				};

				let total_keys = {
					let mut keys = all_keys.lock().unwrap();
					keys.extend(page);
					keys.len()
				};

				// Log progress every 10,000 keys
				const LOG_INTERVAL: usize = 10_000;
				let current_milestone = (total_keys / LOG_INTERVAL) * LOG_INTERVAL;
				let last_milestone = last_logged_milestone.load(Ordering::Relaxed);
				if current_milestone > last_milestone && current_milestone > 0 {
					if last_logged_milestone
						.compare_exchange(
							last_milestone,
							current_milestone,
							Ordering::SeqCst,
							Ordering::Relaxed,
						)
						.is_ok()
					{
						info!(target: LOG_TARGET, "📊 Scraped {total_keys} keys so far...");
					}
				}

				// Subdivide remaining range if this was a full batch
				let new_work = if is_full_batch {
					if let Some((second_last, last)) = last_two_keys {
						subdivide_remaining_range(
							&second_last,
							&last,
							range.end_key.as_ref(),
							&range.prefix,
						)
					} else {
						vec![]
					}
				} else {
					vec![]
				};

				ProcessResult::Success { new_work }
			}
		})
		.await;

		let keys: Vec<_> = all_keys_for_result.lock().unwrap().iter().cloned().collect();
		info!(target: LOG_TARGET, "🎉 Parallel key fetching complete: {} unique keys", keys.len());

		Ok(keys)
	}

	/// Fetches storage data from a node using a dynamic batch size.
	///
	/// This function adjusts the batch size on the fly to help prevent overwhelming the node with
	/// large batch requests, and stay within request size limits enforced by the node.
	///
	/// # Arguments
	///
	/// * `client` - An `Arc` wrapped `HttpClient` used for making the requests.
	/// * `payloads` - A vector of tuples containing a JSONRPC method name and `ArrayParams`
	///
	/// # Returns
	///
	/// Returns a `Result` with a vector of `Option<StorageData>`, where each element corresponds to
	/// the storage data for the given method and parameters. The result will be an `Err` with a
	/// `String` error message if the request fails.
	///
	/// # Errors
	///
	/// This function will return an error if:
	/// * The batch request fails and the batch size is less than 2.
	/// * There are invalid batch params.
	/// * There is an error in the batch response.
	///
	/// # Example
	///
	/// ```ignore
	/// use your_crate::{get_storage_data_dynamic_batch_size, HttpClient, ArrayParams};
	/// use std::sync::Arc;
	///
	/// async fn example() {
	///     let client = HttpClient::new();
	///     let payloads = vec![
	///         ("some_method".to_string(), ArrayParams::new(vec![])),
	///         ("another_method".to_string(), ArrayParams::new(vec![])),
	///     ];
	///     let initial_batch_size = 10;
	///
	///     let storage_data = get_storage_data_dynamic_batch_size(client, payloads, batch_size).await;
	///     match storage_data {
	///         Ok(data) => println!("Storage data: {:?}", data),
	///         Err(e) => eprintln!("Error fetching storage data: {}", e),
	///     }
	/// }
	/// ```
	async fn get_storage_data_dynamic_batch_size(
		client: &Client,
		worker_index: usize,
		payloads: &[(String, ArrayParams)],
		bar: &ProgressBar,
		batch_size: usize,
	) -> std::result::Result<Vec<Option<StorageData>>, String> {
		let mut all_data: Vec<Option<StorageData>> = vec![];
		let mut start_index = 0;
		let total_payloads = payloads.len();

		while start_index < total_payloads {
			let end_index = usize::min(start_index + batch_size, total_payloads);
			let page = &payloads[start_index..end_index];

			trace!(
				target: LOG_TARGET,
				"Worker {worker_index}: fetching values {start_index}..{end_index} of {total_payloads}",
			);

			// Build the batch request
			let mut batch = BatchRequestBuilder::new();
			for (method, params) in page.iter() {
				if batch.insert(method, params.clone()).is_err() {
					panic!("Invalid batch method and/or params; qed");
				}
			}

			let rpc_result = with_timeout(
				client.ws_client.batch_request::<Option<StorageData>>(batch),
				RPC_TIMEOUT,
			)
			.await;

			let batch_response = match rpc_result {
				Ok(Ok(r)) => r,
				Ok(Err(e)) => return Err(format!("RPC error: {e:?}")),
				Err(()) => return Err("timeout".to_string()),
			};

			let batch_response_len = batch_response.len();
			for item in batch_response.into_iter() {
				match item {
					Ok(x) => all_data.push(x),
					Err(e) => return Err(format!("batch item error: {}", e.message())),
				}
			}
			bar.inc(batch_response_len as u64);

			start_index = end_index;
		}

		Ok(all_data)
	}

	/// Synonym of `getPairs` that uses paged queries to first get the keys, and then
	/// map them to values one by one.
	///
	/// This can work with public nodes. But, expect it to be darn slow.
	pub(crate) async fn rpc_get_pairs(
		&self,
		prefix: StorageKey,
		at: B::Hash,
		pending_ext: &mut TestExternalities<HashingFor<B>>,
	) -> Result<Vec<KeyValue>> {
		let parallel = self.parallel_requests().await;
		let keys = logging::with_elapsed_async(
			|| async { self.rpc_get_keys_parallel(&prefix, at, parallel).await },
			"Scraping keys...",
			|keys| format!("Found {} keys", keys.len()),
		)
		.await?;

		if keys.is_empty() {
			return Ok(Default::default());
		}

		let conn_manager = self.conn_manager()?;

		let payloads = keys
			.iter()
			.map(|key| ("state_getStorage".to_string(), rpc_params!(key, at)))
			.collect::<Vec<_>>();

		let bar = ProgressBar::new(payloads.len() as u64);
		bar.enable_steady_tick(Duration::from_secs(1));
		bar.set_message("Downloading key values".to_string());
		bar.set_style(
			ProgressStyle::with_template(
				"[{elapsed_precise}] {msg} {per_sec} [{wide_bar}] {pos}/{len} ({eta})",
			)
			.unwrap()
			.progress_chars("=>-"),
		);

		// Create batches of payloads for dynamic work distribution
		// Each batch is: (start_index, payloads, batch_size)
		const BATCH_SIZE: usize = 1000;
		let batches: VecDeque<_> = payloads
			.chunks(BATCH_SIZE)
			.enumerate()
			.map(|(i, chunk)| (i * BATCH_SIZE, chunk.to_vec(), BATCH_SIZE))
			.collect();

		info!(target: LOG_TARGET, "🔧 Initialized {} batches for value fetching", batches.len());
		info!(target: LOG_TARGET, "🚀 Spawning {parallel} parallel workers for value fetching");

		let results: Arc<Mutex<Vec<Option<StorageData>>>> =
			Arc::new(Mutex::new(vec![None; payloads.len()]));
		let results_for_extraction = results.clone();
		let bar_for_finish = bar.clone();

		run_workers(
			batches,
			conn_manager,
			parallel,
			move |worker_index, (start_index, batch, batch_size), client| {
				let results = results.clone();
				let bar = bar.clone();

				async move {
					debug!(
						target: LOG_TARGET,
						"Value worker {worker_index}: Processing batch at {start_index} with {} payloads",
						batch.len()
					);

					match Self::get_storage_data_dynamic_batch_size(
						&client,
						worker_index,
						&batch,
						&bar,
						batch_size,
					)
					.await
					{
						Ok(batch_results) => {
							let mut results_lock = results.lock().unwrap();
							for (offset, result) in batch_results.into_iter().enumerate() {
								results_lock[start_index + offset] = result;
							}
							ProcessResult::Success { new_work: vec![] }
						},
						Err(e) => {
							debug!(target: LOG_TARGET, "Value worker {worker_index}: failed: {e:?}");
							let new_batch_size = (batch_size / 2).max(10);
							ProcessResult::Retry {
								work: (start_index, batch, new_batch_size),
								sleep_duration: Duration::from_secs(15),
								action: retry_action(&e),
							}
						},
					}
				}
			},
		)
		.await;

		let storage_data = results_for_extraction.lock().unwrap().clone();

		bar_for_finish.finish_with_message("✅ Downloaded key values");
		println!();

		// Check if we got responses for all submitted requests.
		assert_eq!(keys.len(), storage_data.len());

		// Filter out None values - keys without values should NOT be inserted
		// (inserting with empty value would change the trie structure)
		let key_values: Vec<_> = keys
			.iter()
			.zip(storage_data)
			.filter_map(|(key, maybe_value)| maybe_value.map(|data| (key.clone(), data)))
			.collect();

		logging::with_elapsed(
			|| {
				pending_ext.batch_insert(key_values.clone().into_iter().filter_map(|(k, v)| {
					// Don't insert the child keys here, they need to be inserted separately with
					// all their data in the load_child_remote function.
					match is_default_child_storage_key(&k.0) {
						true => None,
						false => Some((k.0, v.0)),
					}
				}));

				Ok(())
			},
			"Inserting keys into DB...",
			|_| "Inserted keys into DB".into(),
		)
		.expect("must succeed; qed");

		Ok(key_values)
	}

	/// Get the values corresponding to `child_keys` at the given `prefixed_top_key`.
	pub(crate) async fn rpc_child_get_storage_paged(
		client: &Client,
		prefixed_top_key: &StorageKey,
		child_keys: Vec<StorageKey>,
		at: B::Hash,
	) -> Result<Vec<KeyValue>, String> {
		let payloads: Vec<_> = child_keys
			.iter()
			.map(|key| {
				(
					"childstate_getStorage".to_string(),
					rpc_params![
						PrefixedStorageKey::new(prefixed_top_key.as_ref().to_vec()),
						key,
						at
					],
				)
			})
			.collect();

		let bar = ProgressBar::new(payloads.len() as u64);
		let storage_data =
			Self::get_storage_data_dynamic_batch_size(client, 0, &payloads, &bar, 1000).await?;

		// Filter out None values
		Ok(child_keys
			.into_iter()
			.zip(storage_data)
			.filter_map(|(key, maybe_value)| maybe_value.map(|v| (key, v)))
			.collect())
	}
}

impl<B: BlockT> Builder<B>
where
	B::Hash: DeserializeOwned,
	B::Header: DeserializeOwned,
{
	/// Fetch all keys and values for a single child trie.
	async fn fetch_single_child_trie(
		client: &Client,
		prefixed_top_key: &StorageKey,
		at: B::Hash,
	) -> Result<(ChildInfo, Vec<KeyValue>), String> {
		let top_key = PrefixedStorageKey::new(prefixed_top_key.0.clone());
		let page_size = 1000u32;

		trace!(
			target: LOG_TARGET,
			"Fetching child trie keys for {:?}",
			HexDisplay::from(&prefixed_top_key.0)
		);

		// Fetch all keys for this child trie
		let mut child_keys = Vec::new();
		let mut start_key: Option<StorageKey> = None;

		loop {
			let rpc_result = with_timeout(
				substrate_rpc_client::ChildStateApi::storage_keys_paged(
					client.ws_client.as_ref(),
					top_key.clone(),
					Some(StorageKey(vec![])),
					page_size,
					start_key.clone(),
					Some(at),
				),
				RPC_TIMEOUT,
			)
			.await;

			let page = match rpc_result {
				Ok(Ok(p)) => p,
				Ok(Err(e)) => {
					debug!(target: LOG_TARGET, "Child trie RPC error: {e:?}");
					return Err(format!("rpc child_get_keys failed: {e:?}"));
				},
				Err(()) => {
					debug!(target: LOG_TARGET, "Child trie RPC timeout");
					return Err("rpc child_get_keys timeout".to_string());
				},
			};

			let is_full_batch = page.len() == page_size as usize;
			start_key = page.last().cloned();
			child_keys.extend(page);

			if !is_full_batch {
				break;
			}
		}

		// Fetch values for all keys
		let child_kv =
			Self::rpc_child_get_storage_paged(client, prefixed_top_key, child_keys, at).await?;

		// Parse the child info
		let un_prefixed = match ChildType::from_prefixed_key(&top_key) {
			Some((ChildType::ParentKeyId, storage_key)) => storage_key,
			None => return Err("invalid child key".to_string()),
		};

		Ok((ChildInfo::new_default(un_prefixed), child_kv))
	}

	/// Load all of the child keys from the remote config, given the already scraped list of top key
	/// pairs.
	///
	/// `top_kv` need not be only child-bearing top keys. It should be all of the top keys that are
	/// included thus far.
	///
	/// This function uses parallel workers to fetch child tries concurrently.
	async fn load_child_remote(
		&self,
		top_kv: &[KeyValue],
		pending_ext: &mut TestExternalities<HashingFor<B>>,
	) -> Result<ChildKeyValues> {
		let child_roots: VecDeque<StorageKey> = top_kv
			.iter()
			.filter(|(k, _)| is_default_child_storage_key(k.as_ref()))
			.map(|(k, _)| k.clone())
			.collect();

		if child_roots.is_empty() {
			info!(target: LOG_TARGET, "👩‍👦 no child roots found to scrape");
			return Ok(Default::default());
		}

		let total_count = child_roots.len();
		info!(
			target: LOG_TARGET,
			"👩‍👦 scraping child-tree data from {} child tries",
			total_count,
		);

		let at = self.as_online().at_expected();
		let conn_manager = self.conn_manager()?;
		let parallel = self.parallel_requests().await;

		let results: Arc<Mutex<Vec<(ChildInfo, Vec<KeyValue>)>>> = Arc::new(Mutex::new(Vec::new()));
		let results_for_extraction = results.clone();
		let completed_count = Arc::new(AtomicUsize::new(0));

		run_workers(
			child_roots,
			conn_manager,
			parallel,
			move |worker_index, prefixed_top_key, client| {
				let results = results.clone();
				let completed_count = completed_count.clone();

				async move {
					match Self::fetch_single_child_trie(&client, &prefixed_top_key, at).await {
						Ok((info, child_kv_inner)) => {
							results.lock().unwrap().push((info, child_kv_inner));

							let done = completed_count.fetch_add(1, Ordering::SeqCst) + 1;
							if done.is_multiple_of(100) || done == total_count {
								info!(
									target: LOG_TARGET,
									"👩‍👦 Child tries progress: {}/{} completed",
									done,
									total_count
								);
							}

							ProcessResult::Success { new_work: vec![] }
						},
						Err(e) => {
							error!(target: LOG_TARGET, "Worker {worker_index}: Failed: {e:?}");
							ProcessResult::Retry {
								work: prefixed_top_key,
								sleep_duration: Duration::from_secs(5),
								action: retry_action(&e),
							}
						},
					}
				}
			},
		)
		.await;

		// Extract results and populate pending_ext
		let child_kv_results = results_for_extraction.lock().unwrap().clone();

		let mut child_kv = Vec::new();
		for (info, kv_inner) in child_kv_results {
			let key_values: Vec<(Vec<u8>, Vec<u8>)> =
				kv_inner.iter().cloned().map(|(k, v)| (k.0, v.0)).collect();
			for (k, v) in key_values {
				pending_ext.insert_child(info.clone(), k, v);
			}
			child_kv.push((info, kv_inner));
		}

		info!(
			target: LOG_TARGET,
			"👩‍👦 Completed scraping {} child tries",
			child_kv.len()
		);

		Ok(child_kv)
	}

	/// Build `Self` from a network node denoted by `uri`.
	///
	/// This function concurrently populates `pending_ext`. the return value is only for writing to
	/// cache, we can also optimize further.
	async fn load_top_remote(
		&self,
		pending_ext: &mut TestExternalities<HashingFor<B>>,
	) -> Result<TopKeyValues> {
		let config = self.as_online();
		let at = self
			.as_online()
			.at
			.expect("online config must be initialized by this point; qed.");
		info!(target: LOG_TARGET, "scraping key-pairs from remote at block height {at:?}");

		let mut keys_and_values = Vec::new();
		for prefix in &config.hashed_prefixes {
			let now = std::time::Instant::now();
			let additional_key_values =
				self.rpc_get_pairs(StorageKey(prefix.to_vec()), at, pending_ext).await?;
			let elapsed = now.elapsed();
			info!(
				target: LOG_TARGET,
				"adding data for hashed prefix: {:?}, took {:.2}s",
				HexDisplay::from(prefix),
				elapsed.as_secs_f32()
			);
			keys_and_values.extend(additional_key_values);
		}

		for key in &config.hashed_keys {
			let key = StorageKey(key.to_vec());
			info!(
				target: LOG_TARGET,
				"adding data for hashed key: {:?}",
				HexDisplay::from(&key)
			);
			match self.rpc_get_storage(key.clone(), Some(at)).await? {
				Some(value) => {
					pending_ext.insert(key.clone().0, value.clone().0);
					keys_and_values.push((key, value));
				},
				None => {
					warn!(
						target: LOG_TARGET,
						"no data found for hashed key: {:?}",
						HexDisplay::from(&key)
					);
				},
			}
		}

		Ok(keys_and_values)
	}

	/// The entry point of execution, if `mode` is online.
	///
	/// Initializes the remote clients and sets the `at` field if not specified.
	async fn init_remote_client(&mut self) -> Result<()> {
		// First, create all clients from URIs, filtering out ones that fail to connect.
		let online_config = self.as_online();
		let mut clients = Vec::new();
		for uri in &online_config.transport_uris {
			if let Some(client) = Client::new(uri.clone()).await {
				clients.push((uri.clone(), Arc::new(tokio::sync::Mutex::new(client))));
			}
		}
		self.conn_manager = Some(ConnectionManager::new(clients)?);

		// Then, if `at` is not set, set it.
		if self.as_online().at.is_none() {
			let at = self.rpc_get_head().await?;
			info!(
				target: LOG_TARGET,
				"since no at is provided, setting it to latest finalized head, {at:?}",
			);
			self.as_online_mut().at = Some(at);
		}

		// Then, a few transformation that we want to perform in the online config:
		let online_config = self.as_online_mut();
		online_config.pallets.iter().for_each(|p| {
			online_config
				.hashed_prefixes
				.push(sp_crypto_hashing::twox_128(p.as_bytes()).to_vec())
		});

		if online_config.child_trie {
			online_config.hashed_prefixes.push(DEFAULT_CHILD_STORAGE_KEY_PREFIX.to_vec());
		}

		// Finally, if by now, we have put any limitations on prefixes that we are interested in, we
		// download everything.
		if online_config
			.hashed_prefixes
			.iter()
			.filter(|p| *p != DEFAULT_CHILD_STORAGE_KEY_PREFIX)
			.count() == 0
		{
			info!(
				target: LOG_TARGET,
				"since no prefix is filtered, the data for all pallets will be downloaded"
			);
			online_config.hashed_prefixes.push(vec![]);
		}

		Ok(())
	}

	/// Load the header for the target block. Tries each client until one succeeds.
	async fn load_header(&self) -> Result<B::Header> {
		let conn_manager = self.conn_manager()?;
		let at = self.as_online().at_expected();

		let num_clients = conn_manager.num_clients().await;
		for i in 0..num_clients {
			let client = conn_manager.get(i).await;
			let result = with_timeout(
				ChainApi::<(), _, B::Header, ()>::header(client.ws_client.as_ref(), Some(at)),
				RPC_TIMEOUT,
			)
			.await;

			match result {
				Ok(Ok(Some(header))) => return Ok(header),
				Ok(Ok(None)) => {
					debug!(target: LOG_TARGET, "Client {i}: header returned None");
				},
				Ok(Err(e)) => {
					debug!(target: LOG_TARGET, "Client {i}: header RPC error: {e:?}");
				},
				Err(()) => {
					debug!(target: LOG_TARGET, "Client {i}: header timeout");
				},
			}
		}

		Err("rpc header failed on all clients")
	}

	/// Load the data from a remote server. The main code path is calling into `load_top_remote` and
	/// `load_child_remote`.
	///
	/// Must be called after `init_remote_client`.
	async fn load_remote_and_maybe_save(&mut self) -> Result<TestExternalities<HashingFor<B>>> {
		let state_version = self.fetch_state_version().await?;
		let mut pending_ext = TestExternalities::new_with_code_and_state(
			Default::default(),
			Default::default(),
			self.overwrite_state_version.unwrap_or(state_version),
		);

		// Load data from the remote into `pending_ext`.
		let top_kv = self.load_top_remote(&mut pending_ext).await?;
		self.load_child_remote(&top_kv, &mut pending_ext).await?;

		let header = self.load_header().await?;
		let (raw_storage, computed_root) = pending_ext.into_raw_snapshot();

		// Verify the downloaded state against the header's state root. Only a complete scrape can
		// reproduce it, so partial scrapes are exempt. An overwritten state version is *not*
		// exempt: a mismatch then means the overwrite is wrong, which callers opt out of via
		// `disable_root_check`.
		if self.as_online().disable_root_check {
			warn!(
				target: LOG_TARGET,
				"⚠️ skipping storage root verification (disable_root_check is set)",
			);
		} else if self.is_complete_scrape() {
			let expected_root = *header.state_root();
			if computed_root != expected_root {
				error!(
					target: LOG_TARGET,
					"❌ storage root mismatch: computed {computed_root:?}, expected {expected_root:?} \
					(from header). The downloaded state is incomplete or corrupted. If you are \
					overwriting the state version, set `disable_root_check` to bypass this check.",
				);
				return Err("storage root mismatch: downloaded state is incomplete or corrupted");
			}
			info!(target: LOG_TARGET, "✅ storage root verified against header: {computed_root:?}");
		} else {
			debug!(
				target: LOG_TARGET,
				"skipping storage root verification for partial scrape (no full-state prefix)",
			);
		}

		// If we need to save a snapshot, save the raw storage and root hash to the snapshot.
		if let Some(path) = self.as_online().state_snapshot.clone().map(|c| c.path) {
			let snapshot =
				Snapshot::<B>::new(state_version, raw_storage.clone(), computed_root, header);
			let encoded = snapshot.encode();
			info!(
				target: LOG_TARGET,
				"writing snapshot of {} bytes to {path:?}",
				encoded.len(),
			);
			std::fs::write(path, encoded).map_err(|_| "fs::write failed")?;
		}

		// Return the externalities (reconstructed from verified snapshot)
		Ok(TestExternalities::from_raw_snapshot(
			raw_storage,
			computed_root,
			self.overwrite_state_version.unwrap_or(state_version),
		))
	}

	async fn do_load_remote(&mut self) -> Result<RemoteExternalities<B>> {
		self.init_remote_client().await?;
		let inner_ext = self.load_remote_and_maybe_save().await?;
		Ok(RemoteExternalities { header: self.load_header().await?, inner_ext })
	}

	fn do_load_offline(&mut self, config: OfflineConfig) -> Result<RemoteExternalities<B>> {
		let (header, inner_ext) = logging::with_elapsed(
			|| {
				info!(target: LOG_TARGET, "Loading snapshot from {:?}", &config.state_snapshot.path);

				let Snapshot { header, state_version, raw_storage, storage_root, .. } =
					Snapshot::<B>::load(&config.state_snapshot.path)?;
				let inner_ext = TestExternalities::from_raw_snapshot(
					raw_storage,
					storage_root,
					self.overwrite_state_version.unwrap_or(state_version),
				);

				Ok((header, inner_ext))
			},
			"Loading snapshot...",
			|_| "Loaded snapshot".into(),
		)?;

		Ok(RemoteExternalities { inner_ext, header })
	}

	pub(crate) async fn pre_build(mut self) -> Result<RemoteExternalities<B>> {
		let mut ext = match self.mode.clone() {
			Mode::Offline(config) => self.do_load_offline(config)?,
			Mode::Online(_) => self.do_load_remote().await?,
			Mode::OfflineOrElseOnline(offline_config, _) => {
				match self.do_load_offline(offline_config) {
					Ok(x) => x,
					Err(_) => self.do_load_remote().await?,
				}
			},
		};

		// inject manual key values.
		if !self.hashed_key_values.is_empty() {
			info!(
				target: LOG_TARGET,
				"extending externalities with {} manually injected key-values",
				self.hashed_key_values.len()
			);
			ext.batch_insert(self.hashed_key_values.into_iter().map(|(k, v)| (k.0, v.0)));
		}

		// exclude manual key values.
		if !self.hashed_blacklist.is_empty() {
			info!(
				target: LOG_TARGET,
				"excluding externalities from {} keys",
				self.hashed_blacklist.len()
			);
			for k in self.hashed_blacklist {
				ext.execute_with(|| sp_io::storage::clear(&k));
			}
		}

		Ok(ext)
	}
}

// Public methods
impl<B: BlockT> Builder<B>
where
	B::Hash: DeserializeOwned,
	B::Header: DeserializeOwned,
{
	/// Create a new builder.
	pub fn new() -> Self {
		Default::default()
	}

	/// Inject a manual list of key and values to the storage.
	pub fn inject_hashed_key_value(mut self, injections: Vec<KeyValue>) -> Self {
		self.hashed_key_values.extend(injections);
		self
	}

	/// Blacklist this hashed key from the final externalities. This is treated as-is, and should be
	/// pre-hashed.
	pub fn blacklist_hashed_key(mut self, hashed: &[u8]) -> Self {
		self.hashed_blacklist.push(hashed.to_vec());
		self
	}

	/// Configure a state snapshot to be used.
	pub fn mode(mut self, mode: Mode<B::Hash>) -> Self {
		self.mode = mode;
		self
	}

	/// The state version to use.
	pub fn overwrite_state_version(mut self, version: StateVersion) -> Self {
		self.overwrite_state_version = Some(version);
		self
	}

	pub async fn build(self) -> Result<RemoteExternalities<B>> {
		let mut ext = self.pre_build().await?;
		ext.commit_all().unwrap();

		info!(
			target: LOG_TARGET,
			"initialized state externalities with storage root {:?} and state_version {:?}",
			ext.as_backend().root(),
			ext.state_version
		);

		Ok(ext)
	}
}

#[cfg(test)]
mod test_prelude {
	pub(crate) use super::*;
	pub(crate) use sp_runtime::testing::{Block as RawBlock, MockCallU64};
	pub(crate) type UncheckedXt = sp_runtime::testing::TestXt<MockCallU64, ()>;
	pub(crate) type Block = RawBlock<UncheckedXt>;

	pub(crate) fn init_logger() {
		sp_tracing::try_init_simple();
	}
}

#[cfg(test)]
mod tests {
	use super::test_prelude::*;

	#[tokio::test]
	async fn can_load_state_snapshot() {
		init_logger();
		Builder::<Block>::new()
			.mode(Mode::Offline(OfflineConfig {
				state_snapshot: SnapshotConfig::new("test_data/test.snap"),
			}))
			.build()
			.await
			.unwrap()
			.execute_with(|| {});
	}

	#[tokio::test]
	async fn can_exclude_from_snapshot() {
		init_logger();

		// get the first key from the snapshot file.
		let some_key = Builder::<Block>::new()
			.mode(Mode::Offline(OfflineConfig {
				state_snapshot: SnapshotConfig::new("test_data/test.snap"),
			}))
			.build()
			.await
			.expect("Can't read state snapshot file")
			.execute_with(|| {
				let key =
					sp_io::storage::next_key(&[]).expect("some key must exist in the snapshot");
				assert!(sp_io::storage::get(&key).is_some());
				key
			});

		Builder::<Block>::new()
			.mode(Mode::Offline(OfflineConfig {
				state_snapshot: SnapshotConfig::new("test_data/test.snap"),
			}))
			.blacklist_hashed_key(&some_key)
			.build()
			.await
			.expect("Can't read state snapshot file")
			.execute_with(|| assert!(sp_io::storage::get(&some_key).is_none()));
	}
}

#[cfg(all(test, feature = "remote-test"))]
mod remote_tests {
	use super::test_prelude::*;
	use frame_support::storage::KeyPrefixIterator;
	use std::{env, os::unix::fs::MetadataExt, path::Path};

	fn endpoint() -> String {
		env::var("TEST_WS").unwrap_or_else(|_| DEFAULT_WS_ENDPOINT.to_string())
	}

	#[tokio::test]
	async fn state_version_is_kept_and_can_be_altered() {
		const CACHE: &'static str = "state_version_is_kept_and_can_be_altered";
		init_logger();

		// first, build a snapshot.
		let ext = Builder::<Block>::new()
			.mode(Mode::Online(OnlineConfig {
				transport_uris: vec![endpoint().clone()],
				pallets: vec!["Proxy".to_owned()],
				child_trie: false,
				state_snapshot: Some(SnapshotConfig::new(CACHE)),
				..Default::default()
			}))
			.build()
			.await
			.unwrap();

		// now re-create the same snapshot.
		let cached_ext = Builder::<Block>::new()
			.mode(Mode::Offline(OfflineConfig { state_snapshot: SnapshotConfig::new(CACHE) }))
			.build()
			.await
			.unwrap();

		assert_eq!(ext.state_version, cached_ext.state_version);

		// now overwrite it
		let other = match ext.state_version {
			StateVersion::V0 => StateVersion::V1,
			StateVersion::V1 => StateVersion::V0,
		};
		let cached_ext = Builder::<Block>::new()
			.mode(Mode::Offline(OfflineConfig { state_snapshot: SnapshotConfig::new(CACHE) }))
			.overwrite_state_version(other)
			.build()
			.await
			.unwrap();

		assert_eq!(cached_ext.state_version, other);
	}

	#[tokio::test]
	async fn snapshot_block_hash_works() {
		const CACHE: &'static str = "snapshot_block_hash_works";
		init_logger();

		// first, build a snapshot.
		let ext = Builder::<Block>::new()
			.mode(Mode::Online(OnlineConfig {
				transport_uris: vec![endpoint().clone()],
				pallets: vec!["Proxy".to_owned()],
				child_trie: false,
				state_snapshot: Some(SnapshotConfig::new(CACHE)),
				..Default::default()
			}))
			.build()
			.await
			.unwrap();

		// now re-create the same snapshot.
		let cached_ext = Builder::<Block>::new()
			.mode(Mode::Offline(OfflineConfig { state_snapshot: SnapshotConfig::new(CACHE) }))
			.build()
			.await
			.unwrap();

		assert_eq!(ext.header.hash(), cached_ext.header.hash());
	}

	#[tokio::test]
	async fn child_keys_are_loaded() {
		const CACHE: &'static str = "snapshot_retains_storage";
		init_logger();

		// This test does not rely on the remote endpoint having child tries. A synthetic child
		// storage entry is inserted locally and then asserted on.
		use sp_state_machine::Backend;

		// Create an externality with child trie scraping enabled.
		let mut child_ext = Builder::<Block>::new()
			.mode(Mode::Online(OnlineConfig {
				transport_uris: vec![endpoint().clone()],
				pallets: vec!["Proxy".to_owned()],
				child_trie: true,
				state_snapshot: Some(SnapshotConfig::new(CACHE)),
				..Default::default()
			}))
			.build()
			.await
			.unwrap();

		// Create an externality without looking for children keys
		let mut ext = Builder::<Block>::new()
			.mode(Mode::Online(OnlineConfig {
				transport_uris: vec![endpoint().clone()],
				pallets: vec!["Proxy".to_owned()],
				child_trie: false,
				state_snapshot: Some(SnapshotConfig::new(CACHE)),
				..Default::default()
			}))
			.build()
			.await
			.unwrap();

		// Generate artificial child storage entry, to ensure the test's assertion is valid.
		let child_info = sp_core::storage::ChildInfo::new_default(b"test_child");
		let child_key: Vec<u8> = b"k1".to_vec();
		let child_value: Vec<u8> = b"v1".to_vec();

		// Record the size of the underlying trie DB before inserting the child entry.
		let child_db_keys_before = child_ext.as_backend().backend_storage().keys().len();

		// Insert child storage only into `child_ext`.
		child_ext.insert_child(child_info.clone(), child_key.clone(), child_value.clone());

		// Assert: the child key exists only in the externalities where it is inserted.
		let child_backend = child_ext.as_backend();
		let backend = ext.as_backend();
		assert_eq!(
			child_backend.child_storage(&child_info, &child_key).unwrap(),
			Some(child_value)
		);
		assert_eq!(backend.child_storage(&child_info, &child_key).unwrap(), None);

		// Structural assertion: insertion increased the underlying DB entry count.
		let child_db_keys_after = child_backend.backend_storage().keys().len();
		assert!(child_db_keys_after > child_db_keys_before);
	}

	#[tokio::test]
	async fn offline_else_online_works() {
		const CACHE: &'static str = "offline_else_online_works_data";
		init_logger();
		// this shows that in the second run, we use the remote and create a snapshot.
		Builder::<Block>::new()
			.mode(Mode::OfflineOrElseOnline(
				OfflineConfig { state_snapshot: SnapshotConfig::new(CACHE) },
				OnlineConfig {
					transport_uris: vec![endpoint().clone()],
					pallets: vec!["Proxy".to_owned()],
					child_trie: false,
					state_snapshot: Some(SnapshotConfig::new(CACHE)),
					..Default::default()
				},
			))
			.build()
			.await
			.unwrap()
			.execute_with(|| {});

		// this shows that in the second run, we are not using the remote
		Builder::<Block>::new()
			.mode(Mode::OfflineOrElseOnline(
				OfflineConfig { state_snapshot: SnapshotConfig::new(CACHE) },
				OnlineConfig {
					transport_uris: vec!["ws://non-existent:666".to_owned()],
					..Default::default()
				},
			))
			.build()
			.await
			.unwrap()
			.execute_with(|| {});

		let to_delete = std::fs::read_dir(Path::new("."))
			.unwrap()
			.into_iter()
			.map(|d| d.unwrap())
			.filter(|p| p.path().file_name().unwrap_or_default() == CACHE)
			.collect::<Vec<_>>();

		assert!(to_delete.len() == 1);
		std::fs::remove_file(to_delete[0].path()).unwrap();
	}

	#[tokio::test]
	async fn can_build_one_small_pallet() {
		init_logger();
		Builder::<Block>::new()
			.mode(Mode::Online(OnlineConfig {
				transport_uris: vec![endpoint().clone()],
				pallets: vec!["Proxy".to_owned()],
				child_trie: false,
				..Default::default()
			}))
			.build()
			.await
			.unwrap()
			.execute_with(|| {});
	}

	#[tokio::test]
	async fn can_build_few_pallet() {
		init_logger();
		Builder::<Block>::new()
			.mode(Mode::Online(OnlineConfig {
				transport_uris: vec![endpoint().clone()],
				pallets: vec!["Proxy".to_owned(), "Multisig".to_owned()],
				child_trie: false,
				..Default::default()
			}))
			.build()
			.await
			.unwrap()
			.execute_with(|| {});
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn can_create_snapshot() {
		const CACHE: &'static str = "can_create_snapshot";
		init_logger();

		Builder::<Block>::new()
			.mode(Mode::Online(OnlineConfig {
				transport_uris: vec![endpoint().clone()],
				state_snapshot: Some(SnapshotConfig::new(CACHE)),
				pallets: vec!["Proxy".to_owned()],
				child_trie: false,
				..Default::default()
			}))
			.build()
			.await
			.unwrap()
			.execute_with(|| {});

		let to_delete = std::fs::read_dir(Path::new("."))
			.unwrap()
			.into_iter()
			.map(|d| d.unwrap())
			.filter(|p| p.path().file_name().unwrap_or_default() == CACHE)
			.collect::<Vec<_>>();

		assert!(to_delete.len() == 1);
		let to_delete = to_delete.first().unwrap();
		assert!(std::fs::metadata(to_delete.path()).unwrap().size() > 1);
		std::fs::remove_file(to_delete.path()).unwrap();
	}

	#[tokio::test]
	async fn can_create_child_snapshot() {
		const CACHE: &'static str = "can_create_child_snapshot";
		init_logger();
		Builder::<Block>::new()
			.mode(Mode::Online(OnlineConfig {
				transport_uris: vec![endpoint().clone()],
				state_snapshot: Some(SnapshotConfig::new(CACHE)),
				pallets: vec!["Crowdloan".to_owned()],
				child_trie: true,
				..Default::default()
			}))
			.build()
			.await
			.unwrap()
			.execute_with(|| {});

		let to_delete = std::fs::read_dir(Path::new("."))
			.unwrap()
			.into_iter()
			.map(|d| d.unwrap())
			.filter(|p| p.path().file_name().unwrap_or_default() == CACHE)
			.collect::<Vec<_>>();

		assert!(to_delete.len() == 1);
		let to_delete = to_delete.first().unwrap();
		assert!(std::fs::metadata(to_delete.path()).unwrap().size() > 1);
		std::fs::remove_file(to_delete.path()).unwrap();
	}

	#[tokio::test]
	async fn can_build_big_pallet() {
		if std::option_env!("TEST_WS").is_none() {
			return;
		}
		init_logger();
		Builder::<Block>::new()
			.mode(Mode::Online(OnlineConfig {
				transport_uris: vec![endpoint().clone()],
				pallets: vec!["Staking".to_owned()],
				child_trie: false,
				..Default::default()
			}))
			.build()
			.await
			.unwrap()
			.execute_with(|| {});
	}

	#[tokio::test]
	async fn can_fetch_all() {
		if std::option_env!("TEST_WS").is_none() {
			return;
		}
		init_logger();
		Builder::<Block>::new()
			.mode(Mode::Online(OnlineConfig {
				transport_uris: vec![endpoint().clone()],
				..Default::default()
			}))
			.build()
			.await
			.unwrap()
			.execute_with(|| {});
	}

	#[tokio::test]
	async fn can_fetch_in_parallel() {
		init_logger();

		let mut builder = Builder::<Block>::new().mode(Mode::Online(OnlineConfig {
			transport_uris: vec![endpoint().clone()],
			..Default::default()
		}));
		builder.init_remote_client().await.unwrap();

		let at = builder.as_online().at.unwrap();

		// Test with a specific prefix
		let prefix = StorageKey(vec![13]);
		let para = builder.rpc_get_keys_parallel(&prefix, at, 4).await.unwrap();
		assert!(!para.is_empty(), "Should fetch some keys with prefix");

		// Test with empty prefix (all keys)
		let prefix = StorageKey(vec![]);
		let para = builder.rpc_get_keys_parallel(&prefix, at, 8).await.unwrap();
		assert!(!para.is_empty(), "Should fetch some keys with empty prefix");
	}

	#[tokio::test]
	#[ignore] // This test takes a long time, run with --ignored
	async fn bridge_hub_polkadot_storage_root_matches() {
		init_logger();

		// Use multiple RPC providers for load distribution
		let endpoints = vec![
			"wss://bridge-hub-polkadot-rpc.n.dwellir.com",
			"wss://sys.ibp.network/bridgehub-polkadot",
			"wss://bridgehub-polkadot.api.onfinality.io/public",
			"wss://dot-rpc.stakeworld.io/bridgehub",
		];

		info!(target: LOG_TARGET, "Connecting to Bridge Hub Polkadot using {} RPC providers", endpoints.len());

		let mut ext = Builder::<Block>::new()
			.mode(Mode::Online(OnlineConfig {
				transport_uris: endpoints.into_iter().map(|e| e.to_owned()).collect(),
				child_trie: true,
				..Default::default()
			}))
			.build()
			.await
			.expect("Failed to build remote externalities");

		// Get the computed storage root from our downloaded state
		let backend = ext.as_backend();
		let computed_root = *backend.root();
		// Get the expected storage root from the block header
		let expected_root = ext.header.state_root;

		info!(
			target: LOG_TARGET,
			"Computed storage root: {:?}",
			computed_root
		);
		info!(
			target: LOG_TARGET,
			"Expected storage root (from header): {:?}",
			expected_root
		);

		// The storage roots must match exactly - this proves we downloaded all keys correctly
		assert_eq!(
			computed_root, expected_root,
			"Storage root mismatch! Computed: {:?}, Expected: {:?}. \
			This indicates that not all keys were fetched or there were duplicates.",
			computed_root, expected_root
		);

		ext.execute_with(|| {
			let key_count = KeyPrefixIterator::<()>::new(vec![], vec![], |_| Ok(())).count();

			info!(target: LOG_TARGET, "Total keys in state: {}", key_count);
			assert!(key_count > 0, "Should have fetched some keys");
		});

		info!(
			target: LOG_TARGET,
			"✅ Storage root verification successful! All keys were fetched correctly."
		);
	}

	#[tokio::test]
	#[ignore]
	async fn asset_hub_polkadot_storage_root_matches() {
		init_logger();

		// Asset Hub carries the largest system-parachain state (incl. child tries), so a full
		// scrape is the strongest end-to-end check that the download matches the on-chain storage
		// root.
		let endpoints = vec![
			"wss://asset-hub-polkadot-rpc.dwellir.com",
			"wss://sys.ibp.network/asset-hub-polkadot",
			"wss://asset-hub-polkadot.api.onfinality.io/public",
			"wss://dot-rpc.stakeworld.io/assethub",
		];

		info!(target: LOG_TARGET, "Connecting to Asset Hub Polkadot using {} RPC providers", endpoints.len());

		let mut ext = Builder::<Block>::new()
			.mode(Mode::Online(OnlineConfig {
				transport_uris: endpoints.into_iter().map(|e| e.to_owned()).collect(),
				child_trie: true,
				..Default::default()
			}))
			.build()
			.await
			.expect("Failed to build remote externalities");

		let backend = ext.as_backend();
		let computed_root = *backend.root();
		let expected_root = ext.header.state_root;

		info!(target: LOG_TARGET, "Computed storage root: {:?}", computed_root);
		info!(target: LOG_TARGET, "Expected storage root (from header): {:?}", expected_root);

		assert_eq!(
			computed_root, expected_root,
			"Storage root mismatch! Computed: {:?}, Expected: {:?}. \
			This indicates that not all keys were fetched or there were duplicates.",
			computed_root, expected_root
		);

		ext.execute_with(|| {
			let key_count = KeyPrefixIterator::<()>::new(vec![], vec![], |_| Ok(())).count();

			info!(target: LOG_TARGET, "Total keys in state: {}", key_count);
			assert!(key_count > 0, "Should have fetched some keys");
		});

		info!(
			target: LOG_TARGET,
			"✅ Storage root verification successful! All keys were fetched correctly."
		);
	}

	#[tokio::test]
	async fn builder_fails_with_invalid_transport_uris() {
		init_logger();

		// Using HTTP/HTTPS URIs should fail because Client::new() returns None for non-WS URIs
		let result = Builder::<Block>::new()
			.mode(Mode::Online(OnlineConfig {
				transport_uris: vec!["http://try-runtime.polkadot.io:443".to_string()],
				pallets: vec!["Proxy".to_owned()],
				..Default::default()
			}))
			.build()
			.await;

		match result {
			Err(e) => assert_eq!(e, "At least one client must be provided"),
			Ok(_) => panic!("Expected error but got success"),
		}

		// Multiple invalid URIs should also fail
		let result = Builder::<Block>::new()
			.mode(Mode::Online(OnlineConfig {
				transport_uris: vec![
					"http://try-runtime.polkadot.io:443".to_string(),
					"https://try-runtime.polkadot.io:443".to_string(),
					"garbage".to_string(),
				],
				pallets: vec!["Proxy".to_owned()],
				..Default::default()
			}))
			.build()
			.await;

		match result {
			Err(e) => assert_eq!(e, "At least one client must be provided"),
			Ok(_) => panic!("Expected error but got success"),
		}
	}
}
