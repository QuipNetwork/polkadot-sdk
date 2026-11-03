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

//! Small helpers reused across builders and scenarios.

use polkadot_primitives::{
	CollatorPair, HeadData, PersistedValidationData, ValidatorId, ValidatorIndex,
};
use sp_core::Pair as _;
use sp_keyring::Sr25519Keyring;

/// The canonical 5-validator key ring used by collator-protocol legacy tests.
pub fn default_validators() -> Vec<ValidatorId> {
	[
		Sr25519Keyring::Alice,
		Sr25519Keyring::Bob,
		Sr25519Keyring::Charlie,
		Sr25519Keyring::Dave,
		Sr25519Keyring::Eve,
	]
	.iter()
	.map(|k| k.public().into())
	.collect()
}

/// The canonical three-group split (2/2/1) used by legacy tests.
pub fn default_validator_groups() -> Vec<Vec<ValidatorIndex>> {
	vec![
		vec![ValidatorIndex(0), ValidatorIndex(1)],
		vec![ValidatorIndex(2), ValidatorIndex(3)],
		vec![ValidatorIndex(4)],
	]
}

/// A throw-away `CollatorPair` for use in `Peer` builders.
pub fn fresh_collator() -> CollatorPair {
	CollatorPair::generate().0
}

/// A neutral, structurally-valid persisted validation data the responder can return when the
/// subsystem asks (PVD shape only — none of the fields here are validated by the test asserts).
pub fn dummy_pvd() -> PersistedValidationData {
	PersistedValidationData {
		parent_head: HeadData(vec![7, 8, 9]),
		relay_parent_number: 5,
		max_pov_size: 1024,
		relay_parent_storage_root: Default::default(),
	}
}
