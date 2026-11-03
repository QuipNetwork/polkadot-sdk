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

//! Failure-message reporter for the test framework.
//!
//! When an assertion times out or a forbidden effect is observed, the harness assembles a
//! [`TimelineReport`] of what happened and panics with the formatted message. The report shows
//! the timeline of observed effects relative to the assertion window, plus a replay seed for
//! reproducing the failure.

pub mod timeline;

pub use timeline::{format_effect, format_timeline, TimelineReport};
