# Focused test layers

The integration suite has five explicit Cargo targets. Production-module unit
tests stay next to their implementation. Select the affected module or case;
do not run every layer after a small change.

| Target | Scope | Required feature |
| --- | --- | --- |
| `contract` | Covenant invariants, adversarial transitions, timeout policy, artifact reproducibility | `tooling` |
| `node` | Client/tracker, invitations, funding, reorgs, concurrent pipelines, opt-in RPC fixtures | `node` (default) |
| `gameplay` | Physics/projection, collision edges, paddle geometry, animation | None; covenant edge checks additionally need `tooling` |
| `ui` | Menu and gameplay controls, completion messages, goal notices | `node` (default) |
| `consensus` | Disposable native consensus validation | `consensus` |

Examples (run from the Pong crate):

```powershell
cargo test -p kaspa-pong --test ui controls::
cargo test -p kaspa-pong --test gameplay collision::rendered_outer_edges
cargo test -p kaspa-pong --features tooling --test contract timeouts::
cargo test -p kaspa-pong --test node invitations::
```

The `node::rpc` tests are ignored by default and require `PONG_KASPAD_BIN`;
they launch disposable local devnet nodes. The client module also retains its
ignored, explicitly configured read-only live-network case. Do not enable these
as routine checks or use funded wallets for testing. Native consensus is also
opt-in, not part of the lightweight UI/gameplay loop.

Two ignored comparisons against historical Git artifacts were removed, along
with a v1.0-relative script-size assertion. They are not current protocol
invariants. Current artifact recompilation and pinned identity checks remain.
The lane-selection check compares state and fees between current workloads,
not a hardcoded historical fee. Historical fee measurements are documented in
CONTACT_DESIGN.md; they must not be mistaken for current fee guarantees.

Keep new integration coverage inside these domain targets. Prefer extending an
existing case table for symmetric sides, boundary values, or input events over
creating another scenario-specific test binary. Retain distinct checks when
they exercise different trust boundaries or real regressions.
