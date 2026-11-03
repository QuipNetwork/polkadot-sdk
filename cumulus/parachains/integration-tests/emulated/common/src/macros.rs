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

// Substrate
pub use frame_support::{pallet_prelude::Weight, weights::WeightToFee};
pub use pallet_asset_conversion;
pub use pallet_assets;
pub use pallet_balances;
pub use pallet_message_queue;
pub use pallet_whitelist;
pub use pallet_xcm;

pub use frame_support::{
	assert_ok,
	storage::{with_transaction_unchecked, TransactionOutcome},
};

// Polkadot
pub use polkadot_runtime_parachains::dmp::Pallet as Dmp;
pub use xcm::{
	latest::AssetTransferFilter,
	prelude::{
		AliasOrigin, All, AllCounted, Asset, AssetId, Assets, BuyExecution, DepositAsset,
		ExpectTransactStatus, Fungible, GeneralIndex, Here, InitiateTransfer, Junction, Location,
		MaybeErrorCode, OriginKind, Outcome, PalletInstance, Parachain, Parent, PayFees,
		RefundSurplus, Transact, Unlimited, VersionedAssetId, VersionedAssets, VersionedLocation,
		VersionedXcm, WeightLimit, Wild, WithdrawAsset, Xcm, XcmContext, XCM_VERSION,
	},
};

pub use xcm_executor::traits::{DropAssets, TransferType};

// Cumulus
pub use asset_test_utils;
pub use cumulus_pallet_xcmp_queue;
pub use parachains_common::AccountId;
pub use xcm_emulator::{
	assert_expected_events, Chain, Parachain as Para, RelayChain, TestArgs, TestContext, TestExt,
};

pub use frame_support::{
	dispatch::{GetDispatchInfo, RawOrigin},
	BoundedVec,
};
pub use xcm_runtime_apis::{
	dry_run::runtime_decl_for_dry_run_api::DryRunApiV2,
	fees::{runtime_decl_for_xcm_payment_api::XcmPaymentApiV2, Error as XcmPaymentApiError},
};

pub use frame_support::traits::{fungible::Mutate, fungibles::Inspect, Currency};
pub use sp_runtime::{traits::Dispatchable, AccountId32};

pub use crate::{create_foreign_pool_with_native_on, ASSETS_PALLET_ID, USDT_ID};

#[macro_export]
macro_rules! test_parachain_is_trusted_teleporter {
	( $sender_para:ty, vec![$( $receiver_para:ty ),+], ($assets:expr, $amount:expr), $xcm_call:ident ) => {
		$crate::macros::paste::paste! {
			// init Origin variables
			let sender = [<$sender_para Sender>]::get();
			let mut para_sender_balance_before =
				<$sender_para as $crate::macros::Chain>::account_data_of(sender.clone()).free;
			let origin = <$sender_para as $crate::macros::Chain>::RuntimeOrigin::signed(sender.clone());
			let fee_asset_item = 0;
			let weight_limit = $crate::macros::WeightLimit::Unlimited;

			$(
				{
					// init Destination variables
					let receiver = [<$receiver_para Receiver>]::get();
					let para_receiver_balance_before =
						<$receiver_para as $crate::macros::Chain>::account_data_of(receiver.clone()).free;
					let para_destination =
						<$sender_para as $crate::macros::Para>::sibling_location_of(
							<$receiver_para as $crate::macros::Para>::para_id());
					let beneficiary: $crate::macros::Location =
						$crate::macros::Junction::AccountId32 { network: None, id: receiver.clone().into() }.into();

					// Dry-run first.
					let call = <$sender_para as $crate::macros::Chain>::RuntimeCall::PolkadotXcm(
						$crate::macros::pallet_xcm::Call::$xcm_call {
						dest: Box::new(para_destination.clone().into()),
						beneficiary: Box::new(beneficiary.clone().into()),
						assets: Box::new($assets.clone().into()),
						fee_asset_item: fee_asset_item,
						weight_limit: weight_limit.clone(),
					});

					// assume up to 90% of max weight
					let max_weight_with_margin_for_error = ($crate::macros::Weight::MAX.ref_time() / 100) * 90;
					assert!(<_ as $crate::macros::GetDispatchInfo>::get_dispatch_info(&call)
						.call_weight.ref_time() < max_weight_with_margin_for_error);

					let mut delivery_fees_amount = 0;
					let mut remote_message = $crate::macros::VersionedXcm::from($crate::macros::Xcm(Vec::new()));
					<$sender_para as $crate::macros::TestExt>::execute_with(|| {
						type Runtime = <$sender_para as $crate::macros::Chain>::Runtime;
						type OriginCaller = <$sender_para as $crate::macros::Chain>::OriginCaller;

						let origin = OriginCaller::system($crate::macros::RawOrigin::Signed(sender.clone()));
						let result = <Runtime as $crate::macros::DryRunApiV2<_,_,_,_>>::dry_run_call(origin, call.clone(),
							$crate::macros::XCM_VERSION).unwrap();


						// We filter the result to get only the messages we are interested in.
						let (destination_to_query, messages_to_query) = &result
							.forwarded_xcms
							.iter()
							.find(|(destination, _)| {
								*destination == $crate::macros::VersionedLocation::from($crate::macros::Location::new(1,
									[$crate::macros::Parachain(<$receiver_para as $crate::macros::Para>::para_id().into())]))
							})
							.unwrap();
						assert_eq!(messages_to_query.len(), 1);
						remote_message = messages_to_query[0].clone();
						// The native asset of every system chain.
						// We want to get the delivery fees in this asset.
						let asset_id_for_delivery_fees = VersionedAssetId::from(Location::new(1, []));
						let delivery_fees =
							<Runtime as $crate::macros::XcmPaymentApiV2<_>>::query_delivery_fees(
								destination_to_query.clone(),
								remote_message.clone(),
								asset_id_for_delivery_fees
							).unwrap();
						let latest_delivery_fees: $crate::macros::Assets = delivery_fees.clone().try_into().unwrap();
						let $crate::macros::Fungible(inner_delivery_fees_amount) = latest_delivery_fees.inner()[0].fun else {
							unreachable!("asset is non-fungible");
						};
						delivery_fees_amount = inner_delivery_fees_amount;
					});

					let receiver_total_issuance_before = <$receiver_para as $crate::macros::TestExt>::execute_with(|| {
						<<$receiver_para as [<$receiver_para Pallet>]>::Balances
							as $crate::macros::Currency<_>>::total_issuance()
					});
					// Send XCM message from Origin Parachain
					<$sender_para as $crate::macros::TestExt>::execute_with(|| {
						let total_issuance_source_of_truth = <$sender_para as $crate::macros::Chain>::native_total_issuance_source_of_truth();
						let total_issuance_before = <<$sender_para as [<$sender_para Pallet>]>::Balances
							as $crate::macros::Currency<_>>::total_issuance();
						let origin = <$sender_para as $crate::macros::Chain>::RuntimeOrigin::signed(sender.clone());
						$crate::macros::assert_ok!(<_ as $crate::macros::Dispatchable>::dispatch(call, origin));
						let total_issuance_after = <<$sender_para as [<$sender_para Pallet>]>::Balances
							as $crate::macros::Currency<_>>::total_issuance();

						type RuntimeEvent = <$sender_para as $crate::macros::Chain>::RuntimeEvent;

						$crate::macros::assert_expected_events!(
							$sender_para,
							vec![
								RuntimeEvent::PolkadotXcm(
									$crate::macros::pallet_xcm::Event::Attempted { outcome: $crate::macros::Outcome::Complete { .. } }
								) => {},
								RuntimeEvent::XcmpQueue(
									$crate::macros::cumulus_pallet_xcmp_queue::Event::XcmpMessageSent { .. }
								) => {},
								RuntimeEvent::Balances(
									$crate::macros::pallet_balances::Event::Withdraw { who, .. }
								) => {
									who: *who == sender,
								},
							]
						);
						if total_issuance_source_of_truth {
							assert_eq!(
								total_issuance_after, total_issuance_before,
								"Unexpected change in sender native token total issuance source of truth"
							);
						} else {
							assert_eq!(
								total_issuance_after, total_issuance_before - $amount,
								"Native token total issuance should have decreased on sender"
							);
						}
					});

					// Receive XCM message in Destination Parachain
					<$receiver_para as $crate::macros::TestExt>::execute_with(|| {
						type RuntimeEvent = <$receiver_para as $crate::macros::Chain>::RuntimeEvent;

						$crate::macros::assert_expected_events!(
							$receiver_para,
							vec![
								RuntimeEvent::Balances(
									$crate::macros::pallet_balances::Event::Deposit { who, .. }
								) => {
									who: *who == receiver,
								},
								RuntimeEvent::MessageQueue(
									$crate::macros::pallet_message_queue::Event::Processed { success: true, .. }
								) => {},
							]
						);
						let receiver_total_issuance_after = <<$receiver_para as [<$receiver_para Pallet>]>::Balances
							as $crate::macros::Currency<_>>::total_issuance();
						if <$receiver_para as $crate::macros::Chain>::native_total_issuance_source_of_truth() {
							assert_eq!(
								receiver_total_issuance_after, receiver_total_issuance_before,
								"Unexpected change in receiver native token total issuance source of truth"
							);
						} else {
							assert!(
								receiver_total_issuance_after > receiver_total_issuance_before,
								"Native token total issuance should have increased on receiver"
							);
						}
					});

					// Check if balances are updated accordingly in Origin and Destination Parachains
					let para_sender_balance_after =
						<$sender_para as $crate::macros::Chain>::account_data_of(sender.clone()).free;
					let para_receiver_balance_after =
						<$receiver_para as $crate::macros::Chain>::account_data_of(receiver.clone()).free;

					assert_eq!(para_sender_balance_before - $amount - delivery_fees_amount, para_sender_balance_after);
					assert!(para_receiver_balance_after > para_receiver_balance_before);

					// Update sender balance
					para_sender_balance_before = <$sender_para as $crate::macros::Chain>::account_data_of(sender.clone()).free;
				}
			)+
		}
	};
}

#[macro_export]
macro_rules! test_relay_is_trusted_teleporter {
	( $sender_relay:ty, vec![$( $receiver_para:ty ),+], ($assets:expr, $amount:expr), $xcm_call:ident ) => {
		$crate::macros::paste::paste! {
			// init Origin variables
			let sender = [<$sender_relay Sender>]::get();
			let mut relay_sender_balance_before =
				<$sender_relay as $crate::macros::Chain>::account_data_of(sender.clone()).free;
			let fee_asset_item = 0;
			let weight_limit = $crate::macros::WeightLimit::Unlimited;

			$(
				{
					// init Destination variables
					let receiver = [<$receiver_para Receiver>]::get();
					let para_receiver_balance_before =
						<$receiver_para as $crate::macros::Chain>::account_data_of(receiver.clone()).free;
					let para_destination =
						<$sender_relay as $crate::macros::RelayChain>::child_location_of(
							<$receiver_para as $crate::macros::Para>::para_id());
					let beneficiary: $crate::macros::Location =
						$crate::macros::Junction::AccountId32 { network: None, id: receiver.clone().into() }.into();

					// Dry-run first.
					let call = <$sender_relay as $crate::macros::Chain>::RuntimeCall::XcmPallet(
						$crate::macros::pallet_xcm::Call::$xcm_call {
						dest: Box::new(para_destination.clone().into()),
						beneficiary: Box::new(beneficiary.clone().into()),
						assets: Box::new($assets.clone().into()),
						fee_asset_item: fee_asset_item,
						weight_limit: weight_limit.clone(),
					});

					// verify sane weight for a call
					// assume up to 90% of max weight
					let max_weight_with_margin_for_error = ($crate::macros::Weight::MAX.ref_time() / 100) * 90;
					assert!(<_ as $crate::macros::GetDispatchInfo>::get_dispatch_info(&call)
						.call_weight.ref_time() < max_weight_with_margin_for_error);

					let mut delivery_fees_amount = 0;
					let mut remote_message = $crate::macros::VersionedXcm::from($crate::macros::Xcm(Vec::new()));
					<$sender_relay as $crate::macros::TestExt>::execute_with(|| {
						$crate::macros::Dmp::<<$sender_relay as $crate::macros::Chain>::Runtime>::make_parachain_reachable(
							<$receiver_para as $crate::macros::Para>::para_id());
						type Runtime = <$sender_relay as $crate::macros::Chain>::Runtime;
						type OriginCaller = <$sender_relay as $crate::macros::Chain>::OriginCaller;

						let origin = OriginCaller::system($crate::macros::RawOrigin::Signed(sender.clone()));
						let result = <Runtime as $crate::macros::DryRunApiV2<_,_,_,_>>::dry_run_call(origin, call.clone(),
							$crate::macros::XCM_VERSION).unwrap();
						// We filter the result to get only the messages we are interested in.
						let (destination_to_query, messages_to_query) = &result
							.forwarded_xcms
							.iter()
							.find(|(destination, _)| {
								*destination == $crate::macros::VersionedLocation::from($crate::macros::Location::new(0,
									[$crate::macros::Parachain(<$receiver_para as $crate::macros::Para>::para_id().into())]))
							})
							.unwrap();
						assert_eq!(messages_to_query.len(), 1);
						remote_message = messages_to_query[0].clone();
						// The native asset of every system chain.
						// We want to get the delivery fees in this asset.
						let asset_id_for_delivery_fees = VersionedAssetId::from(Location::new(0, []));
						let delivery_fees =
							<Runtime as $crate::macros::XcmPaymentApiV2<_>>::query_delivery_fees(
								destination_to_query.clone(),
								remote_message.clone(),
								asset_id_for_delivery_fees
							).unwrap();
						let latest_delivery_fees: $crate::macros::Assets = delivery_fees.clone().try_into().unwrap();
						let $crate::macros::Fungible(inner_delivery_fees_amount) = latest_delivery_fees.inner()[0].fun else {
							unreachable!("asset is non-fungible");
						};
						delivery_fees_amount = inner_delivery_fees_amount;
					});

					// Reset to send actual message.
					<$sender_relay as $crate::macros::TestExt>::reset_ext();
					<$receiver_para as $crate::macros::TestExt>::reset_ext();

					// Send XCM message from Relay.
					<$sender_relay as $crate::macros::TestExt>::execute_with(|| {
						$crate::macros::Dmp::<<$sender_relay as $crate::macros::Chain>::Runtime>::make_parachain_reachable(
							<$receiver_para as $crate::macros::Para>::para_id());
						let origin = <$sender_relay as $crate::macros::Chain>::RuntimeOrigin::signed(sender.clone());
						$crate::macros::assert_ok!(<_ as $crate::macros::Dispatchable>::dispatch(call, origin));

						type RuntimeEvent = <$sender_relay as $crate::macros::Chain>::RuntimeEvent;

						$crate::macros::assert_expected_events!(
							$sender_relay,
							vec![
								RuntimeEvent::XcmPallet(
									$crate::macros::pallet_xcm::Event::Attempted { outcome: $crate::macros::Outcome::Complete { .. } }
								) => {},
								RuntimeEvent::Balances(
									$crate::macros::pallet_balances::Event::Withdraw { who, .. }
								) => {
									who: *who == sender,
								},
								RuntimeEvent::XcmPallet(
									$crate::macros::pallet_xcm::Event::Sent { .. }
								) => {},
							]
						);
					});

					// Receive XCM message in Destination Parachain
					<$receiver_para as $crate::macros::TestExt>::execute_with(|| {
						type RuntimeEvent = <$receiver_para as $crate::macros::Chain>::RuntimeEvent;

						$crate::macros::assert_expected_events!(
							$receiver_para,
							vec![
								RuntimeEvent::Balances(
									$crate::macros::pallet_balances::Event::Deposit { who, .. }
								) => {
									who: *who == receiver,
								},
								RuntimeEvent::MessageQueue(
									$crate::macros::pallet_message_queue::Event::Processed { success: true, .. }
								) => {},
							]
						);
					});

					// Check if balances are updated accordingly in Origin and Parachain
					let relay_sender_balance_after =
						<$sender_relay as $crate::macros::Chain>::account_data_of(sender.clone()).free;
					let para_receiver_balance_after =
						<$receiver_para as $crate::macros::Chain>::account_data_of(receiver.clone()).free;

					assert_eq!(relay_sender_balance_before - $amount - delivery_fees_amount, relay_sender_balance_after);
					assert!(para_receiver_balance_after > para_receiver_balance_before);

					// Update sender balance
					relay_sender_balance_before = <$sender_relay as $crate::macros::Chain>::account_data_of(sender.clone()).free;
				}
			)+
		}
	};
}

#[macro_export]
macro_rules! test_parachain_is_trusted_teleporter_for_relay {
	( $sender_para:ty, $receiver_relay:ty, $amount:expr, $xcm_call:ident ) => {
		$crate::macros::paste::paste! {
			// init Origin variables
			let sender = [<$sender_para Sender>]::get();
			// Mint assets to `$sender_para` to succeed with teleport.
			<$sender_para as $crate::macros::TestExt>::execute_with(|| {
				$crate::macros::assert_ok!(<<$sender_para as [<$sender_para Pallet>]>::Balances
					as $crate::macros::Mutate<_>>::mint_into(&sender, $amount + 10_000_000_000));

			});
			let mut para_sender_balance_before =
				<$sender_para as $crate::macros::Chain>::account_data_of(sender.clone()).free;
			let origin = <$sender_para as $crate::macros::Chain>::RuntimeOrigin::signed(sender.clone());
			let assets: $crate::macros::Assets = ($crate::macros::Parent, $amount).into();
			let fee_asset_item = 0;
			let weight_limit = $crate::macros::WeightLimit::Unlimited;

			// We need to mint funds into the checking account of `$receiver_relay`
			// for it to accept a teleport from `$sender_para`.
			// Else we'd get a `NotWithdrawable` error since it tries to reduce the check account balance, which
			// would be 0.
			<$receiver_relay as $crate::macros::TestExt>::execute_with(|| {
				let check_account = <$receiver_relay as [<$receiver_relay Pallet>]>::XcmPallet::check_account();
				$crate::macros::assert_ok!(<<$receiver_relay as [<$receiver_relay Pallet>]>::Balances
					as $crate::macros::Mutate<_>>::mint_into(&check_account, $amount));
			});

			// Init destination variables.
			let receiver = [<$receiver_relay Receiver>]::get();
			let relay_receiver_balance_before =
				<$receiver_relay as $crate::macros::Chain>::account_data_of(receiver.clone()).free;
			let relay_destination: $crate::macros::Location = $crate::macros::Parent.into();
			let beneficiary: $crate::macros::Location =
				$crate::macros::Junction::AccountId32 { network: None, id: receiver.clone().into() }.into();

			// Dry-run first.
			let call = <$sender_para as $crate::macros::Chain>::RuntimeCall::PolkadotXcm($crate::macros::pallet_xcm::Call::$xcm_call {
				dest: Box::new(relay_destination.clone().into()),
				beneficiary: Box::new(beneficiary.clone().into()),
				assets: Box::new(assets.clone().into()),
				fee_asset_item: fee_asset_item,
				weight_limit: weight_limit.clone(),
			});

			// verify sane weight for a call
			// assume up to 90% of max weight
			let max_weight_with_margin_for_error = ($crate::macros::Weight::MAX.ref_time() / 100) * 90;
			assert!(<_ as $crate::macros::GetDispatchInfo>::get_dispatch_info(&call)
				.call_weight.ref_time() < max_weight_with_margin_for_error);

			// These will be filled in the closure.
			let mut delivery_fees_amount = 0;
			let mut remote_message = $crate::macros::VersionedXcm::from($crate::macros::Xcm(Vec::new()));
			<$sender_para as $crate::macros::TestExt>::execute_with(|| {
				type Runtime = <$sender_para as $crate::macros::Chain>::Runtime;
				type OriginCaller = <$sender_para as $crate::macros::Chain>::OriginCaller;

				let origin = OriginCaller::system($crate::macros::RawOrigin::Signed(sender.clone()));
				let result = <Runtime as $crate::macros::DryRunApiV2<_,_,_,_>>::dry_run_call(origin, call.clone(),
					$crate::macros::XCM_VERSION).unwrap();
				// We filter the result to get only the messages we are interested in.
				let (destination_to_query, messages_to_query) = &result
					.forwarded_xcms
					.iter()
					.find(|(destination, _)| {
						*destination == $crate::macros::VersionedLocation::from($crate::macros::Location::parent())
					})
					.unwrap();
				assert_eq!(messages_to_query.len(), 1);
				remote_message = messages_to_query[0].clone();
				// The native asset of every system chain.
				// We want to get the delivery fees in this asset.
				let asset_id_for_delivery_fees = VersionedAssetId::from(Location::parent());
				let delivery_fees =
					<Runtime as $crate::macros::XcmPaymentApiV2<_>>::query_delivery_fees(
						destination_to_query.clone(),
						remote_message.clone(),
						asset_id_for_delivery_fees
					).unwrap();
				let latest_delivery_fees: $crate::macros::Assets = delivery_fees.clone().try_into().unwrap();
				delivery_fees_amount = if let Some(first_asset) = latest_delivery_fees.inner().first() {
					let $crate::macros::Fungible(inner_delivery_fees_amount) = first_asset.fun else {
						unreachable!("asset is non-fungible");
					};
					inner_delivery_fees_amount
				} else {
					0
				}
			});

			// Reset to send actual message.
			<$sender_para as $crate::macros::TestExt>::reset_ext();
			<$receiver_relay as $crate::macros::TestExt>::reset_ext();
			// Mint assets to `$sender_para` to succeed with teleport.
			<$sender_para as $crate::macros::TestExt>::execute_with(|| {
				$crate::macros::assert_ok!(<<$sender_para as [<$sender_para Pallet>]>::Balances
					as $crate::macros::Mutate<_>>::mint_into(&sender, $amount + 10_000_000_000));
			});

			// Since we reset everything, we need to mint funds into the checking account again.
			<$receiver_relay as $crate::macros::TestExt>::execute_with(|| {
				let check_account = <$receiver_relay as [<$receiver_relay Pallet>]>::XcmPallet::check_account();
				$crate::macros::assert_ok!(<<$receiver_relay as [<$receiver_relay Pallet>]>::Balances
					as $crate::macros::Mutate<_>>::mint_into(&check_account, $amount));
			});

			// Send XCM message from Parachain.
			<$sender_para as $crate::macros::TestExt>::execute_with(|| {
				let origin = <$sender_para as $crate::macros::Chain>::RuntimeOrigin::signed(sender.clone());
				$crate::macros::assert_ok!(<_ as $crate::macros::Dispatchable>::dispatch(call, origin));

				type RuntimeEvent = <$sender_para as $crate::macros::Chain>::RuntimeEvent;

				$crate::macros::assert_expected_events!(
					$sender_para,
					vec![
						RuntimeEvent::PolkadotXcm(
							$crate::macros::pallet_xcm::Event::Attempted { outcome: $crate::macros::Outcome::Complete { .. } }
						) => {},
						RuntimeEvent::Balances(
							$crate::macros::pallet_balances::Event::Withdraw { who, .. }
						) => {
							who: *who == sender,
						},
						RuntimeEvent::PolkadotXcm(
							$crate::macros::pallet_xcm::Event::Sent { .. }
						) => {},
					]
				);
			});

			// Receive XCM message in Destination Parachain
			<$receiver_relay as $crate::macros::TestExt>::execute_with(|| {
				type RuntimeEvent = <$receiver_relay as $crate::macros::Chain>::RuntimeEvent;

				$crate::macros::assert_expected_events!(
					$receiver_relay,
					vec![
						RuntimeEvent::Balances(
							$crate::macros::pallet_balances::Event::Deposit { who, .. }
						) => {
							who: *who == receiver,
						},
						RuntimeEvent::MessageQueue(
							$crate::macros::pallet_message_queue::Event::Processed { success: true, .. }
						) => {},
					]
				);
			});

			// Check if balances are updated accordingly in Origin and Relay Chain
			let para_sender_balance_after =
				<$sender_para as $crate::macros::Chain>::account_data_of(sender.clone()).free;
			let relay_receiver_balance_after =
				<$receiver_relay as $crate::macros::Chain>::account_data_of(receiver.clone()).free;

			assert_eq!(para_sender_balance_before - $amount - delivery_fees_amount, para_sender_balance_after);
			assert!(relay_receiver_balance_after > relay_receiver_balance_before);

			// Update sender balance
			para_sender_balance_before = <$sender_para as $crate::macros::Chain>::account_data_of(sender.clone()).free;
		}
	};
}

#[macro_export]
macro_rules! test_chain_can_claim_assets {
	( $sender_para:ty, $xcm_config:ty, $network_id:expr, $asset:expr, $amount:expr ) => {
		$crate::macros::paste::paste! {
			use xcm_executor::traits::TransactAsset;
			let sender = [<$sender_para Sender>]::get();
			let origin = <$sender_para as $crate::macros::Chain>::RuntimeOrigin::signed(sender.clone());
			// Receiver is the same as sender
			let beneficiary: $crate::macros::Location =
				$crate::macros::Junction::AccountId32 { network: Some($network_id), id: sender.clone().into() }.into();
			let assets: $crate::macros::Assets = $asset.clone().into();
			let versioned_assets: $crate::macros::VersionedAssets = assets.clone().into();
			let context = $crate::macros::XcmContext { origin: None, message_id: Default::default(), topic: None };

			<$sender_para as $crate::macros::TestExt>::execute_with(|| {
				// Mint some assets to trap.
				let holdings =
					<$xcm_config as xcm_executor::Config>::AssetTransactor::mint_asset(
						&$asset, &context,
					).unwrap();
				let total_issuance_before = <<$sender_para as [<$sender_para Pallet>]>::Balances
					as $crate::macros::Currency<_>>::total_issuance();
				// Assets are trapped for whatever reason.
				// The possible reasons for this might differ from runtime to runtime, so here we just drop them directly.
				<<$sender_para as [<$sender_para Pallet>]>::PolkadotXcm as $crate::macros::DropAssets>::drop_assets(
					&beneficiary, holdings, &context,
				);
				// assert trapping assets does not alter total issuance
				let total_issuance_after = <<$sender_para as [<$sender_para Pallet>]>::Balances
					as $crate::macros::Currency<_>>::total_issuance();
				assert_eq!(total_issuance_before, total_issuance_after);

				type RuntimeEvent = <$sender_para as $crate::macros::Chain>::RuntimeEvent;
				$crate::macros::assert_expected_events!(
					$sender_para,
					vec![
						RuntimeEvent::PolkadotXcm(
							$crate::macros::pallet_xcm::Event::AssetsTrapped { origin, assets, .. }
						) => {
							origin: *origin == beneficiary,
							assets: *assets == versioned_assets,
						},
					]
				);

				let balance_before = <<$sender_para as [<$sender_para Pallet>]>::Balances
					as $crate::macros::Currency<_>>::free_balance(&sender);

				// Different origin or different assets won't work.
				let other_origin = <$sender_para as $crate::macros::Chain>::RuntimeOrigin::signed([<$sender_para Receiver>]::get());
				assert!(<$sender_para as [<$sender_para Pallet>]>::PolkadotXcm::claim_assets(
					other_origin,
					Box::new(versioned_assets.clone().into()),
					Box::new(beneficiary.clone().into()),
				).is_err());
				let other_versioned_assets: $crate::macros::VersionedAssets = $crate::macros::Assets::new().into();
				assert!(<$sender_para as [<$sender_para Pallet>]>::PolkadotXcm::claim_assets(
					origin.clone(),
					Box::new(other_versioned_assets.into()),
					Box::new(beneficiary.clone().into()),
				).is_err());

				// Assets will be claimed to `beneficiary`, which is the same as `sender`.
				$crate::macros::assert_ok!(<$sender_para as [<$sender_para Pallet>]>::PolkadotXcm::claim_assets(
					origin.clone(),
					Box::new(versioned_assets.clone().into()),
					Box::new(beneficiary.clone().into()),
				));

				$crate::macros::assert_expected_events!(
					$sender_para,
					vec![
						RuntimeEvent::PolkadotXcm(
							$crate::macros::pallet_xcm::Event::AssetsClaimed { origin, assets, .. }
						) => {
							origin: *origin == beneficiary,
							assets: *assets == versioned_assets,
						},
					]
				);

				// After claiming the assets, the balance has increased.
				let balance_after = <<$sender_para as [<$sender_para Pallet>]>::Balances
					as $crate::macros::Currency<_>>::free_balance(&sender);
				assert_eq!(balance_after, balance_before + $amount);

				// assert claiming trapped assets does not alter total issuance
				let total_issuance_after = <<$sender_para as [<$sender_para Pallet>]>::Balances
					as $crate::macros::Currency<_>>::total_issuance();
				assert_eq!(total_issuance_before, total_issuance_after);

				// Claiming the assets again doesn't work.
				assert!(<$sender_para as [<$sender_para Pallet>]>::PolkadotXcm::claim_assets(
					origin.clone(),
					Box::new(versioned_assets.clone().into()),
					Box::new(beneficiary.clone().into()),
				).is_err());

				let balance = <<$sender_para as [<$sender_para Pallet>]>::Balances
					as $crate::macros::Currency<_>>::free_balance(&sender);
				assert_eq!(balance, balance_after);

				let holdings =
					<$xcm_config as xcm_executor::Config>::AssetTransactor::mint_asset(
						&$asset, &context,
					).unwrap();
				let total_issuance_before = <<$sender_para as [<$sender_para Pallet>]>::Balances
					as $crate::macros::Currency<_>>::total_issuance();
				// You can also claim assets and send them to a different account.
				<<$sender_para as [<$sender_para Pallet>]>::PolkadotXcm as $crate::macros::DropAssets>::drop_assets(
					&beneficiary, holdings, &context,
				);
				let receiver = [<$sender_para Receiver>]::get();
				let other_beneficiary: $crate::macros::Location =
					$crate::macros::Junction::AccountId32 { network: Some($network_id), id: receiver.clone().into() }.into();
				let balance_before = <<$sender_para as [<$sender_para Pallet>]>::Balances
					as $crate::macros::Currency<_>>::free_balance(&receiver);
				$crate::macros::assert_ok!(<$sender_para as [<$sender_para Pallet>]>::PolkadotXcm::claim_assets(
					origin.clone(),
					Box::new(versioned_assets.clone().into()),
					Box::new(other_beneficiary.clone().into()),
				));
				let balance_after = <<$sender_para as [<$sender_para Pallet>]>::Balances
					as $crate::macros::Currency<_>>::free_balance(&receiver);
				assert_eq!(balance_after, balance_before + $amount);
				// assert claiming trapped assets does not alter total issuance
				let total_issuance_after = <<$sender_para as [<$sender_para Pallet>]>::Balances
					as $crate::macros::Currency<_>>::total_issuance();
				assert_eq!(total_issuance_before, total_issuance_after);
			});
		}
	};
}

#[macro_export]
macro_rules! test_can_estimate_and_pay_exact_fees {
	( $sender_para:ty, $asset_hub:ty, $receiver_para:ty, ($asset_id:expr, $amount:expr), $owner_prefix:ty ) => {
		$crate::macros::paste::paste! {
			// We first define the call we'll use throughout the test.
			fn get_call(
				estimated_local_fees: impl Into<$crate::macros::Asset>,
				estimated_intermediate_fees: impl Into<$crate::macros::Asset>,
				estimated_remote_fees: impl Into<$crate::macros::Asset>,
			) -> <$sender_para as $crate::macros::Chain>::RuntimeCall {
				type RuntimeCall = <$sender_para as $crate::macros::Chain>::RuntimeCall;

				let beneficiary = [<$receiver_para Receiver>]::get();
				let xcm_in_destination = $crate::macros::Xcm::<()>::builder_unsafe()
					.pay_fees(estimated_remote_fees)
					.deposit_asset($crate::macros::AllCounted(1), beneficiary)
					.build();
				let ah_to_receiver = <$asset_hub as $crate::macros::Para>::sibling_location_of(
					<$receiver_para as $crate::macros::Para>::para_id());
				let xcm_in_reserve = $crate::macros::Xcm::<()>::builder_unsafe()
					.pay_fees(estimated_intermediate_fees)
					.deposit_reserve_asset(
						$crate::macros::AllCounted(1),
						ah_to_receiver,
						xcm_in_destination,
					)
					.build();
				let sender_to_ah = <$sender_para as $crate::macros::Para>::sibling_location_of(
					<$asset_hub as $crate::macros::Para>::para_id());
				let local_xcm = $crate::macros::Xcm::<<$sender_para as $crate::macros::Chain>::RuntimeCall>::builder()
					.withdraw_asset(($asset_id, $amount))
					.pay_fees(estimated_local_fees)
					.initiate_reserve_withdraw($crate::macros::AllCounted(1), sender_to_ah, xcm_in_reserve)
					.build();

				RuntimeCall::PolkadotXcm($crate::macros::pallet_xcm::Call::execute {
					message: Box::new($crate::macros::VersionedXcm::from(local_xcm)),
					max_weight: $crate::macros::Weight::from_parts(10_000_000_000, 500_000),
				})
			}

			let destination = <$sender_para as $crate::macros::Para>::sibling_location_of(
				<$receiver_para as $crate::macros::Para>::para_id());
			let sender = [<$sender_para Sender>]::get();
			let sender_as_seen_by_ah = <$asset_hub as $crate::macros::Para>::sibling_location_of(
				<$sender_para as $crate::macros::Para>::para_id());
			let sov_of_sender_on_ah = <$asset_hub as $crate::macros::Para>::sovereign_account_id_of(sender_as_seen_by_ah.clone());
			let asset_owner = [<$owner_prefix AssetOwner>]::get();

			// Fund parachain's sender account.
			// TODO: consider mint_foreign_asset to be part of xcm_emulator::Chain trait
			$sender_para::mint_foreign_asset(
				<$sender_para as $crate::macros::Chain>::RuntimeOrigin::signed(asset_owner.clone()),
				$asset_id.clone().into(),
				sender.clone(),
				$amount * 2,
			);

			// Fund the parachain origin's SA on Asset Hub with the native tokens.
			// TODO: consider fund_accounts to be part of xcm_emulator::Chain trait
			$asset_hub::fund_accounts(vec![(sov_of_sender_on_ah.clone(), $amount * 2)]);

			let beneficiary_id = [<$receiver_para Receiver>]::get();

			let test_args = $crate::macros::TestContext {
				sender: sender.clone(),
				receiver: beneficiary_id.clone(),
				args: $crate::macros::TestArgs::new_para(
					destination,
					beneficiary_id.clone(),
					$amount,
					($asset_id, $amount).into(),
					None,
					0,
				),
			};
			let mut test = ParaToParaThroughAHTest::new(test_args);

			// We get these from the closure.
			let mut local_execution_fees = 0;
			let mut local_delivery_fees = 0;
			let mut remote_message = $crate::macros::VersionedXcm::from($crate::macros::Xcm::<()>(Vec::new()));
			<$sender_para as $crate::macros::TestExt>::execute_with(|| {
				type Runtime = <$sender_para as $crate::macros::Chain>::Runtime;
				type OriginCaller = <$sender_para as $crate::macros::Chain>::OriginCaller;

				let call = get_call(
					($crate::macros::Parent, 100_000_000_000u128),
					($crate::macros::Parent, 100_000_000_000u128),
					($crate::macros::Parent, 100_000_000_000u128),
				);
				let origin = OriginCaller::system($crate::macros::RawOrigin::Signed(sender.clone()));
				let result = <Runtime as $crate::macros::DryRunApiV2<_,_,_,_>>::dry_run_call(origin, call.clone(),
					$crate::macros::XCM_VERSION).unwrap();
				let local_xcm = result.local_xcm.unwrap().clone();
				let local_xcm_weight = <Runtime as $crate::macros::XcmPaymentApiV2<_>>::query_xcm_weight(local_xcm).unwrap();
				local_execution_fees = <Runtime as $crate::macros::XcmPaymentApiV2<_>>::query_weight_to_asset_fee(
					local_xcm_weight,
					$crate::macros::VersionedAssetId::from($crate::macros::AssetId($crate::macros::Location::parent())),
				)
				.unwrap();
				// We filter the result to get only the messages we are interested in.
				let (destination_to_query, messages_to_query) = &result
					.forwarded_xcms
					.iter()
					.find(|(destination, _)| {
						*destination == $crate::macros::VersionedLocation::from(
							$crate::macros::Location::new(1, [$crate::macros::Parachain(1000)]))
					})
					.unwrap();
				assert_eq!(messages_to_query.len(), 1);
				remote_message = messages_to_query[0].clone();
				let del_fee_pen = {
					let fees_in_pen =
						<Runtime as $crate::macros::XcmPaymentApiV2<_>>::query_delivery_fees(
							destination_to_query.clone(),
							remote_message.clone(),
							$crate::macros::VersionedAssetId::from($crate::macros::AssetId(
								$crate::macros::Location::here(),
							)),
						).unwrap();
					$crate::xcm_helpers::get_amount_from_versioned_assets(fees_in_pen)
				};
				let exec_fee_pen =
					<Runtime as $crate::macros::XcmPaymentApiV2<_>>::query_weight_to_asset_fee(
						local_xcm_weight,
						$crate::macros::VersionedAssetId::from($crate::macros::AssetId(
							$crate::macros::Location::here(),
						)),
					).unwrap();
				local_delivery_fees = $crate::macros::with_transaction_unchecked(|| {
					$crate::macros::assert_ok!(
						$crate::macros::pallet_asset_conversion::Pallet::<Runtime>::swap_tokens_for_exact_tokens(
							<$sender_para as $crate::macros::Chain>::RuntimeOrigin::from(
								$crate::macros::RawOrigin::Signed(sender.clone()),
							),
							vec![
								Box::new($crate::macros::Location::parent()),
								Box::new($crate::macros::Location::here()),
							],
							exec_fee_pen,
							local_execution_fees,
							sender.clone(),
							false,
						)
					);
					let needed_wnd =
						$crate::macros::pallet_asset_conversion::Pallet::<Runtime>::quote_price_tokens_for_exact_tokens(
							$crate::macros::Location::parent(),
							$crate::macros::Location::here(),
							del_fee_pen,
							true,
						).expect("pool exists and del_fee_pen is nonzero; qed");
					$crate::macros::TransactionOutcome::Rollback(needed_wnd)
				});
			});

			// These are set in the AssetHub closure.
			let mut intermediate_execution_fees = 0;
			let mut intermediate_delivery_fees = 0;
			let mut intermediate_remote_message = $crate::macros::VersionedXcm::from($crate::macros::Xcm::<()>(Vec::new()));
			<$asset_hub as $crate::macros::TestExt>::execute_with(|| {
				type Runtime = <$asset_hub as $crate::macros::Chain>::Runtime;
				type RuntimeCall = <$asset_hub as $crate::macros::Chain>::RuntimeCall;

				// First we get the execution fees.
				let weight = <Runtime as $crate::macros::XcmPaymentApiV2<_>>::query_xcm_weight(remote_message.clone()).unwrap();
				intermediate_execution_fees = <Runtime as $crate::macros::XcmPaymentApiV2<_>>::query_weight_to_asset_fee(
					weight,
					$crate::macros::VersionedAssetId::from($crate::macros::AssetId($crate::macros::Location::new(1, []))),
				)
				.unwrap();

				// We have to do this to turn `VersionedXcm<()>` into `VersionedXcm<RuntimeCall>`.
				let xcm_program =
					$crate::macros::VersionedXcm::from($crate::macros::Xcm::<RuntimeCall>::from(
						remote_message.clone().try_into().unwrap()));

				// Now we get the delivery fees to the final destination.
				let result = <Runtime as $crate::macros::DryRunApiV2<_,_,_,_>>::dry_run_xcm(
					sender_as_seen_by_ah.clone().into(), xcm_program).unwrap();
				let (destination_to_query, messages_to_query) = &result
					.forwarded_xcms
					.iter()
					.find(|(destination, _)| {
						*destination == $crate::macros::VersionedLocation::from($crate::macros::Location::new(1,
							[$crate::macros::Parachain(2001)]))
					})
					.unwrap();
				// There's actually two messages here.
				// One created when the message we sent from `$sender_para` arrived and was executed.
				// The second one when we dry-run the xcm.
				// We could've gotten the message from the queue without having to dry-run, but
				// offchain applications would have to dry-run, so we do it here as well.
				intermediate_remote_message = messages_to_query[0].clone();
				let asset_id_for_delivery_fees = VersionedAssetId::from(Location::parent());
				let delivery_fees = <Runtime as $crate::macros::XcmPaymentApiV2<_>>::query_delivery_fees(
					destination_to_query.clone(),
					intermediate_remote_message.clone(),
					asset_id_for_delivery_fees,
				)
				.unwrap();
				intermediate_delivery_fees = $crate::xcm_helpers::get_amount_from_versioned_assets(delivery_fees);
			});

			// Dry-running is done.
			<$sender_para as $crate::macros::TestExt>::reset_ext();
			<$asset_hub as $crate::macros::TestExt>::reset_ext();
			<$receiver_para as $crate::macros::TestExt>::reset_ext();

			// Fund accounts again.
			$sender_para::mint_foreign_asset(
				<$sender_para as $crate::macros::Chain>::RuntimeOrigin::signed(asset_owner.clone()),
				$asset_id.clone().into(),
				sender.clone(),
				$amount * 2,
			);
			$sender_para::fund_accounts(vec![(sender.clone(), $amount * 2)]);

			$asset_hub::fund_accounts(vec![(sov_of_sender_on_ah, $amount * 2)]);

			// Get the final execution fees at the destination.
			//
			// Note: We need to do this after resetting the externalities to get an accurate value here.
			// Workaround for https://github.com/paritytech/polkadot-sdk/issues/11486.
			let mut final_execution_fees = 0;
			<$receiver_para as $crate::macros::TestExt>::execute_with(|| {
				type Runtime = <$receiver_para as $crate::macros::Chain>::Runtime;

				let weight = <Runtime as $crate::macros::XcmPaymentApiV2<_>>::query_xcm_weight(
					intermediate_remote_message.clone()).expect("`query_xcm_weight` returned none");
				final_execution_fees =
					<Runtime as $crate::macros::XcmPaymentApiV2<_>>::query_weight_to_asset_fee(weight,
						$crate::macros::VersionedAssetId::from($crate::macros::AssetId($crate::macros::Location::parent())))
						.expect("`query_weight_to_asset_fee` returned none");
			});

			// Actually run the extrinsic.
			let sender_assets_before = <$sender_para as $crate::macros::TestExt>::execute_with(|| {
				type Assets = <$sender_para as [<$sender_para Pallet>]>::Assets;
				<Assets as $crate::macros::Inspect<_>>::balance($asset_id.clone().into(), &sender)
			});
			let receiver_assets_before = <$receiver_para as $crate::macros::TestExt>::execute_with(|| {
				type Assets = <$receiver_para as [<$receiver_para Pallet>]>::Assets;
				<Assets as $crate::macros::Inspect<_>>::balance($asset_id.clone().into(), &beneficiary_id)
			});

			test.set_assertion::<$sender_para>(sender_assertions);
			test.set_assertion::<$asset_hub>(hop_assertions);
			test.set_assertion::<$receiver_para>(receiver_assertions);
			let call = get_call(
				($crate::macros::Parent, local_execution_fees + local_delivery_fees),
				($crate::macros::Parent, intermediate_execution_fees + intermediate_delivery_fees),
				($crate::macros::Parent, final_execution_fees),
			);
			test.set_call(call);
			test.assert();

			let sender_assets_after = <$sender_para as $crate::macros::TestExt>::execute_with(|| {
				type Assets = <$sender_para as [<$sender_para Pallet>]>::Assets;
				<Assets as $crate::macros::Inspect<_>>::balance($asset_id.clone().into(), &sender)
			});
			let receiver_assets_after = <$receiver_para as $crate::macros::TestExt>::execute_with(|| {
				type Assets = <$receiver_para as [<$receiver_para Pallet>]>::Assets;
				<Assets as $crate::macros::Inspect<_>>::balance($asset_id.into(), &beneficiary_id)
			});

			// We know the exact fees on every hop.
			assert_eq!(sender_assets_after, sender_assets_before - $amount);
			assert_eq!(
				receiver_assets_after,
				receiver_assets_before + $amount -
					local_execution_fees -
					local_delivery_fees -
					intermediate_execution_fees -
					intermediate_delivery_fees -
					final_execution_fees
			);
		}
	};
}

#[macro_export]
macro_rules! test_dry_run_transfer_across_pk_bridge {
	( $sender_asset_hub:ty, $sender_bridge_hub:ty, $destination:expr ) => {
		$crate::macros::paste::paste! {

			let who = $crate::macros::AccountId32::new([1u8; 32]);
			let transfer_amount = 10_000_000_000_000u128;
			let initial_balance = transfer_amount * 10;

			// AssetHub setup.
			$sender_asset_hub::force_xcm_version($destination, $crate::macros::XCM_VERSION);

			<$sender_asset_hub as $crate::macros::TestExt>::execute_with(|| {
				type Runtime = <$sender_asset_hub as $crate::macros::Chain>::Runtime;
				type RuntimeCall = <$sender_asset_hub as $crate::macros::Chain>::RuntimeCall;
				type OriginCaller = <$sender_asset_hub as $crate::macros::Chain>::OriginCaller;
				type Balances = <$sender_asset_hub as [<$sender_asset_hub Pallet>]>::Balances;

				// Give some initial funds.
				<Balances as $crate::macros::Mutate<_>>::set_balance(&who, initial_balance);

				let beneficiary: $crate::macros::Location = $crate::macros::Junction::AccountId32 {
						id: who.clone().into(),
						network: None,
					}.into();

				let call = RuntimeCall::PolkadotXcm($crate::macros::pallet_xcm::Call::transfer_assets_using_type_and_then {
					dest: Box::new($crate::macros::VersionedLocation::from($destination)),
					assets: Box::new($crate::macros::VersionedAssets::from(vec![
						($crate::macros::Parent, transfer_amount).into(),
					])),
					assets_transfer_type: Box::new($crate::macros::TransferType::LocalReserve),
					remote_fees_id: Box::new($crate::macros::VersionedAssetId::from($crate::macros::Parent)),
					fees_transfer_type: Box::new($crate::macros::TransferType::LocalReserve),
					custom_xcm_on_dest: Box::new($crate::macros::VersionedXcm::<()>::from($crate::macros::Xcm::<()>::builder_unsafe().deposit_asset(AllCounted(1), beneficiary).build())),
					weight_limit: $crate::macros::Unlimited,
				});
				let origin = OriginCaller::system($crate::macros::RawOrigin::Signed(who));
				let result = <Runtime as $crate::macros::DryRunApiV2<_,_,_,_>>::dry_run_call(origin, call.clone(),
					$crate::macros::XCM_VERSION).unwrap();

				// We assert the dry run succeeds and sends only one message to the local bridge hub.
				assert!(result.execution_result.is_ok());
				assert_eq!(result.forwarded_xcms.len(), 1);
				assert_eq!(result.forwarded_xcms[0].0, $crate::macros::VersionedLocation::from(
					$crate::macros::Location::new(1, [$crate::macros::Parachain(
						<$sender_bridge_hub as $crate::macros::Para>::para_id().into())])));
			});
		}
	};
}

#[macro_export]
macro_rules! test_xcm_fee_querying_apis_work_for_asset_hub {
	( $asset_hub:ty ) => {
		$crate::macros::paste::paste! {

			<$asset_hub as $crate::macros::TestExt>::execute_with(|| {
				// Setup a pool between USDT and WND.
				type RuntimeOrigin = <$asset_hub as $crate::macros::Chain>::RuntimeOrigin;
				type Assets = <$asset_hub as [<$asset_hub Pallet>]>::Assets;
				type AssetConversion = <$asset_hub as [<$asset_hub Pallet>]>::AssetConversion;
				let wnd = $crate::macros::Location::new(1, []);
				let usdt = $crate::macros::Location::new(0, [$crate::macros::PalletInstance($crate::macros::ASSETS_PALLET_ID),
					$crate::macros::GeneralIndex($crate::macros::USDT_ID.into())]);
				let sender = [<$asset_hub Sender>]::get();
				$crate::macros::assert_ok!(AssetConversion::create_pool(
					RuntimeOrigin::signed(sender.clone()),
					Box::new(wnd.clone()),
					Box::new(usdt.clone()),
				));

				type Runtime = <$asset_hub as $crate::macros::Chain>::Runtime;
				let acceptable_payment_assets = <Runtime as $crate::macros::XcmPaymentApiV2<_>>::query_acceptable_payment_assets(
					$crate::macros::XCM_VERSION).unwrap();
				assert_eq!(acceptable_payment_assets, vec![
					$crate::macros::VersionedAssetId::from($crate::macros::AssetId(wnd.clone())),
					$crate::macros::VersionedAssetId::from($crate::macros::AssetId(usdt.clone())),
				]);

				let program = $crate::macros::Xcm::<()>::builder()
					.withdraw_asset(($crate::macros::Parent, 100u128))
					.buy_execution(($crate::macros::Parent, 10u128), $crate::macros::Unlimited)
					.deposit_asset($crate::macros::All, [0u8; 32])
					.build();
				let weight = <Runtime as $crate::macros::XcmPaymentApiV2<_>>::query_xcm_weight(
					$crate::macros::VersionedXcm::from(program)).unwrap();
				let fee_in_wnd = <Runtime as $crate::macros::XcmPaymentApiV2<_>>::query_weight_to_asset_fee(weight,
					$crate::macros::VersionedAssetId::from($crate::macros::AssetId(wnd.clone()))).unwrap();
				// Assets not in a pool don't work.
				assert!(<Runtime as $crate::macros::XcmPaymentApiV2<_>>::query_weight_to_asset_fee(weight,
					$crate::macros::VersionedAssetId::from(
						$crate::macros::AssetId($crate::macros::Location::new(0,
							[$crate::macros::PalletInstance($crate::macros::ASSETS_PALLET_ID),
								$crate::macros::GeneralIndex(1)]
							)
						)
					)
				).is_err());
				let fee_in_usdt_fail = <Runtime as $crate::macros::XcmPaymentApiV2<_>>::query_weight_to_asset_fee(weight,
					$crate::macros::VersionedAssetId::from($crate::macros::AssetId(usdt.clone())));
				// Weight to asset fee fails because there's not enough asset in the pool.
				// We just created it, there's none.
				assert_eq!(fee_in_usdt_fail, Err($crate::macros::XcmPaymentApiError::AssetNotFound));
				// We add some.
				$crate::macros::assert_ok!(Assets::mint(
					RuntimeOrigin::signed(sender.clone()),
					$crate::macros::USDT_ID.into(),
					sender.clone().into(),
					5_000_000_000_000
				));
				// We make 1 WND = 4 USDT.
				$crate::macros::assert_ok!(AssetConversion::add_liquidity(
					RuntimeOrigin::signed(sender.clone()),
					Box::new(wnd),
					Box::new(usdt.clone()),
					1_000_000_000_000,
					4_000_000_000_000,
					0,
					0,
					sender.into()
				));
				// Now it works.
				let fee_in_usdt = <Runtime as $crate::macros::XcmPaymentApiV2<_>>::query_weight_to_asset_fee(weight,
					$crate::macros::VersionedAssetId::from($crate::macros::AssetId(usdt))
				);
				$crate::macros::assert_ok!(fee_in_usdt);
				assert!(fee_in_usdt.unwrap() > fee_in_wnd);
			});
		}
	};
}

#[macro_export]
macro_rules! test_cross_chain_alias {
	( vec![$( ($sender_para:ty, $receiver_para:ty, $is_teleport:expr, $expected_success:expr) ),+], $origin:expr, $target:expr, $fees:expr ) => {
		$crate::macros::paste::paste! {
			$(
				{
					let para_destination = <$sender_para as $crate::macros::Para>::sibling_location_of(
						<$receiver_para as $crate::macros::Para>::para_id());
					let account: $crate::macros::AccountId = $origin.clone().into();
					$sender_para::fund_accounts(vec![(account.clone(), $fees * 10)]);
					let total_fees: $crate::macros::Asset = ($crate::macros::Location::parent(), $fees).into();
					let fees: $crate::macros::Asset = ($crate::macros::Location::parent(), $fees / 2).into();

					let remote_fees = if $is_teleport {
						Some($crate::macros::AssetTransferFilter::Teleport(fees.clone().into()))
					} else {
						let source_para_sa = <$receiver_para as $crate::macros::Para>::sovereign_account_id_of(
							<$receiver_para as $crate::macros::Para>::sibling_location_of(
								<$sender_para as $crate::macros::Para>::para_id()),
						);
						$receiver_para::fund_accounts(vec![(source_para_sa, $fees * 10)]);
						Some($crate::macros::AssetTransferFilter::ReserveWithdraw(fees.clone().into()))
					};
					<$sender_para as $crate::macros::TestExt>::execute_with(|| {
						type RuntimeEvent = <$sender_para as $crate::macros::Chain>::RuntimeEvent;
						let xcm_message = $crate::macros::Xcm::<()>(vec![
							$crate::macros::WithdrawAsset(total_fees.into()),
							$crate::macros::PayFees { asset: fees.clone() },
							$crate::macros::InitiateTransfer {
								destination: para_destination,
								remote_fees,
								preserve_origin: true,
								assets: $crate::macros::BoundedVec::new(),
								remote_xcm: $crate::macros::Xcm(vec![
									// try to alias into `account`
									$crate::macros::AliasOrigin($target.clone().into()),
									$crate::macros::RefundSurplus,
									$crate::macros::DepositAsset {
										assets: $crate::macros::Wild($crate::macros::AllCounted(1)),
										beneficiary: $target.clone().into(),
									},
								]),
							},
							$crate::macros::RefundSurplus,
							$crate::macros::DepositAsset { assets: $crate::macros::Wild($crate::macros::AllCounted(1)),
								beneficiary: account.clone().into() },
						]);

						let signed_origin = <$sender_para as $crate::macros::Chain>::RuntimeOrigin::signed(account.into());
						$crate::macros::assert_ok!(<$sender_para as [<$sender_para Pallet>]>::PolkadotXcm::execute(
							signed_origin,
							Box::new($crate::macros::VersionedXcm::from(xcm_message.into())),
							$crate::macros::Weight::MAX
						));
						$crate::macros::assert_expected_events!(
							$sender_para,
							vec![
								RuntimeEvent::PolkadotXcm($crate::macros::pallet_xcm::Event::Sent { .. }) => {},
							]
						);
					});

					<$receiver_para as $crate::macros::TestExt>::execute_with(|| {
						type RuntimeEvent = <$receiver_para as $crate::macros::Chain>::RuntimeEvent;
						$crate::macros::assert_expected_events!(
							$receiver_para,
							vec![
								RuntimeEvent::MessageQueue($crate::macros::pallet_message_queue::Event::Processed {
									success, ..
								}) => { success: *success == $expected_success, },
							]
						);
					});
				}
			)+
		}
	};
}

#[macro_export]
macro_rules! assert_whitelisted {
    ($chain:ident, $expected_call_hash:expr) => {
		type RuntimeEvent = <$chain as $crate::macros::Chain>::RuntimeEvent;
		$crate::macros::assert_expected_events!(
			$chain,
			vec![
				RuntimeEvent::Whitelist($crate::macros::pallet_whitelist::Event::CallWhitelisted { call_hash }) => {
						call_hash: *call_hash == $expected_call_hash,
				},
			]
		);
    };
}

/// Read the balance of the `Assets`-pallet asset `$id` for `$who` on `$chain`.
#[macro_export]
macro_rules! assets_balance_on {
	( $chain:ident, $id:expr, $who:expr ) => {
		$crate::macros::paste::paste! {
			<$chain as $crate::macros::TestExt>::ext_wrapper(|| {
				type Assets = <$chain as [<$chain Pallet>]>::Assets;
				<Assets as frame_support::traits::fungibles::Inspect<_>>::balance($id, $who)
			})
		}
	};
}

/// Read the balance of the `ForeignAssets`-pallet asset `$id` for `$who` on `$chain`.
#[macro_export]
macro_rules! foreign_balance_on {
	( $chain:ident, $id:expr, $who:expr ) => {
		$crate::macros::paste::paste! {
			<$chain as $crate::macros::TestExt>::ext_wrapper(|| {
				type ForeignAssets = <$chain as [<$chain Pallet>]>::ForeignAssets;
				<ForeignAssets as frame_support::traits::fungibles::Inspect<_>>::balance($id, $who)
			})
		}
	};
}

/// Read the total issuance of the `Assets`-pallet asset `$id` on `$chain`.
#[macro_export]
macro_rules! assets_issuance_on {
	( $chain:ident, $id:expr ) => {
		$crate::macros::paste::paste! {
			<$chain as $crate::macros::TestExt>::ext_wrapper(|| {
				type Assets = <$chain as [<$chain Pallet>]>::Assets;
				<Assets as frame_support::traits::fungibles::Inspect<_>>::total_issuance($id)
			})
		}
	};
}

/// Read the total issuance of the `ForeignAssets`-pallet asset `$id` on `$chain`.
#[macro_export]
macro_rules! foreign_issuance_on {
	( $chain:ident, $id:expr ) => {
		$crate::macros::paste::paste! {
			<$chain as $crate::macros::TestExt>::ext_wrapper(|| {
				type ForeignAssets = <$chain as [<$chain Pallet>]>::ForeignAssets;
				<ForeignAssets as frame_support::traits::fungibles::Inspect<_>>::total_issuance($id)
			})
		}
	};
}

/// Read the total native-balance issuance on `$chain`.
#[macro_export]
macro_rules! balances_issuance_on {
	( $chain:ident ) => {
		$crate::macros::paste::paste! {
			<$chain as $crate::macros::TestExt>::ext_wrapper(|| {
				type Balances = <$chain as [<$chain Pallet>]>::Balances;
				<Balances as frame_support::traits::fungible::Inspect<_>>::total_issuance()
			})
		}
	};
}

/// Whether the `Assets`-pallet asset `$id` exists on `$chain`.
#[macro_export]
macro_rules! asset_exists_on {
	( $chain:ident, $id:expr ) => {
		$crate::macros::paste::paste! {
			<$chain as $crate::macros::TestExt>::ext_wrapper(|| {
				type Assets = <$chain as [<$chain Pallet>]>::Assets;
				<Assets as frame_support::traits::fungibles::Inspect<_>>::asset_exists($id)
			})
		}
	};
}
