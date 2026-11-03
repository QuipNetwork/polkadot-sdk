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

//! Types and trait to aggregate multiple versioned transaction extension pipelines.

use crate::{
	traits::{
		DecodeWithVersion, DecodeWithVersionWithMemTracking, DispatchInfoOf, DispatchOriginOf,
		Dispatchable, InvalidVersion, Pipeline, PipelineAtVers, PipelineMetadataBuilder,
		PipelineVersion, PostDispatchInfoOf,
	},
	transaction_validity::{TransactionSource, TransactionValidityError, ValidTransaction},
};
use alloc::vec::Vec;
use codec::Encode;
use core::fmt::Debug;
use scale_info::TypeInfo;
use sp_weights::Weight;

/// An item in [`MultiVersion`]. It represents a transaction extension pipeline of a specific
/// single version.
pub trait MultiVersionItem {
	/// The version of the transaction extension pipeline.
	///
	/// `None` means that the item has no version and can't be decoded.
	const VERSION: Option<u8>;
}

impl MultiVersionItem for InvalidVersion {
	const VERSION: Option<u8> = None;
}

impl<const VERSION: u8, Extension> MultiVersionItem for PipelineAtVers<VERSION, Extension> {
	const VERSION: Option<u8> = Some(VERSION);
}

macro_rules! declare_multi_version_enum {
	($( $variant:tt, )*) => {

		/// An implementation of [`Pipeline`] that aggregates multiple versioned transaction
		/// extension pipeline.
		///
		/// It is an enum where each variant has its own version, duplicated version must be
		/// avoided, only the first used version will be effective other duplicated version will be
		/// ignored.
		///
		/// Versioned transaction extension pipelines are configured using the generic parameters.
		///
		/// # Example
		///
		/// ```
		/// use sp_runtime::traits::{MultiVersion, PipelineAtVers};
		///
		/// struct PaymentExt;
		/// struct PaymentExtV2;
		/// struct NonceExt;
		///
		/// type ExtV1 = PipelineAtVers<1, (NonceExt, PaymentExt)>;
		/// type ExtV4 = PipelineAtVers<4, (NonceExt, PaymentExtV2)>;
		///
		/// /// The transaction extension pipeline that supports both version 1 and 4.
		/// type TransactionExtension = MultiVersion<ExtV1, ExtV4>;
		/// ```
		#[allow(private_interfaces)]
		#[derive(PartialEq, Eq, Clone, Debug, TypeInfo)]
		pub enum MultiVersion<
			$(
				$variant = InvalidVersion,
			)*
		> {
			$(
				/// The transaction extension pipeline of a specific version.
				$variant($variant),
			)*
		}

		impl<$( $variant: PipelineVersion, )*> PipelineVersion for MultiVersion<$( $variant, )*> {
			fn version(&self) -> u8 {
				match self {
					$(
						MultiVersion::$variant(v) => v.version(),
					)*
				}
			}
		}

		// It encodes without the variant index.
		impl<$( $variant: Encode, )*> Encode for MultiVersion<$( $variant, )*> {
			fn size_hint(&self) -> usize {
				match self {
					$(
						MultiVersion::$variant(v) => v.size_hint(),
					)*
				}
			}
			fn encode(&self) -> Vec<u8> {
				match self {
					$(
						MultiVersion::$variant(v) => v.encode(),
					)*
				}
			}
			fn encode_to<CodecOutput: codec::Output + ?Sized>(&self, dest: &mut CodecOutput) {
				match self {
					$(
						MultiVersion::$variant(v) => v.encode_to(dest),
					)*
				}
			}
			fn encoded_size(&self) -> usize {
				match self {
					$(
						MultiVersion::$variant(v) => v.encoded_size(),
					)*
				}
			}
			fn using_encoded<FunctionResult, Function: FnOnce(&[u8]) -> FunctionResult>(
				&self,
				f: Function
			) -> FunctionResult {
				match self {
					$(
						MultiVersion::$variant(v) => v.using_encoded(f),
					)*
				}
			}
		}

		// It decodes from a specified version.
		impl<$( $variant: DecodeWithVersion + MultiVersionItem, )*>
			DecodeWithVersion for MultiVersion<$( $variant, )*>
		{
			fn decode_with_version<CodecInput: codec::Input>(
				extension_version: u8,
				input: &mut CodecInput,
			) -> Result<Self, codec::Error> {
				$(
					// Here we could try all variants without checking for the version,
					// but the error would be less informative.
					// Otherwise we could change the trait `DecodeWithVersion` to return an enum of
					// 3 variants: ok, error and invalid_version.
					if $variant::VERSION == Some(extension_version) {
						return Ok(MultiVersion::$variant($variant::decode_with_version(extension_version, input)?));
					}
				)*

				Err(codec::Error::from("Invalid extension version"))
			}
		}

		impl<$( $variant: DecodeWithVersionWithMemTracking + MultiVersionItem, )*>
			DecodeWithVersionWithMemTracking for MultiVersion<$( $variant, )*>
		{}

		impl<$( $variant: Pipeline<Call> + MultiVersionItem, )* Call: Dispatchable>
			Pipeline<Call> for MultiVersion<$( $variant, )*>
		{
			fn build_metadata(builder: &mut PipelineMetadataBuilder) {
				$(
					$variant::build_metadata(builder);
				)*
			}
			fn validate_only(
				&self,
				origin: DispatchOriginOf<Call>,
				call: &Call,
				info: &DispatchInfoOf<Call>,
				len: usize,
				source: TransactionSource,
			) -> Result<ValidTransaction, TransactionValidityError> {
				match self {
					$(
						MultiVersion::$variant(v) => v.validate_only(origin, call, info, len, source),
					)*
				}
			}
			fn dispatch_transaction(
				self,
				origin: DispatchOriginOf<Call>,
				call: Call,
				info: &DispatchInfoOf<Call>,
				len: usize,
			) -> crate::ApplyExtrinsicResultWithInfo<PostDispatchInfoOf<Call>> {
				match self {
					$(
						MultiVersion::$variant(v) => v.dispatch_transaction(origin, call, info, len),
					)*
				}
			}
			fn weight(&self, call: &Call) -> Weight {
				match self {
					$(
						MultiVersion::$variant(v) => v.weight(call),
					)*
				}
			}
		}
	};
}

declare_multi_version_enum! {
	A, B, C, D, E, F, G, H, I, J, K, L, M, N, O, P, Q, R, S, T, U, V, W, X, Y, Z,
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		traits::{
			AsTransactionAuthorizedOrigin, DecodeWithVersion, DispatchInfoOf, Dispatchable,
			Implication, Pipeline, PipelineVersion, TransactionExtension, TransactionSource,
			ValidateResult,
		},
		transaction_validity::{InvalidTransaction, TransactionValidityError, ValidTransaction},
		DispatchError,
	};
	use codec::{Decode, DecodeWithMemTracking, Encode};
	use core::fmt::Debug;
	use scale_info::TypeInfo;
	use sp_weights::Weight;

	// --------------------------------------------------------
	// A mock call type and origin used for testing
	// --------------------------------------------------------
	#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq, TypeInfo)]
	pub struct MockCall(pub u32);

	#[derive(Clone, Debug)]
	pub struct MockOrigin(pub u8);

	impl AsTransactionAuthorizedOrigin for MockOrigin {
		fn is_transaction_authorized(&self) -> bool {
			// Let's say any origin != 0 is authorized
			self.0 != 0
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
			// If the origin is 0, dispatch fails.
			// Also, if the call is 0, dispatch fails.
			if origin.0 == 0 {
				return Err(DispatchError::Other("Unauthorized origin=0").into());
			}
			if self.0 == 0 {
				return Err(DispatchError::Other("call=0").into());
			}
			Ok(Default::default())
		}
	}

	// --------------------------------------------------------
	// Let's define two single-version pipelines with versions 4 and 7
	// --------------------------------------------------------

	// A single-version extension pipeline that "succeeds" only if token != 0
	#[derive(Clone, Debug, Encode, Decode, DecodeWithMemTracking, PartialEq, Eq, TypeInfo)]
	pub struct SimpleExtensionV4 {
		pub token: u8,
		pub declared_weight: u64,
	}

	impl TransactionExtension<MockCall> for SimpleExtensionV4 {
		const IDENTIFIER: &'static str = "SimpleExtV4";
		type Implicit = ();
		type Val = ();
		type Pre = ();

		fn implicit(&self) -> Result<Self::Implicit, TransactionValidityError> {
			Ok(())
		}

		fn weight(&self, _call: &MockCall) -> Weight {
			Weight::from_parts(self.declared_weight, 0)
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
			if self.token == 0 {
				Err(InvalidTransaction::Custom(44).into())
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

	pub type PipelineV4 = PipelineAtVers<4, SimpleExtensionV4>;

	// Another single-version extension pipeline, version=7
	#[derive(Clone, Debug, Encode, Decode, DecodeWithMemTracking, PartialEq, Eq, TypeInfo)]
	pub struct SimpleExtensionV7 {
		pub token: u8,
		pub declared_weight: u64,
	}

	impl TransactionExtension<MockCall> for SimpleExtensionV7 {
		const IDENTIFIER: &'static str = "SimpleExtV7";
		type Implicit = ();
		type Val = ();
		type Pre = ();

		fn implicit(&self) -> Result<Self::Implicit, TransactionValidityError> {
			Ok(())
		}

		fn weight(&self, _call: &MockCall) -> Weight {
			Weight::from_parts(self.declared_weight, 0)
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
			if self.token == 0 {
				Err(InvalidTransaction::Custom(77).into())
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

	pub type PipelineV7 = PipelineAtVers<7, SimpleExtensionV7>;

	// --------------------------------------------------------
	// Our MultiVersion definition under test
	// --------------------------------------------------------

	pub type MyMultiExt = MultiVersion<PipelineV4, PipelineV7>;

	// --------------------------------------------------------
	// Actual tests
	// --------------------------------------------------------

	#[test]
	fn decode_with_version_works_for_known_versions() {
		// Build a pipeline for version=4
		let pipeline_v4 = PipelineV4::new(SimpleExtensionV4 { token: 99, declared_weight: 123 });
		let encoded_v4 = pipeline_v4.encode();
		let decoded_v4 = MyMultiExt::decode_with_version(4, &mut &encoded_v4[..])
			.expect("decode with version=4");
		let expected_v4 = MultiVersion::A(pipeline_v4);
		assert_eq!(decoded_v4, expected_v4);

		// Build a pipeline for version=7
		let pipeline_v7 = PipelineV7::new(SimpleExtensionV7 { token: 55, declared_weight: 777 });
		let encoded_v7 = pipeline_v7.encode();
		let decoded_v7 = MyMultiExt::decode_with_version(7, &mut &encoded_v7[..])
			.expect("decode with version=7");
		let expected_v7 = MultiVersion::B(pipeline_v7);
		assert_eq!(decoded_v7, expected_v7);
	}

	#[test]
	fn decode_with_unknown_version_fails() {
		let pipeline_v4 = PipelineV4::new(SimpleExtensionV4 { token: 1, declared_weight: 100 });
		let encoded_v4 = pipeline_v4.encode();

		// Attempt decode with version=123 => fails
		let decode_err = MyMultiExt::decode_with_version(123, &mut &encoded_v4[..])
			.expect_err("decode must fail with unknown version=123");
		assert!(format!("{}", decode_err).contains("Invalid extension version"));
	}

	#[test]
	fn version_is_correct() {
		// The variant "A" is always the first in our MultiVersion and is version=4
		let multi_a =
			MyMultiExt::A(PipelineV4::new(SimpleExtensionV4 { token: 1, declared_weight: 10 }));
		assert_eq!(multi_a.version(), 4);

		// The variant "B" is version=7
		let multi_b =
			MyMultiExt::B(PipelineV7::new(SimpleExtensionV7 { token: 2, declared_weight: 20 }));
		assert_eq!(multi_b.version(), 7);
	}

	#[test]
	fn weight_check_works() {
		let multi_a =
			MyMultiExt::A(PipelineV4::new(SimpleExtensionV4 { token: 1, declared_weight: 500 }));
		let multi_b =
			MyMultiExt::B(PipelineV7::new(SimpleExtensionV7 { token: 1, declared_weight: 999 }));

		let call = MockCall(0);
		assert_eq!(multi_a.weight(&call).ref_time(), 500);
		assert_eq!(multi_b.weight(&call).ref_time(), 999);
	}

	#[test]
	fn validate_only_logic_works() {
		// A with token=0 => invalid
		let invalid_a =
			MyMultiExt::A(PipelineV4::new(SimpleExtensionV4 { token: 0, declared_weight: 123 }));
		let call = MockCall(42);
		let validity = invalid_a.validate_only(
			MockOrigin(42),
			&call,
			&Default::default(),
			0,
			TransactionSource::Local,
		);
		assert_eq!(
			validity,
			Err(TransactionValidityError::Invalid(InvalidTransaction::Custom(44)))
		);

		// B with token=0 => invalid
		let invalid_b =
			MyMultiExt::B(PipelineV7::new(SimpleExtensionV7 { token: 0, declared_weight: 456 }));
		let validity_b = invalid_b.validate_only(
			MockOrigin(42),
			&call,
			&Default::default(),
			0,
			TransactionSource::Local,
		);
		assert_eq!(
			validity_b,
			Err(TransactionValidityError::Invalid(InvalidTransaction::Custom(77)))
		);

		// A with token=some => ok
		let valid_a =
			MyMultiExt::A(PipelineV4::new(SimpleExtensionV4 { token: 55, declared_weight: 10 }));
		let result_ok_a = valid_a.validate_only(
			MockOrigin(1),
			&call,
			&Default::default(),
			0,
			TransactionSource::External,
		);
		assert!(result_ok_a.is_ok(), "valid scenario for pipeline A");
	}

	#[test]
	fn dispatch_transaction_works() {
		// "A" with token != 0 => valid
		let pipeline_a = PipelineV4::new(SimpleExtensionV4 { token: 33, declared_weight: 1 });
		let multi_a = MyMultiExt::A(pipeline_a);
		let call_good = MockCall(42);
		multi_a
			.dispatch_transaction(MockOrigin(9), call_good.clone(), &Default::default(), 0)
			.expect("Should not fail validity")
			.expect("Success");

		// but call=0 => dispatch fails
		let fail_res =
			MyMultiExt::A(PipelineV4::new(SimpleExtensionV4 { token: 1, declared_weight: 10 }))
				.dispatch_transaction(MockOrigin(9), MockCall(0), &Default::default(), 0)
				.expect("Should be a valid transaction from viewpoint of extension");
		let block_err = fail_res.expect_err("actual dispatch error");
		assert_eq!(block_err.error, DispatchError::Other("call=0"));

		// "B" scenario
		let pipeline_b = PipelineV7::new(SimpleExtensionV7 { token: 2, declared_weight: 99 });
		let multi_b = MyMultiExt::B(pipeline_b);
		let outcome_ok = multi_b
			.dispatch_transaction(MockOrigin(1), call_good, &Default::default(), 0)
			.expect("Should pass validity");
		assert!(outcome_ok.is_ok());
	}
}
