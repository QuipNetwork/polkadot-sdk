# PSM Pallet

A module hosting one or more Peg Stability Modules. Each PSM enables 1:1 swaps
between a specific internal stablecoin and that PSM's pre-approved external
assets on Substrate-based blockchains.

## Terminology

Throughout this pallet two distinct token roles are referenced:

- **Internal** — the stablecoin a PSM issues and burns (e.g. runtime's own USD-pegged stablecoin).
  Each PSM instance is keyed by its internal asset id; multiple instances can
  coexist, each with its own reserve, debt ceiling, fee destination and
  approved externals. Mint operations credit the user with the internal asset;
  redeem operations burn it. Fees are collected in the internal asset and
  forwarded to that instance's `PsmInfo::fee_destination`.
- **External** — third-party assets (e.g. USDC, USDT) approved on a
  specific PSM via `add_external_asset` and held in that PSM's reserve. Users
  deposit external to mint internal, and burn internal to redeem external. A
  PSM may approve multiple externals, each identified by `asset_id`.

## Overview

The PSM pallet hosts one or more PSM instances, each keyed by its internal
asset id. Each instance:

- **Holds a per-instance reserve account** derived from
  `blake2_256((PalletId::TYPE_ID, PalletId, internal_asset).encode())`.
  External assets deposited by users are held there.
- **Mints and burns its own internal asset**. Users receive the internal asset
  when depositing external assets, and burn the internal asset when
  redeeming.
- **Routes fees to the instance's `fee_destination`**. Mint and redeem fees are
  collected in the internal asset and transferred to the per-instance account
  recorded in `PsmInfo`.
- **Has independent per-external circuit breakers**. Each approved external on
  each instance can be paused without affecting others.

## Swap Lifecycle

### 1. Mint (External → Internal)

```rust
mint(origin, internal_asset, asset_id, external_amount)
```

- Deposits `external_amount` of `asset_id` into `internal_asset`'s PSM reserve
- Mints `internal_asset` to the user (minus minting fee)
- Fee is minted as `internal_asset` and transferred to the instance's `fee_destination`
- Enforces the per-instance aggregate `max_debt` and the per-external normalised ceiling
- Requires the swap (in internal units) to be `>= PsmInfo::min_swap_amount`

### 2. Redeem (Internal → External)

```rust
redeem(origin, internal_asset, asset_id, amount)
```

- Burns `amount` of `internal_asset` from the user
- Transfers external asset from the instance's reserve to the user
- Redemption fee is transferred from the user as `internal_asset` to `fee_destination`
- Limited by the per-external tracked debt (`PsmDebt`), not raw reserve balance
- Requires `amount >= PsmInfo::min_swap_amount`

## Debt Ceiling

Each PSM instance has an absolute internal-asset debt ceiling stored on
`PsmInfo::max_debt`. Within that, per-external ceilings are derived from
ceiling weights:

```
max_asset_debt(internal, external) =
    (AssetCeilingWeight[internal, external] / sum_of_weights[internal])
        * Psm[internal].max_debt
```

Setting an asset's weight to 0% disables minting for that external and
redistributes its share to the others within the same instance.

## Fee Structure

Fees are stored per `(internal_asset, external_asset)` pair, calculated using
`Permill::mul_ceil` (rounds up), and routed to the instance's `fee_destination`:

- **Minting Fee**: `fee = MintingFee[internal, external].mul_ceil(internal_equivalent)`
  -- deducted from internal-asset output, minted to `fee_destination`
- **Redemption Fee**: `fee = RedemptionFee[internal, external].mul_ceil(amount)`
  -- transferred from the user to `fee_destination`

With 0.5% fees on both sides, arbitrage opportunities exist when the internal
asset trades outside $0.995-$1.005.

## Circuit Breaker

Each approved external on each instance has an independent circuit breaker
with three levels:

| Level             | Minting | Redemption | Use Case                          |
| ----------------- | ------- | ---------- | --------------------------------- |
| `AllEnabled`      | Allowed | Allowed    | Normal operation                  |
| `MintingDisabled` | Blocked | Allowed    | Drain debt from a problematic external |
| `AllDisabled`     | Blocked | Blocked    | Full emergency halt of an external |

`set_asset_status` is callable at both the `Full` (`full_admin`) and
`Emergency` (`emergency_admin`) levels.

## Governance Operations

All governance extrinsics take `internal_asset` as the first parameter to
identify the PSM instance being configured.

| Extrinsic | Required Level | Description |
| --- | --- | --- |
| `set_minting_fee(internal_asset, asset_id, fee)` | Full | Update minting fee for the pair |
| `set_redemption_fee(internal_asset, asset_id, fee)` | Full | Update redemption fee for the pair |
| `set_max_debt(internal_asset, value)` | Full or Emergency | Update absolute debt ceiling for the PSM |
| `set_asset_ceiling_weight(internal_asset, asset_id, weight)` | Full or Emergency | Update external ceiling weight |
| `set_asset_status(internal_asset, asset_id, status)` | Full or Emergency | Set per-external circuit breaker level |
| `add_external_asset(internal_asset, asset_id)` | Full | Approve external on a PSM |
| `remove_external_asset(internal_asset, asset_id)` | Full | Remove external from a PSM (zero debt) |

### Privilege Levels

Each PSM instance stores two admin origins, both set to the creator on `create_psm`
and reassignable by the `full_admin`. An incoming origin is matched against them to
resolve a privilege level:

- **Full** (the `full_admin`): can modify all parameters, approve/remove externals,
  reassign either admin, and remove the instance
- **Emergency** (the `emergency_admin`): can modify circuit breaker status, ceiling
  weights, and debt ceilings only

### Asset Offboarding Workflow

For an external `asset_id` on instance `internal_asset`:

1. Set the external's ceiling weight to `0%` (or use `set_asset_status(.., MintingDisabled)`):
   either pauses new minting while still allowing redemptions
2. Redemptions slowly drain `PsmDebt[internal_asset, asset_id]`
3. Once debt reaches zero, call `remove_external_asset(internal_asset, asset_id)`

Lowering a ceiling weight (or `max_debt`) below outstanding debt is allowed: the ceiling is a
mint-time throttle, so the external simply cannot be minted until redemptions bring its debt
back under the new ceiling.

### Asset Onboarding Requirements

Before calling `add_external_asset(internal_asset, asset_id)`:

- A PSM must already be registered for `internal_asset`
- The external `asset_id` must already exist in the `Fungibles` implementation
- The internal asset's live decimals must still match the snapshot in `PsmInfo`
- `|external_decimals − internal_decimals|` must be within `MAX_DECIMALS_DIFF`
- The PSM must still be below `MaxExternals`

After `add_external_asset`, the external starts with an `AssetCeilingWeight` of `0%`, so its
per-external ceiling is zero and **minting is disabled**. Before the first mint, call
`set_asset_ceiling_weight(internal_asset, asset_id, weight)` with a non-zero weight (and
optionally `set_minting_fee` / `set_redemption_fee`, which otherwise default to 0.5%).
Skipping this step makes the first mint fail with `ExceedsMaxPsmDebt`.

## Configuration

```rust
impl pallet_psm::Config for Runtime {
    type Fungibles = Assets;
    type Currency = Balances;
    type RuntimeOrigin = RuntimeOrigin;
    type PalletsOrigin = OriginCaller;
    type AssetId = u32;
    type WeightInfo = weights::SubstrateWeight<Runtime>;
    type PalletId = PsmPalletId;
    type MaxExternals = ConstU32<10>;
    type CreationDeposit = PsmCreationDeposit;
}
```

`Fungibles` must expose metadata for both internal and external assets, because
`add_external_asset` snapshots the external's decimals and the pallet validates
on every swap that live decimals still match.

### Per-Instance Parameters (Set via Governance)

| Parameter            | Description                                  | Suggested Value         |
| -------------------- | -------------------------------------------- | ----------------------- |
| `PsmInfo::max_debt`  | Absolute internal-asset debt ceiling         | Per-instance, governance-set |
| `PsmInfo::min_swap_amount` | Minimum swap amount in internal-asset units | Per-instance, set on `create_psm` |
| `MintingFee`         | Fee for external → internal (per pair)       | 0.5%                    |
| `RedemptionFee`      | Fee for internal → external (per pair)       | 0.5%                    |
| `AssetCeilingWeight` | Per-external share of the PSM's `max_debt`   | e.g. 50%/50% (USDC/USDT) |

### Required Config Constants

- `PalletId`: Unique identifier; sub-accounts are derived per instance.
- `MaxExternals`: Maximum number of approved externals per PSM instance.

The per-instance minimum swap amount is not a config constant — it is set on `create_psm`
and stored in `PsmInfo::min_swap_amount`.

## Events

All events carry `internal_asset` so consumers can attribute them to the correct PSM.

- `Minted { internal_asset, who, asset_id, external_amount, received, fee }`
- `Redeemed { internal_asset, who, asset_id, paid, external_received, fee }`
- `MintingFeeUpdated { internal_asset, asset_id, old_value, new_value }`
- `RedemptionFeeUpdated { internal_asset, asset_id, old_value, new_value }`
- `MaxDebtUpdated { internal_asset, old_value, new_value }`
- `AssetCeilingWeightUpdated { internal_asset, asset_id, old_value, new_value }`
- `AssetStatusUpdated { internal_asset, asset_id, status }`
- `ExternalAssetAdded { internal_asset, asset_id }`
- `ExternalAssetRemoved { internal_asset, asset_id }`

## Errors

- `InsufficientReserve`: PSM doesn't have enough external asset for redemption
- `ExceedsMaxPsmDebt`: Mint would exceed the instance's aggregate or per-external ceiling
- `BelowMinimumSwap`: Swap amount below the instance's `min_swap_amount`
- `MintingStopped`: Minting disabled by the per-external circuit breaker
- `AllSwapsStopped`: All swaps disabled by the per-external circuit breaker
- `UnsupportedAsset`: External not approved on this PSM
- `PsmNotFound`: No PSM registered for `internal_asset`
- `AssetAlreadyApproved`: External already approved on this PSM
- `AssetDoesNotExist`: External does not exist in the fungibles backend
- `AssetNotApproved`: External not approved (governance path)
- `AssetHasDebt`: Cannot remove an external with outstanding debt
- `InsufficientPrivilege`: Emergency origin attempted a Full-only operation
- `TooManyAssets`: PSM at `MaxExternals`
- `DecimalsMismatch`: Live decimals diverged from the registration snapshot
- `DecimalsRangeExceeded`: `|external_decimals − internal_decimals|` exceeds `MAX_DECIMALS_DIFF`
- `ConversionOverflow`: Decimal scaling overflowed
- `AmountTooSmallAfterConversion`: Counter-asset conversion rounds to zero
- `Unexpected`: An unexpected invariant violation occurred (defensive check)

## Testing

Run tests with:

```bash
SKIP_WASM_BUILD=1 cargo test -p pallet-psm
```
