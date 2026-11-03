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

//! Mock types and XcmConfig for all executor unit tests.

use alloc::collections::btree_map::BTreeMap;
use codec::{Decode, DecodeWithMemTracking, Encode};
use core::cell::RefCell;
use frame_support::{
	dispatch::{DispatchInfo, DispatchResultWithPostInfo, GetDispatchInfo, PostDispatchInfo},
	parameter_types,
	traits::{Everything, Nothing, ProcessMessageError},
	weights::Weight,
};
use sp_runtime::traits::Dispatchable;
use xcm::prelude::*;

use crate::{
	traits::{
		ClaimAssets, DropAssets, FeeManager, ProcessTransaction, Properties, ShouldExecute,
		TransactAsset, WeightBounds, WeightTrader,
	},
	AssetsInHolding, Config, FeeReason, XcmExecutor,
};

/// Mock credit implementation for testing purposes.
pub use crate::test_helpers::MockCredit;

/// We create an XCVM instance instead of calling `XcmExecutor::<_>::prepare_and_execute` so we
/// can inspect its fields.
pub fn instantiate_executor(
	origin: impl Into<Location>,
	message: Xcm<<XcmConfig as Config>::RuntimeCall>,
) -> (XcmExecutor<XcmConfig>, Weight) {
	let mut vm =
		XcmExecutor::<XcmConfig>::new(origin, message.using_encoded(sp_io::hashing::blake2_256));
	let weight = XcmExecutor::<XcmConfig>::prepare(message.clone(), Weight::MAX)
		.unwrap()
		.weight_of();
	vm.message_weight = weight;
	(vm, weight)
}

parameter_types! {
	pub const MaxAssetsIntoHolding: u32 = 10;
	pub const BaseXcmWeight: Weight = Weight::from_parts(1, 1);
	pub const MaxInstructions: u32 = 10;
	pub UniversalLocation: InteriorLocation = [GlobalConsensus(ByGenesis([0; 32])), Parachain(1000)].into();
	/// Simulate the chain’s existential deposit.
	pub const ExistentialDeposit: u128 = 2;
}

/// Test origin.
#[derive(Debug)]
pub struct TestOrigin;

/// Test call.
///
/// Doesn't dispatch anything, has an empty implementation of [`Dispatchable`] that
/// just returns `Ok` with an empty [`PostDispatchInfo`].
#[derive(
	Debug, Encode, Decode, DecodeWithMemTracking, Eq, PartialEq, Clone, Copy, scale_info::TypeInfo,
)]
pub struct TestCall;
impl Dispatchable for TestCall {
	type RuntimeOrigin = TestOrigin;
	type Config = ();
	type Info = ();
	type PostInfo = PostDispatchInfo;

	fn dispatch(self, _origin: Self::RuntimeOrigin) -> DispatchResultWithPostInfo {
		Ok(PostDispatchInfo::default())
	}
}
impl GetDispatchInfo for TestCall {
	fn get_dispatch_info(&self) -> DispatchInfo {
		DispatchInfo::default()
	}
}

/// Test weigher that just returns a fixed weight for every program.
pub struct TestWeigher;
impl<C> WeightBounds<C> for TestWeigher {
	fn weight(_message: &mut Xcm<C>, _weight_limit: Weight) -> Result<Weight, InstructionError> {
		Ok(Weight::from_parts(2, 2))
	}

	fn instr_weight(_instruction: &mut Instruction<C>) -> Result<Weight, XcmError> {
		Ok(Weight::from_parts(2, 2))
	}
}

thread_local! {
	pub static ASSETS: RefCell<BTreeMap<Location, AssetsInHolding>> = RefCell::new(BTreeMap::new());
	pub static SENT_XCM: RefCell<Vec<(Location, Xcm<()>)>> = RefCell::new(Vec::new());
}

pub fn add_asset(who: impl Into<Location>, what: impl Into<Asset>) {
	use xcm::latest::Fungibility;

	let asset = what.into();
	let mut holding = AssetsInHolding::new();
	match asset.fun {
		Fungibility::Fungible(amount) => {
			holding.fungible.insert(asset.id, Box::new(MockCredit(amount)));
		},
		Fungibility::NonFungible(instance) => {
			holding.non_fungible.insert((asset.id, instance));
		},
	}
	ASSETS.with(|a| {
		a.borrow_mut()
			.entry(who.into())
			.or_insert(AssetsInHolding::new())
			.subsume_assets(holding)
	});
}

pub fn asset_list(who: impl Into<Location>) -> Vec<Asset> {
	assets(who).assets_iter().collect()
}

pub fn assets(who: impl Into<Location>) -> AssetsInHolding {
	ASSETS
		.with(|a| a.borrow().get(&who.into()).map(|h| h.unsafe_clone_for_tests()))
		.unwrap_or_else(|| AssetsInHolding::new())
}

pub fn get_first_fungible(assets: &AssetsInHolding) -> Option<Asset> {
	assets.fungible_assets_iter().next()
}

/// Test asset transactor that withdraws from and deposits to a thread local assets storage.
pub struct TestAssetTransactor;
impl TransactAsset for TestAssetTransactor {
	fn deposit_asset(
		what: AssetsInHolding,
		who: &Location,
		_context: Option<&XcmContext>,
	) -> Result<(), (AssetsInHolding, XcmError)> {
		// Collect assets first to avoid borrow/move conflict
		let assets_vec: Vec<_> = what.assets_iter().collect();
		for asset in &assets_vec {
			if let Fungibility::Fungible(amount) = asset.fun {
				// fail if below the configured existential deposit
				if amount < ExistentialDeposit::get() {
					return Err((
						what,
						XcmError::FailedToTransactAsset(
							sp_runtime::TokenError::BelowMinimum.into(),
						),
					));
				}
			}
		}
		for asset in assets_vec {
			add_asset(who.clone(), asset);
		}
		Ok(())
	}

	fn withdraw_asset(
		what: &Asset,
		who: &Location,
		_context: Option<&XcmContext>,
	) -> Result<AssetsInHolding, XcmError> {
		ASSETS.with(|a| {
			a.borrow_mut()
				.get_mut(who)
				.ok_or(XcmError::NotWithdrawable)?
				.try_take(what.clone().into())
				.map_err(|_| XcmError::NotWithdrawable)
		})
	}

	fn mint_asset(what: &Asset, _: &XcmContext) -> Result<AssetsInHolding, XcmError> {
		Ok(crate::test_helpers::mock_asset_to_holding(what.clone()))
	}
}

/// Test barrier that just lets everything through.
pub struct TestBarrier;
impl ShouldExecute for TestBarrier {
	fn should_execute<Call>(
		_origin: &Location,
		_instructions: &mut [Instruction<Call>],
		_max_weight: Weight,
		_properties: &mut Properties,
	) -> Result<(), ProcessMessageError> {
		Ok(())
	}
}

/// Test weight to fee that just multiplies `Weight.ref_time` and `Weight.proof_size`.
pub struct WeightToFee;
impl WeightToFee {
	pub fn weight_to_fee(weight: &Weight) -> u128 {
		weight.ref_time() as u128 * weight.proof_size() as u128
	}
}

/// Test weight trader that just buys weight with the native asset (`Here`) and
/// uses the test `WeightToFee`.
pub struct TestTrader {
	weight_bought_so_far: Weight,
}
impl WeightTrader for TestTrader {
	fn new() -> Self {
		Self { weight_bought_so_far: Weight::zero() }
	}

	fn buy_weight(
		&mut self,
		weight: Weight,
		mut payment: AssetsInHolding,
		_context: &XcmContext,
	) -> Result<AssetsInHolding, (AssetsInHolding, XcmError)> {
		let amount = WeightToFee::weight_to_fee(&weight);
		let required: Asset = (Here, amount).into();
		// Try to take the required amount from payment
		match payment.try_take(required.into()) {
			Ok(_) => {
				self.weight_bought_so_far.saturating_accrue(weight);
				// Return the unused payment
				Ok(payment)
			},
			Err(_) => Err((payment, XcmError::TooExpensive)),
		}
	}

	fn refund_weight(&mut self, weight: Weight, _context: &XcmContext) -> Option<AssetsInHolding> {
		use xcm::latest::Fungibility;

		let weight = weight.min(self.weight_bought_so_far);
		let amount = WeightToFee::weight_to_fee(&weight);
		self.weight_bought_so_far -= weight;
		if amount > 0 {
			let asset: Asset = (Here, amount).into();
			let mut holding = AssetsInHolding::new();
			match asset.fun {
				Fungibility::Fungible(amount) => {
					holding.fungible.insert(asset.id, Box::new(MockCredit(amount)));
				},
				Fungibility::NonFungible(instance) => {
					holding.non_fungible.insert((asset.id, instance));
				},
			}
			Some(holding)
		} else {
			None
		}
	}
}

/// Account where all dropped assets are deposited.
pub const TRAPPED_ASSETS: [u8; 32] = [255; 32];

/// Test asset trap that moves all dropped assets to the `TRAPPED_ASSETS` account.
pub struct TestAssetTrap;
impl DropAssets for TestAssetTrap {
	fn drop_assets(_origin: &Location, assets: AssetsInHolding, _context: &XcmContext) -> Weight {
		ASSETS.with(|a| {
			a.borrow_mut()
				.entry(TRAPPED_ASSETS.into())
				.or_insert(AssetsInHolding::new())
				.subsume_assets(assets)
		});
		Weight::zero()
	}
}

impl ClaimAssets for TestAssetTrap {
	fn claim_assets(
		_origin: &Location,
		_ticket: &Location,
		what: &Assets,
		_context: &XcmContext,
	) -> Option<AssetsInHolding> {
		ASSETS.with(|a| {
			let mut assets = a.borrow_mut();
			let trapped = assets.get_mut(&TRAPPED_ASSETS.into())?;
			let mut claimed = AssetsInHolding::new();
			for asset in what.inner().iter() {
				if let Ok(taken) = trapped.try_take(asset.clone().into()) {
					claimed.subsume_assets(taken);
				}
			}
			if claimed.is_empty() {
				None
			} else {
				Some(claimed)
			}
		})
	}
}

/// A destination that passes validation but whose delivery always fails.
pub fn undeliverable_destination() -> Location {
	Location::new(1, [Parachain(9999)])
}

/// Test sender that puts messages in a dummy queue. Delivery succeeds for every destination
/// except [`undeliverable_destination`].
///
/// It charges `1` for the delivery fee.
pub struct TestSender;
impl SendXcm for TestSender {
	type Ticket = (Location, Xcm<()>);

	fn validate(
		destination: &mut Option<Location>,
		message: &mut Option<Xcm<()>>,
	) -> SendResult<Self::Ticket> {
		let ticket = (destination.take().unwrap(), message.take().unwrap());
		let delivery_fee: Asset = (Here, 1u128).into();
		Ok((ticket, delivery_fee.into()))
	}

	fn deliver(ticket: Self::Ticket) -> Result<XcmHash, SendError> {
		if ticket.0 == undeliverable_destination() {
			return Err(SendError::Transport("undeliverable destination"));
		}
		SENT_XCM.with(|q| q.borrow_mut().push(ticket));
		Ok([0; 32])
	}
}

/// Gets queued test messages.
pub fn sent_xcm() -> Vec<(Location, Xcm<()>)> {
	SENT_XCM.with(|q| (*q.borrow()).clone())
}

/// A mock contract address that doesn't need to pay for fees.
pub const WAIVED_CONTRACT_ADDRESS: [u8; 20] = [128; 20];

/// Test fee manager that will waive the fee for some origins.
///
/// Doesn't do anything with the fee, which effectively burns it.
pub struct TestFeeManager;
impl FeeManager for TestFeeManager {
	fn is_waived(origin: Option<&Location>, _: FeeReason) -> bool {
		let Some(origin) = origin else { return false };
		// Match the root origin and a particular smart contract account.
		matches!(
			origin.unpack(),
			(0, []) | (0, [AccountKey20 { network: None, key: WAIVED_CONTRACT_ADDRESS }])
		)
	}

	fn handle_fee(_: AssetsInHolding, _: Option<&XcmContext>, _: FeeReason) {}
}

/// Dummy transactional processor that doesn't rollback storage changes, just
/// aims to rollback executor state.
pub struct TestTransactionalProcessor;
impl ProcessTransaction for TestTransactionalProcessor {
	const IS_TRANSACTIONAL: bool = true;

	fn process<F>(f: F) -> Result<(), XcmError>
	where
		F: FnOnce() -> Result<(), XcmError>,
	{
		f()
	}
}

/// Test XcmConfig that uses all the test implementations in this file.
pub struct XcmConfig;
impl Config for XcmConfig {
	type RuntimeCall = TestCall;
	type XcmSender = TestSender;
	type XcmEventEmitter = ();
	type AssetTransactor = TestAssetTransactor;
	type OriginConverter = ();
	type IsReserve = ();
	type IsTeleporter = ();
	type UniversalLocation = UniversalLocation;
	type Barrier = TestBarrier;
	type Weigher = TestWeigher;
	type Trader = TestTrader;
	type ResponseHandler = ();
	type AssetTrap = TestAssetTrap;
	type AssetLocker = ();
	type AssetExchanger = ();
	type SubscriptionService = ();
	type PalletInstancesInfo = ();
	type MaxAssetsIntoHolding = MaxAssetsIntoHolding;
	type FeeManager = TestFeeManager;
	type MessageExporter = ();
	type UniversalAliases = Nothing;
	type CallDispatcher = Self::RuntimeCall;
	type SafeCallFilter = Everything;
	type Aliasers = Nothing;
	type TransactionalProcessor = TestTransactionalProcessor;
	type HrmpNewChannelOpenRequestHandler = ();
	type HrmpChannelAcceptedHandler = ();
	type HrmpChannelClosingHandler = ();
	type XcmRecorder = ();
}
