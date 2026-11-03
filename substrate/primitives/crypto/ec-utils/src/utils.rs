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

//! Generic executions of the operations for *Arkworks* elliptic curves.

// As not all functions are used by each elliptic curve and some elliptic
// curve may be excluded by the build we resort to `#[allow(unused)]` to
// suppress the expected warning.
#![allow(unused)]

use alloc::{vec, vec::Vec};
use ark_ec::{
	pairing::{MillerLoopOutput, Pairing},
	short_weierstrass::{Affine as SWAffine, SWCurveConfig},
	twisted_edwards::{Affine as TEAffine, Projective as TEProjective, TECurveConfig},
	CurveGroup,
};
use ark_ff::{AdditiveGroup, Field, Zero};
use ark_scale::{
	ark_serialize::{CanonicalDeserialize, CanonicalSerialize, Compress, Validate},
	scale::{Decode, Encode, Output},
	ArkScaleMaxEncodedLen, MaxEncodedLen,
};
use sp_runtime_interface::RIType;

/// Unexpected failure message.
pub const FAIL_MSG: &str = "Unexpected failure, bad arguments, broken host/runtime contract; qed";

// SCALE encoding parameters shared by all the enabled modules
const SCALE_USAGE: u8 = ark_scale::make_usage(Compress::No, Validate::No);
type ArkScale<T> = ark_scale::ArkScale<T, SCALE_USAGE>;

/// Convenience alias for a big integer represented as a sequence of `u64` limbs.
pub type BigInteger = Vec<u64>;

/// `Output` adapter for `&mut [u8]`, which doesn't natively implement it in `no_std`.
struct SliceOutput<'a> {
	buf: &'a mut [u8],
	offset: usize,
}

impl<'a> Output for SliceOutput<'a> {
	fn write(&mut self, bytes: &[u8]) {
		self.buf[self.offset..self.offset + bytes.len()].copy_from_slice(bytes);
		self.offset += bytes.len();
	}

	fn push_byte(&mut self, byte: u8) {
		self.buf[self.offset] = byte;
		self.offset += 1;
	}
}

/// Error type for host call operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
	/// Encoding error due to small output buffer.
	Encode = 1,
	/// Input data decoding error
	Decode = 2,
	/// Input sequences have different lengths.
	/// Applies to `msm` operations.
	LengthMismatch = 3,
	/// The projective result of a twisted Edwards operation has `z = 0`
	/// and therefore no affine representative. Reachable on *incomplete*
	/// TE curves like Bandersnatch when the inputs are not in the
	/// prime-order subgroup. The runtime-side hook decides whether to
	/// recover (e.g. by substituting [`invalid_projective_fallback`])
	/// or to surface the error.
	DegeneratePoint = 4,
	/// Unknown error.
	Unknown = 255,
}

#[inline(always)]
pub fn encoded_len<T: CanonicalSerialize + ArkScaleMaxEncodedLen>() -> usize {
	ArkScale::<T>::max_encoded_len()
}

#[inline(always)]
pub fn buffer_for<T: CanonicalSerialize + ArkScaleMaxEncodedLen>() -> Vec<u8> {
	vec![0_u8; encoded_len::<T>()]
}

/// Return a `Result<(), Error>` as a single `u32` through the FFI boundary.
pub struct HostcallResult;

impl RIType for HostcallResult {
	type FFIType = u32;
	type Inner = Result<(), Error>;
}

#[cfg(not(substrate_runtime))]
impl sp_runtime_interface::host::IntoFFIValue for HostcallResult {
	fn into_ffi_value(
		value: Self::Inner,
		_context: &mut dyn sp_runtime_interface::sp_wasm_interface::FunctionContext,
	) -> sp_runtime_interface::sp_wasm_interface::Result<Self::FFIType> {
		Ok(match value {
			Ok(()) => 0,
			Err(e) => e as u32,
		})
	}
}

#[cfg(substrate_runtime)]
impl sp_runtime_interface::wasm::FromFFIValue for HostcallResult {
	fn from_ffi_value(arg: Self::FFIType) -> Self::Inner {
		match arg {
			0 => Ok(()),
			1 => Err(Error::Encode),
			2 => Err(Error::Decode),
			3 => Err(Error::LengthMismatch),
			4 => Err(Error::DegeneratePoint),
			_ => Err(Error::Unknown),
		}
	}
}

#[inline(always)]
pub fn encode_iter<T: CanonicalSerialize>(iter: impl Iterator<Item = T>) -> Vec<u8> {
	encode(iter.collect::<Vec<_>>())
}

#[inline(always)]
pub fn encode<T: CanonicalSerialize>(val: T) -> Vec<u8> {
	ArkScale::from(val).encode()
}

#[inline(always)]
pub fn encode_into<T: CanonicalSerialize>(val: T, buf: &mut [u8]) -> Result<(), Error> {
	let val = ArkScale::from(val);
	// Size hint uses arkworks `serialized_size`, which is accurate
	if val.size_hint() > buf.len() {
		return Err(Error::Encode);
	}
	val.encode_to(&mut SliceOutput { buf, offset: 0 });
	Ok(())
}

#[inline(always)]
pub fn decode<T: CanonicalDeserialize>(mut buf: &[u8]) -> Result<T, Error> {
	ArkScale::<T>::decode(&mut buf).map_err(|_| Error::Decode).map(|v| v.0)
}

/// Fallible projective-to-affine conversion for *twisted Edwards* projectives.
///
/// Arkworks' standard `From<Projective> for Affine` for TE branches on
/// `is_zero()` (i.e. `x == 0 && y == z`) and otherwise calls
/// `z.inverse().unwrap()`. On *incomplete* twisted Edwards curves such as
/// Bandersnatch, HWCD arithmetic fed non-subgroup inputs can land in
/// `(0, Y, T, 0)` or `(X, 0, T, 0)` states with `z = 0` that miss the
/// `is_zero()` short-circuit, hitting the panicking inverse. This trait
/// returns `None` in that case so callers can choose what to ship over
/// the FFI boundary instead of crashing.
///
/// Not defined for short Weierstrass: SW `into_affine()` is total.
/// `z = 0` there is just the point at infinity (the identity), with a
/// valid affine representation through arkworks' `infinity` flag.
pub trait IntoAffineSafe {
	type Affine;
	fn into_affine_safe(self) -> Option<Self::Affine>;
}

impl<P: TECurveConfig> IntoAffineSafe for TEProjective<P> {
	type Affine = TEAffine<P>;
	#[inline]
	fn into_affine_safe(self) -> Option<TEAffine<P>> {
		(!self.z.is_zero()).then(|| self.into_affine())
	}
}

/// Pairing multi Miller loop.
///
/// Receives encoded:
/// - `g1`: `Vec<G1Affine>`.
/// - `g2`: `Vec<G2Affine>`.
/// Writes encoded `TargetField` to `out`.
pub fn multi_miller_loop<T: Pairing>(g1: &[u8], g2: &[u8], out: &mut [u8]) -> Result<(), Error> {
	let g1 = decode::<Vec<<T as Pairing>::G1Affine>>(g1)?;
	let g2 = decode::<Vec<<T as Pairing>::G2Affine>>(g2)?;
	let res = T::multi_miller_loop(g1, g2);
	encode_into(res.0, out)
}

/// Pairing final exponentiation.
///
/// Receives encoded `TargetField`.
/// Writes encoded `TargetField` to `in_out`.
pub fn final_exponentiation<T: Pairing>(in_out: &mut [u8]) -> Result<(), Error> {
	let target = decode::<<T as Pairing>::TargetField>(in_out)?;
	let res = T::final_exponentiation(MillerLoopOutput(target)).ok_or(Error::Unknown)?;
	encode_into(res.0, in_out)
}

/// Short Weierstrass multi scalar multiplication.
///
/// Expects encoded:
/// - `bases`: `Vec<SWAffine<SWCurveConfig>>`.
/// - `scalars`: `Vec<SWCurveConfig::ScalarField>`.
/// Writes encoded `SWAffine<SWCurveConfig>` to `out`.
pub fn msm_sw<T: SWCurveConfig>(bases: &[u8], scalars: &[u8], out: &mut [u8]) -> Result<(), Error> {
	let bases = decode::<Vec<SWAffine<T>>>(bases)?;
	let scalars = decode::<Vec<T::ScalarField>>(scalars)?;
	let res = T::msm(&bases, &scalars).map_err(|_| Error::LengthMismatch)?.into_affine();
	encode_into::<SWAffine<T>>(res, out)
}

/// Short Weierstrass affine multiplication.
///
/// Expects encoded:
/// - `base`: `SWAffine<SWCurveConfig>`.
/// - `scalar`: `BigInteger`.
/// Writes encoded `SWAffine<SWCurveConfig>` to `out`.
pub fn mul_sw<T: SWCurveConfig>(base: &[u8], scalar: &[u8], out: &mut [u8]) -> Result<(), Error> {
	let base = decode::<SWAffine<T>>(base)?;
	let scalar = decode::<BigInteger>(scalar)?;
	let res = T::mul_affine(&base, &scalar).into_affine();
	encode_into::<SWAffine<T>>(res, out)
}

/// Invalid projective point with all-zero coordinates.
///
/// This is not a valid curve point - it represents an undefined/degenerate
/// result in projective coordinates. Useful as a sentinel value when
/// operations produce a `z = 0` projective that has no affine representative.
/// Any downstream validity or subgroup check will reject it.
pub const fn invalid_projective_fallback<T: TECurveConfig>() -> TEProjective<T> {
	TEProjective::<T>::new_unchecked(
		T::BaseField::ZERO,
		T::BaseField::ZERO,
		T::BaseField::ZERO,
		T::BaseField::ZERO,
	)
}

/// Twisted Edwards multi scalar multiplication.
///
/// Expects encoded:
/// - `bases`: `Vec<TEAffine<TECurveConfig>>`.
/// - `scalars`: `Vec<TECurveConfig::ScalarField>`.
/// Writes encoded `TEAffine<TECurveConfig>` to `out`. Returns
/// [`Error::DegeneratePoint`] if the projective result has `z = 0` and
/// therefore no affine representative (reachable on incomplete TE forms
/// like Bandersnatch when fed non-subgroup bases). The runtime-side hook
/// decides the policy for that case (e.g. substitute
/// [`invalid_projective_fallback`]).
pub fn msm_te<T: TECurveConfig>(bases: &[u8], scalars: &[u8], out: &mut [u8]) -> Result<(), Error> {
	let bases = decode::<Vec<TEAffine<T>>>(bases)?;
	let scalars = decode::<Vec<T::ScalarField>>(scalars)?;
	let res = T::msm(&bases, &scalars).map_err(|_| Error::LengthMismatch)?;
	let aff = res.into_affine_safe().ok_or(Error::DegeneratePoint)?;
	encode_into::<TEAffine<T>>(aff, out)
}

/// Twisted Edwards affine multiplication.
///
/// Expects encoded:
/// - `base`: `TEAffine<TECurveConfig>`.
/// - `scalar`: `BigInteger`.
/// Writes encoded `TEAffine<TECurveConfig>` to `out`. Returns
/// [`Error::DegeneratePoint`] if the projective result has `z = 0`,
/// under the same conditions and contract as [`msm_te`].
pub fn mul_te<T: TECurveConfig>(base: &[u8], scalar: &[u8], out: &mut [u8]) -> Result<(), Error> {
	let base_aff = decode::<TEAffine<T>>(base)?;
	let scalar = decode::<BigInteger>(scalar)?;
	let res = T::mul_affine(&base_aff, &scalar);
	let aff = res.into_affine_safe().ok_or(Error::DegeneratePoint)?;
	encode_into::<TEAffine<T>>(aff, out)
}

#[cfg(test)]
pub mod testing {
	use super::*;
	use ark_ec::{AffineRepr, VariableBaseMSM};
	use ark_ff::PrimeField;
	use ark_std::{test_rng, UniformRand};

	pub fn msm_args<P: AffineRepr>(count: usize) -> (Vec<P>, Vec<P::ScalarField>) {
		let mut rng = test_rng();
		(0..count).map(|_| (P::rand(&mut rng), P::ScalarField::rand(&mut rng))).unzip()
	}

	pub fn mul_args<P: AffineRepr>() -> (P, P::ScalarField) {
		let (p, s) = msm_args::<P>(1);
		(p[0], s[0])
	}

	fn pairing_args<E: Pairing>() -> (E::G1Affine, E::G2Affine) {
		let mut rng = test_rng();
		(E::G1Affine::rand(&mut rng), E::G2Affine::rand(&mut rng))
	}

	pub fn mul_test<SubAffine, ArkAffine>()
	where
		SubAffine: AffineRepr + ArkScaleMaxEncodedLen,
		ArkAffine: AffineRepr<ScalarField = SubAffine::ScalarField>,
		ArkAffine::Config: ark_ec::short_weierstrass::SWCurveConfig,
	{
		let (p, s) = mul_args::<SubAffine>();

		// This goes implicitly through the hostcall
		let r1 = (p * s).into_affine();

		// This directly calls into arkworks
		let p_enc = encode(p);
		let s_enc = encode(s.into_bigint().as_ref());
		let mut r2_enc = buffer_for::<SubAffine>();
		mul_sw::<ArkAffine::Config>(&p_enc, &s_enc, &mut r2_enc).unwrap();
		let r2 = decode::<SubAffine>(&r2_enc).unwrap();

		assert_eq!(r1, r2);
	}

	pub fn msm_test<SubAffine, ArkAffine>()
	where
		SubAffine: AffineRepr + ArkScaleMaxEncodedLen,
		ArkAffine: AffineRepr<ScalarField = SubAffine::ScalarField>,
		ArkAffine::Config: ark_ec::short_weierstrass::SWCurveConfig,
	{
		let (bases, scalars) = msm_args::<SubAffine>(10);

		// This goes implicitly through the hostcall
		let r1 = SubAffine::Group::msm(&bases, &scalars).unwrap().into_affine();

		// This directly calls into arkworks
		let bases_enc = encode(&bases[..]);
		let scalars_enc = encode(&scalars[..]);
		let mut r2_enc = buffer_for::<SubAffine>();
		msm_sw::<ArkAffine::Config>(&bases_enc, &scalars_enc, &mut r2_enc).unwrap();
		let r2 = decode::<SubAffine>(&r2_enc).unwrap();

		assert_eq!(r1, r2);
	}

	pub fn mul_te_test<SubAffine, ArkAffine>()
	where
		SubAffine: AffineRepr + ArkScaleMaxEncodedLen,
		ArkAffine: AffineRepr<ScalarField = SubAffine::ScalarField>,
		ArkAffine::Config: ark_ec::twisted_edwards::TECurveConfig,
	{
		let (p, s) = mul_args::<SubAffine>();

		// This goes implicitly through the hostcall
		let r1 = (p * s).into_affine();

		// This directly calls into arkworks
		let p_enc = encode(p);
		let s_enc = encode(s.into_bigint().as_ref());
		let mut r2_enc = buffer_for::<SubAffine>();
		mul_te::<ArkAffine::Config>(&p_enc, &s_enc, &mut r2_enc).unwrap();
		let r2 = decode::<SubAffine>(&r2_enc).unwrap();

		assert_eq!(r1, r2);
	}

	pub fn msm_te_test<SubAffine, ArkAffine>()
	where
		SubAffine: AffineRepr + ArkScaleMaxEncodedLen,
		ArkAffine: AffineRepr<ScalarField = SubAffine::ScalarField>,
		ArkAffine::Config: ark_ec::twisted_edwards::TECurveConfig,
	{
		let (bases, scalars) = msm_args::<SubAffine>(10);

		// This goes implicitly through the hostcall
		let r1 = SubAffine::Group::msm(&bases, &scalars).unwrap().into_affine();

		// This directly calls into arkworks
		let bases_enc = encode(&bases[..]);
		let scalars_enc = encode(&scalars[..]);
		let mut r2_enc = buffer_for::<SubAffine>();
		msm_te::<ArkAffine::Config>(&bases_enc, &scalars_enc, &mut r2_enc).unwrap();
		let r2 = decode::<SubAffine>(&r2_enc).unwrap();

		assert_eq!(r1, r2);
	}

	pub fn pairing_test<SubPairing, ArkPairing>()
	where
		SubPairing: Pairing,
		<SubPairing as Pairing>::TargetField: ArkScaleMaxEncodedLen,
		ArkPairing: Pairing,
	{
		let (g1, g2) = pairing_args::<SubPairing>();

		// This goes implicitly through the `multi_miller_loop` and `final_exponentiation` hostcalls
		let r1 = SubPairing::pairing(g1, g2).0;

		// Pairing via direct arkworks calls
		let g1_enc = encode(vec![g1]);
		let g2_enc = encode(vec![g2]);
		let mut r2_enc = buffer_for::<<SubPairing as Pairing>::TargetField>();
		multi_miller_loop::<ArkPairing>(&g1_enc, &g2_enc, &mut r2_enc).unwrap();
		final_exponentiation::<ArkPairing>(&mut r2_enc).unwrap();
		let r2 = decode::<<SubPairing as Pairing>::TargetField>(&r2_enc).unwrap();

		assert_eq!(r1, r2);
	}
}
