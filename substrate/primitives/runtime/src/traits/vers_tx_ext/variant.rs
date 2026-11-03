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

//! Implementation of versioned transaction extension pipeline that aggregate a version 0 and
//! other versions.

use crate::{
	generic::ExtensionVersion,
	traits::{
		AsTransactionAuthorizedOrigin, DecodeWithVersion, DecodeWithVersionWithMemTracking,
		DispatchInfoOf, DispatchTransaction, Dispatchable, Pipeline, PipelineAtVers,
		PipelineMetadataBuilder, PipelineVersion, PostDispatchInfoOf, TransactionExtension,
	},
	transaction_validity::TransactionSource,
};
use alloc::vec::Vec;
use codec::{Decode, Encode};
use scale_info::TypeInfo;
use sp_weights::Weight;

/// Version 0 of the transaction extension version.
const EXTENSION_V0_VERSION: ExtensionVersion = 0;

/// A versioned transaction extension pipeline defined with 2 variants: one for the version 0 and
/// one for other versions.
///
/// The generic `ExtensionOtherVersions` should not re-define a transaction extension pipeline for
/// the version 0, it will be ignored. The transaction extension pipeline for the version 0 is
/// defined by the generic `ExtensionV0`.
#[derive(PartialEq, Eq, Clone, Debug, TypeInfo)]
pub enum ExtensionVariant<ExtensionV0, ExtensionOtherVersions> {
	/// A transaction extension pipeline for the version 0.
	V0(ExtensionV0),
	/// A transaction extension pipeline for other versions.
	Other(ExtensionOtherVersions),
}

impl<ExtensionV0, ExtensionOtherVersions: PipelineVersion> PipelineVersion
	for ExtensionVariant<ExtensionV0, ExtensionOtherVersions>
{
	fn version(&self) -> u8 {
		match self {
			ExtensionVariant::V0(_) => EXTENSION_V0_VERSION,
			ExtensionVariant::Other(ext) => ext.version(),
		}
	}
}

impl<ExtensionV0: Encode, ExtensionOtherVersions: Encode> Encode
	for ExtensionVariant<ExtensionV0, ExtensionOtherVersions>
{
	fn encode(&self) -> Vec<u8> {
		match self {
			ExtensionVariant::V0(ext) => ext.encode(),
			ExtensionVariant::Other(ext) => ext.encode(),
		}
	}
	fn size_hint(&self) -> usize {
		match self {
			ExtensionVariant::V0(ext) => ext.size_hint(),
			ExtensionVariant::Other(ext) => ext.size_hint(),
		}
	}
	fn encode_to<T: codec::Output + ?Sized>(&self, dest: &mut T) {
		match self {
			ExtensionVariant::V0(ext) => ext.encode_to(dest),
			ExtensionVariant::Other(ext) => ext.encode_to(dest),
		}
	}
	fn encoded_size(&self) -> usize {
		match self {
			ExtensionVariant::V0(ext) => ext.encoded_size(),
			ExtensionVariant::Other(ext) => ext.encoded_size(),
		}
	}
	fn using_encoded<R, F: FnOnce(&[u8]) -> R>(&self, f: F) -> R {
		match self {
			ExtensionVariant::V0(ext) => ext.using_encoded(f),
			ExtensionVariant::Other(ext) => ext.using_encoded(f),
		}
	}
}

impl<ExtensionV0: Decode, ExtensionOtherVersions: DecodeWithVersion> DecodeWithVersion
	for ExtensionVariant<ExtensionV0, ExtensionOtherVersions>
{
	fn decode_with_version<I: codec::Input>(
		extension_version: u8,
		input: &mut I,
	) -> Result<Self, codec::Error> {
		match extension_version {
			EXTENSION_V0_VERSION => Ok(ExtensionVariant::V0(Decode::decode(input)?)),
			_ => Ok(ExtensionVariant::Other(DecodeWithVersion::decode_with_version(
				extension_version,
				input,
			)?)),
		}
	}
}

impl<ExtensionV0: Decode, ExtensionOtherVersions: DecodeWithVersionWithMemTracking>
	DecodeWithVersionWithMemTracking for ExtensionVariant<ExtensionV0, ExtensionOtherVersions>
{
}

impl<
		Call: Dispatchable + Encode,
		ExtensionV0: TransactionExtension<Call>,
		ExtensionOtherVersions: Pipeline<Call>,
	> Pipeline<Call> for ExtensionVariant<ExtensionV0, ExtensionOtherVersions>
where
	<Call as Dispatchable>::RuntimeOrigin: AsTransactionAuthorizedOrigin,
{
	fn build_metadata(builder: &mut PipelineMetadataBuilder) {
		PipelineAtVers::<EXTENSION_V0_VERSION, ExtensionV0>::build_metadata(builder);
		ExtensionOtherVersions::build_metadata(builder);
	}
	fn validate_only(
		&self,
		origin: super::DispatchOriginOf<Call>,
		call: &Call,
		info: &DispatchInfoOf<Call>,
		len: usize,
		source: TransactionSource,
	) -> Result<
		crate::transaction_validity::ValidTransaction,
		crate::transaction_validity::TransactionValidityError,
	> {
		match self {
			ExtensionVariant::V0(ext) => ext
				.validate_only(origin, call, info, len, source, EXTENSION_V0_VERSION)
				.map(|x| x.0),
			ExtensionVariant::Other(ext) => ext.validate_only(origin, call, info, len, source),
		}
	}
	fn dispatch_transaction(
		self,
		origin: super::DispatchOriginOf<Call>,
		call: Call,
		info: &DispatchInfoOf<Call>,
		len: usize,
	) -> crate::ApplyExtrinsicResultWithInfo<PostDispatchInfoOf<Call>> {
		match self {
			ExtensionVariant::V0(ext) => {
				ext.dispatch_transaction(origin, call, info, len, EXTENSION_V0_VERSION)
			},
			ExtensionVariant::Other(ext) => ext.dispatch_transaction(origin, call, info, len),
		}
	}
	fn weight(&self, call: &Call) -> Weight {
		match self {
			ExtensionVariant::V0(ext) => ext.weight(call),
			ExtensionVariant::Other(ext) => ext.weight(call),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		traits::{
			AsTransactionAuthorizedOrigin, DispatchInfoOf, Dispatchable, Implication,
			PipelineAtVers, TransactionExtension, TransactionSource, ValidateResult,
		},
		transaction_validity::{InvalidTransaction, TransactionValidityError, ValidTransaction},
		DispatchError,
	};
	use codec::{Decode, DecodeWithMemTracking, Encode};
	use core::fmt::Debug;
	use sp_weights::Weight;

	// --------------------------------------------------------------------
	// 1. Mock call and "origin" type
	// --------------------------------------------------------------------

	#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
	pub struct MockCall(pub u64);
	#[derive(Debug)]
	pub struct MockOrigin;

	impl Dispatchable for MockCall {
		type RuntimeOrigin = MockOrigin;
		type Config = ();
		type Info = ();
		type PostInfo = ();

		fn dispatch(
			self,
			_origin: Self::RuntimeOrigin,
		) -> crate::DispatchResultWithInfo<Self::PostInfo> {
			if self.0 == 0 {
				return Err(DispatchError::Other("call is 0").into());
			}
			Ok(Default::default())
		}
	}

	// We'll implement the AsTransactionAuthorizedOrigin for Option<u64>:
	impl AsTransactionAuthorizedOrigin for MockOrigin {
		fn is_transaction_authorized(&self) -> bool {
			true
		}
	}

	// --------------------------------------------------------------------
	// 2. Mock Extension used as "ExtensionV0"
	// --------------------------------------------------------------------

	/// A trivial extension type for "old-school version 0".
	#[derive(Clone, Debug, Encode, Decode, DecodeWithMemTracking, PartialEq, Eq, TypeInfo)]
	pub struct ExtV0 {
		pub success_token: bool,
		pub w: u64,
	}

	impl TransactionExtension<MockCall> for ExtV0 {
		const IDENTIFIER: &'static str = "OldSchoolV0";
		type Implicit = ();
		fn implicit(&self) -> Result<Self::Implicit, TransactionValidityError> {
			Ok(())
		}
		type Val = ();
		type Pre = ();

		fn weight(&self, _call: &MockCall) -> Weight {
			Weight::from_parts(self.w, 0)
		}

		fn validate(
			&self,
			origin: MockOrigin,
			_call: &MockCall,
			_info: &DispatchInfoOf<MockCall>,
			_len: usize,
			_self_implicit: Self::Implicit,
			_inherited_implication: &impl Implication,
			_source: TransactionSource,
		) -> ValidateResult<Self::Val, MockCall> {
			if !self.success_token {
				Err(InvalidTransaction::Custom(99).into())
			} else {
				Ok((ValidTransaction::default(), (), origin))
			}
		}

		fn prepare(
			self,
			_val: Self::Val,
			_origin: &MockOrigin,
			_call: &MockCall,
			_info: &DispatchInfoOf<MockCall>,
			_len: usize,
		) -> Result<Self::Pre, TransactionValidityError> {
			Ok(())
		}
	}

	// --------------------------------------------------------------------
	// 3. Another pipeline that is used for "Other" versions: We'll define a minimal versioned
	//    pipeline with one version
	// --------------------------------------------------------------------

	/// Another extension for "some version" pipeline.
	#[derive(Clone, Debug, Encode, Decode, DecodeWithMemTracking, PartialEq, Eq, TypeInfo)]
	pub struct OtherExtension {
		pub token: u16,
		pub w: u64,
	}

	impl TransactionExtension<MockCall> for OtherExtension {
		const IDENTIFIER: &'static str = "OtherExtension";
		type Implicit = ();
		fn implicit(&self) -> Result<Self::Implicit, TransactionValidityError> {
			Ok(())
		}
		type Val = ();
		type Pre = ();

		fn weight(&self, _call: &MockCall) -> Weight {
			Weight::from_parts(self.w, 0)
		}

		fn validate(
			&self,
			origin: MockOrigin,
			_call: &MockCall,
			_info: &DispatchInfoOf<MockCall>,
			_len: usize,
			_self_implicit: Self::Implicit,
			_inherited_implication: &impl Implication,
			_source: TransactionSource,
		) -> ValidateResult<Self::Val, MockCall> {
			// If 'token' is 0 => invalid. Else ok.
			if self.token == 0 {
				return Err(InvalidTransaction::Custom(7).into());
			}
			Ok((ValidTransaction::default(), (), origin))
		}

		fn prepare(
			self,
			_val: Self::Val,
			_origin: &MockOrigin,
			_call: &MockCall,
			_info: &DispatchInfoOf<MockCall>,
			_len: usize,
		) -> Result<Self::Pre, TransactionValidityError> {
			Ok(())
		}
	}

	type ExtV2 = PipelineAtVers<2, OtherExtension>;

	// --------------------------------------------------------------------
	// Actual unit tests
	// --------------------------------------------------------------------

	type Variant = ExtensionVariant<ExtV0, ExtV2>;

	#[test]
	fn decode_v0() {
		// If extension_version == 0 => decode as V0
		let v0_data = ExtV0 { success_token: true, w: 42 }.encode();
		let candidate = Variant::decode_with_version(0, &mut &v0_data[..])
			.expect("decode with v0 must succeed");
		let ExtensionVariant::V0(ext_v0) = candidate else { panic!("Expected V0 variant") };
		assert!(ext_v0.success_token);
		assert_eq!(ext_v0.w, 42);
	}

	#[test]
	fn decode_other() {
		// If extension_version == 2 => decode as Other
		let pipeline = ExtV2::new(OtherExtension { token: 9, w: 888 });
		let encoded = pipeline.encode();
		let candidate = Variant::decode_with_version(2, &mut &encoded[..])
			.expect("decode with version=2 => 'Other'");
		let ExtensionVariant::Other(p) = candidate else { panic!("Expected Other variant") };
		assert_eq!(p.extension.token, 9);
		assert_eq!(p.extension.w, 888);
	}

	#[test]
	fn version_check() {
		let v0_var: Variant = ExtensionVariant::V0(ExtV0 { success_token: true, w: 1 });
		let other_var: Variant =
			ExtensionVariant::Other(ExtV2::new(OtherExtension { token: 1, w: 1 }));
		assert_eq!(v0_var.version(), 0);
		assert_eq!(other_var.version(), 2);
	}

	#[test]
	fn weight_check() {
		let v0_var: Variant = ExtensionVariant::V0(ExtV0 { success_token: true, w: 100 });
		let other_var: Variant =
			ExtensionVariant::Other(ExtV2::new(OtherExtension { token: 2, w: 555 }));
		let call = MockCall(123);

		assert_eq!(v0_var.weight(&call).ref_time(), 100);
		assert_eq!(other_var.weight(&call).ref_time(), 555);
	}

	#[test]
	fn validate_only_works() {
		{
			// v0 + success_token => ok
			let v0_var: Variant = ExtensionVariant::V0(ExtV0 { success_token: true, w: 100 });
			let call = MockCall(1);
			let valid = v0_var.validate_only(
				MockOrigin,
				&call,
				&Default::default(),
				0,
				TransactionSource::External,
			);
			assert!(valid.is_ok());
		}
		{
			// other => token=0 => fail
			let var_other: Variant =
				ExtensionVariant::Other(ExtV2::new(OtherExtension { token: 0, w: 5 }));
			let call = MockCall(1);
			let fail = var_other.validate_only(
				MockOrigin,
				&call,
				&Default::default(),
				0,
				TransactionSource::Local,
			);
			assert_eq!(fail, Err(TransactionValidityError::Invalid(InvalidTransaction::Custom(7))));
		}
	}

	#[test]
	fn dispatch_transaction_works() {
		// We'll do "v0" scenario with success_token => true => valid
		let v0_var: Variant = ExtensionVariant::V0(ExtV0 { success_token: true, w: 12 });
		let call = MockCall(42);
		let result = v0_var.dispatch_transaction(MockOrigin, call.clone(), &Default::default(), 0);
		let extrinsic_outcome = result.expect("Ok(ApplyExtrinsicResultWithInfo)");
		assert!(extrinsic_outcome.is_ok(), "call with origin Some => dispatch Ok");

		// If call is 0 => call fails at dispatch
		let err = Variant::V0(ExtV0 { success_token: true, w: 1 })
			.dispatch_transaction(MockOrigin, MockCall(0), &Default::default(), 0)
			.expect("valid")
			.expect_err("dispatch error");

		assert_eq!(err.error, DispatchError::Other("call is 0"));

		// check scenario for "other" too
		let var_other: Variant =
			ExtensionVariant::Other(ExtV2::new(OtherExtension { token: 5, w: 55 }));
		let outcome = var_other
			.dispatch_transaction(MockOrigin, call, &Default::default(), 0)
			.expect("Should be ok");
		assert!(outcome.is_ok());
	}
}
