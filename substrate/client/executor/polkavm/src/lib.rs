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

use polkavm::{CallError, Caller, Reg};
use sc_executor_common::{
	error::{Error, WasmError},
	wasm_runtime::{AllocationStats, HeapAllocStrategy, WasmInstance, WasmModule},
};
use sp_runtime_interface::unpack_ptr_and_len;
use sp_wasm_interface::{
	Function, FunctionContext, HostFunctions, Pointer, Value, ValueType, WordSize,
};

#[repr(transparent)]
pub struct InstancePre(polkavm::InstancePre<(), String>);

#[repr(transparent)]
pub struct Instance(polkavm::Instance<(), String>);

impl WasmModule for InstancePre {
	fn new_instance(
		&self,
		_heap_alloc_strategy: HeapAllocStrategy,
	) -> Result<Box<dyn WasmInstance>, Error> {
		Ok(Box::new(Instance(self.0.instantiate()?)))
	}
}

impl WasmInstance for Instance {
	fn call_with_allocation_stats(
		&mut self,
		name: &str,
		raw_data: &[u8],
	) -> (Result<Vec<u8>, Error>, Option<AllocationStats>) {
		let pc = match self.0.module().exports().find(|e| e.symbol() == name) {
			Some(export) => export.program_counter(),
			None => {
				return (
					Err(format!("cannot call into the runtime: export not found: '{name}'").into()),
					None,
				)
			},
		};

		let Ok(raw_data_length) = u32::try_from(raw_data.len()) else {
			return (
				Err(format!("cannot call runtime method '{name}': input payload is too big").into()),
				None,
			);
		};

		// TODO: This will leak guest memory; find a better solution.

		// Make sure that the memory is cleared...
		if let Err(err) = self.0.reset_memory() {
			return (
				Err(format!(
					"call into the runtime method '{name}' failed: reset memory failed: {err}"
				)
				.into()),
				None,
			);
		}

		// ... and allocate space for the input payload.
		if let Err(err) = self.0.sbrk(raw_data_length) {
			return (
				Err(format!(
					"call into the runtime method '{name}' failed: reset memory failed: {err}"
				)
				.into()),
				None,
			);
		}

		// Grab the address of where the guest's heap starts; that's where we've just allocated
		// the memory for the input payload.
		let data_pointer = self.0.module().memory_map().heap_base();

		if let Err(err) = self.0.write_memory(data_pointer, raw_data) {
			return (Err(format!("call into the runtime method '{name}': failed to write the input payload into guest memory: {err}").into()), None);
		}

		match self.0.call_typed(&mut (), pc, (data_pointer, raw_data_length)) {
			Ok(()) => {},
			Err(CallError::Trap) => {
				return (
					Err(format!("call into the runtime method '{name}' failed: trap").into()),
					None,
				)
			},
			Err(CallError::Error(err)) => {
				return (
					Err(format!("call into the runtime method '{name}' failed: {err}").into()),
					None,
				)
			},
			Err(CallError::User(err)) => {
				return (
					Err(format!("call into the runtime method '{name}' failed: {err}").into()),
					None,
				)
			},
			Err(CallError::NotEnoughGas) => unreachable!("gas metering is never enabled"),
			Err(CallError::Step) => unreachable!("stepping is never enabled"),
		};

		let result = self.0.reg(Reg::A0);
		let (result_pointer, result_length) = unpack_ptr_and_len(result);
		let output = match self.0.read_memory(result_pointer, result_length) {
			Ok(output) => output,
			Err(error) => {
				return (Err(format!("call into the runtime method '{name}' failed: failed to read the return payload: {error}").into()), None)
			},
		};

		(Ok(output), None)
	}
}

struct Context<'r, 'a>(&'r mut polkavm::Caller<'a, ()>);

impl<'r, 'a> FunctionContext for Context<'r, 'a> {
	fn read_memory_into(
		&mut self,
		address: Pointer<u8>,
		dest: &mut [u8],
	) -> sp_wasm_interface::Result<()> {
		self.0
			.instance
			.read_memory_into(u32::from(address), dest)
			.map_err(|error| error.to_string())
			.map(|_| ())
	}

	fn write_memory(&mut self, address: Pointer<u8>, data: &[u8]) -> sp_wasm_interface::Result<()> {
		self.0
			.instance
			.write_memory(u32::from(address), data)
			.map_err(|error| error.to_string())
	}

	fn allocate_memory(&mut self, size: WordSize) -> sp_wasm_interface::Result<Pointer<u8>> {
		let pointer = match self.0.instance.sbrk(0) {
			Ok(pointer) => pointer.expect("fetching the current heap pointer never fails"),
			Err(err) => return Err(format!("sbrk failed: {err}")),
		};

		// TODO: This will leak guest memory; find a better solution.
		match self.0.instance.sbrk(size) {
			Ok(Some(_)) => (),
			Ok(None) => return Err(String::from("allocation error")),
			Err(err) => return Err(format!("sbrk failed: {err}")),
		}

		Ok(Pointer::new(pointer))
	}

	fn deallocate_memory(&mut self, _ptr: Pointer<u8>) -> sp_wasm_interface::Result<()> {
		// This is only used by the allocator host function, which is unused under PolkaVM.
		unimplemented!("'deallocate_memory' is never used when running under PolkaVM");
	}

	fn register_panic_error_message(&mut self, _message: &str) {
		unimplemented!("'register_panic_error_message' is never used when running under PolkaVM");
	}
}

fn call_host_function(caller: &mut Caller<()>, function: &dyn Function) -> Result<(), String> {
	let mut args = [Value::I64(0); Reg::ARG_REGS.len()];
	let mut nth_reg = 0;
	for (nth_arg, kind) in function.signature().args.iter().enumerate() {
		match kind {
			ValueType::I32 => {
				args[nth_arg] = Value::I32(caller.instance.reg(Reg::ARG_REGS[nth_reg]) as i32);
				nth_reg += 1;
			},
			ValueType::F32 => {
				args[nth_arg] = Value::F32(caller.instance.reg(Reg::ARG_REGS[nth_reg]) as u32);
				nth_reg += 1;
			},
			ValueType::I64 => {
				if caller.instance.is_64_bit() {
					args[nth_arg] = Value::I64(caller.instance.reg(Reg::ARG_REGS[nth_reg]) as i64);
					nth_reg += 1;
				} else {
					let value_lo = caller.instance.reg(Reg::ARG_REGS[nth_reg]);
					nth_reg += 1;

					let value_hi = caller.instance.reg(Reg::ARG_REGS[nth_reg]);
					nth_reg += 1;

					args[nth_arg] =
						Value::I64((u64::from(value_lo) | (u64::from(value_hi) << 32)) as i64);
				}
			},
			ValueType::F64 => {
				if caller.instance.is_64_bit() {
					args[nth_arg] = Value::F64(caller.instance.reg(Reg::ARG_REGS[nth_reg]));
					nth_reg += 1;
				} else {
					let value_lo = caller.instance.reg(Reg::ARG_REGS[nth_reg]);
					nth_reg += 1;

					let value_hi = caller.instance.reg(Reg::ARG_REGS[nth_reg]);
					nth_reg += 1;

					args[nth_arg] = Value::F64(u64::from(value_lo) | (u64::from(value_hi) << 32));
				}
			},
		}
	}

	log::trace!(
		"Calling host function: '{}', args = {:?}",
		function.name(),
		&args[..function.signature().args.len()]
	);

	let value = match function
		.execute(&mut Context(caller), &mut args.into_iter().take(function.signature().args.len()))
	{
		Ok(value) => value,
		Err(error) => {
			let name = function.name();
			return Err(format!("call into the host function '{name}' failed: {error}"));
		},
	};

	if let Some(value) = value {
		match value {
			Value::I32(value) => {
				caller.instance.set_reg(Reg::A0, value as u64);
			},
			Value::F32(value) => {
				caller.instance.set_reg(Reg::A0, value as u64);
			},
			Value::I64(value) => {
				if caller.instance.is_64_bit() {
					caller.instance.set_reg(Reg::A0, value as u64);
				} else {
					caller.instance.set_reg(Reg::A0, value as u64);
					caller.instance.set_reg(Reg::A1, (value >> 32) as u64);
				}
			},
			Value::F64(value) => {
				if caller.instance.is_64_bit() {
					caller.instance.set_reg(Reg::A0, value as u64);
				} else {
					caller.instance.set_reg(Reg::A0, value as u64);
					caller.instance.set_reg(Reg::A1, (value >> 32) as u64);
				}
			},
		}
	}

	Ok(())
}

pub fn create_runtime<H>(blob: &polkavm::ProgramBlob) -> Result<Box<dyn WasmModule>, WasmError>
where
	H: HostFunctions,
{
	static ENGINE: std::sync::OnceLock<Result<polkavm::Engine, polkavm::Error>> =
		std::sync::OnceLock::new();

	let engine = ENGINE.get_or_init(|| {
		let config = polkavm::Config::from_env()?;
		polkavm::Engine::new(&config)
	});

	let engine = match engine {
		Ok(ref engine) => engine,
		Err(ref error) => {
			return Err(WasmError::Other(error.to_string()));
		},
	};

	let module =
		polkavm::Module::from_blob(&engine, &polkavm::ModuleConfig::default(), blob.clone())?;

	let mut linker = polkavm::Linker::new();

	for function in H::host_functions() {
		linker.define_untyped(function.name(), |mut caller: Caller<()>| {
			call_host_function(&mut caller, function)
		})?;
	}
	let instance_pre = linker.instantiate_pre(&module)?;
	Ok(Box::new(InstancePre(instance_pre)))
}
