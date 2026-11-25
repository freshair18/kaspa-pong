# Focused tests

From the Pong crate, select the affected case or module.

| Target | Coverage |
| --- | --- |
| Library (`--lib`) | Reusable covenant transactions, queue recovery, submission, RPC and menu behavior |
| `node` | Wallets and client settings |
| `gameplay` | Physics, projection, collisions and animation |
| `ui` | Controls, approvals, rematches and game messages |
| `contract` | Reproduction of the shipped covenant artifact |

```powershell
cargo test --locked -p kaspa-pong --lib node::queue_book::tests
cargo test --locked -p kaspa-pong --lib reusable::tests
cargo test --locked -p kaspa-pong --test ui controls::
cargo test --locked -p kaspa-pong --test gameplay collision::rendered_outer_edges
```

When changing the covenant or compiler, rebuild the embedded artifact in a
temporary directory and compare its identity.

```powershell
cargo test --locked -p kaspa-pong --no-default-features --features tooling --test contract current_artifact_compilation -- --ignored
```
