// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023 Snowfork <hello@snowfork.com>
// SPDX-FileCopyrightText: 2021-2022 Parity Technologies (UK) Ltd.
#![cfg_attr(not(feature = "std"), no_std)]
pub mod v1;
pub mod v2;
use codec::Encode;
use sp_crypto_hashing::blake2_256;
use sp_std::marker::PhantomData;
use xcm::prelude::{AccountKey20, Ethereum, GlobalConsensus, Location};
use xcm_executor::traits::ConvertLocation;

pub use snowbridge_verification_primitives::*;

/// DEPRECATED in favor of [xcm_builder::ExternalConsensusLocationsConverterFor]
pub struct EthereumLocationsConverterFor<AccountId>(PhantomData<AccountId>);
impl<AccountId> ConvertLocation<AccountId> for EthereumLocationsConverterFor<AccountId>
where
	AccountId: From<[u8; 32]> + Clone,
{
	fn convert_location(location: &Location) -> Option<AccountId> {
		match location.unpack() {
			(2, [GlobalConsensus(Ethereum { chain_id })]) => {
				Some(Self::from_chain_id(chain_id).into())
			},
			(2, [GlobalConsensus(Ethereum { chain_id }), AccountKey20 { network: _, key }]) => {
				Some(Self::from_chain_id_with_key(chain_id, *key).into())
			},
			_ => None,
		}
	}
}

impl<AccountId> EthereumLocationsConverterFor<AccountId> {
	pub fn from_chain_id(chain_id: &u64) -> [u8; 32] {
		(b"ethereum-chain", chain_id).using_encoded(blake2_256)
	}
	pub fn from_chain_id_with_key(chain_id: &u64, key: [u8; 20]) -> [u8; 32] {
		(b"ethereum-chain", chain_id, key).using_encoded(blake2_256)
	}
}

pub type CallIndex = [u8; 2];
