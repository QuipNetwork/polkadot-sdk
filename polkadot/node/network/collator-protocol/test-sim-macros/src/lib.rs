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

//! Proc-macros for the collator-protocol deterministic test framework.
//!
//! [`macro@sim_test`] marks a generic scenario function and fans it out to one `#[test]`
//! shell per registered subsystem-under-test implementation. The result is that every
//! scenario doubles as a differential test: the same prose runs against `LegacyValidator`
//! and `ExperimentalValidator`, and any divergence in observable behaviour fails the test.
//!
//! Registration of implementations is hardcoded into this macro for now (just the two
//! validator-side variants). When more impls onboard, the registry can become an attribute
//! argument or a separate `register_impls!` macro.
//!
//! # Usage
//!
//! ```ignore
//! #[sim_test]
//! fn my_scenario<S>()
//! where
//!     S: polkadot_collator_protocol_test_sim::harness::SubsystemUnderTest<
//!         Message = polkadot_node_subsystem::messages::CollatorProtocolMessage,
//!     >,
//!     polkadot_node_subsystem::messages::AllMessages: From<
//!         <S::Message as polkadot_overseer::AssociateOutgoing>::OutgoingMessages,
//!     >,
//!     polkadot_node_subsystem::messages::AllMessages: From<S::Message>,
//! {
//!     // ...scenario body parameterised over S...
//! }
//! ```
//!
//! Expands to two `#[test]` functions, `my_scenario__legacy` and `my_scenario__experimental`,
//! each calling `my_scenario::<LegacyValidator>()` / `my_scenario::<ExperimentalValidator>()`.

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{
	parse::{Parse, ParseStream, Parser},
	parse_macro_input,
	punctuated::Punctuated,
	ItemFn, LitStr, Token,
};

/// `#[sim_test]` — fan-out attribute for differential scenarios.
///
/// The annotated function must be generic over a single type parameter conventionally
/// named `S` and bounded as a [`SubsystemUnderTest`]. The macro generates one
/// `#[test]`-marked wrapper per registered implementation; each wrapper instantiates the
/// generic body with the corresponding adapter type and calls it.
///
/// # Filters
///
/// `#[sim_test(only = "legacy")]` runs the scenario against `LegacyValidator` only.
/// `#[sim_test(only = "experimental")]` runs against `ExperimentalValidator` only.
/// `#[sim_test(skip = "legacy")]` / `#[sim_test(skip = "experimental")]` invert the filter.
/// Unfiltered (`#[sim_test]`) runs against both.
///
/// # Known-bug expected-failure
///
/// `#[sim_test(bug_on = "experimental")]` marks the experimental wrapper with
/// `#[should_panic]` so the suite stays green while a tracked defect is open. The bug must
/// be filed somewhere — either inline as `bug_on = "experimental", bug_url = "..."` or
/// via the surrounding scenario module's doc comment. When the bug is fixed the
/// `should_panic` flips and the test fails loudly, prompting removal of the marker.
///
/// `bug_on` and `only`/`skip` are mutually exclusive at the *same impl*: if you want
/// `bug_on = "experimental"`, do not also `skip = "experimental"`; the legacy wrapper
/// stays unmodified.
///
/// [`SubsystemUnderTest`]: ../polkadot_collator_protocol_test_sim/harness/sim/trait.SubsystemUnderTest.html
#[proc_macro_attribute]
pub fn sim_test(attr: TokenStream, item: TokenStream) -> TokenStream {
	let opts = match parse_opts(attr) {
		Ok(o) => o,
		Err(e) => return e.to_compile_error().into(),
	};
	let input = parse_macro_input!(item as ItemFn);
	let fn_name = input.sig.ident.clone();
	let legacy_name = format_ident!("{}__legacy", fn_name);
	let experimental_name = format_ident!("{}__experimental", fn_name);

	let bug_url = match &opts.bug_url {
		Some(url) => quote! { Some(#url) },
		None => quote! { None },
	};

	let legacy_test = if opts.filter.includes_legacy() {
		gen_variant(
			&fn_name,
			&legacy_name,
			quote! { crate::impls::LegacyValidator },
			"legacy",
			opts.bug_on_legacy,
			&bug_url,
		)
	} else {
		quote! {}
	};
	let experimental_test = if opts.filter.includes_experimental() {
		gen_variant(
			&fn_name,
			&experimental_name,
			quote! { crate::impls::ExperimentalValidator },
			"experimental",
			opts.bug_on_experimental,
			&bug_url,
		)
	} else {
		quote! {}
	};

	let expanded = quote! {
		#input
		#legacy_test
		#experimental_test
	};
	expanded.into()
}

/// Generate one `#[test]` wrapper for a single impl.
///
/// Without `bug_on`, the wrapper just runs the scenario. With `bug_on`, the scenario is a known
/// bug on this impl: it is expected to fail until fixed. The known-bug semantics — swallow the
/// failure while the bug is open, fail loudly with a self-documenting message once it's fixed —
/// live in the generic `polkadot_subsystem_test_sim::run_known_bug`; this just wires the
/// generated test into it. `impl_label` disambiguates the message between the two impls.
fn gen_variant(
	fn_name: &proc_macro2::Ident,
	test_name: &proc_macro2::Ident,
	impl_ty: proc_macro2::TokenStream,
	impl_label: &str,
	bug_on: bool,
	bug_url: &proc_macro2::TokenStream,
) -> proc_macro2::TokenStream {
	let body = if bug_on {
		// Label the known-bug message with both the generated test name and the impl, e.g.
		// `foo__experimental (experimental)`, so a fixed-bug failure points at exactly one wrapper.
		let labelled = format!("{test_name} ({impl_label})");
		quote! {
			polkadot_subsystem_test_sim::run_known_bug(
				#labelled,
				#bug_url,
				|| #fn_name::<#impl_ty>(),
			);
		}
	} else {
		quote! {
			#fn_name::<#impl_ty>();
		}
	};
	quote! {
		#[::core::prelude::v1::test]
		#[allow(non_snake_case)]
		fn #test_name() {
			#body
		}
	}
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Impl {
	Legacy,
	Experimental,
}

#[derive(Default)]
struct Opts {
	filter: FilterOpt,
	bug_on_legacy: bool,
	bug_on_experimental: bool,
	bug_url: Option<String>,
}

#[derive(Clone, Copy)]
enum FilterOpt {
	Both,
	Only(Impl),
}

impl Default for FilterOpt {
	fn default() -> Self {
		FilterOpt::Both
	}
}

impl FilterOpt {
	fn includes_legacy(self) -> bool {
		matches!(self, FilterOpt::Both | FilterOpt::Only(Impl::Legacy))
	}
	fn includes_experimental(self) -> bool {
		matches!(self, FilterOpt::Both | FilterOpt::Only(Impl::Experimental))
	}
}

struct KeyValue {
	key: syn::Ident,
	value: LitStr,
}

impl Parse for KeyValue {
	fn parse(input: ParseStream) -> syn::Result<Self> {
		let key: syn::Ident = input.parse()?;
		input.parse::<Token![=]>()?;
		let value: LitStr = input.parse()?;
		Ok(KeyValue { key, value })
	}
}

fn parse_opts(attr: TokenStream) -> syn::Result<Opts> {
	if attr.is_empty() {
		return Ok(Opts::default());
	}
	let parser = |input: ParseStream| -> syn::Result<Opts> {
		let pairs: Punctuated<KeyValue, Token![,]> = Punctuated::parse_terminated(input)?;
		let mut opts = Opts::default();
		let mut filter_seen = false;
		// Remember where each `bug_on` was written so a later consistency error can point at
		// the offending key rather than the attribute as a whole.
		let mut bug_on_legacy_span = None;
		let mut bug_on_experimental_span = None;
		let mut bug_url_span = None;
		for kv in pairs {
			let key = kv.key.to_string();
			let val = kv.value.value();
			match (key.as_str(), val.as_str()) {
				("only", "legacy") | ("skip", "experimental") => {
					if filter_seen {
						return Err(syn::Error::new(
							kv.key.span(),
							"only one of `only` / `skip` may be supplied",
						));
					}
					opts.filter = FilterOpt::Only(Impl::Legacy);
					filter_seen = true;
				},
				("only", "experimental") | ("skip", "legacy") => {
					if filter_seen {
						return Err(syn::Error::new(
							kv.key.span(),
							"only one of `only` / `skip` may be supplied",
						));
					}
					opts.filter = FilterOpt::Only(Impl::Experimental);
					filter_seen = true;
				},
				("bug_on", "legacy") => {
					if bug_on_legacy_span.replace(kv.key.span()).is_some() {
						return Err(syn::Error::new(
							kv.key.span(),
							"duplicate `bug_on = \"legacy\"`",
						));
					}
					opts.bug_on_legacy = true;
				},
				("bug_on", "experimental") => {
					if bug_on_experimental_span.replace(kv.key.span()).is_some() {
						return Err(syn::Error::new(
							kv.key.span(),
							"duplicate `bug_on = \"experimental\"`",
						));
					}
					opts.bug_on_experimental = true;
				},
				("bug_url", _) => {
					if bug_url_span.replace(kv.key.span()).is_some() {
						return Err(syn::Error::new(kv.key.span(), "duplicate `bug_url`"));
					}
					opts.bug_url = Some(val);
				},
				_ => {
					return Err(syn::Error::new(
						kv.key.span(),
						"`#[sim_test]` accepts: only/skip/bug_on = \"legacy\" | \"experimental\", \
						 bug_url = \"...\"",
					));
				},
			}
		}

		// A `bug_on` for an impl the filter excludes would be silently dropped: no wrapper is
		// generated for that impl, so the known-bug marker never runs and a fixed bug would
		// never flip the test loud. Reject the combination instead of swallowing it. (The
		// docs promise these are mutually exclusive at the same impl.)
		if opts.bug_on_legacy && !opts.filter.includes_legacy() {
			return Err(syn::Error::new(
				bug_on_legacy_span.expect("set when bug_on_legacy is true"),
				"`bug_on = \"legacy\"` conflicts with the filter, which excludes the legacy \
				 wrapper — the known-bug marker would never run. Drop the `only`/`skip` that \
				 excludes legacy, or drop this `bug_on`.",
			));
		}
		if opts.bug_on_experimental && !opts.filter.includes_experimental() {
			return Err(syn::Error::new(
				bug_on_experimental_span.expect("set when bug_on_experimental is true"),
				"`bug_on = \"experimental\"` conflicts with the filter, which excludes the \
				 experimental wrapper — the known-bug marker would never run. Drop the \
				 `only`/`skip` that excludes experimental, or drop this `bug_on`.",
			));
		}

		Ok(opts)
	};
	parser.parse(attr)
}
