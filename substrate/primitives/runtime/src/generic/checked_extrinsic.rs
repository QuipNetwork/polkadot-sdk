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

//! Generic implementation of an extrinsic that has passed the verification
//! stage.

use super::unchecked_extrinsic::ExtensionVersion;
use crate::{
	traits::{
		self, AsTransactionAuthorizedOrigin, DispatchInfoOf, DispatchTransaction, Dispatchable,
		ExtensionVariant, InvalidVersion, MaybeDisplay, Member, Pipeline, PostDispatchInfoOf,
		TransactionExtension,
	},
	transaction_validity::{TransactionSource, TransactionValidity},
};
use codec::Encode;
use sp_weights::Weight;

/// Version 0 of the transaction extension version.
const EXTENSION_V0_VERSION: ExtensionVersion = 0;

/// The kind of extrinsic this is, including any fields required of that kind. This is basically
/// the full extrinsic except the `Call`.
///
/// Bare extrinsics and signed extrinsics are extended with the transaction extension version 0,
/// specified by the generic parameter `ExtensionV0`.
///
/// General extrinsics support multiple transaction extension version, specified by both
/// `ExtensionV0` and `ExtensionOtherVersions`, by default `ExtensionOtherVersions` is set to
/// invalid version, making `ExtensionV0` the only supported version. If you want to support more
/// versions, you need to specify the `ExtensionOtherVersions` parameter.
#[derive(PartialEq, Eq, Clone, Debug)]
pub enum ExtrinsicFormat<AccountId, ExtensionV0, ExtensionOtherVersions = InvalidVersion> {
	/// Extrinsic is bare; it must pass either the bare forms of `TransactionExtension` or
	/// `ValidateUnsigned`, both deprecated, or alternatively a `ProvideInherent`.
	Bare,
	/// Extrinsic has a default `Origin` of `Signed(AccountId)` and must pass all
	/// `TransactionExtension`s regular checks and includes all extension data.
	Signed(AccountId, ExtensionV0),
	/// Extrinsic has a default `Origin` of `None` and must pass all `TransactionExtension`s.
	/// regular checks and includes all extension data.
	General(ExtensionVariant<ExtensionV0, ExtensionOtherVersions>),
}

/// Definition of something that the external world might want to say; its existence implies that it
/// has been checked and is good, particularly with regards to the signature.
///
/// This is typically passed into [`traits::Applyable::apply`], which should execute
/// [`CheckedExtrinsic::function`], alongside all other bits and bobs.
///
/// Bare extrinsics and signed extrinsics are extended with the transaction extension version 0,
/// specified by the generic parameter `ExtensionV0`.
///
/// General extrinsics support multiple transaction extension versions, specified by both
/// `ExtensionV0` and `ExtensionOtherVersions`, by default `ExtensionOtherVersions` is set to
/// invalid version, making `ExtensionV0` the only supported version. If you want to support more
/// versions, you need to specify the `ExtensionOtherVersions` parameter.
#[derive(PartialEq, Eq, Clone, Debug)]
pub struct CheckedExtrinsic<AccountId, Call, ExtensionV0, ExtensionOtherVersions = InvalidVersion> {
	/// Who this purports to be from and the number of extrinsics have come before
	/// from the same signer, if anyone (note this is not a signature).
	pub format: ExtrinsicFormat<AccountId, ExtensionV0, ExtensionOtherVersions>,

	/// The function that should be called.
	pub function: Call,
}

impl<AccountId, Call, ExtensionV0, ExtensionOtherVersions, RuntimeOrigin> traits::Applyable
	for CheckedExtrinsic<AccountId, Call, ExtensionV0, ExtensionOtherVersions>
where
	AccountId: Member + MaybeDisplay,
	Call: Member + Dispatchable<RuntimeOrigin = RuntimeOrigin> + Encode,
	ExtensionV0: TransactionExtension<Call>,
	ExtensionOtherVersions: Pipeline<Call>,
	RuntimeOrigin: From<Option<AccountId>> + AsTransactionAuthorizedOrigin,
{
	type Call = Call;

	#[allow(deprecated)]
	fn validate<I: crate::traits::ValidateUnsigned<Call = Self::Call>>(
		&self,
		source: TransactionSource,
		info: &DispatchInfoOf<Self::Call>,
		len: usize,
	) -> TransactionValidity {
		match self.format {
			ExtrinsicFormat::Bare => {
				let inherent_validation = I::validate_unsigned(source, &self.function)?;
				let legacy_validation = ExtensionV0::bare_validate(&self.function, info, len)?;
				Ok(legacy_validation.combine_with(inherent_validation))
			},
			ExtrinsicFormat::Signed(ref signer, ref extension) => {
				let origin = Some(signer.clone()).into();
				extension
					.validate_only(origin, &self.function, info, len, source, EXTENSION_V0_VERSION)
					.map(|x| x.0)
			},
			ExtrinsicFormat::General(ref extension) => {
				extension.validate_only(None.into(), &self.function, info, len, source)
			},
		}
	}

	#[allow(deprecated)]
	fn apply<I: crate::traits::ValidateUnsigned<Call = Self::Call>>(
		self,
		info: &DispatchInfoOf<Self::Call>,
		len: usize,
	) -> crate::ApplyExtrinsicResultWithInfo<PostDispatchInfoOf<Self::Call>> {
		match self.format {
			ExtrinsicFormat::Bare => {
				I::pre_dispatch(&self.function)?;
				// TODO: Separate logic from `TransactionExtension` into a new `InherentExtension`
				// interface.
				ExtensionV0::bare_validate_and_prepare(&self.function, info, len)?;
				let res = self.function.dispatch(None.into());
				let mut post_info = res.unwrap_or_else(|err| err.post_info);
				let pd_res = res.map(|_| ()).map_err(|e| e.error);
				// TODO: Separate logic from `TransactionExtension` into a new `InherentExtension`
				// interface.
				ExtensionV0::bare_post_dispatch(info, &mut post_info, len, &pd_res)?;
				Ok(res)
			},
			ExtrinsicFormat::Signed(signer, extension) => extension.dispatch_transaction(
				Some(signer).into(),
				self.function,
				info,
				len,
				EXTENSION_V0_VERSION,
			),
			ExtrinsicFormat::General(extension) => {
				extension.dispatch_transaction(None.into(), self.function, info, len)
			},
		}
	}
}

impl<AccountId, Call, ExtensionV0, ExtensionOtherVersions>
	CheckedExtrinsic<AccountId, Call, ExtensionV0, ExtensionOtherVersions>
where
	Call: Dispatchable + Encode,
	ExtensionV0: TransactionExtension<Call>,
	ExtensionOtherVersions: Pipeline<Call>,
	<Call as Dispatchable>::RuntimeOrigin: AsTransactionAuthorizedOrigin,
{
	/// Returns the weight of the extension of this transaction, if present. If the transaction
	/// doesn't use any extension, the weight returned is equal to zero.
	pub fn extension_weight(&self) -> Weight {
		match &self.format {
			ExtrinsicFormat::Bare => Weight::zero(),
			ExtrinsicFormat::Signed(_, ext) => ext.weight(&self.function),
			ExtrinsicFormat::General(ext) => ext.weight(&self.function),
		}
	}
}
