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

//! Script of mock answers to subsystem queries.
//!
//! ```ignore
//! let script = QueryScript::builder()
//!     .can_second(|_| true)
//!     .build();
//! ```
//!
//! A script is a set of typed handlers, one per query family. Each handler receives the typed
//! query (and its embedded reply channel) and answers via that channel. Unhandled queries panic
//! with a clear message — the test forgot to script the path.

use crate::{contract::Query, harness::dispatcher::AnswerQuery};
use polkadot_node_subsystem::messages::{
	CandidateBackingMessage, ChainApiMessage, ProspectiveParachainsMessage, RuntimeApiMessage,
};

/// Boxed handler per query family.
type RuntimeHandler = Box<dyn FnMut(RuntimeApiMessage)>;
type ChainApiHandler = Box<dyn FnMut(ChainApiMessage)>;
type ProspectiveHandler = Box<dyn FnMut(ProspectiveParachainsMessage)>;
type CanSecondHandler = Box<dyn FnMut(CandidateBackingMessage)>;

/// A script of mock answers. Drive by passing it as the `responder` to the harness's
/// dispatcher. Construct via [`QueryScript::builder`].
pub struct QueryScript {
	runtime: Option<RuntimeHandler>,
	chain_api: Option<ChainApiHandler>,
	prospective: Option<ProspectiveHandler>,
	can_second: Option<CanSecondHandler>,
}

impl QueryScript {
	/// Open a fresh builder.
	pub fn builder() -> QueryScriptBuilder {
		QueryScriptBuilder::default()
	}
}

impl AnswerQuery for QueryScript {
	fn answer(&mut self, query: Query) {
		match query {
			Query::Runtime(msg) => match self.runtime.as_mut() {
				Some(h) => h(msg),
				None => panic!(
					"QueryScript missing handler for RuntimeApi queries; \
					 .runtime(...) on the builder. Got: {:?}",
					msg
				),
			},
			Query::ChainApi(msg) => match self.chain_api.as_mut() {
				Some(h) => h(msg),
				None => panic!(
					"QueryScript missing handler for ChainApi queries; \
					 .chain_api(...) on the builder. Got: {:?}",
					msg
				),
			},
			Query::Prospective(msg) => match self.prospective.as_mut() {
				Some(h) => h(msg),
				None => panic!(
					"QueryScript missing handler for ProspectiveParachains queries; \
					 .prospective(...) on the builder. Got: {:?}",
					msg
				),
			},
			Query::CanSecond(msg) => match self.can_second.as_mut() {
				Some(h) => h(msg),
				None => panic!(
					"QueryScript missing handler for CanSecond; \
					 .can_second(...) on the builder. Got: {:?}",
					msg
				),
			},
		}
	}
}

/// Builder for [`QueryScript`].
#[derive(Default)]
pub struct QueryScriptBuilder {
	runtime: Option<RuntimeHandler>,
	chain_api: Option<ChainApiHandler>,
	prospective: Option<ProspectiveHandler>,
	can_second: Option<CanSecondHandler>,
}

impl QueryScriptBuilder {
	/// Install a handler for `RuntimeApi` queries.
	pub fn runtime<F>(mut self, f: F) -> Self
	where
		F: FnMut(RuntimeApiMessage) + 'static,
	{
		self.runtime = Some(Box::new(f));
		self
	}

	/// Install a handler for `ChainApi` queries.
	pub fn chain_api<F>(mut self, f: F) -> Self
	where
		F: FnMut(ChainApiMessage) + 'static,
	{
		self.chain_api = Some(Box::new(f));
		self
	}

	/// Install a handler for `ProspectiveParachains` queries.
	pub fn prospective<F>(mut self, f: F) -> Self
	where
		F: FnMut(ProspectiveParachainsMessage) + 'static,
	{
		self.prospective = Some(Box::new(f));
		self
	}

	/// Install a handler for `CandidateBacking::CanSecond` queries.
	pub fn can_second<F>(mut self, f: F) -> Self
	where
		F: FnMut(CandidateBackingMessage) + 'static,
	{
		self.can_second = Some(Box::new(f));
		self
	}

	/// Finish building. The resulting [`QueryScript`] panics on any unhandled query family.
	pub fn build(self) -> QueryScript {
		QueryScript {
			runtime: self.runtime,
			chain_api: self.chain_api,
			prospective: self.prospective,
			can_second: self.can_second,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::harness::dispatcher::AnswerQuery;
	use polkadot_node_subsystem::messages::ChainApiMessage;

	#[test]
	fn handler_receives_chain_api_query() {
		let mut count = 0;
		let mut script = QueryScript::builder()
			.chain_api(move |_msg| {
				count += 1;
				let _ = count;
			})
			.build();

		let (tx, _rx) = futures::channel::oneshot::channel();
		script.answer(Query::ChainApi(ChainApiMessage::FinalizedBlockNumber(tx)));
		// no panic: handler ran.
	}

	#[test]
	#[should_panic(expected = "QueryScript missing handler for ChainApi")]
	fn missing_handler_panics() {
		let mut script = QueryScript::builder().build();
		let (tx, _rx) = futures::channel::oneshot::channel();
		script.answer(Query::ChainApi(ChainApiMessage::FinalizedBlockNumber(tx)));
	}
}
