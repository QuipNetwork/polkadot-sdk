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

//! Schema for BEEFY state persisted in the aux-db.

use crate::{
	worker::{PersistedState, PersistedStateV4},
	LOG_TARGET,
};
use codec::{Decode, DecodeAll, Encode};
use log::{debug, trace, warn};
use sc_client_api::{backend::AuxStore, Backend};
use sp_blockchain::{Error as ClientError, Result as ClientResult};
use sp_consensus_beefy::AuthorityIdBound;
use sp_runtime::traits::Block as BlockT;

const VERSION_KEY: &[u8] = b"beefy_auxschema_version";
const WORKER_STATE_KEY: &[u8] = b"beefy_voter_state";

const CURRENT_VERSION: u32 = 5;

/// Write current schema version and voter state atomically.
pub(crate) fn write_current_version_and_voter_state<
	B: BlockT,
	BE: AuxStore,
	AuthorityId: AuthorityIdBound,
>(
	backend: &BE,
	state: &PersistedState<B, AuthorityId>,
) -> ClientResult<()> {
	debug!(target: LOG_TARGET, "🥩 write aux schema version {:?}", CURRENT_VERSION);
	trace!(target: LOG_TARGET, "🥩 persisting {:?}", state);

	let version = CURRENT_VERSION.encode();
	let state = state.encode();

	AuxStore::insert_aux(
		backend,
		&[(VERSION_KEY, version.as_slice()), (WORKER_STATE_KEY, state.as_slice())],
		&[],
	)
}

/// Write voter state.
pub(crate) fn write_voter_state<B: BlockT, BE: AuxStore, AuthorityId: AuthorityIdBound>(
	backend: &BE,
	state: &PersistedState<B, AuthorityId>,
) -> ClientResult<()> {
	trace!(target: LOG_TARGET, "🥩 persisting {:?}", state);
	AuxStore::insert_aux(backend, &[(WORKER_STATE_KEY, state.encode().as_slice())], &[])
}

fn load_decode<BE: AuxStore, T: Decode>(backend: &BE, key: &[u8]) -> ClientResult<Option<T>> {
	match backend.get_aux(key)? {
		None => Ok(None),
		Some(t) => T::decode_all(&mut &t[..])
			.map_err(|e| ClientError::Backend(format!("BEEFY DB is corrupted: {}", e)))
			.map(Some),
	}
}

/// Load persistent data from the backend, migrating older schemas when present.
///
/// If the backend contains an older supported schema, migrate it to the latest schema and save the
/// migrated state back to the aux-db.
pub(crate) fn load_and_migrate_persistent<B, BE, AuthorityId: AuthorityIdBound>(
	backend: &BE,
) -> ClientResult<Option<PersistedState<B, AuthorityId>>>
where
	B: BlockT,
	BE: Backend<B>,
{
	let version: Option<u32> = load_decode(backend, VERSION_KEY)?;

	match version {
		None => (),

		Some(v) if 1 <= v && v <= 3 =>
		// versions 1, 2 & 3 are obsolete and should be ignored
		{
			warn!(target: LOG_TARGET,  "🥩 backend contains a BEEFY state of an obsolete version {v}. ignoring...")
		},
		Some(4) => {
			let Some(old) =
				load_decode::<_, PersistedStateV4<B, AuthorityId>>(backend, WORKER_STATE_KEY)?
			else {
				// v4 marker present, but no state.
				return Ok(None);
			};

			let new_state: PersistedState<B, AuthorityId> = old.try_into()?;

			debug!(
				target: LOG_TARGET,
				"🥩 Migrating BEEFY aux-db schema v4 -> v5",
			);

			write_current_version_and_voter_state(backend, &new_state)?;

			// `new_state` and the freshly persisted bytes are equivalent (encode/decode is a
			// round-trip), so return the in-memory value directly and avoid the extra DB read.
			return Ok(Some(new_state));
		},
		Some(5) => {
			return load_decode::<_, PersistedState<B, AuthorityId>>(backend, WORKER_STATE_KEY)
		},
		other => {
			return Err(ClientError::Backend(format!("Unsupported BEEFY DB version: {:?}", other)))
		},
	}

	// No persistent state found in DB.
	Ok(None)
}

#[cfg(test)]
pub(crate) mod tests {
	use super::*;
	use crate::{
		round::{RoundTrackerV4, RoundsV4, VoteWeight},
		tests::BeefyTestNet,
		worker::VoterOracleV4,
	};
	use codec::DecodeAll;
	use sc_network_test::TestNetFactory;
	use sp_application_crypto::RuntimeAppPublic;
	use sp_consensus_beefy::{
		ecdsa_crypto, known_payloads, test_utils::Keyring, Commitment, Payload, ValidatorSet,
		VoteMessage,
	};
	use sp_core::H256;
	use sp_runtime::{
		generic::Digest,
		traits::{Header as HeaderT, NumberFor, Zero},
	};
	use std::{
		collections::{BTreeMap, VecDeque},
		marker::PhantomData,
	};
	use substrate_test_runtime_client as test_client;

	// also used in tests.rs
	pub fn verify_persisted_version<B: BlockT, BE: Backend<B>>(backend: &BE) -> bool {
		let version: u32 = load_decode(backend, VERSION_KEY).unwrap().unwrap();
		version == CURRENT_VERSION
	}

	#[tokio::test]
	async fn should_load_and_migrate_persistent_sanity_checks() {
		let mut net = BeefyTestNet::new(1);
		let backend = net.peer(0).client().as_backend();

		// version not available in db -> None
		assert_eq!(
			load_and_migrate_persistent::<test_client::runtime::Block, _, ecdsa_crypto::AuthorityId>(
				&*backend,
			)
			.unwrap(),
			None
		);

		// populate version in db
		AuxStore::insert_aux(&*backend, &[(VERSION_KEY, CURRENT_VERSION.encode().as_slice())], &[])
			.unwrap();
		// verify correct version is retrieved
		assert_eq!(load_decode(&*backend, VERSION_KEY).unwrap(), Some(CURRENT_VERSION));

		// version is available in db but state isn't -> None
		assert_eq!(
			load_and_migrate_persistent::<test_client::runtime::Block, _, ecdsa_crypto::AuthorityId>(
				&*backend,
			)
			.unwrap(),
			None
		);

		// full `PersistedState` load is tested in `tests.rs`.
	}

	#[tokio::test]
	async fn should_migrate_v4_to_v5() {
		type TestBlock = test_client::runtime::Block;
		type TestAuthority = ecdsa_crypto::AuthorityId;
		type TestSig = <TestAuthority as RuntimeAppPublic>::Signature;
		type PreviousVotes = BTreeMap<
			(TestAuthority, NumberFor<TestBlock>),
			VoteMessage<NumberFor<TestBlock>, TestAuthority, TestSig>,
		>;
		type MigratedRoundTracker = (BTreeMap<TestAuthority, TestSig>, VoteWeight);
		type MigratedRounds = (
			BTreeMap<Commitment<NumberFor<TestBlock>>, MigratedRoundTracker>,
			PreviousVotes,
			NumberFor<TestBlock>,
			ValidatorSet<TestAuthority>,
			BTreeMap<TestAuthority, VoteWeight>,
			bool,
			Option<NumberFor<TestBlock>>,
		);
		type MigratedVoterOracle = (
			VecDeque<MigratedRounds>,
			u32,
			<TestBlock as BlockT>::Header,
			NumberFor<TestBlock>,
			PhantomData<fn() -> TestAuthority>,
		);
		type MigratedState = (NumberFor<TestBlock>, MigratedVoterOracle, NumberFor<TestBlock>);

		let mut net = BeefyTestNet::new(1);
		let backend = net.peer(0).client().as_backend();
		let beefy_genesis: NumberFor<TestBlock> = 1;

		let validators = vec![
			Keyring::<TestAuthority>::Alice.public(),
			Keyring::<TestAuthority>::Alice.public(),
			Keyring::<TestAuthority>::Bob.public(),
		];
		let validator_set = ValidatorSet::new(validators, 0).unwrap();

		let best_grandpa = <test_client::runtime::Header as HeaderT>::new(
			beefy_genesis,
			Default::default(),
			Default::default(),
			H256::random(),
			Digest::default(),
		);
		let commitment: Commitment<NumberFor<TestBlock>> = Commitment {
			payload: Payload::from_single_entry(known_payloads::MMR_ROOT_ID, vec![]),
			block_number: beefy_genesis,
			validator_set_id: validator_set.id(),
		};
		let mut votes: BTreeMap<TestAuthority, TestSig> = BTreeMap::new();
		votes.insert(
			Keyring::<TestAuthority>::Alice.public(),
			Keyring::<TestAuthority>::Alice.sign(b"vote"),
		);
		// Hardcoded so the migration assertion is independent of the production helper that
		// derives weights from a validator set.
		let expected_voting_weights = BTreeMap::from([
			(Keyring::<TestAuthority>::Alice.public(), 2 as VoteWeight),
			(Keyring::<TestAuthority>::Bob.public(), 1 as VoteWeight),
		]);
		let tracker = RoundTrackerV4::<TestAuthority> { votes };
		let mut rounds_map: BTreeMap<
			Commitment<NumberFor<TestBlock>>,
			RoundTrackerV4<TestAuthority>,
		> = BTreeMap::new();
		rounds_map.insert(commitment.clone(), tracker);

		let voting_oracle = VoterOracleV4::<TestBlock, TestAuthority> {
			sessions: VecDeque::from([RoundsV4::<TestBlock, TestAuthority> {
				rounds: rounds_map,
				previous_votes: BTreeMap::<
					(TestAuthority, NumberFor<TestBlock>),
					VoteMessage<NumberFor<TestBlock>, TestAuthority, TestSig>,
				>::new(),
				session_start: beefy_genesis,
				validator_set: validator_set.clone(),
				mandatory_done: false,
				best_done: None,
			}]),
			min_block_delta: 1,
			best_grandpa_block_header: best_grandpa.clone(),
			best_beefy_block: Zero::zero(),
			_phantom: PhantomData,
		};

		let state_v4 = PersistedStateV4::<TestBlock, TestAuthority> {
			best_voted: Zero::zero(),
			voting_oracle,
			pallet_genesis: beefy_genesis,
		};
		let encoded_state_v4 = state_v4.encode();
		let encoded_version_v4 = 4u32.encode();

		AuxStore::insert_aux(
			&*backend,
			&[
				(VERSION_KEY, encoded_version_v4.as_slice()),
				(WORKER_STATE_KEY, encoded_state_v4.as_slice()),
			],
			&[],
		)
		.unwrap();

		assert_eq!(load_decode::<_, u32>(&*backend, VERSION_KEY).unwrap(), Some(4));

		let migrated = load_and_migrate_persistent::<TestBlock, _, TestAuthority>(&*backend)
			.unwrap()
			.expect("migration should produce a state; qed.");

		let (
			migrated_best_voted,
			(
				mut migrated_sessions,
				migrated_min_block_delta,
				migrated_best_grandpa,
				migrated_best_beefy,
				_phantom,
			),
			migrated_pallet_genesis,
		): MigratedState =
			DecodeAll::decode_all(&mut &*migrated.encode()).expect("decode migrated state; qed.");

		let zero: NumberFor<TestBlock> = Zero::zero();

		assert_eq!(migrated_best_voted, zero);
		assert_eq!(migrated_min_block_delta, 1);
		assert_eq!(migrated_best_grandpa, best_grandpa);
		assert_eq!(migrated_best_beefy, zero);
		assert_eq!(migrated_pallet_genesis, beefy_genesis);
		assert_eq!(migrated_sessions.len(), 1);

		let (
			migrated_rounds,
			migrated_previous_votes,
			migrated_session_start,
			migrated_validator_set,
			migrated_voting_weights,
			migrated_mandatory_done,
			migrated_best_done,
		) = migrated_sessions.pop_front().expect("session exists; checked above; qed.");

		assert_eq!(migrated_previous_votes, BTreeMap::new());
		assert_eq!(migrated_session_start, beefy_genesis);
		assert_eq!(migrated_validator_set, validator_set);
		assert_eq!(migrated_voting_weights, expected_voting_weights);
		assert!(!migrated_mandatory_done);
		assert_eq!(migrated_best_done, None);
		assert_eq!(migrated_rounds.len(), 1);

		let (migrated_votes, migrated_accumulated_votes_weight) =
			migrated_rounds.get(&commitment).expect("round exists; checked above; qed.");

		assert_eq!(migrated_votes.len(), 1);
		assert_eq!(
			migrated_votes.get(&Keyring::<TestAuthority>::Alice.public()),
			Some(&Keyring::<TestAuthority>::Alice.sign(b"vote"))
		);
		assert_eq!(*migrated_accumulated_votes_weight, 2);

		assert!(verify_persisted_version(&*backend));
	}
}
