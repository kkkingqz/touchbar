# Contributing

## Building

```bash
cargo build --workspace
```

System libraries: libwayland, EGL/GLES, libdrm, libudev, libinput, and
libsystemd. `ripgrep` is a hard dependency of the acceptance scripts. Component
plugin work additionally needs the `wasm32-wasip2` Rust target and
`wasm-component-ld`.

## The release gate

```bash
./scripts/test-sandbox-security.sh
```

This is the gate that matters. It checks formatting and lint policy, runs the
complete workspace suite and the longer ignored security campaigns, compiles
every fuzz target, audits both lockfiles, and exercises a real supervised
component host and the permission CLI.

Add `FUZZ_SECONDS=60` for fresh sanitizer fuzz campaigns and `RUN_APPLE_GPU=1`
to include the Apple GPU DMA-BUF rendering path without taking over the Touch
Bar. `./scripts/test-packaging.sh` validates both systemd units, the udev
rules, and the installer without writing to `/usr`.

### It needs a real login session

Run it on the host from a user login session. The supervisor's confinement
requires a private, user-owned cgroup with the `pids` controller delegated,
Landlock, seccomp, `pidfd`, and a session bus. A graphical login alone does not
guarantee cgroup delegation. The gate
uses `scripts/ci-session.sh` to run tests in a delegated user systemd scope
when necessary; it preserves the caller's environment and needs no sudo.
The wrapper moves its runner into a `touchbar-supervisor` leaf, leaving the
parent empty. The installed user service uses the same layout through
`DelegateSubgroup=touchbar-supervisor`. Commands and component hosts get
sibling cgroups under that parent; the supervisor enables its delegated
controllers there before configuring child limits. Command launches require
the 33-task limit before starting any child. Delegation without enabling the
controller used to leave the limit absent; the per-user process rlimit cannot
substitute for it. Child cgroup directories are created with private modes
regardless of the caller's umask, so SIGKILL recovery can safely reclaim them.

Command tests require standalone GNU coreutils and a standalone `/usr/bin/sh`.
They select `/usr/bin/gnu<tool>` when available, otherwise `/usr/bin/<tool>`,
and copy the binaries to private, user-owned fixtures outside the package.
This supports distributions whose default coreutils are multiply linked Rust
applets, and systemd user namespaces where host root ownership is unmapped.
Production ownership, hardlink, and descriptor-pinning checks remain enforced.
Root-owned command paths remain rejected when their owner is unmapped in the
service's user namespace.
Missing fixtures fail explicitly rather than silently skipping assertions.
The GPU asset acceptance script uses Python 3.11 or newer to read the package
manifest and bind every declared asset digest.

If you build in a container, compile tests there, then run the resulting binary
on the host. For example:

```bash
# In the build environment; Cargo prints the test executable path.
cargo test -p touchbar-plugin-supervisor --lib --no-run
# In the host login session, using the path Cargo printed above.
./scripts/ci-session.sh target/debug/deps/touchbar_plugin_supervisor-<hash>
```

A container or a hosted runner may lack delegation, the host user manager, or
kernel confinement. The wrapper reports a missing user manager explicitly;
it does not weaken the sandbox to make these environments pass.

## What CI covers, and what it does not

GitHub Actions runs formatting, lint, the workspace test suite excluding
`touchbar-plugin-supervisor`, fuzz-target compilation, and a dependency audit
of both lockfiles.

`touchbar-plugin-supervisor` is excluded on purpose. Its tests assert operating
system confinement that a hosted runner cannot provide, so running them there
would assert nothing while reporting green — worse than not running them. The
same applies to the supervised component host, the permission CLI, and
packaging.

**Run `./scripts/test-sandbox-security.sh` locally before opening a pull
request.** Its output is the evidence that the sandbox still holds; a green CI
badge is not.

## Documentation images

```bash
./scripts/generate-doc-images.sh
```

Renders every first-party item at each responsive width in both themes through
the production renderer. Replay is deterministic, but rasterization runs on
whatever GPU is present, so images differ between renderers. Regenerate on the
same machine that last did, and `--check` confirms a tree is current locally.

## Commits

Commit messages explain why a change was made, not only what changed. The
project follows a greenfield v1 rule while unpublished: superseded APIs and
formats are changed in place and removed completely rather than kept behind
compatibility shims.

## Button packs and profile fragments

Use the [three-layer authoring contract](docs/design/button-sets.md): native
rendering and broker operations, shared Wasm interaction modules, and one
explicitly registered TOML description per set. Keep state and resources local
to the instance even when two descriptions use identical code. A layout-only
edit needs `plugin check`/`pack`, with no Wasm build. Added actions require a build.
The two-counter package in `examples/button-set-logic` is a runnable example.

`./scripts/build-first-party-packs.sh` builds and checks all seven first-party
packs, including App Actions and Omarchy. It also builds separately declared
appearance providers. Both logic-only and legacy component worlds remain valid.
First-party Wasm updates should be intentional migration/release artifacts;
avoid committing rebuilt unrelated example components.

`./scripts/test-packaging.sh` includes sealed-set and authoring acceptance,
legacy launches, profile include/reload, permissions, presentations, effects and
asset lifecycle. These tests use temporary stores and delegated test scopes.
They validate packaging without installing a persistent service. Run supervisor
and ignored security tests on the host login session, as described above;
container compilation alone does not verify host confinement.
