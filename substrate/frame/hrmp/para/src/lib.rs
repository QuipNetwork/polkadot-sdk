// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! # Parachain HRMP pallet
//!
//! User-facing half of HRMP channel management. Runs on a parachain, holding channel deposits and
//! driving open / accept / close on the relay-chain counterpart (`pallet-hrmp-relay`) over XCM.

#![cfg_attr(not(feature = "std"), no_std)]

pub use pallet::*;

/// Used to send an XCM `Transact` to the HRMP pallet on the remote relay chain.
pub trait SendToRelay {}

#[frame_support::pallet]
pub mod pallet {
	use super::*;

	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// Sends messages to the relay chain.
		type SendToRelay: SendToRelay;
	}

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	// - TODO: hrmp_init_open_channel — request to open a channel; holds the sender deposit, XCM to
	//   the relay.

	// - TODO: hrmp_accept_open_channel — accept a pending open request; holds the recipient
	//   deposit.

	// - TODO: hrmp_close_channel — close a channel; releases both deposits.

	// - TODO: hrmp_cancel_open_request — cancel a pending open request; releases the sender
	//   deposit.

	// - TODO: establish_channel_with_system — open a bidirectional channel with a system chain (no
	//   deposit).

	// - TODO: poke_channel_deposits — re-sync a channel's deposits to the current config.
}
