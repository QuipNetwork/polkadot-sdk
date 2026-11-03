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

use polkadot_primitives::{
	node_features, CandidateDescriptorVersion, CollatorId, CollatorSignature,
};
use polkadot_primitives_test_helpers::{make_candidate, CandidateDescriptor};
use sp_application_crypto::ByteArray;

#[test]
fn share_seconded_circulated_to_cluster() {
	let config =
		TestConfig { validator_count: 20, group_size: 3, local_validator: LocalRole::Validator };

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

		// sharing a `Seconded` message confirms a candidate, which leads to new
		// fragment chain updates.
		answer_expected_hypothetical_membership_request(&mut overseer, vec![]).await;

		overseer
	});
}

#[test]
fn cluster_valid_statement_before_seconded_ignored() {
	let config =
		TestConfig { validator_count: 20, group_size: 3, local_validator: LocalRole::Validator };

	let relay_parent = Hash::repeat_byte(1);
	let peer_a = PeerId::random();

	test_harness(config, |state, mut overseer| async move {
		let local_validator = state.local.clone().unwrap();
		let local_group_index = local_validator.group_index.unwrap();
		let candidate_hash = CandidateHash(Hash::repeat_byte(42));

		let test_leaf = state.make_dummy_leaf(relay_parent);

		// peer A is in group, has scheduling parent in view.
		let other_group_validators = state.group_validators(local_group_index, true);
		let v_a = other_group_validators[0];
		connect_peer(
			&mut overseer,
			peer_a.clone(),
			Some(vec![state.discovery_id(v_a)].into_iter().collect()),
		)
		.await;

		send_peer_view_change(&mut overseer, peer_a.clone(), view![relay_parent]).await;
		activate_leaf(&mut overseer, &test_leaf, &state, true, vec![]).await;

		let signed_valid = state.sign_statement(
			v_a,
			CompactStatement::Valid(candidate_hash),
			&SigningContext { parent_hash: relay_parent, session_index: 1 },
		);

		send_peer_message(
			&mut overseer,
			peer_a.clone(),
			protocol_v3::StatementDistributionMessage::Statement(
				relay_parent,
				signed_valid.as_unchecked().clone(),
			),
		)
		.await;

		assert_matches!(
			overseer.recv().await,
			AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r))) => {
				assert_eq!(p, peer_a);
				assert_eq!(r, COST_UNEXPECTED_STATEMENT_CLUSTER_REJECTED.into());
			}
		);

		overseer
	});
}

#[test]
fn cluster_statement_bad_signature() {
	let config =
		TestConfig { validator_count: 20, group_size: 3, local_validator: LocalRole::Validator };

	let relay_parent = Hash::repeat_byte(1);
	let peer_a = PeerId::random();

	test_harness(config, |state, mut overseer| async move {
		let local_validator = state.local.clone().unwrap();
		let local_group_index = local_validator.group_index.unwrap();
		let candidate_hash = CandidateHash(Hash::repeat_byte(42));

		let test_leaf = state.make_dummy_leaf(relay_parent);

		// peer A is in group, has scheduling parent in view.
		let other_group_validators = state.group_validators(local_group_index, true);
		let v_a = other_group_validators[0];
		let v_b = other_group_validators[1];

		connect_peer(
			&mut overseer,
			peer_a.clone(),
			Some(vec![state.discovery_id(v_a)].into_iter().collect()),
		)
		.await;

		send_peer_view_change(&mut overseer, peer_a.clone(), view![relay_parent]).await;
		activate_leaf(&mut overseer, &test_leaf, &state, true, vec![]).await;

		// sign statements with wrong signing context, leading to bad signature.
		let statements = vec![
			(v_a, CompactStatement::Seconded(candidate_hash)),
			(v_b, CompactStatement::Seconded(candidate_hash)),
		]
		.into_iter()
		.map(|(v, s)| {
			state.sign_statement(
				v,
				s,
				&SigningContext { parent_hash: Hash::repeat_byte(69), session_index: 1 },
			)
		})
		.map(|s| s.as_unchecked().clone());

		for statement in statements {
			send_peer_message(
				&mut overseer,
				peer_a.clone(),
				protocol_v3::StatementDistributionMessage::Statement(
					relay_parent,
					statement.clone(),
				),
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_a && r == COST_INVALID_SIGNATURE.into() => { },
				"{:?}",
				statement
			);
		}

		overseer
	});
}

#[test]
fn useful_cluster_statement_from_non_cluster_peer_rejected() {
	let config =
		TestConfig { validator_count: 20, group_size: 3, local_validator: LocalRole::Validator };

	let relay_parent = Hash::repeat_byte(1);
	let peer_a = PeerId::random();

	test_harness(config, |state, mut overseer| async move {
		let local_validator = state.local.clone().unwrap();
		let local_group_index = local_validator.group_index.unwrap();
		let candidate_hash = CandidateHash(Hash::repeat_byte(42));

		let test_leaf = state.make_dummy_leaf(relay_parent);

		// peer A is not in group, has scheduling parent in view.
		let not_our_group = if local_group_index.0 == 0 { GroupIndex(1) } else { GroupIndex(0) };

		let that_group_validators = state.group_validators(not_our_group, false);
		let v_non = that_group_validators[0];

		connect_peer(
			&mut overseer,
			peer_a.clone(),
			Some(vec![state.discovery_id(v_non)].into_iter().collect()),
		)
		.await;

		send_peer_view_change(&mut overseer, peer_a.clone(), view![relay_parent]).await;
		activate_leaf(&mut overseer, &test_leaf, &state, true, vec![]).await;

		let statement = state
			.sign_statement(
				v_non,
				CompactStatement::Seconded(candidate_hash),
				&SigningContext { parent_hash: relay_parent, session_index: 1 },
			)
			.as_unchecked()
			.clone();

		send_peer_message(
			&mut overseer,
			peer_a.clone(),
			protocol_v3::StatementDistributionMessage::Statement(relay_parent, statement),
		)
		.await;

		assert_matches!(
			overseer.recv().await,
			AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
				if p == peer_a && r == COST_UNEXPECTED_STATEMENT_INVALID_SENDER.into() => { }
		);

		overseer
	});
}

// Both validators in the test are part of backing groups assigned to same parachain
#[test]
fn elastic_scaling_useful_cluster_statement_from_non_cluster_peer_rejected() {
	let config =
		TestConfig { validator_count: 20, group_size: 3, local_validator: LocalRole::Validator };

	let relay_parent = Hash::repeat_byte(1);
	let peer_a = PeerId::random();

	test_harness(config, |state, mut overseer| async move {
		let candidate_hash = CandidateHash(Hash::repeat_byte(42));

		let test_leaf = state.make_dummy_leaf_with_multiple_cores_per_para(relay_parent, 3);

		// Peer A is not in our group, but its group is assigned to same para as we are.
		let not_our_group = GroupIndex(1);

		let that_group_validators = state.group_validators(not_our_group, false);
		let v_non = that_group_validators[0];

		connect_peer(
			&mut overseer,
			peer_a.clone(),
			Some(vec![state.discovery_id(v_non)].into_iter().collect()),
		)
		.await;

		send_peer_view_change(&mut overseer, peer_a.clone(), view![relay_parent]).await;
		activate_leaf(&mut overseer, &test_leaf, &state, true, vec![]).await;

		let statement = state
			.sign_statement(
				v_non,
				CompactStatement::Seconded(candidate_hash),
				&SigningContext { parent_hash: relay_parent, session_index: 1 },
			)
			.as_unchecked()
			.clone();

		send_peer_message(
			&mut overseer,
			peer_a.clone(),
			protocol_v3::StatementDistributionMessage::Statement(relay_parent, statement),
		)
		.await;

		assert_matches!(
			overseer.recv().await,
			AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
				if p == peer_a && r == COST_UNEXPECTED_STATEMENT_INVALID_SENDER.into() => { }
		);

		overseer
	});
}

#[test]
fn statement_from_non_cluster_originator_unexpected() {
	let config =
		TestConfig { validator_count: 20, group_size: 3, local_validator: LocalRole::Validator };

	let relay_parent = Hash::repeat_byte(1);
	let peer_a = PeerId::random();

	test_harness(config, |state, mut overseer| async move {
		let local_validator = state.local.clone().unwrap();
		let local_group_index = local_validator.group_index.unwrap();
		let candidate_hash = CandidateHash(Hash::repeat_byte(42));

		let test_leaf = state.make_dummy_leaf(relay_parent);

		// peer A is not in group, has scheduling parent in view.
		let other_group_validators = state.group_validators(local_group_index, true);
		let v_a = other_group_validators[0];

		connect_peer(&mut overseer, peer_a.clone(), None).await;

		send_peer_view_change(&mut overseer, peer_a.clone(), view![relay_parent]).await;
		activate_leaf(&mut overseer, &test_leaf, &state, true, vec![]).await;

		let statement = state
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
			protocol_v3::StatementDistributionMessage::Statement(relay_parent, statement),
		)
		.await;

		assert_matches!(
			overseer.recv().await,
			AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
				if p == peer_a && r == COST_UNEXPECTED_STATEMENT_INVALID_SENDER.into() => { }
		);

		overseer
	});
}

#[test]
fn seconded_statement_leads_to_request() {
	let group_size = 3;
	let config =
		TestConfig { validator_count: 20, group_size, local_validator: LocalRole::Validator };

	let relay_parent = Hash::repeat_byte(1);
	let peer_a = PeerId::random();

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

		// peer A is in group, has scheduling parent in view.
		let other_group_validators = state.group_validators(local_group_index, true);
		let v_a = other_group_validators[0];

		connect_peer(
			&mut overseer,
			peer_a.clone(),
			Some(vec![state.discovery_id(v_a)].into_iter().collect()),
		)
		.await;

		send_peer_view_change(&mut overseer, peer_a.clone(), view![relay_parent]).await;
		activate_leaf(&mut overseer, &test_leaf, &state, true, vec![]).await;

		let statement = state
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
			protocol_v3::StatementDistributionMessage::Statement(relay_parent, statement),
		)
		.await;

		assert_matches!(
			overseer.recv().await,
			AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
				if p == peer_a && r == BENEFIT_VALID_STATEMENT_FIRST.into() => { }
		);

		handle_sent_request(
			&mut overseer,
			peer_a,
			candidate_hash,
			StatementFilter::blank(group_size),
			candidate.clone(),
			pvd.clone(),
			vec![],
		)
		.await;

		assert_matches!(
			overseer.recv().await,
			AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
				if p == peer_a && r == BENEFIT_VALID_RESPONSE.into() => { }
		);

		answer_expected_hypothetical_membership_request(&mut overseer, vec![]).await;

		overseer
	});
}

#[test]
fn cluster_statements_shared_seconded_first() {
	let config =
		TestConfig { validator_count: 20, group_size: 3, local_validator: LocalRole::Validator };

	let relay_parent = Hash::repeat_byte(1);
	let peer_a = PeerId::random();

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

		// peer A is in group, no scheduling parent in view.
		{
			let other_group_validators = state.group_validators(local_group_index, true);

			connect_peer(
				&mut overseer,
				peer_a.clone(),
				Some(vec![state.discovery_id(other_group_validators[0])].into_iter().collect()),
			)
			.await;
		}

		activate_leaf(&mut overseer, &test_leaf, &state, true, vec![]).await;

		let full_signed = state
			.sign_statement(
				local_validator.validator_index,
				CompactStatement::Seconded(candidate_hash),
				&SigningContext { session_index: 1, parent_hash: relay_parent },
			)
			.convert_to_superpayload(StatementWithPVD::Seconded(candidate.clone(), pvd.clone()))
			.unwrap();

		let valid_signed = state
			.sign_statement(
				local_validator.validator_index,
				CompactStatement::Valid(candidate_hash),
				&SigningContext { session_index: 1, parent_hash: relay_parent },
			)
			.convert_to_superpayload(StatementWithPVD::Valid(candidate_hash))
			.unwrap();

		overseer
			.send(FromOrchestra::Communication {
				msg: StatementDistributionMessage::Share(relay_parent, full_signed),
			})
			.await;

		// result of new confirmed candidate.
		answer_expected_hypothetical_membership_request(&mut overseer, vec![]).await;

		overseer
			.send(FromOrchestra::Communication {
				msg: StatementDistributionMessage::Share(relay_parent, valid_signed),
			})
			.await;

		send_peer_view_change(&mut overseer, peer_a.clone(), view![relay_parent]).await;

		assert_matches!(
			overseer.recv().await,
			AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessages(messages)) => {
				assert_eq!(messages.len(), 2);

				assert_eq!(messages[0].0, vec![peer_a]);
				assert_eq!(messages[1].0, vec![peer_a]);

				assert_matches!(
					&messages[0].1,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::StatementDistribution(
						protocol_v3::StatementDistributionMessage::Statement(
							r,
							s,
						)
					)) if r == &relay_parent
						&& s.unchecked_payload() == &CompactStatement::Seconded(candidate_hash) => {}
				);

				assert_matches!(
					&messages[1].1,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::StatementDistribution(
						protocol_v3::StatementDistributionMessage::Statement(
							r,
							s,
						)
					)) if r == &relay_parent
						&& s.unchecked_payload() == &CompactStatement::Valid(candidate_hash) => {}
				);
			}
		);

		overseer
	});
}

#[test]
fn cluster_accounts_for_implicit_view() {
	let config =
		TestConfig { validator_count: 20, group_size: 3, local_validator: LocalRole::Validator };

	let relay_parent = Hash::repeat_byte(1);
	let peer_a = PeerId::random();
	let peer_b = PeerId::random();

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

		// peer A is in group, has scheduling parent in view.
		// peer B is in group, has no scheduling parent in view.
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

			send_peer_view_change(&mut overseer, peer_a.clone(), view![relay_parent]).await;
		}

		activate_leaf(&mut overseer, &test_leaf, &state, true, vec![]).await;

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

		// sharing a `Seconded` message confirms a candidate, which leads to new
		// fragment chain updates.
		answer_expected_hypothetical_membership_request(&mut overseer, vec![]).await;

		// activate new leaf, which has relay-parent in implicit view.
		let next_relay_parent = Hash::repeat_byte(2);
		let mut next_test_leaf = state.make_dummy_leaf(next_relay_parent);
		next_test_leaf.parent_hash = relay_parent;
		next_test_leaf.number = 2;

		activate_leaf(&mut overseer, &next_test_leaf, &state, false, vec![]).await;

		send_peer_view_change(&mut overseer, peer_a.clone(), view![next_relay_parent]).await;
		send_peer_view_change(&mut overseer, peer_b.clone(), view![next_relay_parent]).await;

		// peer B never had the relay parent in its view, so this tests that
		// the implicit view is working correctly for B.
		//
		// the fact that the statement isn't sent again to A also indicates that it's working.
		assert_matches!(
			overseer.recv().await,
			AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessages(messages)) => {
				assert_eq!(messages.len(), 1);
				assert_matches!(
					&messages[0],
					(
						peers,
						ValidationProtocols::V3(protocol_v3::ValidationProtocol::StatementDistribution(
							protocol_v3::StatementDistributionMessage::Statement(
								r,
								s,
							)
						))
					) => {
						assert_eq!(peers, &vec![peer_b.clone()]);
						assert_eq!(r, &relay_parent);
						assert_eq!(s.unchecked_payload(), &CompactStatement::Seconded(candidate_hash));
						assert_eq!(s.unchecked_validator_index(), local_validator.validator_index);
					}
				)
			}
		);

		overseer
	});
}

#[test]
fn cluster_messages_imported_after_confirmed_candidate_importable_check() {
	let group_size = 3;
	let config =
		TestConfig { validator_count: 20, group_size, local_validator: LocalRole::Validator };

	let relay_parent = Hash::repeat_byte(1);
	let peer_a = PeerId::random();

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

		// peer A is in group, has scheduling parent in view.
		let other_group_validators = state.group_validators(local_group_index, true);
		let v_a = other_group_validators[0];
		{
			connect_peer(
				&mut overseer,
				peer_a.clone(),
				Some(vec![state.discovery_id(v_a)].into_iter().collect()),
			)
			.await;

			send_peer_view_change(&mut overseer, peer_a.clone(), view![relay_parent]).await;
		}

		activate_leaf(&mut overseer, &test_leaf, &state, true, vec![]).await;

		// Peer sends `Seconded` statement.
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

		// Send a request to peer and mock its response.
		{
			handle_sent_request(
				&mut overseer,
				peer_a,
				candidate_hash,
				StatementFilter::blank(group_size),
				candidate.clone(),
				pvd.clone(),
				vec![],
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_a && r == BENEFIT_VALID_RESPONSE.into()
			);
		}

		answer_expected_hypothetical_membership_request(
			&mut overseer,
			vec![(
				HypotheticalCandidate::Complete {
					candidate_hash,
					receipt: Arc::new(candidate.clone()),
					persisted_validation_data: pvd.clone(),
				},
				vec![relay_parent],
			)],
		)
		.await;

		assert_matches!(
			overseer.recv().await,
			AllMessages::CandidateBacking(CandidateBackingMessage::Statement {
			scheduling_parent: r,
			statement: s,
		}) if r == relay_parent => {
				assert_matches!(
					s.payload(),
					FullStatementWithPVD::Seconded(c, p)
						 if c == &candidate && *p == pvd => {}
				);
				assert_eq!(s.validator_index(), v_a);
			}
		);

		overseer
	});
}

#[test]
fn cluster_messages_imported_after_new_leaf_importable_check() {
	let group_size = 3;
	let config =
		TestConfig { validator_count: 20, group_size, local_validator: LocalRole::Validator };

	let relay_parent = Hash::repeat_byte(1);
	let peer_a = PeerId::random();

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

		// peer A is in group, has scheduling parent in view.
		let other_group_validators = state.group_validators(local_group_index, true);
		let v_a = other_group_validators[0];
		{
			connect_peer(
				&mut overseer,
				peer_a.clone(),
				Some(vec![state.discovery_id(v_a)].into_iter().collect()),
			)
			.await;

			send_peer_view_change(&mut overseer, peer_a.clone(), view![relay_parent]).await;
		}

		activate_leaf(&mut overseer, &test_leaf, &state, true, vec![]).await;

		// Peer sends `Seconded` statement.
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

		// Send a request to peer and mock its response.
		{
			handle_sent_request(
				&mut overseer,
				peer_a,
				candidate_hash,
				StatementFilter::blank(group_size),
				candidate.clone(),
				pvd.clone(),
				vec![],
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_a && r == BENEFIT_VALID_RESPONSE.into() => { }
			);
		}

		answer_expected_hypothetical_membership_request(&mut overseer, vec![]).await;

		let next_relay_parent = Hash::repeat_byte(2);
		let mut next_test_leaf = state.make_dummy_leaf(next_relay_parent);
		next_test_leaf.parent_hash = relay_parent;
		next_test_leaf.number = 2;

		activate_leaf(
			&mut overseer,
			&next_test_leaf,
			&state,
			false,
			vec![(
				HypotheticalCandidate::Complete {
					candidate_hash,
					receipt: Arc::new(candidate.clone()),
					persisted_validation_data: pvd.clone(),
				},
				vec![relay_parent],
			)],
		)
		.await;

		assert_matches!(
			overseer.recv().await,
			AllMessages::CandidateBacking(CandidateBackingMessage::Statement {
			scheduling_parent: r,
			statement: s,
		}) if r == relay_parent => {
				assert_matches!(
					s.payload(),
					FullStatementWithPVD::Seconded(c, p)
						 if c == &candidate && *p == pvd
				);
				assert_eq!(s.validator_index(), v_a);
			}
		);

		overseer
	});
}

#[test]
fn ensure_seconding_limit_is_respected() {
	// use a scheduling_lookahead of two to restrict the per-core seconding limit to 2.
	let scheduling_lookahead = 2;
	let config =
		TestConfig { validator_count: 20, group_size: 4, local_validator: LocalRole::Validator };
	let relay_parent = Hash::repeat_byte(1);
	let peer_a = PeerId::random();

	test_harness(config, |state, mut overseer| async move {
		let local_validator = state.local.clone().unwrap();
		let local_group_index = local_validator.group_index.unwrap();
		let local_para = ParaId::from(local_group_index.0);

		let test_leaf =
			state.make_dummy_leaf_with_scheduling_lookahead(relay_parent, scheduling_lookahead);

		let (candidate_1, pvd_1) = make_candidate(
			relay_parent,
			1,
			local_para,
			test_leaf.para_data(local_para).head_data.clone(),
			vec![4, 5, 6].into(),
			Hash::repeat_byte(42).into(),
		);
		let (candidate_2, pvd_2) = make_candidate(
			relay_parent,
			1,
			local_para,
			test_leaf.para_data(local_para).head_data.clone(),
			vec![7, 8, 9].into(),
			Hash::repeat_byte(43).into(),
		);
		let (candidate_3, _pvd_3) = make_candidate(
			relay_parent,
			1,
			local_para,
			test_leaf.para_data(local_para).head_data.clone(),
			vec![10, 11, 12].into(),
			Hash::repeat_byte(44).into(),
		);
		let candidate_hash_1 = candidate_1.hash();
		let candidate_hash_2 = candidate_2.hash();
		let candidate_hash_3 = candidate_3.hash();

		let other_group_validators = state.group_validators(local_group_index, true);
		let v_a = other_group_validators[0];

		// peers A,B,C are in group, have relay parent in view.
		{
			connect_peer(
				&mut overseer,
				peer_a.clone(),
				Some(vec![state.discovery_id(v_a)].into_iter().collect()),
			)
			.await;

			send_peer_view_change(&mut overseer, peer_a.clone(), view![relay_parent]).await;
		}

		activate_leaf(&mut overseer, &test_leaf, &state, true, vec![]).await;

		// Confirm the candidates locally so that we don't send out requests.

		// Candidate 1.
		{
			let validator_index = state.local.as_ref().unwrap().validator_index;
			let statement = state
				.sign_full_statement(
					validator_index,
					Statement::Seconded(candidate_1),
					&SigningContext { parent_hash: relay_parent, session_index: 1 },
					pvd_1,
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

		// Candidate 2.
		{
			let validator_index = state.local.as_ref().unwrap().validator_index;
			let statement = state
				.sign_full_statement(
					validator_index,
					Statement::Seconded(candidate_2),
					&SigningContext { parent_hash: relay_parent, session_index: 1 },
					pvd_2,
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

		// Send first statement from peer A.
		{
			let statement = state
				.sign_statement(
					v_a,
					CompactStatement::Seconded(candidate_hash_1),
					&SigningContext { parent_hash: relay_parent, session_index: 1 },
				)
				.as_unchecked()
				.clone();

			send_peer_message(
				&mut overseer,
				peer_a.clone(),
				protocol_v3::StatementDistributionMessage::Statement(relay_parent, statement),
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_a && r == BENEFIT_VALID_STATEMENT_FIRST.into() => { }
			);
		}

		// Send second statement from peer A.
		{
			let statement = state
				.sign_statement(
					v_a,
					CompactStatement::Seconded(candidate_hash_2),
					&SigningContext { parent_hash: relay_parent, session_index: 1 },
				)
				.as_unchecked()
				.clone();

			send_peer_message(
				&mut overseer,
				peer_a.clone(),
				protocol_v3::StatementDistributionMessage::Statement(relay_parent, statement),
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_a && r == BENEFIT_VALID_STATEMENT_FIRST.into() => { }
			);
		}

		// Send third statement from peer A.
		{
			let statement = state
				.sign_statement(
					v_a,
					CompactStatement::Seconded(candidate_hash_3),
					&SigningContext { parent_hash: relay_parent, session_index: 1 },
				)
				.as_unchecked()
				.clone();

			send_peer_message(
				&mut overseer,
				peer_a.clone(),
				protocol_v3::StatementDistributionMessage::Statement(relay_parent, statement),
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(p, r)))
					if p == peer_a && r == COST_EXCESSIVE_SECONDED.into() => { }
			);
		}

		overseer
	});
}

#[test]
fn delayed_reputation_changes() {
	let config =
		TestConfig { validator_count: 20, group_size: 3, local_validator: LocalRole::Validator };

	let keystore = test_helpers::mock::make_ferdie_keystore();
	let req_protocol_names = ReqProtocolNames::new(&GENESIS_HASH, None);
	let (candidate_req_receiver, req_cfg) = IncomingRequest::get_config_receiver::<
		Block,
		sc_network::NetworkWorker<Block, Hash>,
	>(&req_protocol_names);
	let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(0);

	let state = TestState::from_config(config, req_cfg.inbound_queue.unwrap(), &mut rng);

	// We can't use the test harness as we need to spawn our own subsystem with custom config.
	let (context, mut virtual_overseer) =
		polkadot_node_subsystem_test_helpers::make_subsystem_context(
			sp_core::testing::TaskExecutor::new(),
		);
	let subsystem = async move {
		let subsystem = crate::StatementDistributionSubsystem {
			keystore,
			req_receiver: Some(candidate_req_receiver),
			metrics: Default::default(),
			reputation: ReputationAggregator::new(|_| false),
		};

		if let Err(e) = subsystem.run_inner(context, Duration::from_millis(100)).await {
			panic!("Fatal error: {:?}", e);
		}
	};

	let test_fut = async move {
		let relay_parent = Hash::repeat_byte(1);
		let peer_a = PeerId::random();

		let local_validator = state.local.clone().unwrap();
		let local_group_index = local_validator.group_index.unwrap();
		let candidate_hash = CandidateHash(Hash::repeat_byte(42));

		let test_leaf = state.make_dummy_leaf(relay_parent);

		// peer A is in group, has scheduling parent in view.
		let other_group_validators = state.group_validators(local_group_index, true);
		let v_a = other_group_validators[0];
		connect_peer(
			&mut virtual_overseer,
			peer_a.clone(),
			Some(vec![state.discovery_id(v_a)].into_iter().collect()),
		)
		.await;

		send_peer_view_change(&mut virtual_overseer, peer_a.clone(), view![relay_parent]).await;
		activate_leaf(&mut virtual_overseer, &test_leaf, &state, true, vec![]).await;

		let signed_valid = state.sign_statement(
			v_a,
			CompactStatement::Valid(candidate_hash),
			&SigningContext { parent_hash: relay_parent, session_index: 1 },
		);

		send_peer_message(
			&mut virtual_overseer,
			peer_a.clone(),
			protocol_v3::StatementDistributionMessage::Statement(
				relay_parent,
				signed_valid.as_unchecked().clone(),
			),
		)
		.await;

		assert_matches!(virtual_overseer.rx.next().timeout(Duration::from_millis(50)).await, None);
		// Wait enough to fire reputation delay
		futures_timer::Delay::new(Duration::from_millis(60)).await;

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Batch(reps))) => {
				let mut expected = HashMap::new();
				expected.insert(peer_a, COST_UNEXPECTED_STATEMENT_CLUSTER_REJECTED.cost_or_benefit());
				assert_eq!(expected, reps);
			}
		);

		virtual_overseer
	};

	futures::pin_mut!(test_fut);
	futures::pin_mut!(subsystem);
	futures::executor::block_on(future::join(
		async move {
			let mut virtual_overseer = test_fut.await;
			// Ensure we have handled all responses.
			if let Ok(Some(msg)) = virtual_overseer.rx.try_next() {
				panic!("Did not handle all responses: {:?}", msg);
			}
			// Conclude.
			virtual_overseer.send(FromOrchestra::Signal(OverseerSignal::Conclude)).await;
		},
		subsystem,
	));
}

// V1-backed statement distributed to a V3-capable peer is accepted.
// The local validator shares a Seconded statement for a V1 candidate
// descriptor; the cluster peer (connected via V3 protocol) receives it.
#[test]
fn v1_backed_statement_distributed_to_v3_capable_peer() {
	let config =
		TestConfig { validator_count: 20, group_size: 3, local_validator: LocalRole::Validator };

	let relay_parent = Hash::repeat_byte(1);
	let peer_a = PeerId::random();

	test_harness(config, |mut state, mut overseer| async move {
		let local_validator = state.local.clone().unwrap();
		let local_group_index = local_validator.group_index.unwrap();
		let local_para = ParaId::from(local_group_index.0);

		// Enable V3 feature — both local and peer are V3-capable.
		state
			.node_features
			.resize(node_features::FeatureIndex::CandidateReceiptV3 as usize + 1, false);
		state
			.node_features
			.set(node_features::FeatureIndex::CandidateReceiptV3 as u8 as usize, true);

		let test_leaf = state.make_dummy_leaf(relay_parent);

		// Build a V1 candidate descriptor directly with non-zero collator/signature.
		// When converted to V2 layout, collator[8..24] maps to `reserved1[0..16]`
		// which triggers V1 detection even with V3 enabled.
		let (candidate, pvd) = make_candidate(
			relay_parent,
			1,
			local_para,
			test_leaf.para_data(local_para).head_data.clone(),
			vec![4, 5, 6].into(),
			Hash::repeat_byte(42).into(),
		);

		// Convert to V1 descriptor with non-zero collator bytes.
		let mut v1_descriptor: CandidateDescriptor = candidate.descriptor.into();
		v1_descriptor.collator =
			CollatorId::from_slice(&(0u8..32).collect::<Vec<_>>()).expect("32 bytes; qed");
		v1_descriptor.signature =
			CollatorSignature::from_slice(&(0u8..64).collect::<Vec<_>>()).expect("64 bytes; qed");
		let candidate = CommittedCandidateReceipt {
			descriptor: v1_descriptor.into(),
			commitments: candidate.commitments,
		};

		assert_eq!(candidate.descriptor.version(), CandidateDescriptorVersion::V1);

		let candidate_hash = candidate.hash();

		// peer A is in group, has scheduling parent in view (V3-capable peer).
		{
			let other_group_validators = state.group_validators(local_group_index, true);

			connect_peer(
				&mut overseer,
				peer_a.clone(),
				Some(vec![state.discovery_id(other_group_validators[0])].into_iter().collect()),
			)
			.await;

			send_peer_view_change(&mut overseer, peer_a.clone(), view![relay_parent]).await;
		}

		activate_leaf(&mut overseer, &test_leaf, &state, true, vec![]).await;

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

		// Assert V1-backed statement is sent to the V3-capable cluster peer.
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

		// Sharing a `Seconded` message confirms a candidate, which leads to new
		// fragment chain updates.
		answer_expected_hypothetical_membership_request(&mut overseer, vec![]).await;

		overseer
	});
}
