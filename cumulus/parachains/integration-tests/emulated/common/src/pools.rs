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

pub use paste;

pub use pallet_asset_conversion;

/// This function assumes that the asset is prefunded.
///
/// Usually, this function is only called from the other macros in this module.
#[macro_export]
macro_rules! create_pool_with_native_location_on {
	( $chain:ident, $native_location:expr, $asset_id:expr, $asset_owner:expr, $wnd_amount:expr, $asset_amount:expr ) => {
		$crate::pools::paste::paste! {
			<$chain>::execute_with(|| {
				type RuntimeEvent = <$chain as Chain>::RuntimeEvent;
				let owner = $asset_owner;
				let signed_owner = <$chain as Chain>::RuntimeOrigin::signed(owner.clone());
				let native_location: Location = $native_location;

				assert_ok!(<$chain as [<$chain Pallet>]>::AssetConversion::create_pool(
					signed_owner.clone(),
					Box::new(native_location.clone()),
					Box::new($asset_id.clone()),
				));

				assert_expected_events!(
					$chain,
					vec![
						RuntimeEvent::AssetConversion($crate::pools::pallet_asset_conversion::Event::PoolCreated { .. }) => {},
					]
				);

				assert_ok!(<$chain as [<$chain Pallet>]>::AssetConversion::add_liquidity(
					signed_owner,
					Box::new(native_location),
					Box::new($asset_id),
					$wnd_amount,
					$asset_amount,
					0,
					0,
					owner.into()
				));

				assert_expected_events!(
					$chain,
					vec![
						RuntimeEvent::AssetConversion($crate::pools::pallet_asset_conversion::Event::LiquidityAdded { .. }) => {},
					]
				);
			});
		}
	};
}

#[macro_export]
macro_rules! create_pool_with_relay_native_on {
	// default amounts
	( $chain:ident, $asset_id:expr, $asset_owner:expr ) => {
		$crate::create_pool_with_relay_native_on!(
			$chain,
			$asset_id,
			$asset_owner,
			1_000_000_000_000,
			2_000_000_000_000
		);
	};

	// custom amounts
	( $chain:ident, $asset_id:expr, $asset_owner:expr, $wnd_amount:expr, $asset_amount:expr ) => {
		$crate::pools::paste::paste! {
			<$chain>::execute_with(|| {
				let owner = $asset_owner;
				let signed_owner = <$chain as Chain>::RuntimeOrigin::signed(owner.clone());

				let asset_id = match $asset_id.interior.last() {
					Some(GeneralIndex(id)) => *id as u32,
					_ => unreachable!(),
				};
				assert_ok!(<$chain as [<$chain Pallet>]>::Assets::mint(
					signed_owner.clone(),
					asset_id.into(),
					owner.clone().into(),
					10_000_000_000_000, // For it to have more than enough.
				));
			});

			let parent_location: Location = Parent.into();
			$crate::create_pool_with_native_location_on!($chain, parent_location, $asset_id, $asset_owner, $wnd_amount, $asset_amount);
		}
	};
}

#[macro_export]
macro_rules! create_foreign_pool_with_parent_native_on {
	// default amounts and pallet name
	( $chain:ident, $asset_id:expr, $asset_owner:expr ) => {
		$crate::create_foreign_pool_with_parent_native_on!(
			$chain,
			ForeignAssets,
			$asset_id,
			$asset_owner,
			1_000_000_000_000,
			2_000_000_000_000
		);
	};

	// default amounts, custom pallet name
	( $chain:ident, $foreign_pallet_assets:ident, $asset_id:expr, $asset_owner:expr ) => {
		$crate::create_foreign_pool_with_parent_native_on!(
			$chain,
			$foreign_pallet_assets,
			$asset_id,
			$asset_owner,
			1_000_000_000_000,
			2_000_000_000_000
		);
	};

	// custom amounts, default pallet name
	( $chain:ident, $asset_id:expr, $asset_owner:expr, $wnd_amount:expr, $asset_amount:expr ) => {
		$crate::create_foreign_pool_with_parent_native_on!(
			$chain,
			ForeignAssets,
			$asset_id,
			$asset_owner,
			$wnd_amount,
			$asset_amount
		);
	};

	// custom amounts, custom pallet name
	( $chain:ident, $foreign_pallet_assets:ident, $asset_id:expr, $asset_owner:expr, $wnd_amount:expr, $asset_amount:expr ) => {
		$crate::pools::paste::paste! {
			<$chain>::execute_with(|| {
				let owner = $asset_owner;
				let signed_owner = <$chain as Chain>::RuntimeOrigin::signed(owner.clone());

				assert_ok!(<$chain as [<$chain Pallet>]>::$foreign_pallet_assets::mint(
						signed_owner.clone(),
						$asset_id.clone().into(),
						owner.clone().into(),
						10_000_000_000_000, // For it to have more than enough.
				));
			});

			let wnd_location: Location = Parent.into();
			$crate::create_pool_with_native_location_on!($chain, wnd_location, $asset_id, $asset_owner, $wnd_amount, $asset_amount);
		}
	};
}

#[macro_export]
macro_rules! create_foreign_pool_with_native_on {
	// default amounts and pallet name
	( $chain:ident, $asset_id:expr, $asset_owner:expr ) => {
		$crate::create_foreign_pool_with_native_on!(
			$chain,
			Assets,
			$asset_id,
			$asset_owner,
			1_000_000_000_000,
			2_000_000_000_000
		);
	};

	// default amounts, custom pallet name
	( $chain:ident, $foreign_pallet_asset:ident, $asset_id:expr, $asset_owner:expr ) => {
		$crate::create_foreign_pool_with_native_on!(
			$chain,
			$foreign_pallet_asset,
			$asset_id,
			$asset_owner,
			1_000_000_000_000,
			2_000_000_000_000
		);
	};

	// custom amounts, default pallet name
	( $chain:ident, $asset_id:expr, $asset_owner:expr, $wnd_amount:expr, $asset_amount:expr ) => {
		$crate::create_foreign_pool_with_native_on!(
			$chain,
			Assets,
			$asset_id,
			$asset_owner,
			$wnd_amount,
			$asset_amount
		);
	};

	// custom amounts, custom pallet name
	( $chain:ident, $foreign_asset_pallet:ident, $asset_id:expr, $asset_owner:expr, $wnd_amount:expr, $asset_amount:expr ) => {
		$crate::pools::paste::paste! {
			<$chain>::execute_with(|| {
				let owner = $asset_owner;
				let signed_owner = <$chain as Chain>::RuntimeOrigin::signed(owner.clone());

				assert_ok!(<$chain as [<$chain Pallet>]>::$foreign_asset_pallet::mint(
						signed_owner.clone(),
						$asset_id.clone().into(),
						owner.clone().into(),
						10_000_000_000_000, // For it to have more than enough.
				));
			});

			let native_location: Location = Here.into();
			$crate::create_pool_with_native_location_on!($chain, native_location, $asset_id, $asset_owner, $wnd_amount, $asset_amount);
		}
	};
}
