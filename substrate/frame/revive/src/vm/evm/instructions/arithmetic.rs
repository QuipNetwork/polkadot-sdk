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

pub mod i256;
use i256::{i256_div, i256_mod};
mod modular;
use modular::Modular;

use crate::{
	Error, U256,
	vm::{
		Ext,
		evm::{EVMGas, EvmOpcodeCosts, Interpreter, interpreter::Halt},
	},
};
use core::ops::ControlFlow;
use revm::interpreter::gas::{EXP, LOW, VERYLOW};

/// Implements the ADD instruction - adds two values from stack.
pub fn add<E: Ext>(interpreter: &mut Interpreter<E>) -> ControlFlow<Halt> {
	interpreter.ext.charge_or_halt(EVMGas(VERYLOW))?;
	let ([op1], op2) = interpreter.stack.popn_top()?;
	*op2 = op1.overflowing_add(*op2).0;
	ControlFlow::Continue(())
}

/// Implements the MUL instruction - multiplies two values from stack.
pub fn mul<E: Ext>(interpreter: &mut Interpreter<E>) -> ControlFlow<Halt> {
	interpreter.ext.charge_or_halt(EVMGas(LOW))?;
	let ([op1], op2) = interpreter.stack.popn_top()?;
	*op2 = op1.overflowing_mul(*op2).0;
	ControlFlow::Continue(())
}

/// Implements the SUB instruction - subtracts two values from stack.
pub fn sub<E: Ext>(interpreter: &mut Interpreter<E>) -> ControlFlow<Halt> {
	interpreter.ext.charge_or_halt(EVMGas(VERYLOW))?;
	let ([op1], op2) = interpreter.stack.popn_top()?;
	*op2 = op1.overflowing_sub(*op2).0;
	ControlFlow::Continue(())
}

/// Implements the DIV instruction - divides two values from stack.
pub fn div<E: Ext>(interpreter: &mut Interpreter<E>) -> ControlFlow<Halt> {
	interpreter.ext.charge_or_halt(EvmOpcodeCosts::Div)?;
	let ([op1], op2) = interpreter.stack.popn_top()?;
	if !op2.is_zero() {
		*op2 = op1 / *op2;
	}
	ControlFlow::Continue(())
}

/// Implements the SDIV instruction.
///
/// Performs signed division of two values from stack.
pub fn sdiv<E: Ext>(interpreter: &mut Interpreter<E>) -> ControlFlow<Halt> {
	interpreter.ext.charge_or_halt(EvmOpcodeCosts::SDiv)?;
	let ([op1], op2) = interpreter.stack.popn_top()?;
	*op2 = i256_div(op1, *op2);
	ControlFlow::Continue(())
}
/// Implements the MOD instruction.
///
/// Pops two values from stack and pushes the remainder of their division.
pub fn rem<E: Ext>(interpreter: &mut Interpreter<E>) -> ControlFlow<Halt> {
	interpreter.ext.charge_or_halt(EvmOpcodeCosts::Mod)?;
	let ([op1], op2) = interpreter.stack.popn_top()?;
	if !op2.is_zero() {
		*op2 = op1 % *op2;
	}
	ControlFlow::Continue(())
}

/// Implements the SMOD instruction.
///
/// Performs signed modulo of two values from stack.
pub fn smod<E: Ext>(interpreter: &mut Interpreter<E>) -> ControlFlow<Halt> {
	interpreter.ext.charge_or_halt(EvmOpcodeCosts::SMod)?;
	let ([op1], op2) = interpreter.stack.popn_top()?;
	*op2 = i256_mod(op1, *op2);
	ControlFlow::Continue(())
}

/// Implements the ADDMOD instruction.
///
/// Pops three values from stack and pushes (a + b) % n.
pub fn addmod<E: Ext>(interpreter: &mut Interpreter<E>) -> ControlFlow<Halt> {
	interpreter.ext.charge_or_halt(EvmOpcodeCosts::AddMod)?;
	let ([op1, op2], op3) = interpreter.stack.popn_top()?;
	*op3 = op1.add_mod(op2, *op3);
	ControlFlow::Continue(())
}

/// Implements the MULMOD instruction.
///
/// Pops three values from stack and pushes (a * b) % n.
pub fn mulmod<E: Ext>(interpreter: &mut Interpreter<E>) -> ControlFlow<Halt> {
	interpreter.ext.charge_or_halt(EvmOpcodeCosts::MulMod)?;
	let ([op1, op2], op3) = interpreter.stack.popn_top()?;
	*op3 = op1.mul_mod(op2, *op3);
	ControlFlow::Continue(())
}

/// Implements the EXP instruction - exponentiates two values from stack.
pub fn exp<E: Ext>(interpreter: &mut Interpreter<E>) -> ControlFlow<Halt> {
	let ([op1], op2) = interpreter.stack.popn_top()?;
	let Some(gas_cost) = exp_cost(*op2) else {
		return ControlFlow::Break(Error::<E::T>::OutOfGas.into());
	};
	interpreter.ext.charge_or_halt(EVMGas(gas_cost))?;
	*op2 = op1.overflowing_pow(*op2).0;
	ControlFlow::Continue(())
}

/// Implements the `SIGNEXTEND` opcode as defined in the Ethereum Yellow Paper.
///
/// In the yellow paper `SIGNEXTEND` is defined to take two inputs, we will call them
/// `x` and `y`, and produce one output.
///
/// The first `t` bits of the output (numbering from the left, starting from 0) are
/// equal to the `t`-th bit of `y`, where `t` is equal to `256 - 8(x + 1)`.
///
/// The remaining bits of the output are equal to the corresponding bits of `y`.
///
/// **Note**: If `x >= 32` then the output is equal to `y` since `t <= 0`.
///
/// To efficiently implement this algorithm in the case `x < 32` we do the following.
///
/// Let `b` be equal to the `t`-th bit of `y` and let `s = 255 - t = 8x + 7`
/// (this is effectively the same index as `t`, but numbering the bits from the
/// right instead of the left).
///
/// We can create a bit mask which is all zeros up to and including the `t`-th bit,
/// and all ones afterwards by computing the quantity `2^s - 1`.
///
/// We can use this mask to compute the output depending on the value of `b`.
///
/// If `b == 1` then the yellow paper says the output should be all ones up to
/// and including the `t`-th bit, followed by the remaining bits of `y`; this is equal to
/// `y | !mask` where `|` is the bitwise `OR` and `!` is bitwise negation.
///
/// Similarly, if `b == 0` then the yellow paper says the output should start with all zeros,
/// then end with bits from `b`; this is equal to `y & mask` where `&` is bitwise `AND`.
pub fn signextend<E: Ext>(interpreter: &mut Interpreter<E>) -> ControlFlow<Halt> {
	interpreter.ext.charge_or_halt(EVMGas(LOW))?;
	let ([ext], x) = interpreter.stack.popn_top()?;
	// For 31 we also don't need to do anything.
	if ext < U256::from(31) {
		let ext = &ext.0[0];
		let bit_index = (8 * ext + 7) as usize;
		let bit = x.bit(bit_index);
		let mask = (U256::from(1) << bit_index) - U256::from(1);
		*x = if bit { *x | !mask } else { *x & mask };
	}
	ControlFlow::Continue(())
}

/// `EXP` opcode cost calculation.
fn exp_cost(power: U256) -> Option<u64> {
	if power.is_zero() {
		Some(EXP)
	} else {
		// EIP-160: EXP cost increase
		let gas_byte = U256::from(50);
		let gas = U256::from(EXP)
			.checked_add(gas_byte.checked_mul(U256::from(log2floor(power) / 8 + 1))?)?;

		u64::try_from(gas).ok()
	}
}

const fn log2floor(value: U256) -> u64 {
	let mut l: u64 = 256;
	let mut i = 3;
	loop {
		if value.0[i] == 0u64 {
			l -= 64;
		} else {
			l -= value.0[i].leading_zeros() as u64;
			if l == 0 {
				return l;
			} else {
				return l - 1;
			}
		}
		if i == 0 {
			break;
		}
		i -= 1;
	}
	l
}
