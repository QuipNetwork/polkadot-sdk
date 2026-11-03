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

use alloc::{
	boxed::Box,
	collections::{
		btree_map::{self, BTreeMap},
		btree_set::BTreeSet,
	},
	vec::Vec,
};
use core::{fmt::Formatter, mem};
use frame_support::traits::tokens::imbalance::ImbalanceAccounting;
use xcm::latest::{
	Asset, AssetFilter, AssetId, AssetInstance, Assets,
	Fungibility::{Fungible, NonFungible},
	InteriorLocation, Location, Reanchorable,
	WildAsset::{All, AllCounted, AllOf, AllOfCounted},
	WildFungibility::{Fungible as WildFungible, NonFungible as WildNonFungible},
};

/// An error emitted by `take` operations.
#[derive(Debug)]
pub enum TakeError {
	/// There was an attempt to take an asset without saturating (enough of) which did not exist.
	AssetUnderflow(Asset),
}

/// Helper struct for creating a backup of assets in holding in a safe way.
///
/// Duplicating holding involves unsafe cloning of any imbalances, but this type makes sure that
/// either the backup or the original are dropped without resolving any duplicated imbalances.
pub struct BackupAssetsInHolding {
	// private inner holding safely managed by the wrapper
	inner: AssetsInHolding,
}

impl BackupAssetsInHolding {
	/// Clones `other` and keeps it in this safe wrapper that will safely drop duplicated
	/// imbalances.
	pub fn safe_backup(other: &AssetsInHolding) -> Self {
		Self {
			inner: AssetsInHolding {
				fungible: other
					.fungible
					.iter()
					.map(|(id, accounting)| (id.clone(), accounting.unsafe_clone()))
					.collect(),
				non_fungible: other.non_fungible.clone(),
			},
		}
	}

	/// Replace `target` with the backup held within `self`. It is basically a mem swap so that the
	/// original holdings of `target` will be dropped without resolving inner imbalances.
	pub fn restore_into(&mut self, target: &mut AssetsInHolding) {
		core::mem::swap(target, &mut self.inner);
	}

	/// This object holds an unsafe clone of `inner` and needs to drop it without resolving its held
	/// imbalances.
	pub fn safe_drop(&mut self) {
		// set amount to 0 so that no accounting is done on imbalance Drop
		self.inner.fungible.iter_mut().for_each(|(_, accounting)| {
			accounting.forget_imbalance();
		});
	}
}

impl Drop for BackupAssetsInHolding {
	fn drop(&mut self) {
		self.safe_drop();
	}
}

/// Map of non-wildcard fungible and non-fungible assets held in the holding register.
pub struct AssetsInHolding {
	/// The fungible assets.
	pub fungible: BTreeMap<AssetId, Box<dyn ImbalanceAccounting<u128>>>,
	/// The non-fungible assets.
	// TODO: Consider BTreeMap<AssetId, BTreeSet<AssetInstance>>
	//   or even BTreeMap<AssetId, SortedVec<AssetInstance>>
	pub non_fungible: BTreeSet<(AssetId, AssetInstance)>,
}

impl PartialEq for AssetsInHolding {
	fn eq(&self, other: &Self) -> bool {
		if self.non_fungible != other.non_fungible {
			return false;
		}
		if self.fungible.len() != other.fungible.len() {
			return false;
		}
		if !self
			.fungible
			.iter()
			.zip(other.fungible.iter())
			.all(|(left, right)| left.0 == right.0 && left.1.amount() == right.1.amount())
		{
			return false;
		}
		true
	}
}

impl core::fmt::Debug for AssetsInHolding {
	fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
		let fungibles: BTreeMap<&AssetId, u128> =
			self.fungible.iter().map(|(id, accounting)| (id, accounting.amount())).collect();
		f.debug_struct("AssetsInHolding")
			.field("fungible", &fungibles)
			.field("non_fungible", &self.non_fungible)
			.finish()
	}
}

impl AssetsInHolding {
	/// New value, containing no assets.
	pub fn new() -> Self {
		AssetsInHolding { fungible: BTreeMap::new(), non_fungible: BTreeSet::new() }
	}

	/// New holding containing a single fungible imbalance.
	pub fn new_from_fungible_credit(
		asset: AssetId,
		credit: Box<dyn ImbalanceAccounting<u128>>,
	) -> Self {
		let mut new = AssetsInHolding { fungible: BTreeMap::new(), non_fungible: BTreeSet::new() };
		new.fungible.insert(asset, credit);
		new
	}

	/// New holding containing a single non fungible.
	pub fn new_from_non_fungible(class: AssetId, instance: AssetInstance) -> Self {
		let mut new = AssetsInHolding { fungible: BTreeMap::new(), non_fungible: BTreeSet::new() };
		new.non_fungible.insert((class, instance));
		new
	}

	/// Total number of distinct assets.
	pub fn len(&self) -> usize {
		self.fungible.len() + self.non_fungible.len()
	}

	/// Returns `true` if `self` contains no assets.
	pub fn is_empty(&self) -> bool {
		self.fungible.is_empty() && self.non_fungible.is_empty()
	}

	/// A borrowing iterator over the fungible assets.
	pub fn fungible_assets_iter(&self) -> impl Iterator<Item = Asset> + '_ {
		self.fungible
			.iter()
			.map(|(id, accounting)| Asset { fun: Fungible(accounting.amount()), id: id.clone() })
	}

	/// A borrowing iterator over the non-fungible assets.
	pub fn non_fungible_assets_iter(&self) -> impl Iterator<Item = Asset> + '_ {
		self.non_fungible
			.iter()
			.map(|(id, instance)| Asset { fun: NonFungible(*instance), id: id.clone() })
	}

	/// A consuming iterator over all assets.
	pub fn into_assets_iter(self) -> impl Iterator<Item = Asset> {
		self.fungible
			.into_iter()
			.map(|(id, accounting)| Asset { fun: Fungible(accounting.amount()), id })
			.chain(
				self.non_fungible
					.into_iter()
					.map(|(id, instance)| Asset { fun: NonFungible(instance), id }),
			)
	}

	/// A consuming iterator that yields one [`AssetsInHolding`] per individual asset —
	/// each fungible imbalance and each non-fungible instance — by transferring ownership
	/// out of the underlying maps. No `AssetId` clones and no `BTreeMap` lookups, unlike
	/// the borrowing-iterator + `try_take` pattern.
	///
	/// Iteration order matches [`Self::assets_iter`] and [`Self::into_assets_iter`]:
	/// fungibles in sorted-by-`AssetId` order, then non-fungibles in sorted order.
	pub fn into_per_asset_holdings(self) -> impl Iterator<Item = AssetsInHolding> {
		let fungibles = self.fungible.into_iter().map(|(asset_id, accounting)| {
			AssetsInHolding::new_from_fungible_credit(asset_id, accounting)
		});
		let non_fungibles = self
			.non_fungible
			.into_iter()
			.map(|(class, instance)| AssetsInHolding::new_from_non_fungible(class, instance));
		fungibles.chain(non_fungibles)
	}

	/// A borrowing iterator over all assets.
	pub fn assets_iter(&self) -> impl Iterator<Item = Asset> + '_ {
		self.fungible_assets_iter().chain(self.non_fungible_assets_iter())
	}

	/// Mutate `self` to contain all given `assets`, saturating if necessary.
	///
	/// NOTE: [`AssetsInHolding`] are always sorted
	pub fn subsume_assets(&mut self, assets: AssetsInHolding) {
		// for fungibles, find matching fungibles and sum their amounts so we end-up having just
		// single such fungible but with increased amount inside
		for (asset_id, accounting) in assets.fungible.into_iter() {
			match self.fungible.entry(asset_id) {
				btree_map::Entry::Occupied(mut e) => {
					e.get_mut().saturating_subsume(accounting);
				},
				btree_map::Entry::Vacant(e) => {
					e.insert(accounting);
				},
			}
		}
		// for non-fungibles, every entry is unique so there is no notion of amount to sum-up
		// together if there is the same non-fungible in both holdings (same instance_id) these
		// will be collapsed into just single one
		let mut non_fungible = assets.non_fungible;
		self.non_fungible.append(&mut non_fungible);
	}

	/// Swaps two mutable AssetsInHolding, without deinitializing either one.
	pub fn swapped(&mut self, mut with: AssetsInHolding) -> Self {
		mem::swap(&mut *self, &mut with);
		with
	}

	/// Consume `self` and return `Assets` as assets interpreted from the perspective of a `target`
	/// chain. The local chain's `context` is provided.
	///
	/// Any assets which were unable to be reanchored are introduced into `failed_bin` instead.
	///
	/// WARNING: this will drop/resolve any inner imbalances for the reanchored assets. Meant to be
	/// used in crosschain operations where the asset is consumed (imbalance dropped/resolved)
	/// locally, and a reanchored version of it is to be minted on a remote location.
	pub fn reanchor_and_burn_local(
		self,
		target: &Location,
		context: &InteriorLocation,
		failed_bin: &mut Self,
	) -> Assets {
		let mut assets: Vec<Asset> = self
			.fungible
			.into_iter()
			.filter_map(|(mut id, accounting)| match id.reanchor(target, context) {
				Ok(()) => Some(Asset::from((id, Fungible(accounting.amount())))),
				Err(()) => {
					failed_bin.fungible.insert(id, accounting);
					None
				},
			})
			.chain(self.non_fungible.into_iter().filter_map(|(mut class, inst)| {
				match class.reanchor(target, context) {
					Ok(()) => Some(Asset::from((class, inst))),
					Err(()) => {
						failed_bin.non_fungible.insert((class, inst));
						None
					},
				}
			}))
			.collect();
		assets.sort();
		assets.into()
	}

	/// Return all inner assets, but interpreted from the perspective of a `target` chain. The local
	/// chain's `context` is provided.
	///
	/// **Warning**: This method returns `Assets` which only contains amounts (not imbalances).
	/// The returned `Assets` is suitable for cross-chain messaging but does not preserve the
	/// imbalance accounting semantics of the original `AssetsInHolding`. Do not use the returned
	/// value for local balance operations that require imbalance tracking.
	pub fn reanchored_assets(&self, target: &Location, context: &InteriorLocation) -> Assets {
		let mut assets: Vec<Asset> = self
			.fungible
			.iter()
			.filter_map(|(id, accounting)| match id.clone().reanchored(target, context) {
				Ok(new_id) => Some(Asset::from((new_id, Fungible(accounting.amount())))),
				Err(()) => None,
			})
			.chain(self.non_fungible.iter().filter_map(|(class, inst)| {
				match class.clone().reanchored(target, context) {
					Ok(new_class) => Some(Asset::from((new_class, *inst))),
					Err(()) => None,
				}
			}))
			.collect();
		assets.sort();
		assets.into()
	}

	/// Returns `true` if `asset` is contained within `self`.
	pub fn contains_asset(&self, asset: &Asset) -> bool {
		match asset {
			Asset { fun: Fungible(amount), id } => {
				self.fungible.get(id).map_or(false, |a| a.amount() >= *amount)
			},
			Asset { fun: NonFungible(instance), id } => {
				self.non_fungible.contains(&(id.clone(), *instance))
			},
		}
	}

	/// Returns `true` if all `assets` are contained within `self`.
	pub fn contains_assets(&self, assets: &Assets) -> bool {
		assets.inner().iter().all(|a| self.contains_asset(a))
	}

	/// Returns an error unless all `assets` are contained in `self`.
	pub fn ensure_contains(&self, assets: &Assets) -> Result<(), TakeError> {
		for asset in assets.inner().iter() {
			match asset {
				Asset { fun: Fungible(amount), id } => {
					if self.fungible.get(id).map_or(true, |a| a.amount() < *amount) {
						return Err(TakeError::AssetUnderflow((id.clone(), *amount).into()));
					}
				},
				Asset { fun: NonFungible(instance), id } => {
					let id_instance = (id.clone(), *instance);
					if !self.non_fungible.contains(&id_instance) {
						return Err(TakeError::AssetUnderflow(id_instance.into()));
					}
				},
			}
		}
		return Ok(());
	}

	/// Mutates `self` to its original value less `mask` and returns assets that were removed.
	///
	/// If `saturate` is `true`, then `self` is considered to be masked by `mask`, thereby avoiding
	/// any attempt at reducing it by assets it does not contain. In this case, the function is
	/// infallible. If `saturate` is `false` and `mask` references a definite asset which `self`
	/// does not contain then an error is returned.
	///
	/// The number of unique assets which are removed will respect the `count` parameter in the
	/// counted wildcard variants.
	///
	/// Returns `Ok` with the definite assets token from `self` and mutates `self` to its value
	/// minus `mask`. Returns `Err` in the non-saturating case where `self` did not contain (enough
	/// of) a definite asset to be removed.
	fn general_take(
		&mut self,
		mask: AssetFilter,
		saturate: bool,
	) -> Result<AssetsInHolding, TakeError> {
		let mut taken = AssetsInHolding::new();
		let maybe_limit = mask.limit().map(|x| x as usize);
		match mask {
			AssetFilter::Wild(All) | AssetFilter::Wild(AllCounted(_)) => match maybe_limit {
				None => return Ok(self.swapped(AssetsInHolding::new())),
				Some(limit) if self.len() <= limit => {
					return Ok(self.swapped(AssetsInHolding::new()))
				},
				Some(0) => return Ok(AssetsInHolding::new()),
				Some(limit) => {
					let fungible = mem::replace(&mut self.fungible, Default::default());
					fungible.into_iter().for_each(|(c, amount)| {
						if taken.len() < limit {
							taken.fungible.insert(c, amount);
						} else {
							self.fungible.insert(c, amount);
						}
					});
					let non_fungible = mem::replace(&mut self.non_fungible, Default::default());
					non_fungible.into_iter().for_each(|(c, instance)| {
						if taken.len() < limit {
							taken.non_fungible.insert((c, instance));
						} else {
							self.non_fungible.insert((c, instance));
						}
					});
				},
			},
			AssetFilter::Wild(AllOfCounted { fun: WildFungible, id, .. }) |
			AssetFilter::Wild(AllOf { fun: WildFungible, id }) => {
				if maybe_limit.map_or(true, |l| l >= 1) {
					if let Some((id, amount)) = self.fungible.remove_entry(&id) {
						taken.fungible.insert(id, amount);
					}
				}
			},
			AssetFilter::Wild(AllOfCounted { fun: WildNonFungible, id, .. }) |
			AssetFilter::Wild(AllOf { fun: WildNonFungible, id }) => {
				let non_fungible = mem::replace(&mut self.non_fungible, Default::default());
				non_fungible.into_iter().for_each(|(c, instance)| {
					if c == id && maybe_limit.map_or(true, |l| taken.len() < l) {
						taken.non_fungible.insert((c, instance));
					} else {
						self.non_fungible.insert((c, instance));
					}
				});
			},
			AssetFilter::Definite(assets) => {
				if !saturate {
					self.ensure_contains(&assets)?;
				}
				for asset in assets.into_inner().into_iter() {
					match asset {
						Asset { fun: Fungible(amount), id } => {
							let (remove, balance) = match self.fungible.get_mut(&id) {
								Some(self_amount) => {
									// Ok to use `saturating_take()` because we checked with
									// `self.ensure_contains()` above against `saturate` flag
									let balance = self_amount.saturating_take(amount);
									(self_amount.amount() == 0, Some(balance))
								},
								None => (false, None),
							};
							if remove {
								self.fungible.remove(&id);
							}
							if let Some(balance) = balance {
								let other = Self::new_from_fungible_credit(id, balance);
								taken.subsume_assets(other);
							}
						},
						Asset { fun: NonFungible(instance), id } => {
							let id_instance = (id, instance);
							if self.non_fungible.remove(&id_instance) {
								taken.non_fungible.insert((id_instance.0, id_instance.1));
							}
						},
					}
				}
			},
		}
		Ok(taken)
	}

	/// Mutates `self` to its original value less `mask` and returns `true` iff it contains at least
	/// `mask`.
	///
	/// Returns `Ok` with the non-wildcard equivalence of `mask` taken and mutates `self` to its
	/// value minus `mask` if `self` contains `asset`, and return `Err` otherwise.
	pub fn saturating_take(&mut self, asset: AssetFilter) -> Self {
		self.general_take(asset, true)
			.expect("general_take never results in error when saturating")
	}

	/// Mutates `self` to its original value less `mask` and returns `true` iff it contains at least
	/// `mask`.
	///
	/// Returns `Ok` with the non-wildcard equivalence of `asset` taken and mutates `self` to its
	/// value minus `asset` if `self` contains `asset`, and return `Err` otherwise.
	pub fn try_take(&mut self, mask: AssetFilter) -> Result<Self, TakeError> {
		self.general_take(mask, false)
	}

	/// Return the assets in `self`, but (asset-wise) of no greater value than `mask`.
	///
	/// The number of unique assets which are returned will respect the `count` parameter in the
	/// counted wildcard variants of `mask`.
	///
	/// Example:
	///
	/// ```
	/// use staging_xcm_executor::AssetsInHolding;
	/// use xcm::latest::prelude::*;
	/// // Note: In real usage, AssetsInHolding is created through TransactAsset operations
	/// // For this example, we use Assets type instead to demonstrate the min() output
	/// let assets_i_have: Assets = vec![ (Here, 100).into(), (Junctions::from([GeneralIndex(0)]), 100).into() ].into();
	/// let assets_they_want: AssetFilter = vec![ (Here, 200).into(), (Junctions::from([GeneralIndex(0)]), 50).into() ].into();
	///
	/// // Normally you would call this on AssetsInHolding, but for documentation purposes:
	/// // let assets_we_can_trade: Assets = assets_i_have.min(&assets_they_want);
	/// // assert_eq!(assets_we_can_trade.inner(), &vec![
	/// // 	(Here, 100).into(), (Junctions::from([GeneralIndex(0)]), 50).into(),
	/// // ]);
	/// ```
	pub fn min(&self, mask: &AssetFilter) -> Assets {
		let mut masked = Assets::new();
		let maybe_limit = mask.limit().map(|x| x as usize);
		if maybe_limit.map_or(false, |l| l == 0) {
			return masked;
		}
		match mask {
			AssetFilter::Wild(All) | AssetFilter::Wild(AllCounted(_)) => {
				if maybe_limit.map_or(true, |l| self.len() <= l) {
					return self.assets_iter().collect::<Vec<Asset>>().into();
				} else {
					for (c, accounting) in self.fungible.iter() {
						masked.push((c.clone(), accounting.amount()).into());
						if maybe_limit.map_or(false, |l| masked.len() >= l) {
							return masked;
						}
					}
					for (c, instance) in self.non_fungible.iter() {
						masked.push((c.clone(), *instance).into());
						if maybe_limit.map_or(false, |l| masked.len() >= l) {
							return masked;
						}
					}
				}
			},
			AssetFilter::Wild(AllOfCounted { fun: WildFungible, id, .. }) |
			AssetFilter::Wild(AllOf { fun: WildFungible, id }) => {
				if let Some(accounting) = self.fungible.get(&id) {
					masked.push((id.clone(), accounting.amount()).into());
				}
			},
			AssetFilter::Wild(AllOfCounted { fun: WildNonFungible, id, .. }) |
			AssetFilter::Wild(AllOf { fun: WildNonFungible, id }) => {
				for (c, instance) in self.non_fungible.iter() {
					if c == id {
						masked.push((c.clone(), *instance).into());
						if maybe_limit.map_or(false, |l| masked.len() >= l) {
							return masked;
						}
					}
				}
			},
			AssetFilter::Definite(assets) => {
				for asset in assets.inner().iter() {
					match asset {
						Asset { fun: Fungible(amount), id } => {
							if let Some(m) = self.fungible.get(id) {
								masked
									.push((id.clone(), Fungible(*amount.min(&m.amount()))).into());
							}
						},
						Asset { fun: NonFungible(instance), id } => {
							let id_instance = (id.clone(), *instance);
							if self.non_fungible.contains(&id_instance) {
								masked.push(id_instance.into());
							}
						},
					}
				}
			},
		}
		masked
	}

	/// Clone this holding for testing purposes only.
	///
	/// This uses `unsafe_clone()` on the imbalance accounting trait objects,
	/// which may not maintain proper accounting invariants. Only use in tests.
	#[cfg(feature = "std")]
	pub fn unsafe_clone_for_tests(&self) -> Self {
		Self {
			fungible: self
				.fungible
				.iter()
				.map(|(id, accounting)| (id.clone(), accounting.unsafe_clone()))
				.collect(),
			non_fungible: self.non_fungible.clone(),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::tests::mock::*;
	use alloc::vec;
	use xcm::latest::prelude::*;

	#[allow(non_snake_case)]
	/// Concrete fungible constructor
	fn CF(amount: u128) -> Asset {
		(Here, amount).into()
	}
	#[allow(non_snake_case)]
	/// Concrete fungible constructor with index for GeneralIndex
	fn CFG(index: u128, amount: u128) -> Asset {
		(GeneralIndex(index), amount).into()
	}
	#[allow(non_snake_case)]
	/// Concrete fungible constructor (parent=1)
	fn CFP(amount: u128) -> Asset {
		(Parent, amount).into()
	}
	#[allow(non_snake_case)]
	/// Concrete fungible constructor (parent=2)
	fn CFPP(amount: u128) -> Asset {
		((Parent, Parent), amount).into()
	}
	#[allow(non_snake_case)]
	/// Concrete non-fungible constructor
	fn CNF(instance_id: u8) -> Asset {
		(Here, [instance_id; 4]).into()
	}

	/// Helper to convert a single Asset into AssetsInHolding for tests
	fn asset_to_holding(asset: Asset) -> AssetsInHolding {
		// Since we can't directly convert Asset to AssetsInHolding, we create an empty
		// holding and manually insert the asset
		let mut holding = AssetsInHolding::new();
		match asset.fun {
			Fungible(amount) => {
				holding.fungible.insert(asset.id, Box::new(MockCredit(amount)));
			},
			NonFungible(instance) => {
				holding.non_fungible.insert((asset.id, instance));
			},
		}
		holding
	}

	fn test_assets() -> AssetsInHolding {
		let mut assets = AssetsInHolding::new();
		assets.subsume_assets(asset_to_holding(CF(300)));
		assets.subsume_assets(asset_to_holding(CNF(40)));
		assets
	}

	#[test]
	fn assets_in_holding_order_works() {
		// populate assets in non-ordered fashion
		let mut assets = AssetsInHolding::new();
		assets.subsume_assets(asset_to_holding(CFPP(300)));
		assets.subsume_assets(asset_to_holding(CFP(200)));
		assets.subsume_assets(asset_to_holding(CNF(2)));
		assets.subsume_assets(asset_to_holding(CF(100)));
		assets.subsume_assets(asset_to_holding(CNF(1)));
		assets.subsume_assets(asset_to_holding(CFG(10, 400)));
		assets.subsume_assets(asset_to_holding(CFG(15, 500)));

		// following is the order we expect from AssetsInHolding
		// - fungibles before non-fungibles
		// - for fungibles, sort by parent first, if parents match, then by other components like
		//   general index
		// - for non-fungibles, sort by instance_id
		let mut iter = assets.unsafe_clone_for_tests().into_assets_iter();
		// fungible, order by parent, parent=0
		assert_eq!(Some(CF(100)), iter.next());
		// fungible, order by parent then by general index, parent=0, general index=10
		assert_eq!(Some(CFG(10, 400)), iter.next());
		// fungible, order by parent then by general index, parent=0, general index=15
		assert_eq!(Some(CFG(15, 500)), iter.next());
		// fungible, order by parent, parent=1
		assert_eq!(Some(CFP(200)), iter.next());
		// fungible, order by parent, parent=2
		assert_eq!(Some(CFPP(300)), iter.next());
		// non-fungible, after fungibles, order by instance id, id=1
		assert_eq!(Some(CNF(1)), iter.next());
		// non-fungible, after fungibles, order by instance id, id=2
		assert_eq!(Some(CNF(2)), iter.next());
		// nothing else in the assets
		assert_eq!(None, iter.next());

		// lets add copy of the assets to the assets itself, just to check if order stays the same
		// we also expect 2x amount for every fungible and collapsed non-fungibles
		let assets_same = assets.unsafe_clone_for_tests();
		assets.subsume_assets(assets_same);

		let mut iter = assets.into_assets_iter();
		assert_eq!(Some(CF(200)), iter.next());
		assert_eq!(Some(CFG(10, 800)), iter.next());
		assert_eq!(Some(CFG(15, 1000)), iter.next());
		assert_eq!(Some(CFP(400)), iter.next());
		assert_eq!(Some(CFPP(600)), iter.next());
		assert_eq!(Some(CNF(1)), iter.next());
		assert_eq!(Some(CNF(2)), iter.next());
		assert_eq!(None, iter.next());
	}

	#[test]
	fn subsume_assets_equal_length_holdings() {
		let mut t1 = test_assets();
		let mut t2 = AssetsInHolding::new();
		t2.subsume_assets(asset_to_holding(CF(300)));
		t2.subsume_assets(asset_to_holding(CNF(50)));

		let t1_clone = t1.unsafe_clone_for_tests();
		let mut t2_clone = t2.unsafe_clone_for_tests();

		// ensure values for same fungibles are summed up together
		// and order is also ok (see assets_in_holding_order_works())
		t1.subsume_assets(t2.unsafe_clone_for_tests());
		let mut iter = t1.into_assets_iter();
		assert_eq!(Some(CF(600)), iter.next());
		assert_eq!(Some(CNF(40)), iter.next());
		assert_eq!(Some(CNF(50)), iter.next());
		assert_eq!(None, iter.next());

		// try the same initial holdings but other way around
		// expecting same exact result as above
		t2_clone.subsume_assets(t1_clone.unsafe_clone_for_tests());
		let mut iter = t2_clone.into_assets_iter();
		assert_eq!(Some(CF(600)), iter.next());
		assert_eq!(Some(CNF(40)), iter.next());
		assert_eq!(Some(CNF(50)), iter.next());
		assert_eq!(None, iter.next());
	}

	#[test]
	fn subsume_assets_different_length_holdings() {
		let mut t1 = AssetsInHolding::new();
		t1.subsume_assets(asset_to_holding(CFP(400)));
		t1.subsume_assets(asset_to_holding(CFPP(100)));

		let mut t2 = AssetsInHolding::new();
		t2.subsume_assets(asset_to_holding(CF(100)));
		t2.subsume_assets(asset_to_holding(CNF(50)));
		t2.subsume_assets(asset_to_holding(CNF(40)));
		t2.subsume_assets(asset_to_holding(CFP(100)));
		t2.subsume_assets(asset_to_holding(CFPP(100)));

		let t1_clone = t1.unsafe_clone_for_tests();
		let mut t2_clone = t2.unsafe_clone_for_tests();

		// ensure values for same fungibles are summed up together
		// and order is also ok (see assets_in_holding_order_works())
		t1.subsume_assets(t2);
		let mut iter = t1.into_assets_iter();
		assert_eq!(Some(CF(100)), iter.next());
		assert_eq!(Some(CFP(500)), iter.next());
		assert_eq!(Some(CFPP(200)), iter.next());
		assert_eq!(Some(CNF(40)), iter.next());
		assert_eq!(Some(CNF(50)), iter.next());
		assert_eq!(None, iter.next());

		// try the same initial holdings but other way around
		// expecting same exact result as above
		t2_clone.subsume_assets(t1_clone);
		let mut iter = t2_clone.into_assets_iter();
		assert_eq!(Some(CF(100)), iter.next());
		assert_eq!(Some(CFP(500)), iter.next());
		assert_eq!(Some(CFPP(200)), iter.next());
		assert_eq!(Some(CNF(40)), iter.next());
		assert_eq!(Some(CNF(50)), iter.next());
		assert_eq!(None, iter.next());
	}

	#[test]
	fn subsume_assets_empty_holding() {
		let mut t1 = AssetsInHolding::new();
		let t2 = AssetsInHolding::new();
		t1.subsume_assets(t2.unsafe_clone_for_tests());
		let mut iter = t1.unsafe_clone_for_tests().into_assets_iter();
		assert_eq!(None, iter.next());

		t1.subsume_assets(asset_to_holding(CFP(400)));
		t1.subsume_assets(asset_to_holding(CNF(40)));
		t1.subsume_assets(asset_to_holding(CFPP(100)));

		let t1_clone = t1.unsafe_clone_for_tests();
		let mut t2_clone = t2.unsafe_clone_for_tests();

		// ensure values for same fungibles are summed up together
		// and order is also ok (see assets_in_holding_order_works())
		t1.subsume_assets(t2.unsafe_clone_for_tests());
		let mut iter = t1.into_assets_iter();
		assert_eq!(Some(CFP(400)), iter.next());
		assert_eq!(Some(CFPP(100)), iter.next());
		assert_eq!(Some(CNF(40)), iter.next());
		assert_eq!(None, iter.next());

		// try the same initial holdings but other way around
		// expecting same exact result as above
		t2_clone.subsume_assets(t1_clone.unsafe_clone_for_tests());
		let mut iter = t2_clone.into_assets_iter();
		assert_eq!(Some(CFP(400)), iter.next());
		assert_eq!(Some(CFPP(100)), iter.next());
		assert_eq!(Some(CNF(40)), iter.next());
		assert_eq!(None, iter.next());
	}

	#[test]
	fn into_assets_iter_works() {
		let assets = test_assets();
		let mut iter = assets.into_assets_iter();
		// Order defined by implementation: CF, CNF
		assert_eq!(Some(CF(300)), iter.next());
		assert_eq!(Some(CNF(40)), iter.next());
		assert_eq!(None, iter.next());
	}

	#[test]
	fn assets_into_works() {
		let mut assets_vec: Vec<Asset> = Vec::new();
		assets_vec.push(CF(300));
		assets_vec.push(CNF(40));
		// Push same group of tokens again
		assets_vec.push(CF(300));
		assets_vec.push(CNF(40));

		let mut assets = AssetsInHolding::new();
		for asset in assets_vec {
			assets.subsume_assets(asset_to_holding(asset));
		}
		let mut iter = assets.into_assets_iter();
		// Fungibles add
		assert_eq!(Some(CF(600)), iter.next());
		// Non-fungibles collapse
		assert_eq!(Some(CNF(40)), iter.next());
		assert_eq!(None, iter.next());
	}

	#[test]
	fn min_all_and_none_works() {
		let assets = test_assets();
		let none = Assets::new().into();
		let all = All.into();

		let none_min = assets.min(&none);
		assert_eq!(None, none_min.inner().iter().next());
		let all_min = assets.min(&all);
		let all_min_vec: Vec<_> = all_min.inner().iter().cloned().collect();
		let assets_vec: Vec<_> = assets.assets_iter().collect();
		assert_eq!(all_min_vec, assets_vec);
	}

	#[test]
	fn min_counted_works() {
		let mut assets = AssetsInHolding::new();
		assets.subsume_assets(asset_to_holding(CNF(40)));
		assets.subsume_assets(asset_to_holding(CF(3000)));
		assets.subsume_assets(asset_to_holding(CNF(80)));
		let all = WildAsset::AllCounted(6).into();

		let all = assets.min(&all);
		assert_eq!(all.inner(), &vec![CF(3000), CNF(40), CNF(80)]);
	}

	#[test]
	fn min_all_concrete_works() {
		let assets = test_assets();
		let fungible = Wild((Here, WildFungible).into());
		let non_fungible = Wild((Here, WildNonFungible).into());

		let fungible = assets.min(&fungible);
		assert_eq!(fungible.inner(), &vec![CF(300)]);
		let non_fungible = assets.min(&non_fungible);
		assert_eq!(non_fungible.inner(), &vec![CNF(40)]);
	}

	#[test]
	fn min_basic_works() {
		let assets1 = test_assets();

		// Create Assets directly instead of going through AssetsInHolding
		let assets2: Assets = vec![
			// This is more then 300, so it should stay at 300
			CF(600),
			// This asset should be included
			CNF(40),
		]
		.into();

		let assets_min = assets1.min(&assets2.into());
		assert_eq!(assets_min.inner(), &vec![CF(300), CNF(40)]);
	}

	#[test]
	fn saturating_take_all_and_none_works() {
		let mut assets = test_assets();

		let taken_none = assets.saturating_take(vec![].into());
		assert_eq!(None, taken_none.assets_iter().next());
		let taken_all = assets.saturating_take(All.into());
		// Everything taken
		assert_eq!(None, assets.assets_iter().next());
		let all_iter = taken_all.assets_iter();
		assert!(all_iter.eq(test_assets().assets_iter()));
	}

	#[test]
	fn saturating_take_all_concrete_works() {
		let mut assets = test_assets();
		let fungible = Wild((Here, WildFungible).into());
		let non_fungible = Wild((Here, WildNonFungible).into());

		let fungible = assets.saturating_take(fungible);
		let fungible = fungible.assets_iter().collect::<Vec<_>>();
		assert_eq!(fungible, vec![CF(300)]);
		let non_fungible = assets.saturating_take(non_fungible);
		let non_fungible = non_fungible.assets_iter().collect::<Vec<_>>();
		assert_eq!(non_fungible, vec![CNF(40)]);
	}

	#[test]
	fn saturating_take_basic_works() {
		let mut assets1 = test_assets();

		// Create Assets directly instead of going through AssetsInHolding
		let assets2: Assets = vec![
			// This is more then 300, so it takes everything
			CF(600),
			// This asset should be taken
			CNF(40),
		]
		.into();

		let taken = assets1.saturating_take(assets2.into());
		let taken_vec: Vec<_> = taken.assets_iter().collect();
		assert_eq!(taken_vec, vec![CF(300), CNF(40)]);
	}

	#[test]
	fn try_take_all_counted_works() {
		let mut assets = AssetsInHolding::new();
		assets.subsume_assets(asset_to_holding(CNF(40)));
		assets.subsume_assets(asset_to_holding(CF(3000)));
		assets.subsume_assets(asset_to_holding(CNF(80)));
		let all = assets.try_take(WildAsset::AllCounted(6).into()).unwrap();
		let all_vec: Vec<_> = all.assets_iter().collect();
		assert_eq!(all_vec, vec![CF(3000), CNF(40), CNF(80)]);
	}

	#[test]
	fn try_take_fungibles_counted_works() {
		let mut assets = AssetsInHolding::new();
		assets.subsume_assets(asset_to_holding(CNF(40)));
		assets.subsume_assets(asset_to_holding(CF(3000)));
		assets.subsume_assets(asset_to_holding(CNF(80)));
		let assets_vec: Vec<_> = assets.assets_iter().collect();
		assert_eq!(assets_vec, vec![CF(3000), CNF(40), CNF(80)]);
	}

	#[test]
	fn try_take_non_fungibles_counted_works() {
		let mut assets = AssetsInHolding::new();
		assets.subsume_assets(asset_to_holding(CNF(40)));
		assets.subsume_assets(asset_to_holding(CF(3000)));
		assets.subsume_assets(asset_to_holding(CNF(80)));
		let assets_vec: Vec<_> = assets.assets_iter().collect();
		assert_eq!(assets_vec, vec![CF(3000), CNF(40), CNF(80)]);
	}
}
