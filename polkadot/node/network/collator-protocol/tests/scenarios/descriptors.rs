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

//! V1 / V3 descriptor handling.

mod v1_descriptor_version_detection_with_v3_enabled {
	use crate::common::{
		builders::{Candidate, ProtocolVersion::V1},
		chain::CoreSchedule,
		harness::CollatorSut,
		world::{bootstrap_world, collator_world_config, World, WorldExt as _},
	};
	use polkadot_node_primitives::{BlockData, PoV};
	use polkadot_primitives::{
		CandidateCommitments, CollatorId, CollatorSignature, CoreIndex, HeadData, Id as ParaId,
	};
	use polkadot_primitives_test_helpers::CandidateDescriptor;

	const PARA: ParaId = ParaId::new(2000);

	#[crate::sim_test]
	fn v1_shape_descriptor_via_v1_protocol_under_v3_node_feature<S: CollatorSut>() {
		let config = collator_world_config()
			.with_schedule(CoreIndex(0), CoreSchedule::always(PARA))
			.with_v3_descriptors_enabled();
		let mut w: World<S> = bootstrap_world::<S>(config, None);
		w.new_block().activate();
		let leaf = w.leaf();

		// Build a V1-shape descriptor with non-zero reserved bytes (so V1 detection hits even
		// under a V3-enabled validator).
		let mut collator_bytes = [0u8; 32];
		collator_bytes.iter_mut().enumerate().for_each(|(i, b)| *b = i as u8);
		let mut signature_bytes = [0u8; 64];
		signature_bytes.iter_mut().enumerate().for_each(|(i, b)| *b = i as u8);

		let leaf_n = w.leaf_number();
		let pvd = polkadot_primitives::PersistedValidationData {
			parent_head: HeadData(Vec::new()),
			relay_parent_number: leaf_n,
			relay_parent_storage_root: polkadot_primitives::Hash::zero(),
			max_pov_size: 5 * 1024 * 1024,
		};

		let commitments = CandidateCommitments {
			head_data: HeadData(vec![1]),
			horizontal_messages: Default::default(),
			upward_messages: Default::default(),
			new_validation_code: None,
			processed_downward_messages: 0,
			hrmp_watermark: 0,
		};

		let descriptor: CandidateDescriptor = CandidateDescriptor {
			para_id: PARA,
			relay_parent: leaf,
			collator: CollatorId::from(sp_core::sr25519::Public::from_raw(collator_bytes)),
			persisted_validation_data_hash: pvd.hash(),
			pov_hash: polkadot_primitives::Hash::zero(),
			erasure_root: polkadot_primitives::Hash::zero(),
			signature: CollatorSignature::from(sp_core::sr25519::Signature::from_raw(
				signature_bytes,
			)),
			para_head: commitments.head_data.hash(),
			validation_code_hash: polkadot_primitives_test_helpers::dummy_validation_code().hash(),
		};
		let receipt_v2: polkadot_primitives::CandidateReceiptV2 =
			polkadot_primitives::CandidateReceiptV2 {
				descriptor: descriptor.into(),
				commitments_hash: commitments.hash(),
			};
		let candidate = Candidate::from_receipt(receipt_v2.clone());

		let peer = w.declared_peer(PARA, V1);
		w.base.sim.send(peer.advertise(leaf, None, None));
		let request_id = w.fetch_request(&candidate);
		w.respond_fetch_v1(request_id, receipt_v2, PoV { block_data: BlockData(vec![1]) });
		w.expect_second(&candidate);
	}
}

mod v3_session_index_checks {
	use crate::common::{
		builders::{Candidate, ProtocolVersion::V2},
		harness::CollatorSut,
		world::{activated_world, WorldExt as _},
	};
	use polkadot_node_primitives::{BlockData, PoV};
	use polkadot_primitives::{
		CandidateReceiptV2, CoreIndex, HeadData, Id as ParaId, MutateDescriptorV2,
		PersistedValidationData,
	};
	use polkadot_primitives_test_helpers::dummy_committed_candidate_receipt_v2;
	use std::time::Duration;

	const PARA: ParaId = ParaId::new(2000);

	/// V2 candidate with a session_index that doesn't match the relay parent's session is
	/// rejected. (Our chain has session 0; we set descriptor.session_index=999.)
	#[crate::sim_test]
	fn v2_descriptor_with_wrong_session_index_rejects<S: CollatorSut>() {
		let mut w = activated_world::<S>(&[(CoreIndex(0), PARA)]);

		let pvd = PersistedValidationData {
			parent_head: HeadData(Vec::new()),
			relay_parent_number: w.leaf_number(),
			relay_parent_storage_root: polkadot_primitives::Hash::zero(),
			max_pov_size: 5 * 1024 * 1024,
		};
		let mut committed = dummy_committed_candidate_receipt_v2(w.leaf());
		committed.descriptor.set_para_id(PARA);
		committed.descriptor.set_persisted_validation_data_hash(pvd.hash());
		committed.descriptor.set_core_index(CoreIndex(0));
		committed.descriptor.set_session_index(999); // wrong session
		let receipt: CandidateReceiptV2 = committed.to_plain();
		let candidate = Candidate::from_receipt(receipt.clone());

		let peer = w.declared_peer(PARA, V2);
		w.advertise_with_parent_head(
			&peer,
			w.leaf(),
			candidate.hash(),
			HeadData(Vec::new()).hash(),
		);
		let request_id = w.fetch_request(&candidate);
		w.respond_fetch_v2(request_id, receipt, PoV { block_data: BlockData(vec![1]) });
		w.expect_no_second(&candidate, Duration::from_millis(500));
	}
}
