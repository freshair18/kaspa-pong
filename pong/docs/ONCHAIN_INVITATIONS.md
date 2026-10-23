# On-chain invitations (version 19)

Host creates one Lobby transaction with a 324-byte manifest on devnet (318
fixed bytes plus the UTF-8 network name). Guest enters its checksummed game code,
downloads the creation transaction, verifies it locally, matches its Lobby against
the guest's own node, then funds and signs the start. No public offer file or
partially signed transaction is exchanged. `invitation.json` is a local public
record only. Fresh matches and both restarted clients are required.

## Run the required directory

This release includes a small persistent directory in `pong-node index`. It uses
the pinned node's accepted-chain Full API. Someone must operate it continuously
on the chosen network, starting before invitation creation or from a retained
chain anchor preceding it. Neither client silently chooses a remote service.
`invitation_index: null` (the default) makes Host/Join fail before funding/signing,
with configuration instructions.

1. Run a compatible synchronized Kaspa node with `--utxoindex`; the existing
   `.\pong --config node.json probe` checks network, UTXO index and accepted-chain
   v2 Full support. Full responses must contain transaction payloads and historical
   input UTXOs. Ordinary stable-release SDKs lacking this API are insufficient.
2. Run the directory without a wallet:

   ```powershell
   .\pong --config node.json index --state directory-v19.json --listen 127.0.0.1:17720
   ```

   On first start, coverage begins **after** the node's current pruning-point
   hash. Use `--anchor FULL_RETAINED_CHAIN_HASH` to select an earlier available
   starting point explicitly. A persisted directory retains its original coverage
   anchor and cursor; a conflicting version/network/anchor is rejected. Use a new
   state path for a new directory. No state/wallet migration or deletion occurs.
3. Add the explicit directory address to each player's node configuration:

   ```json
   { "invitation_index": "http://127.0.0.1:17720" }
   ```

   Merge that property into the existing configuration; keep each client's own
   node endpoint/network. Different nodes and independent working directories are
   supported. The HTTP client supports `http://HOST:PORT`, including explicit
   remote addresses. For separate machines use a trusted private network/tunnel,
   or a tunnel exposing the remote directory at each client's loopback port.
   TLS, authentication and public-service rate limiting are deployment concerns;
   the built-in server is a small read-only HTTP/1.0 service, not a public SaaS.
   Its default bind address is loopback; exposing a remote listener is an explicit
   operator choice. A local development directory has since been configured on this
   workstation at `127.0.0.1:17720`, selected by ignored `pong/node.json`, with
   state `target/invitation-directory-v19.json`. It covers fresh invitations after
   its recorded coverage anchor; it is not a public/shared hosted deployment and
   does not auto-start at boot. Ordinary `pong.cmd` launches restart the configured
   loopback directory from this existing state if no listener is available;
   explicit `--config` and operator commands are not auto-managed. Helper logs
   use `target/invitation-directory-TIMESTAMP.*.log`. The helper's existing-service
   path and a real local HTTP lookup were verified; its cold-start branch has
   not been tested by stopping the running service. To restart manually:

   ```powershell
   .\pong --config node.json index --state target/invitation-directory-v19.json --listen 127.0.0.1:17720
   ```

   Keep this service running. Existing state supplies its original anchor; do
   not substitute a new one. Another computer must use a reachable shared
   directory address/tunnel rather than its own loopback address.
4. Host selects the guest's player key:

   ```powershell
   .\pong HOST_WALLET host GUEST_PLAYER_PUBLIC_KEY
   .\pong GUEST_WALLET join ABCD-EFGH-JKMN-PQRS
   ```

   The illustrated code is a placeholder; use the host's actual code. The host
   checks directory reachability before creating the Lobby. After its own node
   observes acceptance, it displays the code. The directory may still be catching
   up; the guest now displays lookup status and automatically retries empty
   lookups for up to 60 seconds, with q/Esc returning to the menu. Authentication
   failures and unavailable directories are displayed as errors. The selected wallet remains
   explicit. `pong WALLET invitations` lists authenticated directory candidates
   addressed to that player's public key. These listings are advisory until the
   guest's node verifies the unspent Lobby.

The directory persists accepted creations and open/spent classification, polling
from its cursor and undoing reorgs atomically. Its undo window is 2,048 accepting
blocks; an older rollback fails with an explicit reindex error. It serves bounded
HTTP requests between *every* chain batch, including a continuously advancing
chain, and does not rewrite snapshots for unchanged batches. Stored invitations
are public. Back up the directory state if old still-open invitations matter.
It retains public creation records even after closing; storage grows with
invitation volume, not indefinitely with empty blocks.

Cold guests require no shared files or preexisting cache. The directory returns
the creation transaction through `/lookup/CODE`; `/player/XONLY_PUBLIC_KEY` lists
addressed candidates. A directory started after the creation was pruned cannot
recover that payload from a pruned node. An offline directory whose cursor was
pruned also requires a new explicit coverage anchor or an operator-provided node
retaining the required history. There is no claim of arbitrary historical
recovery, and an empty result explicitly reports the coverage possibility.

## Authentication and money boundary

The binary manifest includes magic `PONGI`, version, length-bounded network name,
pre-creation observation anchor, host player/guest player/refund/funding public
keys, queue collateral, fixed ball interval, per-player stake, Court collateral,
funding amount, exact creation fee and funding outpoint, followed by a 64-byte
host **player** Schnorr signature. Integers are little-endian; keys/hashes are raw
32-byte values; trailing bytes are rejected. The signature hashes the complete
unsigned manifest with BLAKE2b-256 and domain
`KaspaPong/creation-intent/19\0`. The short code uses the same hash domain only
for its non-authorizing checksum.

The signed funding outpoint is the unique creation identity. The signature does
not reference the containing transaction ID or artifact ID, so there is no
signature/transaction/artifact circularity. Fixed version 19 binds this release's
rules; clients additionally require the sole pinned artifact and exact current
participants, pace, five-KAS stake and seven-KAS collateral. The player signature
works when the selected funding key differs from the player/refund key. The
creation transaction's separate ordinary P2PK signature is also VM-checked using
the signed funding amount/key. Its exact single source, principal, external fee,
change destination and computed genesis covenant ID are checked. A forged cached
transaction ID is rejected after recomputing it.

Both clients check the manifest through `Invitation::validate`; the guest never
trusts redundant directory metadata. Before funding it captures its own node's
current cursor, queries that node's UTXO index, and matches the full Lobby
outpoint, script, value and covenant identity. Accepted DAA metadata comes from
that node. This bootstrap lets a valid old unspent Lobby be joined without
replaying from a pruned creation anchor. Tip-only bootstrap reorgs retain the
older root; a reaccepted creation refreshes its metadata before spending replay.
Normal chain replay, independent covenant execution and current-source checks
then gate signing/submission. Removed/spent roots cannot enable a new spend.

The manifest signature is **client-validated, not covenant-validated**. The
covenant permanently commits the Lobby terms at creation and requires the
invited guest's `ALL` signature at start. Creating a fake sponsored Lobby cannot
spend the host's ordinary coins. The official client rejects any purported host
intent without the player signature and exact accepted creation. The host's
authorized creation deliberately locks the stated funds under those committed
rules; starting needs no later host signature. A custom client may accept an
unsigned sponsored Lobby, but that does not give it access to an existing host
Lobby or alter covenant restrictions. The directory cannot authorize funding,
change participants or payout destinations, forge host intent, or override the
guest node. It can withhold/omit data and cause unavailability; completeness and
collision enumeration are not cryptographically guaranteed by an untrusted
directory.

Host locks 12 KAS: its 5-KAS stake and 7-KAS operational collateral. Guest adds
exactly its 5-KAS stake plus the external start fee, using an owned mature ordinary
coin and its corresponding funding key. The local `StartPackage` draft is never
shared. Real candidate inputs are selected by actual storage mass; guest player
and funding signatures use `ALL`. Canonical inputs, all seven initial game
outputs, exact change, fee cap, storage mass and independent covenant execution
are checked before authorization. Court holds 11 KAS and each of six queues
holds 1 KAS. Principal is excluded from network fees. Host creation fees and guest
start fees remain attributed to the actual funding key; the cold bootstrap seeds
the validated creation fee into the ledger.

Pre-start cancellation still requires the refund key and returns all 12 KAS,
funding its network fee externally. Joining and cancellation race for the same
Lobby UTXO, so only the accepted spend wins; the guest rechecks sources after
preparation and the existing durable pending/reservation path handles uncertain
submission and reorgs. Gameplay continues to use its existing concurrent actor
pipelines. Normal/timeout completion still returns 7-KAS collateral to the host
refund key and the 10-KAS pot to the winner's player key; mutual cancellation
returns each player's 5-KAS stake.

## Codes and collisions

The displayed code has 12 Crockford-base32 identity digits (60 bits) and four
checksum digits (20 bits), grouped in fours. Input accepts ASCII case, whitespace
and hyphens; routing reconstructs the canonical code before forming an HTTP
request. The checksum detects accidental mistyping with a 20-bit probabilistic
check. It is not authentication. Full 256-bit transaction identities are used
internally and for game directories. A lookup returns every matching open
candidate; distinct matches are explicitly rejected as ambiguous. Host also
prints a full code: 52 padded base32 identity digits plus four checksum digits.
Use that code to resolve a collision. A malicious directory can suppress a
collision; host signature/own-node checks still protect funds, and full codes
avoid dependence on complete prefix enumeration.

## Pinned capability check and research

The implementation was checked against local dependency sources pinned to
Argent `9a9f4b107116d0b259fae629f200fa3b663e7e9d` and rusty-kaspa
`a41a333b08848f41bf737b72592e463a6011b8ac`, not assumed from an unrelated SDK.
Argent `TxContext::payload` inserts the manifest before funding signs; its
genesis builder derives covenant identity from source outpoint and initial
outputs. Rusty-kaspa exposes `get_virtual_chain_from_block_v2(..., Full, ...)`,
including accepted transaction payloads and historical input UTXOs, plus
`get_utxos_by_addresses`. The service bounds chain batches and can lose old
history through pruning. `get_blocks` is a retained-block traversal API;
`get_utxo_return_address` returns a source address given transaction ID and
accepting DAA, not a general payload/short-code history API. Mempool lookup is
not accepted historical lookup. These capabilities motivate the persistent
directory rather than assuming arbitrary `getTransaction(txid)` exists.

Primary implementations inspected:

- [Kasia transaction generator](https://github.com/K-Kluster/Kasia/blob/staging/src/service/transaction-generator.ts)
  supplies payload/network/change in WASM Generator settings;
  [account service](https://github.com/K-Kluster/Kasia/blob/staging/src/service/account-service.ts)
  signs pending transactions locally with the selected keys before submission.
  [Historical syncer](https://github.com/K-Kluster/Kasia/blob/staging/src/service/historical-syncer.ts)
  retrieves receiver handshakes and sender/context histories from its indexer.
  Pong borrows the separation of on-chain payload and cross-device discovery.
- [Kasia indexer payload operations](https://github.com/K-Kluster/kasia-indexer/blob/main/protocol/src/operation.rs)
  parse versioned `ciph_msg` payload variants;
  [virtual-chain processor](https://github.com/K-Kluster/kasia-indexer/blob/main/indexer-actors/src/virtual_chain_processor.rs)
  maintains acceptance/cursors and indexed participant views. Pong uses a small
  dedicated accepted-payload directory rather than claiming Kasia can parse
  Pong manifests unchanged.
- [kdapp generator](https://github.com/michaelsutton/kdapp/blob/master/kdapp/src/generator.rs)
  packs a prefix/nonce/payload and signs the funding transaction;
  [proxy](https://github.com/michaelsutton/kdapp/blob/master/kdapp/src/proxy.rs)
  follows acceptance and reversions;
  [engine](https://github.com/michaelsutton/kdapp/blob/master/kdapp/src/engine.rs)
  verifies application command signatures. Pong uses explicit payload framing
  and reorg-aware observation, but its financial/game enforcement remains in
  Argent covenants. It does not copy kdapp's transaction-ID pattern mining.
- [Pinned node API](https://github.com/kaspanet/rusty-kaspa/blob/a41a333b08848f41bf737b72592e463a6011b8ac/rpc/core/src/api/rpc.rs)
  and [service implementation](https://github.com/kaspanet/rusty-kaspa/blob/a41a333b08848f41bf737b72592e463a6011b8ac/rpc/service/src/service.rs)
  are the exact locally inspected retrieval/retention boundary.

## Focused verification

The `invitations` module in `tests/node.rs` covers signed creation and altered metadata,
separate player/funding keys, code typo/collision/full-identity handling,
guest-only funding/change/fee checks, independent re-signed covenant attacks,
absent/spent or incorrect Lobby UTXOs, competing joins, acceptance reorgs,
cold-bootstrap tip rollback/creation retiming, bounded directory undo, missing
configuration and wrong participants/network. The cold-start transport test
uses real loopback TCP/HTTP with separately created temporary host/guest
directories and a persisted/reloaded synthetic accepted-chain index. Every
served batch advances; no shared file reaches the guest. Own-node matching is
tested with synthetic node UTXO results. This is not a two-live-node or funded
network test. No performance, fee-saving or latency measurement is claimed.

Parent integration owns compilation, running the focused target, matching
installation and final results. Suggested target:

```powershell
cargo test -p kaspa-pong --test node invitations::
```

The worker did not invoke Cargo, use a funded wallet, read private wallet
contents, stop user processes, publish services or commit changes.
