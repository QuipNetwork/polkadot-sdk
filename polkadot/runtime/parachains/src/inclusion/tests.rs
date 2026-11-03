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

use super::*;
use crate::{
	configuration::HostConfiguration,
	initializer::SessionChangeNotification,
	mock::{
		new_test_ext, MockGenesisConfig, ParaInclusion, Paras, ParasShared, Scheduler, System, Test,
	},
	paras::{ParaGenesisArgs, ParaKind},
	paras_inherent::DisputedBitfield,
	shared::AllowedSchedulingParentsTracker,
};
use polkadot_primitives::{
	effective_minimum_backing_votes, vstaging::RelayParentInfo, AvailabilityBitfield,
	CandidateDescriptorV2, CandidateDescriptorVersion, ClaimQueueOffset, CoreSelector,
	SessionIndex, SignedAvailabilityBitfields, UMPSignal, UncheckedSignedAvailabilityBitfields,
	UMP_SEPARATOR,
};

use assert_matches::assert_matches;
use codec::DecodeAll;
use frame_support::assert_noop;
use polkadot_primitives::{
	BlockNumber, CandidateCommitments, CollatorId, CollatorSignature,
	CompactStatement as Statement, Hash, MutateDescriptorV2, SignedAvailabilityBitfield,
	SignedStatement, ValidationCode, ValidatorId, ValidityAttestation, PARACHAIN_KEY_TYPE_ID,
};
use polkadot_primitives_test_helpers::{dummy_validation_code, CandidateDescriptor};
use sc_keystore::LocalKeystore;
use sp_core::ByteArray;
use sp_keyring::Sr25519Keyring;
use sp_keystore::{Keystore, KeystorePtr};
use std::sync::Arc;

fn default_config() -> HostConfiguration<BlockNumber> {
	let mut config = HostConfiguration::default();
	config.scheduler_params.num_cores = 1;
	config.max_code_size = 0b100000;
	config.max_head_data_size = 0b100000;
	config.scheduler_params.group_rotation_frequency = u32::MAX;
	config
}

pub(crate) fn genesis_config(paras: Vec<(ParaId, ParaKind)>) -> MockGenesisConfig {
	MockGenesisConfig {
		paras: paras::GenesisConfig {
			paras: paras
				.into_iter()
				.map(|(id, para_kind)| {
					(
						id,
						ParaGenesisArgs {
							genesis_head: Vec::new().into(),
							validation_code: dummy_validation_code(),
							para_kind,
						},
					)
				})
				.collect(),
			..Default::default()
		},
		configuration: configuration::GenesisConfig { config: default_config() },
		..Default::default()
	}
}

fn default_allowed_scheduling_parent_tracker() -> AllowedSchedulingParentsTracker<Hash, BlockNumber>
{
	let mut allowed = AllowedSchedulingParentsTracker::default();

	let relay_parent = System::parent_hash();
	let parent_number = System::block_number().saturating_sub(1);

	allowed.update(relay_parent, Default::default(), parent_number, 1);

	// Also populate the AllowedRelayParents DoubleMap so that
	// verify_backed_candidate can look up the relay parent info.
	let session_index = shared::CurrentSessionIndex::<Test>::get();
	shared::AllowedRelayParents::<Test>::insert(
		session_index,
		relay_parent,
		RelayParentInfo { number: parent_number, state_root: Default::default() },
	);

	allowed
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum BackingKind {
	#[allow(unused)]
	Unanimous,
	Threshold,
	Lacking,
}

pub(crate) fn back_candidate(
	candidate: CommittedCandidateReceipt,
	validators: &[Sr25519Keyring],
	group: &[ValidatorIndex],
	keystore: &KeystorePtr,
	signing_context: &SigningContext,
	kind: BackingKind,
	core_index: CoreIndex,
) -> BackedCandidate {
	let mut validator_indices = bitvec::bitvec![u8, BitOrderLsb0; 0; group.len()];
	let threshold = effective_minimum_backing_votes(
		group.len(),
		configuration::ActiveConfig::<Test>::get().minimum_backing_votes,
	);

	let signing = match kind {
		BackingKind::Unanimous => group.len(),
		BackingKind::Threshold => threshold as usize,
		BackingKind::Lacking => threshold.saturating_sub(1),
	};

	let mut validity_votes = Vec::with_capacity(signing);
	let candidate_hash = candidate.hash();

	for (idx_in_group, val_idx) in group.iter().enumerate().take(signing) {
		let key: Sr25519Keyring = validators[val_idx.0 as usize];
		*validator_indices.get_mut(idx_in_group).unwrap() = true;

		let signature = SignedStatement::sign(
			&keystore,
			Statement::Valid(candidate_hash),
			signing_context,
			*val_idx,
			&key.public().into(),
		)
		.unwrap()
		.unwrap()
		.signature()
		.clone();

		validity_votes.push(ValidityAttestation::Explicit(signature).into());
	}

	let backed =
		BackedCandidate::new(candidate, validity_votes, validator_indices.clone(), core_index);

	let successfully_backed = polkadot_primitives::check_candidate_backing(
		backed.candidate().hash(),
		backed.validity_votes(),
		validator_indices.as_bitslice(),
		signing_context,
		group.len(),
		|i| Some(validators[group[i].0 as usize].public().into()),
	)
	.ok()
	.unwrap_or(0) >=
		threshold;

	match kind {
		BackingKind::Unanimous | BackingKind::Threshold => assert!(successfully_backed),
		BackingKind::Lacking => assert!(!successfully_backed),
	};

	backed
}

pub(crate) fn run_to_block_default_notifications(to: BlockNumber, new_session: Vec<BlockNumber>) {
	run_to_block(to, |b| {
		new_session.contains(&b).then_some(SessionChangeNotification {
			prev_config: configuration::ActiveConfig::<Test>::get(),
			new_config: configuration::ActiveConfig::<Test>::get(),
			session_index: shared::CurrentSessionIndex::<Test>::get() + 1,
			..Default::default()
		})
	});
}

pub(crate) fn run_to_block(
	to: BlockNumber,
	new_session: impl Fn(BlockNumber) -> Option<SessionChangeNotification<BlockNumber>>,
) {
	while System::block_number() < to {
		let b = System::block_number();

		ParaInclusion::initializer_finalize();
		Paras::initializer_finalize(b);
		ParasShared::initializer_finalize();

		if let Some(notification) = new_session(b + 1) {
			ParasShared::initializer_on_new_session(
				notification.session_index,
				notification.random_seed,
				&notification.new_config,
				notification.validators.clone(),
			);
			let outgoing = Paras::initializer_on_new_session(&notification);
			ParaInclusion::initializer_on_new_session(&notification, &outgoing);
		}

		System::on_finalize(b);

		System::on_initialize(b + 1);
		System::set_block_number(b + 1);

		ParasShared::initializer_initialize(b + 1);
		Paras::initializer_initialize(b + 1);
		ParaInclusion::initializer_initialize(b + 1);
	}
}

pub(crate) fn expected_bits() -> usize {
	paras::Parachains::<Test>::get().len() +
		configuration::ActiveConfig::<Test>::get().scheduler_params.num_cores as usize
}

fn default_bitfield() -> AvailabilityBitfield {
	AvailabilityBitfield(bitvec::bitvec![u8, BitOrderLsb0; 0; expected_bits()])
}

fn default_availability_votes() -> BitVec<u8, BitOrderLsb0> {
	bitvec::bitvec![u8, BitOrderLsb0; 0; shared::ActiveValidatorKeys::<Test>::get().len()]
}

fn default_backing_bitfield() -> BitVec<u8, BitOrderLsb0> {
	bitvec::bitvec![u8, BitOrderLsb0; 0; shared::ActiveValidatorKeys::<Test>::get().len()]
}

fn backing_bitfield(v: &[usize]) -> BitVec<u8, BitOrderLsb0> {
	let mut b = default_backing_bitfield();
	for i in v {
		b.set(*i, true);
	}
	b
}

pub(crate) fn validator_pubkeys(val_ids: &[Sr25519Keyring]) -> Vec<ValidatorId> {
	val_ids.iter().map(|v| v.public().into()).collect()
}

pub(crate) fn sign_bitfield(
	keystore: &KeystorePtr,
	key: &Sr25519Keyring,
	validator_index: ValidatorIndex,
	bitfield: AvailabilityBitfield,
	signing_context: &SigningContext,
) -> SignedAvailabilityBitfield {
	SignedAvailabilityBitfield::sign(
		&keystore,
		bitfield,
		&signing_context,
		validator_index,
		&key.public().into(),
	)
	.unwrap()
	.unwrap()
}

pub(crate) struct TestCandidateBuilder {
	pub(crate) para_id: ParaId,
	pub(crate) head_data: HeadData,
	pub(crate) para_head_hash: Option<Hash>,
	pub(crate) pov_hash: Hash,
	pub(crate) relay_parent: Hash,
	pub(crate) persisted_validation_data_hash: Hash,
	pub(crate) new_validation_code: Option<ValidationCode>,
	pub(crate) validation_code: ValidationCode,
	pub(crate) hrmp_watermark: BlockNumber,
	/// Creates a v2 descriptor if set.
	pub(crate) core_index: Option<CoreIndex>,
	/// The core selector to use.
	pub(crate) core_selector: Option<u8>,
	/// Creates a v3 descriptor if set (requires core_index to also be set).
	/// The scheduling_parent is the relay chain block that determines scheduling.
	pub(crate) scheduling_parent: Option<Hash>,
	/// Session index to embed in the V2/V3 descriptor.
	pub(crate) descriptor_session_index: Option<SessionIndex>,
	/// Scheduling session offset for V3 descriptors.
	/// scheduling_session = descriptor_session_index + scheduling_session_offset.
	pub(crate) scheduling_session_offset: Option<u8>,
}

impl std::default::Default for TestCandidateBuilder {
	fn default() -> Self {
		let zeros = Hash::zero();
		Self {
			para_id: 0.into(),
			head_data: Default::default(),
			para_head_hash: None,
			pov_hash: zeros,
			relay_parent: zeros,
			persisted_validation_data_hash: zeros,
			new_validation_code: None,
			validation_code: dummy_validation_code(),
			hrmp_watermark: 0u32.into(),
			core_index: None,
			core_selector: None,
			scheduling_parent: None,
			descriptor_session_index: None,
			scheduling_session_offset: None,
		}
	}
}

impl TestCandidateBuilder {
	pub(crate) fn build(self) -> CommittedCandidateReceipt {
		let descriptor = if let (Some(core_index), Some(scheduling_parent)) =
			(self.core_index, self.scheduling_parent)
		{
			// V3 descriptor with explicit scheduling_parent.
			CandidateDescriptorV2::new_v3(
				self.para_id,
				self.relay_parent,
				core_index,
				self.descriptor_session_index.unwrap_or(0),
				self.descriptor_session_index.unwrap_or(0), // scheduling_session_index
				self.persisted_validation_data_hash,
				self.pov_hash,
				Default::default(),
				self.para_head_hash.unwrap_or_else(|| self.head_data.hash()),
				self.validation_code.hash(),
				scheduling_parent,
			)
		} else if let Some(core_index) = self.core_index {
			CandidateDescriptorV2::new(
				self.para_id,
				self.relay_parent,
				core_index,
				self.descriptor_session_index.unwrap_or(0),
				self.persisted_validation_data_hash,
				self.pov_hash,
				Default::default(),
				self.para_head_hash.unwrap_or_else(|| self.head_data.hash()),
				self.validation_code.hash(),
			)
		} else {
			CandidateDescriptor {
				para_id: self.para_id,
				pov_hash: self.pov_hash,
				relay_parent: self.relay_parent,
				persisted_validation_data_hash: self.persisted_validation_data_hash,
				validation_code_hash: self.validation_code.hash(),
				para_head: self.para_head_hash.unwrap_or_else(|| self.head_data.hash()),
				erasure_root: Default::default(),
				signature: CollatorSignature::from_slice(
					&mut (0..64).into_iter().collect::<Vec<_>>().as_slice(),
				)
				.expect("64 bytes; qed"),
				collator: CollatorId::from_slice(
					&mut (0..32).into_iter().collect::<Vec<_>>().as_slice(),
				)
				.expect("32 bytes; qed"),
			}
			.into()
		};
		let mut ccr = CommittedCandidateReceipt {
			descriptor,
			commitments: CandidateCommitments {
				head_data: self.head_data,
				new_validation_code: self.new_validation_code,
				hrmp_watermark: self.hrmp_watermark,
				..Default::default()
			},
		};

		// Apply scheduling_session_offset for V3 descriptors if specified.
		if let Some(offset) = self.scheduling_session_offset {
			ccr.descriptor.set_scheduling_session_offset(offset);
		}

		let version = ccr.descriptor.version();
		if version == CandidateDescriptorVersion::V2 || version == CandidateDescriptorVersion::V3 {
			ccr.commitments.upward_messages.force_push(UMP_SEPARATOR);

			ccr.commitments.upward_messages.force_push(
				UMPSignal::SelectCore(
					CoreSelector(self.core_selector.unwrap_or_default()),
					ClaimQueueOffset(0),
				)
				.encode(),
			);
		}

		ccr
	}
}

pub(crate) fn make_vdata_hash(para_id: ParaId) -> Option<Hash> {
	let relay_parent_number = frame_system::Pallet::<Test>::block_number() - 1;
	make_vdata_hash_with_block_number(para_id, relay_parent_number)
}

fn make_vdata_hash_with_block_number(
	para_id: ParaId,
	relay_parent_number: BlockNumber,
) -> Option<Hash> {
	let persisted_validation_data = crate::util::make_persisted_validation_data::<Test>(
		para_id,
		relay_parent_number,
		Default::default(),
	)?;
	Some(persisted_validation_data.hash())
}

/// Wrapper around `sanitize_bitfields` with less parameters.
fn simple_sanitize_bitfields(
	unchecked_bitfields: UncheckedSignedAvailabilityBitfields,
	disputed_bitfield: DisputedBitfield,
	expected_bits: usize,
) -> SignedAvailabilityBitfields {
	let parent_hash = frame_system::Pallet::<Test>::parent_hash();
	let session_index = shared::CurrentSessionIndex::<Test>::get();
	let validators = shared::ActiveValidatorKeys::<Test>::get();

	crate::paras_inherent::sanitize_bitfields::<Test>(
		unchecked_bitfields,
		disputed_bitfield,
		expected_bits,
		parent_hash,
		session_index,
		&validators,
	)
}
/// Process a set of already sanitized bitfields.
pub(crate) fn process_bitfields(
	signed_bitfields: SignedAvailabilityBitfields,
) -> Vec<(CoreIndex, CandidateHash)> {
	let validators = shared::ActiveValidatorKeys::<Test>::get();

	let (_weight, bitfields) = ParaInclusion::update_pending_availability_and_get_freed_cores(
		&validators[..],
		signed_bitfields,
	);
	bitfields
}

#[test]
fn free_timedout() {
	let chain_a = ParaId::from(1_u32);
	let chain_b = ParaId::from(2_u32);
	let chain_c = ParaId::from(3_u32);
	let chain_d = ParaId::from(4_u32);
	let chain_e = ParaId::from(5_u32);
	let chain_f = ParaId::from(6_u32);
	let thread_a = ParaId::from(7_u32);

	let paras = vec![
		(chain_a, ParaKind::Parachain),
		(chain_b, ParaKind::Parachain),
		(chain_c, ParaKind::Parachain),
		(chain_d, ParaKind::Parachain),
		(chain_e, ParaKind::Parachain),
		(chain_f, ParaKind::Parachain),
		(thread_a, ParaKind::Parathread),
	];
	let mut config = genesis_config(paras);
	config.configuration.config.scheduler_params.group_rotation_frequency = 3;
	new_test_ext(config).execute_with(|| {
		let timed_out_cores = ParaInclusion::free_timedout();
		assert!(timed_out_cores.is_empty());

		let make_candidate = |core_index: u32, timed_out: bool| {
			let default_candidate = TestCandidateBuilder::default().build();
			let backed_in_number = if timed_out { 0 } else { 5 };

			CandidatePendingAvailability {
				core: CoreIndex::from(core_index),
				hash: default_candidate.hash(),
				descriptor: default_candidate.descriptor.clone(),
				availability_votes: default_availability_votes(),
				relay_parent_number: 0,
				backed_in_number,
				backers: default_backing_bitfield(),
				backing_group: GroupIndex::from(core_index),
				commitments: default_candidate.commitments.clone(),
			}
		};

		PendingAvailability::<Test>::insert(
			chain_a,
			[make_candidate(0, true)].into_iter().collect::<VecDeque<_>>(),
		);

		PendingAvailability::<Test>::insert(
			&chain_b,
			[make_candidate(1, false)].into_iter().collect::<VecDeque<_>>(),
		);

		// 2 chained candidates. The first one is timed out. The other will be evicted also.
		let mut c_candidates = VecDeque::new();
		c_candidates.push_back(make_candidate(2, true));
		c_candidates.push_back(make_candidate(3, false));

		PendingAvailability::<Test>::insert(&chain_c, c_candidates);

		// 2 chained candidates. All are timed out.
		let mut d_candidates = VecDeque::new();
		d_candidates.push_back(make_candidate(4, true));
		d_candidates.push_back(make_candidate(5, true));

		PendingAvailability::<Test>::insert(&chain_d, d_candidates);

		// 3 chained candidates. The second one is timed out. The first one will remain in place.
		// With the current time out predicate this scenario is impossible. But this is not a
		// concern for this module.
		let mut e_candidates = VecDeque::new();
		e_candidates.push_back(make_candidate(6, false));
		e_candidates.push_back(make_candidate(7, true));
		e_candidates.push_back(make_candidate(8, false));

		PendingAvailability::<Test>::insert(&chain_e, e_candidates);

		// 3 chained candidates, none are timed out.
		let mut f_candidates = VecDeque::new();
		f_candidates.push_back(make_candidate(9, false));
		f_candidates.push_back(make_candidate(10, false));
		f_candidates.push_back(make_candidate(11, false));

		PendingAvailability::<Test>::insert(&chain_f, f_candidates);

		run_to_block(5, |_| None);

		assert_eq!(PendingAvailability::<Test>::get(&chain_a).unwrap().len(), 1);
		assert_eq!(PendingAvailability::<Test>::get(&chain_b).unwrap().len(), 1);
		assert_eq!(PendingAvailability::<Test>::get(&chain_c).unwrap().len(), 2);
		assert_eq!(PendingAvailability::<Test>::get(&chain_d).unwrap().len(), 2);
		assert_eq!(PendingAvailability::<Test>::get(&chain_e).unwrap().len(), 3);
		assert_eq!(PendingAvailability::<Test>::get(&chain_f).unwrap().len(), 3);

		let timed_out_cores = ParaInclusion::free_timedout();

		assert_eq!(
			timed_out_cores,
			vec![
				CoreIndex(0),
				CoreIndex(2),
				CoreIndex(3),
				CoreIndex(4),
				CoreIndex(5),
				CoreIndex(7),
				CoreIndex(8),
			]
		);

		assert!(PendingAvailability::<Test>::get(&chain_a).unwrap().is_empty());
		assert_eq!(PendingAvailability::<Test>::get(&chain_b).unwrap().len(), 1);
		assert!(PendingAvailability::<Test>::get(&chain_c).unwrap().is_empty());
		assert!(PendingAvailability::<Test>::get(&chain_d).unwrap().is_empty());
		assert_eq!(
			PendingAvailability::<Test>::get(&chain_e)
				.unwrap()
				.into_iter()
				.map(|c| c.core)
				.collect::<Vec<_>>(),
			vec![CoreIndex(6)]
		);
		assert_eq!(
			PendingAvailability::<Test>::get(&chain_f)
				.unwrap()
				.into_iter()
				.map(|c| c.core)
				.collect::<Vec<_>>(),
			vec![CoreIndex(9), CoreIndex(10), CoreIndex(11)]
		);
	});
}

#[test]
fn free_disputed() {
	let chain_a = ParaId::from(1_u32);
	let chain_b = ParaId::from(2_u32);
	let chain_c = ParaId::from(3_u32);
	let chain_d = ParaId::from(4_u32);
	let chain_e = ParaId::from(5_u32);
	let chain_f = ParaId::from(6_u32);
	let thread_a = ParaId::from(7_u32);

	let paras = vec![
		(chain_a, ParaKind::Parachain),
		(chain_b, ParaKind::Parachain),
		(chain_c, ParaKind::Parachain),
		(chain_d, ParaKind::Parachain),
		(chain_e, ParaKind::Parachain),
		(chain_f, ParaKind::Parachain),
		(thread_a, ParaKind::Parathread),
	];
	let mut config = genesis_config(paras);
	config.configuration.config.scheduler_params.group_rotation_frequency = 3;
	new_test_ext(config).execute_with(|| {
		let disputed_cores = ParaInclusion::free_disputed(&BTreeSet::new());
		assert!(disputed_cores.is_empty());

		let disputed_cores = ParaInclusion::free_disputed(
			&[CandidateHash::default()].into_iter().collect::<BTreeSet<_>>(),
		);
		assert!(disputed_cores.is_empty());

		let make_candidate = |core_index: u32| {
			let default_candidate = TestCandidateBuilder::default().build();

			CandidatePendingAvailability {
				core: CoreIndex::from(core_index),
				hash: CandidateHash(Hash::from_low_u64_be(core_index as _)),
				descriptor: default_candidate.descriptor.clone(),
				availability_votes: default_availability_votes(),
				relay_parent_number: 0,
				backed_in_number: 0,
				backers: default_backing_bitfield(),
				backing_group: GroupIndex::from(core_index),
				commitments: default_candidate.commitments.clone(),
			}
		};

		// Disputed
		PendingAvailability::<Test>::insert(
			chain_a,
			[make_candidate(0)].into_iter().collect::<VecDeque<_>>(),
		);

		// Not disputed.
		PendingAvailability::<Test>::insert(
			&chain_b,
			[make_candidate(1)].into_iter().collect::<VecDeque<_>>(),
		);

		// 2 chained candidates. The first one is disputed. The other will be evicted also.
		let mut c_candidates = VecDeque::new();
		c_candidates.push_back(make_candidate(2));
		c_candidates.push_back(make_candidate(3));

		PendingAvailability::<Test>::insert(&chain_c, c_candidates);

		// 2 chained candidates. All are disputed.
		let mut d_candidates = VecDeque::new();
		d_candidates.push_back(make_candidate(4));
		d_candidates.push_back(make_candidate(5));

		PendingAvailability::<Test>::insert(&chain_d, d_candidates);

		// 3 chained candidates. The second one is disputed. The first one will remain in place.
		let mut e_candidates = VecDeque::new();
		e_candidates.push_back(make_candidate(6));
		e_candidates.push_back(make_candidate(7));
		e_candidates.push_back(make_candidate(8));

		PendingAvailability::<Test>::insert(&chain_e, e_candidates);

		// 3 chained candidates, none are disputed.
		let mut f_candidates = VecDeque::new();
		f_candidates.push_back(make_candidate(9));
		f_candidates.push_back(make_candidate(10));
		f_candidates.push_back(make_candidate(11));

		PendingAvailability::<Test>::insert(&chain_f, f_candidates);

		run_to_block(5, |_| None);

		assert_eq!(PendingAvailability::<Test>::get(&chain_a).unwrap().len(), 1);
		assert_eq!(PendingAvailability::<Test>::get(&chain_b).unwrap().len(), 1);
		assert_eq!(PendingAvailability::<Test>::get(&chain_c).unwrap().len(), 2);
		assert_eq!(PendingAvailability::<Test>::get(&chain_d).unwrap().len(), 2);
		assert_eq!(PendingAvailability::<Test>::get(&chain_e).unwrap().len(), 3);
		assert_eq!(PendingAvailability::<Test>::get(&chain_f).unwrap().len(), 3);

		let disputed_candidates = [
			CandidateHash(Hash::from_low_u64_be(0)),
			CandidateHash(Hash::from_low_u64_be(2)),
			CandidateHash(Hash::from_low_u64_be(4)),
			CandidateHash(Hash::from_low_u64_be(5)),
			CandidateHash(Hash::from_low_u64_be(7)),
		]
		.into_iter()
		.collect::<BTreeSet<_>>();
		let disputed_cores = ParaInclusion::free_disputed(&disputed_candidates);

		assert_eq!(
			disputed_cores.into_iter().map(|(core, _)| core).collect::<Vec<_>>(),
			vec![
				CoreIndex(0),
				CoreIndex(2),
				CoreIndex(3),
				CoreIndex(4),
				CoreIndex(5),
				CoreIndex(7),
				CoreIndex(8),
			]
		);

		assert!(PendingAvailability::<Test>::get(&chain_a).unwrap().is_empty());
		assert_eq!(PendingAvailability::<Test>::get(&chain_b).unwrap().len(), 1);
		assert!(PendingAvailability::<Test>::get(&chain_c).unwrap().is_empty());
		assert!(PendingAvailability::<Test>::get(&chain_d).unwrap().is_empty());
		assert_eq!(
			PendingAvailability::<Test>::get(&chain_e)
				.unwrap()
				.into_iter()
				.map(|c| c.core)
				.collect::<Vec<_>>(),
			vec![CoreIndex(6)]
		);
		assert_eq!(
			PendingAvailability::<Test>::get(&chain_f)
				.unwrap()
				.into_iter()
				.map(|c| c.core)
				.collect::<Vec<_>>(),
			vec![CoreIndex(9), CoreIndex(10), CoreIndex(11)]
		);
	});
}

#[test]
fn bitfield_checks() {
	let chain_a = ParaId::from(1_u32);
	let chain_b = ParaId::from(2_u32);
	let thread_a = ParaId::from(3_u32);

	let paras = vec![
		(chain_a, ParaKind::Parachain),
		(chain_b, ParaKind::Parachain),
		(thread_a, ParaKind::Parathread),
	];
	let validators = vec![
		Sr25519Keyring::Alice,
		Sr25519Keyring::Bob,
		Sr25519Keyring::Charlie,
		Sr25519Keyring::Dave,
		Sr25519Keyring::Ferdie,
	];
	let keystore: KeystorePtr = Arc::new(LocalKeystore::in_memory());
	for validator in validators.iter() {
		Keystore::sr25519_generate_new(
			&*keystore,
			PARACHAIN_KEY_TYPE_ID,
			Some(&validator.to_seed()),
		)
		.unwrap();
	}
	let validator_public = validator_pubkeys(&validators);

	new_test_ext(genesis_config(paras.clone())).execute_with(|| {
		shared::Pallet::<Test>::set_active_validators_ascending(validator_public.clone());
		shared::Pallet::<Test>::set_session_index(5);

		let signing_context =
			SigningContext { parent_hash: System::parent_hash(), session_index: 5 };

		// too many bits in bitfield
		{
			let mut bare_bitfield = default_bitfield();
			bare_bitfield.0.push(false);
			let signed = sign_bitfield(
				&keystore,
				&validators[0],
				ValidatorIndex(0),
				bare_bitfield,
				&signing_context,
			);

			let checked_bitfields = simple_sanitize_bitfields(
				vec![signed.into()],
				DisputedBitfield::zeros(expected_bits()),
				expected_bits(),
			);
			assert_eq!(
				checked_bitfields.len(),
				0,
				"Bitfield has wrong size, it should have been filtered."
			);
		}

		// not enough bits
		{
			let bare_bitfield = default_bitfield();
			let signed = sign_bitfield(
				&keystore,
				&validators[0],
				ValidatorIndex(0),
				bare_bitfield,
				&signing_context,
			);

			let checked_bitfields = simple_sanitize_bitfields(
				vec![signed.into()],
				DisputedBitfield::zeros(expected_bits()),
				expected_bits() + 1,
			);
			assert_eq!(
				checked_bitfields.len(),
				0,
				"Bitfield has wrong size, it should have been filtered."
			);
		}

		// non-pending bit set.
		{
			let mut bare_bitfield = default_bitfield();
			*bare_bitfield.0.get_mut(0).unwrap() = true;
			let signed = sign_bitfield(
				&keystore,
				&validators[0],
				ValidatorIndex(0),
				bare_bitfield,
				&signing_context,
			);

			// the threshold to free a core is 4 availability votes, but we only expect 1 valid
			// valid bitfield because `signed_0` will get skipped for being out of order.
			let checked_bitfields = simple_sanitize_bitfields(
				vec![signed.into()],
				DisputedBitfield::zeros(expected_bits()),
				expected_bits(),
			);
			assert_eq!(checked_bitfields.len(), 1, "No bitfields should have been filtered!");

			let x = process_bitfields(checked_bitfields);
			assert!(x.is_empty(), "No core should be freed.");
		}

		// empty bitfield signed: always ok, but kind of useless.
		{
			let bare_bitfield = default_bitfield();
			let signed = sign_bitfield(
				&keystore,
				&validators[0],
				ValidatorIndex(0),
				bare_bitfield,
				&signing_context,
			);
			let checked_bitfields = simple_sanitize_bitfields(
				vec![signed.into()],
				DisputedBitfield::zeros(expected_bits()),
				expected_bits(),
			);
			assert_eq!(checked_bitfields.len(), 1, "No bitfields should have been filtered!");

			let x = process_bitfields(checked_bitfields);
			assert!(x.is_empty(), "No core should be freed.");
		}

		// bitfield signed with pending bit signed.
		{
			let mut bare_bitfield = default_bitfield();

			let default_candidate = TestCandidateBuilder::default().build();
			PendingAvailability::<Test>::insert(
				chain_a,
				[CandidatePendingAvailability {
					core: CoreIndex::from(0),
					hash: default_candidate.hash(),
					descriptor: default_candidate.descriptor,
					availability_votes: default_availability_votes(),
					relay_parent_number: 0,
					backed_in_number: 0,
					backers: default_backing_bitfield(),
					backing_group: GroupIndex::from(0),
					commitments: default_candidate.commitments,
				}]
				.into_iter()
				.collect::<VecDeque<_>>(),
			);

			*bare_bitfield.0.get_mut(0).unwrap() = true;
			let signed = sign_bitfield(
				&keystore,
				&validators[0],
				ValidatorIndex(0),
				bare_bitfield,
				&signing_context,
			);

			let checked_bitfields = simple_sanitize_bitfields(
				vec![signed.into()],
				DisputedBitfield::zeros(expected_bits()),
				expected_bits(),
			);
			assert_eq!(checked_bitfields.len(), 1, "No bitfields should have been filtered!");

			let x = process_bitfields(checked_bitfields);
			assert!(x.is_empty(), "No core should be freed.");

			PendingAvailability::<Test>::remove(chain_a);
		}
	});
}

#[test]
fn availability_threshold_is_supermajority() {
	assert_eq!(3, availability_threshold(4));
	assert_eq!(5, availability_threshold(6));
	assert_eq!(7, availability_threshold(9));
}

#[test]
fn supermajority_bitfields_trigger_availability() {
	let chain_a = ParaId::from(0_u32);
	let chain_b = ParaId::from(1_u32);
	let chain_c = ParaId::from(2_u32);
	let chain_d = ParaId::from(3_u32);
	let thread_a = ParaId::from(4_u32);

	let paras = vec![
		(chain_a, ParaKind::Parachain),
		(chain_b, ParaKind::Parachain),
		(chain_c, ParaKind::Parachain),
		(chain_d, ParaKind::Parachain),
		(thread_a, ParaKind::Parathread),
	];
	let validators = vec![
		Sr25519Keyring::Alice,
		Sr25519Keyring::Bob,
		Sr25519Keyring::Charlie,
		Sr25519Keyring::Dave,
		Sr25519Keyring::Ferdie,
		Sr25519Keyring::One,
		Sr25519Keyring::Two,
	];
	let keystore: KeystorePtr = Arc::new(LocalKeystore::in_memory());
	for validator in validators.iter() {
		Keystore::sr25519_generate_new(
			&*keystore,
			PARACHAIN_KEY_TYPE_ID,
			Some(&validator.to_seed()),
		)
		.unwrap();
	}
	let validator_public = validator_pubkeys(&validators);

	new_test_ext(genesis_config(paras)).execute_with(|| {
		shared::Pallet::<Test>::set_active_validators_ascending(validator_public.clone());
		shared::Pallet::<Test>::set_session_index(5);

		let signing_context =
			SigningContext { parent_hash: System::parent_hash(), session_index: 5 };

		// Chain A only has one candidate pending availability. It will be made available now.
		let candidate_a = TestCandidateBuilder {
			para_id: chain_a,
			head_data: vec![1, 2, 3, 4].into(),
			..Default::default()
		}
		.build();

		PendingAvailability::<Test>::insert(
			chain_a,
			[CandidatePendingAvailability {
				core: CoreIndex::from(0),
				hash: candidate_a.hash(),
				descriptor: candidate_a.clone().descriptor,
				availability_votes: default_availability_votes(),
				relay_parent_number: 0,
				backed_in_number: 0,
				backers: backing_bitfield(&[3, 4]),
				backing_group: GroupIndex::from(0),
				commitments: candidate_a.clone().commitments,
			}]
			.into_iter()
			.collect::<VecDeque<_>>(),
		);

		// Chain B only has one candidate pending availability. It won't be made available now.
		let candidate_b = TestCandidateBuilder {
			para_id: chain_b,
			head_data: vec![5, 6, 7, 8].into(),
			..Default::default()
		}
		.build();

		PendingAvailability::<Test>::insert(
			chain_b,
			[CandidatePendingAvailability {
				core: CoreIndex::from(1),
				hash: candidate_b.hash(),
				descriptor: candidate_b.descriptor,
				availability_votes: default_availability_votes(),
				relay_parent_number: 0,
				backed_in_number: 0,
				backers: backing_bitfield(&[0, 2]),
				backing_group: GroupIndex::from(1),
				commitments: candidate_b.commitments,
			}]
			.into_iter()
			.collect::<VecDeque<_>>(),
		);

		// Chain C has three candidates pending availability. The first and third candidates will be
		// made available. Only the first candidate will be evicted from the core and enacted.
		let candidate_c_1 = TestCandidateBuilder {
			para_id: chain_c,
			head_data: vec![7, 8].into(),
			..Default::default()
		}
		.build();
		let candidate_c_2 = TestCandidateBuilder {
			para_id: chain_c,
			head_data: vec![9, 10].into(),
			..Default::default()
		}
		.build();
		let candidate_c_3 = TestCandidateBuilder {
			para_id: chain_c,
			head_data: vec![11, 12].into(),
			..Default::default()
		}
		.build();

		let mut c_candidates = VecDeque::new();
		c_candidates.push_back(CandidatePendingAvailability {
			core: CoreIndex::from(2),
			hash: candidate_c_1.hash(),
			descriptor: candidate_c_1.descriptor.clone(),
			availability_votes: default_availability_votes(),
			relay_parent_number: 0,
			backed_in_number: 0,
			backers: backing_bitfield(&[1]),
			backing_group: GroupIndex::from(2),
			commitments: candidate_c_1.commitments.clone(),
		});
		c_candidates.push_back(CandidatePendingAvailability {
			core: CoreIndex::from(3),
			hash: candidate_c_2.hash(),
			descriptor: candidate_c_2.descriptor.clone(),
			availability_votes: default_availability_votes(),
			relay_parent_number: 0,
			backed_in_number: 0,
			backers: backing_bitfield(&[5]),
			backing_group: GroupIndex::from(3),
			commitments: candidate_c_2.commitments.clone(),
		});
		c_candidates.push_back(CandidatePendingAvailability {
			core: CoreIndex::from(4),
			hash: candidate_c_3.hash(),
			descriptor: candidate_c_3.descriptor.clone(),
			availability_votes: default_availability_votes(),
			relay_parent_number: 0,
			backed_in_number: 0,
			backers: backing_bitfield(&[6]),
			backing_group: GroupIndex::from(4),
			commitments: candidate_c_3.commitments.clone(),
		});

		PendingAvailability::<Test>::insert(chain_c, c_candidates);

		// this bitfield signals that a and b are available.
		let all_available = {
			let mut bare_bitfield = default_bitfield();
			for bit in 0..=4 {
				*bare_bitfield.0.get_mut(bit).unwrap() = true;
			}

			bare_bitfield
		};

		let threshold = availability_threshold(validators.len());

		// 5 of 7 first value >= 2/3
		assert_eq!(threshold, 5);

		let signed_bitfields = validators
			.iter()
			.enumerate()
			.filter_map(|(i, key)| {
				let to_sign = if i < 4 {
					all_available.clone()
				} else if i < 5 {
					// this bitfield signals that only a, c1 and c3 are available.
					let mut bare_bitfield = default_bitfield();
					*bare_bitfield.0.get_mut(0).unwrap() = true;
					*bare_bitfield.0.get_mut(2).unwrap() = true;
					*bare_bitfield.0.get_mut(4).unwrap() = true;

					bare_bitfield
				} else {
					// sign nothing.
					return None;
				};

				Some(
					sign_bitfield(
						&keystore,
						key,
						ValidatorIndex(i as _),
						to_sign,
						&signing_context,
					)
					.into(),
				)
			})
			.collect::<Vec<_>>();

		let old_len = signed_bitfields.len();
		let checked_bitfields = simple_sanitize_bitfields(
			signed_bitfields,
			DisputedBitfield::zeros(expected_bits()),
			expected_bits(),
		);
		assert_eq!(checked_bitfields.len(), old_len, "No bitfields should have been filtered!");

		// only chain A's core and candidate's C1 core are freed.
		let v = process_bitfields(checked_bitfields);
		assert_eq!(
			vec![(CoreIndex(2), candidate_c_1.hash()), (CoreIndex(0), candidate_a.hash())],
			v
		);

		let votes = |bits: &[usize]| {
			let mut votes = default_availability_votes();
			for bit in bits {
				*votes.get_mut(*bit).unwrap() = true;
			}

			votes
		};

		assert!(PendingAvailability::<Test>::get(&chain_a).unwrap().is_empty());
		assert_eq!(
			PendingAvailability::<Test>::get(&chain_b)
				.unwrap()
				.pop_front()
				.unwrap()
				.availability_votes,
			votes(&[0, 1, 2, 3])
		);
		let mut pending_c = PendingAvailability::<Test>::get(&chain_c).unwrap();
		assert_eq!(pending_c.pop_front().unwrap().availability_votes, votes(&[0, 1, 2, 3]));
		assert_eq!(pending_c.pop_front().unwrap().availability_votes, votes(&[0, 1, 2, 3, 4]));
		assert!(pending_c.is_empty());

		// and check that chain heads.
		assert_eq!(paras::Heads::<Test>::get(&chain_a), Some(vec![1, 2, 3, 4].into()));
		assert_ne!(paras::Heads::<Test>::get(&chain_b), Some(vec![5, 6, 7, 8].into()));
		assert_eq!(paras::Heads::<Test>::get(&chain_c), Some(vec![7, 8].into()));

		// Check that rewards are applied.
		{
			let rewards = crate::mock::availability_rewards();

			assert_eq!(rewards.len(), 5);
			assert_eq!(rewards.get(&ValidatorIndex(0)).unwrap(), &2);
			assert_eq!(rewards.get(&ValidatorIndex(1)).unwrap(), &2);
			assert_eq!(rewards.get(&ValidatorIndex(2)).unwrap(), &2);
			assert_eq!(rewards.get(&ValidatorIndex(3)).unwrap(), &2);
			assert_eq!(rewards.get(&ValidatorIndex(4)).unwrap(), &2);
		}

		{
			let rewards = crate::mock::backing_rewards();

			assert_eq!(rewards.len(), 3);
			assert_eq!(rewards.get(&ValidatorIndex(3)).unwrap(), &1);
			assert_eq!(rewards.get(&ValidatorIndex(4)).unwrap(), &1);
			assert_eq!(rewards.get(&ValidatorIndex(1)).unwrap(), &1);
		}

		// Add a new bitfield which will make candidate C2 available also. This will also evict and
		// enact C3.
		let signed_bitfields = vec![sign_bitfield(
			&keystore,
			&validators[5],
			ValidatorIndex(5),
			{
				let mut bare_bitfield = default_bitfield();
				*bare_bitfield.0.get_mut(3).unwrap() = true;
				bare_bitfield
			},
			&signing_context,
		)
		.into()];

		let old_len = signed_bitfields.len();
		let checked_bitfields = simple_sanitize_bitfields(
			signed_bitfields,
			DisputedBitfield::zeros(expected_bits()),
			expected_bits(),
		);
		assert_eq!(checked_bitfields.len(), old_len, "No bitfields should have been filtered!");

		let v = process_bitfields(checked_bitfields);
		assert_eq!(
			vec![(CoreIndex(3), candidate_c_2.hash()), (CoreIndex(4), candidate_c_3.hash())],
			v
		);

		assert!(PendingAvailability::<Test>::get(&chain_a).unwrap().is_empty());
		assert_eq!(
			PendingAvailability::<Test>::get(&chain_b)
				.unwrap()
				.pop_front()
				.unwrap()
				.availability_votes,
			votes(&[0, 1, 2, 3])
		);
		assert!(PendingAvailability::<Test>::get(&chain_c).unwrap().is_empty());

		// and check that chain heads.
		assert_eq!(paras::Heads::<Test>::get(&chain_a), Some(vec![1, 2, 3, 4].into()));
		assert_ne!(paras::Heads::<Test>::get(&chain_b), Some(vec![5, 6, 7, 8].into()));
		assert_eq!(paras::Heads::<Test>::get(&chain_c), Some(vec![11, 12].into()));

		// Check that rewards are applied.
		{
			let rewards = crate::mock::availability_rewards();

			assert_eq!(rewards.len(), 6);
			assert_eq!(rewards.get(&ValidatorIndex(0)).unwrap(), &4);
			assert_eq!(rewards.get(&ValidatorIndex(1)).unwrap(), &4);
			assert_eq!(rewards.get(&ValidatorIndex(2)).unwrap(), &4);
			assert_eq!(rewards.get(&ValidatorIndex(3)).unwrap(), &4);
			assert_eq!(rewards.get(&ValidatorIndex(4)).unwrap(), &3);
			assert_eq!(rewards.get(&ValidatorIndex(5)).unwrap(), &1);
		}

		{
			let rewards = crate::mock::backing_rewards();

			assert_eq!(rewards.len(), 5);
			assert_eq!(rewards.get(&ValidatorIndex(3)).unwrap(), &1);
			assert_eq!(rewards.get(&ValidatorIndex(4)).unwrap(), &1);
			assert_eq!(rewards.get(&ValidatorIndex(1)).unwrap(), &1);
			assert_eq!(rewards.get(&ValidatorIndex(5)).unwrap(), &1);
			assert_eq!(rewards.get(&ValidatorIndex(6)).unwrap(), &1);
		}
	});
}

#[test]
fn candidate_checks() {
	let chain_a = ParaId::from(1_u32);
	let chain_b = ParaId::from(2_u32);
	let thread_a = ParaId::from(3_u32);

	// The block number of the relay-parent for testing.
	const RELAY_PARENT_NUM: BlockNumber = 4;

	let paras = vec![
		(chain_a, ParaKind::Parachain),
		(chain_b, ParaKind::Parachain),
		(thread_a, ParaKind::Parathread),
	];
	let validators = vec![
		Sr25519Keyring::Alice,
		Sr25519Keyring::Bob,
		Sr25519Keyring::Charlie,
		Sr25519Keyring::Dave,
		Sr25519Keyring::Ferdie,
		Sr25519Keyring::One,
	];
	let keystore: KeystorePtr = Arc::new(LocalKeystore::in_memory());
	for validator in validators.iter() {
		Keystore::sr25519_generate_new(
			&*keystore,
			PARACHAIN_KEY_TYPE_ID,
			Some(&validator.to_seed()),
		)
		.unwrap();
	}
	let validator_public = validator_pubkeys(&validators);

	new_test_ext(genesis_config(paras)).execute_with(|| {
		shared::Pallet::<Test>::set_active_validators_ascending(validator_public.clone());
		shared::Pallet::<Test>::set_session_index(5);

		run_to_block(5, |_| None);

		let signing_context =
			SigningContext { parent_hash: System::parent_hash(), session_index: 5 };

		let group_validators = |group_index: GroupIndex| {
			match group_index {
				group_index if group_index == GroupIndex::from(0) => Some(vec![0, 1]),
				group_index if group_index == GroupIndex::from(1) => Some(vec![2, 3]),
				group_index if group_index == GroupIndex::from(2) => Some(vec![4]),
				group_index if group_index == GroupIndex::from(3) => Some(vec![5]),
				_ => panic!("Group index out of bounds"),
			}
			.map(|m| m.into_iter().map(ValidatorIndex).collect::<Vec<_>>())
		};

		// When processing candidates, we compute the group index from scheduler.
		let validator_groups = vec![
			vec![ValidatorIndex(0), ValidatorIndex(1)],
			vec![ValidatorIndex(2), ValidatorIndex(3)],
			vec![ValidatorIndex(4)],
			vec![ValidatorIndex(5)],
		];
		Scheduler::set_validator_groups(validator_groups);

		let thread_collator: CollatorId = Sr25519Keyring::Two.public().into();
		let chain_a_assignment = (chain_a, CoreIndex::from(0));
		let chain_b_assignment = (chain_b, CoreIndex::from(1));

		let thread_a_assignment = (thread_a, CoreIndex::from(2));
		let allowed_scheduling_parents = default_allowed_scheduling_parent_tracker();

		// no candidates.
		assert_eq!(
			ParaInclusion::process_candidates(
				&allowed_scheduling_parents,
				&BTreeMap::new(),
				&group_validators,
			),
			Ok(Default::default())
		);

		// Check candidate ordering
		{
			let candidate_a = TestCandidateBuilder {
				para_id: chain_a,
				relay_parent: System::parent_hash(),
				pov_hash: Hash::repeat_byte(1),
				persisted_validation_data_hash: make_vdata_hash(chain_a).unwrap(),
				hrmp_watermark: RELAY_PARENT_NUM,
				..Default::default()
			}
			.build();
			let candidate_b_1 = TestCandidateBuilder {
				para_id: chain_b,
				relay_parent: System::parent_hash(),
				pov_hash: Hash::repeat_byte(2),
				persisted_validation_data_hash: make_vdata_hash(chain_b).unwrap(),
				hrmp_watermark: RELAY_PARENT_NUM,
				head_data: HeadData(vec![1, 2, 3]),
				..Default::default()
			}
			.build();

			// Make candidate b2 a child of b1.
			let candidate_b_2 = TestCandidateBuilder {
				para_id: chain_b,
				relay_parent: System::parent_hash(),
				pov_hash: Hash::repeat_byte(3),
				persisted_validation_data_hash: make_persisted_validation_data_with_parent::<Test>(
					RELAY_PARENT_NUM,
					Default::default(),
					candidate_b_1.commitments.head_data.clone(),
				)
				.hash(),
				hrmp_watermark: RELAY_PARENT_NUM,
				head_data: HeadData(vec![5, 6, 7]),
				..Default::default()
			}
			.build();

			let backed_a = back_candidate(
				candidate_a,
				&validators,
				group_validators(GroupIndex::from(0)).unwrap().as_ref(),
				&keystore,
				&signing_context,
				BackingKind::Threshold,
				CoreIndex(0),
			);

			let backed_b_1 = back_candidate(
				candidate_b_1.clone(),
				&validators,
				group_validators(GroupIndex::from(2)).unwrap().as_ref(),
				&keystore,
				&signing_context,
				BackingKind::Threshold,
				CoreIndex(2),
			);

			let backed_b_2 = back_candidate(
				candidate_b_2,
				&validators,
				group_validators(GroupIndex::from(1)).unwrap().as_ref(),
				&keystore,
				&signing_context,
				BackingKind::Threshold,
				CoreIndex(1),
			);

			// candidates are required to be sorted in dependency order.
			assert_noop!(
				ParaInclusion::process_candidates(
					&allowed_scheduling_parents,
					&vec![(
						chain_b,
						vec![
							(backed_b_2.clone(), CoreIndex(1)),
							(backed_b_1.clone(), CoreIndex(2))
						]
					),]
					.into_iter()
					.collect(),
					&group_validators,
				),
				Error::<Test>::ValidationDataHashMismatch
			);

			// candidates are no longer required to be sorted by core index.
			ParaInclusion::process_candidates(
				&allowed_scheduling_parents,
				&vec![
					(
						chain_b,
						vec![
							(backed_b_1.clone(), CoreIndex(2)),
							(backed_b_2.clone(), CoreIndex(1)),
						],
					),
					(chain_a_assignment.0, vec![(backed_a.clone(), chain_a_assignment.1)]),
				]
				.into_iter()
				.collect(),
				&group_validators,
			)
			.unwrap();

			// candidate does not build on top of the latest unincluded head

			let candidate_b_3 = TestCandidateBuilder {
				para_id: chain_b,
				relay_parent: System::parent_hash(),
				pov_hash: Hash::repeat_byte(4),
				persisted_validation_data_hash: make_persisted_validation_data_with_parent::<Test>(
					RELAY_PARENT_NUM,
					Default::default(),
					candidate_b_1.commitments.head_data.clone(),
				)
				.hash(),
				hrmp_watermark: RELAY_PARENT_NUM,
				head_data: HeadData(vec![8, 9]),
				..Default::default()
			}
			.build();

			let backed_b_3 = back_candidate(
				candidate_b_3,
				&validators,
				group_validators(GroupIndex::from(3)).unwrap().as_ref(),
				&keystore,
				&signing_context,
				BackingKind::Threshold,
				CoreIndex(3),
			);

			assert_noop!(
				ParaInclusion::process_candidates(
					&allowed_scheduling_parents,
					&vec![(chain_b, vec![(backed_b_3, CoreIndex(3))])].into_iter().collect(),
					&group_validators,
				),
				Error::<Test>::ValidationDataHashMismatch
			);
		}

		// candidate not backed.
		{
			let candidate = TestCandidateBuilder {
				para_id: chain_a,
				relay_parent: System::parent_hash(),
				pov_hash: Hash::repeat_byte(1),
				persisted_validation_data_hash: make_vdata_hash(chain_a).unwrap(),
				hrmp_watermark: RELAY_PARENT_NUM,
				..Default::default()
			}
			.build();

			// Insufficient backing.
			let backed = back_candidate(
				candidate.clone(),
				&validators,
				group_validators(GroupIndex::from(0)).unwrap().as_ref(),
				&keystore,
				&signing_context,
				BackingKind::Lacking,
				CoreIndex(0),
			);

			assert_noop!(
				ParaInclusion::process_candidates(
					&allowed_scheduling_parents,
					&vec![(chain_a_assignment.0, vec![(backed, chain_a_assignment.1)])]
						.into_iter()
						.collect(),
					&group_validators,
				),
				Error::<Test>::InsufficientBacking
			);

			// Wrong backing group.
			let backed = back_candidate(
				candidate,
				&validators,
				group_validators(GroupIndex::from(1)).unwrap().as_ref(),
				&keystore,
				&signing_context,
				BackingKind::Threshold,
				CoreIndex(0),
			);

			assert_noop!(
				ParaInclusion::process_candidates(
					&allowed_scheduling_parents,
					&vec![(chain_a_assignment.0, vec![(backed, chain_a_assignment.1)])]
						.into_iter()
						.collect(),
					&group_validators,
				),
				Error::<Test>::InvalidBacking
			);
		}

		// one of candidates is not based on allowed relay parent.
		{
			let wrong_parent_hash = Hash::repeat_byte(222);
			assert!(System::parent_hash() != wrong_parent_hash);

			let candidate_a = TestCandidateBuilder {
				para_id: chain_a,
				relay_parent: wrong_parent_hash,
				pov_hash: Hash::repeat_byte(1),
				persisted_validation_data_hash: make_vdata_hash(chain_a).unwrap(),
				..Default::default()
			}
			.build();

			let candidate_b = TestCandidateBuilder {
				para_id: chain_b,
				relay_parent: System::parent_hash(),
				pov_hash: Hash::repeat_byte(2),
				persisted_validation_data_hash: make_vdata_hash(chain_b).unwrap(),
				hrmp_watermark: RELAY_PARENT_NUM,
				..Default::default()
			}
			.build();

			let backed_a = back_candidate(
				candidate_a,
				&validators,
				group_validators(GroupIndex::from(0)).unwrap().as_ref(),
				&keystore,
				&signing_context,
				BackingKind::Threshold,
				chain_a_assignment.1,
			);

			let backed_b = back_candidate(
				candidate_b,
				&validators,
				group_validators(GroupIndex::from(1)).unwrap().as_ref(),
				&keystore,
				&signing_context,
				BackingKind::Threshold,
				chain_b_assignment.1,
			);

			assert_noop!(
				ParaInclusion::process_candidates(
					&allowed_scheduling_parents,
					&vec![
						(chain_b_assignment.0, vec![(backed_b, chain_b_assignment.1)]),
						(chain_a_assignment.0, vec![(backed_a, chain_a_assignment.1)])
					]
					.into_iter()
					.collect(),
					&group_validators,
				),
				Error::<Test>::DisallowedRelayParent
			);
		}

		// candidate not well-signed by collator.
		{
			let mut candidate = TestCandidateBuilder {
				para_id: thread_a,
				relay_parent: System::parent_hash(),
				pov_hash: Hash::repeat_byte(1),
				persisted_validation_data_hash: make_vdata_hash(thread_a).unwrap(),
				hrmp_watermark: RELAY_PARENT_NUM,
				..Default::default()
			}
			.build();

			assert_eq!(CollatorId::from(Sr25519Keyring::Two.public()), thread_collator);

			// change the candidate after signing.
			candidate.descriptor.set_pov_hash(Hash::repeat_byte(2));

			let backed = back_candidate(
				candidate,
				&validators,
				group_validators(GroupIndex::from(2)).unwrap().as_ref(),
				&keystore,
				&signing_context,
				BackingKind::Threshold,
				thread_a_assignment.1,
			);

			let candidate_receipt_with_backing_validator_indices =
				ParaInclusion::process_candidates(
					&allowed_scheduling_parents,
					&vec![(thread_a_assignment.0, vec![(backed.clone(), thread_a_assignment.1)])]
						.into_iter()
						.collect(),
					&group_validators,
				)
				.expect("candidate is accepted with bad collator signature");

			let mut expected = std::collections::HashMap::<
				CandidateHash,
				(CandidateReceipt, Vec<(ValidatorIndex, ValidityAttestation)>),
			>::new();
			let backed_candidate = backed;
			let candidate_receipt_with_backers = expected
				.entry(backed_candidate.hash())
				.or_insert_with(|| (backed_candidate.receipt(), Vec::new()));
			let (validator_indices, Some(_)) = backed_candidate.validator_indices_and_core_index()
			else {
				panic!("Expected core index")
			};
			assert_eq!(backed_candidate.validity_votes().len(), validator_indices.count_ones());
			candidate_receipt_with_backers.1.extend(
				validator_indices
					.iter()
					.enumerate()
					.filter(|(_, signed)| **signed)
					.zip(backed_candidate.validity_votes().iter().cloned())
					.filter_map(|((validator_index_within_group, _), attestation)| {
						let grp_idx = GroupIndex(2);
						group_validators(grp_idx).map(|validator_indices| {
							(validator_indices[validator_index_within_group], attestation)
						})
					}),
			);

			assert_eq!(
				expected,
				candidate_receipt_with_backing_validator_indices
					.into_iter()
					.map(|c| (c.0.hash(), c))
					.collect()
			);
		}

		// interfering code upgrade - reject
		{
			let candidate = TestCandidateBuilder {
				para_id: chain_a,
				relay_parent: System::parent_hash(),
				pov_hash: Hash::repeat_byte(1),
				new_validation_code: Some(dummy_validation_code()),
				persisted_validation_data_hash: make_vdata_hash(chain_a).unwrap(),
				hrmp_watermark: RELAY_PARENT_NUM,
				..Default::default()
			}
			.build();

			let backed = back_candidate(
				candidate,
				&validators,
				group_validators(GroupIndex::from(0)).unwrap().as_ref(),
				&keystore,
				&signing_context,
				BackingKind::Threshold,
				chain_a_assignment.1,
			);

			{
				let cfg = configuration::ActiveConfig::<Test>::get();
				let expected_at = 10 + cfg.validation_upgrade_delay;
				assert_eq!(expected_at, 12);
				Paras::schedule_code_upgrade(
					chain_a,
					vec![9, 8, 7, 6, 5, 4, 3, 2, 1].into(),
					expected_at,
					&cfg,
					UpgradeStrategy::SetGoAheadSignal,
				);
			}

			assert_noop!(
				ParaInclusion::process_candidates(
					&allowed_scheduling_parents,
					&vec![(chain_a_assignment.0, vec![(backed, chain_a_assignment.1)])]
						.into_iter()
						.collect(),
					&group_validators,
				),
				Error::<Test>::PrematureCodeUpgrade
			);
		}

		// Bad validation data hash - reject
		{
			let candidate = TestCandidateBuilder {
				para_id: chain_a,
				relay_parent: System::parent_hash(),
				pov_hash: Hash::repeat_byte(1),
				persisted_validation_data_hash: [42u8; 32].into(),
				hrmp_watermark: RELAY_PARENT_NUM,
				..Default::default()
			}
			.build();

			let backed = back_candidate(
				candidate,
				&validators,
				group_validators(GroupIndex::from(0)).unwrap().as_ref(),
				&keystore,
				&signing_context,
				BackingKind::Threshold,
				chain_a_assignment.1,
			);

			assert_noop!(
				ParaInclusion::process_candidates(
					&allowed_scheduling_parents,
					&vec![(chain_a_assignment.0, vec![(backed, chain_a_assignment.1)])]
						.into_iter()
						.collect(),
					&group_validators,
				),
				Error::<Test>::ValidationDataHashMismatch
			);
		}

		// bad validation code hash
		{
			let candidate = TestCandidateBuilder {
				para_id: chain_a,
				relay_parent: System::parent_hash(),
				pov_hash: Hash::repeat_byte(1),
				persisted_validation_data_hash: make_vdata_hash(chain_a).unwrap(),
				hrmp_watermark: RELAY_PARENT_NUM,
				validation_code: ValidationCode(vec![9, 8, 7, 6, 5, 4, 3, 2, 1]),
				..Default::default()
			}
			.build();

			let backed = back_candidate(
				candidate,
				&validators,
				group_validators(GroupIndex::from(0)).unwrap().as_ref(),
				&keystore,
				&signing_context,
				BackingKind::Threshold,
				chain_a_assignment.1,
			);

			assert_noop!(
				ParaInclusion::process_candidates(
					&allowed_scheduling_parents,
					&vec![(chain_a_assignment.0, vec![(backed, chain_a_assignment.1)])]
						.into_iter()
						.collect(),
					&group_validators,
				),
				Error::<Test>::InvalidValidationCodeHash
			);
		}

		// Para head hash in descriptor doesn't match head data
		{
			let candidate = TestCandidateBuilder {
				para_id: chain_a,
				relay_parent: System::parent_hash(),
				pov_hash: Hash::repeat_byte(1),
				persisted_validation_data_hash: make_vdata_hash(chain_a).unwrap(),
				hrmp_watermark: RELAY_PARENT_NUM,
				para_head_hash: Some(Hash::random()),
				..Default::default()
			}
			.build();

			let backed = back_candidate(
				candidate,
				&validators,
				group_validators(GroupIndex::from(0)).unwrap().as_ref(),
				&keystore,
				&signing_context,
				BackingKind::Threshold,
				chain_a_assignment.1,
			);

			assert_noop!(
				ParaInclusion::process_candidates(
					&allowed_scheduling_parents,
					&vec![(chain_a_assignment.0, vec![(backed, chain_a_assignment.1)])]
						.into_iter()
						.collect(),
					&group_validators,
				),
				Error::<Test>::ParaHeadMismatch
			);
		}
	});
}

#[test]
fn backing_works() {
	let chain_a = ParaId::from(1_u32);
	let chain_b = ParaId::from(2_u32);
	let thread_a = ParaId::from(3_u32);

	// The block number of the relay-parent for testing.
	const RELAY_PARENT_NUM: BlockNumber = 4;

	let paras = vec![
		(chain_a, ParaKind::Parachain),
		(chain_b, ParaKind::Parachain),
		(thread_a, ParaKind::Parathread),
	];
	let validators = vec![
		Sr25519Keyring::Alice,
		Sr25519Keyring::Bob,
		Sr25519Keyring::Charlie,
		Sr25519Keyring::Dave,
		Sr25519Keyring::Ferdie,
	];
	let keystore: KeystorePtr = Arc::new(LocalKeystore::in_memory());
	for validator in validators.iter() {
		Keystore::sr25519_generate_new(
			&*keystore,
			PARACHAIN_KEY_TYPE_ID,
			Some(&validator.to_seed()),
		)
		.unwrap();
	}
	let validator_public = validator_pubkeys(&validators);

	new_test_ext(genesis_config(paras)).execute_with(|| {
		shared::Pallet::<Test>::set_active_validators_ascending(validator_public.clone());
		shared::Pallet::<Test>::set_session_index(5);

		run_to_block(5, |_| None);

		let signing_context =
			SigningContext { parent_hash: System::parent_hash(), session_index: 5 };

		let group_validators = |group_index: GroupIndex| {
			match group_index {
				group_index if group_index == GroupIndex::from(0) => Some(vec![0, 1]),
				group_index if group_index == GroupIndex::from(1) => Some(vec![2, 3]),
				group_index if group_index == GroupIndex::from(2) => Some(vec![4]),
				_ => panic!("Group index out of bounds for 2 parachains and 1 parathread core"),
			}
			.map(|vs| vs.into_iter().map(ValidatorIndex).collect::<Vec<_>>())
		};

		// When processing candidates, we compute the group index from scheduler.
		let validator_groups = vec![
			vec![ValidatorIndex(0), ValidatorIndex(1)],
			vec![ValidatorIndex(2), ValidatorIndex(3)],
			vec![ValidatorIndex(4)],
		];
		Scheduler::set_validator_groups(validator_groups);

		let allowed_scheduling_parents = default_allowed_scheduling_parent_tracker();

		let chain_a_assignment = (chain_a, CoreIndex::from(0));
		let chain_b_assignment = (chain_b, CoreIndex::from(1));
		let thread_a_assignment = (thread_a, CoreIndex::from(2));

		let candidate_a = TestCandidateBuilder {
			para_id: chain_a,
			relay_parent: System::parent_hash(),
			pov_hash: Hash::repeat_byte(1),
			persisted_validation_data_hash: make_vdata_hash(chain_a).unwrap(),
			hrmp_watermark: RELAY_PARENT_NUM,
			..Default::default()
		}
		.build();

		let candidate_b = TestCandidateBuilder {
			para_id: chain_b,
			relay_parent: System::parent_hash(),
			pov_hash: Hash::repeat_byte(2),
			persisted_validation_data_hash: make_vdata_hash(chain_b).unwrap(),
			hrmp_watermark: RELAY_PARENT_NUM,
			..Default::default()
		}
		.build();

		let candidate_c = TestCandidateBuilder {
			para_id: thread_a,
			relay_parent: System::parent_hash(),
			pov_hash: Hash::repeat_byte(3),
			persisted_validation_data_hash: make_vdata_hash(thread_a).unwrap(),
			hrmp_watermark: RELAY_PARENT_NUM,
			..Default::default()
		}
		.build();

		let backed_a = back_candidate(
			candidate_a.clone(),
			&validators,
			group_validators(GroupIndex::from(0)).unwrap().as_ref(),
			&keystore,
			&signing_context,
			BackingKind::Threshold,
			chain_a_assignment.1,
		);

		let backed_b = back_candidate(
			candidate_b.clone(),
			&validators,
			group_validators(GroupIndex::from(1)).unwrap().as_ref(),
			&keystore,
			&signing_context,
			BackingKind::Threshold,
			chain_b_assignment.1,
		);

		let backed_c = back_candidate(
			candidate_c.clone(),
			&validators,
			group_validators(GroupIndex::from(2)).unwrap().as_ref(),
			&keystore,
			&signing_context,
			BackingKind::Threshold,
			thread_a_assignment.1,
		);

		let backed_candidates = vec![
			(chain_a_assignment.0, vec![(backed_a, chain_a_assignment.1)]),
			(chain_b_assignment.0, vec![(backed_b, chain_b_assignment.1)]),
			(thread_a_assignment.0, vec![(backed_c, thread_a_assignment.1)]),
		]
		.into_iter()
		.collect::<BTreeMap<_, _>>();

		let get_backing_group_idx = {
			// the order defines the group implicitly for this test case
			let backed_candidates_with_groups = backed_candidates
				.values()
				.enumerate()
				.map(|(idx, backed_candidates)| {
					(backed_candidates.iter().next().unwrap().0.hash(), GroupIndex(idx as _))
				})
				.collect::<Vec<_>>();

			move |candidate_hash_x: CandidateHash| -> Option<GroupIndex> {
				backed_candidates_with_groups.iter().find_map(|(candidate_hash, grp)| {
					if *candidate_hash == candidate_hash_x {
						Some(*grp)
					} else {
						None
					}
				})
			}
		};

		let candidate_receipt_with_backing_validator_indices = ParaInclusion::process_candidates(
			&allowed_scheduling_parents,
			&backed_candidates,
			&group_validators,
		)
		.expect("candidates scheduled, in order, and backed");

		// Transform the votes into the setup we expect
		let expected = {
			let mut intermediate = std::collections::HashMap::<
				CandidateHash,
				(CandidateReceipt, Vec<(ValidatorIndex, ValidityAttestation)>),
			>::new();
			backed_candidates.values().for_each(|backed_candidates| {
				let backed_candidate = backed_candidates.iter().next().unwrap().0.clone();
				let candidate_receipt_with_backers = intermediate
					.entry(backed_candidate.hash())
					.or_insert_with(|| (backed_candidate.receipt(), Vec::new()));
				let (validator_indices, Some(_)) =
					backed_candidate.validator_indices_and_core_index()
				else {
					panic!("Expected injected core index")
				};
				assert_eq!(backed_candidate.validity_votes().len(), validator_indices.count_ones());
				candidate_receipt_with_backers.1.extend(
					validator_indices
						.iter()
						.enumerate()
						.filter(|(_, signed)| **signed)
						.zip(backed_candidate.validity_votes().iter().cloned())
						.filter_map(|((validator_index_within_group, _), attestation)| {
							let grp_idx = get_backing_group_idx(backed_candidate.hash()).unwrap();
							group_validators(grp_idx).map(|validator_indices| {
								(validator_indices[validator_index_within_group], attestation)
							})
						}),
				);
			});
			intermediate.into_values().collect::<Vec<_>>()
		};

		// sort, since we use a hashmap above
		let assure_candidate_sorting = |mut candidate_receipts_with_backers: Vec<(
			CandidateReceipt,
			Vec<(ValidatorIndex, ValidityAttestation)>,
		)>| {
			candidate_receipts_with_backers.sort_by(|(cr1, _), (cr2, _)| {
				cr1.descriptor().para_id().cmp(&cr2.descriptor().para_id())
			});
			candidate_receipts_with_backers
		};
		assert_eq!(
			assure_candidate_sorting(expected),
			assure_candidate_sorting(candidate_receipt_with_backing_validator_indices)
		);

		let backers = {
			let num_backers = effective_minimum_backing_votes(
				group_validators(GroupIndex(0)).unwrap().len(),
				configuration::ActiveConfig::<Test>::get().minimum_backing_votes,
			);
			backing_bitfield(&(0..num_backers).collect::<Vec<_>>())
		};
		assert_eq!(
			PendingAvailability::<Test>::get(&chain_a),
			Some(
				[CandidatePendingAvailability {
					core: CoreIndex::from(0),
					hash: candidate_a.hash(),
					descriptor: candidate_a.descriptor,
					availability_votes: default_availability_votes(),
					relay_parent_number: System::block_number() - 1,
					backed_in_number: System::block_number(),
					backers,
					backing_group: GroupIndex::from(0),
					commitments: candidate_a.commitments,
				}]
				.into_iter()
				.collect::<VecDeque<_>>()
			)
		);

		let backers = {
			let num_backers = effective_minimum_backing_votes(
				group_validators(GroupIndex(0)).unwrap().len(),
				configuration::ActiveConfig::<Test>::get().minimum_backing_votes,
			);
			backing_bitfield(&(0..num_backers).map(|v| v + 2).collect::<Vec<_>>())
		};
		assert_eq!(
			PendingAvailability::<Test>::get(&chain_b),
			Some(
				[CandidatePendingAvailability {
					core: CoreIndex::from(1),
					hash: candidate_b.hash(),
					descriptor: candidate_b.descriptor,
					availability_votes: default_availability_votes(),
					relay_parent_number: System::block_number() - 1,
					backed_in_number: System::block_number(),
					backers,
					backing_group: GroupIndex::from(1),
					commitments: candidate_b.commitments,
				}]
				.into_iter()
				.collect::<VecDeque<_>>()
			)
		);

		assert_eq!(
			PendingAvailability::<Test>::get(&thread_a),
			Some(
				[CandidatePendingAvailability {
					core: CoreIndex::from(2),
					hash: candidate_c.hash(),
					descriptor: candidate_c.descriptor,
					availability_votes: default_availability_votes(),
					relay_parent_number: System::block_number() - 1,
					backed_in_number: System::block_number(),
					backers: backing_bitfield(&[4]),
					backing_group: GroupIndex::from(2),
					commitments: candidate_c.commitments
				}]
				.into_iter()
				.collect::<VecDeque<_>>()
			)
		);
	});
}

#[test]
fn backing_works_with_elastic_scaling() {
	let chain_a = ParaId::from(1_u32);
	let chain_b = ParaId::from(2_u32);
	let thread_a = ParaId::from(3_u32);

	// The block number of the relay-parent for testing.
	const RELAY_PARENT_NUM: BlockNumber = 4;

	let paras = vec![
		(chain_a, ParaKind::Parachain),
		(chain_b, ParaKind::Parachain),
		(thread_a, ParaKind::Parathread),
	];
	let validators = vec![
		Sr25519Keyring::Alice,
		Sr25519Keyring::Bob,
		Sr25519Keyring::Charlie,
		Sr25519Keyring::Dave,
		Sr25519Keyring::Ferdie,
	];
	let keystore: KeystorePtr = Arc::new(LocalKeystore::in_memory());
	for validator in validators.iter() {
		Keystore::sr25519_generate_new(
			&*keystore,
			PARACHAIN_KEY_TYPE_ID,
			Some(&validator.to_seed()),
		)
		.unwrap();
	}
	let validator_public = validator_pubkeys(&validators);

	new_test_ext(genesis_config(paras)).execute_with(|| {
		shared::Pallet::<Test>::set_active_validators_ascending(validator_public.clone());
		shared::Pallet::<Test>::set_session_index(5);

		run_to_block(5, |_| None);

		let signing_context =
			SigningContext { parent_hash: System::parent_hash(), session_index: 5 };

		let group_validators = |group_index: GroupIndex| {
			match group_index {
				group_index if group_index == GroupIndex::from(0) => Some(vec![0, 1]),
				group_index if group_index == GroupIndex::from(1) => Some(vec![2, 3]),
				group_index if group_index == GroupIndex::from(2) => Some(vec![4]),
				_ => panic!("Group index out of bounds for 2 parachains and 1 parathread core"),
			}
			.map(|vs| vs.into_iter().map(ValidatorIndex).collect::<Vec<_>>())
		};

		// When processing candidates, we compute the group index from scheduler.
		let validator_groups = vec![
			vec![ValidatorIndex(0), ValidatorIndex(1)],
			vec![ValidatorIndex(2), ValidatorIndex(3)],
			vec![ValidatorIndex(4)],
		];
		Scheduler::set_validator_groups(validator_groups);

		let allowed_scheduling_parents = default_allowed_scheduling_parent_tracker();

		let candidate_a = TestCandidateBuilder {
			para_id: chain_a,
			relay_parent: System::parent_hash(),
			pov_hash: Hash::repeat_byte(1),
			persisted_validation_data_hash: make_vdata_hash(chain_a).unwrap(),
			hrmp_watermark: RELAY_PARENT_NUM,
			..Default::default()
		}
		.build();

		let candidate_b_1 = TestCandidateBuilder {
			para_id: chain_b,
			relay_parent: System::parent_hash(),
			pov_hash: Hash::repeat_byte(2),
			persisted_validation_data_hash: make_vdata_hash(chain_b).unwrap(),
			hrmp_watermark: RELAY_PARENT_NUM,
			..Default::default()
		}
		.build();

		// Make candidate b2 a child of b1.
		let candidate_b_2 = TestCandidateBuilder {
			para_id: chain_b,
			relay_parent: System::parent_hash(),
			pov_hash: Hash::repeat_byte(3),
			persisted_validation_data_hash: make_persisted_validation_data_with_parent::<Test>(
				RELAY_PARENT_NUM,
				Default::default(),
				candidate_b_1.commitments.head_data.clone(),
			)
			.hash(),
			hrmp_watermark: RELAY_PARENT_NUM,
			..Default::default()
		}
		.build();

		let backed_a = back_candidate(
			candidate_a.clone(),
			&validators,
			group_validators(GroupIndex::from(0)).unwrap().as_ref(),
			&keystore,
			&signing_context,
			BackingKind::Threshold,
			CoreIndex(0),
		);

		let backed_b_1 = back_candidate(
			candidate_b_1.clone(),
			&validators,
			group_validators(GroupIndex::from(1)).unwrap().as_ref(),
			&keystore,
			&signing_context,
			BackingKind::Threshold,
			CoreIndex(1),
		);

		let backed_b_2 = back_candidate(
			candidate_b_2.clone(),
			&validators,
			group_validators(GroupIndex::from(2)).unwrap().as_ref(),
			&keystore,
			&signing_context,
			BackingKind::Threshold,
			CoreIndex(2),
		);

		let mut backed_candidates = BTreeMap::new();
		backed_candidates.insert(chain_a, vec![(backed_a, CoreIndex(0))]);
		backed_candidates
			.insert(chain_b, vec![(backed_b_1, CoreIndex(1)), (backed_b_2, CoreIndex(2))]);

		let get_backing_group_idx = {
			// the order defines the group implicitly for this test case
			let backed_candidates_with_groups = backed_candidates
				.values()
				.enumerate()
				.map(|(idx, backed_candidates)| {
					backed_candidates
						.iter()
						.enumerate()
						.map(|(i, c)| (c.0.hash(), GroupIndex((idx + i) as _)))
						.collect()
				})
				.collect::<Vec<Vec<_>>>()
				.concat();

			move |candidate_hash_x: CandidateHash| -> Option<GroupIndex> {
				backed_candidates_with_groups.iter().find_map(|(candidate_hash, grp)| {
					if *candidate_hash == candidate_hash_x {
						Some(*grp)
					} else {
						None
					}
				})
			}
		};

		let candidate_receipt_with_backing_validator_indices = ParaInclusion::process_candidates(
			&allowed_scheduling_parents,
			&backed_candidates,
			&group_validators,
		)
		.expect("candidates scheduled, in order, and backed");

		// Transform the votes into the setup we expect
		let mut expected = std::collections::HashMap::<
			CandidateHash,
			(CandidateReceipt, Vec<(ValidatorIndex, ValidityAttestation)>),
		>::new();
		backed_candidates.values().for_each(|backed_candidates| {
			for backed_candidate in backed_candidates {
				let backed_candidate = backed_candidate.0.clone();
				let candidate_receipt_with_backers = expected
					.entry(backed_candidate.hash())
					.or_insert_with(|| (backed_candidate.receipt(), Vec::new()));
				let (validator_indices, Some(_)) =
					backed_candidate.validator_indices_and_core_index()
				else {
					panic!("Expected injected core index")
				};
				assert_eq!(backed_candidate.validity_votes().len(), validator_indices.count_ones());
				candidate_receipt_with_backers.1.extend(
					validator_indices
						.iter()
						.enumerate()
						.filter(|(_, signed)| **signed)
						.zip(backed_candidate.validity_votes().iter().cloned())
						.filter_map(|((validator_index_within_group, _), attestation)| {
							let grp_idx = get_backing_group_idx(backed_candidate.hash()).unwrap();
							group_validators(grp_idx).map(|validator_indices| {
								(validator_indices[validator_index_within_group], attestation)
							})
						}),
				);
			}
		});

		assert_eq!(
			expected,
			candidate_receipt_with_backing_validator_indices
				.into_iter()
				.map(|c| (c.0.hash(), c))
				.collect()
		);

		let backers = {
			let num_backers = effective_minimum_backing_votes(
				group_validators(GroupIndex(0)).unwrap().len(),
				configuration::ActiveConfig::<Test>::get().minimum_backing_votes,
			);
			backing_bitfield(&(0..num_backers).collect::<Vec<_>>())
		};
		assert_eq!(
			PendingAvailability::<Test>::get(&chain_a),
			Some(
				[CandidatePendingAvailability {
					core: CoreIndex::from(0),
					hash: candidate_a.hash(),
					descriptor: candidate_a.descriptor,
					availability_votes: default_availability_votes(),
					relay_parent_number: System::block_number() - 1,
					backed_in_number: System::block_number(),
					backers,
					backing_group: GroupIndex::from(0),
					commitments: candidate_a.commitments
				}]
				.into_iter()
				.collect::<VecDeque<_>>()
			)
		);

		// Both candidates of b will be recorded on chain.
		assert_eq!(
			PendingAvailability::<Test>::get(&chain_b),
			Some(
				[
					CandidatePendingAvailability {
						core: CoreIndex::from(1),
						hash: candidate_b_1.hash(),
						descriptor: candidate_b_1.descriptor,
						availability_votes: default_availability_votes(),
						relay_parent_number: System::block_number() - 1,
						backed_in_number: System::block_number(),
						backers: backing_bitfield(&[2, 3]),
						backing_group: GroupIndex::from(1),
						commitments: candidate_b_1.commitments
					},
					CandidatePendingAvailability {
						core: CoreIndex::from(2),
						hash: candidate_b_2.hash(),
						descriptor: candidate_b_2.descriptor,
						availability_votes: default_availability_votes(),
						relay_parent_number: System::block_number() - 1,
						backed_in_number: System::block_number(),
						backers: backing_bitfield(&[4]),
						backing_group: GroupIndex::from(2),
						commitments: candidate_b_2.commitments
					}
				]
				.into_iter()
				.collect::<VecDeque<_>>()
			)
		);
	});
}

#[test]
fn can_include_candidate_with_ok_code_upgrade() {
	let chain_a = ParaId::from(1_u32);

	// The block number of the relay-parent for testing.
	const RELAY_PARENT_NUM: BlockNumber = 4;

	let paras = vec![(chain_a, ParaKind::Parachain)];
	let validators = vec![
		Sr25519Keyring::Alice,
		Sr25519Keyring::Bob,
		Sr25519Keyring::Charlie,
		Sr25519Keyring::Dave,
		Sr25519Keyring::Ferdie,
	];
	let keystore: KeystorePtr = Arc::new(LocalKeystore::in_memory());
	for validator in validators.iter() {
		Keystore::sr25519_generate_new(
			&*keystore,
			PARACHAIN_KEY_TYPE_ID,
			Some(&validator.to_seed()),
		)
		.unwrap();
	}
	let validator_public = validator_pubkeys(&validators);

	new_test_ext(genesis_config(paras)).execute_with(|| {
		shared::Pallet::<Test>::set_active_validators_ascending(validator_public.clone());
		shared::Pallet::<Test>::set_session_index(5);

		run_to_block(5, |_| None);

		let signing_context =
			SigningContext { parent_hash: System::parent_hash(), session_index: 5 };

		let group_validators = |group_index: GroupIndex| {
			match group_index {
				group_index if group_index == GroupIndex::from(0) => Some(vec![0, 1, 2, 3, 4]),
				_ => panic!("Group index out of bounds for 1 parachain"),
			}
			.map(|vs| vs.into_iter().map(ValidatorIndex).collect::<Vec<_>>())
		};

		// When processing candidates, we compute the group index from scheduler.
		let validator_groups = vec![vec![
			ValidatorIndex(0),
			ValidatorIndex(1),
			ValidatorIndex(2),
			ValidatorIndex(3),
			ValidatorIndex(4),
		]];
		Scheduler::set_validator_groups(validator_groups);

		let allowed_scheduling_parents = default_allowed_scheduling_parent_tracker();
		let chain_a_assignment = (chain_a, CoreIndex::from(0));
		let candidate_a = TestCandidateBuilder {
			para_id: chain_a,
			relay_parent: System::parent_hash(),
			pov_hash: Hash::repeat_byte(1),
			persisted_validation_data_hash: make_vdata_hash(chain_a).unwrap(),
			new_validation_code: Some(vec![9, 8, 7, 6, 5, 4, 3, 2, 1].into()),
			hrmp_watermark: RELAY_PARENT_NUM,
			..Default::default()
		}
		.build();

		let backed_a = back_candidate(
			candidate_a.clone(),
			&validators,
			group_validators(GroupIndex::from(0)).unwrap().as_ref(),
			&keystore,
			&signing_context,
			BackingKind::Threshold,
			chain_a_assignment.1,
		);

		let _ = ParaInclusion::process_candidates(
			&allowed_scheduling_parents,
			&vec![(chain_a_assignment.0, vec![(backed_a, chain_a_assignment.1)])]
				.into_iter()
				.collect::<BTreeMap<_, _>>(),
			group_validators,
		)
		.expect("candidates scheduled, in order, and backed");

		let backers = {
			let num_backers = effective_minimum_backing_votes(
				group_validators(GroupIndex(0)).unwrap().len(),
				configuration::ActiveConfig::<Test>::get().minimum_backing_votes,
			);
			backing_bitfield(&(0..num_backers).collect::<Vec<_>>())
		};
		assert_eq!(
			PendingAvailability::<Test>::get(&chain_a),
			Some(
				[CandidatePendingAvailability {
					core: CoreIndex::from(0),
					hash: candidate_a.hash(),
					descriptor: candidate_a.descriptor,
					availability_votes: default_availability_votes(),
					relay_parent_number: System::block_number() - 1,
					backed_in_number: System::block_number(),
					backers,
					backing_group: GroupIndex::from(0),
					commitments: candidate_a.commitments
				}]
				.into_iter()
				.collect::<VecDeque<_>>()
			)
		);
	});
}

#[test]
fn check_allowed_scheduling_parents() {
	let chain_a = ParaId::from(1);
	let chain_b = ParaId::from(2);
	let thread_a = ParaId::from(3);

	let paras = vec![
		(chain_a, ParaKind::Parachain),
		(chain_b, ParaKind::Parachain),
		(thread_a, ParaKind::Parathread),
	];
	let validators = vec![
		Sr25519Keyring::Alice,
		Sr25519Keyring::Bob,
		Sr25519Keyring::Charlie,
		Sr25519Keyring::Dave,
		Sr25519Keyring::Ferdie,
	];
	let keystore: KeystorePtr = Arc::new(LocalKeystore::in_memory());
	for validator in validators.iter() {
		Keystore::sr25519_generate_new(
			&*keystore,
			PARACHAIN_KEY_TYPE_ID,
			Some(&validator.to_seed()),
		)
		.unwrap();
	}
	let validator_public = validator_pubkeys(&validators);
	let mut config = genesis_config(paras);
	config.configuration.config.scheduler_params.group_rotation_frequency = 1;

	new_test_ext(config).execute_with(|| {
		shared::Pallet::<Test>::set_active_validators_ascending(validator_public.clone());
		shared::Pallet::<Test>::set_session_index(5);

		run_to_block(5, |_| None);

		let group_validators = |group_index: GroupIndex| {
			match group_index {
				group_index if group_index == GroupIndex::from(0) => Some(vec![0, 1]),
				group_index if group_index == GroupIndex::from(1) => Some(vec![2, 3]),
				group_index if group_index == GroupIndex::from(2) => Some(vec![4]),
				_ => panic!("Group index out of bounds for 2 parachains and 1 parathread core"),
			}
			.map(|vs| vs.into_iter().map(ValidatorIndex).collect::<Vec<_>>())
		};

		// When processing candidates, we compute the group index from scheduler.
		let validator_groups = vec![
			vec![ValidatorIndex(0), ValidatorIndex(1)],
			vec![ValidatorIndex(2), ValidatorIndex(3)],
			vec![ValidatorIndex(4)],
		];
		Scheduler::set_validator_groups(validator_groups);

		// Base each candidate on one of allowed relay parents.
		//
		// Note that the group rotation frequency is set to 1 above,
		// which means groups shift at each relay parent.
		//
		// For example, candidate `a` is based on block 1,
		// thus it will be included in block 2, its group index is
		// core = 0 shifted 2 times: one for group rotation and one for
		// fetching the group assigned to the next block.
		//
		// Candidates `b` and `c` are constructed accordingly.

		let relay_parent_a = (1, Hash::repeat_byte(0x1));
		let relay_parent_b = (2, Hash::repeat_byte(0x2));
		let relay_parent_c = (3, Hash::repeat_byte(0x3));

		let mut allowed_scheduling_parents = AllowedSchedulingParentsTracker::default();
		let max_ancestry_len = 3;
		allowed_scheduling_parents.update(
			relay_parent_a.1,
			Default::default(),
			relay_parent_a.0,
			max_ancestry_len,
		);
		allowed_scheduling_parents.update(
			relay_parent_b.1,
			Default::default(),
			relay_parent_b.0,
			max_ancestry_len,
		);
		allowed_scheduling_parents.update(
			relay_parent_c.1,
			Default::default(),
			relay_parent_c.0,
			max_ancestry_len,
		);

		// Also populate the AllowedRelayParents DoubleMap for relay parent lookups.
		for (number, hash) in [relay_parent_a, relay_parent_b, relay_parent_c] {
			shared::AllowedRelayParents::<Test>::insert(
				5,
				hash,
				RelayParentInfo { number, state_root: Default::default() },
			);
		}

		let chain_a_assignment = (chain_a, CoreIndex::from(0));

		let chain_b_assignment = (chain_b, CoreIndex::from(1));
		let thread_a_assignment = (thread_a, CoreIndex::from(2));

		let candidate_a = TestCandidateBuilder {
			para_id: chain_a,
			relay_parent: relay_parent_a.1,
			pov_hash: Hash::repeat_byte(1),
			persisted_validation_data_hash: make_vdata_hash_with_block_number(
				chain_a,
				relay_parent_a.0,
			)
			.unwrap(),
			hrmp_watermark: relay_parent_a.0,
			..Default::default()
		}
		.build();
		let signing_context_a = SigningContext { parent_hash: relay_parent_a.1, session_index: 5 };

		let candidate_b = TestCandidateBuilder {
			para_id: chain_b,
			relay_parent: relay_parent_b.1,
			pov_hash: Hash::repeat_byte(2),
			persisted_validation_data_hash: make_vdata_hash_with_block_number(
				chain_b,
				relay_parent_b.0,
			)
			.unwrap(),
			hrmp_watermark: relay_parent_b.0,
			..Default::default()
		}
		.build();
		let signing_context_b = SigningContext { parent_hash: relay_parent_b.1, session_index: 5 };

		let candidate_c = TestCandidateBuilder {
			para_id: thread_a,
			relay_parent: relay_parent_c.1,
			pov_hash: Hash::repeat_byte(3),
			persisted_validation_data_hash: make_vdata_hash_with_block_number(
				thread_a,
				relay_parent_c.0,
			)
			.unwrap(),
			hrmp_watermark: relay_parent_c.0,
			..Default::default()
		}
		.build();
		let signing_context_c = SigningContext { parent_hash: relay_parent_c.1, session_index: 5 };

		let backed_a = back_candidate(
			candidate_a.clone(),
			&validators,
			group_validators(GroupIndex::from(2)).unwrap().as_ref(),
			&keystore,
			&signing_context_a,
			BackingKind::Threshold,
			chain_a_assignment.1,
		);

		let backed_b = back_candidate(
			candidate_b.clone(),
			&validators,
			group_validators(GroupIndex::from(1)).unwrap().as_ref(),
			&keystore,
			&signing_context_b,
			BackingKind::Threshold,
			chain_b_assignment.1,
		);

		let backed_c = back_candidate(
			candidate_c.clone(),
			&validators,
			group_validators(GroupIndex::from(0)).unwrap().as_ref(),
			&keystore,
			&signing_context_c,
			BackingKind::Threshold,
			thread_a_assignment.1,
		);

		let backed_candidates = vec![
			(chain_a_assignment.0, vec![(backed_a, chain_a_assignment.1)]),
			(chain_b_assignment.0, vec![(backed_b, chain_b_assignment.1)]),
			(thread_a_assignment.0, vec![(backed_c, thread_a_assignment.1)]),
		]
		.into_iter()
		.collect::<BTreeMap<_, _>>();

		ParaInclusion::process_candidates(
			&allowed_scheduling_parents,
			&backed_candidates,
			&group_validators,
		)
		.expect("candidates scheduled, in order, and backed");
	});
}

#[test]
fn session_change_wipes() {
	let chain_a = ParaId::from(1_u32);
	let chain_b = ParaId::from(2_u32);
	let thread_a = ParaId::from(3_u32);

	let paras = vec![
		(chain_a, ParaKind::Parachain),
		(chain_b, ParaKind::Parachain),
		(thread_a, ParaKind::Parathread),
	];
	let validators = vec![
		Sr25519Keyring::Alice,
		Sr25519Keyring::Bob,
		Sr25519Keyring::Charlie,
		Sr25519Keyring::Dave,
		Sr25519Keyring::Ferdie,
	];
	let keystore: KeystorePtr = Arc::new(LocalKeystore::in_memory());
	for validator in validators.iter() {
		Keystore::sr25519_generate_new(
			&*keystore,
			PARACHAIN_KEY_TYPE_ID,
			Some(&validator.to_seed()),
		)
		.unwrap();
	}
	let validator_public = validator_pubkeys(&validators);

	new_test_ext(genesis_config(paras)).execute_with(|| {
		shared::Pallet::<Test>::set_active_validators_ascending(validator_public.clone());
		shared::Pallet::<Test>::set_session_index(5);

		let validators_new =
			vec![Sr25519Keyring::Alice, Sr25519Keyring::Bob, Sr25519Keyring::Charlie];

		let validator_public_new = validator_pubkeys(&validators_new);

		run_to_block(10, |_| None);

		let candidate = TestCandidateBuilder::default().build();
		PendingAvailability::<Test>::insert(
			&chain_a,
			[CandidatePendingAvailability {
				core: CoreIndex::from(0),
				hash: candidate.hash(),
				descriptor: candidate.descriptor.clone(),
				availability_votes: default_availability_votes(),
				relay_parent_number: 5,
				backed_in_number: 6,
				backers: default_backing_bitfield(),
				backing_group: GroupIndex::from(0),
				commitments: candidate.commitments.clone(),
			}]
			.into_iter()
			.collect::<VecDeque<_>>(),
		);

		PendingAvailability::<Test>::insert(
			&chain_b,
			[CandidatePendingAvailability {
				core: CoreIndex::from(1),
				hash: candidate.hash(),
				descriptor: candidate.descriptor,
				availability_votes: default_availability_votes(),
				relay_parent_number: 6,
				backed_in_number: 7,
				backers: default_backing_bitfield(),
				backing_group: GroupIndex::from(1),
				commitments: candidate.commitments,
			}]
			.into_iter()
			.collect::<VecDeque<_>>(),
		);

		run_to_block(11, |_| None);

		assert_eq!(shared::CurrentSessionIndex::<Test>::get(), 5);

		assert!(PendingAvailability::<Test>::get(&chain_a).is_some());
		assert!(PendingAvailability::<Test>::get(&chain_b).is_some());

		run_to_block(12, |n| match n {
			12 => Some(SessionChangeNotification {
				validators: validator_public_new.clone(),
				queued: Vec::new(),
				prev_config: default_config(),
				new_config: default_config(),
				random_seed: Default::default(),
				session_index: 6,
			}),
			_ => None,
		});

		assert_eq!(shared::CurrentSessionIndex::<Test>::get(), 6);

		assert!(PendingAvailability::<Test>::iter().collect::<Vec<_>>().is_empty());
	});
}

/// Assert that the encoding of a known `AggregateMessageOrigin` did not change.
#[test]
fn aggregate_origin_decode_regression_check() {
	let ump = AggregateMessageOrigin::Ump(UmpQueueId::Para(u32::MAX.into()));
	let raw = (0u8, 0u8, u32::MAX).encode();
	let decoded = AggregateMessageOrigin::decode_all(&mut &raw[..]);
	assert_eq!(decoded, Ok(ump), "Migration needed for AggregateMessageOrigin");
}

#[test]
fn para_upgrade_delay_scheduled_from_inclusion() {
	let chain_a = ParaId::from(1_u32);

	// The block number of the relay-parent for testing.
	const RELAY_PARENT_NUM: BlockNumber = 4;

	let paras = vec![(chain_a, ParaKind::Parachain)];
	let validators = vec![
		Sr25519Keyring::Alice,
		Sr25519Keyring::Bob,
		Sr25519Keyring::Charlie,
		Sr25519Keyring::Dave,
		Sr25519Keyring::Ferdie,
	];
	let keystore: KeystorePtr = Arc::new(LocalKeystore::in_memory());
	for validator in validators.iter() {
		Keystore::sr25519_generate_new(
			&*keystore,
			PARACHAIN_KEY_TYPE_ID,
			Some(&validator.to_seed()),
		)
		.unwrap();
	}
	let validator_public = validator_pubkeys(&validators);

	new_test_ext(genesis_config(paras)).execute_with(|| {
		shared::Pallet::<Test>::set_active_validators_ascending(validator_public.clone());
		shared::Pallet::<Test>::set_session_index(5);

		let new_validation_code: ValidationCode = vec![9, 8, 7, 6, 5, 4, 3, 2, 1].into();
		let new_validation_code_hash = new_validation_code.hash();

		// Otherwise upgrade is no-op.
		assert_ne!(new_validation_code, dummy_validation_code());

		run_to_block(5, |_| None);

		let signing_context =
			SigningContext { parent_hash: System::parent_hash(), session_index: 5 };

		let group_validators = |group_index: GroupIndex| {
			match group_index {
				group_index if group_index == GroupIndex::from(0) => Some(vec![0, 1, 2, 3, 4]),
				_ => panic!("Group index out of bounds for 1 parachain"),
			}
			.map(|vs| vs.into_iter().map(ValidatorIndex).collect::<Vec<_>>())
		};

		// When processing candidates, we compute the group index from scheduler.
		let validator_groups = vec![vec![
			ValidatorIndex(0),
			ValidatorIndex(1),
			ValidatorIndex(2),
			ValidatorIndex(3),
			ValidatorIndex(4),
		]];
		Scheduler::set_validator_groups(validator_groups);

		let allowed_scheduling_parents = default_allowed_scheduling_parent_tracker();

		let chain_a_assignment = (chain_a, CoreIndex::from(0));
		let candidate_a = TestCandidateBuilder {
			para_id: chain_a,
			relay_parent: System::parent_hash(),
			pov_hash: Hash::repeat_byte(1),
			persisted_validation_data_hash: make_vdata_hash(chain_a).unwrap(),
			new_validation_code: Some(new_validation_code.clone()),
			hrmp_watermark: RELAY_PARENT_NUM,
			..Default::default()
		}
		.build();

		let backed_a = back_candidate(
			candidate_a.clone(),
			&validators,
			group_validators(GroupIndex::from(0)).unwrap().as_ref(),
			&keystore,
			&signing_context,
			BackingKind::Threshold,
			chain_a_assignment.1,
		);

		let _ =
			ParaInclusion::process_candidates(
				&allowed_scheduling_parents,
				&vec![(chain_a_assignment.0, vec![(backed_a, chain_a_assignment.1)])]
					.into_iter()
					.collect::<BTreeMap<_, _>>(),
				&group_validators,
			)
			.expect("candidates scheduled, in order, and backed");

		// Run a couple of blocks before the inclusion.
		run_to_block(7, |_| None);

		let mut bare_bitfield = default_bitfield();
		*bare_bitfield.0.get_mut(0).unwrap() = true;

		let signed_bitfields = validators
			.iter()
			.enumerate()
			.map(|(i, key)| {
				sign_bitfield(
					&keystore,
					key,
					ValidatorIndex(i as _),
					bare_bitfield.clone(),
					&signing_context,
				)
				.into()
			})
			.collect::<Vec<_>>();

		let checked_bitfields = simple_sanitize_bitfields(
			signed_bitfields,
			DisputedBitfield::zeros(expected_bits()),
			expected_bits(),
		);

		let v = process_bitfields(checked_bitfields);
		assert_eq!(vec![(CoreIndex(0), candidate_a.hash())], v);

		assert!(PendingAvailability::<Test>::get(&chain_a).unwrap().is_empty());

		let active_vote_state = paras::Pallet::<Test>::active_vote_state(&new_validation_code_hash)
			.expect("prechecking must be initiated");

		let cause = &active_vote_state.causes()[0];
		// Upgrade block is the block of inclusion, not candidate's parent.
		assert_matches!(cause,
			paras::PvfCheckCause::Upgrade { id, included_at, upgrade_strategy: UpgradeStrategy::SetGoAheadSignal }
				if id == &chain_a && included_at == &7
		);
	});
}

/// Test that verify_backed_candidate succeeds for a V3 candidate whose relay_parent
/// is from a previous session.
/// Note: rejection of V3 candidates when the feature is disabled is handled by
/// `check_version_acceptance` in the sanitization pipeline, not by `verify_backed_candidate`.
#[test]
fn cross_session_relay_parent_v3() {
	let chain_a = ParaId::from(1_u32);
	let paras = vec![(chain_a, ParaKind::Parachain)];

	new_test_ext(genesis_config(paras)).execute_with(|| {
		shared::Pallet::<Test>::set_session_index(5);
		run_to_block(5, |_| None);

		// Relay parent from the PREVIOUS session (session 4).
		let old_relay_parent = Hash::repeat_byte(0xAA);
		let old_relay_parent_number: BlockNumber = 1;

		// Insert this old relay parent into the DoubleMap under session 4.
		shared::AllowedRelayParents::<Test>::insert(
			4u32, // session 4
			old_relay_parent,
			RelayParentInfo { number: old_relay_parent_number, state_root: Default::default() },
		);

		// Compute PVD hash matching the old relay parent's block number.
		let pvd_hash = make_vdata_hash_with_block_number(chain_a, old_relay_parent_number).unwrap();

		// Build a V3 candidate: relay_parent from session 4, scheduling_parent from
		// current session 5 (the System::parent_hash()).
		let scheduling_parent = System::parent_hash();
		let candidate = TestCandidateBuilder {
			para_id: chain_a,
			relay_parent: old_relay_parent,
			pov_hash: Hash::repeat_byte(1),
			persisted_validation_data_hash: pvd_hash,
			hrmp_watermark: old_relay_parent_number,
			core_index: Some(CoreIndex(0)),
			scheduling_parent: Some(scheduling_parent),
			descriptor_session_index: Some(4), // points to session 4
			..Default::default()
		}
		.build();

		let parent_head = paras::Heads::<Test>::get(chain_a).unwrap_or_default();

		// verify_backed_candidate uses version() which correctly identifies this as V3
		// and looks up the relay parent in session 4's DoubleMap via session_index().
		let check_ctx = CandidateCheckContext::<Test>::new(None);
		let result = check_ctx.verify_backed_candidate(&candidate, parent_head);
		assert!(result.is_ok(), "V3 cross-session relay parent should succeed");
		assert_eq!(result.unwrap(), old_relay_parent_number);
	});
}

/// Test that verify_backed_candidate fails when the relay parent's session has been pruned.
#[test]
fn cross_session_relay_parent_pruned_session_fails() {
	let chain_a = ParaId::from(1_u32);
	let paras = vec![(chain_a, ParaKind::Parachain)];

	new_test_ext(genesis_config(paras)).execute_with(|| {
		shared::Pallet::<Test>::set_session_index(5);
		run_to_block(5, |_| None);

		// Relay parent claims to be from session 2, but we DON'T insert it in the DoubleMap.
		// (Simulates the session having been pruned.)
		let old_relay_parent = Hash::repeat_byte(0xBB);
		let old_relay_parent_number: BlockNumber = 2;
		let pvd_hash = make_vdata_hash_with_block_number(chain_a, old_relay_parent_number).unwrap();

		let candidate = TestCandidateBuilder {
			para_id: chain_a,
			relay_parent: old_relay_parent,
			pov_hash: Hash::repeat_byte(1),
			persisted_validation_data_hash: pvd_hash,
			hrmp_watermark: old_relay_parent_number,
			core_index: Some(CoreIndex(0)),
			scheduling_parent: Some(System::parent_hash()),
			descriptor_session_index: Some(2), // session 2, which is pruned
			..Default::default()
		}
		.build();

		let parent_head = paras::Heads::<Test>::get(chain_a).unwrap_or_default();

		let check_ctx = CandidateCheckContext::<Test>::new(None);
		let result = check_ctx.verify_backed_candidate(&candidate, parent_head);
		assert_matches!(result, Err(Error::<Test>::DisallowedRelayParent));
	});
}

/// Test that verify_backed_candidate fails if the V3 descriptor has the wrong session_index
/// (e.g., session 5 but relay parent is only in session 4).
#[test]
fn cross_session_relay_parent_wrong_session_index_fails() {
	let chain_a = ParaId::from(1_u32);
	let paras = vec![(chain_a, ParaKind::Parachain)];

	new_test_ext(genesis_config(paras)).execute_with(|| {
		shared::Pallet::<Test>::set_session_index(5);
		run_to_block(5, |_| None);

		let old_relay_parent = Hash::repeat_byte(0xCC);
		let old_relay_parent_number: BlockNumber = 2;

		// Insert relay parent in session 4 only.
		shared::AllowedRelayParents::<Test>::insert(
			4u32,
			old_relay_parent,
			RelayParentInfo { number: old_relay_parent_number, state_root: Default::default() },
		);

		let pvd_hash = make_vdata_hash_with_block_number(chain_a, old_relay_parent_number).unwrap();

		// V3 descriptor with session_index=5 (wrong — relay parent is in session 4).
		let candidate = TestCandidateBuilder {
			para_id: chain_a,
			relay_parent: old_relay_parent,
			pov_hash: Hash::repeat_byte(1),
			persisted_validation_data_hash: pvd_hash,
			hrmp_watermark: old_relay_parent_number,
			core_index: Some(CoreIndex(0)),
			scheduling_parent: Some(System::parent_hash()),
			descriptor_session_index: Some(5), // wrong session
			..Default::default()
		}
		.build();

		let parent_head = paras::Heads::<Test>::get(chain_a).unwrap_or_default();

		// Should fail: relay parent is in session 4 but descriptor says session 5.
		let check_ctx = CandidateCheckContext::<Test>::new(None);
		let result = check_ctx.verify_backed_candidate(&candidate, parent_head);
		assert_matches!(result, Err(Error::<Test>::DisallowedRelayParent));
	});
}

/// Process_candidates with a V3 candidate built on a relay parent
/// from a previous session, while the scheduling parent is in the current session.
#[test]
fn cross_session_process_candidates_v3() {
	let chain_a = ParaId::from(1_u32);
	let paras = vec![(chain_a, ParaKind::Parachain)];

	let validators = vec![Sr25519Keyring::Alice, Sr25519Keyring::Bob, Sr25519Keyring::Charlie];
	let keystore: KeystorePtr = Arc::new(LocalKeystore::in_memory());
	for validator in validators.iter() {
		Keystore::sr25519_generate_new(
			&*keystore,
			PARACHAIN_KEY_TYPE_ID,
			Some(&validator.to_seed()),
		)
		.unwrap();
	}
	let validator_public = validator_pubkeys(&validators);

	new_test_ext(genesis_config(paras)).execute_with(|| {
		shared::Pallet::<Test>::set_active_validators_ascending(validator_public.clone());
		shared::Pallet::<Test>::set_session_index(5);

		run_to_block(5, |_| None);

		let group_validators = |group_index: GroupIndex| {
			match group_index {
				group_index if group_index == GroupIndex::from(0) => Some(vec![0, 1, 2]),
				_ => None,
			}
			.map(|vs| vs.into_iter().map(ValidatorIndex).collect::<Vec<_>>())
		};

		Scheduler::set_validator_groups(vec![vec![
			ValidatorIndex(0),
			ValidatorIndex(1),
			ValidatorIndex(2),
		]]);

		// The scheduling parent is in the current session's scheduling tracker.
		let scheduling_parent = System::parent_hash();
		let scheduling_parent_number: BlockNumber = 4;

		let mut allowed_scheduling_parents = AllowedSchedulingParentsTracker::default();
		allowed_scheduling_parents.update(
			scheduling_parent,
			Default::default(),
			scheduling_parent_number,
			10,
		);

		// The relay parent is from the PREVIOUS session (4).
		let old_relay_parent = Hash::repeat_byte(0xDD);
		let old_relay_parent_number: BlockNumber = 2;

		// Insert relay parent in session 4 DoubleMap.
		shared::AllowedRelayParents::<Test>::insert(
			4u32,
			old_relay_parent,
			RelayParentInfo { number: old_relay_parent_number, state_root: Default::default() },
		);

		let pvd_hash = make_vdata_hash_with_block_number(chain_a, old_relay_parent_number).unwrap();

		// Build a V3 candidate with cross-session relay parent.
		let candidate = TestCandidateBuilder {
			para_id: chain_a,
			relay_parent: old_relay_parent,
			pov_hash: Hash::repeat_byte(1),
			persisted_validation_data_hash: pvd_hash,
			hrmp_watermark: old_relay_parent_number,
			core_index: Some(CoreIndex(0)),
			scheduling_parent: Some(scheduling_parent),
			descriptor_session_index: Some(4), // relay parent's session
			..Default::default()
		}
		.build();

		// Sign with the scheduling_parent as context (V3 signing uses scheduling parent).
		let signing_context = SigningContext { parent_hash: scheduling_parent, session_index: 5 };

		let backed = back_candidate(
			candidate.clone(),
			&validators,
			group_validators(GroupIndex::from(0)).unwrap().as_ref(),
			&keystore,
			&signing_context,
			BackingKind::Threshold,
			CoreIndex(0),
		);

		let backed_candidates = vec![(chain_a, vec![(backed, CoreIndex(0))])]
			.into_iter()
			.collect::<BTreeMap<_, _>>();

		// process_candidates should succeed with v3_enabled=true.
		let result = ParaInclusion::process_candidates(
			&allowed_scheduling_parents,
			&backed_candidates,
			&group_validators,
		);

		assert!(
			result.is_ok(),
			"V3 cross-session process_candidates should succeed, got: {:?}",
			result.err()
		);
	});
}

/// `verify_backed_candidate` rejects a candidate whose `new_validation_code` exceeds the
/// on-chain `HostConfiguration.max_code_size`.
#[test]
fn verify_backed_candidate_rejects_oversized_new_validation_code() {
	let chain_a = ParaId::from(1_u32);
	let paras = vec![(chain_a, ParaKind::Parachain)];

	new_test_ext(genesis_config(paras)).execute_with(|| {
		run_to_block(2, |_| None);
		let _ = default_allowed_scheduling_parent_tracker();

		let max_code_size = configuration::ActiveConfig::<Test>::get().max_code_size as usize;
		let oversized_code = vec![0u8; max_code_size + 1];

		let parent_number = System::block_number().saturating_sub(1);
		let candidate = TestCandidateBuilder {
			para_id: chain_a,
			relay_parent: System::parent_hash(),
			pov_hash: Hash::repeat_byte(1),
			persisted_validation_data_hash: make_vdata_hash(chain_a).unwrap(),
			hrmp_watermark: parent_number,
			new_validation_code: Some(oversized_code.into()),
			..Default::default()
		}
		.build();

		let parent_head = paras::Heads::<Test>::get(chain_a).unwrap_or_default();

		let check_ctx = CandidateCheckContext::<Test>::new(None);
		assert_matches!(
			check_ctx.verify_backed_candidate(&candidate, parent_head),
			Err(Error::<Test>::NewCodeTooLarge)
		);
	});
}

/// `check_validation_outputs_for_runtime_api` returns `false` when the commitments contain a
/// `new_validation_code` larger than the on-chain `HostConfiguration.max_code_size`.
#[test]
fn check_validation_outputs_for_runtime_api_rejects_oversized_new_validation_code() {
	let chain_a = ParaId::from(1_u32);
	let paras = vec![(chain_a, ParaKind::Parachain)];

	new_test_ext(genesis_config(paras)).execute_with(|| {
		run_to_block(2, |_| None);

		let max_code_size = configuration::ActiveConfig::<Test>::get().max_code_size as usize;
		let oversized_code = vec![0u8; max_code_size + 1];

		let commitments = CandidateCommitments {
			new_validation_code: Some(oversized_code.into()),
			..Default::default()
		};

		assert!(!ParaInclusion::check_validation_outputs_for_runtime_api(chain_a, 1, commitments,));
	});
}
