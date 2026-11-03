// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
// SPDX-License-Identifier: Apache-2.0

//! V3 scheduling signature verifier backed by parachain Aura authorities.
//!
//! Implements [`VerifySchedulingSignature`] for parachains running Aura: maps the relay slot
//! at `internal_scheduling_parent` (supplied by the caller) to the parachain slot, looks up the
//! eligible Aura author from this pallet's cached authority set, and verifies the 64-byte
//! signature in [`SignedSchedulingInfo`] over the encoded `SchedulingInfoPayload`.

use crate::{Authorities, Config};
use codec::{Decode, Encode};
use cumulus_primitives_core::{
	relay_chain::RELAY_CHAIN_SLOT_DURATION_MILLIS, SignedSchedulingInfo, VerifySchedulingSignature,
};
use sp_application_crypto::RuntimeAppPublic;
use sp_consensus_aura::Slot;

/// Verifier for V3 [`SignedSchedulingInfo`] against parachain Aura authorities.
///
/// Wired by the parachain runtime as
/// `type SchedulingSignatureVerifier = AuraSchedulingVerifier<Runtime>;` on
/// [`cumulus_pallet_parachain_system::Config`]. The Aura crypto is taken from
/// [`pallet_aura::Config::AuthorityId`].
pub struct AuraSchedulingVerifier<T>(core::marker::PhantomData<T>);

impl<T> VerifySchedulingSignature for AuraSchedulingVerifier<T>
where
	T: Config,
	T: pallet_timestamp::Config,
{
	const V3_SCHEDULING_ENABLED: bool = true;

	/// Verify that `signed_info` was produced by the Aura author eligible at the parachain slot
	/// derived from `relay_slot` (the slot of the internal scheduling parent).
	///
	/// Returns `true` only when every step succeeds; all error paths return `false` (fail-closed)
	/// so the PVF rejects the candidate without panicking on adversarial input.
	fn verify(signed_info: &SignedSchedulingInfo, relay_slot: Slot) -> bool {
		// 1. The relay slot at the internal scheduling parent gives the para slot that determines
		//    the valid author.
		let para_slot_duration: u64 =
			match TryInto::<u64>::try_into(pallet_aura::Pallet::<T>::slot_duration()) {
				Ok(d) if d > 0 => d,
				_ => return false,
			};

		let para_slot: u64 = match u64::from(relay_slot)
			.checked_mul(RELAY_CHAIN_SLOT_DURATION_MILLIS)
			.map(|product| product / para_slot_duration)
		{
			Some(s) => s,
			None => return false,
		};

		// 2. Look up the eligible Aura author.
		let authorities = Authorities::<T>::get();
		let author_idx = match pallet_aura::Pallet::<T>::slot_author_index(Slot::from(para_slot)) {
			Some(idx) => idx as usize,
			None => return false,
		};
		let author = match authorities.get(author_idx) {
			Some(author) => author,
			None => return false,
		};

		// 3. Decode the 64-byte signature blob as the authority's expected signature type and
		//    verify over the encoded SchedulingInfoPayload.
		let signature = match <T::AuthorityId as RuntimeAppPublic>::Signature::decode(
			&mut &signed_info.signature[..],
		) {
			Ok(sig) => sig,
			Err(e) => {
				log::error!(
					target: "aura-ext::scheduling-verifier",
					"failed to decode scheduling signature: {e}",
				);
				return false;
			},
		};

		author.verify(&signed_info.payload.encode(), &signature)
	}
}
