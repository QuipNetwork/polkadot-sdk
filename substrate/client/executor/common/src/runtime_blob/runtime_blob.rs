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

use crate::{error::WasmError, wasm_runtime::HeapAllocStrategy};
use polkavm::ArcBytes;
use wasm_instrument::parity_wasm::elements::{
	deserialize_buffer, serialize, ExportEntry, External, Internal, MemorySection, MemoryType,
	Module, Section,
};

/// A program blob containing a Substrate runtime.
#[derive(Clone)]
pub struct RuntimeBlob(BlobKind);

#[derive(Clone)]
enum BlobKind {
	WebAssembly(Module),
	PolkaVM((polkavm::ProgramBlob, ArcBytes)),
}

impl RuntimeBlob {
	/// Create `RuntimeBlob` from the given WASM or PolkaVM compressed program blob.
	///
	/// See [`sp_maybe_compressed_blob`] for details about decompression.
	pub fn uncompress_if_needed(wasm_code: &[u8]) -> Result<Self, WasmError> {
		use sp_maybe_compressed_blob::CODE_BLOB_BOMB_LIMIT;
		let wasm_code = sp_maybe_compressed_blob::decompress(wasm_code, CODE_BLOB_BOMB_LIMIT)
			.map_err(|e| WasmError::Other(format!("Decompression error: {:?}", e)))?;
		Self::new(&wasm_code)
	}

	/// Create `RuntimeBlob` from the given WASM or PolkaVM program blob.
	///
	/// Returns `Err` if the blob cannot be deserialized.
	///
	/// Will only accept a PolkaVM program if the `SUBSTRATE_ENABLE_POLKAVM` environment
	/// variable is set to `1`.
	pub fn new(raw_blob: &[u8]) -> Result<Self, WasmError> {
		if raw_blob.starts_with(b"PVM\0") {
			if crate::is_polkavm_enabled() {
				let raw = ArcBytes::from(raw_blob);
				let blob = polkavm::ProgramBlob::parse(raw.clone())?;
				return Ok(Self(BlobKind::PolkaVM((blob, raw))));
			} else {
				return Err(WasmError::Other("expected a WASM runtime blob, found a PolkaVM runtime blob; set the 'SUBSTRATE_ENABLE_POLKAVM' environment variable to enable the experimental PolkaVM-based executor".to_string()));
			}
		}

		let raw_module: Module = deserialize_buffer(raw_blob)
			.map_err(|e| WasmError::Other(format!("cannot deserialize module: {:?}", e)))?;
		Ok(Self(BlobKind::WebAssembly(raw_module)))
	}

	/// Run a pass that instrument this module so as to introduce a deterministic stack height
	/// limit.
	///
	/// It will introduce a global mutable counter. The instrumentation will increase the counter
	/// according to the "cost" of the callee. If the cost exceeds the `stack_depth_limit` constant,
	/// the instrumentation will trap. The counter will be decreased as soon as the the callee
	/// returns.
	///
	/// The stack cost of a function is computed based on how much locals there are and the maximum
	/// depth of the wasm operand stack.
	///
	/// Only valid for WASM programs; will return an error if the blob is a PolkaVM program.
	pub fn inject_stack_depth_metering(self, stack_depth_limit: u32) -> Result<Self, WasmError> {
		let injected_module =
			wasm_instrument::inject_stack_limiter(self.into_webassembly_blob()?, stack_depth_limit)
				.map_err(|e| {
					WasmError::Other(format!("cannot inject the stack limiter: {:?}", e))
				})?;

		Ok(Self(BlobKind::WebAssembly(injected_module)))
	}

	/// Converts a WASM memory import into a memory section and exports it.
	///
	/// Does nothing if there's no memory import.
	///
	/// May return an error in case the WASM module is invalid.
	///
	/// Only valid for WASM programs; will return an error if the blob is a PolkaVM program.
	pub fn convert_memory_import_into_export(&mut self) -> Result<(), WasmError> {
		let raw_module = self.as_webassembly_blob_mut()?;
		let import_section = match raw_module.import_section_mut() {
			Some(import_section) => import_section,
			None => return Ok(()),
		};

		let import_entries = import_section.entries_mut();
		for index in 0..import_entries.len() {
			let entry = &import_entries[index];
			let memory_ty = match entry.external() {
				External::Memory(memory_ty) => *memory_ty,
				_ => continue,
			};

			let memory_name = entry.field().to_owned();
			import_entries.remove(index);

			raw_module
				.insert_section(Section::Memory(MemorySection::with_entries(vec![memory_ty])))
				.map_err(|error| {
					WasmError::Other(format!(
					"can't convert a memory import into an export: failed to insert a new memory section: {}",
					error
				))
				})?;

			if raw_module.export_section_mut().is_none() {
				// A module without an export section is somewhat unrealistic, but let's do this
				// just in case to cover all of our bases.
				raw_module
					.insert_section(Section::Export(Default::default()))
					.expect("an export section can be always inserted if it doesn't exist; qed");
			}
			raw_module
				.export_section_mut()
				.expect("export section already existed or we just added it above, so it always exists; qed")
				.entries_mut()
				.push(ExportEntry::new(memory_name, Internal::Memory(0)));

			break;
		}

		Ok(())
	}

	/// Removes the maximum page limit from the memory section.
	///
	/// Only valid for WASM programs; will return an error if the blob is a PolkaVM program.
	pub fn clear_memory_max_limit(&mut self) -> Result<(), WasmError> {
		let raw_module = self.as_webassembly_blob_mut()?;
		let memory_section = match raw_module.memory_section_mut() {
			Some(section) => section,
			None => return Ok(()),
		};
		for memory_ty in memory_section.entries_mut() {
			*memory_ty = MemoryType::new(memory_ty.limits().initial(), None);
		}
		Ok(())
	}

	/// Modifies the blob's memory section according to the given `heap_alloc_strategy`.
	///
	/// Will return an error in case there is no memory section present,
	/// or if the memory section is empty.
	///
	/// Only valid for WASM programs; will return an error if the blob is a PolkaVM program.
	#[deprecated]
	pub fn setup_memory_according_to_heap_alloc_strategy(
		&mut self,
		heap_alloc_strategy: HeapAllocStrategy,
	) -> Result<(), WasmError> {
		let raw_module = self.as_webassembly_blob_mut()?;
		let memory_section = raw_module
			.memory_section_mut()
			.ok_or_else(|| WasmError::Other("no memory section found".into()))?;

		if memory_section.entries().is_empty() {
			return Err(WasmError::Other("memory section is empty".into()));
		}
		for memory_ty in memory_section.entries_mut() {
			let initial = memory_ty.limits().initial();
			let (min, max) = match heap_alloc_strategy {
				HeapAllocStrategy::Dynamic { maximum_pages } => {
					// Ensure `initial <= maximum_pages`
					(maximum_pages.map(|m| m.min(initial)).unwrap_or(initial), maximum_pages)
				},
				HeapAllocStrategy::Static { extra_pages } => {
					let pages = initial.saturating_add(extra_pages);
					(pages, Some(pages))
				},
			};
			*memory_ty = MemoryType::new(min, max);
		}
		Ok(())
	}

	/// Scans the wasm blob for the first section with the name that matches the given. Returns the
	/// contents of the custom section if found or `None` otherwise.
	///
	/// Only valid for WASM programs; will return an error if the blob is a PolkaVM program.
	pub fn custom_section_contents(&self, section_name: &str) -> Option<&[u8]> {
		self.as_webassembly_blob()
			.ok()?
			.custom_sections()
			.find(|cs| cs.name() == section_name)
			.map(|cs| cs.payload())
	}

	/// Consumes this runtime blob and serializes it.
	pub fn serialize(self) -> Vec<u8> {
		match self.0 {
			BlobKind::WebAssembly(raw_module) => {
				serialize(raw_module).expect("serializing into a vec should succeed; qed")
			},
			BlobKind::PolkaVM(ref blob) => blob.1.to_vec(),
		}
	}

	fn as_webassembly_blob(&self) -> Result<&Module, WasmError> {
		match self.0 {
			BlobKind::WebAssembly(ref raw_module) => Ok(raw_module),
			BlobKind::PolkaVM(..) => Err(WasmError::Other(
				"expected a WebAssembly program; found a PolkaVM program blob".into(),
			)),
		}
	}

	fn as_webassembly_blob_mut(&mut self) -> Result<&mut Module, WasmError> {
		match self.0 {
			BlobKind::WebAssembly(ref mut raw_module) => Ok(raw_module),
			BlobKind::PolkaVM(..) => Err(WasmError::Other(
				"expected a WebAssembly program; found a PolkaVM program blob".into(),
			)),
		}
	}

	fn into_webassembly_blob(self) -> Result<Module, WasmError> {
		match self.0 {
			BlobKind::WebAssembly(raw_module) => Ok(raw_module),
			BlobKind::PolkaVM(..) => Err(WasmError::Other(
				"expected a WebAssembly program; found a PolkaVM program blob".into(),
			)),
		}
	}

	/// Gets a reference to the inner PolkaVM program blob, if this is a PolkaVM program.
	pub fn as_polkavm_blob(&self) -> Option<&polkavm::ProgramBlob> {
		match self.0 {
			BlobKind::WebAssembly(..) => None,
			BlobKind::PolkaVM((ref blob, _)) => Some(blob),
		}
	}
}
