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

use super::*;
use crate as pallet_derivatives;
use frame_support::{assert_err, assert_ok, traits::tokens::asset_ops::common_strategies::*};
use mock::*;

use xcm::prelude::*;
use xcm_executor::XcmExecutor;

#[test]
fn predefined_id_collection() {
	new_test_ext().execute_with(|| {
		let id = AssetId(Location::new(1, [Parachain(1111), PalletInstance(42), GeneralIndex(1)]));

		// An invalid origin is rejected.
		assert_err!(
			PredefinedIdDerivativeCollections::create_derivative(RuntimeOrigin::none(), id.clone()),
			DispatchError::BadOrigin,
		);

		assert_ok!(PredefinedIdDerivativeCollections::create_derivative(
			RuntimeOrigin::signed(1),
			id.clone()
		));

		// EnsureDerivativeCreateOrigin yielded a strategy to assign the item's owner to the
		// parachain's sovereign account.
		let owner =
			unique_items::ItemOwner::<Test, PredefinedIdCollectionsInstance>::get(&id).unwrap();

		assert_eq!(owner, 1111);

		// The inner errors are propagated
		assert_err!(
			PredefinedIdDerivativeCollections::create_derivative(
				RuntimeOrigin::signed(2),
				id.clone()
			),
			unique_items::Error::<Test, PredefinedIdCollectionsInstance>::AlreadyExists,
		);

		// An invalid origin is rejected.
		assert_err!(
			PredefinedIdDerivativeCollections::destroy_derivative(
				RuntimeOrigin::signed(1),
				id.clone()
			),
			DispatchError::BadOrigin,
		);

		assert_ok!(PredefinedIdDerivativeCollections::destroy_derivative(
			RuntimeOrigin::root(),
			id.clone()
		));
		assert!(
			unique_items::ItemOwner::<Test, PredefinedIdCollectionsInstance>::get(&id).is_none()
		);

		// Only the assets that have the reserve location convertible to an account
		// cna be registered as derivatives
		let invalid_id = AssetId(Location::new(0, [PalletInstance(42), GeneralIndex(1)]));
		assert_err!(
			PredefinedIdDerivativeCollections::create_derivative(RuntimeOrigin::signed(3), invalid_id),
			pallet_derivatives::Error::<Test, PredefinedIdDerivativeCollectionsInstance>::InvalidAsset,
		);
	});
}

#[test]
fn auto_id_collection() {
	new_test_ext().execute_with(|| {
		let id_a =
			AssetId(Location::new(1, [Parachain(2222), PalletInstance(42), GeneralIndex(1)]));
		let id_b =
			AssetId(Location::new(1, [Parachain(3333), PalletInstance(42), GeneralIndex(2)]));

		// An invalid origin is rejected.
		assert_err!(
			AutoIdDerivativeCollections::create_derivative(RuntimeOrigin::none(), id_a.clone()),
			DispatchError::BadOrigin,
		);

		assert_ok!(AutoIdDerivativeCollections::create_derivative(
			RuntimeOrigin::signed(3),
			id_a.clone()
		));

		let derivative_id_a = AutoIdDerivativeCollections::get_derivative(&id_a).unwrap();

		// EnsureDerivativeCreateOrigin yielded a strategy to assign the item's owner to the
		// parachain's sovereign account.
		let owner_a =
			unique_items::ItemOwner::<Test, AutoIdCollectionsInstance>::get(&derivative_id_a)
				.unwrap();

		assert_eq!(owner_a, 2222);

		// The stored mapping prevents derivative duplication
		assert_err!(
			AutoIdDerivativeCollections::create_derivative(RuntimeOrigin::signed(4), id_a.clone()),
			pallet_derivatives::Error::<Test, AutoIdCollectionsInstance>::DerivativeAlreadyExists,
		);

		assert_ok!(AutoIdDerivativeCollections::create_derivative(
			RuntimeOrigin::signed(5),
			id_b.clone()
		));

		let derivative_id_b = AutoIdDerivativeCollections::get_derivative(&id_b).unwrap();

		assert_ne!(derivative_id_a, derivative_id_b);

		// EnsureDerivativeCreateOrigin yielded a strategy to assign the item's owner to the
		// parachain's sovereign account.
		let owner_b =
			unique_items::ItemOwner::<Test, AutoIdCollectionsInstance>::get(&derivative_id_b)
				.unwrap();

		assert_eq!(owner_b, 3333);

		// The stored mapping prevents derivative duplication
		assert_err!(
			AutoIdDerivativeCollections::create_derivative(RuntimeOrigin::signed(6), id_b.clone()),
			pallet_derivatives::Error::<Test, AutoIdCollectionsInstance>::DerivativeAlreadyExists,
		);

		// An invalid origin is rejected.
		assert_err!(
			AutoIdDerivativeCollections::destroy_derivative(RuntimeOrigin::signed(7), id_a.clone()),
			DispatchError::BadOrigin,
		);

		assert_ok!(AutoIdDerivativeCollections::destroy_derivative(
			RuntimeOrigin::root(),
			id_a.clone()
		));
		assert!(unique_items::ItemOwner::<Test, AutoIdCollectionsInstance>::get(&derivative_id_a)
			.is_none());

		assert_ok!(AutoIdDerivativeCollections::destroy_derivative(
			RuntimeOrigin::root(),
			id_b.clone()
		));
		assert!(unique_items::ItemOwner::<Test, AutoIdCollectionsInstance>::get(&derivative_id_b)
			.is_none());

		// Only the assets that have the reserve location convertible to an account
		// cna be registered as derivatives
		let invalid_id = AssetId(Location::new(0, [PalletInstance(42), GeneralIndex(1)]));
		assert_err!(
			AutoIdDerivativeCollections::create_derivative(RuntimeOrigin::signed(8), invalid_id),
			pallet_derivatives::Error::<Test, AutoIdDerivativeCollectionsInstance>::InvalidAsset,
		);
	});
}

#[test]
fn duplicate_derivative_id_is_rejected() {
	new_test_ext().execute_with(|| {
		let id_a = AssetId(Location::new(1, [Parachain(1111)]));
		let id_b = AssetId(Location::new(1, [Parachain(2222)]));
		let derivative_id = 100;

		assert_ok!(AutoIdDerivativeCollections::try_register_derivative(&id_a, &derivative_id));
		assert_err!(
			AutoIdDerivativeCollections::try_register_derivative(&id_b, &derivative_id),
			pallet_derivatives::Error::<Test, AutoIdCollectionsInstance>::DerivativeAlreadyExists,
		);

		assert_eq!(AutoIdDerivativeCollections::get_derivative(&id_a), Ok(derivative_id));
		assert_eq!(AutoIdDerivativeCollections::get_original(&derivative_id), Ok(id_a));
		assert_err!(
			AutoIdDerivativeCollections::get_derivative(&id_b),
			pallet_derivatives::Error::<Test, AutoIdCollectionsInstance>::DerivativeNotFound,
		);
	});
}

#[test]
fn local_nfts() {
	new_test_ext().execute_with(|| {
		let collection_owner = 1;
		let nft_initial_owner = 2;
		let nft_beneficiary = 3;

		// Create NFT collection
		let collection_id = AutoIdCollections::create(WithConfig::new(
			ConfigValue(collection_owner),
			AutoId::auto(),
		))
		.unwrap();

		// Mint NFT within the collection
		let nft_local_id = 112;
		assert_ok!(PredefinedIdNfts::create(WithConfig::new(
			ConfigValue(nft_initial_owner),
			PredefinedId::from((collection_id, nft_local_id)),
		)));

		// The NFT should be deposited to the correct account
		assert_eq!(
			unique_items::ItemOwner::<Test, PredefinedIdNftsInstance>::get(&(
				collection_id,
				nft_local_id
			)),
			Some(nft_initial_owner),
		);

		let local_nfts_pallet_index = <PredefinedIdNfts as PalletInfoAccess>::index() as u8;
		let nft_asset: Asset = (
			(PalletInstance(local_nfts_pallet_index), GeneralIndex(collection_id.into())),
			Index(nft_local_id.into()),
		)
			.into();
		let nft_beneficiary_location = AccountIndex64 { index: nft_beneficiary, network: None };

		let message = Xcm::builder_unpaid()
			.unpaid_execution(Unlimited, None)
			.withdraw_asset(nft_asset)
			.deposit_asset(AllCounted(1), nft_beneficiary_location)
			.build();

		let origin: Location = AccountIndex64 { index: nft_initial_owner, network: None }.into();
		let mut hash = message.using_encoded(sp_io::hashing::blake2_256);

		// Transfer the NFT from one account to another
		// This NFT is local, it should be handled by the LocalNftsTransactor
		let outcome = XcmExecutor::<XcmConfig>::prepare_and_execute(
			origin,
			message,
			&mut hash,
			Weight::MAX,
			Weight::zero(),
		);
		assert_ok!(outcome.ensure_complete());

		// The NFT should be deposited to the correct beneficiary
		assert_eq!(
			unique_items::ItemOwner::<Test, PredefinedIdNftsInstance>::get(&(
				collection_id,
				nft_local_id
			)),
			Some(nft_beneficiary),
		);
	});
}

#[test]
fn derivative_nfts() {
	sp_tracing::try_init_simple();
	new_test_ext().execute_with(|| {
		let foreign_para_id = 2222;

		// Create derivative NFT collection
		let foreign_collection_id = AssetId(Location::new(
			1,
			[Parachain(foreign_para_id), PalletInstance(42), GeneralIndex(1)],
		));
		let foreign_nft_id = Index(112);
		assert_ok!(AutoIdDerivativeCollections::create_derivative(
			RuntimeOrigin::signed(1),
			foreign_collection_id.clone(),
		));

		let derivative_collection_id =
			AutoIdDerivativeCollections::get_derivative(&foreign_collection_id).unwrap();

		// There is no derivative NFT yet
		assert!(DerivativeNfts::get_derivative(&(foreign_collection_id.clone(), foreign_nft_id))
			.is_err());

		let nft_beneficiary = 1;

		let nft_asset: Asset = (foreign_collection_id.clone(), foreign_nft_id).into();
		let nft_beneficiary_location = AccountIndex64 { index: nft_beneficiary, network: None };

		let deposited_assets: Assets = nft_asset.clone().into();
		let message = Xcm::builder_unpaid()
			.unpaid_execution(Unlimited, None)
			.reserve_asset_deposited(deposited_assets)
			.deposit_asset(AllCounted(1), nft_beneficiary_location)
			.build();

		let origin = Location::new(1, [Parachain(foreign_para_id)]);
		let mut hash = message.using_encoded(sp_io::hashing::blake2_256);

		// Deposit a foreign NFT (i.e., create a derivative NFT)
		let outcome = XcmExecutor::<XcmConfig>::prepare_and_execute(
			origin,
			message,
			&mut hash,
			Weight::MAX,
			Weight::zero(),
		);
		assert_ok!(outcome.ensure_complete());

		// The derivative NFT should exist now
		let derivative_full_nft_id =
			DerivativeNfts::get_derivative(&(foreign_collection_id.clone(), foreign_nft_id))
				.unwrap();

		// The derivative NFT should be deposited to the correct beneficiary
		assert_eq!(
			unique_items::ItemOwner::<Test, PredefinedIdNftsInstance>::get(&derivative_full_nft_id),
			Some(nft_beneficiary),
		);
		// The derivative NFT is deposited within the correct collection
		assert_eq!(derivative_collection_id, derivative_full_nft_id.0);

		let derivative_local_nft_id = derivative_full_nft_id.1;

		let nft_owner = nft_beneficiary;
		let another_nft_beneficiary = nft_beneficiary + 1;

		let local_nfts_pallet_index = <PredefinedIdNfts as PalletInfoAccess>::index() as u8;
		let nft_asset_as_local: Asset = (
			(
				PalletInstance(local_nfts_pallet_index),
				GeneralIndex(derivative_collection_id.into()),
			),
			Index(derivative_local_nft_id.into()),
		)
			.into();
		let another_nft_beneficiary_location =
			AccountIndex64 { index: another_nft_beneficiary, network: None };
		let message = Xcm::builder_unpaid()
			.unpaid_execution(Unlimited, None)
			.withdraw_asset(nft_asset_as_local)
			.deposit_asset(AllCounted(1), another_nft_beneficiary_location)
			.build();

		let origin: Location = AccountIndex64 { index: nft_owner, network: None }.into();
		let mut hash = message.using_encoded(sp_io::hashing::blake2_256);

		// Try to transfer the derivative NFT as if it were a local one
		// (this might lead the chain to act as a reserve location for NFTs which doesn't belong to
		// it).
		//
		// The LocalNftsTransactor should prevent this as it checks the NFT for being
		// non-derivative.
		let outcome = XcmExecutor::<XcmConfig>::prepare_and_execute(
			origin,
			message,
			&mut hash,
			Weight::MAX,
			Weight::zero(),
		);
		assert_err!(
			outcome.ensure_complete(),
			InstructionError { index: 1, error: XcmError::AssetNotFound }
		);

		let message = Xcm::builder_unpaid()
			.unpaid_execution(Unlimited, None)
			.withdraw_asset(nft_asset)
			.deposit_asset(AllCounted(1), another_nft_beneficiary_location)
			.build();
		let origin: Location = AccountIndex64 { index: nft_owner, network: None }.into();
		let mut hash = message.using_encoded(sp_io::hashing::blake2_256);

		// Transfer the derivative NFT from one account to another
		let outcome = XcmExecutor::<XcmConfig>::prepare_and_execute(
			origin,
			message,
			&mut hash,
			Weight::MAX,
			Weight::zero(),
		);
		assert_ok!(outcome.ensure_complete());

		// The derivative NFT should be deposited to the correct beneficiary
		assert_eq!(
			unique_items::ItemOwner::<Test, PredefinedIdNftsInstance>::get(&derivative_full_nft_id),
			Some(another_nft_beneficiary),
		);
	});
}
