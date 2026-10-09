# Packaging and service activation

The installed layout is intentionally conventional:

```text
/usr/lib/touchbar/touchbard
/usr/lib/touchbar/touchbar-sessiond
/usr/bin/touchbarctl
/usr/lib/touchbar/touchbar-plugin-host
/usr/lib/touchbar/touchbar-plugin-supervisor
/usr/lib/touchbar/touchbar-secret-helper
/usr/lib/touchbar/service-admin-root
/usr/lib/touchbar/touchbar-t2-mode
/usr/lib/systemd/system/touchbar.service
/usr/lib/systemd/system/touchbar-t2-display.service
/usr/lib/systemd/system/touchbar-t2-sleep.service
/usr/lib/systemd/user/touchbar-session.service
/usr/lib/udev/rules.d/90-touchbar-t2.rules
/usr/lib/udev/rules.d/99-touchbar.rules
```

The hardware service has a runtime dependency on libinput and udev. Its
libinput callback accepts only exact `/dev/input/eventN` paths, rejects write
access, and opens with `O_NOFOLLOW`. The systemd unit independently confines
it to read-only `char-input` devices and the Unix/netlink address families
needed by libinput device discovery. Seat event payloads are discarded inside
`touchbard`; only an activity boolean reaches the backlight policy.

Package installation alone does not take over the Touch Bar. After the guarded
physical handoff test passes, activate the installed services from the
graphical user account:

```bash
./scripts/test-packaging.sh
./scripts/install-development-build.sh
touchbar-activate
```

The packaging test builds the exact release artifacts, validates temporary
copies of both units with `systemd-analyze`, verifies the udev rules, and checks
that every installed executable maps to the expected build artifact. It does
not write to `/usr`, change service state, or stop tiny-dfr.

Activation creates `/etc/touchbar/enabled` and, where tiny-dfr is installed,
masks `tiny-dfr.service` because tiny-dfr's package also has udev rules that
start it directly; merely disabling the unit cannot establish one deterministic
DRM owner. Package installation without that root-owned marker is inert. The
tiny-dfr package and configuration remain installed.

## Intel T2

The T2 Touch Bar (USB `05ac:8302`) has two USB configurations. Configuration 1
is the native mode: the T2 draws the function row itself and `hid-appletb-kbd`
drives it. Configuration 2 is the display mode, in which `appletbdrm` exposes
the panel as a portrait 60-pixel-wide DRM card and `hid-multitouch` exposes the
digitizer (`Apple Inc. Touch Bar Display Touchpad`). touchbard owns the display
mode; the backlight is `hid-appletb-bl`, whose three hardware steps are used
directly.

`90-touchbar-t2.rules` enumerates the device in native mode every time, and
`touchbar-t2-mode display` (run by `touchbar-t2-display.service` at boot and
whenever the device enumerates while TouchBar is active) switches it to the
display configuration. Activation does not enable `touchbar.service` for
`multi-user.target` on T2: udev starts it when the display configuration's card
appears, so boot never waits for the switch.

`appletbdrm` has no USB suspend/resume callbacks, so in display mode the USB
core force-unbinds it on every system suspend and probes it again inside the
resume transition. `touchbar-t2-sleep.service` therefore stops touchbard and
returns the Touch Bar to its native configuration before every sleep, and
restarts `touchbar-t2-display.service` after resume. Check the current
state with `/usr/lib/touchbar/touchbar-t2-mode status`.

tiny-dfr is not required on T2. Without it, `touchbar-rollback` returns the
Touch Bar to its native configuration instead.

T2 MacBook Pros with switchable graphics have two render nodes. The session
compositor renders on one DRM render node and offers the same node to
plugins, because a buffer from one GPU generally cannot be imported by the
other. It chooses the GPU that started the display at boot (`boot_vga`),
which is the integrated Intel GPU, so the discrete GPU stays asleep; set
`TOUCHBAR_RENDER_NODE=/dev/dri/renderDN` in the user unit to choose another.
The start-up log names both, for example
`compositor-renderer=Mesa Intel(R) UHD Graphics 630 (CFL GT2) render-node=/dev/dri/renderD128`.

## Desktop context

Automatic profiles select contributions from context facts such as
`application.id`. The user unit enables both desktop sources, each of which
stands aside outside its desktop, and the media source.

- `--hyprland-context` reads Hyprland's event socket (`application.id`,
  `workspace.id`, `activity.*`). Without `HYPRLAND_INSTANCE_SIGNATURE` in the
  unit's environment it logs `context-source=hyprland disabled=true` once and
  does not retry.
- `--gnome-context` provides `application.id` on GNOME through the
  [Window Monitor Pro](https://extensions.gnome.org/extension/8549/window-monitor-pro/)
  extension, which signals every focus change over D-Bus.
  The value is the focused window's WM class, trimmed and lowercased, for
  example `org.gnome.ptyxis` or `firefox_firefox` for the snap Firefox. The bare
  desktop has no `application.id`. Without the extension, or while it is
  disabled, the fact is absent and profiles behave as if no application were
  focused; the source follows GNOME Shell's extension state, so enabling the
  extension later takes effect without a restart. GNOME provides neither
  `workspace.id` nor `activity.*`. Only signals from the current owner of
  `org.gnome.Shell` are accepted. Losing that owner clears the cached focus;
  a replacement owner triggers a fresh extension/focus snapshot. Temporary
  snapshot errors are retried after one second in the background reader.
  A successful snapshot, including a confirmed inactive extension, stops
  those retries; normal focus tracking remains event-driven.
- `--media-context` follows MPRIS players on the session bus and describes the
  now-playing session, chosen as on macOS: the player that most recently
  started playing keeps it while paused, until another one starts or it goes
  away. The media plugin shows the same session: which player started last
  is history that a plugin process started later cannot know, so the session
  daemon records its choice in `$XDG_RUNTIME_DIR/touchbar/media-session` and
  the plugin supervisors follow it while that player plays or is paused.
  - `media.player`: the player's bus name without the MPRIS prefix and
    instance suffix, for example `vlc`, `rhythmbox`, `spotify`, or `firefox`
    for `org.mpris.MediaPlayer2.firefox.instance_1_42`. Chromium-based browsers
    report `chromium`.
  - `media.playback-status`: `playing` or `paused`.
  - `media.active`: true while the session plays and for a minute after it
    stops, so that a paused video keeps its controls long enough to resume it.

  Without a session the three facts are absent. playerctld's mirror name is
  ignored. For a browser, pair the facts with the focused window:

  ```toml
  [[rule]]
  profile = "media"
  priority = 100
  when = { kind = "all", predicates = [
    { kind = "text-equals", key = "application.id", value = "firefox_firefox" },
    { kind = "text-equals", key = "media.player", value = "firefox" },
    { kind = "boolean-equals", key = "media.active", value = true },
  ] }
  ```

## Application shortcuts

Plugins granted `keyboard.shortcut.v1` press fixed shortcuts, such as
alt+left in a browser, only in the applications their grant names and only
while one of them has the focus. Wayland lets no ordinary client type into
another application, so the session daemon presses them through the desktop's
RemoteDesktop portal (`xdg-desktop-portal` with a backend that implements it,
such as GNOME's). The first press asks once whether TouchBar may control the
keyboard; the portal's restore token, kept in
`$XDG_DATA_HOME/touchbar/remote-desktop.token`, keeps that approval. Revoke it
in the desktop's privacy settings or by deleting the file. The portal session
closes after 20 seconds without a press, so GNOME's remote-control indicator
shows only while shortcuts are in use. A refusal is not asked again for ten
minutes. Without the portal, shortcuts are unavailable and nothing else
changes.

The user compositor is enabled under `graphical-session.target`, not the
longer-lived user `default.target`; logout tears it down and a subsequent
graphical login starts it again without relying on compatibility aliases or a
desktop autostart wrapper.

The recovery command is:

```bash
touchbar-rollback
```

It stops the user compositor and hardware service, then unmasks and starts
tiny-dfr where it is installed, or returns an Intel T2 Touch Bar to its native
configuration. It deliberately leaves the TouchBar package installed for
diagnosis or another activation attempt. The rollback helper does not require
the `touchbard` binary or unit to remain intact; recovery depends only on the
already-installed tiny-dfr unit or the T2 mode script. It fails closed if
systemd cannot prove that `touchbard` stopped, so the two DRM owners are never
intentionally started together.

Validate an active installation without changing service state:

```bash
./scripts/test-installed-lifecycle.sh
```

`touchbarctl hardware status` reports both service readiness and whether the
hardware-owned recovery fallback is latched. Holding physical Fn for eight
seconds enters that mode without relying on a visible Touch Bar control;
release and repeat the hold to resume authenticated user sessions.

After installing the current build, exercise that complete disconnect/latch/
reconnect sequence interactively with:

```bash
./scripts/test-installed-recovery.sh
```

The script only observes the already-active services and waits for the two
physical Fn holds; it does not stop a Touch Bar manager or request privileges.

The suspend/resume acceptance is deliberately explicit because it suspends the
machine. Save work first, then run it from the graphical session:

```bash
./scripts/test-installed-lifecycle.sh --suspend
```

After resume it waits for both services and their control sockets, prints the
relevant system and user journals, and fails if the hardware/session boundary
did not recover. `touchbar-rollback` remains the recovery command.
