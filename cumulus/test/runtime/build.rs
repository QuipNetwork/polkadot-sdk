// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.

// Cumulus is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Cumulus is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Cumulus.  If not, see <http://www.gnu.org/licenses/>.

#[path = "src/flavors.rs"]
mod flavors;

define_flavors!(consts);

#[cfg(feature = "std")]
fn main() {
	use substrate_wasm_builder::WasmBuilder;

	// A runtime with 6s slot duration which only authors one block per slot.
	WasmBuilder::init_with_defaults().build();

	WasmBuilder::init_with_defaults()
		.enable_feature("spec-version-3")
		.set_file_name(spec_version_incremented::WASM_FILE_NAME)
		.build();

	WasmBuilder::init_with_defaults()
		.enable_feature("velocity-3")
		.enable_feature("spec-version-3")
		.set_file_name(elastic_scaling::WASM_FILE_NAME)
		.build();

	// A runtime with 6s slots and block velocity 12.
	// Coupled with 12 cores it can produce a block every 500ms.
	WasmBuilder::init_with_defaults()
		.enable_feature("velocity-12")
		.set_file_name(elastic_scaling_500ms::WASM_FILE_NAME)
		.build();

	// A runtime that uses a relay parent offset of 2.
	WasmBuilder::init_with_defaults()
		.enable_feature("relay-parent-offset-2")
		.set_file_name(relay_parent_offset_2::WASM_FILE_NAME)
		.build();

	WasmBuilder::init_with_defaults()
		.enable_feature("sync-backing")
		.enable_feature("12s-slot")
		.set_file_name(sync_backing::WASM_FILE_NAME)
		.build();

	// An elastic scaling runtime with 12s slots.
	WasmBuilder::init_with_defaults()
		.enable_feature("12s-slot")
		.enable_feature("velocity-3")
		.enable_feature("spec-version-3")
		.set_file_name(elastic_scaling_12s_slot::WASM_FILE_NAME)
		.build();

	// A runtime that uses block-bundling.
	WasmBuilder::init_with_defaults()
		.enable_feature("velocity-12")
		.set_file_name(block_bundling::WASM_FILE_NAME)
		.build();

	WasmBuilder::init_with_defaults()
		.enable_feature("v3-descriptor")
		.set_file_name(v3::WASM_FILE_NAME)
		.build();

	WasmBuilder::init_with_defaults()
		.enable_feature("v3-descriptor")
		.enable_feature("relay-parent-offset-2")
		.set_file_name(v3_rpo_2::WASM_FILE_NAME)
		.build();

	WasmBuilder::init_with_defaults()
		.enable_feature("v3-descriptor")
		.enable_feature("relay-parent-offset-4")
		.set_file_name(v3_rpo_4::WASM_FILE_NAME)
		.build();

	WasmBuilder::init_with_defaults()
		.enable_feature("v3-descriptor")
		.enable_feature("relay-parent-offset-6")
		.set_file_name(v3_rpo_6::WASM_FILE_NAME)
		.build();

	WasmBuilder::init_with_defaults()
		.enable_feature("v3-descriptor")
		.enable_feature("relay-parent-offset-15")
		.set_file_name(v3_rpo_15::WASM_FILE_NAME)
		.build();

	WasmBuilder::init_with_defaults()
		.enable_feature("v3-descriptor")
		.enable_feature("velocity-3")
		.set_file_name(elastic_scaling_v3::WASM_FILE_NAME)
		.build();

	// A runtime with 18s slot duration with increased spec version for runtime upgrade testing.
	WasmBuilder::init_with_defaults()
		.enable_feature("18s-slot")
		.enable_feature("spec-version-3")
		.set_file_name(slot_duration_18s::WASM_FILE_NAME)
		.build();

	WasmBuilder::new()
		.with_current_project()
		.enable_feature("with-authority-discovery")
		.enable_feature("relay-parent-offset-2")
		.set_file_name(with_authority_discovery::WASM_FILE_NAME)
		.build();
}

#[cfg(not(feature = "std"))]
fn main() {}
