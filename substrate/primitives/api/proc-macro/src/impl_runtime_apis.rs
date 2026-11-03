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

use crate::utils::{
	extract_api_version, extract_block_type_from_trait_path, extract_impl_trait,
	extract_parameter_names_types_and_borrows, generate_crate_access,
	generate_runtime_mod_name_for_trait, prefix_function_with_trait, versioned_trait_name,
	AllowSelfRefInParameters, ApiVersion, RequireQualifiedTraitPath,
};

use proc_macro2::{Span, TokenStream};

use quote::quote;

use syn::{
	fold::{self, Fold},
	parse::{Error, Parse, ParseStream, Result},
	parse_macro_input, parse_quote,
	spanned::Spanned,
	visit_mut::{self, VisitMut},
	Attribute, Ident, ImplItem, ItemImpl, Path, Signature, Type, TypePath,
};

use std::collections::HashMap;

/// The structure used for parsing the runtime api implementations.
struct RuntimeApiImpls {
	impls: Vec<ItemImpl>,
}

impl Parse for RuntimeApiImpls {
	fn parse(input: ParseStream) -> Result<Self> {
		let mut impls = Vec::new();

		while !input.is_empty() {
			impls.push(ItemImpl::parse(input)?);
		}

		if impls.is_empty() {
			Err(Error::new(Span::call_site(), "No api implementation given!"))
		} else {
			Ok(Self { impls })
		}
	}
}

/// Generates the call to the implementation of the requested function.
/// The generated code includes decoding of the input arguments and encoding of the output.
fn generate_impl_call(
	signature: &Signature,
	runtime: &Type,
	input: &Ident,
	impl_trait: &Path,
	api_version: &ApiVersion,
) -> Result<TokenStream> {
	let params =
		extract_parameter_names_types_and_borrows(signature, AllowSelfRefInParameters::No)?;

	let c = generate_crate_access();
	let fn_name = &signature.ident;
	let fn_name_str = fn_name.to_string();
	let pnames = params.iter().map(|v| &v.0);
	let pnames2 = params.iter().map(|v| &v.0);
	let ptypes = params.iter().map(|v| &v.1);
	let pborrow = params.iter().map(|v| &v.2);

	let decode_params = if params.is_empty() {
		quote!(
			if !#input.is_empty() {
				panic!(
					"Bad input data provided to {}: expected no parameters, but input buffer is not empty. Nothing bad happened: someone sent an invalid transaction to the node.",
					#fn_name_str
				);
			}
		)
	} else {
		let let_binding = if params.len() == 1 {
			quote! {
				let #( #pnames )* : #( #ptypes )*
			}
		} else {
			quote! {
				let ( #( #pnames ),* ) : ( #( #ptypes ),* )
			}
		};

		quote!(
			#let_binding =
				match #c::Decode::decode(
					&mut #input,
				) {
					Ok(res) => res,
					Err(e) => panic!("Bad input data provided to {}: {}. Nothing bad happened: someone sent an invalid transaction to the node.", #fn_name_str, e),
				};
		)
	};

	let fn_calls = if let Some(feature_gated) = &api_version.feature_gated {
		let pnames = pnames2;
		let pnames2 = pnames.clone();
		let pborrow2 = pborrow.clone();

		let feature_name = &feature_gated.0;
		let impl_trait_fg = extend_with_api_version(impl_trait.clone(), Some(feature_gated.1));
		let impl_trait = extend_with_api_version(impl_trait.clone(), api_version.custom);

		quote!(
			#[cfg(feature = #feature_name)]
			#[allow(deprecated)]
			let r = <#runtime as #impl_trait_fg>::#fn_name(#( #pborrow #pnames ),*);

			#[cfg(not(feature = #feature_name))]
			#[allow(deprecated)]
			let r = <#runtime as #impl_trait>::#fn_name(#( #pborrow2 #pnames2 ),*);

			r
		)
	} else {
		let pnames = pnames2;
		let impl_trait = extend_with_api_version(impl_trait.clone(), api_version.custom);

		quote!(
			#[allow(deprecated)]
			<#runtime as #impl_trait>::#fn_name(#( #pborrow #pnames ),*)
		)
	};

	Ok(quote!(
		#decode_params

		#fn_calls
	))
}

/// Generate all the implementation calls for the given functions.
fn generate_impl_calls(
	impls: &[ItemImpl],
	input: &Ident,
) -> Result<Vec<(Ident, Ident, TokenStream, Vec<Attribute>)>> {
	let mut impl_calls = Vec::new();

	for impl_ in impls {
		let trait_api_ver = extract_api_version(&impl_.attrs, impl_.span())?;
		let impl_trait_path = extract_impl_trait(impl_, RequireQualifiedTraitPath::Yes)?;
		let impl_trait = extend_with_runtime_decl_path(impl_trait_path.clone());
		let impl_trait_ident = &impl_trait_path
			.segments
			.last()
			.ok_or_else(|| Error::new(impl_trait_path.span(), "Empty trait path not possible!"))?
			.ident;

		for item in &impl_.items {
			if let ImplItem::Fn(method) = item {
				let impl_call = generate_impl_call(
					&method.sig,
					&impl_.self_ty,
					input,
					&impl_trait,
					&trait_api_ver,
				)?;
				let mut attrs = filter_cfg_and_allow_attrs(&impl_.attrs);

				// Add any `#[cfg(feature = X)]` attributes of the method to result
				attrs.extend(filter_cfg_and_allow_attrs(&method.attrs));

				impl_calls.push((
					impl_trait_ident.clone(),
					method.sig.ident.clone(),
					impl_call,
					attrs,
				));
			}
		}
	}

	Ok(impl_calls)
}

/// Generate the dispatch function that is used in native to call into the runtime.
fn generate_dispatch_function(impls: &[ItemImpl]) -> Result<TokenStream> {
	let data = Ident::new("_sp_api_input_data_", Span::call_site());
	let c = generate_crate_access();
	let impl_calls =
		generate_impl_calls(impls, &data)?
			.into_iter()
			.map(|(trait_, fn_name, impl_, attrs)| {
				let name = prefix_function_with_trait(&trait_, &fn_name);
				quote!(
					#( #attrs )*
					#name => Some(#c::Encode::encode(&{ #impl_ })),
				)
			});

	Ok(quote!(
		#c::std_enabled! {
			pub fn dispatch(method: &str, mut #data: &[u8]) -> Option<Vec<u8>> {
				match method {
					#( #impl_calls )*
					_ => None,
				}
			}
		}
	))
}

/// Generate the interface functions that are used to call into the runtime in wasm.
fn generate_wasm_interface(impls: &[ItemImpl]) -> Result<TokenStream> {
	let input = Ident::new("input", Span::call_site());
	let c = generate_crate_access();

	let impl_calls =
        generate_impl_calls(impls, &input)?
            .into_iter()
            .map(|(trait_, fn_name, impl_, attrs)| {
                let fn_name =
                    Ident::new(&prefix_function_with_trait(&trait_, &fn_name), Span::call_site());

                quote!(
                    #c::std_disabled! {
                        #( #attrs )*
                        #[no_mangle]
                        #[cfg_attr(any(target_arch = "riscv32", target_arch = "riscv64"), #c::__private::polkavm_export(abi = #c::__private::polkavm_abi))]
                        pub unsafe extern fn #fn_name(input_data: *mut u8, input_len: usize) -> u64 {
                            let mut #input = if input_len == 0 {
                                &[0u8; 0]
                            } else {
                                unsafe {
                                    ::core::slice::from_raw_parts(input_data, input_len)
                                }
                            };

                            #c::init_runtime_logger();

                            let output = (move || { #impl_ })();
                            #c::to_substrate_wasm_fn_return_value(&output)
                        }
                    }
                )
            });

	Ok(quote!( #( #impl_calls )* ))
}

fn generate_runtime_api_base_structures() -> Result<TokenStream> {
	let crate_ = generate_crate_access();

	Ok(quote!(
		pub struct RuntimeApi {}
		#crate_::std_enabled! {
			/// Implements all runtime apis for the client side.
			pub struct RuntimeApiImpl<Block: #crate_::BlockT, C: #crate_::CallApiAt<Block> + 'static> {
				call: &'static C,
				transaction_depth: std::cell::RefCell<u16>,
				changes: std::cell::RefCell<#crate_::OverlayedChanges<#crate_::HashingFor<Block>>>,
				recorder: std::option::Option<#crate_::ProofRecorder<Block>>,
				call_context: #crate_::CallContext,
				extensions: std::cell::RefCell<#crate_::Extensions>,
				extensions_generated_for: std::cell::RefCell<std::option::Option<Block::Hash>>,
			}

			#[automatically_derived]
			impl<Block: #crate_::BlockT, C: #crate_::CallApiAt<Block>> #crate_::ApiExt<Block> for
				RuntimeApiImpl<Block, C>
			{
				fn execute_in_transaction<F: FnOnce(&Self) -> #crate_::TransactionOutcome<R>, R>(
					&self,
					call: F,
				) -> R where Self: Sized {
					self.start_transaction();

					*std::cell::RefCell::borrow_mut(&self.transaction_depth) += 1;
					let res = call(self);
					std::cell::RefCell::borrow_mut(&self.transaction_depth)
						.checked_sub(1)
						.expect("Transactions are opened and closed together; qed");

					self.commit_or_rollback_transaction(
						std::matches!(res, #crate_::TransactionOutcome::Commit(_))
					);

					res.into_inner()
				}

				fn has_api<A: #crate_::RuntimeApiInfo + ?Sized>(
					&self,
					at: <Block as #crate_::BlockT>::Hash,
				) -> std::result::Result<bool, #crate_::ApiError> where Self: Sized {
					#crate_::CallApiAt::<Block>::runtime_version_at(self.call, at, self.call_context)
					.map(|v| #crate_::RuntimeVersion::has_api_with(&v, &A::ID, |v| v == A::VERSION))
				}

				fn has_api_with<A: #crate_::RuntimeApiInfo + ?Sized, P: Fn(u32) -> bool>(
					&self,
					at: <Block as #crate_::BlockT>::Hash,
					pred: P,
				) -> std::result::Result<bool, #crate_::ApiError> where Self: Sized {
					#crate_::CallApiAt::<Block>::runtime_version_at(self.call, at, self.call_context)
					.map(|v| #crate_::RuntimeVersion::has_api_with(&v, &A::ID, pred))
				}

				fn api_version<A: #crate_::RuntimeApiInfo + ?Sized>(
					&self,
					at: <Block as #crate_::BlockT>::Hash,
				) -> std::result::Result<Option<u32>, #crate_::ApiError> where Self: Sized {
					#crate_::CallApiAt::<Block>::runtime_version_at(self.call, at, self.call_context)
					.map(|v| #crate_::RuntimeVersion::api_version(&v, &A::ID))
				}

				fn record_proof(&mut self) {
					self.recorder = std::option::Option::Some(std::default::Default::default());
				}

				fn record_proof_with_recorder(&mut self, recorder: #crate_::ProofRecorder<Block>) {
					self.recorder = std::option::Option::Some(recorder);
				}

				fn proof_recorder(&self) -> std::option::Option<#crate_::ProofRecorder<Block>> {
					std::clone::Clone::clone(&self.recorder)
				}

				fn extract_proof(
					&mut self,
				) -> std::option::Option<#crate_::StorageProof> {
					let recorder = std::option::Option::take(&mut self.recorder);
					std::option::Option::map(recorder, |recorder| {
						#crate_::ProofRecorder::<Block>::drain_storage_proof(recorder)
					})
				}

				fn into_storage_changes<B: #crate_::StateBackend<#crate_::HashingFor<Block>>>(
					&self,
					backend: &B,
					parent_hash: Block::Hash,
				) -> ::core::result::Result<
					#crate_::StorageChanges<Block>,
				String
					> where Self: Sized {
						let state_version = #crate_::CallApiAt::<Block>::runtime_version_at(self.call, std::clone::Clone::clone(&parent_hash), self.call_context)
							.map(|v| #crate_::RuntimeVersion::state_version(&v))
							.map_err(|e| format!("Failed to get state version: {}", e))?;

						#crate_::OverlayedChanges::drain_storage_changes(
							&mut std::cell::RefCell::borrow_mut(&self.changes),
							backend,
							state_version,
						)
					}

				fn set_call_context(&mut self, call_context: #crate_::CallContext) {
					self.call_context = call_context;
				}

				fn register_extension<E: #crate_::Extension>(&mut self, extension: E) {
					std::cell::RefCell::borrow_mut(&self.extensions).register(extension);
				}

				fn set_overlayed_changes(&mut self, changes: #crate_::OverlayedChanges<#crate_::HashingFor<Block>>) {
					*self.changes.borrow_mut() = changes;
				}
			}

			#[automatically_derived]
			impl<Block: #crate_::BlockT, C> #crate_::ConstructRuntimeApi<Block, C>
				for RuntimeApi
			where
				C: #crate_::CallApiAt<Block> + 'static,
			{
				type RuntimeApi = RuntimeApiImpl<Block, C>;

				fn construct_runtime_api<'a>(
					call: &'a C,
				) -> #crate_::ApiRef<'a, Self::RuntimeApi> {
					RuntimeApiImpl {
						call: unsafe { std::mem::transmute(call) },
						transaction_depth: 0.into(),
						changes: std::default::Default::default(),
						recorder: std::default::Default::default(),
						call_context: #crate_::CallContext::Offchain,
						extensions: std::default::Default::default(),
						extensions_generated_for: std::default::Default::default(),
					}.into()
				}
			}

			#[automatically_derived]
			impl<Block: #crate_::BlockT, C: #crate_::CallApiAt<Block>> RuntimeApiImpl<Block, C> {
				fn commit_or_rollback_transaction(&self, commit: bool) {
					let proof = "\
                    We only close a transaction when we opened one ourself.
                    Other parts of the runtime that make use of transactions (state-machine)
                    also balance their transactions. The runtime cannot close client initiated
                    transactions; qed";

					let res = if commit {
						let res = if let Some(recorder) = &self.recorder {
							#crate_::ProofRecorder::<Block>::commit_transaction(&recorder)
						} else {
							Ok(())
						};

						let res2 = #crate_::OverlayedChanges::commit_transaction(
							&mut std::cell::RefCell::borrow_mut(&self.changes)
						);

						#crate_::Extensions::commit_transaction(
							&mut std::cell::RefCell::borrow_mut(&self.extensions),
							#crate_::TransactionType::Host,
						);

						// Will panic on an `Err` below, however we should call commit
						// on the recorder and the changes together.
						std::result::Result::and(res, std::result::Result::map_err(res2, drop))
					} else {
						let res = if let Some(recorder) = &self.recorder {
							#crate_::ProofRecorder::<Block>::rollback_transaction(&recorder)
						} else {
							Ok(())
						};

						let res2 = #crate_::OverlayedChanges::rollback_transaction(
							&mut std::cell::RefCell::borrow_mut(&self.changes)
						);

						#crate_::Extensions::rollback_transaction(
							&mut std::cell::RefCell::borrow_mut(&self.extensions),
							#crate_::TransactionType::Host,
						);

						// Will panic on an `Err` below, however we should call commit
						// on the recorder and the changes together.
						std::result::Result::and(res, std::result::Result::map_err(res2, drop))
					};

					std::result::Result::expect(res, proof);
				}

				fn start_transaction(&self) {
					#crate_::OverlayedChanges::start_transaction(
						&mut std::cell::RefCell::borrow_mut(&self.changes)
					);
					if let Some(recorder) = &self.recorder {
						#crate_::ProofRecorder::<Block>::start_transaction(&recorder);
					}

					#crate_::Extensions::start_transaction(
						&mut std::cell::RefCell::borrow_mut(&self.extensions),
						#crate_::TransactionType::Host,
					);
				}
			}
		}
	))
}

/// Extend the given trait path with module that contains the declaration of the trait for the
/// runtime.
fn extend_with_runtime_decl_path(mut trait_: Path) -> Path {
	let runtime = {
		let trait_name = &trait_
			.segments
			.last()
			.as_ref()
			.expect("Trait path should always contain at least one item; qed")
			.ident;

		generate_runtime_mod_name_for_trait(trait_name)
	};

	let pos = trait_.segments.len() - 1;
	trait_.segments.insert(pos, runtime.into());
	trait_
}

fn extend_with_api_version(mut trait_: Path, version: Option<u32>) -> Path {
	let version = if let Some(v) = version {
		v
	} else {
		// nothing to do
		return trait_;
	};

	let trait_name = &mut trait_
		.segments
		.last_mut()
		.expect("Trait path should always contain at least one item; qed")
		.ident;
	*trait_name = versioned_trait_name(trait_name, version);

	trait_
}

/// Adds a feature guard to `attributes`.
///
/// Depending on `enable`, the feature guard either enables ('feature = "something"`) or disables
/// (`not(feature = "something")`).
fn add_feature_guard(attrs: &mut Vec<Attribute>, feature_name: &str, enable: bool) {
	let attr = match enable {
		true => parse_quote!(#[cfg(feature = #feature_name)]),
		false => parse_quote!(#[cfg(not(feature = #feature_name))]),
	};
	attrs.push(attr);
}

/// Generates the implementations of the apis for the runtime.
fn generate_api_impl_for_runtime(impls: &[ItemImpl]) -> Result<TokenStream> {
	let mut impls_prepared = Vec::new();

	// We put `runtime` before each trait to get the trait that is intended for the runtime and
	// we put the `RuntimeBlock` as first argument for the trait generics.
	for impl_ in impls.iter() {
		let trait_api_ver = extract_api_version(&impl_.attrs, impl_.span())?;

		let mut impl_ = impl_.clone();
		impl_.attrs = filter_cfg_and_allow_attrs(&impl_.attrs);

		let trait_ = extract_impl_trait(&impl_, RequireQualifiedTraitPath::Yes)?.clone();
		let trait_ = extend_with_runtime_decl_path(trait_);
		// If the trait api contains feature gated version - there are staging methods in it. Handle
		// them explicitly here by adding staging implementation with `#cfg(feature = ...)` and
		// stable implementation with `#[cfg(not(feature = ...))]`.
		if let Some(feature_gated) = trait_api_ver.feature_gated {
			let mut feature_gated_impl = impl_.clone();
			add_feature_guard(&mut feature_gated_impl.attrs, &feature_gated.0, true);
			feature_gated_impl.trait_.as_mut().unwrap().1 =
				extend_with_api_version(trait_.clone(), Some(feature_gated.1));

			impls_prepared.push(feature_gated_impl);

			// Finally add `#[cfg(not(feature = ...))]` for the stable implementation (which is
			// generated outside this if).
			add_feature_guard(&mut impl_.attrs, &feature_gated.0, false);
		}

		// Generate stable trait implementation.
		let trait_ = extend_with_api_version(trait_, trait_api_ver.custom);
		impl_.trait_.as_mut().unwrap().1 = trait_;
		impls_prepared.push(impl_);
	}

	Ok(quote!( #( #impls_prepared )* ))
}

/// Auxiliary data structure that is used to convert `impl Api for Runtime` to
/// `impl Api for RuntimeApi`.
/// This requires us to replace the runtime `Block` with the node `Block`,
/// `impl Api for Runtime` with `impl Api for RuntimeApi` and replace the method implementations
/// with code that calls into the runtime.
struct ApiRuntimeImplToApiRuntimeApiImpl<'a> {
	runtime_block: &'a TypePath,
}

impl<'a> ApiRuntimeImplToApiRuntimeApiImpl<'a> {
	/// Process the given item implementation.
	fn process(mut self, input: ItemImpl) -> ItemImpl {
		let mut input = self.fold_item_impl(input);

		let crate_ = generate_crate_access();

		// Delete all functions, because all of them are default implemented by
		// `decl_runtime_apis!`. We only need to implement the `__runtime_api_internal_call_api_at`
		// function.
		input.items.clear();
		input.items.push(parse_quote! {
			fn __runtime_api_internal_call_api_at(
				&self,
				at: <__SrApiBlock__ as #crate_::BlockT>::Hash,
				params: std::vec::Vec<u8>,
				fn_name: &dyn Fn(#crate_::RuntimeVersion) -> &'static str,
			) -> std::result::Result<std::vec::Vec<u8>, #crate_::ApiError> {
				// If we are not already in a transaction, we should create a new transaction
				// and then commit/roll it back at the end!
				let transaction_depth = *std::cell::RefCell::borrow(&self.transaction_depth);

				if transaction_depth == 0 {
					self.start_transaction();
				}

				let res = (|| {
					let version = #crate_::CallApiAt::<__SrApiBlock__>::runtime_version_at(
						self.call,
						at,
						self.call_context,
					)?;

					match &mut *std::cell::RefCell::borrow_mut(&self.extensions_generated_for) {
						Some(generated_for) => {
							if *generated_for != at {
								return std::result::Result::Err(
									#crate_::ApiError::UsingSameInstanceForDifferentBlocks
								)
							}
						},
						generated_for @ None => {
							#crate_::CallApiAt::<__SrApiBlock__>::initialize_extensions(
								self.call,
								at,
								&mut std::cell::RefCell::borrow_mut(&self.extensions),
							)?;

							*generated_for = Some(at);
						}
					}

					let params = #crate_::CallApiAtParams {
						at,
						function: (*fn_name)(version),
						arguments: params,
						overlayed_changes: &self.changes,
						call_context: self.call_context,
						recorder: &self.recorder,
						extensions: &self.extensions,
					};

					#crate_::CallApiAt::<__SrApiBlock__>::call_api_at(
						self.call,
						params,
					)
				})();

				if transaction_depth == 0 {
					self.commit_or_rollback_transaction(std::result::Result::is_ok(&res));
				}

				res
			}
		});

		input
	}
}

impl<'a> Fold for ApiRuntimeImplToApiRuntimeApiImpl<'a> {
	fn fold_type_path(&mut self, input: TypePath) -> TypePath {
		let new_ty_path =
			if input == *self.runtime_block { parse_quote!(__SrApiBlock__) } else { input };

		fold::fold_type_path(self, new_ty_path)
	}

	fn fold_item_impl(&mut self, mut input: ItemImpl) -> ItemImpl {
		let crate_ = generate_crate_access();

		// Implement the trait for the `RuntimeApiImpl`
		input.self_ty =
			Box::new(parse_quote!( RuntimeApiImpl<__SrApiBlock__, RuntimeApiImplCall> ));

		input.generics.params.push(parse_quote!(
			__SrApiBlock__: #crate_::BlockT
		));

		input
			.generics
			.params
			.push(parse_quote!( RuntimeApiImplCall: #crate_::CallApiAt<__SrApiBlock__> + 'static ));

		let where_clause = input.generics.make_where_clause();

		where_clause.predicates.push(parse_quote! {
			RuntimeApiImplCall::StateBackend:
				#crate_::StateBackend<#crate_::HashingFor<__SrApiBlock__>>
		});

		where_clause.predicates.push(parse_quote! { &'static RuntimeApiImplCall: Send });

		input.attrs = filter_cfg_and_allow_attrs(&input.attrs);

		fold::fold_item_impl(self, input)
	}
}

/// Generate the implementations of the runtime apis for the `RuntimeApi` type.
fn generate_api_impl_for_runtime_api(impls: &[ItemImpl]) -> Result<TokenStream> {
	let mut result = Vec::with_capacity(impls.len());

	for impl_ in impls {
		let impl_trait_path = extract_impl_trait(impl_, RequireQualifiedTraitPath::Yes)?;
		let runtime_block = extract_block_type_from_trait_path(impl_trait_path)?;
		let mut runtime_mod_path = extend_with_runtime_decl_path(impl_trait_path.clone());
		// remove the trait to get just the module path
		runtime_mod_path.segments.pop();

		let mut processed_impl =
			ApiRuntimeImplToApiRuntimeApiImpl { runtime_block }.process(impl_.clone());

		processed_impl.attrs.push(parse_quote!(#[automatically_derived]));

		result.push(processed_impl);
	}

	let crate_ = generate_crate_access();

	Ok(quote!( #crate_::std_enabled! { #( #result )* } ))
}

fn populate_runtime_api_versions(
	result: &mut Vec<TokenStream>,
	sections: &mut Vec<TokenStream>,
	attrs: Vec<Attribute>,
	id: Path,
	version: TokenStream,
	crate_access: &TokenStream,
) {
	result.push(quote!(
			#( #attrs )*
			(#id, #version)
	));

	sections.push(quote!(
		#crate_access::std_disabled! {
			#( #attrs )*
			const _: () = {
				// All sections with the same name are going to be merged by concatenation.
				#[link_section = "runtime_apis"]
				static SECTION_CONTENTS: [u8; 12] = #crate_access::serialize_runtime_api_info(#id, #version);
			};
		}
	));
}

/// Generates `RUNTIME_API_VERSIONS` that holds all version information about the implemented
/// runtime apis.
fn generate_runtime_api_versions(impls: &[ItemImpl]) -> Result<TokenStream> {
	let mut result = Vec::<TokenStream>::with_capacity(impls.len());
	let mut sections = Vec::<TokenStream>::with_capacity(impls.len());
	let mut processed_traits = HashMap::new();

	let c = generate_crate_access();

	for impl_ in impls {
		let versions = extract_api_version(&impl_.attrs, impl_.span())?;
		let api_ver = versions.custom.map(|a| a as u32);

		let mut path = extend_with_runtime_decl_path(
			extract_impl_trait(impl_, RequireQualifiedTraitPath::Yes)?.clone(),
		);
		// Remove the trait
		let trait_ = path
			.segments
			.pop()
			.expect("extract_impl_trait already checks that this is valid; qed")
			.into_value()
			.ident;

		let span = trait_.span();
		if let Some(other_span) = processed_traits.insert(trait_, span) {
			let mut error = Error::new(
				span,
				"Two traits with the same name detected! \
                    The trait name is used to generate its ID. \
                    Please rename one trait at the declaration!",
			);

			error.combine(Error::new(other_span, "First trait implementation."));

			return Err(error);
		}

		let id: Path = parse_quote!( #path ID );
		let mut attrs = filter_cfg_and_allow_attrs(&impl_.attrs);

		// Handle API versioning
		// If feature gated version is set - handle it first
		if let Some(feature_gated) = versions.feature_gated {
			let feature_gated_version = feature_gated.1 as u32;
			// the attributes for the feature gated staging api
			let mut feature_gated_attrs = attrs.clone();
			add_feature_guard(&mut feature_gated_attrs, &feature_gated.0, true);
			populate_runtime_api_versions(
				&mut result,
				&mut sections,
				feature_gated_attrs,
				id.clone(),
				quote!( #feature_gated_version ),
				&c,
			);

			// Add `#[cfg(not(feature ...))]` to the initial attributes. If the staging feature flag
			// is not set we want to set the stable api version
			add_feature_guard(&mut attrs, &feature_gated.0, false);
		}

		// Now add the stable api version to the versions list. If the api has got staging functions
		// there might be a `#[cfg(not(feature ...))]` attribute attached to the stable version.
		let base_api_version = quote!( #path VERSION );
		let api_ver = api_ver.map(|a| quote!( #a )).unwrap_or_else(|| base_api_version);
		populate_runtime_api_versions(&mut result, &mut sections, attrs, id, api_ver, &c);
	}

	Ok(quote!(
		pub const RUNTIME_API_VERSIONS: #c::ApisVec = #c::create_apis_vec!([ #( #result ),* ]);

		#( #sections )*
	))
}

/// replaces `Self` with explicit `ItemImpl.self_ty`.
struct ReplaceSelfImpl {
	self_ty: Box<Type>,
}

impl ReplaceSelfImpl {
	/// Replace `Self` with `ItemImpl.self_ty`
	fn replace(&mut self, trait_: &mut ItemImpl) {
		visit_mut::visit_item_impl_mut(self, trait_)
	}
}

impl VisitMut for ReplaceSelfImpl {
	fn visit_type_mut(&mut self, ty: &mut syn::Type) {
		match ty {
			Type::Path(p) if p.path.is_ident("Self") => {
				*ty = *self.self_ty.clone();
			},
			ty => syn::visit_mut::visit_type_mut(self, ty),
		}
	}
}

/// Rename `Self` to `ItemImpl.self_ty` in all items.
fn rename_self_in_trait_impls(impls: &mut [ItemImpl]) {
	impls.iter_mut().for_each(|i| {
		let mut checker = ReplaceSelfImpl { self_ty: i.self_ty.clone() };
		checker.replace(i);
	});
}

/// The implementation of the `impl_runtime_apis!` macro.
pub fn impl_runtime_apis_impl(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
	// Parse all impl blocks
	let RuntimeApiImpls { impls: mut api_impls } = parse_macro_input!(input as RuntimeApiImpls);

	impl_runtime_apis_impl_inner(&mut api_impls)
		.unwrap_or_else(|e| e.to_compile_error())
		.into()
}

fn impl_runtime_apis_impl_inner(api_impls: &mut [ItemImpl]) -> Result<TokenStream> {
	rename_self_in_trait_impls(api_impls);

	let dispatch_impl = generate_dispatch_function(api_impls)?;
	let api_impls_for_runtime = generate_api_impl_for_runtime(api_impls)?;
	let base_runtime_api = generate_runtime_api_base_structures()?;
	let runtime_api_versions = generate_runtime_api_versions(api_impls)?;
	let wasm_interface = generate_wasm_interface(api_impls)?;
	let api_impls_for_runtime_api = generate_api_impl_for_runtime_api(api_impls)?;

	let runtime_metadata = crate::runtime_metadata::generate_impl_runtime_metadata(api_impls)?;

	let impl_ = quote!(
		#base_runtime_api

		#api_impls_for_runtime

		#api_impls_for_runtime_api

		#runtime_api_versions

		#runtime_metadata

		pub mod api {
			use super::*;

			#dispatch_impl

			#wasm_interface
		}
	);

	let impl_ = expander::Expander::new("impl_runtime_apis")
		.dry(std::env::var("EXPAND_MACROS").is_err())
		.verbose(true)
		.write_to_out_dir(impl_)
		.expect("Does not fail because of IO in OUT_DIR; qed");

	Ok(impl_)
}

// Filters all attributes except the cfg and allow ones.
fn filter_cfg_and_allow_attrs(attrs: &[Attribute]) -> Vec<Attribute> {
	attrs
		.iter()
		.filter(|a| a.path().is_ident("cfg") || a.path().is_ident("allow"))
		.cloned()
		.collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn filter_non_cfg_attributes() {
		let cfg_std: Attribute = parse_quote!(#[cfg(feature = "std")]);
		let cfg_benchmarks: Attribute = parse_quote!(#[cfg(feature = "runtime-benchmarks")]);
		let allow: Attribute = parse_quote!(#[allow(non_camel_case_types)]);

		let attrs = vec![
			cfg_std.clone(),
			parse_quote!(#[derive(Debug)]),
			parse_quote!(#[test]),
			cfg_benchmarks.clone(),
			parse_quote!(#[allow(non_camel_case_types)]),
		];

		let filtered = filter_cfg_and_allow_attrs(&attrs);
		assert_eq!(filtered.len(), 3);
		assert_eq!(cfg_std, filtered[0]);
		assert_eq!(cfg_benchmarks, filtered[1]);
		assert_eq!(allow, filtered[2]);
	}

	#[test]
	fn impl_trait_rename_self_param() {
		let code = quote::quote! {
			impl client::Core<Block> for Runtime {
				fn initialize_block(header: &HeaderFor<Self>) -> Output<Self> {
					let _: HeaderFor<Self> = header.clone();
					example_fn::<Self>(header)
				}
			}
		};
		let expected = quote::quote! {
			impl client::Core<Block> for Runtime {
				fn initialize_block(header: &HeaderFor<Runtime>) -> Output<Runtime> {
					let _: HeaderFor<Runtime> = header.clone();
					example_fn::<Runtime>(header)
				}
			}
		};

		// Parse the items
		let RuntimeApiImpls { impls: mut api_impls } =
			syn::parse2::<RuntimeApiImpls>(code).unwrap();

		// Run the renamer which is being run first in the `impl_runtime_apis!` macro.
		rename_self_in_trait_impls(&mut api_impls);
		let result: TokenStream = quote::quote! {  #(#api_impls)* };

		assert_eq!(result.to_string(), expected.to_string());
	}
}
