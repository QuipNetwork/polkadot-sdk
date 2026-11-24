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

//! subcommand utilities
use crate::{
	error::{self, Error},
	OutputType,
};
use serde_json::json;
use sp_core::{
	crypto::{
		unwrap_or_default_ss58_version, ExposeSecret, SecretString, SecretStringError,
		Ss58AddressFormat, Ss58Codec, Zeroize,
	},
	hexdisplay::HexDisplay,
	Pair,
};
use sp_runtime::{traits::IdentifyAccount, MultiSigner};
use std::path::PathBuf;

/// Public key type for Runtime
pub type PublicFor<P> = <P as sp_core::Pair>::Public;
/// Seed type for Runtime
pub type SeedFor<P> = <P as sp_core::Pair>::Seed;

/// helper method to fetch uri from `Option<String>` either as a file or read from stdin
pub fn read_uri(uri: Option<&String>) -> error::Result<String> {
	let uri = if let Some(uri) = uri {
		let file = PathBuf::from(&uri);
		if file.is_file() {
			std::fs::read_to_string(uri)?.trim_end().to_owned()
		} else {
			uri.into()
		}
	} else {
		rpassword::prompt_password("URI: ")?
	};

	Ok(uri)
}

/// Try to parse given `uri` and print relevant information.
///
/// 1. Try to construct the `Pair` while using `uri` as input for [`sp_core::Pair::from_phrase`].
///
/// 2. Try to construct the `Pair` while using `uri` as input for
/// [`sp_core::Pair::from_string_with_seed`].
///
/// 3. Try to construct the `Pair::Public` while using `uri` as input for
///    [`sp_core::crypto::Ss58Codec::from_string_with_version`].
pub fn print_from_uri<Pair>(
	uri: &str,
	password: Option<SecretString>,
	network_override: Option<Ss58AddressFormat>,
	output: OutputType,
) where
	Pair: sp_core::Pair,
	Pair::Public: Into<MultiSigner>,
{
	let password = password.as_ref().map(|s| s.expose_secret().as_str());
	let network_id = String::from(unwrap_or_default_ss58_version(network_override));
	if let Ok((pair, seed)) = Pair::from_phrase(uri, password) {
		let public_key = pair.public();
		let network_override = unwrap_or_default_ss58_version(network_override);

		match output {
			OutputType::Json => {
				let json = json!({
					"secretPhrase": uri,
					"networkId": network_id,
					"secretSeed": format_seed::<Pair>(seed),
					"publicKey": format_public_key::<Pair>(public_key.clone()),
					"ss58PublicKey": public_key.to_ss58check_with_version(network_override),
					"accountId": format_account_id::<Pair>(public_key),
					"ss58Address": pair.public().into().into_account().to_ss58check_with_version(network_override),
				});
				println!(
					"{}",
					serde_json::to_string_pretty(&json).expect("Json pretty print failed")
				);
			},
			OutputType::Text => {
				println!(
					"Secret phrase:       {}\n  \
					Network ID:        {}\n  \
					Secret seed:       {}\n  \
					Public key (hex):  {}\n  \
					Account ID:        {}\n  \
					Public key (SS58): {}\n  \
					SS58 Address:      {}",
					uri,
					network_id,
					format_seed::<Pair>(seed),
					format_public_key::<Pair>(public_key.clone()),
					format_account_id::<Pair>(public_key.clone()),
					public_key.to_ss58check_with_version(network_override),
					pair.public().into().into_account().to_ss58check_with_version(network_override),
				);
			},
		}
	} else if let Ok((pair, seed)) = Pair::from_string_with_seed(uri, password) {
		let public_key = pair.public();
		let network_override = unwrap_or_default_ss58_version(network_override);

		match output {
			OutputType::Json => {
				let json = json!({
					"secretKeyUri": uri,
					"networkId": network_id,
					"secretSeed": if let Some(seed) = seed { format_seed::<Pair>(seed) } else { "n/a".into() },
					"publicKey": format_public_key::<Pair>(public_key.clone()),
					"ss58PublicKey": public_key.to_ss58check_with_version(network_override),
					"accountId": format_account_id::<Pair>(public_key),
					"ss58Address": pair.public().into().into_account().to_ss58check_with_version(network_override),
				});
				println!(
					"{}",
					serde_json::to_string_pretty(&json).expect("Json pretty print failed")
				);
			},
			OutputType::Text => {
				println!(
					"Secret Key URI `{}` is account:\n  \
					Network ID:        {}\n  \
					Secret seed:       {}\n  \
					Public key (hex):  {}\n  \
					Account ID:        {}\n  \
					Public key (SS58): {}\n  \
					SS58 Address:      {}",
					uri,
					network_id,
					if let Some(seed) = seed { format_seed::<Pair>(seed) } else { "n/a".into() },
					format_public_key::<Pair>(public_key.clone()),
					format_account_id::<Pair>(public_key.clone()),
					public_key.to_ss58check_with_version(network_override),
					pair.public().into().into_account().to_ss58check_with_version(network_override),
				);
			},
		}
	} else if let Ok((public_key, network)) = Pair::Public::from_string_with_version(uri) {
		let network_override = network_override.unwrap_or(network);

		match output {
			OutputType::Json => {
				let json = json!({
					"publicKeyUri": uri,
					"networkId": String::from(network_override),
					"publicKey": format_public_key::<Pair>(public_key.clone()),
					"accountId": format_account_id::<Pair>(public_key.clone()),
					"ss58PublicKey": public_key.to_ss58check_with_version(network_override),
					"ss58Address": public_key.to_ss58check_with_version(network_override),
				});

				println!(
					"{}",
					serde_json::to_string_pretty(&json).expect("Json pretty print failed")
				);
			},
			OutputType::Text => {
				println!(
					"Public Key URI `{}` is account:\n  \
					 Network ID/Version: {}\n  \
					 Public key (hex):   {}\n  \
					 Account ID:         {}\n  \
					 Public key (SS58):  {}\n  \
					 SS58 Address:       {}",
					uri,
					String::from(network_override),
					format_public_key::<Pair>(public_key.clone()),
					format_account_id::<Pair>(public_key.clone()),
					public_key.to_ss58check_with_version(network_override),
					public_key.to_ss58check_with_version(network_override),
				);
			},
		}
	} else {
		println!("Invalid phrase/URI given");
	}
}

/// Try to parse given `public` as hex encoded public key and print relevant information.
pub fn print_from_public<Pair>(
	public_str: &str,
	network_override: Option<Ss58AddressFormat>,
	output: OutputType,
) -> Result<(), Error>
where
	Pair: sp_core::Pair,
	Pair::Public: Into<MultiSigner>,
{
	let public = array_bytes::hex2bytes(public_str)?;

	let public_key = Pair::Public::try_from(&public)
		.map_err(|_| "Failed to construct public key from given hex")?;

	let network_override = unwrap_or_default_ss58_version(network_override);

	match output {
		OutputType::Json => {
			let json = json!({
				"networkId": String::from(network_override),
				"publicKey": format_public_key::<Pair>(public_key.clone()),
				"accountId": format_account_id::<Pair>(public_key.clone()),
				"ss58PublicKey": public_key.to_ss58check_with_version(network_override),
				"ss58Address": public_key.to_ss58check_with_version(network_override),
			});

			println!("{}", serde_json::to_string_pretty(&json).expect("Json pretty print failed"));
		},
		OutputType::Text => {
			println!(
				"Network ID/Version: {}\n  \
				 Public key (hex):   {}\n  \
				 Account ID:         {}\n  \
				 Public key (SS58):  {}\n  \
				 SS58 Address:       {}",
				String::from(network_override),
				format_public_key::<Pair>(public_key.clone()),
				format_account_id::<Pair>(public_key.clone()),
				public_key.to_ss58check_with_version(network_override),
				public_key.to_ss58check_with_version(network_override),
			);
		},
	}

	Ok(())
}

/// Why a secret URI was rejected, in fixed words that never include it.
///
/// `SecretStringError::InvalidFormat` carries `AddressUriError`, whose
/// `InvalidCharacterInfo` holds a full copy of the input, so the error's
/// `Debug` and `Display` print the mnemonic and the password. Only the
/// variant is used here.
pub(crate) fn secret_uri_error(error: &SecretStringError) -> String {
	let reason = match error {
		SecretStringError::InvalidFormat(_) => {
			"it contains a character that is not allowed (look for quotes, a byte-order mark or \
			 a line break)"
		},
		SecretStringError::InvalidPhrase => "it is not a valid BIP-39 mnemonic",
		SecretStringError::InvalidPassword => "the password is not valid",
		SecretStringError::InvalidSeed => "the seed is not valid",
		SecretStringError::InvalidSeedLength => "the seed has the wrong length",
		SecretStringError::InvalidPath => "the derivation path is not valid",
	};
	format!("Invalid secret URI: {reason}; the secret URI is not shown")
}

/// generate a pair from suri
pub fn pair_from_suri<P: Pair>(suri: &str, password: Option<SecretString>) -> Result<P, Error> {
	let result = if let Some(pass) = password {
		let mut pass_str = pass.expose_secret().clone();
		let pair = P::from_string(suri, Some(&pass_str));
		pass_str.zeroize();
		pair
	} else {
		P::from_string(suri, None)
	};

	Ok(result.map_err(|err| secret_uri_error(&err))?)
}

/// formats seed as hex
pub fn format_seed<P: sp_core::Pair>(seed: SeedFor<P>) -> String {
	format!("0x{}", HexDisplay::from(&seed.as_ref()))
}

/// formats public key as hex
fn format_public_key<P: sp_core::Pair>(public_key: PublicFor<P>) -> String {
	format!("0x{}", HexDisplay::from(&public_key.as_ref()))
}

/// formats public key as accountId as hex
fn format_account_id<P: sp_core::Pair>(public_key: PublicFor<P>) -> String
where
	PublicFor<P>: Into<MultiSigner>,
{
	format!("0x{}", HexDisplay::from(&public_key.into().into_account().as_ref()))
}

/// Allows for calling $method with appropriate crypto impl.
#[macro_export]
macro_rules! with_crypto_scheme {
	(
		$scheme:expr,
		$method:ident ( $($params:expr),* $(,)?) $(,)?
	) => {
		$crate::with_crypto_scheme!($scheme, $method<>($($params),*))
	};
	(
		$scheme:expr,
		$method:ident<$($generics:ty),*>( $( $params:expr ),* $(,)?) $(,)?
	) => {
		match $scheme {
			$crate::CryptoScheme::Ecdsa => {
				$method::<sp_core::ecdsa::Pair, $($generics),*>($($params),*)
			}
			$crate::CryptoScheme::Sr25519 => {
				$method::<sp_core::sr25519::Pair, $($generics),*>($($params),*)
			}
			$crate::CryptoScheme::Ed25519 => {
				$method::<sp_core::ed25519::Pair, $($generics),*>($($params),*)
			}
		}
	};
}

#[cfg(test)]
pub(crate) mod tests_support {
	/// sp-core's `DEV_PHRASE`, standing in for a secret.
	pub(crate) const PHRASE: &str =
		"bottom drive obey lake curtain smoke basket hold race lonely fit walk";
	pub(crate) const PASSWORD: &str = "Tr0ub4dor-hunter2";

	/// The secret fragments that appear in `text`.
	pub(crate) fn echoed(text: &str) -> Vec<&'static str> {
		let text = text.to_lowercase();
		PHRASE
			.split(' ')
			.filter(|w| w.len() > 3)
			.chain([PASSWORD])
			.filter(|secret| text.contains(&secret.to_lowercase()))
			.collect()
	}

	/// Secret URIs every scheme rejects. The format errors carry a full copy of
	/// the input inside `SecretStringError`; the bad checksum does not.
	pub(crate) fn rejected_suris() -> Vec<String> {
		vec![
			format!("\"{PHRASE}\""),
			format!("{PHRASE}\n"),
			format!("\u{feff}{PHRASE}"),
			format!("{PHRASE}///{PASSWORD}\nx"),
			"bottom drive obey lake curtain smoke basket hold race lonely fit bottom".to_owned(),
		]
	}
}

#[cfg(test)]
mod tests {
	use super::{tests_support::*, *};

	fn assert_rejected_without_echo<P: Pair>(scheme: &str) {
		for suri in rejected_suris() {
			let Err(error) = pair_from_suri::<P>(&suri, None) else {
				panic!("{scheme}: {suri:?} must be rejected");
			};
			for text in [format!("{error}"), format!("{error:?}")] {
				assert!(text.contains("Invalid secret URI"), "{scheme}: {text}");
				assert!(echoed(&text).is_empty(), "{scheme}: {text} shows {:?}", echoed(&text));
			}
		}
	}

	#[test]
	fn pair_from_suri_does_not_echo_a_rejected_suri() {
		assert_rejected_without_echo::<sp_core::sr25519::Pair>("sr25519");
		assert_rejected_without_echo::<sp_core::ed25519::Pair>("ed25519");
		assert_rejected_without_echo::<sp_core::ecdsa::Pair>("ecdsa");
		assert_rejected_without_echo::<quip_crypto_primitives::substrate::sr25519_fndsa512::Pair>(
			"hybrid-babe-h444",
		);
		assert_rejected_without_echo::<quip_crypto_primitives::substrate::ed25519_fndsa512::Pair>(
			"hybrid-grandpa-h244",
		);
	}

	/// The source of the echo: the error itself carries the input.
	#[test]
	fn a_format_error_carries_the_suri_and_the_message_does_not() {
		let Err(error) = sp_core::sr25519::Pair::from_string(&format!("\"{PHRASE}\""), None) else {
			panic!("a quote is not allowed");
		};
		assert!(!echoed(&format!("{error:?}")).is_empty(), "the SDK error carries the SURI");
		assert!(echoed(&secret_uri_error(&error)).is_empty());
	}
}
