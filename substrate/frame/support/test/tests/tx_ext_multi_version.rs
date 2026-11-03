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

//! Test that FRAME support multiple versions for transaction extensions.

use codec::{Decode, DecodeWithMemTracking, Encode};
use core::fmt::Debug;
use frame_support::{
	derive_impl,
	dispatch::{DispatchInfo, GetDispatchInfo, PostDispatchInfo},
	pallet_prelude::Weight,
};
use scale_info::TypeInfo;
use sp_runtime::{
	generic,
	generic::Preamble,
	testing::UintAuthorityId,
	traits::{
		Applyable, BlakeTwo256, Checkable, ExtensionVariant, IdentityLookup, TransactionExtension,
	},
	transaction_validity::{
		InvalidTransaction, TransactionSource, TransactionValidityError, ValidTransaction,
	},
	DispatchResult,
};

// This transaction extension is valid iff `token != 0`, when valid the resulting origin is
// `frame_system::Origin::Signed(100)`.
#[derive(Clone, Debug, Encode, Decode, DecodeWithMemTracking, PartialEq, Eq, TypeInfo)]
pub struct SimpleExt<const N: u32> {
	pub token: u8,
}

const SIMPLE_EXT_IDS: [&str; 8] = [
	"SimpleExt0",
	"SimpleExt1",
	"SimpleExt2",
	"SimpleExt3",
	"SimpleExt4",
	"SimpleExt5",
	"SimpleExt6",
	"SimpleExt7",
];

impl<const N: u32> TransactionExtension<RuntimeCall> for SimpleExt<N> {
	const IDENTIFIER: &'static str = SIMPLE_EXT_IDS[N as usize];
	type Implicit = ();
	type Val = ();
	type Pre = ();

	fn implicit(&self) -> Result<Self::Implicit, TransactionValidityError> {
		Ok(())
	}

	fn weight(&self, _call: &RuntimeCall) -> Weight {
		Weight::from_parts(N as u64 + 10, 0)
	}

	fn validate(
		&self,
		_origin: RuntimeOrigin,
		_call: &RuntimeCall,
		_info: &DispatchInfo,
		_len: usize,
		_self_implicit: Self::Implicit,
		_inherited_implication: &impl sp_runtime::traits::Implication,
		_source: TransactionSource,
	) -> sp_runtime::traits::ValidateResult<Self::Val, RuntimeCall> {
		if self.token == 0 {
			return Err(InvalidTransaction::Custom(N as u8).into());
		}
		Ok((ValidTransaction::default(), (), frame_system::Origin::<Runtime>::Signed(100).into()))
	}

	fn prepare(
		self,
		_val: Self::Val,
		_origin: &RuntimeOrigin,
		_call: &RuntimeCall,
		_info: &DispatchInfo,
		_len: usize,
	) -> Result<Self::Pre, TransactionValidityError> {
		Ok(())
	}

	fn post_dispatch_details(
		_pre: Self::Pre,
		_info: &DispatchInfo,
		_post_info: &PostDispatchInfo,
		_len: usize,
		_result: &DispatchResult,
	) -> Result<Weight, TransactionValidityError> {
		Ok(Weight::zero())
	}
}

pub type SimpleExtV0 = SimpleExt<0>;
pub type SimpleExtV4 = SimpleExt<4>;
pub type SimpleExtV7 = SimpleExt<7>;

pub type Ext4 = sp_runtime::traits::PipelineAtVers<4, SimpleExtV4>;
pub type Ext7 = sp_runtime::traits::PipelineAtVers<7, SimpleExtV7>;

pub type OtherVersions = sp_runtime::traits::MultiVersion<Ext4, Ext7>;

pub type AccountId = u64;
pub type UncheckedExtrinsic = generic::UncheckedExtrinsic<
	AccountId,
	RuntimeCall,
	UintAuthorityId,
	SimpleExtV0,
	OtherVersions,
>;

pub type BlockNumber = u32;
pub type Block = generic::Block<generic::Header<BlockNumber, BlakeTwo256>, UncheckedExtrinsic>;

#[frame_support::runtime]
mod runtime {
	#[runtime::runtime]
	#[runtime::derive(
		RuntimeCall,
		RuntimeEvent,
		RuntimeError,
		RuntimeOrigin,
		RuntimeFreezeReason,
		RuntimeHoldReason,
		RuntimeSlashReason,
		RuntimeLockId,
		RuntimeTask
	)]
	pub struct Runtime;

	#[runtime::pallet_index(30)]
	pub type System = frame_system;
}

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Runtime {
	type AccountId = AccountId;
	type Lookup = sp_runtime::traits::IdentityLookup<AccountId>;
	type BaseCallFilter = frame_support::traits::Everything;
	type RuntimeOrigin = RuntimeOrigin;
	type RuntimeCall = RuntimeCall;
	type RuntimeEvent = RuntimeEvent;
	type PalletInfo = PalletInfo;
	type OnSetCode = ();
	type Block = Block;
}

#[test]
fn test_metadata() {
	let metadata = Runtime::metadata_ir();

	assert_eq!(
		metadata.extrinsic.extensions_by_version,
		[(0, vec![0]), (4, vec![1]), (7, vec![2]),].into_iter().collect()
	);

	assert_eq!(
		metadata
			.extrinsic
			.extensions_in_versions
			.iter()
			.map(|ext| ext.identifier)
			.collect::<Vec<_>>(),
		vec!["SimpleExt0", "SimpleExt4", "SimpleExt7"]
	);
}

#[test]
fn dispatch_of_valid_extrinsic_succeeds() {
	let mut ext = sp_io::TestExternalities::new(Default::default());
	ext.execute_with(|| {
		// Create an unchecked extrinsic with transaction extension: token=7,
		// This is valid and will result in a signed origin with account id `100`.
		let xt = UncheckedExtrinsic::from_parts(
			RuntimeCall::System(frame_system::Call::remark { remark: vec![1, 2] }),
			Preamble::General(ExtensionVariant::Other(OtherVersions::A(Ext4::new(SimpleExtV4 {
				token: 7,
			})))),
		);

		let len = xt.using_encoded(|e| e.len());
		let info = xt.get_dispatch_info();

		let checked = xt.check(&IdentityLookup::default());
		assert!(checked.is_ok(), "Should produce a valid checked extrinsic");
		let checked = checked.unwrap();

		checked
			.validate::<Runtime>(TransactionSource::External, &info, len)
			.expect("valid");

		checked.apply::<Runtime>(&info, len).expect("valid").expect("success");
	});
}

#[test]
fn dispatch_of_invalid_extrinsic_fails() {
	let mut ext = sp_io::TestExternalities::new(Default::default());
	ext.execute_with(|| {
		// Create an unchecked extrinsic with transaction extension: token=0,
		// This is invalid as `SimpleExt` validation fails for token=0,
		let xt = UncheckedExtrinsic::from_parts(
			RuntimeCall::System(frame_system::Call::remark { remark: vec![1, 2] }),
			Preamble::General(ExtensionVariant::Other(OtherVersions::A(Ext4::new(SimpleExtV4 {
				token: 0,
			})))),
		);

		let len = xt.using_encoded(|e| e.len());
		let info = xt.get_dispatch_info();

		let checked = xt.check(&IdentityLookup::default());
		assert!(checked.is_ok(), "Should produce a valid checked extrinsic");
		let checked = checked.unwrap();

		checked
			.validate::<Runtime>(TransactionSource::External, &info, len)
			.expect_err("invalid, `SimpleExt` is invalid if token == 0");

		checked.apply::<Runtime>(&info, len).expect_err("invalid");
	});
}
