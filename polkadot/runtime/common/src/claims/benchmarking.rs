// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! Benchmarking for claims pallet

#[cfg(feature = "runtime-benchmarks")]
use super::*;
use crate::claims::Call;
use frame_benchmarking::v2::*;
use frame_support::{
	dispatch::{DispatchInfo, GetDispatchInfo},
	traits::{Currency, UnfilteredDispatchable},
};
use frame_system::RawOrigin;
use secp_utils::*;
use sp_runtime::{traits::DispatchTransaction, DispatchResult};

const SEED: u32 = 0;

const MAX_CLAIMS: u32 = 10_000;
const MIN_VALUE: u32 = 1_000_000;

/// Claim amount used by the benchmarks. Floored to the runtime's existential deposit, since these
/// claims carry a vesting schedule: [`Pallet::process_claim`] rejects a vesting claim whose
/// resulting balance would be below the ED ([`Error::ClaimBelowExistentialDeposit`]), because the
/// dest account must stay alive to hold the vesting lock. The benchmark deposits into fresh
/// accounts, so without the floor this trips on chains where ED exceeds [`MIN_VALUE`] (1M).
fn bench_claim_value<T: Config>() -> BalanceOf<T> {
	CurrencyOf::<T>::minimum_balance().max(MIN_VALUE.into())
}

fn create_claim<T: Config>(input: u32) -> DispatchResult {
	let secret_key = libsecp256k1::SecretKey::parse(&keccak_256(&input.encode())).unwrap();
	let eth_address = eth(&secret_key);
	let vesting = Some((100_000u32.into(), 1_000u32.into(), 100u32.into()));
	super::Pallet::<T>::mint_claim(
		RawOrigin::Root.into(),
		eth_address,
		bench_claim_value::<T>(),
		vesting,
		None,
	)?;
	Ok(())
}

fn create_claim_attest<T: Config>(input: u32) -> DispatchResult {
	let secret_key = libsecp256k1::SecretKey::parse(&keccak_256(&input.encode())).unwrap();
	let eth_address = eth(&secret_key);
	let vesting = Some((100_000u32.into(), 1_000u32.into(), 100u32.into()));
	super::Pallet::<T>::mint_claim(
		RawOrigin::Root.into(),
		eth_address,
		bench_claim_value::<T>(),
		vesting,
		Some(Default::default()),
	)?;
	Ok(())
}

#[benchmarks(
		where
			<T as frame_system::Config>::RuntimeCall: IsSubType<Call<T>> + From<Call<T>>,
			<T as frame_system::Config>::RuntimeCall: Dispatchable<Info = DispatchInfo> + GetDispatchInfo,
			<<T as frame_system::Config>::RuntimeCall as Dispatchable>::RuntimeOrigin: AsSystemOriginSigner<T::AccountId> + AsTransactionAuthorizedOrigin + Clone,
			<<T as frame_system::Config>::RuntimeCall as Dispatchable>::PostInfo: Default,
	)]
mod benchmarks {
	use super::*;

	#[allow(deprecated)]
	use sp_runtime::traits::ValidateUnsigned;

	// Benchmark `claim` including `validate_unsigned` logic.
	#[benchmark]
	fn claim() -> Result<(), BenchmarkError> {
		let c = MAX_CLAIMS;
		for _ in 0..c / 2 {
			create_claim::<T>(c)?;
			create_claim_attest::<T>(u32::MAX - c)?;
		}
		let secret_key = libsecp256k1::SecretKey::parse(&keccak_256(&c.encode())).unwrap();
		let eth_address = eth(&secret_key);
		let account: T::AccountId = account("user", c, SEED);
		let vesting = Some((100_000u32.into(), 1_000u32.into(), 100u32.into()));
		let signature = sig::<T>(&secret_key, &account.encode(), &[][..]);
		super::Pallet::<T>::mint_claim(
			RawOrigin::Root.into(),
			eth_address,
			bench_claim_value::<T>(),
			vesting,
			None,
		)?;
		assert_eq!(Claims::<T>::get(eth_address), Some(bench_claim_value::<T>()));
		let source = sp_runtime::transaction_validity::TransactionSource::External;
		let call_enc =
			Call::<T>::claim { dest: account.clone(), ethereum_signature: signature.clone() }
				.encode();

		#[block]
		{
			let call = <Call<T> as Decode>::decode(&mut &*call_enc)
				.expect("call is encoded above, encoding must be correct");
			#[allow(deprecated)]
			super::Pallet::<T>::validate_unsigned(source, &call)
				.map_err(|e| -> &'static str { e.into() })?;
			call.dispatch_bypass_filter(RawOrigin::None.into())?;
		}

		assert_eq!(Claims::<T>::get(eth_address), None);
		Ok(())
	}

	// Benchmark `mint_claim` when there already exists `c` claims in storage.
	#[benchmark]
	fn mint_claim() -> Result<(), BenchmarkError> {
		let c = MAX_CLAIMS;
		for _ in 0..c / 2 {
			create_claim::<T>(c)?;
			create_claim_attest::<T>(u32::MAX - c)?;
		}
		let eth_address = account("eth_address", 0, SEED);
		let vesting = Some((100_000u32.into(), 1_000u32.into(), 100u32.into()));
		let statement = StatementKind::Regular;

		#[extrinsic_call]
		_(RawOrigin::Root, eth_address, bench_claim_value::<T>(), vesting, Some(statement));

		assert_eq!(Claims::<T>::get(eth_address), Some(bench_claim_value::<T>()));
		Ok(())
	}

	// Benchmark `claim_attest` including `validate_unsigned` logic.
	#[benchmark]
	fn claim_attest() -> Result<(), BenchmarkError> {
		let c = MAX_CLAIMS;
		for _ in 0..c / 2 {
			create_claim::<T>(c)?;
			create_claim_attest::<T>(u32::MAX - c)?;
		}
		// Crate signature
		let attest_c = u32::MAX - c;
		let secret_key = libsecp256k1::SecretKey::parse(&keccak_256(&attest_c.encode())).unwrap();
		let eth_address = eth(&secret_key);
		let account: T::AccountId = account("user", c, SEED);
		let vesting = Some((100_000u32.into(), 1_000u32.into(), 100u32.into()));
		let statement = StatementKind::Regular;
		let signature = sig::<T>(&secret_key, &account.encode(), statement.to_text());
		super::Pallet::<T>::mint_claim(
			RawOrigin::Root.into(),
			eth_address,
			bench_claim_value::<T>(),
			vesting,
			Some(statement),
		)?;
		assert_eq!(Claims::<T>::get(eth_address), Some(bench_claim_value::<T>()));
		let call_enc = Call::<T>::claim_attest {
			dest: account.clone(),
			ethereum_signature: signature.clone(),
			statement: StatementKind::Regular.to_text().to_vec(),
		}
		.encode();
		let source = sp_runtime::transaction_validity::TransactionSource::External;

		#[block]
		{
			let call = <Call<T> as Decode>::decode(&mut &*call_enc)
				.expect("call is encoded above, encoding must be correct");
			#[allow(deprecated)]
			super::Pallet::<T>::validate_unsigned(source, &call)
				.map_err(|e| -> &'static str { e.into() })?;
			call.dispatch_bypass_filter(RawOrigin::None.into())?;
		}

		assert_eq!(Claims::<T>::get(eth_address), None);
		Ok(())
	}

	// Benchmark `attest` including prevalidate logic.
	#[benchmark]
	fn attest() -> Result<(), BenchmarkError> {
		let c = MAX_CLAIMS;
		for _ in 0..c / 2 {
			create_claim::<T>(c)?;
			create_claim_attest::<T>(u32::MAX - c)?;
		}
		let attest_c = u32::MAX - c;
		let secret_key = libsecp256k1::SecretKey::parse(&keccak_256(&attest_c.encode())).unwrap();
		let eth_address = eth(&secret_key);
		let account: T::AccountId = account("user", c, SEED);
		let vesting = Some((100_000u32.into(), 1_000u32.into(), 100u32.into()));
		let statement = StatementKind::Regular;
		super::Pallet::<T>::mint_claim(
			RawOrigin::Root.into(),
			eth_address,
			bench_claim_value::<T>(),
			vesting,
			Some(statement),
		)?;
		Preclaims::<T>::insert(&account, eth_address);
		assert_eq!(Claims::<T>::get(eth_address), Some(bench_claim_value::<T>()));

		let stmt = StatementKind::Regular.to_text().to_vec();

		#[extrinsic_call]
		_(RawOrigin::Signed(account), stmt);

		assert_eq!(Claims::<T>::get(eth_address), None);
		Ok(())
	}

	#[benchmark]
	fn move_claim() -> Result<(), BenchmarkError> {
		let c = MAX_CLAIMS;
		for _ in 0..c / 2 {
			create_claim::<T>(c)?;
			create_claim_attest::<T>(u32::MAX - c)?;
		}
		let attest_c = u32::MAX - c;
		let secret_key = libsecp256k1::SecretKey::parse(&keccak_256(&attest_c.encode())).unwrap();
		let eth_address = eth(&secret_key);

		let new_secret_key =
			libsecp256k1::SecretKey::parse(&keccak_256(&(u32::MAX / 2).encode())).unwrap();
		let new_eth_address = eth(&new_secret_key);

		let account: T::AccountId = account("user", c, SEED);
		Preclaims::<T>::insert(&account, eth_address);

		assert!(Claims::<T>::contains_key(eth_address));
		assert!(!Claims::<T>::contains_key(new_eth_address));

		#[extrinsic_call]
		_(RawOrigin::Root, eth_address, new_eth_address, Some(account));

		assert!(!Claims::<T>::contains_key(eth_address));
		assert!(Claims::<T>::contains_key(new_eth_address));
		Ok(())
	}

	// Benchmark the time it takes to do `repeat` number of keccak256 hashes
	#[benchmark(extra)]
	fn keccak256(i: Linear<0, 10_000>) {
		let bytes = (i).encode();

		#[block]
		{
			for _ in 0..i {
				let _hash = keccak_256(&bytes);
			}
		}
	}

	// Benchmark the time it takes to do `repeat` number of `eth_recover`
	#[benchmark(extra)]
	fn eth_recover(i: Linear<0, 1_000>) {
		// Crate signature
		let secret_key = libsecp256k1::SecretKey::parse(&keccak_256(&i.encode())).unwrap();
		let account: T::AccountId = account("user", i, SEED);
		let signature = sig::<T>(&secret_key, &account.encode(), &[][..]);
		let data = account.using_encoded(to_ascii_hex);
		let extra = StatementKind::default().to_text();

		#[block]
		{
			for _ in 0..i {
				assert!(super::Pallet::<T>::eth_recover(&signature, &data, extra).is_some());
			}
		}
	}

	#[benchmark]
	fn prevalidate_attests() -> Result<(), BenchmarkError> {
		let c = MAX_CLAIMS;
		for _ in 0..c / 2 {
			create_claim::<T>(c)?;
			create_claim_attest::<T>(u32::MAX - c)?;
		}
		let ext = PrevalidateAttests::<T>::new();
		let call = super::Call::attest { statement: StatementKind::Regular.to_text().to_vec() };
		let call: <T as frame_system::Config>::RuntimeCall = call.into();
		let info = call.get_dispatch_info();
		let attest_c = u32::MAX - c;
		let secret_key = libsecp256k1::SecretKey::parse(&keccak_256(&attest_c.encode())).unwrap();
		let eth_address = eth(&secret_key);
		let account: T::AccountId = account("user", c, SEED);
		let vesting = Some((100_000u32.into(), 1_000u32.into(), 100u32.into()));
		let statement = StatementKind::Regular;
		super::Pallet::<T>::mint_claim(
			RawOrigin::Root.into(),
			eth_address,
			bench_claim_value::<T>(),
			vesting,
			Some(statement),
		)?;
		Preclaims::<T>::insert(&account, eth_address);
		assert_eq!(Claims::<T>::get(eth_address), Some(bench_claim_value::<T>()));

		#[block]
		{
			assert!(ext
				.test_run(RawOrigin::Signed(account).into(), &call, &info, 0, 0, |_| {
					Ok(Default::default())
				})
				.unwrap()
				.is_ok());
		}

		Ok(())
	}

	impl_benchmark_test_suite!(
		Pallet,
		crate::claims::mock::new_test_ext(),
		crate::claims::mock::Test,
	);
}
