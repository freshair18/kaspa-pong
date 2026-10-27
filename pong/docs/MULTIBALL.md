# Multiball rules

Both players must use the same covenant version to start a match.

## Match terms

Configure offers Super Fast (5 DAA/tick), Fast (6), Medium (7), and Slow (8),
one to six balls in the visible menu. Slots seven and eight are supported as
unlisted key choices. Speed option zero opens manual configuration; speed and
goal menus advertise 1–9, with A–F accepted as unlisted keys for 10–15.
The client-only Auto goal selection, entered as zero, resolves to 3, 5, 6, 7, 8,
9, 10, or 11 goals for one through eight balls. Only the resolved score is committed.

Each player currently stakes five KAS. The invitation includes an eight-bit
whole-KAS stake field for future use, but current clients reject every value
other than five. This does not enable variable wagers.

## Launch phases

The period is one full paddle-to-paddle crossing: `P = 27 * ball_tick_daa`.
The initial launch epoch is the accepted opening time plus a 25-DAA delay.
Ball i starts at `epoch + ceil(i * P / ball_count)`.

In games with multiple balls, each ball retains that phase throughout the match.
Following a goal at chain time g, its next launch is the first phase-aligned slot at or after `g + 20`
DAA. It waits at center until then while the other balls continue. These times
use logical game DAA rather than RPC arrival or local wall time.
With one ball there is no phase to preserve: its next launch is exactly `g + 20` DAA.

Each serve selects a height, left/right direction, and slope using reproducible
variation including the ball identity. Balls pass through one another. Their
colors (yellow, magenta, orange, violet, lime, bright blue, rose pink, mint green) remain associated
with the same ball slot after goals and respawns. Balls draw over the center
KASPA label. Numeric countdowns are hidden; the waiting ball and direction arrow
remain visible. Slots four through eight reuse the existing sparkle sounds.

## Invitations

The compact code carries an eight-byte tag and a four-byte checksum.
The tag uses network byte order with these fields, from most to least significant:

| Field | Bits | Current values |
| --- | ---: | --- |
| Code version | 5 | 2; independent of covenant version 26 |
| Speed | 4 | Actual DAA ticks per movement step, 1–15 |
| Ball count minus one | 3 | 0–7 |
| Winning score | 4 | 1–15; Auto has no wire value |
| Whole-KAS stake | 8 | 5; other amounts rejected |
| Random nonce | 40 | Operating-system randomness |

Unknown code versions and speed zero are rejected even with a valid checksum.
Signed creation manifests
commit the complete tag and settings; the host verifies the match. Saved signed
invitations from both prior unversioned speed encodings remain valid for local
recovery, including their original funding and pending-transaction records.
New invitation discovery requires the current versioned encoding.

For 1,000 independently generated codes from the same host with identical terms
and code version, the probability of any repeated random tag is approximately
0.000045%; for 10,000 it is approximately 0.0045%. Different host public keys,
settings, or versions distinguish codes. The checksum detects copying errors;
it adds no random collision resistance or authentication.

## Validation and responsiveness

The Court selects the earliest pending contact across active balls. Movement
history cannot be compacted past that boundary. Receiver signatures, exact
acceptance DAA, bounded queue processing, response timeouts, and the separate
winner-payout collector remain authoritative.

More balls increase contact frequency and transaction workload. Rendering and
audio run locally; they do not make an unaccepted transaction authoritative.
Public-network responsiveness must be evaluated with actual matches.

Focused checks use the existing test targets: `contract multiball::` for the
phase invariant, VM execution, immutable terms and payout; `node multiball::`
for accepted-state replay and rollback; and `gameplay multiball::` for the
eight colored ball layers, hidden countdowns, and goal playback.
Menu and audio unit modules also cover Auto defaults, nine distinct sparkle
variants, and mixing overlapping voices. The audible device smoke test remains
opt-in.
