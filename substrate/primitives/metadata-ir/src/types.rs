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

use codec::{Compact, Decode, Encode};
use derive_where::derive_where;
use scale_info::{
	form::{Form, MetaForm, PortableForm},
	prelude::{collections::BTreeMap, vec::Vec},
	IntoPortable, Registry,
};

/// The intermediate representation for the runtime metadata.
/// Contains the needed context that allows conversion to multiple metadata versions.
///
/// # Note
///
/// Further fields could be added or removed to ensure proper conversion.
/// When the IR does not contain enough information to generate a specific version
/// of the runtime metadata an appropriate default value is used (ie, empty vector).
pub struct MetadataIR<T: Form = MetaForm> {
	/// Pallet metadata.
	pub pallets: Vec<PalletMetadataIR<T>>,
	/// Metadata of the extrinsic.
	pub extrinsic: ExtrinsicMetadataIR<T>,
	/// The type of the `Runtime`.
	pub ty: T::Type,
	/// Metadata of the Runtime API.
	pub apis: Vec<RuntimeApiMetadataIR<T>>,
	/// The outer enums types as found in the runtime.
	pub outer_enums: OuterEnumsIR<T>,
}

/// Metadata of a runtime trait.
#[derive(Encode)]
#[derive_where(Clone, PartialEq, Eq, Debug;)]
pub struct RuntimeApiMetadataIR<T: Form = MetaForm> {
	/// Trait name.
	pub name: T::String,
	/// Trait methods.
	pub methods: Vec<RuntimeApiMethodMetadataIR<T>>,
	/// Trait documentation.
	pub docs: Vec<T::String>,
	/// Deprecation info.
	pub deprecation_info: ItemDeprecationInfoIR<T>,
	/// Runtime API version.
	pub version: Compact<u32>,
}

impl IntoPortable for RuntimeApiMetadataIR {
	type Output = RuntimeApiMetadataIR<PortableForm>;

	fn into_portable(self, registry: &mut Registry) -> Self::Output {
		RuntimeApiMetadataIR {
			name: self.name.into_portable(registry),
			methods: registry.map_into_portable(self.methods),
			docs: registry.map_into_portable(self.docs),
			deprecation_info: self.deprecation_info.into_portable(registry),
			version: self.version,
		}
	}
}

/// Metadata of a runtime method.
#[derive(Encode)]
#[derive_where(Clone, PartialEq, Eq, Debug;)]
pub struct RuntimeApiMethodMetadataIR<T: Form = MetaForm> {
	/// Method name.
	pub name: T::String,
	/// Method parameters.
	pub inputs: Vec<RuntimeApiMethodParamMetadataIR<T>>,
	/// Method output.
	pub output: T::Type,
	/// Method documentation.
	pub docs: Vec<T::String>,
	/// Deprecation info
	pub deprecation_info: ItemDeprecationInfoIR<T>,
}

impl IntoPortable for RuntimeApiMethodMetadataIR {
	type Output = RuntimeApiMethodMetadataIR<PortableForm>;

	fn into_portable(self, registry: &mut Registry) -> Self::Output {
		RuntimeApiMethodMetadataIR {
			name: self.name.into_portable(registry),
			inputs: registry.map_into_portable(self.inputs),
			output: registry.register_type(&self.output),
			docs: registry.map_into_portable(self.docs),
			deprecation_info: self.deprecation_info.into_portable(registry),
		}
	}
}

/// Metadata of a runtime method parameter.
#[derive(Encode)]
#[derive_where(Clone, PartialEq, Eq, Debug;)]
pub struct RuntimeApiMethodParamMetadataIR<T: Form = MetaForm> {
	/// Parameter name.
	pub name: T::String,
	/// Parameter type.
	pub ty: T::Type,
}

impl IntoPortable for RuntimeApiMethodParamMetadataIR {
	type Output = RuntimeApiMethodParamMetadataIR<PortableForm>;

	fn into_portable(self, registry: &mut Registry) -> Self::Output {
		RuntimeApiMethodParamMetadataIR {
			name: self.name.into_portable(registry),
			ty: registry.register_type(&self.ty),
		}
	}
}

/// Metadata of a pallet view function method.
#[derive(Encode, Decode)]
#[derive_where(Clone, PartialEq, Eq, Debug;)]
pub struct PalletViewFunctionMetadataIR<T: Form = MetaForm> {
	/// Method name.
	pub name: T::String,
	/// Method id.
	pub id: [u8; 32],
	/// Method parameters.
	pub inputs: Vec<PalletViewFunctionParamMetadataIR<T>>,
	/// Method output.
	pub output: T::Type,
	/// Method documentation.
	pub docs: Vec<T::String>,
	/// Deprecation info
	pub deprecation_info: ItemDeprecationInfoIR<T>,
}

impl IntoPortable for PalletViewFunctionMetadataIR {
	type Output = PalletViewFunctionMetadataIR<PortableForm>;

	fn into_portable(self, registry: &mut Registry) -> Self::Output {
		PalletViewFunctionMetadataIR {
			name: self.name.into_portable(registry),
			id: self.id,
			inputs: registry.map_into_portable(self.inputs),
			output: registry.register_type(&self.output),
			docs: registry.map_into_portable(self.docs),
			deprecation_info: self.deprecation_info.into_portable(registry),
		}
	}
}

/// Metadata of a pallet view function method argument.
#[derive(Encode, Decode)]
#[derive_where(Clone, PartialEq, Eq, Debug;)]
pub struct PalletViewFunctionParamMetadataIR<T: Form = MetaForm> {
	/// Parameter name.
	pub name: T::String,
	/// Parameter type.
	pub ty: T::Type,
}

impl IntoPortable for PalletViewFunctionParamMetadataIR {
	type Output = PalletViewFunctionParamMetadataIR<PortableForm>;

	fn into_portable(self, registry: &mut Registry) -> Self::Output {
		PalletViewFunctionParamMetadataIR {
			name: self.name.into_portable(registry),
			ty: registry.register_type(&self.ty),
		}
	}
}

/// The intermediate representation for a pallet metadata.
#[derive(Encode)]
#[derive_where(Clone, PartialEq, Eq, Debug;)]
pub struct PalletMetadataIR<T: Form = MetaForm> {
	/// Pallet name.
	pub name: T::String,
	/// Pallet storage metadata.
	pub storage: Option<PalletStorageMetadataIR<T>>,
	/// Pallet calls metadata.
	pub calls: Option<PalletCallMetadataIR<T>>,
	/// Pallet view functions metadata.
	pub view_functions: Vec<PalletViewFunctionMetadataIR<T>>,
	/// Pallet event metadata.
	pub event: Option<PalletEventMetadataIR<T>>,
	/// Pallet constants metadata.
	pub constants: Vec<PalletConstantMetadataIR<T>>,
	/// Pallet error metadata.
	pub error: Option<PalletErrorMetadataIR<T>>,
	/// Config's trait associated types.
	pub associated_types: Vec<PalletAssociatedTypeMetadataIR<T>>,
	/// Define the index of the pallet, this index will be used for the encoding of pallet event,
	/// call and origin variants.
	pub index: u8,
	/// Pallet documentation.
	pub docs: Vec<T::String>,
	/// Deprecation info
	pub deprecation_info: ItemDeprecationInfoIR<T>,
}

impl IntoPortable for PalletMetadataIR {
	type Output = PalletMetadataIR<PortableForm>;

	fn into_portable(self, registry: &mut Registry) -> Self::Output {
		PalletMetadataIR {
			name: self.name.into_portable(registry),
			storage: self.storage.map(|storage| storage.into_portable(registry)),
			calls: self.calls.map(|calls| calls.into_portable(registry)),
			view_functions: self
				.view_functions
				.into_iter()
				.map(|view_functions| view_functions.into_portable(registry))
				.collect(),
			event: self.event.map(|event| event.into_portable(registry)),
			constants: registry.map_into_portable(self.constants),
			error: self.error.map(|error| error.into_portable(registry)),
			associated_types: registry.map_into_portable(self.associated_types),
			index: self.index,
			docs: registry.map_into_portable(self.docs),
			deprecation_info: self.deprecation_info.into_portable(registry),
		}
	}
}

/// Metadata of the extrinsic used by the runtime.
#[derive(Encode)]
#[derive_where(Clone, PartialEq, Eq, Debug;)]
pub struct ExtrinsicMetadataIR<T: Form = MetaForm> {
	/// The type of the extrinsic.
	///
	/// Note: Field used for metadata V14 only.
	pub ty: T::Type,
	/// Extrinsic versions.
	pub versions: Vec<u8>,
	/// The type of the address that signs the extrinsic
	pub address_ty: T::Type,
	/// The type of the outermost Call enum.
	pub call_ty: T::Type,
	/// The type of the extrinsic's signature.
	pub signature_ty: T::Type,
	/// The type of the outermost Extra/Extensions enum.
	// TODO: metadata-v16: remove this, the `implicit` type can be found in `extensions::implicit`.
	pub extra_ty: T::Type,
	/// The transaction extensions for each version as a list of index in reference to items in
	/// `extensions_in_versions` field.
	pub extensions_by_version: BTreeMap<u8, Vec<u32>>,
	/// The list of all transaction extensions used in `extensions_by_version`.
	pub extensions_in_versions: Vec<TransactionExtensionMetadataIR<T>>,
}

impl<T: Form> ExtrinsicMetadataIR<T> {
	/// The transaction extensions in the order they appear in the extrinsic for the version 0 if
	/// defined.
	pub fn extensions_v0(&self) -> Option<Vec<TransactionExtensionMetadataIR<T>>> {
		self.extensions_by_version.get(&0).map(|indices| {
			indices
				.iter()
				.map(|i| self.extensions_in_versions[*i as usize].clone())
				.collect()
		})
	}
}

impl IntoPortable for ExtrinsicMetadataIR {
	type Output = ExtrinsicMetadataIR<PortableForm>;

	fn into_portable(self, registry: &mut Registry) -> Self::Output {
		ExtrinsicMetadataIR {
			ty: registry.register_type(&self.ty),
			versions: self.versions,
			address_ty: registry.register_type(&self.address_ty),
			call_ty: registry.register_type(&self.call_ty),
			signature_ty: registry.register_type(&self.signature_ty),
			extra_ty: registry.register_type(&self.extra_ty),
			extensions_by_version: self.extensions_by_version,
			extensions_in_versions: registry.map_into_portable(self.extensions_in_versions),
		}
	}
}

/// Metadata of a pallet's associated type.
#[derive(Encode)]
#[derive_where(Clone, PartialEq, Eq, Debug;)]
pub struct PalletAssociatedTypeMetadataIR<T: Form = MetaForm> {
	/// The name of the associated type.
	pub name: T::String,
	/// The type of the associated type.
	pub ty: T::Type,
	/// The documentation of the associated type.
	pub docs: Vec<T::String>,
}

impl IntoPortable for PalletAssociatedTypeMetadataIR {
	type Output = PalletAssociatedTypeMetadataIR<PortableForm>;

	fn into_portable(self, registry: &mut Registry) -> Self::Output {
		PalletAssociatedTypeMetadataIR {
			name: self.name.into_portable(registry),
			ty: registry.register_type(&self.ty),
			docs: registry.map_into_portable(self.docs),
		}
	}
}

/// Metadata of an extrinsic's signed extension.
#[derive(Encode)]
#[derive_where(Clone, PartialEq, Eq, Debug;)]
pub struct TransactionExtensionMetadataIR<T: Form = MetaForm> {
	/// The unique signed extension identifier, which may be different from the type name.
	pub identifier: T::String,
	/// The type of the signed extension, with the data to be included in the extrinsic.
	pub ty: T::Type,
	/// The type of the implicit data, with the data to be included in the signed payload.
	pub implicit: T::Type,
}

impl IntoPortable for TransactionExtensionMetadataIR {
	type Output = TransactionExtensionMetadataIR<PortableForm>;

	fn into_portable(self, registry: &mut Registry) -> Self::Output {
		TransactionExtensionMetadataIR {
			identifier: self.identifier.into_portable(registry),
			ty: registry.register_type(&self.ty),
			implicit: registry.register_type(&self.implicit),
		}
	}
}

/// All metadata of the pallet's storage.
///
/// The common prefix used by all storage entries.
#[derive(Encode)]
#[derive_where(Clone, PartialEq, Eq, Debug;)]
pub struct PalletStorageMetadataIR<T: Form = MetaForm> {
	/// The common prefix used by all storage entries.
	pub prefix: T::String,
	/// Metadata for all storage entries.
	pub entries: Vec<StorageEntryMetadataIR<T>>,
}

impl IntoPortable for PalletStorageMetadataIR {
	type Output = PalletStorageMetadataIR<PortableForm>;

	fn into_portable(self, registry: &mut Registry) -> Self::Output {
		PalletStorageMetadataIR {
			prefix: self.prefix.into_portable(registry),
			entries: registry.map_into_portable(self.entries),
		}
	}
}

/// Metadata about one storage entry.
#[derive(Encode)]
#[derive_where(Clone, PartialEq, Eq, Debug;)]
pub struct StorageEntryMetadataIR<T: Form = MetaForm> {
	/// Variable name of the storage entry.
	pub name: T::String,
	/// An `Option` modifier of that storage entry.
	pub modifier: StorageEntryModifierIR,
	/// Type of the value stored in the entry.
	pub ty: StorageEntryTypeIR<T>,
	/// Default value (SCALE encoded).
	pub default: Vec<u8>,
	/// Storage entry documentation.
	pub docs: Vec<T::String>,
	/// Deprecation info
	pub deprecation_info: ItemDeprecationInfoIR<T>,
}

impl IntoPortable for StorageEntryMetadataIR {
	type Output = StorageEntryMetadataIR<PortableForm>;

	fn into_portable(self, registry: &mut Registry) -> Self::Output {
		StorageEntryMetadataIR {
			name: self.name.into_portable(registry),
			modifier: self.modifier,
			ty: self.ty.into_portable(registry),
			default: self.default,
			docs: registry.map_into_portable(self.docs),
			deprecation_info: self.deprecation_info.into_portable(registry),
		}
	}
}

/// A storage entry modifier indicates how a storage entry is returned when fetched and what the
/// value will be if the key is not present. Specifically this refers to the "return type" when
/// fetching a storage entry, and what the value will be if the key is not present.
///
/// `Optional` means you should expect an `Option<T>`, with `None` returned if the key is not
/// present. `Default` means you should expect a `T` with the default value of default if the key is
/// not present.
#[derive(Clone, PartialEq, Eq, Encode, Debug)]
pub enum StorageEntryModifierIR {
	/// The storage entry returns an `Option<T>`, with `None` if the key is not present.
	Optional,
	/// The storage entry returns `T::Default` if the key is not present.
	Default,
}

/// Hasher used by storage maps
#[derive(Clone, PartialEq, Eq, Encode, Debug)]
pub enum StorageHasherIR {
	/// 128-bit Blake2 hash.
	Blake2_128,
	/// 256-bit Blake2 hash.
	Blake2_256,
	/// Multiple 128-bit Blake2 hashes concatenated.
	Blake2_128Concat,
	/// 128-bit XX hash.
	Twox128,
	/// 256-bit XX hash.
	Twox256,
	/// Multiple 64-bit XX hashes concatenated.
	Twox64Concat,
	/// Identity hashing (no hashing).
	Identity,
}

/// A type of storage value.
#[derive(Encode)]
#[derive_where(Clone, PartialEq, Eq, Debug;)]
pub enum StorageEntryTypeIR<T: Form = MetaForm> {
	/// Plain storage entry (just the value).
	Plain(T::Type),
	/// A storage map.
	Map {
		/// One or more hashers, should be one hasher per key element.
		hashers: Vec<StorageHasherIR>,
		/// The type of the key, can be a tuple with elements for each of the hashers.
		key: T::Type,
		/// The type of the value.
		value: T::Type,
	},
}

impl IntoPortable for StorageEntryTypeIR {
	type Output = StorageEntryTypeIR<PortableForm>;

	fn into_portable(self, registry: &mut Registry) -> Self::Output {
		match self {
			Self::Plain(plain) => StorageEntryTypeIR::Plain(registry.register_type(&plain)),
			Self::Map { hashers, key, value } => StorageEntryTypeIR::Map {
				hashers,
				key: registry.register_type(&key),
				value: registry.register_type(&value),
			},
		}
	}
}

/// Metadata for all calls in a pallet
#[derive(Encode)]
#[derive_where(Clone, PartialEq, Eq, Debug;)]
pub struct PalletCallMetadataIR<T: Form = MetaForm> {
	/// The corresponding enum type for the pallet call.
	pub ty: T::Type,
	/// Deprecation status of the pallet call
	pub deprecation_info: EnumDeprecationInfoIR<T>,
}

impl IntoPortable for PalletCallMetadataIR {
	type Output = PalletCallMetadataIR<PortableForm>;

	fn into_portable(self, registry: &mut Registry) -> Self::Output {
		PalletCallMetadataIR {
			ty: registry.register_type(&self.ty),
			deprecation_info: self.deprecation_info.into_portable(registry),
		}
	}
}

/// Metadata about the pallet Event type.
#[derive(Encode)]
#[derive_where(Clone, PartialEq, Eq, Debug;)]
pub struct PalletEventMetadataIR<T: Form = MetaForm> {
	/// The Event type.
	pub ty: T::Type,
	/// Deprecation info of the event
	pub deprecation_info: EnumDeprecationInfoIR<T>,
}

impl IntoPortable for PalletEventMetadataIR {
	type Output = PalletEventMetadataIR<PortableForm>;

	fn into_portable(self, registry: &mut Registry) -> Self::Output {
		PalletEventMetadataIR {
			ty: registry.register_type(&self.ty),
			deprecation_info: self.deprecation_info.into_portable(registry),
		}
	}
}

/// Metadata about one pallet constant.
#[derive(Encode)]
#[derive_where(Clone, PartialEq, Eq, Debug;)]
pub struct PalletConstantMetadataIR<T: Form = MetaForm> {
	/// Name of the pallet constant.
	pub name: T::String,
	/// Type of the pallet constant.
	pub ty: T::Type,
	/// Value stored in the constant (SCALE encoded).
	pub value: Vec<u8>,
	/// Documentation of the constant.
	pub docs: Vec<T::String>,
	/// Deprecation info
	pub deprecation_info: ItemDeprecationInfoIR<T>,
}

impl IntoPortable for PalletConstantMetadataIR {
	type Output = PalletConstantMetadataIR<PortableForm>;

	fn into_portable(self, registry: &mut Registry) -> Self::Output {
		PalletConstantMetadataIR {
			name: self.name.into_portable(registry),
			ty: registry.register_type(&self.ty),
			value: self.value,
			docs: registry.map_into_portable(self.docs),
			deprecation_info: self.deprecation_info.into_portable(registry),
		}
	}
}

/// Metadata about a pallet error.
#[derive(Encode)]
#[derive_where(Clone, PartialEq, Eq, Debug;)]
pub struct PalletErrorMetadataIR<T: Form = MetaForm> {
	/// The error type information.
	pub ty: T::Type,
	/// Deprecation info
	pub deprecation_info: EnumDeprecationInfoIR<T>,
}

impl IntoPortable for PalletErrorMetadataIR {
	type Output = PalletErrorMetadataIR<PortableForm>;

	fn into_portable(self, registry: &mut Registry) -> Self::Output {
		PalletErrorMetadataIR {
			ty: registry.register_type(&self.ty),
			deprecation_info: self.deprecation_info.into_portable(registry),
		}
	}
}

/// The type of the outer enums.
#[derive(Encode)]
#[derive_where(Clone, PartialEq, Eq, Debug;)]
pub struct OuterEnumsIR<T: Form = MetaForm> {
	/// The type of the outer `RuntimeCall` enum.
	pub call_enum_ty: T::Type,
	/// The type of the outer `RuntimeEvent` enum.
	pub event_enum_ty: T::Type,
	/// The module error type of the
	/// [`DispatchError::Module`](https://docs.rs/sp-runtime/24.0.0/sp_runtime/enum.DispatchError.html#variant.Module) variant.
	///
	/// The `Module` variant will be 5 scale encoded bytes which are normally decoded into
	/// an `{ index: u8, error: [u8; 4] }` struct. This type ID points to an enum type which
	/// instead interprets the first `index` byte as a pallet variant, and the remaining `error`
	/// bytes as the appropriate `pallet::Error` type. It is an equally valid way to decode the
	/// error bytes, and can be more informative.
	///
	/// # Note
	///
	/// - This type cannot be used directly to decode `sp_runtime::DispatchError` from the chain.
	///   It provides just the information needed to decode `sp_runtime::DispatchError::Module`.
	/// - Decoding the 5 error bytes into this type will not always lead to all of the bytes being
	///   consumed; many error types do not require all of the bytes to represent them fully.
	pub error_enum_ty: T::Type,
}

impl IntoPortable for OuterEnumsIR {
	type Output = OuterEnumsIR<PortableForm>;

	fn into_portable(self, registry: &mut Registry) -> Self::Output {
		OuterEnumsIR {
			call_enum_ty: registry.register_type(&self.call_enum_ty),
			event_enum_ty: registry.register_type(&self.event_enum_ty),
			error_enum_ty: registry.register_type(&self.error_enum_ty),
		}
	}
}

/// Deprecation information for generic items.
#[derive(Encode)]
#[derive_where(Clone, PartialEq, Eq, Debug;)]
pub enum ItemDeprecationInfoIR<T: Form = MetaForm> {
	/// Item is not deprecated.
	NotDeprecated,
	/// Item is fully deprecated without a note.
	DeprecatedWithoutNote,
	/// Item is fully deprecated with a note and an optional `since` field.
	Deprecated {
		/// Note explaining the deprecation
		note: T::String,
		/// Optional value for noting the version when the deprecation occurred.
		since: Option<T::String>,
	},
}

impl IntoPortable for ItemDeprecationInfoIR {
	type Output = ItemDeprecationInfoIR<PortableForm>;

	fn into_portable(self, registry: &mut Registry) -> Self::Output {
		match self {
			Self::NotDeprecated => ItemDeprecationInfoIR::NotDeprecated,
			Self::DeprecatedWithoutNote => ItemDeprecationInfoIR::DeprecatedWithoutNote,
			Self::Deprecated { note, since } => {
				let note = note.into_portable(registry);
				let since = since.map(|x| x.into_portable(registry));
				ItemDeprecationInfoIR::Deprecated { note, since }
			},
		}
	}
}

/// Deprecation information for enums in which specific variants can be deprecated.
/// If the map is empty, then nothing is deprecated.
#[derive(Encode)]
#[derive_where(Clone, PartialEq, Eq, Debug;)]
pub struct EnumDeprecationInfoIR<T: Form = MetaForm>(pub BTreeMap<u8, VariantDeprecationInfoIR<T>>);

impl<T: Form> EnumDeprecationInfoIR<T> {
	/// Construct an instance in which nothing is marked for deprecation.
	pub fn nothing_deprecated() -> Self {
		Self(BTreeMap::new())
	}

	/// Are any variants deprecated?
	pub fn has_deprecated_variants(&self) -> bool {
		!self.0.is_empty()
	}

	/// Is a specific variant deprecated?
	pub fn is_variant_deprecated(&self, variant_index: u8) -> bool {
		self.0.contains_key(&variant_index)
	}
}

impl IntoPortable for EnumDeprecationInfoIR {
	type Output = EnumDeprecationInfoIR<PortableForm>;

	fn into_portable(self, registry: &mut Registry) -> Self::Output {
		let entries = self.0.into_iter().map(|(k, entry)| (k, entry.into_portable(registry)));
		EnumDeprecationInfoIR(entries.collect())
	}
}

/// Deprecation information for an item or variant in the metadata.
#[derive(Encode)]
#[derive_where(Clone, PartialEq, Eq, Debug;)]
pub enum VariantDeprecationInfoIR<T: Form = MetaForm> {
	/// Variant is deprecated without a note.
	DeprecatedWithoutNote,
	/// Variant is deprecated with a note and an optional `since` field.
	Deprecated {
		/// Note explaining the deprecation
		note: T::String,
		/// Optional value for noting the version when the deprecation occurred.
		since: Option<T::String>,
	},
}

impl<T: Form> Into<ItemDeprecationInfoIR<T>> for VariantDeprecationInfoIR<T> {
	fn into(self) -> ItemDeprecationInfoIR<T> {
		match self {
			Self::Deprecated { note, since } => ItemDeprecationInfoIR::Deprecated { note, since },
			Self::DeprecatedWithoutNote => ItemDeprecationInfoIR::DeprecatedWithoutNote,
		}
	}
}

impl IntoPortable for VariantDeprecationInfoIR {
	type Output = VariantDeprecationInfoIR<PortableForm>;

	fn into_portable(self, registry: &mut Registry) -> Self::Output {
		match self {
			Self::Deprecated { note, since } => {
				let note = note.into_portable(registry);
				let since = since.map(|x| x.into_portable(registry));
				VariantDeprecationInfoIR::Deprecated { note, since }
			},
			Self::DeprecatedWithoutNote => VariantDeprecationInfoIR::DeprecatedWithoutNote,
		}
	}
}
