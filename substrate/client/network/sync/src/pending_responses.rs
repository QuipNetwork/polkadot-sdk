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

//! [`PendingResponses`] is responsible for keeping track of pending responses and
//! polling them. [`Stream`] implemented by [`PendingResponses`] never terminates.

use crate::{strategy::StrategyKey, LOG_TARGET};
use futures::{
	channel::oneshot,
	future::BoxFuture,
	stream::{BoxStream, FusedStream, Stream},
	FutureExt, StreamExt,
};
use log::error;
use std::any::Any;

use sc_network::{request_responses::RequestFailure, types::ProtocolName};
use sc_network_types::PeerId;
use std::task::{Context, Poll, Waker};
use tokio_stream::StreamMap;

/// Response result.
type ResponseResult =
	Result<Result<(Box<dyn Any + Send>, ProtocolName), RequestFailure>, oneshot::Canceled>;

/// A future yielding [`ResponseResult`].
pub(crate) type ResponseFuture = BoxFuture<'static, ResponseResult>;

/// An event we receive once a pending response future resolves.
pub(crate) struct ResponseEvent {
	pub peer_id: PeerId,
	pub key: StrategyKey,
	pub response: ResponseResult,
}

/// Stream taking care of polling pending responses.
pub(crate) struct PendingResponses {
	/// Pending responses
	pending_responses: StreamMap<(PeerId, StrategyKey), BoxStream<'static, ResponseResult>>,
	/// Waker to implement never terminating stream
	waker: Option<Waker>,
}

impl PendingResponses {
	pub fn new() -> Self {
		Self { pending_responses: StreamMap::new(), waker: None }
	}

	pub fn insert(&mut self, peer_id: PeerId, key: StrategyKey, response_future: ResponseFuture) {
		if self
			.pending_responses
			.insert((peer_id, key), Box::pin(response_future.into_stream()))
			.is_some()
		{
			error!(
				target: LOG_TARGET,
				"Discarded pending response from peer {peer_id}, strategy key: {key:?}.",
			);
			debug_assert!(false);
		}

		if let Some(waker) = self.waker.take() {
			waker.wake();
		}
	}

	pub fn remove(&mut self, peer_id: PeerId, key: StrategyKey) -> bool {
		self.pending_responses.remove(&(peer_id, key)).is_some()
	}

	pub fn remove_all(&mut self, peer_id: &PeerId) {
		let to_remove = self
			.pending_responses
			.keys()
			.filter(|(peer, _key)| peer == peer_id)
			.cloned()
			.collect::<Vec<_>>();
		to_remove.iter().for_each(|k| {
			self.pending_responses.remove(k);
		});
	}

	pub fn len(&self) -> usize {
		self.pending_responses.len()
	}
}

impl Stream for PendingResponses {
	type Item = ResponseEvent;

	fn poll_next(
		mut self: std::pin::Pin<&mut Self>,
		cx: &mut Context<'_>,
	) -> Poll<Option<Self::Item>> {
		match self.pending_responses.poll_next_unpin(cx) {
			Poll::Ready(Some(((peer_id, key), response))) => {
				// We need to manually remove the stream, because `StreamMap` doesn't know yet that
				// it's going to yield `None`, so may not remove it before the next request is made
				// to the same peer.
				self.pending_responses.remove(&(peer_id, key));

				Poll::Ready(Some(ResponseEvent { peer_id, key, response }))
			},
			Poll::Ready(None) | Poll::Pending => {
				self.waker = Some(cx.waker().clone());

				Poll::Pending
			},
		}
	}
}

// As [`PendingResponses`] never terminates, we can easily implement [`FusedStream`] for it.
impl FusedStream for PendingResponses {
	fn is_terminated(&self) -> bool {
		false
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn explicit_cancellation_preserves_replacement_response() {
		let mut pending = PendingResponses::new();
		let peer = PeerId::random();
		let key = StrategyKey::new("test");
		let (old_tx, old_rx) = oneshot::channel::<()>();
		pending.insert(
			peer,
			key,
			async move {
				old_rx.await?;
				Ok(Ok((Box::new(1u32) as Box<dyn Any + Send>, ProtocolName::Static("test"))))
			}
			.boxed(),
		);
		assert!(pending.next().now_or_never().is_none());

		// CancelRequest removes the old future before StartRequest installs its replacement.
		assert!(pending.remove(peer, key));
		let (new_tx, new_rx) = oneshot::channel::<()>();
		pending.insert(
			peer,
			key,
			async move {
				new_rx.await?;
				Ok(Ok((Box::new(2u32) as Box<dyn Any + Send>, ProtocolName::Static("test"))))
			}
			.boxed(),
		);
		assert!(old_tx.send(()).is_err());
		new_tx.send(()).unwrap();
		let event = futures::executor::block_on(pending.next()).unwrap();
		assert_eq!(event.peer_id, peer);
		assert_eq!(event.key, key);
		let (response, _) = event.response.unwrap().unwrap();
		assert_eq!(*response.downcast::<u32>().unwrap(), 2);
		assert_eq!(pending.len(), 0);
		assert!(pending.next().now_or_never().is_none());
	}
}
