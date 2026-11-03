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

//! Stub for `availability-store`. Answers `StoreAvailableData` with `Ok(())` immediately so
//! the backing seconding flow can progress.

use crate::harness::router::{RouteAttempt, SubsystemSlot};
use futures::{channel::mpsc, future::BoxFuture, FutureExt, SinkExt};
use polkadot_node_subsystem::{
	messages::{AllMessages, AvailabilityStoreMessage},
	FromOrchestra, OverseerSignal,
};

/// Always-OK availability store stub.
pub struct AvailabilityStoreStub {
	inbound_tx: mpsc::Sender<FromOrchestra<AvailabilityStoreMessage>>,
}

impl AvailabilityStoreStub {
	/// Spawn the stub on `sim`'s executor.
	pub fn spawn<S>(sim: &mut crate::harness::Sim<S>) -> Self
	where
		S: crate::harness::SubsystemUnderTest,
		AllMessages: From<<S::Message as polkadot_overseer::AssociateOutgoing>::OutgoingMessages>,
		AllMessages: From<S::Message>,
	{
		let (inbound_tx, mut inbound_rx) =
			mpsc::channel::<FromOrchestra<AvailabilityStoreMessage>>(0);
		let fut = async move {
			use futures::StreamExt;
			while let Some(msg) = inbound_rx.next().await {
				match msg {
					FromOrchestra::Signal(OverseerSignal::Conclude) => break,
					FromOrchestra::Signal(_) => {},
					FromOrchestra::Communication { msg } => match msg {
						AvailabilityStoreMessage::StoreAvailableData { tx, .. } => {
							let _ = tx.send(Ok(()));
						},
						AvailabilityStoreMessage::StoreChunk { tx, .. } => {
							let _ = tx.send(Ok(()));
						},
						// Other AvailabilityStoreMessage variants (queries) — drop
						// silently. Tests that rely on them should override this stub.
						_ => {},
					},
				}
			}
		};
		sim.executor_mut().spawn(fut.boxed());
		sim.executor_mut().poll_until_pending();
		Self { inbound_tx }
	}
}

impl SubsystemSlot for AvailabilityStoreStub {
	fn name(&self) -> &'static str {
		"availability-store-stub"
	}

	fn send_signal(&self, signal: OverseerSignal) -> BoxFuture<'static, ()> {
		let mut tx = self.inbound_tx.clone();
		async move {
			let _ = tx.send(FromOrchestra::Signal(signal)).await;
		}
		.boxed()
	}

	fn try_route(&self, msg: AllMessages) -> RouteAttempt {
		match msg {
			AllMessages::AvailabilityStore(inner) => {
				let mut tx = self.inbound_tx.clone();
				let fut = async move {
					let _ = tx.send(FromOrchestra::Communication { msg: inner }).await;
				}
				.boxed();
				RouteAttempt::Accepted(fut)
			},
			other => RouteAttempt::Declined(other),
		}
	}
}
