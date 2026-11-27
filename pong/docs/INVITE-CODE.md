# What is in an invite code

## Decoded layout

The invite is transmitted as Base58. Offsets below refer to the 44 decoded bytes and start at zero.

| Bytes | Length | Value |
| --- | ---: | --- |
| 0–31 | 32 | Host's x-only secp256k1 public key |
| 32–39 | 8 | Tag, stored as a big-endian unsigned integer |
| 40–43 | 4 | First four bytes of `BLAKE2b-256(domain || bytes[0..40])` |

`bytes[0..40]` excludes byte 40. The checksum domain is the ASCII string `KaspaPong/creation-intent/26` followed by a zero byte. The checksum catches copy errors. The host proves ownership by signing the start.

## Tag bit layout

Bit 63 is the most significant bit of the decoded tag. Both ends of each range are included.

| Tag bits | Width | Meaning |
| --- | ---: | --- |
| 63–59 | 5 | Invite format version. The current parser accepts 15. |
| 58–55 | 4 | DAA per ball step, directly encoded from 1 to 15. Presets 5, 6, 7, and 8 are Super Fast, Fast, Medium, and Slow. |
| 54–52 | 3 | Number of balls minus one. Add 1 to the stored value to get 1–8 balls. |
| 51–48 | 4 | Goals required to win, directly encoded from 1 to 15. The host resolves Auto before encoding. |
| 47–40 | 8 | Wager per player, directly encoded as 5–254 whole KAS. |
| 39–0 | 40 | Random nonce generated for the offer. |

For example, version 15, Fast, three balls, six goals, and a 5 KAS wager give the tag prefix `7B 26 05`. The next five bytes are the nonce.

## Decode and match

1. Strip ASCII whitespace and hyphens. Decode Base58 and require exactly 44 bytes.
2. Verify the checksum and public key. Extract the tag and check its version and field ranges.
3. Join on the client's selected network. Use the **host key and full tag together** to identify the offer.

The reservation and start carry the queue information separately.

| Payload | Fields in order |
| --- | --- |
| Guest reservation | `QGAMEB`, host key and tag, guest set descriptor, funded amount in sompi as a little-endian 64-bit integer |
| Host start | `QSTARTB`, host set descriptor |

Each 129-byte descriptor contains the owner key, set covenant ID, previous game ID, generation, three lane reserves, and prior side. See [match startup](STARTUP.md) for how the payloads are checked against accepted transactions.

See [invite encoder and parser](../src/node/invitations.rs), [game speed encoding](../src/engine.rs#L54), [queue descriptors and payloads](../src/node/queue_codes.rs).
