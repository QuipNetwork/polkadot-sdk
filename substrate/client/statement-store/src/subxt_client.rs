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

//! Custom subxt configuration for runtimes interacting with statement-store, used only for tests
//!
//! The runtime uses `VerifyMultiSignature` instead of the standard
//! `VerifySignature` transaction extension, and includes a `RestrictOrigins`
//! extension that encodes as a bool (`false` = 0x00 to disable origin
//! restrictions). These non-standard extensions cannot be auto-defaulted by
//! frame-decode, so this module provides a `CustomConfig` that handles them
//! explicitly.

use scale_info::PortableRegistry;
use sp_core::{sr25519, Pair};
use subxt::{
	config::{
		substrate::SubstrateConfig,
		transaction_extensions::{
			ChargeAssetTxPayment, ChargeTransactionPayment, CheckGenesis, CheckMetadataHash,
			CheckMortality, CheckNonce, CheckSpecVersion, CheckTxVersion, VerifySignature,
		},
		ClientState, Config, DefaultExtrinsicParamsBuilder, TransactionExtension,
		TransactionExtensions,
	},
	dynamic::Value,
	ext::{frame_decode, scale_value::value},
	transactions::Signer,
	utils::H256,
	OnlineClient,
};

/// Wrapper around `VerifySignature` that matches the runtime's `VerifyMultiSignature` name
pub struct VerifyMultiSignature<T: Config>(VerifySignature<T>);

impl<T: Config> frame_decode::extrinsics::TransactionExtension<PortableRegistry>
	for VerifyMultiSignature<T>
{
	const NAME: &str = "VerifyMultiSignature";

	fn encode_value_to(
		&self,
		type_id: u32,
		type_resolver: &PortableRegistry,
		v: &mut Vec<u8>,
	) -> Result<(), frame_decode::extrinsics::TransactionExtensionError> {
		self.0.encode_value_to(type_id, type_resolver, v)
	}

	fn encode_value_for_signer_payload_to(
		&self,
		type_id: u32,
		type_resolver: &PortableRegistry,
		v: &mut Vec<u8>,
	) -> Result<(), frame_decode::extrinsics::TransactionExtensionError> {
		self.0.encode_value_for_signer_payload_to(type_id, type_resolver, v)
	}

	fn encode_implicit_to(
		&self,
		type_id: u32,
		type_resolver: &PortableRegistry,
		v: &mut Vec<u8>,
	) -> Result<(), frame_decode::extrinsics::TransactionExtensionError> {
		self.0.encode_implicit_to(type_id, type_resolver, v)
	}
}

impl<T: Config> TransactionExtension<T> for VerifyMultiSignature<T> {
	type Decoded = <VerifySignature<T> as TransactionExtension<T>>::Decoded;
	type Params = ();

	fn new(
		client: &ClientState<T>,
		params: Self::Params,
	) -> Result<Self, subxt::error::TransactionExtensionError> {
		Ok(VerifyMultiSignature(VerifySignature::new(client, params)?))
	}

	fn inject_signature(&mut self, account_id: &T::AccountId, signature: &T::Signature) {
		self.0.inject_signature(account_id, signature);
	}
}

/// Custom transaction extension for `RestrictOrigins`
///
/// This extension encodes as `false` (0x00) to disable origin restrictions
/// It is a `bool` in the runtime (not `Option<T>`), so frame-decode cannot
/// auto-default it and it must be handled explicitly
pub struct RestrictOrigins;

impl frame_decode::extrinsics::TransactionExtension<PortableRegistry> for RestrictOrigins {
	const NAME: &str = "RestrictOrigins";

	fn encode_value_to(
		&self,
		_type_id: u32,
		_type_resolver: &PortableRegistry,
		v: &mut Vec<u8>,
	) -> Result<(), frame_decode::extrinsics::TransactionExtensionError> {
		// Encode `false` disables origin restriction
		v.push(0x00);
		Ok(())
	}

	fn encode_implicit_to(
		&self,
		_type_id: u32,
		_type_resolver: &PortableRegistry,
		_v: &mut Vec<u8>,
	) -> Result<(), frame_decode::extrinsics::TransactionExtensionError> {
		Ok(())
	}
}

impl<T: Config> TransactionExtension<T> for RestrictOrigins {
	type Decoded = u8;
	type Params = ();

	fn new(
		_client: &ClientState<T>,
		_params: Self::Params,
	) -> Result<Self, subxt::error::TransactionExtensionError> {
		Ok(RestrictOrigins)
	}
}

/// Custom subxt `Config`
///
/// Registers the non-standard `VerifyMultiSignature` and `RestrictOrigins`
/// transaction extensions so that subxt can correctly encode extrinsics
#[derive(Debug, Clone)]
pub struct CustomConfig(SubstrateConfig);

impl Default for CustomConfig {
	fn default() -> Self {
		CustomConfig(SubstrateConfig::new())
	}
}

impl Config for CustomConfig {
	type AccountId = <SubstrateConfig as Config>::AccountId;
	type Address = subxt::utils::MultiAddress<Self::AccountId, ()>;
	type Signature = <SubstrateConfig as Config>::Signature;
	type Hasher = <SubstrateConfig as Config>::Hasher;
	type Header = <SubstrateConfig as Config>::Header;
	type AssetId = <SubstrateConfig as Config>::AssetId;
	type TransactionExtensions = (
		VerifyMultiSignature<Self>,
		CheckSpecVersion,
		CheckTxVersion,
		CheckNonce,
		CheckGenesis<Self>,
		CheckMortality<Self>,
		ChargeAssetTxPayment<Self>,
		ChargeTransactionPayment,
		CheckMetadataHash,
		RestrictOrigins,
	);

	fn genesis_hash(&self) -> Option<subxt::config::HashFor<Self>> {
		self.0.genesis_hash()
	}

	fn spec_and_transaction_version_for_block_number(
		&self,
		block_number: u64,
	) -> Option<(u32, u32)> {
		self.0.spec_and_transaction_version_for_block_number(block_number)
	}

	fn metadata_for_spec_version(&self, spec_version: u32) -> Option<subxt::metadata::ArcMetadata> {
		self.0.metadata_for_spec_version(spec_version)
	}

	fn set_metadata_for_spec_version(
		&self,
		spec_version: u32,
		metadata: subxt::metadata::ArcMetadata,
	) {
		self.0.set_metadata_for_spec_version(spec_version, metadata)
	}
}

/// Builds params for CustomConfig's transaction extensions (9 defaults + RestrictOrigins)
fn build_params(
	nonce: u64,
) -> <<CustomConfig as Config>::TransactionExtensions as TransactionExtensions<CustomConfig>>::Params
{
	let (a, b, c, d, e, f, g, h, i) = DefaultExtrinsicParamsBuilder::<CustomConfig>::new()
		.immortal()
		.nonce(nonce)
		.build();
	(a, b, c, d, e, f, g, h, i, ())
}

/// Submits an extrinsic with an explicit nonce and waits for it to be finalized
pub async fn submit_extrinsic<S: Signer<CustomConfig>>(
	client: &OnlineClient<CustomConfig>,
	call: &subxt::transactions::DynamicPayload<Vec<Value>>,
	signer: &S,
	nonce: u64,
) -> Result<H256, anyhow::Error> {
	let tx_in_block = client
		.tx()
		.await?
		.sign_and_submit_then_watch(call, signer, build_params(nonce))
		.await?
		.wait_for_finalized()
		.await?;

	tx_in_block.wait_for_success().await?;
	Ok(tx_in_block.block_hash())
}

/// Gets the current nonce for an account
pub async fn get_account_nonce(
	client: &OnlineClient<CustomConfig>,
	account_id: &<CustomConfig as Config>::AccountId,
) -> Result<u64, anyhow::Error> {
	let nonce = client.tx().await?.account_nonce(account_id).await?;
	Ok(nonce)
}

/// Matches `indiv_pallet_people_lite::MSG_PREFIX`
pub const MSG_PREFIX: &[u8; 30] = b"pop:people-lite:register using";

/// Builds a sudo call wrapping `PeopleLite::increase_attestation_allowance`
pub fn create_increase_allowance_call(
	who: Vec<u8>,
	count: u32,
) -> subxt::transactions::DynamicPayload<Vec<Value>> {
	subxt::tx::dynamic(
		"Sudo",
		"sudo",
		vec![value! {
			PeopleLite(increase_attestation_allowance {
				account: Value::from_bytes(who),
				count: Value::u128(count as u128),
			})
		}],
	)
}

/// Builds a `PeopleLite::attest` call
pub fn create_attest_call(
	candidate: Vec<u8>,
	candidate_signature: Vec<u8>,
	ring_vrf_key: Vec<u8>,
	proof_of_ownership: Vec<u8>,
	consumer_registration: Option<Value>,
) -> subxt::transactions::DynamicPayload<Vec<Value>> {
	let reg = match consumer_registration {
		Some(v) => Value::unnamed_variant("Some", [v]),
		None => Value::unnamed_variant("None", []),
	};
	subxt::tx::dynamic(
		"PeopleLite",
		"attest",
		vec![
			Value::from_bytes(candidate),
			Value::unnamed_variant("Sr25519", [Value::from_bytes(candidate_signature)]),
			Value::from_bytes(ring_vrf_key),
			Value::from_bytes(proof_of_ownership),
			reg,
		],
	)
}

/// Builds consumer registration parameters for `create_attest_call`
///
/// The `LiteConsumerRegistrationParams` struct has fields:
///   signature: `MultiSignature`, account: `AccountId`,
///   identifier_key: `CommunicationIdentifier` (`[u8; 65]`),
///   username: `Username` (`BoundedVec<u8, 32>`), reserved_username: `Option<Username>`
///
/// The signing payload is `(account, verifier, identifier_key, username_prefix,
/// reserved_username).encode()` where `verifier` is the attester (origin of the attest call)
pub fn create_consumer_registration_params(
	consumer_pair: &sr25519::Pair,
	consumer_account: &[u8; 32],
	verifier_account: &[u8; 32],
) -> Value {
	use sp_core::Encode;

	let identifier_key = [0u8; 65];
	let username = b"testuser.00";
	let reserved_username: Option<Vec<u8>> = None;

	// Build SCALE-encoded signing payload matching LiteConsumerRegistrationParams::signing_payload:
	// (account, verifier, identifier_key, username[..separator], reserved_username).encode()
	// The username prefix is the part before the '.' separator
	let separator_idx = username.iter().position(|b| *b == b'.').unwrap_or(username.len());
	let username_prefix = &username[..separator_idx];
	let payload = (
		consumer_account,
		verifier_account,
		&identifier_key,
		&username_prefix.to_vec(),
		&reserved_username,
	)
		.encode();

	let sig = consumer_pair.sign(&payload);
	value! {
		{
			signature: Value::unnamed_variant("Sr25519", [Value::from_bytes(sig.0.to_vec())]),
			account: Value::from_bytes(consumer_account.to_vec()),
			identifier_key: Value::from_bytes(identifier_key.to_vec()),
			username: Value::from_bytes(username.to_vec()),
			reserved_username: Value::unnamed_variant("None", []),
		}
	}
}

/// Sets statement allowances at runtime via a sudo extrinsic signed by Alice
pub async fn set_allowances_via_sudo(
	ws_uri: &str,
	items: Vec<(Vec<u8>, Vec<u8>)>,
) -> Result<(), anyhow::Error> {
	log::info!("Setting {} statement allowances via sudo...", items.len());

	let client = OnlineClient::<CustomConfig>::from_insecure_url_with_config(
		CustomConfig::default(),
		ws_uri,
	)
	.await?;
	let alice = subxt_signer::sr25519::dev::alice();

	let items_value: Vec<Value> = items
		.into_iter()
		.map(|(key, value)| value!((Value::from_bytes(key), Value::from_bytes(value))))
		.collect();
	let call = subxt::tx::dynamic(
		"Sudo",
		"sudo",
		vec![value! {
			System(set_storage { items: items_value })
		}],
	);

	client
		.tx()
		.await?
		.sign_and_submit_then_watch_default(&call, &alice)
		.await?
		.wait_for_finalized_success()
		.await?;

	Ok(())
}
