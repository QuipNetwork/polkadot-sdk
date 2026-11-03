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

//! Bounties pallet benchmarking.

use super::*;

use alloc::{vec, vec::Vec};
use frame_benchmarking::v2::*;
use frame_system::{pallet_prelude::BlockNumberFor as SystemBlockNumberFor, RawOrigin};
use sp_runtime::traits::{BlockNumberProvider, Bounded};

use crate::Pallet as Bounties;
use pallet_treasury::Pallet as Treasury;

const SEED: u32 = 0;

fn set_block_number<T: Config<I>, I: 'static>(n: BlockNumberFor<T, I>) {
	<T as pallet_treasury::Config<I>>::BlockNumberProvider::set_block_number(n);
}

fn minimum_balance<T: Config<I>, I: 'static>() -> BalanceOf<T, I> {
	let minimum_balance = T::Currency::minimum_balance();

	if minimum_balance.is_zero() {
		1u32.into()
	} else {
		minimum_balance
	}
}

// Create bounties that are approved for use in `on_initialize`.
fn create_approved_bounties<T: Config<I>, I: 'static>(n: u32) -> Result<(), BenchmarkError> {
	for i in 0..n {
		let (caller, _curator, _fee, value, reason) =
			setup_bounty::<T, I>(i, T::MaximumReasonLength::get());
		Bounties::<T, I>::propose_bounty(RawOrigin::Signed(caller).into(), value, reason)?;
		let bounty_id = BountyCount::<T, I>::get() - 1;
		let approve_origin =
			T::SpendOrigin::try_successful_origin().map_err(|_| BenchmarkError::Weightless)?;
		Bounties::<T, I>::approve_bounty(approve_origin, bounty_id)?;
	}
	ensure!(BountyApprovals::<T, I>::get().len() == n as usize, "Not all bounty approved");
	Ok(())
}

// Create the pre-requisite information needed to create a treasury `propose_bounty`.
fn setup_bounty<T: Config<I>, I: 'static>(
	u: u32,
	d: u32,
) -> (T::AccountId, T::AccountId, BalanceOf<T, I>, BalanceOf<T, I>, Vec<u8>) {
	let caller = account("caller", u, SEED);
	let value: BalanceOf<T, I> = T::BountyValueMinimum::get().saturating_mul(100u32.into());
	let fee = value / 2u32.into();
	let deposit = T::BountyDepositBase::get() +
		T::DataDepositPerByte::get() * T::MaximumReasonLength::get().into();
	let _ = T::Currency::make_free_balance_be(&caller, deposit + minimum_balance::<T, I>());
	let curator = account("curator", u, SEED);
	let _ =
		T::Currency::make_free_balance_be(&curator, fee / 2u32.into() + minimum_balance::<T, I>());
	let reason = vec![0; d as usize];
	(caller, curator, fee, value, reason)
}

fn create_bounty<T: Config<I>, I: 'static>(
) -> Result<(AccountIdLookupOf<T>, BountyIndex), BenchmarkError> {
	let (caller, curator, fee, value, reason) =
		setup_bounty::<T, I>(0, T::MaximumReasonLength::get());
	let curator_lookup = T::Lookup::unlookup(curator.clone());
	Bounties::<T, I>::propose_bounty(RawOrigin::Signed(caller).into(), value, reason)?;
	let bounty_id = BountyCount::<T, I>::get() - 1;
	let approve_origin =
		T::SpendOrigin::try_successful_origin().map_err(|_| BenchmarkError::Weightless)?;
	Bounties::<T, I>::approve_bounty(approve_origin.clone(), bounty_id)?;
	set_block_number::<T, I>(T::SpendPeriod::get());
	Treasury::<T, I>::on_initialize(frame_system::Pallet::<T>::block_number());
	Bounties::<T, I>::propose_curator(approve_origin, bounty_id, curator_lookup.clone(), fee)?;
	Bounties::<T, I>::accept_curator(RawOrigin::Signed(curator).into(), bounty_id)?;
	Ok((curator_lookup, bounty_id))
}

fn setup_pot_account<T: Config<I>, I: 'static>() {
	let pot_account = Bounties::<T, I>::account_id();
	let value = minimum_balance::<T, I>().saturating_mul(1_000_000_000u32.into());
	let _ = T::Currency::make_free_balance_be(&pot_account, value);
}

fn assert_last_event<T: Config<I>, I: 'static>(generic_event: <T as Config<I>>::RuntimeEvent) {
	frame_system::Pallet::<T>::assert_last_event(generic_event.into());
}

#[instance_benchmarks]
mod benchmarks {
	use super::*;

	#[benchmark]
	fn propose_bounty(d: Linear<0, { T::MaximumReasonLength::get() }>) {
		let (caller, _, _, value, description) = setup_bounty::<T, I>(0, d);

		#[extrinsic_call]
		_(RawOrigin::Signed(caller), value, description);
	}

	#[benchmark]
	fn approve_bounty() -> Result<(), BenchmarkError> {
		let (caller, _, _, value, reason) = setup_bounty::<T, I>(0, T::MaximumReasonLength::get());
		Bounties::<T, I>::propose_bounty(RawOrigin::Signed(caller).into(), value, reason)?;
		let bounty_id = BountyCount::<T, I>::get() - 1;
		let approve_origin =
			T::SpendOrigin::try_successful_origin().map_err(|_| BenchmarkError::Weightless)?;

		#[extrinsic_call]
		_(approve_origin as T::RuntimeOrigin, bounty_id);

		Ok(())
	}

	#[benchmark]
	fn propose_curator() -> Result<(), BenchmarkError> {
		setup_pot_account::<T, I>();
		let (caller, curator, fee, value, reason) =
			setup_bounty::<T, I>(0, T::MaximumReasonLength::get());
		let curator_lookup = T::Lookup::unlookup(curator);
		Bounties::<T, I>::propose_bounty(RawOrigin::Signed(caller).into(), value, reason)?;
		let bounty_id = BountyCount::<T, I>::get() - 1;
		let approve_origin =
			T::SpendOrigin::try_successful_origin().map_err(|_| BenchmarkError::Weightless)?;
		Bounties::<T, I>::approve_bounty(approve_origin.clone(), bounty_id)?;
		set_block_number::<T, I>(T::SpendPeriod::get());
		Treasury::<T, I>::on_initialize(frame_system::Pallet::<T>::block_number());

		#[extrinsic_call]
		_(approve_origin as T::RuntimeOrigin, bounty_id, curator_lookup, fee);

		Ok(())
	}

	#[benchmark]
	fn approve_bounty_with_curator() -> Result<(), BenchmarkError> {
		setup_pot_account::<T, I>();
		let (caller, curator, fee, value, reason) =
			setup_bounty::<T, I>(0, T::MaximumReasonLength::get());
		let curator_lookup = T::Lookup::unlookup(curator.clone());
		Bounties::<T, I>::propose_bounty(RawOrigin::Signed(caller).into(), value, reason)?;
		let bounty_id = BountyCount::<T, I>::get() - 1;
		let approve_origin =
			T::SpendOrigin::try_successful_origin().map_err(|_| BenchmarkError::Weightless)?;
		Treasury::<T, I>::on_initialize(SystemBlockNumberFor::<T>::zero());

		#[extrinsic_call]
		_(approve_origin as T::RuntimeOrigin, bounty_id, curator_lookup, fee);

		assert_last_event::<T, I>(Event::CuratorProposed { bounty_id, curator }.into());

		Ok(())
	}

	// Worst case when curator is inactive and any sender unassigns the curator,
	// or if `BountyUpdatePeriod` is large enough and `RejectOrigin` executes the call.
	#[benchmark]
	fn unassign_curator() -> Result<(), BenchmarkError> {
		setup_pot_account::<T, I>();
		let (curator_lookup, _) = create_bounty::<T, I>()?;
		Treasury::<T, I>::on_initialize(frame_system::Pallet::<T>::block_number());
		let bounty_id = BountyCount::<T, I>::get() - 1;
		let bounty_update_period = T::BountyUpdatePeriod::get();
		let inactivity_timeout = T::SpendPeriod::get().saturating_add(bounty_update_period);
		set_block_number::<T, I>(inactivity_timeout.saturating_add(2u32.into()));

		// If `BountyUpdatePeriod` overflows the inactivity timeout the benchmark still executes the
		// slash
		let origin = if Pallet::<T, I>::treasury_block_number() <= inactivity_timeout {
			let curator = T::Lookup::lookup(curator_lookup).map_err(<&str>::from)?;
			T::RejectOrigin::try_successful_origin()
				.unwrap_or_else(|_| RawOrigin::Signed(curator).into())
		} else {
			let caller = whitelisted_caller();
			RawOrigin::Signed(caller).into()
		};

		#[extrinsic_call]
		_(origin as T::RuntimeOrigin, bounty_id);

		Ok(())
	}

	#[benchmark]
	fn accept_curator() -> Result<(), BenchmarkError> {
		setup_pot_account::<T, I>();
		let (caller, curator, fee, value, reason) =
			setup_bounty::<T, I>(0, T::MaximumReasonLength::get());
		let curator_lookup = T::Lookup::unlookup(curator.clone());
		Bounties::<T, I>::propose_bounty(RawOrigin::Signed(caller).into(), value, reason)?;
		let bounty_id = BountyCount::<T, I>::get() - 1;
		let approve_origin =
			T::SpendOrigin::try_successful_origin().map_err(|_| BenchmarkError::Weightless)?;
		Bounties::<T, I>::approve_bounty(approve_origin.clone(), bounty_id)?;
		set_block_number::<T, I>(T::SpendPeriod::get());
		Treasury::<T, I>::on_initialize(frame_system::Pallet::<T>::block_number());
		Bounties::<T, I>::propose_curator(approve_origin, bounty_id, curator_lookup, fee)?;

		#[extrinsic_call]
		_(RawOrigin::Signed(curator), bounty_id);

		Ok(())
	}

	#[benchmark]
	fn award_bounty() -> Result<(), BenchmarkError> {
		setup_pot_account::<T, I>();
		let (curator_lookup, _) = create_bounty::<T, I>()?;
		Treasury::<T, I>::on_initialize(frame_system::Pallet::<T>::block_number());

		let bounty_id = BountyCount::<T, I>::get() - 1;
		let curator = T::Lookup::lookup(curator_lookup).map_err(<&str>::from)?;

		let beneficiary = T::Lookup::unlookup(account("beneficiary", 0, SEED));

		#[extrinsic_call]
		_(RawOrigin::Signed(curator), bounty_id, beneficiary);

		Ok(())
	}

	#[benchmark]
	fn claim_bounty() -> Result<(), BenchmarkError> {
		setup_pot_account::<T, I>();
		let (curator_lookup, _) = create_bounty::<T, I>()?;
		Treasury::<T, I>::on_initialize(frame_system::Pallet::<T>::block_number());

		let bounty_id = BountyCount::<T, I>::get() - 1;
		let curator = T::Lookup::lookup(curator_lookup).map_err(<&str>::from)?;

		let beneficiary_account: T::AccountId = account("beneficiary", 0, SEED);
		let beneficiary = T::Lookup::unlookup(beneficiary_account.clone());
		Bounties::<T, I>::award_bounty(
			RawOrigin::Signed(curator.clone()).into(),
			bounty_id,
			beneficiary,
		)?;

		set_block_number::<T, I>(
			T::SpendPeriod::get() + T::BountyDepositPayoutDelay::get() + 1u32.into(),
		);
		assert!(
			T::Currency::free_balance(&beneficiary_account).is_zero(),
			"Beneficiary already has balance"
		);

		#[extrinsic_call]
		_(RawOrigin::Signed(curator), bounty_id);

		assert!(
			!T::Currency::free_balance(&beneficiary_account).is_zero(),
			"Beneficiary didn't get paid"
		);

		Ok(())
	}

	#[benchmark]
	fn close_bounty_proposed() -> Result<(), BenchmarkError> {
		setup_pot_account::<T, I>();
		let (caller, _, _, value, reason) = setup_bounty::<T, I>(0, 0);
		Bounties::<T, I>::propose_bounty(RawOrigin::Signed(caller).into(), value, reason)?;
		let bounty_id = BountyCount::<T, I>::get() - 1;
		let approve_origin =
			T::RejectOrigin::try_successful_origin().map_err(|_| BenchmarkError::Weightless)?;

		#[extrinsic_call]
		close_bounty(approve_origin, bounty_id);

		Ok(())
	}

	#[benchmark]
	fn close_bounty_active() -> Result<(), BenchmarkError> {
		setup_pot_account::<T, I>();
		let (_, _) = create_bounty::<T, I>()?;
		Treasury::<T, I>::on_initialize(frame_system::Pallet::<T>::block_number());
		let bounty_id = BountyCount::<T, I>::get() - 1;
		let approve_origin =
			T::RejectOrigin::try_successful_origin().map_err(|_| BenchmarkError::Weightless)?;

		#[extrinsic_call]
		close_bounty(approve_origin as T::RuntimeOrigin, bounty_id);

		assert_last_event::<T, I>(Event::BountyCanceled { index: bounty_id }.into());

		Ok(())
	}

	#[benchmark]
	fn extend_bounty_expiry() -> Result<(), BenchmarkError> {
		setup_pot_account::<T, I>();
		let (curator_lookup, _) = create_bounty::<T, I>()?;
		Treasury::<T, I>::on_initialize(frame_system::Pallet::<T>::block_number());

		let bounty_id = BountyCount::<T, I>::get() - 1;
		let curator = T::Lookup::lookup(curator_lookup).map_err(<&str>::from)?;

		#[extrinsic_call]
		_(RawOrigin::Signed(curator), bounty_id, Vec::new());

		assert_last_event::<T, I>(Event::BountyExtended { index: bounty_id }.into());

		Ok(())
	}

	#[benchmark]
	fn spend_funds(b: Linear<0, 100>) -> Result<(), BenchmarkError> {
		setup_pot_account::<T, I>();
		create_approved_bounties::<T, I>(b)?;

		let mut budget_remaining = BalanceOf::<T, I>::max_value();
		let mut imbalance = PositiveImbalanceOf::<T, I>::zero();
		let mut total_weight = Weight::zero();
		let mut missed_any = false;

		#[block]
		{
			<Bounties<T, I> as pallet_treasury::SpendFunds<T, I>>::spend_funds(
				&mut budget_remaining,
				&mut imbalance,
				&mut total_weight,
				&mut missed_any,
			);
		}

		assert!(!missed_any, "Missed some");

		if b > 0 {
			assert!(budget_remaining < BalanceOf::<T, I>::max_value(), "Budget not used");
			assert_last_event::<T, I>(Event::BountyBecameActive { index: b - 1 }.into());
		} else {
			assert!(budget_remaining == BalanceOf::<T, I>::max_value(), "Budget used");
		}

		Ok(())
	}

	#[benchmark]
	fn poke_deposit() -> Result<(), BenchmarkError> {
		// Create a bounty
		let (caller, _, _, value, reason) = setup_bounty::<T, I>(0, 5); // 5 bytes description
		Bounties::<T, I>::propose_bounty(RawOrigin::Signed(caller.clone()).into(), value, reason)?;
		let bounty_id = BountyCount::<T, I>::get() - 1;
		let old_deposit = T::Currency::reserved_balance(&caller);
		// Modify the description to be maximum length
		let max_description: Vec<u8> = vec![0; T::MaximumReasonLength::get() as usize];
		let bounded_description: BoundedVec<u8, T::MaximumReasonLength> =
			max_description.try_into().unwrap();
		BountyDescriptions::<T, I>::insert(bounty_id, &bounded_description);

		// Ensure caller has enough balance for new deposit
		let new_deposit = Bounties::<T, I>::calculate_bounty_deposit(&bounded_description);
		let required_balance = new_deposit.saturating_add(minimum_balance::<T, I>());
		T::Currency::make_free_balance_be(&caller, required_balance);

		#[extrinsic_call]
		_(RawOrigin::Signed(caller.clone()), bounty_id);

		let bounty = crate::Bounties::<T, I>::get(bounty_id).unwrap();
		assert_eq!(bounty.bond, new_deposit);
		assert_eq!(T::Currency::reserved_balance(&caller), new_deposit);
		assert_last_event::<T, I>(
			Event::DepositPoked { bounty_id, proposer: caller, old_deposit, new_deposit }.into(),
		);

		Ok(())
	}

	#[benchmark]
	fn reclaim_bounty_funds() -> Result<(), BenchmarkError> {
		setup_pot_account::<T, I>();

		let (caller, _, _, value, reason) = setup_bounty::<T, I>(0, T::MaximumReasonLength::get());
		Bounties::<T, I>::propose_bounty(RawOrigin::Signed(caller).into(), value, reason)?;
		let bounty_id = BountyCount::<T, I>::get() - 1;
		let approve_origin =
			T::SpendOrigin::try_successful_origin().map_err(|_| BenchmarkError::Weightless)?;
		Bounties::<T, I>::approve_bounty(approve_origin.clone(), bounty_id)?;
		set_block_number::<T, I>(T::SpendPeriod::get());
		Treasury::<T, I>::on_initialize(frame_system::Pallet::<T>::block_number());

		let bounty_account = Bounties::<T, I>::bounty_account_id(bounty_id);
		// Drop the bounty record so its account counts as stale.
		crate::Bounties::<T, I>::remove(bounty_id);
		BountyDescriptions::<T, I>::remove(bounty_id);

		// Mint all relevant assets into the bounty account so the benchmark exercises real
		// transfers instead of being a no-op for runtimes with non-trivial TransferAllAssets.
		T::TransferAllAssets::ensure_successful(&bounty_account);

		assert!(
			!T::Currency::free_balance(&bounty_account).is_zero(),
			"Bounty account should have a balance to reclaim"
		);

		let caller: T::AccountId = whitelisted_caller();

		#[extrinsic_call]
		_(RawOrigin::Signed(caller), bounty_id);

		assert_last_event::<T, I>(Event::BountyFundsReclaimed { bounty_id }.into());

		Ok(())
	}
	impl_benchmark_test_suite!(
		Bounties,
		crate::tests::ExtBuilder::default().build(),
		crate::tests::Test
	);
}
