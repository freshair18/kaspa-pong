# Wager funding and payout

The client fixes the stake at **5 KAS per player**, for a **10 KAS winner's
pot**. There is no amount selector, CLI flag or configuration override. The
covenant carries a bounded stake parameter so a future rules/client update can
support other amounts; the current client rejects invitations with another
amount.

No additional collateral is required. Actor outputs are funded from the two
stakes themselves. Money flow at the default amount is:

| Transition | Locked funds / destination |
| --- | --- |
| Host creates Lobby | Host locks only its 5 KAS stake; host pays the lobby fee separately |
| Guest signs and funds start | Guest contributes 5 KAS + the external start fee; unused funding returns to its source address |
| Live match | Court holds 4 KAS, six Player lanes hold 1 KAS each: exactly the 10 KAS pot |
| Normal win or response-timeout win | Collector pays the entire 10 KAS to the winner's player address |
| Pre-start Lobby cancellation | The host's 5 KAS returns to its committed refund address; the guest has not yet contributed |

Resolution, end authorization and collection use additional ordinary funding
for their fees. They preserve the full pot; it is never deducted to pay network
fees. Fee totals in the client exclude stake contributions.
Fees are not automatically shared equally: the host pays for Lobby,
the guest pays for start, and each player pays for the actions it submits.

## Authenticated creation, guest-completed join

The Lobby creation transaction carries a compact invitation manifest signed by
the host's player key. It binds the network/rules version, participants, refund
and funding keys, financial terms, anchor and specific funding outpoint. Its
signature does not depend on the containing transaction ID, avoiding circular
signing. The ordinary funding key separately signs creation, supporting wallets
whose player and fee keys differ. Clients reconstruct and authenticate the
manifest and require its exact accepted unspent Lobby before funding a join.

The guest constructs the transition locally, chooses a real owned coin, adds its
stake and external fee, and signs with ALL. Lobby.start requires the invited
guest's signature and enforces the full seven-output game split, keys and
values. Creation establishes the host's authorization; no second host signature
is exchanged. Canonical outputs, source metadata, storage mass and independent
covenant execution remain required before submission. A local completed join is
retained durably for retry. Competing cancellation or joins spend the same Lobby;
only one can succeed, and a losing join does not consume the guest's coin.

The host cannot guarantee guest funding feasibility before locking Lobby:
the guest's actual coin is not known yet. An unfundable join needs different
guest funding, or Lobby cancellation. Unjoined accepted Lobbies are eligible for
host-only background refund immediately when the host leaves setup. The intent
is durable and retries in the menu or on the next client run. One-hour expiry
remains a fallback; there is no saved-match resume flow. A guest who has not
submitted a join has no stake locked and cannot cancel the host's invitation.

The short code and invitation directory locate public data only. Neither can
authorize a spend or replace covenant validation. See
[on-chain invitations](ONCHAIN_INVITATIONS.md) for discovery infrastructure and
history-coverage limits. No compatibility with the previous offer-file flow is
provided; local invitation diagnostics are not exchanged between players.

## Addresses and lifecycle

Winnings go to the winning player key committed before launch.
Unjoined Lobby cancellation uses the host refund key. It is no
longer carried in live Court/Collector state. A wallet
with separate player/fee keys can fund later transactions directly from either
owned address using its corresponding key; no balance migration or automatic
transfer is needed. These keys remain in the existing wallet format.

Normal play is first to three. If the receiving player fails to prove its
contact within its fifty-DAA accepted-progress response window, the opponent can
claim the whole match and the same winner's pot. This is five nominal seconds
of chain time; a valid late proof and an eligible claim race for the Court UTXO.
The window begins at max(contact time, accepted flight/progress anchor). Only
accepted receiver work consuming pre-contact batches renews it; post-contact
commands, nonreceiver work and watermark-only changes do not. New outgoing-flight
confirmation also gives the next receiver a fair window even after a late proof.
Remaining pre-contact history is bounded, so renewal cannot go on forever.
Empty checkpoints
that advance neither contact nor watermark and consume no batches are rejected,
so an unresponsive player cannot endlessly replace Court with no-progress spends.

Leaving queues cancellation only while the host's Lobby remains unspent.
After the guest joins there is no cancellation, even by mutual agreement: the
match ends by score or response timeout. Both clients must restart and create
fresh matches for these rules; no old wallets or saved games are migrated or
deleted. Only synthetic, unfunded fixtures are used for local verification.

Version 23 requires both clients and the invitation index to restart with the
matching executable and fresh index state. Finish or recover funded older games
using the preserved previous client; the new artifact does not spend old game
outputs. Their common pending source outpoints remain conservatively reserved
without migrating or executing old rules, to prevent accidental conflicting spends.
