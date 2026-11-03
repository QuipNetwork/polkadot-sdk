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

//! Dispatch system. Contains a macro for defining runtime modules and
//! generating values representing lazy module function calls.

use crate::traits::UnfilteredDispatchable;
use codec::{Codec, Decode, DecodeWithMemTracking, Encode, EncodeLike, MaxEncodedLen};
use core::fmt;
use scale_info::TypeInfo;
#[cfg(feature = "std")]
use serde::{Deserialize, Serialize};
use sp_runtime::{
	generic::{CheckedExtrinsic, UncheckedExtrinsic},
	traits::{
		Dispatchable, ExtensionPostDispatchWeightHandler, RefundWeight, TransactionExtension,
	},
	DispatchError,
};
use sp_weights::Weight;

/// The return type of a `Dispatchable` in frame. When returned explicitly from
/// a dispatchable function it allows overriding the default `PostDispatchInfo`
/// returned from a dispatch.
pub type DispatchResultWithPostInfo = sp_runtime::DispatchResultWithInfo<PostDispatchInfo>;

#[docify::export]
/// Un-augmented version of `DispatchResultWithPostInfo` that can be returned from
/// dispatchable functions and is automatically converted to the augmented type. Should be
/// used whenever the `PostDispatchInfo` does not need to be overwritten. As this should
/// be the common case it is the implicit return type when none is specified.
pub type DispatchResult = Result<(), sp_runtime::DispatchError>;

/// The error type contained in a `DispatchResultWithPostInfo`.
pub type DispatchErrorWithPostInfo = sp_runtime::DispatchErrorWithPostInfo<PostDispatchInfo>;

/// Serializable version of pallet dispatchable.
pub trait Callable<T> {
	type RuntimeCall: UnfilteredDispatchable + Codec + Clone + PartialEq + Eq;
}

// dirty hack to work around serde_derive issue
// https://github.com/rust-lang/rust/issues/51331
pub type CallableCallFor<A, R> = <A as Callable<R>>::RuntimeCall;

/// Means to checks if the dispatchable is feeless.
///
/// This is automatically implemented for all dispatchables during pallet expansion.
/// If a call is marked by [`#[pallet::feeless_if]`](`macro@frame_support_procedural::feeless_if`)
/// attribute, the corresponding closure is checked.
pub trait CheckIfFeeless {
	/// The Origin type of the runtime.
	type Origin;

	/// Checks if the dispatchable satisfies the feeless condition as defined by
	/// [`#[pallet::feeless_if]`](`macro@frame_support_procedural::feeless_if`)
	fn is_feeless(&self, origin: &Self::Origin) -> bool;
}

/// Origin for the System pallet.
#[derive(
	PartialEq, Eq, Clone, Debug, Encode, Decode, DecodeWithMemTracking, TypeInfo, MaxEncodedLen,
)]
pub enum RawOrigin<AccountId> {
	/// The system itself ordained this dispatch to happen: this is the highest privilege level.
	Root,
	/// It is signed by some public key and we provide the `AccountId`.
	Signed(AccountId),
	/// It is signed by nobody, can be either:
	/// * included and agreed upon by the validators anyway,
	/// * or unsigned transaction validated by a pallet.
	None,
	/// It is signed by nobody, the extrinsic is authorized by the runtime.
	///
	/// Authorization logic is defined by pallets.
	/// See trait [`Authorize`](crate::traits::Authorize) and attribute macro
	/// [`authorize`](crate::pallet_macros::authorize) for more details.
	Authorized,
}

impl<AccountId> From<Option<AccountId>> for RawOrigin<AccountId> {
	fn from(s: Option<AccountId>) -> RawOrigin<AccountId> {
		match s {
			Some(who) => RawOrigin::Signed(who),
			None => RawOrigin::None,
		}
	}
}

impl<AccountId> RawOrigin<AccountId> {
	/// Returns `Some` with a reference to the `AccountId` if `self` is `Signed`, `None` otherwise.
	pub fn as_signed(&self) -> Option<&AccountId> {
		match &self {
			Self::Signed(x) => Some(x),
			_ => None,
		}
	}

	/// Returns `true` if `self` is `Root`, `None` otherwise.
	pub fn is_root(&self) -> bool {
		matches!(&self, Self::Root)
	}

	/// Returns `true` if `self` is `None`, `None` otherwise.
	pub fn is_none(&self) -> bool {
		matches!(&self, Self::None)
	}
}

/// A type that can be used as a parameter in a dispatchable function.
///
/// When using `decl_module` all arguments for call functions must implement this trait.
pub trait Parameter:
	Codec + DecodeWithMemTracking + EncodeLike + Clone + Eq + fmt::Debug + scale_info::TypeInfo
{
}
impl<T> Parameter for T where
	T: Codec + DecodeWithMemTracking + EncodeLike + Clone + Eq + fmt::Debug + scale_info::TypeInfo
{
}

/// Means of classifying a dispatchable function.
pub trait ClassifyDispatch<T> {
	/// Classify the dispatch function based on input data `target` of type `T`. When implementing
	/// this for a dispatchable, `T` will be a tuple of all arguments given to the function (except
	/// origin).
	fn classify_dispatch(&self, target: T) -> DispatchClass;
}

/// Indicates if dispatch function should pay fees or not.
///
/// If set to `Pays::No`, the block resource limits are applied, yet no fee is deducted.
pub trait PaysFee<T> {
	fn pays_fee(&self, _target: T) -> Pays;
}

/// Explicit enum to denote if a transaction pays fee or not.
#[derive(Clone, Copy, Eq, PartialEq, Debug, Encode, Decode, DecodeWithMemTracking, TypeInfo)]
pub enum Pays {
	/// Transactor will pay related fees.
	Yes,
	/// Transactor will NOT pay related fees.
	No,
}

impl Default for Pays {
	fn default() -> Self {
		Self::Yes
	}
}

impl From<Pays> for PostDispatchInfo {
	fn from(pays_fee: Pays) -> Self {
		Self { actual_weight: None, pays_fee }
	}
}

impl From<bool> for Pays {
	fn from(b: bool) -> Self {
		match b {
			true => Self::Yes,
			false => Self::No,
		}
	}
}

/// A generalized group of dispatch types.
///
/// NOTE whenever upgrading the enum make sure to also update
/// [DispatchClass::all] and [DispatchClass::non_mandatory] helper functions.
#[cfg_attr(feature = "std", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "std", serde(rename_all = "camelCase"))]
#[derive(PartialEq, Eq, Clone, Copy, Encode, Decode, DecodeWithMemTracking, Debug, TypeInfo)]
pub enum DispatchClass {
	/// A normal dispatch.
	Normal,
	/// An operational dispatch.
	Operational,
	/// A mandatory dispatch. These kinds of dispatch are always included regardless of their
	/// weight, therefore it is critical that they are separately validated to ensure that a
	/// malicious validator cannot craft a valid but impossibly heavy block. Usually this just
	/// means ensuring that the extrinsic can only be included once and that it is always very
	/// light.
	///
	/// Do *NOT* use it for extrinsics that can be heavy.
	///
	/// The only real use case for this is inherent extrinsics that are required to execute in a
	/// block for the block to be valid, and it solves the issue in the case that the block
	/// initialization is sufficiently heavy to mean that those inherents do not fit into the
	/// block. Essentially, we assume that in these exceptional circumstances, it is better to
	/// allow an overweight block to be created than to not allow any block at all to be created.
	Mandatory,
}

impl Default for DispatchClass {
	fn default() -> Self {
		Self::Normal
	}
}

impl DispatchClass {
	/// Returns an array containing all dispatch classes.
	pub fn all() -> &'static [DispatchClass] {
		&[DispatchClass::Normal, DispatchClass::Operational, DispatchClass::Mandatory]
	}

	/// Returns an array of all dispatch classes except `Mandatory`.
	pub fn non_mandatory() -> &'static [DispatchClass] {
		&[DispatchClass::Normal, DispatchClass::Operational]
	}
}

/// A trait that represents one or many values of given type.
///
/// Useful to accept as parameter type to let the caller pass either a single value directly
/// or an iterator.
pub trait OneOrMany<T> {
	/// The iterator type.
	type Iter: Iterator<Item = T>;
	/// Convert this item into an iterator.
	fn into_iter(self) -> Self::Iter;
}

impl OneOrMany<DispatchClass> for DispatchClass {
	type Iter = core::iter::Once<DispatchClass>;
	fn into_iter(self) -> Self::Iter {
		core::iter::once(self)
	}
}

impl<'a> OneOrMany<DispatchClass> for &'a [DispatchClass] {
	type Iter = core::iter::Cloned<core::slice::Iter<'a, DispatchClass>>;
	fn into_iter(self) -> Self::Iter {
		self.iter().cloned()
	}
}

/// A bundle of static information collected from the `#[pallet::weight]` attributes.
#[derive(Clone, Copy, Eq, PartialEq, Default, Debug, Encode, Decode, TypeInfo)]
pub struct DispatchInfo {
	/// Weight of this transaction's call.
	pub call_weight: Weight,
	/// Weight of this transaction's extension.
	pub extension_weight: Weight,
	/// Class of this transaction.
	pub class: DispatchClass,
	/// Does this transaction pay fees.
	pub pays_fee: Pays,
}

impl DispatchInfo {
	/// Returns the weight used by this extrinsic's extension and call when applied.
	pub fn total_weight(&self) -> Weight {
		self.call_weight.saturating_add(self.extension_weight)
	}
}

/// A `Dispatchable` function (aka transaction) that can carry some static information along with
/// it, using the `#[pallet::weight]` attribute.
pub trait GetDispatchInfo {
	/// Return a `DispatchInfo`, containing relevant information of this dispatch.
	///
	/// This is done independently of its encoded size.
	fn get_dispatch_info(&self) -> DispatchInfo;
}

impl GetDispatchInfo for () {
	fn get_dispatch_info(&self) -> DispatchInfo {
		DispatchInfo::default()
	}
}

/// Extract the actual weight from a dispatch result if any or fall back to the default weight.
pub fn extract_actual_weight(result: &DispatchResultWithPostInfo, info: &DispatchInfo) -> Weight {
	match result {
		Ok(post_info) => post_info,
		Err(err) => &err.post_info,
	}
	.calc_actual_weight(info)
}

/// Extract the actual pays_fee from a dispatch result if any or fall back to the default
/// weight.
pub fn extract_actual_pays_fee(result: &DispatchResultWithPostInfo, info: &DispatchInfo) -> Pays {
	match result {
		Ok(post_info) => post_info,
		Err(err) => &err.post_info,
	}
	.pays_fee(info)
}

/// Weight information that is only available post dispatch.
/// NOTE: This can only be used to reduce the weight or fee, not increase it.
#[derive(
	Clone, Copy, Eq, PartialEq, Default, Debug, Encode, Decode, DecodeWithMemTracking, TypeInfo,
)]
pub struct PostDispatchInfo {
	/// Actual weight consumed by a call or `None` which stands for the worst case static weight.
	pub actual_weight: Option<Weight>,
	/// Whether this transaction should pay fees when all is said and done.
	pub pays_fee: Pays,
}

impl PostDispatchInfo {
	/// Calculate how much (if any) weight was not used by the `Dispatchable`.
	pub fn calc_unspent(&self, info: &DispatchInfo) -> Weight {
		info.total_weight() - self.calc_actual_weight(info)
	}

	/// Calculate how much weight was actually spent by the `Dispatchable`.
	pub fn calc_actual_weight(&self, info: &DispatchInfo) -> Weight {
		if let Some(actual_weight) = self.actual_weight {
			let info_total_weight = info.total_weight();
			if actual_weight.any_gt(info_total_weight) {
				log::error!(
					target: crate::LOG_TARGET,
					"Post dispatch weight is greater than pre dispatch weight. \
					Pre dispatch weight may underestimating the actual weight. \
					Greater post dispatch weight components are ignored.
					Pre dispatch weight: {info_total_weight:?},
					Post dispatch weight: {actual_weight:?}",
				);
			}
			actual_weight.min(info.total_weight())
		} else {
			info.total_weight()
		}
	}

	/// Determine if user should actually pay fees at the end of the dispatch.
	pub fn pays_fee(&self, info: &DispatchInfo) -> Pays {
		// If they originally were not paying fees, or the post dispatch info
		// says they should not pay fees, then they don't pay fees.
		// This is because the pre dispatch information must contain the
		// worst case for weight and fees paid.
		if info.pays_fee == Pays::No || self.pays_fee == Pays::No {
			Pays::No
		} else {
			// Otherwise they pay.
			Pays::Yes
		}
	}
}

impl From<()> for PostDispatchInfo {
	fn from(_: ()) -> Self {
		Self { actual_weight: None, pays_fee: Default::default() }
	}
}

impl sp_runtime::traits::Printable for PostDispatchInfo {
	fn print(&self) {
		"actual_weight=".print();
		match self.actual_weight {
			Some(weight) => weight.print(),
			None => "max-weight".print(),
		};
		"pays_fee=".print();
		match self.pays_fee {
			Pays::Yes => "Yes".print(),
			Pays::No => "No".print(),
		}
	}
}

/// Allows easy conversion from `DispatchError` to `DispatchErrorWithPostInfo` for dispatchables
/// that want to return a custom a posterior weight on error.
pub trait WithPostDispatchInfo {
	/// Call this on your modules custom errors type in order to return a custom weight on error.
	///
	/// # Example
	///
	/// ```ignore
	/// let who = ensure_signed(origin).map_err(|e| e.with_weight(Weight::from_parts(100, 0)))?;
	/// ensure!(who == me, Error::<T>::NotMe.with_weight(200_000));
	/// ```
	fn with_weight(self, actual_weight: Weight) -> DispatchErrorWithPostInfo;
}

impl<T> WithPostDispatchInfo for T
where
	T: Into<DispatchError>,
{
	fn with_weight(self, actual_weight: Weight) -> DispatchErrorWithPostInfo {
		DispatchErrorWithPostInfo {
			post_info: PostDispatchInfo {
				actual_weight: Some(actual_weight),
				pays_fee: Default::default(),
			},
			error: self.into(),
		}
	}
}

/// Implementation for unchecked extrinsic.
impl<Address, Call, Signature, ExtensionV0, ExtensionOtherVersions> GetDispatchInfo
	for UncheckedExtrinsic<Address, Call, Signature, ExtensionV0, ExtensionOtherVersions>
where
	Call: GetDispatchInfo + Dispatchable + Encode,
	ExtensionV0: TransactionExtension<Call>,
	ExtensionOtherVersions: sp_runtime::traits::Pipeline<Call>,
	<Call as Dispatchable>::RuntimeOrigin: sp_runtime::traits::AsTransactionAuthorizedOrigin,
{
	fn get_dispatch_info(&self) -> DispatchInfo {
		let mut info = self.function.get_dispatch_info();
		info.extension_weight = self.extension_weight();
		info
	}
}

/// Implementation for checked extrinsic.
impl<AccountId, Call, ExtensionV0, ExtensionOtherVersions> GetDispatchInfo
	for CheckedExtrinsic<AccountId, Call, ExtensionV0, ExtensionOtherVersions>
where
	Call: GetDispatchInfo + Dispatchable + Encode,
	ExtensionV0: TransactionExtension<Call>,
	ExtensionOtherVersions: sp_runtime::traits::Pipeline<Call>,
	<Call as Dispatchable>::RuntimeOrigin: sp_runtime::traits::AsTransactionAuthorizedOrigin,
{
	fn get_dispatch_info(&self) -> DispatchInfo {
		let mut info = self.function.get_dispatch_info();
		info.extension_weight = self.extension_weight();
		info
	}
}

/// A struct holding value for each `DispatchClass`.
#[derive(Clone, Eq, PartialEq, Default, Debug, Encode, Decode, TypeInfo, MaxEncodedLen)]
pub struct PerDispatchClass<T> {
	/// Value for `Normal` extrinsics.
	normal: T,
	/// Value for `Operational` extrinsics.
	operational: T,
	/// Value for `Mandatory` extrinsics.
	mandatory: T,
}

impl<T> PerDispatchClass<T> {
	/// Create new `PerDispatchClass` with the same value for every class.
	pub fn new(val: impl Fn(DispatchClass) -> T) -> Self {
		Self {
			normal: val(DispatchClass::Normal),
			operational: val(DispatchClass::Operational),
			mandatory: val(DispatchClass::Mandatory),
		}
	}

	/// Get a mutable reference to current value of given class.
	pub fn get_mut(&mut self, class: DispatchClass) -> &mut T {
		match class {
			DispatchClass::Operational => &mut self.operational,
			DispatchClass::Normal => &mut self.normal,
			DispatchClass::Mandatory => &mut self.mandatory,
		}
	}

	/// Get current value for given class.
	pub fn get(&self, class: DispatchClass) -> &T {
		match class {
			DispatchClass::Normal => &self.normal,
			DispatchClass::Operational => &self.operational,
			DispatchClass::Mandatory => &self.mandatory,
		}
	}
}

impl<T: Clone> PerDispatchClass<T> {
	/// Set the value of given class.
	pub fn set(&mut self, new: T, class: impl OneOrMany<DispatchClass>) {
		for class in class.into_iter() {
			*self.get_mut(class) = new.clone();
		}
	}
}

impl PerDispatchClass<Weight> {
	/// Returns the total weight consumed by all extrinsics in the block.
	///
	/// Saturates on overflow.
	pub fn total(&self) -> Weight {
		let mut sum = Weight::zero();
		for class in DispatchClass::all() {
			sum.saturating_accrue(*self.get(*class));
		}
		sum
	}

	/// Add some weight to the given class. Saturates at the numeric bounds.
	pub fn add(mut self, weight: Weight, class: DispatchClass) -> Self {
		self.accrue(weight, class);
		self
	}

	/// Increase the weight of the given class. Saturates at the numeric bounds.
	pub fn accrue(&mut self, weight: Weight, class: DispatchClass) {
		self.get_mut(class).saturating_accrue(weight);
	}

	/// Try to increase the weight of the given class. Saturates at the numeric bounds.
	pub fn checked_accrue(&mut self, weight: Weight, class: DispatchClass) -> Result<(), ()> {
		self.get_mut(class).checked_accrue(weight).ok_or(())
	}

	/// Reduce the weight of the given class. Saturates at the numeric bounds.
	pub fn reduce(&mut self, weight: Weight, class: DispatchClass) {
		self.get_mut(class).saturating_reduce(weight);
	}
}

/// Means of weighing some particular kind of data (`T`).
pub trait WeighData<T> {
	/// Weigh the data `T` given by `target`. When implementing this for a dispatchable, `T` will be
	/// a tuple of all arguments given to the function (except origin).
	fn weigh_data(&self, target: T) -> Weight;
}

impl<T> WeighData<T> for Weight {
	fn weigh_data(&self, _: T) -> Weight {
		return *self;
	}
}

impl<T> PaysFee<T> for (Weight, DispatchClass, Pays) {
	fn pays_fee(&self, _: T) -> Pays {
		self.2
	}
}

impl<T> WeighData<T> for (Weight, DispatchClass) {
	fn weigh_data(&self, args: T) -> Weight {
		return self.0.weigh_data(args);
	}
}

impl<T> WeighData<T> for (Weight, DispatchClass, Pays) {
	fn weigh_data(&self, args: T) -> Weight {
		return self.0.weigh_data(args);
	}
}

impl<T> ClassifyDispatch<T> for (Weight, DispatchClass) {
	fn classify_dispatch(&self, _: T) -> DispatchClass {
		self.1
	}
}

impl<T> PaysFee<T> for (Weight, DispatchClass) {
	fn pays_fee(&self, _: T) -> Pays {
		Pays::Yes
	}
}

impl<T> WeighData<T> for (Weight, Pays) {
	fn weigh_data(&self, args: T) -> Weight {
		return self.0.weigh_data(args);
	}
}

impl<T> ClassifyDispatch<T> for (Weight, Pays) {
	fn classify_dispatch(&self, _: T) -> DispatchClass {
		DispatchClass::Normal
	}
}

impl<T> PaysFee<T> for (Weight, Pays) {
	fn pays_fee(&self, _: T) -> Pays {
		self.1
	}
}

impl From<(Option<Weight>, Pays)> for PostDispatchInfo {
	fn from(post_weight_info: (Option<Weight>, Pays)) -> Self {
		let (actual_weight, pays_fee) = post_weight_info;
		Self { actual_weight, pays_fee }
	}
}

impl From<Option<Weight>> for PostDispatchInfo {
	fn from(actual_weight: Option<Weight>) -> Self {
		Self { actual_weight, pays_fee: Default::default() }
	}
}

impl<T> ClassifyDispatch<T> for Weight {
	fn classify_dispatch(&self, _: T) -> DispatchClass {
		DispatchClass::Normal
	}
}

impl<T> PaysFee<T> for Weight {
	fn pays_fee(&self, _: T) -> Pays {
		Pays::Yes
	}
}

impl<T> ClassifyDispatch<T> for (Weight, DispatchClass, Pays) {
	fn classify_dispatch(&self, _: T) -> DispatchClass {
		self.1
	}
}

impl RefundWeight for PostDispatchInfo {
	fn refund(&mut self, weight: Weight) {
		if let Some(actual_weight) = self.actual_weight.as_mut() {
			actual_weight.saturating_reduce(weight);
		}
	}
}

impl ExtensionPostDispatchWeightHandler<DispatchInfo> for PostDispatchInfo {
	fn set_extension_weight(&mut self, info: &DispatchInfo) {
		let actual_weight = self
			.actual_weight
			.unwrap_or(info.call_weight)
			.saturating_add(info.extension_weight);
		self.actual_weight = Some(actual_weight);
	}
}

impl ExtensionPostDispatchWeightHandler<()> for PostDispatchInfo {
	fn set_extension_weight(&mut self, _: &()) {}
}

// TODO: Eventually remove these

impl<T> ClassifyDispatch<T> for u64 {
	fn classify_dispatch(&self, _: T) -> DispatchClass {
		DispatchClass::Normal
	}
}

impl<T> PaysFee<T> for u64 {
	fn pays_fee(&self, _: T) -> Pays {
		Pays::Yes
	}
}

impl<T> WeighData<T> for u64 {
	fn weigh_data(&self, _: T) -> Weight {
		return Weight::from_parts(*self, 0);
	}
}

impl<T> WeighData<T> for (u64, DispatchClass, Pays) {
	fn weigh_data(&self, args: T) -> Weight {
		return self.0.weigh_data(args);
	}
}

impl<T> ClassifyDispatch<T> for (u64, DispatchClass, Pays) {
	fn classify_dispatch(&self, _: T) -> DispatchClass {
		self.1
	}
}

impl<T> PaysFee<T> for (u64, DispatchClass, Pays) {
	fn pays_fee(&self, _: T) -> Pays {
		self.2
	}
}

impl<T> WeighData<T> for (u64, DispatchClass) {
	fn weigh_data(&self, args: T) -> Weight {
		return self.0.weigh_data(args);
	}
}

impl<T> ClassifyDispatch<T> for (u64, DispatchClass) {
	fn classify_dispatch(&self, _: T) -> DispatchClass {
		self.1
	}
}

impl<T> PaysFee<T> for (u64, DispatchClass) {
	fn pays_fee(&self, _: T) -> Pays {
		Pays::Yes
	}
}

impl<T> WeighData<T> for (u64, Pays) {
	fn weigh_data(&self, args: T) -> Weight {
		return self.0.weigh_data(args);
	}
}

impl<T> ClassifyDispatch<T> for (u64, Pays) {
	fn classify_dispatch(&self, _: T) -> DispatchClass {
		DispatchClass::Normal
	}
}

impl<T> PaysFee<T> for (u64, Pays) {
	fn pays_fee(&self, _: T) -> Pays {
		self.1
	}
}

// END TODO

#[cfg(test)]
// Do not complain about unused `dispatch` and `dispatch_aux`.
#[allow(dead_code)]
mod weight_tests {
	use super::*;
	use sp_core::parameter_types;
	use sp_runtime::{generic, traits::BlakeTwo256};
	use sp_weights::RuntimeDbWeight;

	pub use self::frame_system::{Call, Config};

	fn from_actual_ref_time(ref_time: Option<u64>) -> PostDispatchInfo {
		PostDispatchInfo {
			actual_weight: ref_time.map(|t| Weight::from_all(t)),
			pays_fee: Default::default(),
		}
	}

	fn from_post_weight_info(ref_time: Option<u64>, pays_fee: Pays) -> PostDispatchInfo {
		PostDispatchInfo { actual_weight: ref_time.map(|t| Weight::from_all(t)), pays_fee }
	}

	#[crate::pallet(dev_mode)]
	pub mod frame_system {
		use super::{frame_system, frame_system::pallet_prelude::*};
		pub use crate::dispatch::RawOrigin;
		use crate::pallet_prelude::*;

		#[pallet::pallet]
		pub struct Pallet<T>(_);

		#[pallet::config]
		#[pallet::disable_frame_system_supertrait_check]
		pub trait Config: 'static {
			type Block: Parameter + sp_runtime::traits::Block;
			type AccountId;
			type Balance;
			type BaseCallFilter: crate::traits::Contains<Self::RuntimeCall>;
			type RuntimeOrigin;
			type RuntimeCall;
			type RuntimeTask;
			type PalletInfo: crate::traits::PalletInfo;
			type DbWeight: Get<crate::weights::RuntimeDbWeight>;
		}

		#[pallet::error]
		pub enum Error<T> {
			/// Required by construct_runtime
			CallFiltered,
		}

		#[pallet::origin]
		pub type Origin<T> = RawOrigin<<T as Config>::AccountId>;

		#[pallet::call]
		impl<T: Config> Pallet<T> {
			// no arguments, fixed weight
			#[pallet::weight(1000)]
			pub fn f00(_origin: OriginFor<T>) -> DispatchResult {
				unimplemented!();
			}

			#[pallet::weight((1000, DispatchClass::Mandatory))]
			pub fn f01(_origin: OriginFor<T>) -> DispatchResult {
				unimplemented!();
			}

			#[pallet::weight((1000, Pays::No))]
			pub fn f02(_origin: OriginFor<T>) -> DispatchResult {
				unimplemented!();
			}

			#[pallet::weight((1000, DispatchClass::Operational, Pays::No))]
			pub fn f03(_origin: OriginFor<T>) -> DispatchResult {
				unimplemented!();
			}

			// weight = a x 10 + b
			#[pallet::weight(((_a * 10 + _eb * 1) as u64, DispatchClass::Normal, Pays::Yes))]
			pub fn f11(_origin: OriginFor<T>, _a: u32, _eb: u32) -> DispatchResult {
				unimplemented!();
			}

			#[pallet::weight((0, DispatchClass::Operational, Pays::Yes))]
			pub fn f12(_origin: OriginFor<T>, _a: u32, _eb: u32) -> DispatchResult {
				unimplemented!();
			}

			#[pallet::weight(T::DbWeight::get().reads(3) + T::DbWeight::get().writes(2) + Weight::from_all(10_000))]
			pub fn f20(_origin: OriginFor<T>) -> DispatchResult {
				unimplemented!();
			}

			#[pallet::weight(T::DbWeight::get().reads_writes(6, 5) + Weight::from_all(40_000))]
			pub fn f21(_origin: OriginFor<T>) -> DispatchResult {
				unimplemented!();
			}

			#[pallet::weight(1000)]
			pub fn f99(_origin: OriginFor<T>) -> DispatchResult {
				Ok(())
			}

			#[pallet::weight(1000)]
			pub fn f100(_origin: OriginFor<T>) -> DispatchResultWithPostInfo {
				Ok(crate::dispatch::PostDispatchInfo {
					actual_weight: Some(Weight::from_parts(500, 0)),
					pays_fee: Pays::Yes,
				})
			}
		}

		pub mod pallet_prelude {
			pub type OriginFor<T> = <T as super::Config>::RuntimeOrigin;

			pub type HeaderFor<T> =
				<<T as super::Config>::Block as sp_runtime::traits::HeaderProvider>::HeaderT;

			pub type BlockNumberFor<T> = <HeaderFor<T> as sp_runtime::traits::Header>::Number;
		}
	}

	type BlockNumber = u32;
	type AccountId = u32;
	type Balance = u32;
	type Header = generic::Header<BlockNumber, BlakeTwo256>;
	type UncheckedExtrinsic = generic::UncheckedExtrinsic<u32, RuntimeCall, (), ()>;
	type Block = generic::Block<Header, UncheckedExtrinsic>;

	crate::construct_runtime!(
		pub enum Runtime
		{
			System: self::frame_system,
		}
	);

	parameter_types! {
		pub const DbWeight: RuntimeDbWeight = RuntimeDbWeight {
			read: 100,
			write: 1000,
		};
	}

	impl Config for Runtime {
		type Block = Block;
		type AccountId = AccountId;
		type Balance = Balance;
		type BaseCallFilter = crate::traits::Everything;
		type RuntimeOrigin = RuntimeOrigin;
		type RuntimeCall = RuntimeCall;
		type RuntimeTask = RuntimeTask;
		type DbWeight = DbWeight;
		type PalletInfo = PalletInfo;
	}

	#[test]
	fn weights_are_correct() {
		// #[pallet::weight(1000)]
		let info = Call::<Runtime>::f00 {}.get_dispatch_info();
		assert_eq!(info.total_weight(), Weight::from_parts(1000, 0));
		assert_eq!(info.class, DispatchClass::Normal);
		assert_eq!(info.pays_fee, Pays::Yes);

		// #[pallet::weight((1000, DispatchClass::Mandatory))]
		let info = Call::<Runtime>::f01 {}.get_dispatch_info();
		assert_eq!(info.total_weight(), Weight::from_parts(1000, 0));
		assert_eq!(info.class, DispatchClass::Mandatory);
		assert_eq!(info.pays_fee, Pays::Yes);

		// #[pallet::weight((1000, Pays::No))]
		let info = Call::<Runtime>::f02 {}.get_dispatch_info();
		assert_eq!(info.total_weight(), Weight::from_parts(1000, 0));
		assert_eq!(info.class, DispatchClass::Normal);
		assert_eq!(info.pays_fee, Pays::No);

		// #[pallet::weight((1000, DispatchClass::Operational, Pays::No))]
		let info = Call::<Runtime>::f03 {}.get_dispatch_info();
		assert_eq!(info.total_weight(), Weight::from_parts(1000, 0));
		assert_eq!(info.class, DispatchClass::Operational);
		assert_eq!(info.pays_fee, Pays::No);

		// #[pallet::weight(((_a * 10 + _eb * 1) as u64, DispatchClass::Normal, Pays::Yes))]
		let info = Call::<Runtime>::f11 { a: 13, eb: 20 }.get_dispatch_info();
		assert_eq!(info.total_weight(), Weight::from_parts(150, 0)); // 13*10 + 20
		assert_eq!(info.class, DispatchClass::Normal);
		assert_eq!(info.pays_fee, Pays::Yes);

		// #[pallet::weight((0, DispatchClass::Operational, Pays::Yes))]
		let info = Call::<Runtime>::f12 { a: 10, eb: 20 }.get_dispatch_info();
		assert_eq!(info.total_weight(), Weight::zero());
		assert_eq!(info.class, DispatchClass::Operational);
		assert_eq!(info.pays_fee, Pays::Yes);

		// #[pallet::weight(T::DbWeight::get().reads(3) + T::DbWeight::get().writes(2) +
		// Weight::from_all(10_000))]
		let info = Call::<Runtime>::f20 {}.get_dispatch_info();
		assert_eq!(info.total_weight(), Weight::from_parts(12300, 10000)); // 100*3 + 1000*2 + 10_1000
		assert_eq!(info.class, DispatchClass::Normal);
		assert_eq!(info.pays_fee, Pays::Yes);

		// #[pallet::weight(T::DbWeight::get().reads_writes(6, 5) + Weight::from_all(40_000))]
		let info = Call::<Runtime>::f21 {}.get_dispatch_info();
		assert_eq!(info.total_weight(), Weight::from_parts(45600, 40000)); // 100*6 + 1000*5 + 40_1000
		assert_eq!(info.class, DispatchClass::Normal);
		assert_eq!(info.pays_fee, Pays::Yes);
	}

	#[test]
	fn extract_actual_weight_works() {
		let pre = DispatchInfo {
			call_weight: Weight::from_parts(1000, 0),
			extension_weight: Weight::zero(),
			..Default::default()
		};
		assert_eq!(
			extract_actual_weight(&Ok(from_actual_ref_time(Some(7))), &pre),
			Weight::from_parts(7, 0)
		);
		assert_eq!(
			extract_actual_weight(&Ok(from_actual_ref_time(Some(1000))), &pre),
			Weight::from_parts(1000, 0)
		);
		assert_eq!(
			extract_actual_weight(
				&Err(DispatchError::BadOrigin.with_weight(Weight::from_parts(9, 0))),
				&pre
			),
			Weight::from_parts(9, 0)
		);
	}

	#[test]
	fn extract_actual_weight_caps_at_pre_weight() {
		let pre = DispatchInfo {
			call_weight: Weight::from_parts(1000, 0),
			extension_weight: Weight::zero(),
			..Default::default()
		};
		assert_eq!(
			extract_actual_weight(&Ok(from_actual_ref_time(Some(1250))), &pre),
			Weight::from_parts(1000, 0)
		);
		assert_eq!(
			extract_actual_weight(
				&Err(DispatchError::BadOrigin.with_weight(Weight::from_parts(1300, 0))),
				&pre
			),
			Weight::from_parts(1000, 0),
		);
	}

	#[test]
	fn extract_actual_pays_fee_works() {
		let pre = DispatchInfo {
			call_weight: Weight::from_parts(1000, 0),
			extension_weight: Weight::zero(),
			..Default::default()
		};
		assert_eq!(extract_actual_pays_fee(&Ok(from_actual_ref_time(Some(7))), &pre), Pays::Yes);
		assert_eq!(
			extract_actual_pays_fee(&Ok(from_actual_ref_time(Some(1000)).into()), &pre),
			Pays::Yes
		);
		assert_eq!(
			extract_actual_pays_fee(&Ok(from_post_weight_info(Some(1000), Pays::Yes)), &pre),
			Pays::Yes
		);
		assert_eq!(
			extract_actual_pays_fee(&Ok(from_post_weight_info(Some(1000), Pays::No)), &pre),
			Pays::No
		);
		assert_eq!(
			extract_actual_pays_fee(
				&Err(DispatchError::BadOrigin.with_weight(Weight::from_parts(9, 0))),
				&pre
			),
			Pays::Yes
		);
		assert_eq!(
			extract_actual_pays_fee(
				&Err(DispatchErrorWithPostInfo {
					post_info: PostDispatchInfo { actual_weight: None, pays_fee: Pays::No },
					error: DispatchError::BadOrigin,
				}),
				&pre
			),
			Pays::No
		);

		let pre = DispatchInfo {
			call_weight: Weight::from_parts(1000, 0),
			extension_weight: Weight::zero(),
			pays_fee: Pays::No,
			..Default::default()
		};
		assert_eq!(extract_actual_pays_fee(&Ok(from_actual_ref_time(Some(7))), &pre), Pays::No);
		assert_eq!(extract_actual_pays_fee(&Ok(from_actual_ref_time(Some(1000))), &pre), Pays::No);
		assert_eq!(
			extract_actual_pays_fee(&Ok(from_post_weight_info(Some(1000), Pays::Yes)), &pre),
			Pays::No
		);
	}

	#[test]
	fn weight_accrue_works() {
		let mut post_dispatch = PostDispatchInfo {
			actual_weight: Some(Weight::from_parts(1100, 25)),
			pays_fee: Pays::Yes,
		};
		post_dispatch.refund(Weight::from_parts(100, 15));
		assert_eq!(
			post_dispatch,
			PostDispatchInfo {
				actual_weight: Some(Weight::from_parts(1000, 10)),
				pays_fee: Pays::Yes
			}
		);

		let mut post_dispatch = PostDispatchInfo { actual_weight: None, pays_fee: Pays::Yes };
		post_dispatch.refund(Weight::from_parts(100, 15));
		assert_eq!(post_dispatch, PostDispatchInfo { actual_weight: None, pays_fee: Pays::Yes });
	}
}

#[cfg(test)]
mod per_dispatch_class_tests {
	use super::*;
	use sp_runtime::traits::Zero;
	use DispatchClass::*;

	#[test]
	fn add_works() {
		let a = PerDispatchClass {
			normal: (5, 10).into(),
			operational: (20, 30).into(),
			mandatory: Weight::MAX,
		};
		assert_eq!(
			a.clone()
				.add((20, 5).into(), Normal)
				.add((10, 10).into(), Operational)
				.add((u64::MAX, 3).into(), Mandatory),
			PerDispatchClass {
				normal: (25, 15).into(),
				operational: (30, 40).into(),
				mandatory: Weight::MAX
			}
		);
		let b = a
			.add(Weight::MAX, Normal)
			.add(Weight::MAX, Operational)
			.add(Weight::MAX, Mandatory);
		assert_eq!(
			b,
			PerDispatchClass {
				normal: Weight::MAX,
				operational: Weight::MAX,
				mandatory: Weight::MAX
			}
		);
		assert_eq!(b.total(), Weight::MAX);
	}

	#[test]
	fn accrue_works() {
		let mut a = PerDispatchClass::default();

		a.accrue((10, 15).into(), Normal);
		assert_eq!(a.normal, (10, 15).into());
		assert_eq!(a.total(), (10, 15).into());

		a.accrue((20, 25).into(), Operational);
		assert_eq!(a.operational, (20, 25).into());
		assert_eq!(a.total(), (30, 40).into());

		a.accrue((30, 35).into(), Mandatory);
		assert_eq!(a.mandatory, (30, 35).into());
		assert_eq!(a.total(), (60, 75).into());

		a.accrue((u64::MAX, 10).into(), Operational);
		assert_eq!(a.operational, (u64::MAX, 35).into());
		assert_eq!(a.total(), (u64::MAX, 85).into());

		a.accrue((10, u64::MAX).into(), Normal);
		assert_eq!(a.normal, (20, u64::MAX).into());
		assert_eq!(a.total(), Weight::MAX);
	}

	#[test]
	fn reduce_works() {
		let mut a = PerDispatchClass {
			normal: (10, u64::MAX).into(),
			mandatory: (u64::MAX, 10).into(),
			operational: (20, 20).into(),
		};

		a.reduce((5, 100).into(), Normal);
		assert_eq!(a.normal, (5, u64::MAX - 100).into());
		assert_eq!(a.total(), (u64::MAX, u64::MAX - 70).into());

		a.reduce((15, 5).into(), Operational);
		assert_eq!(a.operational, (5, 15).into());
		assert_eq!(a.total(), (u64::MAX, u64::MAX - 75).into());

		a.reduce((50, 0).into(), Mandatory);
		assert_eq!(a.mandatory, (u64::MAX - 50, 10).into());
		assert_eq!(a.total(), (u64::MAX - 40, u64::MAX - 75).into());

		a.reduce((u64::MAX, 100).into(), Operational);
		assert!(a.operational.is_zero());
		assert_eq!(a.total(), (u64::MAX - 45, u64::MAX - 90).into());

		a.reduce((5, u64::MAX).into(), Normal);
		assert!(a.normal.is_zero());
		assert_eq!(a.total(), (u64::MAX - 50, 10).into());
	}

	#[test]
	fn checked_accrue_works() {
		let mut a = PerDispatchClass::default();

		a.checked_accrue((1, 2).into(), Normal).unwrap();
		a.checked_accrue((3, 4).into(), Operational).unwrap();
		a.checked_accrue((5, 6).into(), Mandatory).unwrap();
		a.checked_accrue((7, 8).into(), Operational).unwrap();
		a.checked_accrue((9, 0).into(), Normal).unwrap();

		assert_eq!(
			a,
			PerDispatchClass {
				normal: (10, 2).into(),
				operational: (10, 12).into(),
				mandatory: (5, 6).into(),
			}
		);

		a.checked_accrue((u64::MAX - 10, u64::MAX - 2).into(), Normal).unwrap();
		a.checked_accrue((0, 0).into(), Normal).unwrap();
		a.checked_accrue((1, 0).into(), Normal).unwrap_err();
		a.checked_accrue((0, 1).into(), Normal).unwrap_err();

		assert_eq!(
			a,
			PerDispatchClass {
				normal: Weight::MAX,
				operational: (10, 12).into(),
				mandatory: (5, 6).into(),
			}
		);
	}

	#[test]
	fn checked_accrue_does_not_modify_on_error() {
		let mut a = PerDispatchClass {
			normal: 0.into(),
			operational: Weight::MAX / 2 + 2.into(),
			mandatory: 10.into(),
		};

		a.checked_accrue(Weight::MAX / 2, Operational).unwrap_err();
		a.checked_accrue(Weight::MAX - 9.into(), Mandatory).unwrap_err();
		a.checked_accrue(Weight::MAX, Normal).unwrap(); // This one works

		assert_eq!(
			a,
			PerDispatchClass {
				normal: Weight::MAX,
				operational: Weight::MAX / 2 + 2.into(),
				mandatory: 10.into(),
			}
		);
	}

	#[test]
	fn total_works() {
		assert!(PerDispatchClass::default().total().is_zero());

		assert_eq!(
			PerDispatchClass {
				normal: 0.into(),
				operational: (10, 20).into(),
				mandatory: (20, u64::MAX).into(),
			}
			.total(),
			(30, u64::MAX).into()
		);

		assert_eq!(
			PerDispatchClass {
				normal: (u64::MAX - 10, 10).into(),
				operational: (3, u64::MAX).into(),
				mandatory: (4, u64::MAX).into(),
			}
			.total(),
			(u64::MAX - 3, u64::MAX).into()
		);
	}
}

#[cfg(test)]
mod test_extensions {
	use codec::{Decode, DecodeWithMemTracking, Encode};
	use scale_info::TypeInfo;
	use sp_runtime::{
		impl_tx_ext_default,
		traits::{
			DispatchInfoOf, DispatchOriginOf, Dispatchable, PostDispatchInfoOf,
			TransactionExtension,
		},
		transaction_validity::TransactionValidityError,
	};
	use sp_weights::Weight;

	use super::{DispatchResult, PostDispatchInfo};

	/// Test extension that refunds half its cost if the preset inner flag is set.
	#[derive(Clone, Eq, PartialEq, Debug, Encode, Decode, DecodeWithMemTracking, TypeInfo)]
	pub struct HalfCostIf(pub bool);

	impl<RuntimeCall: Dispatchable> TransactionExtension<RuntimeCall> for HalfCostIf {
		const IDENTIFIER: &'static str = "HalfCostIf";
		type Implicit = ();
		type Val = ();
		type Pre = bool;

		fn weight(&self, _: &RuntimeCall) -> sp_weights::Weight {
			Weight::from_parts(100, 0)
		}

		fn prepare(
			self,
			_val: Self::Val,
			_origin: &DispatchOriginOf<RuntimeCall>,
			_call: &RuntimeCall,
			_info: &DispatchInfoOf<RuntimeCall>,
			_len: usize,
		) -> Result<Self::Pre, TransactionValidityError> {
			Ok(self.0)
		}

		fn post_dispatch_details(
			pre: Self::Pre,
			_info: &DispatchInfoOf<RuntimeCall>,
			_post_info: &PostDispatchInfoOf<RuntimeCall>,
			_len: usize,
			_result: &DispatchResult,
		) -> Result<Weight, TransactionValidityError> {
			if pre {
				Ok(Weight::from_parts(50, 0))
			} else {
				Ok(Weight::zero())
			}
		}
		impl_tx_ext_default!(RuntimeCall; validate);
	}

	/// Test extension that refunds its cost if the actual post dispatch weight up until this point
	/// in the extension pipeline is less than the preset inner `ref_time` amount.
	#[derive(Clone, Eq, PartialEq, Debug, Encode, Decode, DecodeWithMemTracking, TypeInfo)]
	pub struct FreeIfUnder(pub u64);

	impl<RuntimeCall: Dispatchable> TransactionExtension<RuntimeCall> for FreeIfUnder
	where
		RuntimeCall: Dispatchable<PostInfo = PostDispatchInfo>,
	{
		const IDENTIFIER: &'static str = "FreeIfUnder";
		type Implicit = ();
		type Val = ();
		type Pre = u64;

		fn weight(&self, _: &RuntimeCall) -> sp_weights::Weight {
			Weight::from_parts(200, 0)
		}

		fn prepare(
			self,
			_val: Self::Val,
			_origin: &DispatchOriginOf<RuntimeCall>,
			_call: &RuntimeCall,
			_info: &DispatchInfoOf<RuntimeCall>,
			_len: usize,
		) -> Result<Self::Pre, TransactionValidityError> {
			Ok(self.0)
		}

		fn post_dispatch_details(
			pre: Self::Pre,
			_info: &DispatchInfoOf<RuntimeCall>,
			post_info: &PostDispatchInfoOf<RuntimeCall>,
			_len: usize,
			_result: &DispatchResult,
		) -> Result<Weight, TransactionValidityError> {
			if let Some(actual) = post_info.actual_weight {
				if pre > actual.ref_time() {
					return Ok(Weight::from_parts(200, 0));
				}
			}
			Ok(Weight::zero())
		}
		impl_tx_ext_default!(RuntimeCall; validate);
	}

	/// Test extension that sets its actual post dispatch `ref_time` weight to the preset inner
	/// amount.
	#[derive(Clone, Eq, PartialEq, Debug, Encode, Decode, DecodeWithMemTracking, TypeInfo)]
	pub struct ActualWeightIs(pub u64);

	impl<RuntimeCall: Dispatchable> TransactionExtension<RuntimeCall> for ActualWeightIs {
		const IDENTIFIER: &'static str = "ActualWeightIs";
		type Implicit = ();
		type Val = ();
		type Pre = u64;

		fn weight(&self, _: &RuntimeCall) -> sp_weights::Weight {
			Weight::from_parts(300, 0)
		}

		fn prepare(
			self,
			_val: Self::Val,
			_origin: &DispatchOriginOf<RuntimeCall>,
			_call: &RuntimeCall,
			_info: &DispatchInfoOf<RuntimeCall>,
			_len: usize,
		) -> Result<Self::Pre, TransactionValidityError> {
			Ok(self.0)
		}

		fn post_dispatch_details(
			pre: Self::Pre,
			_info: &DispatchInfoOf<RuntimeCall>,
			_post_info: &PostDispatchInfoOf<RuntimeCall>,
			_len: usize,
			_result: &DispatchResult,
		) -> Result<Weight, TransactionValidityError> {
			Ok(Weight::from_parts(300u64.saturating_sub(pre), 0))
		}
		impl_tx_ext_default!(RuntimeCall; validate);
	}
}

#[cfg(test)]
// Do not complain about unused `dispatch` and `dispatch_aux`.
#[allow(dead_code)]
mod extension_weight_tests {
	use crate::assert_ok;

	use super::*;
	use sp_core::parameter_types;
	use sp_runtime::{
		generic::{self, ExtrinsicFormat},
		traits::{Applyable, BlakeTwo256, DispatchTransaction, TransactionExtension},
	};
	use sp_weights::RuntimeDbWeight;
	use test_extensions::{ActualWeightIs, FreeIfUnder, HalfCostIf};

	use super::weight_tests::frame_system;
	use frame_support::construct_runtime;

	pub type TxExtension = (HalfCostIf, FreeIfUnder, ActualWeightIs);
	pub type UncheckedExtrinsic = generic::UncheckedExtrinsic<u64, RuntimeCall, (), TxExtension>;
	pub type Header = generic::Header<BlockNumber, BlakeTwo256>;
	pub type Block = generic::Block<Header, UncheckedExtrinsic>;
	pub type AccountId = u64;
	pub type Balance = u32;
	pub type BlockNumber = u32;

	construct_runtime!(
		pub enum ExtRuntime {
			System: frame_system,
		}
	);

	impl frame_system::Config for ExtRuntime {
		type Block = Block;
		type AccountId = AccountId;
		type Balance = Balance;
		type BaseCallFilter = crate::traits::Everything;
		type RuntimeOrigin = RuntimeOrigin;
		type RuntimeCall = RuntimeCall;
		type RuntimeTask = RuntimeTask;
		type DbWeight = DbWeight;
		type PalletInfo = PalletInfo;
	}

	parameter_types! {
		pub const DbWeight: RuntimeDbWeight = RuntimeDbWeight {
			read: 100,
			write: 1000,
		};
	}

	pub struct ExtBuilder {}

	impl Default for ExtBuilder {
		fn default() -> Self {
			Self {}
		}
	}

	impl ExtBuilder {
		pub fn build(self) -> sp_io::TestExternalities {
			let mut ext = sp_io::TestExternalities::new(Default::default());
			ext.execute_with(|| {});
			ext
		}

		pub fn build_and_execute(self, test: impl FnOnce() -> ()) {
			self.build().execute_with(|| {
				test();
			})
		}
	}

	#[test]
	fn no_post_dispatch_with_no_refund() {
		ExtBuilder::default().build_and_execute(|| {
			let call = RuntimeCall::System(frame_system::Call::<ExtRuntime>::f99 {});
			let ext: TxExtension = (HalfCostIf(false), FreeIfUnder(1500), ActualWeightIs(0));
			let uxt = UncheckedExtrinsic::new_signed(call.clone(), 0, (), ext.clone());
			assert_eq!(uxt.extension_weight(), Weight::from_parts(600, 0));

			let mut info = call.get_dispatch_info();
			assert_eq!(info.total_weight(), Weight::from_parts(1000, 0));
			info.extension_weight = ext.weight(&call);
			let (pre, _) = ext.validate_and_prepare(Some(0).into(), &call, &info, 0, 0).unwrap();
			let res = call.dispatch(Some(0).into());
			let mut post_info = res.unwrap();
			assert!(post_info.actual_weight.is_none());
			assert_ok!(<TxExtension as TransactionExtension<RuntimeCall>>::post_dispatch(
				pre,
				&info,
				&mut post_info,
				0,
				&Ok(()),
			));
			assert!(post_info.actual_weight.is_none());
		});
	}

	#[test]
	fn no_post_dispatch_refunds_when_dispatched() {
		ExtBuilder::default().build_and_execute(|| {
			let call = RuntimeCall::System(frame_system::Call::<ExtRuntime>::f99 {});
			let ext: TxExtension = (HalfCostIf(true), FreeIfUnder(100), ActualWeightIs(0));
			let uxt = UncheckedExtrinsic::new_signed(call.clone(), 0, (), ext.clone());
			assert_eq!(uxt.extension_weight(), Weight::from_parts(600, 0));

			let mut info = call.get_dispatch_info();
			assert_eq!(info.total_weight(), Weight::from_parts(1000, 0));
			info.extension_weight = ext.weight(&call);
			let post_info =
				ext.dispatch_transaction(Some(0).into(), call, &info, 0, 0).unwrap().unwrap();
			// 1000 call weight + 50 + 200 + 0
			assert_eq!(post_info.actual_weight, Some(Weight::from_parts(1250, 0)));
		});
	}

	#[test]
	fn post_dispatch_with_refunds() {
		ExtBuilder::default().build_and_execute(|| {
			let call = RuntimeCall::System(frame_system::Call::<ExtRuntime>::f100 {});
			// First testcase
			let ext: TxExtension = (HalfCostIf(false), FreeIfUnder(2000), ActualWeightIs(0));
			let uxt = UncheckedExtrinsic::new_signed(call.clone(), 0, (), ext.clone());
			assert_eq!(uxt.extension_weight(), Weight::from_parts(600, 0));

			let mut info = call.get_dispatch_info();
			assert_eq!(info.call_weight, Weight::from_parts(1000, 0));
			info.extension_weight = ext.weight(&call);
			assert_eq!(info.total_weight(), Weight::from_parts(1600, 0));
			let (pre, _) = ext.validate_and_prepare(Some(0).into(), &call, &info, 0, 0).unwrap();
			let res = call.clone().dispatch(Some(0).into());
			let mut post_info = res.unwrap();
			// 500 actual call weight
			assert_eq!(post_info.actual_weight, Some(Weight::from_parts(500, 0)));
			// add the 600 worst case extension weight
			post_info.set_extension_weight(&info);
			// extension weight should be refunded
			assert_ok!(<TxExtension as TransactionExtension<RuntimeCall>>::post_dispatch(
				pre,
				&info,
				&mut post_info,
				0,
				&Ok(()),
			));
			// 500 actual call weight + 100 + 0 + 0
			assert_eq!(post_info.actual_weight, Some(Weight::from_parts(600, 0)));

			// Second testcase
			let ext: TxExtension = (HalfCostIf(false), FreeIfUnder(1100), ActualWeightIs(200));
			let (pre, _) = ext.validate_and_prepare(Some(0).into(), &call, &info, 0, 0).unwrap();
			let res = call.clone().dispatch(Some(0).into());
			let mut post_info = res.unwrap();
			// 500 actual call weight
			assert_eq!(post_info.actual_weight, Some(Weight::from_parts(500, 0)));
			// add the 600 worst case extension weight
			post_info.set_extension_weight(&info);
			// extension weight should be refunded
			assert_ok!(<TxExtension as TransactionExtension<RuntimeCall>>::post_dispatch(
				pre,
				&info,
				&mut post_info,
				0,
				&Ok(()),
			));
			// 500 actual call weight + 100 + 200 + 200
			assert_eq!(post_info.actual_weight, Some(Weight::from_parts(1000, 0)));

			// Third testcase
			let ext: TxExtension = (HalfCostIf(true), FreeIfUnder(1060), ActualWeightIs(200));
			let (pre, _) = ext.validate_and_prepare(Some(0).into(), &call, &info, 0, 0).unwrap();
			let res = call.clone().dispatch(Some(0).into());
			let mut post_info = res.unwrap();
			// 500 actual call weight
			assert_eq!(post_info.actual_weight, Some(Weight::from_parts(500, 0)));
			// add the 600 worst case extension weight
			post_info.set_extension_weight(&info);
			// extension weight should be refunded
			assert_ok!(<TxExtension as TransactionExtension<RuntimeCall>>::post_dispatch(
				pre,
				&info,
				&mut post_info,
				0,
				&Ok(()),
			));
			// 500 actual call weight + 50 + 0 + 200
			assert_eq!(post_info.actual_weight, Some(Weight::from_parts(750, 0)));

			// Fourth testcase
			let ext: TxExtension = (HalfCostIf(false), FreeIfUnder(100), ActualWeightIs(300));
			let (pre, _) = ext.validate_and_prepare(Some(0).into(), &call, &info, 0, 0).unwrap();
			let res = call.clone().dispatch(Some(0).into());
			let mut post_info = res.unwrap();
			// 500 actual call weight
			assert_eq!(post_info.actual_weight, Some(Weight::from_parts(500, 0)));
			// add the 600 worst case extension weight
			post_info.set_extension_weight(&info);
			// extension weight should be refunded
			assert_ok!(<TxExtension as TransactionExtension<RuntimeCall>>::post_dispatch(
				pre,
				&info,
				&mut post_info,
				0,
				&Ok(()),
			));
			// 500 actual call weight + 100 + 200 + 300
			assert_eq!(post_info.actual_weight, Some(Weight::from_parts(1100, 0)));
		});
	}

	#[test]
	fn checked_extrinsic_apply() {
		ExtBuilder::default().build_and_execute(|| {
			let call = RuntimeCall::System(frame_system::Call::<ExtRuntime>::f100 {});
			// First testcase
			let ext: TxExtension = (HalfCostIf(false), FreeIfUnder(2000), ActualWeightIs(0));
			let xt = CheckedExtrinsic {
				format: ExtrinsicFormat::<_, _>::Signed(0, ext.clone()),
				function: call.clone(),
			};
			assert_eq!(xt.extension_weight(), Weight::from_parts(600, 0));
			let mut info = call.get_dispatch_info();
			assert_eq!(info.call_weight, Weight::from_parts(1000, 0));
			info.extension_weight = ext.weight(&call);
			assert_eq!(info.total_weight(), Weight::from_parts(1600, 0));
			let post_info = xt.apply::<ExtRuntime>(&info, 0).unwrap().unwrap();
			// 500 actual call weight + 100 + 0 + 0
			assert_eq!(post_info.actual_weight, Some(Weight::from_parts(600, 0)));

			// Second testcase
			let ext: TxExtension = (HalfCostIf(false), FreeIfUnder(1100), ActualWeightIs(200));
			let xt = CheckedExtrinsic {
				format: ExtrinsicFormat::<_, _>::Signed(0, ext),
				function: call.clone(),
			};
			let post_info = xt.apply::<ExtRuntime>(&info, 0).unwrap().unwrap();
			// 500 actual call weight + 100 + 200 + 200
			assert_eq!(post_info.actual_weight, Some(Weight::from_parts(1000, 0)));

			// Third testcase
			let ext: TxExtension = (HalfCostIf(true), FreeIfUnder(1060), ActualWeightIs(200));
			let xt = CheckedExtrinsic {
				format: ExtrinsicFormat::<_, _>::Signed(0, ext),
				function: call.clone(),
			};
			let post_info = xt.apply::<ExtRuntime>(&info, 0).unwrap().unwrap();
			// 500 actual call weight + 50 + 0 + 200
			assert_eq!(post_info.actual_weight, Some(Weight::from_parts(750, 0)));

			// Fourth testcase
			let ext: TxExtension = (HalfCostIf(false), FreeIfUnder(100), ActualWeightIs(300));
			let xt = CheckedExtrinsic {
				format: ExtrinsicFormat::<_, _>::Signed(0, ext),
				function: call.clone(),
			};
			let post_info = xt.apply::<ExtRuntime>(&info, 0).unwrap().unwrap();
			// 500 actual call weight + 100 + 200 + 300
			assert_eq!(post_info.actual_weight, Some(Weight::from_parts(1100, 0)));
		});
	}
}
