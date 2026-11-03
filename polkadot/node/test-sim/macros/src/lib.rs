// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! Proc-macros for the `polkadot-subsystem-test-sim` test framework.

use proc_macro::TokenStream;
use quote::quote;
use syn::{
	parse::{Parse, ParseStream},
	parse_macro_input, ItemFn, LitStr, Token,
};

/// Mark a test as asserting behavior that is a *known, tracked bug*.
///
/// The annotated test asserts the **correct** (not-yet-implemented) behavior. While the bug is
/// open the body fails; the failure is swallowed and the test passes. The moment the body stops
/// failing — the bug got fixed — the test fails loudly, telling the author to remove this
/// attribute so the test asserts the fixed behavior going forward.
///
/// This enables a test-first workflow: land the scenarios as known-bugs first; the fix PR's diff
/// is then just the removal of these one-line attributes, highlighting exactly what it fixed.
///
/// ```ignore
/// #[known_bug(url = "github:paritytech/polkadot-sdk#12345")]
/// #[test]
/// fn rejects_double_spend() {
///     // asserts the fixed behavior; panics today, passes once #12345 lands.
/// }
/// ```
///
/// `url` is optional: `#[known_bug]` is allowed for a bug with no tracking issue yet.
///
/// Expands to a call to `polkadot_subsystem_test_sim::run_known_bug`, so the consumer crate must
/// depend on `polkadot-subsystem-test-sim`. Place `#[known_bug]` above `#[test]`.
#[proc_macro_attribute]
pub fn known_bug(attr: TokenStream, item: TokenStream) -> TokenStream {
	let args = parse_macro_input!(attr as KnownBugArgs);
	let input = parse_macro_input!(item as ItemFn);

	let ItemFn { attrs, vis, sig, block } = input;
	let test_name = sig.ident.to_string();
	let url = match args.url {
		Some(url) => quote! { Some(#url) },
		None => quote! { None },
	};

	quote! {
		#(#attrs)*
		#vis #sig {
			polkadot_subsystem_test_sim::run_known_bug(#test_name, #url, || #block);
		}
	}
	.into()
}

/// Parsed `#[known_bug]` / `#[known_bug(url = "...")]` arguments.
#[derive(Default)]
struct KnownBugArgs {
	url: Option<String>,
}

impl Parse for KnownBugArgs {
	fn parse(input: ParseStream) -> syn::Result<Self> {
		let mut args = KnownBugArgs::default();
		if input.is_empty() {
			return Ok(args);
		}
		// Only `url = "..."` is supported.
		let key: syn::Ident = input.parse()?;
		if key != "url" {
			return Err(syn::Error::new(key.span(), "`#[known_bug]` only accepts `url = \"...\"`"));
		}
		input.parse::<Token![=]>()?;
		let val: LitStr = input.parse()?;
		args.url = Some(val.value());
		Ok(args)
	}
}
