// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// Cumulus is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Cumulus is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Cumulus. If not, see <https://www.gnu.org/licenses/>.

use super::*;
use assert_matches::assert_matches;
use codec::{Decode, Encode};
use cumulus_primitives_core::relay_chain::{
	BlockId, BlockNumber, CandidateCommitments, CandidateDescriptorV2, CoreIndex, CoreState,
};
use cumulus_relay_chain_interface::{
	ChildInfo, InboundDownwardMessage, InboundHrmpMessage, OccupiedCoreAssumption, PHash, PHeader,
	PersistedValidationData, RelayChainResult, StorageValue, ValidationCodeHash, ValidatorId,
};
use cumulus_test_client::runtime::{Block, Header};
use futures::{channel::mpsc, SinkExt, Stream};
use polkadot_node_primitives::AvailableData;
use polkadot_node_subsystem::{
	messages::{AvailabilityRecoveryMessage, RuntimeApiRequest},
	RecoveryError, TimeoutExt,
};
use polkadot_primitives::{vstaging::RelayParentInfo, CandidateEvent, NodeFeatures};
use rstest::rstest;
use sc_client_api::{
	BlockImportNotification, ClientInfo, CompactProof, FinalityNotification, FinalityNotifications,
	FinalizeSummary, ImportNotifications, StorageEventStream, StorageKey,
};
use sc_consensus::import_queue::RuntimeOrigin;
use sc_utils::mpsc::{TracingUnboundedReceiver, TracingUnboundedSender};
use sp_api::RuntimeApiInfo;
use sp_blockchain::Info;
use sp_core::H256;
use sp_runtime::{generic::SignedBlock, Justifications};
use sp_version::RuntimeVersion;
use std::{
	borrow::Cow,
	collections::{BTreeMap, VecDeque},
	ops::Range,
	sync::{Arc, Mutex},
};
use tokio::task;

const GENESIS_HASH: PHash = PHash::zero();
const TEST_SESSION_INDEX: SessionIndex = 0;

struct AvailabilityRecoverySubsystemHandle {
	tx: mpsc::Sender<AvailabilityRecoveryMessage>,
}

impl AvailabilityRecoverySubsystemHandle {
	fn new() -> (Self, mpsc::Receiver<AvailabilityRecoveryMessage>) {
		let (tx, rx) = mpsc::channel(10);

		(Self { tx }, rx)
	}
}

#[async_trait::async_trait]
impl RecoveryHandle for AvailabilityRecoverySubsystemHandle {
	async fn send_recovery_msg(
		&mut self,
		message: AvailabilityRecoveryMessage,
		_origin: &'static str,
	) {
		self.tx.send(message).await.expect("Receiver dropped");
	}
}

struct ParachainClientInner<Block: BlockT> {
	import_notifications_rx: Option<TracingUnboundedReceiver<BlockImportNotification<Block>>>,
	finality_notifications_rx: Option<TracingUnboundedReceiver<FinalityNotification<Block>>>,
	usage_infos: Vec<ClientInfo<Block>>,
	block_statuses: Arc<Mutex<HashMap<Block::Hash, BlockStatus>>>,
}

impl<Block: BlockT> ParachainClientInner<Block> {
	fn new(
		usage_infos: Vec<ClientInfo<Block>>,
		block_statuses: Arc<Mutex<HashMap<Block::Hash, BlockStatus>>>,
	) -> (
		Self,
		TracingUnboundedSender<BlockImportNotification<Block>>,
		TracingUnboundedSender<FinalityNotification<Block>>,
	) {
		let (import_notifications_tx, import_notifications_rx) =
			sc_utils::mpsc::tracing_unbounded("import_notif", 10);
		let (finality_notifications_tx, finality_notifications_rx) =
			sc_utils::mpsc::tracing_unbounded("finality_notif", 10);
		(
			Self {
				import_notifications_rx: Some(import_notifications_rx),
				finality_notifications_rx: Some(finality_notifications_rx),
				usage_infos,
				block_statuses,
			},
			import_notifications_tx,
			finality_notifications_tx,
		)
	}
}
struct ParachainClient<Block: BlockT> {
	inner: Arc<Mutex<ParachainClientInner<Block>>>,
}

impl<Block: BlockT> ParachainClient<Block> {
	fn new(
		usage_infos: Vec<ClientInfo<Block>>,
		block_statuses: Arc<Mutex<HashMap<Block::Hash, BlockStatus>>>,
	) -> (
		Self,
		TracingUnboundedSender<BlockImportNotification<Block>>,
		TracingUnboundedSender<FinalityNotification<Block>>,
	) {
		let (inner, import_notifications_tx, finality_notifications_tx) =
			ParachainClientInner::new(usage_infos, block_statuses);
		(
			Self { inner: Arc::new(Mutex::new(inner)) },
			import_notifications_tx,
			finality_notifications_tx,
		)
	}
}

impl<Block: BlockT> BlockchainEvents<Block> for ParachainClient<Block> {
	fn import_notification_stream(&self) -> ImportNotifications<Block> {
		self.inner
			.lock()
			.expect("poisoned lock")
			.import_notifications_rx
			.take()
			.expect("Should only be taken once")
	}

	fn every_import_notification_stream(&self) -> ImportNotifications<Block> {
		unimplemented!()
	}

	fn finality_notification_stream(&self) -> FinalityNotifications<Block> {
		self.inner
			.lock()
			.expect("poisoned lock")
			.finality_notifications_rx
			.take()
			.expect("Should only be taken once")
	}

	fn storage_changes_notification_stream(
		&self,
		_filter_keys: Option<&[StorageKey]>,
		_child_filter_keys: Option<&[(StorageKey, Option<Vec<StorageKey>>)]>,
	) -> sp_blockchain::Result<StorageEventStream<Block::Hash>> {
		unimplemented!()
	}
}

impl<Block: BlockT> BlockBackend<Block> for ParachainClient<Block> {
	fn block_body(
		&self,
		_: Block::Hash,
	) -> sp_blockchain::Result<Option<Vec<<Block as BlockT>::Extrinsic>>> {
		unimplemented!()
	}

	fn block(&self, _: Block::Hash) -> sp_blockchain::Result<Option<SignedBlock<Block>>> {
		unimplemented!()
	}

	fn block_status(&self, hash: Block::Hash) -> sp_blockchain::Result<sp_consensus::BlockStatus> {
		Ok(self
			.inner
			.lock()
			.expect("Poisoned lock")
			.block_statuses
			.lock()
			.expect("Poisoned lock")
			.get(&hash)
			.cloned()
			.unwrap_or(BlockStatus::Unknown))
	}

	fn justifications(&self, _: Block::Hash) -> sp_blockchain::Result<Option<Justifications>> {
		unimplemented!()
	}

	fn block_hash(&self, _: NumberFor<Block>) -> sp_blockchain::Result<Option<Block::Hash>> {
		unimplemented!()
	}

	fn indexed_transaction(&self, _: H256) -> sp_blockchain::Result<Option<Vec<u8>>> {
		unimplemented!()
	}

	fn has_indexed_transaction(&self, _: H256) -> sp_blockchain::Result<bool> {
		unimplemented!()
	}

	fn block_indexed_hashes(&self, _: Block::Hash) -> sp_blockchain::Result<Option<Vec<H256>>> {
		unimplemented!()
	}

	fn block_indexed_body(&self, _: Block::Hash) -> sp_blockchain::Result<Option<Vec<Vec<u8>>>> {
		unimplemented!()
	}

	fn requires_full_sync(&self) -> bool {
		unimplemented!()
	}
}

impl<Block: BlockT> UsageProvider<Block> for ParachainClient<Block> {
	fn usage_info(&self) -> ClientInfo<Block> {
		let infos = &mut self.inner.lock().expect("Poisoned lock").usage_infos;
		assert!(!infos.is_empty());

		if infos.len() == 1 {
			infos.last().unwrap().clone()
		} else {
			infos.remove(0)
		}
	}
}

struct ParachainImportQueue<Block: BlockT> {
	import_requests_tx: TracingUnboundedSender<Vec<IncomingBlock<Block>>>,
}

impl<Block: BlockT> ParachainImportQueue<Block> {
	fn new() -> (Self, TracingUnboundedReceiver<Vec<IncomingBlock<Block>>>) {
		let (import_requests_tx, import_requests_rx) =
			sc_utils::mpsc::tracing_unbounded("test_import_req_forwarding", 10);
		(Self { import_requests_tx }, import_requests_rx)
	}
}

impl<Block: BlockT> ImportQueueService<Block> for ParachainImportQueue<Block> {
	fn import_blocks(&mut self, origin: BlockOrigin, blocks: Vec<IncomingBlock<Block>>) {
		assert_matches!(origin, BlockOrigin::ConsensusBroadcast);
		self.import_requests_tx.unbounded_send(blocks).unwrap();
	}

	fn import_justifications(
		&mut self,
		_: RuntimeOrigin,
		_: Block::Hash,
		_: NumberFor<Block>,
		_: Justifications,
	) {
		unimplemented!()
	}
}

#[derive(Default)]
struct DummySyncOracle {
	is_major_syncing: bool,
}

impl DummySyncOracle {
	fn new(is_major_syncing: bool) -> Self {
		Self { is_major_syncing }
	}
}

impl SyncOracle for DummySyncOracle {
	fn is_major_syncing(&self) -> bool {
		self.is_major_syncing
	}

	fn is_offline(&self) -> bool {
		false
	}
}

#[derive(Clone)]
struct RelaychainInner {
	runtime_version: u32,
	import_notifications: Vec<PHeader>,
	candidates_pending_availability: HashMap<PHash, Vec<CommittedCandidateReceipt>>,
}

#[derive(Clone)]
struct Relaychain {
	inner: Arc<Mutex<RelaychainInner>>,
}

impl Relaychain {
	fn new(relay_chain_blocks: Vec<(PHeader, Vec<CommittedCandidateReceipt>)>) -> Self {
		let (candidates_pending_availability, import_notifications) = relay_chain_blocks
			.into_iter()
			.map(|(header, receipt)| ((header.hash(), receipt), header))
			.unzip();
		Self {
			inner: Arc::new(Mutex::new(RelaychainInner {
				import_notifications,
				candidates_pending_availability,
				// The version that introduced candidates_pending_availability
				runtime_version:
					RuntimeApiRequest::CANDIDATES_PENDING_AVAILABILITY_RUNTIME_REQUIREMENT,
			})),
		}
	}

	fn set_runtime_version(&self, version: u32) {
		self.inner.lock().expect("Poisoned lock").runtime_version = version;
	}
}

#[async_trait::async_trait]
impl RelayChainInterface for Relaychain {
	async fn version(&self, _: PHash) -> RelayChainResult<RuntimeVersion> {
		let version = self.inner.lock().expect("Poisoned lock").runtime_version;

		let apis = sp_version::create_apis_vec!([(
			<dyn polkadot_primitives::runtime_api::ParachainHost<polkadot_primitives::Block>>::ID,
			version
		)])
		.into_owned()
		.to_vec();

		Ok(RuntimeVersion {
			spec_name: Cow::Borrowed("test"),
			impl_name: Cow::Borrowed("test"),
			authoring_version: 1,
			spec_version: 1,
			impl_version: 0,
			apis: Cow::Owned(apis),
			transaction_version: 5,
			system_version: 1,
		})
	}

	async fn validators(&self, _: PHash) -> RelayChainResult<Vec<ValidatorId>> {
		unimplemented!("Not needed for test")
	}

	async fn best_block_hash(&self) -> RelayChainResult<PHash> {
		unimplemented!("Not needed for test")
	}

	async fn finalized_block_hash(&self) -> RelayChainResult<PHash> {
		unimplemented!("Not needed for test")
	}

	async fn retrieve_dmq_contents(
		&self,
		_: ParaId,
		_: PHash,
	) -> RelayChainResult<Vec<InboundDownwardMessage>> {
		unimplemented!("Not needed for test")
	}

	async fn retrieve_all_inbound_hrmp_channel_contents(
		&self,
		_: ParaId,
		_: PHash,
	) -> RelayChainResult<BTreeMap<ParaId, Vec<InboundHrmpMessage>>> {
		unimplemented!("Not needed for test")
	}

	async fn persisted_validation_data(
		&self,
		_: PHash,
		_: ParaId,
		_: OccupiedCoreAssumption,
	) -> RelayChainResult<Option<PersistedValidationData>> {
		unimplemented!("Not needed for test")
	}

	async fn validation_code_hash(
		&self,
		_: PHash,
		_: ParaId,
		_: OccupiedCoreAssumption,
	) -> RelayChainResult<Option<ValidationCodeHash>> {
		unimplemented!("Not needed for test")
	}

	async fn candidate_pending_availability(
		&self,
		hash: PHash,
		_: ParaId,
	) -> RelayChainResult<Option<CommittedCandidateReceipt>> {
		if self.inner.lock().expect("Poisoned lock").runtime_version >=
			RuntimeApiRequest::CANDIDATES_PENDING_AVAILABILITY_RUNTIME_REQUIREMENT
		{
			panic!("Should have used candidates_pending_availability instead");
		}

		Ok(self
			.inner
			.lock()
			.expect("Poisoned lock")
			.candidates_pending_availability
			.remove(&hash)
			.map(|mut c| {
				assert_eq!(c.len(), 1);
				c.pop().unwrap()
			}))
	}

	async fn candidates_pending_availability(
		&self,
		hash: PHash,
		_: ParaId,
	) -> RelayChainResult<Vec<CommittedCandidateReceipt>> {
		if self.inner.lock().expect("Poisoned lock").runtime_version <
			RuntimeApiRequest::CANDIDATES_PENDING_AVAILABILITY_RUNTIME_REQUIREMENT
		{
			panic!("Should have used candidate_pending_availability instead");
		}

		Ok(self
			.inner
			.lock()
			.expect("Poisoned lock")
			.candidates_pending_availability
			.remove(&hash)
			.expect("Not found"))
	}

	async fn session_index_for_child(&self, _: PHash) -> RelayChainResult<SessionIndex> {
		Ok(TEST_SESSION_INDEX)
	}

	async fn import_notification_stream(
		&self,
	) -> RelayChainResult<Pin<Box<dyn Stream<Item = PHeader> + Send>>> {
		Ok(Box::pin(
			futures::stream::iter(std::mem::take(
				&mut self.inner.lock().expect("Poisoned lock").import_notifications,
			))
			.chain(futures::stream::pending()),
		))
	}

	async fn finality_notification_stream(
		&self,
	) -> RelayChainResult<Pin<Box<dyn Stream<Item = PHeader> + Send>>> {
		unimplemented!("Not needed for test")
	}

	async fn is_major_syncing(&self) -> RelayChainResult<bool> {
		unimplemented!("Not needed for test");
	}

	fn overseer_handle(&self) -> RelayChainResult<OverseerHandle> {
		unimplemented!("Not needed for test")
	}

	async fn get_storage_by_key(
		&self,
		_: PHash,
		_: &[u8],
	) -> RelayChainResult<Option<StorageValue>> {
		unimplemented!("Not needed for test")
	}

	async fn prove_read(
		&self,
		_: PHash,
		_: &Vec<Vec<u8>>,
	) -> RelayChainResult<sc_client_api::StorageProof> {
		unimplemented!("Not needed for test")
	}

	async fn prove_child_read(
		&self,
		_: PHash,
		_: &ChildInfo,
		_: &[Vec<u8>],
	) -> RelayChainResult<sc_client_api::StorageProof> {
		unimplemented!("Not needed for test")
	}

	async fn wait_for_block(&self, _: PHash) -> RelayChainResult<()> {
		unimplemented!("Not needed for test");
	}

	async fn new_best_notification_stream(
		&self,
	) -> RelayChainResult<Pin<Box<dyn Stream<Item = PHeader> + Send>>> {
		unimplemented!("Not needed for test");
	}

	async fn header(&self, _: BlockId) -> RelayChainResult<Option<PHeader>> {
		unimplemented!("Not needed for test");
	}

	async fn availability_cores(
		&self,
		_: PHash,
	) -> RelayChainResult<Vec<CoreState<PHash, NumberFor<Block>>>> {
		unimplemented!("Not needed for test");
	}

	async fn claim_queue(
		&self,
		_: PHash,
	) -> RelayChainResult<BTreeMap<CoreIndex, VecDeque<ParaId>>> {
		unimplemented!("Not needed for test");
	}

	async fn call_runtime_api(
		&self,
		_method_name: &'static str,
		_hash: PHash,
		_payload: &[u8],
	) -> RelayChainResult<Vec<u8>> {
		unimplemented!("Not needed for test")
	}

	async fn scheduling_lookahead(&self, _: PHash) -> RelayChainResult<u32> {
		unimplemented!("Not needed for test")
	}

	async fn candidate_events(&self, _: PHash) -> RelayChainResult<Vec<CandidateEvent>> {
		unimplemented!("Not needed for test");
	}

	async fn max_relay_parent_session_age(&self, _at: PHash) -> RelayChainResult<u32> {
		unimplemented!("Not needed for test");
	}

	async fn node_features(&self, _at: PHash) -> RelayChainResult<NodeFeatures> {
		unimplemented!("Not needed for test");
	}

	async fn ancestor_relay_parent_info(
		&self,
		_at: PHash,
		_session_index: SessionIndex,
		_relay_parent: PHash,
	) -> RelayChainResult<Option<RelayParentInfo<PHash, BlockNumber>>> {
		unimplemented!("Not needed for test");
	}
}

fn make_candidate_chain(candidate_number_range: Range<u32>) -> Vec<CommittedCandidateReceipt> {
	let mut latest_parent_hash = GENESIS_HASH;
	let mut candidates = vec![];

	for number in candidate_number_range {
		let head_data = Header {
			number,
			digest: Default::default(),
			extrinsics_root: Default::default(),
			parent_hash: latest_parent_hash,
			state_root: Default::default(),
		};

		latest_parent_hash = head_data.hash();

		candidates.push(CommittedCandidateReceipt {
			descriptor: CandidateDescriptorV2::new(
				ParaId::from(1000),
				PHash::zero(),
				CoreIndex(0),
				0,
				PHash::zero(),
				PHash::zero(),
				PHash::zero(),
				PHash::zero(),
				PHash::zero().into(),
			),
			commitments: CandidateCommitments {
				head_data: head_data.encode().into(),
				upward_messages: vec![].try_into().expect("empty vec fits within bounds"),
				new_validation_code: None,
				horizontal_messages: vec![].try_into().expect("empty vec fits within bounds"),
				processed_downward_messages: 0,
				hrmp_watermark: 0_u32,
			},
		});
	}

	candidates
}

fn dummy_usage_info(finalized_number: u32) -> ClientInfo<Block> {
	ClientInfo {
		chain: Info {
			best_hash: PHash::zero(),
			best_number: 0,
			genesis_hash: PHash::zero(),
			finalized_hash: PHash::zero(),
			// Only this field is being used.
			finalized_number,
			finalized_state: None,
			number_leaves: 0,
			block_gap: None,
		},
		usage: None,
	}
}

fn dummy_pvd() -> PersistedValidationData {
	PersistedValidationData {
		parent_head: vec![].into(),
		relay_parent_number: 1,
		relay_parent_storage_root: PHash::zero(),
		max_pov_size: 100,
	}
}

#[tokio::test]
async fn pending_candidate_height_lower_than_latest_finalized() {
	sp_tracing::init_for_tests();

	for finalized_number in [3, 4, 5] {
		let (recovery_subsystem_tx, mut recovery_subsystem_rx) =
			AvailabilityRecoverySubsystemHandle::new();
		let recovery_delay_range =
			RecoveryDelayRange { min: Duration::from_millis(0), max: Duration::from_millis(10) };
		let (_explicit_recovery_chan_tx, explicit_recovery_chan_rx) = mpsc::channel(10);
		let candidates = make_candidate_chain(1..4);
		let relay_chain_client = Relaychain::new(vec![(
			PHeader {
				parent_hash: PHash::from_low_u64_be(0),
				number: 1,
				state_root: PHash::random(),
				extrinsics_root: PHash::random(),
				digest: Default::default(),
			},
			candidates,
		)]);
		let (parachain_client, _import_notifications_tx, _finality_notifications_tx) =
			ParachainClient::new(vec![dummy_usage_info(finalized_number)], Default::default());
		let (parachain_import_queue, mut import_requests_rx) = ParachainImportQueue::new();

		// If the latest finalized block has a larger height compared to the pending candidate, the
		// new candidate won't be recovered. Candidates have heights is 1, 2 and 3. Latest finalized
		// block is 3, 4 or 5.
		let pov_recovery = PoVRecovery::<Block, _, _>::new(
			Box::new(recovery_subsystem_tx),
			recovery_delay_range,
			Arc::new(parachain_client),
			Box::new(parachain_import_queue),
			relay_chain_client,
			ParaId::new(1000),
			explicit_recovery_chan_rx,
			Arc::new(DummySyncOracle::default()),
		);

		task::spawn(pov_recovery.run());

		// No recovery message received
		assert_matches!(
			recovery_subsystem_rx.next().timeout(Duration::from_millis(100)).await,
			None
		);

		// No import request received
		assert_matches!(import_requests_rx.next().timeout(Duration::from_millis(100)).await, None);
	}
}

#[rstest]
#[case(RuntimeApiRequest::CANDIDATES_PENDING_AVAILABILITY_RUNTIME_REQUIREMENT)]
#[case(10)]
#[tokio::test]
async fn single_pending_candidate_recovery_success(
	#[case] runtime_version: u32,
	#[values(true, false)] latest_block_data: bool,
) {
	sp_tracing::init_for_tests();

	let (recovery_subsystem_tx, mut recovery_subsystem_rx) =
		AvailabilityRecoverySubsystemHandle::new();
	let recovery_delay_range =
		RecoveryDelayRange { min: Duration::from_millis(0), max: Duration::from_millis(10) };
	let (_explicit_recovery_chan_tx, explicit_recovery_chan_rx) = mpsc::channel(10);
	let candidates = make_candidate_chain(1..2);
	let header = Header::decode(&mut &candidates[0].commitments.head_data.0[..]).unwrap();
	let candidate_hash = candidates[0].hash();

	let relay_chain_client = Relaychain::new(vec![(
		PHeader {
			parent_hash: PHash::from_low_u64_be(0),
			number: 1,
			state_root: PHash::random(),
			extrinsics_root: PHash::random(),
			digest: Default::default(),
		},
		candidates,
	)]);
	relay_chain_client.set_runtime_version(runtime_version);

	let mut known_blocks = HashMap::new();
	known_blocks.insert(GENESIS_HASH, BlockStatus::InChainWithState);
	let (parachain_client, _import_notifications_tx, _finality_notifications_tx) =
		ParachainClient::new(vec![dummy_usage_info(0)], Arc::new(Mutex::new(known_blocks)));
	let (parachain_import_queue, mut import_requests_rx) = ParachainImportQueue::new();

	let pov_recovery = PoVRecovery::<Block, _, _>::new(
		Box::new(recovery_subsystem_tx),
		recovery_delay_range,
		Arc::new(parachain_client),
		Box::new(parachain_import_queue),
		relay_chain_client,
		ParaId::new(1000),
		explicit_recovery_chan_rx,
		Arc::new(DummySyncOracle::default()),
	);

	task::spawn(pov_recovery.run());

	assert_matches!(
		recovery_subsystem_rx.next().await,
		Some(AvailabilityRecoveryMessage::RecoverAvailableData(
			receipt,
			session_index,
			None,
			None,
			response_tx
		)) => {
			assert_eq!(receipt.hash(), candidate_hash);
			assert_eq!(session_index, TEST_SESSION_INDEX);
			let block_data =
					ParachainBlockData::<Block>::new(
						vec![Block::new(header.clone(), vec![])],
						CompactProof { encoded_nodes: vec![] },
						None
					);

			response_tx.send(
				Ok(
					AvailableData {
						pov: Arc::new(PoV {
							block_data: if latest_block_data {
								block_data
							} else {
								block_data.as_v0().unwrap()
							}.encode().into()
						}),
						validation_data: dummy_pvd(),
					}
				)
			).unwrap()
		}
	);

	// No more recovery messages received.
	assert_matches!(recovery_subsystem_rx.next().timeout(Duration::from_millis(100)).await, None);

	// Received import request for the recovered candidate
	assert_matches!(import_requests_rx.next().await, Some(incoming_blocks) => {
		assert_eq!(incoming_blocks.len(), 1);
		assert_eq!(incoming_blocks[0].header, Some(header));
	});

	// No import request received
	assert_matches!(import_requests_rx.next().timeout(Duration::from_millis(100)).await, None);
}

#[tokio::test]
async fn single_pending_candidate_recovery_retry_succeeds() {
	sp_tracing::init_for_tests();

	let (recovery_subsystem_tx, mut recovery_subsystem_rx) =
		AvailabilityRecoverySubsystemHandle::new();
	let recovery_delay_range =
		RecoveryDelayRange { min: Duration::from_millis(0), max: Duration::from_millis(10) };
	let (_explicit_recovery_chan_tx, explicit_recovery_chan_rx) = mpsc::channel(10);
	let candidates = make_candidate_chain(1..2);
	let header = Header::decode(&mut &candidates[0].commitments.head_data.0[..]).unwrap();
	let candidate_hash = candidates[0].hash();

	let relay_chain_client = Relaychain::new(vec![(
		PHeader {
			parent_hash: PHash::from_low_u64_be(0),
			number: 1,
			state_root: PHash::random(),
			extrinsics_root: PHash::random(),
			digest: Default::default(),
		},
		candidates,
	)]);
	let mut known_blocks = HashMap::new();
	known_blocks.insert(GENESIS_HASH, BlockStatus::InChainWithState);
	let (parachain_client, _import_notifications_tx, _finality_notifications_tx) =
		ParachainClient::new(vec![dummy_usage_info(0)], Arc::new(Mutex::new(known_blocks)));
	let (parachain_import_queue, mut import_requests_rx) = ParachainImportQueue::new();

	let pov_recovery = PoVRecovery::<Block, _, _>::new(
		Box::new(recovery_subsystem_tx),
		recovery_delay_range,
		Arc::new(parachain_client),
		Box::new(parachain_import_queue),
		relay_chain_client,
		ParaId::new(1000),
		explicit_recovery_chan_rx,
		Arc::new(DummySyncOracle::default()),
	);

	task::spawn(pov_recovery.run());

	// First recovery fails.
	assert_matches!(
		recovery_subsystem_rx.next().await,
		Some(AvailabilityRecoveryMessage::RecoverAvailableData(
			receipt,
			session_index,
			None,
			None,
			response_tx
		)) => {
			assert_eq!(receipt.hash(), candidate_hash);
			assert_eq!(session_index, TEST_SESSION_INDEX);
			response_tx.send(
				Err(RecoveryError::Unavailable)
			).unwrap()
		}
	);
	// Candidate is not imported.
	assert_matches!(import_requests_rx.next().timeout(Duration::from_millis(100)).await, None);

	// Recovery is retried and it succeeds now.
	assert_matches!(
		recovery_subsystem_rx.next().await,
		Some(AvailabilityRecoveryMessage::RecoverAvailableData(
			receipt,
			session_index,
			None,
			None,
			response_tx
		)) => {
			assert_eq!(receipt.hash(), candidate_hash);
			assert_eq!(session_index, TEST_SESSION_INDEX);
			response_tx.send(
				Ok(
					AvailableData {
						pov: Arc::new(PoV {
							block_data: ParachainBlockData::<Block>::new(
								vec![Block::new(header.clone(), Vec::new())],
								CompactProof { encoded_nodes: vec![] },
								None
							).encode().into()
						}),
						validation_data: dummy_pvd(),
					}
				)
			).unwrap()
		}
	);

	// No more recovery messages received.
	assert_matches!(recovery_subsystem_rx.next().timeout(Duration::from_millis(100)).await, None);

	// Received import request for the recovered candidate
	assert_matches!(import_requests_rx.next().await, Some(incoming_blocks) => {
		assert_eq!(incoming_blocks.len(), 1);
		assert_eq!(incoming_blocks[0].header, Some(header));
	});

	// No import request received
	assert_matches!(import_requests_rx.next().timeout(Duration::from_millis(100)).await, None);
}

#[tokio::test]
async fn single_pending_candidate_recovery_retry_fails() {
	sp_tracing::init_for_tests();

	let (recovery_subsystem_tx, mut recovery_subsystem_rx) =
		AvailabilityRecoverySubsystemHandle::new();
	let recovery_delay_range =
		RecoveryDelayRange { min: Duration::from_millis(0), max: Duration::from_millis(10) };
	let (_explicit_recovery_chan_tx, explicit_recovery_chan_rx) = mpsc::channel(10);
	let candidates = make_candidate_chain(1..2);
	let candidate_hash = candidates[0].hash();

	let relay_chain_client = Relaychain::new(vec![(
		PHeader {
			parent_hash: PHash::from_low_u64_be(0),
			number: 1,
			state_root: PHash::random(),
			extrinsics_root: PHash::random(),
			digest: Default::default(),
		},
		candidates,
	)]);
	let mut known_blocks = HashMap::new();
	known_blocks.insert(GENESIS_HASH, BlockStatus::InChainWithState);
	let (parachain_client, _import_notifications_tx, _finality_notifications_tx) =
		ParachainClient::new(vec![dummy_usage_info(0)], Arc::new(Mutex::new(known_blocks)));
	let (parachain_import_queue, mut import_requests_rx) = ParachainImportQueue::new();

	let pov_recovery = PoVRecovery::<Block, _, _>::new(
		Box::new(recovery_subsystem_tx),
		recovery_delay_range,
		Arc::new(parachain_client),
		Box::new(parachain_import_queue),
		relay_chain_client,
		ParaId::new(1000),
		explicit_recovery_chan_rx,
		Arc::new(DummySyncOracle::default()),
	);

	task::spawn(pov_recovery.run());

	// First recovery fails.
	assert_matches!(
		recovery_subsystem_rx.next().await,
		Some(AvailabilityRecoveryMessage::RecoverAvailableData(
			receipt,
			session_index,
			None,
			None,
			response_tx
		)) => {
			assert_eq!(receipt.hash(), candidate_hash);
			assert_eq!(session_index, TEST_SESSION_INDEX);
			response_tx.send(
				Err(RecoveryError::Unavailable)
			).unwrap()
		}
	);
	// Candidate is not imported.
	assert_matches!(import_requests_rx.next().timeout(Duration::from_millis(100)).await, None);

	// Second retry fails.
	assert_matches!(
		recovery_subsystem_rx.next().await,
		Some(AvailabilityRecoveryMessage::RecoverAvailableData(
			receipt,
			session_index,
			None,
			None,
			response_tx
		)) => {
			assert_eq!(receipt.hash(), candidate_hash);
			assert_eq!(session_index, TEST_SESSION_INDEX);
			response_tx.send(
				Err(RecoveryError::Unavailable)
			).unwrap()
		}
	);
	// Candidate is not imported.
	assert_matches!(import_requests_rx.next().timeout(Duration::from_millis(100)).await, None);

	// After the second attempt, give up.
	// No more recovery messages received.
	assert_matches!(recovery_subsystem_rx.next().timeout(Duration::from_millis(100)).await, None);
}

#[tokio::test]
async fn single_pending_candidate_recovery_irrecoverable_error() {
	sp_tracing::init_for_tests();

	let (recovery_subsystem_tx, mut recovery_subsystem_rx) =
		AvailabilityRecoverySubsystemHandle::new();
	let recovery_delay_range =
		RecoveryDelayRange { min: Duration::from_millis(0), max: Duration::from_millis(10) };
	let (_explicit_recovery_chan_tx, explicit_recovery_chan_rx) = mpsc::channel(10);
	let candidates = make_candidate_chain(1..2);
	let candidate_hash = candidates[0].hash();

	let relay_chain_client = Relaychain::new(vec![(
		PHeader {
			parent_hash: PHash::from_low_u64_be(0),
			number: 1,
			state_root: PHash::random(),
			extrinsics_root: PHash::random(),
			digest: Default::default(),
		},
		candidates,
	)]);
	let mut known_blocks = HashMap::new();
	known_blocks.insert(GENESIS_HASH, BlockStatus::InChainWithState);
	let (parachain_client, _import_notifications_tx, _finality_notifications_tx) =
		ParachainClient::new(vec![dummy_usage_info(0)], Arc::new(Mutex::new(known_blocks)));
	let (parachain_import_queue, mut import_requests_rx) = ParachainImportQueue::new();

	let pov_recovery = PoVRecovery::<Block, _, _>::new(
		Box::new(recovery_subsystem_tx),
		recovery_delay_range,
		Arc::new(parachain_client),
		Box::new(parachain_import_queue),
		relay_chain_client,
		ParaId::new(1000),
		explicit_recovery_chan_rx,
		Arc::new(DummySyncOracle::default()),
	);

	task::spawn(pov_recovery.run());

	// Recovery succeeds but the block data is wrong. Will not be retried.
	assert_matches!(
		recovery_subsystem_rx.next().await,
		Some(AvailabilityRecoveryMessage::RecoverAvailableData(
			receipt,
			session_index,
			None,
			None,
			response_tx
		)) => {
			assert_eq!(receipt.hash(), candidate_hash);
			assert_eq!(session_index, TEST_SESSION_INDEX);
			response_tx.send(
				Ok(
					AvailableData {
						pov: Arc::new(PoV {
							// Empty block data. It will fail to decode.
							block_data: vec![].into()
						}),
						validation_data: dummy_pvd(),
					}
				)
			).unwrap()
		}
	);
	// Candidate is not imported.
	assert_matches!(import_requests_rx.next().timeout(Duration::from_millis(100)).await, None);

	// No more recovery messages received.
	assert_matches!(recovery_subsystem_rx.next().timeout(Duration::from_millis(100)).await, None);
}

#[tokio::test]
async fn pending_candidates_recovery_skipped_while_syncing() {
	sp_tracing::init_for_tests();

	let (recovery_subsystem_tx, mut recovery_subsystem_rx) =
		AvailabilityRecoverySubsystemHandle::new();
	let recovery_delay_range =
		RecoveryDelayRange { min: Duration::from_millis(0), max: Duration::from_millis(10) };
	let (_explicit_recovery_chan_tx, explicit_recovery_chan_rx) = mpsc::channel(10);
	let candidates = make_candidate_chain(1..4);

	let relay_chain_client = Relaychain::new(vec![(
		PHeader {
			parent_hash: PHash::from_low_u64_be(0),
			number: 1,
			state_root: PHash::random(),
			extrinsics_root: PHash::random(),
			digest: Default::default(),
		},
		candidates,
	)]);
	let mut known_blocks = HashMap::new();
	known_blocks.insert(GENESIS_HASH, BlockStatus::InChainWithState);
	let (parachain_client, _import_notifications_tx, _finality_notifications_tx) =
		ParachainClient::new(vec![dummy_usage_info(0)], Arc::new(Mutex::new(known_blocks)));
	let (parachain_import_queue, mut import_requests_rx) = ParachainImportQueue::new();

	let pov_recovery = PoVRecovery::<Block, _, _>::new(
		Box::new(recovery_subsystem_tx),
		recovery_delay_range,
		Arc::new(parachain_client),
		Box::new(parachain_import_queue),
		relay_chain_client,
		ParaId::new(1000),
		explicit_recovery_chan_rx,
		Arc::new(DummySyncOracle::new(true)),
	);

	task::spawn(pov_recovery.run());

	// No recovery messages received.
	assert_matches!(recovery_subsystem_rx.next().timeout(Duration::from_millis(100)).await, None);

	// No candidate is imported.
	assert_matches!(import_requests_rx.next().timeout(Duration::from_millis(100)).await, None);
}

#[tokio::test]
async fn candidate_is_imported_while_awaiting_recovery() {
	sp_tracing::init_for_tests();

	let (recovery_subsystem_tx, mut recovery_subsystem_rx) =
		AvailabilityRecoverySubsystemHandle::new();
	let recovery_delay_range =
		RecoveryDelayRange { min: Duration::from_millis(0), max: Duration::from_millis(10) };
	let (_explicit_recovery_chan_tx, explicit_recovery_chan_rx) = mpsc::channel(10);
	let candidates = make_candidate_chain(1..2);
	let header = Header::decode(&mut &candidates[0].commitments.head_data.0[..]).unwrap();
	let candidate_hash = candidates[0].hash();

	let relay_chain_client = Relaychain::new(vec![(
		PHeader {
			parent_hash: PHash::from_low_u64_be(0),
			number: 1,
			state_root: PHash::random(),
			extrinsics_root: PHash::random(),
			digest: Default::default(),
		},
		candidates,
	)]);
	let mut known_blocks = HashMap::new();
	known_blocks.insert(GENESIS_HASH, BlockStatus::InChainWithState);
	let (parachain_client, import_notifications_tx, _finality_notifications_tx) =
		ParachainClient::new(vec![dummy_usage_info(0)], Arc::new(Mutex::new(known_blocks)));
	let (parachain_import_queue, mut import_requests_rx) = ParachainImportQueue::new();

	let pov_recovery = PoVRecovery::<Block, _, _>::new(
		Box::new(recovery_subsystem_tx),
		recovery_delay_range,
		Arc::new(parachain_client),
		Box::new(parachain_import_queue),
		relay_chain_client,
		ParaId::new(1000),
		explicit_recovery_chan_rx,
		Arc::new(DummySyncOracle::default()),
	);

	task::spawn(pov_recovery.run());

	let recovery_response_tx;

	assert_matches!(
		recovery_subsystem_rx.next().await,
		Some(AvailabilityRecoveryMessage::RecoverAvailableData(
			receipt,
			session_index,
			None,
			None,
			response_tx
		)) => {
			assert_eq!(receipt.hash(), candidate_hash);
			assert_eq!(session_index, TEST_SESSION_INDEX);
			recovery_response_tx = response_tx;
		}
	);

	// While candidate is pending recovery, import the candidate from external source.
	let (unpin_sender, _unpin_receiver) = sc_utils::mpsc::tracing_unbounded("test_unpin", 10);
	import_notifications_tx
		.unbounded_send(BlockImportNotification::new(
			header.hash(),
			BlockOrigin::ConsensusBroadcast,
			header.clone(),
			false,
			None,
			unpin_sender,
		))
		.unwrap();

	recovery_response_tx
		.send(Ok(AvailableData {
			pov: Arc::new(PoV {
				block_data: ParachainBlockData::<Block>::new(
					vec![Block::new(header.clone(), vec![])],
					CompactProof { encoded_nodes: vec![] },
					None,
				)
				.encode()
				.into(),
			}),
			validation_data: dummy_pvd(),
		}))
		.unwrap();

	// Received import request for the recovered candidate. This could be optimised to not trigger a
	// reimport.
	assert_matches!(import_requests_rx.next().await, Some(incoming_blocks) => {
		assert_eq!(incoming_blocks.len(), 1);
		assert_eq!(incoming_blocks[0].header, Some(header));
	});

	// No more recovery messages received.
	assert_matches!(recovery_subsystem_rx.next().timeout(Duration::from_millis(100)).await, None);

	// No more import requests received
	assert_matches!(import_requests_rx.next().timeout(Duration::from_millis(100)).await, None);
}

#[tokio::test]
async fn candidate_is_finalized_while_awaiting_recovery() {
	sp_tracing::init_for_tests();

	let (recovery_subsystem_tx, mut recovery_subsystem_rx) =
		AvailabilityRecoverySubsystemHandle::new();
	let recovery_delay_range =
		RecoveryDelayRange { min: Duration::from_millis(0), max: Duration::from_millis(10) };
	let (_explicit_recovery_chan_tx, explicit_recovery_chan_rx) = mpsc::channel(10);
	let candidates = make_candidate_chain(1..2);
	let header = Header::decode(&mut &candidates[0].commitments.head_data.0[..]).unwrap();
	let candidate_hash = candidates[0].hash();

	let relay_chain_client = Relaychain::new(vec![(
		PHeader {
			parent_hash: PHash::from_low_u64_be(0),
			number: 1,
			state_root: PHash::random(),
			extrinsics_root: PHash::random(),
			digest: Default::default(),
		},
		candidates,
	)]);
	let mut known_blocks = HashMap::new();
	known_blocks.insert(GENESIS_HASH, BlockStatus::InChainWithState);
	let (parachain_client, _import_notifications_tx, finality_notifications_tx) =
		ParachainClient::new(vec![dummy_usage_info(0)], Arc::new(Mutex::new(known_blocks)));
	let (parachain_import_queue, mut import_requests_rx) = ParachainImportQueue::new();

	let pov_recovery = PoVRecovery::<Block, _, _>::new(
		Box::new(recovery_subsystem_tx),
		recovery_delay_range,
		Arc::new(parachain_client),
		Box::new(parachain_import_queue),
		relay_chain_client,
		ParaId::new(1000),
		explicit_recovery_chan_rx,
		Arc::new(DummySyncOracle::default()),
	);

	task::spawn(pov_recovery.run());

	let recovery_response_tx;

	assert_matches!(
		recovery_subsystem_rx.next().await,
		Some(AvailabilityRecoveryMessage::RecoverAvailableData(
			receipt,
			session_index,
			None,
			None,
			response_tx
		)) => {
			assert_eq!(receipt.hash(), candidate_hash);
			assert_eq!(session_index, TEST_SESSION_INDEX);
			// save it for later.
			recovery_response_tx = response_tx;
		}
	);

	// While candidate is pending recovery, it gets finalized.
	let (unpin_sender, _unpin_receiver) = sc_utils::mpsc::tracing_unbounded("test_unpin", 10);
	finality_notifications_tx
		.unbounded_send(FinalityNotification::from_summary(
			FinalizeSummary { header: header.clone(), finalized: vec![], stale_blocks: vec![] },
			unpin_sender,
		))
		.unwrap();

	recovery_response_tx
		.send(Ok(AvailableData {
			pov: Arc::new(PoV {
				block_data: ParachainBlockData::<Block>::new(
					vec![Block::new(header.clone(), vec![])],
					CompactProof { encoded_nodes: vec![] },
					None,
				)
				.encode()
				.into(),
			}),
			validation_data: dummy_pvd(),
		}))
		.unwrap();

	// No more recovery messages received.
	assert_matches!(recovery_subsystem_rx.next().timeout(Duration::from_millis(100)).await, None);

	// candidate is imported
	assert_matches!(import_requests_rx.next().await, Some(incoming_blocks) => {
		assert_eq!(incoming_blocks.len(), 1);
		assert_eq!(incoming_blocks[0].header, Some(header));
	});

	// No more import requests received
	assert_matches!(import_requests_rx.next().timeout(Duration::from_millis(100)).await, None);
}

#[tokio::test]
async fn chained_recovery_success() {
	sp_tracing::init_for_tests();

	let (recovery_subsystem_tx, mut recovery_subsystem_rx) =
		AvailabilityRecoverySubsystemHandle::new();
	let recovery_delay_range =
		RecoveryDelayRange { min: Duration::from_millis(0), max: Duration::from_millis(0) };
	let (_explicit_recovery_chan_tx, explicit_recovery_chan_rx) = mpsc::channel(10);
	let candidates = make_candidate_chain(1..4);
	let headers = candidates
		.iter()
		.map(|candidate| Header::decode(&mut &candidate.commitments.head_data.0[..]).unwrap())
		.collect::<Vec<_>>();
	let candidate_hashes = candidates.iter().map(|candidate| candidate.hash()).collect::<Vec<_>>();

	let relay_chain_client = Relaychain::new(vec![(
		PHeader {
			parent_hash: PHash::from_low_u64_be(0),
			number: 1,
			state_root: PHash::random(),
			extrinsics_root: PHash::random(),
			digest: Default::default(),
		},
		// 3 pending candidates
		candidates,
	)]);
	let mut known_blocks = HashMap::new();
	known_blocks.insert(GENESIS_HASH, BlockStatus::InChainWithState);
	let known_blocks = Arc::new(Mutex::new(known_blocks));
	let (parachain_client, import_notifications_tx, _finality_notifications_tx) =
		ParachainClient::new(vec![dummy_usage_info(0)], known_blocks.clone());
	let (parachain_import_queue, mut import_requests_rx) = ParachainImportQueue::new();

	let pov_recovery = PoVRecovery::<Block, _, _>::new(
		Box::new(recovery_subsystem_tx),
		recovery_delay_range,
		Arc::new(parachain_client),
		Box::new(parachain_import_queue),
		relay_chain_client,
		ParaId::new(1000),
		explicit_recovery_chan_rx,
		Arc::new(DummySyncOracle::default()),
	);

	task::spawn(pov_recovery.run());

	// Candidates are recovered in the right order.
	for (candidate_hash, header) in candidate_hashes.into_iter().zip(headers.into_iter()) {
		assert_matches!(
			recovery_subsystem_rx.next().await,
			Some(AvailabilityRecoveryMessage::RecoverAvailableData(
				receipt,
				session_index,
				None,
				None,
				response_tx
			)) => {
				assert_eq!(receipt.hash(), candidate_hash);
				assert_eq!(session_index, TEST_SESSION_INDEX);
				response_tx
					.send(Ok(AvailableData {
						pov: Arc::new(PoV {
							block_data: ParachainBlockData::<Block>::new(
								vec![Block::new(header.clone(), vec![])],
								CompactProof { encoded_nodes: vec![] },
								None
							)
							.encode()
							.into(),
						}),
						validation_data: dummy_pvd(),
						}))
					.unwrap();
			}
		);

		assert_matches!(import_requests_rx.next().await, Some(incoming_blocks) => {
			assert_eq!(incoming_blocks.len(), 1);
			assert_eq!(incoming_blocks[0].header, Some(header.clone()));
		});

		known_blocks
			.lock()
			.expect("Poisoned lock")
			.insert(header.hash(), BlockStatus::InChainWithState);

		let (unpin_sender, _unpin_receiver) = sc_utils::mpsc::tracing_unbounded("test_unpin", 10);
		import_notifications_tx
			.unbounded_send(BlockImportNotification::new(
				header.hash(),
				BlockOrigin::ConsensusBroadcast,
				header,
				false,
				None,
				unpin_sender,
			))
			.unwrap();
	}

	// No more recovery messages received.
	assert_matches!(recovery_subsystem_rx.next().timeout(Duration::from_millis(100)).await, None);

	// No more import requests received
	assert_matches!(import_requests_rx.next().timeout(Duration::from_millis(100)).await, None);
}

#[tokio::test]
async fn chained_recovery_child_succeeds_before_parent() {
	sp_tracing::init_for_tests();

	let (recovery_subsystem_tx, mut recovery_subsystem_rx) =
		AvailabilityRecoverySubsystemHandle::new();
	let recovery_delay_range =
		RecoveryDelayRange { min: Duration::from_millis(0), max: Duration::from_millis(0) };
	let (_explicit_recovery_chan_tx, explicit_recovery_chan_rx) = mpsc::channel(10);
	let candidates = make_candidate_chain(1..3);
	let headers = candidates
		.iter()
		.map(|candidate| Header::decode(&mut &candidate.commitments.head_data.0[..]).unwrap())
		.collect::<Vec<_>>();
	let candidate_hashes = candidates.iter().map(|candidate| candidate.hash()).collect::<Vec<_>>();

	let relay_chain_client = Relaychain::new(vec![(
		PHeader {
			parent_hash: PHash::from_low_u64_be(0),
			number: 1,
			state_root: PHash::random(),
			extrinsics_root: PHash::random(),
			digest: Default::default(),
		},
		// 2 pending candidates
		candidates,
	)]);
	let mut known_blocks = HashMap::new();
	known_blocks.insert(GENESIS_HASH, BlockStatus::InChainWithState);
	let known_blocks = Arc::new(Mutex::new(known_blocks));
	let (parachain_client, _import_notifications_tx, _finality_notifications_tx) =
		ParachainClient::new(vec![dummy_usage_info(0)], known_blocks.clone());
	let (parachain_import_queue, mut import_requests_rx) = ParachainImportQueue::new();

	let pov_recovery = PoVRecovery::<Block, _, _>::new(
		Box::new(recovery_subsystem_tx),
		recovery_delay_range,
		Arc::new(parachain_client),
		Box::new(parachain_import_queue),
		relay_chain_client,
		ParaId::new(1000),
		explicit_recovery_chan_rx,
		Arc::new(DummySyncOracle::default()),
	);

	task::spawn(pov_recovery.run());

	let mut recovery_responses_senders = vec![];

	for candidate_hash in candidate_hashes.iter() {
		assert_matches!(
			recovery_subsystem_rx.next().await,
			Some(AvailabilityRecoveryMessage::RecoverAvailableData(
				receipt,
				session_index,
				None,
				None,
				response_tx
			)) => {
				assert_eq!(receipt.hash(), *candidate_hash);
				assert_eq!(session_index, TEST_SESSION_INDEX);
				recovery_responses_senders.push(response_tx);
			}
		);
	}

	// Send out the responses in reverse order.
	for (recovery_response_sender, header) in
		recovery_responses_senders.into_iter().zip(headers.iter()).rev()
	{
		recovery_response_sender
			.send(Ok(AvailableData {
				pov: Arc::new(PoV {
					block_data: ParachainBlockData::<Block>::new(
						vec![Block::new(header.clone(), vec![])],
						CompactProof { encoded_nodes: vec![] },
						None,
					)
					.encode()
					.into(),
				}),
				validation_data: dummy_pvd(),
			}))
			.unwrap();
	}

	assert_matches!(import_requests_rx.next().await, Some(incoming_blocks) => {
		// The two import requests will be batched.
		assert_eq!(incoming_blocks.len(), 2);
		assert_eq!(incoming_blocks[0].header, Some(headers[0].clone()));
		assert_eq!(incoming_blocks[1].header, Some(headers[1].clone()));
	});

	// No more recovery messages received.
	assert_matches!(recovery_subsystem_rx.next().timeout(Duration::from_millis(100)).await, None);

	// No more import requests received
	assert_matches!(import_requests_rx.next().timeout(Duration::from_millis(100)).await, None);
}

#[tokio::test]
async fn recovery_multiple_blocks_per_candidate() {
	sp_tracing::init_for_tests();

	let (recovery_subsystem_tx, mut recovery_subsystem_rx) =
		AvailabilityRecoverySubsystemHandle::new();
	let recovery_delay_range =
		RecoveryDelayRange { min: Duration::from_millis(0), max: Duration::from_millis(0) };
	let (_explicit_recovery_chan_tx, explicit_recovery_chan_rx) = mpsc::channel(10);
	let candidates = make_candidate_chain(1..4);
	let candidate = candidates.last().unwrap();
	let headers = candidates
		.iter()
		.map(|c| Header::decode(&mut &c.commitments.head_data.0[..]).unwrap())
		.collect::<Vec<_>>();
	let header = headers.last().unwrap();

	let relay_chain_client = Relaychain::new(vec![(
		PHeader {
			parent_hash: PHash::from_low_u64_be(0),
			number: 1,
			state_root: PHash::random(),
			extrinsics_root: PHash::random(),
			digest: Default::default(),
		},
		vec![candidate.clone()],
	)]);
	let mut known_blocks = HashMap::new();
	known_blocks.insert(GENESIS_HASH, BlockStatus::InChainWithState);
	let known_blocks = Arc::new(Mutex::new(known_blocks));
	let (parachain_client, import_notifications_tx, _finality_notifications_tx) =
		ParachainClient::new(vec![dummy_usage_info(0)], known_blocks.clone());
	let (parachain_import_queue, mut import_requests_rx) = ParachainImportQueue::new();

	let pov_recovery = PoVRecovery::<Block, _, _>::new(
		Box::new(recovery_subsystem_tx),
		recovery_delay_range,
		Arc::new(parachain_client),
		Box::new(parachain_import_queue),
		relay_chain_client,
		ParaId::new(1000),
		explicit_recovery_chan_rx,
		Arc::new(DummySyncOracle::default()),
	);

	task::spawn(pov_recovery.run());

	// Candidates are recovered in the right order.
	assert_matches!(
		recovery_subsystem_rx.next().await,
		Some(AvailabilityRecoveryMessage::RecoverAvailableData(
			receipt,
			session_index,
			None,
			None,
			response_tx
		)) => {
			assert_eq!(receipt.hash(), candidate.hash());
			assert_eq!(session_index, TEST_SESSION_INDEX);
			response_tx
				.send(Ok(AvailableData {
					pov: Arc::new(PoV {
						block_data: ParachainBlockData::<Block>::new(
							headers.iter().map(|h| Block::new(h.clone(), vec![])).collect(),
							CompactProof { encoded_nodes: vec![] },
							None
						)
						.encode()
						.into(),
					}),
					validation_data: dummy_pvd(),
					}))
				.unwrap();
		}
	);

	assert_matches!(import_requests_rx.next().await, Some(incoming_blocks) => {
		assert_eq!(incoming_blocks.len(), 3);
		assert_eq!(incoming_blocks.iter().map(|b| b.header.clone().unwrap()).collect::<Vec<_>>(), headers);
	});

	known_blocks
		.lock()
		.expect("Poisoned lock")
		.insert(header.hash(), BlockStatus::InChainWithState);

	let (unpin_sender, _unpin_receiver) = sc_utils::mpsc::tracing_unbounded("test_unpin", 10);
	import_notifications_tx
		.unbounded_send(BlockImportNotification::new(
			header.hash(),
			BlockOrigin::ConsensusBroadcast,
			header.clone(),
			false,
			None,
			unpin_sender,
		))
		.unwrap();

	// No more recovery messages received.
	assert_matches!(recovery_subsystem_rx.next().timeout(Duration::from_millis(100)).await, None);

	// No more import requests received
	assert_matches!(import_requests_rx.next().timeout(Duration::from_millis(100)).await, None);
}
