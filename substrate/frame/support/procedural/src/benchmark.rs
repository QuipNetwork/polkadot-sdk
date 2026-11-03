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

//! Home of the parsing and expansion code for the new pallet benchmarking syntax

use derive_syn_parse::Parse;
use frame_support_procedural_tools::generate_access_from_frame_or_crate;
use proc_macro::TokenStream;
use proc_macro2::{Ident, Span, TokenStream as TokenStream2};
use quote::{quote, quote_spanned, ToTokens};
use syn::{
	parse::{Nothing, ParseStream},
	parse_quote,
	punctuated::Punctuated,
	spanned::Spanned,
	token::{Comma, Gt, Lt, PathSep},
	Attribute, Error, Expr, ExprBlock, ExprCall, ExprPath, FnArg, Item, ItemFn, ItemMod, Pat, Path,
	PathArguments, PathSegment, Result, ReturnType, Signature, Stmt, Token, Type, TypePath,
	Visibility, WhereClause, WherePredicate,
};

mod keywords {
	use syn::custom_keyword;

	custom_keyword!(benchmark);
	custom_keyword!(benchmarks);
	custom_keyword!(block);
	custom_keyword!(extra);
	custom_keyword!(pov_mode);
	custom_keyword!(extrinsic_call);
	custom_keyword!(skip_meta);
	custom_keyword!(BenchmarkError);
	custom_keyword!(Result);
	custom_keyword!(MaxEncodedLen);
	custom_keyword!(Measured);
	custom_keyword!(Ignored);

	pub const BENCHMARK_TOKEN: &str = stringify!(benchmark);
	pub const BENCHMARKS_TOKEN: &str = stringify!(benchmarks);
}

/// This represents the raw parsed data for a param definition such as `x: Linear<10, 20>`.
#[derive(Clone)]
struct ParamDef {
	name: String,
	_typ: Type,
	start: syn::GenericArgument,
	end: syn::GenericArgument,
}

/// Allows easy parsing of the `<10, 20>` component of `x: Linear<10, 20>`.
#[derive(Parse)]
struct RangeArgs {
	_lt_token: Lt,
	start: syn::GenericArgument,
	_comma: Comma,
	end: syn::GenericArgument,
	_trailing_comma: Option<Comma>,
	_gt_token: Gt,
}

#[derive(Clone, Debug)]
struct BenchmarkAttrs {
	skip_meta: bool,
	extra: bool,
	pov_mode: Option<PovModeAttr>,
}

/// Represents a single benchmark option
enum BenchmarkAttr {
	Extra,
	SkipMeta,
	/// How the PoV should be measured.
	PoV(PovModeAttr),
}

impl syn::parse::Parse for PovModeAttr {
	fn parse(input: ParseStream) -> Result<Self> {
		let _pov: keywords::pov_mode = input.parse()?;
		let _eq: Token![=] = input.parse()?;
		let root = PovEstimationMode::parse(input)?;

		let mut maybe_content = None;
		let _ = || -> Result<()> {
			let content;
			syn::braced!(content in input);
			maybe_content = Some(content);
			Ok(())
		}();

		let per_key = match maybe_content {
			Some(content) => {
				let per_key = Punctuated::<PovModeKeyAttr, Token![,]>::parse_terminated(&content)?;
				per_key.into_iter().collect()
			},
			None => Vec::new(),
		};

		Ok(Self { root, per_key })
	}
}

impl syn::parse::Parse for BenchmarkAttr {
	fn parse(input: ParseStream) -> Result<Self> {
		let lookahead = input.lookahead1();
		if lookahead.peek(keywords::extra) {
			let _extra: keywords::extra = input.parse()?;
			Ok(BenchmarkAttr::Extra)
		} else if lookahead.peek(keywords::skip_meta) {
			let _skip_meta: keywords::skip_meta = input.parse()?;
			Ok(BenchmarkAttr::SkipMeta)
		} else if lookahead.peek(keywords::pov_mode) {
			PovModeAttr::parse(input).map(BenchmarkAttr::PoV)
		} else {
			Err(lookahead.error())
		}
	}
}

/// A `#[pov_mode = .. { .. }]` attribute.
#[derive(Debug, Clone)]
struct PovModeAttr {
	/// The root mode for this benchmarks.
	root: PovEstimationMode,
	/// The pov-mode for a specific key. This overwrites `root` for this key.
	per_key: Vec<PovModeKeyAttr>,
}

/// A single key-value pair inside the `{}` of a `#[pov_mode = .. { .. }]` attribute.
#[derive(Debug, Clone, derive_syn_parse::Parse)]
struct PovModeKeyAttr {
	/// A specific storage key for which to set the PoV mode.
	key: Path,
	_underscore: Token![:],
	/// The PoV mode for this key.
	mode: PovEstimationMode,
}

/// How the PoV should be estimated.
#[derive(Debug, Eq, PartialEq, Clone, Copy)]
pub enum PovEstimationMode {
	/// Use the maximal encoded length as provided by [`codec::MaxEncodedLen`].
	MaxEncodedLen,
	/// Measure the accessed value size in the pallet benchmarking and add some trie overhead.
	Measured,
	/// Do not estimate the PoV size for this storage item or benchmark.
	Ignored,
}

impl syn::parse::Parse for PovEstimationMode {
	fn parse(input: ParseStream) -> Result<Self> {
		let lookahead = input.lookahead1();
		if lookahead.peek(keywords::MaxEncodedLen) {
			let _max_encoded_len: keywords::MaxEncodedLen = input.parse()?;
			return Ok(PovEstimationMode::MaxEncodedLen);
		} else if lookahead.peek(keywords::Measured) {
			let _measured: keywords::Measured = input.parse()?;
			return Ok(PovEstimationMode::Measured);
		} else if lookahead.peek(keywords::Ignored) {
			let _ignored: keywords::Ignored = input.parse()?;
			return Ok(PovEstimationMode::Ignored);
		} else {
			return Err(lookahead.error());
		}
	}
}

impl ToString for PovEstimationMode {
	fn to_string(&self) -> String {
		match self {
			PovEstimationMode::MaxEncodedLen => "MaxEncodedLen".into(),
			PovEstimationMode::Measured => "Measured".into(),
			PovEstimationMode::Ignored => "Ignored".into(),
		}
	}
}

impl quote::ToTokens for PovEstimationMode {
	fn to_tokens(&self, tokens: &mut TokenStream2) {
		match self {
			PovEstimationMode::MaxEncodedLen => tokens.extend(quote!(MaxEncodedLen)),
			PovEstimationMode::Measured => tokens.extend(quote!(Measured)),
			PovEstimationMode::Ignored => tokens.extend(quote!(Ignored)),
		}
	}
}

impl syn::parse::Parse for BenchmarkAttrs {
	fn parse(input: ParseStream) -> syn::Result<Self> {
		let mut extra = false;
		let mut skip_meta = false;
		let mut pov_mode = None;
		let args = Punctuated::<BenchmarkAttr, Token![,]>::parse_terminated(&input)?;

		for arg in args.into_iter() {
			match arg {
				BenchmarkAttr::Extra => {
					if extra {
						return Err(input.error("`extra` can only be specified once"));
					}
					extra = true;
				},
				BenchmarkAttr::SkipMeta => {
					if skip_meta {
						return Err(input.error("`skip_meta` can only be specified once"));
					}
					skip_meta = true;
				},
				BenchmarkAttr::PoV(mode) => {
					if pov_mode.is_some() {
						return Err(input.error("`pov_mode` can only be specified once"));
					}
					pov_mode = Some(mode);
				},
			}
		}
		Ok(BenchmarkAttrs { extra, skip_meta, pov_mode })
	}
}

/// Represents the parsed extrinsic call for a benchmark
#[derive(Clone)]
enum BenchmarkCallDef {
	ExtrinsicCall { origin: Expr, expr_call: ExprCall, attr_span: Span }, // #[extrinsic_call]
	Block { block: ExprBlock, attr_span: Span },                          // #[block]
}

impl BenchmarkCallDef {
	/// Returns the `span()` for attribute
	fn attr_span(&self) -> Span {
		match self {
			BenchmarkCallDef::ExtrinsicCall { origin: _, expr_call: _, attr_span } => *attr_span,
			BenchmarkCallDef::Block { block: _, attr_span } => *attr_span,
		}
	}
}

/// Represents a parsed `#[benchmark]` or `#[instance_benchmark]` item.
#[derive(Clone)]
struct BenchmarkDef {
	params: Vec<ParamDef>,
	setup_stmts: Vec<Stmt>,
	call_def: BenchmarkCallDef,
	verify_stmts: Vec<Stmt>,
	last_stmt: Option<Stmt>,
	fn_sig: Signature,
	fn_vis: Visibility,
	fn_attrs: Vec<Attribute>,
}

/// used to parse something compatible with `Result<T, E>`
#[derive(Parse)]
struct ResultDef {
	_result_kw: keywords::Result,
	_lt: Token![<],
	unit: Type,
	_comma: Comma,
	e_type: TypePath,
	_gt: Token![>],
}

/// Ensures that `ReturnType` is a `Result<(), BenchmarkError>`, if specified
fn ensure_valid_return_type(item_fn: &ItemFn) -> Result<()> {
	if let ReturnType::Type(_, typ) = &item_fn.sig.output {
		let non_unit = |span| return Err(Error::new(span, "expected `()`"));
		let Type::Path(TypePath { path, qself: _ }) = &**typ else {
			return Err(Error::new(
					typ.span(),
					"Only `Result<(), BenchmarkError>` or a blank return type is allowed on benchmark function definitions",
				));
		};
		let seg = path
			.segments
			.last()
			.expect("to be parsed as a TypePath, it must have at least one segment; qed");
		let res: ResultDef = syn::parse2(seg.to_token_stream())?;
		// ensure T in Result<T, E> is ()
		let Type::Tuple(tup) = res.unit else { return non_unit(res.unit.span()) };
		if !tup.elems.is_empty() {
			return non_unit(tup.span());
		}
		let TypePath { path, qself: _ } = res.e_type;
		let seg = path
			.segments
			.last()
			.expect("to be parsed as a TypePath, it must have at least one segment; qed");
		syn::parse2::<keywords::BenchmarkError>(seg.to_token_stream())?;
	}
	Ok(())
}

/// Ensure that the passed statements do not contain any forbidden variable names
fn ensure_no_forbidden_variable_names(stmts: &[Stmt]) -> Result<()> {
	const FORBIDDEN_VAR_NAMES: [&str; 2] = ["recording", "verify"];
	for stmt in stmts {
		let Stmt::Local(l) = stmt else { continue };
		let Pat::Ident(ident) = &l.pat else { continue };
		if FORBIDDEN_VAR_NAMES.contains(&ident.ident.to_string().as_str()) {
			return Err(Error::new(
				ident.span(),
				format!(
					"Variables {FORBIDDEN_VAR_NAMES:?} are reserved for benchmarking internals.",
				),
			));
		}
	}
	Ok(())
}

/// Parses params such as `x: Linear<0, 1>`
fn parse_params(item_fn: &ItemFn) -> Result<Vec<ParamDef>> {
	let mut params: Vec<ParamDef> = Vec::new();
	for arg in &item_fn.sig.inputs {
		let invalid_param = |span| {
			return Err(Error::new(
				span,
				"Invalid benchmark function param. A valid example would be `x: Linear<5, 10>`.",
			));
		};

		let FnArg::Typed(arg) = arg else { return invalid_param(arg.span()) };
		let Pat::Ident(ident) = &*arg.pat else { return invalid_param(arg.span()) };

		// check param name
		let var_span = ident.span();
		let invalid_param_name = || {
			return Err(Error::new(
					var_span,
					"Benchmark parameter names must consist of a single lowercase letter (a-z) and no other characters.",
				));
		};
		let name = ident.ident.to_token_stream().to_string();
		if name.len() > 1 {
			return invalid_param_name();
		};
		let Some(name_char) = name.chars().next() else { return invalid_param_name() };
		if !name_char.is_alphabetic() || !name_char.is_lowercase() {
			return invalid_param_name();
		}

		// parse type
		let typ = &*arg.ty;
		let Type::Path(tpath) = typ else { return invalid_param(typ.span()) };
		let Some(segment) = tpath.path.segments.last() else { return invalid_param(typ.span()) };
		let args = segment.arguments.to_token_stream().into();
		let Ok(args) = syn::parse::<RangeArgs>(args) else { return invalid_param(typ.span()) };

		params.push(ParamDef { name, _typ: typ.clone(), start: args.start, end: args.end });
	}
	Ok(params)
}

/// Used in several places where the `#[extrinsic_call]` or `#[body]` annotation is missing
fn missing_call<T>(item_fn: &ItemFn) -> Result<T> {
	return Err(Error::new(
		item_fn.block.brace_token.span.join(),
		"No valid #[extrinsic_call] or #[block] annotation could be found in benchmark function body."
	));
}

/// Finds the `BenchmarkCallDef` and its index (within the list of stmts for the fn) and
/// returns them. Also handles parsing errors for invalid / extra call defs. AKA this is
/// general handling for `#[extrinsic_call]` and `#[block]`
fn parse_call_def(item_fn: &ItemFn) -> Result<(usize, BenchmarkCallDef)> {
	// #[extrinsic_call] / #[block] handling
	let call_defs = item_fn.block.stmts.iter().enumerate().filter_map(|(i, child)| {
			if let Stmt::Expr(Expr::Call(expr_call), _semi) = child {
				// #[extrinsic_call] case
				expr_call.attrs.iter().enumerate().find_map(|(k, attr)| {
					let segment = attr.path().segments.last()?;
					let _: keywords::extrinsic_call = syn::parse(segment.ident.to_token_stream().into()).ok()?;
					let mut expr_call = expr_call.clone();

					// consume #[extrinsic_call] tokens
					expr_call.attrs.remove(k);

					// extract origin from expr_call
					let Some(origin) = expr_call.args.first().cloned() else {
						return Some(Err(Error::new(expr_call.span(), "Single-item extrinsic calls must specify their origin as the first argument.")))
					};

					Some(Ok((i, BenchmarkCallDef::ExtrinsicCall { origin, expr_call, attr_span: attr.span() })))
				})
			} else if let Stmt::Expr(Expr::Block(block), _) = child {
				// #[block] case
				block.attrs.iter().enumerate().find_map(|(k, attr)| {
					let segment = attr.path().segments.last()?;
					let _: keywords::block = syn::parse(segment.ident.to_token_stream().into()).ok()?;
					let mut block = block.clone();

					// consume #[block] tokens
					block.attrs.remove(k);

					Some(Ok((i, BenchmarkCallDef::Block { block, attr_span: attr.span() })))
				})
			} else {
				None
			}
		}).collect::<Result<Vec<_>>>()?;
	Ok(match &call_defs[..] {
		[(i, call_def)] => (*i, call_def.clone()), // = 1
		[] => return missing_call(item_fn),
		_ => {
			return Err(Error::new(
				call_defs[1].1.attr_span(),
				"Only one #[extrinsic_call] or #[block] attribute is allowed per benchmark.",
			))
		},
	})
}

impl BenchmarkDef {
	/// Constructs a [`BenchmarkDef`] by traversing an existing [`ItemFn`] node.
	pub fn from(item_fn: &ItemFn) -> Result<BenchmarkDef> {
		let params = parse_params(item_fn)?;
		ensure_valid_return_type(item_fn)?;
		let (i, call_def) = parse_call_def(&item_fn)?;

		let (verify_stmts, last_stmt) = match item_fn.sig.output {
			ReturnType::Default =>
			// no return type, last_stmt should be None
			{
				(Vec::from(&item_fn.block.stmts[(i + 1)..item_fn.block.stmts.len()]), None)
			},
			ReturnType::Type(_, _) => {
				// defined return type, last_stmt should be Result<(), BenchmarkError>
				// compatible and should not be included in verify_stmts
				if i + 1 >= item_fn.block.stmts.len() {
					return Err(Error::new(
						item_fn.block.span(),
						"Benchmark `#[block]` or `#[extrinsic_call]` item cannot be the \
						last statement of your benchmark function definition if you have \
						defined a return type. You should return something compatible \
						with Result<(), BenchmarkError> (i.e. `Ok(())`) as the last statement \
						or change your signature to a blank return type.",
					));
				}
				let Some(stmt) = item_fn.block.stmts.last() else { return missing_call(item_fn) };
				(
					Vec::from(&item_fn.block.stmts[(i + 1)..item_fn.block.stmts.len() - 1]),
					Some(stmt.clone()),
				)
			},
		};

		let setup_stmts = Vec::from(&item_fn.block.stmts[0..i]);
		ensure_no_forbidden_variable_names(&setup_stmts)?;

		Ok(BenchmarkDef {
			params,
			setup_stmts,
			call_def,
			verify_stmts,
			last_stmt,
			fn_sig: item_fn.sig.clone(),
			fn_vis: item_fn.vis.clone(),
			fn_attrs: item_fn.attrs.clone(),
		})
	}
}

/// Parses and expands a `#[benchmarks]` or `#[instance_benchmarks]` invocation
pub fn benchmarks(
	attrs: TokenStream,
	tokens: TokenStream,
	instance: bool,
) -> syn::Result<TokenStream> {
	let krate = generate_access_from_frame_or_crate("frame-benchmarking")?;
	// gather module info
	let module: ItemMod = syn::parse(tokens)?;
	let mod_span = module.span();
	let where_clause = match syn::parse::<Nothing>(attrs.clone()) {
		Ok(_) => {
			if instance {
				quote!(T: Config<I>, I: 'static)
			} else {
				quote!(T: Config)
			}
		},
		Err(_) => {
			let mut where_clause_predicates = syn::parse::<WhereClause>(attrs)?.predicates;

			// Ensure the where clause contains the Config trait bound
			if instance {
				where_clause_predicates.push(syn::parse_str::<WherePredicate>("T: Config<I>")?);
				where_clause_predicates.push(syn::parse_str::<WherePredicate>("I:'static")?);
			} else {
				where_clause_predicates.push(syn::parse_str::<WherePredicate>("T: Config")?);
			}

			where_clause_predicates.to_token_stream()
		},
	};
	let mod_vis = module.vis;
	let mod_name = module.ident;

	// consume #[benchmarks] attribute by excluding it from mod_attrs
	let mod_attrs: Vec<&Attribute> = module
		.attrs
		.iter()
		.filter(|attr| !attr.path().is_ident(keywords::BENCHMARKS_TOKEN))
		.collect();

	let mut benchmark_names: Vec<Ident> = Vec::new();
	let mut extra_benchmark_names: Vec<Ident> = Vec::new();
	let mut skip_meta_benchmark_names: Vec<Ident> = Vec::new();
	// Map benchmarks to PoV modes.
	let mut pov_modes = Vec::new();

	let (_brace, mut content) =
		module.content.ok_or(syn::Error::new(mod_span, "Module cannot be empty!"))?;

	// find all function defs marked with #[benchmark]
	let benchmark_fn_metas = content.iter_mut().filter_map(|stmt| {
		// parse as a function def first
		let Item::Fn(func) = stmt else { return None };

		// find #[benchmark] attribute on function def
		let benchmark_attr =
			func.attrs.iter().find(|attr| attr.path().is_ident(keywords::BENCHMARK_TOKEN))?;

		Some((benchmark_attr.clone(), func.clone(), stmt))
	});

	// parse individual benchmark defs and args
	for (benchmark_attr, func, stmt) in benchmark_fn_metas {
		// parse benchmark def
		let benchmark_def = BenchmarkDef::from(&func)?;

		// record benchmark name
		let name = &func.sig.ident;
		benchmark_names.push(name.clone());

		// Check if we need to parse any args
		if benchmark_attr.meta.require_path_only().is_err() {
			// parse any args provided to #[benchmark]
			let benchmark_attrs: BenchmarkAttrs = benchmark_attr.parse_args()?;

			// record name sets
			if benchmark_attrs.extra {
				extra_benchmark_names.push(name.clone());
			} else if benchmark_attrs.skip_meta {
				skip_meta_benchmark_names.push(name.clone());
			}

			if let Some(mode) = benchmark_attrs.pov_mode {
				let mut modes = Vec::new();
				// We cannot expand strings here since it is no-std, but syn does not expand bytes.
				let name = name.to_string();
				let m = mode.root.to_string();
				modes.push(quote!(("ALL".as_bytes().to_vec(), #m.as_bytes().to_vec())));

				for attr in mode.per_key.iter() {
					// syn always puts spaces in quoted paths:
					let key = attr.key.clone().into_token_stream().to_string().replace(" ", "");
					let mode = attr.mode.to_string();
					modes.push(quote!((#key.as_bytes().to_vec(), #mode.as_bytes().to_vec())));
				}

				pov_modes.push(
					quote!((#name.as_bytes().to_vec(), #krate::__private::vec![#(#modes),*])),
				);
			}
		}

		// expand benchmark
		let expanded = expand_benchmark(benchmark_def, name, instance, where_clause.clone());

		// replace original function def with expanded code
		*stmt = Item::Verbatim(expanded);
	}

	// generics
	let type_use_generics = match instance {
		false => quote!(T),
		true => quote!(T, I),
	};

	let frame_system = generate_access_from_frame_or_crate("frame-system")?;

	// benchmark name variables
	let benchmark_names_str: Vec<String> = benchmark_names.iter().map(|n| n.to_string()).collect();
	let extra_benchmark_names_str: Vec<String> =
		extra_benchmark_names.iter().map(|n| n.to_string()).collect();
	let skip_meta_benchmark_names_str: Vec<String> =
		skip_meta_benchmark_names.iter().map(|n| n.to_string()).collect();
	let mut selected_benchmark_mappings: Vec<TokenStream2> = Vec::new();
	let mut benchmarks_by_name_mappings: Vec<TokenStream2> = Vec::new();
	let test_idents: Vec<Ident> = benchmark_names_str
		.iter()
		.map(|n| Ident::new(format!("test_benchmark_{}", n).as_str(), Span::call_site()))
		.collect();
	for i in 0..benchmark_names.len() {
		let name_ident = &benchmark_names[i];
		let name_str = &benchmark_names_str[i];
		let test_ident = &test_idents[i];
		selected_benchmark_mappings.push(quote!(#name_str => SelectedBenchmark::#name_ident));
		benchmarks_by_name_mappings.push(quote!(#name_str => Self::#test_ident()))
	}

	let impl_test_function = content
		.iter_mut()
		.find_map(|item| {
			let Item::Macro(item_macro) = item else {
				return None;
			};

			if !item_macro
				.mac
				.path
				.segments
				.iter()
				.any(|s| s.ident == "impl_benchmark_test_suite")
			{
				return None;
			}

			let tokens = item_macro.mac.tokens.clone();
			*item = Item::Verbatim(quote! {});

			Some(quote! {
				impl_test_function!(
					(#( {} #benchmark_names )*)
					(#( #extra_benchmark_names )*)
					(#( #skip_meta_benchmark_names )*)
					#tokens
				);
			})
		})
		.unwrap_or(quote! {});

	// emit final quoted tokens
	let res = quote! {
		#(#mod_attrs)
		*
		#mod_vis mod #mod_name {
			#(#content)
			*

			#[allow(non_camel_case_types)]
			enum SelectedBenchmark {
				#(#benchmark_names),
				*
			}

			impl<#type_use_generics> #krate::BenchmarkingSetup<#type_use_generics> for SelectedBenchmark where #where_clause {
				fn components(&self) -> #krate::__private::Vec<(#krate::BenchmarkParameter, u32, u32)> {
					match self {
						#(
							Self::#benchmark_names => {
								<#benchmark_names as #krate::BenchmarkingSetup<#type_use_generics>>::components(&#benchmark_names)
							}
						)
						*
					}
				}

				fn instance(
					&self,
					recording: &mut impl #krate::Recording,
					components: &[(#krate::BenchmarkParameter, u32)],
					verify: bool,
				) -> Result<(), #krate::BenchmarkError> {
					match self {
						#(
							Self::#benchmark_names => {
								<#benchmark_names as #krate::BenchmarkingSetup<
									#type_use_generics
								>>::instance(&#benchmark_names, recording, components, verify)
							}
						)
						*
					}
				}
			}
			#[cfg(any(feature = "runtime-benchmarks", test))]
			impl<#type_use_generics> #krate::Benchmarking for Pallet<#type_use_generics>
			where T: #frame_system::Config,#where_clause
			{
				fn benchmarks(
					extra: bool,
				) -> #krate::__private::Vec<#krate::BenchmarkMetadata> {
					let mut all_names = #krate::__private::vec![
						#(#benchmark_names_str),
						*
					];
					if !extra {
						let extra = [
							#(#extra_benchmark_names_str),
							*
						];
						all_names.retain(|x| !extra.contains(x));
					}
					let pov_modes:
						#krate::__private::Vec<(
							#krate::__private::Vec<u8>,
							#krate::__private::Vec<(
								#krate::__private::Vec<u8>,
								#krate::__private::Vec<u8>
							)>,
						)> = #krate::__private::vec![
						#( #pov_modes ),*
					];
					all_names.into_iter().map(|benchmark| {
						let selected_benchmark = match benchmark {
							#(#selected_benchmark_mappings),
							*,
							_ => panic!("all benchmarks should be selectable")
						};
						let components = <SelectedBenchmark as #krate::BenchmarkingSetup<#type_use_generics>>::components(&selected_benchmark);
						let name = benchmark.as_bytes().to_vec();
						let modes = pov_modes.iter().find(|p| p.0 == name).map(|p| p.1.clone());

						#krate::BenchmarkMetadata {
							name: benchmark.as_bytes().to_vec(),
							components,
							pov_modes: modes.unwrap_or_default(),
						}
					}).collect::<#krate::__private::Vec<_>>()
				}

				fn run_benchmark(
					extrinsic: &[u8],
					c: &[(#krate::BenchmarkParameter, u32)],
					whitelist: &[#krate::__private::TrackedStorageKey],
					verify: bool,
					internal_repeats: u32,
				) -> Result<#krate::__private::Vec<#krate::BenchmarkResult>, #krate::BenchmarkError> {
					#krate::benchmarking::wipe_db();
					let extrinsic = #krate::__private::str::from_utf8(extrinsic).map_err(|_| "`extrinsic` is not a valid utf-8 string!")?;
					let selected_benchmark = match extrinsic {
						#(#selected_benchmark_mappings),
						*,
						_ => return Err("Could not find extrinsic.".into()),
					};
					let mut whitelist = whitelist.to_vec();
					let whitelisted_caller_key = <#frame_system::Account<
						T,
					> as #krate::__private::storage::StorageMap<_, _,>>::hashed_key_for(
						#krate::whitelisted_caller::<T::AccountId>()
					);
					whitelist.push(whitelisted_caller_key.into());
					let transactional_layer_key = #krate::__private::TrackedStorageKey::new(
						#krate::__private::storage::transactional::TRANSACTION_LEVEL_KEY.into(),
					);
					whitelist.push(transactional_layer_key);
					// Whitelist the `:extrinsic_index`.
					let extrinsic_index = #krate::__private::TrackedStorageKey::new(
						#krate::__private::well_known_keys::EXTRINSIC_INDEX.into()
					);
					whitelist.push(extrinsic_index);
					// Whitelist the `:intrablock_entropy`.
					let intrablock_entropy = #krate::__private::TrackedStorageKey::new(
						#krate::__private::well_known_keys::INTRABLOCK_ENTROPY.into()
					);
					whitelist.push(intrablock_entropy);

					#krate::benchmarking::set_whitelist(whitelist.clone());
					let mut results: #krate::__private::Vec<#krate::BenchmarkResult> = #krate::__private::Vec::new();

					let on_before_start = || {
						// Set the block number to at least 1 so events are deposited.
						if #krate::__private::Zero::is_zero(&#frame_system::Pallet::<T>::block_number()) {
							#frame_system::Pallet::<T>::set_block_number(1u32.into());
						}

						// Commit the externalities to the database, flushing the DB cache.
						// This will enable worst case scenario for reading from the database.
						#krate::benchmarking::commit_db();

						// Access all whitelisted keys to get them into the proof recorder since the
						// recorder does not have a whitelist.
						// NOTE: We read from the global whitelist because the benchmark setup code
						// may have added additional keys via add_to_whitelist() or add_to_whitelist_child().
						let current_whitelist = #krate::benchmarking::get_whitelist();
						for key in &current_whitelist {
							if let Some(child_trie_key) = &key.child_trie_key {
								let child_info = #krate::__private::storage::ChildInfo::new_default(child_trie_key);
								#krate::__private::storage::child::get_raw(&child_info, &key.key);
							} else {
								#krate::__private::storage::unhashed::get_raw(&key.key);
							}
						}

						// Reset the read/write counter so we don't count operations in the setup process.
						#krate::benchmarking::reset_read_write_count();
					};

					// Always do at least one internal repeat...
					for _ in 0 .. internal_repeats.max(1) {
						// Always reset the state after the benchmark.
						#krate::__private::defer!(#krate::benchmarking::wipe_db());

						// Time the extrinsic logic.
						#krate::__private::log::trace!(
							target: "benchmark",
							"Start Benchmark: {} ({:?})",
							extrinsic,
							c
						);

						let mut recording = #krate::BenchmarkRecording::new(&on_before_start);
						<SelectedBenchmark as #krate::BenchmarkingSetup<#type_use_generics>>::instance(&selected_benchmark, &mut recording, c, verify)?;

						// Calculate the diff caused by the benchmark.
						let elapsed_extrinsic = recording.elapsed_extrinsic().expect("elapsed time should be recorded");
						let diff_pov = recording.diff_pov().unwrap_or_default();

						// Commit the changes to get proper write count
						#krate::benchmarking::commit_db();
						#krate::__private::log::trace!(
							target: "benchmark",
							"End Benchmark: {} ns", elapsed_extrinsic
						);
						let read_write_count = #krate::benchmarking::read_write_count();
						#krate::__private::log::trace!(
							target: "benchmark",
							"Read/Write Count {:?}", read_write_count
						);

						// Time the storage root recalculation.
						let start_storage_root = #krate::current_time();
						#krate::__private::storage_root(#krate::__private::StateVersion::V1);
						let finish_storage_root = #krate::current_time();
						let elapsed_storage_root = finish_storage_root - start_storage_root;

						let skip_meta = [ #(#skip_meta_benchmark_names_str),* ];
						let read_and_written_keys = if skip_meta.contains(&extrinsic) {
							#krate::__private::vec![(b"Skipped Metadata".to_vec(), 0, 0, false)]
						} else {
							#krate::benchmarking::get_read_and_written_keys()
						};

						results.push(#krate::BenchmarkResult {
							components: c.to_vec(),
							extrinsic_time: elapsed_extrinsic,
							storage_root_time: elapsed_storage_root,
							reads: read_write_count.0,
							repeat_reads: read_write_count.1,
							writes: read_write_count.2,
							repeat_writes: read_write_count.3,
							proof_size: diff_pov,
							keys: read_and_written_keys,
						});
					}

					return Ok(results);
				}
			}

			#[cfg(test)]
			impl<#type_use_generics> Pallet<#type_use_generics> where T: #frame_system::Config, #where_clause {
				/// Test a particular benchmark by name.
				///
				/// This isn't called `test_benchmark_by_name` just in case some end-user eventually
				/// writes a benchmark, itself called `by_name`; the function would be shadowed in
				/// that case.
				///
				/// This is generally intended to be used by child test modules such as those created
				/// by the `impl_benchmark_test_suite` macro. However, it is not an error if a pallet
				/// author chooses not to implement benchmarks.
				#[allow(unused)]
				fn test_bench_by_name(name: &[u8]) -> Result<(), #krate::BenchmarkError> {
					let name = #krate::__private::str::from_utf8(name)
						.map_err(|_| -> #krate::BenchmarkError { "`name` is not a valid utf8 string!".into() })?;
					match name {
						#(#benchmarks_by_name_mappings),
						*,
						_ => Err("Could not find test for requested benchmark.".into()),
					}
				}
			}

			#impl_test_function
		}
		#mod_vis use #mod_name::*;
	};
	Ok(res.into())
}

/// Prepares a [`Vec<ParamDef>`] to be interpolated by [`quote!`] by creating easily-iterable
/// arrays formatted in such a way that they can be interpolated directly.
struct UnrolledParams {
	param_ranges: Vec<TokenStream2>,
	param_names: Vec<TokenStream2>,
}

impl UnrolledParams {
	/// Constructs an [`UnrolledParams`] from a [`Vec<ParamDef>`]
	fn from(params: &Vec<ParamDef>) -> UnrolledParams {
		let param_ranges: Vec<TokenStream2> = params
			.iter()
			.map(|p| {
				let name = Ident::new(&p.name, Span::call_site());
				let start = &p.start;
				let end = &p.end;
				quote!(#name, #start, #end)
			})
			.collect();
		let param_names: Vec<TokenStream2> = params
			.iter()
			.map(|p| {
				let name = Ident::new(&p.name, Span::call_site());
				quote!(#name)
			})
			.collect();
		UnrolledParams { param_ranges, param_names }
	}
}

/// Performs expansion of an already-parsed [`BenchmarkDef`].
fn expand_benchmark(
	benchmark_def: BenchmarkDef,
	name: &Ident,
	is_instance: bool,
	where_clause: TokenStream2,
) -> TokenStream2 {
	// set up variables needed during quoting
	let krate = match generate_access_from_frame_or_crate("frame-benchmarking") {
		Ok(ident) => ident,
		Err(err) => return err.to_compile_error().into(),
	};
	let frame_system = match generate_access_from_frame_or_crate("frame-system") {
		Ok(path) => path,
		Err(err) => return err.to_compile_error().into(),
	};
	let codec = quote!(#krate::__private::codec);
	let traits = quote!(#krate::__private::traits);
	let setup_stmts = benchmark_def.setup_stmts;
	let verify_stmts = benchmark_def.verify_stmts;
	let last_stmt = benchmark_def.last_stmt;
	let test_ident =
		Ident::new(format!("test_benchmark_{}", name.to_string()).as_str(), Span::call_site());

	// unroll params (prepare for quoting)
	let unrolled = UnrolledParams::from(&benchmark_def.params);
	let param_names = unrolled.param_names;
	let param_ranges = unrolled.param_ranges;

	let type_use_generics = match is_instance {
		false => quote!(T),
		true => quote!(T, I),
	};

	// used in the benchmarking impls
	let (pre_call, post_call, fn_call_body) = match &benchmark_def.call_def {
		BenchmarkCallDef::ExtrinsicCall { origin, expr_call, attr_span: _ } => {
			let mut expr_call = expr_call.clone();

			// remove first arg from expr_call
			let mut final_args = Punctuated::<Expr, Comma>::new();
			let args: Vec<&Expr> = expr_call.args.iter().collect();
			for arg in &args[1..] {
				final_args.push((*(*arg)).clone());
			}
			expr_call.args = final_args;

			let origin = match origin {
				Expr::Cast(t) => {
					let ty = t.ty.clone();
					quote_spanned! { origin.span() =>
						<<T as #frame_system::Config>::RuntimeOrigin as From<#ty>>::from(#origin);
					}
				},
				_ => quote_spanned! { origin.span() =>
					Into::<<T as #frame_system::Config>::RuntimeOrigin>::into(#origin);
				},
			};

			// determine call name (handles `_` and normal call syntax)
			let expr_span = expr_call.span();
			let call_err = || {
				syn::Error::new(expr_span, "Extrinsic call must be a function call or `_`")
					.to_compile_error()
			};
			let call_name = match *expr_call.func {
				Expr::Path(expr_path) => {
					// normal function call
					let Some(segment) = expr_path.path.segments.last() else { return call_err() };
					segment.ident.to_string()
				},
				Expr::Infer(_) => {
					// `_` style
					// replace `_` with fn name
					name.to_string()
				},
				_ => return call_err(),
			};

			// modify extrinsic call to be prefixed with "new_call_variant"
			let call_name = format!("new_call_variant_{}", call_name);
			let mut punct: Punctuated<PathSegment, PathSep> = Punctuated::new();
			punct.push(PathSegment {
				arguments: PathArguments::None,
				ident: Ident::new(call_name.as_str(), Span::call_site()),
			});
			*expr_call.func = Expr::Path(ExprPath {
				attrs: vec![],
				qself: None,
				path: Path { leading_colon: None, segments: punct },
			});
			let pre_call = quote! {
				let __call = Call::<#type_use_generics>::#expr_call;
				let __benchmarked_call_encoded = #codec::Encode::encode(&__call);
			};
			let post_call = quote! {
				let __call_decoded = <Call<#type_use_generics> as #codec::Decode>
					::decode(&mut &__benchmarked_call_encoded[..])
					.expect("call is encoded above, encoding must be correct");
				#[allow(clippy::useless_conversion)]
				let __origin = #origin;
				<Call<#type_use_generics> as #traits::UnfilteredDispatchable>::dispatch_bypass_filter(
					__call_decoded,
					__origin,
				)
			};
			(
				// (pre_call, post_call, fn_call_body):
				pre_call.clone(),
				quote!(#post_call?;),
				quote! {
					#pre_call
					#post_call.unwrap();
				},
			)
		},
		BenchmarkCallDef::Block { block, attr_span: _ } => {
			(quote!(), quote!(#block), quote!(#block))
		},
	};

	let vis = benchmark_def.fn_vis;

	// remove #[benchmark] attribute
	let fn_attrs = benchmark_def
		.fn_attrs
		.iter()
		.filter(|attr| !attr.path().is_ident(keywords::BENCHMARK_TOKEN));

	// modify signature generics, ident, and inputs, e.g:
	// before: `fn bench(u: Linear<1, 100>) -> Result<(), BenchmarkError>`
	// after: `fn _bench <T, I>(u: u32, verify: bool) where T: Config<I>, I: 'static -> Result<(),
	// BenchmarkError>`
	let mut sig = benchmark_def.fn_sig;
	sig.generics = parse_quote!(<#type_use_generics>);
	sig.generics.where_clause = parse_quote!(where #where_clause);
	sig.ident =
		Ident::new(format!("_{}", name.to_token_stream().to_string()).as_str(), Span::call_site());
	let mut fn_param_inputs: Vec<TokenStream2> =
		param_names.iter().map(|name| quote!(#name: u32)).collect();
	fn_param_inputs.push(quote!(verify: bool));
	sig.inputs = parse_quote!(#(#fn_param_inputs),*);

	// used in instance() impl
	let impl_last_stmt = match &last_stmt {
		Some(stmt) => quote!(#stmt),
		None => quote!(Ok(())),
	};
	let fn_attrs_clone = fn_attrs.clone();

	let fn_def = quote! {
		#(
			#fn_attrs_clone
		)*
		#vis #sig {
			#(
				#setup_stmts
			)*
			#fn_call_body
			if verify {
				#(
					#verify_stmts
				)*
			}
			#last_stmt
		}
	};

	// generate final quoted tokens
	let res = quote! {
		// benchmark function definition
		#fn_def

		#[allow(non_camel_case_types)]
		#(
			#fn_attrs
		)*
		struct #name;

		#[allow(unused_variables)]
		impl<#type_use_generics> #krate::BenchmarkingSetup<#type_use_generics>
		for #name where #where_clause {
			fn components(&self) -> #krate::__private::Vec<(#krate::BenchmarkParameter, u32, u32)> {
				#krate::__private::vec! [
					#(
						(#krate::BenchmarkParameter::#param_ranges)
					),*
				]
			}

			fn instance(
				&self,
				recording: &mut impl #krate::Recording,
				components: &[(#krate::BenchmarkParameter, u32)],
				verify: bool
			) -> Result<(), #krate::BenchmarkError> {
				#(
					// prepare instance #param_names
					let #param_names = components.iter()
						.find(|&c| c.0 == #krate::BenchmarkParameter::#param_names)
						.ok_or("Could not find component during benchmark preparation.")?
						.1;
				)*

				// benchmark setup code
				#(
					#setup_stmts
				)*
				#pre_call
				recording.start();
				#post_call
				recording.stop();
				if verify {
					#(
						#verify_stmts
					)*
				}
				#impl_last_stmt
			}
		}

		#[cfg(test)]
		impl<#type_use_generics> Pallet<#type_use_generics> where T: #frame_system::Config, #where_clause {
			#[allow(unused)]
			fn #test_ident() -> Result<(), #krate::BenchmarkError> {
				let selected_benchmark = SelectedBenchmark::#name;
				let components = <
					SelectedBenchmark as #krate::BenchmarkingSetup<T, _>
				>::components(&selected_benchmark);
				let execute_benchmark = |
					c: #krate::__private::Vec<(#krate::BenchmarkParameter, u32)>
				| -> Result<(), #krate::BenchmarkError> {
					// Always reset the state after the benchmark.
					#krate::__private::defer!(#krate::benchmarking::wipe_db());

					let on_before_start = || {
						// Set the block number to at least 1 so events are deposited.
						if #krate::__private::Zero::is_zero(&#frame_system::Pallet::<T>::block_number()) {
							#frame_system::Pallet::<T>::set_block_number(1u32.into());
						}
					};

					// Run execution + verification
					<SelectedBenchmark as #krate::BenchmarkingSetup<T, _>>::test_instance(&selected_benchmark,  &c, &on_before_start)
				};

				if components.is_empty() {
					execute_benchmark(Default::default())?;
				} else {
					let num_values: u32 = if let Ok(ev) = std::env::var("VALUES_PER_COMPONENT") {
						ev.parse().map_err(|_| {
							#krate::BenchmarkError::Stop(
								"Could not parse env var `VALUES_PER_COMPONENT` as u32."
							)
						})?
					} else {
						6
					};

					if num_values < 2 {
						return Err("`VALUES_PER_COMPONENT` must be at least 2".into());
					}

					for (name, low, high) in components.clone().into_iter() {
						// Test the lowest, highest (if its different from the lowest)
						// and up to num_values-2 more equidistant values in between.
						// For 0..10 and num_values=6 this would mean: [0, 2, 4, 6, 8, 10]
						if high < low {
							return Err("The start of a `ParamRange` must be less than or equal to the end".into());
						}

						let mut values = #krate::__private::vec![low];
						let diff = (high - low).min(num_values - 1);
						let slope = (high - low) as f32 / diff as f32;

						for i in 1..=diff {
							let value = ((low as f32 + slope * i as f32) as u32)
											.clamp(low, high);
							values.push(value);
						}

						for component_value in values {
							// Select the max value for all the other components.
							let c: #krate::__private::Vec<(#krate::BenchmarkParameter, u32)> = components
								.iter()
								.map(|(n, _, h)|
									if *n == name {
										(*n, component_value)
									} else {
										(*n, *h)
									}
								)
								.collect();

							execute_benchmark(c)?;
						}
					}
				}
				return Ok(());
			}
		}
	};
	res
}
