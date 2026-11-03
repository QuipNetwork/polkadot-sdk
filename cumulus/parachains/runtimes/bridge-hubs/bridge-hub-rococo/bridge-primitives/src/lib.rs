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

//! Module with configuration which reflects BridgeHubRococo runtime setup (AccountId, Headers,
//! Hashes...)

#![warn(missing_docs)]
#![cfg_attr(not(feature = "std"), no_std)]

pub use bp_bridge_hub_cumulus::*;
use bp_messages::*;
use bp_runtime::{
	decl_bridge_finality_runtime_apis, decl_bridge_messages_runtime_apis, Chain, ChainId, Parachain,
};
use codec::{Decode, Encode};
use frame_support::{
	dispatch::DispatchClass,
	sp_runtime::{MultiAddress, MultiSigner, StateVersion},
};

/// BridgeHubRococo parachain.
#[derive(Debug)]
pub struct BridgeHubRococo;

impl Chain for BridgeHubRococo {
	const ID: ChainId = *b"bhro";

	type BlockNumber = BlockNumber;
	type Hash = Hash;
	type Hasher = Hasher;
	type Header = Header;

	type AccountId = AccountId;
	type Balance = Balance;
	type Nonce = Nonce;
	type Signature = Signature;

	const STATE_VERSION: StateVersion = StateVersion::V1;

	fn max_extrinsic_size() -> u32 {
		*BlockLength::get().max.get(DispatchClass::Normal)
	}

	fn max_extrinsic_weight() -> Weight {
		BlockWeightsForAsyncBacking::get()
			.get(DispatchClass::Normal)
			.max_extrinsic
			.unwrap_or(Weight::MAX)
	}
}

impl Parachain for BridgeHubRococo {
	const PARACHAIN_ID: u32 = BRIDGE_HUB_ROCOCO_PARACHAIN_ID;
	const MAX_HEADER_SIZE: u32 = MAX_BRIDGE_HUB_HEADER_SIZE;
}

impl ChainWithMessages for BridgeHubRococo {
	const WITH_CHAIN_MESSAGES_PALLET_NAME: &'static str =
		WITH_BRIDGE_HUB_ROCOCO_MESSAGES_PALLET_NAME;

	const MAX_UNREWARDED_RELAYERS_IN_CONFIRMATION_TX: MessageNonce =
		MAX_UNREWARDED_RELAYERS_IN_CONFIRMATION_TX;
	const MAX_UNCONFIRMED_MESSAGES_IN_CONFIRMATION_TX: MessageNonce =
		MAX_UNCONFIRMED_MESSAGES_IN_CONFIRMATION_TX;
}

/// Public key of the chain account that may be used to verify signatures.
pub type AccountSigner = MultiSigner;

/// The address format for describing accounts.
pub type Address = MultiAddress<AccountId, ()>;

/// Identifier of BridgeHubRococo in the Rococo relay chain.
pub const BRIDGE_HUB_ROCOCO_PARACHAIN_ID: u32 = 1013;

/// Name of the With-BridgeHubRococo messages pallet instance that is deployed at bridged chains.
pub const WITH_BRIDGE_HUB_ROCOCO_MESSAGES_PALLET_NAME: &str = "BridgeRococoMessages";

/// Name of the With-BridgeHubRococo bridge-relayers pallet instance that is deployed at bridged
/// chains.
pub const WITH_BRIDGE_HUB_ROCOCO_RELAYERS_PALLET_NAME: &str = "BridgeRelayers";

/// Pallet index of `BridgeWestendMessages: pallet_bridge_messages::<Instance3>`.
pub const WITH_BRIDGE_ROCOCO_TO_WESTEND_MESSAGES_PALLET_INDEX: u8 = 51;

decl_bridge_finality_runtime_apis!(bridge_hub_rococo);
decl_bridge_messages_runtime_apis!(bridge_hub_rococo, LegacyLaneId);

frame_support::parameter_types! {
	/// The XCM fee that is paid for executing XCM program (with `ExportMessage` instruction) at the Rococo
	/// BridgeHub.
	/// (initially was calculated by test `BridgeHubRococo::can_calculate_weight_for_paid_export_message_with_reserve_transfer` + `33%`)
	pub const BridgeHubRococoBaseXcmFeeInRocs: u128 = 72_091_666;

	/// Transaction fee that is paid at the Rococo BridgeHub for delivering single inbound message.
	/// (initially was calculated by test `BridgeHubRococo::can_calculate_fee_for_standalone_message_delivery_transaction` + `33%`)
	pub const BridgeHubRococoBaseDeliveryFeeInRocs: u128 = 297_685_840;

	/// Transaction fee that is paid at the Rococo BridgeHub for delivering single outbound message confirmation.
	/// (initially was calculated by test `BridgeHubRococo::can_calculate_fee_for_standalone_message_confirmation_transaction` + `33%`)
	pub const BridgeHubRococoBaseConfirmationFeeInRocs: u128 = 56_782_099;
}

/// Wrapper over `BridgeHubRococo`'s `RuntimeCall` that can be used without a runtime.
#[derive(Decode, Encode)]
pub enum RuntimeCall {
	/// Points to the `pallet_xcm_bridge_hub` pallet instance for `BridgeHubWestend`.
	#[codec(index = 52)]
	XcmOverBridgeHubWestend(bp_xcm_bridge_hub::XcmBridgeHubCall),
}
