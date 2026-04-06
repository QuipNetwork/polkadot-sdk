// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

extern crate alloc;

use alloc::collections::btree_map::BTreeMap;
use codec::{Decode, Encode};
use cumulus_primitives_core::{
	relay_chain, AbridgedHostConfiguration, AbridgedHrmpChannel, ParaId,
};
use polkadot_primitives::{Header, UpgradeGoAhead};
use sp_consensus_babe::{
	digests::{CompatibleDigestItem, PreDigest, PrimaryPreDigest},
	AuthorityId, AuthorityPair, BabeAuthorityWeight, VrfSignature,
};
use sp_core::{
	sr25519::vrf::{VrfPreOutput, VrfProof, VrfSignature as Sr25519VrfSignature},
	Pair, H256,
};
use sp_runtime::{
	traits::{HashingFor, Header as HeaderT},
	Digest, DigestItem,
};
use sp_trie::PrefixedMemoryDB;

/// Builds a sproof (portmanteau of 'spoof' and 'proof') of the relay chain state.
#[derive(Clone)]
pub struct RelayStateSproofBuilder {
	/// The para id of the current parachain.
	///
	/// This doesn't get into the storage proof produced by the builder, however, it is used for
	/// generation of the storage image and by auxiliary methods.
	///
	/// It's recommended to change this value once in the very beginning of usage.
	///
	/// The default value is 200.
	pub para_id: ParaId,

	pub host_config: AbridgedHostConfiguration,
	pub dmq_mqc_head: Option<relay_chain::Hash>,
	pub upgrade_go_ahead: Option<UpgradeGoAhead>,
	pub relay_dispatch_queue_remaining_capacity: Option<(u32, u32)>,
	pub hrmp_ingress_channel_index: Option<Vec<ParaId>>,
	pub hrmp_egress_channel_index: Option<Vec<ParaId>>,
	pub hrmp_channels: BTreeMap<relay_chain::HrmpChannelId, AbridgedHrmpChannel>,
	pub current_slot: relay_chain::Slot,
	pub current_epoch: u64,
	pub randomness: relay_chain::Hash,
	pub additional_key_values: Vec<(Vec<u8>, Vec<u8>)>,
	pub included_para_head: Option<relay_chain::HeadData>,
	pub num_authorities: u64,
}

impl Default for RelayStateSproofBuilder {
	fn default() -> Self {
		RelayStateSproofBuilder {
			para_id: ParaId::from(200),
			host_config: cumulus_primitives_core::AbridgedHostConfiguration {
				max_code_size: 2 * 1024 * 1024,
				max_head_data_size: 1024 * 1024,
				max_upward_queue_count: 8,
				max_upward_queue_size: 1024,
				max_upward_message_size: 256,
				max_upward_message_num_per_candidate: 5,
				hrmp_max_message_num_per_candidate: 5,
				validation_upgrade_cooldown: 6,
				validation_upgrade_delay: 6,
				async_backing_params: relay_chain::AsyncBackingParams {
					allowed_ancestry_len: 0,
					max_candidate_depth: 0,
				},
			},
			dmq_mqc_head: None,
			upgrade_go_ahead: None,
			relay_dispatch_queue_remaining_capacity: None,
			hrmp_ingress_channel_index: None,
			hrmp_egress_channel_index: None,
			hrmp_channels: BTreeMap::new(),
			current_slot: 0.into(),
			current_epoch: 0u64,
			randomness: relay_chain::Hash::default(),
			additional_key_values: vec![],
			included_para_head: None,
			num_authorities: 1,
		}
	}
}

impl RelayStateSproofBuilder {
	/// Returns a mutable reference to HRMP channel metadata for a channel (`sender`,
	/// `self.para_id`).
	///
	/// If there is no channel, a new default one is created.
	///
	/// It also updates the `hrmp_ingress_channel_index`, creating it if needed.
	pub fn upsert_inbound_channel(&mut self, sender: ParaId) -> &mut AbridgedHrmpChannel {
		let in_index = self.hrmp_ingress_channel_index.get_or_insert_with(Vec::new);
		if let Err(idx) = in_index.binary_search(&sender) {
			in_index.insert(idx, sender);
		}

		self.upsert_channel(relay_chain::HrmpChannelId { sender, recipient: self.para_id })
	}

	/// Returns a mutable reference to HRMP channel metadata for a channel (`self.para_id`,
	/// `recipient`).
	///
	/// If there is no channel, a new default one is created.
	///
	/// It also updates the `hrmp_egress_channel_index`, creating it if needed.
	pub fn upsert_outbound_channel(&mut self, recipient: ParaId) -> &mut AbridgedHrmpChannel {
		let in_index = self.hrmp_egress_channel_index.get_or_insert_with(Vec::new);
		if let Err(idx) = in_index.binary_search(&recipient) {
			in_index.insert(idx, recipient);
		}

		self.upsert_channel(relay_chain::HrmpChannelId { sender: self.para_id, recipient })
	}

	/// Creates a new default entry in the hrmp channels mapping if not exists, and returns mutable
	/// reference to it.
	fn upsert_channel(&mut self, id: relay_chain::HrmpChannelId) -> &mut AbridgedHrmpChannel {
		self.hrmp_channels.entry(id).or_insert_with(|| AbridgedHrmpChannel {
			max_capacity: 0,
			max_total_size: 0,
			max_message_size: 0,
			msg_count: 0,
			total_size: 0,
			mqc_head: None,
		})
	}

	/// Build sproof and generate relay parent descendants with the configured authorities.
	///
	/// Returns a tuple of (state_root, storage_proof, relay_parent_descendants).
	pub fn into_state_root_proof_and_descendants(
		self,
		relay_parent_offset: u64,
	) -> (polkadot_primitives::Hash, sp_state_machine::StorageProof, Vec<Header>) {
		let authorities = generate_authority_pairs(self.num_authorities);
		let (state_root, proof) = self.into_state_root_and_proof();
		let descendants =
			build_relay_parent_descendants(relay_parent_offset + 1, state_root.into(), authorities);
		(state_root, proof, descendants)
	}

	pub fn into_state_root_and_proof(
		mut self,
	) -> (polkadot_primitives::Hash, sp_state_machine::StorageProof) {
		// Generate and add authorities if num_authorities is set
		if self.num_authorities > 0 {
			let authorities = generate_authority_pairs(self.num_authorities);
			let auth_pair = convert_to_authority_weight_pair(&authorities);

			// Add authorities to the sproof builder
			self.additional_key_values.push((
				relay_chain::well_known_keys::AUTHORITIES.to_vec(),
				auth_pair.clone().encode(),
			));
			self.additional_key_values.push((
				relay_chain::well_known_keys::NEXT_AUTHORITIES.to_vec(),
				auth_pair.encode(),
			));
		}

		let (db, root) =
			PrefixedMemoryDB::<HashingFor<polkadot_primitives::Block>>::default_with_root();
		let state_version = Default::default(); // for test using default.
		let mut backend = sp_state_machine::TrieBackendBuilder::new(db, root).build();

		let mut relevant_keys = Vec::new();
		{
			use codec::Encode as _;

			let mut insert = |key: Vec<u8>, value: Vec<u8>| {
				relevant_keys.push(key.clone());
				backend.insert(vec![(None, vec![(key, Some(value))])], state_version);
			};

			insert(relay_chain::well_known_keys::ACTIVE_CONFIG.to_vec(), self.host_config.encode());
			if let Some(dmq_mqc_head) = self.dmq_mqc_head {
				insert(
					relay_chain::well_known_keys::dmq_mqc_head(self.para_id),
					dmq_mqc_head.encode(),
				);
			}
			if let Some(para_head) = self.included_para_head {
				insert(relay_chain::well_known_keys::para_head(self.para_id), para_head.encode());
			}
			if let Some(relay_dispatch_queue_remaining_capacity) =
				self.relay_dispatch_queue_remaining_capacity
			{
				insert(
					relay_chain::well_known_keys::relay_dispatch_queue_remaining_capacity(
						self.para_id,
					)
					.key,
					relay_dispatch_queue_remaining_capacity.encode(),
				);
			}
			if let Some(upgrade_go_ahead) = self.upgrade_go_ahead {
				insert(
					relay_chain::well_known_keys::upgrade_go_ahead_signal(self.para_id),
					upgrade_go_ahead.encode(),
				);
			}
			if let Some(hrmp_ingress_channel_index) = self.hrmp_ingress_channel_index {
				let mut sorted = hrmp_ingress_channel_index.clone();
				sorted.sort();
				assert_eq!(sorted, hrmp_ingress_channel_index);

				insert(
					relay_chain::well_known_keys::hrmp_ingress_channel_index(self.para_id),
					hrmp_ingress_channel_index.encode(),
				);
			}
			if let Some(hrmp_egress_channel_index) = self.hrmp_egress_channel_index {
				let mut sorted = hrmp_egress_channel_index.clone();
				sorted.sort();
				assert_eq!(sorted, hrmp_egress_channel_index);

				insert(
					relay_chain::well_known_keys::hrmp_egress_channel_index(self.para_id),
					hrmp_egress_channel_index.encode(),
				);
			}
			for (channel, metadata) in self.hrmp_channels {
				insert(relay_chain::well_known_keys::hrmp_channels(channel), metadata.encode());
			}
			insert(relay_chain::well_known_keys::EPOCH_INDEX.to_vec(), self.current_epoch.encode());
			insert(
				relay_chain::well_known_keys::ONE_EPOCH_AGO_RANDOMNESS.to_vec(),
				self.randomness.encode(),
			);
			insert(relay_chain::well_known_keys::CURRENT_SLOT.to_vec(), self.current_slot.encode());

			for (key, value) in self.additional_key_values {
				insert(key, value);
			}
		}

		let root = *backend.root();
		let proof = sp_state_machine::prove_read(backend, relevant_keys).expect("prove read");
		(root, proof)
	}
}

/// Generate a vector of AuthorityPairs
pub fn generate_authority_pairs(num_authorities: u64) -> Vec<AuthorityPair> {
	(0..num_authorities).map(|i| AuthorityPair::from_seed(&[i as u8; 32])).collect()
}

/// Convert AuthorityPair to (AuthorityId, BabeAuthorityWeight)
fn convert_to_authority_weight_pair(
	authorities: &[AuthorityPair],
) -> Vec<(AuthorityId, BabeAuthorityWeight)> {
	authorities
		.iter()
		.map(|auth| (auth.public().into(), Default::default()))
		.collect()
}

/// Add a BABE pre-digest to a generic header
fn add_babe_pre_digest(header: &mut Header, authority_index: u32, block_number: u64) {
	/// This method generates some vrf data, but only to make the compiler happy
	fn generate_testing_vrf() -> VrfSignature {
		let vrf_proof_bytes = [0u8; 64];
		let proof: VrfProof = VrfProof::decode(&mut vrf_proof_bytes.as_slice()).unwrap();
		let vrf_pre_out_bytes = [0u8; 32];
		let pre_output: VrfPreOutput =
			VrfPreOutput::decode(&mut vrf_pre_out_bytes.as_slice()).unwrap();
		VrfSignature::from_sr25519_with_zero_pq(Sr25519VrfSignature { pre_output, proof })
	}

	let pre_digest = PrimaryPreDigest {
		authority_index,
		slot: block_number.into(),
		vrf_signature: generate_testing_vrf(),
	};

	header
		.digest_mut()
		.push(DigestItem::babe_pre_digest(PreDigest::Primary(pre_digest)));
}

/// Create a mock chain of relay headers as descendants of the relay parent
pub fn build_relay_parent_descendants(
	num_headers: u64,
	state_root: H256,
	authorities: Vec<AuthorityPair>,
) -> Vec<Header> {
	let mut headers = Vec::with_capacity(num_headers as usize);

	let mut previous_hash = None;

	for block_number in 0..num_headers as u32 {
		let mut header = Header {
			number: block_number,
			parent_hash: previous_hash.unwrap_or_default(),
			state_root,
			extrinsics_root: H256::default(),
			digest: Digest::default(),
		};
		let authority_index = block_number % (authorities.len() as u32);

		// Add pre-digest
		add_babe_pre_digest(&mut header, authority_index, block_number as u64);

		// Sign and seal the header
		let signature = authorities[authority_index as usize].sign(header.hash().as_bytes());
		header.digest_mut().push(DigestItem::babe_seal(signature.into()));

		previous_hash = Some(header.hash());
		headers.push(header);
	}

	headers
}

#[cfg(test)]
mod tests {
	use super::*;
	use codec::Decode;
	use proptest::prelude::*;
	use sp_consensus_babe::{
		digests::{PreDigest, PrimaryPreDigest},
		AuthorityId, AuthorityPair, BabeAuthorityWeight,
	};
	use sp_core::{crypto::Pair, sr25519::Signature, H256};
	use sp_runtime::{
		generic::{Digest, Header},
		DigestItem,
	};

	/// Tests for `generate_authority_pairs`
	#[test]
	fn test_generate_authority_pairs_count() {
		// Test case 1: Zero authorities
		assert_eq!(generate_authority_pairs(0).len(), 0);

		// Test case 2: A small number of authorities
		assert_eq!(generate_authority_pairs(5).len(), 5);

		// Test case 3: A larger number of authorities
		assert_eq!(generate_authority_pairs(100).len(), 100);

		// Test case 4: Uniqueness of generated authorities
		let pairs = generate_authority_pairs(10);
		let public_keys: std::collections::HashSet<_> =
			pairs.iter().map(|pair| pair.public()).collect();

		assert_eq!(pairs.len(), public_keys.len());
	}

	/// Tests for `convert_to_authority_weight_pair`
	#[test]
	fn test_convert_to_authority_weight_pair() {
		let num_authorities = 3;
		let authorities = generate_authority_pairs(num_authorities);
		let converted_pairs = convert_to_authority_weight_pair(&authorities);

		// Check the count is correct
		assert_eq!(converted_pairs.len(), num_authorities as usize);

		for (i, (authority_id, weight)) in converted_pairs.iter().enumerate() {
			// Check that the AuthorityId is derived correctly from the public key
			let expected_id: AuthorityId = authorities[i].public().into();
			assert_eq!(*authority_id, expected_id);

			// Check that the weight is the default (usually 1)
			assert_eq!(*weight, BabeAuthorityWeight::default());
		}
	}

	/// Tests for `add_babe_pre_digest`
	#[test]
	fn test_add_babe_pre_digest() {
		let mut header = Header {
			number: 0,
			parent_hash: H256::default(),
			state_root: H256::default(),
			extrinsics_root: H256::default(),
			digest: Digest::default(),
		};
		let authority_index = 42;
		let block_number = 100;

		add_babe_pre_digest(&mut header, authority_index, block_number);

		// Ensure exactly one digest item was added
		assert_eq!(header.digest().logs.len(), 1);

		// Check if the added digest item is the correct type and data
		let digest_item = &header.digest().logs[0];

		let pre_digest_data = match digest_item {
			DigestItem::PreRuntime(id, data) if id == &sp_consensus_babe::BABE_ENGINE_ID => {
				PreDigest::decode(&mut &data[..]).unwrap()
			},
			_ => panic!("Expected a BABE pre-digest"),
		};

		match pre_digest_data {
			PreDigest::Primary(PrimaryPreDigest {
				authority_index: auth_idx,
				slot,
				vrf_signature: _,
			}) => {
				assert_eq!(auth_idx, authority_index);
				assert_eq!(slot, relay_chain::Slot::from(block_number));
			},
			_ => panic!("Expected a Primary PreDigest"),
		}
	}

	proptest! {
		// Proptest for `build_relay_parent_descendants` to ensure general properties hold.
		#[test]
		fn prop_test_build_relay_parent_descendants(
			num_headers in 1..20u64, // Test a reasonable range of headers
			seed_bytes: [u8; 32],
			num_authorities in 1..5u64,
		) {
			let state_root = H256::from(seed_bytes);
			let authorities = generate_authority_pairs(num_authorities);

			// Skip test if no authorities are generated (proptest range ensures at least 1)
			if authorities.is_empty() {
				return Ok(());
			}

			let headers = build_relay_parent_descendants(num_headers, state_root, authorities.clone());

			// 1. Check the correct number of headers are generated
			prop_assert_eq!(headers.len(), num_headers as usize);

			let mut previous_hash: Option<H256> = None;

			for (i, header) in headers.iter().enumerate() {
				let block_number = i as u32;
				let expected_authority_index = block_number % (num_authorities as u32);
				let authority_pair = &authorities[expected_authority_index as usize];

				// 2. Check block number and parent hash linkage
				prop_assert_eq!(header.number, block_number);
				prop_assert_eq!(header.parent_hash, previous_hash.unwrap_or_default());
				prop_assert_eq!(header.state_root, state_root);

				// 3. Check for the presence of Babe Pre-Digest and Seal (should be exactly 2 items)
				prop_assert_eq!(header.digest().logs.len(), 2);

				let pre_digest_item = &header.digest().logs[0];
				let seal_item = &header.digest().logs[1];

				// 4. Validate Pre-Digest content
				let pre_digest_data = match pre_digest_item {
					DigestItem::PreRuntime(id, data) if id == &sp_consensus_babe::BABE_ENGINE_ID => {
						PreDigest::decode(&mut &data[..]).unwrap()
					}
					_ => panic!("Expected a BABE pre-digest"),
				};

				if let PreDigest::Primary(PrimaryPreDigest { authority_index, slot, .. }) = pre_digest_data {
					prop_assert_eq!(authority_index, expected_authority_index);
					prop_assert_eq!(slot, relay_chain::Slot::from(block_number as u64));
				} else {
					panic!("Pre-Digest should be Primary");
				}

				// 5. Validate Seal content (check signature)
				let signature = match seal_item {
					DigestItem::Seal(id, data) if id == &sp_consensus_babe::BABE_ENGINE_ID => {
						let raw_sig = Signature::decode(&mut &data[..]).expect("Valid signature");
						sp_consensus_babe::AuthoritySignature::from(raw_sig)
					}
					_ => panic!("Expected a BABE seal"),
				};

				// The signature must be valid for the header's hash without the seal, signed by the expected authority
				// We need to create a copy of the header without the seal to get the correct hash for verification.
				let mut header_without_seal = header.clone();
				header_without_seal.digest_mut().pop(); // Remove the seal
				let header_hash_for_verification = header_without_seal.hash();
				prop_assert!(AuthorityPair::verify(&signature, header_hash_for_verification.as_bytes(), &authority_pair.public()));

				let header_hash = header.hash();

				previous_hash = Some(header_hash);
			}
		}
	}

	/// Test to ensure that when num_authorities is populated, the authorities are included in the
	/// proof
	#[test]
	fn test_authorities_included_in_proof() {
		let mut builder = RelayStateSproofBuilder::default();
		builder.num_authorities = 3;

		let (state_root, proof) = builder.into_state_root_and_proof();

		// Verify that the proof contains the authorities keys
		let authorities_key = relay_chain::well_known_keys::AUTHORITIES;
		let next_authorities_key = relay_chain::well_known_keys::NEXT_AUTHORITIES;

		// At minimum, we should be able to verify that authorities data exists in the storage
		// by reconstructing the storage and checking if the keys exist
		use sp_runtime::traits::HashingFor;
		use sp_state_machine::{Backend, TrieBackendBuilder};
		let db = proof.into_memory_db::<HashingFor<polkadot_primitives::Block>>();
		let backend = TrieBackendBuilder::new(db, state_root).build();

		// Verify authorities key exists and contains 3 authorities
		let authorities_data = backend.storage(authorities_key).unwrap().unwrap();
		let authorities: Vec<(AuthorityId, BabeAuthorityWeight)> =
			codec::Decode::decode(&mut &authorities_data[..]).unwrap();
		assert_eq!(authorities.len(), 3);

		// Verify next_authorities key exists and contains the same 3 authorities
		let next_authorities_data = backend.storage(next_authorities_key).unwrap().unwrap();
		let next_authorities: Vec<(AuthorityId, BabeAuthorityWeight)> =
			codec::Decode::decode(&mut &next_authorities_data[..]).unwrap();
		assert_eq!(next_authorities.len(), 3);

		// Verify they are the same authorities
		assert_eq!(authorities, next_authorities);
	}

	/// Test to ensure into_state_root_proof_and_descendants generates relay_parent_offset+1 headers
	#[test]
	fn test_into_state_root_proof_and_descendants_generates_correct_number_of_headers() {
		let mut builder = RelayStateSproofBuilder::default();
		builder.num_authorities = 2;

		// Test with different relay_parent_offsets
		let test_cases = vec![0, 1, 5, 10];

		for relay_parent_offset in test_cases {
			let builder_clone = builder.clone();
			let (state_root, _proof, descendants) =
				builder_clone.into_state_root_proof_and_descendants(relay_parent_offset);

			// Should generate relay_parent_offset + 1 headers
			let expected_num_headers = relay_parent_offset + 1;
			assert_eq!(
				descendants.len(),
				expected_num_headers as usize,
				"Failed for relay_parent_offset {}: expected {} headers, got {}",
				relay_parent_offset,
				expected_num_headers,
				descendants.len()
			);

			// Verify the headers are properly linked
			for (i, header) in descendants.iter().enumerate() {
				assert_eq!(header.number, i as u32);
				assert_eq!(header.state_root, state_root.into());
			}

			// Verify each header has proper digest items (pre-digest and seal)
			for header in &descendants {
				assert_eq!(
					header.digest().logs.len(),
					2,
					"Each header should have pre-digest and seal"
				);
			}
		}
	}
}
