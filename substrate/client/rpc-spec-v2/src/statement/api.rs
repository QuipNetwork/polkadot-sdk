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

#![allow(non_snake_case)]

use crate::statement::error::Error;
use jsonrpsee::proc_macros::rpc;
use sp_core::Bytes;
use sp_statement_store::{AddFilterResponse, SubmitOutcome, SubscribeEvent, TopicFilter};

#[rpc(client, server)]
pub trait StatementSpecApi {
	/// Opens a new statement subscription
	#[subscription(
		name = "statement_unstable_subscribe" => "statement_unstable_subscribeEvent",
		unsubscribe = "statement_unstable_unsubscribe",
		item = SubscribeEvent,
		with_extensions,
	)]
	async fn statement_unstable_subscribe(&self);

	/// Attaches a filter to an existing subscription
	#[method(name = "statement_unstable_add_filter", with_extensions)]
	async fn statement_unstable_add_filter(
		&self,
		subscription: String,
		topic_filter: TopicFilter,
	) -> Result<AddFilterResponse, Error>;

	/// Detaches a filter from a subscription
	#[method(name = "statement_unstable_remove_filter", with_extensions)]
	fn statement_unstable_remove_filter(
		&self,
		subscription: String,
		filter_id: String,
	) -> Result<(), Error>;

	/// Submits a SCALE-encoded statement to the store
	#[method(name = "statement_unstable_submit")]
	fn statement_unstable_submit(&self, encoded: Bytes) -> Result<SubmitOutcome, Error>;
}
