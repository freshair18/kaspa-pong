# Multiball rules

This branch starts from master commit `5dc901e`. It changes the covenant state
and invitation format; both players must use the new client and start a fresh
match. Wallets and old recovery records are preserved.

## Match terms

Configure offers Super Fast (5 DAA/tick), Fast (6), Medium (7), and Slow (8),
one to three balls, and a winning score from one to nine. The client-only Auto
selection, entered as zero, resolves to 3, 5, or 6 goals for one, two, or three
balls. Only the resolved score is committed to the game.

Each player currently stakes five KAS. The invitation includes an eight-bit
whole-KAS stake field for future use, but current clients reject every value
other than five. This does not enable variable wagers.

## Launch phases

The period is one full paddle-to-paddle crossing: `P = 27 * ball_tick_daa`.
The initial launch epoch is the accepted opening time plus the existing
25-DAA countdown. Ball i starts at `epoch + ceil(i * P / ball_count)`.

Each ball retains that phase throughout the match. Following a goal at chain
time g, its next launch is the first phase-aligned slot at or after `g + 20`
DAA. It waits at center until then while the other balls continue. These times
use logical game DAA rather than RPC arrival or local wall time.

Each serve selects a height, left/right direction, and slope using reproducible
variation including the ball identity. Balls pass through one another. Their
colors (yellow, magenta, orange) and sparkle-sound families remain associated
with the same ball slot after goals and respawns. Balls draw over the center
KASPA label.

## Invitations

The compact code carries the complete eight-byte tag and a four-byte checksum,
making it roughly three Base58 characters longer than the former six-byte tag.
The tag uses network byte order with these fields, from most to least significant:

| Field | Bits | Current values |
| --- | ---: | --- |
| Code version | 5 | 1; independent of covenant version 24 |
| Speed | 4 | 0 Super Fast, 1 Fast, 2 Medium, 3 Slow; 4–15 reserved |
| Ball count minus one | 2 | 0–2 |
| Winning score | 4 | 1–9; Auto has no wire value |
| Whole-KAS stake | 8 | 5; other amounts rejected |
| Random nonce | 41 | Operating-system randomness |

Menus and available speeds are unchanged. Unknown code versions and reserved
speed values are rejected even with a valid checksum. Signed creation manifests
commit the complete tag and settings; the host verifies the match. Saved signed
invitations from both prior unversioned speed encodings remain valid for local
recovery, including their original funding and pending-transaction records.
New invitation discovery requires the current versioned encoding.

For 1,000 independently generated codes from the same host with identical terms
and code version, the probability of any repeated random tag is approximately
0.000023%; for 10,000 it is approximately 0.0023%. Different host public keys,
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
three colored ball layers, independent countdowns, and goal playback.
Menu and audio unit modules also cover Auto defaults, nine distinct sparkle
variants, and mixing overlapping voices. The audible device smoke test remains
opt-in.
