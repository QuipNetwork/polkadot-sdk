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

//! Tests for the claims pallet.

#[cfg(test)]
use super::*;
use crate::{claims, claims::mock::*};
use claims::Call as ClaimsCall;
use hex_literal::hex;
use secp_utils::*;
use sp_runtime::transaction_validity::TransactionSource::External;

use codec::Encode;
// The testing primitives are very useful for avoiding having to work with signatures
// or public keys. `u64` is used as the `AccountId` and no `Signature`s are required.
use frame_support::{
	assert_err, assert_noop, assert_ok,
	dispatch::{GetDispatchInfo, Pays},
	traits::ExistenceRequirement,
};
use sp_runtime::{
	traits::DispatchTransaction, transaction_validity::TransactionLongevity,
	DispatchError::BadOrigin, TokenError,
};

#[test]
fn basic_setup_works() {
	new_test_ext().execute_with(|| {
		assert_eq!(claims::Total::<Test>::get(), total_claims());
		assert_eq!(claims::Claims::<Test>::get(&eth(&alice())), Some(100));
		assert_eq!(claims::Claims::<Test>::get(&eth(&dave())), Some(200));
		assert_eq!(claims::Claims::<Test>::get(&eth(&eve())), Some(300));
		assert_eq!(claims::Claims::<Test>::get(&eth(&frank())), Some(400));
		assert_eq!(claims::Claims::<Test>::get(&EthereumAddress::default()), None);
		assert_eq!(claims::Vesting::<Test>::get(&eth(&alice())), Some((50, 10, 1)));
	});
}

#[test]
fn serde_works() {
	let x = EthereumAddress(hex!["0123456789abcdef0123456789abcdef01234567"]);
	let y = serde_json::to_string(&x).unwrap();
	assert_eq!(y, "\"0x0123456789abcdef0123456789abcdef01234567\"");
	let z: EthereumAddress = serde_json::from_str(&y).unwrap();
	assert_eq!(x, z);
}

#[test]
fn claiming_works() {
	new_test_ext().execute_with(|| {
		assert_eq!(Balances::free_balance(42), 0);
		assert_ok!(claims::mock::Claims::claim(
			RuntimeOrigin::none(),
			42,
			sig::<Test>(&alice(), &42u64.encode(), &[][..])
		));
		assert_eq!(Balances::free_balance(&42), 100);
		assert_eq!(claims::mock::Vesting::vesting_balance(&42), Some(50));
		assert_eq!(claims::Total::<Test>::get(), total_claims() - 100);
	});
}

#[test]
fn basic_claim_moving_works() {
	new_test_ext().execute_with(|| {
		assert_eq!(Balances::free_balance(42), 0);
		assert_noop!(
			claims::mock::Claims::move_claim(
				RuntimeOrigin::signed(1),
				eth(&alice()),
				eth(&bob()),
				None
			),
			BadOrigin
		);
		assert_ok!(claims::mock::Claims::move_claim(
			RuntimeOrigin::signed(6),
			eth(&alice()),
			eth(&bob()),
			None
		));
		assert_noop!(
			claims::mock::Claims::claim(
				RuntimeOrigin::none(),
				42,
				sig::<Test>(&alice(), &42u64.encode(), &[][..])
			),
			Error::<Test>::SignerHasNoClaim
		);
		assert_ok!(claims::mock::Claims::claim(
			RuntimeOrigin::none(),
			42,
			sig::<Test>(&bob(), &42u64.encode(), &[][..])
		));
		assert_eq!(Balances::free_balance(&42), 100);
		assert_eq!(claims::mock::Vesting::vesting_balance(&42), Some(50));
		assert_eq!(claims::Total::<Test>::get(), total_claims() - 100);
	});
}

#[test]
fn claim_attest_moving_works() {
	new_test_ext().execute_with(|| {
		assert_ok!(claims::mock::Claims::move_claim(
			RuntimeOrigin::signed(6),
			eth(&dave()),
			eth(&bob()),
			None
		));
		let s = sig::<Test>(&bob(), &42u64.encode(), StatementKind::Regular.to_text());
		assert_ok!(claims::mock::Claims::claim_attest(
			RuntimeOrigin::none(),
			42,
			s,
			StatementKind::Regular.to_text().to_vec()
		));
		assert_eq!(Balances::free_balance(&42), 200);
	});
}

#[test]
fn attest_moving_works() {
	new_test_ext().execute_with(|| {
		assert_ok!(claims::mock::Claims::move_claim(
			RuntimeOrigin::signed(6),
			eth(&eve()),
			eth(&bob()),
			Some(42)
		));
		assert_ok!(claims::mock::Claims::attest(
			RuntimeOrigin::signed(42),
			StatementKind::Saft.to_text().to_vec()
		));
		assert_eq!(Balances::free_balance(&42), 300);
	});
}

#[test]
fn claiming_does_not_bypass_signing() {
	new_test_ext().execute_with(|| {
		assert_ok!(claims::mock::Claims::claim(
			RuntimeOrigin::none(),
			42,
			sig::<Test>(&alice(), &42u64.encode(), &[][..])
		));
		assert_noop!(
			claims::mock::Claims::claim(
				RuntimeOrigin::none(),
				42,
				sig::<Test>(&dave(), &42u64.encode(), &[][..])
			),
			Error::<Test>::InvalidStatement,
		);
		assert_noop!(
			claims::mock::Claims::claim(
				RuntimeOrigin::none(),
				42,
				sig::<Test>(&eve(), &42u64.encode(), &[][..])
			),
			Error::<Test>::InvalidStatement,
		);
		assert_ok!(claims::mock::Claims::claim(
			RuntimeOrigin::none(),
			42,
			sig::<Test>(&frank(), &42u64.encode(), &[][..])
		));
	});
}

#[test]
fn attest_claiming_works() {
	new_test_ext().execute_with(|| {
		assert_eq!(Balances::free_balance(42), 0);
		let s = sig::<Test>(&dave(), &42u64.encode(), StatementKind::Saft.to_text());
		let r = claims::mock::Claims::claim_attest(
			RuntimeOrigin::none(),
			42,
			s.clone(),
			StatementKind::Saft.to_text().to_vec(),
		);
		assert_noop!(r, Error::<Test>::InvalidStatement);

		let r = claims::mock::Claims::claim_attest(
			RuntimeOrigin::none(),
			42,
			s,
			StatementKind::Regular.to_text().to_vec(),
		);
		assert_noop!(r, Error::<Test>::SignerHasNoClaim);
		// ^^^ we use ecdsa_recover, so an invalid signature just results in a random signer id
		// being recovered, which realistically will never have a claim.

		let s = sig::<Test>(&dave(), &42u64.encode(), StatementKind::Regular.to_text());
		assert_ok!(claims::mock::Claims::claim_attest(
			RuntimeOrigin::none(),
			42,
			s,
			StatementKind::Regular.to_text().to_vec()
		));
		assert_eq!(Balances::free_balance(&42), 200);
		assert_eq!(claims::Total::<Test>::get(), total_claims() - 200);

		let s = sig::<Test>(&dave(), &42u64.encode(), StatementKind::Regular.to_text());
		let r = claims::mock::Claims::claim_attest(
			RuntimeOrigin::none(),
			42,
			s,
			StatementKind::Regular.to_text().to_vec(),
		);
		assert_noop!(r, Error::<Test>::SignerHasNoClaim);
	});
}

#[test]
fn attesting_works() {
	new_test_ext().execute_with(|| {
		assert_eq!(Balances::free_balance(42), 0);
		assert_noop!(
			claims::mock::Claims::attest(
				RuntimeOrigin::signed(69),
				StatementKind::Saft.to_text().to_vec()
			),
			Error::<Test>::SenderHasNoClaim
		);
		assert_noop!(
			claims::mock::Claims::attest(
				RuntimeOrigin::signed(42),
				StatementKind::Regular.to_text().to_vec()
			),
			Error::<Test>::InvalidStatement
		);
		assert_ok!(claims::mock::Claims::attest(
			RuntimeOrigin::signed(42),
			StatementKind::Saft.to_text().to_vec()
		));
		assert_eq!(Balances::free_balance(&42), 300);
		assert_eq!(claims::Total::<Test>::get(), total_claims() - 300);
	});
}

#[test]
fn claim_cannot_clobber_preclaim() {
	new_test_ext().execute_with(|| {
		assert_eq!(Balances::free_balance(42), 0);
		// Alice's claim is 100
		assert_ok!(claims::mock::Claims::claim(
			RuntimeOrigin::none(),
			42,
			sig::<Test>(&alice(), &42u64.encode(), &[][..])
		));
		assert_eq!(Balances::free_balance(&42), 100);
		// Eve's claim is 300 through Account 42
		assert_ok!(claims::mock::Claims::attest(
			RuntimeOrigin::signed(42),
			StatementKind::Saft.to_text().to_vec()
		));
		assert_eq!(Balances::free_balance(&42), 100 + 300);
		assert_eq!(claims::Total::<Test>::get(), total_claims() - 400);
	});
}

#[test]
fn valid_attest_transactions_are_free() {
	new_test_ext().execute_with(|| {
		let p = PrevalidateAttests::<Test>::new();
		let c = claims::mock::RuntimeCall::Claims(ClaimsCall::attest {
			statement: StatementKind::Saft.to_text().to_vec(),
		});
		let di = c.get_dispatch_info();
		assert_eq!(di.pays_fee, Pays::No);
		let r = p.validate_only(Some(42).into(), &c, &di, 20, External, 0);
		assert_eq!(r.unwrap().0, ValidTransaction::default());
	});
}

#[test]
fn invalid_attest_transactions_are_recognized() {
	new_test_ext().execute_with(|| {
		let p = PrevalidateAttests::<Test>::new();
		let c = claims::mock::RuntimeCall::Claims(ClaimsCall::attest {
			statement: StatementKind::Regular.to_text().to_vec(),
		});
		let di = c.get_dispatch_info();
		let r = p.validate_only(Some(42).into(), &c, &di, 20, External, 0);
		assert!(r.is_err());
		let c = claims::mock::RuntimeCall::Claims(ClaimsCall::attest {
			statement: StatementKind::Saft.to_text().to_vec(),
		});
		let di = c.get_dispatch_info();
		let r = p.validate_only(Some(69).into(), &c, &di, 20, External, 0);
		assert!(r.is_err());
	});
}

#[test]
fn cannot_bypass_attest_claiming() {
	new_test_ext().execute_with(|| {
		assert_eq!(Balances::free_balance(42), 0);
		let s = sig::<Test>(&dave(), &42u64.encode(), &[]);
		let r = claims::mock::Claims::claim(RuntimeOrigin::none(), 42, s.clone());
		assert_noop!(r, Error::<Test>::InvalidStatement);
	});
}

#[test]
fn add_claim_works() {
	new_test_ext().execute_with(|| {
		assert_noop!(
			claims::mock::Claims::mint_claim(
				RuntimeOrigin::signed(42),
				eth(&bob()),
				200,
				None,
				None
			),
			sp_runtime::traits::BadOrigin,
		);
		assert_eq!(Balances::free_balance(42), 0);
		assert_noop!(
			claims::mock::Claims::claim(
				RuntimeOrigin::none(),
				69,
				sig::<Test>(&bob(), &69u64.encode(), &[][..])
			),
			Error::<Test>::SignerHasNoClaim,
		);
		assert_ok!(claims::mock::Claims::mint_claim(
			RuntimeOrigin::root(),
			eth(&bob()),
			200,
			None,
			None
		));
		assert_eq!(claims::Total::<Test>::get(), total_claims() + 200);
		assert_ok!(claims::mock::Claims::claim(
			RuntimeOrigin::none(),
			69,
			sig::<Test>(&bob(), &69u64.encode(), &[][..])
		));
		assert_eq!(Balances::free_balance(&69), 200);
		assert_eq!(claims::mock::Vesting::vesting_balance(&69), None);
		assert_eq!(claims::Total::<Test>::get(), total_claims());
	});
}

#[test]
fn add_claim_with_vesting_works() {
	new_test_ext().execute_with(|| {
		assert_noop!(
			claims::mock::Claims::mint_claim(
				RuntimeOrigin::signed(42),
				eth(&bob()),
				200,
				Some((50, 10, 1)),
				None
			),
			sp_runtime::traits::BadOrigin,
		);
		assert_eq!(Balances::free_balance(42), 0);
		assert_noop!(
			claims::mock::Claims::claim(
				RuntimeOrigin::none(),
				69,
				sig::<Test>(&bob(), &69u64.encode(), &[][..])
			),
			Error::<Test>::SignerHasNoClaim,
		);
		assert_ok!(claims::mock::Claims::mint_claim(
			RuntimeOrigin::root(),
			eth(&bob()),
			200,
			Some((50, 10, 1)),
			None
		));
		assert_ok!(claims::mock::Claims::claim(
			RuntimeOrigin::none(),
			69,
			sig::<Test>(&bob(), &69u64.encode(), &[][..])
		));
		assert_eq!(Balances::free_balance(&69), 200);
		assert_eq!(claims::mock::Vesting::vesting_balance(&69), Some(50));

		// Make sure we can not transfer the vested balance.
		assert_err!(
			<Balances as Currency<_>>::transfer(&69, &80, 180, ExistenceRequirement::AllowDeath),
			TokenError::Frozen,
		);
	});
}

#[test]
fn claim_rejects_sub_ed_vesting_claim_into_empty_account() {
	new_test_ext().execute_with(|| {
		// The mock's existential deposit is 1, so a value of 0 is below it. A vesting claim into an
		// empty account whose resulting balance would be below ED must be rejected, otherwise the
		// account would be dusted and the vesting lock placed on a dead account.
		claims::Claims::<Test>::insert(eth(&bob()), 0);
		claims::Vesting::<Test>::insert(eth(&bob()), (0, 0, 1));
		assert_eq!(Balances::free_balance(&69), 0);
		assert_noop!(
			claims::mock::Claims::claim(
				RuntimeOrigin::none(),
				69,
				sig::<Test>(&bob(), &69u64.encode(), &[][..])
			),
			Error::<Test>::ClaimBelowExistentialDeposit,
		);
		// The guard fires before `deposit_creating`, so no balance was created.
		assert_eq!(Balances::free_balance(&69), 0);
	});
}

#[test]
fn claim_allows_sub_ed_vesting_claim_into_funded_account() {
	new_test_ext().execute_with(|| {
		// A sub-ED claim is fine if the destination already holds funds: the resulting balance
		// stays above ED, so the account is not dusted and can carry the vesting lock.
		let _ = <Balances as Currency<_>>::deposit_creating(&69, 100);
		claims::Claims::<Test>::insert(eth(&bob()), 0); // claim value (0) is below ED
		claims::Vesting::<Test>::insert(eth(&bob()), (50, 10, 1));
		assert_ok!(claims::mock::Claims::claim(
			RuntimeOrigin::none(),
			69,
			sig::<Test>(&bob(), &69u64.encode(), &[][..])
		));
		assert_eq!(Balances::free_balance(&69), 100); // balance unchanged
		assert_eq!(claims::mock::Vesting::vesting_balance(&69), Some(50)); // vesting lock applied
	});
}

#[test]
fn add_claim_with_statement_works() {
	new_test_ext().execute_with(|| {
		assert_noop!(
			claims::mock::Claims::mint_claim(
				RuntimeOrigin::signed(42),
				eth(&bob()),
				200,
				None,
				Some(StatementKind::Regular)
			),
			sp_runtime::traits::BadOrigin,
		);
		assert_eq!(Balances::free_balance(42), 0);
		let signature = sig::<Test>(&bob(), &69u64.encode(), StatementKind::Regular.to_text());
		assert_noop!(
			claims::mock::Claims::claim_attest(
				RuntimeOrigin::none(),
				69,
				signature.clone(),
				StatementKind::Regular.to_text().to_vec()
			),
			Error::<Test>::SignerHasNoClaim
		);
		assert_ok!(claims::mock::Claims::mint_claim(
			RuntimeOrigin::root(),
			eth(&bob()),
			200,
			None,
			Some(StatementKind::Regular)
		));
		assert_noop!(
			claims::mock::Claims::claim_attest(
				RuntimeOrigin::none(),
				69,
				signature.clone(),
				vec![],
			),
			Error::<Test>::SignerHasNoClaim
		);
		assert_ok!(claims::mock::Claims::claim_attest(
			RuntimeOrigin::none(),
			69,
			signature.clone(),
			StatementKind::Regular.to_text().to_vec()
		));
		assert_eq!(Balances::free_balance(&69), 200);
	});
}

#[test]
fn origin_signed_claiming_fail() {
	new_test_ext().execute_with(|| {
		assert_eq!(Balances::free_balance(42), 0);
		assert_err!(
			claims::mock::Claims::claim(
				RuntimeOrigin::signed(42),
				42,
				sig::<Test>(&alice(), &42u64.encode(), &[][..])
			),
			sp_runtime::traits::BadOrigin,
		);
	});
}

#[test]
fn double_claiming_doesnt_work() {
	new_test_ext().execute_with(|| {
		assert_eq!(Balances::free_balance(42), 0);
		assert_ok!(claims::mock::Claims::claim(
			RuntimeOrigin::none(),
			42,
			sig::<Test>(&alice(), &42u64.encode(), &[][..])
		));
		assert_noop!(
			claims::mock::Claims::claim(
				RuntimeOrigin::none(),
				42,
				sig::<Test>(&alice(), &42u64.encode(), &[][..])
			),
			Error::<Test>::SignerHasNoClaim
		);
	});
}

#[test]
fn claiming_while_vested_doesnt_work() {
	new_test_ext().execute_with(|| {
		CurrencyOf::<Test>::make_free_balance_be(&69, total_claims());
		assert_eq!(Balances::free_balance(69), total_claims());
		// A user is already vested
		assert_ok!(<Test as Config>::VestingSchedule::add_vesting_schedule(
			&69,
			total_claims(),
			100,
			10
		));
		assert_ok!(claims::mock::Claims::mint_claim(
			RuntimeOrigin::root(),
			eth(&bob()),
			200,
			Some((50, 10, 1)),
			None
		));
		// New total
		assert_eq!(claims::Total::<Test>::get(), total_claims() + 200);

		// They should not be able to claim
		assert_noop!(
			claims::mock::Claims::claim(
				RuntimeOrigin::none(),
				69,
				sig::<Test>(&bob(), &69u64.encode(), &[][..])
			),
			Error::<Test>::VestedBalanceExists,
		);
	});
}

#[test]
fn non_sender_sig_doesnt_work() {
	new_test_ext().execute_with(|| {
		assert_eq!(Balances::free_balance(42), 0);
		assert_noop!(
			claims::mock::Claims::claim(
				RuntimeOrigin::none(),
				42,
				sig::<Test>(&alice(), &69u64.encode(), &[][..])
			),
			Error::<Test>::SignerHasNoClaim
		);
	});
}

#[test]
fn non_claimant_doesnt_work() {
	new_test_ext().execute_with(|| {
		assert_eq!(Balances::free_balance(42), 0);
		assert_noop!(
			claims::mock::Claims::claim(
				RuntimeOrigin::none(),
				42,
				sig::<Test>(&bob(), &69u64.encode(), &[][..])
			),
			Error::<Test>::SignerHasNoClaim
		);
	});
}

#[test]
fn real_eth_sig_works() {
	new_test_ext().execute_with(|| {
			// "Pay RUSTs to the TEST account:2a00000000000000"
			let sig = hex!["444023e89b67e67c0562ed0305d252a5dd12b2af5ac51d6d3cb69a0b486bc4b3191401802dc29d26d586221f7256cd3329fe82174bdf659baea149a40e1c495d1c"];
			let sig = EcdsaSignature(sig);
			let who = 42u64.using_encoded(to_ascii_hex);
			let signer = claims::mock::Claims::eth_recover(&sig, &who, &[][..]).unwrap();
			assert_eq!(signer.0, hex!["6d31165d5d932d571f3b44695653b46dcc327e84"]);
		});
}

#[test]
#[allow(deprecated)]
fn validate_unsigned_works() {
	use sp_runtime::traits::ValidateUnsigned;
	let source = sp_runtime::transaction_validity::TransactionSource::External;

	new_test_ext().execute_with(|| {
		assert_eq!(
			Pallet::<Test>::validate_unsigned(
				source,
				&ClaimsCall::claim {
					dest: 1,
					ethereum_signature: sig::<Test>(&alice(), &1u64.encode(), &[][..])
				}
			),
			Ok(ValidTransaction {
				priority: 100,
				requires: vec![],
				provides: vec![("claims", eth(&alice())).encode()],
				longevity: TransactionLongevity::max_value(),
				propagate: true,
			})
		);
		assert_eq!(
			Pallet::<Test>::validate_unsigned(
				source,
				&ClaimsCall::claim { dest: 0, ethereum_signature: EcdsaSignature([0; 65]) }
			),
			InvalidTransaction::Custom(ValidityError::InvalidEthereumSignature.into()).into(),
		);
		assert_eq!(
			Pallet::<Test>::validate_unsigned(
				source,
				&ClaimsCall::claim {
					dest: 1,
					ethereum_signature: sig::<Test>(&bob(), &1u64.encode(), &[][..])
				}
			),
			InvalidTransaction::Custom(ValidityError::SignerHasNoClaim.into()).into(),
		);
		let s = sig::<Test>(&dave(), &1u64.encode(), StatementKind::Regular.to_text());
		let call = ClaimsCall::claim_attest {
			dest: 1,
			ethereum_signature: s,
			statement: StatementKind::Regular.to_text().to_vec(),
		};
		assert_eq!(
			Pallet::<Test>::validate_unsigned(source, &call),
			Ok(ValidTransaction {
				priority: 100,
				requires: vec![],
				provides: vec![("claims", eth(&dave())).encode()],
				longevity: TransactionLongevity::max_value(),
				propagate: true,
			})
		);
		assert_eq!(
			Pallet::<Test>::validate_unsigned(
				source,
				&ClaimsCall::claim_attest {
					dest: 1,
					ethereum_signature: EcdsaSignature([0; 65]),
					statement: StatementKind::Regular.to_text().to_vec()
				}
			),
			InvalidTransaction::Custom(ValidityError::InvalidEthereumSignature.into()).into(),
		);

		let s = sig::<Test>(&bob(), &1u64.encode(), StatementKind::Regular.to_text());
		let call = ClaimsCall::claim_attest {
			dest: 1,
			ethereum_signature: s,
			statement: StatementKind::Regular.to_text().to_vec(),
		};
		assert_eq!(
			Pallet::<Test>::validate_unsigned(source, &call),
			InvalidTransaction::Custom(ValidityError::SignerHasNoClaim.into()).into(),
		);

		let s = sig::<Test>(&dave(), &1u64.encode(), StatementKind::Saft.to_text());
		let call = ClaimsCall::claim_attest {
			dest: 1,
			ethereum_signature: s,
			statement: StatementKind::Regular.to_text().to_vec(),
		};
		assert_eq!(
			Pallet::<Test>::validate_unsigned(source, &call),
			InvalidTransaction::Custom(ValidityError::SignerHasNoClaim.into()).into(),
		);

		let s = sig::<Test>(&dave(), &1u64.encode(), StatementKind::Saft.to_text());
		let call = ClaimsCall::claim_attest {
			dest: 1,
			ethereum_signature: s,
			statement: StatementKind::Saft.to_text().to_vec(),
		};
		assert_eq!(
			Pallet::<Test>::validate_unsigned(source, &call),
			InvalidTransaction::Custom(ValidityError::InvalidStatement.into()).into(),
		);
	});
}
