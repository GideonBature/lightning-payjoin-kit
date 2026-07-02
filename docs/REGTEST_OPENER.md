utj# Regtest Channel Opener

`lpk-regtest-opener` is a runnable product demo for proving the
`lightning-payjoin-kit` privacy-input flow on Bitcoin Core regtest.

It does five things:

1. Creates a temporary regtest wallet.
2. Mines spendable regtest funds.
3. Builds a normal single-funder channel funding transaction for comparison.
4. Coordinates a privacy-input channel funding transaction through the mock async directory.
5. Verifies the simulated Lightning funding handoff, broadcasts the private transaction, and mines it.

The normal control transaction is signed but not broadcast. The private
transaction is broadcast by default.

## Folder Placement

The runnable product surface lives in:

```text
src/bin/lpk-regtest-opener.rs
```

That is intentional: it keeps the demo inside the existing crate, lets it call
the library directly, and avoids creating a separate workspace or frontend
before the regtest flow is proven.

## Run

Start Bitcoin Core regtest:

```bash
docker compose up -d bitcoind
```

Run the opener:

```bash
cargo run --features corepc --bin lpk-regtest-opener
```

Useful options:

```bash
cargo run --features corepc --bin lpk-regtest-opener -- --no-broadcast
cargo run --features corepc --bin lpk-regtest-opener -- --channel-sats 2000000
cargo run --features corepc --bin lpk-regtest-opener -- --help
```

Defaults match `docker-compose.yml`:

```text
RPC URL:      http://127.0.0.1:18443
RPC user:     lpk
RPC password: lpk
```

## Expected Output

The command prints:

- the regtest wallet and funded peer outpoints
- a normal single-funder transaction summary
- the async directory session id
- the counterparty fee contribution
- the private collaborative transaction summary
- the simulated channel funding outpoint and channel balances
- the broadcast txid and mined height
- the raw private transaction hex

The important evidence is:

```text
normal open: 1 input
private open: 2 inputs
counterparty channel balance: 0 sats
```

That demonstrates the PoC claim: the channel funding transaction has inputs
from both peers, but privacy-input mode does not give the counterparty channel
liquidity.

## Scope

This is a regtest demo, not a production node plugin. It uses:

- Bitcoin Core regtest for funding, broadcast, and mining
- `MockDirectory` for the async Payjoin coordination shape
- `MemoryWallet` with deterministic test keys
- `SimulatedChannelFunder` for the Lightning funding safety boundary

The next product step is a local dashboard that wraps this same flow and shows
the transaction comparison visually.
