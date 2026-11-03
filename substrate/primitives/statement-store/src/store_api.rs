// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
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

pub use crate::runtime_api::StatementSource;
use crate::{Hash, Statement, Topic, MAX_ANY_TOPICS, MAX_TOPICS};
use sp_core::{bounded_vec::BoundedVec, Bytes, ConstU32};
use std::collections::HashSet;

/// Identifier for a filter attached to a multi-filter subscription
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FilterId(u64);

impl FilterId {
	/// Creates a filter id from its numeric representation
	pub fn new(id: u64) -> Self {
		FilterId(id)
	}

	/// Returns the numeric representation of this filter id
	pub fn as_u64(&self) -> u64 {
		self.0
	}
}

/// Live statement event emitted by a multi-filter subscription
#[derive(Debug, Clone)]
pub struct LiveStatementEvent {
	/// Hash of the statement
	pub hash: Hash,
	/// SCALE-encoded statement bytes
	pub encoded: Vec<u8>,
	/// Filter ids that matched the statement
	pub matched_filter_ids: Vec<FilterId>,
}

/// Statement store error.
#[derive(Debug, Clone, Eq, PartialEq, thiserror::Error)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Error {
	/// Database error.
	#[error("Database error: {0:?}")]
	Db(String),
	/// Decoding error
	#[error("Decoding error: {0:?}")]
	Decode(String),
	/// Error reading from storage.
	#[error("Storage error: {0:?}")]
	Storage(String),
	/// Invalid configuration.
	#[error("Invalid configuration: {0}")]
	InvalidConfig(String),
}

/// Filter for subscribing to statements with different topics.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub enum TopicFilter {
	/// Matches all topics.
	Any,
	/// Matches only statements including all of the given topics.
	/// Bytes are expected to be a 32-byte topic. Up to [`MAX_TOPICS`] topics can be provided.
	MatchAll(BoundedVec<Topic, ConstU32<{ MAX_TOPICS as u32 }>>),
	/// Matches statements including any of the given topics.
	/// Bytes are expected to be a 32-byte topic. Up to [`MAX_ANY_TOPICS`] topics can be provided.
	MatchAny(BoundedVec<Topic, ConstU32<{ MAX_ANY_TOPICS as u32 }>>),
}

/// Topic filter for statement subscriptions, optimized for matching.
#[derive(Clone, Debug)]
pub enum OptimizedTopicFilter {
	/// Matches all topics.
	Any,
	/// Matches only statements including all of the given topics.
	/// Up to `4` topics can be provided.
	MatchAll(HashSet<Topic>),
	/// Matches statements including any of the given topics.
	/// Up to `128` topics can be provided.
	MatchAny(HashSet<Topic>),
}

impl OptimizedTopicFilter {
	/// Check if the statement matches the filter.
	pub fn matches(&self, statement: &Statement) -> bool {
		match self {
			OptimizedTopicFilter::Any => true,
			OptimizedTopicFilter::MatchAll(topics) => {
				topics.iter().all(|topic| statement.topics().contains(topic))
			},
			OptimizedTopicFilter::MatchAny(topics) => {
				statement.topics().iter().any(|topic| topics.contains(topic))
			},
		}
	}
}

// Convert TopicFilter to CheckedTopicFilter.
impl From<TopicFilter> for OptimizedTopicFilter {
	fn from(filter: TopicFilter) -> Self {
		match filter {
			TopicFilter::Any => OptimizedTopicFilter::Any,
			TopicFilter::MatchAll(topics) => {
				let mut parsed_topics = HashSet::with_capacity(topics.len());
				for topic in topics {
					parsed_topics.insert(topic);
				}
				OptimizedTopicFilter::MatchAll(parsed_topics)
			},
			TopicFilter::MatchAny(topics) => {
				let mut parsed_topics = HashSet::with_capacity(topics.len());
				for topic in topics {
					parsed_topics.insert(topic);
				}
				OptimizedTopicFilter::MatchAny(parsed_topics)
			},
		}
	}
}

/// Reason why a statement was rejected from the store.
#[derive(Debug, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(tag = "reason", rename_all = "camelCase"))]
pub enum RejectionReason {
	/// Statement data exceeds the maximum allowed size for the account.
	DataTooLarge {
		/// The size of the submitted statement data.
		submitted_size: usize,
		/// Still available data size for the account.
		available_size: usize,
	},
	/// Attempting to replace a channel message with lower or equal expiry.
	ChannelPriorityTooLow {
		/// The expiry of the submitted statement.
		submitted_expiry: u64,
		/// The minimum expiry of the existing channel message.
		min_expiry: u64,
	},
	/// Account reached its statement limit and submitted expiry is too low to evict existing.
	AccountFull {
		/// The expiry of the submitted statement.
		submitted_expiry: u64,
		/// The minimum expiry of the existing statement.
		min_expiry: u64,
	},
	/// The global statement store is full and cannot accept new statements.
	StoreFull,
	/// Account has no allowance set.
	NoAllowance,
}

impl RejectionReason {
	/// Returns a short string label suitable for use in metrics.
	pub fn label(&self) -> &'static str {
		match self {
			RejectionReason::DataTooLarge { .. } => "data_too_large",
			RejectionReason::ChannelPriorityTooLow { .. } => "channel_priority_too_low",
			RejectionReason::AccountFull { .. } => "account_full",
			RejectionReason::StoreFull => "store_full",
			RejectionReason::NoAllowance => "no_allowance",
		}
	}
}

/// Reason why a statement failed validation.
#[derive(Debug, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(tag = "reason", rename_all = "camelCase"))]
pub enum InvalidReason {
	/// Statement has no proof.
	NoProof,
	/// Proof validation failed.
	BadProof,
	/// Statement exceeds max allowed statement size.
	EncodingTooLarge {
		/// The size of the submitted statement encoding.
		submitted_size: usize,
		/// The maximum allowed size.
		max_size: usize,
	},
	/// Statement has already expired. The expiry field is in the past.
	AlreadyExpired,
}

impl InvalidReason {
	/// Returns a short string label suitable for use in metrics.
	pub fn label(&self) -> &'static str {
		match self {
			InvalidReason::NoProof => "no_proof",
			InvalidReason::BadProof => "bad_proof",
			InvalidReason::EncodingTooLarge { .. } => "encoding_too_large",
			InvalidReason::AlreadyExpired => "already_expired",
		}
	}
}

/// Statement submission outcome
#[derive(Debug, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(tag = "status", rename_all = "camelCase"))]
pub enum SubmitResult {
	/// Statement was accepted as new.
	New,
	/// Statement was already known.
	Known,
	/// Statement was already known but has expired.
	KnownExpired,
	/// Statement was rejected because the store is full or priority is too low.
	Rejected(RejectionReason),
	/// Statement failed validation.
	Invalid(InvalidReason),
	/// Internal store error.
	InternalError(Error),
}

/// Rejection reason as exposed by the spec-v2 `statement_unstable_submit` RPC.
#[derive(Debug, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
	feature = "serde",
	serde(tag = "reason", rename_all = "camelCase", rename_all_fields = "camelCase")
)]
pub enum SubmitRejectionReason {
	/// Statement data exceeds the maximum allowed size for the account.
	DataTooLarge {
		/// The size of the submitted statement data.
		submitted_size: usize,
		/// Still available data size for the account.
		available_size: usize,
	},
	/// Attempting to replace a channel message with lower or equal expiry.
	ChannelPriorityTooLow {
		/// The expiry of the submitted statement.
		submitted_expiry: u64,
		/// The minimum expiry of the existing channel message.
		min_expiry: u64,
	},
	/// Account reached its statement limit and submitted expiry is too low to evict existing.
	AccountFull {
		/// The expiry of the submitted statement.
		submitted_expiry: u64,
		/// The minimum expiry of the existing statement.
		min_expiry: u64,
	},
	/// The global statement store is full and cannot accept new statements.
	StoreFull,
	/// Account has no allowance set.
	NoAllowance,
}

impl From<RejectionReason> for SubmitRejectionReason {
	fn from(reason: RejectionReason) -> Self {
		match reason {
			RejectionReason::DataTooLarge { submitted_size, available_size } => {
				SubmitRejectionReason::DataTooLarge { submitted_size, available_size }
			},
			RejectionReason::ChannelPriorityTooLow { submitted_expiry, min_expiry } => {
				SubmitRejectionReason::ChannelPriorityTooLow { submitted_expiry, min_expiry }
			},
			RejectionReason::AccountFull { submitted_expiry, min_expiry } => {
				SubmitRejectionReason::AccountFull { submitted_expiry, min_expiry }
			},
			RejectionReason::StoreFull => SubmitRejectionReason::StoreFull,
			RejectionReason::NoAllowance => SubmitRejectionReason::NoAllowance,
		}
	}
}

/// Invalid reason as exposed by the spec-v2 `statement_unstable_submit` RPC.
#[derive(Debug, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
	feature = "serde",
	serde(tag = "reason", rename_all = "camelCase", rename_all_fields = "camelCase")
)]
pub enum SubmitInvalidReason {
	/// Statement has no proof.
	NoProof,
	/// Proof validation failed.
	BadProof,
	/// Statement exceeds max allowed statement size.
	EncodingTooLarge {
		/// The size of the submitted statement encoding.
		submitted_size: usize,
		/// The maximum allowed size.
		max_size: usize,
	},
	/// Statement has already expired. The expiry field is in the past.
	AlreadyExpired,
}

impl From<InvalidReason> for SubmitInvalidReason {
	fn from(reason: InvalidReason) -> Self {
		match reason {
			InvalidReason::NoProof => SubmitInvalidReason::NoProof,
			InvalidReason::BadProof => SubmitInvalidReason::BadProof,
			InvalidReason::EncodingTooLarge { submitted_size, max_size } => {
				SubmitInvalidReason::EncodingTooLarge { submitted_size, max_size }
			},
			InvalidReason::AlreadyExpired => SubmitInvalidReason::AlreadyExpired,
		}
	}
}

/// Statement submission outcome exposed by the spec-v2 `statement_unstable_submit` RPC.
#[derive(Debug, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(tag = "status", rename_all = "camelCase"))]
pub enum SubmitOutcome {
	/// Statement was accepted as new
	New,
	/// Statement was already known
	Known,
	/// Statement was rejected because the store is full or priority is too low
	Rejected(SubmitRejectionReason),
	/// Statement failed validation
	Invalid(SubmitInvalidReason),
}

impl SubmitOutcome {
	/// Converts a store submission result into the RPC-visible outcome
	pub fn from_submit_result(result: SubmitResult) -> std::result::Result<Self, Error> {
		match result {
			SubmitResult::New => Ok(SubmitOutcome::New),
			SubmitResult::Known => Ok(SubmitOutcome::Known),
			// The store only returns `KnownExpired` for `Network` sources; the RPC path is `Local`.
			SubmitResult::KnownExpired => {
				Err(Error::Storage("unexpected KnownExpired on local submission".into()))
			},
			SubmitResult::Rejected(reason) => Ok(SubmitOutcome::Rejected(reason.into())),
			SubmitResult::Invalid(reason) => Ok(SubmitOutcome::Invalid(reason.into())),
			SubmitResult::InternalError(error) => Err(error),
		}
	}
}

/// An item returned by the statement subscription stream.
#[derive(Debug, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(tag = "event", content = "data", rename_all = "camelCase"))]
pub enum StatementEvent {
	/// A batch of statements matching the subscription filter.
	NewStatements {
		/// A batch of statements matching the subscription filter, each entry is a SCALE-encoded
		/// statement.
		statements: Vec<Bytes>,
		/// An optional count of how many more matching statements are in the store after this
		/// batch. This guarantees to the client that it will receive at least this many more
		/// statements in the subscription stream, but it may receive more if new statements are
		/// added to the store that match the filter.
		#[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "Option::is_none"))]
		remaining: Option<u32>,
	},
}

/// Result type for `Error`
pub type Result<T> = std::result::Result<T, Error>;

/// Decision returned by the filter used in [`StatementStore::statements_by_hashes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterDecision {
	/// Skip this statement, continue to next.
	Skip,
	/// Take this statement, continue to next.
	Take,
	/// Stop iteration, return collected statements.
	Abort,
}

/// A batch of statements read from the admission journal by
/// [`StatementStore::admitted_statements`].
#[derive(Debug)]
pub struct AdmittedBatch {
	/// The taken statements, oldest admission first.
	pub statements: Vec<(Hash, Statement)>,
	/// Admission sequence number to resume the walk from.
	pub cursor: u64,
	/// Whether the walk reached the watermark; always equals `cursor >= watermark`.
	pub done: bool,
}

/// Statement store API.
pub trait StatementStore: Send + Sync {
	/// Return all statements.
	fn statements(&self) -> Result<Vec<(Hash, Statement)>>;

	/// Return recent statements with their admission sequence numbers, oldest admission
	/// first, and clear the internal index.
	///
	/// This consumes and clears the recently received statements,
	/// allowing new statements to be collected from this point forward.
	fn take_recent_statements(&self) -> Result<Vec<(u64, Hash, Statement)>>;

	/// Get statement by hash.
	fn statement(&self, hash: &Hash) -> Result<Option<Statement>>;

	/// Check if statement exists in the store
	///
	/// Fast index check without accessing the DB.
	fn has_statement(&self, hash: &Hash) -> bool;

	/// Fetch statements by their hashes with a filter callback.
	///
	/// The callback receives (hash, encoded_bytes, decoded_statement) and returns:
	/// - `Skip`: ignore this statement, continue to next
	/// - `Take`: include this statement in the result, continue to next
	/// - `Abort`: stop iteration, return collected statements so far
	///
	/// Returns (statements, number_of_hashes_processed).
	fn statements_by_hashes(
		&self,
		hashes: &[Hash],
		filter: &mut dyn FnMut(&Hash, &[u8], &Statement) -> FilterDecision,
	) -> Result<(Vec<(Hash, Statement)>, usize)>;

	/// One past the newest assigned admission sequence number.
	///
	/// Every statement currently in the store was admitted below this boundary, so it
	/// serves as the watermark for a subsequent [`Self::admitted_statements`] walk.
	fn admission_watermark(&self) -> Result<u64>;

	/// Walk the admission journal from `cursor` (inclusive) towards `watermark`
	/// (exclusive), collecting statements through a filter callback.
	///
	/// The callback receives (hash, encoded_bytes, decoded_statement) and returns:
	/// - `Skip`: leave this statement out of the batch, continue to next
	/// - `Take`: include this statement in the batch, continue to next
	/// - `Abort`: stop the walk before this statement
	///
	/// Sequence numbers whose statement has left the store are passed over. The returned
	/// cursor sits after every visited statement except an `Abort`ed one, which the next
	/// walk revisits first.
	///
	/// A call visits at most `scan_limit` journal entries, passed-over ones included, so
	/// its cost stays bounded even when the filter takes nothing. `scan_limit` must be
	/// positive: a walk that visits no entries cannot advance the cursor. A walk stopped
	/// by the limit returns a partial cursor to resume from, exactly like an `Abort`.
	fn admitted_statements(
		&self,
		cursor: u64,
		watermark: u64,
		scan_limit: usize,
		filter: &mut dyn FnMut(&Hash, &[u8], &Statement) -> FilterDecision,
	) -> Result<AdmittedBatch>;

	/// Return the data of all known statements which include all topics and have no `DecryptionKey`
	/// field.
	fn broadcasts(&self, match_all_topics: &[Topic]) -> Result<Vec<Vec<u8>>>;

	/// Return the data of all known statements whose decryption key is identified as `dest` (this
	/// will generally be the public key or a hash thereof for symmetric ciphers, or a hash of the
	/// private key for symmetric ciphers).
	fn posted(&self, match_all_topics: &[Topic], dest: [u8; 32]) -> Result<Vec<Vec<u8>>>;

	/// Return the decrypted data of all known statements whose decryption key is identified as
	/// `dest`. The key must be available to the client.
	fn posted_clear(&self, match_all_topics: &[Topic], dest: [u8; 32]) -> Result<Vec<Vec<u8>>>;

	/// Return all known statements which include all topics and have no `DecryptionKey`
	/// field.
	fn broadcasts_stmt(&self, match_all_topics: &[Topic]) -> Result<Vec<Vec<u8>>>;

	/// Return all known statements whose decryption key is identified as `dest` (this
	/// will generally be the public key or a hash thereof for symmetric ciphers, or a hash of the
	/// private key for symmetric ciphers).
	fn posted_stmt(&self, match_all_topics: &[Topic], dest: [u8; 32]) -> Result<Vec<Vec<u8>>>;

	/// Return the statement and the decrypted data of all known statements whose decryption key is
	/// identified as `dest`. The key must be available to the client.
	///
	/// The result is for each statement: the SCALE-encoded statement concatenated to the
	/// decrypted data.
	fn posted_clear_stmt(&self, match_all_topics: &[Topic], dest: [u8; 32])
		-> Result<Vec<Vec<u8>>>;

	/// Submit a statement.
	fn submit(&self, statement: Statement, source: StatementSource) -> SubmitResult;

	/// Remove a statement from the store.
	fn remove(&self, hash: &Hash) -> Result<()>;

	/// Remove all statements authored by `who`.
	fn remove_by(&self, who: [u8; 32]) -> Result<()>;
}
