# Contact resolution and collection split

Baseline: v1.0 at 0bd10da. Initial two-stage redesign: 50f0bbd.
Client-only fee optimization: fd015c5. Atomic contact implementation: 97023f9.
Current branch: codex/contact-resolution, package 2.0.0-dev.
See SESSION_HANDOFF.md for the final current artifact and verification status.

## What is sharded

All six movement lanes use Player. Court performs bounded movement-prefix
verification and collision physics, consuming only the receiving side's three
lanes. Collector performs the six-lane collection and refund at match end.
Argent's typed leader/delegate coordination authenticates each exact input group
and the authorized successors.

Court still contains small score/signature gates for leaving live play. It does
not contain Collector's six-lane ownership/value verification and refund body.
Court.settle emits a Collector after a winning score; Court.cancel emits one
only with both player signatures. Court.timeout authorizes an opponent's match
win after an unanswered contact. None consumes movement queues. Collector later
consumes all six latest queues and returns operational collateral to the agreed
refund key separately from the wager payout. There is no Finished actor in the
current contract.

There is no dispatcher transaction before contacts. Ending remains two
transactions: authorization then collection/refund, replacing the previous
collection then reclaim. Already-in-flight commands may be accepted after
authorization; the tracker preserves their latest successors for collection.

This is operation-level contract separation, not a split of collision physics
into per-angle or per-paddle selector shards. The earlier Court/Resolver/Collector
selector prototype failed with "gen__resolver_template expects byte[32]". That
failure was not isolated sufficiently to establish a compiler limitation.
The pinned SDK includes actor selectors; the successful current design uses
direct actor transitions and coordinated delegates, without changing Argent.

## Input and timing rules

| Operation | Game inputs | Result |
| --- | ---: | --- |
| Movement | 1 Player | One replacement lane with another signed batch |
| Atomic contact / prefix checkpoint | Court + 3 Players = 4 | Updated ball and receiving-side queues |
| End authorization / timeout / mutual cancel | Court = 1 | Collector, preserving Court's principal |
| Collection and payout | Collector + 6 Players = 7 | Operational refund and winner's pot, or both stakes back on mutual cancel |

Fee inputs are additional. Three lanes per side and four batches per lane
remain. Opponent queues are untouched by routine contact resolution.

Court folds at most four chronological timestamp groups strictly before the
requested boundary. Its relative DAA lock prevents acceptance before that
boundary, which cannot pass the next contact. Equal-DAA and later commands
remain queued; they cannot retroactively rescue a missed ball. Original command
acceptance timestamps survive every queue recreation.

A fully reached receiving contact deadline computes hit/miss and the outgoing
trajectory in the same transition. Queue-heavy prefixes can still require
multiple bounded resolves. Queue pressure triggers early compaction; quiet
tails wait for contact. Every prefix/contact requires that paddle owner's
signature. An unanswered receiving contact permits the opponent to claim the
entire match after fifty DAA from max(contact time, accepted response anchor).
A genuinely consumed receiver prefix renews the anchor using its acceptance,
as does confirmation of a new flight. Watermark-only or nonreceiver work and
post-contact commands cannot. Once due, remaining pre-contact history is finite
(at most twelve batches), preventing endless renewals. A valid late proof can
still race an eligible timeout.

## Measured tradeoff

The operation split was measured against 97023f9 before changing court geometry,
using identical funding/relay rate and VM-validated transaction construction.

| Fee, sompi | Atomic Court before split | Separate Collector |
| --- | ---: | ---: |
| Movement | 543,000 | 659,400 |
| Contact | 7,115,400 | 4,238,600 |
| Both ending transactions | 8,605,500 | 9,551,200 |
| One movement + contact + complete ending | 16,263,900 | 14,449,200 |

Contacts cost 40.4% less in that fixture, but movement costs 21.4% more and the
ending costs 11.0% more. The combined example is 11.2% lower. Player now needs
a Collector delegate, so its script grows from 2,118 to 2,815 bytes. Court
shrinks from 28,342 to 11,867 bytes; complete routine contact scripts shrink
from 34,696 to 20,312 bytes in this intermediate artifact. After the 58x32
geometry update, the version-13 Court was 11,875 bytes and the contact total was
20,320 bytes; Player and Collector remain 2,815 and 8,984 bytes respectively.

These numbers predate receiver signatures, match timeouts and wager payouts.
They describe the script-split experiment, not exact current fees, a universal
full-match saving or a measured latency gain. Changes affect encoded sizes;
the current artifact's inspection report is authoritative. More movement per
contact can offset the contact saving. The host funds the lobby and operational
collateral; the guest funds its own stake and the joint-start fee. Movement,
resolution and ending fees are not automatically shared equally.

The historical Git-artifact comparison harness has been retired. Current
artifact reproducibility and pinned identity remain covered by the `contract`
test target; the figures above are historical measurements, not current checks.

Older 53.8%/63.4% fee comparisons apply to the previous atomic implementation
against the initial redesign/v1.0, not to every current match.

## Geometry and preserved behavior

The current geometry is 58x32. Version 16 lengthens paddles to nine cells
(centers 4..27) and includes the ball's one-cell vertical radius at paddle
contacts. It preserves five rebound slopes, two horizontal cells per tick, serve
delays and first-to-three scoring. The narrower court naturally shortens flight.
The subsequent fixed-pace change uses seven DAA per ball tick.
The original seven-cell, point-ball experiment reported no angled cycles for
aligned paddles. That result does not apply to the wider version-16 catch area.
Rerunning its classifier in memory with centers 4..27 and hit offsets <=5 gives
4.5805% angled cycles overall (129,024 cases), 19.6801% for aligned paddles
(5,376 cases), and 22.7679% when both are centered (224 cases). Original analysis
files are unchanged. Longer reach makes stationary angled cycles more likely;
no further geometry or rebound-rule change is included here.
Indefinitely horizontal rallies are deliberately out of scope.

Seven refundable coins, explicit wallets, Host/Join, batching, three in-flight
movements, buffered rendering, reorg recovery and fee caps remain. New rules
require restarted clients and fresh matches. No old wallets or game files are
migrated/deleted; v1.0 remains recoverable from its tag.
