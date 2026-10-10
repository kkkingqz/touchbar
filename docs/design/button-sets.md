# Button sets: engine, logic, descriptions

The first-party packs use three layers. `touchbar-plugin-host` builds and renders
native retained UI, routes contacts, owns animation clocks and communicates with
the policy broker. A Wasm `button-set-logic` module handles named actions and
returns typed state patches. Each set has its own TOML description, including
layout, state/action bindings, optional selection predicates and panels.

## Register a file explicitly

The package manifest declares one logic module and every set file it uses:

```toml
[runtime]
kind = "button-set-logic"
entrypoint = "component/plugin.wasm"
world = "touchbar:plugin/button-set-logic@1.0.0"

[[sets]]
id = "counter"
file = "sets/counter.toml"

[[sets]]
id = "compact-counter"
file = "sets/compact-counter.toml"
```

There is no directory scan. A file's `id` must match its reference. Set packages
use `[[sets]]` instead of legacy `[[items]]`; the host derives the catalog from
these descriptions. Assets and broker permission scopes remain in the manifest.
The complete runnable example is `examples/button-set-logic`.

```toml
version = 1
id = "compact-counter"
label = "Compact Counter"
logic_config = "counter"
default_width = 120

[layout]
kind = "button"
widget_id = 10
label = "Increment compact counter"
on_activate = "increment"
text_from = "count_text"
font_size = 18
background = "accent"
color = "on-accent"
```

`logic_config` selects the module's behavior. `describe` declares the accepted
named actions, their event/value types and the state schema. `initialize` returns
initial state. The host checks every binding against that contract before using
the set. `handle-action`, context, visibility, presentation and broker callbacks
return patches; they do not return UI trees. Descriptions cannot supply commands,
executables, interpreter snippets or arbitrary native function names.

## Reuse code, keep state separate

Both counter descriptions call the same `increment` implementation in the same
Wasm artifact. Each selected set gets a separate Wasmtime Store, initial state,
broker connection, pending requests, resources, contacts and presentation state.
Incrementing one counter leaves the other at zero. A later description can reuse
existing actions by naming them; no Wasm rebuild is needed. Additional behavior
belongs in Rust modules or shared libraries and requires a build.

The concrete engine facade is `touchbar_logic_sdk::engine::ENGINE`. For example,
a media action calls `ENGINE.media().command(...)`, volume logic calls
`ENGINE.audio().set_volume(percent)`, and a shortcut action calls
`ENGINE.keyboard().shortcut("reload")`. Command adapters use
`ENGINE.commands().run(...)`; framed local service adapters use
`ENGINE.local().connect(...)` and `.send(...)`. These are typed entry points to
the existing broker. The manifest still bounds their command IDs, endpoints,
arguments and other scopes, and the user still controls consent. Shared code
confers no grant and cannot use another instance's resources.

`describe`, initialization and context rendering are pure phases. Privileged
operations require an allowed callback and, where required, a fresh physical
activation. Visibility changes and panel dismissal cancel contacts; stale Up
cannot activate a new or restarted surface. Logic closes or cancels its own
subscriptions and requests when hidden. Removing an instance disconnects its
broker; supervisor resource handles are released independently of other sets.

## Layout and state

Layouts support rows, columns, stacks, buttons, text, symbols, packaged images,
sliders, meters, progress, scrubbers, keyed repeats, bounded canvas records,
responsive variants, validated shader effects and host-timed motion. Rows size
children by `width`, columns by `height`. That dimension alone is fixed; with
`minimum`, `maximum` or `grow`, it is the flex basis and may shrink or grow within
those constraints. State bindings use the `_from` fields;
`enabled_from` removes interaction when false and `visible_from` hides content.
Semantic color bindings must resolve to a known theme role.

State contains booleans, finite numbers, text and bounded lists of keyed scalar
records. Repeat keys preserve action identity through reordering; removed keys
cancel their contacts. Canvas records support rect, line, circle and text
commands, semantic paint, or all four numeric `r/g/b/a` channels in [0,1]. Motion
and effect definitions remain in TOML, with stable IDs and bounded parameters;
the host advances their clocks and honors reduced motion without per-frame Wasm
render calls. Omarchy's theme-derived OLED geometry is bounded drawing data.

`[[panels]]` can contain `[[panels.element]]` items, spacing and typed groups,
preserving individual widths and principal items. `expanded_panel` and
`press_and_hold_panel` refer to local panel IDs. Helpers share their owning
set's lifecycle; a panel's referenced primary items keep their own instances.

A `[selection]` supplies exact application/activity identities and optional
`label`, `enabled_by_default` and `priority`. It generates a package-local
automatic profile. Package priorities order package rules; user rules retain
precedence. Cross-package composition belongs in the user's profiles, not here.

Limits include 256 KiB per file, 64 declared files and 1 MiB per catalog, 256
static/512 expanded nodes, 32 layout levels and 16 KiB state text. The host also
checks action types, asset IDs, viewport values and native UI/effect budgets.

## Authoring and immutable installation

```sh
touchbarctl plugin build --package examples/button-set-logic
touchbarctl plugin check --package examples/button-set-logic
touchbarctl plugin test --package examples/button-set-logic
# Edit or add sets/*.toml, then explicitly register a new file.
# The existing module needs no build when its behavior is unchanged.
touchbarctl plugin check --package examples/button-set-logic
touchbarctl plugin pack --package examples/button-set-logic --output counter.touchbar
# User-chosen installation:
touchbarctl plugin install --path counter.touchbar
```

`check` validates the resolved catalog and renders declared width cases; replay
can exercise physical input and scope-checked offline broker fixtures. Package
archives and lock metadata cover every referenced set file as well as Wasm and
assets. Production supervisors verify digests and pass a sealed set bundle to
the host. The host never overlays mutable source files on those sealed bytes.
Editing an installed immutable package is detected as tampering; author changes
in a source package, check it and install the resulting new package.

Legacy `plugin` and `visibility-aware-plugin` components, native packages and
separate `appearance-provider` components remain supported. The appearance
provider's world, entrypoint and permission projection are unchanged by this
migration. Root profile includes are documented in [profile composition](profile-composition.md).
