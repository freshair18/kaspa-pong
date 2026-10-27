# Direct on-chain game creation

Pong does not use an invitation directory for normal play. The host watches
first; the guest creates the first game transaction after receiving the code.

## Flow

1. `pong host` uses its selected wallet player key and generates a fresh public
   eight-byte rendezvous tag with 40 random bits, a five-bit code version,
   four-bit speed, ball count, resolved winning score, and whole-KAS stake fields.
   Current clients require code version 2 and five KAS. No private key
   or address is generated per game.
2. The host snapshots its accepted-chain cursor, starts watching Full accepted
   chain updates, and displays a checksummed code containing its public key and
   rendezvous tag.
3. `pong join CODE` decodes the host key and tag and displays the game terms,
   stake, pot, and network for review. Transaction preparation starts immediately;
   after it is ready, a fresh Enter authorizes joining. Escape cancels without
   sending. The joiner creates the existing Lobby
   genesis transaction with itself as `left`, the decoded host key as `right`,
   and its own five-KAS stake, committing the decoded game settings. It signs
   and submits the creation transaction.
4. The host watcher accepts only authenticated `PONGI` creation transactions on
   its configured network whose `right` key and signed rendezvous tag match. It validates
   the artifact, signed manifest, ordinary creation input, covenant genesis and
   stake and all advertised settings before acting.
5. The host funds the existing `Lobby.start` transition with its selected wallet
   and signs it as `right`. Court, Player
   queues, batched commands, resolution and settlement are unchanged.

The host must already be running when the code is shared. This is intentional:
it removes the need to retain or search historical chain data. If the host exits
before the guest's Lobby is accepted, the guest can cancel its still-open Lobby
using its own refund key and begin a fresh match.

After a completed payout, the host prepares a new rematch code with identical
terms and watches accepted-chain additions before displaying it. Enter on the
completion screen approves hosting that rematch. The guest can paste the new
code on its completion screen, then review and confirm the next game. Rematches
require the same players and settings, reject reuse of the completed game's
tag, and create fresh recovery directories. Rollbacks invalidate the offer and
watcher; stale or unfinished states cannot accept rematch input.

## Why the code is not a short transaction prefix

A direct watcher must reconstruct the host key and distinguish concurrent games
without asking a directory. The compact Base58 code therefore carries the host's
complete x-only public key, a fresh public rendezvous tag, and one checksum. A short
identifier alone would need an index, host service, or retained history scan.

## Node requirements

Each client needs a compatible synchronized Kaspa node on the same network with
UTXO indexing and the accepted-chain Full API. They can use different nodes and
different configuration files. `node.json`, when present, is only a local
endpoint configuration; it contains no discovery state and is not exchanged.

```powershell
.\pong --config node.json probe
.\pong alice host
.\pong bob join HOST_GAME_CODE
```

The legacy `index` and `invitations` commands remain available for inspecting
older directory-based games. Normal Host/Join never call them.
