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

//! Generic implementation of an unchecked (pre-verification) extrinsic.

use crate::{
	generic::{CheckedExtrinsic, ExtrinsicFormat},
	traits::{
		self, AsTransactionAuthorizedOrigin, Checkable, DecodeWithVersion,
		DecodeWithVersionWithMemTracking, Dispatchable, ExtensionVariant, ExtrinsicCall,
		ExtrinsicLike, ExtrinsicMetadata, IdentifyAccount, InvalidVersion, LazyExtrinsic,
		MaybeDisplay, Member, Pipeline, PipelineVersion, SignaturePayload, TransactionExtension,
	},
	transaction_validity::{InvalidTransaction, TransactionValidityError},
	OpaqueExtrinsic,
};
#[cfg(all(not(feature = "std"), feature = "serde"))]
use alloc::format;
use alloc::{vec, vec::Vec};
use codec::{
	Compact, CountedInput, Decode, DecodeWithMemLimit, DecodeWithMemTracking, Encode, EncodeLike,
	Input,
};
use core::fmt::{
	Debug, {self},
};
use scale_info::{build::Fields, meta_type, Path, StaticTypeInfo, Type, TypeInfo, TypeParameter};
use sp_io::hashing::blake2_256;
use sp_weights::Weight;

/// Type to represent the version of the [Extension](TransactionExtension) used in this extrinsic.
pub type ExtensionVersion = u8;
/// Type to represent the extrinsic format version which defines an [UncheckedExtrinsic].
pub type ExtrinsicVersion = u8;

/// Current version of the [`UncheckedExtrinsic`] encoded format.
///
/// This version needs to be bumped if the encoded representation changes.
/// It ensures that if the representation is changed and the format is not known,
/// the decoding fails.
pub const EXTRINSIC_FORMAT_VERSION: ExtrinsicVersion = 5;
/// Legacy version of the [`UncheckedExtrinsic`] encoded format.
///
/// This version was used in the signed/unsigned transaction model and is still supported for
/// compatibility reasons. It will be deprecated in favor of v5 extrinsics and an inherent/general
/// transaction model.
pub const LEGACY_EXTRINSIC_FORMAT_VERSION: ExtrinsicVersion = 4;

/// Maximum decoded heap size for a runtime call (in bytes).
pub const DEFAULT_MAX_CALL_SIZE: usize = 16 * 1024 * 1024; // 16 MiB

/// The `SignaturePayload` of `UncheckedExtrinsic`.
pub type UncheckedSignaturePayload<Address, Signature, Extension> = (Address, Signature, Extension);

impl<Address: TypeInfo, Signature: TypeInfo, Extension: TypeInfo> SignaturePayload
	for UncheckedSignaturePayload<Address, Signature, Extension>
{
	type SignatureAddress = Address;
	type Signature = Signature;
	type SignatureExtra = Extension;
}

/// A "header" for extrinsics leading up to the call itself. Determines the type of extrinsic and
/// holds any necessary specialized data.
///
/// Bare extrinsics and signed extrinsics are extended with the transaction extension version 0,
/// specified by the generic parameter `ExtensionV0`.
///
/// General extrinsics support multiple transaction extension version, specified by both
/// `ExtensionV0` and `ExtensionOtherVersions`, by default `ExtensionOtherVersions` is set to
/// invalid version, making `ExtensionV0` the only supported version. If you want to support more
/// versions, you need to specify the `ExtensionOtherVersions` parameter.
#[derive(Eq, PartialEq, Clone)]
pub enum Preamble<Address, Signature, ExtensionV0, ExtensionOtherVersions = InvalidVersion> {
	/// An extrinsic without a signature or any extension. This means it's either an inherent or
	/// an old-school "Unsigned" (we don't use that terminology any more since it's confusable with
	/// the general transaction which is without a signature but does have an extension).
	///
	/// NOTE: In the future, once we remove `ValidateUnsigned`, this will only serve Inherent
	/// extrinsics and thus can be renamed to `Inherent`.
	Bare(ExtrinsicVersion),
	/// An old-school transaction extrinsic which includes a signature of some hard-coded crypto.
	/// Available only on extrinsic version 4.
	Signed(Address, Signature, ExtensionV0),
	/// A new-school transaction extrinsic which does not include a signature by default. The
	/// origin authorization, through signatures or other means, is performed by the transaction
	/// extension in this extrinsic. Available starting with extrinsic version 5.
	General(ExtensionVariant<ExtensionV0, ExtensionOtherVersions>),
}

const VERSION_MASK: u8 = 0b0011_1111;
const TYPE_MASK: u8 = 0b1100_0000;
const BARE_EXTRINSIC: u8 = 0b0000_0000;
const SIGNED_EXTRINSIC: u8 = 0b1000_0000;
const GENERAL_EXTRINSIC: u8 = 0b0100_0000;

impl<Address, Signature, ExtensionV0, ExtensionOtherVersions> Decode
	for Preamble<Address, Signature, ExtensionV0, ExtensionOtherVersions>
where
	Address: Decode,
	Signature: Decode,
	ExtensionV0: Decode,
	ExtensionOtherVersions: DecodeWithVersion,
{
	fn decode<I: Input>(input: &mut I) -> Result<Self, codec::Error> {
		let version_and_type = input.read_byte()?;

		let version = version_and_type & VERSION_MASK;
		let xt_type = version_and_type & TYPE_MASK;

		let preamble = match (version, xt_type) {
			(
				extrinsic_version @ LEGACY_EXTRINSIC_FORMAT_VERSION..=EXTRINSIC_FORMAT_VERSION,
				BARE_EXTRINSIC,
			) => Self::Bare(extrinsic_version),
			(LEGACY_EXTRINSIC_FORMAT_VERSION, SIGNED_EXTRINSIC) => {
				let address = Address::decode(input)?;
				let signature = Signature::decode(input)?;
				let ext = ExtensionV0::decode(input)?;
				Self::Signed(address, signature, ext)
			},
			(EXTRINSIC_FORMAT_VERSION, GENERAL_EXTRINSIC) => {
				let ext_version = ExtensionVersion::decode(input)?;
				let ext =
					ExtensionVariant::<ExtensionV0, ExtensionOtherVersions>::decode_with_version(
						ext_version,
						input,
					)?;
				Self::General(ext)
			},
			(_, _) => return Err("Invalid transaction version".into()),
		};

		Ok(preamble)
	}
}

impl<Address, Signature, ExtensionV0, ExtensionOtherVersions> DecodeWithMemTracking
	for Preamble<Address, Signature, ExtensionV0, ExtensionOtherVersions>
where
	Address: DecodeWithMemTracking,
	Signature: DecodeWithMemTracking,
	ExtensionV0: DecodeWithMemTracking,
	ExtensionOtherVersions: DecodeWithVersionWithMemTracking,
{
}

impl<Address, Signature, ExtensionV0, ExtensionOtherVersions> Encode
	for Preamble<Address, Signature, ExtensionV0, ExtensionOtherVersions>
where
	Address: Encode,
	Signature: Encode,
	ExtensionV0: Encode,
	ExtensionOtherVersions: Encode + PipelineVersion,
{
	fn size_hint(&self) -> usize {
		match &self {
			Preamble::Bare(_) => EXTRINSIC_FORMAT_VERSION.size_hint(),
			Preamble::Signed(address, signature, ext) => LEGACY_EXTRINSIC_FORMAT_VERSION
				.size_hint()
				.saturating_add(address.size_hint())
				.saturating_add(signature.size_hint())
				.saturating_add(ext.size_hint()),
			Preamble::General(ext) => EXTRINSIC_FORMAT_VERSION
				.size_hint()
				.saturating_add(0u8.size_hint()) // version
				.saturating_add(ext.size_hint()),
		}
	}

	fn encode_to<T: codec::Output + ?Sized>(&self, dest: &mut T) {
		match &self {
			Preamble::Bare(extrinsic_version) => {
				(extrinsic_version | BARE_EXTRINSIC).encode_to(dest);
			},
			Preamble::Signed(address, signature, ext) => {
				(LEGACY_EXTRINSIC_FORMAT_VERSION | SIGNED_EXTRINSIC).encode_to(dest);
				address.encode_to(dest);
				signature.encode_to(dest);
				ext.encode_to(dest);
			},
			Preamble::General(ext) => {
				(EXTRINSIC_FORMAT_VERSION | GENERAL_EXTRINSIC).encode_to(dest);
				ext.version().encode_to(dest);
				ext.encode_to(dest);
			},
		}
	}
}

impl<Address, Signature, ExtensionV0, ExtensionOtherVersions>
	Preamble<Address, Signature, ExtensionV0, ExtensionOtherVersions>
{
	/// Returns `Some` if this is a signed extrinsic, together with the relevant inner fields.
	pub fn to_signed(self) -> Option<(Address, Signature, ExtensionV0)> {
		match self {
			Self::Signed(a, s, e) => Some((a, s, e)),
			_ => None,
		}
	}
}

impl<Address, Signature, ExtensionV0, ExtensionOtherVersions> fmt::Debug
	for Preamble<Address, Signature, ExtensionV0, ExtensionOtherVersions>
where
	Address: fmt::Debug,
	ExtensionV0: fmt::Debug,
	ExtensionOtherVersions: fmt::Debug,
{
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		match self {
			Self::Bare(_) => write!(f, "Bare"),
			Self::Signed(address, _, tx_ext) => write!(f, "Signed({:?}, {:?})", address, tx_ext),
			Self::General(tx_ext) => write!(f, "General({:?})", tx_ext),
		}
	}
}

/// An extrinsic right from the external world. This is unchecked and so can contain a signature.
///
/// An extrinsic is formally described as any external data that is originating from the outside of
/// the runtime and fed into the runtime as a part of the block-body.
///
/// Prerequisite concepts: Inherents vs Transactions
/// * Inherents are special types of extrinsics that are placed into the block by the block-builder.
///   They are unsigned because the assertion is that they are "inherently true" by virtue of
///   getting past all validators.
/// * Transactions are all other statements provided by external entities that the chain deems
///   values and decided to include in the block. This value is typically in the form of fee
///   payment, but it could in principle be any other interaction. Transactions are either signed or
///   unsigned. A sensible transaction pool should ensure that only transactions that are worthwhile
///   are considered for block-building.
///
/// Types of extrinsics:
/// - **Bare**: An extrinsic without a signature or any additional data. After being checked, bare
///   extrinsics can be applied, the apply logic is extended with `ExtensionV0`.
///   Bare extrinsics are typically used for inherent extrinsics.
///   Bare extrinsics are also used for some unsigned transactions, this feature will be deprecated,
///   see [transaction horizon](https://github.com/paritytech/polkadot-sdk/issues/2415).
/// - **Signed**: An extrinsic with a signature and extended with transaction extension
///   `ExtensionV0`. (transaction extension: [`TransactionExtension`]).
/// - **General**: An extrinsic extended with a versioned transaction extension. The transaction
///   extension is specified by both `ExtensionV0` and `ExtensionOtherVersions`. By default,
///   `ExtensionOtherVersions` is set to invalid version, making `ExtensionV0` the only supported
///   version. General transaction is a generalization of signed transaction that doesn't hardcode a
///   signature, instead signature is to be set and checked by a transaction extension.
///
/// #[cfg_attr(all(feature = "std", not(windows)), doc = simple_mermaid::mermaid!("../../docs/mermaid/extrinsics.mmd"))]
///
/// This type is by no means enforced within Substrate, but given its genericness, it is highly
/// likely that for most use-cases it will suffice. Thus, the encoding of this type will dictate
/// exactly what bytes should be sent to a runtime to transact with it.
///
/// This can be checked using [`Checkable`], yielding a [`CheckedExtrinsic`], which is the
/// counterpart of this type after its signature (and other non-negotiable validity checks) have
/// passed.
///
/// # Usage:
///
/// Usage with multiple versions for general transaction.
/// ```
/// use sp_runtime::{
///     generic::UncheckedExtrinsic,
///     traits::{MultiVersion, PipelineAtVers},
/// };
///
/// struct Signature; // Some signature scheme.
/// struct Call; // Some call type.
/// struct AccountId; // Type for account identifier that can sign a transaction.
///
/// // Some implementation of `TransactionExtension`.
/// struct PaymentExt;
/// struct PaymentV2Ext;
/// struct SigV2Ext;
/// struct NonceExt;
/// struct NonceV2Ext;
///
/// // Definition of the extrinsic.
/// type ExtensionV0 = (SigV2Ext, NonceExt, PaymentExt);
/// type ExtensionV1 = PipelineAtVers<1, (SigV2Ext, NonceExt, PaymentV2Ext)>;
/// type ExtensionV2 = PipelineAtVers<2, (SigV2Ext, NonceV2Ext, PaymentV2Ext)>;
///
/// type OtherVersions = MultiVersion<ExtensionV1, ExtensionV2>;
///
/// type Extrinsic = UncheckedExtrinsic<
///     AccountId,
///     Call,
///     Signature,
///     ExtensionV0,
///     OtherVersions,
/// >;
/// ```
#[derive(DecodeWithMemTracking, Eq, Clone)]
#[codec(decode_with_mem_tracking_bound(
	Address: DecodeWithMemTracking,
	Call: DecodeWithMemTracking,
	Signature: DecodeWithMemTracking,
	ExtensionV0: DecodeWithMemTracking,
	ExtensionOtherVersions: DecodeWithVersionWithMemTracking,
))]
pub struct UncheckedExtrinsic<
	Address,
	Call,
	Signature,
	ExtensionV0,
	ExtensionOtherVersions = InvalidVersion,
	const MAX_CALL_SIZE: usize = DEFAULT_MAX_CALL_SIZE,
> {
	/// Information regarding the type of extrinsic this is (inherent or transaction) as well as
	/// associated extension (`Extension`) data if it's a transaction and a possible signature.
	pub preamble: Preamble<Address, Signature, ExtensionV0, ExtensionOtherVersions>,
	/// The function that should be called.
	pub function: Call,
	/// Stores the raw encoded call.
	///
	/// This is mainly interesting if this extrinsic was created by decoding it from bytes. In this
	/// case this field should be set to `Some` holding the original bytes used to decode the
	/// [`Self::function`]. This is done to protect against decode implementations of `Call` that
	/// are not bijective (encodes to the exact same bytes it was encoded from). If this `field`
	/// is set, it is being used when re-encoding this transaction.
	pub encoded_call: Option<Vec<u8>>,
	/// The encoded length of this extrinsic.
	///
	/// Used internally to optimize some allocations, should be set to `None` if not known.
	#[codec(skip)]
	pub encoded_len: Option<usize>,
}

impl<
		Address: Debug,
		Call: Debug,
		Signature: Debug,
		ExtensionV0: Debug,
		ExtensionOtherVersions: Debug,
		const MAX_CALL_SIZE: usize,
	> Debug
	for UncheckedExtrinsic<
		Address,
		Call,
		Signature,
		ExtensionV0,
		ExtensionOtherVersions,
		MAX_CALL_SIZE,
	>
{
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		let mut debug_struct = f.debug_struct("UncheckedExtrinsic");

		debug_struct.field("preamble", &self.preamble);

		// Given our current allocator, we can at maximum allocate 32MiB and this is not enough for
		// big transactions.
		if self.encoded_len.unwrap_or_default() < 1024 * 1024 {
			debug_struct.field("function", &self.function)
		} else {
			debug_struct.field("function", &"Too big to be printed from the runtime")
		}
		.finish()
	}
}

impl<
		Address: PartialEq,
		Call: PartialEq,
		Signature: PartialEq,
		ExtensionV0: PartialEq,
		ExtensionOtherVersions: PartialEq,
		const MAX_CALL_SIZE: usize,
	> PartialEq
	for UncheckedExtrinsic<
		Address,
		Call,
		Signature,
		ExtensionV0,
		ExtensionOtherVersions,
		MAX_CALL_SIZE,
	>
{
	fn eq(&self, other: &Self) -> bool {
		self.preamble == other.preamble && self.function == other.function
	}
}

/// Manual [`TypeInfo`] implementation because of custom encoding. The data is a valid encoded
/// `Vec<u8>`, but requires some logic to extract the signature and payload.
///
/// See [`UncheckedExtrinsic::encode`] and [`UncheckedExtrinsic::decode`].
impl<Address, Call, Signature, ExtensionV0, const MAX_CALL_SIZE: usize, ExtensionOtherVersions>
	TypeInfo
	for UncheckedExtrinsic<
		Address,
		Call,
		Signature,
		ExtensionV0,
		ExtensionOtherVersions,
		MAX_CALL_SIZE,
	>
where
	Address: StaticTypeInfo,
	Call: StaticTypeInfo,
	Signature: StaticTypeInfo,
	ExtensionV0: StaticTypeInfo,
	ExtensionOtherVersions: StaticTypeInfo,
{
	type Identity = UncheckedExtrinsic<
		Address,
		Call,
		Signature,
		ExtensionV0,
		ExtensionOtherVersions,
		MAX_CALL_SIZE,
	>;

	fn type_info() -> Type {
		Type::builder()
			.path(Path::new("UncheckedExtrinsic", module_path!()))
			// Include the type parameter types, even though they are not used directly in any of
			// the described fields. These type definitions can be used by downstream consumers
			// to help construct the custom decoding from the opaque bytes (see below).
			.type_params(vec![
				TypeParameter::new("Address", Some(meta_type::<Address>())),
				TypeParameter::new("Call", Some(meta_type::<Call>())),
				TypeParameter::new("Signature", Some(meta_type::<Signature>())),
				TypeParameter::new("Extra", Some(meta_type::<ExtensionV0>())),
				// NOTE: We avoid adding `ExtensionOtherVersions` in the type parameter type info
				// because they break tools like subxt that hard-coded the number of type
				// parameters. Also in general type parameters are not useful, information related
				// to transaction extension are available in the metadata like for v16 the fields
				// `transaction_extensions_by_version` and `transaction_extensions`.
			])
			.docs(&["UncheckedExtrinsic raw bytes, requires custom decoding routine"])
			// Because of the custom encoding, we can only accurately describe the encoding as an
			// opaque `Vec<u8>`. Downstream consumers will need to manually implement the codec to
			// encode/decode the `signature` and `function` fields.
			.composite(Fields::unnamed().field(|f| f.ty::<Vec<u8>>()))
	}
}

impl<Address, Call, Signature, ExtensionV0, ExtensionOtherVersions, const MAX_CALL_SIZE: usize>
	UncheckedExtrinsic<Address, Call, Signature, ExtensionV0, ExtensionOtherVersions, MAX_CALL_SIZE>
{
	/// New instance of a bare (ne unsigned) extrinsic. This could be used for an inherent or an
	/// old-school "unsigned transaction" (which are new being deprecated in favour of general
	/// transactions).
	#[deprecated = "Use new_bare instead"]
	pub fn new_unsigned(function: Call) -> Self {
		Self::new_bare(function)
	}

	/// Returns `true` if this extrinsic instance is an inherent, `false`` otherwise.
	pub fn is_inherent(&self) -> bool {
		matches!(self.preamble, Preamble::Bare(_))
	}

	/// Returns `true` if this extrinsic instance is an old-school signed transaction, `false`
	/// otherwise.
	pub fn is_signed(&self) -> bool {
		matches!(self.preamble, Preamble::Signed(..))
	}

	/// Create an `UncheckedExtrinsic` from a `Preamble` and the actual `Call`.
	pub fn from_parts(
		function: Call,
		preamble: Preamble<Address, Signature, ExtensionV0, ExtensionOtherVersions>,
	) -> Self {
		Self { preamble, function, encoded_call: None, encoded_len: None }
	}

	/// New instance of a bare (ne unsigned) extrinsic.
	pub fn new_bare(function: Call) -> Self {
		Self::from_parts(function, Preamble::Bare(EXTRINSIC_FORMAT_VERSION))
	}

	/// New instance of a bare (ne unsigned) extrinsic on extrinsic format version 4.
	pub fn new_bare_legacy(function: Call) -> Self {
		Self::from_parts(function, Preamble::Bare(LEGACY_EXTRINSIC_FORMAT_VERSION))
	}

	/// New instance of an old-school signed transaction on extrinsic format version 4.
	pub fn new_signed(
		function: Call,
		signed: Address,
		signature: Signature,
		tx_ext: ExtensionV0,
	) -> Self {
		Self::from_parts(function, Preamble::Signed(signed, signature, tx_ext))
	}

	/// New instance of a new-school unsigned transaction.
	pub fn new_transaction(function: Call, tx_ext: ExtensionV0) -> Self {
		Self::from_parts(function, Preamble::General(ExtensionVariant::V0(tx_ext)))
	}

	fn decode_with_len<I: Input>(input: &mut I, len: usize) -> Result<Self, codec::Error>
	where
		Preamble<Address, Signature, ExtensionV0, ExtensionOtherVersions>: DecodeWithMemTracking,
		Call: DecodeWithMemTracking,
	{
		let mut input = CountedInput::new(input);

		// Decode the preamble using the same memory limit as we will use for the `call`.
		// The preamble is not that big, but we play it safe.
		let preamble =
			DecodeWithMemLimit::decode_with_mem_limit(&mut input, MAX_CALL_SIZE.saturating_add(1))?;

		struct CloneBytes<'a, I>(&'a mut I, Vec<u8>);
		impl<I: Input> Input for CloneBytes<'_, I> {
			fn remaining_len(&mut self) -> Result<Option<usize>, codec::Error> {
				self.0.remaining_len()
			}

			fn read(&mut self, into: &mut [u8]) -> Result<(), codec::Error> {
				self.0.read(into)?;

				self.1.extend_from_slice(into);
				Ok(())
			}

			fn descend_ref(&mut self) -> Result<(), codec::Error> {
				self.0.descend_ref()
			}

			fn ascend_ref(&mut self) {
				self.0.ascend_ref();
			}

			fn on_before_alloc_mem(&mut self, size: usize) -> Result<(), codec::Error> {
				self.0.on_before_alloc_mem(size)
			}
		}

		let mut clone_bytes = CloneBytes(&mut input, Vec::with_capacity(len));

		// Adds 1 byte to the `MAX_CALL_SIZE` as the decoding fails exactly at the given value and
		// the maximum should be allowed to fit in.
		let function =
			Call::decode_with_mem_limit(&mut clone_bytes, MAX_CALL_SIZE.saturating_add(1))?;

		let encoded_call = Some(clone_bytes.1);

		if input.count() != len as u64 {
			return Err("Invalid length prefix".into());
		}

		Ok(Self { preamble, function, encoded_call, encoded_len: Some(len) })
	}

	fn encode_without_prefix(&self) -> Vec<u8>
	where
		Preamble<Address, Signature, ExtensionV0, ExtensionOtherVersions>: Encode,
		Call: Encode,
	{
		let mut encoded = self.preamble.encode();

		match &self.encoded_call {
			Some(call) => {
				encoded.extend(call);
			},
			None => {
				self.function.encode_to(&mut encoded);
			},
		}

		encoded
	}
}

impl<Address, Call, Signature, ExtensionV0, const MAX_CALL_SIZE: usize, ExtensionOtherVersions>
	ExtrinsicLike
	for UncheckedExtrinsic<
		Address,
		Call,
		Signature,
		ExtensionV0,
		ExtensionOtherVersions,
		MAX_CALL_SIZE,
	>
{
	fn is_signed(&self) -> Option<bool> {
		Some(matches!(self.preamble, Preamble::Signed(..)))
	}

	fn is_bare(&self) -> bool {
		matches!(self.preamble, Preamble::Bare(_))
	}
}

impl<Address, Call, Signature, ExtensionV0, const MAX_CALL_SIZE: usize, ExtensionOtherVersions>
	ExtrinsicCall
	for UncheckedExtrinsic<
		Address,
		Call,
		Signature,
		ExtensionV0,
		ExtensionOtherVersions,
		MAX_CALL_SIZE,
	>
{
	type Call = Call;

	fn call(&self) -> &Call {
		&self.function
	}

	fn into_call(self) -> Self::Call {
		self.function
	}
}

// TODO: Migrate existing extension pipelines to support current `Signed` transactions as `General`
// transactions by adding an extension to validate signatures, as they are currently validated in
// the `Checkable` implementation for `Signed` transactions.

impl<
		LookupSource,
		AccountId,
		Call,
		Signature,
		ExtensionV0,
		const MAX_CALL_SIZE: usize,
		ExtensionOtherVersions,
		Lookup,
	> Checkable<Lookup>
	for UncheckedExtrinsic<
		LookupSource,
		Call,
		Signature,
		ExtensionV0,
		ExtensionOtherVersions,
		MAX_CALL_SIZE,
	>
where
	LookupSource: Member + MaybeDisplay,
	Call: Encode + Member + Dispatchable,
	Signature: Member + traits::Verify,
	<Signature as traits::Verify>::Signer: IdentifyAccount<AccountId = AccountId>,
	ExtensionV0: Encode + TransactionExtension<Call>,
	AccountId: Member + MaybeDisplay,
	Lookup: traits::Lookup<Source = LookupSource, Target = AccountId>,
{
	type Checked = CheckedExtrinsic<AccountId, Call, ExtensionV0, ExtensionOtherVersions>;

	fn check(self, lookup: &Lookup) -> Result<Self::Checked, TransactionValidityError> {
		Ok(match self.preamble {
			Preamble::Signed(signed, signature, tx_ext) => {
				let signed = lookup.lookup(signed)?;
				// The `Implicit` is "implicitly" included in the payload.
				let raw_payload = SignedPayload::new(
					CallAndMaybeEncoded { encoded: self.encoded_call, call: self.function },
					tx_ext,
				)?;

				let mut payload = Vec::with_capacity(self.encoded_len.unwrap_or_default());
				raw_payload.0.encode_to(&mut payload);

				let payload =
					if payload.len() > 256 { blake2_256(&payload).to_vec() } else { payload };

				if !signature.verify(payload.as_ref(), &signed) {
					return Err(InvalidTransaction::BadProof.into());
				}

				let (function, tx_ext, _) = raw_payload.deconstruct();
				CheckedExtrinsic { format: ExtrinsicFormat::Signed(signed, tx_ext), function }
			},
			Preamble::General(tx_ext) => CheckedExtrinsic {
				format: ExtrinsicFormat::General(tx_ext),
				function: self.function,
			},
			Preamble::Bare(_) => {
				CheckedExtrinsic { format: ExtrinsicFormat::Bare, function: self.function }
			},
		})
	}

	#[cfg(feature = "try-runtime")]
	fn unchecked_into_checked_i_know_what_i_am_doing(
		self,
		lookup: &Lookup,
	) -> Result<Self::Checked, TransactionValidityError> {
		Ok(match self.preamble {
			Preamble::Signed(signed, _, tx_ext) => {
				let signed = lookup.lookup(signed)?;
				CheckedExtrinsic {
					format: ExtrinsicFormat::Signed(signed, tx_ext),
					function: self.function,
				}
			},
			Preamble::General(tx_ext) => CheckedExtrinsic {
				format: ExtrinsicFormat::General(tx_ext),
				function: self.function,
			},
			Preamble::Bare(_) => {
				CheckedExtrinsic { format: ExtrinsicFormat::Bare, function: self.function }
			},
		})
	}
}

impl<
		Address,
		Call: Dispatchable,
		Signature,
		ExtensionV0,
		const MAX_CALL_SIZE: usize,
		ExtensionOtherVersions,
	> ExtrinsicMetadata
	for UncheckedExtrinsic<
		Address,
		Call,
		Signature,
		ExtensionV0,
		ExtensionOtherVersions,
		MAX_CALL_SIZE,
	>
{
	const VERSIONS: &'static [u8] = &[LEGACY_EXTRINSIC_FORMAT_VERSION, EXTRINSIC_FORMAT_VERSION];
	type TransactionExtensionPipelines = ExtensionVariant<ExtensionV0, ExtensionOtherVersions>;
}

impl<Address, Call, Signature, ExtensionV0, const MAX_CALL_SIZE: usize, ExtensionOtherVersions>
	UncheckedExtrinsic<Address, Call, Signature, ExtensionV0, ExtensionOtherVersions, MAX_CALL_SIZE>
where
	Call: Dispatchable + Encode,
	ExtensionV0: TransactionExtension<Call>,
	ExtensionOtherVersions: Pipeline<Call>,
	<Call as Dispatchable>::RuntimeOrigin: AsTransactionAuthorizedOrigin,
{
	/// Returns the weight of the extension of this transaction, if present. If the transaction
	/// doesn't use any extension, the weight returned is equal to zero.
	pub fn extension_weight(&self) -> Weight {
		match &self.preamble {
			Preamble::Bare(_) => Weight::zero(),
			Preamble::Signed(_, _, ext) => ext.weight(&self.function),
			Preamble::General(ext) => ext.weight(&self.function),
		}
	}
}

impl<Address, Call, Signature, ExtensionV0, ExtensionOtherVersions, const MAX_CALL_SIZE: usize>
	Decode
	for UncheckedExtrinsic<
		Address,
		Call,
		Signature,
		ExtensionV0,
		ExtensionOtherVersions,
		MAX_CALL_SIZE,
	>
where
	Address: DecodeWithMemTracking,
	Signature: DecodeWithMemTracking,
	Call: DecodeWithMemTracking,
	ExtensionV0: DecodeWithMemTracking,
	ExtensionOtherVersions: DecodeWithVersionWithMemTracking,
{
	fn decode<I: Input>(input: &mut I) -> Result<Self, codec::Error> {
		// This is a little more complicated than usual since the binary format must be compatible
		// with SCALE's generic `Vec<u8>` type. Basically this just means accepting that there
		// will be a prefix of vector length.
		let expected_length: Compact<u32> = Decode::decode(input)?;

		Self::decode_with_len(input, expected_length.0 as usize)
	}
}

#[docify::export(unchecked_extrinsic_encode_impl)]
impl<Address, Call, Signature, ExtensionV0, const MAX_CALL_SIZE: usize, ExtensionOtherVersions>
	Encode
	for UncheckedExtrinsic<
		Address,
		Call,
		Signature,
		ExtensionV0,
		ExtensionOtherVersions,
		MAX_CALL_SIZE,
	>
where
	Preamble<Address, Signature, ExtensionV0, ExtensionOtherVersions>: Encode,
	Call: Encode,
	ExtensionV0: Encode,
	ExtensionOtherVersions: Encode,
{
	fn encode(&self) -> Vec<u8> {
		let tmp = self.encode_without_prefix();

		let compact_len = codec::Compact::<u32>(tmp.len() as u32);

		// Allocate the output buffer with the correct length
		let mut output = Vec::with_capacity(compact_len.size_hint() + tmp.len());

		compact_len.encode_to(&mut output);
		output.extend(tmp);

		output
	}
}

impl<Address, Call, Signature, ExtensionV0, const MAX_CALL_SIZE: usize, ExtensionOtherVersions>
	EncodeLike
	for UncheckedExtrinsic<
		Address,
		Call,
		Signature,
		ExtensionV0,
		ExtensionOtherVersions,
		MAX_CALL_SIZE,
	>
where
	Self: Encode,
{
}

#[cfg(feature = "serde")]
impl<Address, Signature, Call, ExtensionV0, const MAX_CALL_SIZE: usize, ExtensionOtherVersions>
	serde::Serialize
	for UncheckedExtrinsic<
		Address,
		Call,
		Signature,
		ExtensionV0,
		ExtensionOtherVersions,
		MAX_CALL_SIZE,
	>
where
	Address: Encode,
	Signature: Encode,
	Call: Encode,
	ExtensionV0: Encode,
	ExtensionOtherVersions: Encode + PipelineVersion,
{
	fn serialize<S>(&self, seq: S) -> Result<S::Ok, S::Error>
	where
		S: ::serde::Serializer,
	{
		self.using_encoded(|bytes| seq.serialize_bytes(bytes))
	}
}

#[cfg(feature = "serde")]
impl<
		'a,
		Address,
		Signature,
		Call,
		ExtensionV0,
		const MAX_CALL_SIZE: usize,
		ExtensionOtherVersions,
	> serde::Deserialize<'a>
	for UncheckedExtrinsic<
		Address,
		Call,
		Signature,
		ExtensionV0,
		ExtensionOtherVersions,
		MAX_CALL_SIZE,
	>
where
	Address: DecodeWithMemTracking,
	Signature: DecodeWithMemTracking,
	Call: DecodeWithMemTracking,
	ExtensionV0: DecodeWithMemTracking,
	ExtensionOtherVersions: DecodeWithVersionWithMemTracking,
{
	fn deserialize<D>(de: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'a>,
	{
		let r = sp_core::bytes::deserialize(de)?;
		Self::decode(&mut &r[..])
			.map_err(|e| serde::de::Error::custom(format!("Decode error: {}", e)))
	}
}

/// Something which holds the actual call and maybe its encoded form.
pub struct CallAndMaybeEncoded<T> {
	encoded: Option<Vec<u8>>,
	call: T,
}

impl<T> CallAndMaybeEncoded<T> {
	/// Converts `self` into the underlying call.
	pub fn into_call(self) -> T {
		self.call
	}
}

impl<T> From<T> for CallAndMaybeEncoded<T> {
	fn from(value: T) -> Self {
		Self { call: value, encoded: None }
	}
}

impl<T: Encode> Encode for CallAndMaybeEncoded<T> {
	fn encode_to<O: codec::Output + ?Sized>(&self, dest: &mut O) {
		match &self.encoded {
			Some(enc) => dest.write(enc),
			None => self.call.encode_to(dest),
		}
	}

	fn using_encoded<R, F: FnOnce(&[u8]) -> R>(&self, f: F) -> R {
		match &self.encoded {
			Some(enc) => f(&enc),
			None => self.call.using_encoded(f),
		}
	}
}

/// A payload that has been signed for an unchecked extrinsics.
///
/// Note that the payload that we sign to produce unchecked extrinsic signature
/// is going to be different than the `SignaturePayload` - so the thing the extrinsic
/// actually contains.
pub struct SignedPayload<Call: Dispatchable, ExtensionV0: TransactionExtension<Call>>(
	(CallAndMaybeEncoded<Call>, ExtensionV0, ExtensionV0::Implicit),
);

impl<Call, ExtensionV0> SignedPayload<Call, ExtensionV0>
where
	Call: Encode + Dispatchable,
	ExtensionV0: TransactionExtension<Call>,
{
	/// Create new `SignedPayload` for extrinsic format version 4.
	///
	/// This function may fail if `implicit` of `ExtensionV0` is not available.
	pub fn new(
		call: impl Into<CallAndMaybeEncoded<Call>>,
		tx_ext: ExtensionV0,
	) -> Result<Self, TransactionValidityError> {
		let implicit = ExtensionV0::implicit(&tx_ext)?;
		Ok(Self((call.into(), tx_ext, implicit)))
	}

	/// Create new `SignedPayload` from raw components.
	pub fn from_raw(
		call: impl Into<CallAndMaybeEncoded<Call>>,
		tx_ext: ExtensionV0,
		implicit: ExtensionV0::Implicit,
	) -> Self {
		Self((call.into(), tx_ext, implicit))
	}

	/// Deconstruct the payload into it's components.
	pub fn deconstruct(self) -> (Call, ExtensionV0, ExtensionV0::Implicit) {
		let (call, ext, implicit) = self.0;
		(call.call, ext, implicit)
	}
}

impl<Call, ExtensionV0> Encode for SignedPayload<Call, ExtensionV0>
where
	Call: Encode + Dispatchable,
	ExtensionV0: TransactionExtension<Call>,
{
	/// Get an encoded version of this `blake2_256`-hashed payload.
	fn using_encoded<R, F: FnOnce(&[u8]) -> R>(&self, f: F) -> R {
		self.0.using_encoded(|payload| {
			if payload.len() > 256 {
				f(&blake2_256(payload)[..])
			} else {
				f(payload)
			}
		})
	}
}

impl<Call, ExtensionV0> EncodeLike for SignedPayload<Call, ExtensionV0>
where
	Call: Encode + Dispatchable,
	ExtensionV0: TransactionExtension<Call>,
{
}

impl<Address, Call, Signature, ExtensionV0, const MAX_CALL_SIZE: usize, ExtensionOtherVersions>
	From<
		UncheckedExtrinsic<
			Address,
			Call,
			Signature,
			ExtensionV0,
			ExtensionOtherVersions,
			MAX_CALL_SIZE,
		>,
	> for OpaqueExtrinsic
where
	Preamble<Address, Signature, ExtensionV0, ExtensionOtherVersions>: Encode,
	Call: Encode,
	ExtensionV0: Encode,
	ExtensionOtherVersions: Encode + PipelineVersion,
{
	fn from(
		extrinsic: UncheckedExtrinsic<
			Address,
			Call,
			Signature,
			ExtensionV0,
			ExtensionOtherVersions,
			MAX_CALL_SIZE,
		>,
	) -> Self {
		Self::from_blob(extrinsic.encode_without_prefix())
	}
}

impl<Address, Call, Signature, ExtensionV0, ExtensionOtherVersions, const MAX_CALL_SIZE: usize>
	LazyExtrinsic
	for UncheckedExtrinsic<
		Address,
		Call,
		Signature,
		ExtensionV0,
		ExtensionOtherVersions,
		MAX_CALL_SIZE,
	>
where
	Preamble<Address, Signature, ExtensionV0, ExtensionOtherVersions>: DecodeWithMemTracking,
	Call: DecodeWithMemTracking,
	ExtensionV0: DecodeWithMemTracking,
	ExtensionOtherVersions: DecodeWithVersionWithMemTracking + PipelineVersion,
{
	fn decode_unprefixed(data: &[u8]) -> Result<Self, codec::Error> {
		Self::decode_with_len(&mut &data[..], data.len())
	}
}

#[cfg(test)]
mod legacy {
	use codec::{Compact, Decode, Encode, EncodeLike, Error, Input};
	use scale_info::{
		build::Fields, meta_type, Path, StaticTypeInfo, Type, TypeInfo, TypeParameter,
	};

	pub type UncheckedSignaturePayloadV4<Address, Signature, Extra> = (Address, Signature, Extra);

	#[derive(PartialEq, Eq, Clone, Debug)]
	pub struct UncheckedExtrinsicV4<Address, Call, Signature, Extra> {
		pub signature: Option<UncheckedSignaturePayloadV4<Address, Signature, Extra>>,
		pub function: Call,
	}

	impl<Address, Call, Signature, Extra> TypeInfo
		for UncheckedExtrinsicV4<Address, Call, Signature, Extra>
	where
		Address: StaticTypeInfo,
		Call: StaticTypeInfo,
		Signature: StaticTypeInfo,
		Extra: StaticTypeInfo,
	{
		type Identity = UncheckedExtrinsicV4<Address, Call, Signature, Extra>;

		fn type_info() -> Type {
			Type::builder()
				.path(Path::new("UncheckedExtrinsic", module_path!()))
				// Include the type parameter types, even though they are not used directly in any
				// of the described fields. These type definitions can be used by downstream
				// consumers to help construct the custom decoding from the opaque bytes (see
				// below).
				.type_params(vec![
					TypeParameter::new("Address", Some(meta_type::<Address>())),
					TypeParameter::new("Call", Some(meta_type::<Call>())),
					TypeParameter::new("Signature", Some(meta_type::<Signature>())),
					TypeParameter::new("Extra", Some(meta_type::<Extra>())),
				])
				.docs(&["OldUncheckedExtrinsic raw bytes, requires custom decoding routine"])
				// Because of the custom encoding, we can only accurately describe the encoding as
				// an opaque `Vec<u8>`. Downstream consumers will need to manually implement the
				// codec to encode/decode the `signature` and `function` fields.
				.composite(Fields::unnamed().field(|f| f.ty::<Vec<u8>>()))
		}
	}

	impl<Address, Call, Signature, Extra> UncheckedExtrinsicV4<Address, Call, Signature, Extra> {
		pub fn new_signed(
			function: Call,
			signed: Address,
			signature: Signature,
			extra: Extra,
		) -> Self {
			Self { signature: Some((signed, signature, extra)), function }
		}

		pub fn new_unsigned(function: Call) -> Self {
			Self { signature: None, function }
		}
	}

	impl<Address, Call, Signature, Extra> Decode
		for UncheckedExtrinsicV4<Address, Call, Signature, Extra>
	where
		Address: Decode,
		Signature: Decode,
		Call: Decode,
		Extra: Decode,
	{
		fn decode<I: Input>(input: &mut I) -> Result<Self, Error> {
			// This is a little more complicated than usual since the binary format must be
			// compatible with SCALE's generic `Vec<u8>` type. Basically this just means accepting
			// that there will be a prefix of vector length.
			let expected_length: Compact<u32> = Decode::decode(input)?;
			let before_length = input.remaining_len()?;

			let version = input.read_byte()?;

			let is_signed = version & 0b1000_0000 != 0;
			let version = version & 0b0111_1111;
			if version != 4u8 {
				return Err("Invalid transaction version".into());
			}

			let signature = is_signed.then(|| Decode::decode(input)).transpose()?;
			let function = Decode::decode(input)?;

			if let Some((before_length, after_length)) =
				input.remaining_len()?.and_then(|a| before_length.map(|b| (b, a)))
			{
				let length = before_length.saturating_sub(after_length);

				if length != expected_length.0 as usize {
					return Err("Invalid length prefix".into());
				}
			}

			Ok(Self { signature, function })
		}
	}

	#[docify::export(unchecked_extrinsic_encode_impl)]
	impl<Address, Call, Signature, Extra> Encode
		for UncheckedExtrinsicV4<Address, Call, Signature, Extra>
	where
		Address: Encode,
		Signature: Encode,
		Call: Encode,
		Extra: Encode,
	{
		fn encode(&self) -> Vec<u8> {
			let mut tmp = Vec::with_capacity(core::mem::size_of::<Self>());

			// 1 byte version id.
			match self.signature.as_ref() {
				Some(s) => {
					tmp.push(4u8 | 0b1000_0000);
					s.encode_to(&mut tmp);
				},
				None => {
					tmp.push(4u8 & 0b0111_1111);
				},
			}
			self.function.encode_to(&mut tmp);

			let compact_len = codec::Compact::<u32>(tmp.len() as u32);

			// Allocate the output buffer with the correct length
			let mut output = Vec::with_capacity(compact_len.size_hint() + tmp.len());

			compact_len.encode_to(&mut output);
			output.extend(tmp);

			output
		}
	}

	impl<Address, Call, Signature, Extra> EncodeLike
		for UncheckedExtrinsicV4<Address, Call, Signature, Extra>
	where
		Address: Encode,
		Signature: Encode,
		Call: Encode,
		Extra: Encode,
	{
	}
}

#[cfg(test)]
mod tests {
	use super::{legacy::UncheckedExtrinsicV4, *};
	use crate::{
		codec::{Decode, Encode},
		impl_tx_ext_default,
		testing::TestSignature as TestSig,
		traits::{FakeDispatchable, IdentityLookup, TransactionExtension},
	};
	use sp_io::hashing::blake2_256;

	type TestContext = IdentityLookup<u64>;
	type TestAccountId = u64;

	// Custom Call enum that supports sorting on decode
	#[derive(Debug, Clone, Eq, PartialEq, Encode, TypeInfo)]
	enum Call {
		Raw(Vec<u8>),
		Sort(Vec<u8>),
	}

	impl Decode for Call {
		fn decode<I: Input>(input: &mut I) -> Result<Self, codec::Error> {
			let variant = input.read_byte()?;
			match variant {
				0 => {
					let data = Vec::<u8>::decode(input)?;
					Ok(Call::Raw(data))
				},
				1 => {
					let mut data = Vec::<u8>::decode(input)?;
					// Sort the data on decode
					data.sort();
					Ok(Call::Sort(data))
				},
				_ => Err("Invalid Call variant".into()),
			}
		}
	}

	impl DecodeWithMemTracking for Call {}

	impl From<Call> for Vec<u8> {
		fn from(call: Call) -> Vec<u8> {
			match call {
				Call::Sort(data) | Call::Raw(data) => data,
			}
		}
	}

	impl From<Vec<u8>> for FakeDispatchable<Call> {
		fn from(value: Vec<u8>) -> Self {
			Self(Call::Raw(value))
		}
	}

	type TestCall = FakeDispatchable<Call>;

	const TEST_ACCOUNT: TestAccountId = 0;

	// NOTE: this is demonstration. One can simply use `()` for testing.
	#[derive(
		Debug,
		Encode,
		Decode,
		DecodeWithMemTracking,
		Clone,
		Eq,
		PartialEq,
		Ord,
		PartialOrd,
		TypeInfo,
	)]
	struct DummyExtension;
	impl TransactionExtension<TestCall> for DummyExtension {
		const IDENTIFIER: &'static str = "DummyExtension";
		type Implicit = ();
		type Val = ();
		type Pre = ();
		impl_tx_ext_default!(TestCall; weight validate prepare);
	}

	type Ex = UncheckedExtrinsic<TestAccountId, TestCall, TestSig, DummyExtension>;
	type CEx = CheckedExtrinsic<TestAccountId, TestCall, DummyExtension>;

	#[test]
	fn unsigned_codec_should_work() {
		let call: TestCall = Call::Raw(vec![0u8; 0]).into();
		let ux = Ex::new_bare(call);
		let encoded = ux.encode();
		assert_eq!(Ex::decode(&mut &encoded[..]), Ok(ux));
	}

	#[test]
	fn invalid_length_prefix_is_detected() {
		let ux = Ex::new_bare(Call::Raw(vec![0u8; 0]).into());
		let mut encoded = ux.encode();

		let length = Compact::<u32>::decode(&mut &encoded[..]).unwrap();
		Compact(length.0 + 10).encode_to(&mut &mut encoded[..1]);

		assert_eq!(Ex::decode(&mut &encoded[..]), Err("Invalid length prefix".into()));
	}

	#[test]
	fn transaction_codec_should_work() {
		let ux = Ex::new_transaction(vec![0u8; 0].into(), DummyExtension);
		let encoded = ux.encode();
		assert_eq!(Ex::decode(&mut &encoded[..]), Ok(ux));
	}

	#[test]
	fn signed_codec_should_work() {
		let ux = Ex::new_signed(
			vec![0u8; 0].into(),
			TEST_ACCOUNT,
			TestSig(TEST_ACCOUNT, (vec![0u8; 0], DummyExtension).encode()),
			DummyExtension,
		);
		let encoded = ux.encode();
		assert_eq!(Ex::decode(&mut &encoded[..]), Ok(ux));
	}

	#[test]
	fn large_signed_codec_should_work() {
		let ux = Ex::new_signed(
			vec![0u8; 0].into(),
			TEST_ACCOUNT,
			TestSig(
				TEST_ACCOUNT,
				(vec![0u8; 257], DummyExtension).using_encoded(blake2_256)[..].to_owned(),
			),
			DummyExtension,
		);
		let encoded = ux.encode();
		assert_eq!(Ex::decode(&mut &encoded[..]), Ok(ux));
	}

	#[test]
	fn unsigned_check_should_work() {
		let ux = Ex::new_bare(vec![0u8; 0].into());
		assert!(ux.is_inherent());
		assert_eq!(
			<Ex as Checkable<TestContext>>::check(ux, &Default::default()),
			Ok(CEx { format: ExtrinsicFormat::Bare, function: vec![0u8; 0].into() }),
		);
	}

	#[test]
	fn badly_signed_check_should_fail() {
		let ux = Ex::new_signed(
			vec![0u8; 0].into(),
			TEST_ACCOUNT,
			TestSig(TEST_ACCOUNT, vec![0u8; 0].into()),
			DummyExtension,
		);
		assert!(!ux.is_inherent());
		assert_eq!(
			<Ex as Checkable<TestContext>>::check(ux, &Default::default()),
			Err(InvalidTransaction::BadProof.into()),
		);
	}

	#[test]
	fn transaction_check_should_work() {
		let ux = Ex::new_transaction(vec![0u8; 0].into(), DummyExtension);
		assert!(!ux.is_inherent());
		assert_eq!(
			<Ex as Checkable<TestContext>>::check(ux, &Default::default()),
			Ok(CEx {
				format: ExtrinsicFormat::General(ExtensionVariant::V0(DummyExtension)),
				function: vec![0u8; 0].into()
			}),
		);
	}

	#[test]
	fn signed_check_should_work() {
		let sig_payload = SignedPayload::from_raw(
			FakeDispatchable::from(vec![0u8; 0]),
			DummyExtension,
			DummyExtension.implicit().unwrap(),
		);
		let ux = Ex::new_signed(
			vec![0u8; 0].into(),
			TEST_ACCOUNT,
			TestSig(TEST_ACCOUNT, sig_payload.encode()),
			DummyExtension,
		);
		assert!(!ux.is_inherent());
		assert_eq!(
			<Ex as Checkable<TestContext>>::check(ux, &Default::default()),
			Ok(CEx {
				format: ExtrinsicFormat::Signed(TEST_ACCOUNT, DummyExtension),
				function: Call::Raw(vec![0u8; 0]).into()
			}),
		);
	}

	#[test]
	fn large_signed_check_uses_blake2_256_hashed_payload() {
		// A 257-byte call ensures the encoded `(call, ext, implicit)` tuple is > 256 bytes,
		// triggering the blake2_256 hashing branch in both `SignedPayload::using_encoded`
		// (used to build the signature here) and `check` (which re-derives the same
		// payload). The two paths must produce the same bytes for `verify` to succeed.
		let large_call_data = vec![0u8; 257];
		let call: TestCall = large_call_data.clone().into();
		let sig_payload = SignedPayload::from_raw(
			call.clone(),
			DummyExtension,
			DummyExtension.implicit().unwrap(),
		);

		// Sanity-check the test setup: the *raw* encoded tuple is > 256 bytes, but
		// `SignedPayload::encode()` returns the 32-byte blake2_256 hash.
		let raw_payload = (call.clone(), DummyExtension, ()).encode();
		assert!(raw_payload.len() > 256);
		let signed_bytes = sig_payload.encode();
		assert_eq!(signed_bytes.len(), 32);
		assert_eq!(signed_bytes, blake2_256(&raw_payload).to_vec());

		let ux =
			Ex::new_signed(call, TEST_ACCOUNT, TestSig(TEST_ACCOUNT, signed_bytes), DummyExtension);
		assert!(!ux.is_inherent());
		assert_eq!(
			<Ex as Checkable<TestContext>>::check(ux, &Default::default()),
			Ok(CEx {
				format: ExtrinsicFormat::Signed(TEST_ACCOUNT, DummyExtension),
				function: Call::Raw(large_call_data).into(),
			}),
		);
	}

	#[test]
	fn large_signed_check_fails_if_signed_over_unhashed_payload() {
		// If the signer (incorrectly) signs the *raw* > 256 byte payload instead of the
		// blake2_256 hash, `check` must reject it with `BadProof` because `check` always
		// hashes payloads that exceed 256 bytes.
		let large_call_data = vec![0u8; 257];
		let call: TestCall = large_call_data.into();
		let raw_payload = (call.clone(), DummyExtension, ()).encode();
		assert!(raw_payload.len() > 256);

		let ux =
			Ex::new_signed(call, TEST_ACCOUNT, TestSig(TEST_ACCOUNT, raw_payload), DummyExtension);
		assert_eq!(
			<Ex as Checkable<TestContext>>::check(ux, &Default::default()),
			Err(InvalidTransaction::BadProof.into()),
		);
	}

	#[test]
	fn encoding_matches_vec() {
		let ex = Ex::new_bare(Call::Raw(vec![0u8; 0]).into());
		let encoded = ex.encode();
		let decoded = Ex::decode(&mut encoded.as_slice()).unwrap();
		assert_eq!(decoded, ex);
		let as_vec: Vec<u8> = Decode::decode(&mut encoded.as_slice()).unwrap();
		assert_eq!(as_vec.encode(), encoded);
	}

	#[test]
	fn conversion_to_opaque() {
		let ux = Ex::new_bare(Call::Raw(vec![0u8; 0]).into());
		let encoded = ux.encode();
		let opaque: OpaqueExtrinsic = ux.into();
		let opaque_encoded = opaque.encode();
		assert_eq!(opaque_encoded, encoded);
	}

	#[test]
	fn large_bad_prefix_should_work() {
		let encoded = (Compact::<u32>::from(u32::MAX), Preamble::<(), (), ()>::Bare(0)).encode();
		assert!(Ex::decode(&mut &encoded[..]).is_err());
	}

	#[test]
	fn legacy_short_signed_encode_decode() {
		let call: TestCall = Call::Raw(vec![0u8; 4]).into();
		let signed = TEST_ACCOUNT;
		let extension = DummyExtension;
		let implicit = extension.implicit().unwrap();
		let legacy_signature = TestSig(TEST_ACCOUNT, (&call, &extension, &implicit).encode());

		let old_ux =
			UncheckedExtrinsicV4::<TestAccountId, TestCall, TestSig, DummyExtension>::new_signed(
				call.clone(),
				signed,
				legacy_signature.clone(),
				extension.clone(),
			);

		let encoded_old_ux = old_ux.encode();
		let decoded_old_ux = Ex::decode(&mut &encoded_old_ux[..]).unwrap();

		assert_eq!(decoded_old_ux.function, call);
		assert_eq!(
			decoded_old_ux.preamble,
			Preamble::Signed(signed, legacy_signature.clone(), extension.clone())
		);

		let new_ux =
			Ex::new_signed(call.clone(), signed, legacy_signature.clone(), extension.clone());

		let new_checked = new_ux.check(&IdentityLookup::<TestAccountId>::default()).unwrap();
		let old_checked =
			decoded_old_ux.check(&IdentityLookup::<TestAccountId>::default()).unwrap();
		assert_eq!(new_checked, old_checked);
	}

	#[test]
	fn legacy_long_signed_encode_decode() {
		let call: TestCall = Call::Raw(vec![0u8; 257]).into();
		let signed = TEST_ACCOUNT;
		let extension = DummyExtension;
		let implicit = extension.implicit().unwrap();
		let signature = TestSig(
			TEST_ACCOUNT,
			blake2_256(&(&call, DummyExtension, &implicit).encode()[..]).to_vec(),
		);

		let old_ux =
			UncheckedExtrinsicV4::<TestAccountId, TestCall, TestSig, DummyExtension>::new_signed(
				call.clone(),
				signed,
				signature.clone(),
				extension.clone(),
			);

		let encoded_old_ux = old_ux.encode();
		let decoded_old_ux = Ex::decode(&mut &encoded_old_ux[..]).unwrap();

		assert_eq!(decoded_old_ux.function, call);
		assert_eq!(
			decoded_old_ux.preamble,
			Preamble::Signed(signed, signature.clone(), extension.clone())
		);

		let new_ux = Ex::new_signed(call.clone(), signed, signature.clone(), extension.clone());

		let new_checked = new_ux.check(&IdentityLookup::<TestAccountId>::default()).unwrap();
		let old_checked =
			decoded_old_ux.check(&IdentityLookup::<TestAccountId>::default()).unwrap();
		assert_eq!(new_checked, old_checked);
	}

	#[test]
	fn legacy_unsigned_encode_decode() {
		let call: TestCall = Call::Raw(vec![0u8; 0]).into();

		let old_ux =
			UncheckedExtrinsicV4::<TestAccountId, TestCall, TestSig, DummyExtension>::new_unsigned(
				call.clone(),
			);

		let encoded_old_ux = old_ux.encode();
		let decoded_old_ux = Ex::decode(&mut &encoded_old_ux[..]).unwrap();

		assert_eq!(decoded_old_ux.function, call);
		assert_eq!(decoded_old_ux.preamble, Preamble::Bare(LEGACY_EXTRINSIC_FORMAT_VERSION));

		let new_legacy_ux = Ex::new_bare_legacy(call.clone());
		assert_eq!(encoded_old_ux, new_legacy_ux.encode());

		let new_ux = Ex::new_bare(call.clone());
		let encoded_new_ux = new_ux.encode();
		let decoded_new_ux = Ex::decode(&mut &encoded_new_ux[..]).unwrap();
		assert_eq!(new_ux, decoded_new_ux);

		let new_checked = new_ux.check(&IdentityLookup::<TestAccountId>::default()).unwrap();
		let old_checked =
			decoded_old_ux.check(&IdentityLookup::<TestAccountId>::default()).unwrap();
		assert_eq!(new_checked, old_checked);
	}

	#[test]
	fn max_call_heap_size_should_be_checked() {
		// Should be able to decode an `UncheckedExtrinsic` that contains a call with
		// heap size < `MAX_CALL_HEAP_SIZE`
		let ux = Ex::new_bare(Call::Raw(vec![0u8; DEFAULT_MAX_CALL_SIZE]).into());
		let encoded = ux.encode();
		assert_eq!(Ex::decode(&mut &encoded[..]), Ok(ux));

		// Otherwise should fail
		let ux = Ex::new_bare(Call::Raw(vec![0u8; DEFAULT_MAX_CALL_SIZE + 1]).into());
		let encoded = ux.encode();
		assert_eq!(
			Ex::decode(&mut &encoded[..]).unwrap_err().to_string(),
			"Could not decode `FakeDispatchable.0`:\n\tHeap memory limit exceeded while decoding\n"
		);
	}

	/// Ensures that a decoded extrinsic encodes to the exact same encoded bytes from what it was
	/// decoded from.
	#[test]
	fn encoding_is_stable() {
		// Create a call with unsorted data
		let unsorted_data = vec![5u8, 3, 7, 1, 9, 2, 8, 4, 6, 0];
		let call = Call::Sort(unsorted_data.clone());

		let unsorted_encoded = call.encode();

		let sig_payload = SignedPayload::from_raw(
			FakeDispatchable::from(call.clone()),
			DummyExtension,
			DummyExtension.implicit().unwrap(),
		);
		let sig_payload_encoded = sig_payload.encode();

		let ux = Ex::new_signed(
			call.into(),
			TEST_ACCOUNT,
			TestSig(TEST_ACCOUNT, sig_payload_encoded.clone()),
			DummyExtension,
		);

		// Encode and decode the extrinsic
		// During decode, the Sort variant will sort the data
		let encoded = ux.encode();
		let decoded_ux = Ex::decode(&mut &encoded[..]).unwrap();

		// The decoded call should have sorted data
		let mut expected_sorted_data = unsorted_data;
		expected_sorted_data.sort();

		let expected_decoded_call =
			FakeDispatchable::from(Call::Sort(expected_sorted_data.clone()));
		assert_eq!(decoded_ux.function, expected_decoded_call);

		// Verify that the decoded call encodes differently than the original
		let sorted_encoded = Call::Sort(expected_sorted_data).encode();
		assert_ne!(
			unsorted_encoded, sorted_encoded,
			"Sorted and unsorted should encode differently"
		);

		// Ensure that we can verify the signature successfully.
		assert_eq!(
			<Ex as Checkable<TestContext>>::check(decoded_ux, &Default::default())
				.unwrap()
				.function,
			expected_decoded_call,
		)
	}
}
