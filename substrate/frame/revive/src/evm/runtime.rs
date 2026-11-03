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
//! Runtime types for integrating `pallet-revive` with the EVM.
use crate::{
	AccountIdOf, AddressMapper, BalanceOf, CallOf, Config, LOG_TARGET, Pallet, Zero,
	evm::{
		CreateCallMode,
		api::{GenericTransaction, TransactionSigned},
		fees::InfoT,
	},
};
use codec::{Decode, DecodeWithMemTracking, Encode};
use frame_support::{
	dispatch::{DispatchInfo, GetDispatchInfo},
	traits::{
		InherentBuilder, IsSubType, SignedTransactionBuilder,
		fungible::Balanced,
		tokens::{Fortitude, Precision, Preservation},
	},
};
use pallet_transaction_payment::Config as TxConfig;
use scale_info::{StaticTypeInfo, TypeInfo};
use sp_core::U256;
use sp_runtime::{
	Debug, OpaqueExtrinsic, Weight,
	generic::{self, CheckedExtrinsic, ExtrinsicFormat},
	traits::{
		Checkable, ExtrinsicCall, ExtrinsicLike, ExtrinsicMetadata, LazyExtrinsic, Pipeline,
		TransactionExtension,
	},
	transaction_validity::{InvalidTransaction, TransactionValidityError},
};

/// Used to set the weight limit argument of a `eth_call` or `eth_instantiate_with_code` call.
pub trait SetWeightLimit {
	/// Set the weight limit of this call.
	///
	/// Returns the replaced weight.
	fn set_weight_limit(&mut self, weight_limit: Weight) -> Weight;
}

/// Wraps [`generic::UncheckedExtrinsic`] to support checking unsigned
/// [`crate::Call::eth_transact`] extrinsic.
#[derive(Encode, Decode, DecodeWithMemTracking, Clone, PartialEq, Eq, Debug)]
pub struct UncheckedExtrinsic<Address, Signature, E: EthExtra>(
	pub  generic::UncheckedExtrinsic<
		Address,
		CallOf<E::Config>,
		Signature,
		E::ExtensionV0,
		E::ExtensionOtherVersions,
	>,
);

impl<Address, Signature, E: EthExtra> TypeInfo for UncheckedExtrinsic<Address, Signature, E>
where
	Address: StaticTypeInfo,
	Signature: StaticTypeInfo,
	E::ExtensionV0: StaticTypeInfo,
{
	type Identity = generic::UncheckedExtrinsic<
		Address,
		CallOf<E::Config>,
		Signature,
		E::ExtensionV0,
		E::ExtensionOtherVersions,
	>;
	fn type_info() -> scale_info::Type {
		generic::UncheckedExtrinsic::<
			Address,
			CallOf<E::Config>,
			Signature,
			E::ExtensionV0,
			E::ExtensionOtherVersions,
		>::type_info()
	}
}

impl<Address, Signature, E: EthExtra>
	From<
		generic::UncheckedExtrinsic<
			Address,
			CallOf<E::Config>,
			Signature,
			E::ExtensionV0,
			E::ExtensionOtherVersions,
		>,
	> for UncheckedExtrinsic<Address, Signature, E>
{
	fn from(
		utx: generic::UncheckedExtrinsic<
			Address,
			CallOf<E::Config>,
			Signature,
			E::ExtensionV0,
			E::ExtensionOtherVersions,
		>,
	) -> Self {
		Self(utx)
	}
}

impl<Address: TypeInfo, Signature: TypeInfo, E: EthExtra> ExtrinsicLike
	for UncheckedExtrinsic<Address, Signature, E>
{
	fn is_bare(&self) -> bool {
		ExtrinsicLike::is_bare(&self.0)
	}
}

impl<Address, Signature, E: EthExtra> ExtrinsicMetadata
	for UncheckedExtrinsic<Address, Signature, E>
{
	const VERSIONS: &'static [u8] = generic::UncheckedExtrinsic::<
		Address,
		CallOf<E::Config>,
		Signature,
		E::ExtensionV0,
		E::ExtensionOtherVersions,
	>::VERSIONS;
	type TransactionExtensionPipelines = <generic::UncheckedExtrinsic<
		Address,
		CallOf<E::Config>,
		Signature,
		E::ExtensionV0,
		E::ExtensionOtherVersions,
	> as ExtrinsicMetadata>::TransactionExtensionPipelines;
}

impl<Address: TypeInfo, Signature: TypeInfo, E: EthExtra> ExtrinsicCall
	for UncheckedExtrinsic<Address, Signature, E>
{
	type Call = CallOf<E::Config>;

	fn call(&self) -> &Self::Call {
		self.0.call()
	}

	fn into_call(self) -> Self::Call {
		self.0.into_call()
	}
}

impl<LookupSource, Signature, E, Lookup> Checkable<Lookup>
	for UncheckedExtrinsic<LookupSource, Signature, E>
where
	E: EthExtra,
	Self: Encode,
	<E::Config as frame_system::Config>::Nonce: TryFrom<U256>,
	CallOf<E::Config>: SetWeightLimit,
	// required by Checkable for `generic::UncheckedExtrinsic`
	generic::UncheckedExtrinsic<
		LookupSource,
		CallOf<E::Config>,
		Signature,
		E::ExtensionV0,
		E::ExtensionOtherVersions,
	>: Checkable<
			Lookup,
			Checked = CheckedExtrinsic<
				AccountIdOf<E::Config>,
				CallOf<E::Config>,
				E::ExtensionV0,
				E::ExtensionOtherVersions,
			>,
		>,
{
	type Checked = CheckedExtrinsic<
		AccountIdOf<E::Config>,
		CallOf<E::Config>,
		E::ExtensionV0,
		E::ExtensionOtherVersions,
	>;

	fn check(self, lookup: &Lookup) -> Result<Self::Checked, TransactionValidityError> {
		if !self.0.is_signed() {
			if let Some(crate::Call::eth_transact { payload }) = self.0.function.is_sub_type() {
				log::trace!(
					target: LOG_TARGET,
					"eth_transact substrate tx hash: 0x{}",
					sp_core::hexdisplay::HexDisplay::from(&sp_crypto_hashing::blake2_256(&self.encode())),
				);
				let checked = E::try_into_checked_extrinsic(payload, self.encoded_size())?;
				return Ok(checked);
			};
		}
		self.0.check(lookup)
	}

	#[cfg(feature = "try-runtime")]
	fn unchecked_into_checked_i_know_what_i_am_doing(
		self,
		lookup: &Lookup,
	) -> Result<Self::Checked, TransactionValidityError> {
		self.0.unchecked_into_checked_i_know_what_i_am_doing(lookup)
	}
}

impl<Address, Signature, E: EthExtra> GetDispatchInfo
	for UncheckedExtrinsic<Address, Signature, E>
{
	fn get_dispatch_info(&self) -> DispatchInfo {
		self.0.get_dispatch_info()
	}
}

impl<Address: Encode, Signature: Encode, E: EthExtra> serde::Serialize
	for UncheckedExtrinsic<Address, Signature, E>
{
	fn serialize<S>(&self, seq: S) -> Result<S::Ok, S::Error>
	where
		S: ::serde::Serializer,
	{
		self.0.serialize(seq)
	}
}

impl<'a, Address: DecodeWithMemTracking, Signature: DecodeWithMemTracking, E: EthExtra>
	serde::Deserialize<'a> for UncheckedExtrinsic<Address, Signature, E>
{
	fn deserialize<D>(de: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'a>,
	{
		let r = sp_core::bytes::deserialize(de)?;
		Decode::decode(&mut &r[..])
			.map_err(|e| serde::de::Error::custom(alloc::format!("Decode error: {}", e)))
	}
}

impl<Address, Signature, E: EthExtra> SignedTransactionBuilder
	for UncheckedExtrinsic<Address, Signature, E>
where
	Address: TypeInfo,
	Signature: TypeInfo,
	E::ExtensionV0: TypeInfo,
{
	type Address = Address;
	type Signature = Signature;
	type Extension = E::ExtensionV0;

	fn new_signed_transaction(
		call: Self::Call,
		signed: Address,
		signature: Signature,
		tx_ext: E::ExtensionV0,
	) -> Self {
		generic::UncheckedExtrinsic::new_signed(call, signed, signature, tx_ext).into()
	}
}

impl<Address, Signature, E: EthExtra> InherentBuilder for UncheckedExtrinsic<Address, Signature, E>
where
	Address: TypeInfo,
	Signature: TypeInfo,
	E::ExtensionV0: TypeInfo,
{
	fn new_inherent(call: Self::Call) -> Self {
		generic::UncheckedExtrinsic::new_bare(call).into()
	}
}

impl<Address, Signature, E: EthExtra> From<UncheckedExtrinsic<Address, Signature, E>>
	for OpaqueExtrinsic
where
	Address: Encode,
	Signature: Encode,
	E::ExtensionV0: Encode,
{
	fn from(extrinsic: UncheckedExtrinsic<Address, Signature, E>) -> Self {
		extrinsic.0.into()
	}
}

impl<Address, Signature, E: EthExtra> LazyExtrinsic for UncheckedExtrinsic<Address, Signature, E>
where
	generic::UncheckedExtrinsic<
		Address,
		CallOf<E::Config>,
		Signature,
		E::ExtensionV0,
		E::ExtensionOtherVersions,
	>: LazyExtrinsic,
{
	fn decode_unprefixed(data: &[u8]) -> Result<Self, codec::Error> {
		Ok(Self(LazyExtrinsic::decode_unprefixed(data)?))
	}
}

/// EthExtra convert an unsigned [`crate::Call::eth_transact`] into a [`CheckedExtrinsic`].
pub trait EthExtra {
	/// The Runtime configuration.
	type Config: Config + TxConfig;

	/// The Runtime's transaction extension version 0.
	/// It should include at least:
	/// - [`frame_system::CheckNonce`] to ensure that the nonce from the Ethereum transaction is
	///   correct.
	type ExtensionV0: TransactionExtension<CallOf<Self::Config>>;

	/// The Runtime's transaction extension versions other than 0.
	///
	/// Use [`sp_runtime::traits::InvalidVersion`] if no other versions should be supported.
	type ExtensionOtherVersions: Pipeline<CallOf<Self::Config>>;

	/// Get the transaction extension to apply to an unsigned [`crate::Call::eth_transact`]
	/// extrinsic.
	///
	/// # Parameters
	/// - `nonce`: The nonce extracted from the Ethereum transaction.
	/// - `tip`: The transaction tip calculated from the Ethereum transaction.
	fn get_eth_extension(
		nonce: <Self::Config as frame_system::Config>::Nonce,
		tip: BalanceOf<Self::Config>,
	) -> Self::ExtensionV0;

	/// Convert the unsigned [`crate::Call::eth_transact`] into a [`CheckedExtrinsic`].
	/// and ensure that the fees from the Ethereum transaction correspond to the fees computed from
	/// the encoded_len and the injected weight_limit.
	///
	/// # Parameters
	/// - `payload`: The RLP-encoded Ethereum transaction.
	/// - `encoded_len`: The encoded length of the extrinsic.
	fn try_into_checked_extrinsic(
		payload: &[u8],
		encoded_len: usize,
	) -> Result<
		CheckedExtrinsic<
			AccountIdOf<Self::Config>,
			CallOf<Self::Config>,
			Self::ExtensionV0,
			Self::ExtensionOtherVersions,
		>,
		InvalidTransaction,
	>
	where
		<Self::Config as frame_system::Config>::Nonce: TryFrom<U256>,
		CallOf<Self::Config>: SetWeightLimit,
	{
		let tx = TransactionSigned::decode(&payload).map_err(|err| {
			log::debug!(target: LOG_TARGET, "Failed to decode transaction: {err:?}");
			InvalidTransaction::Call
		})?;

		// Check transaction type and reject unsupported transaction types
		match &tx {
			crate::evm::api::TransactionSigned::Transaction1559Signed(_) |
			crate::evm::api::TransactionSigned::Transaction2930Signed(_) |
			crate::evm::api::TransactionSigned::TransactionLegacySigned(_) => {
				// Supported transaction types, continue processing
			},
			crate::evm::api::TransactionSigned::Transaction7702Signed(_) => {
				log::debug!(target: LOG_TARGET, "EIP-7702 transactions are not supported");
				return Err(InvalidTransaction::Call);
			},
			crate::evm::api::TransactionSigned::Transaction4844Signed(_) => {
				log::debug!(target: LOG_TARGET, "EIP-4844 transactions are not supported");
				return Err(InvalidTransaction::Call);
			},
		}

		let signer_addr = tx.recover_eth_address().map_err(|err| {
			log::debug!(target: LOG_TARGET, "Failed to recover signer: {err:?}");
			InvalidTransaction::BadProof
		})?;

		let signer = <Self::Config as Config>::AddressMapper::to_fallback_account_id(&signer_addr);
		let base_fee = <Pallet<Self::Config>>::evm_base_fee();
		let tx = GenericTransaction::from_signed(tx, base_fee, None);
		let nonce = tx.nonce.unwrap_or_default().try_into().map_err(|_| {
			log::debug!(target: LOG_TARGET, "Failed to convert nonce");
			InvalidTransaction::Call
		})?;

		log::debug!(target: LOG_TARGET, "Decoded Ethereum transaction with signer: {signer_addr:?} nonce: {nonce:?}");
		log::trace!(target: LOG_TARGET, "Decoded Ethereum transaction was: {tx:?}");
		let call_info = tx.into_call::<Self::Config>(CreateCallMode::ExtrinsicExecution(
			encoded_len as u32,
			payload.to_vec(),
		))?;
		let storage_credit = <Self::Config as Config>::Currency::withdraw(
			&signer,
			call_info.storage_deposit,
			Precision::Exact,
			Preservation::Preserve,
			Fortitude::Polite,
		).map_err(|_| {
			log::debug!(target: LOG_TARGET, "Not enough balance to hold additional storage deposit of {:?}", call_info.storage_deposit);
			InvalidTransaction::Payment
		})?;
		<Self::Config as Config>::FeeInfo::deposit_txfee(storage_credit);

		crate::tracing::if_tracing(|tracer| {
			tracer.watch_address(&Pallet::<Self::Config>::block_author());
			tracer.watch_address(&signer_addr);
		});

		log::debug!(target: LOG_TARGET, "\
			Created checked Ethereum transaction with: \
			from={signer_addr:?} \
			eth_gas={} \
			encoded_len={encoded_len} \
			tx_fee={:?} \
			storage_deposit={:?} \
			weight_limit={} \
			nonce={nonce:?}\
			",
			call_info.eth_gas_limit,
			call_info.tx_fee,
			call_info.storage_deposit,
			call_info.weight_limit,
		);

		// We can't calculate a tip because it needs to be based on the actual gas used which we
		// cannot know pre-dispatch. Hence we never supply a tip here or it would be way too high.
		Ok(CheckedExtrinsic {
			format: ExtrinsicFormat::Signed(
				signer.into(),
				Self::get_eth_extension(nonce, Zero::zero()),
			),
			function: call_info.call,
		})
	}
}

#[cfg(test)]
mod test {
	use super::*;
	use crate::{
		EthTransactInfo, RUNTIME_PALLETS_ADDR, Weight,
		evm::*,
		test_utils::*,
		tests::{
			Address, ExtBuilder, RuntimeCall, RuntimeOrigin, SignedExtra, Test, UncheckedExtrinsic,
		},
	};
	use frame_support::traits::fungible::Mutate;
	use pallet_revive_fixtures::compile_module;
	use sp_runtime::traits::{self, Checkable, DispatchTransaction, LookupError};

	type AccountIdOf<T> = <T as frame_system::Config>::AccountId;

	struct TestContext;

	impl traits::Lookup for TestContext {
		type Source = Address;
		type Target = AccountIdOf<Test>;
		fn lookup(&self, s: Self::Source) -> Result<Self::Target, LookupError> {
			match s {
				Self::Source::Id(id) => Ok(id),
				_ => Err(LookupError),
			}
		}
	}

	/// A builder for creating an unchecked extrinsic, and test that the check function works.
	#[derive(Clone)]
	struct UncheckedExtrinsicBuilder {
		tx: GenericTransaction,
		before_validate: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
		dry_run: Option<EthTransactInfo<BalanceOf<Test>>>,
	}

	impl UncheckedExtrinsicBuilder {
		/// Create a new builder with default values.
		fn new() -> Self {
			Self {
				tx: GenericTransaction {
					from: Some(Account::default().address()),
					chain_id: Some(<Test as Config>::ChainId::get().into()),
					..Default::default()
				},
				before_validate: None,
				dry_run: None,
			}
		}

		fn data(mut self, data: Vec<u8>) -> Self {
			self.tx.input = Bytes(data).into();
			self
		}

		fn fund_account(account: &Account) {
			let _ = <Test as Config>::Currency::set_balance(
				&account.substrate_account(),
				100_000_000_000_000,
			);
		}

		fn estimate_gas(&mut self) {
			let account = Account::default();
			Self::fund_account(&account);

			let dry_run =
				crate::Pallet::<Test>::dry_run_eth_transact(self.tx.clone(), None, true, None);
			self.tx.gas_price = Some(<Pallet<Test>>::evm_base_fee());

			match dry_run {
				Ok(dry_run) => {
					self.tx.gas = Some(dry_run.eth_gas);
					self.dry_run = Some(dry_run);
				},
				Err(err) => {
					log::debug!(target: LOG_TARGET, "Failed to estimate gas: {:?}", err);
				},
			}
		}

		/// Create a new builder with a call to the given address.
		fn call_with(dest: H160) -> Self {
			let mut builder = Self::new();
			builder.tx.to = Some(dest);
			builder
		}

		/// Create a new builder with an instantiate call.
		fn instantiate_with(code: Vec<u8>, data: Vec<u8>) -> Self {
			let mut builder = Self::new();
			builder.tx.input = Bytes(code.into_iter().chain(data.into_iter()).collect()).into();
			builder
		}

		/// Set before_validate function.
		fn before_validate(mut self, f: impl Fn() + Send + Sync + 'static) -> Self {
			self.before_validate = Some(std::sync::Arc::new(f));
			self
		}

		fn check(
			self,
		) -> Result<
			(u32, RuntimeCall, SignedExtra, GenericTransaction, Weight, TransactionSigned),
			TransactionValidityError,
		> {
			self.mutate_estimate_and_check(Box::new(|_| ()))
		}

		/// Call `check` on the unchecked extrinsic, and `pre_dispatch` on the signed extension.
		fn mutate_estimate_and_check(
			mut self,
			f: Box<dyn FnOnce(&mut GenericTransaction) -> ()>,
		) -> Result<
			(u32, RuntimeCall, SignedExtra, GenericTransaction, Weight, TransactionSigned),
			TransactionValidityError,
		> {
			ExtBuilder::default().build().execute_with(|| self.estimate_gas());
			ExtBuilder::default().build().execute_with(|| {
				f(&mut self.tx);
				let UncheckedExtrinsicBuilder { tx, before_validate, .. } = self.clone();

				// Fund the account.
				let account = Account::default();
				Self::fund_account(&account);

				let signed_transaction =
					account.sign_transaction(tx.clone().try_into_unsigned().unwrap());
				let call = RuntimeCall::Contracts(crate::Call::eth_transact {
					payload: signed_transaction.signed_payload().clone(),
				});

				let uxt: UncheckedExtrinsic = generic::UncheckedExtrinsic::new_bare(call).into();
				let encoded_len = uxt.encoded_size();
				let result: CheckedExtrinsic<_, _, _> = uxt.check(&TestContext {})?;
				let (account_id, extra): (AccountId32, SignedExtra) = match result.format {
					ExtrinsicFormat::Signed(signer, extra) => (signer, extra),
					_ => unreachable!(),
				};

				before_validate.map(|f| f());
				extra.clone().validate_and_prepare(
					RuntimeOrigin::signed(account_id),
					&result.function,
					&result.function.get_dispatch_info(),
					encoded_len,
					0,
				)?;

				Ok((
					encoded_len as u32,
					result.function,
					extra,
					tx,
					self.dry_run.unwrap().weight_required,
					signed_transaction,
				))
			})
		}
	}

	#[test]
	fn check_eth_transact_call_works() {
		let builder = UncheckedExtrinsicBuilder::call_with(H160::from([1u8; 20]));
		let (expected_encoded_len, call, _, tx, weight_required, signed_transaction) =
			builder.check().unwrap();
		let expected_effective_gas_price =
			ExtBuilder::default().build().execute_with(|| Pallet::<Test>::evm_base_fee());

		match call {
			RuntimeCall::Contracts(crate::Call::eth_call::<Test> {
				dest,
				value,
				weight_limit,
				data,
				transaction_encoded,
				effective_gas_price,
				encoded_len,
				..
			}) if dest == tx.to.unwrap() &&
				value == tx.value.unwrap_or_default().as_u64().into() &&
				data == tx.input.to_vec() &&
				transaction_encoded == signed_transaction.signed_payload() &&
				effective_gas_price == expected_effective_gas_price =>
			{
				assert_eq!(encoded_len, expected_encoded_len);
				assert!(
					weight_limit.all_gte(weight_required),
					"Assert failed: weight_limit={weight_limit:?} >= weight_required={weight_required:?}"
				);
			},
			_ => panic!("Call does not match."),
		}
	}

	#[test]
	fn check_eth_transact_instantiate_works() {
		let (expected_code, _) = compile_module("dummy").unwrap();
		let expected_data = vec![];
		let builder = UncheckedExtrinsicBuilder::instantiate_with(
			expected_code.clone(),
			expected_data.clone(),
		);
		let (expected_encoded_len, call, _, tx, weight_required, signed_transaction) =
			builder.check().unwrap();
		let expected_effective_gas_price =
			ExtBuilder::default().build().execute_with(|| Pallet::<Test>::evm_base_fee());
		let expected_value = tx.value.unwrap_or_default().as_u64().into();

		match call {
			RuntimeCall::Contracts(crate::Call::eth_instantiate_with_code::<Test> {
				value,
				weight_limit,
				code,
				data,
				transaction_encoded,
				effective_gas_price,
				encoded_len,
				..
			}) if value == expected_value &&
				code == expected_code &&
				data == expected_data &&
				transaction_encoded == signed_transaction.signed_payload() &&
				effective_gas_price == expected_effective_gas_price =>
			{
				assert_eq!(encoded_len, expected_encoded_len);
				assert!(
					weight_limit.all_gte(weight_required),
					"Assert failed: weight_limit={weight_limit:?} >= weight_required={weight_required:?}"
				);
			},
			_ => panic!("Call does not match."),
		}
	}

	#[test]
	fn check_eth_transact_nonce_works() {
		let builder = UncheckedExtrinsicBuilder::call_with(H160::from([1u8; 20]));

		assert_eq!(
			builder.mutate_estimate_and_check(Box::new(|tx| tx.nonce = Some(1u32.into()))),
			Err(TransactionValidityError::Invalid(InvalidTransaction::Future))
		);

		let builder =
			UncheckedExtrinsicBuilder::call_with(H160::from([1u8; 20])).before_validate(|| {
				<crate::System<Test>>::inc_account_nonce(Account::default().substrate_account());
			});

		assert_eq!(
			builder.check(),
			Err(TransactionValidityError::Invalid(InvalidTransaction::Stale))
		);
	}

	#[test]
	fn check_eth_transact_chain_id_works() {
		let builder = UncheckedExtrinsicBuilder::call_with(H160::from([1u8; 20]));

		assert_eq!(
			builder.mutate_estimate_and_check(Box::new(|tx| tx.chain_id = Some(42.into()))),
			Err(TransactionValidityError::Invalid(InvalidTransaction::Call))
		);
	}

	#[test]
	fn check_instantiate_data() {
		let code: Vec<u8> = polkavm_common::program::BLOB_MAGIC
			.into_iter()
			.chain(b"invalid code".iter().cloned())
			.collect();
		let data = vec![1];

		let builder = UncheckedExtrinsicBuilder::instantiate_with(code.clone(), data.clone());

		// Fail because the tx input fail to get the blob length
		assert_eq!(
			builder.check(),
			Err(TransactionValidityError::Invalid(InvalidTransaction::Call))
		);
	}

	#[test]
	fn check_transaction_fees() {
		let scenarios: Vec<(_, Box<dyn FnOnce(&mut GenericTransaction)>, _)> = vec![
			(
				"Eth fees too low",
				Box::new(|tx| {
					tx.gas_price = Some(100u64.into());
				}),
				InvalidTransaction::Payment,
			),
			(
				"Gas fees too low",
				Box::new(|tx| {
					tx.gas = Some(tx.gas.unwrap() / 2);
				}),
				InvalidTransaction::Payment,
			),
		];

		for (msg, update_tx, err) in scenarios {
			let res = UncheckedExtrinsicBuilder::call_with(H160::from([1u8; 20]))
				.mutate_estimate_and_check(update_tx);

			assert_eq!(res, Err(TransactionValidityError::Invalid(err)), "{}", msg);
		}
	}

	#[test]
	fn eth_pre_dispatch_weight_matches_check_weight_booking() {
		let builder = UncheckedExtrinsicBuilder::call_with(H160::from([1u8; 20]));
		let (encoded_len, call, _, _, _, signed_transaction) = builder.check().unwrap();

		ExtBuilder::default().build().execute_with(|| {
			let reported =
				Pallet::<Test>::eth_pre_dispatch_weight(signed_transaction.signed_payload())
					.unwrap();
			let info = <Test as Config>::FeeInfo::dispatch_info(&call);
			let expected = frame_system::calculate_consumed_extrinsic_weight::<CallOf<Test>>(
				&<Test as frame_system::Config>::BlockWeights::get(),
				&info,
				encoded_len as usize,
			);

			assert_eq!(reported, expected);
		});
	}

	#[test]
	fn check_transaction_tip() {
		let (code, _) = compile_module("dummy").unwrap();
		// create some dummy data to increase the gas fee
		let data = vec![42u8; crate::limits::CALLDATA_BYTES as usize];
		let (_, _, extra, _tx, _gas_required, _) =
			UncheckedExtrinsicBuilder::instantiate_with(code.clone(), data.clone())
				.mutate_estimate_and_check(Box::new(|tx| {
					tx.gas_price = Some(tx.gas_price.unwrap() * 103 / 100);
					log::debug!(target: LOG_TARGET, "Gas price: {:?}", tx.gas_price);
				}))
				.unwrap();

		assert_eq!(U256::from(extra.1.tip()), 0u32.into());
	}

	#[test]
	fn check_runtime_pallets_addr_works() {
		let remark: CallOf<Test> =
			frame_system::Call::remark { remark: b"Hello, world!".to_vec() }.into();

		let builder =
			UncheckedExtrinsicBuilder::call_with(RUNTIME_PALLETS_ADDR).data(remark.encode());
		let (_, call, _, _, _, _) = builder.check().unwrap();

		match call {
			RuntimeCall::Contracts(crate::Call::eth_substrate_call {
				call: inner_call, ..
			}) => {
				assert_eq!(*inner_call, remark);
			},
			_ => panic!("Expected the RuntimeCall::Contracts variant, got: {:?}", call),
		}
	}

	/// The raw bytes seen in this test is of a deployment transaction from [eip-2470] which publish
	/// a contract at a predicable address on any chain that it's run on. We use these bytes to test
	/// that if we were to run this transaction on pallet-revive that it would run and also produce
	/// a contract at the address described in the EIP.
	///
	/// Note: the linked EIP is not an EIP for Nick's method, it's just an EIP that makes use of
	/// Nick's method.
	///
	/// [eip-2470]: https://eips.ethereum.org/EIPS/eip-2470
	#[test]
	fn contract_deployment_with_nick_method_works() {
		// Arrange
		let raw_transaction_bytes = alloy_core::hex!(
			"0xf9016c8085174876e8008303c4d88080b90154608060405234801561001057600080fd5b50610134806100206000396000f3fe6080604052348015600f57600080fd5b506004361060285760003560e01c80634af63f0214602d575b600080fd5b60cf60048036036040811015604157600080fd5b810190602081018135640100000000811115605b57600080fd5b820183602082011115606c57600080fd5b80359060200191846001830284011164010000000083111715608d57600080fd5b91908080601f016020809104026020016040519081016040528093929190818152602001838380828437600092019190915250929550509135925060eb915050565b604080516001600160a01b039092168252519081900360200190f35b6000818351602085016000f5939250505056fea26469706673582212206b44f8a82cb6b156bfcc3dc6aadd6df4eefd204bc928a4397fd15dacf6d5320564736f6c634300060200331b83247000822470"
		);

		let mut signed_transaction = TransactionSigned::decode(raw_transaction_bytes.as_slice())
			.expect("Invalid raw transaction bytes");
		if let TransactionSigned::TransactionLegacySigned(ref mut legacy_transaction) =
			signed_transaction
		{
			legacy_transaction.transaction_legacy_unsigned.gas =
				U256::from_dec_str("3750815700000").unwrap();
		}
		let generic_transaction = GenericTransaction::from_signed(
			signed_transaction.clone(),
			ExtBuilder::default().build().execute_with(|| Pallet::<Test>::evm_base_fee()),
			None,
		);

		let unchecked_extrinsic_builder = UncheckedExtrinsicBuilder {
			tx: generic_transaction,
			before_validate: None,
			dry_run: None,
		};

		// Act
		let eth_transact_result = unchecked_extrinsic_builder.check();

		// Assert
		let (
			_encoded_len,
			_function,
			_extra,
			generic_transaction,
			_gas_required,
			_signed_transaction,
		) = eth_transact_result.expect("eth_transact failed");
		assert!(
			generic_transaction.chain_id.is_none(),
			"Chain Id in the generic transaction is not None"
		);
	}
}
