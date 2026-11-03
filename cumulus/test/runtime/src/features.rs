// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.

// Cumulus is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Cumulus is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Cumulus.  If not, see <http://www.gnu.org/licenses/>.

pub(crate) const SCHEDULING_V3_ENABLED: bool = cfg!(feature = "v3-descriptor");

pub(crate) const fn relay_parent_offset() -> u32 {
	if cfg!(feature = "relay-parent-offset-15") {
		return 15;
	}
	if cfg!(feature = "relay-parent-offset-6") {
		return 6;
	}
	if cfg!(feature = "relay-parent-offset-4") {
		return 4;
	}
	if cfg!(feature = "relay-parent-offset-2") {
		return 2;
	}

	0
}

pub(crate) const fn slot_duration() -> u64 {
	if cfg!(feature = "18s-slot") {
		return 18000;
	}

	if cfg!(feature = "12s-slot") {
		return 12000;
	}

	6000
}

pub(crate) const fn block_processing_velocity() -> u32 {
	if cfg!(feature = "velocity-12") {
		return 12;
	}

	if cfg!(feature = "velocity-6") {
		return 6;
	}

	if cfg!(feature = "velocity-3") {
		return 3;
	}

	1
}

pub(crate) const fn unincluded_segment_capacity() -> u32 {
	if cfg!(feature = "sync-backing") {
		return 1;
	}

	// Without sync backing, the block flow is the following:
	//
	// - Collator produces the block(s) on relay chain block `X`
	// - In the meantime the relay chain is building block `X + 1`
	// - The collator sends the collation to the relay chain, and it gets backed on chain in relay
	//   block `X + 2`
	// - The collation then gets included on chain in relay block `X + 3`
	//
	// With `relay_parent_offset() = N`, the collator builds on relay tip `R - N` while the
	// chain is at `R`, so the buffer must additionally absorb `N * velocity` parablocks worth
	// of in-flight blocks between the relay parent and the relay tip.
	block_processing_velocity() * (3 + relay_parent_offset())
}
