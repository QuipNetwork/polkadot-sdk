# HRMP channels

- `para/` — `pallet-hrmp-para`: user facing control-plane, runs on a parachain.
- `relay/` — `pallet-hrmp-relay`: receives messages from the parachain, runs on the relay chain.
- `primitives/` — `hrmp-primitives`: shared parachain<->relay XCM message types.
