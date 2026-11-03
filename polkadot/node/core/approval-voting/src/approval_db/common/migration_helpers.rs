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

use bitvec::{order::Lsb0 as BitOrderLsb0, vec::BitVec};

use polkadot_node_primitives::approval::v2::{
	AssignmentCertKindV2, AssignmentCertV2, VrfPreOutput, VrfProof, VrfSignature,
	RELAY_VRF_MODULO_CONTEXT,
};

pub fn make_bitvec(len: usize) -> BitVec<u8, BitOrderLsb0> {
	bitvec::bitvec![u8, BitOrderLsb0; 0; len]
}

pub fn dummy_assignment_cert(kind: AssignmentCertKindV2) -> AssignmentCertV2 {
	let ctx = schnorrkel::signing_context(RELAY_VRF_MODULO_CONTEXT);
	let msg = b"test-garbage";
	let mut prng = rand_core::OsRng;
	let keypair = schnorrkel::Keypair::generate_with(&mut prng);
	let (inout, proof, _) = keypair.vrf_sign(ctx.bytes(msg));
	let preout = inout.to_preout();

	AssignmentCertV2 {
		kind,
		vrf: VrfSignature { pre_output: VrfPreOutput(preout), proof: VrfProof(proof) },
	}
}
