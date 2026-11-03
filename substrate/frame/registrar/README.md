# Parachain registrar

- `para/` — `pallet-registrar-para`: user facing control-plane, runs on a parachain.
- `relay/` — `pallet-registrar-relay`: receives messages from the parachain, runs on the relay chain.
- `primitives/` — `registrar-primitives`: shared parachain<->relay XCM message types.
