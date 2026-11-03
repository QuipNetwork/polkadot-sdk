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

//! Procedural macros used in `pallet-revive`.
//!
//! Most likely you should use the [`#[define_env]`][`macro@define_env`] attribute macro which hides
//! boilerplate of defining external environment for a polkavm module.

use proc_macro::TokenStream;
use proc_macro2::{Literal, Span, TokenStream as TokenStream2};
use quote::{quote, ToTokens};
use syn::{parse_quote, punctuated::Punctuated, spanned::Spanned, token::Comma, FnArg, Ident};

/// Defines a host functions set that can be imported by contract polkavm code.
///
/// **CAUTION**: Be advised that all functions defined by this macro
/// cause undefined behaviour inside the contract if the signature does not match.
///
/// WARNING: It is CRITICAL for contracts to make sure that the signatures match exactly.
/// Failure to do so may result in undefined behavior, traps or security vulnerabilities inside the
/// contract. The runtime itself is unharmed due to sandboxing.
/// For example, if a function is called with an incorrect signature, it could lead to memory
/// corruption or unexpected results within the contract.
#[proc_macro_attribute]
pub fn define_env(attr: TokenStream, item: TokenStream) -> TokenStream {
	if !attr.is_empty() {
		let msg = r#"Invalid `define_env` attribute macro: expected no attributes:
					 - `#[define_env]`"#;
		let span = TokenStream2::from(attr).span();
		return syn::Error::new(span, msg).to_compile_error().into();
	}

	let item = syn::parse_macro_input!(item as syn::ItemMod);

	match EnvDef::try_from(item) {
		Ok(mut def) => expand_env(&mut def).into(),
		Err(e) => e.to_compile_error().into(),
	}
}

/// Parsed environment definition.
struct EnvDef {
	host_funcs: Vec<HostFn>,
}

/// Parsed host function definition.
struct HostFn {
	item: syn::ItemFn,
	name: String,
	returns: HostFnReturn,
	cfg: Option<syn::Attribute>,
}

enum HostFnReturn {
	Unit,
	U32,
	U64,
	ReturnCode,
}

impl HostFnReturn {
	fn map_output(&self) -> TokenStream2 {
		match self {
			Self::Unit => quote! { |_| None },
			_ => quote! { |ret_val| Some(ret_val.into()) },
		}
	}

	fn success_type(&self) -> syn::ReturnType {
		match self {
			Self::Unit => syn::ReturnType::Default,
			Self::U32 => parse_quote! { -> u32 },
			Self::U64 => parse_quote! { -> u64 },
			Self::ReturnCode => parse_quote! { -> ReturnErrorCode },
		}
	}

	fn trace_return_value(&self) -> TokenStream2 {
		match self {
			Self::Unit => quote! { None },
			Self::U32 => quote! { result.as_ref().ok().map(|r| *r as u64) },
			Self::ReturnCode => quote! { result.as_ref().ok().copied().map(u64::from) },
			Self::U64 => quote! { result.as_ref().ok().copied() },
		}
	}
}

impl EnvDef {
	pub fn try_from(item: syn::ItemMod) -> syn::Result<Self> {
		let span = item.span();
		let err = |msg| syn::Error::new(span, msg);
		let items = &item
			.content
			.as_ref()
			.ok_or(err("Invalid environment definition, expected `mod` to be inlined."))?
			.1;

		let extract_fn = |i: &syn::Item| match i {
			syn::Item::Fn(i_fn) => Some(i_fn.clone()),
			_ => None,
		};

		let host_funcs = items
			.iter()
			.filter_map(extract_fn)
			.map(HostFn::try_from)
			.collect::<Result<Vec<_>, _>>()?;

		Ok(Self { host_funcs })
	}
}

impl HostFn {
	pub fn try_from(mut item: syn::ItemFn) -> syn::Result<Self> {
		let err = |span, msg| {
			let msg = format!("Invalid host function definition.\n{}", msg);
			syn::Error::new(span, msg)
		};

		// process attributes
		let msg = "Only #[cfg] and #[mutating] attributes are allowed.";
		let span = item.span();
		let mut attrs = item.attrs.clone();
		attrs.retain(|a| !a.path().is_ident("doc"));
		let mut mutating = false;
		let mut cfg = None;
		while let Some(attr) = attrs.pop() {
			let ident = attr.path().get_ident().ok_or(err(span, msg))?.to_string();
			match ident.as_str() {
				"mutating" => {
					if mutating {
						return Err(err(span, "#[mutating] can only be specified once"));
					}
					mutating = true;
				},
				"cfg" => {
					if cfg.is_some() {
						return Err(err(span, "#[cfg] can only be specified once"));
					}
					cfg = Some(attr);
				},
				id => return Err(err(span, &format!("Unsupported attribute \"{id}\". {msg}"))),
			}
		}

		if mutating {
			let stmt = syn::parse_quote! {
				if self.ext().is_read_only() {
					return Err(Error::<E::T>::StateChangeDenied.into());
				}
			};
			item.block.stmts.insert(0, stmt);
		}

		let name = item.sig.ident.to_string();

		let msg = "Every function must start with these two parameters: &mut self, memory: &mut M";
		let special_args = item
			.sig
			.inputs
			.iter()
			.take(2)
			.enumerate()
			.map(|(i, arg)| is_valid_special_arg(i, arg))
			.fold(0u32, |acc, valid| if valid { acc + 1 } else { acc });

		if special_args != 2 {
			return Err(err(span, msg));
		}

		// process return type
		let msg = r#"Should return one of the following:
				- Result<(), TrapReason>,
				- Result<ReturnErrorCode, TrapReason>,
				- Result<u32, TrapReason>,
				- Result<u64, TrapReason>"#;
		let ret_ty = match item.clone().sig.output {
			syn::ReturnType::Type(_, ty) => Ok(ty.clone()),
			_ => Err(err(span, &msg)),
		}?;
		match *ret_ty {
			syn::Type::Path(tp) => {
				let result = &tp.path.segments.last().ok_or(err(span, &msg))?;
				let (id, span) = (result.ident.to_string(), result.ident.span());
				id.eq(&"Result".to_string()).then_some(()).ok_or(err(span, &msg))?;

				match &result.arguments {
					syn::PathArguments::AngleBracketed(group) => {
						if group.args.len() != 2 {
							return Err(err(span, &msg));
						};

						let arg2 = group.args.last().ok_or(err(span, &msg))?;

						let err_ty = match arg2 {
							syn::GenericArgument::Type(ty) => Ok(ty.clone()),
							_ => Err(err(arg2.span(), &msg)),
						}?;

						match err_ty {
							syn::Type::Path(tp) => Ok(tp
								.path
								.segments
								.first()
								.ok_or(err(arg2.span(), &msg))?
								.ident
								.to_string()),
							_ => Err(err(tp.span(), &msg)),
						}?
						.eq("TrapReason")
						.then_some(())
						.ok_or(err(span, &msg))?;

						let arg1 = group.args.first().ok_or(err(span, &msg))?;
						let ok_ty = match arg1 {
							syn::GenericArgument::Type(ty) => Ok(ty.clone()),
							_ => Err(err(arg1.span(), &msg)),
						}?;
						let ok_ty_str = match ok_ty {
							syn::Type::Path(tp) => Ok(tp
								.path
								.segments
								.first()
								.ok_or(err(arg1.span(), &msg))?
								.ident
								.to_string()),
							syn::Type::Tuple(tt) => {
								if !tt.elems.is_empty() {
									return Err(err(arg1.span(), &msg));
								};
								Ok("()".to_string())
							},
							_ => Err(err(ok_ty.span(), &msg)),
						}?;
						let returns = match ok_ty_str.as_str() {
							"()" => Ok(HostFnReturn::Unit),
							"u32" => Ok(HostFnReturn::U32),
							"u64" => Ok(HostFnReturn::U64),
							"ReturnErrorCode" => Ok(HostFnReturn::ReturnCode),
							_ => Err(err(arg1.span(), &msg)),
						}?;

						Ok(Self { item, name, returns, cfg })
					},
					_ => Err(err(span, &msg)),
				}
			},
			_ => Err(err(span, &msg)),
		}
	}
}

fn is_valid_special_arg(idx: usize, arg: &FnArg) -> bool {
	match (idx, arg) {
		(0, FnArg::Receiver(rec)) => rec.reference.is_some() && rec.mutability.is_some(),
		(1, FnArg::Typed(pat)) => {
			let ident = if let syn::Pat::Ident(ref ident) = *pat.pat {
				&ident.ident
			} else {
				return false;
			};
			if !(ident == "memory" || ident == "_memory") {
				return false;
			}
			matches!(*pat.ty, syn::Type::Reference(_))
		},
		_ => false,
	}
}

fn arg_decoder<'a, P, I>(param_names: P, param_types: I) -> TokenStream2
where
	P: Iterator<Item = &'a std::boxed::Box<syn::Pat>> + Clone,
	I: Iterator<Item = &'a std::boxed::Box<syn::Type>> + Clone,
{
	const ALLOWED_REGISTERS: usize = 6;

	// too many arguments
	if param_names.clone().count() > ALLOWED_REGISTERS {
		panic!("Syscalls take a maximum of {ALLOWED_REGISTERS} arguments");
	}

	// all of them take one register but we truncate them before passing into the function
	// it is important to not allow any type which has illegal bit patterns like 'bool'
	if !param_types.clone().all(|ty| {
		let syn::Type::Path(path) = &**ty else {
			panic!("Type needs to be path");
		};
		let Some(ident) = path.path.get_ident() else {
			panic!("Type needs to be ident");
		};
		matches!(ident.to_string().as_ref(), "u8" | "u16" | "u32" | "u64")
	}) {
		panic!("Only primitive unsigned integers are allowed as arguments to syscalls");
	}

	// one argument per register
	let bindings = param_names.zip(param_types).enumerate().map(|(idx, (name, ty))| {
		let reg = quote::format_ident!("__a{}__", idx);
		quote! {
			let #name = #reg as #ty;
		}
	});
	quote! {
		#( #bindings )*
	}
}

/// Expands environment definition.
/// Should generate source code for:
///  - implementations of the host functions to be added to the polkavm runtime environment (see
///    `expand_impls()`).
fn expand_env(def: &EnvDef) -> TokenStream2 {
	let impls = expand_functions(def);
	let bench_impls = expand_bench_functions(def);
	let docs = expand_func_doc(def);
	let all_syscalls = expand_func_list(def);
	let lookup_syscall = expand_func_lookup(def);
	let all_trace_ops = expand_trace_op_list(def);
	let lookup_trace_op = expand_trace_op_lookup(def);

	quote! {
		/// Returns the list of all syscalls that contracts can import.
		pub fn list_syscalls() -> &'static [&'static [u8]] {
			#all_syscalls
		}

		/// Return the index of a syscall in the `list_syscalls()` list.
		pub fn lookup_syscall_index(name: &'static str) -> Option<u8> {
			#lookup_syscall
		}

		/// Returns the list of all trace operations (real syscalls + synthetic trace steps).
		pub fn list_trace_ops() -> &'static [&'static [u8]] {
			#all_trace_ops
		}

		/// Return the index of a trace operation in the `list_trace_ops()` list.
		pub fn lookup_trace_op_index(name: &'static str) -> Option<u8> {
			#lookup_trace_op
		}

		impl<'a, E: Ext, M: PolkaVmInstance<E::T>> Runtime<'a, E, M> {
			fn handle_ecall(
				&mut self,
				memory: &mut M,
				__syscall_symbol__: &[u8],
			) -> Result<Option<u64>, TrapReason>
			{
				#impls
			}
		}

		#[cfg(feature = "runtime-benchmarks")]
		impl<'a, E: Ext, M: ?Sized + Memory<E::T>> Runtime<'a, E, M> {
			#bench_impls
		}

		/// Documentation of the syscalls (host functions) available to contracts.
		///
		/// Each of the functions in this trait represent a function that is callable
		/// by the contract. Guests use the function name as the import symbol.
		///
		/// # Note
		///
		/// This module is not meant to be used by any code. Rather, it is meant to be
		/// consumed by humans through rustdoc.
		#[cfg(doc)]
		pub trait SyscallDoc {
			#docs
		}
	}
}

fn expand_functions(def: &EnvDef) -> TokenStream2 {
	let impls = def.host_funcs.iter().map(|f| {
		// skip the self and memory argument
		let params = f.item.sig.inputs.iter().skip(2);
		let param_names = params.clone().filter_map(|arg| {
			let FnArg::Typed(arg) = arg else {
				return None;
			};
			Some(&arg.pat)
		});
		let param_types = params.clone().filter_map(|arg| {
			let FnArg::Typed(arg) = arg else {
				return None;
			};
			Some(&arg.ty)
		});
		let arg_decoder = arg_decoder(param_names, param_types);
		let cfg = &f.cfg;
		let name = &f.name;
		let syscall_symbol = Literal::byte_string(name.as_bytes());
		let body = &f.item.block;
		let map_output = f.returns.map_output();
		let trace_return = f.returns.trace_return_value();
		let output = &f.item.sig.output;

		// wrapped host function body call with host function traces
		let wrapped_body_with_trace = {
			let trace_fmt_args = params.clone().filter_map(|arg| match arg {
				syn::FnArg::Receiver(_) => None,
				syn::FnArg::Typed(p) => match *p.pat.clone() {
					syn::Pat::Ident(ref pat_ident) => Some(pat_ident.ident.clone()),
					_ => None,
				},
			});

			let params_fmt_str = trace_fmt_args
				.clone()
				.map(|s| format!("{s}: {{:?}}"))
				.collect::<Vec<_>>()
				.join(", ");
			let trace_fmt_str = format!("{}({}) = {{:?}} weight_consumed: {{:?}}", name, params_fmt_str);
			let trace_args_for_tracer: Vec<_> = trace_fmt_args.clone().collect();

			quote! {
				crate::tracing::if_tracing(|tracer| {
					tracer.enter_ecall(#name, &[#( #trace_args_for_tracer as u64 ),*], self)
				});

				// wrap body in closure to make sure the tracing is always executed
				let result = (|| #body)();
				::log::trace!(target: "runtime::revive::strace", #trace_fmt_str, #( #trace_fmt_args, )* result, self.ext.frame_meter().weight_consumed());

				crate::tracing::if_tracing(|tracer| tracer.exit_step(self, #trace_return));
				result
			}
		};

		quote! {
			#cfg
			#syscall_symbol => {
				// closure is needed so that "?" can infere the correct type
				(|| #output {
					#arg_decoder
					#wrapped_body_with_trace
				})().map(#map_output)
			},
		}
	});

	quote! {
		crate::tracing::if_tracing(|tracer| {
			tracer.enter_ecall(crate::tracing::PVM_FUEL_NAME, &[], self)
		});

		let __sync_result__ = self.ext
			.frame_meter_mut()
			.sync_from_executor(memory.gas())
			.map_err(TrapReason::from);

		crate::tracing::if_tracing(|tracer| tracer.exit_step(self, None));

		__sync_result__?;

		// This is the overhead to call an empty syscall that always needs to be charged.
		self.charge_gas(crate::vm::RuntimeCosts::HostFn).map_err(TrapReason::from)?;

		// They will be mapped to variable names by the syscall specific code.
		let (__a0__, __a1__, __a2__, __a3__, __a4__, __a5__) = memory.read_input_regs();

		// Execute the syscall specific logic in a closure so that the gas metering code is always executed.
		let result = (|| match __syscall_symbol__ {
			#( #impls )*
			_ => Err(TrapReason::SupervisorError(Error::<E::T>::InvalidSyscall.into()))
		})();

		// Write gas from pallet-revive into polkavm after leaving the host function.
		let gas = self.ext.frame_meter_mut().sync_to_executor();
		memory.set_gas(gas.into());
		result
	}
}

fn expand_bench_functions(def: &EnvDef) -> TokenStream2 {
	let impls = def.host_funcs.iter().map(|f| {
		// skip the context and memory argument
		let params = f.item.sig.inputs.iter().skip(2);
		let cfg = &f.cfg;
		let name = &f.name;
		let body = &f.item.block;
		let output = &f.item.sig.output;

		let name = Ident::new(&format!("bench_{name}"), Span::call_site());
		quote! {
			#cfg
			pub fn #name(&mut self, memory: &mut M, #(#params),*) #output {
				#body
			}
		}
	});

	quote! {
		#( #impls )*
	}
}

fn expand_func_doc(def: &EnvDef) -> TokenStream2 {
	let docs = def.host_funcs.iter().map(|func| {
		// Remove auxiliary args: `ctx: _` and `memory: _`
		let func_decl = {
			let mut sig = func.item.sig.clone();
			sig.inputs = sig
				.inputs
				.iter()
				.skip(2)
				.map(|p| p.clone())
				.collect::<Punctuated<FnArg, Comma>>();
			sig.output = func.returns.success_type();
			sig.to_token_stream()
		};
		let func_doc = {
			let func_docs = {
				let docs = func.item.attrs.iter().filter(|a| a.path().is_ident("doc")).map(|d| {
					let docs = d.to_token_stream();
					quote! { #docs }
				});
				quote! { #( #docs )* }
			};
			quote! {
				#func_docs
			}
		};
		quote! {
			#func_doc
			#func_decl;
		}
	});

	quote! {
		#( #docs )*
	}
}

fn expand_func_list(def: &EnvDef) -> TokenStream2 {
	let docs = def.host_funcs.iter().map(|f| {
		let name = Literal::byte_string(f.name.as_bytes());
		quote! {
			#name.as_slice()
		}
	});
	let len = docs.clone().count();

	quote! {
		{
			static FUNCS: [&[u8]; #len] = [#(#docs),*];
			FUNCS.as_slice()
		}
	}
}

fn expand_func_lookup(def: &EnvDef) -> TokenStream2 {
	let arms = def.host_funcs.iter().enumerate().map(|(idx, f)| {
		let name_str = &f.name;
		quote! {
			#name_str => Some(#idx as u8)
		}
	});
	quote! {
		match name {
			#( #arms, )*
			_ => None,
		}
	}
}

fn expand_trace_op_list(def: &EnvDef) -> TokenStream2 {
	let syscalls = def.host_funcs.iter().map(|f| {
		let name = Literal::byte_string(f.name.as_bytes());
		quote! {
			#name.as_slice()
		}
	});
	let len = syscalls.clone().count() + 1;

	quote! {
		{
			static OPS: [&[u8]; #len] = [
				#(#syscalls,)*
				crate::tracing::PVM_FUEL_NAME.as_bytes(),
			];
			OPS.as_slice()
		}
	}
}

fn expand_trace_op_lookup(def: &EnvDef) -> TokenStream2 {
	let arms = def.host_funcs.iter().enumerate().map(|(idx, f)| {
		let name_str = &f.name;
		quote! {
			#name_str => Some(#idx as u8)
		}
	});
	let pvm_fuel_idx = def.host_funcs.len();

	quote! {
		match name {
			#( #arms, )*
			crate::tracing::PVM_FUEL_NAME => Some(#pvm_fuel_idx as u8),
			_ => None,
		}
	}
}
