// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! All peersets and protocols used for parachains.

use derive_more::Display;
use polkadot_primitives::Hash;
use sc_network::{
	config::SetConfig, peer_store::PeerStoreProvider, service::NotificationMetrics,
	types::ProtocolName, NetworkBackend, NotificationService,
};
use sp_runtime::traits::Block;
use std::{
	collections::{hash_map::Entry, HashMap},
	ops::{Index, IndexMut},
	sync::Arc,
};
use strum::{EnumIter, IntoEnumIterator};

/// The legacy collation protocol name. Only supported on version = 1.
const LEGACY_COLLATION_PROTOCOL_V1: &str = "/polkadot/collation/1";

/// The legacy protocol version. Is always 1 for collation.
const LEGACY_COLLATION_PROTOCOL_VERSION_V1: u32 = 1;

/// Max notification size is currently constant.
pub const MAX_NOTIFICATION_SIZE: u64 = 100 * 1024;

/// Maximum allowed incoming connection streams for validator nodes on the collation protocol.
pub const MAX_AUTHORITY_INCOMING_STREAMS: u32 = 310;

/// The peer-sets and thus the protocols which are used for the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumIter)]
pub enum PeerSet {
	/// The validation peer-set is responsible for all messages related to candidate validation and
	/// communication among validators.
	Validation,
	/// The collation peer-set is used for validator<>collator communication.
	Collation,
}

/// Whether a node is an authority or not.
///
/// Peer set configuration gets adjusted accordingly.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum IsAuthority {
	/// Node is authority.
	Yes,
	/// Node is not an authority.
	No,
}

impl PeerSet {
	/// Get `sc_network` peer set configurations for each peerset on the default version.
	///
	/// Those should be used in the network configuration to register the protocols with the
	/// network service.
	pub fn get_info<B: Block, N: NetworkBackend<B, <B as Block>::Hash>>(
		self,
		is_authority: IsAuthority,
		peerset_protocol_names: &PeerSetProtocolNames,
		metrics: NotificationMetrics,
		peer_store_handle: Arc<dyn PeerStoreProvider>,
	) -> (N::NotificationProtocolConfig, (PeerSet, Box<dyn NotificationService>)) {
		// Networking layer relies on `get_main_name()` being the main name of the protocol
		// for peersets and connection management.
		let protocol = peerset_protocol_names.get_main_name(self);
		let fallback_names = peerset_protocol_names.get_fallback_names(self);
		let max_notification_size = self.get_max_notification_size(is_authority);

		match self {
			PeerSet::Validation => {
				let (config, notification_service) = N::notification_config(
					protocol,
					fallback_names,
					max_notification_size,
					None,
					SetConfig {
						// we allow full nodes to connect to validators for gossip
						// to ensure any `MIN_GOSSIP_PEERS` always include reserved peers
						// we limit the amount of non-reserved slots to be less
						// than `MIN_GOSSIP_PEERS` in total
						in_peers: super::MIN_GOSSIP_PEERS as u32 / 2 - 1,
						out_peers: super::MIN_GOSSIP_PEERS as u32 / 2 - 1,
						reserved_nodes: Vec::new(),
						non_reserved_mode: sc_network::config::NonReservedPeerMode::Accept,
					},
					metrics,
					peer_store_handle,
				);

				(config, (PeerSet::Validation, notification_service))
			},
			PeerSet::Collation => {
				let (config, notification_service) = N::notification_config(
					protocol,
					fallback_names,
					max_notification_size,
					None,
					SetConfig {
						// Non-authority nodes don't need to accept incoming connections on this
						// peer set:
						in_peers: if is_authority == IsAuthority::Yes {
							MAX_AUTHORITY_INCOMING_STREAMS
						} else {
							0
						},
						out_peers: 0,
						reserved_nodes: Vec::new(),
						non_reserved_mode: if is_authority == IsAuthority::Yes {
							sc_network::config::NonReservedPeerMode::Accept
						} else {
							sc_network::config::NonReservedPeerMode::Deny
						},
					},
					metrics,
					peer_store_handle,
				);

				(config, (PeerSet::Collation, notification_service))
			},
		}
	}

	/// Get the max notification size for this peer set.
	pub fn get_max_notification_size(self, _: IsAuthority) -> u64 {
		MAX_NOTIFICATION_SIZE
	}

	/// Get the peer set label for metrics reporting.
	pub fn get_label(self) -> &'static str {
		match self {
			PeerSet::Validation => "validation",
			PeerSet::Collation => "collation",
		}
	}

	/// Get the protocol label for metrics reporting.
	pub fn get_protocol_label(self, version: ProtocolVersion) -> Option<&'static str> {
		// Unfortunately, labels must be static strings, so we must manually cover them
		// for all protocol versions here.
		match self {
			PeerSet::Validation => {
				if version == ValidationVersion::V3.into() {
					Some("validation/3")
				} else {
					None
				}
			},
			PeerSet::Collation => {
				if version == CollationVersion::V1.into() {
					Some("collation/1")
				} else if version == CollationVersion::V2.into() {
					Some("collation/2")
				} else if version == CollationVersion::V3.into() {
					Some("collation/3")
				} else if version == CollationVersion::V4.into() {
					Some("collation/4")
				} else {
					None
				}
			},
		}
	}
}

/// A small and nifty collection that allows to store data pertaining to each peer set.
#[derive(Debug, Default)]
pub struct PerPeerSet<T> {
	validation: T,
	collation: T,
}

impl<T> Index<PeerSet> for PerPeerSet<T> {
	type Output = T;
	fn index(&self, index: PeerSet) -> &T {
		match index {
			PeerSet::Validation => &self.validation,
			PeerSet::Collation => &self.collation,
		}
	}
}

impl<T> IndexMut<PeerSet> for PerPeerSet<T> {
	fn index_mut(&mut self, index: PeerSet) -> &mut T {
		match index {
			PeerSet::Validation => &mut self.validation,
			PeerSet::Collation => &mut self.collation,
		}
	}
}

/// Get `NonDefaultSetConfig`s for all available peer sets, at their default versions.
///
/// Should be used during network configuration (added to `NetworkConfiguration::extra_sets`)
/// or shortly after startup to register the protocols with the network service.
pub fn peer_sets_info<B: Block, N: NetworkBackend<B, <B as Block>::Hash>>(
	is_authority: IsAuthority,
	peerset_protocol_names: &PeerSetProtocolNames,
	metrics: NotificationMetrics,
	peer_store_handle: Arc<dyn PeerStoreProvider>,
) -> Vec<(N::NotificationProtocolConfig, (PeerSet, Box<dyn NotificationService>))> {
	PeerSet::iter()
		.map(|s| {
			s.get_info::<B, N>(
				is_authority,
				&peerset_protocol_names,
				metrics.clone(),
				Arc::clone(&peer_store_handle),
			)
		})
		.collect()
}

/// A generic version of the protocol. This struct must not be created directly.
#[derive(Debug, Clone, Copy, Display, PartialEq, Eq, Hash)]
pub struct ProtocolVersion(u32);

impl From<ProtocolVersion> for u32 {
	fn from(version: ProtocolVersion) -> u32 {
		version.0
	}
}

/// Supported validation protocol versions. Only versions defined here must be used in the codebase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumIter)]
pub enum ValidationVersion {
	/// The third version.
	V3 = 3,
}

/// Supported collation protocol versions. Only versions defined here must be used in the codebase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumIter)]
pub enum CollationVersion {
	/// The first version.
	V1 = 1,
	/// The second version.
	V2 = 2,
	/// The third version, adds explicit scheduling_parent field and candidate descriptor version.
	V3 = 3,
	/// Adds possibility to advertise a list of collations.
	V4 = 4,
}

/// Marker indicating the version is unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnknownVersion;

impl TryFrom<ProtocolVersion> for ValidationVersion {
	type Error = UnknownVersion;

	fn try_from(p: ProtocolVersion) -> Result<Self, UnknownVersion> {
		for v in Self::iter() {
			if v as u32 == p.0 {
				return Ok(v);
			}
		}

		Err(UnknownVersion)
	}
}

impl TryFrom<ProtocolVersion> for CollationVersion {
	type Error = UnknownVersion;

	fn try_from(p: ProtocolVersion) -> Result<Self, UnknownVersion> {
		for v in Self::iter() {
			if v as u32 == p.0 {
				return Ok(v);
			}
		}

		Err(UnknownVersion)
	}
}

impl From<ValidationVersion> for ProtocolVersion {
	fn from(version: ValidationVersion) -> ProtocolVersion {
		ProtocolVersion(version as u32)
	}
}

impl From<CollationVersion> for ProtocolVersion {
	fn from(version: CollationVersion) -> ProtocolVersion {
		ProtocolVersion(version as u32)
	}
}

/// On the wire protocol name to [`PeerSet`] mapping.
#[derive(Debug, Clone)]
pub struct PeerSetProtocolNames {
	protocols: HashMap<ProtocolName, (PeerSet, ProtocolVersion)>,
	names: HashMap<(PeerSet, ProtocolVersion), ProtocolName>,
	genesis_hash: Hash,
	fork_id: Option<String>,
	main_collation_version: Option<CollationVersion>,
}

impl PeerSetProtocolNames {
	/// Construct [`PeerSetProtocolNames`] using `genesis_hash` and `fork_id`.
	pub fn new(genesis_hash: Hash, fork_id: Option<&str>) -> Self {
		Self::new_with_main_collation_version(genesis_hash, fork_id, None)
	}

	/// Same as [`Self::new`], but pins the main collation protocol version.
	///
	/// With `None`, tracks the newest supported version — same as [`Self::new`].
	///
	/// Only intended for the legacy collator protocol, which is capped at
	/// [`CollationVersion::V3`] and will never support newer versions.
	pub fn new_with_main_collation_version(
		genesis_hash: Hash,
		fork_id: Option<&str>,
		main_collation_version: Option<CollationVersion>,
	) -> Self {
		let mut protocols = HashMap::new();
		let mut names = HashMap::new();
		for protocol in PeerSet::iter() {
			match protocol {
				PeerSet::Validation => {
					for version in ValidationVersion::iter() {
						Self::register_main_protocol(
							&mut protocols,
							&mut names,
							protocol,
							version.into(),
							&genesis_hash,
							fork_id,
						);
					}
				},
				PeerSet::Collation => {
					for version in CollationVersion::iter() {
						Self::register_main_protocol(
							&mut protocols,
							&mut names,
							protocol,
							version.into(),
							&genesis_hash,
							fork_id,
						);
					}
					Self::register_legacy_collation_protocol(&mut protocols, protocol);
				},
			}
		}
		Self {
			protocols,
			names,
			genesis_hash,
			fork_id: fork_id.map(|fork_id| fork_id.into()),
			main_collation_version,
		}
	}

	/// Helper function to register main protocol.
	fn register_main_protocol(
		protocols: &mut HashMap<ProtocolName, (PeerSet, ProtocolVersion)>,
		names: &mut HashMap<(PeerSet, ProtocolVersion), ProtocolName>,
		protocol: PeerSet,
		version: ProtocolVersion,
		genesis_hash: &Hash,
		fork_id: Option<&str>,
	) {
		let protocol_name = Self::generate_name(genesis_hash, fork_id, protocol, version);
		names.insert((protocol, version), protocol_name.clone());
		Self::insert_protocol_or_panic(protocols, protocol_name, protocol, version);
	}

	/// Helper function to register legacy collation protocol.
	fn register_legacy_collation_protocol(
		protocols: &mut HashMap<ProtocolName, (PeerSet, ProtocolVersion)>,
		protocol: PeerSet,
	) {
		Self::insert_protocol_or_panic(
			protocols,
			LEGACY_COLLATION_PROTOCOL_V1.into(),
			protocol,
			ProtocolVersion(LEGACY_COLLATION_PROTOCOL_VERSION_V1),
		)
	}

	/// Helper function to make sure no protocols have the same name.
	fn insert_protocol_or_panic(
		protocols: &mut HashMap<ProtocolName, (PeerSet, ProtocolVersion)>,
		name: ProtocolName,
		protocol: PeerSet,
		version: ProtocolVersion,
	) {
		match protocols.entry(name) {
			Entry::Vacant(entry) => {
				entry.insert((protocol, version));
			},
			Entry::Occupied(entry) => {
				panic!(
					"Protocol {:?} (version {}) has the same on-the-wire name as protocol {:?} (version {}): `{}`.",
					protocol,
					version,
					entry.get().0,
					entry.get().1,
					entry.key(),
				);
			},
		}
	}

	/// Lookup the protocol using its on the wire name.
	pub fn try_get_protocol(&self, name: &ProtocolName) -> Option<(PeerSet, ProtocolVersion)> {
		self.protocols.get(name).map(ToOwned::to_owned)
	}

	/// Get the main protocol name. It's used by the networking for keeping track
	/// of peersets and connections.
	pub fn get_main_name(&self, protocol: PeerSet) -> ProtocolName {
		self.get_name(protocol, self.get_main_version(protocol))
	}

	/// Get the protocol name for specific version.
	pub fn get_name(&self, protocol: PeerSet, version: ProtocolVersion) -> ProtocolName {
		self.names
			.get(&(protocol, version))
			.expect("Protocols & versions are specified via enums defined above, and they are all registered in `new()`; qed")
			.clone()
	}

	/// The protocol name of this protocol based on `genesis_hash` and `fork_id`.
	fn generate_name(
		genesis_hash: &Hash,
		fork_id: Option<&str>,
		protocol: PeerSet,
		version: ProtocolVersion,
	) -> ProtocolName {
		let prefix = if let Some(fork_id) = fork_id {
			format!("/{}/{}", hex::encode(genesis_hash), fork_id)
		} else {
			format!("/{}", hex::encode(genesis_hash))
		};

		let short_name = match protocol {
			PeerSet::Validation => "validation",
			PeerSet::Collation => "collation",
		};

		format!("{}/{}/{}", prefix, short_name, version).into()
	}

	/// Get the protocol fallback names for negotiation with older peers.
	pub fn get_fallback_names(&self, protocol: PeerSet) -> Vec<ProtocolName> {
		let mut fallbacks = vec![];
		match protocol {
			PeerSet::Validation => {
				// The validation protocol no longer supports protocol versions 1 and 2,
				// and only version 3 is used. Therefore, fallback protocols remain empty.
			},
			PeerSet::Collation => {
				// Collation V3 fallback so that V4 nodes can negotiate V3 with older peers
				if self.get_main_version(PeerSet::Collation) == CollationVersion::V4.into() {
					fallbacks.push(Self::generate_name(
						&self.genesis_hash,
						self.fork_id.as_deref(),
						PeerSet::Collation,
						CollationVersion::V3.into(),
					));
				}
				// Collation V2 fallback so that V4 nodes can negotiate V2 with older peers
				// instead of falling all the way back to the legacy V1 protocol.
				fallbacks.push(Self::generate_name(
					&self.genesis_hash,
					self.fork_id.as_deref(),
					PeerSet::Collation,
					CollationVersion::V2.into(),
				));
				fallbacks.push(LEGACY_COLLATION_PROTOCOL_V1.into());
			},
		};
		fallbacks
	}

	/// Networking layer relies on this being the version of the main
	/// protocol name reported by `get_main_name()`.
	pub fn get_main_version(&self, protocol: PeerSet) -> ProtocolVersion {
		match protocol {
			PeerSet::Validation => Self::newest_validation_version().into(),
			PeerSet::Collation => self
				.main_collation_version
				.unwrap_or_else(Self::newest_collation_version)
				.into(),
		}
	}

	fn newest_collation_version() -> CollationVersion {
		CollationVersion::iter()
			.max_by_key(|v| *v as u32)
			.expect("`CollationVersion` has at least one variant; qed")
	}

	fn newest_validation_version() -> ValidationVersion {
		ValidationVersion::iter()
			.max_by_key(|v| *v as u32)
			.expect("ValidationVersion` has at least one variant; qed")
	}
}

#[cfg(test)]
mod tests {
	use super::{
		CollationVersion, Hash, PeerSet, PeerSetProtocolNames, ProtocolVersion, ValidationVersion,
	};
	use strum::IntoEnumIterator;

	struct TestVersion(u32);

	impl From<TestVersion> for ProtocolVersion {
		fn from(version: TestVersion) -> ProtocolVersion {
			ProtocolVersion(version.0)
		}
	}

	#[test]
	fn protocol_names_are_correctly_generated() {
		let genesis_hash = Hash::from([
			122, 200, 116, 29, 232, 183, 20, 109, 138, 86, 23, 253, 70, 41, 20, 85, 127, 230, 60,
			38, 90, 127, 28, 16, 231, 218, 227, 40, 88, 238, 187, 128,
		]);
		let name = PeerSetProtocolNames::generate_name(
			&genesis_hash,
			None,
			PeerSet::Validation,
			TestVersion(3).into(),
		);
		let expected =
			"/7ac8741de8b7146d8a5617fd462914557fe63c265a7f1c10e7dae32858eebb80/validation/3";
		assert_eq!(name, expected.into());

		let name = PeerSetProtocolNames::generate_name(
			&genesis_hash,
			None,
			PeerSet::Collation,
			TestVersion(5).into(),
		);
		let expected =
			"/7ac8741de8b7146d8a5617fd462914557fe63c265a7f1c10e7dae32858eebb80/collation/5";
		assert_eq!(name, expected.into());

		let fork_id = Some("test-fork");
		let name = PeerSetProtocolNames::generate_name(
			&genesis_hash,
			fork_id,
			PeerSet::Validation,
			TestVersion(7).into(),
		);
		let expected =
			"/7ac8741de8b7146d8a5617fd462914557fe63c265a7f1c10e7dae32858eebb80/test-fork/validation/7";
		assert_eq!(name, expected.into());

		let name = PeerSetProtocolNames::generate_name(
			&genesis_hash,
			fork_id,
			PeerSet::Collation,
			TestVersion(11).into(),
		);
		let expected =
			"/7ac8741de8b7146d8a5617fd462914557fe63c265a7f1c10e7dae32858eebb80/test-fork/collation/11";
		assert_eq!(name, expected.into());
	}

	#[test]
	fn all_protocol_names_are_known() {
		let genesis_hash = Hash::from([
			122, 200, 116, 29, 232, 183, 20, 109, 138, 86, 23, 253, 70, 41, 20, 85, 127, 230, 60,
			38, 90, 127, 28, 16, 231, 218, 227, 40, 88, 238, 187, 128,
		]);
		let protocol_names = PeerSetProtocolNames::new(genesis_hash, None);

		let validation_main =
			"/7ac8741de8b7146d8a5617fd462914557fe63c265a7f1c10e7dae32858eebb80/validation/3";
		assert_eq!(
			protocol_names.try_get_protocol(&validation_main.into()),
			Some((PeerSet::Validation, TestVersion(3).into())),
		);

		let validation_legacy = "/polkadot/validation/1";
		assert!(protocol_names.try_get_protocol(&validation_legacy.into()).is_none());

		let collation_main =
			"/7ac8741de8b7146d8a5617fd462914557fe63c265a7f1c10e7dae32858eebb80/collation/1";
		assert_eq!(
			protocol_names.try_get_protocol(&collation_main.into()),
			Some((PeerSet::Collation, TestVersion(1).into())),
		);

		let collation_legacy = "/polkadot/collation/1";
		assert_eq!(
			protocol_names.try_get_protocol(&collation_legacy.into()),
			Some((PeerSet::Collation, TestVersion(1).into())),
		);
	}

	#[test]
	fn all_protocol_versions_are_registered() {
		let genesis_hash = Hash::from([
			122, 200, 116, 29, 232, 183, 20, 109, 138, 86, 23, 253, 70, 41, 20, 85, 127, 230, 60,
			38, 90, 127, 28, 16, 231, 218, 227, 40, 88, 238, 187, 128,
		]);
		let protocol_names = PeerSetProtocolNames::new(genesis_hash, None);

		for protocol in PeerSet::iter() {
			match protocol {
				PeerSet::Validation => {
					for version in ValidationVersion::iter() {
						assert_eq!(
							protocol_names.get_name(protocol, version.into()),
							PeerSetProtocolNames::generate_name(
								&genesis_hash,
								None,
								protocol,
								version.into(),
							),
						);
					}
				},
				PeerSet::Collation => {
					for version in CollationVersion::iter() {
						assert_eq!(
							protocol_names.get_name(protocol, version.into()),
							PeerSetProtocolNames::generate_name(
								&genesis_hash,
								None,
								protocol,
								version.into(),
							),
						);
					}
				},
			}
		}
	}

	/// Asserts that every version of a peer set is reachable via the main protocol name
	/// or a fallback. Without this, bumping the main version without adding the old one
	/// as a fallback causes peers to silently downgrade further than intended.
	fn assert_all_versions_negotiable(
		peer_set: PeerSet,
		main_collation_version: Option<CollationVersion>,
		versions: impl Iterator<Item = ProtocolVersion>,
	) {
		let genesis_hash = Hash::from([
			122, 200, 116, 29, 232, 183, 20, 109, 138, 86, 23, 253, 70, 41, 20, 85, 127, 230, 60,
			38, 90, 127, 28, 16, 231, 218, 227, 40, 88, 238, 187, 128,
		]);
		let protocol_names = PeerSetProtocolNames::new_with_main_collation_version(
			genesis_hash,
			None,
			main_collation_version,
		);
		let main_version = protocol_names.get_main_version(peer_set);
		let fallback_names = protocol_names.get_fallback_names(peer_set);

		// Collect versions reachable via main + fallbacks.
		let mut negotiable_versions: std::collections::HashSet<ProtocolVersion> =
			std::collections::HashSet::new();
		negotiable_versions.insert(main_version);
		for fallback in &fallback_names {
			if let Some((ps, version)) = protocol_names.try_get_protocol(fallback) {
				assert_eq!(ps, peer_set);
				negotiable_versions.insert(version);
			}
		}

		for version in versions {
			assert!(
				negotiable_versions.contains(&version),
				"{:?} version {} is not negotiable: \
				 not the main version ({}) and not reachable via any fallback name ({:?}). \
				 Add it to get_fallback_names().",
				peer_set,
				version,
				main_version,
				fallback_names,
			);
		}
	}

	#[test]
	fn all_collation_versions_are_negotiable() {
		assert_all_versions_negotiable(
			PeerSet::Collation,
			None,
			CollationVersion::iter().map(Into::into),
		);
	}

	#[test]
	fn all_validation_versions_are_negotiable() {
		assert_all_versions_negotiable(
			PeerSet::Validation,
			None,
			ValidationVersion::iter().map(Into::into),
		);
	}

	#[test]
	fn all_protocol_versions_have_labels() {
		for protocol in PeerSet::iter() {
			match protocol {
				PeerSet::Validation => {
					for version in ValidationVersion::iter() {
						protocol
							.get_protocol_label(version.into())
							.expect("All validation protocol versions must have a label.");
					}
				},
				PeerSet::Collation => {
					for version in CollationVersion::iter() {
						protocol
							.get_protocol_label(version.into())
							.expect("All collation protocol versions must have a label.");
					}
				},
			}
		}
	}

	#[test]
	fn v3_main_caps_collation_at_v3() {
		// Everything up to V3 stays negotiable...
		assert_all_versions_negotiable(
			PeerSet::Collation,
			Some(CollationVersion::V3),
			CollationVersion::iter().filter(|v| *v != CollationVersion::V4).map(Into::into),
		);

		// ...and V4 is not reachable at all: not the main name, not a fallback.
		let genesis_hash = Hash::from([
			122, 200, 116, 29, 232, 183, 20, 109, 138, 86, 23, 253, 70, 41, 20, 85, 127, 230, 60,
			38, 90, 127, 28, 16, 231, 218, 227, 40, 88, 238, 187, 128,
		]);
		let protocol_names = PeerSetProtocolNames::new_with_main_collation_version(
			genesis_hash,
			None,
			Some(CollationVersion::V3),
		);
		let v4_name = protocol_names.get_name(PeerSet::Collation, CollationVersion::V4.into());
		let main_name = protocol_names.get_main_name(PeerSet::Collation);
		let fallbacks = protocol_names.get_fallback_names(PeerSet::Collation);

		assert_ne!(main_name, v4_name);
		assert!(!fallbacks.contains(&v4_name));
		// The regression test for the duplicate-/3 bug: main never appears in fallbacks.
		assert!(!fallbacks.contains(&main_name));
	}
}
