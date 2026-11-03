// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//  http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Tests for various invariants for the versioned runtime API. They need to be here since this is
//! the only place where we have the metadata we need to run these tests and a runtime that we can
//! call.

extern crate alloc;

use alloc::collections::{BTreeMap, BTreeSet};
use codec::Decode;
use core::any::TypeId;
use polkadot_sdk::{
	pallet_revive::{
		pallet_revive_types::runtime_api::ReviveRuntimeApiVersionDeclarations,
		runtime_decl_for_revive_api::ReviveApi,
	},
	sp_io::TestExternalities,
	sp_metadata_ir::frame_metadata::{
		v16::{
			ItemDeprecationInfo, RuntimeApiMetadata, RuntimeApiMethodMetadata, RuntimeMetadataV16,
		},
		RuntimeMetadata, RuntimeMetadataPrefixed,
	},
};
use scale_info::{
	form::PortableForm, interner::UntrackedSymbol, Field, Type, TypeDef, TypeDefArray,
	TypeDefBitSequence, TypeDefCompact, TypeDefComposite, TypeDefPrimitive, TypeDefSequence,
	TypeDefTuple, TypeDefVariant,
};

const UNVERSIONED_RUNTIME_APIS: [&str; 28] = [
	"eth_block",
	"eth_block_hash",
	"eth_receipt_data",
	"block_gas_limit",
	"max_extrinsic_weight_in_gas",
	"balance",
	"gas_price",
	"nonce",
	"call",
	"instantiate",
	"eth_transact",
	"eth_transact_with_config",
	"eth_estimate_gas",
	"eth_pre_dispatch_weight",
	"upload_code",
	"get_storage",
	"get_storage_var_key",
	"trace_block",
	"trace_tx",
	"trace_call",
	"trace_call_with_config",
	"block_author",
	"address",
	"account_id",
	"runtime_pallets_address",
	"code",
	"new_balance_with_dust",
	"version_declarations",
];

/// Tests all of the invariants which we want to uphold for the versioned runtime API.
///
/// 1. That all functions have a "_versioned" suffix.
/// 2. That if an unversioned counterpart exists that it's been deprecated.
/// 3. That all functions have exactly 1 input argument.
/// 4. That the input argument has the name `${function-name-cleaned:camel}VersionedInputPayload`
///    and all output types have the name `${function-name-cleaned:camel}VersionedOutputPayload`.
/// 5. That the input and output types are enums.
/// 6. That the input and output enums have an equal number of variants which is not zero.
/// 7. That the input and output variants have a scale index of N - 1 where N is the version.
/// 8. That the input and output variants are `V` followed by a non-zero number and that they're
///    contiguous.
/// 9. That the input and output variants have a single un-named field.
/// 10. That the input and output variant field is of the type
///     `${function-name-cleaned:camel}(Input|Output)PayloadV${i}`
/// 11. That the function has a versioned declared in the `ReviveRuntimeApiVersionDeclarations`.
/// 12. That the declared version matches the highest version available on the versioned payload
///     enums.
/// 13. That the `ReviveRuntimeApiVersionDeclarations` does not contain any runtime API function
///     which is not versioned.
#[test]
fn test_versioned_runtime_api_invariants() {
	let mut version_declarations = version_declarations().into_iter().collect::<BTreeMap<_, _>>();
	let (metadata, runtime_api_metadata) = metadata();
	let versioned_runtime_api_functions = runtime_api_metadata
		.methods
		.iter()
		.filter(|function| !UNVERSIONED_RUNTIME_APIS.contains(&function.name.as_str()));

	for function in versioned_runtime_api_functions {
		// Assertion 1
		let Some(function_name_no_suffix) = function.name.strip_suffix("_versioned") else {
			panic!(
				"All versioned runtime API function names must have a \"_versioned\" suffix, but \
                {} does not have it",
				function.name
			)
		};
		let function_name_no_cleaned =
			function_name_no_suffix.strip_prefix("eth_").unwrap_or(function_name_no_suffix);

		// Assertion 2
		if let Some(unversioned_function) = runtime_api_metadata
			.methods
			.iter()
			.find(|function| function.name == function_name_no_suffix)
		{
			assert!(
                matches!(unversioned_function.deprecation_info, ItemDeprecationInfo::Deprecated { .. } | ItemDeprecationInfo::DeprecatedWithoutNote),
                "All versioned runtime API functions must deprecate their unversioned counterparts \
                but {}, the unversioned variant of {} is not deprecated",
                unversioned_function.name,
                function.name
            )
		}

		// Assertion 3
		let [input] = function.inputs.as_slice() else {
			panic!(
				"All versioned runtime API functions must have exactly one input argument, but {} \
                has {} arguments.",
				function.name,
				function.inputs.len()
			)
		};

		// Assertion 4
		let output = function.output;
		let input_type = metadata
			.types
			.resolve(input.ty.id)
			.expect("impossible on well-formed metadata; qed");
		let output_type = metadata
			.types
			.resolve(output.id)
			.expect("impossible on well-formed metadata; qed");
		let output_type =
			extract_ok_type_from_result(output_type, &metadata).unwrap_or(output_type);

		let input_type_name = input_type.path.ident().expect("an input type has an ident; qed");
		let output_type_name = output_type.path.ident().expect("an output type has an ident; qed");

		assert_eq!(
			input_type_name,
			format!("{}VersionedInputPayload", snake_to_camel(function_name_no_cleaned)),
			"All versioned runtime API functions must have an input payload with the name \
            ${{function-name-no-suffix:camel}}VersionedInputPayload but {} has an input payload \
            with the name {}",
			function.name,
			input_type_name
		);
		assert_eq!(
			output_type_name,
			format!("{}VersionedOutputPayload", snake_to_camel(function_name_no_cleaned)),
			"All versioned runtime API functions must have an output payload with the name \
            ${{function-name-no-suffix:camel}}VersionedOutputPayload but {} has an output payload \
            with the name {}",
			function.name,
			output_type_name
		);

		// Assertion 5
		let TypeDef::Variant(ref input_variants) = input_type.type_def else {
			panic!(
				"All versioned runtime API functions must have an argument which is an enum. But \
                the arguments of {} is of the type definition {:?}",
				function.name, input_type.type_def
			)
		};
		let TypeDef::Variant(ref output_variants) = output_type.type_def else {
			panic!(
				"All versioned runtime API functions must have an output which is an enum. But \
                the output of {} is of the type definition {:?}",
				function.name, output_type.type_def
			)
		};

		// Assertion 6
		assert_eq!(
			input_variants.variants.len(),
			output_variants.variants.len(),
			"All versioned runtime API functions must have IO enums which have an equal number of \
            variants. But {} has {} input variants and {} output variants",
			function.name,
			input_variants.variants.len(),
			output_variants.variants.len(),
		);
		assert_ne!(
			input_variants.variants.len(),
			0,
			"All versioned runtime API functions must have IO enums with at least 1 variant. But \
            {} has zero variants",
			function.name,
		);

		for (i, (input_variant, output_variant)) in
			input_variants.variants.iter().zip(output_variants.variants.iter()).enumerate()
		{
			let version = i + 1;

			// Assertion 7
			assert_eq!(
				input_variant.index as usize, i,
				"Expected variant {} in {} to have a scale index of {} but it has an index of {}",
				input_variant.name, input_type_name, i, input_variant.index
			);
			assert_eq!(
				output_variant.index as usize, i,
				"Expected variant {} in {} to have a scale index of {} but it has an index of {}",
				output_variant.name, output_type_name, i, output_variant.index
			);

			// Assertion 8
			assert_eq!(
				input_variant.name,
				format!("V{version}"),
				"Expected variant {} in {} to be {} called",
				input_variant.name,
				input_type_name,
				format!("V{version}")
			);
			assert_eq!(
				output_variant.name,
				format!("V{version}"),
				"Expected variant {} in {} to be {} called",
				output_variant.name,
				output_type_name,
				format!("V{version}")
			);

			// Assertion 9
			let [input_field @ Field { name: None, .. }] = input_variant.fields.as_slice() else {
				panic!(
					"Expected variant {} in {} to have a single un-named field",
					input_variant.name, input_type_name
				)
			};
			let [output_field @ Field { name: None, .. }] = output_variant.fields.as_slice() else {
				panic!(
					"Expected variant {} in {} to have a single un-named field",
					output_variant.name, output_type_name
				)
			};

			// Assertion 10
			let input_field_type = metadata
				.types
				.resolve(input_field.ty.id)
				.expect("impossible on well-formed metadata; qed");
			let output_field_type = metadata
				.types
				.resolve(output_field.ty.id)
				.expect("impossible on well-formed metadata; qed");

			let input_field_type_name =
				input_field_type.path.ident().expect("an input type has an ident; qed");
			let output_field_type_name =
				output_field_type.path.ident().expect("an output type has an ident; qed");

			assert_eq!(
				input_field_type_name,
				format!("{}InputPayloadV{}", snake_to_camel(function_name_no_cleaned), version),
				"Expected variant {} of {} to have the type {} but it has the type {}",
				input_variant.name,
				input_type_name,
				format!("{}InputPayloadV{}", snake_to_camel(function_name_no_cleaned), version),
				input_field_type_name
			);
			assert_eq!(
				output_field_type_name,
				format!("{}OutputPayloadV{}", snake_to_camel(function_name_no_cleaned), version),
				"Expected variant {} of {} to have the type {} but it has the type {}",
				output_variant.name,
				output_type_name,
				format!("{}OutputPayloadV{}", snake_to_camel(function_name_no_cleaned), version),
				output_field_type_name
			);
		}

		// Assertion 11
		let Some(declared_version) = version_declarations.remove(&function.name) else {
			panic!(
                "All versioned runtime API functions must have a highest version declared for them \
                on the ReviveRuntimeApiVersionDeclarations but {} has no such declaration",
                function.name
            )
		};

		// Assertion 12
		assert_eq!(
			declared_version as usize,
			input_variants.variants.len(),
			"The declared version for {} is {} but its enum variants have up to V{}",
			function.name,
			declared_version,
			input_variants.variants.len()
		);
	}

	// Assertion 13
	assert!(
		version_declarations.is_empty(),
		"The ReviveRuntimeApiVersionDeclarations contains version declarations for unversioned \
        runtime API function: {:?}",
		version_declarations.into_keys().collect::<BTreeSet<_>>()
	)
}

#[test]
fn unversioned_runtime_api_functions_are_unchanged_by_versioning() {
	let (pre_versioning_metadata, pre_versioning_runtime_api) = pre_versioning_metadata();
	let (post_versioning_metadata, post_versioning_runtime_api) = metadata();

	// The opcodes of the execution tracer's steps changed from bare `u8`s into the `EvmOpcodeV1`
	// and `PolkavmSyscallV1` types when the tracing wire types were added. They're all
	// scale-compatible with a `u8` and the change is intentional, therefore these pairs of types
	// are reconciled as being equal.
	let pre_versioning_u8_type_id = type_id_of(&pre_versioning_metadata, |ty| {
		matches!(&ty.type_def, TypeDef::Primitive(TypeDefPrimitive::U8))
	});
	let mut recursion_guard = BTreeSet::new();
	for reconciled_type_name in ["EvmOpcodeV1", "PolkavmSyscallV1"] {
		recursion_guard.insert((
			pre_versioning_u8_type_id,
			type_id_of(&post_versioning_metadata, |ty| {
				ty.path.ident().as_deref() == Some(reconciled_type_name)
			}),
		));
	}

	for function_name in UNVERSIONED_RUNTIME_APIS
		.into_iter()
		.filter(|function_name| *function_name != "version_declarations")
	{
		let pre_versioning_function = pre_versioning_runtime_api
			.methods
			.iter()
			.find(|method| method.name == function_name)
			.unwrap_or_else(|| {
				panic!(
					"the pre-versioning metadata must contain the `{function_name}` runtime API \
                    function"
				)
			});
		let post_versioning_function = post_versioning_runtime_api
			.methods
			.iter()
			.find(|method| method.name == function_name)
			.unwrap_or_else(|| {
				panic!(
					"the post-versioning metadata must contain the `{function_name}` runtime API \
                    function"
				)
			});

		check_function(
			State {
				metadata: &pre_versioning_metadata,
				path: FunctionPathItem::new(function_name).into(),
				item: pre_versioning_function,
			},
			State {
				metadata: &post_versioning_metadata,
				path: FunctionPathItem::new(function_name).into(),
				item: post_versioning_function,
			},
			&mut recursion_guard,
		);
	}
}

fn check_function(
	pre_versioning_state: State<'_, RuntimeApiMethodMetadata<PortableForm>>,
	post_versioning_state: State<'_, RuntimeApiMethodMetadata<PortableForm>>,
	recursion_guard: &mut BTreeSet<(u32, u32)>,
) {
	assert_eq!(
		pre_versioning_state.item.inputs.len(),
		post_versioning_state.item.inputs.len(),
		"Encountered a runtime API function which has a different number of arguments before \
        and after versioning. \n\
        Before: {}\n\
        After: {}\n\
        Pre Versioning Item Path: {:?}\n
        Post Versioning Item Path: {:?}\n",
		pre_versioning_state.item.inputs.len(),
		post_versioning_state.item.inputs.len(),
		pre_versioning_state.path,
		post_versioning_state.path,
	);

	for (pre_versioning_argument, post_versioning_argument) in pre_versioning_state
		.item
		.inputs
		.iter()
		.zip(post_versioning_state.item.inputs.iter())
	{
		check_type(
			pre_versioning_state
				.push(ArgumentPathItem::new(pre_versioning_argument.name.as_str()))
				.with_item(&pre_versioning_argument.ty),
			post_versioning_state
				.push(ArgumentPathItem::new(post_versioning_argument.name.as_str()))
				.with_item(&post_versioning_argument.ty),
			recursion_guard,
		)
	}

	check_type(
		pre_versioning_state.with_item(&pre_versioning_state.item.output),
		post_versioning_state.with_item(&post_versioning_state.item.output),
		recursion_guard,
	);
}

fn check_type(
	pre_versioning_state: State<'_, UntrackedSymbol<TypeId>>,
	post_versioning_state: State<'_, UntrackedSymbol<TypeId>>,
	recursion_guard: &mut BTreeSet<(u32, u32)>,
) {
	if !recursion_guard.insert((pre_versioning_state.item.id, post_versioning_state.item.id)) {
		return;
	}

	let pre_versioning_type = pre_versioning_state
		.metadata
		.types
		.resolve(pre_versioning_state.item.id)
		.expect("can't happen on well-formed metadata; qed");
	let post_versioning_type = post_versioning_state
		.metadata
		.types
		.resolve(post_versioning_state.item.id)
		.expect("can't happen on well-formed metadata; qed");

	let pre_versioning_type_name = pre_versioning_type.path.ident();
	let post_versioning_type_name = post_versioning_type.path.ident();

	let pre_versioning_state = match &pre_versioning_type_name {
		Some(type_name) => pre_versioning_state.push(TypePathItem::new(type_name.as_str())),
		None => pre_versioning_state,
	}
	.with_item(&pre_versioning_type.type_def);
	let post_versioning_state = match &post_versioning_type_name {
		Some(type_name) => post_versioning_state.push(TypePathItem::new(type_name.as_str())),
		None => post_versioning_state,
	}
	.with_item(&post_versioning_type.type_def);

	match (pre_versioning_state.item, post_versioning_state.item) {
		(
			TypeDef::Composite(pre_versioning_type_def),
			TypeDef::Composite(post_versioning_type_def),
		) => check_type_composite(
			pre_versioning_state.with_item(pre_versioning_type_def),
			post_versioning_state.with_item(post_versioning_type_def),
			recursion_guard,
		),
		(TypeDef::Variant(pre_versioning_type_def), TypeDef::Variant(post_versioning_type_def)) => {
			check_type_variant(
				pre_versioning_state.with_item(pre_versioning_type_def),
				post_versioning_state.with_item(post_versioning_type_def),
				recursion_guard,
			)
		},
		(
			TypeDef::Sequence(pre_versioning_type_def),
			TypeDef::Sequence(post_versioning_type_def),
		) => check_type_sequence(
			pre_versioning_state.with_item(pre_versioning_type_def),
			post_versioning_state.with_item(post_versioning_type_def),
			recursion_guard,
		),
		(TypeDef::Array(pre_versioning_type_def), TypeDef::Array(post_versioning_type_def)) => {
			check_type_array(
				pre_versioning_state.with_item(pre_versioning_type_def),
				post_versioning_state.with_item(post_versioning_type_def),
				recursion_guard,
			)
		},
		(TypeDef::Tuple(pre_versioning_type_def), TypeDef::Tuple(post_versioning_type_def)) => {
			check_type_tuple(
				pre_versioning_state.with_item(pre_versioning_type_def),
				post_versioning_state.with_item(post_versioning_type_def),
				recursion_guard,
			)
		},
		(
			TypeDef::Primitive(pre_versioning_type_def),
			TypeDef::Primitive(post_versioning_type_def),
		) => check_type_primitive(
			pre_versioning_state.with_item(pre_versioning_type_def),
			post_versioning_state.with_item(post_versioning_type_def),
			recursion_guard,
		),
		(TypeDef::Compact(pre_versioning_type_def), TypeDef::Compact(post_versioning_type_def)) => {
			check_type_compact(
				pre_versioning_state.with_item(pre_versioning_type_def),
				post_versioning_state.with_item(post_versioning_type_def),
				recursion_guard,
			)
		},
		(
			TypeDef::BitSequence(pre_versioning_type_def),
			TypeDef::BitSequence(post_versioning_type_def),
		) => check_type_bit_sequence(
			pre_versioning_state.with_item(pre_versioning_type_def),
			post_versioning_state.with_item(post_versioning_type_def),
			recursion_guard,
		),
		_ => {
			panic!(
                "Encountered a type which has a different kind of type definition before and after \
                versioning. \n\
                Before: {:?}\n\
                After: {:?}\n\
                Pre Versioning Item Path: {:?}\n\
                Post Versioning Item Path: {:?}\n",
                pre_versioning_state.item,
                post_versioning_state.item,
                pre_versioning_state.path,
                post_versioning_state.path,
            )
		},
	}
}

fn check_type_composite(
	pre_versioning_state: State<'_, TypeDefComposite<PortableForm>>,
	post_versioning_state: State<'_, TypeDefComposite<PortableForm>>,
	recursion_guard: &mut BTreeSet<(u32, u32)>,
) {
	assert_eq!(
		pre_versioning_state.item.fields.len(),
		post_versioning_state.item.fields.len(),
		"Encountered a composite type which has a different number of fields before and after \
        versioning. \n\
        Before: {}\n\
        After: {}\n\
        Pre Versioning Item Path: {:?}\n\
        Post Versioning Item Path: {:?}\n",
		pre_versioning_state.item.fields.len(),
		post_versioning_state.item.fields.len(),
		pre_versioning_state.path,
		post_versioning_state.path,
	);

	for (index, (pre_versioning_field, post_versioning_field)) in pre_versioning_state
		.item
		.fields
		.iter()
		.zip(post_versioning_state.item.fields.iter())
		.enumerate()
	{
		check_type(
			pre_versioning_state
				.push(FieldPathItem::new(FieldIdentifier::new(index, pre_versioning_field)))
				.with_item(&pre_versioning_field.ty),
			post_versioning_state
				.push(FieldPathItem::new(FieldIdentifier::new(index, post_versioning_field)))
				.with_item(&post_versioning_field.ty),
			recursion_guard,
		)
	}
}

fn check_type_variant(
	pre_versioning_state: State<'_, TypeDefVariant<PortableForm>>,
	post_versioning_state: State<'_, TypeDefVariant<PortableForm>>,
	recursion_guard: &mut BTreeSet<(u32, u32)>,
) {
	assert_eq!(
		pre_versioning_state.item.variants.len(),
		post_versioning_state.item.variants.len(),
		"Encountered an enum type which has a different number of variants before and after \
        versioning. \n\
        Before: {}\n\
        After: {}\n\
        Pre Versioning Item Path: {:?}\n\
        Post Versioning Item Path: {:?}\n",
		pre_versioning_state.item.variants.len(),
		post_versioning_state.item.variants.len(),
		pre_versioning_state.path,
		post_versioning_state.path,
	);

	let mut pre_versioning_variants = pre_versioning_state.item.variants.iter().collect::<Vec<_>>();
	pre_versioning_variants.sort_by_key(|variant| variant.index);
	let mut post_versioning_variants =
		post_versioning_state.item.variants.iter().collect::<Vec<_>>();
	post_versioning_variants.sort_by_key(|variant| variant.index);

	for (pre_versioning_variant, post_versioning_variant) in
		pre_versioning_variants.into_iter().zip(post_versioning_variants)
	{
		let pre_versioning_state =
			pre_versioning_state.push(VariantPathItem::new(pre_versioning_variant.name.as_str()));
		let post_versioning_state =
			post_versioning_state.push(VariantPathItem::new(post_versioning_variant.name.as_str()));

		assert_eq!(
			pre_versioning_variant.index,
			post_versioning_variant.index,
			"Encountered a variant which has a different index before and after versioning. \n\
            Before: {}\n\
            After: {}\n\
            Pre Versioning Item Path: {:?}\n\
            Post Versioning Item Path: {:?}\n",
			pre_versioning_variant.index,
			post_versioning_variant.index,
			pre_versioning_state.path,
			post_versioning_state.path,
		);

		assert_eq!(
			pre_versioning_variant.fields.len(),
			post_versioning_variant.fields.len(),
			"Encountered a variant which has a different number of fields before and after \
            versioning. \n\
            Before: {}\n\
            After: {}\n\
            Pre Versioning Item Path: {:?}\n\
            Post Versioning Item Path: {:?}\n",
			pre_versioning_variant.fields.len(),
			post_versioning_variant.fields.len(),
			pre_versioning_state.path,
			post_versioning_state.path,
		);

		for (index, (pre_versioning_field, post_versioning_field)) in pre_versioning_variant
			.fields
			.iter()
			.zip(post_versioning_variant.fields.iter())
			.enumerate()
		{
			check_type(
				pre_versioning_state
					.push(FieldPathItem::new(FieldIdentifier::new(index, pre_versioning_field)))
					.with_item(&pre_versioning_field.ty),
				post_versioning_state
					.push(FieldPathItem::new(FieldIdentifier::new(index, post_versioning_field)))
					.with_item(&post_versioning_field.ty),
				recursion_guard,
			)
		}
	}
}

fn check_type_sequence(
	pre_versioning_state: State<'_, TypeDefSequence<PortableForm>>,
	post_versioning_state: State<'_, TypeDefSequence<PortableForm>>,
	recursion_guard: &mut BTreeSet<(u32, u32)>,
) {
	check_type(
		pre_versioning_state.with_item(&pre_versioning_state.item.type_param),
		post_versioning_state.with_item(&post_versioning_state.item.type_param),
		recursion_guard,
	);
}

fn check_type_array(
	pre_versioning_state: State<'_, TypeDefArray<PortableForm>>,
	post_versioning_state: State<'_, TypeDefArray<PortableForm>>,
	recursion_guard: &mut BTreeSet<(u32, u32)>,
) {
	assert_eq!(
		pre_versioning_state.item.len,
		post_versioning_state.item.len,
		"Encountered an array type which has a different length before and after versioning. \n\
        Before: {}\n\
        After: {}\n\
        Pre Versioning Item Path: {:?}\n\
        Post Versioning Item Path: {:?}\n",
		pre_versioning_state.item.len,
		post_versioning_state.item.len,
		pre_versioning_state.path,
		post_versioning_state.path,
	);

	check_type(
		pre_versioning_state.with_item(&pre_versioning_state.item.type_param),
		post_versioning_state.with_item(&post_versioning_state.item.type_param),
		recursion_guard,
	);
}

fn check_type_tuple(
	pre_versioning_state: State<'_, TypeDefTuple<PortableForm>>,
	post_versioning_state: State<'_, TypeDefTuple<PortableForm>>,
	recursion_guard: &mut BTreeSet<(u32, u32)>,
) {
	assert_eq!(
		pre_versioning_state.item.fields.len(),
		post_versioning_state.item.fields.len(),
		"Encountered a tuple type which has a different number of fields before and after \
        versioning. \n\
        Before: {}\n\
        After: {}\n\
        Pre Versioning Item Path: {:?}\n\
        Post Versioning Item Path: {:?}\n",
		pre_versioning_state.item.fields.len(),
		post_versioning_state.item.fields.len(),
		pre_versioning_state.path,
		post_versioning_state.path,
	);

	for (index, (pre_versioning_field, post_versioning_field)) in pre_versioning_state
		.item
		.fields
		.iter()
		.zip(post_versioning_state.item.fields.iter())
		.enumerate()
	{
		check_type(
			pre_versioning_state
				.push(FieldPathItem::new(FieldIdentifier::Index(index)))
				.with_item(pre_versioning_field),
			post_versioning_state
				.push(FieldPathItem::new(FieldIdentifier::Index(index)))
				.with_item(post_versioning_field),
			recursion_guard,
		)
	}
}

fn check_type_primitive(
	pre_versioning_state: State<'_, TypeDefPrimitive>,
	post_versioning_state: State<'_, TypeDefPrimitive>,
	_: &mut BTreeSet<(u32, u32)>,
) {
	assert_eq!(
		pre_versioning_state.item,
		post_versioning_state.item,
		"Encountered a primitive type which is different before and after versioning. \n\
        Before: {:?}\n\
        After: {:?}\n\
        Pre Versioning Item Path: {:?}\n\
        Post Versioning Item Path: {:?}\n",
		pre_versioning_state.item,
		post_versioning_state.item,
		pre_versioning_state.path,
		post_versioning_state.path,
	);
}

fn check_type_compact(
	pre_versioning_state: State<'_, TypeDefCompact<PortableForm>>,
	post_versioning_state: State<'_, TypeDefCompact<PortableForm>>,
	recursion_guard: &mut BTreeSet<(u32, u32)>,
) {
	check_type(
		pre_versioning_state.with_item(&pre_versioning_state.item.type_param),
		post_versioning_state.with_item(&post_versioning_state.item.type_param),
		recursion_guard,
	);
}

fn check_type_bit_sequence(
	pre_versioning_state: State<'_, TypeDefBitSequence<PortableForm>>,
	post_versioning_state: State<'_, TypeDefBitSequence<PortableForm>>,
	recursion_guard: &mut BTreeSet<(u32, u32)>,
) {
	check_type(
		pre_versioning_state.with_item(&pre_versioning_state.item.bit_store_type),
		post_versioning_state.with_item(&post_versioning_state.item.bit_store_type),
		recursion_guard,
	);
	check_type(
		pre_versioning_state.with_item(&pre_versioning_state.item.bit_order_type),
		post_versioning_state.with_item(&post_versioning_state.item.bit_order_type),
		recursion_guard,
	);
}

struct State<'a, T> {
	metadata: &'a RuntimeMetadataV16,
	path: PathItem<'a>,
	item: &'a T,
}

impl<'a, T> State<'a, T> {
	fn push(&self, item: impl Into<PathItem<'a>>) -> Self {
		let mut path = self.path.clone();
		path.push(item);
		Self { metadata: self.metadata, path, item: self.item }
	}

	fn with_item<B>(&self, item: &'a B) -> State<'a, B> {
		State { metadata: self.metadata, path: self.path.clone(), item }
	}
}

macro_rules! define_path_item {
    (
        $(
            $ident: ident { $($field_ident: ident: $field_ty: ty),* $(,)? }
        ),* $(,)?
    ) => {
        #[allow(clippy::enum_variant_names)]
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        enum PathItem<'a> {
            $(
                $ident(alloc::boxed::Box<$ident<'a>>)
            ),*
        }

        impl<'a> PathItem<'a> {
            fn push(&mut self, item: impl Into<Self>) {
                match self {
                    $(
                        Self::$ident(this) => this.push(item)
                    ),*
                }
            }
        }

        $(
            #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
            struct $ident<'a> {
                $(
                    $field_ident: $field_ty,
                )*
                inner: Option<PathItem<'a>>
            }

            impl<'a> $ident<'a> {
                pub fn new($($field_ident: $field_ty),*) -> Self {
                    Self {
                        $(
                            $field_ident,
                        )*
                        inner: None
                    }
                }

                fn push(&mut self, item: impl Into<PathItem<'a>>) {
                    match self.inner {
                        Some(ref mut inner) => inner.push(item),
                        ref mut inner @ None => *inner = Some(item.into())
                    }
                }
            }

            impl<'a> From<$ident<'a>> for PathItem<'a> {
                fn from(value: $ident<'a>) -> Self {
                    Self::$ident(alloc::boxed::Box::new(value))
                }
            }
        )*
    };
}

define_path_item! {
	FunctionPathItem { function_name: &'a str },
	ArgumentPathItem { argument_name: &'a str },
	TypePathItem { type_name: &'a str },
	FieldPathItem { field_identifier: FieldIdentifier<'a> },
	VariantPathItem { variant_name: &'a str },
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum FieldIdentifier<'a> {
	Index(usize),
	Name(&'a str),
}

impl<'a> FieldIdentifier<'a> {
	fn new(index: usize, field: &'a Field<PortableForm>) -> Self {
		match &field.name {
			Some(name) => Self::Name(name.as_str()),
			None => Self::Index(index),
		}
	}
}

fn metadata() -> (RuntimeMetadataV16, RuntimeApiMetadata<PortableForm>) {
	let opaque_metadata = TestExternalities::default().execute_with(|| {
		revive_dev_runtime::Runtime::metadata_at_version(16)
			.expect("the runtime serves v16 metadata")
	});
	decode_metadata(&opaque_metadata)
}

fn pre_versioning_metadata() -> (RuntimeMetadataV16, RuntimeApiMetadata<PortableForm>) {
	decode_metadata(include_bytes!("./pre-versioning-revive-metadata.scale"))
}

fn type_id_of(
	metadata: &RuntimeMetadataV16,
	predicate: impl Fn(&Type<PortableForm>) -> bool,
) -> u32 {
	metadata
		.types
		.types
		.iter()
		.find(|ty| predicate(&ty.ty))
		.expect("the metadata contains the requested type")
		.id
}

fn decode_metadata(bytes: &[u8]) -> (RuntimeMetadataV16, RuntimeApiMetadata<PortableForm>) {
	let RuntimeMetadata::V16(metadata) = RuntimeMetadataPrefixed::decode(&mut &bytes[..])
		.expect("the runtime metadata decodes")
		.1
	else {
		panic!("the runtime must serve v16 metadata")
	};

	let runtime_api_metadata = metadata
		.apis
		.iter()
		.find(|api| api.name == "ReviveApi")
		.expect("the runtime exposes the ReviveApi")
		.clone();

	(metadata, runtime_api_metadata)
}

fn version_declarations() -> ReviveRuntimeApiVersionDeclarations {
	TestExternalities::default().execute_with(|| {
		<revive_dev_runtime::Runtime as ReviveApi<_, _, _, _, _, _>>::version_declarations()
	})
}

fn snake_to_camel(string: &str) -> String {
	fn internal(
		mut chars: core::iter::Peekable<impl Iterator<Item = char>>,
		mut acc: String,
	) -> String {
		let char1 = chars.next();
		let char2 = chars.peek().copied();

		match (char1, char2) {
			(Some('_'), Some(char2)) => {
				let _ = chars.next().expect("peek succeeded, next must succeed; qed");
				let char = char2.to_ascii_uppercase();
				acc.push(char);
				internal(chars, acc)
			},
			(Some(char1), Some(_) | None) => {
				let char1 = if acc.is_empty() { char1.to_ascii_uppercase() } else { char1 };
				acc.push(char1);
				internal(chars, acc)
			},
			(None, None) => acc,
			(None, Some(_)) => {
				unreachable!(
					"First pull from the iterator can't fail while subsequent one succeeds"
				)
			},
		}
	}

	internal(string.chars().peekable(), String::new())
}

fn extract_ok_type_from_result<'a>(
	ty: &Type<PortableForm>,
	metadata: &'a RuntimeMetadataV16,
) -> Option<&'a Type<PortableForm>> {
	let Some("Result") = ty.path.ident().as_deref() else { return None };

	let TypeDef::Variant(ref variants) = ty.type_def else {
		return None;
	};

	let [variant1, variant2] = variants.variants.as_slice() else {
		return None;
	};

	if variant1.name != "Ok" || variant1.index != 0 || variant2.name != "Err" || variant2.index != 1
	{
		return None;
	};

	let [field @ Field { name: None, .. }] = variant1.fields.as_slice() else { return None };

	Some(
		metadata
			.types
			.resolve(field.ty.id)
			.expect("impossible on well-formed metadata; qed"),
	)
}
