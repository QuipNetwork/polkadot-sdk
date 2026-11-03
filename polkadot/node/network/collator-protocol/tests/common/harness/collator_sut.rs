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

//! Convenience trait alias bundling the bounds every collator-protocol scenario needs on
//! its generic `S` parameter. Without this, every `#[sim_test] fn name<S>()` has to repeat
//! the same three-line `where` clause that the macro can't synthesise. With it, the
//! scenario reads `fn name<S: CollatorSut>()`.

use polkadot_node_subsystem::messages::{AllMessages, CollatorProtocolMessage};
use polkadot_overseer::AssociateOutgoing;
use polkadot_subsystem_test_sim::harness::SubsystemUnderTest;

/// Blanket-implemented for any `SubsystemUnderTest` whose message type is
/// `CollatorProtocolMessage`. No manual `impl CollatorSut for ...` ever needed.
pub trait CollatorSut: SubsystemUnderTest<Message = CollatorProtocolMessage>
where
	AllMessages: From<<Self::Message as AssociateOutgoing>::OutgoingMessages>,
	AllMessages: From<Self::Message>,
{
}

impl<T> CollatorSut for T
where
	T: SubsystemUnderTest<Message = CollatorProtocolMessage>,
	AllMessages: From<<T::Message as AssociateOutgoing>::OutgoingMessages>,
	AllMessages: From<T::Message>,
{
}
