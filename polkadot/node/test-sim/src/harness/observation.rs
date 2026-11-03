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

//! Timestamped observation entries collected by the recorder.

use crate::contract::Effect;
use std::time::Duration;

/// A value paired with the simulated time at which it was observed.
#[derive(Debug, Clone)]
pub struct Stamped<T> {
	/// Simulated time elapsed since the start of the scenario.
	pub sim_t: Duration,
	/// Wrapped value.
	pub value: T,
}

/// Single observation entry recorded by the harness. Currently always an effect; a future
/// extension may add `QueryAnswered` etc. when responder traces are needed.
#[derive(Debug, Clone)]
pub enum Observation {
	/// The subsystem emitted an [`Effect`].
	Effect(Stamped<Effect>),
}
