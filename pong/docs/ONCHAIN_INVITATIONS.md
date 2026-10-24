# Direct on-chain game creation

Pong does not use an invitation directory for normal play. It follows the same
live-discovery shape as kdapp: the player who shares a game key watches first;
the other player creates the first game transaction after receiving that key.

## Flow

1. `pong host` generates a fresh x-only secp256k1 player key locally. Its
   private half is saved in a pending local wallet file before anything is
   displayed. The ordinary selected wallet remains the fee payer.
2. The host snapshots its accepted-chain cursor, starts watching Full accepted
   chain updates, and displays a 56-character checksummed code. The code is the
   complete public half of that fresh key.
3. `pong join CODE` decodes the host key. The joiner creates the existing Lobby
   genesis transaction with itself as `left`, the decoded host key as `right`,
   and its own five-KAS stake. It signs and submits the creation transaction.
4. The host watcher accepts only authenticated `PONGI` creation transactions on
   its configured network whose `right` key equals the fresh key. It validates
   the artifact, signed manifest, ordinary creation input, covenant genesis and
   stake before acting.
5. The host moves its saved per-game wallet into the game directory, funds the
   existing `Lobby.start` transition, and signs it as `right`. Court, Player
   queues, batched commands, resolution and settlement are unchanged.

The host must already be running when the code is shared. This is intentional:
it removes the need to retain or search historical chain data. If the host exits
before the guest's Lobby is accepted, the guest can cancel its still-open Lobby
using its own refund key and begin a fresh match.

## Why the code is not a short transaction prefix

A direct watcher must reconstruct the public key from the code without asking a
directory. An x-only public key is 32 bytes, so the code encodes all 256 bits
plus a checksum. A 16-character identifier has only 60 identity bits; it needs
an index, a host service, or a retained transaction-history scan to map it back
to the Lobby.

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
