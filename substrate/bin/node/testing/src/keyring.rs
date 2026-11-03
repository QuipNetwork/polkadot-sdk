// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! Test accounts.

use codec::Encode;
use kitchensink_runtime::{CheckedExtrinsic, SessionKeys, TxExtension, UncheckedExtrinsic};
use node_primitives::{AccountId, Balance, Nonce};
use sp_core::{crypto::get_public_from_string_or_panic, ecdsa, ed25519, sr25519};
use sp_crypto_hashing::blake2_256;
use sp_keyring::Sr25519Keyring;
use sp_runtime::generic::{self, Era, ExtrinsicFormat};

/// Alice's account id.
pub fn alice() -> AccountId {
	Sr25519Keyring::Alice.into()
}

/// Bob's account id.
pub fn bob() -> AccountId {
	Sr25519Keyring::Bob.into()
}

/// Charlie's account id.
pub fn charlie() -> AccountId {
	Sr25519Keyring::Charlie.into()
}

/// Dave's account id.
pub fn dave() -> AccountId {
	Sr25519Keyring::Dave.into()
}

/// Eve's account id.
pub fn eve() -> AccountId {
	Sr25519Keyring::Eve.into()
}

/// Ferdie's account id.
pub fn ferdie() -> AccountId {
	Sr25519Keyring::Ferdie.into()
}

/// Convert keyrings into `SessionKeys`.
///
/// # Panics
///
/// Function will panic when invalid string is provided.
pub fn session_keys_from_seed(seed: &str) -> SessionKeys {
	SessionKeys {
		grandpa: get_public_from_string_or_panic::<ed25519::Public>(seed).into(),
		babe: get_public_from_string_or_panic::<sr25519::Public>(seed).into(),
		im_online: get_public_from_string_or_panic::<sr25519::Public>(seed).into(),
		authority_discovery: get_public_from_string_or_panic::<sr25519::Public>(seed).into(),
		mixnet: get_public_from_string_or_panic::<sr25519::Public>(seed).into(),
		beefy: get_public_from_string_or_panic::<ecdsa::Public>(seed).into(),
	}
}

/// Returns transaction extra.
pub fn tx_ext(nonce: Nonce, extra_fee: Balance) -> TxExtension {
	(
		frame_system::AuthorizeCall::new(),
		frame_system::CheckNonZeroSender::new(),
		frame_system::CheckSpecVersion::new(),
		frame_system::CheckTxVersion::new(),
		frame_system::CheckGenesis::new(),
		frame_system::CheckEra::from(Era::mortal(256, 0)),
		frame_system::CheckNonce::from(nonce),
		frame_system::CheckWeight::new(),
		pallet_skip_feeless_payment::SkipCheckIfFeeless::from(
			pallet_asset_conversion_tx_payment::ChargeAssetTxPayment::from(extra_fee, None),
		),
		frame_metadata_hash_extension::CheckMetadataHash::new(false),
		pallet_revive::evm::tx_extension::SetOrigin::default(),
		frame_system::WeightReclaim::new(),
	)
}

/// Sign given `CheckedExtrinsic`.
pub fn sign(
	xt: CheckedExtrinsic,
	spec_version: u32,
	tx_version: u32,
	genesis_hash: [u8; 32],
	metadata_hash: Option<[u8; 32]>,
) -> UncheckedExtrinsic {
	match xt.format {
		ExtrinsicFormat::Signed(signed, tx_ext) => {
			let payload = (
				xt.function,
				tx_ext.clone(),
				spec_version,
				tx_version,
				genesis_hash,
				genesis_hash,
				metadata_hash,
			);
			let key = Sr25519Keyring::from_account_id(&signed).unwrap();
			let signature =
				payload
					.using_encoded(|b| {
						if b.len() > 256 {
							key.sign(&blake2_256(b))
						} else {
							key.sign(b)
						}
					})
					.into();
			generic::UncheckedExtrinsic::new_signed(
				payload.0,
				sp_runtime::MultiAddress::Id(signed),
				signature,
				tx_ext,
			)
			.into()
		},
		ExtrinsicFormat::Bare => generic::UncheckedExtrinsic::new_bare(xt.function).into(),
		ExtrinsicFormat::General(tx_ext) => {
			generic::UncheckedExtrinsic::from_parts(xt.function, generic::Preamble::General(tx_ext))
				.into()
		},
	}
}
