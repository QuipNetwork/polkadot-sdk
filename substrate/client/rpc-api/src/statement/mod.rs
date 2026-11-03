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

//! Substrate Statement Store RPC API.
//!
//! Exposes two JSON-RPC methods: `statement_subscribeStatement` (a subscription streaming matching
//! statements as `StatementEvent` notifications on `statement_statement`) and `statement_submit`
//! (submit a SCALE-encoded statement). See the `StatementApi` trait below for the wire-format
//! examples.

use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use sp_core::Bytes;
use sp_statement_store::{StatementEvent, SubmitResult, TopicFilter};

pub mod error;

/// Substrate statement RPC API
#[rpc(client, server)]
pub trait StatementApi {
	/// Subscribe to new statements that match the provided filters.
	///
	/// # Parameters
	///
	/// - `topic_filter` — Which topics to match. Use `TopicFilter::Any` to match all topics,
	///   `TopicFilter::MatchAll(vec)` to match statements that include all provided topics, or
	///   `TopicFilter::MatchAny(vec)` to match statements that include any of the provided topics.
	///
	/// # Returns
	///
	/// Returns a stream of `StatementEvent` values.
	/// When a subscription is initiated the endpoint will first return all matching statements
	/// already in the store in batches as `StatementEvent::NewStatements`.
	///
	/// NewStatements includes an Optional field `remaining` which indicates how many more
	/// statements are left to be sent in the initial batch of existing statements. The field
	/// guarantees to the client that it will receive at least this many more statements in the
	/// subscription stream, but it may receive more if new statements are added to the store that
	/// match the filter.
	///
	///  If there are no statements in the store matching the filter, an empty batch of statements
	/// is sent.
	///
	/// # Examples
	///
	/// Subscribe, matching statements that include *all* of the given topics (use `"any"` to match
	/// everything, or `{ "matchAny": [...] }` for any-of):
	///
	/// ```json
	/// { "jsonrpc": "2.0", "id": 1, "method": "statement_subscribeStatement",
	///   "params": [{ "matchAll": ["0xdede…", "0xadad…"] }] }
	/// ```
	///
	/// Notifications arrive on `statement_statement`. The already-stored matches are delivered
	/// first in batches, each carrying `remaining` (how many more are guaranteed to follow). An
	/// empty initial batch is sent when nothing matches:
	///
	/// ```json
	/// { "jsonrpc": "2.0", "method": "statement_statement",
	///   "params": { "subscription": 4851578855668545,
	///     "result": { "event": "newStatements", "data": { "statements": [], "remaining": 0 } } } }
	/// ```
	///
	/// A non-empty batch from the initial set (each statement is hex-encoded SCALE):
	///
	/// ```json
	/// { "jsonrpc": "2.0", "method": "statement_statement",
	///   "params": { "subscription": 4851578855668545,
	///     "result": { "event": "newStatements",
	///       "data": { "statements": ["0x1000010000", "0x100001000000"], "remaining": 10 } } } }
	/// ```
	///
	/// Statements arriving live, after the initial set is drained, carry no `remaining`:
	///
	/// ```json
	/// { "jsonrpc": "2.0", "method": "statement_statement",
	///   "params": { "subscription": 4851578855668545,
	///     "result": { "event": "newStatements", "data": { "statements": ["0x1000010000"] } } } }
	/// ```
	#[subscription(
		name = "statement_subscribeStatement" => "statement_statement",
		unsubscribe = "statement_unsubscribeStatement",
		item = StatementEvent,
		with_extensions,
	)]
	fn subscribe_statement(&self, topic_filter: TopicFilter);

	/// Submit a SCALE-encoded statement.
	///
	/// See `Statement` definition for more details.
	///
	/// Returns `SubmitResult` indicating success or failure reason.
	///
	/// # Examples
	///
	/// ```json
	/// { "jsonrpc": "2.0", "id": 2, "method": "statement_submit", "params": ["0x…scale-encoded…"] }
	/// ```
	///
	/// On success the result is `{ "status": "new" }`. Other outcomes are `known`, `knownExpired`,
	/// `rejected` and `invalid` (each with a reason), and `internalError`.
	#[method(name = "statement_submit")]
	fn submit(&self, encoded: Bytes) -> RpcResult<SubmitResult>;
}
