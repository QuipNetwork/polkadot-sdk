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

//! Transaction extension which validates a signature against a payload constructed from a call and
//! the rest of the transaction extension pipeline.

use crate::{Config, WeightInfo};
use codec::{Decode, DecodeWithMemTracking, Encode};
use frame_support::{pallet_prelude::TransactionSource, traits::OriginTrait};
use scale_info::TypeInfo;
use sp_io::hashing::blake2_256;
use sp_runtime::{
	impl_tx_ext_default,
	traits::{
		transaction_extension::TransactionExtension, AsTransactionAuthorizedOrigin, DispatchInfoOf,
		Dispatchable, Verify,
	},
	transaction_validity::{InvalidTransaction, TransactionValidityError, ValidTransaction},
};
use sp_weights::Weight;

/// Extension that, if enabled, validates a signature type against the payload constructed from the
/// call and the rest of the transaction extension pipeline. This extension provides the
/// functionality that traditionally signed transactions had with the implicit signature checking
/// implemented in [`Checkable`](sp_runtime::traits::Checkable). It is meant to be placed ahead of
/// any other extensions that do authorization work in the [`TransactionExtension`] pipeline.
///
/// The variants are ordered so that the [`Disabled`](Self::Disabled) (passthrough) state encodes
/// as the SCALE byte `0x00`, which is compatible with current signers and wallets.
#[derive(Encode, Decode, DecodeWithMemTracking, Clone, Eq, PartialEq, TypeInfo, Default)]
#[scale_info(skip_type_params(T))]
pub enum VerifySignature<T>
where
	T: Config + Send + Sync,
{
	/// The extension is disabled and will be passthrough.
	#[default]
	Disabled,
	/// The extension will verify the signature and, if successful, authorize a traditionally
	/// signed transaction.
	Signed {
		/// The signature provided by the transaction submitter.
		signature: T::Signature,
		/// The account that signed the payload.
		account: T::AccountId,
	},
}

impl<T> core::fmt::Debug for VerifySignature<T>
where
	T: Config + Send + Sync,
{
	#[cfg(feature = "std")]
	fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
		write!(f, "VerifySignature")
	}

	#[cfg(not(feature = "std"))]
	fn fmt(&self, _: &mut core::fmt::Formatter) -> core::fmt::Result {
		Ok(())
	}
}

impl<T> VerifySignature<T>
where
	T: Config + Send + Sync,
{
	/// Create a new extension instance that will validate the provided signature.
	pub fn new_with_signature(signature: T::Signature, account: T::AccountId) -> Self {
		Self::Signed { signature, account }
	}

	/// Create a new passthrough extension instance.
	pub fn new_disabled() -> Self {
		Self::Disabled
	}
}

impl<T> TransactionExtension<T::RuntimeCall> for VerifySignature<T>
where
	T: Config + Send + Sync,
	<T::RuntimeCall as Dispatchable>::RuntimeOrigin: AsTransactionAuthorizedOrigin,
{
	const IDENTIFIER: &'static str = "VerifyMultiSignature";
	type Implicit = ();
	type Val = ();
	type Pre = ();

	fn weight(&self, _call: &T::RuntimeCall) -> Weight {
		match &self {
			// The benchmarked weight of the payload construction and signature checking.
			Self::Signed { .. } => T::WeightInfo::verify_signature(),
			// When the extension is passthrough, it consumes no weight.
			Self::Disabled => Weight::zero(),
		}
	}

	fn validate(
		&self,
		mut origin: <T::RuntimeCall as Dispatchable>::RuntimeOrigin,
		_call: &T::RuntimeCall,
		_info: &DispatchInfoOf<T::RuntimeCall>,
		_len: usize,
		_: (),
		inherited_implication: &impl Encode,
		_source: TransactionSource,
	) -> Result<
		(ValidTransaction, Self::Val, <T::RuntimeCall as Dispatchable>::RuntimeOrigin),
		TransactionValidityError,
	> {
		// If the extension is disabled, return early.
		let (signature, account) = match &self {
			Self::Signed { signature, account } => (signature, account),
			Self::Disabled => return Ok((Default::default(), (), origin)),
		};

		// This extension must receive an unauthorized origin as it is meant to headline the
		// authorization extension pipeline. Any extensions that precede this one must not authorize
		// any origin and serve some other functional purpose.
		if origin.is_transaction_authorized() {
			return Err(InvalidTransaction::BadSigner.into());
		}

		// Construct the payload that the signature will be validated against. The inherited
		// implication contains the encoded bytes of the call and all of the extension data of the
		// extensions that follow in the `TransactionExtension` pipeline.
		//
		// In other words:
		// - extensions that precede this extension are ignored in terms of signature validation;
		// - extensions that follow this extension are included in the payload to be signed (as if
		//   they were the entire `SignedExtension` pipeline in the traditional signed transaction
		//   model).
		//
		// The encoded bytes of the payload are then hashed using `blake2_256`.
		let msg = inherited_implication.using_encoded(blake2_256);

		// The extension was enabled, so the signature must match.
		if !signature.verify(&msg[..], account) {
			Err(InvalidTransaction::BadProof)?
		}

		// Return the signer as the transaction origin.
		origin.set_caller_from_signed(account.clone());
		Ok((ValidTransaction::default(), (), origin))
	}

	impl_tx_ext_default!(T::RuntimeCall; prepare);
}
