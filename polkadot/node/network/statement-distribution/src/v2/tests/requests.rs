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

use bitvec::order::Lsb0;
use codec::{Decode, Encode};
use polkadot_node_network_protocol::{
	request_response::v2 as request_v2, v3::BackedCandidateManifest,
};
use polkadot_primitives_test_helpers::{make_candidate, make_candidate_v2, make_candidate_v3};
use sc_network::config::{
	IncomingRequest as RawIncomingRequest, OutgoingResponse as RawOutgoingResponse,
};

use polkadot_primitives::{
	ClaimQueueOffset, CoreSelector, MutateDescriptorV2, UMPSignal, UMP_SEPARATOR,
};
use rstest::rstest;

#[rstest]
#[case(false)]
#[case(true)]
fn cluster_peer_allowed_to_send_incomplete_statements(#[case] use_v3_descriptor: bool) {
	let group_size = 3;
	let config =
		TestConfig { validator_count: 20, group_size, local_validator: LocalRole::Validator };

	let relay_parent = Hash::repeat_byte(1);
	let peer_a = PeerId::random();
	let peer_b = PeerId::random();
	let peer_c = PeerId::random();

	test_harness(config, |state, mut overseer| async move {
		let local_validator = state.local.clone().unwrap();
		let local_group_index = local_validator.group_index.unwrap();
		let local_para = ParaId::from(local_group_index.0);

		let test_leaf = state.make_dummy_leaf(relay_parent);

		let (mut candidate, pvd) = make_candidate_v2(
			relay_parent,
			1,
			local_para,
			test_leaf.para_data(local_para).head_data.clone(),
			vec![4, 5, 6].into(),
			Hash::repeat_byte(42).into(),
		);
		candidate.descriptor.set_core_index(CoreIndex(local_group_index.0));
		if use_v3_descriptor {
			candidate.descriptor.set_version(1);
			candidate.descriptor.set_scheduling_parent(relay_parent);
			// V3 descriptors require UMP signals.
			candidate.commitments.upward_messages.force_push(UMP_SEPARATOR);
			candidate
				.commitments
				.upward_messages
				.force_push(UMPSignal::SelectCore(CoreSelector(0), ClaimQueueOffset(0)).encode());
		}

		let candidate_hash = candidate.hash();

		let other_group_validators = state.group_validators(local_group_index, true);
		let v_a = other_group_validators[0];
		let v_b = other_group_validators[1];

		// peer A is in group, has scheduling parent in view.
		// peer B is in group, has no scheduling parent in view.
		// peer C is not in group, has scheduling parent in view.
		{
			connect_peer(
				&mut overseer,
				peer_a.clone(),
				Some(vec![state.discovery_id(other_group_validators[0])].into_iter().collect()),
			)
			.await;

			connect_peer(
				&mut overseer,
				peer_b.clone(),
				Some(vec![state.discovery_id(other_group_validators[1])].into_iter().collect()),
			)
			.await;

			connect_peer(&mut overseer, peer_c.clone(), None).await;

			send_peer_view_change(&mut overseer, peer_a.clone(), view![relay_parent]).await;
			send_peer_view_change(&mut overseer, peer_c.clone(), view![relay_parent]).await;
		}

		activate_leaf(&mut overseer, &test_leaf, &state, true, vec![]).await;

		// Peer in cluster sends a statement, triggering a request.
		{
			let a_seconded = state
				.sign_statement(
					v_a,
					CompactStatement::Seconded(candidate_hash),
					&SigningContext { parent_hash: relay_parent, session_index: 1 },
				)
				.as_unchecked()
				.clone();

			send_peer_message(
				&mut overseer,
				peer_a.clone(),
				protocol_v3::StatementDistributionMessage::Statement(relay_parent, a_seconded),
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_a && r == BENEFIT_VALID_STATEMENT_FIRST.into() => { }
			);
		}

		// Send a request to peer and mock its response to include just one statement.
		{
			let b_seconded = state
				.sign_statement(
					v_b,
					CompactStatement::Seconded(candidate_hash),
					&SigningContext { parent_hash: relay_parent, session_index: 1 },
				)
				.as_unchecked()
				.clone();
			let statements = vec![b_seconded.clone()];
			// `1` indicates statements NOT to request.
			let mask = StatementFilter::blank(group_size);
			handle_sent_request(
				&mut overseer,
				peer_a,
				candidate_hash,
				mask,
				candidate.clone(),
				pvd.clone(),
				statements,
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_a && r == BENEFIT_VALID_STATEMENT.into() => { }
			);

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_a && r == BENEFIT_VALID_RESPONSE.into() => { }
			);

			assert_matches!(
				overseer.recv().await,
				AllMessages:: NetworkBridgeTx(
					NetworkBridgeTxMessage::SendValidationMessage(
						peers,
						ValidationProtocols::V3(
							protocol_v3::ValidationProtocol::StatementDistribution(
								protocol_v3::StatementDistributionMessage::Statement(hash, statement),
							),
						),
					)
				) => {
					assert_eq!(peers, vec![peer_a]);
					assert_eq!(hash, relay_parent);
					assert_eq!(statement, b_seconded);
				}
			);
		}

		answer_expected_hypothetical_membership_request(&mut overseer, vec![]).await;

		overseer
	});
}

#[test]
fn peer_reported_for_providing_statements_meant_to_be_masked_out() {
	let validator_count = 6;
	let group_size = 3;
	let config = TestConfig { validator_count, group_size, local_validator: LocalRole::Validator };

	// use a scheduling_lookahead of two to restrict the per-core seconding limit to 2.
	let scheduling_lookahead = 2;

	let relay_parent = Hash::repeat_byte(1);
	let peer_c = PeerId::random();
	let peer_d = PeerId::random();
	let peer_e = PeerId::random();

	test_harness(config, |state, mut overseer| async move {
		let local_validator = state.local.clone().unwrap();
		let local_group_index = local_validator.group_index.unwrap();

		let other_group = next_group_index(local_group_index, validator_count, group_size);
		let other_para = ParaId::from(other_group.0);

		let test_leaf =
			state.make_dummy_leaf_with_scheduling_lookahead(relay_parent, scheduling_lookahead);

		let (candidate_1, pvd_1) = make_candidate(
			relay_parent,
			1,
			other_para,
			test_leaf.para_data(other_para).head_data.clone(),
			vec![4, 5, 6].into(),
			Hash::repeat_byte(42).into(),
		);
		let (candidate_2, pvd_2) = make_candidate(
			relay_parent,
			1,
			other_para,
			test_leaf.para_data(other_para).head_data.clone(),
			vec![7, 8, 9].into(),
			Hash::repeat_byte(43).into(),
		);
		let (candidate_3, pvd_3) = make_candidate(
			relay_parent,
			1,
			other_para,
			test_leaf.para_data(other_para).head_data.clone(),
			vec![10, 11, 12].into(),
			Hash::repeat_byte(44).into(),
		);
		let candidate_hash_1 = candidate_1.hash();
		let candidate_hash_2 = candidate_2.hash();
		let candidate_hash_3 = candidate_3.hash();

		let target_group_validators = state.group_validators(other_group, true);
		let v_c = target_group_validators[0];
		let v_d = target_group_validators[1];
		let v_e = target_group_validators[2];

		// Connect C, D, E
		{
			connect_peer(
				&mut overseer,
				peer_c.clone(),
				Some(vec![state.discovery_id(v_c)].into_iter().collect()),
			)
			.await;

			connect_peer(
				&mut overseer,
				peer_d.clone(),
				Some(vec![state.discovery_id(v_d)].into_iter().collect()),
			)
			.await;

			connect_peer(
				&mut overseer,
				peer_e.clone(),
				Some(vec![state.discovery_id(v_e)].into_iter().collect()),
			)
			.await;

			send_peer_view_change(&mut overseer, peer_c.clone(), view![relay_parent]).await;
			send_peer_view_change(&mut overseer, peer_d.clone(), view![relay_parent]).await;
			send_peer_view_change(&mut overseer, peer_e.clone(), view![relay_parent]).await;
		}

		activate_leaf(&mut overseer, &test_leaf, &state, true, vec![]).await;

		// Send gossip topology.
		send_new_topology(&mut overseer, state.make_dummy_topology()).await;

		// Peer C advertises candidate 1.
		{
			let manifest = BackedCandidateManifest {
				scheduling_parent: relay_parent,
				candidate_hash: candidate_hash_1,
				group_index: other_group,
				para_id: other_para,
				parent_head_data_hash: pvd_1.parent_head.hash(),
				statement_knowledge: StatementFilter {
					seconded_in_group: bitvec::bitvec![u8, Lsb0; 1, 1, 0],
					validated_in_group: bitvec::bitvec![u8, Lsb0; 0, 0, 0],
				},
			};

			send_peer_message(
				&mut overseer,
				peer_c.clone(),
				protocol_v3::StatementDistributionMessage::BackedCandidateManifest(
					manifest.clone(),
				),
			)
			.await;

			let statements = vec![
				state
					.sign_statement(
						v_c,
						CompactStatement::Seconded(candidate_hash_1),
						&SigningContext { parent_hash: relay_parent, session_index: 1 },
					)
					.as_unchecked()
					.clone(),
				state
					.sign_statement(
						v_d,
						CompactStatement::Seconded(candidate_hash_1),
						&SigningContext { parent_hash: relay_parent, session_index: 1 },
					)
					.as_unchecked()
					.clone(),
			];
			handle_sent_request(
				&mut overseer,
				peer_c,
				candidate_hash_1,
				StatementFilter::blank(group_size),
				candidate_1.clone(),
				pvd_1.clone(),
				statements,
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_c && r == BENEFIT_VALID_STATEMENT.into()
			);
			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_c && r == BENEFIT_VALID_STATEMENT.into()
			);

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_c && r == BENEFIT_VALID_RESPONSE.into()
			);

			answer_expected_hypothetical_membership_request(&mut overseer, vec![]).await;
		}

		// Peer C advertises candidate 2.
		{
			let manifest = BackedCandidateManifest {
				scheduling_parent: relay_parent,
				candidate_hash: candidate_hash_2,
				group_index: other_group,
				para_id: other_para,
				parent_head_data_hash: pvd_2.parent_head.hash(),
				statement_knowledge: StatementFilter {
					seconded_in_group: bitvec::bitvec![u8, Lsb0; 0, 1, 1],
					validated_in_group: bitvec::bitvec![u8, Lsb0; 0, 0, 0],
				},
			};

			send_peer_message(
				&mut overseer,
				peer_c.clone(),
				protocol_v3::StatementDistributionMessage::BackedCandidateManifest(
					manifest.clone(),
				),
			)
			.await;

			let statements = vec![
				state
					.sign_statement(
						v_d,
						CompactStatement::Seconded(candidate_hash_2),
						&SigningContext { parent_hash: relay_parent, session_index: 1 },
					)
					.as_unchecked()
					.clone(),
				state
					.sign_statement(
						v_e,
						CompactStatement::Seconded(candidate_hash_2),
						&SigningContext { parent_hash: relay_parent, session_index: 1 },
					)
					.as_unchecked()
					.clone(),
			];
			handle_sent_request(
				&mut overseer,
				peer_c,
				candidate_hash_2,
				StatementFilter::blank(group_size),
				candidate_2.clone(),
				pvd_2.clone(),
				statements,
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_c && r == BENEFIT_VALID_STATEMENT.into()
			);
			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_c && r == BENEFIT_VALID_STATEMENT.into()
			);

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_c && r == BENEFIT_VALID_RESPONSE.into()
			);

			answer_expected_hypothetical_membership_request(&mut overseer, vec![]).await;
		}

		// Peer C sends an announcement for candidate 3. Should hit seconding limit for validator 1.
		//
		// NOTE: The manifest is immediately rejected before a request is made due to
		// "over-seconding" validator 1. On the other hand, if the manifest does not include
		// validator 1 as a seconder, then including its Second statement in the response instead
		// would fail with "Un-requested Statement In Response".
		{
			let manifest = BackedCandidateManifest {
				scheduling_parent: relay_parent,
				candidate_hash: candidate_hash_3,
				group_index: other_group,
				para_id: other_para,
				parent_head_data_hash: pvd_3.parent_head.hash(),
				statement_knowledge: StatementFilter {
					seconded_in_group: bitvec::bitvec![u8, Lsb0; 1, 1, 1],
					validated_in_group: bitvec::bitvec![u8, Lsb0; 0, 0, 0],
				},
			};

			send_peer_message(
				&mut overseer,
				peer_c.clone(),
				protocol_v3::StatementDistributionMessage::BackedCandidateManifest(
					manifest.clone(),
				),
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_c && r == COST_EXCESSIVE_SECONDED.into()
			);
		}

		overseer
	});
}

// Peer reported for not providing enough statements, request retried.
#[test]
fn peer_reported_for_not_enough_statements() {
	let validator_count = 6;
	let group_size = 3;
	let config = TestConfig { validator_count, group_size, local_validator: LocalRole::Validator };

	let relay_parent = Hash::repeat_byte(1);
	let peer_c = PeerId::random();
	let peer_d = PeerId::random();
	let peer_e = PeerId::random();

	test_harness(config, |state, mut overseer| async move {
		let local_validator = state.local.clone().unwrap();
		let local_group_index = local_validator.group_index.unwrap();

		let other_group = next_group_index(local_group_index, validator_count, group_size);
		let other_para = ParaId::from(other_group.0);

		let test_leaf = state.make_dummy_leaf(relay_parent);

		let (candidate, pvd) = make_candidate(
			relay_parent,
			1,
			other_para,
			test_leaf.para_data(other_para).head_data.clone(),
			vec![4, 5, 6].into(),
			Hash::repeat_byte(42).into(),
		);
		let candidate_hash = candidate.hash();

		let target_group_validators = state.group_validators(other_group, true);
		let v_c = target_group_validators[0];
		let v_d = target_group_validators[1];
		let v_e = target_group_validators[2];

		// Connect C, D, E
		{
			connect_peer(
				&mut overseer,
				peer_c.clone(),
				Some(vec![state.discovery_id(v_c)].into_iter().collect()),
			)
			.await;

			connect_peer(
				&mut overseer,
				peer_d.clone(),
				Some(vec![state.discovery_id(v_d)].into_iter().collect()),
			)
			.await;

			connect_peer(
				&mut overseer,
				peer_e.clone(),
				Some(vec![state.discovery_id(v_e)].into_iter().collect()),
			)
			.await;

			send_peer_view_change(&mut overseer, peer_c.clone(), view![relay_parent]).await;
			send_peer_view_change(&mut overseer, peer_d.clone(), view![relay_parent]).await;
			send_peer_view_change(&mut overseer, peer_e.clone(), view![relay_parent]).await;
		}

		activate_leaf(&mut overseer, &test_leaf, &state, true, vec![]).await;

		// Send gossip topology.
		send_new_topology(&mut overseer, state.make_dummy_topology()).await;

		let manifest = BackedCandidateManifest {
			scheduling_parent: relay_parent,
			candidate_hash,
			group_index: other_group,
			para_id: other_para,
			parent_head_data_hash: pvd.parent_head.hash(),
			statement_knowledge: StatementFilter {
				seconded_in_group: bitvec::bitvec![u8, Lsb0; 1, 1, 1],
				validated_in_group: bitvec::bitvec![u8, Lsb0; 1, 1, 0],
			},
		};

		// Peer sends an announcement.
		send_peer_message(
			&mut overseer,
			peer_c.clone(),
			protocol_v3::StatementDistributionMessage::BackedCandidateManifest(manifest.clone()),
		)
		.await;

		let c_seconded = state
			.sign_statement(
				v_c,
				CompactStatement::Seconded(candidate_hash),
				&SigningContext { parent_hash: relay_parent, session_index: 1 },
			)
			.as_unchecked()
			.clone();
		let statements = vec![c_seconded.clone()];
		// `1` indicates statements NOT to request.
		let mask = StatementFilter::blank(group_size);

		// We send a request to peer. Mock its response to include just one statement.
		{
			handle_sent_request(
				&mut overseer,
				peer_c,
				candidate_hash,
				mask.clone(),
				candidate.clone(),
				pvd.clone(),
				statements.clone(),
			)
			.await;

			// The peer is reported for only sending one statement.
			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_c && r == COST_INVALID_RESPONSE.into() => { }
			);
		}

		// We re-try the request.
		{
			let statements = vec![
				c_seconded,
				state
					.sign_statement(
						v_d,
						CompactStatement::Seconded(candidate_hash),
						&SigningContext { parent_hash: relay_parent, session_index: 1 },
					)
					.as_unchecked()
					.clone(),
				state
					.sign_statement(
						v_e,
						CompactStatement::Seconded(candidate_hash),
						&SigningContext { parent_hash: relay_parent, session_index: 1 },
					)
					.as_unchecked()
					.clone(),
			];
			handle_sent_request(
				&mut overseer,
				peer_c,
				candidate_hash,
				mask,
				candidate.clone(),
				pvd.clone(),
				statements.clone(),
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_c && r == BENEFIT_VALID_STATEMENT.into()
			);
			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_c && r == BENEFIT_VALID_STATEMENT.into()
			);
			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_c && r == BENEFIT_VALID_STATEMENT.into()
			);

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_c && r == BENEFIT_VALID_RESPONSE.into()
			);

			answer_expected_hypothetical_membership_request(&mut overseer, vec![]).await;
		}

		overseer
	});
}

// Test that a peer answering an `AttestedCandidateRequest` with duplicate statements is punished.
#[test]
fn peer_reported_for_duplicate_statements() {
	let group_size = 3;
	let config =
		TestConfig { validator_count: 20, group_size, local_validator: LocalRole::Validator };

	let relay_parent = Hash::repeat_byte(1);
	let peer_a = PeerId::random();
	let peer_b = PeerId::random();
	let peer_c = PeerId::random();

	test_harness(config, |state, mut overseer| async move {
		let local_validator = state.local.clone().unwrap();
		let local_group_index = local_validator.group_index.unwrap();
		let local_para = ParaId::from(local_group_index.0);

		let test_leaf = state.make_dummy_leaf(relay_parent);

		let (candidate, pvd) = make_candidate(
			relay_parent,
			1,
			local_para,
			test_leaf.para_data(local_para).head_data.clone(),
			vec![4, 5, 6].into(),
			Hash::repeat_byte(42).into(),
		);
		let candidate_hash = candidate.hash();

		let other_group_validators = state.group_validators(local_group_index, true);
		let v_a = other_group_validators[0];
		let v_b = other_group_validators[1];

		// peer A is in group, has scheduling parent in view.
		// peer B is in group, has no scheduling parent in view.
		// peer C is not in group, has scheduling parent in view.
		{
			connect_peer(
				&mut overseer,
				peer_a.clone(),
				Some(vec![state.discovery_id(other_group_validators[0])].into_iter().collect()),
			)
			.await;

			connect_peer(
				&mut overseer,
				peer_b.clone(),
				Some(vec![state.discovery_id(other_group_validators[1])].into_iter().collect()),
			)
			.await;

			connect_peer(&mut overseer, peer_c.clone(), None).await;

			send_peer_view_change(&mut overseer, peer_a.clone(), view![relay_parent]).await;
			send_peer_view_change(&mut overseer, peer_c.clone(), view![relay_parent]).await;
		}

		activate_leaf(&mut overseer, &test_leaf, &state, true, vec![]).await;

		// Peer in cluster sends a statement, triggering a request.
		{
			let a_seconded = state
				.sign_statement(
					v_a,
					CompactStatement::Seconded(candidate_hash),
					&SigningContext { parent_hash: relay_parent, session_index: 1 },
				)
				.as_unchecked()
				.clone();

			send_peer_message(
				&mut overseer,
				peer_a.clone(),
				protocol_v3::StatementDistributionMessage::Statement(relay_parent, a_seconded),
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_a && r == BENEFIT_VALID_STATEMENT_FIRST.into() => { }
			);
		}

		// Send a request to peer and mock its response to include two identical statements.
		{
			let b_seconded = state
				.sign_statement(
					v_b,
					CompactStatement::Seconded(candidate_hash),
					&SigningContext { parent_hash: relay_parent, session_index: 1 },
				)
				.as_unchecked()
				.clone();
			let statements = vec![b_seconded.clone(), b_seconded.clone()];

			handle_sent_request(
				&mut overseer,
				peer_a,
				candidate_hash,
				StatementFilter::blank(group_size),
				candidate.clone(),
				pvd.clone(),
				statements,
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_a && r == BENEFIT_VALID_STATEMENT.into() => { }
			);

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_a && r == COST_UNREQUESTED_RESPONSE_STATEMENT.into() => { }
			);

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_a && r == BENEFIT_VALID_RESPONSE.into() => { }
			);

			assert_matches!(
				overseer.recv().await,
				AllMessages:: NetworkBridgeTx(
					NetworkBridgeTxMessage::SendValidationMessage(
						peers,
						ValidationProtocols::V3(
							protocol_v3::ValidationProtocol::StatementDistribution(
								protocol_v3::StatementDistributionMessage::Statement(hash, statement),
							),
						),
					)
				) => {
					assert_eq!(peers, vec![peer_a]);
					assert_eq!(hash, relay_parent);
					assert_eq!(statement, b_seconded);
				}
			);
		}

		answer_expected_hypothetical_membership_request(&mut overseer, vec![]).await;

		overseer
	});
}

#[test]
fn peer_reported_for_providing_statements_with_invalid_signatures() {
	let group_size = 3;
	let config =
		TestConfig { validator_count: 20, group_size, local_validator: LocalRole::Validator };

	let relay_parent = Hash::repeat_byte(1);
	let peer_a = PeerId::random();
	let peer_b = PeerId::random();
	let peer_c = PeerId::random();

	test_harness(config, |state, mut overseer| async move {
		let local_validator = state.local.clone().unwrap();
		let local_group_index = local_validator.group_index.unwrap();
		let local_para = ParaId::from(local_group_index.0);

		let test_leaf = state.make_dummy_leaf(relay_parent);

		let (candidate, pvd) = make_candidate(
			relay_parent,
			1,
			local_para,
			test_leaf.para_data(local_para).head_data.clone(),
			vec![4, 5, 6].into(),
			Hash::repeat_byte(42).into(),
		);
		let candidate_hash = candidate.hash();

		let other_group_validators = state.group_validators(local_group_index, true);
		state.group_validators((local_group_index.0 + 1).into(), true);
		let v_a = other_group_validators[0];
		let v_b = other_group_validators[1];

		// peer A is in group, has scheduling parent in view.
		// peer B is in group, has no scheduling parent in view.
		// peer C is not in group, has scheduling parent in view.
		{
			connect_peer(
				&mut overseer,
				peer_a.clone(),
				Some(vec![state.discovery_id(other_group_validators[0])].into_iter().collect()),
			)
			.await;

			connect_peer(
				&mut overseer,
				peer_b.clone(),
				Some(vec![state.discovery_id(other_group_validators[1])].into_iter().collect()),
			)
			.await;

			connect_peer(&mut overseer, peer_c.clone(), None).await;

			send_peer_view_change(&mut overseer, peer_a.clone(), view![relay_parent]).await;
			send_peer_view_change(&mut overseer, peer_c.clone(), view![relay_parent]).await;
		}

		activate_leaf(&mut overseer, &test_leaf, &state, true, vec![]).await;

		// Peer in cluster sends a statement, triggering a request.
		{
			let a_seconded = state
				.sign_statement(
					v_a,
					CompactStatement::Seconded(candidate_hash),
					&SigningContext { parent_hash: relay_parent, session_index: 1 },
				)
				.as_unchecked()
				.clone();

			send_peer_message(
				&mut overseer,
				peer_a.clone(),
				protocol_v3::StatementDistributionMessage::Statement(relay_parent, a_seconded),
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_a && r == BENEFIT_VALID_STATEMENT_FIRST.into() => { }
			);
		}

		// Send a request to peer and mock its response to include invalid statements.
		{
			// Sign statement with wrong signing context, leading to bad signature.
			let b_seconded_invalid = state
				.sign_statement(
					v_b,
					CompactStatement::Seconded(candidate_hash),
					&SigningContext { parent_hash: Hash::repeat_byte(42), session_index: 1 },
				)
				.as_unchecked()
				.clone();
			let statements = vec![b_seconded_invalid.clone()];

			handle_sent_request(
				&mut overseer,
				peer_a,
				candidate_hash,
				StatementFilter::blank(group_size),
				candidate.clone(),
				pvd.clone(),
				statements,
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_a && r == COST_INVALID_SIGNATURE.into() => { }
			);

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_a && r == BENEFIT_VALID_RESPONSE.into() => { }
			);

			answer_expected_hypothetical_membership_request(&mut overseer, vec![]).await;
		}

		overseer
	});
}

#[test]
fn peer_reported_for_invalid_v2_descriptor() {
	let group_size = 3;
	let config =
		TestConfig { validator_count: 20, group_size, local_validator: LocalRole::Validator };

	let relay_parent = Hash::repeat_byte(1);
	let peer_a = PeerId::random();
	let peer_b = PeerId::random();
	let peer_c = PeerId::random();

	test_harness(config, |state, mut overseer| async move {
		let local_validator = state.local.clone().unwrap();
		let local_group_index = local_validator.group_index.unwrap();
		let local_para = ParaId::from(local_group_index.0);

		let test_leaf = state.make_dummy_leaf(relay_parent);

		let (mut candidate, pvd) = make_candidate_v2(
			relay_parent,
			1,
			local_para,
			test_leaf.para_data(local_para).head_data.clone(),
			vec![4, 5, 6].into(),
			Hash::repeat_byte(42).into(),
		);

		candidate.descriptor.set_core_index(CoreIndex(100));

		let candidate_hash = candidate.hash();

		let other_group_validators = state.group_validators(local_group_index, true);
		let v_a = other_group_validators[0];
		let v_b = other_group_validators[1];

		// peer A is in group, has scheduling parent in view.
		// peer B is in group, has no scheduling parent in view.
		// peer C is not in group, has scheduling parent in view.
		{
			connect_peer(
				&mut overseer,
				peer_a.clone(),
				Some(vec![state.discovery_id(v_a)].into_iter().collect()),
			)
			.await;

			connect_peer(
				&mut overseer,
				peer_b.clone(),
				Some(vec![state.discovery_id(v_b)].into_iter().collect()),
			)
			.await;

			connect_peer(&mut overseer, peer_c.clone(), None).await;

			send_peer_view_change(&mut overseer, peer_a.clone(), view![relay_parent]).await;
			send_peer_view_change(&mut overseer, peer_c.clone(), view![relay_parent]).await;
		}

		activate_leaf(&mut overseer, &test_leaf, &state, true, vec![]).await;

		// Peer in cluster sends a statement, triggering a request.
		{
			let a_seconded = state
				.sign_statement(
					v_a,
					CompactStatement::Seconded(candidate_hash),
					&SigningContext { parent_hash: relay_parent, session_index: 1 },
				)
				.as_unchecked()
				.clone();

			send_peer_message(
				&mut overseer,
				peer_a.clone(),
				protocol_v3::StatementDistributionMessage::Statement(relay_parent, a_seconded),
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_a && r == BENEFIT_VALID_STATEMENT_FIRST.into() => { }
			);
		}

		// Send a request to peer and mock its response to include a candidate with invalid core
		// index.
		{
			let b_seconded_invalid = state
				.sign_statement(
					v_b,
					CompactStatement::Seconded(candidate_hash),
					&SigningContext { parent_hash: relay_parent, session_index: 1 },
				)
				.as_unchecked()
				.clone();
			let statements = vec![b_seconded_invalid.clone()];

			handle_sent_request(
				&mut overseer,
				peer_a,
				candidate_hash,
				StatementFilter::blank(group_size),
				candidate.clone(),
				pvd.clone(),
				statements,
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_a && r == COST_INVALID_UMP_SIGNALS.into() => { }
			);
		}

		// Test invalid session index
		candidate.descriptor.set_session_index(100);
		// Set good core index
		candidate.descriptor.set_core_index(CoreIndex(local_group_index.0));

		let candidate_hash = candidate.hash();

		// Peer in cluster sends a statement, triggering a request.
		{
			let a_seconded = state
				.sign_statement(
					v_a,
					CompactStatement::Seconded(candidate_hash),
					&SigningContext { parent_hash: relay_parent, session_index: 1 },
				)
				.as_unchecked()
				.clone();

			send_peer_message(
				&mut overseer,
				peer_a.clone(),
				protocol_v3::StatementDistributionMessage::Statement(relay_parent, a_seconded),
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_a && r == BENEFIT_VALID_STATEMENT_FIRST.into() => { }
			);
		}

		// Send a request to peer and mock its response to include a candidate with invalid session
		// index.
		{
			let b_seconded_invalid = state
				.sign_statement(
					v_b,
					CompactStatement::Seconded(candidate_hash),
					&SigningContext { parent_hash: relay_parent, session_index: 1 },
				)
				.as_unchecked()
				.clone();
			let statements = vec![b_seconded_invalid.clone()];

			handle_sent_request(
				&mut overseer,
				peer_a,
				candidate_hash,
				StatementFilter::blank(group_size),
				candidate.clone(),
				pvd.clone(),
				statements,
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_a && r == COST_INVALID_SESSION_INDEX.into() => { }
			);
		}

		// Test valid candidate does not lead to punishment
		candidate.descriptor.set_session_index(1);

		let candidate_hash = candidate.hash();

		// Peer in cluster sends a statement, triggering a request.
		{
			let a_seconded = state
				.sign_statement(
					v_a,
					CompactStatement::Seconded(candidate_hash),
					&SigningContext { parent_hash: relay_parent, session_index: 1 },
				)
				.as_unchecked()
				.clone();

			send_peer_message(
				&mut overseer,
				peer_a.clone(),
				protocol_v3::StatementDistributionMessage::Statement(relay_parent, a_seconded),
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_a && r == BENEFIT_VALID_STATEMENT_FIRST.into() => { }
			);
		}

		// Send a request to peer and mock its response to include a valid candidate.
		{
			let b_seconded_invalid = state
				.sign_statement(
					v_b,
					CompactStatement::Seconded(candidate_hash),
					&SigningContext { parent_hash: relay_parent, session_index: 1 },
				)
				.as_unchecked()
				.clone();
			let statements = vec![b_seconded_invalid.clone()];

			// v_a has exhausted its seconded statements (3).
			let mut statement_filter = StatementFilter::blank(group_size);
			statement_filter
				.seconded_in_group
				.set(state.index_within_group(local_group_index, v_a).unwrap(), true);

			handle_sent_request(
				&mut overseer,
				peer_a,
				candidate_hash,
				statement_filter,
				candidate.clone(),
				pvd.clone(),
				statements,
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_a && r == BENEFIT_VALID_STATEMENT.into() => { }
			);
			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_a && r == BENEFIT_VALID_RESPONSE.into() => { }
			);

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					peers,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::StatementDistribution(
						protocol_v3::StatementDistributionMessage::Statement(
							r,
							s,
						)
					))
				)) => {
					assert_eq!(peers, vec![peer_a.clone()]);
					assert_eq!(r, relay_parent);
					assert_eq!(s.unchecked_payload(), &CompactStatement::Seconded(candidate_hash));
					assert_eq!(s.unchecked_validator_index(), v_b);
				}
			);

			answer_expected_hypothetical_membership_request(&mut overseer, vec![]).await;
		}
		overseer
	});
}

#[test]
// Test that V3 descriptors use scheduling_session() (session_index + offset) for validation,
// not session_index() directly.
fn v3_descriptor_scheduling_session_validation() {
	let group_size = 3;
	let config =
		TestConfig { validator_count: 20, group_size, local_validator: LocalRole::Validator };

	let relay_parent = Hash::repeat_byte(1);
	let peer_a = PeerId::random();
	let peer_b = PeerId::random();
	let peer_c = PeerId::random();

	test_harness(config, |state, mut overseer| async move {
		let local_validator = state.local.clone().unwrap();
		let local_group_index = local_validator.group_index.unwrap();
		let local_para = ParaId::from(local_group_index.0);

		let test_leaf = state.make_dummy_leaf(relay_parent);

		// V3 candidate. Helper defaults: version=1, session_index=1, scheduling_session_offset=0
		// → scheduling_session()=1 (matches session).
		let (mut candidate, pvd) = make_candidate_v3(
			relay_parent,
			1,
			relay_parent, // scheduling_parent = relay_parent
			local_para,
			test_leaf.para_data(local_para).head_data.clone(),
			vec![4, 5, 6].into(),
			Hash::repeat_byte(42).into(),
		);
		candidate.descriptor.set_core_index(CoreIndex(local_group_index.0));
		// V3 descriptors require UMP signals.
		candidate.commitments.upward_messages.force_push(UMP_SEPARATOR);
		candidate
			.commitments
			.upward_messages
			.force_push(UMPSignal::SelectCore(CoreSelector(0), ClaimQueueOffset(0)).encode());

		let candidate_hash = candidate.hash();

		let other_group_validators = state.group_validators(local_group_index, true);
		let v_a = other_group_validators[0];
		let v_b = other_group_validators[1];

		{
			connect_peer(
				&mut overseer,
				peer_a.clone(),
				Some(vec![state.discovery_id(v_a)].into_iter().collect()),
			)
			.await;

			connect_peer(
				&mut overseer,
				peer_b.clone(),
				Some(vec![state.discovery_id(v_b)].into_iter().collect()),
			)
			.await;

			connect_peer(&mut overseer, peer_c.clone(), None).await;

			send_peer_view_change(&mut overseer, peer_a.clone(), view![relay_parent]).await;
			send_peer_view_change(&mut overseer, peer_c.clone(), view![relay_parent]).await;
		}

		activate_leaf(&mut overseer, &test_leaf, &state, true, vec![]).await;

		// V3 candidate with session_index=1, offset=0 → scheduling_session()=1.
		// The session at the scheduling parent is 1, so this should be valid.
		{
			let a_seconded = state
				.sign_statement(
					v_a,
					CompactStatement::Seconded(candidate_hash),
					&SigningContext { parent_hash: relay_parent, session_index: 1 },
				)
				.as_unchecked()
				.clone();

			send_peer_message(
				&mut overseer,
				peer_a.clone(),
				protocol_v3::StatementDistributionMessage::Statement(relay_parent, a_seconded),
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_a && r == BENEFIT_VALID_STATEMENT_FIRST.into() => { }
			);
		}

		// Send the valid V3 candidate — should succeed.
		{
			let b_seconded = state
				.sign_statement(
					v_b,
					CompactStatement::Seconded(candidate_hash),
					&SigningContext { parent_hash: relay_parent, session_index: 1 },
				)
				.as_unchecked()
				.clone();
			let statements = vec![b_seconded.clone()];

			// v_a has only seconded 1 candidate, so not exhausted → mask is blank.
			handle_sent_request(
				&mut overseer,
				peer_a.clone(),
				candidate_hash,
				StatementFilter::blank(group_size),
				candidate.clone(),
				pvd.clone(),
				statements,
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_a && r == BENEFIT_VALID_STATEMENT.into() => { }
			);
			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_a && r == BENEFIT_VALID_RESPONSE.into() => { }
			);

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					peers,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::StatementDistribution(
						protocol_v3::StatementDistributionMessage::Statement(
							r,
							s,
						)
					))
				)) => {
					assert_eq!(peers, vec![peer_a.clone()]);
					assert_eq!(r, relay_parent);
					assert_eq!(s.unchecked_payload(), &CompactStatement::Seconded(candidate_hash));
					assert_eq!(s.unchecked_validator_index(), v_b);
				}
			);

			answer_expected_hypothetical_membership_request(&mut overseer, vec![]).await;
		}

		// Now test with wrong scheduling_session: session_index=1, offset=5 →
		// scheduling_session()=6 ≠ 1. Should be rejected.
		candidate.descriptor.set_scheduling_session_offset(5);

		let candidate_hash = candidate.hash();

		{
			let a_seconded = state
				.sign_statement(
					v_a,
					CompactStatement::Seconded(candidate_hash),
					&SigningContext { parent_hash: relay_parent, session_index: 1 },
				)
				.as_unchecked()
				.clone();

			send_peer_message(
				&mut overseer,
				peer_a.clone(),
				protocol_v3::StatementDistributionMessage::Statement(relay_parent, a_seconded),
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_a && r == BENEFIT_VALID_STATEMENT_FIRST.into() => { }
			);
		}

		{
			let b_seconded = state
				.sign_statement(
					v_b,
					CompactStatement::Seconded(candidate_hash),
					&SigningContext { parent_hash: relay_parent, session_index: 1 },
				)
				.as_unchecked()
				.clone();
			let statements = vec![b_seconded.clone()];

			handle_sent_request(
				&mut overseer,
				peer_a,
				candidate_hash,
				StatementFilter::blank(group_size),
				candidate.clone(),
				pvd.clone(),
				statements,
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_a && r == COST_INVALID_SESSION_INDEX.into() => { }
			);
		}

		overseer
	});
}

#[test]
// Test that a v2 descriptor with an ApprovedPeer UMP signal is ok with v2 receipts node feature
// enabled.
fn approved_peer_ump_signal() {
	let group_size = 3;
	let config =
		TestConfig { validator_count: 20, group_size, local_validator: LocalRole::Validator };

	let relay_parent = Hash::repeat_byte(1);
	let peer_a = PeerId::random();
	let peer_b = PeerId::random();
	let peer_c = PeerId::random();

	test_harness(config, |state, mut overseer| async move {
		let local_validator = state.local.clone().unwrap();
		let local_group_index = local_validator.group_index.unwrap();
		let local_para = ParaId::from(local_group_index.0);

		let test_leaf = state.make_dummy_leaf(relay_parent);

		let (mut candidate, pvd) = make_candidate_v2(
			relay_parent,
			1,
			local_para,
			test_leaf.para_data(local_para).head_data.clone(),
			vec![4, 5, 6].into(),
			Hash::repeat_byte(42).into(),
		);

		candidate.descriptor.set_core_index(CoreIndex(local_group_index.0));
		candidate.descriptor.set_session_index(1);
		candidate.commitments.upward_messages.force_push(UMP_SEPARATOR);
		candidate
			.commitments
			.upward_messages
			.force_push(UMPSignal::SelectCore(CoreSelector(0), ClaimQueueOffset(0)).encode());
		candidate
			.commitments
			.upward_messages
			.force_push(UMPSignal::ApprovedPeer(vec![1, 2, 3].try_into().unwrap()).encode());

		let candidate_hash = candidate.hash();

		let other_group_validators = state.group_validators(local_group_index, true);
		let v_a = other_group_validators[0];
		let v_b = other_group_validators[1];

		// peer A is in group, has scheduling parent in view.
		// peer B is in group, has no scheduling parent in view.
		// peer C is not in group, has scheduling parent in view.
		{
			connect_peer(
				&mut overseer,
				peer_a.clone(),
				Some(vec![state.discovery_id(v_a)].into_iter().collect()),
			)
			.await;

			connect_peer(
				&mut overseer,
				peer_b.clone(),
				Some(vec![state.discovery_id(v_b)].into_iter().collect()),
			)
			.await;

			connect_peer(&mut overseer, peer_c.clone(), None).await;

			send_peer_view_change(&mut overseer, peer_a.clone(), view![relay_parent]).await;
			send_peer_view_change(&mut overseer, peer_c.clone(), view![relay_parent]).await;
		}

		activate_leaf(&mut overseer, &test_leaf, &state, true, vec![]).await;

		// Peer in cluster sends a statement, triggering a request.
		{
			let a_seconded = state
				.sign_statement(
					v_a,
					CompactStatement::Seconded(candidate_hash),
					&SigningContext { parent_hash: relay_parent, session_index: 1 },
				)
				.as_unchecked()
				.clone();

			send_peer_message(
				&mut overseer,
				peer_a.clone(),
				protocol_v3::StatementDistributionMessage::Statement(relay_parent, a_seconded),
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_a && r == BENEFIT_VALID_STATEMENT_FIRST.into() => { }
			);
		}

		// Send a request to peer and mock its response to include a valid candidate.
		{
			let b_seconded = state
				.sign_statement(
					v_b,
					CompactStatement::Seconded(candidate_hash),
					&SigningContext { parent_hash: relay_parent, session_index: 1 },
				)
				.as_unchecked()
				.clone();
			let statements = vec![b_seconded.clone()];

			handle_sent_request(
				&mut overseer,
				peer_a,
				candidate_hash,
				StatementFilter::blank(group_size),
				candidate.clone(),
				pvd.clone(),
				statements,
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_a && r == BENEFIT_VALID_STATEMENT.into() => { }
			);
			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_a && r == BENEFIT_VALID_RESPONSE.into() => { }
			);

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					peers,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::StatementDistribution(
						protocol_v3::StatementDistributionMessage::Statement(
							r,
							s,
						)
					))
				)) => {
					assert_eq!(peers, vec![peer_a.clone()]);
					assert_eq!(r, relay_parent);
					assert_eq!(s.unchecked_payload(), &CompactStatement::Seconded(candidate_hash));
					assert_eq!(s.unchecked_validator_index(), v_b);
				}
			);

			answer_expected_hypothetical_membership_request(&mut overseer, vec![]).await;
		}
		overseer
	});
}

#[test]
fn peer_reported_for_providing_statements_with_wrong_validator_id() {
	let group_size = 3;
	let config =
		TestConfig { validator_count: 20, group_size, local_validator: LocalRole::Validator };

	let relay_parent = Hash::repeat_byte(1);
	let peer_a = PeerId::random();
	let peer_b = PeerId::random();
	let peer_c = PeerId::random();

	test_harness(config, |state, mut overseer| async move {
		let local_validator = state.local.clone().unwrap();
		let local_group_index = local_validator.group_index.unwrap();
		let local_para = ParaId::from(local_group_index.0);

		let test_leaf = state.make_dummy_leaf(relay_parent);

		let (candidate, pvd) = make_candidate(
			relay_parent,
			1,
			local_para,
			test_leaf.para_data(local_para).head_data.clone(),
			vec![4, 5, 6].into(),
			Hash::repeat_byte(42).into(),
		);
		let candidate_hash = candidate.hash();

		let other_group_validators = state.group_validators(local_group_index, true);
		let next_group_validators = state.group_validators((local_group_index.0 + 1).into(), true);
		let v_a = other_group_validators[0];
		let v_c = next_group_validators[0];

		// peer A is in group, has scheduling parent in view.
		// peer B is in group, has no scheduling parent in view.
		// peer C is not in group, has scheduling parent in view.
		{
			connect_peer(
				&mut overseer,
				peer_a.clone(),
				Some(vec![state.discovery_id(other_group_validators[0])].into_iter().collect()),
			)
			.await;

			connect_peer(
				&mut overseer,
				peer_b.clone(),
				Some(vec![state.discovery_id(other_group_validators[1])].into_iter().collect()),
			)
			.await;

			connect_peer(&mut overseer, peer_c.clone(), None).await;

			send_peer_view_change(&mut overseer, peer_a.clone(), view![relay_parent]).await;
			send_peer_view_change(&mut overseer, peer_c.clone(), view![relay_parent]).await;
		}

		activate_leaf(&mut overseer, &test_leaf, &state, true, vec![]).await;

		// Peer in cluster sends a statement, triggering a request.
		{
			let a_seconded = state
				.sign_statement(
					v_a,
					CompactStatement::Seconded(candidate_hash),
					&SigningContext { parent_hash: relay_parent, session_index: 1 },
				)
				.as_unchecked()
				.clone();

			send_peer_message(
				&mut overseer,
				peer_a.clone(),
				protocol_v3::StatementDistributionMessage::Statement(relay_parent, a_seconded),
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_a && r == BENEFIT_VALID_STATEMENT_FIRST.into() => { }
			);
		}

		// Send a request to peer and mock its response to include a wrong validator ID.
		{
			let c_seconded_invalid = state
				.sign_statement(
					v_c,
					CompactStatement::Seconded(candidate_hash),
					&SigningContext { parent_hash: relay_parent, session_index: 1 },
				)
				.as_unchecked()
				.clone();
			let statements = vec![c_seconded_invalid.clone()];

			handle_sent_request(
				&mut overseer,
				peer_a,
				candidate_hash,
				StatementFilter::blank(group_size),
				candidate.clone(),
				pvd.clone(),
				statements,
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_a && r == COST_UNREQUESTED_RESPONSE_STATEMENT.into() => { }
			);

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_a && r == BENEFIT_VALID_RESPONSE.into() => { }
			);

			answer_expected_hypothetical_membership_request(&mut overseer, vec![]).await;
		}

		overseer
	});
}

#[test]
fn disabled_validators_added_to_unwanted_mask() {
	let group_size = 3;
	let config =
		TestConfig { validator_count: 20, group_size, local_validator: LocalRole::Validator };

	let relay_parent = Hash::repeat_byte(1);
	let peer_disabled = PeerId::random();
	let peer_b = PeerId::random();

	test_harness(config, |state, mut overseer| async move {
		let local_validator = state.local.clone().unwrap();
		let local_group_index = local_validator.group_index.unwrap();
		let local_para = ParaId::from(local_group_index.0);
		let other_group_validators = state.group_validators(local_group_index, true);
		let index_disabled = other_group_validators[0];
		let index_within_group = state.index_within_group(local_group_index, index_disabled);
		let index_b = other_group_validators[1];

		let disabled_validators = vec![index_disabled];
		let test_leaf =
			state.make_dummy_leaf_with_disabled_validators(relay_parent, disabled_validators);

		let (candidate, pvd) = make_candidate(
			relay_parent,
			1,
			local_para,
			test_leaf.para_data(local_para).head_data.clone(),
			vec![4, 5, 6].into(),
			Hash::repeat_byte(42).into(),
		);
		let candidate_hash = candidate.hash();

		// peer A is in group, has scheduling parent in view and disabled.
		// peer B is in group, has scheduling parent in view.
		{
			connect_peer(
				&mut overseer,
				peer_disabled.clone(),
				Some(vec![state.discovery_id(index_disabled)].into_iter().collect()),
			)
			.await;
			connect_peer(
				&mut overseer,
				peer_b.clone(),
				Some(vec![state.discovery_id(index_b)].into_iter().collect()),
			)
			.await;
			send_peer_view_change(&mut overseer, peer_disabled.clone(), view![relay_parent]).await;
			send_peer_view_change(&mut overseer, peer_b.clone(), view![relay_parent]).await;
		}

		activate_leaf(&mut overseer, &test_leaf, &state, true, vec![]).await;

		let seconded_disabled = state
			.sign_statement(
				index_disabled,
				CompactStatement::Seconded(candidate_hash),
				&SigningContext { parent_hash: relay_parent, session_index: 1 },
			)
			.as_unchecked()
			.clone();

		let seconded_b = state
			.sign_statement(
				index_b,
				CompactStatement::Seconded(candidate_hash),
				&SigningContext { parent_hash: relay_parent, session_index: 1 },
			)
			.as_unchecked()
			.clone();
		{
			send_peer_message(
				&mut overseer,
				peer_disabled.clone(),
				protocol_v3::StatementDistributionMessage::Statement(
					relay_parent,
					seconded_disabled.clone(),
				),
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_disabled && r == COST_DISABLED_VALIDATOR.into() => { }
			);
		}

		{
			send_peer_message(
				&mut overseer,
				peer_b.clone(),
				protocol_v3::StatementDistributionMessage::Statement(
					relay_parent,
					seconded_b.clone(),
				),
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_b && r == BENEFIT_VALID_STATEMENT_FIRST.into() => { }
			);
		}

		// Send a request to peer and mock its response with a statement from disabled validator.
		{
			let statements = vec![seconded_disabled];
			let mut mask = StatementFilter::blank(group_size);
			let i = index_within_group.unwrap();
			mask.seconded_in_group.set(i, true);
			mask.validated_in_group.set(i, true);

			handle_sent_request(
				&mut overseer,
				peer_b,
				candidate_hash,
				mask,
				candidate.clone(),
				pvd.clone(),
				statements,
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_b && r == COST_UNREQUESTED_RESPONSE_STATEMENT.into() => { }
			);

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_b && r == BENEFIT_VALID_RESPONSE.into() => { }
			);

			assert_matches!(
				overseer.recv().await,
				AllMessages:: NetworkBridgeTx(
					NetworkBridgeTxMessage::SendValidationMessage(
						peers,
						ValidationProtocols::V3(
							protocol_v3::ValidationProtocol::StatementDistribution(
								protocol_v3::StatementDistributionMessage::Statement(hash, statement),
							),
						),
					)
				) => {
					assert_eq!(peers, vec![peer_disabled]);
					assert_eq!(hash, relay_parent);
					assert_eq!(statement, seconded_b);
				}
			);
			answer_expected_hypothetical_membership_request(&mut overseer, vec![]).await;
		}

		overseer
	});
}

#[test]
fn disabling_works_from_relay_parent_not_the_latest_state() {
	let group_size = 3;
	let config =
		TestConfig { validator_count: 20, group_size, local_validator: LocalRole::Validator };

	let relay_1 = Hash::repeat_byte(1);
	let relay_2 = Hash::repeat_byte(2);
	let peer_disabled = PeerId::random();

	test_harness(config, |state, mut overseer| async move {
		let local_validator = state.local.clone().unwrap();
		let local_group_index = local_validator.group_index.unwrap();
		let local_para = ParaId::from(local_group_index.0);

		let other_group_validators = state.group_validators(local_group_index, true);
		let index_disabled = other_group_validators[0];

		let leaf_1 = state.make_dummy_leaf(relay_1);
		let disabled_validators = vec![index_disabled];
		let leaf_2 = state.make_dummy_leaf_with_disabled_validators(relay_2, disabled_validators);

		let (candidate_1, pvd_1) = make_candidate(
			relay_1,
			1,
			local_para,
			leaf_1.para_data(local_para).head_data.clone(),
			vec![4, 5, 6].into(),
			Hash::repeat_byte(42).into(),
		);
		let candidate_1_hash = candidate_1.hash();

		let (candidate_2, pvd_2) = make_candidate(
			relay_1,
			1,
			local_para,
			leaf_1.para_data(local_para).head_data.clone(),
			vec![4, 5, 6, 7].into(),
			Hash::repeat_byte(42).into(),
		);
		let candidate_2_hash = candidate_2.hash();

		let (candidate_3, _) = make_candidate(
			relay_2,
			1,
			local_para,
			leaf_1.para_data(local_para).head_data.clone(),
			vec![4, 5, 6, 7].into(),
			Hash::repeat_byte(42).into(),
		);
		let candidate_3_hash = candidate_3.hash();

		{
			connect_peer(
				&mut overseer,
				peer_disabled.clone(),
				Some(vec![state.discovery_id(index_disabled)].into_iter().collect()),
			)
			.await;
			send_peer_view_change(&mut overseer, peer_disabled.clone(), view![relay_1]).await;
		}

		activate_leaf(&mut overseer, &leaf_1, &state, true, vec![]).await;

		let seconded_1 = state
			.sign_statement(
				index_disabled,
				CompactStatement::Seconded(candidate_1_hash),
				&SigningContext { parent_hash: relay_1, session_index: 1 },
			)
			.as_unchecked()
			.clone();

		let seconded_2 = state
			.sign_statement(
				index_disabled,
				CompactStatement::Seconded(candidate_2_hash),
				&SigningContext { parent_hash: relay_1, session_index: 1 },
			)
			.as_unchecked()
			.clone();

		let seconded_3 = state
			.sign_statement(
				index_disabled,
				CompactStatement::Seconded(candidate_3_hash),
				&SigningContext { parent_hash: relay_2, session_index: 1 },
			)
			.as_unchecked()
			.clone();

		{
			send_peer_message(
				&mut overseer,
				peer_disabled.clone(),
				protocol_v3::StatementDistributionMessage::Statement(relay_1, seconded_1.clone()),
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
				if p == peer_disabled && r == BENEFIT_VALID_STATEMENT_FIRST.into() => { }
			);
		}

		{
			handle_sent_request(
				&mut overseer,
				peer_disabled,
				candidate_1_hash,
				StatementFilter::blank(group_size),
				candidate_1.clone(),
				pvd_1.clone(),
				vec![seconded_1.clone()],
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_disabled && r == BENEFIT_VALID_STATEMENT.into() => { }
			);

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_disabled && r == BENEFIT_VALID_RESPONSE.into() => { }
			);

			answer_expected_hypothetical_membership_request(&mut overseer, vec![]).await;
		}

		activate_leaf(&mut overseer, &leaf_2, &state, false, vec![]).await;

		{
			send_peer_message(
				&mut overseer,
				peer_disabled.clone(),
				protocol_v3::StatementDistributionMessage::Statement(relay_1, seconded_2.clone()),
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_disabled && r == BENEFIT_VALID_STATEMENT_FIRST.into() => { }
			);
		}

		{
			handle_sent_request(
				&mut overseer,
				peer_disabled,
				candidate_2_hash,
				StatementFilter::blank(group_size),
				candidate_2.clone(),
				pvd_2.clone(),
				vec![seconded_2.clone()],
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_disabled && r == BENEFIT_VALID_STATEMENT.into() => { }
			);

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_disabled && r == BENEFIT_VALID_RESPONSE.into() => { }
			);

			answer_expected_hypothetical_membership_request(&mut overseer, vec![]).await;
		}

		{
			send_peer_message(
				&mut overseer,
				peer_disabled.clone(),
				protocol_v3::StatementDistributionMessage::Statement(relay_2, seconded_3.clone()),
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_disabled && r == COST_DISABLED_VALIDATOR.into() => { }
			);
		}

		overseer
	});
}

#[test]
fn local_node_sanity_checks_incoming_requests() {
	let config =
		TestConfig { validator_count: 20, group_size: 3, local_validator: LocalRole::Validator };

	let relay_parent = Hash::repeat_byte(1);
	let peer_a = PeerId::random();
	let peer_b = PeerId::random();
	let peer_c = PeerId::random();
	let peer_d = PeerId::random();

	test_harness(config, |mut state, mut overseer| async move {
		let local_validator = state.local.clone().unwrap();
		let local_group_index = local_validator.group_index.unwrap();
		let local_para = ParaId::from(local_group_index.0);

		let test_leaf = state.make_dummy_leaf(relay_parent);

		let (candidate, pvd) = make_candidate(
			relay_parent,
			1,
			local_para,
			test_leaf.para_data(local_para).head_data.clone(),
			vec![4, 5, 6].into(),
			Hash::repeat_byte(42).into(),
		);
		let candidate_hash = candidate.hash();

		// peer A is in group, has scheduling parent in view.
		// peer B is in group, has no scheduling parent in view.
		// peer C is not in group, has scheduling parent in view.
		{
			let other_group_validators = state.group_validators(local_group_index, true);

			connect_peer(
				&mut overseer,
				peer_a.clone(),
				Some(vec![state.discovery_id(other_group_validators[0])].into_iter().collect()),
			)
			.await;

			connect_peer(
				&mut overseer,
				peer_b.clone(),
				Some(vec![state.discovery_id(other_group_validators[1])].into_iter().collect()),
			)
			.await;

			connect_peer(&mut overseer, peer_c.clone(), None).await;

			send_peer_view_change(&mut overseer, peer_a.clone(), view![relay_parent]).await;
			send_peer_view_change(&mut overseer, peer_c.clone(), view![relay_parent]).await;
		}

		activate_leaf(&mut overseer, &test_leaf, &state, true, vec![]).await;

		let mask = StatementFilter::blank(state.config.group_size);

		// Should drop requests for unknown candidates.
		{
			let (pending_response, rx) = oneshot::channel();
			state
				.req_sender
				.send(RawIncomingRequest {
					// Request from peer that received manifest.
					peer: peer_c,
					payload: request_v2::AttestedCandidateRequest {
						candidate_hash: candidate.hash(),
						mask: mask.clone(),
					}
					.encode(),
					pending_response,
				})
				.await
				.unwrap();

			assert_matches!(rx.await, Err(oneshot::Canceled));
		}

		// Confirm candidate.
		{
			let full_signed = state
				.sign_statement(
					local_validator.validator_index,
					CompactStatement::Seconded(candidate_hash),
					&SigningContext { session_index: 1, parent_hash: relay_parent },
				)
				.convert_to_superpayload(StatementWithPVD::Seconded(candidate.clone(), pvd.clone()))
				.unwrap();

			overseer
				.send(FromOrchestra::Communication {
					msg: StatementDistributionMessage::Share(relay_parent, full_signed),
				})
				.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					peers,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::StatementDistribution(
						protocol_v3::StatementDistributionMessage::Statement(
							r,
							s,
						)
					))
				)) => {
					assert_eq!(peers, vec![peer_a.clone()]);
					assert_eq!(r, relay_parent);
					assert_eq!(s.unchecked_payload(), &CompactStatement::Seconded(candidate_hash));
					assert_eq!(s.unchecked_validator_index(), local_validator.validator_index);
				}
			);

			answer_expected_hypothetical_membership_request(&mut overseer, vec![]).await;
		}

		// Should drop requests from unknown peers.
		{
			let (pending_response, rx) = oneshot::channel();
			state
				.req_sender
				.send(RawIncomingRequest {
					// Request from peer that received manifest.
					peer: peer_d,
					payload: request_v2::AttestedCandidateRequest {
						candidate_hash: candidate.hash(),
						mask: mask.clone(),
					}
					.encode(),
					pending_response,
				})
				.await
				.unwrap();

			assert_matches!(rx.await, Err(oneshot::Canceled));
		}

		// Should drop requests with bitfields of the wrong size.
		{
			let mask = StatementFilter::blank(state.config.group_size + 1);
			let response = state
				.send_request(
					peer_a,
					request_v2::AttestedCandidateRequest { candidate_hash: candidate.hash(), mask },
				)
				.await
				.await
				.unwrap();

			assert_matches!(
				response,
				RawOutgoingResponse {
					result,
					reputation_changes,
					sent_feedback
				} => {
					assert_matches!(result, Err(()));
					assert_eq!(reputation_changes, vec![COST_INVALID_REQUEST_BITFIELD_SIZE.into()]);
					assert_matches!(sent_feedback, None);
				}
			);
		}

		// Local node should reject requests if we did not send a manifest to that peer.
		{
			let response = state
				.send_request(
					peer_c,
					request_v2::AttestedCandidateRequest {
						candidate_hash: candidate.hash(),
						mask: mask.clone(),
					},
				)
				.await
				.await
				.unwrap();

			// Should get `COST_UNEXPECTED_REQUEST` response.
			assert_matches!(
				response,
				RawOutgoingResponse {
					result,
					reputation_changes,
					sent_feedback
				} => {
					assert_matches!(result, Err(()));
					assert_eq!(reputation_changes, vec![COST_UNEXPECTED_REQUEST.into()]);
					assert_matches!(sent_feedback, None);
				}
			);
		}

		overseer
	});
}

#[test]
fn local_node_checks_that_peer_can_request_before_responding() {
	let config =
		TestConfig { validator_count: 20, group_size: 3, local_validator: LocalRole::Validator };

	let relay_parent = Hash::repeat_byte(1);
	let peer_a = PeerId::random();
	let peer_b = PeerId::random();

	test_harness(config, |mut state, mut overseer| async move {
		let local_validator = state.local.clone().unwrap();
		let local_group_index = local_validator.group_index.unwrap();
		let local_para = ParaId::from(local_group_index.0);

		let test_leaf = state.make_dummy_leaf(relay_parent);

		let (candidate, pvd) = make_candidate(
			relay_parent,
			1,
			local_para,
			test_leaf.para_data(local_para).head_data.clone(),
			vec![4, 5, 6].into(),
			Hash::repeat_byte(42).into(),
		);
		let candidate_hash = candidate.hash();

		// Peers A and B are in group and have relay parent in view.
		let other_group_validators = state.group_validators(local_group_index, true);

		connect_peer(
			&mut overseer,
			peer_a.clone(),
			Some(vec![state.discovery_id(other_group_validators[0])].into_iter().collect()),
		)
		.await;

		connect_peer(
			&mut overseer,
			peer_b.clone(),
			Some(vec![state.discovery_id(other_group_validators[1])].into_iter().collect()),
		)
		.await;
		let peer_b_index = other_group_validators[1];

		send_peer_view_change(&mut overseer, peer_a.clone(), view![relay_parent]).await;
		send_peer_view_change(&mut overseer, peer_b.clone(), view![relay_parent]).await;

		// Finish setup
		activate_leaf(&mut overseer, &test_leaf, &state, true, vec![]).await;

		let mask = StatementFilter::blank(state.config.group_size);

		// Confirm candidate.
		let signed = state.sign_statement(
			local_validator.validator_index,
			CompactStatement::Seconded(candidate_hash),
			&SigningContext { session_index: 1, parent_hash: relay_parent },
		);
		let full_signed = signed
			.clone()
			.convert_to_superpayload(StatementWithPVD::Seconded(candidate.clone(), pvd.clone()))
			.unwrap();

		overseer
			.send(FromOrchestra::Communication {
				msg: StatementDistributionMessage::Share(relay_parent, full_signed),
			})
			.await;

		assert_matches!(
			overseer.recv().await,
			AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
				peers,
				ValidationProtocols::V3(protocol_v3::ValidationProtocol::StatementDistribution(
					protocol_v3::StatementDistributionMessage::Statement(
						r,
						s,
					)
				))
			)) => {
				assert_eq!(peers, vec![peer_a.clone(), peer_b.clone()]);
				assert_eq!(r, relay_parent);
				assert_eq!(s.unchecked_payload(), &CompactStatement::Seconded(candidate_hash));
				assert_eq!(s.unchecked_validator_index(), local_validator.validator_index);
			}
		);

		answer_expected_hypothetical_membership_request(&mut overseer, vec![]).await;

		// Local node should respond to requests from peers in the same group
		// which appear to not have already seen the candidate
		{
			// Peer requests candidate and local responds
			let response = state
				.send_request(
					peer_a,
					request_v2::AttestedCandidateRequest {
						candidate_hash: candidate.hash(),
						mask: mask.clone(),
					},
				)
				.await
				.await
				.unwrap();

			let expected_statements = vec![signed.into_unchecked()];
			assert_matches!(response, full_response => {
				// Response is the same for vstaging.
				let request_v2::AttestedCandidateResponse { candidate_receipt, persisted_validation_data, statements } =
					request_v2::AttestedCandidateResponse::decode(
						&mut full_response.result.expect("We should have a proper answer").as_ref(),
					).expect("Decoding should work");
				assert_eq!(candidate_receipt, candidate);
				assert_eq!(persisted_validation_data, pvd);
				assert_eq!(statements, expected_statements);
			});
		}

		// Local node should reject requests if the requester appears to know
		// the candidate (has sent them a Seconded statement)
		{
			let statement = state
				.sign_statement(
					peer_b_index,
					CompactStatement::Seconded(candidate_hash),
					&SigningContext { parent_hash: relay_parent, session_index: 1 },
				)
				.as_unchecked()
				.clone();

			send_peer_message(
				&mut overseer,
				peer_b.clone(),
				protocol_v3::StatementDistributionMessage::Statement(relay_parent, statement),
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_b && r == BENEFIT_VALID_STATEMENT_FIRST.into() => { }
			);

			let response = state
				.send_request(
					peer_b,
					request_v2::AttestedCandidateRequest {
						candidate_hash: candidate.hash(),
						mask: mask.clone(),
					},
				)
				.await
				.await
				.unwrap();

			// Peer already knows about this candidate. Should reject.
			assert_matches!(
				response,
				RawOutgoingResponse {
					result,
					reputation_changes,
					sent_feedback
				} => {
					assert_matches!(result, Err(()));
					assert_eq!(reputation_changes, vec![COST_UNEXPECTED_REQUEST.into()]);
					assert_matches!(sent_feedback, None);
				}
			);

			// Handling leftover statement distribution message
			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					peers,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::StatementDistribution(
						protocol_v3::StatementDistributionMessage::Statement(
							r,
							s,
						)
					))
				)) => {
					assert_eq!(peers, vec![peer_a.clone()]);
					assert_eq!(r, relay_parent);
					assert_eq!(s.unchecked_payload(), &CompactStatement::Seconded(candidate_hash));
					assert_eq!(s.unchecked_validator_index(), peer_b_index);
				}
			);
		}

		overseer
	});
}

#[test]
fn local_node_respects_statement_mask() {
	let validator_count = 6;
	let group_size = 3;
	let config = TestConfig { validator_count, group_size, local_validator: LocalRole::Validator };

	let relay_parent = Hash::repeat_byte(1);
	let peer_a = PeerId::random();
	let peer_b = PeerId::random();
	let peer_c = PeerId::random();
	let peer_d = PeerId::random();

	test_harness(config, |mut state, mut overseer| async move {
		let local_validator = state.local.clone().unwrap();
		let local_group_index = local_validator.group_index.unwrap();
		let local_para = ParaId::from(local_group_index.0);

		let test_leaf = state.make_dummy_leaf_with_min_backing_votes(relay_parent, 2);

		let (candidate, pvd) = make_candidate(
			relay_parent,
			1,
			local_para,
			test_leaf.para_data(local_para).head_data.clone(),
			vec![4, 5, 6].into(),
			Hash::repeat_byte(42).into(),
		);
		let candidate_hash = candidate.hash();

		let other_group_validators = state.group_validators(local_group_index, true);
		let target_group_validators =
			state.group_validators((local_group_index.0 + 1).into(), true);
		let v_a = other_group_validators[0];
		let v_b = other_group_validators[1];
		let v_c = target_group_validators[0];
		let v_d = target_group_validators[1];

		// peer A is in group, has scheduling parent in view.
		// peer B is in group, has no scheduling parent in view.
		// peer C is not in group, has scheduling parent in view.
		// peer D is not in group, has no scheduling parent in view.
		{
			connect_peer(
				&mut overseer,
				peer_a.clone(),
				Some(vec![state.discovery_id(v_a)].into_iter().collect()),
			)
			.await;

			connect_peer(
				&mut overseer,
				peer_b.clone(),
				Some(vec![state.discovery_id(v_b)].into_iter().collect()),
			)
			.await;

			connect_peer(
				&mut overseer,
				peer_c.clone(),
				Some(vec![state.discovery_id(v_c)].into_iter().collect()),
			)
			.await;

			connect_peer(
				&mut overseer,
				peer_d.clone(),
				Some(vec![state.discovery_id(v_d)].into_iter().collect()),
			)
			.await;

			send_peer_view_change(&mut overseer, peer_a.clone(), view![relay_parent]).await;
			send_peer_view_change(&mut overseer, peer_c.clone(), view![relay_parent]).await;
		}

		activate_leaf(&mut overseer, &test_leaf, &state, true, vec![]).await;

		// Send gossip topology.
		send_new_topology(&mut overseer, state.make_dummy_topology()).await;

		// Confirm the candidate locally so that we don't send out requests.
		{
			let statement = state
				.sign_full_statement(
					local_validator.validator_index,
					Statement::Seconded(candidate.clone()),
					&SigningContext { parent_hash: relay_parent, session_index: 1 },
					pvd.clone(),
				)
				.clone();

			overseer
				.send(FromOrchestra::Communication {
					msg: StatementDistributionMessage::Share(relay_parent, statement),
				})
				.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(peers, _)) if peers == vec![peer_a]
			);

			answer_expected_hypothetical_membership_request(&mut overseer, vec![]).await;
		}

		// Send enough statements to make candidate backable, make sure announcements are sent.

		// Send statement from peer A.
		let statement_a = state
			.sign_statement(
				v_a,
				CompactStatement::Seconded(candidate_hash),
				&SigningContext { parent_hash: relay_parent, session_index: 1 },
			)
			.as_unchecked()
			.clone();
		{
			send_peer_message(
				&mut overseer,
				peer_a.clone(),
				protocol_v3::StatementDistributionMessage::Statement(
					relay_parent,
					statement_a.clone(),
				),
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_a && r == BENEFIT_VALID_STATEMENT_FIRST.into() => { }
			);
		}

		// Send statement from peer B.
		let statement_b = state
			.sign_statement(
				v_b,
				CompactStatement::Seconded(candidate_hash),
				&SigningContext { parent_hash: relay_parent, session_index: 1 },
			)
			.as_unchecked()
			.clone();
		{
			send_peer_message(
				&mut overseer,
				peer_b.clone(),
				protocol_v3::StatementDistributionMessage::Statement(
					relay_parent,
					statement_b.clone(),
				),
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_b && r == BENEFIT_VALID_STATEMENT_FIRST.into() => { }
			);

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(peers, _)) if peers == vec![peer_a]
			);
		}

		// Send Backed notification.
		{
			overseer
				.send(FromOrchestra::Communication {
					msg: StatementDistributionMessage::Backed(candidate_hash),
				})
				.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages:: NetworkBridgeTx(
					NetworkBridgeTxMessage::SendValidationMessage(
						peers,
						ValidationProtocols::V3(
							protocol_v3::ValidationProtocol::StatementDistribution(
								protocol_v3::StatementDistributionMessage::BackedCandidateManifest(manifest),
							),
						),
					)
				) => {
					assert_eq!(peers, vec![peer_c]);
					assert_eq!(manifest, BackedCandidateManifest {
						scheduling_parent: relay_parent,
						candidate_hash,
						group_index: local_group_index,
						para_id: local_para,
						parent_head_data_hash: pvd.parent_head.hash(),
						statement_knowledge: StatementFilter {
							seconded_in_group: bitvec::bitvec![u8, Lsb0; 1, 1, 1],
							validated_in_group: bitvec::bitvec![u8, Lsb0; 0, 0, 0],
						},
					});
				}
			);

			answer_expected_hypothetical_membership_request(&mut overseer, vec![]).await;
		}

		// `1` indicates statements NOT to request.
		let mask = StatementFilter {
			seconded_in_group: bitvec::bitvec![u8, Lsb0; 0, 0, 1],
			validated_in_group: bitvec::bitvec![u8, Lsb0; 0, 0, 0],
		};

		// Incoming request to local node. Local node should send statements, respecting mask.
		{
			let response = state
				.send_request(
					peer_c,
					request_v2::AttestedCandidateRequest { candidate_hash: candidate.hash(), mask },
				)
				.await
				.await
				.unwrap();

			let expected_statements = vec![statement_a, statement_b];
			assert_matches!(response, full_response => {
				// Response is the same for v2.
				let request_v2::AttestedCandidateResponse { candidate_receipt, persisted_validation_data, statements } =
					request_v2::AttestedCandidateResponse::decode(
						&mut full_response.result.expect("We should have a proper answer").as_ref(),
					).expect("Decoding should work");
				assert_eq!(candidate_receipt, candidate);
				assert_eq!(persisted_validation_data, pvd);
				assert_eq!(statements, expected_statements);
			});
		}

		overseer
	});
}

#[test]
fn should_delay_before_retrying_dropped_requests() {
	let validator_count = 6;
	let group_size = 3;
	let config = TestConfig { validator_count, group_size, local_validator: LocalRole::Validator };

	let relay_parent = Hash::repeat_byte(1);
	let peer_c = PeerId::random();
	let peer_d = PeerId::random();
	let peer_e = PeerId::random();

	test_harness(config, |state, mut overseer| async move {
		let local_validator = state.local.clone().unwrap();
		let local_group_index = local_validator.group_index.unwrap();

		let other_group = next_group_index(local_group_index, validator_count, group_size);
		let other_para = ParaId::from(other_group.0);

		let test_leaf = state.make_dummy_leaf(relay_parent);

		let (candidate_1, pvd_1) = make_candidate(
			relay_parent,
			1,
			other_para,
			test_leaf.para_data(other_para).head_data.clone(),
			vec![4, 5, 6].into(),
			Hash::repeat_byte(42).into(),
		);
		let (candidate_2, pvd_2) = make_candidate(
			relay_parent,
			1,
			other_para,
			test_leaf.para_data(other_para).head_data.clone(),
			vec![7, 8, 9].into(),
			Hash::repeat_byte(43).into(),
		);
		let candidate_hash_1 = candidate_1.hash();
		let candidate_hash_2 = candidate_2.hash();

		let target_group_validators = state.group_validators(other_group, true);
		let v_c = target_group_validators[0];
		let v_d = target_group_validators[1];
		let v_e = target_group_validators[2];

		// Connect C, D, E
		{
			connect_peer(
				&mut overseer,
				peer_c.clone(),
				Some(vec![state.discovery_id(v_c)].into_iter().collect()),
			)
			.await;

			connect_peer(
				&mut overseer,
				peer_d.clone(),
				Some(vec![state.discovery_id(v_d)].into_iter().collect()),
			)
			.await;

			connect_peer(
				&mut overseer,
				peer_e.clone(),
				Some(vec![state.discovery_id(v_e)].into_iter().collect()),
			)
			.await;

			send_peer_view_change(&mut overseer, peer_c.clone(), view![relay_parent]).await;
			send_peer_view_change(&mut overseer, peer_d.clone(), view![relay_parent]).await;
			send_peer_view_change(&mut overseer, peer_e.clone(), view![relay_parent]).await;
		}

		activate_leaf(&mut overseer, &test_leaf, &state, true, vec![]).await;

		// Send gossip topology.
		send_new_topology(&mut overseer, state.make_dummy_topology()).await;

		// `1` indicates statements NOT to request.
		let mask = StatementFilter::blank(group_size);

		// Send a request about a candidate.
		{
			let manifest = BackedCandidateManifest {
				scheduling_parent: relay_parent,
				candidate_hash: candidate_hash_1,
				group_index: other_group,
				para_id: other_para,
				parent_head_data_hash: pvd_1.parent_head.hash(),
				statement_knowledge: StatementFilter {
					seconded_in_group: bitvec::bitvec![u8, Lsb0; 1, 1, 1],
					validated_in_group: bitvec::bitvec![u8, Lsb0; 1, 1, 0],
				},
			};

			// Peer sends an announcement.
			send_peer_message(
				&mut overseer,
				peer_c.clone(),
				protocol_v3::StatementDistributionMessage::BackedCandidateManifest(
					manifest.clone(),
				),
			)
			.await;

			// We send a request to peer. Drop the request without sending a response.
			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendRequests(mut requests, IfDisconnected::ImmediateError)) => {
					assert_eq!(requests.len(), 1);
					assert_matches!(
						requests.pop().unwrap(),
						Requests::AttestedCandidateV2(outgoing) => {
							assert_eq!(outgoing.peer, Recipient::Peer(peer_c));
							assert_eq!(outgoing.payload.candidate_hash, candidate_hash_1);
							assert_eq!(outgoing.payload.mask, mask);
						}
					);
				}
			);

			assert_matches!(
				overseer_recv_with_timeout(&mut overseer, Duration::from_millis(100)).await,
				None
			);
		}

		// We still send requests about different candidates as per usual.
		{
			let manifest = BackedCandidateManifest {
				scheduling_parent: relay_parent,
				candidate_hash: candidate_hash_2,
				group_index: other_group,
				para_id: other_para,
				parent_head_data_hash: pvd_2.parent_head.hash(),
				statement_knowledge: StatementFilter {
					seconded_in_group: bitvec::bitvec![u8, Lsb0; 1, 1, 1],
					validated_in_group: bitvec::bitvec![u8, Lsb0; 1, 1, 0],
				},
			};

			// Peer sends an announcement.
			send_peer_message(
				&mut overseer,
				peer_c.clone(),
				protocol_v3::StatementDistributionMessage::BackedCandidateManifest(
					manifest.clone(),
				),
			)
			.await;

			let statements = vec![
				state
					.sign_statement(
						v_c,
						CompactStatement::Seconded(candidate_hash_2),
						&SigningContext { parent_hash: relay_parent, session_index: 1 },
					)
					.as_unchecked()
					.clone(),
				state
					.sign_statement(
						v_d,
						CompactStatement::Seconded(candidate_hash_2),
						&SigningContext { parent_hash: relay_parent, session_index: 1 },
					)
					.as_unchecked()
					.clone(),
				state
					.sign_statement(
						v_e,
						CompactStatement::Seconded(candidate_hash_2),
						&SigningContext { parent_hash: relay_parent, session_index: 1 },
					)
					.as_unchecked()
					.clone(),
			];

			// Don't drop this request.
			handle_sent_request(
				&mut overseer,
				peer_c,
				candidate_hash_2,
				mask.clone(),
				candidate_2.clone(),
				pvd_2.clone(),
				statements.clone(),
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_c && r == BENEFIT_VALID_STATEMENT.into()
			);
			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_c && r == BENEFIT_VALID_STATEMENT.into()
			);
			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_c && r == BENEFIT_VALID_STATEMENT.into()
			);

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_c && r == BENEFIT_VALID_RESPONSE.into()
			);

			answer_expected_hypothetical_membership_request(&mut overseer, vec![]).await;
		}

		// Sleep for the given amount of time. This should reset the delay for the first candidate.
		futures_timer::Delay::new(REQUEST_RETRY_DELAY).await;

		// We re-try the first request the second time  drop it again.
		assert_matches!(
			overseer.recv().await,
			AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendRequests(mut requests, IfDisconnected::ImmediateError)) => {
				assert_eq!(requests.len(), 1);
				assert_matches!(
					requests.pop().unwrap(),
					Requests::AttestedCandidateV2(outgoing) => {
						assert_eq!(outgoing.peer, Recipient::Peer(peer_c));
						assert_eq!(outgoing.payload.candidate_hash, candidate_hash_1);
						assert_eq!(outgoing.payload.mask, mask);
					}
				);
			}
		);

		assert_matches!(
			overseer_recv_with_timeout(&mut overseer, Duration::from_millis(100)).await,
			None
		);

		// Sleep for the given amount of time. This should reset the delay for the first candidate.
		futures_timer::Delay::new(REQUEST_RETRY_DELAY).await;

		// We re-try the first request, for the third time, so let's answer to it.
		{
			let statements = vec![
				state
					.sign_statement(
						v_c,
						CompactStatement::Seconded(candidate_hash_1),
						&SigningContext { parent_hash: relay_parent, session_index: 1 },
					)
					.as_unchecked()
					.clone(),
				state
					.sign_statement(
						v_d,
						CompactStatement::Seconded(candidate_hash_1),
						&SigningContext { parent_hash: relay_parent, session_index: 1 },
					)
					.as_unchecked()
					.clone(),
				state
					.sign_statement(
						v_e,
						CompactStatement::Seconded(candidate_hash_1),
						&SigningContext { parent_hash: relay_parent, session_index: 1 },
					)
					.as_unchecked()
					.clone(),
			];
			handle_sent_request(
				&mut overseer,
				peer_c,
				candidate_hash_1,
				mask,
				candidate_1.clone(),
				pvd_1.clone(),
				statements.clone(),
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_c && r == BENEFIT_VALID_STATEMENT.into()
			);
			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_c && r == BENEFIT_VALID_STATEMENT.into()
			);
			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_c && r == BENEFIT_VALID_STATEMENT.into()
			);

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_c && r == BENEFIT_VALID_RESPONSE.into()
			);

			answer_expected_hypothetical_membership_request(&mut overseer, vec![]).await;
		}

		overseer
	});
}
