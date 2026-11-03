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

use async_trait::async_trait;
use futures::{executor, pending, pin_mut, poll, select, stream, FutureExt};
use std::{collections::HashMap, sync::atomic, task::Poll};

use polkadot_node_network_protocol::{
	peer_set::ValidationVersion, ObservedRole, PeerId, UnifiedReputationChange,
};
use polkadot_node_primitives::{
	BlockData, CollationGenerationConfig, CollationResult, DisputeMessage, InvalidDisputeVote, PoV,
	UncheckedDisputeMessage, ValidDisputeVote,
};
use polkadot_node_subsystem_test_helpers::mock::{dummy_unpin_handle, new_leaf};
use polkadot_node_subsystem_types::messages::{
	NetworkBridgeEvent, PvfExecKind, ReportPeerMessage, RuntimeApiRequest,
};
use polkadot_primitives::{
	CandidateHash, CandidateReceiptV2, CollatorPair, Id as ParaId, InvalidDisputeStatementKind,
	PersistedValidationData, SessionIndex, ValidDisputeStatementKind, ValidatorIndex,
};
use polkadot_primitives_test_helpers::{
	dummy_candidate_descriptor_v2, dummy_candidate_receipt_v2, dummy_hash, dummy_validation_code,
};

use crate::{
	self as overseer,
	dummy::{dummy_overseer_builder, one_for_all_overseer_builder},
	gen::Delay,
	HeadSupportsParachains,
};
use metered;

use assert_matches::assert_matches;
use sp_core::crypto::Pair as _;

use super::*;

type SpawnedSubsystem = crate::gen::SpawnedSubsystem<SubsystemError>;

struct TestSubsystem1(metered::MeteredSender<usize>);

impl<C> overseer::Subsystem<C, SubsystemError> for TestSubsystem1
where
	C: overseer::SubsystemContext<Message = CandidateValidationMessage, Signal = OverseerSignal>,
{
	fn start(self, mut ctx: C) -> SpawnedSubsystem {
		let mut sender = self.0;
		SpawnedSubsystem {
			name: "test-subsystem-1",
			future: Box::pin(async move {
				let mut i = 0;
				loop {
					match ctx.recv().await {
						Ok(FromOrchestra::Communication { .. }) => {
							let _ = sender.send(i).await;
							i += 1;
							continue;
						},
						Ok(FromOrchestra::Signal(OverseerSignal::Conclude)) => return Ok(()),
						Err(_) => return Ok(()),
						_ => (),
					}
				}
			}),
		}
	}
}

struct TestSubsystem2(metered::MeteredSender<usize>);

impl<C> overseer::Subsystem<C, SubsystemError> for TestSubsystem2
where
	C: overseer::SubsystemContext<
		Message = CandidateBackingMessage,
		OutgoingMessages = <CandidateBackingMessage as AssociateOutgoing>::OutgoingMessages,
		Signal = OverseerSignal,
	>,
{
	fn start(self, mut ctx: C) -> SpawnedSubsystem {
		let sender = self.0.clone();
		SpawnedSubsystem {
			name: "test-subsystem-2",
			future: Box::pin(async move {
				let _sender = sender;
				let mut c: usize = 0;
				loop {
					if c < 10 {
						let candidate_receipt = CandidateReceiptV2 {
							descriptor: dummy_candidate_descriptor_v2(dummy_hash()),
							commitments_hash: dummy_hash(),
						};

						let (tx, _) = oneshot::channel();
						ctx.send_message(CandidateValidationMessage::ValidateFromExhaustive {
							validation_data: PersistedValidationData { ..Default::default() },
							validation_code: dummy_validation_code(),
							candidate_receipt,
							pov: PoV { block_data: BlockData(Vec::new()) }.into(),
							scheduling_session_index: 1,
							exec_kind: PvfExecKind::Backing(dummy_hash()),
							response_sender: tx,
						})
						.await;
						c += 1;
						continue;
					}
					match ctx.try_recv().await {
						Ok(Some(FromOrchestra::Signal(OverseerSignal::Conclude))) => break,
						Ok(Some(_)) => continue,
						Err(_) => return Ok(()),
						_ => (),
					}
					pending!();
				}

				Ok(())
			}),
		}
	}
}

struct ReturnOnStart;

impl<C> overseer::Subsystem<C, SubsystemError> for ReturnOnStart
where
	C: overseer::SubsystemContext<Message = CandidateBackingMessage, Signal = OverseerSignal>,
{
	fn start(self, mut _ctx: C) -> SpawnedSubsystem {
		SpawnedSubsystem {
			name: "test-subsystem-4",
			future: Box::pin(async move {
				// Do nothing and exit.
				Ok(())
			}),
		}
	}
}

struct MockSupportsParachains;

#[async_trait]
impl HeadSupportsParachains for MockSupportsParachains {
	async fn head_supports_parachains(&self, _head: &Hash) -> bool {
		true
	}
}

// Checks that a minimal configuration of two jobs can run and exchange messages.
#[test]
fn overseer_works() {
	let spawner = sp_core::testing::TaskExecutor::new();

	executor::block_on(async move {
		let (s1_tx, s1_rx) = metered::channel::<usize>(64);
		let (s2_tx, s2_rx) = metered::channel::<usize>(64);

		let mut s1_rx = s1_rx.fuse();
		let mut s2_rx = s2_rx.fuse();
		let (overseer, handle) = dummy_overseer_builder(spawner, MockSupportsParachains, None)
			.unwrap()
			.replace_candidate_validation(move |_| TestSubsystem1(s1_tx))
			.replace_candidate_backing(move |_| TestSubsystem2(s2_tx))
			.build()
			.unwrap();
		let mut handle = Handle::new(handle);
		let overseer_fut = overseer.run().fuse();

		pin_mut!(overseer_fut);

		let mut s1_results = Vec::new();
		let mut s2_results = Vec::new();

		loop {
			select! {
				_ = overseer_fut => break,
				s1_next = s1_rx.next() => {
					match s1_next {
						Some(msg) => {
							s1_results.push(msg);
							if s1_results.len() == 10 {
								handle.stop().await;
							}
						}
						None => break,
					}
				},
				s2_next = s2_rx.next() => {
					match s2_next {
						Some(_) => s2_results.push(s2_next),
						None => break,
					}
				},
				complete => break,
			}
		}

		assert_eq!(s1_results, (0..10).collect::<Vec<_>>());
	});
}

// Checks activated/deactivated metrics are updated properly.
#[test]
fn overseer_metrics_work() {
	let spawner = sp_core::testing::TaskExecutor::new();

	executor::block_on(async move {
		let first_block_hash = [1; 32].into();
		let second_block_hash = [2; 32].into();
		let unpin_handle = dummy_unpin_handle(dummy_hash());

		let first_block = BlockInfo {
			hash: first_block_hash,
			parent_hash: [0; 32].into(),
			number: 1,
			unpin_handle: unpin_handle.clone(),
		};
		let second_block = BlockInfo {
			hash: second_block_hash,
			parent_hash: first_block_hash,
			number: 2,
			unpin_handle: unpin_handle.clone(),
		};

		let registry = prometheus::Registry::new();
		let (overseer, handle) =
			dummy_overseer_builder(spawner, MockSupportsParachains, Some(&registry))
				.unwrap()
				.build()
				.unwrap();

		let mut handle = Handle::new(handle);
		let overseer_fut = overseer.run_inner().fuse();

		pin_mut!(overseer_fut);

		handle.block_imported(first_block).await;
		handle.block_imported(second_block).await;
		handle
			.send_msg_anon(AllMessages::CandidateValidation(test_candidate_validation_msg()))
			.await;
		handle.stop().await;

		select! {
			res = overseer_fut => {
				assert!(res.is_ok());
				let metrics = extract_metrics(&registry);
				assert_eq!(metrics["activated"], 2);
				assert_eq!(metrics["deactivated"], 1);
				assert_eq!(metrics["relayed"], 1);
			},
			complete => (),
		}
	});
}

fn extract_metrics(registry: &prometheus::Registry) -> HashMap<&'static str, u64> {
	let gather = registry.gather();
	assert!(!gather.is_empty(), "Gathered metrics are not empty. qed");
	let extract = |name: &str| -> u64 {
		gather
			.iter()
			.find(|&mf| dbg!(mf.get_name()) == dbg!(name))
			.expect(&format!("Must contain `{}` metric", name))
			.get_metric()[0]
			.get_counter()
			.get_value() as u64
	};

	let activated = extract("polkadot_parachain_activated_heads_total");
	let deactivated = extract("polkadot_parachain_deactivated_heads_total");
	let relayed = extract("polkadot_parachain_messages_relayed_total");
	let mut result = HashMap::new();
	result.insert("activated", activated);
	result.insert("deactivated", deactivated);
	result.insert("relayed", relayed);
	result
}

// Spawn a subsystem that immediately exits.
//
// Should immediately conclude the overseer itself.
#[test]
fn overseer_ends_on_subsystem_exit() {
	let spawner = sp_core::testing::TaskExecutor::new();

	executor::block_on(async move {
		let (overseer, _handle) = dummy_overseer_builder(spawner, MockSupportsParachains, None)
			.unwrap()
			.replace_candidate_backing(|_| ReturnOnStart)
			.build()
			.unwrap();

		overseer.run_inner().await.unwrap();
	})
}

struct TestSubsystem5(metered::MeteredSender<OverseerSignal>);

impl<C> overseer::Subsystem<C, SubsystemError> for TestSubsystem5
where
	C: overseer::SubsystemContext<Message = CandidateValidationMessage, Signal = OverseerSignal>,
{
	fn start(self, mut ctx: C) -> SpawnedSubsystem {
		let mut sender = self.0.clone();

		SpawnedSubsystem {
			name: "test-subsystem-5",
			future: Box::pin(async move {
				loop {
					match ctx.try_recv().await {
						Ok(Some(FromOrchestra::Signal(OverseerSignal::Conclude))) => break,
						Ok(Some(FromOrchestra::Signal(s))) => {
							sender.send(s).await.unwrap();
							continue;
						},
						Ok(Some(_)) => continue,
						Err(_) => break,
						_ => (),
					}
					pending!();
				}

				Ok(())
			}),
		}
	}
}

struct TestSubsystem6(metered::MeteredSender<OverseerSignal>);

impl<C> Subsystem<C, SubsystemError> for TestSubsystem6
where
	C: overseer::SubsystemContext<Message = CandidateBackingMessage, Signal = OverseerSignal>,
{
	fn start(self, mut ctx: C) -> SpawnedSubsystem {
		let mut sender = self.0.clone();

		SpawnedSubsystem {
			name: "test-subsystem-6",
			future: Box::pin(async move {
				loop {
					match ctx.try_recv().await {
						Ok(Some(FromOrchestra::Signal(OverseerSignal::Conclude))) => break,
						Ok(Some(FromOrchestra::Signal(s))) => {
							sender.send(s).await.unwrap();
							continue;
						},
						Ok(Some(_)) => continue,
						Err(_) => break,
						_ => (),
					}
					pending!();
				}

				Ok(())
			}),
		}
	}
}

// Tests that starting with a defined set of leaves and receiving
// notifications on imported blocks triggers expected `StartWork` and `StopWork` heartbeats.
#[test]
fn overseer_start_stop_works() {
	let spawner = sp_core::testing::TaskExecutor::new();

	executor::block_on(async move {
		let first_block_hash = [1; 32].into();
		let second_block_hash = [2; 32].into();
		let unpin_handle = dummy_unpin_handle(dummy_hash());

		let first_block = BlockInfo {
			hash: first_block_hash,
			parent_hash: [0; 32].into(),
			number: 1,
			unpin_handle: unpin_handle.clone(),
		};
		let second_block = BlockInfo {
			hash: second_block_hash,
			parent_hash: first_block_hash,
			number: 2,
			unpin_handle: unpin_handle.clone(),
		};

		let (tx_5, mut rx_5) = metered::channel(64);
		let (tx_6, mut rx_6) = metered::channel(64);

		let (overseer, handle) = dummy_overseer_builder(spawner, MockSupportsParachains, None)
			.unwrap()
			.replace_candidate_validation(move |_| TestSubsystem5(tx_5))
			.replace_candidate_backing(move |_| TestSubsystem6(tx_6))
			.build()
			.unwrap();
		let mut handle = Handle::new(handle);

		let overseer_fut = overseer.run_inner().fuse();
		pin_mut!(overseer_fut);

		let mut ss5_results = Vec::new();
		let mut ss6_results = Vec::new();

		handle.block_imported(first_block).await;
		handle.block_imported(second_block).await;

		let expected_heartbeats = vec![
			OverseerSignal::ActiveLeaves(ActiveLeavesUpdate {
				activated: Some(new_leaf(first_block_hash, 1)),
				deactivated: Default::default(),
			}),
			OverseerSignal::ActiveLeaves(ActiveLeavesUpdate {
				activated: Some(new_leaf(second_block_hash, 2)),
				deactivated: [first_block_hash].as_ref().into(),
			}),
		];

		loop {
			select! {
				res = overseer_fut => {
					assert!(res.is_ok());
					break;
				},
				res = rx_5.next() => {
					if let Some(res) = res {
						ss5_results.push(res);
					}
				}
				res = rx_6.next() => {
					if let Some(res) = res {
						ss6_results.push(res);
					}
				}
				complete => break,
			}

			if ss5_results.len() == expected_heartbeats.len() &&
				ss6_results.len() == expected_heartbeats.len()
			{
				handle.stop().await;
			}
		}

		assert_eq!(ss5_results, expected_heartbeats);
		assert_eq!(ss6_results, expected_heartbeats);
	});
}

// Tests that starting with a defined set of leaves and receiving
// notifications on imported blocks triggers expected `StartWork` and `StopWork` heartbeats.
#[test]
fn overseer_finalize_works() {
	let spawner = sp_core::testing::TaskExecutor::new();

	executor::block_on(async move {
		let first_block_hash = [1; 32].into();
		let second_block_hash = [2; 32].into();
		let third_block_hash = [3; 32].into();
		let unpin_handle = dummy_unpin_handle(dummy_hash());

		let first_block = BlockInfo {
			hash: first_block_hash,
			parent_hash: [0; 32].into(),
			number: 1,
			unpin_handle: unpin_handle.clone(),
		};
		let second_block = BlockInfo {
			hash: second_block_hash,
			parent_hash: [42; 32].into(),
			number: 2,
			unpin_handle: unpin_handle.clone(),
		};
		let third_block = BlockInfo {
			hash: third_block_hash,
			parent_hash: second_block_hash,
			number: 3,
			unpin_handle: unpin_handle.clone(),
		};

		let (tx_5, mut rx_5) = metered::channel(64);
		let (tx_6, mut rx_6) = metered::channel(64);

		// start with two forks of different height.

		let (overseer, handle) = dummy_overseer_builder(spawner, MockSupportsParachains, None)
			.unwrap()
			.replace_candidate_validation(move |_| TestSubsystem5(tx_5))
			.replace_candidate_backing(move |_| TestSubsystem6(tx_6))
			.build()
			.unwrap();
		let mut handle = Handle::new(handle);

		let overseer_fut = overseer.run_inner().fuse();
		pin_mut!(overseer_fut);

		let mut ss5_results = Vec::new();
		let mut ss6_results = Vec::new();

		// activate two blocks
		handle.block_imported(first_block).await;
		handle.block_imported(second_block).await;

		// this should stop work on both forks we started with earlier.
		handle.block_finalized(third_block).await;

		let expected_heartbeats = vec![
			OverseerSignal::ActiveLeaves(ActiveLeavesUpdate {
				activated: Some(new_leaf(first_block_hash, 1)),
				deactivated: Default::default(),
			}),
			OverseerSignal::ActiveLeaves(ActiveLeavesUpdate {
				activated: Some(new_leaf(second_block_hash, 2)),
				deactivated: Default::default(),
			}),
			OverseerSignal::ActiveLeaves(ActiveLeavesUpdate {
				deactivated: [first_block_hash, second_block_hash].as_ref().into(),
				..Default::default()
			}),
			OverseerSignal::BlockFinalized(third_block_hash, 3),
		];

		loop {
			select! {
				res = overseer_fut => {
					assert!(res.is_ok());
					break;
				},
				res = rx_5.next() => {
					if let Some(res) = res {
						ss5_results.push(res);
					}
				}
				res = rx_6.next() => {
					if let Some(res) = res {
						ss6_results.push(res);
					}
				}
				complete => break,
			}

			if ss5_results.len() == expected_heartbeats.len() &&
				ss6_results.len() == expected_heartbeats.len()
			{
				handle.stop().await;
			}
		}

		assert_eq!(ss5_results.len(), expected_heartbeats.len());
		assert_eq!(ss6_results.len(), expected_heartbeats.len());

		// Notifications on finality for multiple blocks at once
		// may be received in different orders.
		for expected in expected_heartbeats {
			assert!(ss5_results.contains(&expected));
			assert!(ss6_results.contains(&expected));
		}
	});
}

// Tests that finalization of an active leaf doesn't remove it from
// the leaves set.
#[test]
fn overseer_finalize_leaf_preserves_it() {
	let spawner = sp_core::testing::TaskExecutor::new();

	executor::block_on(async move {
		let first_block_hash = [1; 32].into();
		let second_block_hash = [2; 32].into();
		let unpin_handle = dummy_unpin_handle(dummy_hash());

		let first_block = BlockInfo {
			hash: first_block_hash,
			parent_hash: [0; 32].into(),
			number: 1,
			unpin_handle: unpin_handle.clone(),
		};
		let second_block = BlockInfo {
			hash: second_block_hash,
			parent_hash: [42; 32].into(),
			number: 1,
			unpin_handle: unpin_handle.clone(),
		};

		let (tx_5, mut rx_5) = metered::channel(64);
		let (tx_6, mut rx_6) = metered::channel(64);

		// start with two forks at height 1.

		let (overseer, handle) = dummy_overseer_builder(spawner, MockSupportsParachains, None)
			.unwrap()
			.replace_candidate_validation(move |_| TestSubsystem5(tx_5))
			.replace_candidate_backing(move |_| TestSubsystem6(tx_6))
			.build()
			.unwrap();
		let mut handle = Handle::new(handle);

		let overseer_fut = overseer.run_inner().fuse();
		pin_mut!(overseer_fut);

		let mut ss5_results = Vec::new();
		let mut ss6_results = Vec::new();

		handle.block_imported(first_block.clone()).await;
		handle.block_imported(second_block).await;
		// This should stop work on the second block, but only the
		// second block.
		handle.block_finalized(first_block).await;

		let expected_heartbeats = vec![
			OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(new_leaf(
				first_block_hash,
				1,
			))),
			OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(new_leaf(
				second_block_hash,
				2,
			))),
			OverseerSignal::ActiveLeaves(ActiveLeavesUpdate {
				deactivated: [second_block_hash].as_ref().into(),
				..Default::default()
			}),
			OverseerSignal::BlockFinalized(first_block_hash, 1),
		];

		loop {
			select! {
				res = overseer_fut => {
					assert!(res.is_ok());
					break;
				},
				res = rx_5.next() => {
					if let Some(res) = res {
						ss5_results.push(res);
					}
				}
				res = rx_6.next() => {
					if let Some(res) = res {
						ss6_results.push(res);
					}
				}
				complete => break,
			}

			if ss5_results.len() == expected_heartbeats.len() &&
				ss6_results.len() == expected_heartbeats.len()
			{
				handle.stop().await;
			}
		}

		assert_eq!(ss5_results.len(), expected_heartbeats.len());
		assert_eq!(ss6_results.len(), expected_heartbeats.len());

		// Notifications on finality for multiple blocks at once
		// may be received in different orders.
		for expected in expected_heartbeats {
			assert!(ss5_results.contains(&expected));
			assert!(ss6_results.contains(&expected));
		}
	});
}

#[test]
fn do_not_send_empty_leaves_update_on_block_finalization() {
	let spawner = sp_core::testing::TaskExecutor::new();

	executor::block_on(async move {
		let unpin_handle = dummy_unpin_handle(dummy_hash());

		let imported_block = BlockInfo {
			hash: Hash::random(),
			parent_hash: Hash::random(),
			number: 1,
			unpin_handle: unpin_handle.clone(),
		};

		let finalized_block = BlockInfo {
			hash: Hash::random(),
			parent_hash: Hash::random(),
			number: 1,
			unpin_handle: unpin_handle.clone(),
		};

		let (tx_5, mut rx_5) = metered::channel(64);

		let (overseer, handle) = dummy_overseer_builder(spawner, MockSupportsParachains, None)
			.unwrap()
			.replace_candidate_backing(move |_| TestSubsystem6(tx_5))
			.build()
			.unwrap();

		let mut handle = Handle::new(handle);

		let overseer_fut = overseer.run_inner().fuse();
		pin_mut!(overseer_fut);

		let mut ss5_results = Vec::new();

		handle.block_finalized(finalized_block.clone()).await;
		handle.block_imported(imported_block.clone()).await;

		let expected_heartbeats = vec![
			OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(new_leaf(
				imported_block.hash,
				imported_block.number,
			))),
			OverseerSignal::BlockFinalized(finalized_block.hash, 1),
		];

		loop {
			select! {
				res = overseer_fut => {
					assert!(res.is_ok());
					break;
				},
				res = rx_5.next() => {
					if let Some(res) = dbg!(res) {
						ss5_results.push(res);
					}
				}
			}

			if ss5_results.len() == expected_heartbeats.len() {
				handle.stop().await;
			}
		}

		assert_eq!(ss5_results.len(), expected_heartbeats.len());

		for expected in expected_heartbeats {
			assert!(ss5_results.contains(&expected));
		}
	});
}

#[derive(Clone)]
struct CounterSubsystem {
	stop_signals_received: Arc<atomic::AtomicUsize>,
	signals_received: Arc<atomic::AtomicUsize>,
	msgs_received: Arc<atomic::AtomicUsize>,
}

impl CounterSubsystem {
	fn new(
		stop_signals_received: Arc<atomic::AtomicUsize>,
		signals_received: Arc<atomic::AtomicUsize>,
		msgs_received: Arc<atomic::AtomicUsize>,
	) -> Self {
		Self { stop_signals_received, signals_received, msgs_received }
	}
}

impl<C, M> Subsystem<C, SubsystemError> for CounterSubsystem
where
	C: overseer::SubsystemContext<Message = M, Signal = OverseerSignal>,
	M: Send,
{
	fn start(self, mut ctx: C) -> SpawnedSubsystem {
		SpawnedSubsystem {
			name: "counter-subsystem",
			future: Box::pin(async move {
				loop {
					match ctx.try_recv().await {
						Ok(Some(FromOrchestra::Signal(OverseerSignal::Conclude))) => {
							self.stop_signals_received.fetch_add(1, atomic::Ordering::SeqCst);
							break;
						},
						Ok(Some(FromOrchestra::Signal(_))) => {
							self.signals_received.fetch_add(1, atomic::Ordering::SeqCst);
							continue;
						},
						Ok(Some(FromOrchestra::Communication { .. })) => {
							self.msgs_received.fetch_add(1, atomic::Ordering::SeqCst);
							continue;
						},
						Err(_) => (),
						_ => (),
					}
					pending!();
				}

				Ok(())
			}),
		}
	}
}

fn test_candidate_validation_msg() -> CandidateValidationMessage {
	let (response_sender, _) = oneshot::channel();
	let pov = Arc::new(PoV { block_data: BlockData(Vec::new()) });
	let candidate_receipt = CandidateReceiptV2 {
		descriptor: dummy_candidate_descriptor_v2(dummy_hash()),
		commitments_hash: Hash::zero(),
	};

	CandidateValidationMessage::ValidateFromExhaustive {
		validation_data: PersistedValidationData { ..Default::default() },
		validation_code: dummy_validation_code(),
		candidate_receipt,
		pov,
		scheduling_session_index: 1,
		exec_kind: PvfExecKind::Backing(dummy_hash()),
		response_sender,
	}
}

fn test_candidate_backing_msg() -> CandidateBackingMessage {
	let (sender, _) = oneshot::channel();
	CandidateBackingMessage::GetBackableCandidates { candidates: Default::default(), sender }
}

fn test_chain_api_msg() -> ChainApiMessage {
	let (sender, _) = oneshot::channel();
	ChainApiMessage::FinalizedBlockNumber(sender)
}

fn test_collator_generation_msg() -> CollationGenerationMessage {
	CollationGenerationMessage::Initialize(CollationGenerationConfig {
		key: CollatorPair::generate().0,
		collator: Some(Box::new(|_, _| TestCollator.boxed())),
		para_id: Default::default(),
	})
}
struct TestCollator;

impl Future for TestCollator {
	type Output = Option<CollationResult>;

	fn poll(self: Pin<&mut Self>, _cx: &mut futures::task::Context) -> Poll<Self::Output> {
		panic!("at the Disco")
	}
}

impl Unpin for TestCollator {}

fn test_collator_protocol_msg() -> CollatorProtocolMessage {
	CollatorProtocolMessage::CollateOn(Default::default())
}

fn test_network_bridge_event<M>() -> NetworkBridgeEvent<M> {
	NetworkBridgeEvent::PeerDisconnected(PeerId::random())
}

fn test_statement_distribution_with_priority_msg() -> StatementDistributionMessage {
	StatementDistributionMessage::NetworkBridgeUpdate(test_network_bridge_event())
}

fn test_statement_distribution_msg() -> StatementDistributionMessage {
	StatementDistributionMessage::Backed(Default::default())
}

fn test_availability_recovery_msg() -> AvailabilityRecoveryMessage {
	let (sender, _) = oneshot::channel();
	AvailabilityRecoveryMessage::RecoverAvailableData(
		dummy_candidate_receipt_v2(dummy_hash()),
		Default::default(),
		None,
		None,
		sender,
	)
}

fn test_bitfield_distribution_msg() -> BitfieldDistributionMessage {
	BitfieldDistributionMessage::NetworkBridgeUpdate(test_network_bridge_event())
}

fn test_bitfield_distribution_with_priority_msg() -> BitfieldDistributionMessage {
	BitfieldDistributionMessage::NetworkBridgeUpdate(NetworkBridgeEvent::PeerConnected(
		PeerId::random(),
		ObservedRole::Authority,
		ValidationVersion::V3.into(),
		None,
	))
}

fn test_provisioner_msg() -> ProvisionerMessage {
	let (sender, _) = oneshot::channel();
	ProvisionerMessage::RequestInherentData(Default::default(), sender)
}

fn test_runtime_api_msg() -> RuntimeApiMessage {
	let (sender, _) = oneshot::channel();
	RuntimeApiMessage::Request(Default::default(), RuntimeApiRequest::Validators(sender))
}

fn test_availability_store_msg() -> AvailabilityStoreMessage {
	let (sender, _) = oneshot::channel();
	AvailabilityStoreMessage::QueryAvailableData(CandidateHash(Default::default()), sender)
}

fn test_network_bridge_tx_msg() -> NetworkBridgeTxMessage {
	NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(
		PeerId::random(),
		UnifiedReputationChange::BenefitMinor("").into(),
	))
}

fn test_network_bridge_rx_msg() -> NetworkBridgeRxMessage {
	NetworkBridgeRxMessage::NewGossipTopology {
		session: SessionIndex::from(0_u32),
		local_index: None,
		canonical_shuffling: Vec::new(),
		shuffled_indices: Vec::new(),
	}
}

fn test_approval_distribution_msg() -> ApprovalDistributionMessage {
	ApprovalDistributionMessage::NewBlocks(Default::default())
}

fn test_approval_voting_msg() -> ApprovalVotingMessage {
	let (sender, _) = oneshot::channel();
	ApprovalVotingMessage::ApprovedAncestor(Default::default(), 0, sender)
}

fn test_approval_voting_parallel_with_priority_msg() -> ApprovalVotingParallelMessage {
	let (sender, _) = oneshot::channel();
	ApprovalVotingParallelMessage::ApprovedAncestor(Default::default(), 0, sender)
}

fn test_dispute_coordinator_msg() -> DisputeCoordinatorMessage {
	let (sender, _) = oneshot::channel();
	DisputeCoordinatorMessage::RecentDisputes(sender)
}

fn test_dispute_coordinator_msg_with_priority() -> DisputeCoordinatorMessage {
	let (sender, _) = oneshot::channel();
	DisputeCoordinatorMessage::DetermineUndisputedChain {
		base: Default::default(),
		block_descriptions: Default::default(),
		tx: sender,
	}
}

fn test_dispute_distribution_msg() -> DisputeDistributionMessage {
	let dummy_dispute_message = UncheckedDisputeMessage {
		candidate_receipt: dummy_candidate_receipt_v2(dummy_hash()),
		session_index: 0,
		invalid_vote: InvalidDisputeVote {
			validator_index: ValidatorIndex(0),
			signature: sp_core::crypto::UncheckedFrom::unchecked_from([1u8; 64]),
			kind: InvalidDisputeStatementKind::Explicit,
		},
		valid_vote: ValidDisputeVote {
			validator_index: ValidatorIndex(0),
			signature: sp_core::crypto::UncheckedFrom::unchecked_from([2u8; 64]),
			kind: ValidDisputeStatementKind::Explicit,
		},
	};

	DisputeDistributionMessage::SendDispute(
		// We just need dummy data here:
		unsafe {
			std::mem::transmute::<UncheckedDisputeMessage, DisputeMessage>(dummy_dispute_message)
		},
	)
}

fn test_chain_selection_msg() -> ChainSelectionMessage {
	ChainSelectionMessage::Approved(Default::default())
}

fn test_prospective_parachains_msg() -> ProspectiveParachainsMessage {
	ProspectiveParachainsMessage::CandidateBacked(
		ParaId::from(5),
		CandidateHash(Hash::repeat_byte(0)),
	)
}

// Checks that `stop`, `broadcast_signal` and `broadcast_message` are implemented correctly.
#[test]
fn overseer_all_subsystems_receive_signals_and_messages() {
	const NUM_SUBSYSTEMS: usize = 24;
	// -4 for BitfieldSigning, GossipSupport, AvailabilityDistribution and PvfCheckerSubsystem.
	const NUM_SUBSYSTEMS_MESSAGED: usize = NUM_SUBSYSTEMS - 4;

	let spawner = sp_core::testing::TaskExecutor::new();
	executor::block_on(async move {
		let stop_signals_received = Arc::new(atomic::AtomicUsize::new(0));
		let signals_received = Arc::new(atomic::AtomicUsize::new(0));
		let msgs_received = Arc::new(atomic::AtomicUsize::new(0));

		let subsystem = CounterSubsystem::new(
			stop_signals_received.clone(),
			signals_received.clone(),
			msgs_received.clone(),
		);

		let (overseer, handle) =
			one_for_all_overseer_builder(spawner, MockSupportsParachains, subsystem, None)
				.unwrap()
				.build()
				.unwrap();

		let mut handle = Handle::new(handle);
		let overseer_fut = overseer.run_inner().fuse();

		pin_mut!(overseer_fut);

		// send a signal to each subsystem
		let unpin_handle = dummy_unpin_handle(dummy_hash());
		handle
			.block_imported(BlockInfo {
				hash: Default::default(),
				parent_hash: Default::default(),
				number: Default::default(),
				unpin_handle: unpin_handle.clone(),
			})
			.await;

		// send a msg to each subsystem
		// except for BitfieldSigning and GossipSupport as the messages are not instantiable
		handle
			.send_msg_anon(AllMessages::CandidateValidation(test_candidate_validation_msg()))
			.await;
		handle
			.send_msg_anon(AllMessages::CandidateBacking(test_candidate_backing_msg()))
			.await;
		handle
			.send_msg_anon(AllMessages::CollationGeneration(test_collator_generation_msg()))
			.await;
		handle
			.send_msg_anon(AllMessages::CollatorProtocol(test_collator_protocol_msg()))
			.await;
		handle
			.send_msg_anon(AllMessages::StatementDistribution(test_statement_distribution_msg()))
			.await;
		handle
			.send_msg_anon(AllMessages::AvailabilityRecovery(test_availability_recovery_msg()))
			.await;
		// handle.send_msg_anon(AllMessages::BitfieldSigning(test_bitfield_signing_msg())).await;
		// handle.send_msg_anon(AllMessages::GossipSupport(test_bitfield_signing_msg())).await;
		handle
			.send_msg_anon(AllMessages::BitfieldDistribution(test_bitfield_distribution_msg()))
			.await;
		handle.send_msg_anon(AllMessages::Provisioner(test_provisioner_msg())).await;
		handle.send_msg_anon(AllMessages::RuntimeApi(test_runtime_api_msg())).await;
		handle
			.send_msg_anon(AllMessages::AvailabilityStore(test_availability_store_msg()))
			.await;
		handle
			.send_msg_anon(AllMessages::NetworkBridgeTx(test_network_bridge_tx_msg()))
			.await;
		handle
			.send_msg_anon(AllMessages::NetworkBridgeRx(test_network_bridge_rx_msg()))
			.await;
		handle.send_msg_anon(AllMessages::ChainApi(test_chain_api_msg())).await;
		handle
			.send_msg_anon(AllMessages::ApprovalDistribution(test_approval_distribution_msg()))
			.await;
		handle
			.send_msg_anon(AllMessages::ApprovalVotingParallel(
				test_approval_distribution_msg().into(),
			))
			.await;
		handle
			.send_msg_anon(AllMessages::ApprovalVoting(test_approval_voting_msg()))
			.await;
		handle
			.send_msg_anon(AllMessages::DisputeCoordinator(test_dispute_coordinator_msg()))
			.await;
		handle
			.send_msg_anon(AllMessages::DisputeDistribution(test_dispute_distribution_msg()))
			.await;
		handle
			.send_msg_anon(AllMessages::ChainSelection(test_chain_selection_msg()))
			.await;
		handle
			.send_msg_anon(AllMessages::ProspectiveParachains(test_prospective_parachains_msg()))
			.await;
		// handle.send_msg_anon(AllMessages::PvfChecker(test_pvf_checker_msg())).await;

		// Wait until all subsystems have received. Otherwise the messages might race against
		// the conclude signal.
		loop {
			match (&mut overseer_fut).timeout(Duration::from_millis(100)).await {
				None => {
					let r = msgs_received.load(atomic::Ordering::SeqCst);
					if r < NUM_SUBSYSTEMS_MESSAGED {
						Delay::new(Duration::from_millis(100)).await;
					} else if r > NUM_SUBSYSTEMS_MESSAGED {
						panic!("too many messages received??");
					} else {
						break;
					}
				},
				Some(_) => panic!("exited too early"),
			}
		}

		// send a stop signal to each subsystems
		handle.stop().await;

		let res = overseer_fut.await;
		assert_eq!(stop_signals_received.load(atomic::Ordering::SeqCst), NUM_SUBSYSTEMS);
		assert_eq!(signals_received.load(atomic::Ordering::SeqCst), NUM_SUBSYSTEMS);
		assert_eq!(msgs_received.load(atomic::Ordering::SeqCst), NUM_SUBSYSTEMS_MESSAGED);

		assert!(res.is_ok());
	});
}

#[test]
fn context_holds_onto_message_until_enough_signals_received() {
	const CHANNEL_CAPACITY: usize = 64;
	let (candidate_validation_bounded_tx, _) = metered::channel(CHANNEL_CAPACITY);
	let (candidate_backing_bounded_tx, _) = metered::channel(CHANNEL_CAPACITY);
	let (statement_distribution_bounded_tx, _) = metered::channel(CHANNEL_CAPACITY);
	let (availability_distribution_bounded_tx, _) = metered::channel(CHANNEL_CAPACITY);
	let (availability_recovery_bounded_tx, _) = metered::channel(CHANNEL_CAPACITY);
	let (bitfield_signing_bounded_tx, _) = metered::channel(CHANNEL_CAPACITY);
	let (bitfield_distribution_bounded_tx, _) = metered::channel(CHANNEL_CAPACITY);
	let (provisioner_bounded_tx, _) = metered::channel(CHANNEL_CAPACITY);
	let (runtime_api_bounded_tx, _) = metered::channel(CHANNEL_CAPACITY);
	let (availability_store_bounded_tx, _) = metered::channel(CHANNEL_CAPACITY);
	let (network_bridge_rx_bounded_tx, _) = metered::channel(CHANNEL_CAPACITY);
	let (network_bridge_tx_bounded_tx, _) = metered::channel(CHANNEL_CAPACITY);
	let (chain_api_bounded_tx, _) = metered::channel(CHANNEL_CAPACITY);
	let (collator_protocol_bounded_tx, _) = metered::channel(CHANNEL_CAPACITY);
	let (collation_generation_bounded_tx, _) = metered::channel(CHANNEL_CAPACITY);
	let (approval_distribution_bounded_tx, _) = metered::channel(CHANNEL_CAPACITY);
	let (approval_voting_bounded_tx, _) = metered::channel(CHANNEL_CAPACITY);
	let (gossip_support_bounded_tx, _) = metered::channel(CHANNEL_CAPACITY);
	let (dispute_coordinator_bounded_tx, _) = metered::channel(CHANNEL_CAPACITY);
	let (dispute_distribution_bounded_tx, _) = metered::channel(CHANNEL_CAPACITY);
	let (chain_selection_bounded_tx, _) = metered::channel(CHANNEL_CAPACITY);
	let (pvf_checker_bounded_tx, _) = metered::channel(CHANNEL_CAPACITY);
	let (prospective_parachains_bounded_tx, _) = metered::channel(CHANNEL_CAPACITY);
	let (approval_voting_parallel_tx, _) = metered::channel(CHANNEL_CAPACITY);

	let (candidate_validation_unbounded_tx, _) = metered::unbounded();
	let (candidate_backing_unbounded_tx, _) = metered::unbounded();
	let (statement_distribution_unbounded_tx, _) = metered::unbounded();
	let (availability_distribution_unbounded_tx, _) = metered::unbounded();
	let (availability_recovery_unbounded_tx, _) = metered::unbounded();
	let (bitfield_signing_unbounded_tx, _) = metered::unbounded();
	let (bitfield_distribution_unbounded_tx, _) = metered::unbounded();
	let (provisioner_unbounded_tx, _) = metered::unbounded();
	let (runtime_api_unbounded_tx, _) = metered::unbounded();
	let (availability_store_unbounded_tx, _) = metered::unbounded();
	let (network_bridge_tx_unbounded_tx, _) = metered::unbounded();
	let (network_bridge_rx_unbounded_tx, _) = metered::unbounded();
	let (chain_api_unbounded_tx, _) = metered::unbounded();
	let (collator_protocol_unbounded_tx, _) = metered::unbounded();
	let (collation_generation_unbounded_tx, _) = metered::unbounded();
	let (approval_distribution_unbounded_tx, _) = metered::unbounded();
	let (approval_voting_unbounded_tx, _) = metered::unbounded();
	let (gossip_support_unbounded_tx, _) = metered::unbounded();
	let (dispute_coordinator_unbounded_tx, _) = metered::unbounded();
	let (dispute_distribution_unbounded_tx, _) = metered::unbounded();
	let (chain_selection_unbounded_tx, _) = metered::unbounded();
	let (pvf_checker_unbounded_tx, _) = metered::unbounded();
	let (prospective_parachains_unbounded_tx, _) = metered::unbounded();
	let (approval_voting_parallel_unbounded_tx, _) = metered::unbounded();

	let channels_out = ChannelsOut {
		candidate_validation: candidate_validation_bounded_tx.clone(),
		candidate_backing: candidate_backing_bounded_tx.clone(),
		statement_distribution: statement_distribution_bounded_tx.clone(),
		availability_distribution: availability_distribution_bounded_tx.clone(),
		availability_recovery: availability_recovery_bounded_tx.clone(),
		bitfield_signing: bitfield_signing_bounded_tx.clone(),
		bitfield_distribution: bitfield_distribution_bounded_tx.clone(),
		provisioner: provisioner_bounded_tx.clone(),
		runtime_api: runtime_api_bounded_tx.clone(),
		availability_store: availability_store_bounded_tx.clone(),
		network_bridge_tx: network_bridge_tx_bounded_tx.clone(),
		network_bridge_rx: network_bridge_rx_bounded_tx.clone(),
		chain_api: chain_api_bounded_tx.clone(),
		collator_protocol: collator_protocol_bounded_tx.clone(),
		collation_generation: collation_generation_bounded_tx.clone(),
		approval_distribution: approval_distribution_bounded_tx.clone(),
		approval_voting: approval_voting_bounded_tx.clone(),
		gossip_support: gossip_support_bounded_tx.clone(),
		dispute_coordinator: dispute_coordinator_bounded_tx.clone(),
		dispute_distribution: dispute_distribution_bounded_tx.clone(),
		chain_selection: chain_selection_bounded_tx.clone(),
		pvf_checker: pvf_checker_bounded_tx.clone(),
		prospective_parachains: prospective_parachains_bounded_tx.clone(),
		approval_voting_parallel: approval_voting_parallel_tx.clone(),

		candidate_validation_unbounded: candidate_validation_unbounded_tx.clone(),
		candidate_backing_unbounded: candidate_backing_unbounded_tx.clone(),
		statement_distribution_unbounded: statement_distribution_unbounded_tx.clone(),
		availability_distribution_unbounded: availability_distribution_unbounded_tx.clone(),
		availability_recovery_unbounded: availability_recovery_unbounded_tx.clone(),
		bitfield_signing_unbounded: bitfield_signing_unbounded_tx.clone(),
		bitfield_distribution_unbounded: bitfield_distribution_unbounded_tx.clone(),
		provisioner_unbounded: provisioner_unbounded_tx.clone(),
		runtime_api_unbounded: runtime_api_unbounded_tx.clone(),
		availability_store_unbounded: availability_store_unbounded_tx.clone(),
		network_bridge_tx_unbounded: network_bridge_tx_unbounded_tx.clone(),
		network_bridge_rx_unbounded: network_bridge_rx_unbounded_tx.clone(),
		chain_api_unbounded: chain_api_unbounded_tx.clone(),
		collator_protocol_unbounded: collator_protocol_unbounded_tx.clone(),
		collation_generation_unbounded: collation_generation_unbounded_tx.clone(),
		approval_distribution_unbounded: approval_distribution_unbounded_tx.clone(),
		approval_voting_unbounded: approval_voting_unbounded_tx.clone(),
		gossip_support_unbounded: gossip_support_unbounded_tx.clone(),
		dispute_coordinator_unbounded: dispute_coordinator_unbounded_tx.clone(),
		dispute_distribution_unbounded: dispute_distribution_unbounded_tx.clone(),
		chain_selection_unbounded: chain_selection_unbounded_tx.clone(),
		pvf_checker_unbounded: pvf_checker_unbounded_tx.clone(),
		prospective_parachains_unbounded: prospective_parachains_unbounded_tx.clone(),
		approval_voting_parallel_unbounded: approval_voting_parallel_unbounded_tx.clone(),
	};

	let (mut signal_tx, signal_rx) = metered::channel(CHANNEL_CAPACITY);
	let (mut bounded_tx, bounded_rx) = metered::channel(CHANNEL_CAPACITY);
	let (unbounded_tx, unbounded_rx) = metered::unbounded();
	let (to_overseer_tx, _to_overseer_rx) = metered::unbounded();

	let mut ctx = OverseerSubsystemContext::new(
		signal_rx,
		stream::select_with_strategy(
			bounded_rx,
			unbounded_rx,
			orchestra::select_message_channel_strategy,
		),
		channels_out,
		to_overseer_tx,
		"test",
	);

	assert_eq!(ctx.signals_received.load(), 0);

	let test_fut = async move {
		signal_tx.send(OverseerSignal::Conclude).await.unwrap();
		assert_matches!(ctx.recv().await.unwrap(), FromOrchestra::Signal(OverseerSignal::Conclude));

		assert_eq!(ctx.signals_received.load(), 1);
		bounded_tx
			.send(MessagePacket { signals_received: 2, message: () })
			.await
			.unwrap();
		unbounded_tx
			.unbounded_send(MessagePacket { signals_received: 2, message: () })
			.unwrap();

		match poll!(ctx.recv()) {
			Poll::Pending => {},
			Poll::Ready(_) => panic!("ready too early"),
		};

		assert!(ctx.pending_incoming.is_some());

		signal_tx.send(OverseerSignal::Conclude).await.unwrap();
		assert_matches!(ctx.recv().await.unwrap(), FromOrchestra::Signal(OverseerSignal::Conclude));
		assert_matches!(ctx.recv().await.unwrap(), FromOrchestra::Communication { msg: () });
		assert_matches!(ctx.recv().await.unwrap(), FromOrchestra::Communication { msg: () });
		assert!(ctx.pending_incoming.is_none());
	};

	futures::executor::block_on(test_fut);
}

// A subsystem that simulates a slow subsystem, processing messages at a rate of one per second.
// We will use this to test the prioritization of messages in the subsystems generated by orchestra.
#[derive(Clone)]
struct SlowSubsystem {
	num_normal_msgs_received: Arc<atomic::AtomicUsize>,
	num_prio_msgs_received: Arc<atomic::AtomicUsize>,
}

impl SlowSubsystem {
	fn new(
		msgs_received: Arc<atomic::AtomicUsize>,
		prio_msgs_received: Arc<atomic::AtomicUsize>,
	) -> Self {
		Self { num_normal_msgs_received: msgs_received, num_prio_msgs_received: prio_msgs_received }
	}
}

// Trait to determine if a message is a priority message or not, it is by the SlowSubsystem
// to determine if it should count the message as a priority message or not.
trait IsPrioMessage {
	// Tells if the message is a priority message.
	fn is_prio(&self) -> bool {
		// By default, messages are not priority messages.
		false
	}
}

// Implement the IsPrioMessage trait for all message types.
impl IsPrioMessage for CandidateValidationMessage {}
impl IsPrioMessage for CandidateBackingMessage {}
impl IsPrioMessage for ChainApiMessage {}
impl IsPrioMessage for CollationGenerationMessage {}
impl IsPrioMessage for CollatorProtocolMessage {}
impl IsPrioMessage for StatementDistributionMessage {
	fn is_prio(&self) -> bool {
		matches!(self, StatementDistributionMessage::NetworkBridgeUpdate(_))
	}
}
impl IsPrioMessage for ApprovalDistributionMessage {}
impl IsPrioMessage for ApprovalVotingMessage {}
impl IsPrioMessage for ApprovalVotingParallelMessage {
	fn is_prio(&self) -> bool {
		matches!(self, ApprovalVotingParallelMessage::ApprovedAncestor(_, _, _))
	}
}
impl IsPrioMessage for AvailabilityDistributionMessage {}
impl IsPrioMessage for AvailabilityRecoveryMessage {}
impl IsPrioMessage for AvailabilityStoreMessage {}
impl IsPrioMessage for BitfieldDistributionMessage {
	fn is_prio(&self) -> bool {
		matches!(
			self,
			BitfieldDistributionMessage::NetworkBridgeUpdate(NetworkBridgeEvent::PeerConnected(
				_,
				_,
				_,
				_
			),)
		)
	}
}
impl IsPrioMessage for ChainSelectionMessage {}
impl IsPrioMessage for DisputeCoordinatorMessage {
	fn is_prio(&self) -> bool {
		matches!(self, DisputeCoordinatorMessage::DetermineUndisputedChain { .. })
	}
}
impl IsPrioMessage for DisputeDistributionMessage {}
impl IsPrioMessage for GossipSupportMessage {}
impl IsPrioMessage for NetworkBridgeRxMessage {}
impl IsPrioMessage for NetworkBridgeTxMessage {}
impl IsPrioMessage for ProspectiveParachainsMessage {}
impl IsPrioMessage for ProvisionerMessage {}
impl IsPrioMessage for RuntimeApiMessage {}
impl IsPrioMessage for BitfieldSigningMessage {}
impl IsPrioMessage for PvfCheckerMessage {}

impl<C, M> Subsystem<C, SubsystemError> for SlowSubsystem
where
	C: overseer::SubsystemContext<Message = M, Signal = OverseerSignal>,
	M: Send + IsPrioMessage,
{
	fn start(self, mut ctx: C) -> SpawnedSubsystem {
		SpawnedSubsystem {
			name: "counter-subsystem",
			future: Box::pin(async move {
				loop {
					// Simulate a slow processing subsystem to give time for both priority and
					// normal messages to accumulate.
					Delay::new(Duration::from_secs(1)).await;
					match ctx.try_recv().await {
						Ok(Some(FromOrchestra::Signal(OverseerSignal::Conclude))) => break,
						Ok(Some(FromOrchestra::Signal(_))) => continue,
						Ok(Some(FromOrchestra::Communication { msg })) => {
							if msg.is_prio() {
								self.num_prio_msgs_received.fetch_add(1, atomic::Ordering::SeqCst);
							} else {
								self.num_normal_msgs_received
									.fetch_add(1, atomic::Ordering::SeqCst);
							}
							continue;
						},
						Err(_) => (),
						_ => (),
					}
					pending!();
				}

				Ok(())
			}),
		}
	}
}

#[test]
fn overseer_all_subsystems_can_receive_their_priority_messages() {
	const NUM_NORMAL_MESSAGES: usize = 10;
	const NUM_PRIORITY_MESSAGES: usize = 4;
	overseer_check_subsystem_can_receive_their_priority_messages(
		(0..NUM_NORMAL_MESSAGES)
			.map(|_| AllMessages::DisputeCoordinator(test_dispute_coordinator_msg()))
			.collect(),
		(0..NUM_PRIORITY_MESSAGES)
			.map(|_| AllMessages::DisputeCoordinator(test_dispute_coordinator_msg_with_priority()))
			.collect(),
	);

	overseer_check_subsystem_can_receive_their_priority_messages(
		(0..NUM_NORMAL_MESSAGES)
			.map(|_| AllMessages::ApprovalVotingParallel(test_approval_distribution_msg().into()))
			.collect(),
		(0..NUM_PRIORITY_MESSAGES)
			.map(|_| {
				AllMessages::ApprovalVotingParallel(
					test_approval_voting_parallel_with_priority_msg(),
				)
			})
			.collect(),
	);

	overseer_check_subsystem_can_receive_their_priority_messages(
		(0..NUM_NORMAL_MESSAGES)
			.map(|_| AllMessages::StatementDistribution(test_statement_distribution_msg()))
			.collect(),
		(0..NUM_PRIORITY_MESSAGES)
			.map(|_| {
				AllMessages::StatementDistribution(test_statement_distribution_with_priority_msg())
			})
			.collect(),
	);

	overseer_check_subsystem_can_receive_their_priority_messages(
		(0..NUM_NORMAL_MESSAGES)
			.map(|_| AllMessages::BitfieldDistribution(test_bitfield_distribution_msg()))
			.collect(),
		(0..NUM_PRIORITY_MESSAGES)
			.map(|_| {
				AllMessages::BitfieldDistribution(test_bitfield_distribution_with_priority_msg())
			})
			.collect(),
	);
}

// Test that when subsystem processes messages slow, the priority messages are processed before
// the normal messages. This is important to ensure that the subsytem can handle priority messages.
fn overseer_check_subsystem_can_receive_their_priority_messages(
	normal_msgs: Vec<AllMessages>,
	prio_msgs: Vec<AllMessages>,
) {
	let num_normal_messages = normal_msgs.len();
	let num_prio_messages: usize = prio_msgs.len();
	let spawner = sp_core::testing::TaskExecutor::new();
	executor::block_on(async move {
		let msgs_received = Arc::new(atomic::AtomicUsize::new(0));
		let prio_msgs_received = Arc::new(atomic::AtomicUsize::new(0));

		let subsystem = SlowSubsystem::new(msgs_received.clone(), prio_msgs_received.clone());

		let (overseer, handle) =
			one_for_all_overseer_builder(spawner, MockSupportsParachains, subsystem, None)
				.unwrap()
				.build()
				.unwrap();

		let mut handle = Handle::new(handle);
		let overseer_fut = overseer.run_inner().fuse();

		pin_mut!(overseer_fut);

		// send a signal to each subsystem
		let unpin_handle = dummy_unpin_handle(dummy_hash());
		handle
			.block_imported(BlockInfo {
				hash: Default::default(),
				parent_hash: Default::default(),
				number: Default::default(),
				unpin_handle: unpin_handle.clone(),
			})
			.await;

		// Send normal messages first, they are processed 1 per second by the SlowSubsystem, so they
		// should accumulated in the queue.
		for msg in normal_msgs {
			handle.send_msg_anon(msg).await;
		}

		// Send priority messages.
		for msg in prio_msgs {
			handle.send_msg_with_priority(msg, "test", PriorityLevel::High).await;
		}

		loop {
			match (&mut overseer_fut).timeout(Duration::from_millis(100)).await {
				None => {
					let normal_msgs: usize = msgs_received.load(atomic::Ordering::SeqCst);
					let prio_msgs: usize = prio_msgs_received.load(atomic::Ordering::SeqCst);

					assert!(
						prio_msgs == num_prio_messages || normal_msgs < num_normal_messages,
						"we should not receive all normal messages before the prio message"
					);

					assert!(
						normal_msgs <= num_normal_messages && prio_msgs <= num_prio_messages,
						"too many messages received"
					);

					if normal_msgs < num_normal_messages || prio_msgs < num_prio_messages {
						Delay::new(Duration::from_millis(100)).await;
					} else {
						break;
					}
				},
				Some(_) => panic!("exited too early"),
			}
		}

		// send a stop signal to each subsystems
		handle.stop().await;

		let res = overseer_fut.await;
		assert!(res.is_ok());
	});
}
