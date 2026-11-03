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

//! Pallet to process claims from Ethereum addresses.

#[cfg(not(feature = "std"))]
use alloc::{format, string::String};
use alloc::{vec, vec::Vec};
use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use core::fmt::Debug;
use frame_support::{
	ensure,
	traits::{Currency, Get, IsSubType, VestingSchedule},
	weights::Weight,
	DefaultNoBound,
};
pub use pallet::*;
use polkadot_primitives::ValidityError;
use scale_info::TypeInfo;
use serde::{self, Deserialize, Deserializer, Serialize, Serializer};
use sp_io::{crypto::secp256k1_ecdsa_recover, hashing::keccak_256};
use sp_runtime::{
	impl_tx_ext_default,
	traits::{
		AsSystemOriginSigner, AsTransactionAuthorizedOrigin, CheckedSub, DispatchInfoOf,
		Dispatchable, Saturating, TransactionExtension, Zero,
	},
	transaction_validity::{
		InvalidTransaction, TransactionSource, TransactionValidity, TransactionValidityError,
		ValidTransaction,
	},
};

type CurrencyOf<T> = <<T as Config>::VestingSchedule as VestingSchedule<
	<T as frame_system::Config>::AccountId,
>>::Currency;
type BalanceOf<T> = <CurrencyOf<T> as Currency<<T as frame_system::Config>::AccountId>>::Balance;

pub trait WeightInfo {
	fn claim() -> Weight;
	fn mint_claim() -> Weight;
	fn claim_attest() -> Weight;
	fn attest() -> Weight;
	fn move_claim() -> Weight;
	fn prevalidate_attests() -> Weight;
}

pub struct TestWeightInfo;
impl WeightInfo for TestWeightInfo {
	fn claim() -> Weight {
		Weight::zero()
	}
	fn mint_claim() -> Weight {
		Weight::zero()
	}
	fn claim_attest() -> Weight {
		Weight::zero()
	}
	fn attest() -> Weight {
		Weight::zero()
	}
	fn move_claim() -> Weight {
		Weight::zero()
	}
	fn prevalidate_attests() -> Weight {
		Weight::zero()
	}
}

/// The kind of statement an account needs to make for a claim to be valid.
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	Clone,
	Copy,
	Eq,
	PartialEq,
	Debug,
	TypeInfo,
	Serialize,
	Deserialize,
	MaxEncodedLen,
)]
pub enum StatementKind {
	/// Statement required to be made by non-SAFT holders.
	Regular,
	/// Statement required to be made by SAFT holders.
	Saft,
}

impl StatementKind {
	/// Convert this to the (English) statement it represents.
	fn to_text(self) -> &'static [u8] {
		match self {
			StatementKind::Regular => {
				&b"I hereby agree to the terms of the statement whose SHA-256 multihash is \
				Qmc1XYqT6S39WNp2UeiRUrZichUWUPpGEThDE6dAb3f6Ny. (This may be found at the URL: \
				https://statement.polkadot.network/regular.html)"[..]
			},
			StatementKind::Saft => {
				&b"I hereby agree to the terms of the statement whose SHA-256 multihash is \
				QmXEkMahfhHJPzT3RjkXiZVFi77ZeVeuxtAjhojGRNYckz. (This may be found at the URL: \
				https://statement.polkadot.network/saft.html)"[..]
			},
		}
	}
}

impl Default for StatementKind {
	fn default() -> Self {
		StatementKind::Regular
	}
}

/// An Ethereum address (i.e. 20 bytes, used to represent an Ethereum account).
///
/// This gets serialized to the 0x-prefixed hex representation.
#[derive(
	Clone,
	Copy,
	PartialEq,
	Eq,
	Encode,
	Decode,
	DecodeWithMemTracking,
	Default,
	Debug,
	TypeInfo,
	MaxEncodedLen,
)]
pub struct EthereumAddress(pub [u8; 20]);

impl Serialize for EthereumAddress {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		let hex: String = rustc_hex::ToHex::to_hex(&self.0[..]);
		serializer.serialize_str(&format!("0x{}", hex))
	}
}

impl<'de> Deserialize<'de> for EthereumAddress {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: Deserializer<'de>,
	{
		let base_string = String::deserialize(deserializer)?;
		let offset = if base_string.starts_with("0x") { 2 } else { 0 };
		let s = &base_string[offset..];
		if s.len() != 40 {
			Err(serde::de::Error::custom(
				"Bad length of Ethereum address (should be 42 including '0x')",
			))?;
		}
		let raw: Vec<u8> = rustc_hex::FromHex::from_hex(s)
			.map_err(|e| serde::de::Error::custom(format!("{:?}", e)))?;
		let mut r = Self::default();
		r.0.copy_from_slice(&raw);
		Ok(r)
	}
}

impl AsRef<[u8]> for EthereumAddress {
	fn as_ref(&self) -> &[u8] {
		&self.0[..]
	}
}

#[derive(Encode, Decode, DecodeWithMemTracking, Clone, TypeInfo, MaxEncodedLen)]
pub struct EcdsaSignature(pub [u8; 65]);

impl PartialEq for EcdsaSignature {
	fn eq(&self, other: &Self) -> bool {
		&self.0[..] == &other.0[..]
	}
}

impl core::fmt::Debug for EcdsaSignature {
	fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
		write!(f, "EcdsaSignature({:?})", &self.0[..])
	}
}

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use frame_support::pallet_prelude::*;
	use frame_system::pallet_prelude::*;

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	/// Configuration trait.
	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// The overarching event type.
		#[allow(deprecated)]
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;
		type VestingSchedule: VestingSchedule<Self::AccountId, Moment = BlockNumberFor<Self>>;
		#[pallet::constant]
		type Prefix: Get<&'static [u8]>;
		type MoveClaimOrigin: EnsureOrigin<Self::RuntimeOrigin>;
		type WeightInfo: WeightInfo;
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// Someone claimed some DOTs.
		Claimed { who: T::AccountId, ethereum_address: EthereumAddress, amount: BalanceOf<T> },
	}

	#[pallet::error]
	pub enum Error<T> {
		/// Invalid Ethereum signature.
		InvalidEthereumSignature,
		/// Ethereum address has no claim.
		SignerHasNoClaim,
		/// Account ID sending transaction has no claim.
		SenderHasNoClaim,
		/// There's not enough in the pot to pay out some unvested amount. Generally implies a
		/// logic error.
		PotUnderflow,
		/// A needed statement was not included.
		InvalidStatement,
		/// The account already has a vested balance.
		VestedBalanceExists,
		/// The claim has a vesting schedule but its value is below the existential deposit, so the
		/// destination account could not be kept alive to carry the vesting lock.
		ClaimBelowExistentialDeposit,
	}

	#[pallet::storage]
	pub type Claims<T: Config> = StorageMap<_, Identity, EthereumAddress, BalanceOf<T>>;

	#[pallet::storage]
	pub type Total<T: Config> = StorageValue<_, BalanceOf<T>, ValueQuery>;

	/// Vesting schedule for a claim.
	/// First balance is the total amount that should be held for vesting.
	/// Second balance is how much should be unlocked per block.
	/// The block number is when the vesting should start.
	#[pallet::storage]
	pub type Vesting<T: Config> =
		StorageMap<_, Identity, EthereumAddress, (BalanceOf<T>, BalanceOf<T>, BlockNumberFor<T>)>;

	/// The statement kind that must be signed, if any.
	#[pallet::storage]
	pub type Signing<T> = StorageMap<_, Identity, EthereumAddress, StatementKind>;

	/// Pre-claimed Ethereum accounts, by the Account ID that they are claimed to.
	#[pallet::storage]
	pub type Preclaims<T: Config> = StorageMap<_, Identity, T::AccountId, EthereumAddress>;

	#[pallet::genesis_config]
	#[derive(DefaultNoBound)]
	pub struct GenesisConfig<T: Config> {
		pub claims:
			Vec<(EthereumAddress, BalanceOf<T>, Option<T::AccountId>, Option<StatementKind>)>,
		pub vesting: Vec<(EthereumAddress, (BalanceOf<T>, BalanceOf<T>, BlockNumberFor<T>))>,
	}

	#[pallet::genesis_build]
	impl<T: Config> BuildGenesisConfig for GenesisConfig<T> {
		fn build(&self) {
			// build `Claims`
			self.claims.iter().map(|(a, b, _, _)| (*a, *b)).for_each(|(a, b)| {
				Claims::<T>::insert(a, b);
			});
			// build `Total`
			Total::<T>::put(
				self.claims
					.iter()
					.fold(Zero::zero(), |acc: BalanceOf<T>, &(_, b, _, _)| acc + b),
			);
			// build `Vesting`
			self.vesting.iter().for_each(|(k, v)| {
				Vesting::<T>::insert(k, v);
			});
			// build `Signing`
			self.claims
				.iter()
				.filter_map(|(a, _, _, s)| Some((*a, (*s)?)))
				.for_each(|(a, s)| {
					Signing::<T>::insert(a, s);
				});
			// build `Preclaims`
			self.claims.iter().filter_map(|(a, _, i, _)| Some((i.clone()?, *a))).for_each(
				|(i, a)| {
					Preclaims::<T>::insert(i, a);
				},
			);
		}
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Make a claim to collect your DOTs.
		///
		/// The dispatch origin for this call must be _None_.
		///
		/// Unsigned Validation:
		/// A call to claim is deemed valid if the signature provided matches
		/// the expected signed message of:
		///
		/// > Ethereum Signed Message:
		/// > (configured prefix string)(address)
		///
		/// and `address` matches the `dest` account.
		///
		/// Parameters:
		/// - `dest`: The destination account to payout the claim.
		/// - `ethereum_signature`: The signature of an ethereum signed message matching the format
		///   described above.
		///
		/// <weight>
		/// The weight of this call is invariant over the input parameters.
		/// Weight includes logic to validate unsigned `claim` call.
		///
		/// Total Complexity: O(1)
		/// </weight>
		#[pallet::call_index(0)]
		#[pallet::weight(T::WeightInfo::claim())]
		pub fn claim(
			origin: OriginFor<T>,
			dest: T::AccountId,
			ethereum_signature: EcdsaSignature,
		) -> DispatchResult {
			ensure_none(origin)?;

			let data = dest.using_encoded(to_ascii_hex);
			let signer = Self::eth_recover(&ethereum_signature, &data, &[][..])
				.ok_or(Error::<T>::InvalidEthereumSignature)?;
			ensure!(Signing::<T>::get(&signer).is_none(), Error::<T>::InvalidStatement);

			Self::process_claim(signer, dest)?;
			Ok(())
		}

		/// Mint a new claim to collect DOTs.
		///
		/// The dispatch origin for this call must be _Root_.
		///
		/// Parameters:
		/// - `who`: The Ethereum address allowed to collect this claim.
		/// - `value`: The number of DOTs that will be claimed.
		/// - `vesting_schedule`: An optional vesting schedule for these DOTs.
		///
		/// <weight>
		/// The weight of this call is invariant over the input parameters.
		/// We assume worst case that both vesting and statement is being inserted.
		///
		/// Total Complexity: O(1)
		/// </weight>
		#[pallet::call_index(1)]
		#[pallet::weight(T::WeightInfo::mint_claim())]
		pub fn mint_claim(
			origin: OriginFor<T>,
			who: EthereumAddress,
			value: BalanceOf<T>,
			vesting_schedule: Option<(BalanceOf<T>, BalanceOf<T>, BlockNumberFor<T>)>,
			statement: Option<StatementKind>,
		) -> DispatchResult {
			ensure_root(origin)?;
			Total::<T>::mutate(|t| *t += value);
			Claims::<T>::insert(who, value);
			if let Some(vs) = vesting_schedule {
				Vesting::<T>::insert(who, vs);
			}
			if let Some(s) = statement {
				Signing::<T>::insert(who, s);
			}
			Ok(())
		}

		/// Make a claim to collect your DOTs by signing a statement.
		///
		/// The dispatch origin for this call must be _None_.
		///
		/// Unsigned Validation:
		/// A call to `claim_attest` is deemed valid if the signature provided matches
		/// the expected signed message of:
		///
		/// > Ethereum Signed Message:
		/// > (configured prefix string)(address)(statement)
		///
		/// and `address` matches the `dest` account; the `statement` must match that which is
		/// expected according to your purchase arrangement.
		///
		/// Parameters:
		/// - `dest`: The destination account to payout the claim.
		/// - `ethereum_signature`: The signature of an ethereum signed message matching the format
		///   described above.
		/// - `statement`: The identity of the statement which is being attested to in the
		///   signature.
		///
		/// <weight>
		/// The weight of this call is invariant over the input parameters.
		/// Weight includes logic to validate unsigned `claim_attest` call.
		///
		/// Total Complexity: O(1)
		/// </weight>
		#[pallet::call_index(2)]
		#[pallet::weight(T::WeightInfo::claim_attest())]
		pub fn claim_attest(
			origin: OriginFor<T>,
			dest: T::AccountId,
			ethereum_signature: EcdsaSignature,
			statement: Vec<u8>,
		) -> DispatchResult {
			ensure_none(origin)?;

			let data = dest.using_encoded(to_ascii_hex);
			let signer = Self::eth_recover(&ethereum_signature, &data, &statement)
				.ok_or(Error::<T>::InvalidEthereumSignature)?;
			if let Some(s) = Signing::<T>::get(signer) {
				ensure!(s.to_text() == &statement[..], Error::<T>::InvalidStatement);
			}
			Self::process_claim(signer, dest)?;
			Ok(())
		}

		/// Attest to a statement, needed to finalize the claims process.
		///
		/// WARNING: Insecure unless your chain includes `PrevalidateAttests` as a
		/// `TransactionExtension`.
		///
		/// Unsigned Validation:
		/// A call to attest is deemed valid if the sender has a `Preclaim` registered
		/// and provides a `statement` which is expected for the account.
		///
		/// Parameters:
		/// - `statement`: The identity of the statement which is being attested to in the
		///   signature.
		///
		/// <weight>
		/// The weight of this call is invariant over the input parameters.
		/// Weight includes logic to do pre-validation on `attest` call.
		///
		/// Total Complexity: O(1)
		/// </weight>
		#[pallet::call_index(3)]
		#[pallet::weight((
			T::WeightInfo::attest(),
			DispatchClass::Normal,
			Pays::No
		))]
		pub fn attest(origin: OriginFor<T>, statement: Vec<u8>) -> DispatchResult {
			let who = ensure_signed(origin)?;
			let signer = Preclaims::<T>::get(&who).ok_or(Error::<T>::SenderHasNoClaim)?;
			if let Some(s) = Signing::<T>::get(signer) {
				ensure!(s.to_text() == &statement[..], Error::<T>::InvalidStatement);
			}
			Self::process_claim(signer, who.clone())?;
			Preclaims::<T>::remove(&who);
			Ok(())
		}

		#[pallet::call_index(4)]
		#[pallet::weight(T::WeightInfo::move_claim())]
		pub fn move_claim(
			origin: OriginFor<T>,
			old: EthereumAddress,
			new: EthereumAddress,
			maybe_preclaim: Option<T::AccountId>,
		) -> DispatchResultWithPostInfo {
			T::MoveClaimOrigin::try_origin(origin).map(|_| ()).or_else(ensure_root)?;

			Claims::<T>::take(&old).map(|c| Claims::<T>::insert(&new, c));
			Vesting::<T>::take(&old).map(|c| Vesting::<T>::insert(&new, c));
			Signing::<T>::take(&old).map(|c| Signing::<T>::insert(&new, c));
			maybe_preclaim.map(|preclaim| {
				Preclaims::<T>::mutate(&preclaim, |maybe_o| {
					if maybe_o.as_ref().map_or(false, |o| o == &old) {
						*maybe_o = Some(new)
					}
				})
			});
			Ok(Pays::No.into())
		}
	}

	#[allow(deprecated)]
	#[pallet::validate_unsigned]
	impl<T: Config> ValidateUnsigned for Pallet<T> {
		type Call = Call<T>;

		fn validate_unsigned(_source: TransactionSource, call: &Self::Call) -> TransactionValidity {
			const PRIORITY: u64 = 100;

			let (maybe_signer, maybe_statement) = match call {
				// <weight>
				// The weight of this logic is included in the `claim` dispatchable.
				// </weight>
				Call::claim { dest: account, ethereum_signature } => {
					let data = account.using_encoded(to_ascii_hex);
					(Self::eth_recover(&ethereum_signature, &data, &[][..]), None)
				},
				// <weight>
				// The weight of this logic is included in the `claim_attest` dispatchable.
				// </weight>
				Call::claim_attest { dest: account, ethereum_signature, statement } => {
					let data = account.using_encoded(to_ascii_hex);
					(
						Self::eth_recover(&ethereum_signature, &data, &statement),
						Some(statement.as_slice()),
					)
				},
				_ => return Err(InvalidTransaction::Call.into()),
			};

			let signer = maybe_signer.ok_or(InvalidTransaction::Custom(
				ValidityError::InvalidEthereumSignature.into(),
			))?;

			let e = InvalidTransaction::Custom(ValidityError::SignerHasNoClaim.into());
			ensure!(Claims::<T>::contains_key(&signer), e);

			let e = InvalidTransaction::Custom(ValidityError::InvalidStatement.into());
			match Signing::<T>::get(signer) {
				None => ensure!(maybe_statement.is_none(), e),
				Some(s) => ensure!(Some(s.to_text()) == maybe_statement, e),
			}

			Ok(ValidTransaction {
				priority: PRIORITY,
				requires: vec![],
				provides: vec![("claims", signer).encode()],
				longevity: TransactionLongevity::max_value(),
				propagate: true,
			})
		}
	}
}

/// Converts the given binary data into ASCII-encoded hex. It will be twice the length.
fn to_ascii_hex(data: &[u8]) -> Vec<u8> {
	let mut r = Vec::with_capacity(data.len() * 2);
	let mut push_nibble = |n| r.push(if n < 10 { b'0' + n } else { b'a' - 10 + n });
	for &b in data.iter() {
		push_nibble(b / 16);
		push_nibble(b % 16);
	}
	r
}

impl<T: Config> Pallet<T> {
	// Constructs the message that Ethereum RPC's `personal_sign` and `eth_sign` would sign.
	fn ethereum_signable_message(what: &[u8], extra: &[u8]) -> Vec<u8> {
		let prefix = T::Prefix::get();
		let mut l = prefix.len() + what.len() + extra.len();
		let mut rev = Vec::new();
		while l > 0 {
			rev.push(b'0' + (l % 10) as u8);
			l /= 10;
		}
		let mut v = b"\x19Ethereum Signed Message:\n".to_vec();
		v.extend(rev.into_iter().rev());
		v.extend_from_slice(prefix);
		v.extend_from_slice(what);
		v.extend_from_slice(extra);
		v
	}

	// Attempts to recover the Ethereum address from a message signature signed by using
	// the Ethereum RPC's `personal_sign` and `eth_sign`.
	fn eth_recover(s: &EcdsaSignature, what: &[u8], extra: &[u8]) -> Option<EthereumAddress> {
		let msg = keccak_256(&Self::ethereum_signable_message(what, extra));
		let mut res = EthereumAddress::default();
		res.0
			.copy_from_slice(&keccak_256(&secp256k1_ecdsa_recover(&s.0, &msg).ok()?[..])[12..]);
		Some(res)
	}

	fn process_claim(signer: EthereumAddress, dest: T::AccountId) -> sp_runtime::DispatchResult {
		let balance_due = Claims::<T>::get(&signer).ok_or(Error::<T>::SignerHasNoClaim)?;

		let new_total =
			Total::<T>::get().checked_sub(&balance_due).ok_or(Error::<T>::PotUnderflow)?;

		let vesting = Vesting::<T>::get(&signer);
		if let Some(_) = vesting {
			if T::VestingSchedule::vesting_balance(&dest).is_some() {
				return Err(Error::<T>::VestedBalanceExists.into());
			}

			// A vesting schedule installs a balance lock, which requires the account to stay alive,
			// otherwise it is dusted and the lock placed on a non-existent account. The `dest` may
			// already hold funds, so a small claim that tops an existing account over the ED is
			// valid.
			let free_after = CurrencyOf::<T>::free_balance(&dest).saturating_add(balance_due);
			ensure!(
				free_after >= CurrencyOf::<T>::minimum_balance(),
				Error::<T>::ClaimBelowExistentialDeposit,
			);
		}

		// We first need to deposit the balance to ensure that the account exists.
		let _ = CurrencyOf::<T>::deposit_creating(&dest, balance_due);

		// Check if this claim should have a vesting schedule.
		if let Some(vs) = vesting {
			// This can only fail if the account already has a vesting schedule or its balance is
			// below the existential deposit, both of which are checked above.
			T::VestingSchedule::add_vesting_schedule(&dest, vs.0, vs.1, vs.2)
				.map_err(|_| Error::<T>::VestedBalanceExists)?;
		}

		Total::<T>::put(new_total);
		Claims::<T>::remove(&signer);
		Vesting::<T>::remove(&signer);
		Signing::<T>::remove(&signer);

		// Let's deposit an event to let the outside world know this happened.
		Self::deposit_event(Event::<T>::Claimed {
			who: dest,
			ethereum_address: signer,
			amount: balance_due,
		});

		Ok(())
	}
}

/// Validate `attest` calls prior to execution. Needed to avoid a DoS attack since they are
/// otherwise free to place on chain.
#[derive(Encode, Decode, DecodeWithMemTracking, Clone, Eq, PartialEq, TypeInfo)]
#[scale_info(skip_type_params(T))]
pub struct PrevalidateAttests<T>(core::marker::PhantomData<fn(T)>);

impl<T: Config> Debug for PrevalidateAttests<T>
where
	<T as frame_system::Config>::RuntimeCall: IsSubType<Call<T>>,
{
	#[cfg(feature = "std")]
	fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
		write!(f, "PrevalidateAttests")
	}

	#[cfg(not(feature = "std"))]
	fn fmt(&self, _: &mut core::fmt::Formatter) -> core::fmt::Result {
		Ok(())
	}
}

impl<T: Config> PrevalidateAttests<T>
where
	<T as frame_system::Config>::RuntimeCall: IsSubType<Call<T>>,
{
	/// Create new `TransactionExtension` to check runtime version.
	pub fn new() -> Self {
		Self(core::marker::PhantomData)
	}
}

impl<T: Config> TransactionExtension<T::RuntimeCall> for PrevalidateAttests<T>
where
	<T as frame_system::Config>::RuntimeCall: IsSubType<Call<T>>,
	<<T as frame_system::Config>::RuntimeCall as Dispatchable>::RuntimeOrigin:
		AsSystemOriginSigner<T::AccountId> + AsTransactionAuthorizedOrigin + Clone,
{
	const IDENTIFIER: &'static str = "PrevalidateAttests";
	type Implicit = ();
	type Pre = ();
	type Val = ();

	fn weight(&self, call: &T::RuntimeCall) -> Weight {
		if let Some(Call::attest { .. }) = call.is_sub_type() {
			T::WeightInfo::prevalidate_attests()
		} else {
			Weight::zero()
		}
	}

	fn validate(
		&self,
		origin: <T::RuntimeCall as Dispatchable>::RuntimeOrigin,
		call: &T::RuntimeCall,
		_info: &DispatchInfoOf<T::RuntimeCall>,
		_len: usize,
		_self_implicit: Self::Implicit,
		_inherited_implication: &impl Encode,
		_source: TransactionSource,
	) -> Result<
		(ValidTransaction, Self::Val, <T::RuntimeCall as Dispatchable>::RuntimeOrigin),
		TransactionValidityError,
	> {
		if let Some(Call::attest { statement: attested_statement }) = call.is_sub_type() {
			let who = origin.as_system_origin_signer().ok_or(InvalidTransaction::BadSigner)?;
			let signer = Preclaims::<T>::get(who)
				.ok_or(InvalidTransaction::Custom(ValidityError::SignerHasNoClaim.into()))?;
			if let Some(s) = Signing::<T>::get(signer) {
				let e = InvalidTransaction::Custom(ValidityError::InvalidStatement.into());
				ensure!(&attested_statement[..] == s.to_text(), e);
			}
		}
		Ok((ValidTransaction::default(), (), origin))
	}

	impl_tx_ext_default!(T::RuntimeCall; prepare);
}

#[cfg(any(test, feature = "runtime-benchmarks"))]
mod secp_utils {
	use super::*;

	pub fn public(secret: &libsecp256k1::SecretKey) -> libsecp256k1::PublicKey {
		libsecp256k1::PublicKey::from_secret_key(secret)
	}
	pub fn eth(secret: &libsecp256k1::SecretKey) -> EthereumAddress {
		let mut res = EthereumAddress::default();
		res.0.copy_from_slice(&keccak_256(&public(secret).serialize()[1..65])[12..]);
		res
	}
	pub fn sig<T: Config>(
		secret: &libsecp256k1::SecretKey,
		what: &[u8],
		extra: &[u8],
	) -> EcdsaSignature {
		let msg = keccak_256(&super::Pallet::<T>::ethereum_signable_message(
			&to_ascii_hex(what)[..],
			extra,
		));
		let (sig, recovery_id) = libsecp256k1::sign(&libsecp256k1::Message::parse(&msg), secret);
		let mut r = [0u8; 65];
		r[0..64].copy_from_slice(&sig.serialize()[..]);
		r[64] = recovery_id.serialize();
		EcdsaSignature(r)
	}
}

#[cfg(test)]
mod mock;

#[cfg(test)]
mod tests;

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;
