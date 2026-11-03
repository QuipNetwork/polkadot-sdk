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

//! *Ed-on-BLS12-381-Bandersnatch* types and host functions.
//!
//! Bandersnatch is an *incomplete* twisted Edwards curve: the HWCD
//! add/double formulas can produce projective points with `z = 0` when fed
//! cofactor-admixed (non-prime-order-subgroup) inputs. Such points have no
//! affine representative, so the standard `(x || y)` FFI channel cannot
//! carry them: arkworks' `From<Projective> for Affine` panics on
//! `z.inverse().unwrap()`.
//!
//! The shared `utils::mul_te` / `utils::msm_te` helpers detect the
//! degenerate case via `utils::IntoAffineSafe` and return
//! `utils::Error::DegeneratePoint` across the FFI boundary instead of
//! attempting to serialize an unrepresentable point. The runtime-side
//! hooks defined in this module catch that error and substitute the
//! all-zero projective point `(0, 0, 0, 0)`: not a valid curve point and
//! with `z = 0` it has no affine representative, so any downstream
//! validity or subgroup check on the result rejects it. The wire format
//! stays byte-identical to `ArkScale<EdwardsAffine>`: no sentinel bit, no
//! dedicated projective codec.

use crate::utils::{
	self, invalid_projective_fallback, Error, HostcallResult, IntoAffineSafe, FAIL_MSG,
};
use alloc::vec::Vec;
use ark_ec::{AffineRepr, CurveConfig};
use ark_ed_on_bls12_381_bandersnatch_ext::CurveHooks;
use sp_runtime_interface::{
	pass_by::{PassFatPointerAndRead, PassFatPointerAndWrite},
	runtime_interface,
};

/// Group configuration.
pub type BandersnatchConfig = ark_ed_on_bls12_381_bandersnatch_ext::BandersnatchConfig<HostHooks>;

/// Group configuration for Twisted Edwards form (equal to [`BandersnatchConfig`]).
pub type EdwardsConfig = ark_ed_on_bls12_381_bandersnatch_ext::EdwardsConfig<HostHooks>;
/// Twisted Edwards form point affine representation.
pub type EdwardsAffine = ark_ed_on_bls12_381_bandersnatch_ext::EdwardsAffine<HostHooks>;
/// Twisted Edwards form point projective representation.
pub type EdwardsProjective = ark_ed_on_bls12_381_bandersnatch_ext::EdwardsProjective<HostHooks>;

/// Group configuration for Short Weierstrass form (equal to [`BandersnatchConfig`]).
pub type SWConfig = ark_ed_on_bls12_381_bandersnatch_ext::SWConfig<HostHooks>;
/// Short Weierstrass form point affine representation.
pub type SWAffine = ark_ed_on_bls12_381_bandersnatch_ext::SWAffine<HostHooks>;
/// Short Weierstrass form point projective representation.
pub type SWProjective = ark_ed_on_bls12_381_bandersnatch_ext::SWProjective<HostHooks>;

/// Group scalar field (Fr).
pub type ScalarField = <BandersnatchConfig as CurveConfig>::ScalarField;

/// Curve hooks jumping into [`host_calls`] host functions.
#[derive(Copy, Clone)]
pub struct HostHooks;

impl CurveHooks for HostHooks {
	fn msm_te(bases: &[EdwardsAffine], scalars: &[ScalarField]) -> EdwardsProjective {
		let mut out = utils::buffer_for::<EdwardsAffine>();
		match host_calls::ed_on_bls12_381_bandersnatch_msm(
			&utils::encode(bases),
			&utils::encode(scalars),
			&mut out,
		) {
			Ok(()) => utils::decode::<EdwardsAffine>(&out).expect(FAIL_MSG).into_group(),
			Err(Error::DegeneratePoint) => invalid_projective_fallback::<EdwardsConfig>(),
			Err(_) => panic!("{FAIL_MSG}"),
		}
	}

	fn mul_projective_te(base: &EdwardsProjective, scalar: &[u64]) -> EdwardsProjective {
		// A `z = 0` projective cannot ride the affine FFI channel:
		// `into_affine()` would panic. `into_affine_safe()` returns `None`
		// in that case; we honor the same all-zero projective fallback the
		// host applies on its side, locally. Honest subgroup-validated
		// callers never produce such a projective.
		let Some(base_aff) = base.into_affine_safe() else {
			return invalid_projective_fallback::<EdwardsConfig>();
		};
		let mut out = utils::buffer_for::<EdwardsAffine>();
		match host_calls::ed_on_bls12_381_bandersnatch_mul(
			&utils::encode(base_aff),
			&utils::encode(scalar),
			&mut out,
		) {
			Ok(()) => utils::decode::<EdwardsAffine>(&out).expect(FAIL_MSG).into_group(),
			Err(Error::DegeneratePoint) => invalid_projective_fallback::<EdwardsConfig>(),
			Err(_) => panic!("{FAIL_MSG}"),
		}
	}
}

/// Interfaces for working with *Arkworks* *Ed-on-BLS12-381-Bandersnatch* elliptic curve related
/// types from within the runtime.
///
/// All types are (de-)serialized through the wrapper types from `ark-scale`.
///
/// `ArkScale`'s `Usage` generic parameter is expected to be set to "not-validated"
/// and "not-compressed".
///
/// When the projective result of a host call lands at `z = 0` (only reachable
/// via non-subgroup inputs), the host returns `utils::Error::DegeneratePoint`
/// instead of panicking, and the runtime-side `HostHooks` impl substitutes the
/// all-zero projective point `(0, 0, 0, 0)`. See the module-level doc for the
/// full contract.
#[runtime_interface]
pub trait HostCalls {
	/// Twisted Edwards multi scalar multiplication for *Ed-on-BLS12-381-Bandersnatch*.
	///
	/// Receives encoded:
	/// - `bases`: `Vec<EdwardsAffine>`.
	/// - `scalars`: `Vec<ScalarField>`.
	/// Writes encoded: `EdwardsAffine` to `out`.
	fn ed_on_bls12_381_bandersnatch_msm(
		bases: PassFatPointerAndRead<&[u8]>,
		scalars: PassFatPointerAndRead<&[u8]>,
		out: PassFatPointerAndWrite<&mut [u8]>,
	) -> HostcallResult {
		utils::msm_te::<ark_ed_on_bls12_381_bandersnatch::EdwardsConfig>(bases, scalars, out)
	}

	/// Twisted Edwards affine multiplication for *Ed-on-BLS12-381-Bandersnatch*.
	///
	/// Receives encoded:
	/// - `base`: `EdwardsAffine`.
	/// - `scalar`: `BigInteger`.
	/// Writes encoded `EdwardsAffine` to `out`.
	fn ed_on_bls12_381_bandersnatch_mul(
		base: PassFatPointerAndRead<&[u8]>,
		scalar: PassFatPointerAndRead<&[u8]>,
		out: PassFatPointerAndWrite<&mut [u8]>,
	) -> HostcallResult {
		utils::mul_te::<ark_ed_on_bls12_381_bandersnatch::EdwardsConfig>(base, scalar, out)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::utils::testing::*;
	use ark_ec::{
		twisted_edwards::{Affine as TEAffine, Projective as TEProjective, TECurveConfig},
		CurveGroup,
	};
	use ark_ed_on_bls12_381_bandersnatch::{EdwardsConfig as RawConfig, Fq, Fr};
	use ark_ff::{AdditiveGroup, MontFp, PrimeField, Zero};

	#[test]
	fn mul_works() {
		mul_te_test::<EdwardsAffine, ark_ed_on_bls12_381_bandersnatch::EdwardsAffine>();
	}

	#[test]
	fn msm_works() {
		msm_te_test::<EdwardsAffine, ark_ed_on_bls12_381_bandersnatch::EdwardsAffine>();
	}

	#[test]
	fn mul_works_sw() {
		mul_test::<SWAffine, ark_ed_on_bls12_381_bandersnatch::SWAffine>();
	}

	#[test]
	fn msm_works_sw() {
		msm_test::<SWAffine, ark_ed_on_bls12_381_bandersnatch::SWAffine>();
	}

	/// The cofactor-admixed `y = 2` non-subgroup point used as the
	/// degenerate trigger throughout the tests below. Generic so the same
	/// constructor serves both `RawConfig` (for raw-arithmetic precondition
	/// checks) and `EdwardsConfig<HostHooks>` (the runtime-facing type).
	fn y2_non_subgroup<P: TECurveConfig<BaseField = Fq>>() -> TEAffine<P> {
		TEAffine::<P>::get_point_from_y_unchecked(Fq::from(2u64), false)
			.expect("y=2 must yield a valid TEAffine point")
	}

	#[test]
	fn host_mul_with_z_zero_result_returns_fallback() {
		// Sanity: the raw operation does produce z = 0.
		let proj: TEProjective<RawConfig> = y2_non_subgroup::<RawConfig>().into_group();
		let raw_res = <RawConfig as TECurveConfig>::mul_projective(&proj, Fr::MODULUS.0.as_ref());
		assert!(raw_res.z.is_zero(), "test precondition: y=2 * Fr::MODULUS must hit z=0");

		// The raw host call surfaces the degenerate result as an error
		// (the helper can't represent it on the affine FFI channel).
		let scalar_bigint: Vec<u64> = Fr::MODULUS.0.to_vec();
		let input_enc = utils::encode(y2_non_subgroup::<EdwardsConfig>());
		let scalar_enc = utils::encode(scalar_bigint);
		let mut out = utils::buffer_for::<EdwardsAffine>();
		let err = host_calls::ed_on_bls12_381_bandersnatch_mul(&input_enc, &scalar_enc, &mut out)
			.expect_err("z=0 result must surface as Err(DegeneratePoint)");
		assert_eq!(err, Error::DegeneratePoint);

		// The runtime-side hook catches that error and substitutes the
		// all-zero invalid projective point.
		let p_ext: EdwardsProjective = y2_non_subgroup::<EdwardsConfig>().into_group();
		let r = <HostHooks as CurveHooks>::mul_projective_te(&p_ext, Fr::MODULUS.0.as_ref());
		assert_eq!(
			r,
			invalid_projective_fallback::<EdwardsConfig>(),
			"hook must return all-zero projective on degenerate"
		);
	}

	#[test]
	fn mul_projective_with_z_zero_input_returns_fallback() {
		use ark_std::{test_rng, UniformRand};
		let mut rng = test_rng();
		let y = Fq::rand(&mut rng);
		let t = Fq::rand(&mut rng);
		let p = EdwardsProjective::new_unchecked(Fq::ZERO, y, t, Fq::ZERO);
		let r = <HostHooks as CurveHooks>::mul_projective_te(&p, &[7u64, 0, 0, 0]);
		assert_eq!(
			r,
			invalid_projective_fallback::<EdwardsConfig>(),
			"z=0 input must yield all-zero coordinate projective"
		);
	}

	#[test]
	fn fallback_is_invalid_projective_point() {
		// (1) The fallback is the all-zero projective point.
		let fallback = invalid_projective_fallback::<EdwardsConfig>();
		assert!(fallback.x.is_zero(), "fallback x must be zero");
		assert!(fallback.y.is_zero(), "fallback y must be zero");
		assert!(fallback.t.is_zero(), "fallback t must be zero");
		assert!(fallback.z.is_zero(), "fallback z must be zero");

		// (2) `z = 0` means `into_affine()` would hit `z.inverse().unwrap()`
		// and panic, but only if arkworks' `is_zero()` doesn't short-circuit
		// first. `is_zero()` for TE projective requires `!y.is_zero()`, so
		// with `y = 0` the check returns `false` and the panic path is
		// reached. This means `into_affine_safe()` returns `None` and
		// `into_affine()` panics — both correctly rejecting this sentinel.
		assert!(!fallback.is_zero(), "all-zero projective must NOT be considered identity");
		assert!(
			fallback.into_affine_safe().is_none(),
			"all-zero projective must map to None via IntoAffineSafe",
		);

		// (3) Another degenerate z=0 shape: (X=0, Y!=0, Z=0).
		// arkworks' `into_affine()` panics here because `is_zero()`
		// requires `!y.is_zero()` AND `y == z`, so with Y!=0 and Z=0
		// it returns false, falling through to `z.inverse().unwrap()`
		// which panics. `into_affine_safe` returns `None` instead.
		// Note: `msm_te` / `mul_te` can produce either this shape or
		// the all-zero `(0,0,0,0)` shape — both have z=0 and both are
		// caught by `into_affine_safe`.
		let degenerate = TEProjective::<RawConfig>::new_unchecked(
			Fq::ZERO,        // X = 0
			Fq::from(7u64),  // Y != 0 (F-exception)
			Fq::from(11u64), // arbitrary T
			Fq::ZERO,        // Z = 0
		);
		assert!(
			degenerate.into_affine_safe().is_none(),
			"z=0 projective must map to None via IntoAffineSafe",
		);
	}

	/// F_q-rational pair found via Sage brute-force search. They satisfy
	/// d * x_A * x_B * y_A * y_B = 1, which forces HWCD's Z_3 = F*G = 0
	/// even though both inputs are valid affine curve points.
	fn exceptional_pair() -> (EdwardsAffine, EdwardsAffine, EdwardsAffine) {
		let xa: Fq = MontFp!(
			"12611587488970178020234800979835231446181428428390492190317266241455236381927"
		);
		let ya: Fq =
			MontFp!("8625363597705895091270672088731506059935752500467284843225771956507605756711");
		let xb: Fq =
			MontFp!("5253339395048946693631279295832797565125937378490576959411837397991361739535");
		let yb: Fq = MontFp!(
			"24752777243643877000069062635360441442644758493268974317933177186378585499408"
		);
		// True A+B as computed via SW form in Sage (the group-law answer).
		let x_sum: Fq = MontFp!(
			"30239213723729448420307207485613680945165091785466061697591732383921178212543"
		);
		let y_sum: Fq = MontFp!(
			"48407687168732128978323921344344221491641898681064657528705691267288289221251"
		);
		(
			EdwardsAffine::new_unchecked(xa, ya),
			EdwardsAffine::new_unchecked(xb, yb),
			EdwardsAffine::new_unchecked(x_sum, y_sum),
		)
	}

	#[test]
	fn hwcd_exceptional_pair_produces_all_zero_projective() {
		let (a, b, _expected_sum) = exceptional_pair();
		assert!(a.is_on_curve(), "point A must be on curve");
		assert!(b.is_on_curve(), "point B must be on curve");

		// HWCD addition of this exceptional pair produces (0, 0, 0, 0).
		let a_proj: EdwardsProjective = a.into_group();
		let b_proj: EdwardsProjective = b.into_group();
		let sum = a_proj + b_proj;
		assert!(sum.x.is_zero(), "exceptional sum x must be zero");
		assert!(sum.y.is_zero(), "exceptional sum y must be zero");
		assert!(sum.t.is_zero(), "exceptional sum t must be zero");
		assert!(sum.z.is_zero(), "exceptional sum z must be zero");
	}

	#[test]
	fn hwcd_exceptional_pair_recovers_via_sage_sum() {
		let (a, b, sage_sum) = exceptional_pair();
		let a_proj: EdwardsProjective = a.into_group();
		let b_proj: EdwardsProjective = b.into_group();
		let sage_sum_proj: EdwardsProjective = sage_sum.into_group();

		// A + (A+B from arkworks) produces all-zero: the HWCD exception
		// propagates through the (0,0,0,0) intermediate.
		let ark_sum = a_proj + b_proj;
		let a_plus_ark_sum = a_proj + ark_sum;
		assert_eq!(
			a_plus_ark_sum,
			invalid_projective_fallback::<EdwardsConfig>(),
			"A + (A+B from arkworks) must produce all-zero projective"
		);

		// A + (A+B from Sage) gives the correct 2*A + B because A and
		// the Sage sum are not an exceptional pair.
		let two_a = a_proj + a_proj;
		let two_a_plus_b = two_a + b_proj;
		let a_plus_sage_sum = a_proj + sage_sum_proj;
		assert_eq!(
			a_plus_sage_sum.into_affine(),
			two_a_plus_b.into_affine(),
			"A + (A+B from Sage) must equal 2*A + B"
		);
	}

	#[test]
	fn hwcd_exceptional_pair_msm_produces_all_zero() {
		use ark_ec::VariableBaseMSM;

		let (a, b, _expected_sum) = exceptional_pair();

		// msm([A, B], [2, 1]) = 2*A + B, but internally pippenger will
		// compute A+B which hits the exceptional case.
		let bases = vec![a, b];
		let scalars = vec![Fr::from(2u64), Fr::from(1u64)];
		let result = EdwardsProjective::msm(&bases, &scalars).unwrap();

		assert_eq!(
			result,
			invalid_projective_fallback::<EdwardsConfig>(),
			"msm([A, B], [2, 1]) must produce invalid projective fallback"
		);
	}

	#[test]
	fn hwcd_exceptional_pair_msm_te_returns_invalid_projective() {
		let (a, b, _expected_sum) = exceptional_pair();

		// msm_te via the host call should detect the degenerate z=0 result
		// and return the invalid projective point fallback.
		let bases = vec![a, b];
		let scalars = vec![Fr::from(2u64), Fr::from(1u64)];
		let result = <HostHooks as CurveHooks>::msm_te(&bases, &scalars);

		assert_eq!(
			result,
			invalid_projective_fallback::<EdwardsConfig>(),
			"msm_te must return invalid projective fallback"
		);
	}

	#[test]
	fn y2_point_deserialize_checked_vs_unchecked() {
		use ark_scale::ark_serialize::{
			CanonicalDeserialize, CanonicalSerialize, Compress, Validate,
		};

		let p = y2_non_subgroup::<EdwardsConfig>();
		assert!(p.is_on_curve(), "y=2 point must be on curve");
		assert!(
			!p.is_in_correct_subgroup_assuming_on_curve(),
			"y=2 point must NOT be in the prime-order subgroup",
		);

		let mut bytes = Vec::new();
		p.serialize_with_mode(&mut bytes, Compress::No).unwrap();

		// `Validate::No` accepts the non-subgroup point.
		let decoded =
			EdwardsAffine::deserialize_with_mode(&bytes[..], Compress::No, Validate::No).unwrap();
		assert_eq!(decoded, p);

		// `Validate::Yes` over the same bytes rejects it at decode time.
		assert!(
			EdwardsAffine::deserialize_with_mode(&bytes[..], Compress::No, Validate::Yes).is_err(),
			"Validate::Yes must reject non-subgroup point",
		);
	}
}
