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

use super::helper;
use crate::COUNTER;
use proc_macro_warning::FormattedWarning;
use syn::spanned::Spanned;

/// The definition of the pallet validate unsigned implementation.
pub struct ValidateUnsignedDef {
	/// Compile time warning.
	pub warning: Option<FormattedWarning>,
}

impl ValidateUnsignedDef {
	pub fn try_from(item: &mut syn::Item) -> syn::Result<Self> {
		let item = if let syn::Item::Impl(item) = item {
			item
		} else {
			let msg = "Invalid pallet::validate_unsigned, expected item impl";
			return Err(syn::Error::new(item.span(), msg));
		};

		if item.trait_.is_none() {
			let msg = "Invalid pallet::validate_unsigned, expected impl<..> ValidateUnsigned for \
				Pallet<..>";
			return Err(syn::Error::new(item.span(), msg));
		}

		if let Some(last) = item.trait_.as_ref().unwrap().1.segments.last() {
			if last.ident != "ValidateUnsigned" {
				let msg = "Invalid pallet::validate_unsigned, expected trait ValidateUnsigned";
				return Err(syn::Error::new(last.span(), msg));
			}
		} else {
			let msg = "Invalid pallet::validate_unsigned, expected impl<..> ValidateUnsigned for \
				Pallet<..>";
			return Err(syn::Error::new(item.span(), msg));
		}

		helper::check_pallet_struct_usage(&item.self_ty)?;
		helper::check_impl_gen(&item.generics, item.impl_token.span())?;

		// Check if the `#[allow(deprecated)]` attribute is present
		let allow_dep: syn::Attribute = syn::parse_quote!(#[allow(deprecated)]);
		let warning = if item.attrs.iter().any(|attr| attr == &allow_dep) {
			None
		} else {
			const DEPRECATION_MSG: &str = "#[pallet::validate_unsigned] will be removed after \
				April 2027. Use `#[pallet::authorize]` with `frame_system::AuthorizeCall` instead.";
			const REFERENCE_LINK: &str = "https://github.com/paritytech/polkadot-sdk/issues/2415";
			let count = COUNTER.with(|counter| counter.borrow_mut().inc());
			let warning = proc_macro_warning::FormattedWarning::new_deprecated(
				format!("validate_unsigned_deprecation_{count}"),
				format!("\n{DEPRECATION_MSG}\n\n\t\tFor more info see:\n\t\t\t{REFERENCE_LINK}"),
				item.span(),
			);

			Some(warning)
		};

		Ok(ValidateUnsignedDef { warning })
	}
}
