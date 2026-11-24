# Development

Pong uses pinned Argent/Rusty Kaspa Git dependencies; no sibling checkout is
required or discovered automatically. Wrappers respect `CARGO_TARGET_DIR`,
otherwise use this crate's `target` directory. An existing external cache can
still be selected explicitly through `CARGO_TARGET_DIR`. Rust 1.94+ is required.

## Build layout

`src/lib.rs` provides the shared engine, renderer and node modules. Client and
tests import this library rather than compiling separate source copies.
Default features build only the node client. The `pong-build` executable requires
`tooling`. Artifact reproduction and compiler tooling can disable node features.

```powershell
.\setup                         # reports tools/cache; no build/test
.\setup --vscode-ext            # links the local Argent VS Code extension
.\argentc inspect artifacts/reusable # inspect existing compiled scripts
.\argentc build                 # compile to scratch target/argent/reusable
```

These wrappers follow the workflow from
[argent-template](https://github.com/argent-lang/argent-template/tree/fd7e6ad434fbb15001fbc69343c8f0b633d0eba7).
The compiler has explicit `build` and `inspect` commands. Bare or old
`--source/--out` invocations are retired. Scratch compilation never replaces
shipped/live artifacts. Editor settings disable automatic Rust check-on-save.
Reload VS Code after linking the extension. Set `PONG_ARGENT_DIR` explicitly to
its source checkout before using `setup --vscode-ext`;
never uninstall the source-linked junction through the VS Code extension UI.

## Build and publish release packages

1. Push an annotated version tag for the commit you want to release.
2. On GitHub, choose **Actions → Build release packages → Run workflow**
   and enter the tag. The workflow must be on the default branch. It builds
   from the tag with `Cargo.lock` and uploads Windows x64, Linux x64, and macOS
   Apple Silicon and Intel archives to the run.
3. Download the archives from **Artifacts** on the workflow run. Test the
   extracted packages from their own directories. The packaged executable is
   `pong.exe` on Windows and `pong` on Linux and macOS, alongside `artifacts/`.
4. Draft a GitHub release using the same tag, attach the verified archives, and
   publish it when ready. Mark alpha tags as pre-releases.

[Manual GitHub workflows](https://docs.github.com/en/actions/how-tos/manage-workflow-runs/manually-run-a-workflow) ·
[GitHub release instructions](https://docs.github.com/en/repositories/releasing-projects-on-github/managing-releases-in-a-repository)

## Verification scope

Pull-request CI checks formatting and runs Clippy on the default-feature
targets before the Windows build and tests. It pins the Rust toolchain so
formatting and lint rules stay consistent across runs. From the Pong crate,
use the matching local checks and apply formatting when needed.

```powershell
.\check fmt
.\check lint
cargo fmt -p kaspa-pong
```

Clippy treats warnings as errors locally and in CI.

Select the affected module or case. The Windows wrapper requires a filter.

```powershell
.\check unit node::queue_book::tests
.\check unit reusable::tests
.\check ui approval_and_rematch
.\check contract current_artifact_compilation
```

See [focused tests](TESTING.md) for the Cargo commands.

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

The client embeds `artifacts/reusable/artifact.json`, compiled from
`contracts/reusable.ag`. Keep the release bundle under version control and
rebuild it deliberately when the covenant changes. Scratch builds go to
`target/argent/reusable`.
