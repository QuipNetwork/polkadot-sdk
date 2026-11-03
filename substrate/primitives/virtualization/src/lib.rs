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

//! This crate is intended for use by runtime code (e.g. pallet-contracts) to spawn PolkaVM
//! instances and execute calls into them. Its purpose is to add one layer of abstraction so that
//! it works transparently from the actual runtime (via the host functions defined in this crate)
//! but also from tests (which run natively).
//!
//! The crate exposes the runtime-side forwarder API and the [`#[runtime_interface]`] declaration
//! plus the [`VirtManagerBackend`] trait that backs it. The concrete polkavm-driven backend lives
//! in the `sc-virtualization` client crate, which registers a `VirtManager` via [`VirtManagerExt`]
//! before dispatching runtime calls.
//!
//! Please keep in mind that the interface is kept simple because it has to match the interface
//! of the host function so that the abstraction works. It will never expose the whole PolkaVM
//! interface.
//!
//! # ⚠️ Unstable API — Do Not Use in Production ⚠️
//!
//! **This crate's API is unstable and subject to breaking changes without notice.**
//!
//! The virtualization host functions exposed by this crate have **not been stabilized** and are
//! **not available on Polkadot** (or any other production relay/parachain) until they are. Using
//! them in a production runtime **will cause your runtime to break** when the API changes.
//!
//! This crate should **only** be used for:
//! - Local testing and development
//! - Experimentation on test networks
//!
//! **Do not** ship runtimes that depend on this crate to any chain you care about. There is no
//! stability guarantee and no deprecation period — the interface may change at any time.

#![cfg_attr(substrate_runtime, no_std)]

mod forwarder;
mod host_functions;
pub mod tests;

pub use crate::tests::run as run_tests;
pub use forwarder::{Execution, Instance, Module};
#[cfg(not(substrate_runtime))]
pub use host_functions::{ExecBuffer, ExecStatus, VirtManagerBackend, VirtManagerExt};

/// Aggregate of all host functions exposed by this crate.
///
/// Plug this into your node's `HostFunctions` tuple alongside the other host-function sets
/// (e.g. [`sp_io::SubstrateHostFunctions`]). At runtime, register a [`VirtManagerExt`]
/// wrapping your backend on the externalities before any of these host functions are
/// invoked — without it, every call panics. All other interaction with the virtualization
/// machinery should go through [`Module`], [`Instance`], and [`Execution`].
#[cfg(not(substrate_runtime))]
#[doc(inline)]
pub use host_functions::virtualization::HostFunctions;

use num_enum::{IntoPrimitive, TryFromPrimitive};

/// The target we use for all logging.
pub const LOG_TARGET: &str = "virtualization";

/// Maximum length of a syscall symbol in bytes.
pub const MAX_SYSCALL_SYMBOL_LEN: usize = 32;

/// Opaque handle to a compiled module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct ModuleId(u32);

/// Opaque handle to a virtualization instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct InstanceId(u32);

/// The result of running a virtualization instance.
///
/// `V` is the idle [`Instance`] type (returned on completion or error),
/// `I` is the [`Execution`] type (returned when a syscall is encountered).
#[derive(Debug, PartialEq, Eq)]
pub enum ExecResult<V, I> {
	/// Execution finished normally. Returns the idle instance for potential reuse.
	Finished {
		/// The idle instance, ready to be prepared for another call.
		instance: V,
		/// How much gas is remaining after the execution.
		gas_left: i64,
	},
	/// A syscall was encountered. The caller should handle the syscall and then
	/// call [`Execution::run`] again to continue execution.
	Syscall {
		/// The running execution, with memory access available for syscall handling.
		execution: I,
		/// How much gas is remaining at the point of the syscall.
		gas_left: i64,
		/// The symbol identifying the syscall.
		syscall_symbol: SyscallSymbol,
		/// Register arguments a0-a5.
		a0: u64,
		a1: u64,
		a2: u64,
		a3: u64,
		a4: u64,
		a5: u64,
	},
	/// An error occurred during execution. Returns the idle instance.
	Error {
		/// The idle instance, returned for potential reuse or cleanup.
		instance: V,
		/// The error that occurred.
		error: ExecError,
	},
}

/// A syscall symbol with a maximum length of [`MAX_SYSCALL_SYMBOL_LEN`] bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct SyscallSymbol {
	bytes: [u8; MAX_SYSCALL_SYMBOL_LEN],
	len: u64,
}

impl SyscallSymbol {
	/// Build a [`SyscallSymbol`] from a byte slice.
	///
	/// Returns `None` if `bytes` exceeds [`MAX_SYSCALL_SYMBOL_LEN`].
	pub fn new(bytes: &[u8]) -> Option<Self> {
		if bytes.len() > MAX_SYSCALL_SYMBOL_LEN {
			return None;
		}
		let mut buf = [0u8; MAX_SYSCALL_SYMBOL_LEN];
		buf[..bytes.len()].copy_from_slice(bytes);
		Some(Self { bytes: buf, len: bytes.len() as u64 })
	}
}

impl AsRef<[u8]> for SyscallSymbol {
	fn as_ref(&self) -> &[u8] {
		&self.bytes[..self.len as usize]
	}
}

/// Status returned by the `compile_*` host functions.
///
/// Lets the caller distinguish a cheap cache hit from a fresh compile so it can
/// charge weight accordingly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, TryFromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum CompileStatus {
	/// Module was already in the per-extension cache; no compilation occurred.
	Cached = 0,
	/// Module was freshly compiled (and, for [`Module::from_storage_key`], the
	/// program bytes were read from storage).
	Compiled = 1,
}

/// Errors that can be emitted when compiling a program into a module.
#[derive(TryFromPrimitive, IntoPrimitive, strum::EnumCount, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum ModuleError {
	/// The supplied code was invalid.
	InvalidImage = -1,
	/// No module with the given identifier has been compiled yet.
	NotCached = -2,
	/// No code was found at the supplied storage key.
	NotFound = -3,
}

/// Errors that can be emitted when instantiating a new virtualization instance.
#[derive(TryFromPrimitive, IntoPrimitive, strum::EnumCount, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum InstantiateError {
	/// The supplied code was invalid.
	InvalidImage = -1,
	/// The supplied `module_id` was invalid or the module was not found.
	InvalidModule = -2,
}

/// Errors that can be emitted when executing a new virtualization instance.
#[derive(TryFromPrimitive, IntoPrimitive, strum::EnumCount, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum ExecError {
	/// The supplied `instance_id` was invalid or the instance was destroyed.
	InvalidInstance = -1,
	/// The supplied code was invalid. Most likely caused by invalid entry points.
	InvalidImage = -2,
	/// The execution ran out of gas before it could finish.
	OutOfGas = -3,
	/// The execution trapped before it could finish.
	///
	/// This can be caused by executing an `unimp` instruction.
	Trap = -4,
}

/// Errors that can be emitted when accessing a virtualization instance's memory.
#[derive(TryFromPrimitive, IntoPrimitive, strum::EnumCount, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum MemoryError {
	/// The supplied `instance_id` was invalid or the instance was destroyed.
	InvalidInstance = -1,
	/// The memory region specified is not accessible.
	OutOfBounds = -2,
}

/// Errors that can be emitted when destroying a virtualization instance.
#[derive(TryFromPrimitive, IntoPrimitive, strum::EnumCount, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum DestroyError {
	/// The supplied `instance_id` was invalid or the instance was destroyed.
	InvalidInstance = -1,
}
