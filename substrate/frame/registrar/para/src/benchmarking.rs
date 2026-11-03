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

//! Benchmarks for `pallet-registrar-para`.

use super::*;
use frame_benchmarking::v2::*;
use frame_support::traits::Get;
use frame_system::RawOrigin;
use registrar_primitives::{MessageToPara, MessageToParaV1};

/// An account able to pay every consideration this pallet can ask for.
fn funded_manager<T: Config>() -> T::AccountId {
	let who: T::AccountId = account("manager", 0, 0);
	T::ReservationConsideration::ensure_successful(&who, Footprint::from_parts(1, 0));
	T::RegistrationConsideration::ensure_successful(
		&who,
		Pallet::<T>::registration_footprint(T::MaxHeadDataSize::get(), T::MaxCodeSize::get()),
	);
	who
}

/// Reserve a para id for `who` and return it.
fn reserve_for<T: Config>(who: &T::AccountId) -> Result<ParaId, BenchmarkError> {
	Pallet::<T>::reserve(RawOrigin::Signed(who.clone()).into())?;
	Ok(NextFreeParaId::<T>::get().saturating_sub(1))
}

/// Reserve a para id and put it into `Pending`.
fn make_pending<T: Config>(who: &T::AccountId) -> Result<ParaId, BenchmarkError> {
	let para_id = reserve_for::<T>(who)?;
	Pallet::<T>::register(
		RawOrigin::Signed(who.clone()).into(),
		para_id,
		alloc::vec![2u8; T::MaxHeadDataSize::get() as usize],
		T::MaxCodeSize::get(),
		sp_core::H256::repeat_byte(1),
	)?;
	Ok(para_id)
}

#[benchmarks]
mod benchmarks {
	use super::*;

	#[benchmark]
	fn reserve() -> Result<(), BenchmarkError> {
		let who = funded_manager::<T>();

		#[extrinsic_call]
		_(RawOrigin::Signed(who.clone()));

		let para_id = NextFreeParaId::<T>::get().saturating_sub(1);
		assert_eq!(Paras::<T>::get(para_id).map(|i| i.manager), Some(who));
		Ok(())
	}

	/// Requesting a registration. Dominated by shipping the head data to the relay chain.
	#[benchmark]
	fn register(h: Linear<0, { T::MaxHeadDataSize::get() }>) -> Result<(), BenchmarkError> {
		let who = funded_manager::<T>();
		let para_id = reserve_for::<T>(&who)?;

		#[extrinsic_call]
		_(
			RawOrigin::Signed(who),
			para_id,
			alloc::vec![2u8; h as usize],
			T::MaxCodeSize::get(),
			sp_core::H256::repeat_byte(1),
		);

		assert!(matches!(
			Paras::<T>::get(para_id).map(|i| i.state),
			Some(RegistrationState::Pending { .. })
		));
		Ok(())
	}

	/// Asking the relay chain to drop an authorization. The deposit stays held, so this is the
	/// state write plus the message.
	#[benchmark]
	fn cancel_registration() -> Result<(), BenchmarkError> {
		let who = funded_manager::<T>();
		let para_id = make_pending::<T>(&who)?;
		T::BlockNumberProvider::set_block_number(
			T::BlockNumberProvider::current_block_number()
				.saturating_add(T::PendingDeadline::get())
				.saturating_add(1u32.into()),
		);

		#[extrinsic_call]
		_(RawOrigin::Signed(who), para_id);

		assert!(matches!(
			Paras::<T>::get(para_id).map(|i| i.state),
			Some(RegistrationState::Pending { .. })
		));
		Ok(())
	}

	/// The worst case of the messages this call serves is a confirmed cancellation, which releases
	/// the deposit on top of writing the new state.
	#[benchmark]
	fn receive() -> Result<(), BenchmarkError> {
		let who = funded_manager::<T>();
		let para_id = make_pending::<T>(&who)?;
		let message = MessageToPara::V1(MessageToParaV1::CancelResponse {
			para_id,
			message_id: 0,
			outcome: Ok(()),
		});

		#[extrinsic_call]
		_(RawOrigin::Root, message);

		assert_eq!(Paras::<T>::get(para_id).map(|i| i.state), Some(RegistrationState::Reserved));
		Ok(())
	}

	impl_benchmark_test_suite!(Pallet, crate::mock::new_test_ext(), crate::mock::Test);
}
