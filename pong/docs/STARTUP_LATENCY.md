# Startup connection and guest bootstrap

The client keeps one Tokio runtime alive across menus and matches. Once wallet,
network and configuration are selected, connection qualification starts while
the menu accepts input. A single owner retains the configured endpoint,
resolver list, ordered fallbacks, network and unsynced policy, separately from
the resolved peer URL. Directory discovery is a separate service. A changed
connection identity drains the old attempt and disconnects it before replacement.
Reuse checks network/index/sync; qualification older than five seconds repeats
the Full capability check. Failed qualification disconnects before reconnecting.
Gameplay can hand back its replacement peer without replacing configuration.

Background hosted recovery shares that owner and both process signer locks.
Local public-journal scanning restores reserved inputs even without expired
invitations; only due recovery awaits the warmed node. Menu input remains
responsive. Launch and exit join recovery, including durable submission, before
new funding or lock release. Host, guest and direct Run register custom state
paths before signing. Recovery errors cannot skip the scan/recovery launch gate.
Per-match preparation, authorized submits, clock and chain workers drain on
normal and error exits. Runtime destruction is no longer the cleanup mechanism.

Cold guest setup authenticates the signed manifest, rules, participants and
network, then captures an own-node sink before reading the exact unspent Lobby.
It checks amount, script, covenant, non-coinbase status and actual acceptance DAA.
A hash-only selected-chain continuation check and a second exact UTXO read
detect removed cursors, competing spends and root retiming. It retries at most
three times if metadata changes, a cursor is removed, or the window exceeds 64
added blocks. Missing/spent/invalid Lobby is an error. Cancellation returns
before funding. The continuation cursor remains the original captured sink:
gameplay Full replay consumes every subsequent transition and independently
executes covenants. Final exact source checks still precede submission.

This removes the guest's redundant general `Cache::sync` setup pass, including
its Full transaction download/replay and repeated general readiness checks.
It does not replace ongoing Full validation. Separate node reads are not an
atomic snapshot: a later spend/reorg is reconciled by the retained cursor and
source checks, and persistent churn fails closed after bounded bootstrap retries.

Pinned Rusty Kaspa a41a333 supports `get_virtual_chain_from_block_v2` with
explicit `RpcDataVerbosityLevel::None`; omission defaults to Full. The pinned
node implements it using the selected-chain path and bounded acceptance batches
(`rpc/core/src/api/rpc.rs`, `rpc/service/src/service.rs`). No historical tx-ID
lookup or atomic chain/UTXO snapshot API is assumed. Argent remains pinned at
9a9f4b1; no protocol, version or artifact regeneration is involved.

Interactive startup JSON diagnostics and background recovery notices append to
`target/startup-PID.jsonl`, avoiding writes over the alternate-screen menu.
Redirected use keeps stderr diagnostics. These include connection start/ready/failure/reuse/
reconnect, invitation lookup, bootstrap retries/ready, funding/preparation and
required setup persistence. Per-game timing logs retain required authorization
persistence, submit request/ack, accepted-start arrival/validation/UI publication.
`code_entry_total` runs from submission of the code to the menu until validated
start publication, including any recovery gate; it excludes time typing the code.
`accepted_start_arrival.elapsed_ms` is observed delay from submit request, not
node acceptance time. The first terminal render reports its wait from the first
ready Live display, the actual rendering reserve and opening countdown remaining.
The unchanged opening countdown is 25 DAA; the rendering reserve is independently
adaptive (5..15 DAA). UI publication is not proof of a physical display refresh.
Headless runs have no terminal-render measurement. Node acceptance wall time is
unavailable. Synthetic tests establish behavior, not public-network speedups.
