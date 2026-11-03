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

//! Defines the compiled Wasm runtime that uses Wasmtime internally.

use crate::{
	host::HostState,
	instance_wrapper::{EntryPoint, InstanceWrapper, MemoryWrapper},
	util::{self, replace_strategy_if_broken},
};

use parking_lot::Mutex;
use sc_allocator::{AllocationStats, FreeingBumpHeapAllocator};
use sc_executor_common::{
	error::{Error, Result, WasmError},
	runtime_blob::RuntimeBlob,
	util::checked_range,
	wasm_runtime::{HeapAllocStrategy, WasmInstance, WasmModule},
};
use sp_runtime_interface::unpack_ptr_and_len;
use sp_wasm_interface::{HostFunctions, Pointer, WordSize};
use std::{
	path::{Path, PathBuf},
	sync::{
		atomic::{AtomicBool, Ordering},
		Arc,
	},
};
use wasmtime::{AsContext, Cache, CacheConfig, Engine, Memory};

const MAX_INSTANCE_COUNT: u32 = 64;

#[derive(Default)]
pub(crate) struct StoreData {
	/// This will only be set when we call into the runtime.
	pub(crate) host_state: Option<HostState>,
	/// This will be always set once the store is initialized.
	pub(crate) memory: Option<Memory>,
	/// Limits for memory growth enforced by the `StoreLimiter`.
	pub(crate) limits: wasmtime::StoreLimits,
	/// The effective maximum number of memory pages for the allocator to cap growth requests.
	pub(crate) memory_max_pages: Option<u32>,
}

impl StoreData {
	/// Returns a mutable reference to the host state.
	pub fn host_state_mut(&mut self) -> Option<&mut HostState> {
		self.host_state.as_mut()
	}

	/// Returns the host memory.
	pub fn memory(&self) -> Memory {
		self.memory.expect("memory is always set; qed")
	}
}

pub(crate) type Store = wasmtime::Store<StoreData>;

enum Strategy {
	RecreateInstance(InstanceCreator),
}

struct InstanceCreator {
	engine: Engine,
	instance_pre: Arc<wasmtime::InstancePre<StoreData>>,
	instance_counter: Arc<InstanceCounter>,
	heap_alloc_strategy: HeapAllocStrategy,
}

impl InstanceCreator {
	fn instantiate(&mut self) -> Result<InstanceWrapper> {
		InstanceWrapper::new(
			&self.engine,
			&self.instance_pre,
			self.instance_counter.clone(),
			self.heap_alloc_strategy,
		)
	}
}

/// A handle for releasing an instance acquired by [`InstanceCounter::acquire_instance`].
pub(crate) struct ReleaseInstanceHandle {
	counter: Arc<InstanceCounter>,
}

impl Drop for ReleaseInstanceHandle {
	fn drop(&mut self) {
		{
			let mut counter = self.counter.counter.lock();
			*counter = counter.saturating_sub(1);
		}

		self.counter.wait_for_instance.notify_one();
	}
}

/// Keeps track on the number of parallel instances.
///
/// The runtime cache keeps track on the number of parallel instances. The maximum number in the
/// cache is less than what we have configured as [`MAX_INSTANCE_COUNT`] for wasmtime. However, the
/// cache will create on demand instances if required. This instance counter will ensure that we are
/// blocking when we are trying to create too many instances.
#[derive(Default)]
pub(crate) struct InstanceCounter {
	counter: Mutex<u32>,
	wait_for_instance: parking_lot::Condvar,
}

impl InstanceCounter {
	/// Acquire an instance.
	///
	/// Blocks if there is no free instance available.
	///
	/// The returned [`ReleaseInstanceHandle`] should be dropped when the instance isn't used
	/// anymore.
	pub fn acquire_instance(self: Arc<Self>) -> ReleaseInstanceHandle {
		let mut counter = self.counter.lock();

		while *counter >= MAX_INSTANCE_COUNT {
			self.wait_for_instance.wait(&mut counter);
		}
		*counter += 1;

		ReleaseInstanceHandle { counter: self.clone() }
	}
}

/// A `WasmModule` implementation using wasmtime to compile the runtime module to machine code
/// and execute the compiled code.
pub struct WasmtimeRuntime {
	engine: Engine,
	instance_pre: Arc<wasmtime::InstancePre<StoreData>>,
	instantiation_strategy: InternalInstantiationStrategy,
	instance_counter: Arc<InstanceCounter>,
}

impl WasmModule for WasmtimeRuntime {
	fn new_instance(
		&self,
		heap_alloc_strategy: HeapAllocStrategy,
	) -> Result<Box<dyn WasmInstance>> {
		let strategy = match self.instantiation_strategy {
			InternalInstantiationStrategy::Builtin => Strategy::RecreateInstance(InstanceCreator {
				engine: self.engine.clone(),
				instance_pre: self.instance_pre.clone(),
				instance_counter: self.instance_counter.clone(),
				heap_alloc_strategy,
			}),
		};

		Ok(Box::new(WasmtimeInstance { strategy }))
	}
}

/// A `WasmInstance` implementation that reuses compiled module and spawns instances
/// to execute the compiled code.
pub struct WasmtimeInstance {
	strategy: Strategy,
}

impl WasmtimeInstance {
	fn call_impl(
		&mut self,
		method: &str,
		data: &[u8],
		allocation_stats: &mut Option<AllocationStats>,
	) -> Result<Vec<u8>> {
		match &mut self.strategy {
			Strategy::RecreateInstance(ref mut instance_creator) => {
				let mut instance_wrapper = instance_creator.instantiate()?;
				let heap_base = instance_wrapper.extract_heap_base()?;
				let entrypoint = instance_wrapper.resolve_entrypoint(method)?;
				let allocator = FreeingBumpHeapAllocator::new(heap_base);

				perform_call(data, &mut instance_wrapper, entrypoint, allocator, allocation_stats)
			},
		}
	}
}

impl WasmInstance for WasmtimeInstance {
	fn call_with_allocation_stats(
		&mut self,
		method: &str,
		data: &[u8],
	) -> (Result<Vec<u8>>, Option<AllocationStats>) {
		let mut allocation_stats = None;
		let result = self.call_impl(method, data, &mut allocation_stats);
		(result, allocation_stats)
	}

	fn set_heap_alloc_strategy(&mut self, heap_alloc_strategy: HeapAllocStrategy) {
		match &mut self.strategy {
			Strategy::RecreateInstance(ref mut creator) => {
				creator.heap_alloc_strategy = heap_alloc_strategy;
			},
		}
	}
}

/// Prepare a directory structure and a config file to enable wasmtime caching.
///
/// In case of an error the caching will not be enabled.
fn setup_wasmtime_caching(
	cache_path: &Path,
	config: &mut wasmtime::Config,
) -> std::result::Result<(), String> {
	use std::fs;

	let wasmtime_cache_root = cache_path.join("wasmtime");
	fs::create_dir_all(&wasmtime_cache_root)
		.map_err(|err| format!("cannot create the dirs to cache: {}", err))?;

	let mut cache_config = CacheConfig::new();
	cache_config.with_directory(cache_path);

	let cache =
		Cache::new(cache_config).map_err(|err| format!("failed to initiate Cache: {err:?}"))?;

	config.cache(Some(cache));

	Ok(())
}

fn common_config(semantics: &Semantics) -> std::result::Result<wasmtime::Config, WasmError> {
	let mut config = wasmtime::Config::new();
	config.cranelift_opt_level(wasmtime::OptLevel::SpeedAndSize);
	config.cranelift_nan_canonicalization(semantics.canonicalize_nans);

	let profiler = match std::env::var_os("WASMTIME_PROFILING_STRATEGY") {
		Some(os_string) if os_string == "jitdump" => wasmtime::ProfilingStrategy::JitDump,
		Some(os_string) if os_string == "perfmap" => wasmtime::ProfilingStrategy::PerfMap,
		None => wasmtime::ProfilingStrategy::None,
		Some(_) => {
			// Remember if we have already logged a warning due to an unknown profiling strategy.
			static UNKNOWN_PROFILING_STRATEGY: AtomicBool = AtomicBool::new(false);
			// Make sure that the warning will not be relogged regularly.
			if !UNKNOWN_PROFILING_STRATEGY.swap(true, Ordering::Relaxed) {
				log::warn!("WASMTIME_PROFILING_STRATEGY is set to unknown value, ignored.");
			}
			wasmtime::ProfilingStrategy::None
		},
	};
	config.profiler(profiler);

	let native_stack_max = match semantics.deterministic_stack_limit {
		Some(DeterministicStackLimit { native_stack_max, .. }) => native_stack_max,

		// In `wasmtime` 0.35 the default stack size limit was changed from 1MB to 512KB.
		//
		// This broke at least one parachain which depended on the original 1MB limit,
		// so here we restore it to what it was originally.
		None => 1024 * 1024,
	};

	config.max_wasm_stack(native_stack_max as usize);

	config.parallel_compilation(semantics.parallel_compilation);

	// Be clear and specific about the extensions we support. If an update brings new features
	// they should be introduced here as well.
	config.wasm_reference_types(semantics.wasm_reference_types);
	config.wasm_simd(semantics.wasm_simd);
	config.wasm_relaxed_simd(semantics.wasm_simd);
	config.wasm_bulk_memory(semantics.wasm_bulk_memory);
	config.wasm_multi_value(semantics.wasm_multi_value);
	config.wasm_multi_memory(false);
	config.wasm_threads(false);
	config.wasm_memory64(false);
	config.wasm_tail_call(false);
	config.wasm_extended_const(false);

	let (use_pooling, use_cow) = match semantics.instantiation_strategy {
		InstantiationStrategy::PoolingCopyOnWrite => (true, true),
		InstantiationStrategy::Pooling => (true, false),
		InstantiationStrategy::RecreateInstanceCopyOnWrite => (false, true),
		InstantiationStrategy::RecreateInstance => (false, false),
	};

	const WASM_PAGE_SIZE: u64 = 65536;

	config.memory_init_cow(use_cow);
	// Always use the maximum possible dense image size. Per-instance `StoreLimits`
	// enforce the actual memory cap, so the engine-level setting just needs to be
	// large enough to never be the bottleneck.
	config.memory_guaranteed_dense_image_size(u64::MAX);

	if use_pooling {
		const MAX_WASM_PAGES: u64 = 0x10000;

		let mut pooling_config = wasmtime::PoolingAllocationConfig::default();
		pooling_config
			.max_unused_warm_slots(4)
			// Pooling needs a bunch of hard limits to be set; if we go over
			// any of these then the instantiation will fail.
			//
			// Current minimum values for kusama (as of 2022-04-14):
			//   size: 32384
			//   table_elements: 1249
			//   memory_pages: 2070
			.max_core_instance_size(512 * 1024)
			.table_elements(8192)
			// Always reserve the maximum WASM memory (4GB virtual address space per
			// slot). This is only virtual memory (mmap with PROT_NONE), not physical
			// RAM. The actual per-instance memory limit is enforced by `StoreLimits`
			// set at instantiation time, which may vary between calls (e.g. doubled
			// for block import). The pool must accommodate the largest possible limit.
			.max_memory_size(MAX_WASM_PAGES as usize * WASM_PAGE_SIZE as usize)
			.total_tables(MAX_INSTANCE_COUNT)
			.total_memories(MAX_INSTANCE_COUNT)
			// This determines how many instances of the module can be
			// instantiated in parallel from the same `Module`.
			.total_core_instances(MAX_INSTANCE_COUNT);

		config.allocation_strategy(wasmtime::InstanceAllocationStrategy::Pooling(pooling_config));
	}

	Ok(config)
}

/// Knobs for deterministic stack height limiting.
///
/// The WebAssembly standard defines a call/value stack but it doesn't say anything about its
/// size except that it has to be finite. The implementations are free to choose their own notion
/// of limit: some may count the number of calls or values, others would rely on the host machine
/// stack and trap on reaching a guard page.
///
/// This obviously is a source of non-determinism during execution. This feature can be used
/// to instrument the code so that it will count the depth of execution in some deterministic
/// way (the machine stack limit should be so high that the deterministic limit always triggers
/// first).
///
/// The deterministic stack height limiting feature allows to instrument the code so that it will
/// count the number of items that may be on the stack. This counting will only act as an rough
/// estimate of the actual stack limit in wasmtime. This is because wasmtime measures it's stack
/// usage in bytes.
///
/// The actual number of bytes consumed by a function is not trivial to compute  without going
/// through full compilation. Therefore, it's expected that `native_stack_max` is greatly
/// overestimated and thus never reached in practice. The stack overflow check introduced by the
/// instrumentation and that relies on the logical item count should be reached first.
///
/// See [here][stack_height] for more details of the instrumentation
///
/// [stack_height]: https://github.com/paritytech/wasm-instrument/blob/master/src/stack_limiter/mod.rs
#[derive(Clone)]
pub struct DeterministicStackLimit {
	/// A number of logical "values" that can be pushed on the wasm stack. A trap will be triggered
	/// if exceeded.
	///
	/// A logical value is a local, an argument or a value pushed on operand stack.
	pub logical_max: u32,
	/// The maximum number of bytes for stack used by wasmtime JITed code.
	///
	/// It's not specified how much bytes will be consumed by a stack frame for a given wasm
	/// function after translation into machine code. It is also not quite trivial.
	///
	/// Therefore, this number should be chosen conservatively. It must be so large so that it can
	/// fit the [`logical_max`](Self::logical_max) logical values on the stack, according to the
	/// current instrumentation algorithm.
	///
	/// This value cannot be 0.
	pub native_stack_max: u32,
}

/// The instantiation strategy to use for the WASM executor.
///
/// All of the CoW strategies (with `CopyOnWrite` suffix) are only supported when either:
///   a) we're running on Linux,
///   b) we're running on an Unix-like system and we're precompiling
///      our module beforehand and instantiating from a file.
///
/// If the CoW variant of a strategy is unsupported the executor will
/// fall back to the non-CoW equivalent.
#[non_exhaustive]
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum InstantiationStrategy {
	/// Pool the instances to avoid initializing everything from scratch
	/// on each instantiation. Use copy-on-write memory when possible.
	///
	/// This is the fastest instantiation strategy.
	PoolingCopyOnWrite,

	/// Recreate the instance from scratch on every instantiation.
	/// Use copy-on-write memory when possible.
	RecreateInstanceCopyOnWrite,

	/// Pool the instances to avoid initializing everything from scratch
	/// on each instantiation.
	Pooling,

	/// Recreate the instance from scratch on every instantiation. Very slow.
	RecreateInstance,
}

enum InternalInstantiationStrategy {
	Builtin,
}

#[derive(Clone)]
pub struct Semantics {
	/// The instantiation strategy to use.
	pub instantiation_strategy: InstantiationStrategy,

	/// Specifying `Some` will enable deterministic stack height. That is, all executor
	/// invocations will reach stack overflow at the exactly same point across different wasmtime
	/// versions and architectures.
	///
	/// This is achieved by a combination of running an instrumentation pass on input code and
	/// configuring wasmtime accordingly.
	///
	/// Since this feature depends on instrumentation, it can be set only if runtime is
	/// instantiated using the runtime blob, e.g. using [`create_runtime`].
	// I.e. if [`CodeSupplyMode::Verbatim`] is used.
	pub deterministic_stack_limit: Option<DeterministicStackLimit>,

	/// Controls whether wasmtime should compile floating point in a way that doesn't allow for
	/// non-determinism.
	///
	/// By default, the wasm spec allows some local non-determinism wrt. certain floating point
	/// operations. Specifically, those operations that are not defined to operate on bits (e.g.
	/// fneg) can produce NaN values. The exact bit pattern for those is not specified and may
	/// depend on the particular machine that executes wasmtime generated JITed machine code. That
	/// is a source of non-deterministic values.
	///
	/// The classical runtime environment for Substrate allowed it and punted this on the runtime
	/// developers. For PVFs, we want to ensure that execution is deterministic though. Therefore,
	/// for PVF execution this flag is meant to be turned on.
	pub canonicalize_nans: bool,

	/// Configures wasmtime to use multiple threads for compiling.
	pub parallel_compilation: bool,

	/// The heap allocation strategy to use.
	pub heap_alloc_strategy: HeapAllocStrategy,

	/// Enables WASM Multi-Value proposal
	pub wasm_multi_value: bool,

	/// Enables WASM Bulk Memory Operations proposal
	pub wasm_bulk_memory: bool,

	/// Enables WASM Reference Types proposal
	pub wasm_reference_types: bool,

	/// Enables WASM Fixed-Width SIMD proposal
	pub wasm_simd: bool,
}

#[derive(Clone)]
pub struct Config {
	/// The WebAssembly standard requires all imports of an instantiated module to be resolved,
	/// otherwise, the instantiation fails. If this option is set to `true`, then this behavior is
	/// overridden and imports that are requested by the module and not provided by the host
	/// functions will be resolved using stubs. These stubs will trap upon a call.
	pub allow_missing_func_imports: bool,

	/// A directory in which wasmtime can store its compiled artifacts cache.
	pub cache_path: Option<PathBuf>,

	/// Tuning of various semantics of the wasmtime executor.
	pub semantics: Semantics,
}

enum CodeSupplyMode<'a> {
	/// The runtime is instantiated using the given runtime blob.
	Fresh(RuntimeBlob),

	/// The runtime is instantiated using a precompiled module at the given path.
	///
	/// This assumes that the code is already prepared for execution and the same `Config` was
	/// used.
	///
	/// We use a `Path` here instead of simply passing a byte slice to allow `wasmtime` to
	/// map the runtime's linear memory on supported platforms in a copy-on-write fashion.
	Precompiled(&'a Path),

	/// The runtime is instantiated using a precompiled module with the given bytes.
	///
	/// This assumes that the code is already prepared for execution and the same `Config` was
	/// used.
	PrecompiledBytes(&'a [u8]),
}

/// Create a new `WasmtimeRuntime` given the code. This function performs translation from Wasm to
/// machine code, which can be computationally heavy.
///
/// The `H` generic parameter is used to statically pass a set of host functions which are exposed
/// to the runtime.
pub fn create_runtime<H>(
	blob: RuntimeBlob,
	config: Config,
) -> std::result::Result<WasmtimeRuntime, WasmError>
where
	H: HostFunctions,
{
	// SAFETY: this is safe because it doesn't use `CodeSupplyMode::Precompiled`.
	unsafe { do_create_runtime::<H>(CodeSupplyMode::Fresh(blob), config) }
}

/// The same as [`create_runtime`] but takes a path to a precompiled artifact,
/// which makes this function considerably faster than [`create_runtime`].
///
/// # Safety
///
/// The caller must ensure that the compiled artifact passed here was:
///   1) produced by [`prepare_runtime_artifact`],
///   2) written to the disk as a file,
///   3) was not modified,
///   4) will not be modified while any runtime using this artifact is alive, or is being
///      instantiated.
///
/// Failure to adhere to these requirements might lead to crashes and arbitrary code execution.
///
/// It is ok though if the compiled artifact was created by code of another version or with
/// different configuration flags. In such case the caller will receive an `Err` deterministically.
pub unsafe fn create_runtime_from_artifact<H>(
	compiled_artifact_path: &Path,
	config: Config,
) -> std::result::Result<WasmtimeRuntime, WasmError>
where
	H: HostFunctions,
{
	do_create_runtime::<H>(CodeSupplyMode::Precompiled(compiled_artifact_path), config)
}

/// The same as [`create_runtime`] but takes the bytes of a precompiled artifact,
/// which makes this function considerably faster than [`create_runtime`],
/// but slower than the more optimized [`create_runtime_from_artifact`].
/// This is especially slow on non-Linux Unix systems. Useful in very niche cases.
///
/// # Safety
///
/// The caller must ensure that the compiled artifact passed here was:
///   1) produced by [`prepare_runtime_artifact`],
///   2) was not modified,
///
/// Failure to adhere to these requirements might lead to crashes and arbitrary code execution.
///
/// It is ok though if the compiled artifact was created by code of another version or with
/// different configuration flags. In such case the caller will receive an `Err` deterministically.
pub unsafe fn create_runtime_from_artifact_bytes<H>(
	compiled_artifact_bytes: &[u8],
	config: Config,
) -> std::result::Result<WasmtimeRuntime, WasmError>
where
	H: HostFunctions,
{
	do_create_runtime::<H>(CodeSupplyMode::PrecompiledBytes(compiled_artifact_bytes), config)
}

/// # Safety
///
/// This is only unsafe if called with [`CodeSupplyMode::Artifact`]. See
/// [`create_runtime_from_artifact`] to get more details.
unsafe fn do_create_runtime<H>(
	code_supply_mode: CodeSupplyMode<'_>,
	mut config: Config,
) -> std::result::Result<WasmtimeRuntime, WasmError>
where
	H: HostFunctions,
{
	replace_strategy_if_broken(&mut config.semantics.instantiation_strategy);

	let mut wasmtime_config = common_config(&config.semantics)?;
	if let Some(ref cache_path) = config.cache_path {
		if let Err(reason) = setup_wasmtime_caching(cache_path, &mut wasmtime_config) {
			log::warn!(
				"failed to setup wasmtime cache. Performance may degrade significantly: {}.",
				reason,
			);
		}
	}

	let engine = Engine::new(&wasmtime_config)
		.map_err(|e| WasmError::Other(format!("cannot create the wasmtime engine: {:#}", e)))?;

	let (module, instantiation_strategy) = match code_supply_mode {
		CodeSupplyMode::Fresh(blob) => {
			let blob = prepare_blob_for_compilation(blob, &config.semantics)?;
			let serialized_blob = blob.clone().serialize();

			let module = wasmtime::Module::new(&engine, &serialized_blob)
				.map_err(|e| WasmError::Other(format!("cannot create module: {:#}", e)))?;

			match config.semantics.instantiation_strategy {
				InstantiationStrategy::Pooling |
				InstantiationStrategy::PoolingCopyOnWrite |
				InstantiationStrategy::RecreateInstance |
				InstantiationStrategy::RecreateInstanceCopyOnWrite => {
					(module, InternalInstantiationStrategy::Builtin)
				},
			}
		},
		CodeSupplyMode::Precompiled(compiled_artifact_path) => {
			// SAFETY: The unsafety of `deserialize_file` is covered by this function. The
			//         responsibilities to maintain the invariants are passed to the caller.
			//
			//         See [`create_runtime_from_artifact`] for more details.
			let module = wasmtime::Module::deserialize_file(&engine, compiled_artifact_path)
				.map_err(|e| WasmError::Other(format!("cannot deserialize module: {:#}", e)))?;

			(module, InternalInstantiationStrategy::Builtin)
		},
		CodeSupplyMode::PrecompiledBytes(compiled_artifact_bytes) => {
			// SAFETY: The unsafety of `deserialize` is covered by this function. The
			//         responsibilities to maintain the invariants are passed to the caller.
			//
			//         See [`create_runtime_from_artifact_bytes`] for more details.
			let module = wasmtime::Module::deserialize(&engine, compiled_artifact_bytes)
				.map_err(|e| WasmError::Other(format!("cannot deserialize module: {:#}", e)))?;

			(module, InternalInstantiationStrategy::Builtin)
		},
	};

	let mut linker = wasmtime::Linker::new(&engine);
	crate::imports::prepare_imports::<H>(&mut linker, &module, config.allow_missing_func_imports)?;

	let instance_pre = linker
		.instantiate_pre(&module)
		.map_err(|e| WasmError::Other(format!("cannot preinstantiate module: {:#}", e)))?;

	Ok(WasmtimeRuntime {
		engine,
		instance_pre: Arc::new(instance_pre),
		instantiation_strategy,
		instance_counter: Default::default(),
	})
}

fn prepare_blob_for_compilation(
	mut blob: RuntimeBlob,
	semantics: &Semantics,
) -> std::result::Result<RuntimeBlob, WasmError> {
	if let Some(DeterministicStackLimit { logical_max, .. }) = semantics.deterministic_stack_limit {
		blob = blob.inject_stack_depth_metering(logical_max)?;
	}

	// We don't actually need the memory to be imported so we can just convert any memory
	// import into an export with impunity. This simplifies our code since `wasmtime` will
	// now automatically take care of creating the memory for us, and it is also necessary
	// to enable `wasmtime`'s instance pooling. (Imported memories are ineligible for pooling.)
	blob.convert_memory_import_into_export()?;
	blob.clear_memory_max_limit()?;

	Ok(blob)
}

/// Takes a [`RuntimeBlob`] and precompiles it returning the serialized result of compilation. It
/// can then be used for calling [`create_runtime`] avoiding long compilation times.
pub fn prepare_runtime_artifact(
	blob: RuntimeBlob,
	semantics: &Semantics,
) -> std::result::Result<Vec<u8>, WasmError> {
	let mut semantics = semantics.clone();
	replace_strategy_if_broken(&mut semantics.instantiation_strategy);

	let blob = prepare_blob_for_compilation(blob, &semantics)?;

	let engine = Engine::new(&common_config(&semantics)?)
		.map_err(|e| WasmError::Other(format!("cannot create the engine: {:#}", e)))?;

	engine
		.precompile_module(&blob.serialize())
		.map_err(|e| WasmError::Other(format!("cannot precompile module: {:#}", e)))
}

fn perform_call(
	data: &[u8],
	instance_wrapper: &mut InstanceWrapper,
	entrypoint: EntryPoint,
	mut allocator: FreeingBumpHeapAllocator,
	allocation_stats: &mut Option<AllocationStats>,
) -> Result<Vec<u8>> {
	let (data_ptr, data_len) = inject_input_data(instance_wrapper, &mut allocator, data)?;

	let host_state = HostState::new(allocator);

	// Set the host state before calling into wasm.
	instance_wrapper.store_mut().data_mut().host_state = Some(host_state);

	let ret = entrypoint
		.call(instance_wrapper.store_mut(), data_ptr, data_len)
		.map(unpack_ptr_and_len);

	// Reset the host state
	let host_state = instance_wrapper.store_mut().data_mut().host_state.take().expect(
		"the host state is always set before calling into WASM so it can't be None here; qed",
	);
	*allocation_stats = Some(host_state.allocation_stats());

	let (output_ptr, output_len) = ret?;
	let output = extract_output_data(instance_wrapper, output_ptr, output_len)?;

	Ok(output)
}

fn inject_input_data(
	instance: &mut InstanceWrapper,
	allocator: &mut FreeingBumpHeapAllocator,
	data: &[u8],
) -> Result<(Pointer<u8>, WordSize)> {
	let mut ctx = instance.store_mut();
	let memory = ctx.data().memory();
	let data_len = data.len() as WordSize;
	let data_ptr = allocator.allocate(&mut MemoryWrapper(&memory, &mut ctx), data_len)?;
	util::write_memory_from(instance.store_mut(), data_ptr, data)?;
	Ok((data_ptr, data_len))
}

fn extract_output_data(
	instance: &InstanceWrapper,
	output_ptr: u32,
	output_len: u32,
) -> Result<Vec<u8>> {
	let ctx = instance.store();

	// Do a length check before allocating. The returned output should not be bigger than the
	// available WASM memory. Otherwise, a malicious parachain can trigger a large allocation,
	// potentially causing memory exhaustion.
	//
	// Get the size of the WASM memory in bytes.
	let memory_size = ctx.as_context().data().memory().data_size(ctx);
	if checked_range(output_ptr as usize, output_len as usize, memory_size).is_none() {
		Err(Error::OutputExceedsBounds)?
	}
	let mut output = vec![0; output_len as usize];

	util::read_memory_into(ctx, Pointer::new(output_ptr), &mut output)?;
	Ok(output)
}
