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

//! Type to define a versioned transaction extension pipeline for a specific version.

use crate::{
	traits::{
		AsTransactionAuthorizedOrigin, DecodeWithVersion, DecodeWithVersionWithMemTracking,
		DispatchInfoOf, DispatchOriginOf, DispatchTransaction, Dispatchable, Pipeline,
		PipelineMetadataBuilder, PipelineVersion, PostDispatchInfoOf, TransactionExtension,
	},
	transaction_validity::{TransactionSource, TransactionValidityError, ValidTransaction},
};
use codec::{Decode, DecodeWithMemTracking, Encode};
use core::fmt::Debug;
use scale_info::TypeInfo;
use sp_weights::Weight;

/// A transaction extension pipeline defined for a single version.
#[derive(Encode, Clone, Debug, TypeInfo, PartialEq, Eq)]
pub struct PipelineAtVers<const VERSION: u8, Extension> {
	/// The transaction extension pipeline for the version `VERSION`.
	pub extension: Extension,
}

impl<const VERSION: u8, Extension> PipelineAtVers<VERSION, Extension> {
	/// Create a new versioned extension.
	pub fn new(extension: Extension) -> Self {
		Self { extension }
	}
}

impl<const VERSION: u8, Extension: Decode> DecodeWithVersion
	for PipelineAtVers<VERSION, Extension>
{
	fn decode_with_version<I: codec::Input>(
		extension_version: u8,
		input: &mut I,
	) -> Result<Self, codec::Error> {
		if extension_version == VERSION {
			Ok(PipelineAtVers { extension: Extension::decode(input)? })
		} else {
			Err(codec::Error::from("Invalid extension version"))
		}
	}
}

impl<const VERSION: u8, Extension: DecodeWithMemTracking> DecodeWithVersionWithMemTracking
	for PipelineAtVers<VERSION, Extension>
{
}

impl<const VERSION: u8, Extension> PipelineVersion for PipelineAtVers<VERSION, Extension> {
	fn version(&self) -> u8 {
		VERSION
	}
}

impl<const VERSION: u8, Call, Extension> Pipeline<Call> for PipelineAtVers<VERSION, Extension>
where
	Call: Dispatchable<RuntimeOrigin: AsTransactionAuthorizedOrigin> + Encode,
	Extension: TransactionExtension<Call>,
{
	fn build_metadata(builder: &mut PipelineMetadataBuilder) {
		builder.push_versioned_extension(VERSION, Extension::metadata());
	}
	fn validate_only(
		&self,
		origin: DispatchOriginOf<Call>,
		call: &Call,
		info: &DispatchInfoOf<Call>,
		len: usize,
		source: TransactionSource,
	) -> Result<ValidTransaction, TransactionValidityError> {
		self.extension
			.validate_only(origin, call, info, len, source, VERSION)
			.map(|x| x.0)
	}
	fn dispatch_transaction(
		self,
		origin: DispatchOriginOf<Call>,
		call: Call,
		info: &DispatchInfoOf<Call>,
		len: usize,
	) -> crate::ApplyExtrinsicResultWithInfo<PostDispatchInfoOf<Call>> {
		self.extension.dispatch_transaction(origin, call, info, len, VERSION)
	}
	fn weight(&self, call: &Call) -> Weight {
		self.extension.weight(call)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		traits::{
			Dispatchable, Implication, TransactionExtension, TransactionSource, ValidateResult,
		},
		transaction_validity::{InvalidTransaction, TransactionValidityError, ValidTransaction},
		DispatchError,
	};
	use codec::{Decode, DecodeWithMemTracking, Encode};
	use sp_weights::Weight;

	// --- Mock types ---

	/// A mock call type implementing Dispatchable
	#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
	pub struct MockCall(pub u64);
	#[derive(Debug)]
	pub struct MockOrigin(pub u64);

	impl AsTransactionAuthorizedOrigin for MockOrigin {
		fn is_transaction_authorized(&self) -> bool {
			true
		}
	}

	impl Dispatchable for MockCall {
		type RuntimeOrigin = MockOrigin;
		type Config = ();
		type Info = ();
		type PostInfo = ();

		fn dispatch(
			self,
			origin: Self::RuntimeOrigin,
		) -> crate::DispatchResultWithInfo<Self::PostInfo> {
			if origin.0 == 0 {
				return Err(DispatchError::Other("origin is 0").into());
			}
			Ok(Default::default())
		}
	}

	// A trivial extension that sets a known weight and does minimal logic.
	// We simply store an integer "token" and do check logic on it.
	#[derive(PartialEq, Eq, Clone, Debug, Encode, Decode, DecodeWithMemTracking, TypeInfo)]
	pub struct SimpleExtension {
		/// The token for validation logic
		pub token: u32,
		/// The "weight" that this extension claims to cost.
		pub w: u64,
	}

	impl TransactionExtension<MockCall> for SimpleExtension {
		const IDENTIFIER: &'static str = "SimpleExtension";

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
			// any origin is permitted, but if `token == 0` => invalid
			if self.token == 0 {
				Err(InvalidTransaction::Custom(1).into())
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

	// This type represents the versioned extension pipeline for version=3.
	pub type ExtV3 = PipelineAtVers<3, SimpleExtension>;

	// This type represents the versioned extension pipeline for version=10.
	pub type ExtV10 = PipelineAtVers<10, SimpleExtension>;

	// --- Tests ---

	#[test]
	fn decode_with_correct_version_succeeds() {
		let ext_v3 = ExtV3 { extension: SimpleExtension { token: 55, w: 1234 } };
		let encoded = ext_v3.encode();

		let decoded = <ExtV3 as DecodeWithVersion>::decode_with_version(3, &mut &encoded[..])
			.expect("should decode fine with matching version");
		assert_eq!(decoded.extension.token, 55);
		assert_eq!(decoded.extension.w, 1234);
	}

	#[test]
	fn decode_with_incorrect_version_fails() {
		let ext_v3 = ExtV3 { extension: SimpleExtension { token: 55, w: 1234 } };
		let encoded = ext_v3.encode();

		// Attempt decode with version=10
		let decode_err = <ExtV3 as DecodeWithVersion>::decode_with_version(10, &mut &encoded[..])
			.expect_err("should fail decode due to invalid version");
		let decode_err_str = format!("{}", decode_err);
		assert!(decode_err_str.contains("Invalid extension version"));
	}

	#[test]
	fn version_is_correct() {
		let ext_v3 = ExtV3 { extension: SimpleExtension { token: 55, w: 1234 } };
		assert_eq!(ext_v3.version(), 3);

		let ext_v10 = ExtV10 { extension: SimpleExtension { token: 1, w: 1 } };
		assert_eq!(ext_v10.version(), 10);
	}

	#[test]
	fn pipeline_functions_work() {
		let ext_v3 = ExtV3 { extension: SimpleExtension { token: 999, w: 50 } };

		// test "weight" function
		let call = MockCall(0x_f00);
		assert_eq!(ext_v3.weight(&call).ref_time(), 50);

		// test validating logic
		{
			// token = 0 => invalid
			let invalid_ext_v3 = ExtV3 { extension: SimpleExtension { token: 0, w: 10 } };
			let validity = invalid_ext_v3.validate_only(
				MockOrigin(1),
				&call,
				&Default::default(),
				0,
				TransactionSource::External,
			);
			assert_eq!(
				validity,
				Err(TransactionValidityError::Invalid(InvalidTransaction::Custom(1)))
			);
		}

		// ok scenario: token != 0 => OK
		let validity_ok = ext_v3.validate_only(
			MockOrigin(2),
			&call,
			&Default::default(),
			0,
			TransactionSource::Local,
		);
		assert!(validity_ok.is_ok());
		let valid = validity_ok.unwrap();
		assert_eq!(valid, ValidTransaction::default());
	}

	#[test]
	fn dispatch_transaction_works() {
		// This extension is valid => token=1
		let ext_v3 = ExtV3 { extension: SimpleExtension { token: 1, w: 10 } };
		let call = MockCall(123);
		let info = Default::default();
		let len = 0usize;

		// dispatch => OK
		ext_v3
			.clone()
			.dispatch_transaction(MockOrigin(1), call.clone(), &info, len)
			.expect("valid dispatch")
			.expect("should be OK");

		// but if origin is None => the underlying call fails
		let res_fail = ext_v3.dispatch_transaction(MockOrigin(0), call, &info, len);
		let block_err = res_fail.expect("valid").expect_err("should fail");
		assert_eq!(block_err.error, DispatchError::Other("origin is 0"));
	}
}
