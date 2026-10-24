# Direct on-chain game creation

Pong does not use an invitation directory for normal play. The host watches
first; the guest creates the first game transaction after receiving the code.

## Flow

1. `pong host` uses its selected wallet player key and generates a fresh public
   46-random-bit rendezvous tag plus a two-bit speed selection. No private key or address is generated per game.
2. The host snapshots its accepted-chain cursor, starts watching Full accepted
   chain updates, and displays a checksummed code containing its public key and
   rendezvous tag.
3. `pong join CODE` decodes the host key and tag. The joiner creates the existing Lobby
   genesis transaction with itself as `left`, the decoded host key as `right`,
   and its own five-KAS stake. It signs and submits the creation transaction.
4. The host watcher accepts only authenticated `PONGI` creation transactions on
   its configured network whose `right` key and signed rendezvous tag match. It validates
   the artifact, signed manifest, ordinary creation input, covenant genesis and
   stake before acting.
5. The host funds the existing `Lobby.start` transition with its selected wallet
   and signs it as `right`. Court, Player
   queues, batched commands, resolution and settlement are unchanged.

The host must already be running when the code is shared. This is intentional:
it removes the need to retain or search historical chain data. If the host exits
before the guest's Lobby is accepted, the guest can cancel its still-open Lobby
using its own refund key and begin a fresh match.

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
