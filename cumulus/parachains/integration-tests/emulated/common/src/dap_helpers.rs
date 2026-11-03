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

use frame_support::{
	assert_ok,
	traits::{
		fungible::{Inspect as FungibleInspect, Mutate as FungibleMutate},
		Get, Hooks,
	},
	weights::Weight,
};
use parachains_common::{AccountId, Balance};

use xcm_emulator::{Chain, TestExt};

/// Tests that the accumulate-and-forward pallet accumulates native tokens, teleports them to
/// the staging account of `pallet-dap` on AssetHub, and that `pallet-dap`'s `on_idle`
/// subsequently drains and deactivates those funds into the main DAP buffer account.
pub fn test_accumulate_forward_transfers_to_asset_hub<Sender, AH>()
where
	Sender: Chain + TestExt,
	Sender::Runtime: pallet_accumulate_and_forward::Config
		+ pallet_balances::Config<Balance = Balance>
		+ frame_system::Config<AccountId = AccountId>,
	Sender::RuntimeEvent: TryInto<pallet_accumulate_and_forward::Event<Sender::Runtime>>,
	pallet_accumulate_and_forward::Pallet<Sender::Runtime>: Hooks<u32>,
	<Sender::Runtime as pallet_accumulate_and_forward::Config>::MinTransferAmount: Get<Balance>,
	AH: Chain + TestExt,
	AH::Runtime: pallet_xcm::Config
		+ pallet_dap::Config
		+ pallet_balances::Config<Balance = Balance>
		+ pallet_message_queue::Config
		+ frame_system::Config<AccountId = AccountId>,
	AH::RuntimeEvent: TryInto<pallet_message_queue::Event<AH::Runtime>>,
	pallet_dap::Pallet<AH::Runtime>: Hooks<u32>,
{
	let sender_ed = <Sender::Runtime as pallet_balances::Config>::ExistentialDeposit::get();
	let ah_ed = <AH::Runtime as pallet_balances::Config>::ExistentialDeposit::get();
	let accumulation_account =
		pallet_accumulate_and_forward::Pallet::<Sender::Runtime>::accumulation_account();
	let dap_staging_account: AccountId = pallet_dap::Pallet::<AH::Runtime>::staging_account();
	let dap_buffer_account: AccountId = pallet_dap::Pallet::<AH::Runtime>::buffer_account();

	// The fund amount should slightly exceed MinTransferAmount to trigger a transfer.
	let fund_amount =
		<Sender::Runtime as pallet_accumulate_and_forward::Config>::MinTransferAmount::get() + 1;

	// Pre-fund AH's CheckingAccount, as during testing the sender mints its own tokens rather
	// than receiving them from AH via teleport (which would normally accrue them).
	let check_account: AccountId =
		AH::execute_with(|| pallet_xcm::Pallet::<AH::Runtime>::check_account());
	AH::execute_with(|| {
		assert_ok!(pallet_balances::Pallet::<AH::Runtime>::mint_into(
			&check_account,
			fund_amount + ah_ed
		));
	});

	let (ah_total_issuance_before, ah_inactive_issuance_before, buffer_balance_before) =
		AH::execute_with(|| {
			(
				pallet_balances::Pallet::<AH::Runtime>::total_issuance(),
				pallet_balances::Pallet::<AH::Runtime>::inactive_issuance(),
				pallet_balances::Pallet::<AH::Runtime>::balance(&dap_buffer_account),
			)
		});

	// Fund and forward in the same block: every emulated block ends with `on_idle`, which would
	// forward the funds before this block gets to observe it.
	let (available_funds, sender_total_issuance_before) = Sender::execute_with(|| {
		assert_ok!(pallet_balances::Pallet::<Sender::Runtime>::mint_into(
			&accumulation_account,
			sender_ed + fund_amount
		));
		let available_funds =
			pallet_balances::Pallet::<Sender::Runtime>::balance(&accumulation_account) - sender_ed;
		let total_issuance_before = pallet_balances::Pallet::<Sender::Runtime>::total_issuance();

		let _ = <pallet_accumulate_and_forward::Pallet<Sender::Runtime> as Hooks<u32>>::on_idle(
			1,
			Weight::MAX,
		);
		let forward_succeeded = Sender::events().into_iter().any(|e| {
			matches!(
				e.try_into(),
				Ok(pallet_accumulate_and_forward::Event::ForwardSucceeded { .. })
			)
		});
		assert!(forward_succeeded, "Expected AccumulateForward::ForwardSucceeded event");

		(available_funds, total_issuance_before)
	});

	// Delivery fees are waived for the accumulation account, so it retains exactly the ED.
	let accumulation_balance_after = Sender::account_data_of(accumulation_account).free;
	assert_eq!(accumulation_balance_after, sender_ed);

	let sender_total_issuance_after =
		Sender::execute_with(|| pallet_balances::Pallet::<Sender::Runtime>::total_issuance());
	assert_eq!(sender_total_issuance_after, sender_total_issuance_before - available_funds);

	// The XCM message is delivered to AH on the first execute_with call. Funds land in the
	// DAP staging account; the buffer and inactive issuance are unchanged at this point.
	let amount_received = AH::execute_with(|| {
		let mq_processed = AH::events().into_iter().any(|e| {
			matches!(e.try_into(), Ok(pallet_message_queue::Event::Processed { success: true, .. }))
		});
		assert!(mq_processed, "Expected MessageQueue::Processed(success: true) on AssetHub");

		let staging_balance = pallet_balances::Pallet::<AH::Runtime>::balance(&dap_staging_account);
		let ah_ed_balance = <AH::Runtime as pallet_balances::Config>::ExistentialDeposit::get();
		let received = staging_balance.saturating_sub(ah_ed_balance);
		assert!(received > 0, "DAP staging account should have received funds");

		// Buffer and inactive issuance are still unchanged — drain hasn't happened yet.
		assert_eq!(
			pallet_balances::Pallet::<AH::Runtime>::balance(&dap_buffer_account),
			buffer_balance_before,
		);
		assert_eq!(
			pallet_balances::Pallet::<AH::Runtime>::inactive_issuance(),
			ah_inactive_issuance_before,
		);

		// Total issuance is unchanged (teleport is burn-on-send / mint-on-receive).
		assert_eq!(
			pallet_balances::Pallet::<AH::Runtime>::total_issuance(),
			ah_total_issuance_before
		);

		received
	});

	// Trigger `pallet_dap::on_idle` to drain the staging account into the buffer and
	// deactivate the funds.
	AH::execute_with(|| {
		let _ = <pallet_dap::Pallet<AH::Runtime> as Hooks<u32>>::on_idle(1, Weight::MAX);

		let buffer_balance_after =
			pallet_balances::Pallet::<AH::Runtime>::balance(&dap_buffer_account);
		assert_eq!(buffer_balance_after, buffer_balance_before + amount_received);

		assert_eq!(
			pallet_balances::Pallet::<AH::Runtime>::inactive_issuance(),
			ah_inactive_issuance_before + amount_received
		);
	});
}
