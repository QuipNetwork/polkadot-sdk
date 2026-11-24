//! The Quip Substrate profile of a secret URI (WP2 batch 1 design note v2.2,
//! §3 and §4).
//!
//! The grammar is upstream's: [`AddressUri::parse`] (ASCII phrase class,
//! `//hard` and `/soft` junctions, `///password`). What a SURI selects is
//! not:
//!
//! - **A phrase** (an empty one is [`DEV_PHRASE`]) selects the QUIP HD leaf `m / 0x514E' / suite' /
//!   20049' / account' / index'`. Junctions are levels, not derivations: none is `0/0`, one is
//!   `account`, two are `account/index`. The `///password` is the BIP-39 passphrase.
//! - **`0x` + 64 hex digits** is a raw 32-byte master seed. Each hard junction applies
//!   pqhybridsign's HKDF junction with upstream's `DeriveJunction` chain code. A `///` password
//!   (even an empty one) is an error.
//!
//! Common rules: hard junctions only, at most two. An all-digit junction
//! (`^\+?[0-9]+$`) must be canonical (no `+`, no leading zero) and below
//! `2^31`; any other junction text is a name. `0X` is an error. Every error
//! is a [`SecretStringError`] variant that carries no part of the input:
//! never `InvalidFormat`, whose `AddressUriError` holds a copy of the SURI.

use alloc::vec::Vec;

use codec::Encode;
use sp_core::{
	crypto::{AddressUri, AddressUriError, DeriveJunction, SecretStringError, DEV_PHRASE},
	hashing::blake2_256,
};

use zeroize::Zeroizing;

use crate::MASTER_SEED_LEN;

/// At most this many junctions: `account` and `index`.
const MAX_JUNCTIONS: usize = 2;

/// Levels are hardened, so a level is below `2^31`.
const LEVEL_LIMIT: u32 = 1 << 31;

/// What a secret URI selects.
///
/// No `Debug` outside tests: a derived `Debug` would print the phrase, the
/// passphrase or the raw seed. Only the tests compare and print it.
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub(crate) enum Suri<'a> {
	/// A BIP-39 phrase: the QUIP HD leaf at `account`/`index`.
	Phrase {
		/// The phrase text as written (QUIP HD normalizes it).
		phrase: &'a str,
		/// The SURI's own `///` password, if any.
		passphrase: Option<&'a str>,
		/// The first junction's level, or 0.
		account: u32,
		/// The second junction's level, or 0.
		index: u32,
	},
	/// A raw master seed and the chain codes of its hard junctions.
	Raw {
		/// The 32-byte master seed, wiped on drop.
		seed: Zeroizing<[u8; MASTER_SEED_LEN]>,
		/// Upstream `DeriveJunction` chain codes, in order.
		chain_codes: Vec<[u8; 32]>,
	},
}

/// Parses `s` under the Quip Substrate profile.
pub(crate) fn parse(s: &str) -> Result<Suri<'_>, SecretStringError> {
	let uri = AddressUri::parse(s).map_err(|e| input_free(&e))?;
	let mut junctions = Vec::with_capacity(uri.paths.len());
	for path in &uri.paths {
		// `AddressUri` keeps a hard junction's leading `/`; a soft one has none.
		junctions.push(path.strip_prefix('/').ok_or(SecretStringError::InvalidPath)?);
	}
	if junctions.len() > MAX_JUNCTIONS {
		return Err(SecretStringError::InvalidPath);
	}

	let phrase = uri.phrase.unwrap_or(DEV_PHRASE);
	if let Some(hex) = phrase.strip_prefix("0x") {
		if uri.pass.is_some() {
			return Err(SecretStringError::InvalidPassword);
		}
		let seed = decode_seed(hex)?;
		let chain_codes =
			junctions.iter().map(|t| raw_chain_code(t)).collect::<Result<Vec<_>, _>>()?;
		return Ok(Suri::Raw { seed, chain_codes });
	}
	if phrase.starts_with("0X") {
		return Err(SecretStringError::InvalidSeed);
	}

	let levels = junctions.iter().map(|t| phrase_level(t)).collect::<Result<Vec<_>, _>>()?;
	Ok(Suri::Phrase {
		phrase,
		passphrase: uri.pass,
		account: levels.first().copied().unwrap_or(0),
		index: levels.get(1).copied().unwrap_or(0),
	})
}

/// The input-free variant for an `AddressUri` parse error. The error itself
/// holds a full copy of the SURI, so only its kind is kept.
fn input_free(error: &AddressUriError) -> SecretStringError {
	match error {
		AddressUriError::InvalidCharacterInPhrase(_) => SecretStringError::InvalidPhrase,
		AddressUriError::InvalidCharacterInPass(_) => SecretStringError::InvalidPassword,
		AddressUriError::MissingCharacterInHardPath(_) |
		AddressUriError::MissingCharacterInSoftPath(_) => SecretStringError::InvalidPath,
	}
}

/// The value of an all-digit junction (`^\+?[0-9]+$`), which must be
/// canonical and below `2^31`; `None` for any other text (a name).
fn numeric_level(t: &str) -> Option<Result<u32, SecretStringError>> {
	let digits = t.strip_prefix('+').unwrap_or(t);
	if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
		return None;
	}
	let canonical = digits.len() == t.len() && (digits == "0" || !digits.starts_with('0'));
	Some(match digits.parse::<u32>() {
		Ok(n) if canonical && n < LEVEL_LIMIT => Ok(n),
		_ => Err(SecretStringError::InvalidPath),
	})
}

/// A phrase junction's QUIP HD level: an all-digit junction is its value; a
/// name is `u32::from_le_bytes(blake2_256(SCALE(t))[0..4]) & 0x7FFF_FFFF`.
fn phrase_level(t: &str) -> Result<u32, SecretStringError> {
	numeric_level(t).unwrap_or_else(|| {
		let hash = blake2_256(&t.encode());
		Ok(u32::from_le_bytes([hash[0], hash[1], hash[2], hash[3]]) & (LEVEL_LIMIT - 1))
	})
}

/// A raw-seed junction's chain code: upstream `DeriveJunction`'s, for the
/// strict all-digit value or the name.
fn raw_chain_code(t: &str) -> Result<[u8; 32], SecretStringError> {
	match numeric_level(t) {
		Some(level) => level.map(|n| DeriveJunction::hard(u64::from(n)).unwrap_inner()),
		None => Ok(DeriveJunction::hard(t).unwrap_inner()),
	}
}

/// A raw seed's 64 hex digits (either case), decoded into a wiped buffer.
fn decode_seed(hex: &str) -> Result<Zeroizing<[u8; MASTER_SEED_LEN]>, SecretStringError> {
	if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
		return Err(SecretStringError::InvalidSeed);
	}
	if hex.len() != 2 * MASTER_SEED_LEN {
		return Err(SecretStringError::InvalidSeedLength);
	}
	let mut seed = Zeroizing::new([0u8; MASTER_SEED_LEN]);
	for (byte, pair) in seed.iter_mut().zip(hex.as_bytes().chunks_exact(2)) {
		let digit = |c: u8| match c {
			b'0'..=b'9' => c - b'0',
			b'a'..=b'f' => c - b'a' + 10,
			_ => c - b'A' + 10,
		};
		*byte = (digit(pair[0]) << 4) | digit(pair[1]);
	}
	Ok(seed)
}

#[cfg(test)]
mod tests {
	use super::*;

	const LEGAL: &str =
		"legal winner thank year wave sausage worth useful legal winner thank yellow";

	fn phrase(account: u32, index: u32, passphrase: Option<&'static str>) -> Suri<'static> {
		Suri::Phrase { phrase: LEGAL, passphrase, account, index }
	}

	#[test]
	fn phrase_junctions_are_levels() {
		assert_eq!(parse(LEGAL), Ok(phrase(0, 0, None)));
		assert_eq!(parse(&format!("{LEGAL}//1")), Ok(phrase(1, 0, None)));
		assert_eq!(parse(&format!("{LEGAL}//1//7")), Ok(phrase(1, 7, None)));
		assert_eq!(parse(&format!("{LEGAL}//2147483647")), Ok(phrase(2_147_483_647, 0, None)));
		assert_eq!(parse(&format!("{LEGAL}///pw")), Ok(phrase(0, 0, Some("pw"))));
		assert_eq!(parse(&format!("{LEGAL}///")), Ok(phrase(0, 0, Some(""))));
		assert_eq!(parse(&format!("{LEGAL}///p/w")), Ok(phrase(0, 0, Some("p/w"))));
		// Named levels: design §3 cross-check values.
		assert_eq!(parse(&format!("{LEGAL}//Alice")), Ok(phrase(910_586_124, 0, None)));
		assert_eq!(
			parse(&format!("{LEGAL}//Alice//Bob")),
			Ok(phrase(910_586_124, 1_714_080_267, None))
		);
		assert_eq!(phrase_level("stash"), Ok(1_174_590_175));
		// An empty phrase is the dev phrase.
		assert_eq!(
			parse("//Alice"),
			Ok(Suri::Phrase {
				phrase: DEV_PHRASE,
				passphrase: None,
				account: 910_586_124,
				index: 0
			})
		);
	}

	#[test]
	fn raw_seeds_take_chain_codes_and_no_password() {
		let hex = "2a".repeat(32);
		assert_eq!(
			parse(&format!("0x{hex}")),
			Ok(Suri::Raw { seed: Zeroizing::new([0x2a; 32]), chain_codes: vec![] })
		);
		assert_eq!(
			parse(&format!("0x{}", hex.to_uppercase())),
			Ok(Suri::Raw { seed: Zeroizing::new([0x2a; 32]), chain_codes: vec![] })
		);
		assert_eq!(
			parse(&format!("0x{hex}//1//Alice")),
			Ok(Suri::Raw {
				seed: Zeroizing::new([0x2a; 32]),
				chain_codes: vec![
					DeriveJunction::hard(1u64).unwrap_inner(),
					DeriveJunction::hard("Alice").unwrap_inner()
				],
			})
		);
		assert_eq!(parse(&format!("0x{hex}///")), Err(SecretStringError::InvalidPassword));
		assert_eq!(parse(&format!("0x{hex}///pw")), Err(SecretStringError::InvalidPassword));
		assert_eq!(parse(&format!("0X{hex}")), Err(SecretStringError::InvalidSeed));
		assert_eq!(parse(&format!("0x{}", &hex[..62])), Err(SecretStringError::InvalidSeedLength));
	}

	#[test]
	fn strict_digits_and_junction_limits() {
		let hex = "2a".repeat(32);
		for bad in ["01", "00", "+1", "+0", "2147483648", "4294967296", "99999999999999999999"] {
			assert_eq!(
				parse(&format!("{LEGAL}//{bad}")),
				Err(SecretStringError::InvalidPath),
				"{bad}"
			);
			assert_eq!(
				parse(&format!("0x{hex}//{bad}")),
				Err(SecretStringError::InvalidPath),
				"{bad}"
			);
		}
		assert_eq!(parse(&format!("{LEGAL}/soft")), Err(SecretStringError::InvalidPath));
		assert_eq!(parse(&format!("0x{hex}/1")), Err(SecretStringError::InvalidPath));
		assert_eq!(parse(&format!("{LEGAL}//1//2//3")), Err(SecretStringError::InvalidPath));
		assert_eq!(parse(&format!("{LEGAL}//")), Err(SecretStringError::InvalidPath));
	}

	/// Design v2.1: parse errors carry no part of the SURI.
	#[test]
	fn parse_errors_are_input_free() {
		let secrets = ["legal", "winner", "yellow", "Tr0ub4dor"];
		for suri in [
			format!("\"{LEGAL}\""),
			format!("{LEGAL}\n"),
			format!("{LEGAL}///Tr0ub4dor\nx"),
			format!("{LEGAL}\t//1"),
		] {
			let error = parse(&suri).expect_err("malformed");
			assert!(!matches!(error, SecretStringError::InvalidFormat(_)), "{error:?}");
			for text in [format!("{error}"), format!("{error:?}")] {
				for secret in secrets {
					assert!(!text.contains(secret), "{text} shows {secret}");
				}
			}
		}
	}
}
