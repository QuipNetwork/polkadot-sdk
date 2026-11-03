// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023 Snowfork <hello@snowfork.com>
//! Converts messages from Solidity ABI-encoding to XCM

use super::{message::*, traits::*};
use crate::{v2::LOG_TARGET, CallIndex};
use codec::{Decode, Encode};
use core::marker::PhantomData;
use frame_support::ensure;
use snowbridge_core::{ParaId, TokenId};
use sp_core::{Get, H160};
use sp_io::hashing::blake2_256;
use sp_runtime::{traits::MaybeConvert, MultiAddress};
use sp_std::prelude::*;
use xcm::prelude::{Junction::*, *};
use xcm_builder::ExternalConsensusLocationsConverterFor;
use xcm_executor::traits::ConvertLocation;

/// Topic prefix used for generating unique identifiers for messages
const INBOUND_QUEUE_TOPIC_PREFIX: &str = "SnowbridgeInboundQueueV2";

/// Representation of an intermediate parsed message, before final
/// conversion to XCM.
#[derive(Clone, Debug, Encode)]
pub struct PreparedMessage {
	/// Ethereum account that initiated this messaging operation
	pub origin: H160,
	/// The claimer in the case that funds get trapped.
	pub claimer: Location,
	/// The assets bridged from Ethereum
	pub assets: Vec<AssetTransfer>,
	/// The XCM to execute on the destination
	pub remote_xcm: Xcm<()>,
	/// Fee in Ether to cover the xcm execution on AH.
	pub execution_fee: Asset,
}

/// An asset transfer instruction
#[derive(Clone, Debug, Encode)]
pub enum AssetTransfer {
	ReserveDeposit(Asset),
	ReserveWithdraw(Asset),
}

#[derive(Clone, Debug, Encode)]
pub struct CreateAssetCallInfo {
	pub create_call: CallIndex,
	pub deposit: u128,
	pub min_balance: u128,
	pub set_reserves_call: CallIndex,
}

pub struct AssetHubUniversal<LocalNetwork, AssetHubParaId>(
	PhantomData<(LocalNetwork, AssetHubParaId)>,
);
impl<LocalNetwork, AssetHubParaId> Get<InteriorLocation>
	for AssetHubUniversal<LocalNetwork, AssetHubParaId>
where
	LocalNetwork: Get<NetworkId>,
	AssetHubParaId: Get<ParaId>,
{
	fn get() -> InteriorLocation {
		[GlobalConsensus(LocalNetwork::get()), Parachain(AssetHubParaId::get().into())].into()
	}
}

/// Concrete implementation of `ConvertMessage`
pub struct MessageToXcm<
	CreateAssetCall,
	EthereumNetwork,
	LocalNetwork,
	GatewayProxyAddress,
	InboundQueueLocation,
	AssetHubParaId,
	ConvertAssetId,
	AccountId,
> {
	_phantom: PhantomData<(
		CreateAssetCall,
		EthereumNetwork,
		LocalNetwork,
		GatewayProxyAddress,
		InboundQueueLocation,
		AssetHubParaId,
		ConvertAssetId,
		AccountId,
	)>,
}

impl<
		CreateAssetCall,
		EthereumNetwork,
		LocalNetwork,
		GatewayProxyAddress,
		InboundQueueLocation,
		AssetHubParaId,
		ConvertAssetId,
		AccountId,
	>
	MessageToXcm<
		CreateAssetCall,
		EthereumNetwork,
		LocalNetwork,
		GatewayProxyAddress,
		InboundQueueLocation,
		AssetHubParaId,
		ConvertAssetId,
		AccountId,
	>
where
	CreateAssetCall: Get<CreateAssetCallInfo>,
	EthereumNetwork: Get<NetworkId>,
	LocalNetwork: Get<NetworkId>,
	GatewayProxyAddress: Get<H160>,
	InboundQueueLocation: Get<InteriorLocation>,
	AssetHubParaId: Get<ParaId>,
	ConvertAssetId: MaybeConvert<TokenId, Location>,
	AccountId: Into<[u8; 32]> + From<[u8; 32]> + Clone,
{
	/// Parse the message into an intermediate form, with all fields decoded
	/// and prepared.
	fn prepare(message: Message) -> Result<PreparedMessage, ConvertMessageError> {
		// ETH "asset id" is the Ethereum root location. Same location used for the "bridge owner".
		let ether_location = Location::new(2, [GlobalConsensus(EthereumNetwork::get())]);
		let bridge_owner = Self::bridge_owner()?;

		let claimer = message
			.claimer
			// Get the claimer from the message,
			.and_then(|claimer_bytes| Location::decode(&mut claimer_bytes.as_ref()).ok())
			// or use the Snowbridge sovereign on AH as the fallback claimer.
			.unwrap_or_else(|| {
				Location::new(
					0,
					[AccountId32 {
						network: Some(LocalNetwork::get()),
						id: bridge_owner.clone().into(),
					}],
				)
			});

		let mut remote_xcm: Xcm<()> = match &message.payload {
			Payload::Raw(raw) => Self::decode_raw_xcm(raw),
			Payload::CreateAsset { token, network } => Self::make_create_asset_xcm(
				token,
				*network,
				message.value,
				bridge_owner,
				claimer.clone(),
			)?,
		};

		// Asset to cover XCM execution fee
		let execution_fee_asset: Asset = (ether_location.clone(), message.execution_fee).into();

		let mut assets = vec![];

		if message.value > 0 {
			// Asset for remaining ether
			let remaining_ether_asset: Asset = (ether_location.clone(), message.value).into();
			assets.push(AssetTransfer::ReserveDeposit(remaining_ether_asset));
		}

		for asset in &message.assets {
			match asset {
				EthereumAsset::NativeTokenERC20 { token_id, value } => {
					ensure!(*token_id != H160::zero(), ConvertMessageError::InvalidAsset);
					let token_location: Location = Location::new(
						2,
						[
							GlobalConsensus(EthereumNetwork::get()),
							AccountKey20 { network: None, key: (*token_id).into() },
						],
					);
					let asset: Asset = (token_location, *value).into();
					assets.push(AssetTransfer::ReserveDeposit(asset));
				},
				EthereumAsset::ForeignTokenERC20 { token_id, value } => {
					let asset_location = ConvertAssetId::maybe_convert(*token_id)
						.ok_or(ConvertMessageError::InvalidAsset)?;
					let asset_hub_from_ethereum: Location = Location::new(
						1,
						[
							GlobalConsensus(LocalNetwork::get()),
							Parachain(AssetHubParaId::get().into()),
						],
					);
					let ethereum_universal: InteriorLocation =
						[GlobalConsensus(EthereumNetwork::get())].into();
					let reanchored_asset_location = asset_location
						.reanchored(&asset_hub_from_ethereum, &ethereum_universal)
						.map_err(|_| ConvertMessageError::CannotReanchor)?;
					let asset: Asset = (reanchored_asset_location, *value).into();
					assets.push(AssetTransfer::ReserveWithdraw(asset));
				},
			}
		}

		// Add SetTopic instruction if not already present as the last instruction
		if !matches!(remote_xcm.0.last(), Some(SetTopic(_))) {
			let topic = blake2_256(&(INBOUND_QUEUE_TOPIC_PREFIX, message.nonce).encode());
			remote_xcm.0.push(SetTopic(topic));
		}

		let prepared_message = PreparedMessage {
			origin: message.origin,
			claimer,
			assets,
			remote_xcm,
			execution_fee: execution_fee_asset,
		};

		Ok(prepared_message)
	}

	/// Get sovereign account of Ethereum on Asset Hub.
	fn bridge_owner() -> Result<AccountId, ConvertMessageError> {
		let account =
			ExternalConsensusLocationsConverterFor::<
				AssetHubUniversal<LocalNetwork, AssetHubParaId>,
				AccountId,
			>::convert_location(&Location::new(2, [GlobalConsensus(EthereumNetwork::get())]))
			.ok_or(ConvertMessageError::CannotReanchor)?;

		Ok(account)
	}

	/// Construct the remote XCM needed to create a new asset in the `ForeignAssets` pallet
	/// on AssetHub. Polkadot is the only supported network at the moment.
	fn make_create_asset_xcm(
		token: &H160,
		network: super::message::Network,
		eth_value: u128,
		bridge_owner: AccountId,
		claimer: Location,
	) -> Result<Xcm<()>, ConvertMessageError> {
		let dot_asset = Location::new(1, Here);
		let dot_fee: xcm::prelude::Asset = (dot_asset, CreateAssetCall::get().deposit).into();

		let eth_asset: xcm::prelude::Asset =
			(Location::new(2, [GlobalConsensus(EthereumNetwork::get())]), eth_value).into();

		let create_call_index: [u8; 2] = CreateAssetCall::get().create_call;
		let create_min_blance: u128 = CreateAssetCall::get().min_balance;
		let set_reserves_call_index: [u8; 2] = CreateAssetCall::get().set_reserves_call;

		let asset_id = Location::new(
			2,
			[
				GlobalConsensus(EthereumNetwork::get()),
				AccountKey20 { network: None, key: (*token).into() },
			],
		);

		match network {
			super::message::Network::Polkadot => Ok(Self::make_create_asset_xcm_for_polkadot(
				create_call_index,
				set_reserves_call_index,
				create_min_blance,
				asset_id,
				bridge_owner,
				dot_fee,
				eth_asset,
				claimer,
			)),
		}
	}

	/// Construct the asset creation XCM for the Polkdot network.
	fn make_create_asset_xcm_for_polkadot(
		create_call_index: [u8; 2],
		set_reserves_call_index: [u8; 2],
		create_min_blance: u128,
		asset_id: Location,
		bridge_owner: AccountId,
		dot_fee_asset: xcm::prelude::Asset,
		eth_asset: xcm::prelude::Asset,
		claimer: Location,
	) -> Xcm<()> {
		let bridge_owner_bytes: [u8; 32] = bridge_owner.into();
		let reserve_data = assets_common::local_and_foreign_assets::ForeignAssetReserveData {
			reserve: Location::new(2, [GlobalConsensus(EthereumNetwork::get())]),
			teleportable: false,
		};
		vec![
			// Exchange eth for dot to pay the asset creation deposit.
			ExchangeAsset {
				give: eth_asset.into(),
				want: dot_fee_asset.clone().into(),
				maximal: false,
			},
			// Deposit the dot deposit into the bridge sovereign account (where the asset
			// creation fee will be deducted from).
			DepositAsset {
				assets: dot_fee_asset.clone().into(),
				beneficiary: bridge_owner_bytes.into(),
			},
			// Call to create the asset.
			Transact {
				origin_kind: OriginKind::Xcm,
				fallback_max_weight: None,
				call: (
					create_call_index,
					asset_id.clone(),
					MultiAddress::<[u8; 32], ()>::Id(bridge_owner_bytes.into()),
					create_min_blance,
				)
					.encode()
					.into(),
			},
			// Call to set Ethereum as the asset's reserve.
			Transact {
				origin_kind: OriginKind::Xcm,
				fallback_max_weight: None,
				call: (set_reserves_call_index, asset_id, vec![reserve_data]).encode().into(),
			},
			RefundSurplus,
			// Deposit leftover funds to Snowbridge sovereign
			DepositAsset { assets: Wild(AllCounted(2)), beneficiary: claimer },
		]
		.into()
	}

	/// Parse and (non-strictly) decode `raw` XCM bytes into a `Xcm<()>`.
	/// If decoding fails, return an empty `Xcm<()>`—thus allowing the message
	/// to proceed so assets can still be trapped on AH rather than the funds being locked on
	/// Ethereum but not accessible on AH.
	fn decode_raw_xcm(raw: &[u8]) -> Xcm<()> {
		let mut data = raw;
		if let Ok(versioned_xcm) =
			VersionedXcm::<()>::decode_all_with_mem_and_depth_limit(&mut data)
		{
			if let Ok(decoded_xcm) = versioned_xcm.try_into() {
				return decoded_xcm;
			}
		}
		// Decoding failed; allow an empty XCM so the message won't fail entirely.
		Xcm::new()
	}
}

impl<
		CreateAsset,
		EthereumNetwork,
		LocalNetwork,
		GatewayProxyAddress,
		InboundQueueLocation,
		AssetHubParaId,
		ConvertAssetId,
		AccountId,
	> ConvertMessage
	for MessageToXcm<
		CreateAsset,
		EthereumNetwork,
		LocalNetwork,
		GatewayProxyAddress,
		InboundQueueLocation,
		AssetHubParaId,
		ConvertAssetId,
		AccountId,
	>
where
	CreateAsset: Get<CreateAssetCallInfo>,
	EthereumNetwork: Get<NetworkId>,
	LocalNetwork: Get<NetworkId>,
	GatewayProxyAddress: Get<H160>,
	InboundQueueLocation: Get<InteriorLocation>,
	AssetHubParaId: Get<ParaId>,
	ConvertAssetId: MaybeConvert<TokenId, Location>,
	AccountId: Into<[u8; 32]> + From<[u8; 32]> + Clone,
{
	fn convert(message: Message) -> Result<Xcm<()>, ConvertMessageError> {
		let message = Self::prepare(message)?;

		tracing::trace!(target: LOG_TARGET, ?message, "prepared message");

		let mut instructions = vec![
			DescendOrigin(InboundQueueLocation::get()),
			UniversalOrigin(GlobalConsensus(EthereumNetwork::get())),
			ReserveAssetDeposited(message.execution_fee.clone().into()),
		];

		// Set claimer before PayFees, in case the fees are not enough. Then the claimer will be
		// able to claim the funds still.
		instructions.push(SetHints {
			hints: vec![AssetClaimer { location: message.claimer }]
				.try_into()
				.expect("checked statically, qed"),
		});

		instructions.push(PayFees { asset: message.execution_fee.clone() });

		let mut reserve_deposit_assets = vec![];
		let mut reserve_withdraw_assets = vec![];

		for asset in message.assets {
			match asset {
				AssetTransfer::ReserveDeposit(asset) => reserve_deposit_assets.push(asset),
				AssetTransfer::ReserveWithdraw(asset) => reserve_withdraw_assets.push(asset),
			};
		}

		if !reserve_deposit_assets.is_empty() {
			instructions.push(ReserveAssetDeposited(reserve_deposit_assets.into()));
		}
		if !reserve_withdraw_assets.is_empty() {
			instructions.push(WithdrawAsset(reserve_withdraw_assets.into()));
		}

		// If the message origin is not the gateway proxy contract, set the origin to
		// the original sender on Ethereum. Important to be before the arbitrary XCM that is
		// appended to the message on the next line.
		if message.origin != GatewayProxyAddress::get() {
			instructions.push(DescendOrigin(
				AccountKey20 { key: message.origin.into(), network: None }.into(),
			));
		}

		// Add the XCM sent in the message to the end of the xcm instruction
		instructions.extend(message.remote_xcm.0);

		Ok(instructions.into())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	use codec::Encode;
	use frame_support::{assert_err, assert_ok, parameter_types};
	use hex_literal::hex;
	use snowbridge_core::TokenId;
	use snowbridge_test_utils::mock_converter::{
		add_location_override, reanchor_to_ethereum, LocationIdConvert,
	};
	use sp_core::{H160, H256};
	const GATEWAY_ADDRESS: [u8; 20] = hex!["eda338e4dc46038493b885327842fd3e301cab39"];

	parameter_types! {
		pub const EthereumNetwork: NetworkId = NetworkId::Ethereum { chain_id: 1 };
		pub const LocalNetwork: NetworkId = NetworkId::Polkadot;
		pub const GatewayAddress: H160 = H160(GATEWAY_ADDRESS);
		pub InboundQueueLocation: InteriorLocation = [PalletInstance(84)].into();
		pub EthereumUniversalLocation: InteriorLocation =
			[GlobalConsensus(EthereumNetwork::get())].into();
		pub AssetHubParaId: ParaId = 1000.into();
		pub const CreateAssetCallIndex: [u8;2] = [53, 0];
		pub const SetReservesCallIndex: [u8;2] = [53, 33];
		pub const CreateAssetDeposit: u128 = 10_000_000_000u128;
		pub const CreateAssetMinBalance: u128 = 1;
		pub EthereumLocation: Location = Location::new(2,EthereumUniversalLocation::get());
		pub BridgeHubContext: InteriorLocation = [GlobalConsensus(Polkadot),Parachain(1002)].into();
		pub CreateAssetCall: CreateAssetCallInfo = CreateAssetCallInfo {
			create_call: CreateAssetCallIndex::get(),
			deposit: CreateAssetDeposit::get(),
			min_balance: CreateAssetMinBalance::get(),
			set_reserves_call: SetReservesCallIndex::get(),
		};
	}

	pub struct MockFailedTokenConvert;
	impl MaybeConvert<TokenId, Location> for MockFailedTokenConvert {
		fn maybe_convert(_id: TokenId) -> Option<Location> {
			None
		}
	}

	type Converter = MessageToXcm<
		CreateAssetCall,
		EthereumNetwork,
		LocalNetwork,
		GatewayAddress,
		InboundQueueLocation,
		AssetHubParaId,
		LocationIdConvert,
		[u8; 32],
	>;

	type ConverterFailing = MessageToXcm<
		CreateAssetCall,
		EthereumNetwork,
		LocalNetwork,
		GatewayAddress,
		InboundQueueLocation,
		AssetHubParaId,
		MockFailedTokenConvert,
		[u8; 32],
	>;

	#[test]
	fn test_successful_message() {
		sp_io::TestExternalities::default().execute_with(|| {
			let origin: H160 = hex!("29e3b139f4393adda86303fcdaa35f60bb7092bf").into();
			let native_token_id: H160 = hex!("5615deb798bb3e4dfa0139dfa1b3d433cc23b72f").into();
			let dot_location = Location::parent();
			let (foreign_token_id, _) = reanchor_to_ethereum(
				dot_location.clone(),
				EthereumLocation::get(),
				BridgeHubContext::get(),
			);
			add_location_override(dot_location, EthereumLocation::get(), BridgeHubContext::get());
			let beneficiary: Location =
				hex!("908783d8cd24c9e02cee1d26ab9c46d458621ad0150b626c536a40b9df3f09c6").into();
			let token_value = 3_000_000_000_000u128;
			let assets = vec![
				EthereumAsset::NativeTokenERC20 { token_id: native_token_id, value: token_value },
				EthereumAsset::ForeignTokenERC20 { token_id: foreign_token_id, value: token_value },
			];
			let instructions = vec![DepositAsset {
				assets: Wild(AllCounted(1).into()),
				beneficiary: beneficiary.clone(),
			}];
			let xcm: Xcm<()> = instructions.into();
			let versioned_xcm = VersionedXcm::V5(xcm);
			let claimer_location =
				Location::new(0, AccountId32 { network: None, id: H256::random().into() });
			let claimer: Option<Vec<u8>> = Some(claimer_location.clone().encode());
			let value = 6_000_000_000_000u128;
			let execution_fee = 1_000_000_000_000u128;
			let relayer_fee = 5_000_000_000_000u128;

			let message = Message {
				gateway: H160::zero(),
				nonce: 0,
				origin,
				assets,
				payload: Payload::Raw(versioned_xcm.encode()),
				claimer,
				value,
				execution_fee,
				relayer_fee,
			};

			let result = Converter::convert(message);

			assert_ok!(result.clone());

			let xcm = result.unwrap();

			// Convert to vec for easier inspection
			let instructions: Vec<_> = xcm.into_iter().collect();

			// Check last instruction is a SetTopic (automatically added)
			let last_instruction =
				instructions.last().expect("should have at least one instruction");
			assert!(matches!(last_instruction, SetTopic(_)), "Last instruction should be SetTopic");

			let mut asset_claimer_found = false;
			let mut pay_fees_found = false;
			let mut descend_origin_found = 0;
			let mut reserve_deposited_found = 0;
			let mut withdraw_assets_found = 0;
			let mut deposit_asset_found = 0;

			for instruction in &instructions {
				if let SetHints { ref hints } = instruction {
					if let Some(AssetClaimer { ref location }) = hints.clone().into_iter().next() {
						assert_eq!(claimer_location, location.clone());
						asset_claimer_found = true;
					}
				}
				if let DescendOrigin(ref location) = instruction {
					descend_origin_found += 1;
					// The second DescendOrigin should be the message.origin (sender)
					if descend_origin_found == 2 {
						let junctions: Junctions =
							AccountKey20 { key: origin.into(), network: None }.into();
						assert_eq!(junctions, location.clone());
					}
				}
				if let PayFees { ref asset } = instruction {
					let fee_asset = Location::new(2, [GlobalConsensus(EthereumNetwork::get())]);
					assert_eq!(asset.id, AssetId(fee_asset));
					assert_eq!(asset.fun, Fungible(execution_fee));
					pay_fees_found = true;
				}
				if let ReserveAssetDeposited(ref reserve_assets) = instruction {
					reserve_deposited_found += 1;
					if reserve_deposited_found == 1 {
						let fee_asset = Location::new(2, [GlobalConsensus(EthereumNetwork::get())]);
						let fee: Asset = (fee_asset, execution_fee).into();
						let fee_assets: Assets = fee.into();
						assert_eq!(fee_assets, reserve_assets.clone());
					}
					if reserve_deposited_found == 2 {
						let token_asset = Location::new(
							2,
							[
								GlobalConsensus(EthereumNetwork::get()),
								AccountKey20 { network: None, key: native_token_id.into() },
							],
						);
						let token: Asset = (token_asset, token_value).into();

						let remaining_ether_asset: Asset =
							(Location::new(2, [GlobalConsensus(EthereumNetwork::get())]), value)
								.into();

						let expected_assets: Assets = vec![token, remaining_ether_asset].into();
						assert_eq!(expected_assets, reserve_assets.clone());
					}
				}
				if let WithdrawAsset(ref withdraw_assets) = instruction {
					withdraw_assets_found += 1;
					let token_asset = Location::new(1, Here);
					let token: Asset = (token_asset, token_value).into();
					let token_assets: Assets = token.into();
					assert_eq!(token_assets, withdraw_assets.clone());
				}
				if let DepositAsset { ref assets, beneficiary: deposit_beneficiary } = instruction {
					deposit_asset_found += 1;
					if deposit_asset_found == 1 {
						assert_eq!(AssetFilter::from(Wild(AllCounted(1).into())), assets.clone());
						assert_eq!(*deposit_beneficiary, beneficiary);
					}
				}
			}

			// SetAssetClaimer must be in the message.
			assert!(asset_claimer_found);
			// PayFees must be in the message.
			assert!(pay_fees_found);
			// The first DescendOrigin to descend into the InboundV2 pallet index and the
			// DescendOrigin into the message.origin
			assert!(descend_origin_found == 2);
			// Expecting two ReserveAssetDeposited instructions, one for the fee and one for the
			// token being transferred.
			assert!(reserve_deposited_found == 2);
			// Expecting one WithdrawAsset for the foreign ERC-20
			assert!(withdraw_assets_found == 1);
			// Deposit asset added by user
			assert!(deposit_asset_found == 1);
		});
	}

	#[test]
	fn test_message_with_gateway_origin_does_not_descend_origin_into_sender() {
		let origin: H160 = GatewayAddress::get();
		let native_token_id: H160 = hex!("5615deb798bb3e4dfa0139dfa1b3d433cc23b72f").into();
		let beneficiary =
			hex!("908783d8cd24c9e02cee1d26ab9c46d458621ad0150b626c536a40b9df3f09c6").into();
		let message_id: H256 =
			hex!("8b69c7e376e28114618e829a7ec768dbda28357d359ba417a3bd79b11215059d").into();
		let token_value = 3_000_000_000_000u128;
		let assets =
			vec![EthereumAsset::NativeTokenERC20 { token_id: native_token_id, value: token_value }];
		let instructions = vec![
			DepositAsset { assets: Wild(AllCounted(1).into()), beneficiary },
			SetTopic(message_id.into()),
		];
		let xcm: Xcm<()> = instructions.into();
		let versioned_xcm = VersionedXcm::V5(xcm);
		let claimer_account = AccountId32 { network: None, id: H256::random().into() };
		let claimer: Option<Vec<u8>> = Some(claimer_account.clone().encode());
		let value = 6_000_000_000_000u128;
		let execution_fee = 1_000_000_000_000u128;
		let relayer_fee = 5_000_000_000_000u128;

		let message = Message {
			gateway: H160::zero(),
			nonce: 0,
			origin,
			assets,
			payload: Payload::Raw(versioned_xcm.encode()),
			claimer,
			value,
			execution_fee,
			relayer_fee,
		};

		let result = Converter::convert(message);

		assert_ok!(result.clone());

		let xcm = result.unwrap();

		let mut instructions = xcm.into_iter();
		let mut commands_found = 0;
		while let Some(instruction) = instructions.next() {
			if let DescendOrigin(ref _location) = instruction {
				commands_found = commands_found + 1;
			}
		}
		// There should only be 1 DescendOrigin in the message.
		assert!(commands_found == 1);
	}

	#[test]
	fn test_invalid_foreign_erc20() {
		let origin: H160 = hex!("29e3b139f4393adda86303fcdaa35f60bb7092bf").into();
		let token_id: H256 =
			hex!("37a6c666da38711a963d938eafdd09314fd3f95a96a3baffb55f26560f4ecdd8").into();
		let beneficiary =
			hex!("908783d8cd24c9e02cee1d26ab9c46d458621ad0150b626c536a40b9df3f09c6").into();
		let message_id: H256 =
			hex!("8b69c7e376e28114618e829a7ec768dbda28357d359ba417a3bd79b11215059d").into();
		let token_value = 3_000_000_000_000u128;
		let assets = vec![EthereumAsset::ForeignTokenERC20 { token_id, value: token_value }];
		let instructions = vec![
			DepositAsset { assets: Wild(AllCounted(1).into()), beneficiary },
			SetTopic(message_id.into()),
		];
		let xcm: Xcm<()> = instructions.into();
		let versioned_xcm = VersionedXcm::V5(xcm);
		let claimer_account = AccountId32 { network: None, id: H256::random().into() };
		let claimer: Option<Vec<u8>> = Some(claimer_account.clone().encode());
		let value = 0;
		let execution_fee = 1_000_000_000_000u128;
		let relayer_fee = 5_000_000_000_000u128;

		let message = Message {
			gateway: H160::zero(),
			nonce: 0,
			origin,
			assets,
			payload: Payload::Raw(versioned_xcm.encode()),
			claimer,
			value,
			execution_fee,
			relayer_fee,
		};

		assert_err!(ConverterFailing::convert(message), ConvertMessageError::InvalidAsset);
	}

	#[test]
	fn test_invalid_claimer() {
		sp_io::TestExternalities::default().execute_with(|| {
			let origin: H160 = hex!("29e3b139f4393adda86303fcdaa35f60bb7092bf").into();
			let native_token_id: H160 = hex!("5615deb798bb3e4dfa0139dfa1b3d433cc23b72f").into();
			let beneficiary =
				hex!("908783d8cd24c9e02cee1d26ab9c46d458621ad0150b626c536a40b9df3f09c6").into();
			let token_value = 3_000_000_000_000u128;
			let assets = vec![EthereumAsset::NativeTokenERC20 {
				token_id: native_token_id,
				value: token_value,
			}];
			let instructions =
				vec![DepositAsset { assets: Wild(AllCounted(1).into()), beneficiary }];
			let xcm: Xcm<()> = instructions.into();
			let versioned_xcm = VersionedXcm::V5(xcm);
			// Invalid claimer location, cannot be decoded into a Location
			let claimer: Option<Vec<u8>> = Some(vec![]);
			let value = 6_000_000_000_000u128;
			let execution_fee = 1_000_000_000_000u128;
			let relayer_fee = 5_000_000_000_000u128;

			let message = Message {
				gateway: H160::zero(),
				nonce: 0,
				origin,
				assets,
				payload: Payload::Raw(versioned_xcm.encode()),
				claimer,
				value,
				execution_fee,
				relayer_fee,
			};

			let result = Converter::convert(message.clone());

			// Invalid claimer does not break the message conversion
			assert_ok!(result.clone());

			let xcm = result.unwrap();
			let instructions: Vec<_> = xcm.into_iter().collect();

			// Check last instruction is a SetTopic (automatically added)
			let last_instruction =
				instructions.last().expect("should have at least one instruction");
			assert!(matches!(last_instruction, SetTopic(_)), "Last instruction should be SetTopic");

			let mut actual_claimer: Option<Location> = None;
			for instruction in &instructions {
				if let SetHints { ref hints } = instruction {
					if let Some(AssetClaimer { location }) = hints.clone().into_iter().next() {
						actual_claimer = Some(location);
						break;
					}
				}
			}

			// actual claimer should default to Snowbridge sovereign account
			// pinned to the local network, so the location remains unambiguous if
			// it is reanchored or forwarded across consensus systems.
			let bridge_owner = ExternalConsensusLocationsConverterFor::<
				AssetHubUniversal<LocalNetwork, AssetHubParaId>,
				[u8; 32],
			>::convert_location(&Location::new(
				2,
				[GlobalConsensus(EthereumNetwork::get())],
			))
			.unwrap();
			assert_eq!(
				actual_claimer,
				Some(Location::new(
					0,
					[AccountId32 { network: Some(LocalNetwork::get()), id: bridge_owner }]
				))
			);
		});
	}

	#[test]
	fn test_missing_claimer_defaults_to_bridge_owner_on_local_network() {
		sp_io::TestExternalities::default().execute_with(|| {
			let origin: H160 = hex!("29e3b139f4393adda86303fcdaa35f60bb7092bf").into();
			let native_token_id: H160 = hex!("5615deb798bb3e4dfa0139dfa1b3d433cc23b72f").into();
			let beneficiary =
				hex!("908783d8cd24c9e02cee1d26ab9c46d458621ad0150b626c536a40b9df3f09c6").into();
			let token_value = 3_000_000_000_000u128;
			let assets = vec![EthereumAsset::NativeTokenERC20 {
				token_id: native_token_id,
				value: token_value,
			}];
			let instructions =
				vec![DepositAsset { assets: Wild(AllCounted(1).into()), beneficiary }];
			let xcm: Xcm<()> = instructions.into();
			let versioned_xcm = VersionedXcm::V5(xcm);

			let message = Message {
				gateway: H160::zero(),
				nonce: 0,
				origin,
				assets,
				payload: Payload::Raw(versioned_xcm.encode()),
				// No claimer supplied — fallback path should be used.
				claimer: None,
				value: 6_000_000_000_000u128,
				execution_fee: 1_000_000_000_000u128,
				relayer_fee: 5_000_000_000_000u128,
			};

			let xcm = Converter::convert(message).expect("conversion succeeds");

			let bridge_owner = ExternalConsensusLocationsConverterFor::<
				AssetHubUniversal<LocalNetwork, AssetHubParaId>,
				[u8; 32],
			>::convert_location(&Location::new(
				2,
				[GlobalConsensus(EthereumNetwork::get())],
			))
			.unwrap();
			let expected_claimer = Location::new(
				0,
				[AccountId32 { network: Some(LocalNetwork::get()), id: bridge_owner }],
			);

			let claimer = xcm
				.into_iter()
				.find_map(|instruction| match instruction {
					SetHints { hints } => hints
						.into_iter()
						.map(|hint| match hint {
							AssetClaimer { location } => location,
						})
						.next(),
					_ => None,
				})
				.expect("AssetClaimer hint should be present");

			assert_eq!(claimer, expected_claimer);
		});
	}

	#[test]
	fn test_invalid_xcm() {
		sp_io::TestExternalities::default().execute_with(|| {
			let origin: H160 = hex!("29e3b139f4393adda86303fcdaa35f60bb7092bf").into();
			let native_token_id: H160 = hex!("5615deb798bb3e4dfa0139dfa1b3d433cc23b72f").into();
			let token_value = 3_000_000_000_000u128;
			let assets = vec![EthereumAsset::NativeTokenERC20 {
				token_id: native_token_id,
				value: token_value,
			}];
			// invalid xcm
			let versioned_xcm = hex!("8b69c7e376e28114618e829a7ec7").to_vec();
			let claimer_account = AccountId32 { network: None, id: H256::random().into() };
			let claimer: Option<Vec<u8>> = Some(claimer_account.clone().encode());
			let value = 6_000_000_000_000u128;
			let execution_fee = 1_000_000_000_000u128;
			let relayer_fee = 5_000_000_000_000u128;

			let message = Message {
				gateway: H160::zero(),
				nonce: 0,
				origin,
				assets,
				payload: Payload::Raw(versioned_xcm),
				claimer: Some(claimer.encode()),
				value,
				execution_fee,
				relayer_fee,
			};

			let result = Converter::convert(message);

			// Invalid xcm does not break the message, allowing funds to be trapped on AH.
			assert_ok!(result.clone());
		});
	}

	#[test]
	fn message_with_set_topic_respects_user_topic() {
		sp_io::TestExternalities::default().execute_with(|| {
			let origin: H160 = hex!("29e3b139f4393adda86303fcdaa35f60bb7092bf").into();

			// Create a custom topic ID that the user specifies
			let user_topic: [u8; 32] =
				hex!("1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef");

			// User's XCM with a SetTopic as the last instruction
			let instructions = vec![RefundSurplus, SetTopic(user_topic)];
			let xcm: Xcm<()> = instructions.into();
			let versioned_xcm = VersionedXcm::V5(xcm);

			let execution_fee = 1_000_000_000_000u128;
			let value = 0;

			let message = Message {
				gateway: H160::zero(),
				nonce: 0,
				origin,
				assets: vec![],
				payload: Payload::Raw(versioned_xcm.encode()),
				claimer: None,
				value,
				execution_fee,
				relayer_fee: 0,
			};

			let result = Converter::convert(message);
			assert_ok!(result.clone());

			let xcm = result.unwrap();
			let instructions: Vec<_> = xcm.into_iter().collect();

			// The last instruction should be the user's SetTopic
			let last_instruction =
				instructions.last().expect("should have at least one instruction");
			if let SetTopic(ref topic) = last_instruction {
				assert_eq!(*topic, user_topic);
			} else {
				panic!("Last instruction should be SetTopic");
			}
		});
	}

	#[test]
	fn message_with_generates_a_unique_topic_if_no_topic_is_present() {
		sp_io::TestExternalities::default().execute_with(|| {
			let origin: H160 = hex!("29e3b139f4393adda86303fcdaa35f60bb7092bf").into();

			let execution_fee = 1_000_000_000_000u128;
			let value = 0;

			let message = Message {
				gateway: H160::zero(),
				nonce: 0,
				origin,
				assets: vec![],
				payload: Payload::Raw(vec![]),
				claimer: None,
				value,
				execution_fee,
				relayer_fee: 0,
			};

			let result = Converter::convert(message);
			assert_ok!(result.clone());

			let xcm = result.unwrap();
			let instructions: Vec<_> = xcm.into_iter().collect();

			// The last instruction should be a SetTopic
			let last_instruction =
				instructions.last().expect("should have at least one instruction");
			assert!(matches!(last_instruction, SetTopic(_)));
		});
	}

	#[test]
	fn message_with_user_topic_not_last_instruction_gets_appended() {
		sp_io::TestExternalities::default().execute_with(|| {
			let origin: H160 = hex!("29e3b139f4393adda86303fcdaa35f60bb7092bf").into();

			let execution_fee = 1_000_000_000_000u128;
			let value = 0;

			let user_topic: [u8; 32] =
				hex!("1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef");

			// Add a set topic, but not as the last instruction.
			let instructions = vec![SetTopic(user_topic), RefundSurplus];
			let xcm: Xcm<()> = instructions.into();
			let versioned_xcm = VersionedXcm::V5(xcm);

			let message = Message {
				gateway: H160::zero(),
				nonce: 0,
				origin,
				assets: vec![],
				payload: Payload::Raw(versioned_xcm.encode()),
				claimer: None,
				value,
				execution_fee,
				relayer_fee: 0,
			};

			let result = Converter::convert(message);
			assert_ok!(result.clone());

			let xcm = result.unwrap();
			let instructions: Vec<_> = xcm.into_iter().collect();

			// Get the last instruction - should still be a SetTopic, but might not have the
			// original topic since for non-last-instruction topics, the filter_topic function
			// extracts it during prepare() and then the original value is later lost when we
			// append a new one
			let last_instruction =
				instructions.last().expect("should have at least one instruction");

			// Check if the last instruction is a SetTopic (content isn't important)
			assert!(matches!(last_instruction, SetTopic(_)), "Last instruction should be SetTopic");
		});
	}
}
