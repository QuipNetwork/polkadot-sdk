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

#[macro_export]
macro_rules! define_flavor {
	($module: ident, $($capability:tt)*) => {
		#[cfg(feature = "std")]
		pub mod $module {
			$(crate::define_flavor!(impl $module, $capability);)*
		}
	};
	(impl $module: ident, wasm) => {
		include!(concat!(env!("OUT_DIR"), "/wasm_binary_", stringify!($module), ".rs"));
	};
	(impl $module: ident, consts) => {
		pub const WASM_FILE_NAME: &str = concat!("wasm_binary_", stringify!($module), ".rs");
	};
}

#[macro_export]
macro_rules! define_flavors {
	($($capabilities:tt)*) => {
		define_flavor!(spec_version_incremented, $($capabilities)*);
		define_flavor!(relay_parent_offset_2, $($capabilities)*);
		define_flavor!(elastic_scaling_500ms, $($capabilities)*);
		define_flavor!(elastic_scaling, $($capabilities)*);
		define_flavor!(elastic_scaling_12s_slot, $($capabilities)*);
		define_flavor!(block_bundling, $($capabilities)*);
		define_flavor!(sync_backing, $($capabilities)*);
		define_flavor!(v3, $($capabilities)*);
		define_flavor!(v3_rpo_2, $($capabilities)*);
		define_flavor!(v3_rpo_4, $($capabilities)*);
		define_flavor!(v3_rpo_6, $($capabilities)*);
		define_flavor!(v3_rpo_15, $($capabilities)*);
		define_flavor!(elastic_scaling_v3, $($capabilities)*);
		define_flavor!(slot_duration_18s, $($capabilities)*);
		define_flavor!(with_authority_discovery, $($capabilities)*);
	};
}
