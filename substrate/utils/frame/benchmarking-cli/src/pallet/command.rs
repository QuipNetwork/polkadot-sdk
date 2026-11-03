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

use super::{
	types::{ComponentRange, ComponentRangeMap},
	writer, ListOutput, PalletCmd, LOG_TARGET,
};
use crate::{
	pallet::{types::FetchedCode, GenesisBuilderPolicy},
	shared::{
		genesis_state,
		genesis_state::{GenesisStateHandler, SpecGenesisSource, WARN_SPEC_GENESIS_CTOR},
	},
};
use clap::{error::ErrorKind, CommandFactory};
use codec::{Decode, DecodeWithMemTracking, Encode};
use frame_benchmarking::{
	Analysis, BenchmarkBatch, BenchmarkBatchSplitResults, BenchmarkList, BenchmarkParameter,
	BenchmarkResult, BenchmarkSelector,
};
use frame_support::traits::StorageInfo;
use linked_hash_map::LinkedHashMap;
use sc_cli::{execution_method_from_cli, ChainSpec, CliConfiguration, Result, SharedParams};
use sc_client_db::BenchmarkingState;
use sc_executor::{HeapAllocStrategy, WasmExecutor, DEFAULT_HEAP_ALLOC_STRATEGY};
use serde_json::Value;
use sp_core::{
	offchain::{
		testing::{TestOffchainExt, TestTransactionPoolExt},
		OffchainDbExt, OffchainWorkerExt, TransactionPoolExt,
	},
	traits::{CallContext, CodeExecutor, ReadRuntimeVersionExt, WrappedRuntimeCode},
	Hasher,
};
use sp_externalities::Extensions;
use sp_keystore::{testing::MemoryKeystore, KeystoreExt};
use sp_runtime::traits::Hash;
use sp_state_machine::{backend::TryPendingCode, StateMachine};
use sp_trie::{proof_size_extension::ProofSizeExt, recorder::Recorder};
use sp_wasm_interface::{ExtendedHostFunctions, HostFunctions};
use std::{
	borrow::Cow,
	collections::{BTreeMap, BTreeSet, HashMap},
	fmt::Debug,
	fs,
	str::FromStr,
	time,
	time::Duration,
};

type SubstrateAndExtraHF<T> = (
	ExtendedHostFunctions<
		(sp_io::SubstrateHostFunctions, frame_benchmarking::benchmarking::HostFunctions),
		super::logging::logging::HostFunctions,
	>,
	T,
);
/// How the PoV size of a storage item should be estimated.
#[derive(clap::ValueEnum, Debug, Eq, PartialEq, Clone, Copy)]
pub enum PovEstimationMode {
	/// Use the maximal encoded length as provided by [`codec::MaxEncodedLen`].
	MaxEncodedLen,
	/// Measure the accessed value size in the pallet benchmarking and add some trie overhead.
	Measured,
	/// Do not estimate the PoV size for this storage item or benchmark.
	Ignored,
}

impl FromStr for PovEstimationMode {
	type Err = &'static str;

	fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
		match s {
			"MaxEncodedLen" => Ok(Self::MaxEncodedLen),
			"Measured" => Ok(Self::Measured),
			"Ignored" => Ok(Self::Ignored),
			_ => unreachable!("The benchmark! macro should have prevented this"),
		}
	}
}

/// Maps (pallet, benchmark) -> ((pallet, storage) -> PovEstimationMode)
pub(crate) type PovModesMap =
	HashMap<(String, String), HashMap<(String, String), PovEstimationMode>>;

#[derive(Debug, Clone)]
struct SelectedBenchmark {
	pallet: String,
	instance: String,
	extrinsic: String,
	components: Vec<(BenchmarkParameter, u32, u32)>,
	pov_modes: Vec<(String, String)>,
}

// This takes multiple benchmark batches and combines all the results where the pallet, instance,
// and benchmark are the same.
fn combine_batches(
	time_batches: Vec<BenchmarkBatch>,
	db_batches: Vec<BenchmarkBatch>,
) -> Vec<BenchmarkBatchSplitResults> {
	if time_batches.is_empty() && db_batches.is_empty() {
		return Default::default();
	}

	let mut all_benchmarks =
		LinkedHashMap::<_, (Vec<BenchmarkResult>, Vec<BenchmarkResult>)>::new();

	db_batches
		.into_iter()
		.for_each(|BenchmarkBatch { pallet, instance, benchmark, results }| {
			// We use this key to uniquely identify a benchmark among batches.
			let key = (pallet, instance, benchmark);

			match all_benchmarks.get_mut(&key) {
				// We already have this benchmark, so we extend the results.
				Some(x) => x.1.extend(results),
				// New benchmark, so we add a new entry with the initial results.
				None => {
					all_benchmarks.insert(key, (Vec::new(), results));
				},
			}
		});

	time_batches
		.into_iter()
		.for_each(|BenchmarkBatch { pallet, instance, benchmark, results }| {
			// We use this key to uniquely identify a benchmark among batches.
			let key = (pallet, instance, benchmark);

			match all_benchmarks.get_mut(&key) {
				// We already have this benchmark, so we extend the results.
				Some(x) => x.0.extend(results),
				None => panic!("all benchmark keys should have been populated by db batches"),
			}
		});

	all_benchmarks
		.into_iter()
		.map(|((pallet, instance, benchmark), (time_results, db_results))| {
			BenchmarkBatchSplitResults { pallet, instance, benchmark, time_results, db_results }
		})
		.collect::<Vec<_>>()
}

/// Explains possible reasons why the metadata for the benchmarking could not be found.
const ERROR_API_NOT_FOUND: &'static str = "Did not find the benchmarking runtime api. \
This could mean that you either did not build the node correctly with the \
`--features runtime-benchmarks` flag, or the chain spec that you are using was \
not created by a node that was compiled with the flag";

impl PalletCmd {
	fn state_handler_from_cli<HF: HostFunctions>(
		&self,
		chain_spec_from_api: Option<Box<dyn ChainSpec>>,
	) -> Result<GenesisStateHandler> {
		let genesis_builder_to_source = || match self.genesis_builder {
			Some(GenesisBuilderPolicy::Runtime) | Some(GenesisBuilderPolicy::SpecRuntime) => {
				SpecGenesisSource::Runtime(self.genesis_builder_preset.clone())
			},
			Some(GenesisBuilderPolicy::SpecGenesis) | None => {
				log::warn!(target: LOG_TARGET, "{WARN_SPEC_GENESIS_CTOR}");
				SpecGenesisSource::SpecJson
			},
			Some(GenesisBuilderPolicy::None) => SpecGenesisSource::None,
		};

		// First handle chain-spec passed in via API parameter.
		if let Some(chain_spec) = chain_spec_from_api {
			log::debug!("Initializing state handler with chain-spec from API: {:?}", chain_spec);

			let source = genesis_builder_to_source();
			return Ok(GenesisStateHandler::ChainSpec(chain_spec, source));
		};

		// Handle chain-spec passed in via CLI.
		if let Some(chain_spec_path) = &self.shared_params.chain {
			log::debug!(
				"Initializing state handler with chain-spec from path: {:?}",
				chain_spec_path
			);
			let (chain_spec, _) =
				genesis_state::chain_spec_from_path::<HF>(chain_spec_path.to_string().into())?;

			let source = genesis_builder_to_source();

			return Ok(GenesisStateHandler::ChainSpec(chain_spec, source));
		};

		// Check for runtimes. In general, we make sure that `--runtime` and `--chain` are
		// incompatible on the CLI level.
		if let Some(runtime_path) = &self.runtime {
			log::debug!("Initializing state handler with runtime from path: {:?}", runtime_path);

			let runtime_blob = fs::read(runtime_path)?;
			return if let Some(GenesisBuilderPolicy::None) = self.genesis_builder {
				Ok(GenesisStateHandler::Runtime(runtime_blob, None))
			} else {
				Ok(GenesisStateHandler::Runtime(
					runtime_blob,
					Some(self.genesis_builder_preset.clone()),
				))
			};
		};

		Err("Neither a runtime nor a chain-spec were specified".to_string().into())
	}

	/// Runs the pallet benchmarking command.
	pub fn run_with_spec<Hasher, ExtraHostFunctions>(
		&self,
		chain_spec: Option<Box<dyn ChainSpec>>,
	) -> Result<()>
	where
		Hasher: Hash,
		<Hasher as Hash>::Output: DecodeWithMemTracking,
		ExtraHostFunctions: HostFunctions,
	{
		if let Err((error_kind, msg)) = self.check_args(&chain_spec) {
			let mut cmd = PalletCmd::command();
			cmd.error(error_kind, msg).exit();
		};

		let _d = self.execution.as_ref().map(|exec| {
			// We print the error at the end, since there is often A LOT of output.
			sp_core::defer::DeferGuard::new(move || {
				log::error!(
					target: LOG_TARGET,
					"⚠️  Argument `--execution` is deprecated. Its value of `{exec}` has on effect.",
				)
			})
		});

		if let Some(json_input) = &self.json_input {
			let raw_data = match std::fs::read(json_input) {
				Ok(raw_data) => raw_data,
				Err(error) => {
					return Err(format!("Failed to read {:?}: {}", json_input, error).into())
				},
			};
			let batches: Vec<BenchmarkBatchSplitResults> = match serde_json::from_slice(&raw_data) {
				Ok(batches) => batches,
				Err(error) => {
					return Err(format!("Failed to deserialize {:?}: {}", json_input, error).into())
				},
			};
			return self.output_from_results(&batches);
		}
		super::logging::init(self.runtime_log.clone());

		let state_handler =
			self.state_handler_from_cli::<SubstrateAndExtraHF<ExtraHostFunctions>>(chain_spec)?;

		let genesis_patcher = if let Some(ref patch_path) = self.genesis_patch {
			let patch_content = fs::read_to_string(patch_path)
				.map_err(|e| format!("Failed to read genesis patch file: {}", e))?;
			let patch_value: serde_json::Value = serde_json::from_str(&patch_content)
				.map_err(|e| format!("Failed to parse genesis patch JSON: {}", e))?;

			Some(Box::new(move |mut value| {
				sc_chain_spec::json_patch::merge(&mut value, patch_value);
				value
			}) as Box<dyn FnOnce(Value) -> Value + 'static>)
		} else {
			None
		};

		let genesis_storage = state_handler
			.build_storage::<SubstrateAndExtraHF<ExtraHostFunctions>>(genesis_patcher)?;

		let cache_size = Some(self.database_cache_size as usize);
		let state_with_tracking = BenchmarkingState::<Hasher>::new(
			genesis_storage.clone(),
			cache_size,
			// Record proof size
			true,
			// Enable storage tracking
			true,
		)?;

		let state_without_tracking = BenchmarkingState::<Hasher>::new(
			genesis_storage,
			cache_size,
			// Proof recording depends on CLI settings
			!self.disable_proof_recording,
			// Do not enable storage tracking
			false,
		)?;

		let method =
			execution_method_from_cli(self.wasm_method, self.wasmtime_instantiation_strategy);

		let state = &state_without_tracking;
		let runtime = self.runtime_blob(&state_without_tracking)?;
		let runtime_code = runtime.code()?;
		let alloc_strategy = self.alloc_strategy(runtime_code.heap_pages);

		let executor = WasmExecutor::<SubstrateAndExtraHF<ExtraHostFunctions>>::builder()
			.with_execution_method(method)
			.with_allow_missing_host_functions(self.allow_missing_host_functions)
			.with_onchain_heap_alloc_strategy(alloc_strategy)
			.with_offchain_heap_alloc_strategy(alloc_strategy)
			.with_max_runtime_instances(2)
			.with_runtime_cache_size(2)
			.build();

		let runtime_version: sp_version::RuntimeVersion = Self::exec_state_machine(
			StateMachine::new(
				state,
				&mut Default::default(),
				&executor,
				"Core_version",
				&[],
				&mut Self::build_extensions(executor.clone(), state.recorder()),
				&runtime_code,
				CallContext::Offchain,
			),
			"Could not find `Core::version` runtime api.",
		)?;

		let benchmark_api_version = runtime_version
			.api_version(
				&<dyn frame_benchmarking::Benchmark<
					// We need to use any kind of `Block` type to make the compiler happy, not
					// relevant for the `ID`.
					sp_runtime::generic::Block<
						sp_runtime::generic::Header<u32, Hasher>,
						sp_runtime::generic::UncheckedExtrinsic<(), (), (), ()>,
					>,
				> as sp_api::RuntimeApiInfo>::ID,
			)
			.ok_or_else(|| ERROR_API_NOT_FOUND)?;

		let (list, storage_info): (Vec<BenchmarkList>, Vec<StorageInfo>) =
			Self::exec_state_machine(
				StateMachine::new(
					state,
					&mut Default::default(),
					&executor,
					"Benchmark_benchmark_metadata",
					&(self.extra).encode(),
					&mut Self::build_extensions(executor.clone(), state.recorder()),
					&runtime_code,
					CallContext::Offchain,
				),
				ERROR_API_NOT_FOUND,
			)?;

		// Use the benchmark list and the user input to determine the set of benchmarks to run.
		let benchmarks_to_run = self.select_benchmarks_to_run(list)?;

		if let Some(list_output) = self.list {
			list_benchmark(benchmarks_to_run, list_output, self.no_csv_header);
			return Ok(());
		}

		// Run the benchmarks
		let mut batches = Vec::new();
		let mut batches_db = Vec::new();
		let mut progress_timer = time::Instant::now();
		// Maps (pallet, extrinsic) to its component ranges.
		let mut component_ranges = HashMap::<(String, String), Vec<ComponentRange>>::new();
		let pov_modes =
			Self::parse_pov_modes(&benchmarks_to_run, &storage_info, self.ignore_unknown_pov_mode)?;
		let mut failed = Vec::<(String, String)>::new();

		'outer: for (i, SelectedBenchmark { pallet, instance, extrinsic, components, .. }) in
			benchmarks_to_run.clone().into_iter().enumerate()
		{
			log::info!(
				target: LOG_TARGET,
				"[{: >3} % ] Starting benchmark: {pallet}::{extrinsic}",
				(i * 100) / benchmarks_to_run.len(),
			);
			let all_components = if components.is_empty() {
				vec![Default::default()]
			} else {
				let mut all_components = Vec::new();
				for (idx, (name, low, high)) in components.iter().enumerate() {
					let lowest = self.lowest_range_values.get(idx).cloned().unwrap_or(*low);
					let highest = self.highest_range_values.get(idx).cloned().unwrap_or(*high);

					let diff =
						highest.checked_sub(lowest).ok_or("`low` cannot be higher than `high`")?;

					// The slope logic needs at least two points
					// to compute a slope.
					if self.steps < 2 {
						return Err("`steps` must be at least 2.".into());
					}

					let step_size = (diff as f32 / (self.steps - 1) as f32).max(0.0);

					for s in 0..self.steps {
						// This is the value we will be testing for component `name`
						let component_value =
							((lowest as f32 + step_size * s as f32) as u32).clamp(lowest, highest);

						// Select the max value for all the other components.
						let c: Vec<(BenchmarkParameter, u32)> = components
							.iter()
							.enumerate()
							.map(|(idx, (n, _, h))| {
								if n == name {
									(*n, component_value)
								} else {
									(*n, *self.highest_range_values.get(idx).unwrap_or(h))
								}
							})
							.collect();
						all_components.push(c);
					}

					component_ranges
						.entry((pallet.clone(), extrinsic.clone()))
						.or_default()
						.push(ComponentRange { name: name.to_string(), min: lowest, max: highest });
				}
				all_components
			};

			// Ensure each benchmark runs for at least its minimum duration.
			let start = time::Instant::now();
			let mut first = true;

			loop {
				for (s, selected_components) in all_components.iter().enumerate() {
					let params = |verify: bool, repeats: u32| -> Vec<u8> {
						if benchmark_api_version >= 2 {
							(
								pallet.as_bytes(),
								instance.as_bytes(),
								extrinsic.as_bytes(),
								&selected_components.clone(),
								verify,
								repeats,
							)
								.encode()
						} else {
							(
								pallet.as_bytes(),
								extrinsic.as_bytes(),
								&selected_components.clone(),
								verify,
								repeats,
							)
								.encode()
						}
					};

					// Maybe run a verification if we are the first iteration.
					if !self.no_verify && first {
						let state = &state_without_tracking;
						// Don't use these results since verification code will add overhead.
						let _batch: Vec<BenchmarkBatch> = match Self::exec_state_machine::<
							std::result::Result<Vec<BenchmarkBatch>, String>,
							_,
							_,
						>(
							StateMachine::new(
								state,
								&mut Default::default(),
								&executor,
								"Benchmark_dispatch_benchmark",
								&params(true, 1),
								&mut Self::build_extensions(executor.clone(), state.recorder()),
								&runtime_code,
								CallContext::Offchain,
							),
							"dispatch a benchmark",
						) {
							Err(e) => {
								log::error!(target: LOG_TARGET, "Benchmark {pallet}::{extrinsic} failed: {e}");
								failed.push((pallet.clone(), extrinsic.clone()));
								continue 'outer;
							},
							Ok(Err(e)) => {
								log::error!(target: LOG_TARGET, "Benchmark {pallet}::{extrinsic} failed: {e}");
								failed.push((pallet.clone(), extrinsic.clone()));
								continue 'outer;
							},
							Ok(Ok(b)) => b,
						};
					}
					// Do one loop of DB tracking.
					{
						let state = &state_with_tracking;
						let batch: Vec<BenchmarkBatch> = match Self::exec_state_machine::<
							std::result::Result<Vec<BenchmarkBatch>, String>,
							_,
							_,
						>(
							StateMachine::new(
								state,
								&mut Default::default(),
								&executor,
								"Benchmark_dispatch_benchmark",
								&params(false, 1),
								&mut Self::build_extensions(executor.clone(), state.recorder()),
								&runtime_code,
								CallContext::Offchain,
							),
							"dispatch a benchmark",
						) {
							Err(e) => {
								log::error!(target: LOG_TARGET, "Benchmark {pallet}::{extrinsic} failed: {e}");
								failed.push((pallet.clone(), extrinsic.clone()));
								continue 'outer;
							},
							Ok(Err(e)) => {
								log::error!(target: LOG_TARGET, "Benchmark {pallet}::{extrinsic} failed: {e}");
								failed.push((pallet.clone(), extrinsic.clone()));
								continue 'outer;
							},
							Ok(Ok(b)) => b,
						};

						batches_db.extend(batch);
					}

					// Finally run a bunch of loops to get extrinsic timing information.
					let state = &state_without_tracking;
					let batch = match Self::exec_state_machine::<
						std::result::Result<Vec<BenchmarkBatch>, String>,
						_,
						_,
					>(
						StateMachine::new(
							state,
							&mut Default::default(),
							&executor,
							"Benchmark_dispatch_benchmark",
							&params(false, self.repeat),
							&mut Self::build_extensions(executor.clone(), state.recorder()),
							&runtime_code,
							CallContext::Offchain,
						),
						"dispatch a benchmark",
					) {
						Err(e) => {
							return Err(
								format!("Benchmark {pallet}::{extrinsic} failed: {e}").into()
							);
						},
						Ok(Err(e)) => {
							return Err(
								format!("Benchmark {pallet}::{extrinsic} failed: {e}").into()
							);
						},
						Ok(Ok(b)) => b,
					};

					batches.extend(batch);

					// Show progress information at most every 5 seconds.
					if progress_timer.elapsed() >= time::Duration::from_secs(5) {
						progress_timer = time::Instant::now();

						let msg = if first {
							format!(
								"{}/{}",
								s + 1, // s starts at 0.
								all_components.len()
							)
						} else {
							"(overtime)".to_string()
						};

						log::info!(
							target: LOG_TARGET,
							"[{: >3} % ] Running  benchmark: {pallet}::{extrinsic} {msg}",
							(i * 100) / benchmarks_to_run.len()
						);
					}
				}

				first = false;
				if start.elapsed() >= Duration::from_secs(self.min_duration) {
					break;
				}
			}
		}

		if !failed.is_empty() {
			failed.sort();
			eprintln!(
				"The following {} benchmarks failed:\n{}",
				failed.len(),
				failed.iter().map(|(p, e)| format!("- {p}::{e}")).collect::<Vec<_>>().join("\n")
			);
			return Err(format!("{} benchmarks failed", failed.len()).into());
		}

		// Combine all of the benchmark results, so that benchmarks of the same pallet/function
		// are together.
		let batches = combine_batches(batches, batches_db);
		self.output(&batches, &storage_info, &component_ranges, pov_modes)
	}

	fn select_benchmarks_to_run(&self, list: Vec<BenchmarkList>) -> Result<Vec<SelectedBenchmark>> {
		// Use the benchmark list and the user input to determine the set of benchmarks to run.
		let mut benchmarks_to_run = Vec::new();
		list.iter().filter(|item| self.pallet_selected(&item.pallet)).for_each(|item| {
			for benchmark in &item.benchmarks {
				if self.extrinsic_selected(&item.pallet, &benchmark.name) {
					benchmarks_to_run.push((
						item.pallet.clone(),
						item.instance.clone(),
						benchmark.name.clone(),
						benchmark.components.clone(),
						benchmark.pov_modes.clone(),
					))
				}
			}
		});
		// Convert `Vec<u8>` to `String` for better readability.
		let benchmarks_to_run: Vec<_> = benchmarks_to_run
			.into_iter()
			.map(|(pallet, instance, extrinsic, components, pov_modes)| {
				let pallet = String::from_utf8(pallet).expect("Encoded from String; qed");
				let instance = String::from_utf8(instance).expect("Encoded from String; qed");
				let extrinsic =
					String::from_utf8(extrinsic.clone()).expect("Encoded from String; qed");

				SelectedBenchmark {
					pallet,
					instance,
					extrinsic,
					components,
					pov_modes: pov_modes
						.into_iter()
						.map(|(p, s)| {
							(String::from_utf8(p).unwrap(), String::from_utf8(s).unwrap())
						})
						.collect(),
				}
			})
			.collect();

		if benchmarks_to_run.is_empty() {
			return Err("No benchmarks found which match your input. Try `--list --all` to list all available benchmarks. Make sure pallet is in `define_benchmarks!`".into());
		}

		Ok(benchmarks_to_run)
	}

	/// Whether this pallet should be run.
	fn pallet_selected(&self, pallet: &Vec<u8>) -> bool {
		let include = self.pallets.clone();

		let included = include.is_empty() ||
			include.iter().any(|p| p.as_bytes() == pallet) ||
			include.iter().any(|p| p == "*") ||
			include.iter().any(|p| p == "all");
		let excluded = self.exclude_pallets.iter().any(|p| p.as_bytes() == pallet);

		included && !excluded
	}

	/// Whether this extrinsic should be run.
	fn extrinsic_selected(&self, pallet: &Vec<u8>, extrinsic: &Vec<u8>) -> bool {
		if !self.pallet_selected(pallet) {
			return false;
		}

		let extrinsic_filter = self.extrinsic.clone().unwrap_or_default();
		let extrinsic_split: Vec<&str> = extrinsic_filter.split(',').collect();
		let extrinsics: Vec<_> = extrinsic_split.iter().map(|x| x.trim().as_bytes()).collect();

		let included = extrinsic_filter.is_empty() ||
			extrinsic_filter == "*" ||
			extrinsic_filter == "all" ||
			extrinsics.contains(&&extrinsic[..]);

		let excluded = self
			.excluded_extrinsics()
			.iter()
			.any(|(p, e)| p.as_bytes() == pallet && e.as_bytes() == extrinsic);

		included && !excluded
	}

	/// All `(pallet, extrinsic)` tuples that are excluded from the benchmarks.
	fn excluded_extrinsics(&self) -> Vec<(String, String)> {
		let mut excluded = Vec::new();

		for e in &self.exclude_extrinsics {
			let splits = e.split("::").collect::<Vec<_>>();
			if splits.len() != 2 {
				panic!("Invalid argument for '--exclude-extrinsics'. Expected format: 'pallet::extrinsic' but got '{}'", e);
			}
			excluded.push((splits[0].to_string(), splits[1].to_string()));
		}

		excluded
	}

	/// Execute a state machine and decode its return value as `R`.
	fn exec_state_machine<R: Decode, H: Hash, Exec: CodeExecutor>(
		mut machine: StateMachine<BenchmarkingState<H>, H, Exec>,
		hint: &str,
	) -> Result<R> {
		let res = machine
			.execute()
			.map_err(|e| format!("Could not call runtime API to {hint}: {}", e))?;
		let res = R::decode(&mut &res[..])
			.map_err(|e| format!("Failed to decode runtime API result to {hint}: {:?}", e))?;
		Ok(res)
	}

	/// Build the extension that are available for pallet benchmarks.
	fn build_extensions<E: CodeExecutor, H: Hasher + 'static>(
		exe: E,
		maybe_recorder: Option<Recorder<H>>,
	) -> Extensions {
		let mut extensions = Extensions::default();
		let (offchain, _) = TestOffchainExt::new();
		let (pool, _) = TestTransactionPoolExt::new();
		let keystore = MemoryKeystore::new();
		extensions.register(KeystoreExt::new(keystore));
		extensions.register(OffchainWorkerExt::new(offchain.clone()));
		extensions.register(OffchainDbExt::new(offchain));
		extensions.register(TransactionPoolExt::new(pool));
		extensions.register(ReadRuntimeVersionExt::new(exe));
		extensions.register(sp_virtualization::VirtManagerExt::new(
			sc_virtualization::VirtManager::default(),
		));
		if let Some(recorder) = maybe_recorder {
			extensions.register(ProofSizeExt::new(recorder));
		}
		extensions
	}

	/// Load the runtime blob for this benchmark.
	///
	/// The blob will either be loaded from the `:code` key out of the chain spec, or from a file
	/// when specified with `--runtime`.
	fn runtime_blob<'a, H: Hash>(
		&self,
		state: &'a BenchmarkingState<H>,
	) -> Result<FetchedCode<'a, BenchmarkingState<H>, H>> {
		if let Some(runtime) = self.runtime.as_ref() {
			log::debug!(target: LOG_TARGET, "Loading WASM from file {}", runtime.display());
			let code = fs::read(runtime).map_err(|e| {
				format!(
					"Could not load runtime file from path: {}, error: {}",
					runtime.display(),
					e
				)
			})?;
			let hash = sp_crypto_hashing::blake2_256(&code).to_vec();
			let wrapped_code = WrappedRuntimeCode(Cow::Owned(code));

			Ok(FetchedCode::FromFile { wrapped_code, heap_pages: self.heap_pages, hash })
		} else {
			log::info!(target: LOG_TARGET, "Loading WASM from state");
			let state =
				sp_state_machine::backend::BackendRuntimeCode::new(state, TryPendingCode::No);

			Ok(FetchedCode::FromGenesis { state })
		}
	}

	/// Allocation strategy for pallet benchmarking.
	fn alloc_strategy(&self, runtime_heap_pages: Option<u64>) -> HeapAllocStrategy {
		self.heap_pages.or(runtime_heap_pages).map_or(DEFAULT_HEAP_ALLOC_STRATEGY, |p| {
			HeapAllocStrategy::Static { extra_pages: p as _ }
		})
	}

	fn output(
		&self,
		batches: &[BenchmarkBatchSplitResults],
		storage_info: &[StorageInfo],
		component_ranges: &ComponentRangeMap,
		pov_modes: PovModesMap,
	) -> Result<()> {
		// Jsonify the result and write it to a file or stdout if desired.
		if !self.jsonify(&batches)? && !self.quiet {
			// Print the summary only if `jsonify` did not write to stdout.
			self.print_summary(&batches, &storage_info, pov_modes.clone())
		}

		// Create the weights.rs file.
		if let Some(output_path) = &self.output {
			writer::write_results(
				&batches,
				&storage_info,
				&component_ranges,
				pov_modes,
				self.default_pov_mode,
				output_path,
				self,
			)?;
		}

		Ok(())
	}

	/// Re-analyze a batch historic benchmark timing data. Will not take the PoV into account.
	fn output_from_results(&self, batches: &[BenchmarkBatchSplitResults]) -> Result<()> {
		let mut component_ranges = HashMap::<(String, String), HashMap<String, (u32, u32)>>::new();
		for batch in batches {
			let range = component_ranges
				.entry((
					String::from_utf8(batch.pallet.clone()).unwrap(),
					String::from_utf8(batch.benchmark.clone()).unwrap(),
				))
				.or_default();
			for result in &batch.time_results {
				for (param, value) in &result.components {
					let name = param.to_string();
					let (ref mut min, ref mut max) = range.entry(name).or_insert((*value, *value));
					if *value < *min {
						*min = *value;
					}
					if *value > *max {
						*max = *value;
					}
				}
			}
		}

		let component_ranges: HashMap<_, _> = component_ranges
			.into_iter()
			.map(|(key, ranges)| {
				let ranges = ranges
					.into_iter()
					.map(|(name, (min, max))| ComponentRange { name, min, max })
					.collect();
				(key, ranges)
			})
			.collect();

		self.output(batches, &[], &component_ranges, Default::default())
	}

	/// Jsonifies the passed batches and writes them to stdout or into a file.
	/// Can be configured via `--json` and `--json-file`.
	/// Returns whether it wrote to stdout.
	fn jsonify(&self, batches: &[BenchmarkBatchSplitResults]) -> Result<bool> {
		if self.json_output || self.json_file.is_some() {
			let json = serde_json::to_string_pretty(&batches)
				.map_err(|e| format!("Serializing into JSON: {:?}", e))?;

			if let Some(path) = &self.json_file {
				fs::write(path, json)?;
			} else {
				print!("{json}");
				return Ok(true);
			}
		}

		Ok(false)
	}

	/// Prints the results as human-readable summary without raw timing data.
	fn print_summary(
		&self,
		batches: &[BenchmarkBatchSplitResults],
		storage_info: &[StorageInfo],
		pov_modes: PovModesMap,
	) {
		for batch in batches.iter() {
			// Print benchmark metadata
			let pallet = String::from_utf8(batch.pallet.clone()).expect("Encoded from String; qed");
			let benchmark =
				String::from_utf8(batch.benchmark.clone()).expect("Encoded from String; qed");
			println!(
					"Pallet: {:?}, Extrinsic: {:?}, Lowest values: {:?}, Highest values: {:?}, Steps: {:?}, Repeat: {:?}",
					pallet,
					benchmark,
					self.lowest_range_values,
					self.highest_range_values,
					self.steps,
					self.repeat,
				);

			// Skip raw data + analysis if there are no results
			if batch.time_results.is_empty() {
				continue;
			}

			if !self.no_storage_info {
				let mut storage_per_prefix = HashMap::<Vec<u8>, Vec<BenchmarkResult>>::new();
				let pov_mode =
					pov_modes.get(&(pallet, benchmark.clone())).cloned().unwrap_or_default();

				let comments = writer::process_storage_results(
					&mut storage_per_prefix,
					&batch.db_results,
					storage_info,
					&pov_mode,
					self.default_pov_mode,
					self.worst_case_map_values,
					self.additional_trie_layers,
				);
				println!("Raw Storage Info\n========");
				for comment in comments {
					println!("{}", comment);
				}
				println!();
			}

			// Conduct analysis.
			if !self.no_median_slopes {
				println!("Median Slopes Analysis\n========");
				match Analysis::median_slopes(&batch.time_results, BenchmarkSelector::ExtrinsicTime)
				{
					Ok(analysis) => println!("-- Extrinsic Time --\n{}", analysis),
					Err(err) => println!("-- Extrinsic Time --\nError: {:?}", err),
				}
				match Analysis::median_slopes(&batch.db_results, BenchmarkSelector::Reads) {
					Ok(analysis) => println!("Reads = {:?}", analysis),
					Err(err) => println!("Reads: Error: {:?}", err),
				}
				match Analysis::median_slopes(&batch.db_results, BenchmarkSelector::Writes) {
					Ok(analysis) => println!("Writes = {:?}", analysis),
					Err(err) => println!("Writes: Error: {:?}", err),
				}
				match Analysis::median_slopes(&batch.db_results, BenchmarkSelector::ProofSize) {
					Ok(analysis) => println!("Recorded proof Size = {:?}", analysis),
					Err(err) => println!("Recorded proof Size: Error: {:?}", err),
				}
				println!();
			}
			if !self.no_min_squares {
				println!("Min Squares Analysis\n========");
				match Analysis::min_squares_iqr(
					&batch.time_results,
					BenchmarkSelector::ExtrinsicTime,
				) {
					Ok(analysis) => println!("-- Extrinsic Time --\n{}", analysis),
					Err(err) => println!("-- Extrinsic Time --\nError: {:?}", err),
				}
				match Analysis::min_squares_iqr(&batch.db_results, BenchmarkSelector::Reads) {
					Ok(analysis) => println!("Reads = {:?}", analysis),
					Err(err) => println!("Reads: Error: {:?}", err),
				}
				match Analysis::min_squares_iqr(&batch.db_results, BenchmarkSelector::Writes) {
					Ok(analysis) => println!("Writes = {:?}", analysis),
					Err(err) => println!("Writes: Error: {:?}", err),
				}
				match Analysis::min_squares_iqr(&batch.db_results, BenchmarkSelector::ProofSize) {
					Ok(analysis) => println!("Recorded proof Size = {:?}", analysis),
					Err(err) => println!("Recorded proof Size: Error: {:?}", err),
				}
				println!();
			}
		}
	}

	/// Parses the PoV modes per benchmark that were specified by the `#[pov_mode]` attribute.
	fn parse_pov_modes(
		benchmarks: &Vec<SelectedBenchmark>,
		storage_info: &[StorageInfo],
		ignore_unknown_pov_mode: bool,
	) -> Result<PovModesMap> {
		use std::collections::hash_map::Entry;
		let mut parsed = PovModesMap::new();

		for SelectedBenchmark { pallet, extrinsic, pov_modes, .. } in benchmarks {
			for (pallet_storage, mode) in pov_modes {
				let mode = PovEstimationMode::from_str(&mode)?;
				let pallet_storage = pallet_storage.replace(" ", "");
				let splits = pallet_storage.split("::").collect::<Vec<_>>();

				if splits.is_empty() || splits.len() > 2 {
					return Err(format!(
						"Expected 'Pallet::Storage' as storage name but got: {}",
						pallet_storage
					)
					.into());
				}
				let (pov_pallet, pov_storage) =
					(splits[0].trim(), splits.get(1).unwrap_or(&"ALL").trim());

				match parsed
					.entry((pallet.clone(), extrinsic.clone()))
					.or_default()
					.entry((pov_pallet.to_string(), pov_storage.to_string()))
				{
					Entry::Occupied(_) => {
						return Err(format!(
							"Cannot specify pov_mode tag twice for the same key: {}",
							pallet_storage
						)
						.into())
					},
					Entry::Vacant(e) => {
						e.insert(mode);
					},
				}
			}
		}
		log::debug!("Parsed PoV modes: {:?}", parsed);
		Self::check_pov_modes(&parsed, storage_info, ignore_unknown_pov_mode)?;

		Ok(parsed)
	}

	fn check_pov_modes(
		pov_modes: &PovModesMap,
		storage_info: &[StorageInfo],
		ignore_unknown_pov_mode: bool,
	) -> Result<()> {
		// Check that all PoV modes are valid pallet storage keys
		for (pallet, storage) in pov_modes.values().flat_map(|i| i.keys()) {
			let (mut found_pallet, mut found_storage) = (false, false);

			for info in storage_info {
				if pallet == "ALL" || info.pallet_name == pallet.as_bytes() {
					found_pallet = true;
				}
				if storage == "ALL" || info.storage_name == storage.as_bytes() {
					found_storage = true;
				}
			}
			if !found_pallet || !found_storage {
				let err = format!("The PoV mode references an unknown storage item or pallet: `{}::{}`. You can ignore this warning by specifying `--ignore-unknown-pov-mode`", pallet, storage);

				if ignore_unknown_pov_mode {
					log::warn!(target: LOG_TARGET, "Error demoted to warning due to `--ignore-unknown-pov-mode`: {}", err);
				} else {
					return Err(err.into());
				}
			}
		}

		Ok(())
	}

	/// Sanity check the CLI arguments.
	fn check_args(
		&self,
		chain_spec: &Option<Box<dyn ChainSpec>>,
	) -> std::result::Result<(), (ErrorKind, String)> {
		if self.runtime.is_some() && self.shared_params.chain.is_some() {
			unreachable!("Clap should not allow both `--runtime` and `--chain` to be provided.")
		}

		if self.external_repeat.is_some() {
			log::warn!(target: LOG_TARGET, "The `--external-repeat` argument is deprecated and will be removed in a future release.");
		}

		if chain_spec.is_none() && self.runtime.is_none() && self.shared_params.chain.is_none() {
			return Err((
				ErrorKind::MissingRequiredArgument,
				"Provide either a runtime via `--runtime` or a chain spec via `--chain`"
					.to_string(),
			));
		}

		match self.genesis_builder {
			Some(GenesisBuilderPolicy::SpecGenesis | GenesisBuilderPolicy::SpecRuntime) => {
				if chain_spec.is_none() && self.shared_params.chain.is_none() {
					return Err((
						ErrorKind::MissingRequiredArgument,
						"Provide a chain spec via `--chain`.".to_string(),
					));
				}
			},
			_ => {},
		}

		if let Some(output_path) = &self.output {
			if !output_path.is_dir() && output_path.file_name().is_none() {
				return Err((
					ErrorKind::InvalidValue,
					format!("Output path is neither a directory nor a file: {output_path:?}"),
				));
			}
		}

		if let Some(header_file) = &self.header {
			if !header_file.is_file() {
				return Err((
					ErrorKind::InvalidValue,
					format!("Header file could not be found: {header_file:?}"),
				));
			};
		}

		if let Some(handlebars_template_file) = &self.template {
			if !handlebars_template_file.is_file() {
				return Err((
					ErrorKind::InvalidValue,
					format!(
						"Handlebars template file could not be found: {handlebars_template_file:?}"
					),
				));
			};
		}
		Ok(())
	}
}

impl CliConfiguration for PalletCmd {
	fn shared_params(&self) -> &SharedParams {
		&self.shared_params
	}

	fn chain_id(&self, _is_dev: bool) -> Result<String> {
		Ok(match self.shared_params.chain {
			Some(ref chain) => chain.clone(),
			None => "dev".into(),
		})
	}
}

/// List the benchmarks available in the runtime, in a CSV friendly format.
fn list_benchmark(
	benchmarks_to_run: Vec<SelectedBenchmark>,
	list_output: ListOutput,
	no_csv_header: bool,
) {
	let mut benchmarks = BTreeMap::new();

	// Sort and de-dub by pallet and function name.
	benchmarks_to_run.iter().for_each(|bench| {
		benchmarks
			.entry(&bench.pallet)
			.or_insert_with(BTreeSet::new)
			.insert(&bench.extrinsic);
	});

	match list_output {
		ListOutput::All => {
			if !no_csv_header {
				println!("pallet, extrinsic");
			}
			for (pallet, extrinsics) in benchmarks {
				for extrinsic in extrinsics {
					println!("{pallet}, {extrinsic}");
				}
			}
		},
		ListOutput::Pallets => {
			if !no_csv_header {
				println!("pallet");
			};
			for pallet in benchmarks.keys() {
				println!("{pallet}");
			}
		},
	}
}
#[cfg(test)]
mod tests {
	use crate::pallet::PalletCmd;
	use clap::Parser;

	fn cli_succeed(args: &[&str]) -> Result<(), clap::Error> {
		let cmd = PalletCmd::try_parse_from(args)?;
		assert!(cmd.check_args(&None).is_ok());
		Ok(())
	}

	fn cli_fail(args: &[&str]) {
		let cmd = PalletCmd::try_parse_from(args);
		if let Ok(cmd) = cmd {
			assert!(cmd.check_args(&None).is_err());
		}
	}

	#[test]
	fn test_cli_conflicts() -> Result<(), clap::Error> {
		// Runtime tests
		cli_succeed(&[
			"test",
			"--extrinsic",
			"",
			"--pallet",
			"",
			"--runtime",
			"path/to/runtime",
			"--genesis-builder",
			"runtime",
		])?;
		cli_succeed(&[
			"test",
			"--extrinsic",
			"",
			"--pallet",
			"",
			"--runtime",
			"path/to/runtime",
			"--genesis-builder",
			"none",
		])?;
		cli_succeed(&["test", "--extrinsic", "", "--pallet", "", "--runtime", "path/to/runtime"])?;
		cli_succeed(&[
			"test",
			"--extrinsic",
			"",
			"--pallet",
			"",
			"--runtime",
			"path/to/runtime",
			"--genesis-builder-preset",
			"preset",
		])?;
		cli_succeed(&[
			"test",
			"--extrinsic",
			"",
			"--pallet",
			"",
			"--runtime",
			"path/to/runtime",
			"--genesis-patch",
			"path/to/patch.json",
		])?;
		cli_fail(&[
			"test",
			"--extrinsic",
			"",
			"--pallet",
			"",
			"--runtime",
			"path/to/runtime",
			"--genesis-builder",
			"spec",
		]);
		cli_fail(&[
			"test",
			"--extrinsic",
			"",
			"--pallet",
			"",
			"--runtime",
			"path/to/spec",
			"--genesis-builder",
			"spec-genesis",
		]);
		cli_fail(&[
			"test",
			"--extrinsic",
			"",
			"--pallet",
			"",
			"--runtime",
			"path/to/spec",
			"--genesis-builder",
			"spec-runtime",
		]);
		cli_fail(&["test", "--runtime", "path/to/spec", "--genesis-builder", "spec-genesis"]);

		// Spec tests
		cli_succeed(&["test", "--extrinsic", "", "--pallet", "", "--chain", "path/to/spec"])?;
		cli_succeed(&[
			"test",
			"--extrinsic",
			"",
			"--pallet",
			"",
			"--chain",
			"path/to/spec",
			"--genesis-builder",
			"spec",
		])?;
		cli_succeed(&[
			"test",
			"--extrinsic",
			"",
			"--pallet",
			"",
			"--chain",
			"path/to/spec",
			"--genesis-builder",
			"spec-genesis",
		])?;
		cli_succeed(&[
			"test",
			"--extrinsic",
			"",
			"--pallet",
			"",
			"--chain",
			"path/to/spec",
			"--genesis-builder",
			"spec-runtime",
		])?;
		cli_succeed(&[
			"test",
			"--extrinsic",
			"",
			"--pallet",
			"",
			"--chain",
			"path/to/spec",
			"--genesis-builder",
			"none",
		])?;
		cli_fail(&[
			"test",
			"--extrinsic",
			"",
			"--pallet",
			"",
			"--chain",
			"path/to/spec",
			"--genesis-builder",
			"runtime",
		]);
		cli_fail(&[
			"test",
			"--extrinsic",
			"",
			"--pallet",
			"",
			"--chain",
			"path/to/spec",
			"--genesis-builder",
			"runtime",
			"--genesis-builder-preset",
			"preset",
		]);
		Ok(())
	}
}
