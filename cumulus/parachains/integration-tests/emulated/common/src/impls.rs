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

pub use codec::{Decode, Encode};
pub use paste;

pub use crate::{
	xcm_helpers::{xcm_transact_paid_execution, xcm_transact_unpaid_execution},
	PROOF_SIZE_THRESHOLD, REF_TIME_THRESHOLD,
};

// Substrate
pub use frame_support::{
	assert_ok,
	sp_runtime::AccountId32,
	traits::fungibles::Inspect,
	weights::{Weight, WeightMeter},
};
pub use pallet_assets;
pub use pallet_message_queue;
pub use pallet_xcm;
pub use xcm;

// Polkadot
pub use polkadot_runtime_parachains::{
	dmp, hrmp,
	inclusion::{AggregateMessageOrigin, UmpQueueId},
};
pub use xcm::{
	prelude::{
		Asset, InteriorLocation, Location, OriginKind, Outcome, VersionedXcm, XcmError, XcmVersion,
	},
	DoubleEncoded,
};

// Cumulus
pub use cumulus_pallet_parachain_system;
pub use cumulus_pallet_xcmp_queue;
pub use cumulus_primitives_core::{
	relay_chain::HrmpChannelId, DmpMessageHandler, Junction, Junctions, NetworkId, ParaId,
	XcmpMessageHandler,
};
pub use parachains_common::{AccountId, Balance};
pub use xcm_emulator::{
	assert_expected_events, bx, helpers::weight_within_threshold, BridgeLaneId, BridgeMessage,
	BridgeMessageDispatchError, BridgeMessageHandler, Chain, Network, Parachain, RelayChain,
	TestExt,
};

// Bridges
use bp_messages::{
	target_chain::{DispatchMessage, DispatchMessageData, MessageDispatch},
	MessageKey, OutboundLaneData,
};
pub use bp_xcm_bridge_hub::XcmBridgeHubCall;
use pallet_bridge_messages::{Config as BridgeMessagesConfig, LaneIdOf, OutboundLanes, Pallet};
pub use pallet_bridge_messages::{
	Instance1 as BridgeMessagesInstance1, Instance2 as BridgeMessagesInstance2,
	Instance3 as BridgeMessagesInstance3,
};
use pallet_xcm_bridge_hub::XcmBlobMessageDispatchResult;

pub struct BridgeHubMessageHandler<S, SI, T, TI> {
	_marker: std::marker::PhantomData<(S, SI, T, TI)>,
}

struct LaneIdWrapper<LaneId>(LaneId);
impl<LaneId: Encode> From<LaneIdWrapper<LaneId>> for BridgeLaneId {
	fn from(lane_id: LaneIdWrapper<LaneId>) -> BridgeLaneId {
		lane_id.0.encode()
	}
}
impl<LaneId: Decode> From<BridgeLaneId> for LaneIdWrapper<LaneId> {
	fn from(id: BridgeLaneId) -> LaneIdWrapper<LaneId> {
		LaneIdWrapper(LaneId::decode(&mut &id[..]).expect("decodable"))
	}
}

impl<S, SI, T, TI> BridgeMessageHandler for BridgeHubMessageHandler<S, SI, T, TI>
where
	S: BridgeMessagesConfig<SI>,
	SI: 'static,
	T: BridgeMessagesConfig<TI>,
	TI: 'static,
	<T as BridgeMessagesConfig<TI>>::InboundPayload: From<Vec<u8>>,
	<T as BridgeMessagesConfig<TI>>::MessageDispatch:
		MessageDispatch<DispatchLevelResult = XcmBlobMessageDispatchResult>,
{
	fn get_source_outbound_messages() -> Vec<BridgeMessage> {
		// get the source active outbound lanes
		let active_outbound_lanes = OutboundLanes::<S, SI>::iter_keys();

		let mut messages: Vec<BridgeMessage> = Default::default();

		// collect messages from `OutboundMessages` for each active outbound lane in the source
		for lane in active_outbound_lanes {
			let latest_generated_nonce =
				OutboundLanes::<S, SI>::get(lane).unwrap().latest_generated_nonce;
			let latest_received_nonce =
				OutboundLanes::<S, SI>::get(lane).unwrap().latest_received_nonce;

			(latest_received_nonce + 1..=latest_generated_nonce).for_each(|nonce| {
				let encoded_payload: Vec<u8> = Pallet::<S, SI>::outbound_message_data(lane, nonce)
					.expect("Bridge message does not exist")
					.into();
				let payload = Vec::<u8>::decode(&mut &encoded_payload[..])
					.expect("Decoding XCM message failed");
				let message = BridgeMessage { lane_id: LaneIdWrapper(lane).into(), nonce, payload };

				messages.push(message);
			});
		}
		messages
	}

	fn dispatch_target_inbound_message(
		message: BridgeMessage,
	) -> Result<(), BridgeMessageDispatchError> {
		type TargetMessageDispatch<T, I> = <T as BridgeMessagesConfig<I>>::MessageDispatch;
		type InboundPayload<T, I> = <T as BridgeMessagesConfig<I>>::InboundPayload;

		let lane_id = LaneIdWrapper::from(message.lane_id).0;
		let nonce = message.nonce;
		let payload = Ok(From::from(message.payload));

		// Directly dispatch outbound messages assuming everything is correct
		// and bypassing the `Relayers`  and `InboundLane` logic
		let dispatch_result = TargetMessageDispatch::<T, TI>::dispatch(DispatchMessage {
			key: MessageKey { lane_id, nonce },
			data: DispatchMessageData::<InboundPayload<T, TI>> { payload },
		});

		let result = match dispatch_result.dispatch_level_result {
			XcmBlobMessageDispatchResult::Dispatched => Ok(()),
			XcmBlobMessageDispatchResult::InvalidPayload => Err(BridgeMessageDispatchError(
				Box::new(XcmBlobMessageDispatchResult::InvalidPayload),
			)),
			XcmBlobMessageDispatchResult::NotDispatched(e) => Err(BridgeMessageDispatchError(
				Box::new(XcmBlobMessageDispatchResult::NotDispatched(e)),
			)),
		};
		result
	}

	fn notify_source_message_delivery(lane_id: BridgeLaneId) {
		let lane_id: LaneIdOf<S, SI> = LaneIdWrapper::from(lane_id).0;
		let data = OutboundLanes::<S, SI>::get(lane_id).unwrap();
		let new_data = OutboundLaneData {
			oldest_unpruned_nonce: data.oldest_unpruned_nonce + 1,
			latest_received_nonce: data.latest_received_nonce + 1,
			..data
		};

		OutboundLanes::<S, SI>::insert(lane_id, new_data);
	}
}

#[macro_export]
macro_rules! impl_accounts_helpers_for_relay_chain {
	( $chain:ident ) => {
		$crate::impls::paste::paste! {
			impl<N: $crate::impls::Network> $chain<N> {
				/// Fund a set of accounts with a balance
				pub fn fund_accounts(accounts: Vec<($crate::impls::AccountId, $crate::impls::Balance)>) {
					<Self as $crate::impls::TestExt>::execute_with(|| {
						for account in accounts {
							let who = account.0;
							let actual = <Self as [<$chain RelayPallet>]>::Balances::free_balance(&who);
							let actual = actual.saturating_add(<Self as [<$chain RelayPallet>]>::Balances::reserved_balance(&who));

							$crate::impls::assert_ok!(<Self as [<$chain RelayPallet>]>::Balances::force_set_balance(
								<Self as $crate::impls::Chain>::RuntimeOrigin::root(),
								who.into(),
								actual.saturating_add(account.1),
							));
						}
					});
				}
				/// Fund a sovereign account based on its Parachain Id
				pub fn fund_para_sovereign(amount: $crate::impls::Balance, para_id: $crate::impls::ParaId) -> $crate::impls::AccountId32 {
					let sovereign_account = <Self as $crate::impls::RelayChain>::sovereign_account_id_of_child_para(para_id);
					Self::fund_accounts(vec![(sovereign_account.clone(), amount)]);
					sovereign_account
				}
			}
		}
	};
}

#[macro_export]
macro_rules! impl_assert_events_helpers_for_relay_chain {
	( $chain:ident ) => {
		$crate::impls::paste::paste! {
			type [<$chain RuntimeEvent>]<N> = <$chain<N> as $crate::impls::Chain>::RuntimeEvent;

			impl<N: $crate::impls::Network> $chain<N> {
				/// Asserts a dispatchable is completely executed and XCM sent
				pub fn assert_xcm_pallet_attempted_complete(expected_weight: Option<$crate::impls::Weight>) {
					$crate::impls::assert_expected_events!(
						Self,
						vec![
							[<$chain RuntimeEvent>]::<N>::XcmPallet(
								$crate::impls::pallet_xcm::Event::Attempted { outcome: $crate::impls::Outcome::Complete { used: weight } }
							) => {
								weight: $crate::impls::weight_within_threshold(
									($crate::impls::REF_TIME_THRESHOLD, $crate::impls::PROOF_SIZE_THRESHOLD),
									expected_weight.unwrap_or(*weight),
									*weight
								),
							},
						]
					);
				}

				/// Asserts a dispatchable is incompletely executed and XCM sent
				pub fn assert_xcm_pallet_attempted_incomplete(
					expected_weight: Option<$crate::impls::Weight>,
					expected_error: Option<$crate::impls::XcmError>,
				) {
					$crate::impls::assert_expected_events!(
						Self,
						vec![
							// Dispatchable is properly executed and XCM message sent
							[<$chain RuntimeEvent>]::<N>::XcmPallet(
								$crate::impls::pallet_xcm::Event::Attempted { outcome: $crate::impls::Outcome::Incomplete { used: weight, error: $crate::impls::xcm::prelude::InstructionError { error, .. } } }
							) => {
								weight: $crate::impls::weight_within_threshold(
									($crate::impls::REF_TIME_THRESHOLD, $crate::impls::PROOF_SIZE_THRESHOLD),
									expected_weight.unwrap_or(*weight),
									*weight
								),
								error: *error == expected_error.unwrap_or((*error).into()).into(),
							},
						]
					);
				}

				/// Asserts an XCM program is sent.
				pub fn assert_xcm_pallet_sent() {
					$crate::impls::assert_expected_events!(
						Self,
						vec![
							[<$chain RuntimeEvent>]::<N>::XcmPallet($crate::impls::pallet_xcm::Event::Sent { .. }) => {},
						]
					);
				}

				/// Asserts an XCM program from a System Parachain is successfully received and
				/// processed within expectations.
				pub fn assert_ump_queue_processed(
					expected_success: bool,
					expected_id: Option<$crate::impls::ParaId>,
					expected_weight: Option<$crate::impls::Weight>,
				) {
					$crate::impls::assert_expected_events!(
						Self,
						vec![
							// XCM is successfully received and processed
							[<$chain RuntimeEvent>]::<N>::MessageQueue($crate::impls::pallet_message_queue::Event::Processed {
								origin: $crate::impls::AggregateMessageOrigin::Ump($crate::impls::UmpQueueId::Para(id)),
								weight_used,
								success,
								..
							}) => {
								id: *id == expected_id.unwrap_or(*id),
								weight_used: $crate::impls::weight_within_threshold(
									($crate::impls::REF_TIME_THRESHOLD, $crate::impls::PROOF_SIZE_THRESHOLD),
									expected_weight.unwrap_or(*weight_used),
									*weight_used
								),
								success: *success == expected_success,
							},
						]
					);
				}
			}
		}
	};
}

#[macro_export]
macro_rules! impl_hrmp_channels_helpers_for_relay_chain {
	( $chain:ident ) => {
		$crate::impls::paste::paste! {
			impl<N: $crate::impls::Network> $chain<N> {
				/// Init open channel request with another Parachain
				pub fn init_open_channel_call(
					recipient_para_id: $crate::impls::ParaId,
					max_capacity: u32,
					max_message_size: u32,
				) -> $crate::impls::DoubleEncoded<()> {
					use $crate::impls::Encode;

					<Self as $crate::impls::Chain>::RuntimeCall::Hrmp($crate::impls::hrmp::Call::<
						<Self as $crate::impls::Chain>::Runtime,
					>::hrmp_init_open_channel {
						recipient: recipient_para_id,
						proposed_max_capacity: max_capacity,
						proposed_max_message_size: max_message_size,
					})
					.encode()
					.into()
				}
				/// Recipient Parachain accept the open request from another Parachain
				pub fn accept_open_channel_call(sender_para_id: $crate::impls::ParaId) -> $crate::impls::DoubleEncoded<()> {
					use $crate::impls::Encode;

					<Self as $crate::impls::Chain>::RuntimeCall::Hrmp($crate::impls::hrmp::Call::<
						<Self as $crate::impls::Chain>::Runtime,
					>::hrmp_accept_open_channel {
						sender: sender_para_id,
					})
					.encode()
					.into()
				}

				/// A root origin force to open a channel between two Parachains
				pub fn force_process_hrmp_open(sender: $crate::impls::ParaId, recipient: $crate::impls::ParaId) {
					use $crate::impls::Chain;

					<Self as $crate::impls::TestExt>::execute_with(|| {
						let relay_root_origin = <Self as Chain>::RuntimeOrigin::root();

						// Force process HRMP open channel requests without waiting for the next session
						$crate::impls::assert_ok!(<Self as [<$chain RelayPallet>]>::Hrmp::force_process_hrmp_open(
							relay_root_origin,
							0
						));

						let channel_id = $crate::impls::HrmpChannelId { sender, recipient };

						let hrmp_channel_exist = $crate::impls::hrmp::HrmpChannels::<
							<Self as Chain>::Runtime,
						>::contains_key(&channel_id);

						// Check the HRMP channel has been successfully registered
						assert!(hrmp_channel_exist)
					});
				}
			}
		}
	};
}

#[macro_export]
macro_rules! impl_send_transact_helpers_for_relay_chain {
	( $chain:ident ) => {
		$crate::impls::paste::paste! {
			impl<N: $crate::impls::Network> $chain<N> {
				/// A root origin (as governance) sends `xcm::Transact` with `UnpaidExecution` and encoded `call` to child parachain.
				pub fn send_unpaid_transact_to_parachain_as_root(
					recipient: $crate::impls::ParaId,
					call: $crate::impls::DoubleEncoded<()>
				) {
					use $crate::impls::{bx, Chain, RelayChain};

					<Self as $crate::impls::TestExt>::execute_with(|| {
						let root_origin = <Self as Chain>::RuntimeOrigin::root();
						let destination:  $crate::impls::Location = <Self as RelayChain>::child_location_of(recipient);
						let xcm = $crate::impls::xcm_transact_unpaid_execution(call, $crate::impls::OriginKind::Superuser);

						$crate::impls::dmp::Pallet::<<Self as $crate::impls::Chain>::Runtime>::make_parachain_reachable(recipient);

						// Send XCM `Transact`
						$crate::impls::assert_ok!(<Self as [<$chain RelayPallet>]>::XcmPallet::send(
							root_origin,
							bx!(destination.into()),
							bx!(xcm),
						));
						Self::assert_xcm_pallet_sent();
					});
				}
			}
		}
	};
}

#[macro_export]
macro_rules! impl_accounts_helpers_for_parachain {
	( $chain:ident ) => {
		$crate::impls::paste::paste! {
			impl<N: $crate::impls::Network> $chain<N> {
				/// Fund a set of accounts with a balance
				pub fn fund_accounts(accounts: Vec<($crate::impls::AccountId, $crate::impls::Balance)>) {
					<Self as $crate::impls::TestExt>::execute_with(|| {
						for account in accounts {
							let who = account.0;
							let actual = <Self as [<$chain ParaPallet>]>::Balances::free_balance(&who);
							let actual = actual.saturating_add(<Self as [<$chain ParaPallet>]>::Balances::reserved_balance(&who));

							$crate::impls::assert_ok!(<Self as [<$chain ParaPallet>]>::Balances::force_set_balance(
								<Self as $crate::impls::Chain>::RuntimeOrigin::root(),
								who.into(),
								actual.saturating_add(account.1),
							));
						}
					});
				}

				/// Fund a sovereign account of sibling para.
				pub fn fund_para_sovereign(sibling_para_id: $crate::impls::ParaId, balance: $crate::impls::Balance) {
					let sibling_location = Self::sibling_location_of(sibling_para_id);
					let sovereign_account = Self::sovereign_account_id_of(sibling_location);
					Self::fund_accounts(vec![(sovereign_account.into(), balance)])
				}

				/// Return local sovereign account of `para_id` on other `network_id`
				pub fn sovereign_account_of_parachain_on_other_global_consensus(
					network_id: $crate::impls::NetworkId,
					para_id: $crate::impls::ParaId,
				) -> $crate::impls::AccountId {
					let remote_location = $crate::impls::Location::new(
						2,
						[
							$crate::impls::Junction::GlobalConsensus(network_id),
							$crate::impls::Junction::Parachain(para_id.into()),
						],
					);
					<Self as $crate::impls::TestExt>::execute_with(|| {
						Self::sovereign_account_id_of(remote_location)
					})
				}
			}
		}
	};
}

#[macro_export]
macro_rules! impl_assert_events_helpers_for_parachain {
	( $chain:ident ) => {
		$crate::impls::paste::paste! {
			type [<$chain RuntimeEvent>]<N> = <$chain<N> as $crate::impls::Chain>::RuntimeEvent;

			impl<N: $crate::impls::Network> $chain<N> {
				/// Asserts a dispatchable is completely executed and XCM sent
				pub fn assert_xcm_pallet_attempted_complete(expected_weight: Option<$crate::impls::Weight>) {
					$crate::impls::assert_expected_events!(
						Self,
						vec![
							[<$chain RuntimeEvent>]::<N>::PolkadotXcm(
								$crate::impls::pallet_xcm::Event::Attempted { outcome: $crate::impls::Outcome::Complete { used: weight } }
							) => {
								weight: $crate::impls::weight_within_threshold(
									($crate::impls::REF_TIME_THRESHOLD, $crate::impls::PROOF_SIZE_THRESHOLD),
									expected_weight.unwrap_or(*weight),
									*weight
								),
							},
						]
					);
				}

				/// Asserts a dispatchable is incompletely executed and XCM sent
				pub fn assert_xcm_pallet_attempted_incomplete(
					expected_weight: Option<$crate::impls::Weight>,
					expected_error: Option<$crate::impls::XcmError>,
				) {
					$crate::impls::assert_expected_events!(
						Self,
						vec![
							// Dispatchable is properly executed and XCM message sent
							[<$chain RuntimeEvent>]::<N>::PolkadotXcm(
								$crate::impls::pallet_xcm::Event::Attempted { outcome: $crate::impls::Outcome::Incomplete { used: weight, error: $crate::impls::xcm::prelude::InstructionError { error, .. } } }
							) => {
								weight: $crate::impls::weight_within_threshold(
									($crate::impls::REF_TIME_THRESHOLD, $crate::impls::PROOF_SIZE_THRESHOLD),
									expected_weight.unwrap_or(*weight),
									*weight
								),
								error: *error == expected_error.unwrap_or((*error).into()).into(),
							},
						]
					);
				}

				/// Asserts a dispatchable throws and error when trying to be sent
				pub fn assert_xcm_pallet_attempted_error(expected_error: Option<$crate::impls::XcmError>) {
					$crate::impls::assert_expected_events!(
						Self,
						vec![
							// Execution fails in the origin with `Barrier`
							[<$chain RuntimeEvent>]::<N>::PolkadotXcm(
								$crate::impls::pallet_xcm::Event::Attempted { outcome: $crate::impls::Outcome::Error($crate::impls::xcm::prelude::InstructionError { error, .. }) }
							) => {
								error: *error == expected_error.unwrap_or((*error).into()).into(),
							},
						]
					);
				}

				/// Asserts a XCM message is sent
				pub fn assert_xcm_pallet_sent() {
					$crate::impls::assert_expected_events!(
						Self,
						vec![
							[<$chain RuntimeEvent>]::<N>::PolkadotXcm($crate::impls::pallet_xcm::Event::Sent { .. }) => {},
						]
					);
				}

				/// Asserts a XCM message is sent to Relay Chain
				pub fn assert_parachain_system_ump_sent() {
					$crate::impls::assert_expected_events!(
						Self,
						vec![
							[<$chain RuntimeEvent>]::<N>::ParachainSystem(
								$crate::impls::cumulus_pallet_parachain_system::Event::UpwardMessageSent { .. }
							) => {},
						]
					);
				}

				/// Asserts a XCM from Relay Chain is completely executed
				pub fn assert_dmp_queue_complete(expected_weight: Option<$crate::impls::Weight>) {
					$crate::impls::assert_expected_events!(
						Self,
						vec![
							[<$chain RuntimeEvent>]::<N>::MessageQueue($crate::impls::pallet_message_queue::Event::Processed {
								success: true, weight_used: weight, ..
							}) => {
								weight: $crate::impls::weight_within_threshold(
									($crate::impls::REF_TIME_THRESHOLD, $crate::impls::PROOF_SIZE_THRESHOLD),
									expected_weight.unwrap_or(*weight),
									*weight
								),
							},
						]
					);
				}

				/// Asserts a XCM from Relay Chain is incompletely executed
				pub fn assert_dmp_queue_incomplete(
					expected_weight: Option<$crate::impls::Weight>,
				) {
					$crate::impls::assert_expected_events!(
						Self,
						vec![
							[<$chain RuntimeEvent>]::<N>::MessageQueue($crate::impls::pallet_message_queue::Event::Processed {
								success: false, weight_used: weight, ..
							}) => {
								weight: $crate::impls::weight_within_threshold(
									($crate::impls::REF_TIME_THRESHOLD, $crate::impls::PROOF_SIZE_THRESHOLD),
									expected_weight.unwrap_or(*weight),
									*weight
								),
							},
						]
					);
				}

				/// Asserts a XCM from Relay Chain is executed with error
				pub fn assert_dmp_queue_error() {
					$crate::impls::assert_expected_events!(
						Self,
						vec![
							[<$chain RuntimeEvent>]::<N>::MessageQueue($crate::impls::pallet_message_queue::Event::ProcessingFailed {
								..
							}) => {

							},
						]
					);
				}

				/// Asserts a XCM from another Parachain is completely executed
				pub fn assert_xcmp_queue_success(expected_weight: Option<$crate::impls::Weight>) {
					$crate::impls::assert_expected_events!(
						Self,
						vec![
							[<$chain RuntimeEvent>]::<N>::MessageQueue($crate::impls::pallet_message_queue::Event::Processed { success: true, weight_used: weight, .. }
							) => {
								weight: $crate::impls::weight_within_threshold(
									($crate::impls::REF_TIME_THRESHOLD, $crate::impls::PROOF_SIZE_THRESHOLD),
									expected_weight.unwrap_or(*weight),
									*weight
								),
							},
						]
					);
				}
			}
		}
	};
}

#[macro_export]
macro_rules! impl_assets_helpers_for_system_parachain {
	( $chain:ident, $relay_chain:ident ) => {
		$crate::impls::paste::paste! {
			impl<N: $crate::impls::Network> $chain<N> {
				/// Returns the encoded call for `force_create` from the assets pallet
				pub fn force_create_asset_call(
					asset_id: u32,
					owner: $crate::impls::AccountId,
					is_sufficient: bool,
					min_balance: $crate::impls::Balance,
				) -> $crate::impls::DoubleEncoded<()> {
					use $crate::impls::{Chain, Encode};

					<Self as Chain>::RuntimeCall::Assets($crate::impls::pallet_assets::Call::<
						<Self as Chain>::Runtime,
						$crate::impls::pallet_assets::Instance1,
					>::force_create {
						id: asset_id.into(),
						owner: owner.into(),
						is_sufficient,
						min_balance,
					})
					.encode()
					.into()
				}

				/// Returns a `VersionedXcm` for `force_create` from the assets pallet
				pub fn force_create_asset_xcm(
					origin_kind: $crate::impls::OriginKind,
					asset_id: u32,
					owner: $crate::impls::AccountId,
					is_sufficient: bool,
					min_balance: $crate::impls::Balance,
				) -> $crate::impls::VersionedXcm<()> {
					let call = Self::force_create_asset_call(asset_id, owner, is_sufficient, min_balance);
					$crate::impls::xcm_transact_unpaid_execution(call, origin_kind)
				}

				/// Force create and mint assets making use of the assets pallet
				pub fn force_create_and_mint_asset(
					id: u32,
					min_balance: u128,
					is_sufficient: bool,
					asset_owner: $crate::impls::AccountId,
					dmp_weight_threshold: Option<$crate::impls::Weight>,
					amount_to_mint: u128,
				) {
					use $crate::impls::Chain;

					// Force create asset
					Self::force_create_asset_from_relay_as_root(
						id,
						min_balance,
						is_sufficient,
						asset_owner.clone(),
						dmp_weight_threshold
					);

					// Mint asset for System Parachain's sender
					let signed_origin = <Self as Chain>::RuntimeOrigin::signed(asset_owner.clone());
					Self::mint_asset(signed_origin, id, asset_owner, amount_to_mint);
				}

				/// Relay Chain sends `Transact` instruction with `force_create_asset` to Parachain with `Assets` instance of `pallet_assets` .
				pub fn force_create_asset_from_relay_as_root(
					id: u32,
					min_balance: u128,
					is_sufficient: bool,
					asset_owner: $crate::impls::AccountId,
					dmp_weight_threshold: Option<$crate::impls::Weight>,
				) {
					use $crate::impls::{Parachain, Inspect, TestExt};

					<$relay_chain<N>>::send_unpaid_transact_to_parachain_as_root(
						Self::para_id(),
						Self::force_create_asset_call(id, asset_owner.clone(), is_sufficient, min_balance),
					);

					// Receive XCM message in Assets Parachain
					Self::execute_with(|| {
						type RuntimeEvent<N> = <$chain<N> as $crate::impls::Chain>::RuntimeEvent;

						Self::assert_dmp_queue_complete(dmp_weight_threshold);

						$crate::impls::assert_expected_events!(
							Self,
							vec![
								RuntimeEvent::<N>::Assets($crate::impls::pallet_assets::Event::ForceCreated { asset_id, owner }) => {
									asset_id: *asset_id == id,
									owner: *owner == asset_owner,
								},
							]
						);

						assert!(<Self as [<$chain ParaPallet>]>::Assets::asset_exists(id.clone().into()));
					});
				}
			}
		}
	};
}

#[macro_export]
macro_rules! impl_assets_helpers_for_parachain {
	($chain:ident) => {
		$crate::impls::paste::paste! {
			impl<N: $crate::impls::Network> $chain<N> {
				/// Create assets using sudo `Assets::force_create()`
				pub fn force_create_asset(
					id: u32,
					owner: $crate::impls::AccountId,
					is_sufficient: bool,
					min_balance: u128,
					prefund_accounts: Vec<($crate::impls::AccountId, u128)>,
				) {
					use $crate::impls::Inspect;
					let sudo_origin = <$chain<N> as $crate::impls::Chain>::RuntimeOrigin::root();
					<Self as $crate::impls::TestExt>::execute_with(|| {
						$crate::impls::assert_ok!(
							<Self as [<$chain ParaPallet>]>::Assets::force_create(
								sudo_origin,
								id.clone().into(),
								owner.clone().into(),
								is_sufficient,
								min_balance,
							)
						);
						assert!(<Self as [<$chain ParaPallet>]>::Assets::asset_exists(id.clone()));
						type RuntimeEvent<N> = <$chain<N> as $crate::impls::Chain>::RuntimeEvent;
						$crate::impls::assert_expected_events!(
							Self,
							vec![
								RuntimeEvent::<N>::Assets(
									$crate::impls::pallet_assets::Event::ForceCreated {
										asset_id,
										..
									}
								) => { asset_id: *asset_id == id, },
							]
						);
					});
					for (beneficiary, amount) in prefund_accounts.into_iter() {
						let signed_origin =
							<$chain<N> as $crate::impls::Chain>::RuntimeOrigin::signed(owner.clone());
						Self::mint_asset(signed_origin, id.clone(), beneficiary, amount);
					}
				}

				/// Mint assets making use of the assets pallet
				pub fn mint_asset(
					signed_origin: <Self as $crate::impls::Chain>::RuntimeOrigin,
					id: u32,
					beneficiary: $crate::impls::AccountId,
					amount_to_mint: u128,
				) {
					<Self as $crate::impls::TestExt>::execute_with(|| {
						$crate::impls::assert_ok!(<Self as [<$chain ParaPallet>]>::Assets::mint(
							signed_origin,
							id.clone().into(),
							beneficiary.clone().into(),
							amount_to_mint
						));

						type RuntimeEvent<N> = <$chain<N> as $crate::impls::Chain>::RuntimeEvent;

						$crate::impls::assert_expected_events!(
							Self,
							vec![
								RuntimeEvent::<N>::Assets(
									$crate::impls::pallet_assets::Event::Issued { asset_id, owner, amount }
								) => {
									asset_id: *asset_id == id,
									owner: *owner == beneficiary.clone().into(),
									amount: *amount == amount_to_mint,
								},
							]
						);
					});
				}

				/// Returns the encoded call for `create` from the assets pallet
				pub fn create_asset_call(
					asset_id: u32,
					min_balance: $crate::impls::Balance,
					admin: $crate::impls::AccountId,
				) -> $crate::impls::DoubleEncoded<()> {
					use $crate::impls::{Chain, Encode};

					<Self as Chain>::RuntimeCall::Assets($crate::impls::pallet_assets::Call::<
						<Self as Chain>::Runtime,
						$crate::impls::pallet_assets::Instance1,
					>::create {
						id: asset_id.into(),
						min_balance,
						admin: admin.into(),
					})
					.encode()
					.into()
				}
			}
		}
	};
}

#[macro_export]
macro_rules! impl_foreign_assets_helpers_for_parachain {
	// By default, we assume that the pallet_assets instance handling foreign assets is called
	// `ForeignAssets` and uses `Instance2`. There are exceptions like `Penpal`, where it is
	// simply called `Assets`.
	($chain:ident, $asset_id_type:ty, $reserve_data_type:ty) => {
		$crate::impl_foreign_assets_helpers_for_parachain!(
			$chain,
			$asset_id_type,
			$reserve_data_type,
			ForeignAssets,
			$crate::impls::pallet_assets::Instance2
		);
	};

	($chain:ident, $asset_id_type:ty, $reserve_data_type:ty, $pallet_asset_name:ident) => {
		$crate::impl_foreign_assets_helpers_for_parachain!(
			$chain,
			$asset_id_type,
			$reserve_data_type,
			$pallet_asset_name,
			$crate::impls::pallet_assets::Instance2
		);
	};

	($chain:ident, $asset_id_type:ty, $reserve_data_type:ty, $pallet_asset_name:ident, $instance:ty) => {
		$crate::impls::paste::paste! {
			impl<N: $crate::impls::Network> $chain<N> {
				/// Create foreign assets using sudo `ForeignAssets::force_create()`
				pub fn force_create_foreign_asset(
					id: $asset_id_type,
					owner: $crate::impls::AccountId,
					is_sufficient: bool,
					min_balance: u128,
					prefund_accounts: Vec<($crate::impls::AccountId, u128)>,
				) {
					use $crate::impls::Inspect;
					let sudo_origin = <$chain<N> as $crate::impls::Chain>::RuntimeOrigin::root();
					<Self as $crate::impls::TestExt>::execute_with(|| {
						$crate::impls::assert_ok!(
							<Self as [<$chain ParaPallet>]>::$pallet_asset_name::force_create(
								sudo_origin,
								id.clone(),
								owner.clone().into(),
								is_sufficient,
								min_balance,
							)
						);
						assert!(<Self as [<$chain ParaPallet>]>::$pallet_asset_name::asset_exists(id.clone()));
						type RuntimeEvent<N> = <$chain<N> as $crate::impls::Chain>::RuntimeEvent;
						$crate::impls::assert_expected_events!(
							Self,
							vec![
								RuntimeEvent::<N>::$pallet_asset_name(
									$crate::impls::pallet_assets::Event::ForceCreated {
										asset_id,
										..
									}
								) => { asset_id: *asset_id == id, },
							]
						);
					});
					for (beneficiary, amount) in prefund_accounts.into_iter() {
						let signed_origin =
							<$chain<N> as $crate::impls::Chain>::RuntimeOrigin::signed(owner.clone());
						Self::mint_foreign_asset(signed_origin, id.clone(), beneficiary, amount);
					}
				}

				/// Set reserves for foreign asset using the asset's `owner` account.
				pub fn set_foreign_asset_reserves(
					id: $asset_id_type,
					owner: $crate::impls::AccountId,
					reserves: Vec<$reserve_data_type>,
				) {
					use $crate::impls::Inspect;
					let owner_origin =
						<$chain<N> as $crate::impls::Chain>::RuntimeOrigin::signed(owner.clone());
					<Self as $crate::impls::TestExt>::execute_with(|| {
						$crate::impls::assert_ok!(
							<Self as [<$chain ParaPallet>]>::$pallet_asset_name::set_reserves(
								owner_origin,
								id.clone(),
								reserves.try_into().unwrap(),
							)
						);
					});
				}

				/// Mint assets making use of the ForeignAssets pallet-assets instance
				pub fn mint_foreign_asset(
					signed_origin: <Self as $crate::impls::Chain>::RuntimeOrigin,
					id: $asset_id_type,
					beneficiary: $crate::impls::AccountId,
					amount_to_mint: u128,
				) {
					<Self as $crate::impls::TestExt>::execute_with(|| {
						$crate::impls::assert_ok!(<Self as [<$chain ParaPallet>]>::$pallet_asset_name::mint(
							signed_origin,
							id.clone().into(),
							beneficiary.clone().into(),
							amount_to_mint
						));

						type RuntimeEvent<N> = <$chain<N> as $crate::impls::Chain>::RuntimeEvent;

						$crate::impls::assert_expected_events!(
							Self,
							vec![
								RuntimeEvent::<N>::$pallet_asset_name(
									$crate::impls::pallet_assets::Event::Issued { asset_id, owner, amount }
								) => {
									asset_id: *asset_id == id,
									owner: *owner == beneficiary.clone().into(),
									amount: *amount == amount_to_mint,
								},
							]
						);
					});
				}

				/// Returns the encoded call for `create` from the foreign assets pallet
				pub fn create_foreign_asset_call(
					asset_id: $asset_id_type,
					min_balance: $crate::impls::Balance,
					admin: $crate::impls::AccountId,
				) -> $crate::impls::DoubleEncoded<()> {
					use $crate::impls::{Chain, Encode};

					<Self as Chain>::RuntimeCall::$pallet_asset_name($crate::impls::pallet_assets::Call::<
						<Self as Chain>::Runtime,
						$instance,
					>::create {
						id: asset_id.into(),
						min_balance,
						admin: admin.into(),
					})
					.encode()
					.into()
				}
			}
		}
	};
}

#[macro_export]
macro_rules! impl_xcm_helpers_for_parachain {
	( $chain:ident ) => {
		$crate::impls::paste::paste! {
			impl<N: $crate::impls::Network> $chain<N> {
				/// Set XCM version for destination.
				pub fn force_xcm_version(dest: $crate::impls::Location, version: $crate::impls::XcmVersion) {
					<Self as $crate::impls::TestExt>::execute_with(|| {
						$crate::impls::assert_ok!(<Self as [<$chain ParaPallet>]>::PolkadotXcm::force_xcm_version(
							<Self as $crate::impls::Chain>::RuntimeOrigin::root(),
							$crate::impls::bx!(dest),
							version,
						));
					});
				}

				/// Set default/safe XCM version for runtime.
				pub fn force_default_xcm_version(version: Option<$crate::impls::XcmVersion>) {
					<Self as $crate::impls::TestExt>::execute_with(|| {
						$crate::impls::assert_ok!(<Self as [<$chain ParaPallet>]>::PolkadotXcm::force_default_xcm_version(
							<Self as $crate::impls::Chain>::RuntimeOrigin::root(),
							version,
						));
					});
				}
			}
		}
	}
}

#[macro_export]
macro_rules! impl_bridge_helpers_for_chain {
	( $chain:ident, $pallet:ident, $pallet_xcm:ident, $runtime_call_wrapper:path ) => {
		$crate::impls::paste::paste! {
			impl<N: $crate::impls::Network> $chain<N> {
				/// Open bridge with `dest`.
				pub fn open_bridge(
					bridge_location: $crate::impls::Location,
					bridge_destination_universal_location: $crate::impls::InteriorLocation,
					maybe_paid: Option<($crate::impls::Asset, $crate::impls::AccountId)>
				) {
					<Self as $crate::impls::TestExt>::execute_with(|| {
						use $crate::impls::{bx, Chain};
						use $crate::impls::XcmBridgeHubCall;
						use $crate::impls::Encode;

						// important to use `root` and `OriginKind::Xcm`
						let root_origin = <Self as Chain>::RuntimeOrigin::root();

						// construct call
						let call: $crate::impls::DoubleEncoded<()> = $runtime_call_wrapper(XcmBridgeHubCall::open_bridge {
							bridge_destination_universal_location: bx!(
								bridge_destination_universal_location.clone().into()
							)
						}).encode().into();

						let xcm = if let Some((fee_asset, beneficiary)) = maybe_paid {
							$crate::impls::xcm_transact_paid_execution(call, $crate::impls::OriginKind::Xcm, fee_asset, beneficiary)
						} else {
							$crate::impls::xcm_transact_unpaid_execution(call, $crate::impls::OriginKind::Xcm)
						};

						// Send XCM `Transact` with `open_bridge` call
						$crate::impls::assert_ok!(<Self as [<$chain $pallet>]>::$pallet_xcm::send(
							root_origin,
							bx!(bridge_location.into()),
							bx!(xcm),
						));
						Self::assert_xcm_pallet_sent();
					});
				}
			}
		}
	}
}
