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

//! Generic types that can be moved to frame-support once stable.

use super::*;
use core::{
	marker::PhantomData,
	ops::{Div, Rem},
};

/// Bitfield helper for tracking friend votes.
///
/// Uses a vector of u16 values where each bit represents whether a friend at that index has voted.
#[derive(
	CloneNoBound, EqNoBound, PartialEqNoBound, Encode, Decode, DebugNoBound, TypeInfo, MaxEncodedLen,
)]
#[scale_info(skip_type_params(MaxEntries))]
pub struct Bitfield<MaxEntries: Get<u32>>(pub BoundedVec<u16, BitfieldLenOf<MaxEntries>>);

/// Calculates the length of the bitfield in u16 words.
pub type BitfieldLenOf<MaxEntries> = ConstDivCeil<MaxEntries, ConstU32<16>, u32, u32>;

/// Calculate division of two `Get` types and round the result up.
pub struct ConstDivCeil<Dividend, Divisor, R, T>(pub PhantomData<(Dividend, Divisor, R, T)>);

impl<Dividend: Get<T>, Divisor: Get<T>, R: AtLeast32BitUnsigned, T: Into<R>> Get<R>
	for ConstDivCeil<Dividend, Divisor, R, T>
where
	R: Div + Rem + Zero + One + Copy,
{
	fn get() -> R {
		let dividend: R = Dividend::get().into();
		let divisor: R = Divisor::get().into();

		let v = dividend / divisor;
		let remainder = dividend % divisor;

		if remainder.is_zero() {
			v
		} else {
			v + One::one()
		}
	}
}

impl<MaxEntries: Get<u32>> Default for Bitfield<MaxEntries> {
	fn default() -> Self {
		Self(
			vec![0u16; BitfieldLenOf::<MaxEntries>::get() as usize]
				.try_into()
				.expect("Bitfield construction checked in integrity test; qed."),
		)
	}
}

impl<MaxEntries: Get<u32>> Bitfield<MaxEntries> {
	/// Set the bit at the given index to true (friend has voted).
	pub fn set_if_not_set(&mut self, index: usize) -> Result<(), ()> {
		let word_index = index / 16;
		let bit_index = index % 16;

		let word = self.0.get_mut(word_index).ok_or(())?;
		if (*word & (1u16 << bit_index)) == 0 {
			*word |= 1u16 << bit_index;
			Ok(())
		} else {
			Err(())
		}
	}

	/// A new bitfield with the given bit indices set.
	///
	/// Errors if there are duplicate or our of bounds indices.
	pub fn with_bits(mut self, indices: impl IntoIterator<Item = usize>) -> Result<Self, ()> {
		for index in indices {
			self.set_if_not_set(index)?;
		}
		Ok(self)
	}

	/// Count the total number of set bits (total votes).
	pub fn count_ones(&self) -> u32 {
		self.0.iter().cloned().map(u16::count_ones).sum()
	}
}

/// A `Consideration`-like type that tracks who paid for it.
///
/// This is useful in situations where the consideration may be moved around between accounts.
/// Normally, a consideration is just enacted and then later dropped. But if it must be moved
/// between accounts, then tracking this manually is necessary. Hence this type to not blow up the
/// storage type definitions.
#[derive(
	Clone,
	Eq,
	PartialEq,
	Encode,
	Decode,
	Default,
	Debug,
	TypeInfo,
	MaxEncodedLen,
	DecodeWithMemTracking,
)]
pub struct IdentifiedConsideration<AccountId, Footprint, C> {
	/// Account that paid the storage deposit.
	///
	/// This is also the account that will receive the refund.
	pub depositor: AccountId,

	/// Opaque ticket to track the payment of a deposit.
	pub ticket: Option<C>,

	#[doc(hidden)]
	pub _phantom: PhantomData<Footprint>,
}

impl<AccountId: Clone + Eq, Footprint, C: Consideration<AccountId, Footprint>>
	IdentifiedConsideration<AccountId, Footprint, C>
{
	/// Try to take a deposit from `depositor` for the given footprint.
	pub fn new(
		depositor: &AccountId,
		fp: impl Into<Option<Footprint>>,
	) -> Result<Self, DispatchError> {
		let ticket = if let Some(fp) = fp.into() {
			Some(Consideration::<AccountId, Footprint>::new(depositor, fp)?)
		} else {
			None
		};

		Ok(Self { depositor: depositor.clone(), ticket, _phantom: Default::default() })
	}

	/// Update either the depositor or the footprint of the consideration.
	pub fn update(
		self,
		new_depositor: &AccountId,
		new_fp: impl Into<Option<Footprint>>,
	) -> Result<Self, DispatchError> {
		let fp = new_fp.into();
		if let Some(ticket) = self.ticket {
			ticket.drop(&self.depositor)?;
		}

		let ticket = if let Some(fp) = fp {
			Some(Consideration::<AccountId, Footprint>::new(&new_depositor, fp)?)
		} else {
			None
		};
		Ok(Self { depositor: new_depositor.clone(), ticket, _phantom: Default::default() })
	}

	/// Try to drop the consideration and refund the deposit.
	pub fn try_drop(self) -> Result<(), DispatchError> {
		if let Some(ticket) = self.ticket {
			ticket.drop(&self.depositor)?;
		}
		Ok(())
	}
}
