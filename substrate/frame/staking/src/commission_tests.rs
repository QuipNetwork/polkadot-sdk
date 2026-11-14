// This file is part of Substrate.
// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

//! QUIP-12 asynchronous-provider controls, separate from the unchanged stock mock.
use crate::{self as pallet_staking, *};
use frame_election_provider_support::{
	bounds::{ElectionBounds, ElectionBoundsBuilder},
	onchain, BoundedSupportsOf, ElectionProvider, SequentialPhragmen,
};
use frame_support::{
	assert_noop, assert_ok, derive_impl, parameter_types,
	traits::{ConstBool, ConstU128, ConstU32, UnixTime},
	weights::Weight,
};
use sp_runtime::{BuildStorage, Perbill};

type Block = frame_system::mocking::MockBlock<Test>;
frame_support::construct_runtime!(
	pub enum Test {
		System: frame_system,
		Balances: pallet_balances,
		Staking: pallet_staking,
	}
);
#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type Block = Block;
	type AccountData = pallet_balances::AccountData<u128>;
}
#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig)]
impl pallet_balances::Config for Test {
	type Balance = u128;
	type ExistentialDeposit = ConstU128<1>;
	type AccountStore = System;
}
pub struct Clock;
impl UnixTime for Clock {
	fn now() -> core::time::Duration {
		core::time::Duration::ZERO
	}
}
#[derive_impl(crate::config_preludes::TestDefaultConfig)]
impl Config for Test {
	type OldCurrency = Balances;
	type Currency = Balances;
	type UnixTime = Clock;
	type AdminOrigin = frame_system::EnsureRoot<u64>;
	type EraPayout = ();
	type ElectionProvider = AsyncElection;
	type GenesisElectionProvider = SyncElection;
	type VoterList = UseNominatorsAndValidatorsMap<Self>;
	type TargetList = UseValidatorsMap<Self>;
}
parameter_types! {
	pub Bounds: ElectionBounds = ElectionBoundsBuilder::default().build();
	pub static ElectionStatus: Result<Option<Weight>, ()> = Err(());
}
pub struct OnChain;
type SyncElection = onchain::OnChainExecution<OnChain>;
impl onchain::Config for OnChain {
	type System = Test;
	type Solver = SequentialPhragmen<u64, Perbill>;
	type DataProvider = Staking;
	type WeightInfo = ();
	type MaxBackersPerWinner = ConstU32<100>;
	type MaxWinnersPerPage = ConstU32<100>;
	type Bounds = Bounds;
	type Sort = ConstBool<true>;
}
pub struct AsyncElection;
impl ElectionProvider for AsyncElection {
	type AccountId = u64;
	type BlockNumber = u64;
	type Error = <SyncElection as ElectionProvider>::Error;
	type MaxWinnersPerPage = <SyncElection as ElectionProvider>::MaxWinnersPerPage;
	type MaxBackersPerWinner = <SyncElection as ElectionProvider>::MaxBackersPerWinner;
	type MaxBackersPerWinnerFinal = <SyncElection as ElectionProvider>::MaxBackersPerWinnerFinal;
	type Pages = <SyncElection as ElectionProvider>::Pages;
	type DataProvider = Staking;
	fn elect(page: u32) -> Result<BoundedSupportsOf<Self>, Self::Error> {
		SyncElection::elect(page)
	}
	fn duration() -> u64 {
		10
	}
	fn start() -> Result<(), Self::Error> {
		SyncElection::start()
	}
	fn status() -> Result<Option<Weight>, ()> {
		ElectionStatus::get()
	}
}
fn prefs(percent: u32) -> ValidatorPrefs {
	ValidatorPrefs { commission: Perbill::from_percent(percent), blocked: false }
}
fn ext() -> sp_io::TestExternalities {
	ElectionStatus::set(Err(()));
	let mut storage = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();
	pallet_balances::GenesisConfig::<Test> {
		balances: vec![(1, 1000), (2, 1000)],
		..Default::default()
	}
	.assimilate_storage(&mut storage)
	.unwrap();
	let mut ext: sp_io::TestExternalities = storage.into();
	ext.execute_with(|| {
		System::set_block_number(1);
		for who in [1, 2] {
			assert_ok!(Staking::bond(RuntimeOrigin::signed(who), 100, RewardDestination::Stash));
		}
		assert_ok!(Staking::validate(RuntimeOrigin::signed(1), prefs(10)));
	});
	ext
}
#[test]
fn asynchronous_commission_increases_require_an_inactive_round() {
	for phase in [Ok(None), Ok(Some(Weight::zero()))] {
		ext().execute_with(|| {
			ElectionStatus::set(phase);
			assert_noop!(
				Staking::validate(RuntimeOrigin::signed(1), prefs(11)),
				Error::<Test>::CommissionChangeDuringElection
			);
			assert_ok!(Staking::validate(
				RuntimeOrigin::signed(1),
				ValidatorPrefs { blocked: true, ..prefs(10) }
			));
			assert_ok!(Staking::validate(RuntimeOrigin::signed(1), prefs(5)));
			ElectionStatus::set(Err(()));
			assert_ok!(Staking::validate(RuntimeOrigin::signed(1), prefs(20)));
			assert_eq!(Validators::<Test>::get(1).commission, Perbill::from_percent(20));
		});
	}
}
#[test]
fn asynchronous_exit_then_revalidate_cannot_bypass_commission_guard() {
	for phase in [Ok(None), Ok(Some(Weight::zero()))] {
		for nominate in [false, true] {
			ext().execute_with(|| {
				ElectionStatus::set(phase);
				if nominate {
					assert_ok!(Staking::validate(RuntimeOrigin::signed(2), prefs(0)));
					assert_ok!(Staking::nominate(RuntimeOrigin::signed(1), vec![2]));
				} else {
					assert_ok!(Staking::chill(RuntimeOrigin::signed(1)));
				}
				assert!(!Validators::<Test>::contains_key(1));
				assert_noop!(
					Staking::validate(RuntimeOrigin::signed(1), prefs(100)),
					Error::<Test>::CommissionChangeDuringElection
				);
				assert_ok!(Staking::validate(RuntimeOrigin::signed(1), prefs(0)));
				ElectionStatus::set(Err(()));
				assert_ok!(Staking::validate(RuntimeOrigin::signed(1), prefs(100)));
			});
		}
	}
}

#[test]
fn asynchronous_absent_prefs_allow_only_zero_until_between_rounds() {
	for phase in [Ok(None), Ok(Some(Weight::zero()))] {
		for register_zero in [false, true] {
			ext().execute_with(|| {
				ElectionStatus::set(phase);
				assert!(!Validators::<Test>::contains_key(2));
				assert_noop!(
					Staking::validate(RuntimeOrigin::signed(2), prefs(25)),
					Error::<Test>::CommissionChangeDuringElection
				);
				if register_zero {
					assert_ok!(Staking::validate(RuntimeOrigin::signed(2), prefs(0)));
				}
				ElectionStatus::set(Err(()));
				assert_ok!(Staking::validate(RuntimeOrigin::signed(2), prefs(25)));
			});
		}
	}
}

#[test]
fn minimum_commission_still_precedes_the_round_guard() {
	ext().execute_with(|| {
		ElectionStatus::set(Ok(None));
		MinCommission::<Test>::put(Perbill::from_percent(15));
		assert_noop!(
			Staking::validate(RuntimeOrigin::signed(1), prefs(11)),
			Error::<Test>::CommissionTooLow
		);
		assert_noop!(
			Staking::validate(RuntimeOrigin::signed(2), prefs(0)),
			Error::<Test>::CommissionTooLow
		);
	});
}
