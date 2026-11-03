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

use self::test_helpers::mock::new_leaf;
use super::*;
use itertools::Itertools;
use overseer::prometheus::{
	prometheus::{IntCounter, IntCounterVec},
	Histogram, HistogramOpts, HistogramVec, Opts,
};
use polkadot_node_primitives::{
	approval::{
		v1::{DelayTranche, VrfPreOutput, VrfProof, VrfSignature, RELAY_VRF_MODULO_CONTEXT},
		v2::{AssignmentCertKindV2, AssignmentCertV2},
	},
	AvailableData, BlockData, PoV,
};
use polkadot_node_subsystem::{
	messages::{
		AllMessages, ApprovalVotingMessage, AssignmentCheckResult, AvailabilityRecoveryMessage,
	},
	ActiveLeavesUpdate, SubsystemContext,
};
use polkadot_node_subsystem_test_helpers as test_helpers;
use polkadot_node_subsystem_util::TimeoutExt;
use polkadot_overseer::SpawnGlue;
use polkadot_primitives::{
	ApprovalVote, ApprovalVotingParams, CandidateCommitments, CandidateEvent, CoreIndex,
	DisputeStatement, GroupIndex, Header, Id as ParaId, IndexedVec, MutateDescriptorV2,
	NodeFeatures, ValidDisputeStatementKind, ValidationCode, ValidatorSignature,
};
use std::{cmp::max, time::Duration};

use assert_matches::assert_matches;
use parking_lot::Mutex;
use sp_keyring::sr25519::Keyring as Sr25519Keyring;
use sp_keystore::Keystore;
use std::{
	pin::Pin,
	sync::{
		atomic::{AtomicBool, Ordering},
		Arc,
	},
};

use super::{
	approval_db::common::StoredBlockRange,
	backend::BackendWriteOp,
	import::tests::{
		garbage_vrf_signature, AllowedSlots, BabeEpoch, BabeEpochConfiguration,
		CompatibleDigestItem, Digest, DigestItem, PreDigest, SecondaryVRFPreDigest,
	},
};

use polkadot_primitives_test_helpers::{
	dummy_candidate_receipt_v2, dummy_candidate_receipt_v2_bad_sig,
};

const SLOT_DURATION_MILLIS: u64 = 5000;

const TIMEOUT: Duration = Duration::from_millis(2000);

const NUM_APPROVAL_RETRIES: u32 = 3;
const RETRY_BACKOFF: Duration = Duration::from_millis(300);

#[derive(Clone)]
struct TestSyncOracle {
	flag: Arc<AtomicBool>,
	done_syncing_sender: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

struct TestSyncOracleHandle {
	done_syncing_receiver: oneshot::Receiver<()>,
	is_major_syncing: Arc<AtomicBool>,
}

impl TestSyncOracleHandle {
	async fn await_mode_switch(self) {
		let _ = self.done_syncing_receiver.await;
	}
}

impl SyncOracle for TestSyncOracle {
	fn is_major_syncing(&self) -> bool {
		let is_major_syncing = self.flag.load(Ordering::SeqCst);

		if !is_major_syncing {
			if let Some(sender) = self.done_syncing_sender.lock().take() {
				let _ = sender.send(());
			}
		}

		is_major_syncing
	}

	fn is_offline(&self) -> bool {
		unimplemented!("not used in network bridge")
	}
}

// val - result of `is_major_syncing`.
fn make_sync_oracle(val: bool) -> (Box<dyn SyncOracle + Send>, TestSyncOracleHandle) {
	let (tx, rx) = oneshot::channel();
	let flag = Arc::new(AtomicBool::new(val));
	let oracle =
		TestSyncOracle { flag: flag.clone(), done_syncing_sender: Arc::new(Mutex::new(Some(tx))) };
	let handle = TestSyncOracleHandle { done_syncing_receiver: rx, is_major_syncing: flag };

	(Box::new(oracle), handle)
}

#[cfg(test)]
pub mod test_constants {
	use crate::approval_db::common::Config as DatabaseConfig;
	const DATA_COL: u32 = 0;

	pub(crate) const NUM_COLUMNS: u32 = 1;

	pub(crate) const TEST_CONFIG: DatabaseConfig = DatabaseConfig { col_approval_data: DATA_COL };
}

fn slot_to_tick(t: impl Into<Slot>) -> Tick {
	slot_number_to_tick(SLOT_DURATION_MILLIS, t.into())
}

#[derive(Default, Clone)]
struct MockClock {
	inner: Arc<Mutex<MockClockInner>>,
}

impl MockClock {
	fn new(tick: Tick) -> Self {
		let me = Self::default();
		me.inner.lock().set_tick(tick);
		me
	}
}

impl Clock for MockClock {
	fn tick_now(&self) -> Tick {
		self.inner.lock().tick
	}

	fn wait(&self, tick: Tick) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
		let rx = self.inner.lock().register_wakeup(tick, true);

		Box::pin(async move {
			rx.await.expect("i exist in a timeless void. yet, i remain");
		})
	}
}

// This mock clock allows us to manipulate the time and
// be notified when wakeups have been triggered.
#[derive(Default, Debug)]
struct MockClockInner {
	tick: Tick,
	wakeups: Vec<(Tick, oneshot::Sender<()>)>,
}

impl MockClockInner {
	fn set_tick(&mut self, tick: Tick) {
		self.tick = tick;
		self.wakeup_all(tick);
	}

	fn wakeup_all(&mut self, up_to: Tick) {
		// This finds the position of the first wakeup after
		// the given tick, or the end of the map.
		let drain_up_to = self.wakeups.partition_point(|w| w.0 <= up_to);
		for (_, wakeup) in self.wakeups.drain(..drain_up_to) {
			let _ = wakeup.send(());
		}
	}

	fn next_wakeup(&self) -> Option<Tick> {
		self.wakeups.iter().map(|w| w.0).next()
	}

	fn current_wakeup_is(&mut self, tick: Tick) -> bool {
		// first, prune away all wakeups which aren't actually being awaited
		// on.
		self.wakeups.retain(|(_, tx)| !tx.is_canceled());

		// Then see if any remaining wakeups match the tick.
		// This should be the only wakeup.
		self.wakeups.binary_search_by_key(&tick, |w| w.0).is_ok()
	}

	// If `pre_emptive` is true, we compare the given tick to the internal
	// tick of the clock for an early return.
	//
	// Otherwise, the wakeup will only trigger alongside another wakeup of
	// equal or greater tick.
	//
	// When the pre-emptive wakeup is disabled, this can be used in combination with
	// a preceding call to `set_tick` to wait until some other wakeup at that same tick
	//  has been triggered.
	fn register_wakeup(&mut self, tick: Tick, pre_emptive: bool) -> oneshot::Receiver<()> {
		let (tx, rx) = oneshot::channel();

		let pos = self.wakeups.partition_point(|w| w.0 <= tick);
		self.wakeups.insert(pos, (tick, tx));

		if pre_emptive {
			// if `tick > self.tick`, this won't wake up the new
			// listener.
			self.wakeup_all(self.tick);
		}

		rx
	}
}

struct MockAssignmentCriteria<Compute, Check>(Compute, Check);

impl<Compute, Check> AssignmentCriteria for MockAssignmentCriteria<Compute, Check>
where
	Compute: Fn() -> HashMap<polkadot_primitives::CoreIndex, criteria::OurAssignment>,
	Check: Fn(ValidatorIndex) -> Result<DelayTranche, criteria::InvalidAssignment>,
{
	fn compute_assignments(
		&self,
		_keystore: &LocalKeystore,
		_relay_vrf_story: polkadot_node_primitives::approval::v1::RelayVRFStory,
		_config: &criteria::Config,
		_leaving_cores: Vec<(
			CandidateHash,
			polkadot_primitives::CoreIndex,
			polkadot_primitives::GroupIndex,
		)>,
	) -> HashMap<polkadot_primitives::CoreIndex, criteria::OurAssignment> {
		self.0()
	}

	fn check_assignment_cert(
		&self,
		_claimed_core_bitfield: polkadot_node_primitives::approval::v2::CoreBitfield,
		validator_index: ValidatorIndex,
		_config: &criteria::Config,
		_relay_vrf_story: polkadot_node_primitives::approval::v1::RelayVRFStory,
		_assignment: &polkadot_node_primitives::approval::v2::AssignmentCertV2,
		_backing_groups: Vec<polkadot_primitives::GroupIndex>,
	) -> Result<polkadot_node_primitives::approval::v1::DelayTranche, criteria::InvalidAssignment>
	{
		self.1(validator_index)
	}
}

impl<F>
	MockAssignmentCriteria<fn() -> HashMap<polkadot_primitives::CoreIndex, criteria::OurAssignment>, F>
{
	fn check_only(f: F) -> Self {
		MockAssignmentCriteria(Default::default, f)
	}
}

#[derive(Default, Clone)]
struct TestStoreInner {
	stored_block_range: Option<StoredBlockRange>,
	blocks_at_height: HashMap<BlockNumber, Vec<Hash>>,
	block_entries: HashMap<Hash, BlockEntry>,
	candidate_entries: HashMap<CandidateHash, CandidateEntry>,
}

impl Backend for TestStoreInner {
	fn load_block_entry(&self, block_hash: &Hash) -> SubsystemResult<Option<BlockEntry>> {
		Ok(self.block_entries.get(block_hash).cloned())
	}

	fn load_candidate_entry(
		&self,
		candidate_hash: &CandidateHash,
	) -> SubsystemResult<Option<CandidateEntry>> {
		Ok(self.candidate_entries.get(candidate_hash).cloned())
	}

	fn load_blocks_at_height(&self, height: &BlockNumber) -> SubsystemResult<Vec<Hash>> {
		Ok(self.blocks_at_height.get(height).cloned().unwrap_or_default())
	}

	fn load_all_blocks(&self) -> SubsystemResult<Vec<Hash>> {
		let mut hashes: Vec<_> = self.block_entries.keys().cloned().collect();

		hashes.sort_by_key(|k| self.block_entries.get(k).unwrap().block_number());

		Ok(hashes)
	}

	fn load_stored_blocks(&self) -> SubsystemResult<Option<StoredBlockRange>> {
		Ok(self.stored_block_range.clone())
	}

	fn write<I>(&mut self, ops: I) -> SubsystemResult<()>
	where
		I: IntoIterator<Item = BackendWriteOp>,
	{
		for op in ops {
			match op {
				BackendWriteOp::WriteStoredBlockRange(stored_block_range) => {
					self.stored_block_range = Some(stored_block_range);
				},
				BackendWriteOp::DeleteStoredBlockRange => {
					self.stored_block_range = None;
				},
				BackendWriteOp::WriteBlocksAtHeight(h, blocks) => {
					self.blocks_at_height.insert(h, blocks);
				},
				BackendWriteOp::DeleteBlocksAtHeight(h) => {
					let _ = self.blocks_at_height.remove(&h);
				},
				BackendWriteOp::WriteBlockEntry(block_entry) => {
					self.block_entries.insert(block_entry.block_hash(), block_entry);
				},
				BackendWriteOp::DeleteBlockEntry(hash) => {
					let _ = self.block_entries.remove(&hash);
				},
				BackendWriteOp::WriteCandidateEntry(candidate_entry) => {
					self.candidate_entries
						.insert(candidate_entry.candidate_receipt().hash(), candidate_entry);
				},
				BackendWriteOp::DeleteCandidateEntry(candidate_hash) => {
					let _ = self.candidate_entries.remove(&candidate_hash);
				},
			}
		}

		Ok(())
	}
}

#[derive(Default, Clone)]
pub struct TestStore {
	store: Arc<Mutex<TestStoreInner>>,
}

impl Backend for TestStore {
	fn load_block_entry(&self, block_hash: &Hash) -> SubsystemResult<Option<BlockEntry>> {
		let store = self.store.lock();
		store.load_block_entry(block_hash)
	}

	fn load_candidate_entry(
		&self,
		candidate_hash: &CandidateHash,
	) -> SubsystemResult<Option<CandidateEntry>> {
		let store = self.store.lock();
		store.load_candidate_entry(candidate_hash)
	}

	fn load_blocks_at_height(&self, height: &BlockNumber) -> SubsystemResult<Vec<Hash>> {
		let store = self.store.lock();
		store.load_blocks_at_height(height)
	}

	fn load_all_blocks(&self) -> SubsystemResult<Vec<Hash>> {
		let store = self.store.lock();
		store.load_all_blocks()
	}

	fn load_stored_blocks(&self) -> SubsystemResult<Option<StoredBlockRange>> {
		let store = self.store.lock();
		store.load_stored_blocks()
	}

	fn write<I>(&mut self, ops: I) -> SubsystemResult<()>
	where
		I: IntoIterator<Item = BackendWriteOp>,
	{
		let mut store = self.store.lock();
		store.write(ops)
	}
}

fn garbage_assignment_cert_v2(kind: AssignmentCertKindV2) -> AssignmentCertV2 {
	let ctx = schnorrkel::signing_context(RELAY_VRF_MODULO_CONTEXT);
	let msg = b"test-garbage";
	let mut prng = rand_core::OsRng;
	let keypair = schnorrkel::Keypair::generate_with(&mut prng);
	let (inout, proof, _) = keypair.vrf_sign(ctx.bytes(msg));
	let preout = inout.to_preout();

	AssignmentCertV2 {
		kind,
		vrf: VrfSignature { pre_output: VrfPreOutput(preout), proof: VrfProof(proof) },
	}
}

fn sign_approval(
	key: Sr25519Keyring,
	candidate_hash: CandidateHash,
	session_index: SessionIndex,
) -> ValidatorSignature {
	key.sign(&ApprovalVote(candidate_hash).signing_payload(session_index)).into()
}

fn sign_approval_multiple_candidates(
	key: Sr25519Keyring,
	candidate_hashes: Vec<CandidateHash>,
	session_index: SessionIndex,
) -> ValidatorSignature {
	key.sign(&ApprovalVoteMultipleCandidates(&candidate_hashes).signing_payload(session_index))
		.into()
}

type VirtualOverseer =
	polkadot_node_subsystem_test_helpers::TestSubsystemContextHandle<ApprovalVotingMessage>;

#[derive(Default)]
struct HarnessConfigBuilder {
	sync_oracle: Option<(Box<dyn SyncOracle + Send>, TestSyncOracleHandle)>,
	clock: Option<MockClock>,
	backend: Option<TestStore>,
	assignment_criteria: Option<Box<dyn AssignmentCriteria + Send + Sync + 'static>>,
	major_syncing: bool,
}

impl HarnessConfigBuilder {
	pub fn assignment_criteria(
		&mut self,
		assignment_criteria: Box<dyn AssignmentCriteria + Send + Sync + 'static>,
	) -> &mut Self {
		self.assignment_criteria = Some(assignment_criteria);
		self
	}

	pub fn major_syncing(&mut self, value: bool) -> &mut Self {
		self.major_syncing = value;
		self
	}

	pub fn backend(&mut self, store: TestStore) -> &mut Self {
		self.backend = Some(store);
		self
	}

	pub fn build(&mut self) -> HarnessConfig {
		let (sync_oracle, sync_oracle_handle) =
			self.sync_oracle.take().unwrap_or_else(|| make_sync_oracle(self.major_syncing));

		let assignment_criteria = self
			.assignment_criteria
			.take()
			.unwrap_or_else(|| Box::new(MockAssignmentCriteria::check_only(|_| Ok(0))));

		HarnessConfig {
			sync_oracle,
			sync_oracle_handle,
			clock: self.clock.take().unwrap_or_else(|| MockClock::new(0)),
			backend: self.backend.take().unwrap_or_else(|| TestStore::default()),
			assignment_criteria,
		}
	}
}

struct HarnessConfig {
	sync_oracle: Box<dyn SyncOracle + Send>,
	sync_oracle_handle: TestSyncOracleHandle,
	clock: MockClock,
	backend: TestStore,
	assignment_criteria: Box<dyn AssignmentCriteria + Send + Sync + 'static>,
}

impl HarnessConfig {
	pub fn backend(&self) -> TestStore {
		self.backend.clone()
	}
}

impl Default for HarnessConfig {
	fn default() -> Self {
		HarnessConfigBuilder::default().build()
	}
}

struct TestHarness {
	virtual_overseer: VirtualOverseer,
	clock: Arc<MockClock>,
	sync_oracle_handle: TestSyncOracleHandle,
}

fn test_harness<T: Future<Output = VirtualOverseer>>(
	config: HarnessConfig,
	test: impl FnOnce(TestHarness) -> T,
) {
	sp_tracing::init_for_tests();

	let HarnessConfig { sync_oracle, sync_oracle_handle, clock, backend, assignment_criteria } =
		config;

	let pool = sp_core::testing::TaskExecutor::new();
	let (mut context, virtual_overseer) =
		polkadot_node_subsystem_test_helpers::make_subsystem_context(pool.clone());

	let keystore = LocalKeystore::in_memory();
	let _ = keystore.sr25519_generate_new(
		polkadot_primitives::PARACHAIN_KEY_TYPE_ID,
		Some(&Sr25519Keyring::Alice.to_seed()),
	);

	let clock = Arc::new(clock);
	let db = kvdb_memorydb::create(test_constants::NUM_COLUMNS);
	let db = polkadot_node_subsystem_util::database::kvdb_impl::DbAdapter::new(db, &[]);
	let sender = context.sender().clone();
	let subsystem = run(
		context,
		sender.clone(),
		sender.clone(),
		ApprovalVotingSubsystem::with_config_and_clock(
			Config {
				col_approval_data: test_constants::TEST_CONFIG.col_approval_data,
				slot_duration_millis: SLOT_DURATION_MILLIS,
			},
			Arc::new(db),
			Arc::new(keystore),
			sync_oracle,
			Metrics::default(),
			clock.clone(),
			Arc::new(SpawnGlue(pool)),
			NUM_APPROVAL_RETRIES,
			RETRY_BACKOFF,
		),
		assignment_criteria,
		backend,
	);

	let test_fut = test(TestHarness { virtual_overseer, clock, sync_oracle_handle });

	futures::pin_mut!(test_fut);
	futures::pin_mut!(subsystem);

	futures::executor::block_on(future::join(
		async move {
			let mut overseer = test_fut.await;
			overseer_signal(&mut overseer, OverseerSignal::Conclude).await;
		},
		subsystem,
	))
	.1
	.unwrap();
}

async fn overseer_send(overseer: &mut VirtualOverseer, msg: FromOrchestra<ApprovalVotingMessage>) {
	gum::trace!("Sending message:\n{:?}", &msg);
	overseer
		.send(msg)
		.timeout(TIMEOUT)
		.await
		.expect(&format!("{:?} is enough for sending messages.", TIMEOUT));
}

async fn overseer_recv(overseer: &mut VirtualOverseer) -> AllMessages {
	let msg = overseer_recv_with_timeout(overseer, TIMEOUT)
		.await
		.expect(&format!("{:?} is enough to receive messages.", TIMEOUT));

	gum::trace!("Received message:\n{:?}", &msg);

	msg
}

async fn overseer_recv_with_timeout(
	overseer: &mut VirtualOverseer,
	timeout: Duration,
) -> Option<AllMessages> {
	gum::trace!("Waiting for message...");
	overseer.recv().timeout(timeout).await
}

async fn overseer_signal(overseer: &mut VirtualOverseer, signal: OverseerSignal) {
	overseer
		.send(FromOrchestra::Signal(signal))
		.timeout(TIMEOUT)
		.await
		.expect(&format!("{:?} is more than enough for sending signals.", TIMEOUT));
}

fn overlay_txn<T, F>(db: &mut T, mut f: F)
where
	T: Backend,
	F: FnMut(&mut OverlayedBackend<'_, T>),
{
	let mut overlay_db = OverlayedBackend::new(db);
	f(&mut overlay_db);
	let write_ops = overlay_db.into_write_ops();
	db.write(write_ops).unwrap();
}

fn make_candidate(para_id: ParaId, hash: &Hash) -> CandidateReceipt {
	let mut r = dummy_candidate_receipt_v2_bad_sig(*hash, Some(Default::default()));
	r.descriptor.set_para_id(para_id);
	r
}

async fn import_approval(
	overseer: &mut VirtualOverseer,
	block_hash: Hash,
	candidate_index: CandidateIndex,
	validator: ValidatorIndex,
	candidate_hash: CandidateHash,
	session_index: SessionIndex,
	expect_chain_approved: bool,
	signature_opt: Option<ValidatorSignature>,
) -> oneshot::Receiver<ApprovalCheckResult> {
	let signature = signature_opt.unwrap_or(sign_approval(
		Sr25519Keyring::Alice,
		candidate_hash,
		session_index,
	));
	let (tx, rx) = oneshot::channel();
	overseer_send(
		overseer,
		FromOrchestra::Communication {
			msg: ApprovalVotingMessage::ImportApproval(
				CheckedIndirectSignedApprovalVote::from_checked(IndirectSignedApprovalVoteV2 {
					block_hash,
					candidate_indices: candidate_index.into(),
					validator,
					signature,
				}),
				Some(tx),
			),
		},
	)
	.await;
	if expect_chain_approved {
		assert_matches!(
			overseer_recv(overseer).await,
			AllMessages::ChainSelection(ChainSelectionMessage::Approved(b_hash)) => {
				assert_eq!(b_hash, block_hash);
			}
		);
	}
	rx
}

async fn import_assignment(
	overseer: &mut VirtualOverseer,
	block_hash: Hash,
	candidate_index: CandidateIndex,
	validator: ValidatorIndex,
	tranche: DelayTranche,
) -> oneshot::Receiver<AssignmentCheckResult> {
	let (tx, rx) = oneshot::channel();
	overseer_send(
		overseer,
		FromOrchestra::Communication {
			msg: ApprovalVotingMessage::ImportAssignment(
				CheckedIndirectAssignment::from_checked(
					IndirectAssignmentCertV2 {
						block_hash,
						validator,
						cert: garbage_assignment_cert_v2(
							AssignmentCertKindV2::RelayVRFModuloCompact {
								core_bitfield: CoreIndex(0).into(),
							},
						),
					},
					candidate_index.into(),
					tranche,
				),
				Some(tx),
			),
		},
	)
	.await;
	rx
}

async fn import_assignment_v2(
	overseer: &mut VirtualOverseer,
	block_hash: Hash,
	core_indices: Vec<u32>,
	validator: ValidatorIndex,
) -> oneshot::Receiver<AssignmentCheckResult> {
	let (tx, rx) = oneshot::channel();
	overseer_send(
		overseer,
		FromOrchestra::Communication {
			msg: ApprovalVotingMessage::ImportAssignment(
				CheckedIndirectAssignment::from_checked(
					IndirectAssignmentCertV2 {
						block_hash,
						validator,
						cert: garbage_assignment_cert_v2(
							AssignmentCertKindV2::RelayVRFModuloCompact {
								core_bitfield: core_indices
									.clone()
									.into_iter()
									.map(|c| CoreIndex(c))
									.collect::<Vec<_>>()
									.try_into()
									.unwrap(),
							},
						),
					},
					core_indices.try_into().unwrap(),
					0,
				),
				Some(tx),
			),
		},
	)
	.await;
	rx
}
struct BlockConfig {
	slot: Slot,
	candidates: Option<Vec<(CandidateReceipt, CoreIndex, GroupIndex)>>,
	session_info: Option<SessionInfo>,
	end_syncing: bool,
	// The approval voting params returned by the runtime when the session info is fetched at
	// import time. These get cached in the `ExtendedSessionInfo` and are later read by
	// `maybe_create_signature` to decide how many approvals to coalesce.
	approval_voting_params: ApprovalVotingParams,
}

impl Default for BlockConfig {
	fn default() -> Self {
		BlockConfig {
			slot: Slot::from(0),
			candidates: None,
			session_info: None,
			end_syncing: false,
			approval_voting_params: ApprovalVotingParams::default(),
		}
	}
}

struct ChainBuilder {
	blocks_by_hash: HashMap<Hash, (Header, BlockConfig)>,
	blocks_at_height: BTreeMap<u32, Vec<Hash>>,
	is_major_syncing: Arc<AtomicBool>,
}

impl ChainBuilder {
	const GENESIS_HASH: Hash = Hash::repeat_byte(0xff);
	const GENESIS_PARENT_HASH: Hash = Hash::repeat_byte(0x00);

	pub fn new() -> Self {
		let mut builder = Self {
			blocks_by_hash: HashMap::new(),
			blocks_at_height: BTreeMap::new(),
			is_major_syncing: Arc::new(AtomicBool::new(false)),
		};
		builder.add_block_inner(
			Self::GENESIS_HASH,
			Self::GENESIS_PARENT_HASH,
			0,
			BlockConfig { slot: Slot::from(0), ..Default::default() },
		);
		builder
	}
	pub fn major_syncing(&mut self, major_syncing: Arc<AtomicBool>) -> &mut Self {
		self.is_major_syncing = major_syncing;
		self
	}

	pub fn add_block(
		&mut self,
		hash: Hash,
		parent_hash: Hash,
		number: u32,
		config: BlockConfig,
	) -> &mut Self {
		assert!(number != 0, "cannot add duplicate genesis block");
		assert!(hash != Self::GENESIS_HASH, "cannot add block with genesis hash");
		assert!(
			parent_hash != Self::GENESIS_PARENT_HASH,
			"cannot add block with genesis parent hash"
		);
		assert!(self.blocks_by_hash.len() < u8::MAX.into());
		self.add_block_inner(hash, parent_hash, number, config)
	}

	fn add_block_inner(
		&mut self,
		hash: Hash,
		parent_hash: Hash,
		number: u32,
		config: BlockConfig,
	) -> &mut Self {
		let header = ChainBuilder::make_header(parent_hash, config.slot, number);
		assert!(
			self.blocks_by_hash.insert(hash, (header, config)).is_none(),
			"block with hash {:?} already exists",
			hash,
		);
		self.blocks_at_height.entry(number).or_insert_with(Vec::new).push(hash);
		self
	}

	pub async fn build(&self, overseer: &mut VirtualOverseer) {
		for (number, blocks) in self.blocks_at_height.iter() {
			for (i, hash) in blocks.iter().enumerate() {
				let mut cur_hash = *hash;
				let (_, block_config) =
					self.blocks_by_hash.get(&cur_hash).expect("block not found");
				let mut ancestry = Vec::new();
				while cur_hash != Self::GENESIS_PARENT_HASH {
					let (cur_header, _) =
						self.blocks_by_hash.get(&cur_hash).expect("chain is not contiguous");
					ancestry.push((cur_hash, cur_header.clone()));
					cur_hash = cur_header.parent_hash;
				}
				ancestry.reverse();
				import_block(
					overseer,
					ancestry.as_ref(),
					*number,
					block_config,
					false,
					i > 0,
					self.is_major_syncing.clone(),
				)
				.await;
				let _: Option<()> = future::pending().timeout(Duration::from_millis(100)).await;
			}
		}
	}

	fn make_header(parent_hash: Hash, slot: Slot, number: u32) -> Header {
		let digest = {
			let mut digest = Digest::default();
			let vrf_signature = garbage_vrf_signature();
			digest.push(DigestItem::babe_pre_digest(PreDigest::SecondaryVRF(
				SecondaryVRFPreDigest { authority_index: 0, slot, vrf_signature },
			)));
			digest
		};

		Header {
			digest,
			extrinsics_root: Default::default(),
			number,
			state_root: Default::default(),
			parent_hash,
		}
	}
}

fn session_info(keys: &[Sr25519Keyring]) -> SessionInfo {
	SessionInfo {
		validators: keys.iter().map(|v| v.public().into()).collect(),
		discovery_keys: keys.iter().map(|v| v.public().into()).collect(),
		assignment_keys: keys.iter().map(|v| v.public().into()).collect(),
		validator_groups: IndexedVec::<GroupIndex, Vec<ValidatorIndex>>::from(vec![
			vec![ValidatorIndex(0)],
			vec![ValidatorIndex(1)],
		]),
		n_cores: 10,
		needed_approvals: 2,
		zeroth_delay_tranche_width: 5,
		relay_vrf_modulo_samples: 3,
		n_delay_tranches: 50,
		no_show_slots: 2,
		active_validator_indices: vec![],
		dispute_period: 6,
		random_seed: [0u8; 32],
	}
}

async fn import_block(
	overseer: &mut VirtualOverseer,
	hashes: &[(Hash, Header)],
	number: u32,
	config: &BlockConfig,
	gap: bool,
	fork: bool,
	major_syncing: Arc<AtomicBool>,
) {
	let (new_head, new_header) = &hashes[hashes.len() - 1];
	let candidates = config.candidates.clone().unwrap_or(vec![(
		make_candidate(ParaId::from(0_u32), &new_head),
		CoreIndex(0),
		GroupIndex(0),
	)]);

	let session_info = config.session_info.clone().unwrap_or({
		let validators = vec![Sr25519Keyring::Alice, Sr25519Keyring::Bob];
		SessionInfo { needed_approvals: 1, ..session_info(&validators) }
	});

	overseer_send(
		overseer,
		FromOrchestra::Signal(OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(
			new_leaf(*new_head, number),
		))),
	)
	.await;

	assert_matches!(
		overseer_recv(overseer).await,
		AllMessages::ChainApi(ChainApiMessage::BlockHeader(head, h_tx)) => {
			assert_eq!(*new_head, head);
			h_tx.send(Ok(Some(new_header.clone()))).unwrap();
		}
	);

	let is_major_syncing = major_syncing.load(Ordering::SeqCst);
	if config.end_syncing {
		major_syncing.store(false, Ordering::SeqCst);
	}

	if !fork {
		let mut _ancestry_step = 0;
		if gap {
			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::ChainApi(ChainApiMessage::Ancestors {
					hash,
					k,
					response_channel,
				}) => {
					assert_eq!(hash, *new_head);
					let history: Vec<Hash> = hashes.iter().map(|v| v.0).take(k).collect();
					let _ = response_channel.send(Ok(history));
					_ancestry_step = k;
				}
			);

			for i in 0.._ancestry_step {
				match overseer_recv(overseer).await {
					AllMessages::ChainApi(ChainApiMessage::BlockHeader(_, h_tx)) => {
						let (hash, header) = hashes[i as usize].clone();
						assert_eq!(hash, *new_head);
						h_tx.send(Ok(Some(header))).unwrap();
					},
					AllMessages::ChainApi(ChainApiMessage::Ancestors {
						hash,
						k,
						response_channel,
					}) => {
						assert_eq!(hash, *new_head);
						assert_eq!(k as u32, number - 1);
						let history: Vec<Hash> = hashes.iter().map(|v| v.0).take(k).collect();
						response_channel.send(Ok(history)).unwrap();
					},
					_ => unreachable! {},
				}
			}
		}
	}

	if number > 0 && !is_major_syncing {
		assert_matches!(
			overseer_recv(overseer).await,
			AllMessages::RuntimeApi(
				RuntimeApiMessage::Request(hash, RuntimeApiRequest::CandidateEvents(c_tx))
			) => {
				assert_eq!(hash, *new_head);
				let inclusion_events = candidates.into_iter()
					.map(|(r, c, g)| CandidateEvent::CandidateIncluded(r, Vec::new().into(), c, g))
					.collect::<Vec<_>>();
				c_tx.send(Ok(inclusion_events)).unwrap();
			}
		);
		assert_matches!(
			overseer_recv(overseer).await,
			AllMessages::RuntimeApi(
				RuntimeApiMessage::Request(
					req_block_hash,
					RuntimeApiRequest::SessionIndexForChild(s_tx)
				)
			) => {
				let hash = &hashes[(number-1) as usize];
				assert_eq!(req_block_hash, hash.0.clone());
				s_tx.send(Ok(number.into())).unwrap();
			}
		);

		assert_matches!(
			overseer_recv(overseer).await,
			AllMessages::RuntimeApi(
				RuntimeApiMessage::Request(
					req_block_hash,
					RuntimeApiRequest::CurrentBabeEpoch(c_tx),
				)
			) => {
				let hash = &hashes[number as usize];
				assert_eq!(req_block_hash, hash.0.clone());
				let _ = c_tx.send(Ok(BabeEpoch {
					epoch_index: number as _,
					start_slot: Slot::from(0),
					duration: 200,
					authorities: vec![(Sr25519Keyring::Alice.public().into(), 1)],
					randomness: [0u8; 32],
					config: BabeEpochConfiguration {
						c: (1, 4),
						allowed_slots: AllowedSlots::PrimarySlots,
					},
				}));
			}
		);
	}

	if number == 0 && !is_major_syncing {
		assert_matches!(
			overseer_recv(overseer).await,
			AllMessages::ApprovalDistribution(ApprovalDistributionMessage::NewBlocks(v)) => {
				assert_eq!(v.len(), 0usize);
			}
		);
	} else if number > 0 && !is_major_syncing {
		if !fork {
			// SessionInfo won't be called for forks - it's already cached
			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(
					RuntimeApiMessage::Request(
						req_block_hash,
						RuntimeApiRequest::SessionInfo(_, si_tx),
					)
				) => {
					// Make sure all SessionInfo calls are not made for the leaf (but for its relay parent)
					assert_ne!(req_block_hash, hashes[(number-1) as usize].0);
					si_tx.send(Ok(Some(session_info.clone()))).unwrap();
				}
			);
			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(
					RuntimeApiMessage::Request(_, RuntimeApiRequest::NodeFeatures(_, si_tx), )
				) => {
					si_tx.send(Ok(NodeFeatures::EMPTY)).unwrap();
				}
			);
			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(
					RuntimeApiMessage::Request(_, RuntimeApiRequest::ApprovalVotingParams(_, si_tx), )
				) => {
					si_tx.send(Ok(config.approval_voting_params)).unwrap();
				}
			);
		}

		if !is_major_syncing {
			assert_matches!(
				overseer_recv(overseer).await,

				AllMessages::ApprovalDistribution(
					ApprovalDistributionMessage::NewBlocks(mut approval_vec)
				) => {
					assert_eq!(approval_vec.len(), 1);
					let metadata = approval_vec.pop().unwrap();
					let hash = &hashes[number as usize];
					let parent_hash = &hashes[(number - 1) as usize];
					assert_eq!(metadata.hash, hash.0.clone());
					assert_eq!(metadata.parent_hash, parent_hash.0.clone());
					assert_eq!(metadata.slot, config.slot);
				}
			);
		}
	}

	match overseer.peek().timeout(Duration::from_millis(50)).await.flatten() {
		Some(AllMessages::RuntimeApi(RuntimeApiMessage::Request(
			_,
			RuntimeApiRequest::SessionIndexForChild(_),
		))) => {
			let AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				_,
				RuntimeApiRequest::SessionIndexForChild(s_tx),
			)) = overseer.recv().await
			else {
				panic!("Unexpected message");
			};
			s_tx.send(Ok(1u32.into())).unwrap();
		},
		_ => (),
	}
}

#[test]
fn subsystem_rejects_bad_assignment_ok_criteria() {
	test_harness(HarnessConfig::default(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, sync_oracle_handle: _sync_oracle_handle, .. } =
			test_harness;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ChainApi(ChainApiMessage::FinalizedBlockNumber(rx)) => {
				rx.send(Ok(0)).unwrap();
			}
		);

		let block_hash = Hash::repeat_byte(0x01);
		let candidate_index = 0;
		let validator = ValidatorIndex(0);

		let head: Hash = ChainBuilder::GENESIS_HASH;
		let mut builder = ChainBuilder::new();
		let slot = Slot::from(1 as u64);
		builder.add_block(block_hash, head, 1, BlockConfig { slot, ..Default::default() });
		builder.build(&mut virtual_overseer).await;

		let rx =
			import_assignment(&mut virtual_overseer, block_hash, candidate_index, validator, 0)
				.await;

		assert_eq!(rx.await, Ok(AssignmentCheckResult::Accepted),);

		// unknown hash
		let unknown_hash = Hash::repeat_byte(0x02);

		let rx =
			import_assignment(&mut virtual_overseer, unknown_hash, candidate_index, validator, 0)
				.await;

		assert_eq!(
			rx.await,
			Ok(AssignmentCheckResult::Bad(AssignmentCheckError::UnknownBlock(unknown_hash))),
		);

		virtual_overseer
	});
}

#[test]
fn blank_subsystem_act_on_bad_block() {
	test_harness(HarnessConfig::default(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, sync_oracle_handle, .. } = test_harness;
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ChainApi(ChainApiMessage::FinalizedBlockNumber(rx)) => {
				rx.send(Ok(0)).unwrap();
			}
		);

		let (tx, rx) = oneshot::channel();

		let bad_block_hash: Hash = Default::default();

		overseer_send(
			&mut virtual_overseer,
			FromOrchestra::Communication {
				msg: ApprovalVotingMessage::ImportAssignment(
					CheckedIndirectAssignment::from_checked(
						IndirectAssignmentCertV2 {
							block_hash: bad_block_hash,
							validator: 0u32.into(),
							cert: garbage_assignment_cert_v2(
								AssignmentCertKindV2::RelayVRFModuloCompact {
									core_bitfield: CoreIndex(0).into(),
								},
							),
						},
						0u32.into(),
						0,
					),
					Some(tx),
				),
			},
		)
		.await;

		sync_oracle_handle.await_mode_switch().await;

		assert_matches!(
			rx.await,
			Ok(
				AssignmentCheckResult::Bad(AssignmentCheckError::UnknownBlock(hash))
			) => {
				assert_eq!(hash, bad_block_hash);
			}
		);

		virtual_overseer
	});
}

#[test]
fn subsystem_rejects_approval_if_no_candidate_entry() {
	let config = HarnessConfig::default();
	let store = config.backend();
	test_harness(config, |test_harness| async move {
		let TestHarness { mut virtual_overseer, sync_oracle_handle: _sync_oracle_handle, .. } =
			test_harness;
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ChainApi(ChainApiMessage::FinalizedBlockNumber(rx)) => {
				rx.send(Ok(0)).unwrap();
			}
		);

		let block_hash = Hash::repeat_byte(0x01);
		let candidate_index = 0;
		let validator = ValidatorIndex(0);

		let candidate_descriptor = make_candidate(ParaId::from(1_u32), &block_hash);
		let candidate_hash = candidate_descriptor.hash();

		let head: Hash = ChainBuilder::GENESIS_HASH;
		let mut builder = ChainBuilder::new();
		let slot = Slot::from(1 as u64);
		builder.add_block(
			block_hash,
			head,
			1,
			BlockConfig {
				slot,
				candidates: Some(vec![(candidate_descriptor, CoreIndex(1), GroupIndex(1))]),
				session_info: None,
				end_syncing: false,
				approval_voting_params: ApprovalVotingParams::default(),
			},
		);
		builder.build(&mut virtual_overseer).await;

		overlay_txn(&mut store.clone(), |overlay_db| {
			overlay_db.delete_candidate_entry(&candidate_hash)
		});

		let session_index = 1;
		let rx = import_approval(
			&mut virtual_overseer,
			block_hash,
			candidate_index,
			validator,
			candidate_hash,
			session_index,
			false,
			None,
		)
		.await;

		assert_matches!(
			rx.await,
			Ok(ApprovalCheckResult::Bad(ApprovalCheckError::InvalidCandidate(0, hash))) => {
				assert_eq!(candidate_hash, hash);
			}
		);

		virtual_overseer
	});
}

#[test]
fn subsystem_rejects_approval_if_no_block_entry() {
	test_harness(HarnessConfig::default(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, sync_oracle_handle: _sync_oracle_handle, .. } =
			test_harness;
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ChainApi(ChainApiMessage::FinalizedBlockNumber(rx)) => {
				rx.send(Ok(0)).unwrap();
			}
		);

		let block_hash = Hash::repeat_byte(0x01);
		let candidate_index = 0;
		let validator = ValidatorIndex(0);
		let candidate_hash = dummy_candidate_receipt_v2(block_hash).hash();
		let session_index = 1;

		let rx = import_approval(
			&mut virtual_overseer,
			block_hash,
			candidate_index,
			validator,
			candidate_hash,
			session_index,
			false,
			None,
		)
		.await;

		assert_matches!(
			rx.await,
			Ok(ApprovalCheckResult::Bad(ApprovalCheckError::UnknownBlock(hash))) => {
				assert_eq!(hash, block_hash);
			}
		);

		virtual_overseer
	});
}

#[test]
fn subsystem_rejects_approval_before_assignment() {
	test_harness(HarnessConfig::default(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, sync_oracle_handle: _sync_oracle_handle, .. } =
			test_harness;
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ChainApi(ChainApiMessage::FinalizedBlockNumber(rx)) => {
				rx.send(Ok(0)).unwrap();
			}
		);

		let block_hash = Hash::repeat_byte(0x01);

		let candidate_hash = {
			let mut candidate_receipt =
				dummy_candidate_receipt_v2_bad_sig(block_hash, Some(Default::default()));
			candidate_receipt.descriptor.set_para_id(ParaId::from(0_u32));
			candidate_receipt.descriptor.set_relay_parent(block_hash);
			candidate_receipt.hash()
		};

		let candidate_index = 0;
		let validator = ValidatorIndex(0);
		let session_index = 1;

		// Add block hash 00.
		ChainBuilder::new()
			.add_block(
				block_hash,
				ChainBuilder::GENESIS_HASH,
				1,
				BlockConfig {
					slot: Slot::from(1),
					candidates: None,
					session_info: None,
					end_syncing: false,
					approval_voting_params: ApprovalVotingParams::default(),
				},
			)
			.build(&mut virtual_overseer)
			.await;

		let rx = import_approval(
			&mut virtual_overseer,
			block_hash,
			candidate_index,
			validator,
			candidate_hash,
			session_index,
			false,
			None,
		)
		.await;

		assert_matches!(
			rx.await,
			Ok(ApprovalCheckResult::Bad(ApprovalCheckError::NoAssignment(v))) => {
				assert_eq!(v, validator);
			}
		);

		virtual_overseer
	});
}

#[test]
fn subsystem_accepts_duplicate_assignment() {
	test_harness(HarnessConfig::default(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, sync_oracle_handle: _sync_oracle_handle, .. } =
			test_harness;
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ChainApi(ChainApiMessage::FinalizedBlockNumber(rx)) => {
				rx.send(Ok(0)).unwrap();
			}
		);

		let validator = ValidatorIndex(0);
		let candidate_index = 0;
		let block_hash = Hash::repeat_byte(0x01);

		let candidate_receipt1 = {
			let mut receipt = dummy_candidate_receipt_v2(block_hash);
			receipt.descriptor.set_para_id(ParaId::from(1_u32));
			receipt
		}
		.into();
		let candidate_receipt2 = {
			let mut receipt = dummy_candidate_receipt_v2(block_hash);
			receipt.descriptor.set_para_id(ParaId::from(2_u32));
			receipt
		}
		.into();
		let candidate_index1 = 0;
		let candidate_index2 = 1;

		// Add block hash 00.
		ChainBuilder::new()
			.add_block(
				block_hash,
				ChainBuilder::GENESIS_HASH,
				1,
				BlockConfig {
					slot: Slot::from(1),
					candidates: Some(vec![
						(candidate_receipt1, CoreIndex(0), GroupIndex(1)),
						(candidate_receipt2, CoreIndex(1), GroupIndex(1)),
					]),
					session_info: None,
					end_syncing: false,
					approval_voting_params: ApprovalVotingParams::default(),
				},
			)
			.build(&mut virtual_overseer)
			.await;

		// Initial assignment.
		let rx = import_assignment_v2(
			&mut virtual_overseer,
			block_hash,
			vec![candidate_index1, candidate_index2],
			validator,
		)
		.await;

		assert_eq!(rx.await, Ok(AssignmentCheckResult::Accepted));

		// Test with single assigned core.
		let rx =
			import_assignment(&mut virtual_overseer, block_hash, candidate_index, validator, 0)
				.await;

		assert_eq!(rx.await, Ok(AssignmentCheckResult::AcceptedDuplicate));

		// Test with multiple assigned cores. This cannot happen in practice, as tranche0
		// assignments are sent first, but we should still ensure correct behavior.
		let rx = import_assignment_v2(
			&mut virtual_overseer,
			block_hash,
			vec![candidate_index1, candidate_index2],
			validator,
		)
		.await;

		assert_eq!(rx.await, Ok(AssignmentCheckResult::AcceptedDuplicate));

		virtual_overseer
	});
}

#[test]
fn subsystem_rejects_assignment_with_unknown_candidate() {
	test_harness(HarnessConfig::default(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, sync_oracle_handle: _sync_oracle_handle, .. } =
			test_harness;
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ChainApi(ChainApiMessage::FinalizedBlockNumber(rx)) => {
				rx.send(Ok(0)).unwrap();
			}
		);

		let block_hash = Hash::repeat_byte(0x01);
		let candidate_index = 7;
		let validator = ValidatorIndex(0);

		// Add block hash 00.
		ChainBuilder::new()
			.add_block(
				block_hash,
				ChainBuilder::GENESIS_HASH,
				1,
				BlockConfig {
					slot: Slot::from(1),
					candidates: None,
					session_info: None,
					end_syncing: false,
					approval_voting_params: ApprovalVotingParams::default(),
				},
			)
			.build(&mut virtual_overseer)
			.await;

		let rx =
			import_assignment(&mut virtual_overseer, block_hash, candidate_index, validator, 0)
				.await;

		assert_eq!(
			rx.await,
			Ok(AssignmentCheckResult::Bad(AssignmentCheckError::InvalidCandidateIndex(
				candidate_index
			))),
		);

		virtual_overseer
	});
}

#[test]
fn subsystem_rejects_oversized_bitfields() {
	test_harness(HarnessConfig::default(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, sync_oracle_handle: _sync_oracle_handle, .. } =
			test_harness;
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ChainApi(ChainApiMessage::FinalizedBlockNumber(rx)) => {
				rx.send(Ok(0)).unwrap();
			}
		);

		let block_hash = Hash::repeat_byte(0x01);
		let candidate_index = 10;
		let validator = ValidatorIndex(0);

		ChainBuilder::new()
			.add_block(
				block_hash,
				ChainBuilder::GENESIS_HASH,
				1,
				BlockConfig {
					slot: Slot::from(1),
					candidates: None,
					session_info: None,
					end_syncing: false,
					approval_voting_params: ApprovalVotingParams::default(),
				},
			)
			.build(&mut virtual_overseer)
			.await;

		let rx =
			import_assignment(&mut virtual_overseer, block_hash, candidate_index, validator, 0)
				.await;

		assert_eq!(
			rx.await,
			Ok(AssignmentCheckResult::Bad(AssignmentCheckError::InvalidBitfield(
				candidate_index as usize + 1
			))),
		);

		let rx =
			import_assignment_v2(&mut virtual_overseer, block_hash, vec![1, 2, 10, 50], validator)
				.await;

		assert_eq!(
			rx.await,
			Ok(AssignmentCheckResult::Bad(AssignmentCheckError::InvalidBitfield(51))),
		);
		virtual_overseer
	});
}

#[test]
fn subsystem_accepts_and_imports_approval_after_assignment() {
	test_harness(HarnessConfig::default(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, sync_oracle_handle: _sync_oracle_handle, .. } =
			test_harness;
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ChainApi(ChainApiMessage::FinalizedBlockNumber(rx)) => {
				rx.send(Ok(0)).unwrap();
			}
		);

		let block_hash = Hash::repeat_byte(0x01);

		let candidate_hash = {
			let mut candidate_receipt =
				dummy_candidate_receipt_v2_bad_sig(block_hash, Some(Default::default()));
			candidate_receipt.descriptor.set_para_id(ParaId::from(0_u32));
			candidate_receipt.descriptor.set_relay_parent(block_hash);
			candidate_receipt.hash()
		};

		let candidate_index = 0;
		let validator = ValidatorIndex(0);
		let session_index = 1;

		// Add block hash 0x01...
		ChainBuilder::new()
			.add_block(
				block_hash,
				ChainBuilder::GENESIS_HASH,
				1,
				BlockConfig {
					slot: Slot::from(1),
					candidates: None,
					session_info: None,
					end_syncing: false,
					approval_voting_params: ApprovalVotingParams::default(),
				},
			)
			.build(&mut virtual_overseer)
			.await;

		let rx =
			import_assignment(&mut virtual_overseer, block_hash, candidate_index, validator, 0)
				.await;

		assert_eq!(rx.await, Ok(AssignmentCheckResult::Accepted));

		let rx = import_approval(
			&mut virtual_overseer,
			block_hash,
			candidate_index,
			validator,
			candidate_hash,
			session_index,
			true,
			None,
		)
		.await;

		assert_eq!(rx.await, Ok(ApprovalCheckResult::Accepted));

		virtual_overseer
	});
}

#[test]
fn subsystem_second_approval_import_only_schedules_wakeups() {
	test_harness(HarnessConfig::default(), |test_harness| async move {
		let TestHarness {
			mut virtual_overseer,
			clock,
			sync_oracle_handle: _sync_oracle_handle,
			..
		} = test_harness;
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ChainApi(ChainApiMessage::FinalizedBlockNumber(rx)) => {
				rx.send(Ok(0)).unwrap();
			}
		);
		clock.inner.lock().set_tick(APPROVAL_DELAY);

		let block_hash = Hash::repeat_byte(0x01);

		let candidate_hash = {
			let mut candidate_receipt =
				dummy_candidate_receipt_v2_bad_sig(block_hash, Some(Default::default()));
			candidate_receipt.descriptor.set_para_id(ParaId::from(0_u32));
			candidate_receipt.descriptor.set_relay_parent(block_hash);
			candidate_receipt.hash()
		};

		let candidate_index = 0;
		let validator = ValidatorIndex(0);
		let session_index = 1;

		let validators = vec![
			Sr25519Keyring::Alice,
			Sr25519Keyring::Bob,
			Sr25519Keyring::Charlie,
			Sr25519Keyring::Dave,
			Sr25519Keyring::Eve,
		];
		let session_info = SessionInfo {
			validator_groups: IndexedVec::<GroupIndex, Vec<ValidatorIndex>>::from(vec![
				vec![ValidatorIndex(0), ValidatorIndex(1)],
				vec![ValidatorIndex(2)],
				vec![ValidatorIndex(3), ValidatorIndex(4)],
			]),
			needed_approvals: 1,
			..session_info(&validators)
		};

		// Add block hash 0x01...
		ChainBuilder::new()
			.add_block(
				block_hash,
				ChainBuilder::GENESIS_HASH,
				1,
				BlockConfig {
					slot: Slot::from(0),
					candidates: None,
					session_info: Some(session_info),
					end_syncing: false,
					approval_voting_params: ApprovalVotingParams::default(),
				},
			)
			.build(&mut virtual_overseer)
			.await;

		let rx =
			import_assignment(&mut virtual_overseer, block_hash, candidate_index, validator, 0)
				.await;

		assert_eq!(rx.await, Ok(AssignmentCheckResult::Accepted));

		assert!(clock.inner.lock().current_wakeup_is(APPROVAL_DELAY + 2));

		let rx = import_approval(
			&mut virtual_overseer,
			block_hash,
			candidate_index,
			validator,
			candidate_hash,
			session_index,
			false,
			None,
		)
		.await;

		assert_eq!(rx.await, Ok(ApprovalCheckResult::Accepted));

		futures_timer::Delay::new(Duration::from_millis(100)).await;
		assert!(clock.inner.lock().current_wakeup_is(APPROVAL_DELAY + 2));

		let rx = import_approval(
			&mut virtual_overseer,
			block_hash,
			candidate_index,
			validator,
			candidate_hash,
			session_index,
			false,
			None,
		)
		.await;

		assert_eq!(rx.await, Ok(ApprovalCheckResult::Accepted));

		futures_timer::Delay::new(Duration::from_millis(100)).await;
		assert!(clock.inner.lock().current_wakeup_is(APPROVAL_DELAY + 2));

		virtual_overseer
	});
}

#[test]
fn subsystem_assignment_import_updates_candidate_entry_and_schedules_wakeup() {
	test_harness(HarnessConfig::default(), |test_harness| async move {
		let TestHarness {
			mut virtual_overseer,
			clock,
			sync_oracle_handle: _sync_oracle_handle,
			..
		} = test_harness;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ChainApi(ChainApiMessage::FinalizedBlockNumber(rx)) => {
				rx.send(Ok(0)).unwrap();
			}
		);

		let block_hash = Hash::repeat_byte(0x01);

		let candidate_index = 0;
		let validator = ValidatorIndex(0);

		// Add block hash 0x01...
		ChainBuilder::new()
			.add_block(
				block_hash,
				ChainBuilder::GENESIS_HASH,
				1,
				BlockConfig {
					slot: Slot::from(1),
					candidates: None,
					session_info: None,
					end_syncing: false,
					approval_voting_params: ApprovalVotingParams::default(),
				},
			)
			.build(&mut virtual_overseer)
			.await;

		let rx =
			import_assignment(&mut virtual_overseer, block_hash, candidate_index, validator, 0)
				.await;

		assert_eq!(rx.await, Ok(AssignmentCheckResult::Accepted));

		assert!(clock.inner.lock().current_wakeup_is(2));

		virtual_overseer
	});
}

#[test]
fn subsystem_always_has_a_wakeup_when_pending() {
	// Approvals sent after all assignments are no-show, the approval
	// should be counted on the fork relay chain on the next tick.
	test_approvals_on_fork_are_always_considered_after_no_show(
		30,
		vec![(29, false), (30, false), (31, true)],
	);
	// Approvals sent before fork no-shows, the approval
	// should be counted on the fork relay chain when it no-shows.
	test_approvals_on_fork_are_always_considered_after_no_show(
		8, // a tick smaller than the no-show tick which is 30.
		vec![(7, false), (8, false), (29, false), (30, true), (31, true)],
	);
}

fn test_approvals_on_fork_are_always_considered_after_no_show(
	tick_to_send_approval: Tick,
	expected_approval_status: Vec<(Tick, bool)>,
) {
	let config = HarnessConfig::default();
	let store = config.backend();

	test_harness(config, |test_harness| async move {
		let TestHarness {
			mut virtual_overseer,
			clock,
			sync_oracle_handle: _sync_oracle_handle,
			..
		} = test_harness;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ChainApi(ChainApiMessage::FinalizedBlockNumber(rx)) => {
				rx.send(Ok(0)).unwrap();
			}
		);
		let candidate_hash = Hash::repeat_byte(0x04);

		let candidate_descriptor = make_candidate(ParaId::from(1_u32), &candidate_hash);
		let candidate_hash = candidate_descriptor.hash();

		let block_hash = Hash::repeat_byte(0x01);
		let block_hash_fork = Hash::repeat_byte(0x02);

		let candidate_index = 0;
		let validator = ValidatorIndex(0);
		let validators = vec![
			Sr25519Keyring::Alice,
			Sr25519Keyring::Bob,
			Sr25519Keyring::Charlie,
			Sr25519Keyring::Dave,
			Sr25519Keyring::Eve,
		];
		// Add block hash 0x01 and for 0x02
		ChainBuilder::new()
			.add_block(
				block_hash,
				ChainBuilder::GENESIS_HASH,
				1,
				BlockConfig {
					slot: Slot::from(1),
					candidates: Some(vec![(
						candidate_descriptor.clone(),
						CoreIndex(0),
						GroupIndex(0),
					)]),
					session_info: Some(SessionInfo {
						validator_groups: IndexedVec::<GroupIndex, Vec<ValidatorIndex>>::from(
							vec![
								vec![ValidatorIndex(0), ValidatorIndex(1)],
								vec![ValidatorIndex(2)],
								vec![ValidatorIndex(3), ValidatorIndex(4)],
							],
						),
						needed_approvals: 1,
						..session_info(&validators)
					}),
					end_syncing: false,
					approval_voting_params: ApprovalVotingParams::default(),
				},
			)
			.add_block(
				block_hash_fork,
				ChainBuilder::GENESIS_HASH,
				1,
				BlockConfig {
					slot: Slot::from(1),
					candidates: Some(vec![(candidate_descriptor, CoreIndex(0), GroupIndex(0))]),
					session_info: Some(SessionInfo {
						validator_groups: IndexedVec::<GroupIndex, Vec<ValidatorIndex>>::from(
							vec![
								vec![ValidatorIndex(0), ValidatorIndex(1)],
								vec![ValidatorIndex(2)],
								vec![ValidatorIndex(3), ValidatorIndex(4)],
							],
						),
						needed_approvals: 1,
						..session_info(&validators)
					}),
					end_syncing: false,
					approval_voting_params: ApprovalVotingParams::default(),
				},
			)
			.build(&mut virtual_overseer)
			.await;

		// Send assignments for the same candidate on both forks
		let rx =
			import_assignment(&mut virtual_overseer, block_hash, candidate_index, validator, 0)
				.await;
		assert_eq!(rx.await, Ok(AssignmentCheckResult::Accepted));

		let rx = import_assignment(
			&mut virtual_overseer,
			block_hash_fork,
			candidate_index,
			validator,
			0,
		)
		.await;

		assert_eq!(rx.await, Ok(AssignmentCheckResult::Accepted));
		// Wake on APPROVAL_DELAY first
		assert!(clock.inner.lock().current_wakeup_is(2));
		clock.inner.lock().set_tick(2);
		futures_timer::Delay::new(Duration::from_millis(100)).await;

		// Wake up on no-show
		assert!(clock.inner.lock().current_wakeup_is(30));

		for (tick, status) in expected_approval_status
			.iter()
			.filter(|(tick, _)| *tick < tick_to_send_approval)
		{
			// Wake up on no-show
			clock.inner.lock().set_tick(*tick);
			futures_timer::Delay::new(Duration::from_millis(100)).await;
			let block_entry = store.load_block_entry(&block_hash).unwrap().unwrap();
			let block_entry_fork = store.load_block_entry(&block_hash_fork).unwrap().unwrap();
			assert!(!block_entry.is_fully_approved());
			assert_eq!(block_entry_fork.is_fully_approved(), *status);
		}

		clock.inner.lock().set_tick(tick_to_send_approval);
		futures_timer::Delay::new(Duration::from_millis(100)).await;

		// Send the approval for candidate just in the context of 0x01 block.
		let rx = import_approval(
			&mut virtual_overseer,
			block_hash,
			candidate_index,
			validator,
			candidate_hash,
			1,
			false,
			None,
		)
		.await;

		assert_eq!(rx.await, Ok(ApprovalCheckResult::Accepted),);

		// Check approval status for the fork_block is correctly transitioned.
		for (tick, status) in expected_approval_status
			.iter()
			.filter(|(tick, _)| *tick >= tick_to_send_approval)
		{
			// Wake up on no-show
			clock.inner.lock().set_tick(*tick);
			futures_timer::Delay::new(Duration::from_millis(100)).await;
			let block_entry = store.load_block_entry(&block_hash).unwrap().unwrap();
			let block_entry_fork = store.load_block_entry(&block_hash_fork).unwrap().unwrap();
			assert!(block_entry.is_fully_approved());
			assert_eq!(block_entry_fork.is_fully_approved(), *status);
		}

		virtual_overseer
	});
}

#[test]
fn subsystem_process_wakeup_schedules_wakeup() {
	test_harness(HarnessConfig::default(), |test_harness| async move {
		let TestHarness {
			mut virtual_overseer,
			clock,
			sync_oracle_handle: _sync_oracle_handle,
			..
		} = test_harness;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ChainApi(ChainApiMessage::FinalizedBlockNumber(rx)) => {
				rx.send(Ok(0)).unwrap();
			}
		);

		let block_hash = Hash::repeat_byte(0x01);

		let candidate_index = 0;
		let validator = ValidatorIndex(0);

		// Add block hash 0x01...
		ChainBuilder::new()
			.add_block(
				block_hash,
				ChainBuilder::GENESIS_HASH,
				1,
				BlockConfig {
					slot: Slot::from(1),
					candidates: None,
					session_info: None,
					end_syncing: false,
					approval_voting_params: ApprovalVotingParams::default(),
				},
			)
			.build(&mut virtual_overseer)
			.await;

		let rx =
			import_assignment(&mut virtual_overseer, block_hash, candidate_index, validator, 0)
				.await;

		assert_eq!(rx.await, Ok(AssignmentCheckResult::Accepted));

		assert!(clock.inner.lock().current_wakeup_is(2));

		// Activate the wakeup present above, and sleep to allow process_wakeups to execute..
		clock.inner.lock().set_tick(2);
		futures_timer::Delay::new(Duration::from_millis(100)).await;

		// The wakeup should have been rescheduled.
		assert!(clock.inner.lock().current_wakeup_is(30));

		virtual_overseer
	});
}

#[test]
fn linear_import_act_on_leaf() {
	let session = 3u32;

	test_harness(HarnessConfig::default(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, .. } = test_harness;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ChainApi(ChainApiMessage::FinalizedBlockNumber(rx)) => {
				rx.send(Ok(0)).unwrap();
			}
		);

		let mut head: Hash = ChainBuilder::GENESIS_HASH;
		let mut builder = ChainBuilder::new();
		for i in 1..session {
			let slot = Slot::from(i as u64);

			let hash = Hash::repeat_byte(i as u8);
			builder.add_block(hash, head, i, BlockConfig { slot, ..Default::default() });
			head = hash;
		}

		builder.build(&mut virtual_overseer).await;

		let (tx, rx) = oneshot::channel();

		overseer_send(
			&mut virtual_overseer,
			FromOrchestra::Communication {
				msg: ApprovalVotingMessage::ImportAssignment(
					CheckedIndirectAssignment::from_checked(
						IndirectAssignmentCertV2 {
							block_hash: head,
							validator: 0u32.into(),
							cert: garbage_assignment_cert_v2(
								AssignmentCertKindV2::RelayVRFModuloCompact {
									core_bitfield: CoreIndex(0).into(),
								},
							),
						},
						0u32.into(),
						0,
					),
					Some(tx),
				),
			},
		)
		.await;

		assert_eq!(rx.await, Ok(AssignmentCheckResult::Accepted));

		virtual_overseer
	});
}

#[test]
fn forkful_import_at_same_height_act_on_leaf() {
	let session = 3u32;

	test_harness(HarnessConfig::default(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, sync_oracle_handle: _sync_oracle_handle, .. } =
			test_harness;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ChainApi(ChainApiMessage::FinalizedBlockNumber(rx)) => {
				rx.send(Ok(0)).unwrap();
			}
		);

		let mut head: Hash = ChainBuilder::GENESIS_HASH;
		let mut builder = ChainBuilder::new();
		for i in 1..session {
			let slot = Slot::from(i as u64);
			let hash = Hash::repeat_byte(i as u8);
			builder.add_block(hash, head, i, BlockConfig { slot, ..Default::default() });
			head = hash;
		}
		let num_forks = 3;
		let forks = Vec::new();

		for i in 0..num_forks {
			let slot = Slot::from(session as u64);
			let hash = Hash::repeat_byte(session as u8 + i);
			builder.add_block(hash, head, session, BlockConfig { slot, ..Default::default() });
		}
		builder.build(&mut virtual_overseer).await;

		for head in forks.into_iter() {
			let (tx, rx) = oneshot::channel();

			overseer_send(
				&mut virtual_overseer,
				FromOrchestra::Communication {
					msg: ApprovalVotingMessage::ImportAssignment(
						CheckedIndirectAssignment::from_checked(
							IndirectAssignmentCertV2 {
								block_hash: head,
								validator: 0u32.into(),
								cert: garbage_assignment_cert_v2(
									AssignmentCertKindV2::RelayVRFModuloCompact {
										core_bitfield: CoreIndex(0).into(),
									},
								),
							},
							0u32.into(),
							0,
						),
						Some(tx),
					),
				},
			)
			.await;

			assert_eq!(rx.await, Ok(AssignmentCheckResult::Accepted));
		}
		virtual_overseer
	});
}

#[test]
fn test_signing_a_single_candidate_is_backwards_compatible() {
	let session_index = 1;
	let block_hash = Hash::repeat_byte(0x01);
	let candidate_descriptors = (1..10)
		.into_iter()
		.map(|val| make_candidate(ParaId::from(val as u32), &block_hash))
		.collect::<Vec<CandidateReceipt>>();

	let candidate_hashes = candidate_descriptors
		.iter()
		.map(|candidate_descriptor| candidate_descriptor.hash())
		.collect_vec();

	let first_descriptor = candidate_descriptors.first().unwrap();

	let candidate_hash = first_descriptor.hash();

	let sig_a = sign_approval(Sr25519Keyring::Alice, candidate_hash, session_index);

	let sig_b = sign_approval(Sr25519Keyring::Alice, candidate_hash, session_index);

	assert!(DisputeStatement::Valid(ValidDisputeStatementKind::ApprovalChecking)
		.check_signature(
			&Sr25519Keyring::Alice.public().into(),
			candidate_hash,
			session_index,
			&sig_a,
		)
		.is_ok());

	assert!(DisputeStatement::Valid(ValidDisputeStatementKind::ApprovalChecking)
		.check_signature(
			&Sr25519Keyring::Alice.public().into(),
			candidate_hash,
			session_index,
			&sig_b,
		)
		.is_ok());

	let sig_c = sign_approval_multiple_candidates(
		Sr25519Keyring::Alice,
		vec![candidate_hash],
		session_index,
	);

	assert!(DisputeStatement::Valid(
		ValidDisputeStatementKind::ApprovalCheckingMultipleCandidates(
			vec![candidate_hash].try_into().expect("within the coalesce bound")
		)
	)
	.check_signature(&Sr25519Keyring::Alice.public().into(), candidate_hash, session_index, &sig_c,)
	.is_ok());

	assert!(DisputeStatement::Valid(ValidDisputeStatementKind::ApprovalChecking)
		.check_signature(
			&Sr25519Keyring::Alice.public().into(),
			candidate_hash,
			session_index,
			&sig_c,
		)
		.is_ok());

	assert!(DisputeStatement::Valid(
		ValidDisputeStatementKind::ApprovalCheckingMultipleCandidates(
			vec![candidate_hash].try_into().expect("within the coalesce bound")
		)
	)
	.check_signature(&Sr25519Keyring::Alice.public().into(), candidate_hash, session_index, &sig_a,)
	.is_ok());

	let sig_all = sign_approval_multiple_candidates(
		Sr25519Keyring::Alice,
		candidate_hashes.clone(),
		session_index,
	);

	assert!(DisputeStatement::Valid(
		ValidDisputeStatementKind::ApprovalCheckingMultipleCandidates(
			candidate_hashes.clone().try_into().expect("within the coalesce bound")
		)
	)
	.check_signature(
		&Sr25519Keyring::Alice.public().into(),
		*candidate_hashes.first().expect("test"),
		session_index,
		&sig_all,
	)
	.is_ok());
}

#[test]
fn import_checked_approval_updates_entries_and_schedules() {
	let config = HarnessConfig::default();
	let store = config.backend();
	test_harness(config, |test_harness| async move {
		let TestHarness {
			mut virtual_overseer,
			clock,
			sync_oracle_handle: _sync_oracle_handle,
			..
		} = test_harness;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ChainApi(ChainApiMessage::FinalizedBlockNumber(rx)) => {
				rx.send(Ok(0)).unwrap();
			}
		);

		let block_hash = Hash::repeat_byte(0x01);
		let validator_index_a = ValidatorIndex(0);
		let validator_index_b = ValidatorIndex(1);

		let validators = vec![
			Sr25519Keyring::Alice,
			Sr25519Keyring::Bob,
			Sr25519Keyring::Charlie,
			Sr25519Keyring::Dave,
			Sr25519Keyring::Eve,
		];
		let session_info = SessionInfo {
			validator_groups: IndexedVec::<GroupIndex, Vec<ValidatorIndex>>::from(vec![
				vec![ValidatorIndex(0), ValidatorIndex(1)],
				vec![ValidatorIndex(2)],
				vec![ValidatorIndex(3), ValidatorIndex(4)],
			]),
			..session_info(&validators)
		};

		let candidate_descriptor = make_candidate(ParaId::from(1_u32), &block_hash);
		let candidate_hash = candidate_descriptor.hash();

		let head: Hash = ChainBuilder::GENESIS_HASH;
		let mut builder = ChainBuilder::new();
		let slot = Slot::from(1 as u64);
		builder.add_block(
			block_hash,
			head,
			1,
			BlockConfig {
				slot,
				candidates: Some(vec![(candidate_descriptor, CoreIndex(0), GroupIndex(0))]),
				session_info: Some(session_info),
				end_syncing: false,
				approval_voting_params: ApprovalVotingParams::default(),
			},
		);
		builder.build(&mut virtual_overseer).await;

		let candidate_index = 0;

		let rx = import_assignment(
			&mut virtual_overseer,
			block_hash,
			candidate_index,
			validator_index_a,
			0,
		)
		.await;

		assert_eq!(rx.await, Ok(AssignmentCheckResult::Accepted),);

		let rx = import_assignment(
			&mut virtual_overseer,
			block_hash,
			candidate_index,
			validator_index_b,
			0,
		)
		.await;

		assert_eq!(rx.await, Ok(AssignmentCheckResult::Accepted),);

		let session_index = 1;
		let sig_a = sign_approval(Sr25519Keyring::Alice, candidate_hash, session_index);

		let rx = import_approval(
			&mut virtual_overseer,
			block_hash,
			candidate_index,
			validator_index_a,
			candidate_hash,
			session_index,
			false,
			Some(sig_a),
		)
		.await;

		assert_eq!(rx.await, Ok(ApprovalCheckResult::Accepted),);

		// Sleep to ensure we get a consistent read on the database.
		futures_timer::Delay::new(Duration::from_millis(100)).await;

		// The candidate should not yet be approved and a wakeup should be scheduled on the first
		// approval.
		let candidate_entry = store.load_candidate_entry(&candidate_hash).unwrap().unwrap();
		assert!(!candidate_entry.approval_entry(&block_hash).unwrap().is_approved());
		assert!(clock.inner.lock().current_wakeup_is(2));

		// Clear the wake ups to assert that later approval also schedule wakeups.
		clock.inner.lock().wakeup_all(2);

		let sig_b = sign_approval(Sr25519Keyring::Bob, candidate_hash, session_index);
		let rx = import_approval(
			&mut virtual_overseer,
			block_hash,
			candidate_index,
			validator_index_b,
			candidate_hash,
			session_index,
			true,
			Some(sig_b),
		)
		.await;

		assert_eq!(rx.await, Ok(ApprovalCheckResult::Accepted),);

		// Sleep to ensure we get a consistent read on the database.
		//
		// NOTE: Since the response above occurs before writing to the database, we are somewhat
		// breaking the external consistency of the API by reaching into the database directly.
		// Under normal operation, this wouldn't be necessary, since all requests are serialized by
		// the event loop and we write at the end of each pass. However, if the database write were
		// to fail, a downstream subsystem may expect for this candidate to be approved, and
		// possibly take further actions on the assumption that the candidate is approved, when
		// that may not be the reality from the database's perspective. This could be avoided
		// entirely by having replies processed after database writes, but that would constitute a
		// larger refactor and incur a performance penalty.
		futures_timer::Delay::new(Duration::from_millis(100)).await;

		// The candidate should now be approved.
		let candidate_entry = store.load_candidate_entry(&candidate_hash).unwrap().unwrap();
		assert!(candidate_entry.approval_entry(&block_hash).unwrap().is_approved());

		virtual_overseer
	});
}

#[test]
fn subsystem_import_checked_approval_sets_one_block_bit_at_a_time() {
	let config = HarnessConfig::default();
	let store = config.backend();
	test_harness(config, |test_harness| async move {
		let TestHarness { mut virtual_overseer, sync_oracle_handle: _sync_oracle_handle, .. } =
			test_harness;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ChainApi(ChainApiMessage::FinalizedBlockNumber(rx)) => {
				rx.send(Ok(0)).unwrap();
			}
		);

		let block_hash = Hash::repeat_byte(0x01);

		let candidate_receipt1 = {
			let mut receipt = dummy_candidate_receipt_v2(block_hash);
			receipt.descriptor.set_para_id(ParaId::from(1_u32));
			receipt
		};
		let candidate_receipt2 = {
			let mut receipt = dummy_candidate_receipt_v2(block_hash);
			receipt.descriptor.set_para_id(ParaId::from(2_u32));
			receipt
		};
		let candidate_hash1 = candidate_receipt1.hash();
		let candidate_hash2 = candidate_receipt2.hash();
		let candidate_index1 = 0;
		let candidate_index2 = 1;

		let validator1 = ValidatorIndex(0);
		let validator2 = ValidatorIndex(1);
		let session_index = 1;

		let validators = vec![
			Sr25519Keyring::Alice,
			Sr25519Keyring::Bob,
			Sr25519Keyring::Charlie,
			Sr25519Keyring::Dave,
			Sr25519Keyring::Eve,
		];
		let session_info = SessionInfo {
			validator_groups: IndexedVec::<GroupIndex, Vec<ValidatorIndex>>::from(vec![
				vec![ValidatorIndex(0), ValidatorIndex(1)],
				vec![ValidatorIndex(2)],
				vec![ValidatorIndex(3), ValidatorIndex(4)],
			]),
			..session_info(&validators)
		};

		ChainBuilder::new()
			.add_block(
				block_hash,
				ChainBuilder::GENESIS_HASH,
				1,
				BlockConfig {
					slot: Slot::from(0),
					candidates: Some(vec![
						(candidate_receipt1, CoreIndex(1), GroupIndex(1)),
						(candidate_receipt2, CoreIndex(1), GroupIndex(1)),
					]),
					session_info: Some(session_info),
					end_syncing: false,
					approval_voting_params: ApprovalVotingParams::default(),
				},
			)
			.build(&mut virtual_overseer)
			.await;

		let assignments = vec![
			(candidate_index1, validator1),
			(candidate_index2, validator1),
			(candidate_index1, validator2),
			(candidate_index2, validator2),
		];

		for (candidate_index, validator) in assignments {
			let rx =
				import_assignment(&mut virtual_overseer, block_hash, candidate_index, validator, 0)
					.await;
			assert_eq!(rx.await, Ok(AssignmentCheckResult::Accepted));
		}

		let approvals = vec![
			(candidate_index1, validator1, candidate_hash1),
			(candidate_index1, validator2, candidate_hash1),
			(candidate_index2, validator1, candidate_hash2),
			(candidate_index2, validator2, candidate_hash2),
		];

		for (i, (candidate_index, validator, candidate_hash)) in approvals.iter().enumerate() {
			let expect_candidate1_approved = i >= 1;
			let expect_candidate2_approved = i >= 3;
			let expect_block_approved = expect_candidate2_approved;

			let signature = if *validator == validator1 {
				sign_approval(Sr25519Keyring::Alice, *candidate_hash, session_index)
			} else {
				sign_approval(Sr25519Keyring::Bob, *candidate_hash, session_index)
			};
			let rx = import_approval(
				&mut virtual_overseer,
				block_hash,
				*candidate_index,
				*validator,
				*candidate_hash,
				session_index,
				expect_block_approved,
				Some(signature),
			)
			.await;
			assert_eq!(rx.await, Ok(ApprovalCheckResult::Accepted));

			// Sleep to get a consistent read on the database.
			futures_timer::Delay::new(Duration::from_millis(200)).await;

			let block_entry = store.load_block_entry(&block_hash).unwrap().unwrap();
			assert_eq!(block_entry.is_fully_approved(), expect_block_approved);
			assert_eq!(
				block_entry.is_candidate_approved(&candidate_hash1),
				expect_candidate1_approved
			);
			assert_eq!(
				block_entry.is_candidate_approved(&candidate_hash2),
				expect_candidate2_approved
			);
		}

		virtual_overseer
	});
}

// See https://github.com/paritytech/polkadot-sdk/issues/3826
#[test]
fn inclusion_events_can_be_unordered_by_core_index() {
	let assignment_criteria = Box::new(MockAssignmentCriteria(
		|| {
			let mut assignments = HashMap::new();
			for core in 0..3 {
				let _ = assignments.insert(
					CoreIndex(core),
					approval_db::v2::OurAssignment {
						cert: garbage_assignment_cert_v2(
							AssignmentCertKindV2::RelayVRFModuloCompact {
								core_bitfield: vec![CoreIndex(0), CoreIndex(1), CoreIndex(2)]
									.try_into()
									.unwrap(),
							},
						),
						tranche: 0,
						validator_index: ValidatorIndex(0),
						triggered: false,
					}
					.into(),
				);
			}
			assignments
		},
		|_| Ok(0),
	));
	let config = HarnessConfigBuilder::default().assignment_criteria(assignment_criteria).build();
	let store = config.backend();

	test_harness(config, |test_harness| async move {
		let TestHarness {
			mut virtual_overseer,
			clock,
			sync_oracle_handle: _sync_oracle_handle,
			..
		} = test_harness;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ChainApi(ChainApiMessage::FinalizedBlockNumber(rx)) => {
				rx.send(Ok(0)).unwrap();
			}
		);

		let block_hash = Hash::repeat_byte(0x01);

		let candidate_receipt0 = {
			let mut receipt = dummy_candidate_receipt_v2(block_hash);
			receipt.descriptor.set_para_id(ParaId::from(0_u32));
			receipt
		};
		let candidate_receipt1 = {
			let mut receipt = dummy_candidate_receipt_v2(block_hash);
			receipt.descriptor.set_para_id(ParaId::from(1_u32));
			receipt
		};
		let candidate_receipt2 = {
			let mut receipt = dummy_candidate_receipt_v2(block_hash);
			receipt.descriptor.set_para_id(ParaId::from(2_u32));
			receipt
		};
		let candidate_index0 = 0;
		let candidate_index1 = 1;
		let candidate_index2 = 2;

		let validator0 = ValidatorIndex(0);
		let validator1 = ValidatorIndex(1);
		let validator2 = ValidatorIndex(2);
		let validator3 = ValidatorIndex(3);

		let validators = vec![
			Sr25519Keyring::Alice,
			Sr25519Keyring::Bob,
			Sr25519Keyring::Charlie,
			Sr25519Keyring::Dave,
			Sr25519Keyring::Eve,
		];
		let session_info = SessionInfo {
			validator_groups: IndexedVec::<GroupIndex, Vec<ValidatorIndex>>::from(vec![
				vec![validator0, validator1],
				vec![validator2],
				vec![validator3],
			]),
			needed_approvals: 1,
			zeroth_delay_tranche_width: 1,
			relay_vrf_modulo_samples: 1,
			n_delay_tranches: 1,
			no_show_slots: 1,
			..session_info(&validators)
		};

		ChainBuilder::new()
			.add_block(
				block_hash,
				ChainBuilder::GENESIS_HASH,
				1,
				BlockConfig {
					slot: Slot::from(0),
					candidates: Some(vec![
						(candidate_receipt0.clone(), CoreIndex(2), GroupIndex(2)),
						(candidate_receipt1.clone(), CoreIndex(1), GroupIndex(0)),
						(candidate_receipt2.clone(), CoreIndex(0), GroupIndex(1)),
					]),
					session_info: Some(session_info),
					end_syncing: true,
					approval_voting_params: ApprovalVotingParams::default(),
				},
			)
			.build(&mut virtual_overseer)
			.await;

		assert_eq!(clock.inner.lock().next_wakeup().unwrap(), 2);
		clock.inner.lock().wakeup_all(100);

		assert_eq!(clock.inner.lock().wakeups.len(), 0);

		futures_timer::Delay::new(Duration::from_millis(100)).await;

		// Assignment is distributed only once from `approval-voting`
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ApprovalDistribution(ApprovalDistributionMessage::DistributeAssignment(
				_,
				c_indices,
			)) => {
				assert_eq!(c_indices, vec![candidate_index0, candidate_index1, candidate_index2].try_into().unwrap());
			}
		);

		// Candidate 0
		recover_available_data(&mut virtual_overseer).await;
		fetch_validation_code(&mut virtual_overseer).await;

		// Candidate 1
		recover_available_data(&mut virtual_overseer).await;
		fetch_validation_code(&mut virtual_overseer).await;

		// Candidate 2
		recover_available_data(&mut virtual_overseer).await;
		fetch_validation_code(&mut virtual_overseer).await;

		// Check if assignment was triggered for candidate 0.
		let candidate_entry =
			store.load_candidate_entry(&candidate_receipt0.hash()).unwrap().unwrap();
		let our_assignment =
			candidate_entry.approval_entry(&block_hash).unwrap().our_assignment().unwrap();
		assert!(our_assignment.triggered());

		// Check if assignment was triggered for candidate 1.
		let candidate_entry =
			store.load_candidate_entry(&candidate_receipt1.hash()).unwrap().unwrap();
		let our_assignment =
			candidate_entry.approval_entry(&block_hash).unwrap().our_assignment().unwrap();
		assert!(our_assignment.triggered());

		// Check if assignment was triggered for candidate 2.
		let candidate_entry =
			store.load_candidate_entry(&candidate_receipt2.hash()).unwrap().unwrap();
		let our_assignment =
			candidate_entry.approval_entry(&block_hash).unwrap().our_assignment().unwrap();
		assert!(our_assignment.triggered());

		virtual_overseer
	});
}

fn approved_ancestor_test(
	skip_approval: impl Fn(BlockNumber) -> bool,
	approved_height: BlockNumber,
) {
	test_harness(HarnessConfig::default(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, sync_oracle_handle: _sync_oracle_handle, .. } =
			test_harness;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ChainApi(ChainApiMessage::FinalizedBlockNumber(rx)) => {
				rx.send(Ok(0)).unwrap();
			}
		);

		let block_hashes = vec![
			Hash::repeat_byte(0x01),
			Hash::repeat_byte(0x02),
			Hash::repeat_byte(0x03),
			Hash::repeat_byte(0x04),
		];

		let candidate_receipts: Vec<_> = block_hashes
			.iter()
			.enumerate()
			.map(|(i, hash)| {
				let mut candidate_receipt = dummy_candidate_receipt_v2(*hash);
				candidate_receipt.descriptor.set_para_id(i.into());
				candidate_receipt
			})
			.collect();

		let candidate_hashes: Vec<_> = candidate_receipts.iter().map(|r| r.hash()).collect();

		let candidate_index = 0;
		let validator = ValidatorIndex(0);

		let mut builder = ChainBuilder::new();
		for (i, (block_hash, candidate_receipt)) in
			block_hashes.iter().zip(candidate_receipts).enumerate()
		{
			let parent_hash = if i == 0 { ChainBuilder::GENESIS_HASH } else { block_hashes[i - 1] };
			builder.add_block(
				*block_hash,
				parent_hash,
				i as u32 + 1,
				BlockConfig {
					slot: Slot::from(i as u64),
					candidates: Some(vec![(candidate_receipt, CoreIndex(0), GroupIndex(0))]),
					session_info: None,
					end_syncing: false,
					approval_voting_params: ApprovalVotingParams::default(),
				},
			);
		}
		builder.build(&mut virtual_overseer).await;

		for (i, (block_hash, candidate_hash)) in
			block_hashes.iter().zip(candidate_hashes).enumerate()
		{
			let rx = import_assignment(
				&mut virtual_overseer,
				*block_hash,
				candidate_index,
				validator,
				0,
			)
			.await;
			assert_eq!(rx.await, Ok(AssignmentCheckResult::Accepted));

			if skip_approval(i as BlockNumber + 1) {
				continue;
			}

			let rx = import_approval(
				&mut virtual_overseer,
				*block_hash,
				candidate_index,
				validator,
				candidate_hash,
				i as u32 + 1,
				true,
				None,
			)
			.await;
			assert_eq!(rx.await, Ok(ApprovalCheckResult::Accepted));
		}

		let target = block_hashes[block_hashes.len() - 1];
		let block_number = block_hashes.len();

		let (tx, rx) = oneshot::channel();
		overseer_send(
			&mut virtual_overseer,
			FromOrchestra::Communication {
				msg: ApprovalVotingMessage::ApprovedAncestor(target, 0, tx),
			},
		)
		.await;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ChainApi(ChainApiMessage::BlockNumber(hash, tx)) => {
				assert_eq!(target, hash);
				tx.send(Ok(Some(block_number as BlockNumber))).unwrap();
			}
		);

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ChainApi(ChainApiMessage::Ancestors {
				hash,
				k,
				response_channel: tx,
			}) => {
				assert_eq!(target, hash);
				assert_eq!(k, block_number - (0 + 1));
				let ancestors = block_hashes.iter()
					.take(block_number-1)
					.rev()
					.cloned()
					.collect::<Vec<Hash>>();
				tx.send(Ok(ancestors)).unwrap();
			}
		);

		let approved_hash = block_hashes[approved_height as usize - 1];
		let HighestApprovedAncestorBlock { hash, number, .. } = rx.await.unwrap().unwrap();
		assert_eq!(approved_hash, hash);
		assert_eq!(number, approved_height);

		virtual_overseer
	});
}

#[test]
fn subsystem_approved_ancestor_all_approved() {
	// Don't skip any approvals, highest approved ancestor should be 4.
	approved_ancestor_test(|_| false, 4);
}

#[test]
fn subsystem_approved_ancestor_missing_approval() {
	// Skip approval for the third block, highest approved ancestor should be 2.
	approved_ancestor_test(|i| i == 3, 2);
}

#[test]
fn subsystem_validate_approvals_cache() {
	let assignment_criteria = Box::new(MockAssignmentCriteria(
		|| {
			let mut assignments = HashMap::new();
			let _ = assignments.insert(
				CoreIndex(0),
				approval_db::v2::OurAssignment {
					cert: garbage_assignment_cert_v2(AssignmentCertKindV2::RelayVRFModuloCompact {
						core_bitfield: CoreIndex(0).into(),
					}),
					tranche: 0,
					validator_index: ValidatorIndex(0),
					triggered: false,
				}
				.into(),
			);

			let _ = assignments.insert(
				CoreIndex(0),
				approval_db::v2::OurAssignment {
					cert: garbage_assignment_cert_v2(AssignmentCertKindV2::RelayVRFModuloCompact {
						core_bitfield: vec![CoreIndex(0), CoreIndex(1), CoreIndex(2)]
							.try_into()
							.unwrap(),
					}),
					tranche: 0,
					validator_index: ValidatorIndex(0),
					triggered: false,
				}
				.into(),
			);
			assignments
		},
		|_| Ok(0),
	));
	let config = HarnessConfigBuilder::default().assignment_criteria(assignment_criteria).build();
	let store = config.backend();

	test_harness(config, |test_harness| async move {
		let TestHarness { mut virtual_overseer, clock, sync_oracle_handle: _sync_oracle_handle } =
			test_harness;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ChainApi(ChainApiMessage::FinalizedBlockNumber(rx)) => {
				rx.send(Ok(0)).unwrap();
			}
		);

		let block_hash = Hash::repeat_byte(0x01);
		let fork_block_hash = Hash::repeat_byte(0x02);
		let candidate_commitments = CandidateCommitments::default();
		let mut candidate_receipt = dummy_candidate_receipt_v2(block_hash);
		candidate_receipt.commitments_hash = candidate_commitments.hash();
		let candidate_hash = candidate_receipt.hash();
		let slot = Slot::from(1);
		let candidate_index = 0;

		let validators = vec![
			Sr25519Keyring::Alice,
			Sr25519Keyring::Bob,
			Sr25519Keyring::Charlie,
			Sr25519Keyring::Dave,
			Sr25519Keyring::Eve,
		];
		let session_info = SessionInfo {
			validator_groups: IndexedVec::<GroupIndex, Vec<ValidatorIndex>>::from(vec![
				vec![ValidatorIndex(0), ValidatorIndex(1)],
				vec![ValidatorIndex(2)],
				vec![ValidatorIndex(3), ValidatorIndex(4)],
			]),
			..session_info(&validators)
		};

		let candidates = Some(vec![(candidate_receipt.clone(), CoreIndex(0), GroupIndex(0))]);
		ChainBuilder::new()
			.add_block(
				block_hash,
				ChainBuilder::GENESIS_HASH,
				1,
				BlockConfig {
					slot,
					candidates: candidates.clone(),
					session_info: Some(session_info.clone()),
					end_syncing: false,
					approval_voting_params: ApprovalVotingParams::default(),
				},
			)
			.add_block(
				fork_block_hash,
				ChainBuilder::GENESIS_HASH,
				1,
				BlockConfig {
					slot,
					candidates,
					session_info: Some(session_info),
					end_syncing: false,
					approval_voting_params: ApprovalVotingParams::default(),
				},
			)
			.build(&mut virtual_overseer)
			.await;

		assert!(!clock.inner.lock().current_wakeup_is(1));
		clock.inner.lock().wakeup_all(1);

		assert!(clock.inner.lock().current_wakeup_is(slot_to_tick(slot)));
		clock.inner.lock().wakeup_all(slot_to_tick(slot));

		futures_timer::Delay::new(Duration::from_millis(200)).await;

		clock.inner.lock().wakeup_all(slot_to_tick(slot + 2));

		assert_eq!(clock.inner.lock().wakeups.len(), 0);

		futures_timer::Delay::new(Duration::from_millis(200)).await;

		let candidate_entry = store.load_candidate_entry(&candidate_hash).unwrap().unwrap();
		let our_assignment =
			candidate_entry.approval_entry(&block_hash).unwrap().our_assignment().unwrap();
		assert!(our_assignment.triggered());

		// Handle the the next two assignment imports, where only one should trigger approvals work
		handle_double_assignment_import(&mut virtual_overseer, candidate_index).await;

		virtual_overseer
	});
}

#[test]
fn subsystem_doesnt_distribute_duplicate_compact_assignments() {
	let assignment_criteria = Box::new(MockAssignmentCriteria(
		|| {
			let mut assignments = HashMap::new();
			let cert = garbage_assignment_cert_v2(AssignmentCertKindV2::RelayVRFModuloCompact {
				core_bitfield: vec![CoreIndex(0), CoreIndex(1)].try_into().unwrap(),
			});

			let _ = assignments.insert(
				CoreIndex(0),
				approval_db::v2::OurAssignment {
					cert: cert.clone(),
					tranche: 0,
					validator_index: ValidatorIndex(0),
					triggered: false,
				}
				.into(),
			);

			let _ = assignments.insert(
				CoreIndex(1),
				approval_db::v2::OurAssignment {
					cert,
					tranche: 0,
					validator_index: ValidatorIndex(0),
					triggered: false,
				}
				.into(),
			);
			assignments
		},
		|_| Ok(0),
	));

	let config = HarnessConfigBuilder::default().assignment_criteria(assignment_criteria).build();
	let store = config.backend();

	test_harness(config, |test_harness| async move {
		let TestHarness {
			mut virtual_overseer,
			sync_oracle_handle: _sync_oracle_handle,
			clock,
			..
		} = test_harness;
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ChainApi(ChainApiMessage::FinalizedBlockNumber(rx)) => {
				rx.send(Ok(0)).unwrap();
			}
		);

		let block_hash = Hash::repeat_byte(0x01);

		let candidate_receipt1 = {
			let mut receipt = dummy_candidate_receipt_v2(block_hash);
			receipt.descriptor.set_para_id(ParaId::from(1_u32));
			receipt
		};
		let candidate_receipt2 = {
			let mut receipt = dummy_candidate_receipt_v2(block_hash);
			receipt.descriptor.set_para_id(ParaId::from(2_u32));
			receipt
		};
		let candidate_index1 = 0;
		let candidate_index2 = 1;

		// Add block hash 00.
		ChainBuilder::new()
			.add_block(
				block_hash,
				ChainBuilder::GENESIS_HASH,
				1,
				BlockConfig {
					slot: Slot::from(0),
					candidates: Some(vec![
						(candidate_receipt1.clone(), CoreIndex(0), GroupIndex(1)),
						(candidate_receipt2.clone(), CoreIndex(1), GroupIndex(1)),
					]),
					session_info: None,
					end_syncing: false,
					approval_voting_params: ApprovalVotingParams::default(),
				},
			)
			.build(&mut virtual_overseer)
			.await;

		// Activate the wakeup present above, and sleep to allow process_wakeups to execute..
		assert_eq!(Some(2), clock.inner.lock().next_wakeup());
		gum::trace!("clock \n{:?}\n", clock.inner.lock());

		clock.inner.lock().wakeup_all(100);

		assert_eq!(clock.inner.lock().wakeups.len(), 0);

		futures_timer::Delay::new(Duration::from_millis(100)).await;

		// Assignment is distributed only once from `approval-voting`
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ApprovalDistribution(ApprovalDistributionMessage::DistributeAssignment(
				_,
				c_indices,
			)) => {
				assert_eq!(c_indices, vec![candidate_index1, candidate_index2].try_into().unwrap());
			}
		);

		// Candidate 1
		recover_available_data(&mut virtual_overseer).await;
		fetch_validation_code(&mut virtual_overseer).await;

		// Candidate 2
		recover_available_data(&mut virtual_overseer).await;
		fetch_validation_code(&mut virtual_overseer).await;

		// Check if assignment was triggered for candidate 1.
		let candidate_entry =
			store.load_candidate_entry(&candidate_receipt1.hash()).unwrap().unwrap();
		let our_assignment =
			candidate_entry.approval_entry(&block_hash).unwrap().our_assignment().unwrap();
		assert!(our_assignment.triggered());

		// Check if assignment was triggered for candidate 2.
		let candidate_entry =
			store.load_candidate_entry(&candidate_receipt2.hash()).unwrap().unwrap();
		let our_assignment =
			candidate_entry.approval_entry(&block_hash).unwrap().our_assignment().unwrap();
		assert!(our_assignment.triggered());
		virtual_overseer
	});
}

/// Ensure that when two assignments are imported, only one triggers the Approval Checking work
async fn handle_double_assignment_import(
	virtual_overseer: &mut VirtualOverseer,
	candidate_index: CandidateIndex,
) {
	assert_matches!(
		overseer_recv(virtual_overseer).await,
		AllMessages::ApprovalDistribution(ApprovalDistributionMessage::DistributeAssignment(
			_,
			c_indices,
		)) => {
			assert_eq!(Into::<CandidateBitfield>::into(candidate_index), c_indices);
		}
	);

	recover_available_data(virtual_overseer).await;
	fetch_validation_code(virtual_overseer).await;

	assert_matches!(
		overseer_recv(virtual_overseer).await,
		AllMessages::ApprovalDistribution(ApprovalDistributionMessage::DistributeAssignment(
			_,
			c_index
		)) => {
			assert_eq!(Into::<CandidateBitfield>::into(candidate_index), c_index);
		}
	);

	assert_matches!(
		overseer_recv(virtual_overseer).await,
		AllMessages::CandidateValidation(CandidateValidationMessage::ValidateFromExhaustive {
			exec_kind,
			response_sender,
			..
		}) if exec_kind == PvfExecKind::Approval => {
			response_sender.send(Ok(ValidationResult::Valid(Default::default(), Default::default())))
				.unwrap();
		}
	);

	assert_matches!(
		overseer_recv(virtual_overseer).await,
		AllMessages::ApprovalDistribution(ApprovalDistributionMessage::DistributeApproval(_))
	);

	assert_matches!(
		overseer_recv(virtual_overseer).await,
		AllMessages::ApprovalDistribution(ApprovalDistributionMessage::DistributeApproval(_))
	);
	// Assert that there are no more messages being sent by the subsystem
	assert!(overseer_recv(virtual_overseer).timeout(TIMEOUT / 2).await.is_none());
}

/// Handles validation code fetch, returns the received relay parent hash.
async fn fetch_validation_code(virtual_overseer: &mut VirtualOverseer) -> Hash {
	let validation_code = ValidationCode(Vec::new());

	assert_matches!(
		virtual_overseer.recv().await,
		AllMessages::RuntimeApi(RuntimeApiMessage::Request(
			hash,
			RuntimeApiRequest::ValidationCodeByHash(
				_,
				tx,
			)
		)) => {
			tx.send(Ok(Some(validation_code))).unwrap();
			hash
		},
		"overseer did not receive runtime API request for validation code",
	)
}

async fn recover_available_data(virtual_overseer: &mut VirtualOverseer) {
	let pov_block = PoV { block_data: BlockData(Vec::new()) };

	let available_data =
		AvailableData { pov: Arc::new(pov_block), validation_data: Default::default() };

	assert_matches!(
		virtual_overseer.recv().await,
		AllMessages::AvailabilityRecovery(
			AvailabilityRecoveryMessage::RecoverAvailableData(_, _, _, _, tx)
		) => {
			tx.send(Ok(available_data)).unwrap();
		},
		"overseer did not receive recover available data message",
	);
}

async fn recover_available_data_failure(virtual_overseer: &mut VirtualOverseer) {
	let available_data = RecoveryError::Unavailable;

	assert_matches!(
		virtual_overseer.recv().await,
		AllMessages::AvailabilityRecovery(
			AvailabilityRecoveryMessage::RecoverAvailableData(_, _, _, _, tx)
		) => {
			tx.send(Err(available_data)).unwrap();
		},
		"overseer did not receive recover available data message",
	);
}

struct TriggersAssignmentConfig<F1, F2> {
	our_assigned_tranche: DelayTranche,
	assign_validator_tranche: F1,
	no_show_slots: u32,
	assignments_to_import: Vec<u32>,
	approvals_to_import: Vec<u32>,
	ticks: Vec<Tick>,
	should_be_triggered: F2,
}

fn triggers_assignment_test<F1, F2>(config: TriggersAssignmentConfig<F1, F2>)
where
	F1: 'static
		+ Fn(ValidatorIndex) -> Result<DelayTranche, criteria::InvalidAssignment>
		+ Send
		+ Sync
		+ Clone,
	F2: Fn(Tick) -> bool,
{
	let TriggersAssignmentConfig {
		our_assigned_tranche,
		assign_validator_tranche,
		no_show_slots,
		assignments_to_import,
		approvals_to_import,
		ticks,
		should_be_triggered,
	} = config;

	let assignment_criteria = Box::new(MockAssignmentCriteria(
		move || {
			let mut assignments = HashMap::new();
			let _ = assignments.insert(
				CoreIndex(0),
				approval_db::v2::OurAssignment {
					cert: garbage_assignment_cert_v2(AssignmentCertKindV2::RelayVRFModuloCompact {
						core_bitfield: CoreIndex(0).into(),
					}),
					tranche: our_assigned_tranche,
					validator_index: ValidatorIndex(0),
					triggered: false,
				}
				.into(),
			);
			assignments
		},
		assign_validator_tranche.clone(),
	));
	let config = HarnessConfigBuilder::default().assignment_criteria(assignment_criteria).build();
	let store = config.backend();

	test_harness(config, |test_harness| async move {
		let TestHarness {
			mut virtual_overseer,
			clock,
			sync_oracle_handle: _sync_oracle_handle,
			..
		} = test_harness;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ChainApi(ChainApiMessage::FinalizedBlockNumber(rx)) => {
				rx.send(Ok(0)).unwrap();
			}
		);

		let block_hash = Hash::repeat_byte(0x01);
		let candidate_receipt = dummy_candidate_receipt_v2(block_hash);
		let candidate_hash = candidate_receipt.hash();
		let slot = Slot::from(1);
		let candidate_index = 0;

		let validators = vec![
			Sr25519Keyring::Alice,
			Sr25519Keyring::Bob,
			Sr25519Keyring::Charlie,
			Sr25519Keyring::Dave,
			Sr25519Keyring::Eve,
			Sr25519Keyring::Ferdie,
		];
		let session_info = SessionInfo {
			validator_groups: IndexedVec::<GroupIndex, Vec<ValidatorIndex>>::from(vec![
				vec![ValidatorIndex(0), ValidatorIndex(1)],
				vec![ValidatorIndex(2), ValidatorIndex(3)],
				vec![ValidatorIndex(4), ValidatorIndex(5)],
			]),
			relay_vrf_modulo_samples: 2,
			no_show_slots,
			..session_info(&validators)
		};

		ChainBuilder::new()
			.add_block(
				block_hash,
				ChainBuilder::GENESIS_HASH,
				1,
				BlockConfig {
					slot,
					candidates: Some(vec![(candidate_receipt, CoreIndex(0), GroupIndex(2))]),
					session_info: Some(session_info),
					end_syncing: false,
					approval_voting_params: ApprovalVotingParams::default(),
				},
			)
			.build(&mut virtual_overseer)
			.await;

		for validator in assignments_to_import {
			let rx = import_assignment(
				&mut virtual_overseer,
				block_hash,
				candidate_index,
				ValidatorIndex(validator),
				assign_validator_tranche(ValidatorIndex(validator)).unwrap(),
			)
			.await;
			assert_eq!(rx.await, Ok(AssignmentCheckResult::Accepted));
		}

		let n_validators = validators.len();
		for (i, &validator_index) in approvals_to_import.iter().enumerate() {
			let expect_chain_approved = 3 * (i + 1) > n_validators;
			let rx = import_approval(
				&mut virtual_overseer,
				block_hash,
				candidate_index,
				ValidatorIndex(validator_index),
				candidate_hash,
				1,
				expect_chain_approved,
				Some(sign_approval(validators[validator_index as usize], candidate_hash, 1)),
			)
			.await;
			assert_eq!(rx.await, Ok(ApprovalCheckResult::Accepted));
		}

		let debug = false;
		if debug {
			step_until_done(&clock).await;
			return virtual_overseer;
		}

		futures_timer::Delay::new(Duration::from_millis(200)).await;

		for tick in ticks {
			// Assert that this tick is the next to wake up, requiring the test harness to encode
			// all relevant wakeups sequentially.
			assert_eq!(Some(tick), clock.inner.lock().next_wakeup());

			clock.inner.lock().set_tick(tick);
			futures_timer::Delay::new(Duration::from_millis(100)).await;

			match virtual_overseer.peek().timeout(Duration::from_millis(50)).await.flatten() {
				Some(AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					_,
					RuntimeApiRequest::SessionIndexForChild(_),
				))) => {
					let AllMessages::RuntimeApi(RuntimeApiMessage::Request(
						_,
						RuntimeApiRequest::SessionIndexForChild(rx),
					)) = virtual_overseer.recv().await
					else {
						panic!("Unexpected message");
					};
					rx.send(Ok(1u32.into())).unwrap();
					futures_timer::Delay::new(Duration::from_millis(100)).await;
				},
				_ => (),
			}

			// Assert that Alice's assignment is triggered at the correct tick.
			let candidate_entry = store.load_candidate_entry(&candidate_hash).unwrap().unwrap();
			let our_assignment =
				candidate_entry.approval_entry(&block_hash).unwrap().our_assignment().unwrap();
			assert_eq!(our_assignment.triggered(), should_be_triggered(tick), "at tick {:?}", tick);
		}

		virtual_overseer
	});
}

// This method is used to generate a trace for an execution of a triggers_assignment_test given a
// starting configuration. The relevant ticks (all scheduled wakeups) are printed after no further
// ticks are scheduled. To create a valid test, a prefix of the relevant ticks should be included
// in the final test configuration, ending at the tick with the desired inputs to
// should_trigger_assignment.
async fn step_until_done(clock: &MockClock) {
	let mut relevant_ticks = Vec::new();
	loop {
		futures_timer::Delay::new(Duration::from_millis(200)).await;
		let mut clock = clock.inner.lock();
		if let Some(tick) = clock.next_wakeup() {
			relevant_ticks.push(tick);
			clock.set_tick(tick);
		} else {
			break;
		}
	}
}

#[test]
fn subsystem_process_wakeup_trigger_assignment_launch_approval() {
	triggers_assignment_test(TriggersAssignmentConfig {
		our_assigned_tranche: 0,
		assign_validator_tranche: |_| Ok(0),
		no_show_slots: 0,
		assignments_to_import: vec![1],
		approvals_to_import: vec![1],
		ticks: vec![
			10, // Alice wakeup, assignment triggered
		],
		should_be_triggered: |_| true,
	});
}

#[test]
fn subsystem_assignment_triggered_solo_zero_tranche() {
	triggers_assignment_test(TriggersAssignmentConfig {
		our_assigned_tranche: 0,
		assign_validator_tranche: |_| Ok(0),
		no_show_slots: 2,
		assignments_to_import: vec![],
		approvals_to_import: vec![],
		ticks: vec![
			10, // Alice wakeup, assignment triggered
		],
		should_be_triggered: |_| true,
	});
}

#[test]
fn subsystem_assignment_triggered_by_all_with_less_than_threshold() {
	triggers_assignment_test(TriggersAssignmentConfig {
		our_assigned_tranche: 11,
		assign_validator_tranche: |_| Ok(0),
		no_show_slots: 2,
		assignments_to_import: vec![1, 2, 3, 4, 5],
		approvals_to_import: vec![2, 4],
		ticks: vec![
			2,  // APPROVAL_DELAY
			21, // Check for no shows
		],
		should_be_triggered: |t| t == 20,
	});
}

#[test]
fn subsystem_assignment_not_triggered_by_all_with_threshold() {
	triggers_assignment_test(TriggersAssignmentConfig {
		our_assigned_tranche: 11,
		assign_validator_tranche: |_| Ok(0),
		no_show_slots: 2,
		assignments_to_import: vec![1, 2, 3, 4, 5],
		approvals_to_import: vec![1, 3, 5],
		ticks: vec![
			2,  // APPROVAL_DELAY
			21, // Check no shows
		],
		should_be_triggered: |_| false,
	});
}

#[test]
fn subsystem_assignment_triggered_if_below_maximum_and_clock_is_equal() {
	triggers_assignment_test(TriggersAssignmentConfig {
		our_assigned_tranche: 11,
		assign_validator_tranche: |_| Ok(0),
		no_show_slots: 2,
		assignments_to_import: vec![1],
		approvals_to_import: vec![],
		ticks: vec![
			21, // Check no shows
			23, // Alice wakeup, assignment triggered
		],
		should_be_triggered: |tick| tick >= 21,
	});
}

#[test]
fn subsystem_assignment_not_triggered_more_than_maximum() {
	triggers_assignment_test(TriggersAssignmentConfig {
		our_assigned_tranche: 3,
		assign_validator_tranche: |_| Ok(0),
		no_show_slots: 2,
		assignments_to_import: vec![2, 3],
		approvals_to_import: vec![],
		ticks: vec![
			2,  // APPROVAL_DELAY
			13, // Alice wakeup
			30, // Check no shows
		],
		should_be_triggered: |_| false,
	});
}

#[test]
fn subsystem_assignment_triggered_if_at_maximum() {
	triggers_assignment_test(TriggersAssignmentConfig {
		our_assigned_tranche: 21,
		assign_validator_tranche: |_| Ok(2),
		no_show_slots: 2,
		assignments_to_import: vec![1],
		approvals_to_import: vec![],
		ticks: vec![
			12, // Bob wakeup
			30, // Check no shows
		],
		should_be_triggered: |_| false,
	});
}

#[test]
fn subsystem_assignment_not_triggered_by_exact() {
	triggers_assignment_test(TriggersAssignmentConfig {
		our_assigned_tranche: 2,
		assign_validator_tranche: |_| Ok(1),
		no_show_slots: 2,
		assignments_to_import: vec![2, 3],
		approvals_to_import: vec![],
		ticks: vec![
			11, // Charlie and Dave wakeup
		],
		should_be_triggered: |_| false,
	});
}

#[test]
fn subsystem_assignment_not_triggered_if_at_maximum_but_clock_is_before() {
	triggers_assignment_test(TriggersAssignmentConfig {
		our_assigned_tranche: 6,
		assign_validator_tranche: |validator: ValidatorIndex| Ok(validator.0 as _),
		no_show_slots: 0,
		assignments_to_import: vec![2, 3, 4],
		approvals_to_import: vec![],
		ticks: vec![
			12, // Charlie wakeup
			13, // Dave wakeup
			14, // Eve wakeup
		],
		should_be_triggered: |_| false,
	});
}

#[test]
fn subsystem_assignment_not_triggered_if_at_maximum_but_clock_is_before_with_drift() {
	triggers_assignment_test(TriggersAssignmentConfig {
		our_assigned_tranche: 5,
		assign_validator_tranche: |validator: ValidatorIndex| Ok(validator.0 as _),
		no_show_slots: 2,
		assignments_to_import: vec![2, 3, 4],
		approvals_to_import: vec![],
		ticks: vec![
			12, // Charlie wakeup
			13, // Dave wakeup
			15, // Alice wakeup, noop
			30, // Check no shows
			34, // Eve wakeup
		],
		should_be_triggered: |_| false,
	});
}

#[test]
fn pre_covers_dont_stall_approval() {
	// A, B are tranche 0.
	// C is tranche 1.
	//
	// All assignments imported at once, and B, C approvals imported immediately.
	// A no-shows, leading to being covered by C.
	// Technically, this is an approved block, but it will be approved
	// when the no-show timer hits, not as a response to an approval vote.
	//
	// Note that we have 6 validators, otherwise the 2nd approval triggers
	// the >1/3 insta-approval condition.

	let assignment_criteria = Box::new(MockAssignmentCriteria::check_only(
		move |validator_index| match validator_index {
			ValidatorIndex(0 | 1) => Ok(0),
			ValidatorIndex(2) => Ok(1),
			ValidatorIndex(_) => Err(criteria::InvalidAssignment(
				criteria::InvalidAssignmentReason::ValidatorIndexOutOfBounds,
			)),
		},
	));

	let config = HarnessConfigBuilder::default().assignment_criteria(assignment_criteria).build();
	let store = config.backend();
	test_harness(config, |test_harness| async move {
		let TestHarness {
			mut virtual_overseer,
			clock,
			sync_oracle_handle: _sync_oracle_handle,
			..
		} = test_harness;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ChainApi(ChainApiMessage::FinalizedBlockNumber(rx)) => {
				rx.send(Ok(0)).unwrap();
			}
		);

		let block_hash = Hash::repeat_byte(0x01);
		let validator_index_a = ValidatorIndex(0);
		let validator_index_b = ValidatorIndex(1);
		let validator_index_c = ValidatorIndex(2);

		let validators = vec![
			Sr25519Keyring::Alice,
			Sr25519Keyring::Bob,
			Sr25519Keyring::Charlie,
			Sr25519Keyring::Dave,
			Sr25519Keyring::Eve,
			Sr25519Keyring::One,
		];
		let session_info = SessionInfo {
			validator_groups: IndexedVec::<GroupIndex, Vec<ValidatorIndex>>::from(vec![
				vec![ValidatorIndex(0), ValidatorIndex(1)],
				vec![ValidatorIndex(2), ValidatorIndex(5)],
				vec![ValidatorIndex(3), ValidatorIndex(4)],
			]),
			..session_info(&validators)
		};

		let candidate_descriptor = make_candidate(ParaId::from(1_u32), &block_hash);
		let candidate_hash = candidate_descriptor.hash();

		let head: Hash = ChainBuilder::GENESIS_HASH;
		let mut builder = ChainBuilder::new();
		let slot = Slot::from(1 as u64);
		builder.add_block(
			block_hash,
			head,
			1,
			BlockConfig {
				slot,
				candidates: Some(vec![(candidate_descriptor, CoreIndex(0), GroupIndex(0))]),
				session_info: Some(session_info),
				end_syncing: false,
				approval_voting_params: ApprovalVotingParams::default(),
			},
		);
		builder.build(&mut virtual_overseer).await;

		let candidate_index = 0;

		let rx = import_assignment(
			&mut virtual_overseer,
			block_hash,
			candidate_index,
			validator_index_a,
			0,
		)
		.await;

		assert_eq!(rx.await, Ok(AssignmentCheckResult::Accepted),);

		let rx = import_assignment(
			&mut virtual_overseer,
			block_hash,
			candidate_index,
			validator_index_b,
			0,
		)
		.await;

		assert_eq!(rx.await, Ok(AssignmentCheckResult::Accepted),);

		let rx = import_assignment(
			&mut virtual_overseer,
			block_hash,
			candidate_index,
			validator_index_c,
			1,
		)
		.await;

		assert_eq!(rx.await, Ok(AssignmentCheckResult::Accepted),);

		let session_index = 1;
		let sig_b = sign_approval(Sr25519Keyring::Bob, candidate_hash, session_index);

		let rx = import_approval(
			&mut virtual_overseer,
			block_hash,
			candidate_index,
			validator_index_b,
			candidate_hash,
			session_index,
			false,
			Some(sig_b),
		)
		.await;

		assert_eq!(rx.await, Ok(ApprovalCheckResult::Accepted),);

		let sig_c = sign_approval(Sr25519Keyring::Charlie, candidate_hash, session_index);
		let rx = import_approval(
			&mut virtual_overseer,
			block_hash,
			candidate_index,
			validator_index_c,
			candidate_hash,
			session_index,
			false,
			Some(sig_c),
		)
		.await;

		assert_eq!(rx.await, Ok(ApprovalCheckResult::Accepted),);

		// Sleep to ensure we get a consistent read on the database.
		//
		// NOTE: Since the response above occurs before writing to the database, we are somewhat
		// breaking the external consistency of the API by reaching into the database directly.
		// Under normal operation, this wouldn't be necessary, since all requests are serialized by
		// the event loop and we write at the end of each pass. However, if the database write were
		// to fail, a downstream subsystem may expect for this candidate to be approved, and
		// possibly take further actions on the assumption that the candidate is approved, when
		// that may not be the reality from the database's perspective. This could be avoided
		// entirely by having replies processed after database writes, but that would constitute a
		// larger refactor and incur a performance penalty.
		futures_timer::Delay::new(Duration::from_millis(100)).await;

		// The candidate should not be approved.
		let candidate_entry = store.load_candidate_entry(&candidate_hash).unwrap().unwrap();
		assert!(!candidate_entry.approval_entry(&block_hash).unwrap().is_approved());
		assert!(clock.inner.lock().current_wakeup_is(2));

		// Wait for the no-show timer to observe the approval from
		// tranche 0 and set a wakeup for tranche 1.
		clock.inner.lock().set_tick(30);

		// Sleep to ensure we get a consistent read on the database.
		futures_timer::Delay::new(Duration::from_millis(100)).await;

		// The next wakeup should observe the assignment & approval from
		// tranche 1, and the no-show from tranche 0 should be immediately covered.
		assert_eq!(clock.inner.lock().next_wakeup(), Some(31));
		clock.inner.lock().set_tick(31);

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ChainSelection(ChainSelectionMessage::Approved(b_hash)) => {
				assert_eq!(b_hash, block_hash);
			}
		);

		// The candidate and block should now be approved.
		let candidate_entry = store.load_candidate_entry(&candidate_hash).unwrap().unwrap();
		assert!(candidate_entry.approval_entry(&block_hash).unwrap().is_approved());
		assert!(clock.inner.lock().next_wakeup().is_none());

		let block_entry = store.load_block_entry(&block_hash).unwrap().unwrap();
		assert!(block_entry.is_fully_approved());

		virtual_overseer
	});
}

#[test]
fn waits_until_approving_assignments_are_old_enough() {
	// A, B are tranche 0.

	let assignment_criteria = Box::new(MockAssignmentCriteria::check_only(|_| Ok(0)));

	let config = HarnessConfigBuilder::default().assignment_criteria(assignment_criteria).build();
	let store = config.backend();
	test_harness(config, |test_harness| async move {
		let TestHarness {
			mut virtual_overseer,
			clock,
			sync_oracle_handle: _sync_oracle_handle,
			..
		} = test_harness;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ChainApi(ChainApiMessage::FinalizedBlockNumber(rx)) => {
				rx.send(Ok(0)).unwrap();
			}
		);

		clock.inner.lock().set_tick(APPROVAL_DELAY);

		let block_hash = Hash::repeat_byte(0x01);
		let validator_index_a = ValidatorIndex(0);
		let validator_index_b = ValidatorIndex(1);

		let validators = vec![
			Sr25519Keyring::Alice,
			Sr25519Keyring::Bob,
			Sr25519Keyring::Charlie,
			Sr25519Keyring::Dave,
			Sr25519Keyring::Eve,
			Sr25519Keyring::One,
		];
		let session_info = SessionInfo {
			validator_groups: IndexedVec::<GroupIndex, Vec<ValidatorIndex>>::from(vec![
				vec![ValidatorIndex(0), ValidatorIndex(1)],
				vec![ValidatorIndex(2), ValidatorIndex(5)],
				vec![ValidatorIndex(3), ValidatorIndex(4)],
			]),
			..session_info(&validators)
		};

		let candidate_descriptor = make_candidate(ParaId::from(1_u32), &block_hash);
		let candidate_hash = candidate_descriptor.hash();

		let head: Hash = ChainBuilder::GENESIS_HASH;
		let mut builder = ChainBuilder::new();
		let slot = Slot::from(1 as u64);
		builder.add_block(
			block_hash,
			head,
			1,
			BlockConfig {
				slot,
				candidates: Some(vec![(candidate_descriptor, CoreIndex(0), GroupIndex(0))]),
				session_info: Some(session_info),
				end_syncing: false,
				approval_voting_params: ApprovalVotingParams::default(),
			},
		);
		builder.build(&mut virtual_overseer).await;

		let candidate_index = 0;

		let rx = import_assignment(
			&mut virtual_overseer,
			block_hash,
			candidate_index,
			validator_index_a,
			0,
		)
		.await;

		assert_eq!(rx.await, Ok(AssignmentCheckResult::Accepted),);

		let rx = import_assignment(
			&mut virtual_overseer,
			block_hash,
			candidate_index,
			validator_index_b,
			0,
		)
		.await;

		assert_eq!(rx.await, Ok(AssignmentCheckResult::Accepted),);

		assert!(clock.inner.lock().current_wakeup_is(APPROVAL_DELAY + APPROVAL_DELAY));

		let session_index = 1;

		let sig_a = sign_approval(Sr25519Keyring::Alice, candidate_hash, session_index);
		let rx = import_approval(
			&mut virtual_overseer,
			block_hash,
			candidate_index,
			validator_index_a,
			candidate_hash,
			session_index,
			false,
			Some(sig_a),
		)
		.await;

		assert_eq!(rx.await, Ok(ApprovalCheckResult::Accepted),);

		let sig_b = sign_approval(Sr25519Keyring::Bob, candidate_hash, session_index);

		let rx = import_approval(
			&mut virtual_overseer,
			block_hash,
			candidate_index,
			validator_index_b,
			candidate_hash,
			session_index,
			false,
			Some(sig_b),
		)
		.await;

		assert_eq!(rx.await, Ok(ApprovalCheckResult::Accepted),);

		// Sleep to ensure we get a consistent read on the database.
		futures_timer::Delay::new(Duration::from_millis(100)).await;

		// The candidate should not be approved, even though at this
		// point in time we have 2 assignments and 2 approvals.
		//
		// This is because the assignments were imported at tick `APPROVAL_DELAY`
		// and won't be considered until `APPROVAL_DELAY` more ticks have passed.
		let candidate_entry = store.load_candidate_entry(&candidate_hash).unwrap().unwrap();
		assert!(!candidate_entry.approval_entry(&block_hash).unwrap().is_approved());
		assert!(clock.inner.lock().current_wakeup_is(APPROVAL_DELAY + APPROVAL_DELAY));

		// Trigger the wakeup.
		clock.inner.lock().set_tick(APPROVAL_DELAY + APPROVAL_DELAY);

		// Sleep to ensure we get a consistent read on the database.
		futures_timer::Delay::new(Duration::from_millis(100)).await;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ChainSelection(ChainSelectionMessage::Approved(b_hash)) => {
				assert_eq!(b_hash, block_hash);
			}
		);

		// The candidate and block should now be approved.
		let candidate_entry = store.load_candidate_entry(&candidate_hash).unwrap().unwrap();
		assert!(candidate_entry.approval_entry(&block_hash).unwrap().is_approved());
		assert!(clock.inner.lock().next_wakeup().is_none());

		let block_entry = store.load_block_entry(&block_hash).unwrap().unwrap();
		assert!(block_entry.is_fully_approved());

		virtual_overseer
	});
}

#[test]
fn test_approval_is_sent_on_max_approval_coalesce_count() {
	let assignment_criteria = Box::new(MockAssignmentCriteria(
		|| {
			let mut assignments = HashMap::new();
			let _ = assignments.insert(
				CoreIndex(0),
				approval_db::v2::OurAssignment {
					cert: garbage_assignment_cert_v2(AssignmentCertKindV2::RelayVRFModuloCompact {
						core_bitfield: CoreIndex(0).into(),
					}),
					tranche: 0,
					validator_index: ValidatorIndex(0),
					triggered: false,
				}
				.into(),
			);

			let assignments_cert =
				garbage_assignment_cert_v2(AssignmentCertKindV2::RelayVRFModuloCompact {
					core_bitfield: vec![CoreIndex(0), CoreIndex(1), CoreIndex(2)]
						.try_into()
						.unwrap(),
				});
			let _ = assignments.insert(
				CoreIndex(0),
				approval_db::v2::OurAssignment {
					cert: assignments_cert.clone(),
					tranche: 0,
					validator_index: ValidatorIndex(0),
					triggered: false,
				}
				.into(),
			);

			let _ = assignments.insert(
				CoreIndex(1),
				approval_db::v2::OurAssignment {
					cert: assignments_cert.clone(),
					tranche: 0,
					validator_index: ValidatorIndex(0),
					triggered: false,
				}
				.into(),
			);
			assignments
		},
		|_| Ok(0),
	));

	let config = HarnessConfigBuilder::default().assignment_criteria(assignment_criteria).build();
	let store = config.backend();

	test_harness(config, |test_harness| async move {
		let TestHarness { mut virtual_overseer, clock, sync_oracle_handle: _sync_oracle_handle } =
			test_harness;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ChainApi(ChainApiMessage::FinalizedBlockNumber(rx)) => {
				rx.send(Ok(0)).unwrap();
			}
		);

		let block_hash = Hash::repeat_byte(0x01);

		let candidate_commitments = CandidateCommitments::default();

		let candidate_receipt1 = {
			let mut receipt = dummy_candidate_receipt_v2(block_hash);
			receipt.descriptor.set_para_id(ParaId::from(1_u32));
			receipt.commitments_hash = candidate_commitments.hash();
			receipt
		};

		let candidate_hash1 = candidate_receipt1.hash();

		let candidate_receipt2 = {
			let mut receipt = dummy_candidate_receipt_v2(block_hash);
			receipt.descriptor.set_para_id(ParaId::from(2_u32));
			receipt.commitments_hash = candidate_commitments.hash();
			receipt
		};

		let slot = Slot::from(1);
		let candidate_index1 = 0;
		let candidate_index2 = 1;

		let validators = vec![
			Sr25519Keyring::Alice,
			Sr25519Keyring::Bob,
			Sr25519Keyring::Charlie,
			Sr25519Keyring::Dave,
			Sr25519Keyring::Eve,
		];
		let session_info = SessionInfo {
			validator_groups: IndexedVec::<GroupIndex, Vec<ValidatorIndex>>::from(vec![
				vec![ValidatorIndex(0), ValidatorIndex(1)],
				vec![ValidatorIndex(2)],
				vec![ValidatorIndex(3), ValidatorIndex(4)],
			]),
			..session_info(&validators)
		};

		let candidates = Some(vec![
			(candidate_receipt1.clone(), CoreIndex(0), GroupIndex(0)),
			(candidate_receipt2.clone(), CoreIndex(1), GroupIndex(1)),
		]);
		ChainBuilder::new()
			.add_block(
				block_hash,
				ChainBuilder::GENESIS_HASH,
				1,
				BlockConfig {
					slot,
					candidates: candidates.clone(),
					session_info: Some(session_info.clone()),
					end_syncing: false,
					// Configure a coalesce count of 2 so that both candidates are coalesced into a
					// single approval. This value is cached at import and read back by
					// `maybe_create_signature`.
					approval_voting_params: ApprovalVotingParams { max_approval_coalesce_count: 2 },
				},
			)
			.build(&mut virtual_overseer)
			.await;

		assert!(!clock.inner.lock().current_wakeup_is(1));
		clock.inner.lock().wakeup_all(1);

		assert!(clock.inner.lock().current_wakeup_is(slot_to_tick(slot)));
		clock.inner.lock().wakeup_all(slot_to_tick(slot));

		futures_timer::Delay::new(Duration::from_millis(200)).await;

		clock.inner.lock().wakeup_all(slot_to_tick(slot + 2));

		assert_eq!(clock.inner.lock().wakeups.len(), 0);

		futures_timer::Delay::new(Duration::from_millis(200)).await;

		let candidate_entry = store.load_candidate_entry(&candidate_hash1).unwrap().unwrap();
		let our_assignment =
			candidate_entry.approval_entry(&block_hash).unwrap().our_assignment().unwrap();
		assert!(our_assignment.triggered());

		handle_approval_on_max_coalesce_count(
			&mut virtual_overseer,
			vec![candidate_index1, candidate_index2],
		)
		.await;

		virtual_overseer
	});
}

async fn handle_approval_on_max_coalesce_count(
	virtual_overseer: &mut VirtualOverseer,
	candidate_indices: Vec<CandidateIndex>,
) {
	assert_matches!(
		overseer_recv(virtual_overseer).await,
		AllMessages::ApprovalDistribution(ApprovalDistributionMessage::DistributeAssignment(
			_,
			c_indices,
		)) => {
			assert_eq!(TryInto::<CandidateBitfield>::try_into(candidate_indices.clone()).unwrap(), c_indices);
		}
	);

	for _ in &candidate_indices {
		recover_available_data(virtual_overseer).await;
		fetch_validation_code(virtual_overseer).await;
	}

	for _ in &candidate_indices {
		assert_matches!(
			overseer_recv(virtual_overseer).await,
			AllMessages::CandidateValidation(CandidateValidationMessage::ValidateFromExhaustive{exec_kind, response_sender, ..}) if exec_kind == PvfExecKind::Approval => {
				response_sender.send(Ok(ValidationResult::Valid(Default::default(), Default::default())))
					.unwrap();
			}
		);
	}

	assert_matches!(
		overseer_recv(virtual_overseer).await,
		AllMessages::ApprovalDistribution(ApprovalDistributionMessage::DistributeApproval(vote)) => {
			assert_eq!(TryInto::<CandidateBitfield>::try_into(candidate_indices).unwrap(), vote.candidate_indices);
		}
	);

	// Assert that there are no more messages being sent by the subsystem
	assert!(overseer_recv(virtual_overseer).timeout(TIMEOUT / 2).await.is_none());
}

async fn handle_approval_on_max_wait_time(
	virtual_overseer: &mut VirtualOverseer,
	candidate_indices: Vec<CandidateIndex>,
	clock: Arc<MockClock>,
) {
	const TICK_NOW_BEGIN: u64 = 1;

	clock.inner.lock().set_tick(TICK_NOW_BEGIN);

	assert_matches!(
		overseer_recv(virtual_overseer).await,
		AllMessages::ApprovalDistribution(ApprovalDistributionMessage::DistributeAssignment(
			_,
			c_indices,
		)) => {
			assert_eq!(TryInto::<CandidateBitfield>::try_into(candidate_indices.clone()).unwrap(), c_indices);
		}
	);

	for _ in &candidate_indices {
		recover_available_data(virtual_overseer).await;
		fetch_validation_code(virtual_overseer).await;
	}

	for _ in &candidate_indices {
		assert_matches!(
			overseer_recv(virtual_overseer).await,
			AllMessages::CandidateValidation(CandidateValidationMessage::ValidateFromExhaustive{exec_kind, response_sender, ..}) if exec_kind == PvfExecKind::Approval => {
				response_sender.send(Ok(ValidationResult::Valid(Default::default(), Default::default())))
					.unwrap();
			}
		);
	}

	assert!(overseer_recv(virtual_overseer).timeout(TIMEOUT / 2).await.is_none());

	// Move the clock just before we should send the approval
	clock
		.inner
		.lock()
		.set_tick(MAX_APPROVAL_COALESCE_WAIT_TICKS as Tick + TICK_NOW_BEGIN - 1);

	assert!(overseer_recv(virtual_overseer).timeout(TIMEOUT / 2).await.is_none());

	// Move the clock tick, so we can trigger a force sending of the approvals
	clock
		.inner
		.lock()
		.set_tick(MAX_APPROVAL_COALESCE_WAIT_TICKS as Tick + TICK_NOW_BEGIN);

	assert_matches!(
		overseer_recv(virtual_overseer).await,
		AllMessages::ApprovalDistribution(ApprovalDistributionMessage::DistributeApproval(vote)) => {
			assert_eq!(TryInto::<CandidateBitfield>::try_into(candidate_indices).unwrap(), vote.candidate_indices);
		}
	);

	// Assert that there are no more messages being sent by the subsystem
	assert!(overseer_recv(virtual_overseer).timeout(TIMEOUT / 2).await.is_none());
}

#[test]
fn test_approval_is_sent_on_max_approval_coalesce_wait() {
	let assignment_criteria = Box::new(MockAssignmentCriteria(
		|| {
			let mut assignments = HashMap::new();
			let _ = assignments.insert(
				CoreIndex(0),
				approval_db::v2::OurAssignment {
					cert: garbage_assignment_cert_v2(AssignmentCertKindV2::RelayVRFModuloCompact {
						core_bitfield: CoreIndex(0).into(),
					}),
					tranche: 0,
					validator_index: ValidatorIndex(0),
					triggered: false,
				}
				.into(),
			);

			let assignments_cert =
				garbage_assignment_cert_v2(AssignmentCertKindV2::RelayVRFModuloCompact {
					core_bitfield: vec![CoreIndex(0), CoreIndex(1), CoreIndex(2)]
						.try_into()
						.unwrap(),
				});
			let _ = assignments.insert(
				CoreIndex(0),
				approval_db::v2::OurAssignment {
					cert: assignments_cert.clone(),
					tranche: 0,
					validator_index: ValidatorIndex(0),
					triggered: false,
				}
				.into(),
			);

			let _ = assignments.insert(
				CoreIndex(1),
				approval_db::v2::OurAssignment {
					cert: assignments_cert.clone(),
					tranche: 0,
					validator_index: ValidatorIndex(0),
					triggered: false,
				}
				.into(),
			);
			assignments
		},
		|_| Ok(0),
	));

	let config = HarnessConfigBuilder::default().assignment_criteria(assignment_criteria).build();
	let store = config.backend();

	test_harness(config, |test_harness| async move {
		let TestHarness { mut virtual_overseer, clock, sync_oracle_handle: _ } = test_harness;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ChainApi(ChainApiMessage::FinalizedBlockNumber(rx)) => {
				rx.send(Ok(0)).unwrap();
			}
		);

		let block_hash = Hash::repeat_byte(0x01);

		let candidate_commitments = CandidateCommitments::default();

		let candidate_receipt1 = {
			let mut receipt = dummy_candidate_receipt_v2(block_hash);
			receipt.descriptor.set_para_id(ParaId::from(1_u32));
			receipt.commitments_hash = candidate_commitments.hash();
			receipt
		};

		let candidate_hash1 = candidate_receipt1.hash();

		let candidate_receipt2 = {
			let mut receipt = dummy_candidate_receipt_v2(block_hash);
			receipt.descriptor.set_para_id(ParaId::from(2_u32));
			receipt.commitments_hash = candidate_commitments.hash();
			receipt
		};

		let slot = Slot::from(1);
		let candidate_index1 = 0;
		let candidate_index2 = 1;

		let validators = vec![
			Sr25519Keyring::Alice,
			Sr25519Keyring::Bob,
			Sr25519Keyring::Charlie,
			Sr25519Keyring::Dave,
			Sr25519Keyring::Eve,
		];
		let session_info = SessionInfo {
			validator_groups: IndexedVec::<GroupIndex, Vec<ValidatorIndex>>::from(vec![
				vec![ValidatorIndex(0), ValidatorIndex(1)],
				vec![ValidatorIndex(2)],
				vec![ValidatorIndex(3), ValidatorIndex(4)],
			]),
			..session_info(&validators)
		};

		let candidates = Some(vec![
			(candidate_receipt1.clone(), CoreIndex(0), GroupIndex(0)),
			(candidate_receipt2.clone(), CoreIndex(1), GroupIndex(1)),
		]);
		ChainBuilder::new()
			.add_block(
				block_hash,
				ChainBuilder::GENESIS_HASH,
				1,
				BlockConfig {
					slot,
					candidates: candidates.clone(),
					session_info: Some(session_info.clone()),
					end_syncing: false,
					// Configure a coalesce count of 3 so the approvals are cached until the
					// coalesce wait timer expires. This value is cached at import and read back by
					// `maybe_create_signature`.
					approval_voting_params: ApprovalVotingParams { max_approval_coalesce_count: 3 },
				},
			)
			.build(&mut virtual_overseer)
			.await;

		assert!(!clock.inner.lock().current_wakeup_is(1));
		clock.inner.lock().wakeup_all(1);

		assert!(clock.inner.lock().current_wakeup_is(slot_to_tick(slot)));
		clock.inner.lock().wakeup_all(slot_to_tick(slot));

		futures_timer::Delay::new(Duration::from_millis(200)).await;

		clock.inner.lock().wakeup_all(slot_to_tick(slot + 2));

		assert_eq!(clock.inner.lock().wakeups.len(), 0);

		futures_timer::Delay::new(Duration::from_millis(200)).await;

		let candidate_entry = store.load_candidate_entry(&candidate_hash1).unwrap().unwrap();
		let our_assignment =
			candidate_entry.approval_entry(&block_hash).unwrap().our_assignment().unwrap();
		assert!(our_assignment.triggered());

		handle_approval_on_max_wait_time(
			&mut virtual_overseer,
			vec![candidate_index1, candidate_index2],
			clock,
		)
		.await;

		virtual_overseer
	});
}

// Builds a chain with a fork where both relay blocks include the same candidate.
async fn build_chain_with_two_blocks_with_one_candidate_each(
	block_hash1: Hash,
	block_hash2: Hash,
	slot: Slot,
	sync_oracle_handle: TestSyncOracleHandle,
	candidate_receipt: CandidateReceipt,
	approval_voting_params: ApprovalVotingParams,
) -> (ChainBuilder, SessionInfo) {
	let validators = vec![
		Sr25519Keyring::Alice,
		Sr25519Keyring::Bob,
		Sr25519Keyring::Charlie,
		Sr25519Keyring::Dave,
		Sr25519Keyring::Eve,
	];
	let session_info = SessionInfo {
		validator_groups: IndexedVec::<GroupIndex, Vec<ValidatorIndex>>::from(vec![
			vec![ValidatorIndex(0), ValidatorIndex(1)],
			vec![ValidatorIndex(2)],
			vec![ValidatorIndex(3), ValidatorIndex(4)],
		]),
		..session_info(&validators)
	};

	let candidates = Some(vec![(candidate_receipt.clone(), CoreIndex(0), GroupIndex(0))]);
	let mut chain_builder = ChainBuilder::new();

	chain_builder
		.major_syncing(sync_oracle_handle.is_major_syncing.clone())
		.add_block(
			block_hash1,
			ChainBuilder::GENESIS_HASH,
			1,
			BlockConfig {
				slot,
				candidates: candidates.clone(),
				session_info: Some(session_info.clone()),
				end_syncing: false,
				approval_voting_params,
			},
		)
		.add_block(
			block_hash2,
			ChainBuilder::GENESIS_HASH,
			1,
			BlockConfig {
				slot,
				candidates,
				session_info: Some(session_info.clone()),
				end_syncing: true,
				approval_voting_params,
			},
		);
	(chain_builder, session_info)
}

async fn setup_overseer_with_two_blocks_each_with_one_assignment_triggered(
	virtual_overseer: &mut VirtualOverseer,
	store: TestStore,
	clock: &Arc<MockClock>,
	sync_oracle_handle: TestSyncOracleHandle,
	approval_voting_params: ApprovalVotingParams,
) {
	assert_matches!(
		overseer_recv(virtual_overseer).await,
		AllMessages::ChainApi(ChainApiMessage::FinalizedBlockNumber(rx)) => {
			rx.send(Ok(0)).unwrap();
		}
	);

	let block_hash = Hash::repeat_byte(0x01);
	let fork_block_hash = Hash::repeat_byte(0x02);
	let candidate_commitments = CandidateCommitments::default();
	let mut candidate_receipt = dummy_candidate_receipt_v2(block_hash);
	candidate_receipt.commitments_hash = candidate_commitments.hash();
	let candidate_hash = candidate_receipt.hash();
	let slot = Slot::from(1);
	let (chain_builder, _session_info) = build_chain_with_two_blocks_with_one_candidate_each(
		block_hash,
		fork_block_hash,
		slot,
		sync_oracle_handle,
		candidate_receipt,
		approval_voting_params,
	)
	.await;
	chain_builder.build(virtual_overseer).await;

	assert!(!clock.inner.lock().current_wakeup_is(1));
	clock.inner.lock().wakeup_all(1);

	assert!(clock.inner.lock().current_wakeup_is(slot_to_tick(slot)));
	clock.inner.lock().wakeup_all(slot_to_tick(slot));

	futures_timer::Delay::new(Duration::from_millis(200)).await;

	clock.inner.lock().wakeup_all(slot_to_tick(slot + 2));

	assert_eq!(clock.inner.lock().wakeups.len(), 0);

	futures_timer::Delay::new(Duration::from_millis(200)).await;

	let candidate_entry = store.load_candidate_entry(&candidate_hash).unwrap().unwrap();
	let our_assignment =
		candidate_entry.approval_entry(&block_hash).unwrap().our_assignment().unwrap();
	assert!(our_assignment.triggered());
}

// Builds a chain with a fork where both relay blocks include the same candidate.
async fn build_chain_with_block_with_two_candidates(
	block_hash1: Hash,
	slot: Slot,
	sync_oracle_handle: TestSyncOracleHandle,
	candidate_receipt: Vec<CandidateReceipt>,
	approval_voting_params: ApprovalVotingParams,
) -> (ChainBuilder, SessionInfo) {
	let validators = vec![
		Sr25519Keyring::Alice,
		Sr25519Keyring::Bob,
		Sr25519Keyring::Charlie,
		Sr25519Keyring::Dave,
		Sr25519Keyring::Eve,
	];
	let session_info = SessionInfo {
		validator_groups: IndexedVec::<GroupIndex, Vec<ValidatorIndex>>::from(vec![
			vec![ValidatorIndex(0), ValidatorIndex(1)],
			vec![ValidatorIndex(2)],
			vec![ValidatorIndex(3), ValidatorIndex(4)],
		]),
		..session_info(&validators)
	};

	let candidates = Some(
		candidate_receipt
			.iter()
			.enumerate()
			.map(|(i, receipt)| (receipt.clone(), CoreIndex(i as u32), GroupIndex(i as u32)))
			.collect(),
	);
	let mut chain_builder = ChainBuilder::new();

	chain_builder
		.major_syncing(sync_oracle_handle.is_major_syncing.clone())
		.add_block(
			block_hash1,
			ChainBuilder::GENESIS_HASH,
			1,
			BlockConfig {
				slot,
				candidates: candidates.clone(),
				session_info: Some(session_info.clone()),
				end_syncing: true,
				approval_voting_params,
			},
		);
	(chain_builder, session_info)
}

async fn setup_overseer_with_blocks_with_two_assignments_triggered(
	virtual_overseer: &mut VirtualOverseer,
	store: TestStore,
	clock: &Arc<MockClock>,
	sync_oracle_handle: TestSyncOracleHandle,
	approval_voting_params: ApprovalVotingParams,
) {
	assert_matches!(
		overseer_recv(virtual_overseer).await,
		AllMessages::ChainApi(ChainApiMessage::FinalizedBlockNumber(rx)) => {
			rx.send(Ok(0)).unwrap();
		}
	);

	let block_hash = Hash::repeat_byte(0x01);
	let candidate_commitments = CandidateCommitments::default();
	let mut candidate_receipt = dummy_candidate_receipt_v2(block_hash);
	candidate_receipt.commitments_hash = candidate_commitments.hash();
	let candidate_hash = candidate_receipt.hash();

	let mut candidate_commitments2 = CandidateCommitments::default();
	candidate_commitments2.processed_downward_messages = 3;
	let mut candidate_receipt2 = dummy_candidate_receipt_v2(block_hash);
	candidate_receipt2.commitments_hash = candidate_commitments2.hash();
	let candidate_hash2 = candidate_receipt2.hash();

	let slot = Slot::from(1);
	let (chain_builder, _session_info) = build_chain_with_block_with_two_candidates(
		block_hash,
		slot,
		sync_oracle_handle,
		vec![candidate_receipt, candidate_receipt2],
		approval_voting_params,
	)
	.await;
	chain_builder.build(virtual_overseer).await;

	assert!(!clock.inner.lock().current_wakeup_is(1));
	clock.inner.lock().wakeup_all(1);

	assert!(clock.inner.lock().current_wakeup_is(slot_to_tick(slot)));
	clock.inner.lock().wakeup_all(slot_to_tick(slot));

	futures_timer::Delay::new(Duration::from_millis(200)).await;

	clock.inner.lock().wakeup_all(slot_to_tick(slot + 2));

	assert_eq!(clock.inner.lock().wakeups.len(), 0);

	futures_timer::Delay::new(Duration::from_millis(200)).await;

	let candidate_entry = store.load_candidate_entry(&candidate_hash).unwrap().unwrap();
	let our_assignment =
		candidate_entry.approval_entry(&block_hash).unwrap().our_assignment().unwrap();
	assert!(our_assignment.triggered());

	let candidate_entry = store.load_candidate_entry(&candidate_hash2).unwrap().unwrap();
	let our_assignment =
		candidate_entry.approval_entry(&block_hash).unwrap().our_assignment().unwrap();
	assert!(our_assignment.triggered());
}

// Tests that for candidates that we did not approve yet, for which we triggered the assignment and
// the approval work we restart the work to approve it.
#[test]
fn subsystem_relaunches_approval_work_on_restart() {
	let assignment_criteria = Box::new(MockAssignmentCriteria(
		|| {
			let mut assignments = HashMap::new();
			let _ = assignments.insert(
				CoreIndex(0),
				approval_db::v2::OurAssignment {
					cert: garbage_assignment_cert_v2(AssignmentCertKindV2::RelayVRFModuloCompact {
						core_bitfield: CoreIndex(0).into(),
					}),
					tranche: 0,
					validator_index: ValidatorIndex(0),
					triggered: false,
				}
				.into(),
			);

			let _ = assignments.insert(
				CoreIndex(0),
				approval_db::v2::OurAssignment {
					cert: garbage_assignment_cert_v2(AssignmentCertKindV2::RelayVRFModuloCompact {
						core_bitfield: vec![CoreIndex(0), CoreIndex(1), CoreIndex(2)]
							.try_into()
							.unwrap(),
					}),
					tranche: 0,
					validator_index: ValidatorIndex(0),
					triggered: false,
				}
				.into(),
			);
			assignments
		},
		|_| Ok(0),
	));
	let config = HarnessConfigBuilder::default().assignment_criteria(assignment_criteria).build();
	let store = config.backend();
	let store_clone = config.backend();

	test_harness(config, |test_harness| async move {
		let TestHarness { mut virtual_overseer, clock, sync_oracle_handle } = test_harness;

		setup_overseer_with_two_blocks_each_with_one_assignment_triggered(
			&mut virtual_overseer,
			store,
			&clock,
			sync_oracle_handle,
			ApprovalVotingParams::default(),
		)
		.await;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ApprovalDistribution(ApprovalDistributionMessage::DistributeAssignment(
				_,
				_,
			)) => {
			}
		);

		recover_available_data(&mut virtual_overseer).await;
		fetch_validation_code(&mut virtual_overseer).await;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ApprovalDistribution(ApprovalDistributionMessage::DistributeAssignment(
				_,
				_
			)) => {
			}
		);

		// Bail early after the assignment has been distributed but before we answer with the mocked
		// approval from CandidateValidation.
		virtual_overseer
	});

	// Restart a new approval voting subsystem with the same database and major syncing true until
	// the last leaf.
	let config = HarnessConfigBuilder::default().backend(store_clone).major_syncing(true).build();

	test_harness(config, |test_harness| async move {
		let TestHarness { mut virtual_overseer, clock, sync_oracle_handle } = test_harness;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ChainApi(ChainApiMessage::FinalizedBlockNumber(rx)) => {
				rx.send(Ok(0)).unwrap();
			}
		);

		let block_hash = Hash::repeat_byte(0x01);
		let fork_block_hash = Hash::repeat_byte(0x02);
		let candidate_commitments = CandidateCommitments::default();
		let mut candidate_receipt = dummy_candidate_receipt_v2(block_hash);
		candidate_receipt.commitments_hash = candidate_commitments.hash();
		let slot = Slot::from(1);
		clock.inner.lock().set_tick(slot_to_tick(slot + 2));
		let (chain_builder, session_info) = build_chain_with_two_blocks_with_one_candidate_each(
			block_hash,
			fork_block_hash,
			slot,
			sync_oracle_handle,
			candidate_receipt,
			ApprovalVotingParams::default(),
		)
		.await;

		chain_builder.build(&mut virtual_overseer).await;

		futures_timer::Delay::new(Duration::from_millis(2000)).await;

		// On major syncing ending Approval voting should send all the necessary messages for a
		// candidate to be approved.
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ApprovalDistribution(ApprovalDistributionMessage::NewBlocks(
				_,
			)) => {
			}
		);

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ApprovalDistribution(ApprovalDistributionMessage::DistributeAssignment(
				_,
				_,
			)) => {
			}
		);

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ApprovalDistribution(ApprovalDistributionMessage::DistributeAssignment(
				_,
				_,
			)) => {
			}
		);

		// Guarantees the approval work has been relaunched.
		recover_available_data(&mut virtual_overseer).await;
		fetch_validation_code(&mut virtual_overseer).await;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::CandidateValidation(CandidateValidationMessage::ValidateFromExhaustive {
				exec_kind,
				response_sender,
				..
			}) if exec_kind == PvfExecKind::Approval => {
				response_sender.send(Ok(ValidationResult::Valid(Default::default(), Default::default())))
					.unwrap();
			}
		);

		// process_wakeup fetches session info (no longer cached by
		// distribution_messages_for_activation).
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::RuntimeApi(
				RuntimeApiMessage::Request(
					_,
					RuntimeApiRequest::SessionInfo(_, si_tx),
				)
			) => {
				si_tx.send(Ok(Some(session_info.clone()))).unwrap();
			}
		);
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::RuntimeApi(
				RuntimeApiMessage::Request(_, RuntimeApiRequest::NodeFeatures(_, si_tx), )
			) => {
				si_tx.send(Ok(NodeFeatures::EMPTY)).unwrap();
			}
		);

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::ApprovalVotingParams(_, sender))) => {
				let _ = sender.send(Ok(ApprovalVotingParams::default()));
			}
		);

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ApprovalDistribution(ApprovalDistributionMessage::DistributeApproval(_))
		);

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ApprovalDistribution(ApprovalDistributionMessage::DistributeApproval(_))
		);

		// Assert that there are no more messages being sent by the subsystem
		assert!(overseer_recv(&mut virtual_overseer).timeout(TIMEOUT / 2).await.is_none());

		virtual_overseer
	});
}

/// Test that we retry the approval of candidate on availability failure, up to max retries.
#[test]
fn subsystem_relaunches_approval_work_on_availability_failure() {
	let assignment_criteria = Box::new(MockAssignmentCriteria(
		|| {
			let mut assignments = HashMap::new();

			let _ = assignments.insert(
				CoreIndex(0),
				approval_db::v2::OurAssignment {
					cert: garbage_assignment_cert_v2(AssignmentCertKindV2::RelayVRFModuloCompact {
						core_bitfield: vec![CoreIndex(0), CoreIndex(2)].try_into().unwrap(),
					}),
					tranche: 0,
					validator_index: ValidatorIndex(0),
					triggered: false,
				}
				.into(),
			);

			let _ = assignments.insert(
				CoreIndex(1),
				approval_db::v2::OurAssignment {
					cert: garbage_assignment_cert_v2(AssignmentCertKindV2::RelayVRFDelay {
						core_index: CoreIndex(1),
					}),
					tranche: 0,
					validator_index: ValidatorIndex(0),
					triggered: false,
				}
				.into(),
			);
			assignments
		},
		|_| Ok(0),
	));
	let config = HarnessConfigBuilder::default().assignment_criteria(assignment_criteria).build();
	let store = config.backend();

	test_harness(config, |test_harness| async move {
		let TestHarness { mut virtual_overseer, clock, sync_oracle_handle } = test_harness;

		setup_overseer_with_blocks_with_two_assignments_triggered(
			&mut virtual_overseer,
			store,
			&clock,
			sync_oracle_handle,
			ApprovalVotingParams::default(),
		)
		.await;

		// We have two candidates for one we are going to fail the availability for up to
		// max_retries and for the other we are going to succeed on the last retry, so we should
		// see the approval being distributed.
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ApprovalDistribution(ApprovalDistributionMessage::DistributeAssignment(
				_,
				_,
			)) => {
			}
		);

		recover_available_data_failure(&mut virtual_overseer).await;
		fetch_validation_code(&mut virtual_overseer).await;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ApprovalDistribution(ApprovalDistributionMessage::DistributeAssignment(
				_,
				_
			)) => {
			}
		);

		recover_available_data_failure(&mut virtual_overseer).await;
		fetch_validation_code(&mut virtual_overseer).await;

		recover_available_data_failure(&mut virtual_overseer).await;
		fetch_validation_code(&mut virtual_overseer).await;

		recover_available_data_failure(&mut virtual_overseer).await;
		fetch_validation_code(&mut virtual_overseer).await;

		recover_available_data_failure(&mut virtual_overseer).await;
		fetch_validation_code(&mut virtual_overseer).await;

		recover_available_data_failure(&mut virtual_overseer).await;
		fetch_validation_code(&mut virtual_overseer).await;

		recover_available_data_failure(&mut virtual_overseer).await;
		fetch_validation_code(&mut virtual_overseer).await;

		recover_available_data(&mut virtual_overseer).await;
		fetch_validation_code(&mut virtual_overseer).await;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::CandidateValidation(CandidateValidationMessage::ValidateFromExhaustive {
				exec_kind,
				response_sender,
				..
			}) if exec_kind == PvfExecKind::Approval => {
				response_sender.send(Ok(ValidationResult::Valid(Default::default(), Default::default())))
					.unwrap();
			}
		);

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ApprovalDistribution(ApprovalDistributionMessage::DistributeApproval(_))
		);

		// Assert that there are no more messages being sent by the subsystem
		assert!(overseer_recv(&mut virtual_overseer).timeout(TIMEOUT / 2).await.is_none());

		virtual_overseer
	});
}

// Test that cached approvals, which are candidates that we approved but we didn't issue
// the signature yet because we want to coalesce it with more candidate are sent after restart.
#[test]
fn subsystem_sends_pending_approvals_on_approval_restart() {
	let assignment_criteria = Box::new(MockAssignmentCriteria(
		|| {
			let mut assignments = HashMap::new();
			let _ = assignments.insert(
				CoreIndex(0),
				approval_db::v2::OurAssignment {
					cert: garbage_assignment_cert_v2(AssignmentCertKindV2::RelayVRFModuloCompact {
						core_bitfield: CoreIndex(0).into(),
					}),
					tranche: 0,
					validator_index: ValidatorIndex(0),
					triggered: false,
				}
				.into(),
			);

			let _ = assignments.insert(
				CoreIndex(0),
				approval_db::v2::OurAssignment {
					cert: garbage_assignment_cert_v2(AssignmentCertKindV2::RelayVRFModuloCompact {
						core_bitfield: vec![CoreIndex(0), CoreIndex(1), CoreIndex(2)]
							.try_into()
							.unwrap(),
					}),
					tranche: 0,
					validator_index: ValidatorIndex(0),
					triggered: false,
				}
				.into(),
			);
			assignments
		},
		|_| Ok(0),
	));
	let config = HarnessConfigBuilder::default().assignment_criteria(assignment_criteria).build();
	let store = config.backend();
	let store_clone = config.backend();

	test_harness(config, |test_harness| async move {
		let TestHarness { mut virtual_overseer, clock, sync_oracle_handle } = test_harness;

		setup_overseer_with_two_blocks_each_with_one_assignment_triggered(
			&mut virtual_overseer,
			store,
			&clock,
			sync_oracle_handle,
			ApprovalVotingParams { max_approval_coalesce_count: 6 },
		)
		.await;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ApprovalDistribution(ApprovalDistributionMessage::DistributeAssignment(
				_,
				_,
			)) => {
			}
		);

		recover_available_data(&mut virtual_overseer).await;
		fetch_validation_code(&mut virtual_overseer).await;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ApprovalDistribution(ApprovalDistributionMessage::DistributeAssignment(
				_,
				_
			)) => {
			}
		);

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::CandidateValidation(CandidateValidationMessage::ValidateFromExhaustive {
				exec_kind,
				response_sender,
				..
			}) if exec_kind == PvfExecKind::Approval => {
				response_sender.send(Ok(ValidationResult::Valid(Default::default(), Default::default())))
					.unwrap();
			}
		);

		// Assert that there are no more messages being sent by the subsystem
		assert!(overseer_recv(&mut virtual_overseer).timeout(TIMEOUT / 2).await.is_none());

		virtual_overseer
	});

	let config = HarnessConfigBuilder::default().backend(store_clone).major_syncing(true).build();
	// On restart signatures should be sent to approval-distribution without relaunching the
	// approval work.
	test_harness(config, |test_harness| async move {
		let TestHarness { mut virtual_overseer, clock, sync_oracle_handle } = test_harness;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ChainApi(ChainApiMessage::FinalizedBlockNumber(rx)) => {
				rx.send(Ok(0)).unwrap();
			}
		);

		let block_hash = Hash::repeat_byte(0x01);
		let fork_block_hash = Hash::repeat_byte(0x02);
		let candidate_commitments = CandidateCommitments::default();
		let mut candidate_receipt = dummy_candidate_receipt_v2(block_hash);
		candidate_receipt.commitments_hash = candidate_commitments.hash();
		let slot = Slot::from(1);

		clock.inner.lock().set_tick(slot_to_tick(slot + 2));
		let (chain_builder, session_info) = build_chain_with_two_blocks_with_one_candidate_each(
			block_hash,
			fork_block_hash,
			slot,
			sync_oracle_handle,
			candidate_receipt.into(),
			ApprovalVotingParams::default(),
		)
		.await;
		chain_builder.build(&mut virtual_overseer).await;

		futures_timer::Delay::new(Duration::from_millis(2000)).await;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ApprovalDistribution(ApprovalDistributionMessage::NewBlocks(
				_,
			)) => {
			}
		);

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ApprovalDistribution(ApprovalDistributionMessage::DistributeAssignment(
				_,
				_,
			)) => {
			}
		);

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ApprovalDistribution(ApprovalDistributionMessage::DistributeAssignment(
				_,
				_,
			)) => {
			}
		);

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::RuntimeApi(
				RuntimeApiMessage::Request(
					_,
					RuntimeApiRequest::SessionInfo(_, si_tx),
				)
			) => {
				si_tx.send(Ok(Some(session_info.clone()))).unwrap();
			}
		);
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::RuntimeApi(
				RuntimeApiMessage::Request(_, RuntimeApiRequest::NodeFeatures(_, si_tx), )
			) => {
				si_tx.send(Ok(NodeFeatures::EMPTY)).unwrap();
			}
		);

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::ApprovalVotingParams(_, sender))) => {
				let _ = sender.send(Ok(ApprovalVotingParams::default()));
			}
		);

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ApprovalDistribution(ApprovalDistributionMessage::DistributeApproval(_))
		);

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ApprovalDistribution(ApprovalDistributionMessage::DistributeApproval(_))
		);

		// Assert that there are no more messages being sent by the subsystem
		assert!(overseer_recv(&mut virtual_overseer).timeout(TIMEOUT / 2).await.is_none());

		virtual_overseer
	});
}

// Test that after restart approvals are sent after all assignments have been distributed.
#[test]
fn subsystem_sends_assignment_approval_in_correct_order_on_approval_restart() {
	let assignment_criteria = Box::new(MockAssignmentCriteria(
		|| {
			let mut assignments = HashMap::new();

			let _ = assignments.insert(
				CoreIndex(0),
				approval_db::v2::OurAssignment {
					cert: garbage_assignment_cert_v2(AssignmentCertKindV2::RelayVRFModuloCompact {
						core_bitfield: vec![CoreIndex(0), CoreIndex(2)].try_into().unwrap(),
					}),
					tranche: 0,
					validator_index: ValidatorIndex(0),
					triggered: false,
				}
				.into(),
			);

			let _ = assignments.insert(
				CoreIndex(1),
				approval_db::v2::OurAssignment {
					cert: garbage_assignment_cert_v2(AssignmentCertKindV2::RelayVRFDelay {
						core_index: CoreIndex(1),
					}),
					tranche: 0,
					validator_index: ValidatorIndex(0),
					triggered: false,
				}
				.into(),
			);
			assignments
		},
		|_| Ok(0),
	));
	let config = HarnessConfigBuilder::default().assignment_criteria(assignment_criteria).build();
	let store = config.backend();
	let store_clone = config.backend();

	test_harness(config, |test_harness| async move {
		let TestHarness { mut virtual_overseer, clock, sync_oracle_handle } = test_harness;

		setup_overseer_with_blocks_with_two_assignments_triggered(
			&mut virtual_overseer,
			store,
			&clock,
			sync_oracle_handle,
			ApprovalVotingParams { max_approval_coalesce_count: 2 },
		)
		.await;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ApprovalDistribution(ApprovalDistributionMessage::DistributeAssignment(
				_,
				_,
			)) => {
			}
		);

		recover_available_data(&mut virtual_overseer).await;
		fetch_validation_code(&mut virtual_overseer).await;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ApprovalDistribution(ApprovalDistributionMessage::DistributeAssignment(
				_,
				_
			)) => {
			}
		);

		recover_available_data(&mut virtual_overseer).await;
		fetch_validation_code(&mut virtual_overseer).await;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::CandidateValidation(CandidateValidationMessage::ValidateFromExhaustive {
				exec_kind,
				response_sender,
				..
			}) if exec_kind == PvfExecKind::Approval => {
				response_sender.send(Ok(ValidationResult::Valid(Default::default(), Default::default())))
					.unwrap();
			}
		);

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::CandidateValidation(CandidateValidationMessage::ValidateFromExhaustive {
				exec_kind,
				response_sender,
				..
			}) if exec_kind == PvfExecKind::Approval => {
				response_sender.send(Ok(ValidationResult::Valid(Default::default(), Default::default())))
					.unwrap();
			}
		);

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ApprovalDistribution(ApprovalDistributionMessage::DistributeApproval(_))
		);

		// Assert that there are no more messages being sent by the subsystem
		assert!(overseer_recv(&mut virtual_overseer).timeout(TIMEOUT / 2).await.is_none());

		virtual_overseer
	});

	let config = HarnessConfigBuilder::default().backend(store_clone).major_syncing(true).build();
	// On restart  we should first distribute all assignments covering a coalesced approval.
	test_harness(config, |test_harness| async move {
		let TestHarness { mut virtual_overseer, clock, sync_oracle_handle } = test_harness;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ChainApi(ChainApiMessage::FinalizedBlockNumber(rx)) => {
				rx.send(Ok(0)).unwrap();
			}
		);

		let block_hash = Hash::repeat_byte(0x01);
		let candidate_commitments = CandidateCommitments::default();
		let mut candidate_receipt = dummy_candidate_receipt_v2(block_hash);
		candidate_receipt.commitments_hash = candidate_commitments.hash();

		let mut candidate_commitments2 = CandidateCommitments::default();
		candidate_commitments2.processed_downward_messages = 3;
		let mut candidate_receipt2 = dummy_candidate_receipt_v2(block_hash);
		candidate_receipt2.commitments_hash = candidate_commitments2.hash();

		let slot = Slot::from(1);

		clock.inner.lock().set_tick(slot_to_tick(slot + 2));
		let (chain_builder, _session_info) = build_chain_with_block_with_two_candidates(
			block_hash,
			slot,
			sync_oracle_handle,
			vec![candidate_receipt.into(), candidate_receipt2.into()],
			ApprovalVotingParams { max_approval_coalesce_count: 2 },
		)
		.await;
		chain_builder.build(&mut virtual_overseer).await;

		futures_timer::Delay::new(Duration::from_millis(2000)).await;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ApprovalDistribution(ApprovalDistributionMessage::NewBlocks(
				_,
			)) => {
			}
		);

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ApprovalDistribution(ApprovalDistributionMessage::DistributeAssignment(
				_,
				_,
			)) => {
			}
		);

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ApprovalDistribution(ApprovalDistributionMessage::DistributeAssignment(
				_,
				_,
			)) => {
			}
		);

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ApprovalDistribution(ApprovalDistributionMessage::DistributeApproval(approval)) => {
				assert_eq!(approval.candidate_indices.count_ones(), 2);
			}
		);

		// Assert that there are no more messages being sent by the subsystem
		assert!(overseer_recv(&mut virtual_overseer).timeout(TIMEOUT / 2).await.is_none());

		virtual_overseer
	});
}

// Test that if the subsystem missed the triggering of some tranches because it was not running
// it launches the missed assignements on restart.
#[test]
fn subsystem_launches_missed_assignments_on_restart() {
	let test_tranche = 20;
	let assignment_criteria = Box::new(MockAssignmentCriteria(
		move || {
			let mut assignments = HashMap::new();
			let _ = assignments.insert(
				CoreIndex(0),
				approval_db::v2::OurAssignment {
					cert: garbage_assignment_cert_v2(AssignmentCertKindV2::RelayVRFDelay {
						core_index: CoreIndex(0),
					}),
					tranche: test_tranche,
					validator_index: ValidatorIndex(0),
					triggered: false,
				}
				.into(),
			);

			assignments
		},
		|_| Ok(0),
	));
	let config = HarnessConfigBuilder::default().assignment_criteria(assignment_criteria).build();
	let store = config.backend();
	let store_clone = config.backend();

	test_harness(config, |test_harness| async move {
		let TestHarness { mut virtual_overseer, clock, sync_oracle_handle } = test_harness;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ChainApi(ChainApiMessage::FinalizedBlockNumber(rx)) => {
				rx.send(Ok(0)).unwrap();
			}
		);

		let block_hash = Hash::repeat_byte(0x01);
		let fork_block_hash = Hash::repeat_byte(0x02);
		let candidate_commitments = CandidateCommitments::default();
		let mut candidate_receipt = dummy_candidate_receipt_v2(block_hash);
		candidate_receipt.commitments_hash = candidate_commitments.hash();
		let candidate_hash = candidate_receipt.hash();
		let slot = Slot::from(1);
		let (chain_builder, _session_info) = build_chain_with_two_blocks_with_one_candidate_each(
			block_hash,
			fork_block_hash,
			slot,
			sync_oracle_handle,
			candidate_receipt,
			ApprovalVotingParams::default(),
		)
		.await;
		chain_builder.build(&mut virtual_overseer).await;

		assert!(!clock.inner.lock().current_wakeup_is(1));
		clock.inner.lock().wakeup_all(1);

		assert!(clock.inner.lock().current_wakeup_is(slot_to_tick(slot) + test_tranche as u64));
		clock.inner.lock().wakeup_all(slot_to_tick(slot));

		futures_timer::Delay::new(Duration::from_millis(200)).await;

		clock.inner.lock().wakeup_all(slot_to_tick(slot + 2));

		assert_eq!(clock.inner.lock().wakeups.len(), 0);

		futures_timer::Delay::new(Duration::from_millis(200)).await;

		let candidate_entry = store.load_candidate_entry(&candidate_hash).unwrap().unwrap();
		let our_assignment =
			candidate_entry.approval_entry(&block_hash).unwrap().our_assignment().unwrap();
		assert!(!our_assignment.triggered());

		// Assignment is not triggered because its tranches has not been reached.
		virtual_overseer
	});

	// Restart a new approval voting subsystem with the same database and major syncing true until
	// the last leaf.
	let config = HarnessConfigBuilder::default().backend(store_clone).major_syncing(true).build();

	test_harness(config, |test_harness| async move {
		let TestHarness { mut virtual_overseer, clock, sync_oracle_handle } = test_harness;
		let slot = Slot::from(1);
		// 1. Set the clock to the to a tick past the tranche where the assignment should be
		//    triggered.
		clock.inner.lock().set_tick(slot_to_tick(slot) + 2 * test_tranche as u64);
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ChainApi(ChainApiMessage::FinalizedBlockNumber(rx)) => {
				rx.send(Ok(0)).unwrap();
			}
		);

		let block_hash = Hash::repeat_byte(0x01);
		let fork_block_hash = Hash::repeat_byte(0x02);
		let candidate_commitments = CandidateCommitments::default();
		let mut candidate_receipt = dummy_candidate_receipt_v2(block_hash);
		candidate_receipt.commitments_hash = candidate_commitments.hash();
		let (chain_builder, session_info) = build_chain_with_two_blocks_with_one_candidate_each(
			block_hash,
			fork_block_hash,
			slot,
			sync_oracle_handle,
			candidate_receipt,
			ApprovalVotingParams::default(),
		)
		.await;

		chain_builder.build(&mut virtual_overseer).await;

		futures_timer::Delay::new(Duration::from_millis(2000)).await;

		// On major syncing ending Approval voting should send all the necessary messages for a
		// candidate to be approved.
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ApprovalDistribution(ApprovalDistributionMessage::NewBlocks(
				_,
			)) => {
			}
		);

		clock
			.inner
			.lock()
			.wakeup_all(slot_to_tick(slot) + 2 * test_tranche as u64 + RESTART_WAKEUP_DELAY - 1);

		// Subsystem should not send any messages because the assignment is not triggered yet.
		assert!(overseer_recv(&mut virtual_overseer).timeout(TIMEOUT / 2).await.is_none());

		// Set the clock to the tick where the assignment should be triggered.
		clock
			.inner
			.lock()
			.wakeup_all(slot_to_tick(slot) + 2 * test_tranche as u64 + RESTART_WAKEUP_DELAY);

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::RuntimeApi(
				RuntimeApiMessage::Request(
					_,
					RuntimeApiRequest::SessionInfo(_, si_tx),
				)
			) => {
				si_tx.send(Ok(Some(session_info.clone()))).unwrap();
			}
		);

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::RuntimeApi(
				RuntimeApiMessage::Request(_, RuntimeApiRequest::NodeFeatures(_, si_tx), )
			) => {
				si_tx.send(Ok(NodeFeatures::EMPTY)).unwrap();
			}
		);

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::ApprovalVotingParams(_, sender))) => {
				let _ = sender.send(Ok(ApprovalVotingParams::default()));
			}
		);

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ApprovalDistribution(ApprovalDistributionMessage::DistributeAssignment(
				_,
				_,
			)) => {
			}
		);

		// Guarantees the approval work has been relaunched.
		recover_available_data(&mut virtual_overseer).await;
		fetch_validation_code(&mut virtual_overseer).await;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::CandidateValidation(CandidateValidationMessage::ValidateFromExhaustive {
				exec_kind,
				response_sender,
				..
			}) if exec_kind == PvfExecKind::Approval => {
				response_sender.send(Ok(ValidationResult::Valid(Default::default(), Default::default())))
					.unwrap();
			}
		);

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ApprovalDistribution(ApprovalDistributionMessage::DistributeApproval(_))
		);

		clock
			.inner
			.lock()
			.wakeup_all(slot_to_tick(slot) + 2 * test_tranche as u64 + RESTART_WAKEUP_DELAY);

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ApprovalDistribution(ApprovalDistributionMessage::DistributeAssignment(
				_,
				_,
			)) => {
			}
		);

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ApprovalDistribution(ApprovalDistributionMessage::DistributeApproval(_))
		);

		// Assert that there are no more messages being sent by the subsystem
		assert!(overseer_recv(&mut virtual_overseer).timeout(TIMEOUT / 2).await.is_none());

		virtual_overseer
	});
}

// Test we correctly update the timer when we mark the beginning of gathering assignments.
#[test]
fn test_gathering_assignments_statements() {
	let mut state = State {
		keystore: Arc::new(LocalKeystore::in_memory()),
		slot_duration_millis: 6_000,
		clock: Arc::new(MockClock::default()),
		assignment_criteria: Box::new(MockAssignmentCriteria::check_only(|_| Ok(0))),
		per_block_assignments_gathering_times: LruMap::new(ByLength::new(
			MAX_BLOCKS_WITH_ASSIGNMENT_TIMESTAMPS,
		)),
		no_show_stats: NoShowStats::default(),
	};

	for i in 0..200i32 {
		state.mark_begining_of_gathering_assignments(
			i as u32,
			Hash::repeat_byte(i as u8),
			CandidateHash(Hash::repeat_byte(i as u8)),
		);
		assert!(
			state.per_block_assignments_gathering_times.len() <=
				MAX_BLOCKS_WITH_ASSIGNMENT_TIMESTAMPS as usize
		);

		assert_eq!(
			state
				.per_block_assignments_gathering_times
				.iter()
				.map(|(block_number, _)| block_number)
				.min(),
			Some(max(0, i - MAX_BLOCKS_WITH_ASSIGNMENT_TIMESTAMPS as i32 + 1) as u32).as_ref()
		)
	}
	assert_eq!(
		state.per_block_assignments_gathering_times.len(),
		MAX_BLOCKS_WITH_ASSIGNMENT_TIMESTAMPS as usize
	);

	let nothing_changes = state
		.per_block_assignments_gathering_times
		.iter()
		.map(|(block_number, _)| *block_number)
		.sorted()
		.collect::<Vec<_>>();

	for i in 150..200i32 {
		state.mark_begining_of_gathering_assignments(
			i as u32,
			Hash::repeat_byte(i as u8),
			CandidateHash(Hash::repeat_byte(i as u8)),
		);
		assert_eq!(
			nothing_changes,
			state
				.per_block_assignments_gathering_times
				.iter()
				.map(|(block_number, _)| *block_number)
				.sorted()
				.collect::<Vec<_>>()
		);
	}

	for i in 110..120 {
		let block_hash = Hash::repeat_byte(i as u8);
		let candidate_hash = CandidateHash(Hash::repeat_byte(i as u8));

		state.mark_gathered_enough_assignments(i as u32, block_hash, candidate_hash);

		assert!(state
			.per_block_assignments_gathering_times
			.get(&i)
			.unwrap()
			.get(&(block_hash, candidate_hash))
			.unwrap()
			.stage_start
			.is_none());
		state.mark_begining_of_gathering_assignments(i as u32, block_hash, candidate_hash);
		let record = state
			.per_block_assignments_gathering_times
			.get(&i)
			.unwrap()
			.get(&(block_hash, candidate_hash))
			.unwrap();

		assert!(record.stage_start.is_some());
		assert_eq!(record.stage, 1);
	}

	state.cleanup_assignments_gathering_timestamp(200);
	assert_eq!(state.per_block_assignments_gathering_times.len(), 0);
}

// Test we note the time we took to transition RequiredTranche  from Pending to Exact and
// that we increase the stage when we transition from Exact to Pending.
#[test]
fn test_observe_assignment_gathering_status() {
	let mut state = State {
		keystore: Arc::new(LocalKeystore::in_memory()),
		slot_duration_millis: 6_000,
		clock: Arc::new(MockClock::default()),
		assignment_criteria: Box::new(MockAssignmentCriteria::check_only(|_| Ok(0))),
		per_block_assignments_gathering_times: LruMap::new(ByLength::new(
			MAX_BLOCKS_WITH_ASSIGNMENT_TIMESTAMPS,
		)),
		no_show_stats: NoShowStats::default(),
	};

	let metrics_inner = MetricsInner {
		imported_candidates_total: IntCounter::new("dummy", "dummy").unwrap(),
		assignments_produced: Histogram::with_opts(HistogramOpts::new("dummy", "dummy")).unwrap(),
		approvals_produced_total: IntCounterVec::new(Opts::new("dummy", "dummy"), &["dummy"])
			.unwrap(),
		no_shows_total: IntCounter::new("dummy", "dummy").unwrap(),
		observed_no_shows: IntCounter::new("dummy", "dummy").unwrap(),
		approved_by_one_third: IntCounter::new("dummy", "dummy").unwrap(),
		wakeups_triggered_total: IntCounter::new("dummy", "dummy").unwrap(),
		coalesced_approvals_buckets: Histogram::with_opts(HistogramOpts::new("dummy", "dummy"))
			.unwrap(),
		coalesced_approvals_delay: Histogram::with_opts(HistogramOpts::new("dummy", "dummy"))
			.unwrap(),
		candidate_approval_time_ticks: Histogram::with_opts(HistogramOpts::new("dummy", "dummy"))
			.unwrap(),
		block_approval_time_ticks: Histogram::with_opts(HistogramOpts::new("dummy", "dummy"))
			.unwrap(),
		time_db_transaction: Histogram::with_opts(HistogramOpts::new("dummy", "dummy")).unwrap(),
		time_recover_and_approve: Histogram::with_opts(HistogramOpts::new("dummy", "dummy"))
			.unwrap(),
		candidate_signatures_requests_total: IntCounter::new("dummy", "dummy").unwrap(),
		unapproved_candidates_in_unfinalized_chain: prometheus::Gauge::<prometheus::U64>::new(
			"dummy", "dummy",
		)
		.unwrap(),
		assignments_gathering_time_by_stage: HistogramVec::new(
			HistogramOpts::new("test", "test"),
			&["stage"],
		)
		.unwrap(),
	};

	let metrics = Metrics(Some(metrics_inner));
	let block_hash = Hash::repeat_byte(1);
	let candidate_hash = CandidateHash(Hash::repeat_byte(1));
	let block_number = 1;

	// Transition from Pending to Exact and check stage 0 time is recorded.
	state.observe_assignment_gathering_status(
		&metrics,
		&RequiredTranches::Pending {
			considered: 0,
			next_no_show: None,
			maximum_broadcast: 0,
			clock_drift: 0,
		},
		block_hash,
		block_number,
		candidate_hash,
	);

	state.observe_assignment_gathering_status(
		&metrics,
		&RequiredTranches::Exact {
			needed: 2,
			tolerated_missing: 2,
			next_no_show: None,
			last_assignment_tick: None,
		},
		block_hash,
		block_number,
		candidate_hash,
	);

	let value = metrics
		.0
		.as_ref()
		.unwrap()
		.assignments_gathering_time_by_stage
		.get_metric_with_label_values(&["0"])
		.unwrap();

	assert_eq!(value.get_sample_count(), 1);

	// Transition from Exact to Pending to Exact and check stage 1 time is recorded.
	state.observe_assignment_gathering_status(
		&metrics,
		&RequiredTranches::Pending {
			considered: 0,
			next_no_show: None,
			maximum_broadcast: 0,
			clock_drift: 0,
		},
		block_hash,
		block_number,
		candidate_hash,
	);

	state.observe_assignment_gathering_status(
		&metrics,
		&RequiredTranches::Exact {
			needed: 2,
			tolerated_missing: 2,
			next_no_show: None,
			last_assignment_tick: None,
		},
		block_hash,
		block_number,
		candidate_hash,
	);

	let value = metrics
		.0
		.as_ref()
		.unwrap()
		.assignments_gathering_time_by_stage
		.get_metric_with_label_values(&["0"])
		.unwrap();

	assert_eq!(value.get_sample_count(), 1);

	let value = metrics
		.0
		.as_ref()
		.unwrap()
		.assignments_gathering_time_by_stage
		.get_metric_with_label_values(&["1"])
		.unwrap();

	assert_eq!(value.get_sample_count(), 1);
}
