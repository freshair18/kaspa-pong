# Development

Pong uses pinned Argent/Rusty Kaspa Git dependencies; no sibling checkout is
required or discovered automatically. Wrappers respect `CARGO_TARGET_DIR`,
otherwise use this crate's `target` directory. An existing external cache can
still be selected explicitly through `CARGO_TARGET_DIR`. Rust 1.94+ is required.

## Build layout

`src/lib.rs` provides the shared engine, renderer and node modules. Client and
tests import this library rather than compiling separate source copies.
Default features build only the node client. The `pong-build` executable requires
`tooling`; direct-consensus verification requires `consensus`. Contract checks
and compiler tooling can disable node features entirely.

```powershell
.\setup                         # reports tools/cache; no build/test
.\setup --vscode-ext            # links the local Argent VS Code extension
.\argentc inspect artifacts     # inspect existing compiled scripts
.\argentc build                 # compile to scratch target/argent/pong
```

These wrappers follow the workflow from
[argent-template](https://github.com/argent-lang/argent-template/tree/fd7e6ad434fbb15001fbc69343c8f0b633d0eba7).
The compiler has explicit `build` and `inspect` commands. Bare or old
`--source/--out` invocations are retired. Scratch compilation never replaces
shipped/live artifacts. Editor settings disable automatic Rust check-on-save.
Reload VS Code after linking the extension. Set `PONG_ARGENT_DIR` explicitly to
its source checkout before using `setup --vscode-ext`;
never uninstall the source-linked junction through the VS Code extension UI.

## Verification scope

Run the affected module first. Do not rebuild/test merely to load context or
repeat a successful build without a new change or unresolved failure.

```powershell
.\check unit menu::tests
.\check unit reorg_notice_tests
.\check unit refresh_tests
.\check unit fee_funding_tests
.\check contract wager_payout
.\check contract goal_response_timeout
.\check node wager_setup
.\check node launch_storage
.\check node player_ui
.\check node game_reorgs
.\check node concurrent_pipelines
.\check gameplay collision::rendered_outer_edges
.\check ui controls::
.\check contract contact_protocol
.\check contract collection_split_authorization_contact_and_refund_security
.\check contract geometry_58x32
.\check consensus independent_commands_and_idle_physics_survive_actual_consensus
```

A filter is mandatory. `node` checks the client regressions; `unit` selects
library tests. Contract tests keep signature, timestamp, queue, collision and
refund enforcement. One optional direct-consensus test retains actual sequence
locks, mass validation and full settlement/refund.
Choose only the affected filter; the examples are alternatives, not a checklist
to run after every change. See [the five test layers](TESTING.md).

The accepted-progress timeout module is:
`cargo test --locked --offline -p kaspa-pong --features tooling --test contract timeouts::`.
Confirmation and rematch checks use:
`cargo test --locked --offline -p kaspa-pong --test ui approval_and_rematch`.

Native-node tests live in the `node` target's ignored `rpc` module. They use disposable
local devnets, temporary wallets, disabled proof of work and explicit covenant
activation. Never use funded player wallets for verification. Select one scenario:

```powershell
$env:PONG_KASPAD_BIN = 'C:\path\to\kaspad.exe' # Explicit disposable-node test binary.
.\check rpc host_join_parallel_input_queues
.\check rpc rpc_acceptance_submission_and_reconnect
```

The first scenario targets Host/Join, a guest-funded signed start held unaccepted past its
liveness check, the signed fixed pace, three unaccepted commands, a fourth append,
and quitting then starting another match with the same wallets. The second covers RPC
acceptance, controls, contact deadlines, automatic resolution, clock feed and reconnect. Both are ignored by
default; `check rpc` explicitly enables the selected scenario. They use the
built client unless `PONG_CLIENT_BIN` overrides it. The read-only live public-tip
regression is also opt-in; it never submits player transactions.

Normal builds use `target`; the launcher already runs `target/debug/pong-node.exe`.
If an external cache is explicitly configured, install a verified client build
from that cache, preserving any previous installed executable first:

```powershell
$CachedClient = Join-Path $env:CARGO_TARGET_DIR 'debug/pong-node.exe'
# Only when this differs from the installed path, and after preserving its backup:
Copy-Item -LiteralPath $CachedClient -Destination target/debug/pong-node.exe
```

If a running client locks the file, preserve that executable under a renamed
path and replace it safely; do not kill the user's process. Restart clients to
use the new build. Windows sandbox temporary-file denials require normal tool
escalation, not source changes to work around the sandbox.

## Source versus operational outputs

Wallets, sessions, game/state records, node configuration and runtime logs are
local operational files, excluded by `.gitignore`. Do not delete them as a
repository cleanup: they may be needed to recover funded games. None of the
root wallet/session/game-state examples are tracked.

`artifacts/` is the exception: it is the sole current, versioned covenant release
bundle, generated from `contracts/pong.ag`. The client loads `artifact.json` by
default, tests embed it, and its identity is pinned in the rules. The manifest
and SIL files accompany that release for compiler inspection. Keep this bundle
tracked and update it deliberately on protocol changes; ordinary scratch builds
go to ignored `target/argent/pong` instead. Removing it would require a separate
build/distribution workflow change, not just deleting generated files.
