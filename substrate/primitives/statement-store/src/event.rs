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

use alloc::{string::String, vec::Vec};
use sp_core::Bytes;

/// Subscription notification event.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(tag = "event", rename_all = "camelCase"))]
pub enum SubscribeEvent {
	/// Statements admitted before the filter was attached.
	ReplayStatements {
		/// Filter that produced this replay batch.
		#[cfg_attr(feature = "serde", serde(rename = "filterId"))]
		filter_id: String,
		/// SCALE-encoded statements included in this replay batch.
		statements: Vec<Bytes>,
	},
	/// Replay completion marker.
	ReplayDone {
		/// Filter whose replay completed.
		#[cfg_attr(feature = "serde", serde(rename = "filterId"))]
		filter_id: String,
	},
	/// Statements admitted after matching filters were attached.
	NewStatements {
		/// Statement entries included in this notification.
		statements: Vec<NewStatementEntry>,
	},
	/// Terminal notification.
	Stop,
}

/// Statement item included in a `newStatements` notification.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct NewStatementEntry {
	/// SCALE-encoded statement bytes.
	pub statement: Bytes,
	/// Filters that matched this statement.
	#[cfg_attr(feature = "serde", serde(rename = "filterIds"))]
	pub filter_ids: Vec<String>,
}

/// Response returned by `statement_unstable_add_filter`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(untagged))]
pub enum AddFilterResponse {
	/// Filter was added and the string contains its filter id.
	Ok(String),
	/// Filter could not be added because the subscription reached its limit.
	LimitReached(LimitReachedResult),
}

/// Response payload for a limit-reached add-filter result.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct LimitReachedResult {
	/// Machine-readable result tag.
	pub result: LimitReachedTag,
}

/// Result tag returned when the filter limit is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub enum LimitReachedTag {
	/// The subscription cannot accept another filter.
	LimitReached,
}

impl AddFilterResponse {
	/// Returns the limit-reached response.
	pub fn limit_reached() -> Self {
		AddFilterResponse::LimitReached(LimitReachedResult {
			result: LimitReachedTag::LimitReached,
		})
	}
}
