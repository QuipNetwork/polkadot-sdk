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

//! WebRTC DTLS cert/key generation from the node key, and the public addresses that advertise
//! the certificate's hash.

use crate::error::Error;
use hmac::{Hmac, Mac};
use p256::{
	ecdsa::{
		signature::{hazmat::PrehashSigner, SignatureEncoding},
		DerSignature, SigningKey,
	},
	elliptic_curve::{sec1::ToEncodedPoint, Curve},
	pkcs8::EncodePrivateKey,
	EncodedPoint, NistP256, U256,
};
use sha2::{Digest, Sha256};
use x509_cert::{
	attr::AttributeTypeAndValue,
	der::{
		asn1::{Any, BitString, GeneralizedTime, SetOfVec, UtcTime},
		oid::db::{
			rfc4519::COMMON_NAME,
			rfc5912::{ECDSA_WITH_SHA_256, ID_EC_PUBLIC_KEY, SECP_256_R_1},
		},
		DateTime, Encode as _, Tag,
	},
	name::{Name, RelativeDistinguishedName},
	serial_number::SerialNumber,
	spki::{AlgorithmIdentifierOwned, SubjectPublicKeyInfoOwned},
	time::{Time, Validity},
	Certificate, TbsCertificate, Version,
};

use litep2p::{crypto::ed25519::SecretKey as Ed25519SecretKey, transport::webrtc::DtlsCertificate};
use sc_network_types::{
	multiaddr::{Multiaddr, Protocol},
	multihash::Multihash,
};

/// Domain-separation tag used when deriving P-256 key from ed25519 key via HMAC-SHA256.
const CERTIFICATE_KEY_DST: &[u8] = b"substrate-webrtc-p256-v1";

/// Common name used for both subject and issuer, the certificate being self-signed.
const SUBJECT_COMMON_NAME: &str = "polkadot-sdk-webrtc";

/// Deterministically generate a WebRTC DTLS certificate from the node's secret key.
pub fn derive_certificate(
	node_secret_key: Ed25519SecretKey,
) -> Result<DtlsCertificate, litep2p::Error> {
	// NOTE: none of the expects in this function are input-dependent.
	let signing_key = derive_keys(node_secret_key);

	// RFC 5758 §3.2: `parameters` MUST be absent for ECDSA.
	let signature_algorithm =
		AlgorithmIdentifierOwned { oid: ECDSA_WITH_SHA_256, parameters: None };
	let name = common_name();
	// Uncompressed SEC1 form, requested explicitly: RFC 5480 permits several point forms.
	let point = signing_key.verifying_key().as_affine().to_encoded_point(false);

	let tbs_certificate = TbsCertificate {
		// Every other WebRTC stack emits V3, let's do the same, even though it can be V1.
		version: Version::V3,
		serial_number: derive_serial(&point),
		signature: signature_algorithm.clone(),
		issuer: name.clone(),
		validity: validity(),
		subject: name,
		subject_public_key_info: SubjectPublicKeyInfoOwned {
			// RFC 5480 §2.1.1: `namedCurve`, rather than `implicitCurve` or `specifiedCurve`.
			algorithm: AlgorithmIdentifierOwned {
				oid: ID_EC_PUBLIC_KEY,
				parameters: Some(Any::from(SECP_256_R_1)),
			},
			subject_public_key: BitString::new(0, point.as_bytes())
				.expect("a SEC1 point is a whole number of octets; qed"),
		},
		// RFC 5280 §4.1.2.8: conforming CAs MUST NOT generate unique identifiers.
		issuer_unique_id: None,
		subject_unique_id: None,
		extensions: None,
	};

	let tbs_der = tbs_certificate.to_der().expect("the certificate is well-formed; qed");

	// `PrehashSigner` is RFC 6979 deterministic ECDSA; the module's determinism rests on it. If
	// an upgrade ever changes its output (caught by the tests), the old implementation must be
	// vendored into `sc-network`.
	//
	// Signing fails only if the RFC 6979 nonce or a signature scalar is zero (each 2^-256).
	let signature: DerSignature = signing_key
		.sign_prehash(&Sha256::digest(&tbs_der))
		.expect("the signing key is valid; qed");

	let certificate_der = Certificate {
		tbs_certificate,
		signature_algorithm,
		// RFC 3279 §2.2.3: the DER-encoded `Ecdsa-Sig-Value`, with no unused bits.
		signature: BitString::new(0, signature.to_vec())
			.expect("an ECDSA signature is a whole number of octets; qed"),
	}
	.to_der()
	.expect("a well-formed certificate is DER-encodable; qed");

	let pk_pkcs8_der = signing_key
		.to_pkcs8_der()
		.expect("a P-256 signing key is PKCS#8-encodable; qed")
		.as_bytes()
		.to_vec();

	DtlsCertificate::load(certificate_der, pk_pkcs8_der)
}

/// Derive the P-256 key by rejection-sampling HMAC-SHA256(node key, `DST || counter`).
fn derive_keys(node_secret_key: Ed25519SecretKey) -> SigningKey {
	(0u8..255u8)
		.find_map(|counter| {
			let okm = Hmac::<Sha256>::new_from_slice(node_secret_key.as_ref())
				.expect("HMAC accepts keys of any length; qed")
				.chain_update(CERTIFICATE_KEY_DST)
				.chain_update([counter])
				.finalize()
				.into_bytes();

			// Range-checked here rather than by letting `SigningKey::from_slice` reject: which
			// counter wins must not depend on whether out-of-range values are rejected or reduced.
			let scalar = U256::from_be_slice(&okm);
			(scalar != U256::ZERO && scalar < NistP256::ORDER)
				.then(|| SigningKey::from_slice(&okm).expect("checked to be in range; qed"))
		})
		.expect("each iteration succeeds with probability 1 - 2^-32, and we have 256 of them; qed")
}

/// Serial derived from the public key, only to avoid one hardcoded serial for all certificates.
fn derive_serial(point: &EncodedPoint) -> SerialNumber {
	let digest = Sha256::digest(point.as_bytes());

	// A `u64` names exactly one integer, fixing the DER by spec; a byte string would leave
	// signedness to `SerialNumber`.
	SerialNumber::from(u64::from_be_bytes(digest[..8].try_into().expect("8 of 32 bytes; qed")))
}

/// `CN=polkadot-sdk-webrtc`, built explicitly so the attribute's string type is pinned.
fn common_name() -> Name {
	let attribute = AttributeTypeAndValue {
		oid: COMMON_NAME,
		value: Any::new(Tag::Utf8String, SUBJECT_COMMON_NAME.as_bytes())
			.expect("the common name is a valid `UTF8String`; qed"),
	};
	let rdn = SetOfVec::try_from(vec![attribute]).expect("a single-element set is sorted; qed");

	Name::from(vec![RelativeDistinguishedName::from(rdn)])
}

/// Fixed validity dates, required for determinism. WebRTC peers pin the certificate by
/// certhash and ignore its lifetime.
fn validity() -> Validity {
	// RFC 5280 §4.1.2.5: dates through 2049 MUST be encoded as `UTCTime`.
	let not_before = UtcTime::from_date_time(
		DateTime::new(2000, 1, 1, 0, 0, 0).expect("2000-01-01 00:00:00 is a valid date; qed"),
	)
	.expect("2000 is within the `UTCTime` range; qed");
	// RFC 5280 §4.1.2.5: the encoding for certificates with no well-defined expiration date.
	let not_after = GeneralizedTime::from_date_time(
		DateTime::new(9999, 12, 31, 23, 59, 59).expect("9999-12-31 23:59:59 is a valid date; qed"),
	);

	Validity { not_before: Time::UtcTime(not_before), not_after: Time::GeneralTime(not_after) }
}

/// Whether `address` contains the `webrtc-direct` transport
pub(crate) fn is_webrtc_address(address: &Multiaddr) -> bool {
	address.iter().any(|protocol| matches!(protocol, Protocol::WebRTCDirect))
}

/// Check that `address` is a bare `webrtc-direct` address.
///
/// Dns is accepted as host only if the validation is applied to a public address.
fn validate(address: &Multiaddr, public_addr: bool) -> Result<(), Error> {
	let mut iter = address.iter();

	let host_is_valid = match iter.next() {
		Some(Protocol::Ip4(_) | Protocol::Ip6(_)) => true,
		Some(Protocol::Dns(_) | Protocol::Dns4(_) | Protocol::Dns6(_)) => public_addr,
		_ => false,
	};

	// `/udp/<port>/webrtc-direct` and nothing after it.
	let is_valid = host_is_valid &&
		matches!(
			(iter.next(), iter.next(), iter.next()),
			(Some(Protocol::Udp(_)), Some(Protocol::WebRTCDirect), None)
		);

	is_valid
		.then_some(())
		.ok_or_else(|| Error::InvalidWebRtcAddress { address: address.clone() })
}

pub(crate) fn validate_listen_address(address: &Multiaddr) -> Result<(), Error> {
	validate(address, false)
}

pub(crate) fn validate_public_address(address: &Multiaddr) -> Result<(), Error> {
	validate(address, true)
}

/// Append the node's `certhash` to a public `webrtc-direct` address.
pub fn complete_public_address(address: &mut Multiaddr, certhash: Multihash) -> Result<(), Error> {
	validate_public_address(address)?;
	address.push(Protocol::Certhash(certhash));
	Ok(())
}

/// Check the shape of every `webrtc-direct` address, then append `certhash` to the public ones.
pub(crate) fn validate_and_complete_addresses(
	listen_addresses: &[Multiaddr],
	public_addresses: &mut [Multiaddr],
	certhash: Multihash,
) -> Result<(), Error> {
	// Listen addresses are validated but never completed, unlike the public ones below.
	//
	// A listen address names a socket to bind, and `/certhash` is no part of binding one.
	// litep2p appends it itself when it reports the address it actually bound.
	//
	// A public address is the opposite: it is handed to peers to dial, and a `webrtc-direct` dialer
	// verifies the DTLS handshake against that hash, so it has to be there before anything reads
	// `--public-addr`.
	for address in listen_addresses {
		if is_webrtc_address(address) {
			validate_listen_address(address)?;
		}
	}

	for address in public_addresses.iter_mut() {
		if is_webrtc_address(address) {
			complete_public_address(address, certhash)?;
		}
	}

	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::config::{NetworkBackendType, NetworkConfiguration, NodeKeyConfig};
	use sc_network_types::multihash::Code;

	/// Node secret key from raw bytes.
	fn node_key_from(bytes: [u8; 32]) -> Ed25519SecretKey {
		Ed25519SecretKey::try_from_bytes(bytes)
			.expect("any 32 bytes are a valid ed25519 secret key; qed")
	}

	/// Node secret key with every byte set to `byte`.
	fn node_key(byte: u8) -> Ed25519SecretKey {
		node_key_from([byte; 32])
	}

	/// Compute the `/certhash/<hash>` multiaddress component of a certificate.
	fn certhash_component(certificate: &DtlsCertificate) -> String {
		let hash = Code::Sha2_256.digest(certificate.as_parts().0);
		Multiaddr::empty().with(Protocol::Certhash(hash)).to_string()
	}

	#[test]
	fn deterministic_certificate_generation() {
		let key = node_key(7);
		let first = derive_certificate(key.clone()).unwrap();
		let second = derive_certificate(key).unwrap();

		assert_eq!(first.as_parts(), second.as_parts());
		assert_eq!(certhash_component(&first), certhash_component(&second));
	}

	#[test]
	fn derive_certificate_uses_version3() {
		use x509_cert::der::Decode;

		let certificate = derive_certificate(node_key(1)).unwrap();
		let (certificate_der, _) = certificate.as_parts();

		let parsed = Certificate::from_der(certificate_der).unwrap();

		// Both fields are optional in the DER, and dropping either changes the certhash: `version`
		// because v1 is the ASN.1 DEFAULT, `extensions` because it is absent rather than empty.
		assert_eq!(parsed.tbs_certificate.version, Version::V3);
		assert!(parsed.tbs_certificate.extensions.is_none());
	}

	#[test]
	fn different_node_keys_produce_different_certificates() {
		let first = derive_certificate(node_key(1)).unwrap();
		let second = derive_certificate(node_key(2)).unwrap();

		assert_ne!(first.as_parts().0, second.as_parts().0);
		assert_ne!(first.as_parts().1, second.as_parts().1);
		assert_ne!(certhash_component(&first), certhash_component(&second));
	}

	#[test]
	fn stable_certhash() {
		// Pins the node key -> certificate derivation. If this test ever fails, the derivation
		// changed and the certhash published by every node relying on it breaks.
		//
		// Three vectors, because the serial's INTEGER encoding splits on its first byte:
		// `node_key(7)` (serial `0x87..`) gets a `0x00` sign byte, `node_key(42)` (`0x6e..`)
		// encodes plain, and the third key (`0x002b..`) has its leading zero stripped.
		assert_eq!(
			certhash_component(&derive_certificate(node_key(7)).unwrap()),
			"/certhash/uEiAXqXtF_3QIfMcgXwMgneoB4EuSE_EcpGvKhY4yz7HfcA"
		);
		assert_eq!(
			certhash_component(&derive_certificate(node_key(42)).unwrap()),
			"/certhash/uEiAWsH8V-_VMveqodSJYiAhW5FikqSzBNLV0FyeEb_oetA"
		);
		let mut stripped_serial_key = [3u8; 32];
		stripped_serial_key[31] = 26;
		assert_eq!(
			certhash_component(&derive_certificate(node_key_from(stripped_serial_key)).unwrap()),
			"/certhash/uEiAfSKLRHZTkoALez2X0jqB0Yyh6T4DYQGM3wLpbR1u7dQ"
		);
	}

	#[test]
	fn rfc6979_signing_is_pinned() {
		use p256::ecdsa::Signature;

		// Everything else in this module states its own encoding; the signature cannot: ECDSA
		// needs a nonce, and reproducibility rests on `sign_prehash` deriving it via RFC 6979.
		// The RFC leaves inputs that `ecdsa` fills in for us, so pin the signature itself — an
		// upgrade changing any of them fails here, naming the cause.
		//
		// Pinned only for prehashes below the group order: `ecdsa` 0.16 feeds `h1` to the nonce
		// DRBG unreduced, 0.17 reduces it per RFC 6979 §2.3.4. Bumping past 0.16 thus changes
		// the certhash of nodes whose TBS digest is >= `n` — accepted, as that is ~2^-32 of them.
		let key = SigningKey::from_slice(&array_bytes::hex2bytes_unchecked(
			"c9afa9d845ba75166b5c215767b1d6934e50c3db36e89b127b8a622b120f6721",
		))
		.unwrap();

		// RFC 6979 A.2.5: P-256 with SHA-256, message "sample". Pins the HMAC hash
		// (`NistP256::Digest`) and the empty additional-data argument.
		let signature: Signature = key.sign_prehash(&Sha256::digest(b"sample")).unwrap();
		assert_eq!(
			array_bytes::bytes2hex("", signature.to_bytes()),
			"efd48b2aacb6a8fd1140dd9cd45e81d69d2c877b56aaf991c34d0ea84eaf3716\
			 f7cb1c942d657c41d436c7a1b6e29f65f3e900dbb9aff4064dc4ab2f843acda8"
		);
	}

	#[test]
	fn golden_certificate() {
		// Same guarantee as `stable_certhash`, but a diffable artifact: on failure, compare the
		// two encodings field by field to see which one moved.
		//
		//     openssl asn1parse -inform DER -in substrate/client/network/res/webrtc_cert.der
		//
		// Only ever regenerate this alongside a deliberate, documented derivation change: every
		// node's advertised `/certhash/...` changes with it.
		const GOLDEN: &[u8] = include_bytes!("../res/webrtc_cert.der");

		let certificate = derive_certificate(node_key(42)).unwrap();
		assert_eq!(certificate.as_parts().0.as_slice(), GOLDEN);
	}

	#[test]
	fn generated_certificate_is_valid() {
		use p256::ecdsa::{signature::Verifier, VerifyingKey};
		use x509_cert::der::Decode;

		let certificate = derive_certificate(node_key(8)).unwrap();
		let (certificate_der, _) = certificate.as_parts();

		let parsed = Certificate::from_der(certificate_der).unwrap();
		let validity = parsed.tbs_certificate.validity;
		assert_eq!(validity.not_before.to_date_time(), DateTime::new(2000, 1, 1, 0, 0, 0).unwrap());
		assert_eq!(
			validity.not_after.to_date_time(),
			DateTime::new(9999, 12, 31, 23, 59, 59).unwrap()
		);
		// The choice of time type is part of the encoding, not just the instant it denotes.
		assert!(matches!(validity.not_before, Time::UtcTime(_)));
		assert!(matches!(validity.not_after, Time::GeneralTime(_)));

		// The self-signature verifies with the certificate's own public key.
		let public_key = VerifyingKey::from_sec1_bytes(
			parsed
				.tbs_certificate
				.subject_public_key_info
				.subject_public_key
				.as_bytes()
				.unwrap(),
		)
		.unwrap();
		let tbs = parsed.tbs_certificate.to_der().unwrap();
		let signature = DerSignature::try_from(parsed.signature.as_bytes().unwrap()).unwrap();
		public_key.verify(&tbs, &signature).unwrap();
	}

	/// `/ip4/1.2.3.4/udp/30334/webrtc-direct`: the shape an operator is expected to supply.
	fn webrtc_address() -> Multiaddr {
		Multiaddr::empty()
			.with(Protocol::Ip4([1, 2, 3, 4].into()))
			.with(Protocol::Udp(30334))
			.with(Protocol::WebRTCDirect)
	}

	#[test]
	fn bare_webrtc_address_accepted() {
		assert!(validate_listen_address(&webrtc_address()).is_ok());
		assert!(validate_public_address(&webrtc_address()).is_ok());
	}

	/// The one difference between the two kinds: a public address is dialed, so the dialer can
	/// resolve a name, while a listen address is bound and a name is nothing to bind.
	#[test]
	fn dns_host_accepted_for_a_public_address_only() {
		let address = Multiaddr::empty()
			.with(Protocol::Dns("example.com".into()))
			.with(Protocol::Udp(30334))
			.with(Protocol::WebRTCDirect);

		assert!(validate_public_address(&address).is_ok());
		assert!(matches!(
			validate_listen_address(&address),
			Err(Error::InvalidWebRtcAddress { .. }),
		));
	}

	#[test]
	fn operator_supplied_certhash_rejected() {
		// The node presents a certificate of its own; no hash the operator writes can match it.
		let their_certhash = Code::Sha2_256.digest(b"theirs");
		let address = webrtc_address().with(Protocol::Certhash(their_certhash));

		assert!(matches!(
			validate_listen_address(&address),
			Err(Error::InvalidWebRtcAddress { .. }),
		));
	}

	#[test]
	fn webrtc_over_tcp_rejected() {
		let address = Multiaddr::empty()
			.with(Protocol::Ip4([1, 2, 3, 4].into()))
			.with(Protocol::Tcp(30334))
			.with(Protocol::WebRTCDirect);

		assert!(matches!(
			validate_listen_address(&address),
			Err(Error::InvalidWebRtcAddress { .. }),
		));
	}

	/// A configuration with a fixed node key, so the certificate it will present can be derived
	/// in the test as well.
	fn webrtc_config(public_address: &str) -> NetworkConfiguration {
		use crate::config::{ed25519, Secret};

		let mut config = NetworkConfiguration::new_local();
		config.node_key = NodeKeyConfig::Ed25519(Secret::Input(
			ed25519::SecretKey::try_from_bytes([7u8; 32]).unwrap(),
		));
		config.listen_addresses = vec!["/ip4/0.0.0.0/udp/30333/webrtc-direct".parse().unwrap()];
		config.public_addresses = vec![public_address.parse().unwrap()];

		config
	}

	/// The `/certhash` the node of [`webrtc_config`] presents.
	fn node_certhash() -> Protocol<'static> {
		let keypair = webrtc_config("/ip4/1.2.3.4/tcp/30333").node_key.into_keypair().unwrap();
		let certificate = derive_certificate(keypair.secret().into()).unwrap();

		Protocol::Certhash(certificate.certhash().into())
	}

	#[test]
	fn webrtc_public_address_completed() {
		let mut config = webrtc_config("/ip4/203.0.113.9/udp/31234/webrtc-direct");
		config.validate_and_complete_webrtc_addresses().unwrap();

		assert_eq!(
			config.public_addresses,
			vec!["/ip4/203.0.113.9/udp/31234/webrtc-direct"
				.parse::<Multiaddr>()
				.unwrap()
				.with(node_certhash())],
		);
	}

	#[test]
	fn already_completed_public_address_rejected() {
		// Completion is not idempotent by design: a `/certhash` that is already there is rejected
		// rather than tolerated, so running the completion twice is caught instead of hidden.
		let mut config = webrtc_config("/ip4/203.0.113.9/udp/31234/webrtc-direct");
		config.validate_and_complete_webrtc_addresses().unwrap();

		assert!(matches!(
			config.validate_and_complete_webrtc_addresses(),
			Err(Error::InvalidWebRtcAddress { .. }),
		));
	}

	#[test]
	fn removing_webrtc_addresses_drops_completed_public_address() {
		// A collator drops the relay-side WebRTC listeners after validation completed the
		// public address: the removal must drop the now listener-less public address with
		// them instead of leaving it advertised with nothing serving it.
		let mut config = webrtc_config("/ip4/203.0.113.9/udp/31234/webrtc-direct");
		config.validate_and_complete_webrtc_addresses().unwrap();

		config.remove_webrtc_addresses();

		assert!(config.listen_addresses.is_empty());
		assert!(config.public_addresses.is_empty());
	}

	#[test]
	fn removing_webrtc_addresses_keeps_other_addresses() {
		let public_address = "/ip4/203.0.113.9/tcp/31234";
		let mut config = webrtc_config(public_address);
		let tcp_listener: Multiaddr = "/ip4/0.0.0.0/tcp/30333".parse().unwrap();
		config.listen_addresses.push(tcp_listener.clone());
		config.validate_and_complete_webrtc_addresses().unwrap();

		config.remove_webrtc_addresses();

		assert_eq!(config.listen_addresses, vec![tcp_listener]);
		assert_eq!(config.public_addresses, vec![public_address.parse::<Multiaddr>().unwrap()]);
	}

	#[test]
	fn malformed_webrtc_public_address_rejected() {
		// `tcp` rather than `udp`, so there is no shape to complete.
		let mut config = webrtc_config("/ip4/203.0.113.9/tcp/31234/webrtc-direct");

		assert!(matches!(
			config.validate_and_complete_webrtc_addresses(),
			Err(Error::InvalidWebRtcAddress { .. }),
		));
	}

	#[test]
	fn webrtc_public_address_with_certhash_rejected() {
		// The node presents a certificate of its own; no hash the operator writes can match it.
		let address = "/ip4/203.0.113.9/udp/31234/webrtc-direct"
			.parse::<Multiaddr>()
			.unwrap()
			.with(Protocol::Certhash(Code::Sha2_256.digest(b"theirs")));
		let mut config = webrtc_config("/ip4/203.0.113.9/tcp/31234");
		config.public_addresses = vec![address];

		assert!(matches!(
			config.validate_and_complete_webrtc_addresses(),
			Err(Error::InvalidWebRtcAddress { .. }),
		));
	}

	#[test]
	fn webrtc_listen_address_with_certhash_rejected() {
		let address = "/ip4/0.0.0.0/udp/30333/webrtc-direct"
			.parse::<Multiaddr>()
			.unwrap()
			.with(Protocol::Certhash(Code::Sha2_256.digest(b"theirs")));
		let mut config = webrtc_config("/ip4/203.0.113.9/tcp/31234");
		config.listen_addresses = vec![address];

		assert!(matches!(
			config.validate_and_complete_webrtc_addresses(),
			Err(Error::InvalidWebRtcAddress { .. }),
		));
	}

	#[test]
	fn webrtc_public_address_without_a_listener_rejected() {
		// No WebRTC listen address means no certificate, so there is nothing to advertise.
		let mut config = webrtc_config("/ip4/203.0.113.9/udp/31234/webrtc-direct");
		config.listen_addresses = vec!["/ip4/0.0.0.0/tcp/30333".parse().unwrap()];

		assert!(matches!(
			config.validate_and_complete_webrtc_addresses(),
			Err(Error::WebRtcTransportNotConfigured { .. }),
		));
	}

	#[test]
	fn dns_webrtc_listen_address_rejected() {
		// A listen address is bound, so a `dns` host names nothing bindable. Accepting it would
		// leave the node advertising a certhash for a transport that was never started.
		let mut config = webrtc_config("/ip4/203.0.113.9/udp/31234/webrtc-direct");
		config.listen_addresses = vec!["/dns/example.com/udp/30333/webrtc-direct".parse().unwrap()];

		assert!(matches!(
			config.validate_and_complete_webrtc_addresses(),
			Err(Error::InvalidWebRtcAddress { .. }),
		));
	}

	#[test]
	fn public_address_untouched_without_webrtc() {
		// An address of another transport is never touched.
		let address = "/ip4/203.0.113.9/tcp/31234";
		let mut config = webrtc_config(address);
		config.validate_and_complete_webrtc_addresses().unwrap();

		assert_eq!(config.public_addresses, vec![address.parse::<Multiaddr>().unwrap()]);
	}

	#[test]
	fn webrtc_listen_address_rejected_on_libp2p() {
		// libp2p has no WebRTC transport: it would drop the listen address with one warning and
		// carry on, leaving the node reachable on nothing it advertises.
		let mut config = webrtc_config("/ip4/203.0.113.9/tcp/31234");
		config.network_backend = NetworkBackendType::Libp2p;

		assert!(matches!(
			config.validate_and_complete_webrtc_addresses(),
			Err(Error::WebRtcNotSupportedByBackend),
		));
	}

	#[test]
	fn webrtc_public_address_rejected_on_libp2p() {
		// Only the public address is WebRTC here, so there is nothing to complete; the backend
		// check must still reject it.
		let mut config = webrtc_config("/ip4/203.0.113.9/udp/31234/webrtc-direct");
		config.listen_addresses = vec!["/ip4/0.0.0.0/tcp/30333".parse().unwrap()];
		config.network_backend = NetworkBackendType::Libp2p;

		assert!(matches!(
			config.validate_and_complete_webrtc_addresses(),
			Err(Error::WebRtcNotSupportedByBackend),
		));
	}

	#[test]
	fn non_webrtc_addresses_accepted_on_libp2p() {
		let address = "/ip4/203.0.113.9/tcp/31234";
		let mut config = webrtc_config(address);
		config.listen_addresses = vec!["/ip4/0.0.0.0/tcp/30333".parse().unwrap()];
		config.network_backend = NetworkBackendType::Libp2p;

		config.validate_and_complete_webrtc_addresses().unwrap();

		assert_eq!(config.public_addresses, vec![address.parse::<Multiaddr>().unwrap()]);
	}

	#[test]
	fn webrtc_rejected_on_libp2p_before_node_key_is_resolved() {
		// Resolving a file-backed node key writes the file. Rejecting first means a configuration
		// that will not start leaves nothing behind.
		use crate::config::Secret;

		let directory = tempfile::Builder::new().prefix("webrtc").tempdir().unwrap();
		let key_path = directory.path().join("node_key");

		let mut config = webrtc_config("/ip4/203.0.113.9/udp/31234/webrtc-direct");
		config.node_key = NodeKeyConfig::Ed25519(Secret::File(key_path.clone()));
		config.network_backend = NetworkBackendType::Libp2p;

		assert!(config.validate_and_complete_webrtc_addresses().is_err());
		assert!(!key_path.exists(), "the node key file must not be created for a rejected config");
	}
}
