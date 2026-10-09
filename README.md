# TouchBar

**A Touch Bar platform for Linux.** A GPU-composited runtime for the MacBook Pro
Touch Bar, and a sandboxed plugin system for putting your own things on it.

[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

![The animated pixelated Omarchy wordmark sweeping across the Touch Bar](docs/images/packs/omarchy/screensaver-2008-dark.gif)

> **This is a fork** of
> [cameroncooper/touchbar](https://github.com/cameroncooper/touchbar). Its
> `t2-appletbdrm` branch ports the project to **Intel T2 MacBook Pros** running
> **GNOME**, and adds live system controls, real MPRIS media, per-application
> shortcut buttons, and partial Touch Bar updates. See
> [What this fork adds](#what-this-fork-adds). `main` tracks upstream unchanged.

Plugins are WebAssembly components. They reach the desktop only through
capabilities you grant one at a time, scoped to an exact path, origin, D-Bus
member, command template or keyboard shortcut — never a whole subsystem. Every
call is recorded in an append-only audit log.

If you had MTMR, Pock, or BetterTouchTool on macOS and lost it: this is that,
sandboxed. If you run `tiny-dfr` today: this is a superset that hands the strip
straight back if you don't like it. On an Intel T2 it needs no `tiny-dfr` at
all: rollback returns the Touch Bar to the T2's own native function row.

## What this fork adds

### Intel T2 hardware (`appletbdrm`)

Upstream listed Intel T2 as implemented, but `touchbard` only accepted the
Apple silicon `adp` driver and its DSI connector, missed the T2 digitizer's
name, and scaled the backlight onto a range the T2 does not have. This fork:

- drives the `appletbdrm` USB display and the T2 digitizer, and uses the
  backlight's three hardware steps directly;
- reads Fn from every physical keyboard's key state, so key remappers that
  grab the keyboard (such as xremap) keep Fn working;
- switches the Touch Bar between its two USB configurations with
  `touchbar-t2-mode`, and returns it to the **native** configuration before
  every system sleep. `appletbdrm` has no suspend/resume callbacks and is
  force-unbound on each suspend; the native mode avoids that;
- starts `touchbard` from udev when the display appears, so boot never waits
  for it;
- composites on the GPU that started the display at boot (the integrated Intel
  GPU on switchable-graphics models) and offers plugins the same render node,
  so the discrete GPU stays asleep and client buffers always import.

Details: [packaging/README.md § Intel T2](packaging/README.md#intel-t2).

### GNOME and media context

Profiles switch automatically on context facts:

- `--gnome-context` provides `application.id`, the focused window's class,
  through the
  [Window Monitor Pro](https://extensions.gnome.org/extension/8549/window-monitor-pro/)
  extension. It is signal-driven, with no polling.
- `--media-context` provides `media.player`, `media.playback-status` and
  `media.active` for the now-playing MPRIS session, chosen as macOS does: the
  player that last started playing keeps the session while paused.

The [desktop context notes](packaging/README.md#desktop-context) list every
fact. [`config/profiles-gnome.toml.example`](config/profiles-gnome.toml.example)
is a complete GNOME setup: the system row by default, media controls while a
player owns the session, navigation buttons in Firefox and formatting buttons
in LibreOffice, each with volume and brightness sliders.

### Plugins that do real work

- **System Controls** reads and sets the real volume, microphone, display
  brightness and power profile. It sets the display's backlight, never the
  Touch Bar's own. It follows changes made elsewhere live: `pactl subscribe`
  for audio, GNOME Shell's `BrightnessChanged` for the display. Tapping volume
  or brightness opens a slider bar, as on macOS.
- **Media** shows the live now-playing session from real players (VLC,
  Rhythmbox, browsers): title and artist, transport, and a scrubbable timeline.
  It uses two new typed capabilities, `media.read.v1` and `media.control.v1`,
  instead of raw D-Bus.
- **App Actions** (new) puts an application's own shortcuts on the bar:
  back, forward and reload for Firefox; bold, italic and underline for
  LibreOffice. It uses the new `keyboard.shortcut.v1` capability: each grant
  is a fixed chord for named applications, every press needs a fresh physical
  touch, and the keys go through the desktop's RemoteDesktop portal. GNOME asks
  once. If that dialog outlasts the touch, tap the action again after approving:
  expired or cancelled requests do not press keys. Arbitrary text can never be typed.

### Platform changes

- **Partial updates.** The session daemon tracks which columns of the scene
  changed, down to the pixels a plugin actually redrew, and `touchbard` passes
  them to the kernel as `FB_DAMAGE_CLIPS`. `appletbdrm` then sends only those
  rectangles over USB. A frame that changes nothing is not sent at all. While
  media plays, about 1% of the strip is updated each second.
- **Visibility callback.** A component can export the optional
  `visibility` interface (world `visibility-aware-plugin`) to hear when each
  item comes on the bar and leaves it. Components built for the original
  `plugin@1.0.0` world keep working unchanged.
- **Popovers** hide the items they cover instead of letting them show through.
- **Touch targets:** a touch just above or below a thin control, such as the
  media scrubber, goes to the nearest control in that column.
- **Commands** started by a plugin now end with its supervisor, even when the
  supervisor is killed.

## Build a Touch Bar app — no Mac required

`touchbarctl plugin dev` runs the real compositor and puts the real 2008×60
scene in a desktop window, so you can build and test plugins on any Linux
machine.

```bash
git clone -b t2-appletbdrm https://github.com/kkkingqz/touchbar && cd touchbar
cargo build --release

./target/release/touchbarctl plugin new my-plugin
cd my-plugin
touchbarctl plugin build     # compiles to wasm32-wasip2
touchbarctl plugin dev       # opens the simulator
```

`plugin new` scaffolds a working two-item plugin with tests and release
workflows. `plugin test` renders every item at each responsive width, and
`plugin replay` drives deterministic interaction scenarios with typed fixtures
for every capability — no hardware, no network, no live services.

On a supported MacBook, put the local package on the physical strip without
stopping the hardware service or changing installed plugins:

```bash
touchbarctl plugin run --package . --item main --width 320
```

This launches the workspace session compositor against a disposable store and
restores the installed user session on Ctrl-C. Add `--sandboxed` only when the
test is specifically about production permission and broker behavior.

Plugins can draw with the GPU without leaving the sandbox — vector commands
through Canvas2D, or a small validated WGSL body for animated effects — and
neither needs a single capability. See
[the drawing example](examples/drawing-component-plugin). Native plugins can
take a raw GLES context instead, as an explicit escape hatch.

See [Build a plugin](docs/plugin-author-context.md),
[the simulator](docs/design/onscreen-simulator.md), and
[the other examples](examples).

## Have a Touch Bar MacBook?

### Requirements

| | |
|---|---|
| **Verified upstream** | MacBook Pro 13″ M1 (`MacBookPro17,1`) on Asahi Linux |
| **Expected** | MacBook Pro 13″ M2 (`Mac14,7`) — unverified |
| **Intel T2** | MacBook Pro 16″ 2019 (`MacBookPro16,1`), Ubuntu 26.04, GNOME — ported and tested end to end in an isolated stand with a simulated panel; **not yet verified on the physical Touch Bar** |
| **Also needs** | systemd · polkit · Wayland · `tiny-dfr` as the rollback target on Apple silicon (not needed on Intel T2) |
| **For the GNOME features** | the Window Monitor Pro extension; `pactl` (pulseaudio-utils) and `brightnessctl` for System Controls |

### Install

Nothing happens at install time. The services start only once a root-owned
marker exists, so you choose when to hand the strip over.

On Arch, from the AUR recipe:

```bash
cd packaging/aur/touchbar && makepkg -si
```

Anywhere else, build and install from the checkout (asks for your password
through polkit):

```bash
./scripts/install-development-build.sh
```

Then take the strip over:

```bash
touchbar-activate
```

Check it with `touchbarctl hardware status` and `touchbarctl session status`.
While plugin content is visible, hold Fn for F1–F12. Tap Fn and then press and
hold it again within 400 ms to reveal the trusted media row until release.

Put your profiles in `~/.config/touchbar/profiles.toml`; start from
[`config/profiles-gnome.toml.example`](config/profiles-gnome.toml.example) on
GNOME or [`config/profiles.toml.example`](config/profiles.toml.example)
elsewhere.

### Rollback

```bash
touchbar-rollback
```

Stops both services and hands the strip back: to `tiny-dfr` where it is
installed, otherwise to the T2's native function row. It leaves the package in
place, fails closed rather than ever letting two DRM owners start together,
and does not need the TouchBar binaries to still be intact.

If a plugin ever wedges the strip, hold the physical **Fn** key for eight
seconds: the hardware service drops the user session and latches its own trusted
media/Fn row. Hold it again to resume.

## What ships

A fresh install gives you **the system row** — brightness, keyboard light,
media and volume keys, and F1–F12 while Fn is held — with the same dim and
sleep behaviour as `tiny-dfr`. Everything else is opt-in through profiles.

The first-party packs live in [`plugins/`](plugins):

| Pack | Shows |
|---|---|
| [System Controls](plugins/controls) | live volume, microphone, display brightness and power profile; slider bars |
| [Media](plugins/media) | the live now-playing MPRIS session, transport, a scrubbable timeline |
| [App Actions](plugins/app-actions) | Firefox and LibreOffice shortcuts through `keyboard.shortcut.v1` |
| [Hyprland Navigator](plugins/hyprland) | focus context, workspace scrubbing, typed `hyprctl` |
| [Capture Studio](plugins/capture) | multi-action palettes, destructive state |
| [Command Deck](plugins/command-deck) | a user-owned local action service |

Hyprland Navigator, Capture Studio and Command Deck are still upstream's
vertical slices that prove the platform: read them as tutorials. The battery
item in System Controls still shows a fixed level.

![System Controls volume item at 320 pixels](docs/images/packs/controls/volume-320-dark.png)

The [pack catalog](catalog) is open for submissions and currently empty. Once a
pack is listed, the normal user flow is simply
`touchbarctl plugin install plugin-name`: the host shows resolved permissions,
then installs, grants, and enables it after one confirmation. Canonical GitHub
sources and local `--path` packages use the same flow without a catalog entry.

## How it works

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/diagrams/privilege-split-dark.svg">
  <img alt="touchbard runs as root and owns DRM, evdev, backlight and a restricted uinput keyboard. touchbar-sessiond runs as your user and owns profiles, layout, themes and GPU composition. Plugins are sandboxed components above it." src="docs/images/diagrams/privilege-split-light.svg">
</picture>

Two services, split at the login boundary. `touchbard` runs as root and owns the
hardware; it keeps a trusted fallback row when no session is attached, so a
crash in your session leaves a working function row rather than a dark strip.
`touchbar-sessiond` runs as you, owns everything about the experience, and never
receives DRM master, raw input, or uinput.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/diagrams/sandbox-dark.svg">
  <img alt="A component reaches the plugin host over a WIT interface, which reaches the supervisor, which matches an exact grant before queueing work against one of the scoped capabilities." src="docs/images/diagrams/sandbox-light.svg">
</picture>

More in [the design notes](docs/design): the
[service architecture](docs/design/service-architecture.md), the
[capability and consent contract](docs/design/capability-consent.md), and the
[adversarial review](docs/security/sandbox-abuse-review.md).

## Status

Working: the two-service split, GPU composition with zero-copy DMA-BUF and
partial panel updates, the component sandbox and its sixteen capabilities,
per-session and persistent consent, profiles with Hyprland, GNOME and media
context, theming, the package format and content-addressed store,
deterministic replay, and the simulator.

Known limits:

- On Intel T2 the display path has been exercised only against a simulated
  panel; the physical Touch Bar is still to be verified.
- On Apple silicon, physical presentation runs at **~29.9 Hz**, a limit of the
  experimental ADP kernel driver. Plugin rendering and compositing run at
  60 FPS.
- Of the five presentation policies in the protocol, only anchored overlays are
  accepted; the rest are typed but deliberately rejected.
- Components draw from a fixed catalogue of seven icons.
- `touchbard`'s own fallback row still redraws the whole strip on each change.

The development log, with the acceptance measurements taken at each step, is in
[docs/history.md](docs/history.md).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Note that
`./scripts/test-sandbox-security.sh` is the release gate and needs a real login
session — CI cannot assert OS confinement, so a green badge is not the evidence
that the sandbox holds.

Changes that make sense beyond this fork are welcome upstream at
[cameroncooper/touchbar](https://github.com/cameroncooper/touchbar).

## License

MIT ([LICENSE-MIT](LICENSE-MIT)) or Apache-2.0 ([LICENSE-APACHE](LICENSE-APACHE)),
at your option.

## Credits

[Cameron Cooper](https://github.com/cameroncooper) for the original project.
[`tiny-dfr`](https://github.com/WhatAmISupposedToPutHere/tiny-dfr) for showing
the Touch Bar could work well on Linux, and for being a dependable place to
land. [Asahi Linux](https://asahilinux.org) for the kernel the original runs
on, and [T2 Linux](https://t2linux.org) for the Intel T2 drivers.
