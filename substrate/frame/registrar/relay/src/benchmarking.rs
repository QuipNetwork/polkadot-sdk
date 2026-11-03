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

//! Benchmarks for `pallet-registrar-relay`.

use super::*;
use frame_benchmarking::v2::*;
use frame_support::traits::Get;
use frame_system::RawOrigin;
use registrar_primitives::{MessageToRelay, MessageToRelayV1};
use sp_runtime::traits::{BlakeTwo256, Hash};

/// A para id no benchmark setup will collide on.
const PARA_ID: ParaId = 4_242;

fn code_of(len: u32) -> Vec<u8> {
	alloc::vec![1u8; len as usize]
}

/// Park a pending registration for `PARA_ID` expecting exactly `code`.
///
/// Written straight to storage rather than pushed through [`Pallet::authorize_code`]: going
/// through the call would put the whole range at the mercy of whatever minimum code size the
/// configured [`ParachainRegistrar`] enforces, and `c = 0` would fail setup instead of measuring
/// anything.
fn park<T: Config>(code: &[u8]) -> Result<(), BenchmarkError> {
	let genesis_head = alloc::vec![2u8; T::MaxHeadDataSize::get() as usize];
	let pending = PendingRegistration {
		message_id: 0,
		manager: account("manager", 0, 0),
		genesis_head: genesis_head.try_into().map_err(|_| "head data exceeds its own bound")?,
		code_hash: BlakeTwo256::hash(code),
		code_len: code.len() as u32,
	};

	PendingRegistrations::<T>::insert(PARA_ID, pending);
	Ok(())
}

#[benchmarks]
mod benchmarks {
	use super::*;

	/// Accepting a registration request. Dominated by writing the head data.
	#[benchmark]
	fn authorize_code(h: Linear<0, { T::MaxHeadDataSize::get() }>) -> Result<(), BenchmarkError> {
		let manager: T::AccountId = account("manager", 0, 0);
		let code = code_of(T::MaxCodeSize::get());
		let message = MessageToRelay::V1(MessageToRelayV1::Register {
			para_id: PARA_ID,
			message_id: 0,
			manager,
			genesis_head: alloc::vec![2u8; h as usize],
			code_hash: BlakeTwo256::hash(&code),
			code_len: code.len() as u32,
		});

		#[extrinsic_call]
		authorize_code(RawOrigin::Root, message);

		assert!(PendingRegistrations::<T>::contains_key(PARA_ID));
		Ok(())
	}

	/// Uploading the validation code. Dominated by hashing and onboarding the blob.
	#[benchmark]
	fn apply_authorized_code(
		c: Linear<0, { T::MaxCodeSize::get() }>,
	) -> Result<(), BenchmarkError> {
		let code = code_of(c);
		park::<T>(&code)?;

		#[extrinsic_call]
		_(RawOrigin::Authorized, PARA_ID, code);

		assert!(!PendingRegistrations::<T>::contains_key(PARA_ID));
		Ok(())
	}

	/// Deciding whether an unsigned `apply_authorized_code` may enter the pool.
	///
	/// This runs on every node for every candidate transaction, so it is the number that keeps
	/// the free call from being a cheap way to make everyone hash megabytes.
	#[benchmark]
	fn authorize_apply_authorized_code(
		c: Linear<0, { T::MaxCodeSize::get() }>,
	) -> Result<(), BenchmarkError> {
		let code = code_of(c);
		park::<T>(&code)?;
		let call = Call::<T>::apply_authorized_code { para_id: PARA_ID, validation_code: code };

		#[block]
		{
			use frame_support::pallet_prelude::Authorize;
			call.authorize(sp_runtime::transaction_validity::TransactionSource::External)
				.ok_or("call must give some authorization")??;
		}

		Ok(())
	}

	/// Dropping an authorization. The worst case carries the largest head data, since that is what
	/// the entry being removed holds.
	#[benchmark]
	fn cancel_authorization() -> Result<(), BenchmarkError> {
		park::<T>(&code_of(T::MaxCodeSize::get()))?;
		let message = MessageToRelay::V1(MessageToRelayV1::CancelRegistration {
			para_id: PARA_ID,
			message_id: 0,
		});

		#[extrinsic_call]
		_(RawOrigin::Root, message);

		assert!(!PendingRegistrations::<T>::contains_key(PARA_ID));
		Ok(())
	}

	impl_benchmark_test_suite!(Pallet, crate::mock::new_test_ext(), crate::mock::Test);
}
