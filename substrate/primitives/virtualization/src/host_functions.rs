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

use crate::{
	CompileStatus, DestroyError, ExecError, InstanceId, InstantiateError, MemoryError, ModuleError,
	ModuleId, SyscallSymbol,
};
use core::mem;
use num_enum::{IntoPrimitive, TryFromPrimitive};
use sp_runtime_interface::{
	pass_by::{
		ConvertAndReturnAs, PassAs, PassFatPointerAndRead, PassFatPointerAndReadOption,
		PassFatPointerAndWrite, PassPointerAndWrite,
	},
	runtime_interface,
};

/// Buffer shared between runtime and executor for passing syscall data across the
/// host function boundary.
///
/// The runtime allocates this on its stack and passes it via pointer.
/// The host fills it in when returning from [`crate::Execution::run`].
#[derive(Debug, Default)]
#[repr(C)]
pub struct ExecBuffer {
	/// Gas remaining after the execution step.
	pub gas_left: i64,
	/// The syscall symbol (only meaningful when the status is [`ExecStatus::Syscall`]).
	pub syscall_symbol: SyscallSymbol,
	/// Syscall register arguments a0-a5 (only meaningful for [`ExecStatus::Syscall`]).
	pub a0: u64,
	pub a1: u64,
	pub a2: u64,
	pub a3: u64,
	pub a4: u64,
	pub a5: u64,
}

impl AsRef<[u8]> for ExecBuffer {
	fn as_ref(&self) -> &[u8] {
		// SAFETY: `ExecBuffer` is `#[repr(C)]` with a well-defined layout of primitive fields
		// and no implicit padding.
		unsafe {
			core::slice::from_raw_parts(self as *const Self as *const u8, mem::size_of::<Self>())
		}
	}
}

impl AsMut<[u8]> for ExecBuffer {
	fn as_mut(&mut self) -> &mut [u8] {
		// SAFETY: `ExecBuffer` is `#[repr(C)]` with a well-defined layout of primitive fields
		// and no implicit padding.
		unsafe {
			core::slice::from_raw_parts_mut(self as *mut Self as *mut u8, mem::size_of::<Self>())
		}
	}
}

/// Status returned by the `run` host function.
#[derive(Debug, Clone, Copy, PartialEq, Eq, TryFromPrimitive, IntoPrimitive)]
#[repr(u32)]
pub enum ExecStatus {
	/// Execution finished normally.
	Finished = 0,
	/// A syscall was encountered — check the [`ExecBuffer`] for details.
	Syscall = 1,
}

/// Implement the i64 wire encoding for error types used in [`RIIntResult`].
///
/// Errors carry their wire value directly: each variant is declared with a negative
/// `#[repr(i32)]` discriminant, so the encoding is a sign-extending cast to `i64` and
/// the decoding is a `TryFrom<i64>` via `i32`. This relies on [`IntoPrimitive`] (for
/// `i32::from`) and [`TryFromPrimitive`] (for `Self::try_from(i32)`) being derived on
/// the type.
macro_rules! impl_ri_error_encoding {
	($($t:ty),+ $(,)?) => {$(
		impl From<$t> for i64 {
			fn from(error: $t) -> Self {
				i32::from(error) as i64
			}
		}

		impl TryFrom<i64> for $t {
			type Error = ();
			fn try_from(value: i64) -> Result<Self, Self::Error> {
				let v = i32::try_from(value).map_err(|_| ())?;
				Self::try_from(v).map_err(|_| ())
			}
		}
	)+};
}

impl_ri_error_encoding!(ModuleError, InstantiateError, ExecError, DestroyError, MemoryError);

impl From<u32> for ModuleId {
	fn from(id: u32) -> Self {
		Self(id)
	}
}

impl From<ModuleId> for u32 {
	fn from(id: ModuleId) -> Self {
		id.0
	}
}

impl IntoI64 for ModuleId {
	const MAX: i64 = u32::MAX as i64;
}

impl From<ModuleId> for i64 {
	fn from(id: ModuleId) -> Self {
		u32::from(id) as i64
	}
}

impl TryFrom<i64> for ModuleId {
	type Error = ();
	fn try_from(value: i64) -> Result<Self, Self::Error> {
		u32::try_from(value).map(ModuleId::from).map_err(|_| ())
	}
}

/// Result of a successful `compile_*` host function call.
///
/// Pairs the produced [`ModuleId`] with a [`CompileStatus`] so the runtime can tell whether
/// the call hit the in-extension cache (cheap) or required a fresh compile (expensive).
///
/// Wire encoding (packed into the `i64` return value):
/// - low 32 bits hold the `ModuleId`
/// - bits 32-39 hold the [`CompileStatus`] discriminant (`u8`)
/// - bits 40-62 are reserved for future packed fields
/// - the result space stays disjoint from the negative range used by [`ModuleError`].
pub struct CompiledModule {
	pub id: ModuleId,
	pub status: CompileStatus,
}

impl IntoI64 for CompiledModule {
	const MAX: i64 = (1i64 << 40) - 1;
}

impl From<CompiledModule> for i64 {
	fn from(m: CompiledModule) -> Self {
		let status = u8::from(m.status) as i64;
		(status << 32) | (u32::from(m.id) as i64)
	}
}

impl TryFrom<i64> for CompiledModule {
	type Error = ();
	fn try_from(value: i64) -> Result<Self, Self::Error> {
		let id = ModuleId::from(value as u32);
		let status = CompileStatus::try_from((value >> 32) as u8).map_err(|_| ())?;
		Ok(Self { id, status })
	}
}

impl From<u32> for InstanceId {
	fn from(id: u32) -> Self {
		Self(id)
	}
}

impl From<InstanceId> for u32 {
	fn from(id: InstanceId) -> Self {
		id.0
	}
}

impl IntoI64 for InstanceId {
	const MAX: i64 = u32::MAX as i64;
}

impl From<InstanceId> for i64 {
	fn from(id: InstanceId) -> Self {
		u32::from(id) as i64
	}
}

impl TryFrom<i64> for InstanceId {
	type Error = ();
	fn try_from(value: i64) -> Result<Self, Self::Error> {
		u32::try_from(value).map(InstanceId::from).map_err(|_| ())
	}
}

// The following code is an excerpt from RFC-145 implementation (still to be adopted)
// ---vvv--- 8< CUT HERE 8< ---vvv---

/// Used to return less-than-64-bit value passed as `i64` through the FFI boundary.
/// Negative values are used to represent error variants.
pub enum RIIntResult<R, E> {
	/// Successful result
	Ok(R),
	/// Error result
	Err(E),
}

impl<R, E, OR, OE> From<Result<OR, OE>> for RIIntResult<R, E>
where
	R: From<OR>,
	E: From<OE>,
{
	fn from(result: Result<OR, OE>) -> Self {
		match result {
			Ok(value) => Self::Ok(value.into()),
			Err(error) => Self::Err(error.into()),
		}
	}
}

impl<R, E, OR, OE> From<RIIntResult<R, E>> for Result<OR, OE>
where
	OR: From<R>,
	OE: From<E>,
{
	fn from(result: RIIntResult<R, E>) -> Self {
		match result {
			RIIntResult::Ok(value) => Ok(value.into()),
			RIIntResult::Err(error) => Err(error.into()),
		}
	}
}

trait IntoI64: Into<i64> {
	const MAX: i64;
}

impl IntoI64 for u32 {
	const MAX: i64 = u32::MAX as i64;
}

impl<R: Into<i64> + IntoI64, E: Into<i64> + strum::EnumCount> From<RIIntResult<R, E>> for i64 {
	fn from(result: RIIntResult<R, E>) -> Self {
		match result {
			RIIntResult::Ok(value) => value.into(),
			RIIntResult::Err(e) => {
				let error_code: i64 = e.into();
				assert!(
					error_code < 0 && error_code >= -(E::COUNT as i64),
					"Error variant index out of bounds"
				);
				error_code
			},
		}
	}
}

impl<R: TryFrom<i64> + IntoI64, E: TryFrom<i64> + strum::EnumCount> TryFrom<i64>
	for RIIntResult<R, E>
{
	type Error = ();

	fn try_from(value: i64) -> Result<Self, Self::Error> {
		if value >= 0 && value <= R::MAX.into() {
			Ok(RIIntResult::Ok(value.try_into().map_err(|_| ())?))
		} else if value < 0 && value >= -(E::COUNT as i64) {
			Ok(RIIntResult::Err(value.try_into().map_err(|_| ())?))
		} else {
			Err(())
		}
	}
}

pub struct VoidResult;

impl IntoI64 for VoidResult {
	const MAX: i64 = 0;
}

impl From<()> for VoidResult {
	fn from(_: ()) -> Self {
		VoidResult
	}
}

impl From<VoidResult> for () {
	fn from(_: VoidResult) -> Self {
		()
	}
}

impl From<VoidResult> for i64 {
	fn from(_: VoidResult) -> Self {
		0
	}
}

impl TryFrom<i64> for VoidResult {
	type Error = ();

	fn try_from(value: i64) -> Result<Self, Self::Error> {
		if value == 0 {
			Ok(VoidResult)
		} else {
			Err(())
		}
	}
}

// ---^^^--- 8< CUT HERE 8< ---^^^---

/// Host functions used to spawn and call into PolkaVM instances.
///
/// Use [`crate::Instance`] instead of these raw host functions.
///
/// The [`crate::VirtManagerExt`] extension must be registered in the externalities
/// before any of these host functions can be used.
///
/// # ⚠️ Unstable — Do Not Use in Production ⚠️
///
/// **This interface is unstable and subject to breaking changes without notice.**
///
/// These host functions are **not available on Polkadot** (or any other production
/// relay/parachain) until the API has been stabilized. If you use them in a production
/// runtime, your runtime **will break** when the API changes.
///
/// Only use for local testing, development, and experimentation on test networks.
/// There is no stability guarantee and no deprecation period.
#[runtime_interface]
pub trait Virtualization {
	/// Compile the given program bytes into a module.
	///
	/// If `identifier` is `Some` and a module is already cached under it, no compilation
	/// occurs and the returned [`CompiledModule`] carries [`CompileStatus::Cached`].
	/// Otherwise the bytes are compiled, cached under `identifier` if supplied, and the
	/// returned [`CompiledModule`] carries [`CompileStatus::Compiled`].
	///
	/// The contained `module_id` can be passed to [`instantiate`] to create instances.
	fn compile_from_bytes(
		&mut self,
		program: PassFatPointerAndRead<&[u8]>,
		identifier: PassFatPointerAndReadOption<&[u8]>,
	) -> ConvertAndReturnAs<
		Result<CompiledModule, ModuleError>,
		RIIntResult<CompiledModule, ModuleError>,
		i64,
	> {
		use sp_externalities::ExternalitiesExt as _;
		use std::sync::Once;
		static WARN_ONCE: Once = Once::new();
		WARN_ONCE.call_once(|| {
			log::warn!(
				target: crate::LOG_TARGET,
				"Virtualization host functions are UNSTABLE and subject to breaking changes. \
				They are NOT available on Polkadot and using them in production will cause breakage. \
				Only use for testing and experimentation.",
			);
		});

		// Cache lookup first when an identifier is supplied.
		if let Some(identifier) = identifier {
			let cache_result = self
				.extension::<crate::VirtManagerExt>()
				.expect("VirtManagerExt not registered in externalities")
				.lookup(identifier);
			match cache_result {
				Ok(id) => return Ok(CompiledModule { id, status: CompileStatus::Cached }),
				Err(ModuleError::NotCached) => {},
				Err(err) => return Err(err),
			}
		}

		let id = self
			.extension::<crate::VirtManagerExt>()
			.expect("VirtManagerExt not registered in externalities")
			.compile_from_bytes(program, identifier)?;
		Ok(CompiledModule { id, status: CompileStatus::Compiled })
	}

	/// Look up a previously compiled module by `identifier`.
	///
	/// Returns `Ok(module_id)` if a module is cached under `identifier`,
	/// `Err(ModuleError::NotCached)` otherwise. This is a pure cache lookup — no storage
	/// access — so the caller pays only the lookup cost.
	fn lookup(
		&mut self,
		identifier: PassFatPointerAndRead<&[u8]>,
	) -> ConvertAndReturnAs<Result<ModuleId, ModuleError>, RIIntResult<ModuleId, ModuleError>, i64>
	{
		use sp_externalities::ExternalitiesExt as _;
		self.extension::<crate::VirtManagerExt>()
			.expect("VirtManagerExt not registered in externalities")
			.lookup(identifier)
	}

	/// Compile a module whose program bytes live at `storage_key`.
	///
	/// Returns a [`CompiledModule`] carrying [`CompileStatus::Cached`] if a module is already
	/// cached under `storage_key`. On a cache miss, loads the program bytes from storage at
	/// `storage_key`, compiles them, caches the result under that same key, and returns a
	/// [`CompiledModule`] carrying [`CompileStatus::Compiled`]. Pass an empty `child_trie`
	/// to read from the main state trie.
	fn compile_from_storage_key(
		&mut self,
		storage_key: PassFatPointerAndRead<&[u8]>,
		child_trie: PassFatPointerAndRead<&[u8]>,
	) -> ConvertAndReturnAs<
		Result<CompiledModule, ModuleError>,
		RIIntResult<CompiledModule, ModuleError>,
		i64,
	> {
		use sp_externalities::ExternalitiesExt as _;

		// Try the in-memory cache first.
		let cache_result = self
			.extension::<crate::VirtManagerExt>()
			.expect("VirtManagerExt not registered in externalities")
			.lookup(storage_key);

		match cache_result {
			Ok(id) => return Ok(CompiledModule { id, status: CompileStatus::Cached }),
			Err(ModuleError::NotCached) => {},
			Err(err) => return Err(err),
		}

		// Cache miss — load from storage.
		let code = if child_trie.is_empty() {
			self.storage(storage_key)
		} else {
			let child_info = sp_storage::ChildInfo::new_default(child_trie);
			self.child_storage(&child_info, storage_key)
		};

		let code = match code {
			Some(code) => code,
			None => return Err(ModuleError::NotFound),
		};

		// Compile and cache under the storage key so the next lookup hits.
		let id = self
			.extension::<crate::VirtManagerExt>()
			.expect("VirtManagerExt not registered in externalities")
			.compile_from_bytes(&code, Some(storage_key))?;
		Ok(CompiledModule { id, status: CompileStatus::Compiled })
	}

	/// Create a new instance from a compiled module.
	///
	/// Returns the `instance_id` which needs to be passed to reference this instance
	/// when using the other functions of this trait.
	fn instantiate(
		&mut self,
		module_id: PassAs<ModuleId, u32>,
	) -> ConvertAndReturnAs<
		Result<InstanceId, InstantiateError>,
		RIIntResult<InstanceId, InstantiateError>,
		i64,
	> {
		use sp_externalities::ExternalitiesExt as _;
		self.extension::<crate::VirtManagerExt>()
			.expect("VirtManagerExt not registered in externalities")
			.instantiate(module_id)
	}

	/// Prepare the given instance to run the named exported function.
	///
	/// This sets the program counter but does not start execution.
	/// Call [`run`] afterwards to begin.
	fn prepare(
		&mut self,
		instance_id: PassAs<InstanceId, u32>,
		function: PassFatPointerAndRead<&[u8]>,
	) -> ConvertAndReturnAs<Result<(), ExecError>, RIIntResult<VoidResult, ExecError>, i64> {
		use sp_externalities::ExternalitiesExt as _;
		self.extension::<crate::VirtManagerExt>()
			.expect("VirtManagerExt not registered in externalities")
			.prepare(instance_id, function)
	}

	/// Set register a0 and run until the next interrupt.
	///
	/// Returns `ExecStatus::Finished` or `ExecStatus::Syscall` as `u32`.
	/// When a syscall occurs, the syscall arguments are written into the
	/// `exec_buffer` via [`PassPointerAndWrite`].
	fn run(
		&mut self,
		instance_id: PassAs<InstanceId, u32>,
		gas_left: i64,
		a0: u64,
		exec_buffer: PassPointerAndWrite<&mut ExecBuffer, { mem::size_of::<ExecBuffer>() }>,
	) -> ConvertAndReturnAs<Result<u32, ExecError>, RIIntResult<u32, ExecError>, i64> {
		use sp_externalities::ExternalitiesExt as _;
		self.extension::<crate::VirtManagerExt>()
			.expect("VirtManagerExt not registered in externalities")
			.run(instance_id, gas_left, a0)
			.map(|(status, buf)| {
				*exec_buffer = buf;
				u32::from(status)
			})
	}

	/// Destroy this instance.
	///
	/// Any attempt accessing an instance after destruction will yield the `InvalidInstance` error.
	fn destroy(
		&mut self,
		instance_id: PassAs<InstanceId, u32>,
	) -> ConvertAndReturnAs<Result<(), DestroyError>, RIIntResult<VoidResult, DestroyError>, i64> {
		use sp_externalities::ExternalitiesExt as _;
		self.extension::<crate::VirtManagerExt>()
			.expect("VirtManagerExt not registered in externalities")
			.destroy(instance_id)
	}

	/// See [`crate::Execution::read_memory`].
	fn read_memory(
		&mut self,
		instance_id: PassAs<InstanceId, u32>,
		offset: u32,
		dest: PassFatPointerAndWrite<&mut [u8]>,
	) -> ConvertAndReturnAs<Result<(), MemoryError>, RIIntResult<VoidResult, MemoryError>, i64> {
		use sp_externalities::ExternalitiesExt as _;
		self.extension::<crate::VirtManagerExt>()
			.expect("VirtManagerExt not registered in externalities")
			.read_memory(instance_id, offset, dest)
	}

	/// See [`crate::Execution::write_memory`].
	fn write_memory(
		&mut self,
		instance_id: PassAs<InstanceId, u32>,
		offset: u32,
		src: PassFatPointerAndRead<&[u8]>,
	) -> ConvertAndReturnAs<Result<(), MemoryError>, RIIntResult<VoidResult, MemoryError>, i64> {
		use sp_externalities::ExternalitiesExt as _;
		self.extension::<crate::VirtManagerExt>()
			.expect("VirtManagerExt not registered in externalities")
			.write_memory(instance_id, offset, src)
	}
}

/// The host-side operations driven by the virtualization host functions.
///
/// The concrete implementation lives outside this crate (see `sc-virtualization`) so that
/// `sp-virtualization` itself does not depend on a specific virtual machine backend.
#[cfg(not(substrate_runtime))]
pub trait VirtManagerBackend: Send + 'static {
	/// Compile `program` into a new module.
	///
	/// If `identifier` is `Some`, the compiled module is retained in the per-extension cache
	/// keyed by that opaque byte slice so a later [`lookup`] with the same bytes can reuse it.
	///
	/// [`lookup`]: VirtManagerBackend::lookup
	fn compile_from_bytes(
		&mut self,
		program: &[u8],
		identifier: Option<&[u8]>,
	) -> Result<ModuleId, ModuleError>;

	/// Look up a module previously cached under `identifier`.
	///
	/// Returns [`ModuleError::NotCached`] if no module is cached under that key. The backend
	/// never reads storage — `identifier` is an opaque byte slice as far as it is concerned.
	fn lookup(&mut self, identifier: &[u8]) -> Result<ModuleId, ModuleError>;

	fn instantiate(&mut self, module_id: ModuleId) -> Result<InstanceId, InstantiateError>;

	fn prepare(&mut self, instance_id: InstanceId, function: &[u8]) -> Result<(), ExecError>;

	fn run(
		&mut self,
		instance_id: InstanceId,
		gas_left: i64,
		a0: u64,
	) -> Result<(ExecStatus, ExecBuffer), ExecError>;

	fn destroy(&mut self, instance_id: InstanceId) -> Result<(), DestroyError>;

	fn read_memory(
		&mut self,
		instance_id: InstanceId,
		offset: u32,
		dest: &mut [u8],
	) -> Result<(), MemoryError>;

	fn write_memory(
		&mut self,
		instance_id: InstanceId,
		offset: u32,
		src: &[u8],
	) -> Result<(), MemoryError>;
}

#[cfg(not(substrate_runtime))]
sp_externalities::decl_extension! {
	/// Extension wrapping a [`VirtManagerBackend`] so it can be accessed through
	/// the externalities by the virtualization host functions.
	pub struct VirtManagerExt(Box<dyn VirtManagerBackend>);
}

#[cfg(not(substrate_runtime))]
impl VirtManagerExt {
	/// Wrap the given backend so it can be registered as an externalities extension.
	pub fn new<B: VirtManagerBackend>(backend: B) -> Self {
		Self(Box::new(backend))
	}
}
