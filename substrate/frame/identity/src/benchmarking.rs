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

//! Identity pallet benchmarking.

#![cfg(feature = "runtime-benchmarks")]

use super::*;

use crate::{migration::v2::LazyMigrationV1ToV2, Pallet as Identity};
use alloc::{vec, vec::Vec};
use frame_benchmarking::{account, v2::*, whitelisted_caller, BenchmarkError};
use frame_support::{
	assert_ok, ensure,
	traits::{EnsureOrigin, Get, OnFinalize, OnInitialize},
};
use frame_system::RawOrigin;
use sp_runtime::traits::{Bounded, One};

const SEED: u32 = 0;

fn assert_has_event<T: Config>(generic_event: <T as Config>::RuntimeEvent) {
	frame_system::Pallet::<T>::assert_has_event(generic_event.into());
}

fn assert_last_event<T: Config>(generic_event: <T as Config>::RuntimeEvent) {
	frame_system::Pallet::<T>::assert_last_event(generic_event.into());
}

fn run_to_block<T: Config>(n: frame_system::pallet_prelude::BlockNumberFor<T>) {
	while frame_system::Pallet::<T>::block_number() < n {
		crate::Pallet::<T>::on_finalize(frame_system::Pallet::<T>::block_number());
		frame_system::Pallet::<T>::on_finalize(frame_system::Pallet::<T>::block_number());
		frame_system::Pallet::<T>::set_block_number(
			frame_system::Pallet::<T>::block_number() + One::one(),
		);
		frame_system::Pallet::<T>::on_initialize(frame_system::Pallet::<T>::block_number());
		crate::Pallet::<T>::on_initialize(frame_system::Pallet::<T>::block_number());
	}
}

// Adds `r` registrars to the Identity Pallet. These registrars will have set fees and fields.
fn add_registrars<T: Config>(r: u32) -> Result<(), &'static str> {
	for i in 0..r {
		let registrar: T::AccountId = account("registrar", i, SEED);
		let registrar_lookup = T::Lookup::unlookup(registrar.clone());
		let _ = T::Currency::make_free_balance_be(&registrar, BalanceOf::<T>::max_value());
		let registrar_origin = T::RegistrarOrigin::try_successful_origin()
			.expect("RegistrarOrigin has no successful origin required for the benchmark");
		Identity::<T>::add_registrar(registrar_origin, registrar_lookup)?;
		Identity::<T>::set_fee(RawOrigin::Signed(registrar.clone()).into(), i, 10u32.into())?;
		let fields = T::IdentityInformation::all_fields();
		Identity::<T>::set_fields(RawOrigin::Signed(registrar.clone()).into(), i, fields)?;
	}

	assert_eq!(Registrars::<T>::get().len(), r as usize);
	Ok(())
}

// Create `s` sub-accounts for the identity of `who` and return them.
// Each will have 32 bytes of raw data added to it.
fn create_sub_accounts<T: Config>(
	who: &T::AccountId,
	s: u32,
) -> Result<Vec<(T::AccountId, Data)>, &'static str> {
	let mut subs = Vec::new();
	let who_origin = RawOrigin::Signed(who.clone());
	let data = Data::Raw(vec![0; 32].try_into().unwrap());

	for i in 0..s {
		let sub_account = account("sub", i, SEED);
		subs.push((sub_account, data.clone()));
	}

	// Set identity so `set_subs` does not fail.
	if IdentityOf::<T>::get(who).is_none() {
		let _ = T::Currency::make_free_balance_be(who, BalanceOf::<T>::max_value() / 2u32.into());
		let info = T::IdentityInformation::create_identity_info();
		Identity::<T>::set_identity(who_origin.into(), Box::new(info))?;
	}

	Ok(subs)
}

// Adds `s` sub-accounts to the identity of `who`. Each will have 32 bytes of raw data added to it.
// This additionally returns the vector of sub-accounts so it can be modified if needed.
fn add_sub_accounts<T: Config>(
	who: &T::AccountId,
	s: u32,
) -> Result<Vec<(T::AccountId, Data)>, &'static str> {
	let who_origin = RawOrigin::Signed(who.clone());
	let subs = create_sub_accounts::<T>(who, s)?;

	Identity::<T>::set_subs(who_origin.into(), subs.clone())?;

	Ok(subs)
}

fn bench_suffix() -> Vec<u8> {
	b"bench".to_vec()
}

fn bench_username() -> Vec<u8> {
	// len = 24
	b"veryfastbenchmarkmachine".to_vec()
}

fn bounded_username<T: Config>(username: Vec<u8>, suffix: Vec<u8>) -> Username<T> {
	let mut full_username = Vec::with_capacity(username.len() + suffix.len() + 1);
	full_username.extend(username);
	full_username.extend(b".");
	full_username.extend(suffix);
	Username::<T>::try_from(full_username).expect("test usernames should fit within bounds")
}

#[benchmarks]
mod benchmarks {
	use super::*;

	#[benchmark]
	fn add_registrar(r: Linear<1, { T::MaxRegistrars::get() - 1 }>) -> Result<(), BenchmarkError> {
		add_registrars::<T>(r)?;
		ensure!(Registrars::<T>::get().len() as u32 == r, "Registrars not set up correctly.");
		let origin =
			T::RegistrarOrigin::try_successful_origin().map_err(|_| BenchmarkError::Weightless)?;
		let account = T::Lookup::unlookup(account("registrar", r + 1, SEED));

		#[extrinsic_call]
		_(origin as T::RuntimeOrigin, account);

		ensure!(Registrars::<T>::get().len() as u32 == r + 1, "Registrars not added.");
		Ok(())
	}

	#[benchmark]
	fn remove_registrar(r: Linear<1, { T::MaxRegistrars::get() }>) -> Result<(), BenchmarkError> {
		add_registrars::<T>(r)?;
		ensure!(Registrars::<T>::get().len() as u32 == r, "Registrars not set up correctly.");
		let origin =
			T::RegistrarOrigin::try_successful_origin().map_err(|_| BenchmarkError::Weightless)?;
		// Remove the last registrar
		let index = r - 1;

		#[extrinsic_call]
		_(origin as T::RuntimeOrigin, index);

		ensure!(
			Registrars::<T>::get().get(index as usize).map_or(false, Option::is_none),
			"Registrar not removed.",
		);
		Ok(())
	}

	#[benchmark]
	fn set_identity(r: Linear<1, { T::MaxRegistrars::get() }>) -> Result<(), BenchmarkError> {
		add_registrars::<T>(r)?;

		let caller: T::AccountId = whitelisted_caller();
		let caller_lookup = T::Lookup::unlookup(caller.clone());
		let caller_origin: <T as frame_system::Config>::RuntimeOrigin =
			RawOrigin::Signed(caller.clone()).into();
		let _ = T::Currency::make_free_balance_be(&caller, BalanceOf::<T>::max_value());

		// Add an initial identity
		let initial_info = T::IdentityInformation::create_identity_info();
		Identity::<T>::set_identity(caller_origin.clone(), Box::new(initial_info.clone()))?;

		// User requests judgement from all the registrars, and they approve
		for i in 0..r {
			let registrar: T::AccountId = account("registrar", i, SEED);
			let _ = T::Lookup::unlookup(registrar.clone());
			let balance_to_use = T::Currency::minimum_balance() * 10u32.into();
			let _ = T::Currency::make_free_balance_be(&registrar, balance_to_use);

			Identity::<T>::request_judgement(caller_origin.clone(), i, 10u32.into())?;
			Identity::<T>::provide_judgement(
				RawOrigin::Signed(registrar).into(),
				i,
				caller_lookup.clone(),
				Judgement::Reasonable,
				T::Hashing::hash_of(&initial_info),
			)?;
		}

		#[extrinsic_call]
		_(
			RawOrigin::Signed(caller.clone()),
			Box::new(T::IdentityInformation::create_identity_info()),
		);

		assert_last_event::<T>(Event::<T>::IdentitySet { who: caller }.into());
		Ok(())
	}

	// We need to split `set_subs` into two benchmarks to accurately isolate the potential
	// writes caused by new or old sub accounts. The actual weight should simply be
	// the sum of these two weights.
	#[benchmark]
	fn set_subs_new(s: Linear<0, { T::MaxSubAccounts::get() }>) -> Result<(), BenchmarkError> {
		let caller: T::AccountId = whitelisted_caller();

		// Create a new subs vec with sub accounts
		let subs = create_sub_accounts::<T>(&caller, s)?;
		ensure!(SubsOf::<T>::get(&caller).1.len() == 0, "Caller already has subs");

		#[extrinsic_call]
		set_subs(RawOrigin::Signed(caller.clone()), subs);

		ensure!(SubsOf::<T>::get(&caller).1.len() as u32 == s, "Subs not added");
		Ok(())
	}

	#[benchmark]
	fn set_subs_old(p: Linear<0, { T::MaxSubAccounts::get() }>) -> Result<(), BenchmarkError> {
		let caller: T::AccountId = whitelisted_caller();

		// Give them p many previous sub accounts.
		add_sub_accounts::<T>(&caller, p)?;

		// Remove all subs.
		let subs = create_sub_accounts::<T>(&caller, 0)?;
		ensure!(SubsOf::<T>::get(&caller).1.len() as u32 == p, "Caller does have subs",);

		#[extrinsic_call]
		set_subs(RawOrigin::Signed(caller.clone()), subs);

		ensure!(SubsOf::<T>::get(&caller).1.len() == 0, "Subs not removed");
		Ok(())
	}

	#[benchmark]
	fn clear_identity(
		r: Linear<1, { T::MaxRegistrars::get() }>,
		s: Linear<0, { T::MaxSubAccounts::get() }>,
	) -> Result<(), BenchmarkError> {
		let caller: T::AccountId = whitelisted_caller();
		let caller_origin =
			<T as frame_system::Config>::RuntimeOrigin::from(RawOrigin::Signed(caller.clone()));
		let caller_lookup = <T::Lookup as StaticLookup>::unlookup(caller.clone());
		let _ = T::Currency::make_free_balance_be(&caller, BalanceOf::<T>::max_value());

		// Register the registrars
		add_registrars::<T>(r)?;

		// Add sub accounts
		add_sub_accounts::<T>(&caller, s)?;

		// Create their main identity with x additional fields
		let info = T::IdentityInformation::create_identity_info();
		Identity::<T>::set_identity(caller_origin.clone(), Box::new(info.clone()))?;

		// User requests judgement from all the registrars, and they approve
		for i in 0..r {
			let registrar: T::AccountId = account("registrar", i, SEED);
			let balance_to_use = T::Currency::minimum_balance() * 10u32.into();
			let _ = T::Currency::make_free_balance_be(&registrar, balance_to_use);

			Identity::<T>::request_judgement(caller_origin.clone(), i, 10u32.into())?;
			Identity::<T>::provide_judgement(
				RawOrigin::Signed(registrar).into(),
				i,
				caller_lookup.clone(),
				Judgement::Reasonable,
				T::Hashing::hash_of(&info),
			)?;
		}

		ensure!(IdentityOf::<T>::contains_key(&caller), "Identity does not exist.");

		#[extrinsic_call]
		_(RawOrigin::Signed(caller.clone()));

		ensure!(!IdentityOf::<T>::contains_key(&caller), "Identity not cleared.");
		Ok(())
	}

	#[benchmark]
	fn request_judgement(r: Linear<1, { T::MaxRegistrars::get() }>) -> Result<(), BenchmarkError> {
		let caller: T::AccountId = whitelisted_caller();
		let _ = T::Currency::make_free_balance_be(&caller, BalanceOf::<T>::max_value());

		// Register the registrars
		add_registrars::<T>(r)?;

		// Create their main identity with x additional fields
		let info = T::IdentityInformation::create_identity_info();
		let caller_origin =
			<T as frame_system::Config>::RuntimeOrigin::from(RawOrigin::Signed(caller.clone()));
		Identity::<T>::set_identity(caller_origin.clone(), Box::new(info))?;

		#[extrinsic_call]
		_(RawOrigin::Signed(caller.clone()), r - 1, 10u32.into());

		assert_last_event::<T>(
			Event::<T>::JudgementRequested { who: caller, registrar_index: r - 1 }.into(),
		);

		Ok(())
	}

	#[benchmark]
	fn cancel_request(r: Linear<1, { T::MaxRegistrars::get() }>) -> Result<(), BenchmarkError> {
		let caller: T::AccountId = whitelisted_caller();
		let _ = T::Currency::make_free_balance_be(&caller, BalanceOf::<T>::max_value());

		// Register the registrars
		add_registrars::<T>(r)?;

		// Create their main identity with x additional fields
		let info = T::IdentityInformation::create_identity_info();
		let caller_origin =
			<T as frame_system::Config>::RuntimeOrigin::from(RawOrigin::Signed(caller.clone()));
		Identity::<T>::set_identity(caller_origin.clone(), Box::new(info))?;

		Identity::<T>::request_judgement(caller_origin.clone(), r - 1, 10u32.into())?;

		#[extrinsic_call]
		_(RawOrigin::Signed(caller.clone()), r - 1);

		assert_last_event::<T>(
			Event::<T>::JudgementUnrequested { who: caller, registrar_index: r - 1 }.into(),
		);

		Ok(())
	}

	#[benchmark]
	fn set_fee(r: Linear<1, { T::MaxRegistrars::get() - 1 }>) -> Result<(), BenchmarkError> {
		let caller: T::AccountId = whitelisted_caller();
		let caller_lookup = T::Lookup::unlookup(caller.clone());

		add_registrars::<T>(r)?;

		let registrar_origin = T::RegistrarOrigin::try_successful_origin()
			.expect("RegistrarOrigin has no successful origin required for the benchmark");
		Identity::<T>::add_registrar(registrar_origin, caller_lookup)?;

		let registrars = Registrars::<T>::get();
		ensure!(registrars[r as usize].as_ref().unwrap().fee == 0u32.into(), "Fee already set.");

		#[extrinsic_call]
		_(RawOrigin::Signed(caller), r, 100u32.into());

		let updated_registrars = Registrars::<T>::get();
		ensure!(
			updated_registrars[r as usize].as_ref().unwrap().fee == 100u32.into(),
			"Fee not changed."
		);

		Ok(())
	}

	#[benchmark]
	fn set_account_id(r: Linear<1, { T::MaxRegistrars::get() - 1 }>) -> Result<(), BenchmarkError> {
		let caller: T::AccountId = whitelisted_caller();
		let caller_lookup = T::Lookup::unlookup(caller.clone());
		let _ = T::Currency::make_free_balance_be(&caller, BalanceOf::<T>::max_value());

		add_registrars::<T>(r)?;

		let registrar_origin = T::RegistrarOrigin::try_successful_origin()
			.expect("RegistrarOrigin has no successful origin required for the benchmark");
		Identity::<T>::add_registrar(registrar_origin, caller_lookup)?;

		let registrars = Registrars::<T>::get();
		ensure!(registrars[r as usize].as_ref().unwrap().account == caller, "id not set.");

		let new_account = T::Lookup::unlookup(account("new", 0, SEED));

		#[extrinsic_call]
		_(RawOrigin::Signed(caller), r, new_account);

		let updated_registrars = Registrars::<T>::get();
		ensure!(
			updated_registrars[r as usize].as_ref().unwrap().account == account("new", 0, SEED),
			"id not changed."
		);

		Ok(())
	}

	#[benchmark]
	fn set_fields(r: Linear<1, { T::MaxRegistrars::get() - 1 }>) -> Result<(), BenchmarkError> {
		let caller: T::AccountId = whitelisted_caller();
		let caller_lookup = T::Lookup::unlookup(caller.clone());
		let _ = T::Currency::make_free_balance_be(&caller, BalanceOf::<T>::max_value());

		add_registrars::<T>(r)?;

		let registrar_origin = T::RegistrarOrigin::try_successful_origin()
			.expect("RegistrarOrigin has no successful origin required for the benchmark");
		Identity::<T>::add_registrar(registrar_origin, caller_lookup)?;

		let registrars = Registrars::<T>::get();
		ensure!(
			registrars[r as usize].as_ref().unwrap().fields == Default::default(),
			"fields already set."
		);
		let fields = T::IdentityInformation::all_fields();

		#[extrinsic_call]
		_(RawOrigin::Signed(caller), r, fields);

		let updated_registrars = Registrars::<T>::get();
		ensure!(
			updated_registrars[r as usize].as_ref().unwrap().fields != Default::default(),
			"fields not set."
		);

		Ok(())
	}

	#[benchmark]
	fn provide_judgement(
		r: Linear<1, { T::MaxRegistrars::get() - 1 }>,
	) -> Result<(), BenchmarkError> {
		// The user
		let user: T::AccountId = account("user", r, SEED);
		let user_origin =
			<T as frame_system::Config>::RuntimeOrigin::from(RawOrigin::Signed(user.clone()));
		let user_lookup = <T::Lookup as StaticLookup>::unlookup(user.clone());
		let _ = T::Currency::make_free_balance_be(&user, BalanceOf::<T>::max_value());

		let caller: T::AccountId = whitelisted_caller();
		let caller_lookup = T::Lookup::unlookup(caller.clone());
		let _ = T::Currency::make_free_balance_be(&caller, BalanceOf::<T>::max_value());

		add_registrars::<T>(r)?;

		let info = T::IdentityInformation::create_identity_info();
		let info_hash = T::Hashing::hash_of(&info);
		Identity::<T>::set_identity(user_origin.clone(), Box::new(info))?;

		let registrar_origin = T::RegistrarOrigin::try_successful_origin()
			.expect("RegistrarOrigin has no successful origin required for the benchmark");
		Identity::<T>::add_registrar(registrar_origin, caller_lookup)?;
		Identity::<T>::request_judgement(user_origin, r, 10u32.into())?;

		#[extrinsic_call]
		_(RawOrigin::Signed(caller), r, user_lookup, Judgement::Reasonable, info_hash);

		assert_last_event::<T>(
			Event::<T>::JudgementGiven { target: user, registrar_index: r }.into(),
		);

		Ok(())
	}

	#[benchmark]
	fn kill_identity(
		r: Linear<1, { T::MaxRegistrars::get() }>,
		s: Linear<0, { T::MaxSubAccounts::get() }>,
	) -> Result<(), BenchmarkError> {
		add_registrars::<T>(r)?;

		let target: T::AccountId = account("target", 0, SEED);
		let target_origin: <T as frame_system::Config>::RuntimeOrigin =
			RawOrigin::Signed(target.clone()).into();
		let target_lookup = T::Lookup::unlookup(target.clone());
		let _ = T::Currency::make_free_balance_be(&target, BalanceOf::<T>::max_value());

		let info = T::IdentityInformation::create_identity_info();
		Identity::<T>::set_identity(target_origin.clone(), Box::new(info.clone()))?;
		add_sub_accounts::<T>(&target, s)?;

		// User requests judgement from all the registrars, and they approve
		for i in 0..r {
			let registrar: T::AccountId = account("registrar", i, SEED);
			let balance_to_use = T::Currency::minimum_balance() * 10u32.into();
			let _ = T::Currency::make_free_balance_be(&registrar, balance_to_use);

			Identity::<T>::request_judgement(target_origin.clone(), i, 10u32.into())?;
			Identity::<T>::provide_judgement(
				RawOrigin::Signed(registrar).into(),
				i,
				target_lookup.clone(),
				Judgement::Reasonable,
				T::Hashing::hash_of(&info),
			)?;
		}

		ensure!(IdentityOf::<T>::contains_key(&target), "Identity not set");

		let origin =
			T::ForceOrigin::try_successful_origin().map_err(|_| BenchmarkError::Weightless)?;

		#[extrinsic_call]
		_(origin as T::RuntimeOrigin, target_lookup);

		ensure!(!IdentityOf::<T>::contains_key(&target), "Identity not removed");

		Ok(())
	}

	#[benchmark]
	fn add_sub(s: Linear<0, { T::MaxSubAccounts::get() - 1 }>) -> Result<(), BenchmarkError> {
		let caller: T::AccountId = whitelisted_caller();
		add_sub_accounts::<T>(&caller, s)?;
		let sub = account("new_sub", 0, SEED);
		let data = Data::Raw(vec![0; 32].try_into().unwrap());

		ensure!(SubsOf::<T>::get(&caller).1.len() as u32 == s, "Subs not set.");

		#[extrinsic_call]
		_(RawOrigin::Signed(caller.clone()), T::Lookup::unlookup(sub), data);

		ensure!(SubsOf::<T>::get(&caller).1.len() as u32 == s + 1, "Subs not added.");

		Ok(())
	}

	#[benchmark]
	fn rename_sub(s: Linear<1, { T::MaxSubAccounts::get() }>) -> Result<(), BenchmarkError> {
		let caller: T::AccountId = whitelisted_caller();
		let (sub, _) = add_sub_accounts::<T>(&caller, s)?.remove(0);
		let data = Data::Raw(vec![1; 32].try_into().unwrap());

		ensure!(SuperOf::<T>::get(&sub).unwrap().1 != data, "data already set");

		#[extrinsic_call]
		_(RawOrigin::Signed(caller), T::Lookup::unlookup(sub.clone()), data.clone());

		ensure!(SuperOf::<T>::get(&sub).unwrap().1 == data, "data not set");

		Ok(())
	}

	#[benchmark]
	fn remove_sub(s: Linear<1, { T::MaxSubAccounts::get() }>) -> Result<(), BenchmarkError> {
		let caller: T::AccountId = whitelisted_caller();
		let (sub, _) = add_sub_accounts::<T>(&caller, s)?.remove(0);
		ensure!(SuperOf::<T>::contains_key(&sub), "Sub doesn't exists");

		#[extrinsic_call]
		_(RawOrigin::Signed(caller), T::Lookup::unlookup(sub.clone()));

		ensure!(!SuperOf::<T>::contains_key(&sub), "Sub not removed");

		Ok(())
	}

	#[benchmark]
	fn quit_sub(s: Linear<0, { T::MaxSubAccounts::get() - 1 }>) -> Result<(), BenchmarkError> {
		let caller: T::AccountId = whitelisted_caller();
		let sup = account("super", 0, SEED);
		add_sub_accounts::<T>(&sup, s)?;
		let sup_origin = RawOrigin::Signed(sup).into();
		Identity::<T>::add_sub(
			sup_origin,
			T::Lookup::unlookup(caller.clone()),
			Data::Raw(vec![0; 32].try_into().unwrap()),
		)?;
		ensure!(SuperOf::<T>::contains_key(&caller), "Sub doesn't exists");

		#[extrinsic_call]
		_(RawOrigin::Signed(caller.clone()));

		ensure!(!SuperOf::<T>::contains_key(&caller), "Sub not removed");

		Ok(())
	}

	#[benchmark]
	fn add_username_authority() -> Result<(), BenchmarkError> {
		let origin =
			T::UsernameAuthorityOrigin::try_successful_origin().expect("can generate origin");

		let authority: T::AccountId = account("authority", 0, SEED);
		let authority_lookup = T::Lookup::unlookup(authority.clone());
		let suffix = bench_suffix();
		let allocation = 10;

		#[extrinsic_call]
		_(origin as T::RuntimeOrigin, authority_lookup, suffix, allocation);

		assert_last_event::<T>(Event::<T>::AuthorityAdded { authority }.into());
		Ok(())
	}

	#[benchmark]
	fn remove_username_authority() -> Result<(), BenchmarkError> {
		let origin =
			T::UsernameAuthorityOrigin::try_successful_origin().expect("can generate origin");

		let authority: T::AccountId = account("authority", 0, SEED);
		let authority_lookup = T::Lookup::unlookup(authority.clone());
		let suffix = bench_suffix();
		let allocation = 10;

		assert_ok!(Identity::<T>::add_username_authority(
			origin.clone(),
			authority_lookup.clone(),
			suffix.clone(),
			allocation
		));

		#[extrinsic_call]
		_(origin as T::RuntimeOrigin, suffix.into(), authority_lookup);

		assert_last_event::<T>(Event::<T>::AuthorityRemoved { authority }.into());
		Ok(())
	}

	#[benchmark]
	fn set_username_for(p: Linear<0, 1>) -> Result<(), BenchmarkError> {
		// Set up a username authority.
		let auth_origin =
			T::UsernameAuthorityOrigin::try_successful_origin().expect("can generate origin");
		let authority: T::AccountId = account("authority", 0, SEED);
		let authority_lookup = T::Lookup::unlookup(authority.clone());
		let suffix = bench_suffix();
		let allocation = 10;
		let _ = T::Currency::make_free_balance_be(&authority, BalanceOf::<T>::max_value());

		Identity::<T>::add_username_authority(
			auth_origin,
			authority_lookup,
			suffix.clone(),
			allocation,
		)?;

		let username = bench_username();
		let bounded_username = bounded_username::<T>(username.clone(), suffix.clone());

		let (public, signature) = T::BenchmarkHelper::sign_message(&bounded_username[..]);
		let who_account = public.into_account();
		let who_lookup = T::Lookup::unlookup(who_account.clone());

		// Verify signature here to avoid surprise errors at runtime
		assert!(signature.verify(&bounded_username[..], &who_account));
		let use_allocation = match p {
			0 => false,
			1 => true,
			_ => unreachable!(),
		};

		#[extrinsic_call]
		set_username_for(
			RawOrigin::Signed(authority.clone()),
			who_lookup,
			bounded_username.clone().into(),
			Some(signature.into()),
			use_allocation,
		);

		assert_has_event::<T>(
			Event::<T>::UsernameSet {
				who: who_account.clone(),
				username: bounded_username.clone(),
			}
			.into(),
		);
		assert_has_event::<T>(
			Event::<T>::PrimaryUsernameSet { who: who_account, username: bounded_username }.into(),
		);
		if use_allocation {
			let suffix: Suffix<T> = suffix.try_into().unwrap();
			assert_eq!(AuthorityOf::<T>::get(&suffix).unwrap().allocation, 9);
		} else {
			assert_eq!(
				T::Currency::free_balance(&authority),
				BalanceOf::<T>::max_value() - T::UsernameDeposit::get()
			);
		}
		Ok(())
	}

	#[benchmark]
	fn accept_username() -> Result<(), BenchmarkError> {
		let caller: T::AccountId = whitelisted_caller();
		let username = bounded_username::<T>(bench_username(), bench_suffix());

		Identity::<T>::queue_acceptance(&caller, username.clone(), Provider::Allocation);

		#[extrinsic_call]
		_(RawOrigin::Signed(caller.clone()), username.clone());

		assert_last_event::<T>(Event::<T>::UsernameSet { who: caller, username }.into());
		Ok(())
	}

	#[benchmark]
	fn remove_expired_approval(p: Linear<0, 1>) -> Result<(), BenchmarkError> {
		// Set up a username authority.
		let auth_origin =
			T::UsernameAuthorityOrigin::try_successful_origin().expect("can generate origin");
		let authority: T::AccountId = account("authority", 0, SEED);
		let authority_lookup = T::Lookup::unlookup(authority.clone());
		let suffix = bench_suffix();
		let allocation = 10;
		let _ = T::Currency::make_free_balance_be(&authority, BalanceOf::<T>::max_value());

		Identity::<T>::add_username_authority(
			auth_origin,
			authority_lookup,
			suffix.clone(),
			allocation,
		)?;

		let caller: T::AccountId = whitelisted_caller();
		let username = bounded_username::<T>(bench_username(), suffix.clone());
		let username_deposit = T::UsernameDeposit::get();
		let provider = match p {
			0 => {
				let _ = T::Currency::reserve(&authority, username_deposit);
				Provider::AuthorityDeposit(username_deposit)
			},
			1 => Provider::Allocation,
			_ => unreachable!(),
		};
		Identity::<T>::queue_acceptance(&caller, username.clone(), provider);

		let expected_expiration =
			frame_system::Pallet::<T>::block_number() + T::PendingUsernameExpiration::get();

		run_to_block::<T>(expected_expiration + One::one());

		#[extrinsic_call]
		_(RawOrigin::Signed(caller.clone()), username);

		assert_last_event::<T>(Event::<T>::PreapprovalExpired { whose: caller }.into());
		match p {
			0 => {
				assert_eq!(T::Currency::free_balance(&authority), BalanceOf::<T>::max_value());
			},
			1 => {
				let suffix: Suffix<T> = suffix.try_into().unwrap();
				assert_eq!(AuthorityOf::<T>::get(&suffix).unwrap().allocation, 10);
			},
			_ => unreachable!(),
		}
		Ok(())
	}

	#[benchmark]
	fn set_primary_username() -> Result<(), BenchmarkError> {
		let caller: T::AccountId = whitelisted_caller();
		let first_username = bounded_username::<T>(bench_username(), bench_suffix());
		let second_username = bounded_username::<T>(b"slowbenchmark".to_vec(), bench_suffix());

		// First one will be set as primary. Second will not be.
		Identity::<T>::insert_username(&caller, first_username, Provider::Allocation);
		Identity::<T>::insert_username(&caller, second_username.clone(), Provider::Allocation);

		#[extrinsic_call]
		_(RawOrigin::Signed(caller.clone()), second_username.clone());

		assert_last_event::<T>(
			Event::<T>::PrimaryUsernameSet { who: caller, username: second_username }.into(),
		);
		Ok(())
	}

	#[benchmark]
	fn unbind_username() -> Result<(), BenchmarkError> {
		// Set up a username authority.
		let auth_origin =
			T::UsernameAuthorityOrigin::try_successful_origin().expect("can generate origin");
		let authority: T::AccountId = account("authority", 0, SEED);
		let authority_lookup = T::Lookup::unlookup(authority.clone());
		let suffix = bench_suffix();
		let allocation = 10;
		let _ = T::Currency::make_free_balance_be(&authority, BalanceOf::<T>::max_value());

		Identity::<T>::add_username_authority(
			auth_origin,
			authority_lookup,
			suffix.clone(),
			allocation,
		)?;

		let caller: T::AccountId = whitelisted_caller();
		let username = bounded_username::<T>(bench_username(), suffix.clone());

		let username_deposit = T::UsernameDeposit::get();
		Identity::<T>::insert_username(
			&caller,
			username.clone(),
			Provider::AuthorityDeposit(username_deposit),
		);

		#[extrinsic_call]
		_(RawOrigin::Signed(authority), username.clone());

		assert_last_event::<T>(Event::<T>::UsernameUnbound { username }.into());
		Ok(())
	}

	#[benchmark]
	fn remove_username() -> Result<(), BenchmarkError> {
		// Set up a username authority.
		let authority: T::AccountId = account("authority", 0, SEED);
		let suffix = bench_suffix();
		let _ = T::Currency::make_free_balance_be(&authority, BalanceOf::<T>::max_value());
		let caller: T::AccountId = whitelisted_caller();
		let username = bounded_username::<T>(bench_username(), suffix.clone());

		let username_deposit = T::UsernameDeposit::get();
		Identity::<T>::insert_username(
			&caller,
			username.clone(),
			Provider::AuthorityDeposit(username_deposit),
		);
		let now = frame_system::Pallet::<T>::block_number();
		let expiry = now + T::UsernameGracePeriod::get();
		UnbindingUsernames::<T>::insert(&username, expiry);

		frame_system::Pallet::<T>::set_block_number(expiry);

		#[extrinsic_call]
		_(RawOrigin::Signed(caller), username.clone());

		assert_last_event::<T>(Event::<T>::UsernameRemoved { username }.into());
		Ok(())
	}

	#[benchmark]
	fn kill_username(p: Linear<0, 1>) -> Result<(), BenchmarkError> {
		// Set up a username authority.
		let auth_origin =
			T::UsernameAuthorityOrigin::try_successful_origin().expect("can generate origin");
		let authority: T::AccountId = account("authority", 0, SEED);
		let authority_lookup = T::Lookup::unlookup(authority.clone());
		let suffix = bench_suffix();
		let allocation = 10;
		let _ = T::Currency::make_free_balance_be(&authority, BalanceOf::<T>::max_value());

		Identity::<T>::add_username_authority(
			auth_origin,
			authority_lookup,
			suffix.clone(),
			allocation,
		)?;

		let caller: T::AccountId = whitelisted_caller();
		let username = bounded_username::<T>(bench_username(), suffix.clone());
		let username_deposit = T::UsernameDeposit::get();
		let provider = match p {
			0 => {
				let _ = T::Currency::reserve(&authority, username_deposit);
				Provider::AuthorityDeposit(username_deposit)
			},
			1 => Provider::Allocation,
			_ => unreachable!(),
		};
		Identity::<T>::insert_username(&caller, username.clone(), provider);
		UnbindingUsernames::<T>::insert(&username, frame_system::Pallet::<T>::block_number());

		#[extrinsic_call]
		_(RawOrigin::Root, username.clone());

		assert_last_event::<T>(Event::<T>::UsernameKilled { username }.into());
		match p {
			0 => {
				assert_eq!(
					T::Currency::free_balance(&authority),
					BalanceOf::<T>::max_value() - username_deposit
				);
			},
			1 => {
				let suffix: Suffix<T> = suffix.try_into().unwrap();
				assert_eq!(AuthorityOf::<T>::get(&suffix).unwrap().allocation, 10);
			},
			_ => unreachable!(),
		}
		Ok(())
	}

	#[benchmark]
	fn migration_v2_authority_step() -> Result<(), BenchmarkError> {
		let setup = LazyMigrationV1ToV2::<T>::setup_benchmark_env_for_migration();
		assert_eq!(AuthorityOf::<T>::iter().count(), 0);
		#[block]
		{
			LazyMigrationV1ToV2::<T>::authority_step(None);
		}
		assert_eq!(AuthorityOf::<T>::get(&setup.suffix).unwrap().account_id, setup.authority);
		Ok(())
	}

	#[benchmark]
	fn migration_v2_username_step() -> Result<(), BenchmarkError> {
		let setup = LazyMigrationV1ToV2::<T>::setup_benchmark_env_for_migration();
		assert_eq!(UsernameInfoOf::<T>::iter().count(), 0);
		#[block]
		{
			LazyMigrationV1ToV2::<T>::username_step(None);
		}
		assert_eq!(UsernameInfoOf::<T>::iter().next().unwrap().1.owner, setup.account);
		Ok(())
	}

	#[benchmark]
	fn migration_v2_identity_step() -> Result<(), BenchmarkError> {
		let setup = LazyMigrationV1ToV2::<T>::setup_benchmark_env_for_migration();
		#[block]
		{
			LazyMigrationV1ToV2::<T>::identity_step(None);
		}
		assert!(IdentityOf::<T>::get(&setup.account).is_some());
		Ok(())
	}

	#[benchmark]
	fn migration_v2_pending_username_step() -> Result<(), BenchmarkError> {
		let setup = LazyMigrationV1ToV2::<T>::setup_benchmark_env_for_migration();
		#[block]
		{
			LazyMigrationV1ToV2::<T>::pending_username_step(None);
		}
		assert!(PendingUsernames::<T>::get(&setup.username).is_some());
		Ok(())
	}

	#[benchmark]
	fn migration_v2_cleanup_authority_step() -> Result<(), BenchmarkError> {
		let setup = LazyMigrationV1ToV2::<T>::setup_benchmark_env_for_cleanup();
		#[block]
		{
			LazyMigrationV1ToV2::<T>::cleanup_authority_step(None);
		}
		LazyMigrationV1ToV2::<T>::check_authority_cleanup_validity(setup.suffix, setup.authority);
		Ok(())
	}

	#[benchmark]
	fn migration_v2_cleanup_username_step() -> Result<(), BenchmarkError> {
		let setup = LazyMigrationV1ToV2::<T>::setup_benchmark_env_for_cleanup();
		#[block]
		{
			LazyMigrationV1ToV2::<T>::cleanup_username_step(None);
		}
		LazyMigrationV1ToV2::<T>::check_username_cleanup_validity(setup.username, setup.account);
		Ok(())
	}

	impl_benchmark_test_suite!(Identity, crate::tests::new_test_ext(), crate::tests::Test);
}
