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

use crate::{
	statement::{
		api::StatementSpecApiServer,
		error::Error,
		subscription::{
			filter_id_to_string, parse_filter_id, send_subscription_event, StatementSubscriptions,
		},
		LOG_TARGET,
	},
	SubscriptionTaskExecutor,
};
use codec::DecodeAll;
use futures::{FutureExt, StreamExt};
use jsonrpsee::{
	core::async_trait, types::SubscriptionId, ConnectionId, Extensions, PendingSubscriptionSink,
};
use sc_rpc::utils::Subscription;
use sc_statement_store::{AddFilterError, MultiFilterSubscriptionApi};
use sp_core::Bytes;
use sp_statement_store::{
	AddFilterResponse, OptimizedTopicFilter, Statement, StatementSource, StatementStore,
	SubmitOutcome, TopicFilter,
};
use std::sync::Arc;

/// JSON-RPC server implementation for the `statement_unstable_*` methods
pub struct StatementSpec<B> {
	store: Arc<B>,
	executor: SubscriptionTaskExecutor,
	subscriptions: StatementSubscriptions,
}

impl<B> StatementSpec<B>
where
	B: StatementStore + Send + Sync + 'static,
	Arc<B>: MultiFilterSubscriptionApi,
{
	/// Creates a new statement RPC implementation
	pub fn new(store: Arc<B>, executor: SubscriptionTaskExecutor) -> Self {
		Self { store, executor, subscriptions: StatementSubscriptions::new() }
	}
}

fn subscription_id_to_string(id: SubscriptionId) -> String {
	match id {
		SubscriptionId::Num(n) => n.to_string(),
		SubscriptionId::Str(s) => s.into_owned(),
	}
}

fn connection_id(ext: &Extensions) -> ConnectionId {
	ext.get::<ConnectionId>()
		.copied()
		.expect("ConnectionId is always set by jsonrpsee; qed")
}

fn validate_topic_filter(filter: TopicFilter) -> Result<OptimizedTopicFilter, Error> {
	match &filter {
		TopicFilter::MatchAll(topics) if topics.is_empty() => Err(Error::InvalidParam(
			"`matchAll` topic filter must contain between 1 and 4 topics".to_string(),
		)),
		TopicFilter::MatchAny(_) => Err(Error::InvalidParam(
			"`matchAny` topic filter is not supported by statement_unstable_add_filter; \
			 use `\"any\"` or `{\"matchAll\": [...]}` instead"
				.to_string(),
		)),
		_ => Ok(filter.into()),
	}
}

#[async_trait]
impl<B> StatementSpecApiServer for StatementSpec<B>
where
	B: StatementStore + Send + Sync + 'static,
	Arc<B>: MultiFilterSubscriptionApi,
{
	async fn statement_unstable_subscribe(
		&self,
		pending: PendingSubscriptionSink,
		_ext: &Extensions,
	) {
		let subscriptions = self.subscriptions.clone();
		let store = self.store.clone();
		let connection_id = pending.connection_id();
		let sub_id = subscription_id_to_string(pending.subscription_id());

		let (handle, mut live_stream) = store.create_subscription();

		let Some(entry) = subscriptions.register(connection_id, sub_id.clone(), handle) else {
			log::debug!(target: LOG_TARGET, "duplicate subscription id {sub_id}; aborting");
			let _ = pending.reject(Error::InvalidSubscription).await;
			return;
		};

		// On accept failure, dropping `entry` unregisters the subscription.
		let Ok(sink) = pending.accept().await.map(Subscription::from) else { return };

		let fut = async move {
			// Keep the registry entry alive for as long as the subscription task is running;
			// dropping it unregisters this subscription from subsequent filter operations.
			let _subscription_entry = entry;
			loop {
				tokio::select! {
					_ = sink.closed() => {
						log::debug!(
							target: LOG_TARGET,
							"Statement subscription sink closed (connection={connection_id:?}, \
							 sub_id={sub_id}); terminating subscription task",
						);
						break;
					},
					event = live_stream.next() => match event {
						Some(event) => {
							if !send_subscription_event(&sink, event).await {
								log::debug!(
									target: LOG_TARGET,
									"Failed to send statement subscription event \
									 (connection={connection_id:?}, sub_id={sub_id}); terminating \
									 subscription task",
								);
								break;
							}
						},
						None => {
							log::debug!(
								target: LOG_TARGET,
								"Statement live event stream ended (connection={connection_id:?}, \
								 sub_id={sub_id}); terminating subscription task",
							);
							break;
						},
					},
				}
			}
		};

		self.executor
			.spawn("statement-unstable-subscribe-init", Some("rpc"), fut.boxed());
	}

	async fn statement_unstable_add_filter(
		&self,
		ext: &Extensions,
		subscription: String,
		topic_filter: TopicFilter,
	) -> Result<AddFilterResponse, Error> {
		let conn_id = connection_id(ext);
		let topic_filter = validate_topic_filter(topic_filter)?;

		let Some(state) = self.subscriptions.get(conn_id, &subscription) else {
			log::trace!(
				target: LOG_TARGET,
				"add_filter for unknown subscription {subscription} on connection {conn_id:?}",
			);
			return Err(Error::InvalidSubscription);
		};

		match state.add_filter(topic_filter) {
			Ok(filter_id) => Ok(AddFilterResponse::Ok(filter_id_to_string(filter_id))),
			Err(AddFilterError::LimitReached) => Ok(AddFilterResponse::limit_reached()),
			Err(AddFilterError::Stopped) => {
				Err(Error::InternalError("statement subscription matcher stopped".into()))
			},
		}
	}

	fn statement_unstable_remove_filter(
		&self,
		ext: &Extensions,
		subscription: String,
		filter_id: String,
	) -> Result<(), Error> {
		let conn_id = connection_id(ext);
		let Some(state) = self.subscriptions.get(conn_id, &subscription) else { return Ok(()) };
		let Some(parsed) = parse_filter_id(&filter_id) else { return Ok(()) };
		let _ = state.remove_filter(parsed);
		Ok(())
	}

	fn statement_unstable_submit(&self, encoded: Bytes) -> Result<SubmitOutcome, Error> {
		let statement = Statement::decode_all(&mut &encoded[..])
			.map_err(|e| Error::InvalidParam(format!("Error decoding statement: {e}")))?;
		let submit_result = self.store.submit(statement, StatementSource::Local);
		SubmitOutcome::from_submit_result(submit_result)
			.map_err(|e| Error::InternalError(e.to_string()))
	}
}
