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

//! The traits and primitive types for versioned transaction extension pipelines.

use crate::{
	scale_info::StaticTypeInfo,
	traits::{
		DispatchInfoOf, DispatchOriginOf, Dispatchable, PostDispatchInfoOf,
		TransactionExtensionMetadata,
	},
	transaction_validity::{TransactionSource, TransactionValidityError, ValidTransaction},
};
use alloc::{collections::BTreeMap, vec::Vec};
use codec::Encode;
use core::fmt::Debug;
use sp_weights::Weight;

mod at_vers;
mod invalid;
mod multi;
mod variant;
pub use at_vers::PipelineAtVers;
pub use invalid::InvalidVersion;
pub use multi::MultiVersion;
pub use variant::ExtensionVariant;

/// The version for an instance of a versioned transaction extension pipeline.
///
/// This trait is part of [`Pipeline`]. It is defined independently to allow implementation to
/// rely only on it without bounding the whole trait [`Pipeline`].
///
/// (type name is short for versioned (Vers) Pipeline version (Version)).
pub trait PipelineVersion {
	/// Return the version for the given versioned transaction extension pipeline.
	fn version(&self) -> u8;
}

/// A versioned transaction extension pipeline.
///
/// This defines multiple version of a transaction extensions pipeline.
///
/// (type name is short for versioned (Vers) Pipeline).
pub trait Pipeline<Call: Dispatchable>:
	Encode
	+ DecodeWithVersion
	+ DecodeWithVersionWithMemTracking
	+ Debug
	+ StaticTypeInfo
	+ Send
	+ Sync
	+ Clone
	+ PipelineVersion
{
	/// Build the metadata for the versioned transaction extension pipeline.
	fn build_metadata(builder: &mut PipelineMetadataBuilder);

	/// Validate a transaction.
	fn validate_only(
		&self,
		origin: DispatchOriginOf<Call>,
		call: &Call,
		info: &DispatchInfoOf<Call>,
		len: usize,
		source: TransactionSource,
	) -> Result<ValidTransaction, TransactionValidityError>;

	/// Dispatch a transaction.
	fn dispatch_transaction(
		self,
		origin: DispatchOriginOf<Call>,
		call: Call,
		info: &DispatchInfoOf<Call>,
		len: usize,
	) -> crate::ApplyExtrinsicResultWithInfo<PostDispatchInfoOf<Call>>;

	/// Return the pre dispatch weight for the given versioned transaction extension pipeline and
	/// call.
	fn weight(&self, call: &Call) -> Weight;
}

/// A type that can be decoded from a specific version and a [`codec::Input`].
pub trait DecodeWithVersion: Sized {
	/// Decode the type from the given version and input.
	fn decode_with_version<I: codec::Input>(
		extension_version: u8,
		input: &mut I,
	) -> Result<Self, codec::Error>;
}

/// A type implements [`DecodeWithVersion`] where inner decoding is implementing
/// [`codec::DecodeWithMemTracking`].
pub trait DecodeWithVersionWithMemTracking: DecodeWithVersion {}

/// A type to build the metadata for the versioned transaction extension pipeline.
pub struct PipelineMetadataBuilder {
	/// The transaction extension pipeline by version and its list of items as vec of index into
	/// the other field `in_versions`.
	pub by_version: BTreeMap<u8, Vec<u32>>,
	/// The list of all transaction extension item used.
	pub in_versions: Vec<TransactionExtensionMetadata>,
}

impl PipelineMetadataBuilder {
	/// Create a new empty metadata builder.
	pub fn new() -> Self {
		Self { by_version: BTreeMap::new(), in_versions: Vec::new() }
	}

	/// A function to add a versioned transaction extension pipeline to the metadata builder.
	pub fn push_versioned_extension(
		&mut self,
		ext_version: u8,
		ext_items: Vec<TransactionExtensionMetadata>,
	) {
		if self.by_version.contains_key(&ext_version) {
			log::warn!("Duplicate definition for transaction extension version: {}", ext_version);
			debug_assert!(
				false,
				"Duplicate definition for transaction extension version: {ext_version}",
			);
			return;
		}

		let mut ext_item_indices = Vec::with_capacity(ext_items.len());
		for ext_item in ext_items {
			let ext_item_index =
				match self.in_versions.iter().position(|ext| ext.identifier == ext_item.identifier)
				{
					Some(index) => index,
					None => {
						self.in_versions.push(ext_item);
						self.in_versions.len() - 1
					},
				};
			ext_item_indices.push(ext_item_index as u32);
		}
		self.by_version.insert(ext_version, ext_item_indices);
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use scale_info::meta_type;

	#[test]
	fn test_metadata_builder() {
		let mut builder = PipelineMetadataBuilder::new();

		let ext_item_a = TransactionExtensionMetadata {
			identifier: "ExtensionA",
			ty: meta_type::<u64>(),
			implicit: meta_type::<(u32, u8)>(),
		};
		let ext_item_b = TransactionExtensionMetadata {
			identifier: "ExtensionB",
			ty: meta_type::<bool>(),
			implicit: meta_type::<String>(),
		};

		// Push version 1 with ExtensionA.
		builder.push_versioned_extension(1, vec![ext_item_a.clone()]);
		// Push version 2 with ExtensionB, then ExtensionA again.
		builder.push_versioned_extension(2, vec![ext_item_b.clone(), ext_item_a.clone()]);

		// We now expect:
		// - `by_version` to have two entries: {1: [<indices>], 2: [<indices>]}.
		// - `in_versions` to contain ExtensionA and ExtensionB in some order.

		// Check that by_version now has 2 distinct versions defined.
		assert_eq!(builder.by_version.len(), 2);

		// Verify version 1 entries.
		{
			let v1_indices = builder.by_version.get(&1).expect("Version 1 must be present");
			assert_eq!(v1_indices.len(), 1, "Version 1 should have exactly one extension");
			// Since we only ever added ExtensionA for version 1, it must match.
			assert_eq!(builder.in_versions[v1_indices[0] as usize].identifier, "ExtensionA");
		}

		// Verify version 2 entries.
		{
			let v2_indices = builder.by_version.get(&2).expect("Version 2 must be present");
			assert_eq!(v2_indices.len(), 2, "Version 2 should have exactly two extensions");
			// For version 2, we pushed B then A, so the index order should reflect that:
			//   - ExtensionB is new, so it should get appended at the end of `in_versions`.
			//   - ExtensionA was seen previously, so it should reuse the earlier index.

			// First index for version 2 should point to "ExtensionB".
			assert_eq!(builder.in_versions[v2_indices[0] as usize].identifier, "ExtensionB");
			// Second index for version 2 should point back to "ExtensionA".
			assert_eq!(builder.in_versions[v2_indices[1] as usize].identifier, "ExtensionA");
		}

		// There should be exactly 2 unique entries in `in_versions`: [ExtensionA, ExtensionB].
		assert_eq!(builder.in_versions.len(), 2);
	}
}
