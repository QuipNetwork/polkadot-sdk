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

//! The observable contract of the collator-protocol subsystem.
//!
//! Outbound messages emitted by the subsystem are classified into one of two categories:
//!
//! - [`Effect`] — protocol-relevant outputs that tests assert on (seconding decisions, wire
//!   messages, peer reputation changes, connect/disconnect commands, request/response sends).
//! - [`Query`] — information-gathering queries that the harness's responder answers (RuntimeApi,
//!   ChainApi, ProspectiveParachains, `CanSecond`). Tests **never** assert on queries — they are
//!   implementation detail.
//!
//! The classifier ([`classify::classify`]) walks an outgoing `AllMessages`, returns either an
//! `Effect` (recorded into the test's observation log) or a `Query` (forwarded to the responder).

pub mod classify;
pub mod effect;
pub mod query;
pub mod reputation;

pub use classify::{classify, peek_effects, Classified};
pub use effect::{AdvertisementSummary, Effect, ReqKind, RequestId, RespKind, WireMsgKind};
pub use query::Query;
pub use reputation::RepBucket;
