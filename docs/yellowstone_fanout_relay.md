# Yellowstone Fanout Relay

## Goal

We do not read Metis RAM. Instead we want the bot to receive the **same raw
account updates** that Metis receives from the upstream Yellowstone gRPC
provider, so the bot can build its own `PoolStateStore` and compute pool prices
(slippage) locally — without paying for a second full Geyser subscription and
without slowing Metis down.

The end-state architecture (Phase B+):

```text
Yellowstone upstream
        ↓
yellowstone-fanout-relay
        ├── primary, low-latency stream to Metis
        └── non-blocking copy of account updates to the Bot
```

## Critical latency rule

The bot path must **never** block the Metis path. If the bot is slow, bot
updates are dropped — Metis must keep receiving updates with minimal delay.
(This rule applies to Phase B, when the relay sits in front of Metis.)

## Phases

| Phase | What | Touches Metis? |
|-------|------|----------------|
| **A** *(done)* | Standalone tool subscribes directly to upstream, decodes account updates, prints them. Proves decode + delivery. | No |
| B | Relay sits between upstream and Metis; forwards to Metis (priority) + non-blocking copy to bot. | Yes |
| C | Bot `PoolStateStore` built from the relay stream, indexed via `mix.json`. | — |
| D | Per-DEX price/slippage calculators on top of the store. | — |

## Phase A — what was built

Independent crate at `tools/yellowstone_fanout/` (excluded from the main
workspace, so the bot build is untouched). Binary:
`yellowstone_fanout_phase_a`.

Flow:

```text
PublicNode Yellowstone  ->  yellowstone_fanout_phase_a  ->  stdout
```

- Reads `mix.json` (default `/root/c/metis/1/mix.json`) and extracts, per pool,
  the minimal set: `pubkey`, `params.tokenAccountA`, `params.tokenAccountB`,
  capped at `MAX_ACCOUNTS` distinct pubkeys.
- Connects with the **same** setup the bot already uses successfully
  (`account_cache.rs`): TLS + native roots, 64 MiB max decode, optional
  x-token, `yellowstone-grpc-client/proto` **6.1**.
- Subscribes (`commitment = Processed`) to that explicit account list.
- For each `SubscribeUpdateAccount`, emits one compact line:

  ```text
  slot=<u64> pubkey=<base58> owner=<base58> lamports=<u64> data_len=<n> write_version=<u64> startup=<bool>
  ```

- Aggregate metrics every 10s on stderr:
  `updates_from_upstream`, `updates_to_bot`, `last_upstream_slot`.
- Reconnects with exponential backoff (500ms → 10s) on any error.

### Why 6.1 and not 12.3.0?

The friend's note suggested `yellowstone-grpc-proto 12.3.0`. We pinned **6.1**
because the bot already runs that version against this exact PublicNode
endpoint, so there is zero proto/transport mismatch risk for the Phase A proof.
If a later phase needs newer features (or vendored protos with `bytes()`
optimization), we revisit the version then.

### Hot-path discipline (already enforced)

- No per-update logging via `tracing` in the loop.
- No base64 / JSON unless `--dump-json` (or `DUMP_JSON=1`) is explicitly set.
- Only relaxed atomic counters touched per update.

## Running Phase A

```bash
cd tools/yellowstone_fanout
cp .env.example .env       # fill in UPSTREAM_YELLOWSTONE_X_TOKEN if needed
set -a; . ./.env; set +a
cargo run --release --bin yellowstone_fanout_phase_a
# deep debug (heavy): append -- --dump-json
```

### Config (env)

| Var | Default | Meaning |
|-----|---------|---------|
| `UPSTREAM_YELLOWSTONE_ENDPOINT` | — (required) | Upstream gRPC URL |
| `UPSTREAM_YELLOWSTONE_X_TOKEN` | empty | Optional auth token |
| `MIX_JSON` | `/root/c/metis/1/mix.json` | Market cache to build the sub list |
| `MAX_ACCOUNTS` | `50` | Cap on distinct account pubkeys |
| `BOT_OUTPUT_MODE` | `stdout` | `stdout` or `none` (metrics only) |
| `DUMP_JSON` | unset | `1`/`true` to emit full base64 data |

## Phase A acceptance criteria

1. gRPC connects to PublicNode.
2. `SubscribeRequest` is built from `mix.json` accounts.
3. `SubscribeUpdateAccount` messages are received.
4. `pubkey`, `owner`, `data_len`, `slot`, `write_version` decode correctly.
5. Updates arrive with advancing slots.
6. Zero impact on the running Metis (we never touch it in Phase A).

## Security

Tokens/secrets live in env only — never committed. `.env` is git-ignored; only
`.env.example` (no secrets) is tracked.
