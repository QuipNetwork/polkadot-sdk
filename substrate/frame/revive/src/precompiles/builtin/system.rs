// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::{
	Config, H160,
	address::AddressMapper,
	precompiles::{BuiltinAddressMatcher, BuiltinPrecompile, Error, Ext},
	vm::RuntimeCosts,
};
use alloc::vec::Vec;
use alloy_core::sol_types::SolValue;
use codec::Encode;
use core::{marker::PhantomData, num::NonZero};
use pallet_revive_uapi::precompiles::system::ISystem;
use sp_core::hexdisplay::AsBytesRef;

pub struct System<T>(PhantomData<T>);

impl<T: Config> BuiltinPrecompile for System<T> {
	type T = T;
	type Interface = ISystem::ISystemCalls;
	const MATCHER: BuiltinAddressMatcher =
		BuiltinAddressMatcher::Fixed(NonZero::new(0x900).unwrap());
	const HAS_CONTRACT_INFO: bool = false;

	fn call(
		_address: &[u8; 20],
		input: &Self::Interface,
		env: &mut impl Ext<T = Self::T>,
	) -> Result<Vec<u8>, Error> {
		use ISystem::ISystemCalls;
		match input {
			ISystemCalls::terminate(_) if env.is_read_only() => {
				Err(crate::Error::<T>::StateChangeDenied.into())
			},
			ISystemCalls::hashBlake256(ISystem::hashBlake256Call { input }) => {
				env.frame_meter_mut()
					.charge_weight_token(RuntimeCosts::HashBlake256(input.len() as u32))?;
				let output = sp_io::hashing::blake2_256(input.as_bytes_ref());
				Ok(output.abi_encode())
			},
			ISystemCalls::hashBlake128(ISystem::hashBlake128Call { input }) => {
				env.frame_meter_mut()
					.charge_weight_token(RuntimeCosts::HashBlake128(input.len() as u32))?;
				let output = sp_io::hashing::blake2_128(input.as_bytes_ref());
				Ok(output.abi_encode())
			},
			ISystemCalls::toAccountId(ISystem::toAccountIdCall { input }) => {
				env.frame_meter_mut().charge_weight_token(RuntimeCosts::ToAccountId)?;
				let account_id = env.to_account_id(&H160::from_slice(input.as_slice()));
				Ok(account_id.encode().abi_encode())
			},
			ISystemCalls::callerIsOrigin(ISystem::callerIsOriginCall {}) => {
				env.frame_meter_mut().charge_weight_token(RuntimeCosts::CallerIsOrigin)?;
				let is_origin = env.caller_is_origin(true);
				Ok(is_origin.abi_encode())
			},
			ISystemCalls::callerIsRoot(ISystem::callerIsRootCall {}) => {
				env.frame_meter_mut().charge_weight_token(RuntimeCosts::CallerIsRoot)?;
				let is_root = env.caller_is_root(true);
				Ok(is_root.abi_encode())
			},
			ISystemCalls::originIsRoot(ISystem::originIsRootCall {}) => {
				env.frame_meter_mut().charge_weight_token(RuntimeCosts::OriginIsRoot)?;
				let is_root = env.origin_is_root();
				Ok(is_root.abi_encode())
			},
			ISystemCalls::ownCodeHash(ISystem::ownCodeHashCall {}) => {
				env.frame_meter_mut().charge_weight_token(RuntimeCosts::OwnCodeHash)?;
				let caller = env.caller();
				let addr = T::AddressMapper::to_address(caller.account_id()?);
				let output = env.code_hash(&addr.into()).0.abi_encode();
				Ok(output)
			},
			ISystemCalls::minimumBalance(ISystem::minimumBalanceCall {}) => {
				env.frame_meter_mut().charge_weight_token(RuntimeCosts::MinimumBalance)?;
				let minimum_balance = env.minimum_balance();
				Ok(minimum_balance.to_big_endian().abi_encode())
			},
			ISystemCalls::weightLeft(ISystem::weightLeftCall {}) => {
				env.frame_meter_mut().charge_weight_token(RuntimeCosts::WeightLeft)?;
				let ref_time = env.frame_meter().weight_left().unwrap_or_default().ref_time();
				let proof_size = env.frame_meter().weight_left().unwrap_or_default().proof_size();
				let res = (ref_time, proof_size);
				Ok(res.abi_encode())
			},
			ISystemCalls::terminate(ISystem::terminateCall { beneficiary }) => {
				// no need to adjust gas because this always deletes code
				env.frame_meter_mut()
					.charge_weight_token(RuntimeCosts::Terminate { code_removed: true })?;
				let h160 = H160::from_slice(beneficiary.as_slice());
				env.terminate_caller(&h160).map_err(Error::try_to_revert::<T>)?;
				Ok(Vec::new())
			},
			ISystemCalls::sr25519Verify(ISystem::sr25519VerifyCall {
				signature,
				message,
				publicKey,
			}) => {
				env.frame_meter_mut()
					.charge_weight_token(RuntimeCosts::Sr25519Verify(message.len() as _))?;
				let ok = env.sr25519_verify(signature, message, publicKey);
				Ok(ok.abi_encode())
			},
			ISystemCalls::ecdsaToEthAddress(ISystem::ecdsaToEthAddressCall { publicKey }) => {
				env.frame_meter_mut().charge_weight_token(RuntimeCosts::EcdsaToEthAddress)?;
				let address =
					env.ecdsa_to_eth_address(publicKey).map_err(Error::try_to_revert::<T>)?;
				Ok(address.abi_encode())
			},
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		address::AddressMapper,
		call_builder::{CallSetup, caller_funding},
		metering::Token,
		pallet,
		precompiles::{
			BuiltinPrecompile,
			alloy::sol_types::{SolType, sol_data::Bytes},
			tests::run_test_vectors,
		},
		test_utils::ALICE,
		tests::{ExtBuilder, Test},
		vm::RuntimeCosts,
	};

	use alloy_core::primitives::FixedBytes;
	use codec::Decode;
	use frame_support::traits::fungible::Mutate;

	#[test]
	fn test_system_precompile() {
		run_test_vectors::<System<Test>>(include_str!("testdata/900-blake2_256.json"));
		run_test_vectors::<System<Test>>(include_str!("testdata/900-blake2_128.json"));
		run_test_vectors::<System<Test>>(include_str!("testdata/900-to_account_id.json"));
	}

	#[test]
	fn test_system_precompile_unmapped_account() {
		ExtBuilder::default().build().execute_with(|| {
			// given
			let mut call_setup = CallSetup::<Test>::default();
			let (mut ext, _) = call_setup.ext();
			let unmapped_address = H160::zero();

			// when
			let input = ISystem::ISystemCalls::toAccountId(ISystem::toAccountIdCall {
				input: unmapped_address.0.into(),
			});
			let raw_data =
				<System<Test>>::call(&<System<Test>>::MATCHER.base_address(), &input, &mut ext)
					.unwrap();

			// then
			let expected_fallback_account_id =
				Bytes::abi_decode(&raw_data).expect("decoding failed");
			assert_eq!(
				expected_fallback_account_id.0.as_ref()[20..32],
				[0xEE; 12],
				"no fallback suffix found where one should be"
			);
		})
	}

	#[test]
	fn test_system_precompile_mapped_account() {
		use crate::test_utils::EVE;
		ExtBuilder::default().build().execute_with(|| {
			// given
			let mapped_address = {
				<Test as pallet::Config>::Currency::set_balance(&EVE, caller_funding::<Test>());
				let _ = <Test as pallet::Config>::AddressMapper::map(&EVE);
				<Test as pallet::Config>::AddressMapper::to_address(&EVE)
			};

			let mut call_setup = CallSetup::<Test>::default();
			let (mut ext, _) = call_setup.ext();

			// when
			let input = ISystem::ISystemCalls::toAccountId(ISystem::toAccountIdCall {
				input: mapped_address.0.into(),
			});
			let raw_data =
				<System<Test>>::call(&<System<Test>>::MATCHER.base_address(), &input, &mut ext)
					.unwrap();

			// then
			let data = Bytes::abi_decode(&raw_data).expect("decoding failed");
			assert_ne!(
				data.0.as_ref()[20..32],
				[0xEE; 12],
				"fallback suffix found where none should be"
			);
			assert_eq!(
				<Test as frame_system::Config>::AccountId::decode(&mut data.as_ref()),
				Ok(EVE),
			);
		})
	}

	#[test]
	fn sr25519_verify() {
		use crate::precompiles::alloy::sol_types::sol_data::Bool;
		ExtBuilder::default().build().execute_with(|| {
			let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);

			let mut call_setup = CallSetup::<Test>::default();
			let (mut ext, _) = call_setup.ext();

			let mut call_with = |message: &[u8; 11]| {
				// Alice's signature for "hello world"
				#[rustfmt::skip]
				let signature: [u8; 64] = [
					184, 49, 74, 238, 78, 165, 102, 252, 22, 92, 156, 176, 124, 118, 168, 116, 247,
					99, 0, 94, 2, 45, 9, 170, 73, 222, 182, 74, 60, 32, 75, 64, 98, 174, 69, 55, 83,
					85, 180, 98, 208, 75, 231, 57, 205, 62, 4, 105, 26, 136, 172, 17, 123, 99, 90, 255,
					228, 54, 115, 63, 30, 207, 205, 131,
				];

				// Alice's public key
				#[rustfmt::skip]
				let public_key: [u8; 32] = [
					212, 53, 147, 199, 21, 253, 211, 28, 97, 20, 26, 189, 4, 169, 159, 214, 130, 44,
					133, 88, 133, 76, 205, 227, 154, 86, 132, 231, 165, 109, 162, 125,
				];

				let weight_before = ext.frame_meter().weight_consumed();

				let input = ISystem::ISystemCalls::sr25519Verify(ISystem::sr25519VerifyCall {
					signature,
					message: (*message).into(),
					publicKey: public_key.into(),
				});
				let result =
					<System<Test>>::call(&<System<Test>>::MATCHER.base_address(), &input, &mut ext)
						.unwrap();

				let weight_used = ext.frame_meter().weight_consumed() - weight_before;
				assert!(weight_used.ref_time() > 0, "sr25519_verify should charge weight");
				assert_eq!(
					weight_used,
					Token::<Test>::weight(&RuntimeCosts::Sr25519Verify(message.len() as u32)),
					"sr25519_verify should charge the expected weight"
				);
				result
			};
			let result = Bool::abi_decode(&call_with(&b"hello world")).expect("decoding failed");
			assert!(result);
			let result = Bool::abi_decode(&call_with(&b"hello worlD")).expect("decoding failed");
			assert!(!result);
		});
	}

	#[test]
	fn ecdsa_to_eth_address() {
		ExtBuilder::default().build().execute_with(|| {
			let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);

			let mut call_setup = CallSetup::<Test>::default();
			let (mut ext, _) = call_setup.ext();

			let pubkey_compressed = array_bytes::hex2array_unchecked(
				"028db55b05db86c0b1786ca49f095d76344c9e6056b2f02701a7e7f3c20aabfd91",
			);

			let weight_before = ext.frame_meter().weight_consumed();

			let input = ISystem::ISystemCalls::ecdsaToEthAddress(ISystem::ecdsaToEthAddressCall {
				publicKey: pubkey_compressed,
			});
			let result =
				<System<Test>>::call(&<System<Test>>::MATCHER.base_address(), &input, &mut ext)
					.unwrap();

			let expected: FixedBytes<20> = array_bytes::hex2array_unchecked::<_, 20>(
				"09231da7b19A016f9e576d23B16277062F4d46A8",
			)
			.into();
			assert_eq!(result, expected.abi_encode());

			let weight_used = ext.frame_meter().weight_consumed() - weight_before;
			assert!(weight_used.ref_time() > 0, "ecdsa_to_eth_address should charge weight");
			assert_eq!(
				weight_used,
				Token::<Test>::weight(&RuntimeCosts::EcdsaToEthAddress),
				"ecdsa_to_eth_address should charge the expected weight"
			);
		});
	}
}
