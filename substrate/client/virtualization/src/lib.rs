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

//! Host-side PolkaVM backend for the [`sp_virtualization`] host functions.
//!
//! Provides the concrete [`VirtManager`] that drives `polkavm` to compile, instantiate
//! and execute programs on behalf of the runtime. Register it with the externalities via
//! [`sp_virtualization::VirtManagerExt::new`].

use polkavm::{
	CacheModel, CompileError, Config, CostModelKind, Engine, GasMeteringKind, InterruptKind,
	MemoryAccessError, Module, ModuleConfig, ProgramCounter, RawInstance, Reg,
};
use sp_virtualization::{
	DestroyError, ExecBuffer, ExecError, ExecStatus, InstanceId, InstantiateError, MemoryError,
	ModuleError, ModuleId, SyscallSymbol, VirtManagerBackend, LOG_TARGET,
};
use std::{
	collections::HashMap,
	sync::{Arc, LazyLock},
};

/// This is the single PolkaVM engine we use for everything.
///
/// By using a common engine we allow PolkaVM to use caching. This caching is important
/// to reduce startup costs. This is even the case when instances use different code.
static ENGINE: LazyLock<Engine> = LazyLock::new(|| {
	let mut config = Config::from_env().expect("Invalid config.");
	config.set_worker_count(10);
	config.set_default_cost_model(Some(CostModelKind::Full(CacheModel::L2Hit)));
	Engine::new(&config).expect("Failed to initialize PolkaVM.")
});

fn map_memory_error(error: MemoryAccessError) -> MemoryError {
	match error {
		MemoryAccessError::OutOfRangeAccess { .. } | MemoryAccessError::MemoryLimitReached => {
			MemoryError::OutOfBounds
		},
		MemoryAccessError::Error(error) => {
			panic!("Error accessing PolkaVM memory: {error}. This is a bug.");
		},
	}
}

/// A compiled module together with lookup tables derived from it once at compile time.
///
/// Precomputing these avoids an O(n) `exports()` scan on every `prepare` call and a
/// re-copy of the import symbol bytes on every syscall.
struct CompiledModule {
	module: Module,
	/// Export symbol → program counter. Consulted by `prepare`.
	exports: HashMap<Vec<u8>, ProgramCounter>,
	/// Preassembled `SyscallSymbol` for each import, indexed by hostcall index.
	imports: Vec<SyscallSymbol>,
}

impl CompiledModule {
	fn new(module: Module) -> Result<Self, ModuleError> {
		let exports = module
			.exports()
			.map(|e| (e.symbol().as_bytes().to_vec(), e.program_counter()))
			.collect();

		// `ImportsIter` yields `Option<ProgramSymbol>` (None on malformed offsets);
		// we also reject any symbol longer than our fixed-size `SyscallSymbol` buffer
		// so the Ecalli hot path can just index into the vec.
		let imports: Vec<SyscallSymbol> = module
			.imports()
			.into_iter()
			.map(|symbol| {
				let symbol = symbol.ok_or(ModuleError::InvalidImage)?;
				SyscallSymbol::new(symbol.as_bytes()).ok_or(ModuleError::InvalidImage)
			})
			.collect::<Result<_, _>>()?;

		Ok(Self { module, exports, imports })
	}
}

/// The state an instance can be in.
enum InstanceState {
	/// Idle — awaiting a `prepare` call before it can be run.
	Idle(RawInstance),
	/// Ready — prepared for execution, or suspended mid-execution at a syscall.
	Ready(RawInstance),
}

/// An instance together with the compiled module it was instantiated from.
///
/// The module handle is kept here so the hot paths can consult the precomputed
/// export/import tables without going through `RawInstance::module()`.
struct ManagedInstance {
	state: InstanceState,
	module: Arc<CompiledModule>,
}

/// Manages virtualization instances and their lifecycle.
///
/// Instance and module IDs are assigned deterministically from incrementing counters,
/// ensuring no non-determinism across different executions.
///
/// NOTE: The per-instance `cache` deduplicates modules within the lifetime of one
/// `VirtManager` (i.e. one externalities extension, i.e. one block). Cross-block
/// reuse is deferred to PolkaVM's built-in on-disk persistent cache.
pub struct VirtManager {
	instances: HashMap<InstanceId, ManagedInstance>,
	modules: HashMap<ModuleId, Arc<CompiledModule>>,
	cache: HashMap<Vec<u8>, Arc<CompiledModule>>,
	instance_counter: u32,
	module_counter: u32,
}

impl Default for VirtManager {
	fn default() -> Self {
		Self {
			instances: HashMap::new(),
			modules: HashMap::new(),
			cache: HashMap::new(),
			instance_counter: 0,
			module_counter: 0,
		}
	}
}

impl VirtManager {
	fn next_module_id(&mut self) -> ModuleId {
		let old = self.module_counter;
		self.module_counter = old + 1;
		ModuleId::from(old)
	}

	fn next_instance_id(&mut self) -> InstanceId {
		let old = self.instance_counter;
		self.instance_counter = old + 1;
		InstanceId::from(old)
	}

	fn prepare_impl(
		managed: ManagedInstance,
		function: &[u8],
	) -> (ManagedInstance, Result<(), ExecError>) {
		let ManagedInstance { state, module } = managed;
		let mut instance = match state {
			InstanceState::Idle(i) => i,
			ready @ InstanceState::Ready(_) => {
				return (ManagedInstance { state: ready, module }, Err(ExecError::InvalidInstance));
			},
		};
		match module.exports.get(function).copied() {
			Some(pc) => {
				instance.prepare_call_untyped(pc, &[]);
				(ManagedInstance { state: InstanceState::Ready(instance), module }, Ok(()))
			},
			None => {
				log::debug!(
					target: LOG_TARGET,
					"Export not found: {}",
					String::from_utf8_lossy(function),
				);
				(
					ManagedInstance { state: InstanceState::Idle(instance), module },
					Err(ExecError::InvalidImage),
				)
			},
		}
	}

	fn run_impl(
		managed: ManagedInstance,
		gas_left: i64,
		a0: u64,
	) -> (ManagedInstance, Result<(ExecStatus, ExecBuffer), ExecError>) {
		let ManagedInstance { state, module } = managed;
		let mut instance = match state {
			InstanceState::Ready(i) => i,
			idle @ InstanceState::Idle(_) => {
				return (ManagedInstance { state: idle, module }, Err(ExecError::InvalidInstance));
			},
		};

		instance.set_reg(Reg::A0, a0);
		instance.set_gas(gas_left);

		let interrupt = match instance.run() {
			Ok(interrupt) => interrupt,
			Err(err) => panic!("Polkavm failed during execution: {err}. This is a bug."),
		};

		match interrupt {
			InterruptKind::Finished => {
				let gas_left = instance.gas();
				(
					ManagedInstance { state: InstanceState::Idle(instance), module },
					Ok((ExecStatus::Finished, ExecBuffer { gas_left, ..Default::default() })),
				)
			},
			InterruptKind::Trap => (
				ManagedInstance { state: InstanceState::Idle(instance), module },
				Err(ExecError::Trap),
			),
			InterruptKind::NotEnoughGas => (
				ManagedInstance { state: InstanceState::Idle(instance), module },
				Err(ExecError::OutOfGas),
			),
			InterruptKind::Step | InterruptKind::Segfault(_) => {
				unreachable!("PolkaVM is configured per config not to emit Step or Segfault; qed");
			},
			InterruptKind::Ecalli(hostcall_index) => {
				let Some(syscall_symbol) = module.imports.get(hostcall_index as usize).copied()
				else {
					return (
						ManagedInstance { state: InstanceState::Idle(instance), module },
						Err(ExecError::InvalidImage),
					);
				};
				let gas_left = instance.gas();
				let a0 = instance.reg(Reg::A0);
				let a1 = instance.reg(Reg::A1);
				let a2 = instance.reg(Reg::A2);
				let a3 = instance.reg(Reg::A3);
				let a4 = instance.reg(Reg::A4);
				let a5 = instance.reg(Reg::A5);
				(
					ManagedInstance { state: InstanceState::Ready(instance), module },
					Ok((
						ExecStatus::Syscall,
						ExecBuffer { gas_left, syscall_symbol, a0, a1, a2, a3, a4, a5 },
					)),
				)
			},
		}
	}
}

impl VirtManagerBackend for VirtManager {
	fn compile_from_bytes(
		&mut self,
		program: &[u8],
		identifier: Option<&[u8]>,
	) -> Result<ModuleId, ModuleError> {
		let mut module_config = ModuleConfig::new();
		module_config.set_gas_metering(Some(GasMeteringKind::Sync));
		let module =
			Module::new(&ENGINE, &module_config, program.into()).map_err(|err| match err {
				CompileError::ValidationFailed(err) => {
					log::debug!(target: LOG_TARGET, "Failed to compile program: {}", err);
					ModuleError::InvalidImage
				},
				CompileError::Error(err) => {
					panic!("Polkavm failed during compilation: {err}. This is a bug.");
				},
			})?;
		let compiled = Arc::new(CompiledModule::new(module)?);

		let module_id = self.next_module_id();

		if let Some(identifier) = identifier {
			self.cache.insert(identifier.to_vec(), compiled.clone());
		}
		self.modules.insert(module_id, compiled);

		Ok(module_id)
	}

	fn lookup(&mut self, identifier: &[u8]) -> Result<ModuleId, ModuleError> {
		let compiled = self.cache.get(identifier).cloned().ok_or(ModuleError::NotCached)?;
		let module_id = self.next_module_id();
		self.modules.insert(module_id, compiled);
		Ok(module_id)
	}

	fn instantiate(&mut self, module_id: ModuleId) -> Result<InstanceId, InstantiateError> {
		let compiled = self.modules.get(&module_id).ok_or(InstantiateError::InvalidModule)?.clone();

		let instance = compiled.module.instantiate().map_err(|err| {
			log::debug!(target: LOG_TARGET, "Failed to instantiate program: {err}");
			InstantiateError::InvalidImage
		})?;

		let instance_id = self.next_instance_id();

		self.instances.insert(
			instance_id,
			ManagedInstance { state: InstanceState::Idle(instance), module: compiled },
		);

		Ok(instance_id)
	}

	fn prepare(&mut self, instance_id: InstanceId, function: &[u8]) -> Result<(), ExecError> {
		let managed = self.instances.remove(&instance_id).ok_or(ExecError::InvalidInstance)?;
		let (managed, result) = Self::prepare_impl(managed, function);
		self.instances.insert(instance_id, managed);
		result
	}

	fn run(
		&mut self,
		instance_id: InstanceId,
		gas_left: i64,
		a0: u64,
	) -> Result<(ExecStatus, ExecBuffer), ExecError> {
		let managed = self.instances.remove(&instance_id).ok_or(ExecError::InvalidInstance)?;
		let (managed, result) = Self::run_impl(managed, gas_left, a0);
		self.instances.insert(instance_id, managed);
		result
	}

	fn destroy(&mut self, instance_id: InstanceId) -> Result<(), DestroyError> {
		if self.instances.remove(&instance_id).is_some() {
			Ok(())
		} else {
			Err(DestroyError::InvalidInstance)
		}
	}

	fn read_memory(
		&mut self,
		instance_id: InstanceId,
		offset: u32,
		dest: &mut [u8],
	) -> Result<(), MemoryError> {
		let Some(ManagedInstance { state: InstanceState::Ready(instance), .. }) =
			self.instances.get_mut(&instance_id)
		else {
			return Err(MemoryError::InvalidInstance);
		};
		instance.read_memory_into(offset, dest).map(|_| ()).map_err(map_memory_error)
	}

	fn write_memory(
		&mut self,
		instance_id: InstanceId,
		offset: u32,
		src: &[u8],
	) -> Result<(), MemoryError> {
		let Some(ManagedInstance { state: InstanceState::Ready(instance), .. }) =
			self.instances.get_mut(&instance_id)
		else {
			return Err(MemoryError::InvalidInstance);
		};
		instance.write_memory(offset, src).map_err(map_memory_error)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Two `VirtManager` instances must not share any cache state — confirms the cache lives on
	/// the struct, not in process-global storage.
	#[test]
	fn cache_does_not_leak_between_instances() {
		let program = sp_virtualization_test_fixture::binary();
		let key: &[u8] = b"some-key";

		let mut a = VirtManager::default();
		a.compile_from_bytes(program, Some(key)).unwrap();
		assert!(matches!(a.lookup(key), Ok(_)));

		let mut b = VirtManager::default();
		assert!(matches!(b.lookup(key), Err(ModuleError::NotCached)));
	}

	/// Passing `None` to `compile_from_bytes` must not populate the cache.
	#[test]
	fn compile_from_bytes_none_skips_cache() {
		let program = sp_virtualization_test_fixture::binary();
		let key: &[u8] = b"would-be-key";

		let mut m = VirtManager::default();
		m.compile_from_bytes(program, None).unwrap();
		assert!(matches!(m.lookup(key), Err(ModuleError::NotCached)));
	}
}
