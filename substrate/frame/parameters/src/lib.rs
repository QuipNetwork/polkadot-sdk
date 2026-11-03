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

#![cfg_attr(not(feature = "std"), no_std)]
#![deny(missing_docs)]
// Need to enable this one since we document feature-gated stuff.
#![allow(rustdoc::broken_intra_doc_links)]

//! # Parameters
//!
//! Allows to update configuration parameters at runtime.
//!
//! ## Pallet API
//!
//! This pallet exposes two APIs; one *inbound* side to update parameters, and one *outbound* side
//! to access said parameters. Parameters themselves are defined in the runtime config and will be
//! aggregated into an enum. Each parameter is addressed by a `key` and can have a default value.
//! This is not done by the pallet but through the [`frame_support::dynamic_params::dynamic_params`]
//! macro or alternatives.
//!
//! Note that this is incurring one storage read per access. This should not be a problem in most
//! cases but must be considered in weight-restrained code.
//!
//! ### Inbound
//!
//! The inbound side solely consists of the [`Pallet::set_parameter`] extrinsic to update the value
//! of a parameter. Each parameter can have their own admin origin as given by the
//! [`Config::AdminOrigin`].
//!
//! ### Outbound
//!
//! The outbound side is runtime facing for the most part. More general, it provides a `Get`
//! implementation and can be used in every spot where that is accepted. Two macros are in place:
//! [`frame_support::dynamic_params::define_parameters` and
//! [`frame_support::dynamic_params:dynamic_pallet_params`] to define and expose parameters in a
//! typed manner.
//!
//! See the [`pallet`] module for more information about the interfaces this pallet exposes,
//! including its configuration trait, dispatchables, storage items, events and errors.
//!
//! ## Overview
//!
//! This pallet is a good fit for updating parameters without a runtime upgrade. It is very handy to
//! not require a runtime upgrade for a simple parameter change since runtime upgrades require a lot
//! of diligence and always bear risks. It seems overkill to update the whole runtime for a simple
//! parameter change. This pallet allows for fine-grained control over who can update what.
//! The only down-side is that it trades off performance with convenience and should therefore only
//! be used in places where that is proven to be uncritical. Values that are rarely accessed but
//! change often would be a perfect fit.
//!
//! ### Example Configuration
//!
//! Here is an example of how to define some parameters, including their default values:
#![doc = docify::embed!("src/tests/mock.rs", dynamic_params)]
//! A permissioned origin can be define on a per-key basis like this:
#![doc = docify::embed!("src/tests/mock.rs", custom_origin)]
//! The pallet will also require a default value for benchmarking. Ideally this is the variant with
//! the longest encoded length. Although in either case the PoV benchmarking will take the worst
//! case over the whole enum.
#![doc = docify::embed!("src/tests/mock.rs", benchmarking_default)]
//! Now the aggregated parameter needs to be injected into the pallet config:
#![doc = docify::embed!("src/tests/mock.rs", impl_config)]
//! As last step, the parameters can now be used in other pallets 🙌
#![doc = docify::embed!("src/tests/mock.rs", usage)]
//! ### Examples Usage
//!
//! Now to demonstrate how the values can be updated:
#![doc = docify::embed!("src/tests/unit.rs", set_parameters_example)]
//! ## Low Level / Implementation Details
//!
//! The pallet stores the parameters in a storage map and implements the matching `Get<Value>` for
//! each `Key` type. The `Get` then accesses the `Parameters` map to retrieve the value. An event is
//! emitted every time that a value was updated. It is even emitted when the value is changed to the
//! same.
//!
//! The key and value types themselves are defined by macros and aggregated into a runtime wide
//! enum. This enum is then injected into the pallet. This allows it to be used without any changes
//! to the pallet that the parameter will be utilized by.
//!
//! ### Design Goals
//!
//! 1. Easy to update without runtime upgrade.
//! 2. Exposes metadata and docs for user convenience.
//! 3. Can be permissioned on a per-key base.
//!
//! ### Design
//!
//! 1. Everything is done at runtime without the need for `const` values. `Get` allows for this -
//! which is coincidentally an upside and a downside. 2. The types are defined through macros, which
//! allows to expose metadata and docs. 3. Access control is done through the `EnsureOriginWithArg`
//! trait, that allows to pass data along to the origin check. It gets passed in the key. The
//! implementor can then match on the key and the origin to decide whether the origin is
//! permissioned to set the value.

use frame_support::pallet_prelude::*;
use frame_system::pallet_prelude::*;

use frame_support::traits::{
	dynamic_params::{AggregatedKeyValue, IntoKey, Key, RuntimeParameterStore, TryIntoKey},
	EnsureOriginWithArg,
};

mod benchmarking;
#[cfg(test)]
mod tests;
mod weights;

pub use pallet::*;
pub use weights::WeightInfo;

/// The key type of a parameter.
type KeyOf<T> = <<T as Config>::RuntimeParameters as AggregatedKeyValue>::Key;

/// The value type of a parameter.
type ValueOf<T> = <<T as Config>::RuntimeParameters as AggregatedKeyValue>::Value;

#[frame_support::pallet]
pub mod pallet {
	use super::*;

	#[pallet::config(with_default)]
	pub trait Config: frame_system::Config {
		/// The overarching event type.
		#[pallet::no_default_bounds]
		#[allow(deprecated)]
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// The overarching KV type of the parameters.
		///
		/// Usually created by [`frame_support::dynamic_params`] or equivalent.
		#[pallet::no_default_bounds]
		type RuntimeParameters: AggregatedKeyValue;

		/// The origin which may update a parameter.
		///
		/// The key of the parameter is passed in as second argument to allow for fine grained
		/// control.
		#[pallet::no_default_bounds]
		type AdminOrigin: EnsureOriginWithArg<Self::RuntimeOrigin, KeyOf<Self>>;

		/// Weight information for extrinsics in this module.
		type WeightInfo: WeightInfo;
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(crate) fn deposit_event)]
	pub enum Event<T: Config> {
		/// A Parameter was set.
		///
		/// Is also emitted when the value was not changed.
		Updated {
			/// The key that was updated.
			key: <T::RuntimeParameters as AggregatedKeyValue>::Key,
			/// The old value before this call.
			old_value: Option<<T::RuntimeParameters as AggregatedKeyValue>::Value>,
			/// The new value after this call.
			new_value: Option<<T::RuntimeParameters as AggregatedKeyValue>::Value>,
		},
	}

	/// Stored parameters.
	#[pallet::storage]
	pub type Parameters<T: Config> =
		StorageMap<_, Blake2_128Concat, KeyOf<T>, ValueOf<T>, OptionQuery>;

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Set the value of a parameter.
		///
		/// The dispatch origin of this call must be `AdminOrigin` for the given `key`. Values be
		/// deleted by setting them to `None`.
		#[pallet::call_index(0)]
		#[pallet::weight(T::WeightInfo::set_parameter())]
		pub fn set_parameter(
			origin: OriginFor<T>,
			key_value: T::RuntimeParameters,
		) -> DispatchResult {
			let (key, new) = key_value.into_parts();
			T::AdminOrigin::ensure_origin(origin, &key)?;

			let mut old = None;
			Parameters::<T>::mutate(&key, |v| {
				old = v.clone();
				*v = new.clone();
			});

			Self::deposit_event(Event::Updated { key, old_value: old, new_value: new });

			Ok(())
		}
	}
	/// Default implementations of [`DefaultConfig`], which can be used to implement [`Config`].
	pub mod config_preludes {
		use super::*;
		use frame_support::derive_impl;

		/// A configuration for testing.
		pub struct TestDefaultConfig;

		#[derive_impl(frame_system::config_preludes::TestDefaultConfig, no_aggregated_types)]
		impl frame_system::DefaultConfig for TestDefaultConfig {}

		#[frame_support::register_default_impl(TestDefaultConfig)]
		impl DefaultConfig for TestDefaultConfig {
			#[inject_runtime_type]
			type RuntimeEvent = ();
			#[inject_runtime_type]
			type RuntimeParameters = ();

			type AdminOrigin = frame_support::traits::AsEnsureOriginWithArg<
				frame_system::EnsureRoot<Self::AccountId>,
			>;

			type WeightInfo = ();
		}
	}
}

impl<T: Config> RuntimeParameterStore for Pallet<T> {
	type AggregatedKeyValue = T::RuntimeParameters;

	fn get<KV, K>(key: K) -> Option<K::Value>
	where
		KV: AggregatedKeyValue,
		K: Key + Into<<KV as AggregatedKeyValue>::Key>,
		<KV as AggregatedKeyValue>::Key: IntoKey<
			<<Self as RuntimeParameterStore>::AggregatedKeyValue as AggregatedKeyValue>::Key,
		>,
		<<Self as RuntimeParameterStore>::AggregatedKeyValue as AggregatedKeyValue>::Value:
			TryIntoKey<<KV as AggregatedKeyValue>::Value>,
		<KV as AggregatedKeyValue>::Value: TryInto<K::WrappedValue>,
	{
		let key: <KV as AggregatedKeyValue>::Key = key.into();
		let val = Parameters::<T>::get(key.into_key());
		val.and_then(|v| {
			let val: <KV as AggregatedKeyValue>::Value = v.try_into_key().ok()?;
			let val: K::WrappedValue = val.try_into().ok()?;
			let val = val.into();
			Some(val)
		})
	}
}
