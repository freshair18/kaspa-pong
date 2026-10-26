# Wager funding and payout

The client fixes the stake at **5 KAS per player**, for a **10 KAS winner's
pot**. There is no amount selector, CLI flag or configuration override. The
covenant carries a bounded stake parameter so a future rules/client update can
support other amounts; the current client rejects invitations with another
amount. The game code now carries the whole-KAS amount explicitly; current
clients still accept only five. Ball count and winning score are signed match
terms too. Auto winning goals are resolved by the client before encoding.

No additional collateral is required. Actor outputs are funded from the two
stakes themselves. Money flow at the default amount is:

| Transition | Locked funds / destination |
| --- | --- |
| Guest creates Lobby from the host's code | Guest locks its 5 KAS stake and pays the lobby fee separately |
| Host signs and funds start | Host contributes 5 KAS + the external start fee; unused funding returns to its source address |
| Live match | Court holds 4 KAS, six Player lanes hold 1 KAS each: exactly the 10 KAS pot |
| Normal win or response-timeout win | Collector pays the entire 10 KAS to the winner's player address |
| Pre-start Lobby cancellation | The guest's 5 KAS returns to its committed refund address; the host has not yet contributed |

Resolution, end authorization and collection use additional ordinary funding
for their fees. They preserve the full pot; it is never deducted to pay network
fees. Fee totals in the client exclude stake contributions.
Fees are not automatically shared equally: the guest pays for Lobby,
the host pays for start, and each player pays for the actions it submits.

## Authenticated creation, host-completed start

The Lobby creation transaction carries a compact invitation manifest signed by
the guest's player key. It binds the network/rules version, participants, refund
and funding keys, financial terms, anchor and specific funding outpoint. Its
signature does not depend on the containing transaction ID, avoiding circular
signing. The ordinary funding key separately signs creation, supporting wallets
whose player and fee keys differ. Clients reconstruct and authenticate the
manifest and require its exact accepted unspent Lobby before funding a start.

The host constructs the transition locally, chooses a real owned coin, adds its
stake and external fee, and signs with ALL. Lobby.start requires the invited
host's signature and enforces the full seven-output game split, keys and
values. No partially signed transaction is exchanged. Canonical outputs,
source metadata, storage mass and independent
covenant execution remain required before submission. A local completed join is
retained durably for retry. Competing cancellation or joins spend the same Lobby;
only one can succeed, and a losing start does not consume the host's coin.

The guest cannot guarantee host funding feasibility before locking Lobby:
the host's actual coin is not known yet. An unfundable start needs different
host funding, or Lobby cancellation. Unjoined accepted Lobbies are eligible for
guest-only background refund immediately when the guest leaves setup. The intent
is durable and retries in the menu or on the next client run. One-hour expiry
remains a fallback; there is no saved-match resume flow. A guest who has not
created a Lobby has no stake locked. The host cannot cancel the guest's Lobby.

The host's checksummed code identifies its key, terms, and fresh rendezvous tag;
normal play watches accepted chain updates directly. The code cannot authorize
a spend or replace covenant validation. See
[on-chain invitations](ONCHAIN_INVITATIONS.md) for the discovery flow.
No compatibility with the previous offer-file flow is
provided; local invitation diagnostics are not exchanged between players.

## Addresses and lifecycle

Winnings go to the winning player key committed before launch.
Unjoined Lobby cancellation uses the guest refund key. It is no
longer carried in live Court/Collector state. A wallet
with separate player/fee keys can fund later transactions directly from either
owned address using its corresponding key; no balance migration or automatic
transfer is needed. These keys remain in the existing wallet format.

Normal play is first to the configured score from one to nine. Auto defaults
are 3/5/6 for one/two/three balls. If the receiving player fails to prove the
earliest pending contact within its 150-DAA accepted-progress response window,
the opponent can claim the whole match and the same winner's pot. This is fifteen nominal seconds
of chain time; a valid late proof and an eligible claim race for the Court UTXO.
The window begins at max(contact time, accepted flight/progress anchor). Only
accepted receiver work consuming pre-contact batches renews it; post-contact
commands, nonreceiver work and watermark-only changes do not. New outgoing-flight
confirmation also gives the next receiver a fair window even after a late proof.
Remaining pre-contact history is bounded, so renewal cannot go on forever.
Empty checkpoints
that advance neither contact nor watermark and consume no batches are rejected,
so an unresponsive player cannot endlessly replace Court with no-progress spends.

Leaving queues cancellation only while the guest's Lobby remains unspent.
After the host starts there is no cancellation, even by mutual agreement: the
match ends by score or response timeout. Both clients must restart and create
fresh matches for these rules; no old wallets or saved games are migrated or
deleted. Only synthetic, unfunded fixtures are used for local verification.

Version 24 requires both clients to restart with the matching executable and
create fresh games. Finish or recover funded older games
using the preserved previous client; the new artifact does not spend old game
outputs. Their common pending source outpoints remain conservatively reserved
without migrating or executing old rules, to prevent accidental conflicting spends.
