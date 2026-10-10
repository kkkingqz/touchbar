# Button-set migration verification — 2026-10-10

The approved three-layer migration covers 23 primary sets in seven first-party
packages. Every completed task was committed and pushed to `t2-appletbdrm`.
The implementation range starts after `18e132b`; its twelve task commits are:
`a398deb`, `7ef7bcf`, `90f1498`, `b8886e9`, `e86567b`, `b7e4c7a`,
`baab2e0`, `d4bc4e4`, `b6f6e9f`, `1578c01`, `ed223f5`, `eea582c`.

## Fresh review and regression fixes

A separate read-only `gpt-6-astra` reviewer inspected `18e132b..eea582c` in
passes across runtime, rendering, schema, package sealing, profile includes,
first-party logic and authoring. No critical or minor findings were reported.
All seven important findings were reproduced before their fixes:

| Finding | RED → GREEN regression |
|---|---|
| Unchanged hide/show discarded input bindings | `unchanged_hide_show_keeps_bindings_and_rejects_old_contact` |
| Columns ignored height and hid Media seek target | `columns_use_height_and_rows_use_width`, `timeline_seek_is_reachable_at_native_bar_height` |
| Custom panel layouts were not selected | `presentation_selects_custom_panel_then_restores_primary`, `native_presentation_uses_lifecycle_to_select_panel`, `presentation_events_wake_static_frames_without_resize` |
| Motion after replacement sought the new track | `track_change_cancel_seek_and_visibility_cleanup_use_real_logic` with motion after replacement |
| Reused Controls/Omarchy confused public ID with configuration | `reused_configuration_restarts_watch_after_hide_show`, `omarchy_configuration_can_be_reused_under_another_id` |
| Cancellation patch arrived after returned UI construction | `cancellation_patch_is_reflected_in_the_returned_ui` |
| Nested repeat keys collided | `nested_repeat_identity_includes_outer_keys` |

The fix pass retains existing authority checks. Native presentation lifecycle
metadata selects an owner-local sealed panel, without changing guest WIT or
Wayland contracts. Cancellation patches converge under the existing contact
budget. Only captures are cancelled; live bindings remain available for a fresh
contact. Media requires a new Down after track replacement.

## Verification environment and scope

Final post-fix verification passed: **591 workspace tests, 154 native supervisor
tests and 8 additional supervisor campaigns**, plus the complete host packaging
campaign. All seven review findings are fixed, with no deferred minor findings.
The five live host tests passed earlier in this execution; their source did not
change during review fixes.

Rust checks run in `touchbar-build`: `cargo fmt --all -- --check`,
`cargo clippy --workspace --all-targets -- -D warnings`, workspace tests and
doc tests. Supervisor confinement tests run as the freshly compiled native
host binary through `scripts/ci-session.sh` because cgroups/user namespaces differ
inside the build container. A direct retry without that harness correctly failed
closed on missing cgroup delegation; rerunning through the harness passed without
changing code or assertions.
Ordinary supervisor tests: 154; additional ignored security campaigns: 8.
The supervisor CLI tests also run in the build container.

All five other ignored live tests were exercised on the host: sessiond GNOME
provider and GPU damage (2), MPRIS desktop session (1), Fn keyboard and seat0
activity (2). The current MPRIS desktop session had no active player; deterministic
real-broker Media tests and replay cover player state and seek behavior.

The complete host `ci-session.sh test-packaging.sh` campaign builds/checks all
seven packages and covers sealing, file tampering rejection, legacy launches,
shared Wasm isolation, layout-only edits, permission changes, profile include
reload, presentations, native commit budgets, idle behavior, hardware GPU,
DMABUF/explicit sync, canvas/shader effects, assets and temporary installer/store
lifecycle. Authoring tests additionally cover denied shortcuts and resource
release without affecting another set instance.

These are temporary test sessions and stores, not a permanent installation.
AUR and the untracked nested checkout are excluded. No physical Touch Bar
hardware run is claimed. Existing demo-state/product limitations are listed in
`docs/design/first-party-packs.md`.

## Decisions and their costs

The execution ledger's rulings are preserved verbatim below, in execution order.
There are no deferred minor findings.

Ruling: implement in the existing feature checkout — user selected sequential work and commit/push on the ongoing fork branch; avoid moving the branch or touching nested checkout — cost if wrong: less checkout isolation, mitigated by clean tracked baseline.
Ruling: use flatpak-spawn --host git for network operations — bundled git-remote-https lacks libcurl-gnutls.so.4; host ls-remote succeeded — cost if wrong: host Git configuration could differ; verify remote and branch explicitly on every push.
Task 1: Ruling: renderer-specific animation/effect/canvas command representations are completed in Task 3 alongside native decoding — core schema already bounds trees/state; avoid freezing renderer details before reading their consumer — cost if wrong: Task 3 may need additive schema fields, which must receive focused tests.
Task 2: Ruling: add pure handle-context-changed(RenderRequest) to logic contract — existing responsive controls and Omarchy shader data depend on viewport/theme, absent from the initial callback list; renderer remains independent and effect-free context uses Render phase — cost if wrong: one extra mandatory guest callback and host invocation, covered by build and Task 4 phase tests.
Task 2: Ruling: audio.toggle_mute submits optional named toggle-volume through existing command.run.v1 — current controls only grant unmute-volume; existing grants remain unchanged and unknown command scopes fail closed — cost if wrong: caller gets scoped broker error until it declares that exact command.
Task 3 Ruling: canvas uses bounded scalar command records (rect/line/circle/text), shader and motion definitions are declarative and reuse native validators/clock registries — matches current packs and keeps guest output data-only — cost if wrong: further native drawing operations need explicit schema support. Scrubber continuous input shares slider behavior; keyed discrete cells use repeat buttons. Panel rendering uses owner state and replaces its contact routing.
Task 4 Ruling: require a captured Down for continuation of a physical contact, retain capture until Released/Cancelled — native UI emits Activated before Released; otherwise a restarted Store would accept old Up — cost if wrong: synthetic tests must use contact=None or send Down like physical input.
Task 4 Ruling: allow cancel/close in Lifecycle, retaining broker ownership enforcement — existing shared host guard blocked visibility cleanup; this releases resources, grants no new operation scope — cost if wrong: lifecycle may close its own resources earlier, verified phase tests.
Task 4 Ruling: presentation is allowed for delivered native Input callbacks (including real hold), independently of privileged activation; cancellation/context/lifecycle prohibit it — existing hold presentations have no Activated token; broker activation rules remain unchanged — cost if wrong: headless native input can request a harmless presentation like legacy. Revocation enforcement remains the shared broker/supervisor path and is exercised with real migrated drag logic in Task9.
Task 5 Ruling: resolved catalog is serde-skipped metadata on PluginManifest with catalog_items/profiles/bars accessors — raw manifest bytes and explicit set references stay intact for archive and sealing — cost if wrong: consumers must use catalog accessors; converted store/CLI/session metadata, production delivery follows Task6.
Task 5 Ruling: panel layout is optional and defaults to owner primary layout — avoids duplicate static widget IDs or synthetic helper instances when a wider responsive owner is presented — cost if wrong: specialized panels must declare their own layout explicitly; both forms covered.
Task 5 Ruling: directory/headless runtime selects the requested set before creating its Store; CLI checks each separately — production already launches one item per process, so no broker channel or request IDs are shared between stores — cost if wrong: one replay cannot switch across standalone sets; simultaneous reuse covered by real isolated runtime tests.
Task 6 Ruling: reserve child FD9, duplicate sources>=10, explicitly close supervisor-only7/8 and close>=10 — old descriptors3–6 preserved; appearance/shortcut channels stay in supervisor — cost if wrong: any inherited channel expansion must update fixed descriptor close rules.
Task 6 Ruling: package priorities order equal-tier automatic rules by descending priority then source/ID, without exceeding user priority floor — preserve explicit user precedence and activity specificity — cost if wrong: legacy equal-priority overlap resolves by stable ID; tests cover current package and user scenarios.
Task 6 Ruling: open bounded package members nonblocking before checking regular-file metadata — a FIFO otherwise hangs open before validation, reproduced under timeout; applies to new directory reader and supervisor open_beneath — cost if wrong: regular-file reads unaffected; FIFO/real sealed launch verified.
Task 7 Ruling: retain last-good user document if its root disappears after startup — absence during an atomic edit must not silently replace the active catalog — cost if wrong: explicit removal requires a valid replacement document or restart to return to automatic-all-items.
Task 7 Ruling: merged ProfileDocument clears includes and LoadedProfiles retains source paths — runtime consumes a complete validated document, not unresolved references; no API writes it back to user files — cost if wrong: future save support must preserve root/fragments separately.
Task 9 Ruling: move existing typed presentation composition metadata to set-config, re-export from package and allow panel.element/principal_item — flat item refs cannot preserve existing group layout and per-item widths — cost if wrong: additive schema surface; old package names/serialization remain identical and validators/headless/live tests cover it.
Task 9 Ruling: interactive presentation acceptance grants the unchanged command scope in its temporary store — command controls now expose denial via disabled hit targets, so interaction fixture needs actual authorization; independent denial replay/runtime tests cover no-grant behavior — cost if wrong: permission-less interaction coverage is separate from presentation geometry.
Task 9 Ruling: cancellation drops unsent writes and refreshes the reported level; already completed writes from a continuous drag are not rolled back — preserves real-time slider behavior and avoids inventing restoration writes — cost if wrong: cancellation restores authoritative system state rather than the level at initial Down.
Task 10 Ruling: keep the existing one-session Media product surface; keyed repeat behavior is exercised with actual MPRIS bus-name records in renderer tests — current MediaState/packs expose no player-list item, so adding one would exceed migration scope — cost if wrong: a future selectable player list needs a separate data contract/logic action.
Task 10 Ruling: add plain and disabled_opacity properties for metadata wrappers — a presenting wrapper must stop intercepting input while preserving nested transport controls and their appearance — cost if wrong: additive visual schema surface; defaults remain previous renderer behavior and values are bounded.
Task 11 Ruling: add semantic color bindings, bounded RGBA canvas records and optional selection label to the description schema — preserve recording appearance, OLED projection and original automatic-profile label without UI builders in logic — cost if wrong: additive visual metadata; no new broker authority, numeric/color/schema checks remain bounded.
Task 11 Ruling: normalize checked TOML values through a heap value tree and the compact JSON typed deserializer — expanded derived TOML deserializer overflowed the default test-thread stack at the allowed 32 levels; finite values and structural limits are checked before normalization — cost if wrong: extra bounded parse allocations; dates must remain quoted text.
Task 11 Ruling: width is a flex basis when minimum/maximum/grow are declared, otherwise fixed — original native flexible controls must shrink to retain trailing buttons at default widths; a physical coordinate test reproduced the hidden probe — cost if wrong: affects flexible declarative descriptions; literal fixed-width nodes and legacy UI keep their semantics.
Task 11 Ruling: update the live asset acceptance to pass declared set-file digests — supervisor correctly rejected the old asset-only launch arguments — cost if wrong: test launch now follows the sealed-set contract rather than legacy arguments; legacy effect test remains covered.
Task 12: Ruling: update the real CLI context function and its included author document rather than nonexistent sdk_context_markdown — the plan named a stale function; JSON/text now expose the actual new world — cost if wrong: future context generators must preserve both formats.
Task 12: Ruling: retain intentionally rebuilt first-party Wasm artifacts including Omarchy appearance provider — the approved tooling builds all seven complete packages; provider source/world/entrypoint/scopes are unchanged — cost if wrong: provider binary identity changes and existing digest-bound grants may require renewed approval.
Task 12: Ruling: split workspace verification between container and host — supervisor confinement requires native host cgroups/user namespaces; all 154 normal and 8 ignored supervisor tests and both ignored sessiond GNOME/GPU tests passed on host — cost if wrong: separate test binaries need to match the verified source, checked through current no-run build.
Task 12: Ruling: execute final whole-branch review after task completion commit, with review fixes in their own commit — executing-plans requires a final fresh reviewer after all task completion lines — cost if wrong: the pushed feature branch temporarily includes code awaiting final review; no merge or installation.
Task 12: Ruling: commit and push each completed task — direct user instruction overrides the plan's stale final line excluding push — cost if wrong: remote feature branch receives each verified intermediate task, never main.
Final: Ruling: AUR recipe, permanent installation/service operation and nested checkout remain excluded — user explicitly excludes AUR and authorizes source refactoring with commit/push; acceptance uses only temporary sessions — cost if wrong: these separate integration paths are not claimed verified by this migration.
Final: Ruling: historical issues outside 18e132b..eea582c remain outside this review — independent reviewer received the approved migration range; earlier fork fixes have their own history — cost if wrong: an unrelated pre-existing defect can remain outside this migration review.
Final: Ruling: preserve nested repeats using full repeat-key ancestry rather than reject them — schema already accepts recursive templates and users expect independently addressed cells — cost if wrong: nested widget IDs change relative to the formerly colliding implementation; top-level repeat identity is preserved.
Final: Ruling: carry persistent/transient presentation lifecycle through an additive native StartedWithLifecycle event while mapping it to unchanged guest Started — selects sealed owner panel without changing WIT or Wayland contracts; native client schedules a frame even when geometry is unchanged — cost if wrong: native Rust consumers need recompilation and low-frequency presentation confirmations can trigger an extra frame.
Final: Ruling: bound cancellation rerender convergence by 32 captured contacts plus a final pass — cancellation consumes captures without creating new ones; state/routing returned together rather than stale UI — cost if wrong: a nonconvergent guest is rejected instead of displaying inconsistent state.
