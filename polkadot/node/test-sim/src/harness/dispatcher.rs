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

//! Dispatcher: drains outgoing subsystem messages, routes them to either the [`Recorder`]
//! (effects) or the responder (queries) per the [`crate::contract::classify()`] rule.
//!
//! The full responder DSL lands in `crate::responder`. This file defines the dispatch loop and
//! the [`AnswerQuery`] trait that the responder will implement.

use crate::{
	contract::{classify, Classified, Query},
	harness::{pending_fetches::PendingFetches, Recorder},
};
use polkadot_node_subsystem::messages::AllMessages;
use std::time::Duration;

/// Trait implemented by anything that can answer subsystem queries.
///
/// Two methods, with one of them having a default implementation:
///
/// - [`AnswerQuery::try_answer`] is used by the harness's responder chain. It returns `Some(query)`
///   if the responder declined the query (so the next layer gets a chance), or `None` if it
///   consumed it.
/// - [`AnswerQuery::answer`] is the convenience entry point for unit tests and for responders that
///   always handle (or always panic on) every query they see.
///
/// Most responders implement `answer` only. `try_answer` then defaults to "delegate to
/// `answer`, never decline". Composable responders (e.g. a chain-model wrapper that only
/// handles `Runtime`/`ChainApi`) override `try_answer` so the harness can fall through to a
/// later layer.
pub trait AnswerQuery {
	/// Try to answer the query. Return `None` if the query was handled, or `Some(query)` to
	/// pass it on to the next responder in the chain.
	///
	/// Default: delegate to [`answer`] and return `None`.
	///
	/// [`answer`]: AnswerQuery::answer
	fn try_answer(&mut self, query: Query) -> Option<Query> {
		self.answer(query);
		None
	}

	/// Always-consume entry point. Implementations write back to the embedded
	/// `oneshot::Sender`s. If a query is unexpected, implementations panic with a
	/// descriptive message — that is a test bug (the test forgot to script a particular
	/// query path).
	fn answer(&mut self, _query: Query) {
		panic!(
			"AnswerQuery::answer was called on a responder that only implements \
			 try_answer; declined queries should be handled by a later responder layer"
		);
	}
}

/// A layered responder: tries each layer in order; the first that returns `None` wins. If
/// all decline, panics.
///
/// Use to compose multiple responders. The chain is sometimes hand-built; more often
/// [`crate::harness::Sim`] populates it for you (chain model + per-test query script + a
/// default panic-on-unhandled tail).
pub struct LayeredResponder {
	layers: Vec<Box<dyn AnswerQuery>>,
}

impl LayeredResponder {
	/// New empty chain.
	pub fn new() -> Self {
		Self { layers: Vec::new() }
	}

	/// Append a layer to the end of the chain.
	pub fn push<R: AnswerQuery + 'static>(&mut self, responder: R) {
		self.layers.push(Box::new(responder));
	}

	/// Append a pre-boxed layer.
	pub fn push_boxed(&mut self, responder: Box<dyn AnswerQuery>) {
		self.layers.push(responder);
	}
}

impl Default for LayeredResponder {
	fn default() -> Self {
		Self::new()
	}
}

impl AnswerQuery for LayeredResponder {
	fn try_answer(&mut self, query: Query) -> Option<Query> {
		let mut current = Some(query);
		for layer in self.layers.iter_mut() {
			let q = current.take().expect("loop invariant: current is Some at top of iteration");
			match layer.try_answer(q) {
				None => return None,
				Some(declined) => current = Some(declined),
			}
		}
		// Every layer declined.
		current
	}

	fn answer(&mut self, query: Query) {
		match self.try_answer(query) {
			None => {},
			Some(unhandled) => panic!(
				"LayeredResponder: every layer declined the query; install a fall-through \
				 layer (e.g. PanicResponder) or extend an existing layer to cover this case. \
				 Unhandled query: {:?}",
				unhandled
			),
		}
	}
}

/// Drains a single outgoing `AllMessages`, classifies it, and routes:
/// - Effects → recorded into `recorder`.
/// - Queries → forwarded to `responder`.
///
/// Effects are stamped with `sim_t`, the simulated time elapsed since the start of the
/// scenario, supplied per `dispatch` call.
pub struct Dispatcher<'a, R: AnswerQuery + ?Sized> {
	/// Where effects accumulate.
	pub recorder: &'a mut Recorder,
	/// Where queries are routed.
	pub responder: &'a mut R,
	/// Side table for pending fetch response senders extracted from `SendRequests`.
	pub pending: &'a mut PendingFetches,
}

impl<'a, R: AnswerQuery + ?Sized> Dispatcher<'a, R> {
	/// Create a new dispatcher.
	pub fn new(
		recorder: &'a mut Recorder,
		responder: &'a mut R,
		pending: &'a mut PendingFetches,
	) -> Self {
		Self { recorder, responder, pending }
	}

	/// Process a single outbound message. One inbound message can yield multiple classified
	/// entries (e.g. a batched `SendRequests` or `SendCollationMessages`); the dispatcher
	/// records / forwards them in order.
	pub fn dispatch(&mut self, sim_t: Duration, msg: AllMessages) {
		for c in classify(msg, self.pending) {
			match c {
				Classified::Effect(effect) => self.recorder.record_effect(sim_t, effect),
				Classified::Query(query) => self.responder.answer(query),
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::contract::Effect;
	use polkadot_node_network_protocol::peer_set::PeerSet;
	use polkadot_node_subsystem::messages::{ChainApiMessage, NetworkBridgeTxMessage};

	struct AcceptOnly<F: FnMut(Query)>(F);
	impl<F: FnMut(Query)> AnswerQuery for AcceptOnly<F> {
		fn answer(&mut self, query: Query) {
			(self.0)(query);
		}
	}

	struct DeclineOnly;
	impl AnswerQuery for DeclineOnly {
		fn try_answer(&mut self, query: Query) -> Option<Query> {
			Some(query)
		}
	}

	#[test]
	fn layered_responder_first_accept_wins() {
		use std::sync::{Arc, Mutex};
		let hits_a = Arc::new(Mutex::new(0));
		let hits_b = Arc::new(Mutex::new(0));
		let a_clone = hits_a.clone();
		let b_clone = hits_b.clone();
		let mut chain = LayeredResponder::new();
		chain.push(AcceptOnly(move |_| {
			*a_clone.lock().unwrap() += 1;
		}));
		chain.push(AcceptOnly(move |_| {
			*b_clone.lock().unwrap() += 1;
		}));
		let (tx, _rx) = futures::channel::oneshot::channel();
		chain.answer(Query::ChainApi(ChainApiMessage::FinalizedBlockNumber(tx)));
		assert_eq!(*hits_a.lock().unwrap(), 1);
		assert_eq!(*hits_b.lock().unwrap(), 0);
	}

	#[test]
	fn layered_responder_falls_through_on_decline() {
		use std::sync::{Arc, Mutex};
		let hits = Arc::new(Mutex::new(0));
		let hits_clone = hits.clone();
		let mut chain = LayeredResponder::new();
		chain.push(DeclineOnly);
		chain.push(AcceptOnly(move |_| {
			*hits_clone.lock().unwrap() += 1;
		}));
		let (tx, _rx) = futures::channel::oneshot::channel();
		chain.answer(Query::ChainApi(ChainApiMessage::FinalizedBlockNumber(tx)));
		assert_eq!(*hits.lock().unwrap(), 1);
	}

	#[test]
	#[should_panic(expected = "every layer declined")]
	fn layered_responder_panics_when_all_decline() {
		let mut chain = LayeredResponder::new();
		chain.push(DeclineOnly);
		chain.push(DeclineOnly);
		let (tx, _rx) = futures::channel::oneshot::channel();
		chain.answer(Query::ChainApi(ChainApiMessage::FinalizedBlockNumber(tx)));
	}

	struct PanicResponder;
	impl AnswerQuery for PanicResponder {
		fn answer(&mut self, query: Query) {
			panic!("unexpected query: {:?}", query);
		}
	}

	#[test]
	fn effect_message_records_into_recorder() {
		let mut rec = Recorder::new();
		let mut resp = PanicResponder;
		let mut pending = PendingFetches::new();
		let mut disp = Dispatcher::new(&mut rec, &mut resp, &mut pending);
		let peer = sc_network_types::PeerId::random();
		let msg = AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::DisconnectPeers(
			vec![peer],
			PeerSet::Collation,
		));
		disp.dispatch(Duration::ZERO, msg);
		assert_eq!(rec.len(), 1);
		assert!(matches!(
			rec.effects().next().unwrap(),
			Effect::DisconnectPeers { peer_set: PeerSet::Collation, .. }
		));
	}

	struct CountingResponder {
		count: usize,
	}
	impl AnswerQuery for CountingResponder {
		fn answer(&mut self, _query: Query) {
			self.count += 1;
		}
	}

	#[test]
	fn query_message_forwards_to_responder() {
		use polkadot_node_subsystem::messages::ChainApiMessage;
		let mut rec = Recorder::new();
		let mut resp = CountingResponder { count: 0 };
		let mut pending = PendingFetches::new();
		let mut disp = Dispatcher::new(&mut rec, &mut resp, &mut pending);
		let (tx, _rx) = futures::channel::oneshot::channel();
		let msg = AllMessages::ChainApi(ChainApiMessage::FinalizedBlockNumber(tx));
		disp.dispatch(Duration::ZERO, msg);
		assert_eq!(rec.len(), 0, "query is not recorded as effect");
		assert_eq!(resp.count, 1, "query is forwarded to responder");
	}
}
