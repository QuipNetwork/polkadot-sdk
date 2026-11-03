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

//! Subscription logic for statement store.
//!
//! Manages subscriptions to statement topics and notifies subscribers when new statements arrive.
//! Uses multiple matcher tasks to handle subscriptions concurrently, each responsible for a subset
//! of subscriptions. Each matcher task maintains its own list of subscriptions and matches incoming
//! statements against them. When a new statement is submitted, it is sent to all matcher tasks for
//! processing. If a statement matches a subscription's filter, it is sent to the subscriber via an
//! async channel.
//!
//! This design allows for efficient handling of a large number of subscriptions and statements and
//! can be scaled by adjusting the number of matcher tasks.

// Buffer size for the matcher task channels, to backpressure the submission senders.
// This value is generous to allow for bursts of statements without dropping any or backpressuring
// too early.
const MATCHERS_TASK_CHANNEL_BUFFER_SIZE: usize = 80_000;

// Buffer size for individual subscriptions.
const SUBSCRIPTION_BUFFER_SIZE: usize = 128;
const STOP_RESERVE_CHANNEL_SLOTS: usize = 1;

/// Maximum number of active filters attached to one statement subscription.
///
/// Keeps one subscription useful for multiplexing, while bounding internal per-event filter-id
/// metadata to 128 `u64`s, i.e. 1 KiB before `Vec` overhead.
pub const MAX_FILTERS_PER_SUBSCRIPTION: usize = 128;
// Keep replay batches bounded by raw statement bytes. The JSON response is roughly twice this size
// because statements are hex-encoded.
pub(crate) const REPLAY_CHUNK_RAW_BYTES: usize = 4 * 1024 * 1024;
use futures::{Stream, StreamExt};
use itertools::Itertools;
use parking_lot::Mutex;

use crate::LOG_TARGET;
use sc_utils::id_sequence::SeqID;
use sp_core::{traits::SpawnNamed, Bytes, Encode};
pub use sp_statement_store::StatementStore;
use sp_statement_store::{
	FilterId, LiveStatementEvent, OptimizedTopicFilter, Result, Statement, StatementEvent, Topic,
	MAX_TOPICS,
};
use std::{
	collections::{hash_map::Entry, HashMap, HashSet, VecDeque},
	sync::{atomic::AtomicU64, Arc},
};

/// Error returned when attaching a filter to a multi-filter subscription fails
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddFilterError {
	/// The subscription already has the maximum number of active filters
	LimitReached,
	/// The filter could not be registered: the store or the matcher is gone, or the subscription
	/// is no longer active
	Stopped,
}

/// Trait for initiating statement store subscriptions from the RPC module.
pub trait StatementStoreSubscriptionApi: Send + Sync {
	/// Subscribe to statements matching the topic filter.
	///
	/// Returns existing matching statements, a sender channel to send matched statements and a
	/// stream for receiving matched statements when they arrive.
	fn subscribe_statement(
		&self,
		topic_filter: OptimizedTopicFilter,
	) -> Result<(Vec<Vec<u8>>, async_channel::Sender<StatementEvent>, SubscriptionStatementsStream)>;
}

/// Creates multi-filter subscriptions for the RPC module
pub trait MultiFilterSubscriptionApi: Send + Sync {
	/// Creates an empty subscription that can receive filters dynamically
	fn create_subscription(&self) -> (SubscriptionHandle, MultiFilterEventStream);
}

/// Provides cursor-based replay for dynamically attached filters.
pub(crate) trait ReplaySnapshotProvider: Send + Sync {
	/// Capture the committed admission watermark and enqueue filter registration atomically with
	/// respect to submit.
	/// Returns the registered watermark, or `None` when registration could not be enqueued.
	///
	/// `enqueue` is called at most once, with the submit lock held: it must not block or call
	/// back into the store.
	fn register_replay(&self, enqueue: &mut dyn FnMut(u64) -> bool) -> Result<Option<u64>>;

	/// Read the next replay batch from the persisted admission sequence.
	fn replay_batch(
		&self,
		filter: &OptimizedTopicFilter,
		cursor: u64,
		watermark: u64,
	) -> Result<ReplayBatch>;
}

pub(crate) struct ReplayBatch {
	pub(crate) statements: Vec<Vec<u8>>,
	pub(crate) cursor: u64,
	pub(crate) done: bool,
}

/// A handle that attaches, removes, and inspects filters for one multi-filter subscription
#[derive(Clone)]
pub struct SubscriptionHandle {
	pub(crate) sub_id: SeqID,
	pub(crate) inner: Arc<Mutex<SubscriptionHandleInner>>,
	pub(crate) matchers: SubscriptionsMatchersHandlers,
	pub(crate) snapshot_provider: Arc<dyn ReplaySnapshotProvider>,
}

pub(crate) struct SubscriptionHandleInner {
	active_filter_ids: HashSet<FilterId>,
	next_filter_id: u64,
}

impl SubscriptionHandleInner {
	pub(crate) fn new() -> Self {
		Self { active_filter_ids: HashSet::new(), next_filter_id: 0 }
	}
}

impl SubscriptionHandle {
	/// Attaches a filter and returns its id
	pub fn add_filter(
		&self,
		filter: OptimizedTopicFilter,
	) -> std::result::Result<FilterId, AddFilterError> {
		let mut inner = self.inner.lock();
		if inner.active_filter_ids.len() >= MAX_FILTERS_PER_SUBSCRIPTION {
			return Err(AddFilterError::LimitReached);
		}

		let filter_id = FilterId::new(inner.next_filter_id);
		let sub_id = self.sub_id;

		let (result_tx, result_rx) = async_channel::bounded(1);
		let mut result_tx = Some(result_tx);
		let Some(_watermark) = self
			.snapshot_provider
			.register_replay(&mut |watermark| {
				let result_tx =
					result_tx.take().expect("register_replay invokes enqueue at most once; qed");
				self.matchers
					.try_send_by_seq_id(
						sub_id,
						MatcherMessage::RegisterFilter {
							sub_id,
							filter_id,
							filter: filter.clone(),
							watermark,
							result_tx,
						},
					)
					.is_ok()
			})
			.map_err(|_| AddFilterError::Stopped)?
		else {
			return Err(AddFilterError::Stopped);
		};
		match result_rx.recv_blocking() {
			Ok(result) => result?,
			Err(_) => return Err(AddFilterError::Stopped),
		}
		inner.next_filter_id = inner.next_filter_id.wrapping_add(1);
		inner.active_filter_ids.insert(filter_id);
		Ok(filter_id)
	}

	/// Removes a filter from this subscription
	pub fn remove_filter(&self, filter_id: FilterId) -> bool {
		let mut inner = self.inner.lock();
		if !inner.active_filter_ids.remove(&filter_id) {
			return false;
		}
		self.matchers.send_by_seq_id(
			self.sub_id,
			MatcherMessage::RemoveFilter { sub_id: self.sub_id, filter_id },
		);
		true
	}
}

struct PendingReplay {
	filter_id: FilterId,
	filter: OptimizedTopicFilter,
	cursor: u64,
	watermark: u64,
}

pub(crate) struct MultiFilterSubscriptionState {
	pending_replays: VecDeque<PendingReplay>,
	stopped: bool,
	stop_emitted: bool,
}

impl MultiFilterSubscriptionState {
	pub(crate) fn new() -> Self {
		Self { pending_replays: VecDeque::new(), stopped: false, stop_emitted: false }
	}

	fn record_filter_added(
		&mut self,
		filter_id: FilterId,
		filter: OptimizedTopicFilter,
		watermark: u64,
	) {
		self.pending_replays
			.push_back(PendingReplay { filter_id, filter, cursor: 0, watermark });
	}

	fn record_filter_removed(&mut self, filter_id: FilterId) {
		self.pending_replays.retain(|replay| replay.filter_id != filter_id);
	}

	fn next_event(
		&mut self,
		snapshot_provider: &dyn ReplaySnapshotProvider,
	) -> Option<MultiFilterSubscriptionEvent> {
		if !self.stopped {
			if let Some(event) = self.next_replay_event(snapshot_provider) {
				return Some(event);
			}
		}

		if self.stopped && !self.stop_emitted {
			self.stop_emitted = true;
			return Some(MultiFilterSubscriptionEvent::Stop);
		}
		None
	}

	fn next_replay_event(
		&mut self,
		snapshot_provider: &dyn ReplaySnapshotProvider,
	) -> Option<MultiFilterSubscriptionEvent> {
		let replay = self.pending_replays.front_mut()?;
		let filter_id = replay.filter_id;
		if replay.cursor >= replay.watermark {
			self.pending_replays.pop_front();
			return Some(MultiFilterSubscriptionEvent::ReplayDone { filter_id });
		}
		let batch =
			match snapshot_provider.replay_batch(&replay.filter, replay.cursor, replay.watermark) {
				Ok(batch) => batch,
				Err(e) => {
					log::error!(
						target: LOG_TARGET,
						"Stopping subscription: replaying filter {:?} from {} failed: {:?}",
						filter_id,
						replay.cursor,
						e
					);
					self.stopped = true;
					return None;
				},
			};
		replay.cursor = batch.cursor;
		if batch.done && batch.statements.is_empty() {
			self.pending_replays.pop_front();
			return Some(MultiFilterSubscriptionEvent::ReplayDone { filter_id });
		}
		Some(MultiFilterSubscriptionEvent::ReplayStatements {
			filter_id,
			statements: batch.statements,
		})
	}

	fn new_statement_event(
		&mut self,
		hash: sp_statement_store::Hash,
		encoded: Vec<u8>,
		filter_ids: &HashSet<FilterId>,
	) -> Option<MultiFilterSubscriptionEvent> {
		let matched_filter_ids: Vec<FilterId> = filter_ids.iter().copied().collect();

		if matched_filter_ids.is_empty() {
			return None;
		}

		Some(MultiFilterSubscriptionEvent::NewStatement(LiveStatementEvent {
			hash,
			encoded,
			matched_filter_ids,
		}))
	}
}

/// Event emitted by a multi-filter subscription
#[derive(Debug, Clone)]
pub enum MultiFilterSubscriptionEvent {
	/// Replay statements for a newly attached filter
	ReplayStatements {
		/// Filter that produced this replay batch
		filter_id: FilterId,
		/// SCALE-encoded statements included in this replay batch
		statements: Vec<Vec<u8>>,
	},
	/// Replay completed for a newly attached filter
	ReplayDone {
		/// Filter whose replay completed
		filter_id: FilterId,
	},
	/// Live statement event matched one or more active filters
	NewStatement(LiveStatementEvent),
	/// Subscription stopped: the subscriber fell behind its buffer, or replaying a filter failed
	Stop,
}

/// Stream of multi-filter subscription events
pub struct MultiFilterEventStream {
	sub_id: SeqID,
	matchers: SubscriptionsMatchersHandlers,
	rx: async_channel::Receiver<MultiFilterSubscriptionEvent>,
}

impl Stream for MultiFilterEventStream {
	type Item = MultiFilterSubscriptionEvent;

	fn poll_next(
		self: std::pin::Pin<&mut Self>,
		cx: &mut std::task::Context<'_>,
	) -> std::task::Poll<Option<Self::Item>> {
		self.get_mut().rx.poll_next_unpin(cx)
	}
}

impl Drop for MultiFilterEventStream {
	fn drop(&mut self) {
		self.matchers
			.send_by_seq_id(self.sub_id, MatcherMessage::Unsubscribe(self.sub_id));
	}
}

/// Messages sent to matcher tasks.
enum MatcherMessage {
	/// A new statement has been submitted, tagged with the store sequence number.
	NewStatement(u64, Statement),
	/// A new subscription has been created.
	Subscribe { info: IndexedSubscription, tx: async_channel::Sender<StatementEvent> },
	/// A new multi-filter subscription has been created
	SubscribeEmpty {
		seq_id: SeqID,
		snapshot_provider: Arc<dyn ReplaySnapshotProvider>,
		tx: async_channel::Sender<MultiFilterSubscriptionEvent>,
	},
	/// Register a filter at a store watermark. The matcher drains its cursor replay before
	/// processing later live-statement messages from this channel.
	RegisterFilter {
		sub_id: SeqID,
		filter_id: FilterId,
		filter: OptimizedTopicFilter,
		watermark: u64,
		result_tx: async_channel::Sender<std::result::Result<(), AddFilterError>>,
	},
	/// Remove a filter from an existing multi-filter subscription
	RemoveFilter { sub_id: SeqID, filter_id: FilterId },

	/// Unsubscribe the subscription with the given ID.
	Unsubscribe(SeqID),
}

// Handle to manage all subscriptions.
pub struct SubscriptionsHandle {
	// Sequence generator for subscription IDs, atomic for thread safety.
	// Subscription creation is expensive enough that we don't worry about overflow here.
	id_sequence: AtomicU64,
	//  Subscriptions matchers handlers.
	matchers: SubscriptionsMatchersHandlers,
}

impl SubscriptionsHandle {
	/// Create a new SubscriptionsHandle with the given task spawner and number of filter workers.
	pub(crate) fn new(
		task_spawner: Box<dyn SpawnNamed>,
		num_matcher_workers: usize,
	) -> SubscriptionsHandle {
		let mut subscriptions_matchers_senders = Vec::with_capacity(num_matcher_workers);

		for task in 0..num_matcher_workers {
			let (subscription_matcher_sender, subscription_matcher_receiver) =
				async_channel::bounded(MATCHERS_TASK_CHANNEL_BUFFER_SIZE);
			subscriptions_matchers_senders.push(subscription_matcher_sender);
			task_spawner.spawn_blocking(
				"statement-store-subscription-filters",
				Some("statement-store"),
				Box::pin(async move {
					let mut subscriptions = SubscriptionsInfo::new();
					log::debug!(
						target: LOG_TARGET,
						"Started statement subscription matcher task: {task}"
					);
					loop {
						let res = subscription_matcher_receiver.recv().await;
						match res {
							Ok(MatcherMessage::NewStatement(seq, statement)) => {
								subscriptions.notify_matching_filters(seq, &statement);
							},
							Ok(MatcherMessage::Subscribe { info, tx }) => {
								subscriptions.subscribe(info, tx);
							},
							Ok(MatcherMessage::SubscribeEmpty {
								seq_id,
								snapshot_provider,
								tx,
							}) => {
								subscriptions.subscribe_empty(seq_id, snapshot_provider, tx);
							},
							Ok(MatcherMessage::RegisterFilter {
								sub_id,
								filter_id,
								filter,
								watermark,
								result_tx,
							}) => {
								let result = subscriptions
									.register_filter(sub_id, filter_id, filter, watermark);
								let registered = result.is_ok();
								let _ = result_tx.try_send(result);
								if registered {
									subscriptions.drain_ready_events(sub_id);
								}
							},
							Ok(MatcherMessage::RemoveFilter { sub_id, filter_id }) => {
								subscriptions.remove_filter(sub_id, filter_id);
							},
							Ok(MatcherMessage::Unsubscribe(seq_id)) => {
								subscriptions.unsubscribe(seq_id);
							},
							Err(_) => {
								// Expected when the subscription manager is dropped at shutdown.
								log::debug!(
									target: LOG_TARGET,
									"Statement subscription matcher channel closed: {task}"
								);
								break;
							},
						};
					}
				}),
			);
		}
		SubscriptionsHandle {
			id_sequence: AtomicU64::new(0),
			matchers: SubscriptionsMatchersHandlers::new(subscriptions_matchers_senders),
		}
	}

	// Generate the next unique subscription ID.
	fn next_id(&self) -> SeqID {
		let id = self.id_sequence.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
		SeqID::from(id)
	}

	/// Subscribe to statements matching the topic filter.
	pub(crate) fn subscribe(
		&self,
		topic_filter: OptimizedTopicFilter,
		watermark: u64,
	) -> (async_channel::Sender<StatementEvent>, SubscriptionStatementsStream) {
		let next_id = self.next_id();
		let (tx, rx) = async_channel::bounded(SUBSCRIPTION_BUFFER_SIZE);
		let subscription_info = IndexedSubscription {
			topic_filter: topic_filter.clone(),
			seq_id: next_id,
			filter_key: SubscriptionFilterKey::Fixed,
			watermark,
		};
		let subscription_tx = tx.clone();

		let result = (
			tx,
			SubscriptionStatementsStream {
				rx,
				sub_id: subscription_info.seq_id,
				matchers: self.matchers.clone(),
			},
		);

		self.matchers.send_by_seq_id(
			subscription_info.seq_id,
			MatcherMessage::Subscribe { info: subscription_info, tx: subscription_tx },
		);
		result
	}

	pub(crate) fn subscribe_empty(
		&self,
		snapshot_provider: Arc<dyn ReplaySnapshotProvider>,
	) -> (SeqID, MultiFilterEventStream) {
		let sub_id = self.next_id();
		let (tx, rx) =
			async_channel::bounded(SUBSCRIPTION_BUFFER_SIZE + STOP_RESERVE_CHANNEL_SLOTS);
		self.matchers.send_by_seq_id(
			sub_id,
			MatcherMessage::SubscribeEmpty { seq_id: sub_id, snapshot_provider, tx },
		);

		let stream = MultiFilterEventStream { sub_id, matchers: self.matchers.clone(), rx };
		(sub_id, stream)
	}

	pub(crate) fn notify(&self, seq: u64, statement: Statement) {
		self.matchers.send_all(seq, statement);
	}

	pub(crate) fn matchers(&self) -> SubscriptionsMatchersHandlers {
		self.matchers.clone()
	}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
enum SubscriptionFilterKey {
	Fixed,
	Dynamic(FilterId),
}

enum SubscriptionRecord {
	SingleFilter {
		tx: async_channel::Sender<StatementEvent>,
		filter: OptimizedTopicFilter,
	},
	MultiFilter {
		filters: HashMap<FilterId, OptimizedTopicFilter>,
		state: MultiFilterSubscriptionState,
		tx: async_channel::Sender<MultiFilterSubscriptionEvent>,
		snapshot_provider: Arc<dyn ReplaySnapshotProvider>,
	},
}

enum MatchedSubscription {
	Statements,
	Live(HashSet<FilterId>),
}

enum ReadyEventDelivery {
	Continue,
	Stopped,
	Closed,
}

enum PostLiveSendAction {
	None,
	Unsubscribe,
}

type IndexedSubscriptionKey = (SeqID, SubscriptionFilterKey);

// Information about all subscriptions.
// Each matcher task will have its own instance of this struct.
struct SubscriptionsInfo {
	// Subscriptions organized by topic for MatchAll filters.
	//
	// Maps each topic to an array of HashMaps, where the array is indexed by
	// `(number_of_topics_in_filter - 1)`. For example, a subscription requiring
	// topics [A, B] (2 topics) will be stored at index 1 under both topic A and B.
	//
	// This structure allows efficient matching: when a statement arrives with N topics,
	// we only need to check subscriptions that require exactly N or fewer topics.
	subscriptions_match_all_by_topic:
		HashMap<Topic, [HashMap<IndexedSubscriptionKey, IndexedSubscription>; MAX_TOPICS]>,
	// Subscriptions organized by topic for MatchAny filters.
	subscriptions_match_any_by_topic:
		HashMap<Topic, HashMap<IndexedSubscriptionKey, IndexedSubscription>>,
	// Subscriptions that listen with Any filter (i.e., no topic filtering).
	subscriptions_any: HashMap<IndexedSubscriptionKey, IndexedSubscription>,
	// Mapping from subscription ID to subscription state.
	by_sub_id: HashMap<SeqID, SubscriptionRecord>,
}

// Information about one indexed subscription filter.
#[derive(Clone, Debug)]
struct IndexedSubscription {
	// The filter used for this subscription.
	topic_filter: OptimizedTopicFilter,
	// The unique ID of this subscription.
	seq_id: SeqID,
	// The filter key within the subscription.
	filter_key: SubscriptionFilterKey,
	// Store sequence-number boundary captured when this entry was registered: at subscription
	// creation for a fixed filter, at `add_filter` for a dynamic one.
	watermark: u64,
}

impl SubscriptionsInfo {
	fn new() -> SubscriptionsInfo {
		SubscriptionsInfo {
			subscriptions_match_all_by_topic: HashMap::new(),
			subscriptions_match_any_by_topic: HashMap::new(),
			subscriptions_any: HashMap::new(),
			by_sub_id: HashMap::new(),
		}
	}

	// Subscribe a new subscription.
	fn subscribe(
		&mut self,
		subscription_info: IndexedSubscription,
		tx: async_channel::Sender<StatementEvent>,
	) {
		self.by_sub_id.insert(
			subscription_info.seq_id,
			SubscriptionRecord::SingleFilter { tx, filter: subscription_info.topic_filter.clone() },
		);
		self.index_filter(subscription_info);
	}

	fn subscribe_empty(
		&mut self,
		seq_id: SeqID,
		snapshot_provider: Arc<dyn ReplaySnapshotProvider>,
		tx: async_channel::Sender<MultiFilterSubscriptionEvent>,
	) {
		self.by_sub_id.insert(
			seq_id,
			SubscriptionRecord::MultiFilter {
				filters: HashMap::new(),
				state: MultiFilterSubscriptionState::new(),
				tx,
				snapshot_provider,
			},
		);
	}

	#[cfg(test)]
	fn add_filter(
		&mut self,
		sub_id: SeqID,
		filter_id: FilterId,
		filter: OptimizedTopicFilter,
		snapshot_hashes: Vec<sp_statement_store::Hash>,
	) {
		self.add_filter_with_cursor(sub_id, filter_id, filter, snapshot_hashes.len() as u64);
		self.drain_ready_events(sub_id);
	}

	fn register_filter(
		&mut self,
		sub_id: SeqID,
		filter_id: FilterId,
		filter: OptimizedTopicFilter,
		watermark: u64,
	) -> std::result::Result<(), AddFilterError> {
		let Some(SubscriptionRecord::MultiFilter { filters, .. }) = self.by_sub_id.get(&sub_id)
		else {
			return Err(AddFilterError::Stopped);
		};
		if filters.contains_key(&filter_id) {
			return Err(AddFilterError::Stopped);
		}
		let output_closed = matches!(
			self.by_sub_id.get(&sub_id),
			Some(SubscriptionRecord::MultiFilter { tx, .. }) if tx.is_closed()
		);
		if output_closed {
			self.unsubscribe(sub_id);
			return Err(AddFilterError::Stopped);
		}
		self.add_filter_with_cursor(sub_id, filter_id, filter, watermark);
		Ok(())
	}

	fn add_filter_with_cursor(
		&mut self,
		sub_id: SeqID,
		filter_id: FilterId,
		filter: OptimizedTopicFilter,
		watermark: u64,
	) {
		let filter_key = SubscriptionFilterKey::Dynamic(filter_id);
		{
			let Some(SubscriptionRecord::MultiFilter { filters, state, .. }) =
				self.by_sub_id.get_mut(&sub_id)
			else {
				return;
			};

			let Entry::Vacant(entry) = filters.entry(filter_id) else {
				return;
			};
			entry.insert(filter.clone());
			state.record_filter_added(filter_id, filter.clone(), watermark);
		}
		self.index_filter(IndexedSubscription {
			topic_filter: filter,
			seq_id: sub_id,
			filter_key,
			watermark,
		});
	}

	fn remove_filter(&mut self, sub_id: SeqID, filter_id: FilterId) {
		let Some(record) = self.by_sub_id.get_mut(&sub_id) else {
			return;
		};
		let SubscriptionRecord::MultiFilter { filters, state, .. } = record else {
			return;
		};
		let Some(filter) = filters.remove(&filter_id) else {
			return;
		};
		state.record_filter_removed(filter_id);
		self.remove_indexed_filter(sub_id, SubscriptionFilterKey::Dynamic(filter_id), &filter);
		self.drain_ready_events(sub_id);
	}

	fn send_stop(tx: &async_channel::Sender<MultiFilterSubscriptionEvent>) {
		let _ = tx.try_send(MultiFilterSubscriptionEvent::Stop);
		tx.close();
	}

	fn send_ready_event(
		state: &mut MultiFilterSubscriptionState,
		tx: &async_channel::Sender<MultiFilterSubscriptionEvent>,
		event: MultiFilterSubscriptionEvent,
	) -> ReadyEventDelivery {
		if tx.is_closed() {
			return ReadyEventDelivery::Closed;
		}
		if matches!(event, MultiFilterSubscriptionEvent::Stop) {
			Self::send_stop(tx);
			return ReadyEventDelivery::Stopped;
		}
		if tx.len() >= SUBSCRIPTION_BUFFER_SIZE {
			state.stopped = true;
			Self::send_stop(tx);
			return ReadyEventDelivery::Stopped;
		}
		match tx.try_send(event) {
			Ok(()) => ReadyEventDelivery::Continue,
			Err(async_channel::TrySendError::Full(_)) => {
				state.stopped = true;
				Self::send_stop(tx);
				ReadyEventDelivery::Stopped
			},
			Err(async_channel::TrySendError::Closed(_)) => ReadyEventDelivery::Closed,
		}
	}

	fn drain_ready_events(&mut self, sub_id: SeqID) {
		loop {
			let result = {
				let Some(record) = self.by_sub_id.get_mut(&sub_id) else {
					return;
				};
				let SubscriptionRecord::MultiFilter { state, tx, snapshot_provider, .. } = record
				else {
					return;
				};
				let Some(event) = state.next_event(snapshot_provider.as_ref()) else {
					return;
				};
				match Self::send_ready_event(state, tx, event) {
					ReadyEventDelivery::Continue => Ok(()),
					ReadyEventDelivery::Stopped => return,
					ReadyEventDelivery::Closed => Err(()),
				}
			};
			if result.is_err() {
				self.unsubscribe(sub_id);
				return;
			}
		}
	}

	/// Emit a live statement matched for a multi-filter subscription.
	fn deliver_live_statement(
		&mut self,
		sub_id: SeqID,
		hash: sp_statement_store::Hash,
		encoded: Vec<u8>,
		matched_filter_ids: HashSet<FilterId>,
	) {
		let action = {
			let Some(SubscriptionRecord::MultiFilter { state, tx, .. }) =
				self.by_sub_id.get_mut(&sub_id)
			else {
				return;
			};

			match state.new_statement_event(hash, encoded, &matched_filter_ids) {
				Some(event) => match Self::send_ready_event(state, tx, event) {
					ReadyEventDelivery::Closed => PostLiveSendAction::Unsubscribe,
					ReadyEventDelivery::Continue | ReadyEventDelivery::Stopped => {
						PostLiveSendAction::None
					},
				},
				None => PostLiveSendAction::None,
			}
		};

		match action {
			PostLiveSendAction::Unsubscribe => self.unsubscribe(sub_id),
			PostLiveSendAction::None => {},
		}
	}

	fn index_filter(&mut self, subscription_info: IndexedSubscription) {
		let index_key = (subscription_info.seq_id, subscription_info.filter_key);
		match &subscription_info.topic_filter {
			OptimizedTopicFilter::Any => {
				self.subscriptions_any.insert(index_key, subscription_info);
			},
			OptimizedTopicFilter::MatchAll(topics) => {
				for topic in topics {
					self.subscriptions_match_all_by_topic.entry(*topic).or_default()
						[topics.len() - 1]
						.insert(index_key, subscription_info.clone());
				}
			},
			OptimizedTopicFilter::MatchAny(topics) => {
				for topic in topics {
					self.subscriptions_match_any_by_topic
						.entry(*topic)
						.or_default()
						.insert(index_key, subscription_info.clone());
				}
			},
		};
	}

	// Deliver the statement at store sequence number `seq` to every matching subscription.
	// Subscriptions and filters registered above `seq` are skipped: the statement predates them, so
	// it is covered by the subscribe-time snapshot
	// (`StatementStoreSubscriptionApi::subscribe_statement`) for a fixed filter, or by the
	// admission-cursor replay (`ReplaySnapshotProvider::replay_batch`) for a dynamic one.
	fn notify_matching_filters(&mut self, seq: u64, statement: &Statement) {
		let mut matches = HashMap::new();
		self.collect_match_all_subscribers(seq, statement, &mut matches);
		self.collect_match_any_subscribers(seq, statement, &mut matches);
		self.collect_any_subscribers(seq, &mut matches);

		let encoded = statement.encode();
		let bytes_to_send: Bytes = encoded.clone().into();
		let mut needs_unsubscribing = HashSet::new();

		for (sub_id, matched) in matches {
			match matched {
				MatchedSubscription::Statements => {
					let Some(SubscriptionRecord::SingleFilter { tx, .. }) =
						self.by_sub_id.get(&sub_id)
					else {
						continue;
					};
					if let Err(err) = tx.try_send(StatementEvent::NewStatements {
						statements: vec![bytes_to_send.clone()],
						remaining: None,
					}) {
						log::debug!(
							target: LOG_TARGET,
							"Failed to send statement to subscriber {:?}: {:?} unsubscribing it", sub_id, err
						);
						needs_unsubscribing.insert(sub_id);
					}
				},
				MatchedSubscription::Live(filter_ids) if !filter_ids.is_empty() => {
					self.deliver_live_statement(
						sub_id,
						statement.hash(),
						encoded.clone(),
						filter_ids,
					);
				},
				_ => {},
			}
		}

		for sub_id in needs_unsubscribing {
			self.unsubscribe(sub_id);
		}
	}

	// Record a subscription filter as matched, unless the statement's sequence number is below the
	// watermark captured when it was registered (exactly-once delivery: such statements are covered
	// by the subscribe-time snapshot or the admission-cursor replay instead).
	fn record_match(
		matches: &mut HashMap<SeqID, MatchedSubscription>,
		subscription: &IndexedSubscription,
		seq: u64,
	) {
		if seq < subscription.watermark {
			return;
		}
		match subscription.filter_key {
			SubscriptionFilterKey::Fixed => {
				matches.entry(subscription.seq_id).or_insert(MatchedSubscription::Statements);
			},
			SubscriptionFilterKey::Dynamic(filter_id) => {
				let entry = matches
					.entry(subscription.seq_id)
					.or_insert_with(|| MatchedSubscription::Live(HashSet::new()));
				if let MatchedSubscription::Live(filter_ids) = entry {
					filter_ids.insert(filter_id);
				}
			},
		}
	}

	// Collect all subscribers with MatchAny filters that match the given statement.
	fn collect_match_any_subscribers(
		&self,
		seq: u64,
		statement: &Statement,
		matches: &mut HashMap<SeqID, MatchedSubscription>,
	) {
		for statement_topic in statement.topics() {
			if let Some(subscriptions) = self.subscriptions_match_any_by_topic.get(statement_topic)
			{
				for subscription in subscriptions.values() {
					Self::record_match(matches, subscription, seq);
				}
			}
		}
	}

	// Collect all subscribers with MatchAll filters that match the given statement.
	fn collect_match_all_subscribers(
		&self,
		seq: u64,
		statement: &Statement,
		matches: &mut HashMap<SeqID, MatchedSubscription>,
	) {
		let num_topics = statement.topics().len();

		// Check all combinations of topics in the statement to find matching subscriptions.
		// This works well because the maximum allowed topics is small (MAX_TOPICS = 4).
		for num_topics_to_check in 1..=num_topics {
			for topics_combination in statement.topics().iter().combinations(num_topics_to_check) {
				// Find the topic with the fewest subscriptions to minimize the number of checks.
				let Some(Some(topic_with_fewest)) = topics_combination
					.iter()
					.map(|topic| self.subscriptions_match_all_by_topic.get(*topic))
					.min_by_key(|subscriptions| {
						subscriptions.map_or(0, |subscriptions_by_length| {
							subscriptions_by_length[num_topics_to_check - 1].len()
						})
					})
				else {
					continue;
				};

				for subscription in topic_with_fewest[num_topics_to_check - 1]
					.values()
					.filter(|subscription| subscription.topic_filter.matches(statement))
				{
					Self::record_match(matches, subscription, seq);
				}
			}
		}
	}

	// Collect all subscribers that don't filter by topic and want to receive all statements.
	fn collect_any_subscribers(&self, seq: u64, matches: &mut HashMap<SeqID, MatchedSubscription>) {
		for subscription in self.subscriptions_any.values() {
			Self::record_match(matches, subscription, seq);
		}
	}

	// Unsubscribe a subscription by its ID.
	fn unsubscribe(&mut self, id: SeqID) {
		let Some(entry) = self.by_sub_id.remove(&id) else {
			return;
		};

		match entry {
			SubscriptionRecord::SingleFilter { filter, .. } => {
				self.remove_indexed_filter(id, SubscriptionFilterKey::Fixed, &filter);
			},
			SubscriptionRecord::MultiFilter { filters, .. } => {
				for (filter_id, filter) in filters {
					self.remove_indexed_filter(
						id,
						SubscriptionFilterKey::Dynamic(filter_id),
						&filter,
					);
				}
			},
		}
	}

	fn remove_indexed_filter(
		&mut self,
		id: SeqID,
		filter_key: SubscriptionFilterKey,
		filter: &OptimizedTopicFilter,
	) {
		let topics = match filter {
			OptimizedTopicFilter::Any => {
				self.subscriptions_any.remove(&(id, filter_key));
				return;
			},
			OptimizedTopicFilter::MatchAll(topics) => topics,
			OptimizedTopicFilter::MatchAny(topics) => topics,
		};

		// Remove subscription from relevant maps.
		for topic in topics {
			// Check MatchAny map.
			if let Entry::Occupied(mut entry) = self.subscriptions_match_any_by_topic.entry(*topic)
			{
				entry.get_mut().remove(&(id, filter_key));
				if entry.get().is_empty() {
					entry.remove();
				}
			}
			// Check MatchAll map.
			if let Entry::Occupied(mut entry) = self.subscriptions_match_all_by_topic.entry(*topic)
			{
				for subscriptions in entry.get_mut().iter_mut() {
					if subscriptions.remove(&(id, filter_key)).is_some() {
						break;
					}
				}
				if entry.get().iter().all(|s| s.is_empty()) {
					entry.remove();
				}
			}
		}
	}
}

// Handlers to communicate with subscription matcher tasks.
#[derive(Clone)]
pub struct SubscriptionsMatchersHandlers {
	// Channels to send messages to matcher tasks.
	matchers: Vec<async_channel::Sender<MatcherMessage>>,
}

impl SubscriptionsMatchersHandlers {
	/// Create new SubscriptionsMatchersHandlers with the given matcher task senders.
	fn new(matchers: Vec<async_channel::Sender<MatcherMessage>>) -> SubscriptionsMatchersHandlers {
		SubscriptionsMatchersHandlers { matchers }
	}

	// Send a message to the matcher task responsible for the given subscription ID.
	fn send_by_seq_id(&self, id: SeqID, message: MatcherMessage) {
		// If matchers channels are full we backpressure the sender, in this case it will be the
		// processing of new statements.
		if let Err(err) = self.try_send_by_seq_id(id, message) {
			log::error!(
				target: LOG_TARGET,
				"Failed to send statement to matcher task: {:?}", err
			);
		}
	}

	fn try_send_by_seq_id(
		&self,
		id: SeqID,
		message: MatcherMessage,
	) -> std::result::Result<(), async_channel::TrySendError<MatcherMessage>> {
		self.sender_by_seq_id(id).try_send(message)
	}

	fn sender_by_seq_id(&self, id: SeqID) -> async_channel::Sender<MatcherMessage> {
		let index: u64 = id.into();
		self.matchers[index as usize % self.matchers.len()].clone()
	}

	// Send a new statement, tagged with its store sequence number, to all matcher tasks.
	fn send_all(&self, seq: u64, statement: Statement) {
		for sender in &self.matchers {
			if let Err(err) =
				sender.send_blocking(MatcherMessage::NewStatement(seq, statement.clone()))
			{
				log::error!(
					target: LOG_TARGET,
					"Failed to send message to matcher task: {:?}", err
				);
			}
		}
	}
}

// Stream of statements for a subscription.
pub struct SubscriptionStatementsStream {
	// Channel to receive statements.
	pub rx: async_channel::Receiver<StatementEvent>,
	// Subscription ID, used for cleanup on drop.
	sub_id: SeqID,
	// Reference to the matchers for cleanup.
	matchers: SubscriptionsMatchersHandlers,
}

// When the stream is dropped, unsubscribe from the matchers.
impl Drop for SubscriptionStatementsStream {
	fn drop(&mut self) {
		self.matchers
			.send_by_seq_id(self.sub_id, MatcherMessage::Unsubscribe(self.sub_id));
	}
}

impl Stream for SubscriptionStatementsStream {
	type Item = StatementEvent;

	fn poll_next(
		mut self: std::pin::Pin<&mut Self>,
		cx: &mut std::task::Context<'_>,
	) -> std::task::Poll<Option<Self::Item>> {
		self.rx.poll_next_unpin(cx)
	}
}

#[cfg(test)]
mod tests {

	use crate::tests::signed_statement;

	use super::*;
	use sp_core::Decode;
	use sp_statement_store::Topic;
	use std::{
		sync::{mpsc, Mutex as StdMutex},
		thread,
		time::{Duration, Instant},
	};

	fn unwrap_statement(item: StatementEvent) -> Bytes {
		match item {
			StatementEvent::NewStatements { mut statements, .. } => {
				assert_eq!(statements.len(), 1, "Expected exactly one statement in batch");
				statements.remove(0)
			},
		}
	}

	fn fixed_subscription(seq_id: u64, topic_filter: OptimizedTopicFilter) -> IndexedSubscription {
		IndexedSubscription {
			topic_filter,
			seq_id: SeqID::from(seq_id),
			filter_key: SubscriptionFilterKey::Fixed,
			watermark: 0,
		}
	}

	fn live_event_for(statement: &Statement, filter_ids: Vec<FilterId>) -> LiveStatementEvent {
		LiveStatementEvent {
			hash: statement.hash(),
			encoded: statement.encode(),
			matched_filter_ids: filter_ids,
		}
	}

	struct TestReplaySnapshotProvider {
		statements: HashMap<sp_statement_store::Hash, Vec<u8>>,
		snapshot_hashes: Vec<sp_statement_store::Hash>,
		batch_len: u64,
	}

	impl TestReplaySnapshotProvider {
		fn with_snapshot(snapshot: &[Statement], statements: &[Statement]) -> Self {
			Self {
				statements: statements
					.iter()
					.map(|statement| (statement.hash(), statement.encode()))
					.collect(),
				snapshot_hashes: snapshot.iter().map(Statement::hash).collect(),
				batch_len: snapshot.len() as u64,
			}
		}
	}

	impl ReplaySnapshotProvider for TestReplaySnapshotProvider {
		fn register_replay(&self, enqueue: &mut dyn FnMut(u64) -> bool) -> Result<Option<u64>> {
			let watermark = self.snapshot_hashes.len() as u64;
			Ok(enqueue(watermark).then_some(watermark))
		}

		fn replay_batch(
			&self,
			_filter: &OptimizedTopicFilter,
			cursor: u64,
			watermark: u64,
		) -> Result<ReplayBatch> {
			let end = watermark.min(self.snapshot_hashes.len() as u64).min(cursor + self.batch_len);
			let statements = self.snapshot_hashes[cursor as usize..end as usize]
				.iter()
				.filter_map(|hash| self.statements.get(hash).cloned())
				.collect();
			Ok(ReplayBatch { statements, cursor: end, done: end >= watermark })
		}
	}

	struct BlockingReplaySnapshotProvider {
		statements: HashMap<sp_statement_store::Hash, Vec<u8>>,
		snapshot_hashes: Vec<sp_statement_store::Hash>,
		scan_started: mpsc::SyncSender<()>,
		continue_scan: StdMutex<mpsc::Receiver<()>>,
	}

	impl ReplaySnapshotProvider for BlockingReplaySnapshotProvider {
		fn register_replay(&self, enqueue: &mut dyn FnMut(u64) -> bool) -> Result<Option<u64>> {
			let watermark = self.snapshot_hashes.len() as u64;
			Ok(enqueue(watermark).then_some(watermark))
		}

		fn replay_batch(
			&self,
			_filter: &OptimizedTopicFilter,
			cursor: u64,
			watermark: u64,
		) -> Result<ReplayBatch> {
			self.scan_started.send(()).expect("test receiver remains open");
			self.continue_scan
				.lock()
				.expect("test mutex is not poisoned")
				.recv()
				.expect("test sender remains open");
			let end = watermark.min(self.snapshot_hashes.len() as u64);
			let statements = self.snapshot_hashes[cursor as usize..end as usize]
				.iter()
				.filter_map(|hash| self.statements.get(hash).cloned())
				.collect();
			Ok(ReplayBatch { statements, cursor: end, done: end >= watermark })
		}
	}

	struct ReplayLoadBlockingProvider {
		body: Vec<u8>,
		load_started: mpsc::SyncSender<()>,
		continue_load: StdMutex<mpsc::Receiver<()>>,
	}

	impl ReplaySnapshotProvider for ReplayLoadBlockingProvider {
		fn register_replay(&self, enqueue: &mut dyn FnMut(u64) -> bool) -> Result<Option<u64>> {
			Ok(enqueue(1).then_some(1))
		}

		fn replay_batch(
			&self,
			_filter: &OptimizedTopicFilter,
			cursor: u64,
			watermark: u64,
		) -> Result<ReplayBatch> {
			assert_eq!(cursor, 0);
			assert_eq!(watermark, 1);
			self.load_started.send(()).expect("test receiver remains open");
			self.continue_load
				.lock()
				.expect("test mutex is not poisoned")
				.recv()
				.expect("test sender remains open");
			Ok(ReplayBatch { statements: vec![self.body.clone()], cursor: 1, done: true })
		}
	}

	fn recv_multi_filter_event(
		stream: &MultiFilterEventStream,
		timeout: Duration,
	) -> Option<MultiFilterSubscriptionEvent> {
		let deadline = Instant::now() + timeout;
		loop {
			match stream.rx.try_recv() {
				Ok(event) => return Some(event),
				Err(async_channel::TryRecvError::Closed) => return None,
				Err(async_channel::TryRecvError::Empty) if Instant::now() < deadline => {
					thread::sleep(Duration::from_millis(1));
				},
				Err(async_channel::TryRecvError::Empty) => return None,
			}
		}
	}

	#[test]
	fn multi_filter_replays_before_live_submissions_committed_during_snapshot() {
		let subscriptions =
			SubscriptionsHandle::new(Box::new(sp_core::testing::TaskExecutor::new()), 1);
		let filter_id = FilterId::new(0);
		let replayed = signed_statement(41);
		let delayed = signed_statement(40);
		let delayed_hash = delayed.hash();
		let live = signed_statement(42);
		let (scan_started_tx, scan_started_rx) = mpsc::sync_channel(1);
		let (continue_scan_tx, continue_scan_rx) = mpsc::sync_channel(1);
		let provider = Arc::new(BlockingReplaySnapshotProvider {
			statements: [(replayed.hash(), replayed.encode())].into_iter().collect(),
			snapshot_hashes: vec![replayed.hash()],
			scan_started: scan_started_tx,
			continue_scan: StdMutex::new(continue_scan_rx),
		});
		let (sub_id, stream) = subscriptions.subscribe_empty(provider.clone());
		let handle = SubscriptionHandle {
			sub_id,
			inner: Arc::new(Mutex::new(SubscriptionHandleInner::new())),
			matchers: subscriptions.matchers(),
			snapshot_provider: provider,
		};

		let add_filter = thread::spawn(move || handle.add_filter(OptimizedTopicFilter::Any));
		scan_started_rx.recv().expect("snapshot scan starts");

		// Both post-watermark live events wait behind replay.
		subscriptions.notify(0, replayed.clone());
		subscriptions.notify(1, delayed);
		subscriptions.notify(2, live.clone());
		continue_scan_tx.send(()).expect("snapshot scan is still waiting");
		assert_eq!(add_filter.join().expect("add-filter thread does not panic"), Ok(filter_id));

		assert!(matches!(
			recv_multi_filter_event(&stream, Duration::from_secs(1)),
			Some(MultiFilterSubscriptionEvent::ReplayStatements { statements, .. })
				if statements == vec![replayed.encode()]
		));
		assert!(matches!(
			recv_multi_filter_event(&stream, Duration::from_secs(1)),
			Some(MultiFilterSubscriptionEvent::ReplayDone { filter_id: done }) if done == filter_id
		));
		assert!(matches!(
			recv_multi_filter_event(&stream, Duration::from_secs(1)),
			Some(MultiFilterSubscriptionEvent::NewStatement(event))
				if event.hash == delayed_hash && event.matched_filter_ids == vec![filter_id]
		));
		assert!(matches!(
			recv_multi_filter_event(&stream, Duration::from_secs(1)),
			Some(MultiFilterSubscriptionEvent::NewStatement(event))
				if event.hash == live.hash() && event.matched_filter_ids == vec![filter_id]
		));
		assert!(recv_multi_filter_event(&stream, Duration::from_millis(10)).is_none());
	}

	#[test]
	fn add_filter_returns_before_matcher_drains_replay() {
		let subscriptions =
			SubscriptionsHandle::new(Box::new(sp_core::testing::TaskExecutor::new()), 1);
		let statement = signed_statement(43);
		let (load_started_tx, load_started_rx) = mpsc::sync_channel(1);
		let (continue_load_tx, continue_load_rx) = mpsc::sync_channel(1);
		let provider = Arc::new(ReplayLoadBlockingProvider {
			body: statement.encode(),
			load_started: load_started_tx,
			continue_load: StdMutex::new(continue_load_rx),
		});
		let (sub_id, _stream) = subscriptions.subscribe_empty(provider.clone());
		let handle = SubscriptionHandle {
			sub_id,
			inner: Arc::new(Mutex::new(SubscriptionHandleInner::new())),
			matchers: subscriptions.matchers(),
			snapshot_provider: provider,
		};
		let (result_tx, result_rx) = mpsc::sync_channel(1);
		let add_filter = thread::spawn(move || {
			result_tx
				.send(handle.add_filter(OptimizedTopicFilter::Any))
				.expect("test receiver remains open");
		});

		load_started_rx.recv().expect("matcher starts loading replay");
		let result = result_rx.recv_timeout(Duration::from_secs(1));
		continue_load_tx.send(()).expect("replay load is still waiting");
		add_filter.join().expect("add-filter thread does not panic");
		assert_eq!(result, Ok(Ok(FilterId::new(0))));
	}

	#[test]
	fn multi_filter_delivers_re_admitted_hash_skipped_during_replay() {
		let mut subscriptions = SubscriptionsInfo::new();
		let sub_id = SeqID::from(12);
		let filter_id = FilterId::new(1);
		let topic = Topic::from([9u8; 32]);
		let filter = OptimizedTopicFilter::MatchAny(vec![topic].into_iter().collect());
		let (tx, rx) = async_channel::bounded::<MultiFilterSubscriptionEvent>(10);
		let mut statement = signed_statement(42);
		statement.set_topic(0, topic);
		// The statement has an admission entry inside the replay range, but its body is no longer
		// retrievable, so the replay skips that sequence number.
		let provider = Arc::new(TestReplaySnapshotProvider::with_snapshot(
			std::slice::from_ref(&statement),
			&[],
		));

		subscriptions.subscribe_empty(sub_id, provider, tx);
		subscriptions.add_filter(sub_id, filter_id, filter, vec![statement.hash()]);

		// The body could not be loaded, so the replay finishes without emitting it.
		assert!(matches!(
			rx.try_recv(),
			Ok(MultiFilterSubscriptionEvent::ReplayDone { filter_id: done }) if done == filter_id
		));

		// The statement is re-admitted at a new sequence. It was never delivered through replay,
		// so it must reach the subscriber as a live event.
		subscriptions.notify_matching_filters(1, &statement);

		match rx.try_recv() {
			Ok(MultiFilterSubscriptionEvent::NewStatement(event)) => {
				assert_eq!(event.hash, statement.hash());
				assert_eq!(event.matched_filter_ids, vec![filter_id]);
			},
			other => {
				panic!("statement skipped during replay must be delivered live, got {other:?}")
			},
		}

		subscriptions.notify_matching_filters(2, &statement);
		assert!(matches!(
			rx.try_recv(),
			Ok(MultiFilterSubscriptionEvent::NewStatement(event))
				if event.hash == statement.hash() && event.matched_filter_ids == vec![filter_id]
		));
	}

	#[test]
	fn multi_filter_replay_resumes_from_the_returned_cursor() {
		let mut subscriptions = SubscriptionsInfo::new();
		let sub_id = SeqID::from(11);
		let filter_id = FilterId::new(1);
		let topic = Topic::from([9u8; 32]);
		let filter = OptimizedTopicFilter::MatchAny(vec![topic].into_iter().collect());
		let (tx, rx) = async_channel::bounded::<MultiFilterSubscriptionEvent>(10);
		let replayed: Vec<Statement> = (0..3u8)
			.map(|seed| {
				let mut statement = signed_statement(seed);
				statement.set_topic(0, topic);
				statement
			})
			.collect();
		let mut provider = TestReplaySnapshotProvider::with_snapshot(&replayed, &replayed);
		provider.batch_len = 1;

		subscriptions.subscribe_empty(sub_id, Arc::new(provider), tx);
		subscriptions.add_filter(
			sub_id,
			filter_id,
			filter,
			replayed.iter().map(Statement::hash).collect(),
		);

		let mut delivered = Vec::new();
		let mut replay_done = 0;
		while let Ok(event) = rx.try_recv() {
			match event {
				MultiFilterSubscriptionEvent::ReplayStatements { filter_id: id, statements } => {
					assert_eq!(id, filter_id);
					delivered.extend(statements);
				},
				MultiFilterSubscriptionEvent::ReplayDone { filter_id: id } => {
					assert_eq!(id, filter_id);
					replay_done += 1;
				},
				other => panic!("expected replay events, got {other:?}"),
			}
		}
		assert_eq!(delivered, replayed.iter().map(Statement::encode).collect::<Vec<_>>());
		assert_eq!(replay_done, 1);
	}

	#[test]
	fn multi_filter_pushes_ready_live_events_without_request_next() {
		let mut subscriptions = SubscriptionsInfo::new();
		let sub_id = SeqID::from(7);
		let filter_id = FilterId::new(1);
		let topic = Topic::from([9u8; 32]);
		let filter = OptimizedTopicFilter::MatchAny(vec![topic].into_iter().collect());
		let (tx, rx) = async_channel::bounded::<MultiFilterSubscriptionEvent>(10);
		let mut statement = signed_statement(42);
		statement.set_topic(0, topic);
		let provider = Arc::new(TestReplaySnapshotProvider::with_snapshot(
			&[],
			std::slice::from_ref(&statement),
		));

		subscriptions.subscribe_empty(sub_id, provider, tx);
		subscriptions.add_filter(sub_id, filter_id, filter, vec![]);
		assert!(matches!(
			rx.try_recv(),
			Ok(MultiFilterSubscriptionEvent::ReplayDone { filter_id: done_filter })
				if done_filter == filter_id
		));

		subscriptions.notify_matching_filters(0, &statement);

		match rx.try_recv() {
			Ok(MultiFilterSubscriptionEvent::NewStatement(event)) => {
				assert_eq!(event.hash, statement.hash());
				assert_eq!(event.matched_filter_ids, vec![filter_id]);
			},
			other => panic!("expected pushed live statement, got {other:?}"),
		}
	}

	#[test]
	fn multi_filter_reserves_output_slot_for_stop() {
		let mut subscriptions = SubscriptionsInfo::new();
		let sub_id = SeqID::from(8);
		let filter_id = FilterId::new(1);
		let topic = Topic::from([9u8; 32]);
		let filter = OptimizedTopicFilter::MatchAny(vec![topic].into_iter().collect());
		let mut statements =
			Vec::with_capacity(SUBSCRIPTION_BUFFER_SIZE + STOP_RESERVE_CHANNEL_SLOTS);
		for seed in 0..=SUBSCRIPTION_BUFFER_SIZE as u64 {
			let mut statement = signed_statement(seed as u8);
			statement.set_topic(0, topic);
			statements.push(statement);
		}
		let provider = Arc::new(TestReplaySnapshotProvider::with_snapshot(&[], &statements));
		let (tx, rx) = async_channel::bounded::<MultiFilterSubscriptionEvent>(
			SUBSCRIPTION_BUFFER_SIZE + STOP_RESERVE_CHANNEL_SLOTS,
		);

		subscriptions.subscribe_empty(sub_id, provider, tx);
		subscriptions.add_filter(sub_id, filter_id, filter, vec![]);
		assert!(matches!(
			rx.try_recv(),
			Ok(MultiFilterSubscriptionEvent::ReplayDone { filter_id: done_filter })
				if done_filter == filter_id
		));

		for statement in statements.iter().take(SUBSCRIPTION_BUFFER_SIZE) {
			subscriptions.notify_matching_filters(0, statement);
		}
		assert_eq!(rx.len(), SUBSCRIPTION_BUFFER_SIZE);

		subscriptions.notify_matching_filters(0, &statements[SUBSCRIPTION_BUFFER_SIZE]);
		assert_eq!(rx.len(), SUBSCRIPTION_BUFFER_SIZE + STOP_RESERVE_CHANNEL_SLOTS);

		for _ in 0..SUBSCRIPTION_BUFFER_SIZE {
			assert!(matches!(rx.try_recv(), Ok(MultiFilterSubscriptionEvent::NewStatement(_))));
		}
		assert!(matches!(rx.try_recv(), Ok(MultiFilterSubscriptionEvent::Stop)));
		assert!(rx.is_closed());
	}

	#[test]
	fn multi_filter_closed_channel_is_reported_as_closed_before_buffer_limit() {
		let mut state = MultiFilterSubscriptionState::new();
		let (tx, rx) = async_channel::bounded::<MultiFilterSubscriptionEvent>(
			SUBSCRIPTION_BUFFER_SIZE + STOP_RESERVE_CHANNEL_SLOTS,
		);
		for seed in 0..SUBSCRIPTION_BUFFER_SIZE {
			tx.try_send(MultiFilterSubscriptionEvent::NewStatement(live_event_for(
				&signed_statement(seed as u8),
				vec![FilterId::new(1)],
			)))
			.expect("channel capacity exceeds subscription buffer size; qed");
		}
		drop(rx);

		assert!(matches!(
			SubscriptionsInfo::send_ready_event(
				&mut state,
				&tx,
				MultiFilterSubscriptionEvent::NewStatement(live_event_for(
					&signed_statement(42),
					vec![FilterId::new(1)],
				)),
			),
			ReadyEventDelivery::Closed
		));
		assert!(matches!(
			SubscriptionsInfo::send_ready_event(
				&mut state,
				&tx,
				MultiFilterSubscriptionEvent::Stop,
			),
			ReadyEventDelivery::Closed
		));
		assert!(!state.stopped);
	}

	#[test]
	fn test_subscribe_unsubscribe() {
		let mut subscriptions = SubscriptionsInfo::new();

		let (tx1, _rx1) = async_channel::bounded::<StatementEvent>(10);
		let topic1 = Topic::from([8u8; 32]);
		let topic2 = Topic::from([9u8; 32]);
		let sub_info1 = fixed_subscription(
			1,
			OptimizedTopicFilter::MatchAll(vec![topic1, topic2].into_iter().collect()),
		);
		subscriptions.subscribe(sub_info1.clone(), tx1);
		assert!(subscriptions.subscriptions_match_all_by_topic.contains_key(&topic1));
		assert!(subscriptions.subscriptions_match_all_by_topic.contains_key(&topic2));
		assert!(subscriptions.by_sub_id.contains_key(&sub_info1.seq_id));
		assert!(!subscriptions
			.subscriptions_any
			.contains_key(&(sub_info1.seq_id, sub_info1.filter_key)));

		subscriptions.unsubscribe(sub_info1.seq_id);
		assert!(!subscriptions.subscriptions_match_all_by_topic.contains_key(&topic1));
		assert!(!subscriptions.subscriptions_match_all_by_topic.contains_key(&topic2));
	}

	#[test]
	fn test_subscribe_any() {
		let mut subscriptions = SubscriptionsInfo::new();
		let (tx1, _rx1) = async_channel::bounded::<StatementEvent>(10);
		let sub_info1 = fixed_subscription(1, OptimizedTopicFilter::Any);
		subscriptions.subscribe(sub_info1.clone(), tx1);
		assert!(subscriptions
			.subscriptions_any
			.contains_key(&(sub_info1.seq_id, sub_info1.filter_key)));
		assert!(subscriptions.by_sub_id.contains_key(&sub_info1.seq_id));
		subscriptions.unsubscribe(sub_info1.seq_id);
		assert!(!subscriptions
			.subscriptions_any
			.contains_key(&(sub_info1.seq_id, sub_info1.filter_key)));
	}

	#[test]
	fn test_subscribe_match_any() {
		let mut subscriptions = SubscriptionsInfo::new();

		let (tx1, _rx1) = async_channel::bounded::<StatementEvent>(10);
		let topic1 = Topic::from([8u8; 32]);
		let topic2 = Topic::from([9u8; 32]);
		let sub_info1 = fixed_subscription(
			1,
			OptimizedTopicFilter::MatchAny(vec![topic1, topic2].into_iter().collect()),
		);
		subscriptions.subscribe(sub_info1.clone(), tx1);
		assert!(subscriptions.subscriptions_match_any_by_topic.contains_key(&topic1));
		assert!(subscriptions.subscriptions_match_any_by_topic.contains_key(&topic2));
		assert!(subscriptions.by_sub_id.contains_key(&sub_info1.seq_id));
		assert!(!subscriptions
			.subscriptions_any
			.contains_key(&(sub_info1.seq_id, sub_info1.filter_key)));

		subscriptions.unsubscribe(sub_info1.seq_id);
		assert!(!subscriptions.subscriptions_match_all_by_topic.contains_key(&topic1));
		assert!(!subscriptions.subscriptions_match_all_by_topic.contains_key(&topic2));
	}

	#[test]
	fn test_notify_matching_filters_any() {
		let mut subscriptions = SubscriptionsInfo::new();

		let (tx1, rx1) = async_channel::bounded::<StatementEvent>(10);
		let sub_info1 = fixed_subscription(1, OptimizedTopicFilter::Any);
		subscriptions.subscribe(sub_info1.clone(), tx1);

		let statement = signed_statement(1);
		subscriptions.notify_matching_filters(0, &statement);

		let received = unwrap_statement(rx1.try_recv().expect("Should receive statement"));
		let decoded_statement: Statement =
			Statement::decode(&mut &received.0[..]).expect("Should decode statement");
		assert_eq!(decoded_statement, statement);
	}

	#[test]
	fn test_notify_matching_filters_match_all() {
		let mut subscriptions = SubscriptionsInfo::new();

		let (tx1, rx1) = async_channel::bounded::<StatementEvent>(10);
		let topic1 = Topic::from([8u8; 32]);
		let topic2 = Topic::from([9u8; 32]);
		let sub_info1 = fixed_subscription(
			1,
			OptimizedTopicFilter::MatchAll(vec![topic1, topic2].into_iter().collect()),
		);
		subscriptions.subscribe(sub_info1.clone(), tx1);

		let mut statement = signed_statement(1);
		statement.set_topic(0, topic2);
		subscriptions.notify_matching_filters(0, &statement);

		// Should not receive yet, only one topic matched.
		assert!(rx1.try_recv().is_err());

		statement.set_topic(1, topic1);
		subscriptions.notify_matching_filters(1, &statement);

		let received = unwrap_statement(rx1.try_recv().expect("Should receive statement"));
		let decoded_statement: Statement =
			Statement::decode(&mut &received.0[..]).expect("Should decode statement");
		assert_eq!(decoded_statement, statement);
	}

	#[test]
	fn test_notify_matching_filters_match_any() {
		let mut subscriptions = SubscriptionsInfo::new();
		let (tx1, rx1) = async_channel::bounded::<StatementEvent>(10);
		let (tx2, rx2) = async_channel::bounded::<StatementEvent>(10);

		let topic1 = Topic::from([8u8; 32]);
		let topic2 = Topic::from([9u8; 32]);
		let sub_info1 = fixed_subscription(
			1,
			OptimizedTopicFilter::MatchAny(vec![topic1, topic2].into_iter().collect()),
		);

		let sub_info2 = fixed_subscription(
			2,
			OptimizedTopicFilter::MatchAny(vec![topic2].into_iter().collect()),
		);

		subscriptions.subscribe(sub_info1.clone(), tx1);
		subscriptions.subscribe(sub_info2.clone(), tx2);

		let mut statement = signed_statement(1);
		statement.set_topic(0, topic1);
		statement.set_topic(1, topic2);
		subscriptions.notify_matching_filters(0, &statement);

		let received = unwrap_statement(rx1.try_recv().expect("Should receive statement"));
		let decoded_statement: Statement =
			Statement::decode(&mut &received.0[..]).expect("Should decode statement");
		assert_eq!(decoded_statement, statement);

		let received = unwrap_statement(rx2.try_recv().expect("Should receive statement"));
		let decoded_statement: Statement =
			Statement::decode(&mut &received.0[..]).expect("Should decode statement");
		assert_eq!(decoded_statement, statement);
	}

	#[tokio::test]
	async fn test_subscription_handle_with_different_workers_number() {
		for num_workers in 1..5 {
			let subscriptions_handle = SubscriptionsHandle::new(
				Box::new(sp_core::testing::TaskExecutor::new()),
				num_workers,
			);

			let topic1 = Topic::from([8u8; 32]);
			let topic2 = Topic::from([9u8; 32]);

			let streams = (0..5)
				.into_iter()
				.map(|_| {
					subscriptions_handle.subscribe(
						OptimizedTopicFilter::MatchAll(vec![topic1, topic2].into_iter().collect()),
						0,
					)
				})
				.collect::<Vec<_>>();

			let mut statement = signed_statement(1);
			statement.set_topic(0, topic2);
			subscriptions_handle.notify(0, statement.clone());

			statement.set_topic(1, topic1);
			subscriptions_handle.notify(1, statement.clone());

			for (_tx, mut stream) in streams {
				let received =
					unwrap_statement(stream.next().await.expect("Should receive statement"));
				let decoded_statement: Statement =
					Statement::decode(&mut &received.0[..]).expect("Should decode statement");
				assert_eq!(decoded_statement, statement);
			}
		}
	}

	#[tokio::test]
	async fn test_handle_unsubscribe() {
		let subscriptions_handle =
			SubscriptionsHandle::new(Box::new(sp_core::testing::TaskExecutor::new()), 2);

		let topic1 = Topic::from([8u8; 32]);
		let topic2 = Topic::from([9u8; 32]);

		let (tx, mut stream) = subscriptions_handle.subscribe(
			OptimizedTopicFilter::MatchAll(vec![topic1, topic2].into_iter().collect()),
			0,
		);

		let mut statement = signed_statement(1);
		statement.set_topic(0, topic1);
		statement.set_topic(1, topic2);

		// Send a statement and verify it's received.
		subscriptions_handle.notify(0, statement.clone());

		let received = unwrap_statement(stream.next().await.expect("Should receive statement"));
		let decoded_statement: Statement =
			Statement::decode(&mut &received.0[..]).expect("Should decode statement");
		assert_eq!(decoded_statement, statement);

		// Drop the stream to trigger unsubscribe.
		drop(stream);

		// Give some time for the unsubscribe message to be processed.
		tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

		// Send another statement after unsubscribe.
		let mut statement2 = signed_statement(2);
		statement2.set_topic(0, topic1);
		statement2.set_topic(1, topic2);
		subscriptions_handle.notify(1, statement2.clone());

		// The tx channel should be closed/disconnected since the subscription was removed.
		// Give some time for the notification to potentially arrive (it shouldn't).
		tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

		// The sender should fail to send since the subscription is gone.
		// We verify by checking that the tx channel is disconnected.
		assert!(tx.is_closed(), "Sender should be closed after unsubscribe");
	}

	#[test]
	fn test_unsubscribe_nonexistent() {
		let mut subscriptions = SubscriptionsInfo::new();
		// Unsubscribing a non-existent subscription should not panic.
		subscriptions.unsubscribe(SeqID::from(999));
		// Verify internal state is still valid.
		assert!(subscriptions.by_sub_id.is_empty());
		assert!(subscriptions.subscriptions_any.is_empty());
		assert!(subscriptions.subscriptions_match_all_by_topic.is_empty());
		assert!(subscriptions.subscriptions_match_any_by_topic.is_empty());
	}

	#[test]
	fn test_multiple_subscriptions_same_topic() {
		let mut subscriptions = SubscriptionsInfo::new();

		let (tx1, rx1) = async_channel::bounded::<StatementEvent>(10);
		let (tx2, rx2) = async_channel::bounded::<StatementEvent>(10);
		let topic1 = Topic::from([8u8; 32]);
		let topic2 = Topic::from([9u8; 32]);

		let sub_info1 = fixed_subscription(
			1,
			OptimizedTopicFilter::MatchAll(vec![topic1, topic2].into_iter().collect()),
		);
		let sub_info2 = fixed_subscription(
			2,
			OptimizedTopicFilter::MatchAll(vec![topic1, topic2].into_iter().collect()),
		);

		subscriptions.subscribe(sub_info1.clone(), tx1);
		subscriptions.subscribe(sub_info2.clone(), tx2);

		// Both subscriptions should be registered under each topic.
		assert_eq!(
			subscriptions
				.subscriptions_match_all_by_topic
				.get(&topic1)
				.unwrap()
				.iter()
				.map(|s| s.len())
				.sum::<usize>(),
			2
		);
		assert_eq!(
			subscriptions
				.subscriptions_match_all_by_topic
				.get(&topic2)
				.unwrap()
				.iter()
				.map(|s| s.len())
				.sum::<usize>(),
			2
		);

		// Send a matching statement.
		let mut statement = signed_statement(1);
		statement.set_topic(0, topic1);
		statement.set_topic(1, topic2);
		subscriptions.notify_matching_filters(0, &statement);

		// Both should receive.
		assert!(rx1.try_recv().is_ok());
		assert!(rx2.try_recv().is_ok());

		// Unsubscribe one.
		subscriptions.unsubscribe(sub_info1.seq_id);

		// Only one subscription should remain.
		assert_eq!(
			subscriptions
				.subscriptions_match_all_by_topic
				.get(&topic1)
				.unwrap()
				.iter()
				.map(|s| s.len())
				.sum::<usize>(),
			1
		);
		assert_eq!(
			subscriptions
				.subscriptions_match_all_by_topic
				.get(&topic2)
				.unwrap()
				.iter()
				.map(|s| s.len())
				.sum::<usize>(),
			1
		);
		assert!(!subscriptions.by_sub_id.contains_key(&sub_info1.seq_id));
		assert!(subscriptions.by_sub_id.contains_key(&sub_info2.seq_id));

		// Send another statement.
		subscriptions.notify_matching_filters(1, &statement);

		// Only sub2 should receive.
		assert!(rx2.try_recv().is_ok());
		assert!(rx1.try_recv().is_err());
	}

	#[test]
	fn test_subscriber_auto_unsubscribe_on_channel_full() {
		let mut subscriptions = SubscriptionsInfo::new();

		// Create a channel with capacity 1.
		let (tx1, rx1) = async_channel::bounded::<StatementEvent>(1);
		let topic1 = Topic::from([8u8; 32]);

		let sub_info1 = fixed_subscription(
			1,
			OptimizedTopicFilter::MatchAny(vec![topic1].into_iter().collect()),
		);
		subscriptions.subscribe(sub_info1.clone(), tx1);

		let mut statement = signed_statement(1);
		statement.set_topic(0, topic1);

		// First notification should succeed.
		subscriptions.notify_matching_filters(0, &statement);
		assert!(rx1.try_recv().is_ok());

		// Fill the channel.
		subscriptions.notify_matching_filters(1, &statement);
		// Channel is now full.

		// Next notification should trigger auto-unsubscribe.
		subscriptions.notify_matching_filters(2, &statement);

		// Subscription should be removed.
		assert!(!subscriptions.by_sub_id.contains_key(&sub_info1.seq_id));
		assert!(!subscriptions.subscriptions_match_any_by_topic.contains_key(&topic1));
	}

	#[test]
	fn test_match_any_receives_once_per_statement() {
		let mut subscriptions = SubscriptionsInfo::new();

		let (tx1, rx1) = async_channel::bounded::<StatementEvent>(10);
		let topic1 = Topic::from([8u8; 32]);
		let topic2 = Topic::from([9u8; 32]);

		// Subscribe to MatchAny with both topics.
		let sub_info1 = fixed_subscription(
			1,
			OptimizedTopicFilter::MatchAny(vec![topic1, topic2].into_iter().collect()),
		);
		subscriptions.subscribe(sub_info1.clone(), tx1);

		// Create a statement that matches BOTH topics.
		let mut statement = signed_statement(1);
		statement.set_topic(0, topic1);
		statement.set_topic(1, topic2);

		subscriptions.notify_matching_filters(0, &statement);

		// Should receive exactly once, not twice.
		let received = unwrap_statement(rx1.try_recv().expect("Should receive statement"));
		let decoded_statement: Statement =
			Statement::decode(&mut &received.0[..]).expect("Should decode statement");
		assert_eq!(decoded_statement, statement);

		// No more messages.
		assert!(rx1.try_recv().is_err());
	}

	#[test]
	fn test_match_all_receives_once_per_statement() {
		// A `MatchAll` subscriber must receive each matching statement exactly once, even when it
		// is registered under several of the statement's topics and the matcher therefore
		// encounters it across multiple topic combinations.
		let mut subscriptions = SubscriptionsInfo::new();

		let (tx1, rx1) = async_channel::bounded::<StatementEvent>(10);
		let (tx2, _rx2) = async_channel::bounded::<StatementEvent>(10);
		let (tx3, _rx3) = async_channel::bounded::<StatementEvent>(10);

		let topic1 = Topic::from([1u8; 32]);
		let topic2 = Topic::from([2u8; 32]);
		let topic3 = Topic::from([3u8; 32]);
		let topic4 = Topic::from([4u8; 32]);

		// The subscription under test: MatchAll on topic1 AND topic2 (stored under both topics).
		let sub_info1 = fixed_subscription(
			1,
			OptimizedTopicFilter::MatchAll(vec![topic1, topic2].into_iter().collect()),
		);
		subscriptions.subscribe(sub_info1, tx1);

		// Extra MatchAll subscriptions on topic3 so the matcher encounters sub_info1 across
		// several topic combinations of the statement below. They do not match the statement
		// themselves (topic4 is absent).
		let sub_info2 = fixed_subscription(
			2,
			OptimizedTopicFilter::MatchAll(vec![topic3, topic4].into_iter().collect()),
		);
		let sub_info3 = fixed_subscription(
			3,
			OptimizedTopicFilter::MatchAll(vec![topic3, topic4].into_iter().collect()),
		);
		subscriptions.subscribe(sub_info2, tx2);
		subscriptions.subscribe(sub_info3, tx3);

		// Statement carrying topic1, topic2 and topic3.
		let mut statement = signed_statement(1);
		statement.set_topic(0, topic1);
		statement.set_topic(1, topic2);
		statement.set_topic(2, topic3);

		subscriptions.notify_matching_filters(0, &statement);

		// sub_info1 must receive the statement exactly once.
		let received = unwrap_statement(rx1.try_recv().expect("Should receive statement"));
		let decoded_statement: Statement =
			Statement::decode(&mut &received.0[..]).expect("Should decode statement");
		assert_eq!(decoded_statement, statement);

		assert!(
			rx1.try_recv().is_err(),
			"MatchAll subscriber must receive each statement only once"
		);
	}

	#[test]
	fn test_match_all_with_single_topic_matches_statement_with_two_topics() {
		let mut subscriptions = SubscriptionsInfo::new();

		let (tx1, rx1) = async_channel::bounded::<StatementEvent>(10);
		let topic1 = Topic::from([8u8; 32]);
		let topic2 = Topic::from([9u8; 32]);

		// Subscribe with MatchAll on only topic1.
		let sub_info1 = fixed_subscription(
			1,
			OptimizedTopicFilter::MatchAll(vec![topic1].into_iter().collect()),
		);
		subscriptions.subscribe(sub_info1.clone(), tx1);

		// Create a statement that has BOTH topic1 and topic2.
		let mut statement = signed_statement(1);
		statement.set_topic(0, topic1);
		statement.set_topic(1, topic2);

		subscriptions.notify_matching_filters(0, &statement);

		// Should receive because the statement contains topic1 (which is the only required topic).
		let received = unwrap_statement(rx1.try_recv().expect("Should receive statement"));
		let decoded_statement: Statement =
			Statement::decode(&mut &received.0[..]).expect("Should decode statement");
		assert_eq!(decoded_statement, statement);

		// No more messages.
		assert!(rx1.try_recv().is_err());
	}

	#[test]
	fn test_match_all_no_matching_topics() {
		let mut subscriptions = SubscriptionsInfo::new();

		let (tx1, rx1) = async_channel::bounded::<StatementEvent>(10);
		let topic1 = Topic::from([8u8; 32]);
		let topic2 = Topic::from([9u8; 32]);
		let topic3 = Topic::from([10u8; 32]);

		let sub_info1 = fixed_subscription(
			1,
			OptimizedTopicFilter::MatchAll(vec![topic1, topic2].into_iter().collect()),
		);
		subscriptions.subscribe(sub_info1.clone(), tx1);

		// Statement with completely different topics.
		let mut statement = signed_statement(1);
		statement.set_topic(0, topic3);

		subscriptions.notify_matching_filters(0, &statement);

		// Should not receive anything.
		assert!(rx1.try_recv().is_err());
	}

	#[test]
	fn test_match_all_with_unsubscribed_topic_first_in_statement() {
		// This test guards against returning early when one statement topic has no subscriptions.
		// The matcher must still check later topic combinations that can match.
		let mut subscriptions = SubscriptionsInfo::new();

		let (tx1, rx1) = async_channel::bounded::<StatementEvent>(10);
		// topic1 will have NO subscriptions
		let topic1 = Topic::from([1u8; 32]);
		// topic2 WILL have a subscription
		let topic2 = Topic::from([2u8; 32]);

		// Subscribe only to topic2 with MatchAll filter.
		let sub_info1 = fixed_subscription(
			1,
			OptimizedTopicFilter::MatchAll(vec![topic2].into_iter().collect()),
		);
		subscriptions.subscribe(sub_info1, tx1);

		// Create a statement with BOTH topics. topic1 comes first (lower bytes).
		// When iterating combinations(1), [topic1] is checked before [topic2].
		// Since topic1 has no subscriptions, the buggy `return` exits early,
		// preventing the [topic2] combination from being checked.
		let mut statement = signed_statement(1);
		statement.set_topic(0, topic1);
		statement.set_topic(1, topic2);

		subscriptions.notify_matching_filters(0, &statement);

		// The receive succeeds only if the matcher checks the [topic2] combination.
		let received = unwrap_statement(
			rx1.try_recv()
				.expect("Should receive statement from a later matching topic combination"),
		);
		let decoded_statement: Statement =
			Statement::decode(&mut &received.0[..]).expect("Should decode statement");
		assert_eq!(decoded_statement, statement);
	}

	#[tokio::test]
	async fn test_handle_with_match_any_filter() {
		let subscriptions_handle =
			SubscriptionsHandle::new(Box::new(sp_core::testing::TaskExecutor::new()), 2);

		let topic1 = Topic::from([8u8; 32]);
		let topic2 = Topic::from([9u8; 32]);

		let (_tx, mut stream) = subscriptions_handle.subscribe(
			OptimizedTopicFilter::MatchAny(vec![topic1, topic2].into_iter().collect()),
			0,
		);

		// Statement matching only topic1.
		let mut statement1 = signed_statement(1);
		statement1.set_topic(0, topic1);
		subscriptions_handle.notify(0, statement1.clone());

		let received = unwrap_statement(stream.next().await.expect("Should receive statement"));
		let decoded_statement: Statement =
			Statement::decode(&mut &received.0[..]).expect("Should decode statement");
		assert_eq!(decoded_statement, statement1);

		// Statement matching only topic2.
		let mut statement2 = signed_statement(2);
		statement2.set_topic(0, topic2);
		subscriptions_handle.notify(1, statement2.clone());

		let received = unwrap_statement(stream.next().await.expect("Should receive statement"));
		let decoded_statement: Statement =
			Statement::decode(&mut &received.0[..]).expect("Should decode statement");
		assert_eq!(decoded_statement, statement2);
	}

	#[tokio::test]
	async fn test_handle_with_any_filter() {
		let subscriptions_handle =
			SubscriptionsHandle::new(Box::new(sp_core::testing::TaskExecutor::new()), 2);

		let (_tx, mut stream) = subscriptions_handle.subscribe(OptimizedTopicFilter::Any, 0);

		// Send statements with various topics.
		let statement1 = signed_statement(1);
		subscriptions_handle.notify(0, statement1.clone());

		let received = unwrap_statement(stream.next().await.expect("Should receive statement"));
		let decoded_statement: Statement =
			Statement::decode(&mut &received.0[..]).expect("Should decode statement");
		assert_eq!(decoded_statement, statement1);

		let mut statement2 = signed_statement(2);
		statement2.set_topic(0, Topic::from([99u8; 32]));
		subscriptions_handle.notify(1, statement2.clone());

		let received = unwrap_statement(stream.next().await.expect("Should receive statement"));
		let decoded_statement: Statement =
			Statement::decode(&mut &received.0[..]).expect("Should decode statement");
		assert_eq!(decoded_statement, statement2);
	}

	#[tokio::test]
	async fn test_handle_multiple_subscribers_different_filters() {
		let subscriptions_handle =
			SubscriptionsHandle::new(Box::new(sp_core::testing::TaskExecutor::new()), 2);

		let topic1 = Topic::from([8u8; 32]);
		let topic2 = Topic::from([9u8; 32]);

		// Subscriber 1: MatchAll on topic1 and topic2.
		let (_tx1, mut stream1) = subscriptions_handle.subscribe(
			OptimizedTopicFilter::MatchAll(vec![topic1, topic2].into_iter().collect()),
			0,
		);

		// Subscriber 2: MatchAny on topic1.
		let (_tx2, mut stream2) = subscriptions_handle
			.subscribe(OptimizedTopicFilter::MatchAny(vec![topic1].into_iter().collect()), 0);

		// Subscriber 3: Any.
		let (_tx3, mut stream3) = subscriptions_handle.subscribe(OptimizedTopicFilter::Any, 0);

		// Statement matching only topic1.
		let mut statement1 = signed_statement(1);
		statement1.set_topic(0, topic1);
		subscriptions_handle.notify(0, statement1.clone());

		// stream1 should NOT receive (needs both topics).
		// stream2 should receive (MatchAny topic1).
		// stream3 should receive (Any).

		let received2 = unwrap_statement(stream2.next().await.expect("stream2 should receive"));
		let decoded2: Statement = Statement::decode(&mut &received2.0[..]).unwrap();
		assert_eq!(decoded2, statement1);

		let received3 = unwrap_statement(stream3.next().await.expect("stream3 should receive"));
		let decoded3: Statement = Statement::decode(&mut &received3.0[..]).unwrap();
		assert_eq!(decoded3, statement1);

		// Statement matching both topics.
		let mut statement2 = signed_statement(2);
		statement2.set_topic(0, topic1);
		statement2.set_topic(1, topic2);
		subscriptions_handle.notify(1, statement2.clone());

		// All should receive.
		let received1 = unwrap_statement(stream1.next().await.expect("stream1 should receive"));
		let decoded1: Statement = Statement::decode(&mut &received1.0[..]).unwrap();
		assert_eq!(decoded1, statement2);

		let received2 = unwrap_statement(stream2.next().await.expect("stream2 should receive"));
		let decoded2: Statement = Statement::decode(&mut &received2.0[..]).unwrap();
		assert_eq!(decoded2, statement2);

		let received3 = unwrap_statement(stream3.next().await.expect("stream3 should receive"));
		let decoded3: Statement = Statement::decode(&mut &received3.0[..]).unwrap();
		assert_eq!(decoded3, statement2);
	}

	#[test]
	fn test_statement_without_topics_matches_only_any_filter() {
		let mut subscriptions = SubscriptionsInfo::new();

		let (tx_match_all, rx_match_all) = async_channel::bounded::<StatementEvent>(10);
		let (tx_match_any, rx_match_any) = async_channel::bounded::<StatementEvent>(10);
		let (tx_any, rx_any) = async_channel::bounded::<StatementEvent>(10);

		let topic1 = Topic::from([8u8; 32]);
		let topic2 = Topic::from([9u8; 32]);

		// Subscribe with MatchAll filter.
		let sub_match_all = fixed_subscription(
			1,
			OptimizedTopicFilter::MatchAll(vec![topic1, topic2].into_iter().collect()),
		);
		subscriptions.subscribe(sub_match_all, tx_match_all);

		// Subscribe with MatchAny filter.
		let sub_match_any = fixed_subscription(
			2,
			OptimizedTopicFilter::MatchAny(vec![topic1, topic2].into_iter().collect()),
		);
		subscriptions.subscribe(sub_match_any, tx_match_any);

		// Subscribe with Any filter.
		let sub_any = fixed_subscription(3, OptimizedTopicFilter::Any);
		subscriptions.subscribe(sub_any, tx_any);

		// Create a statement without any topics set.
		let statement = signed_statement(1);
		assert!(statement.topics().is_empty(), "Statement should have no topics");

		// Notify all matching filters.
		subscriptions.notify_matching_filters(0, &statement);

		// Any should receive (matches all statements regardless of topics).
		let received =
			unwrap_statement(rx_any.try_recv().expect("Any filter should receive statement"));
		let decoded_statement: Statement =
			Statement::decode(&mut &received.0[..]).expect("Should decode statement");
		assert_eq!(decoded_statement, statement);

		// MatchAll should NOT receive (statement has no topics, filter requires topic1 AND topic2).
		assert!(
			rx_match_all.try_recv().is_err(),
			"MatchAll should not receive statement without topics"
		);

		// MatchAny should NOT receive (statement has no topics, filter requires topic1 OR topic2).
		assert!(
			rx_match_any.try_recv().is_err(),
			"MatchAny should not receive statement without topics"
		);
	}
}
