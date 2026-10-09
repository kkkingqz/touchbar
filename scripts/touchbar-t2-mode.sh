#!/bin/bash
# Switch the Intel T2 Touch Bar (USB 05ac:8302) between its two USB configurations.
#
#   native   configuration 1: the T2 draws the function row itself (hid-appletb-kbd).
#   display  configuration 2: the host draws it through appletbdrm, owned by touchbard.
#
# appletbdrm has no USB suspend/resume callbacks, so the USB core force-unbinds it on
# every system suspend and probes it again inside the resume transition. The native
# configuration takes no such detour, so touchbar-t2-sleep.service returns to it before
# every system sleep and touchbar-t2-display.service leaves it again after resume. Both
# configuration changes are ordinary hotplug and happen while userspace is running.
#
# usage: touchbar-t2-mode native|display|status
set -euo pipefail

readonly vendor=05ac
readonly product=8302
readonly marker=/etc/touchbar/enabled
readonly digitizer_name="Apple Inc. Touch Bar Display Touchpad"
readonly lock=/run/touchbar-t2-mode.lock

log() {
    echo "touchbar-t2-mode: $*"
}

find_device() {
    local device
    for device in /sys/bus/usb/devices/*; do
        [[ -f $device/idVendor && -f $device/idProduct ]] || continue
        [[ $(<"$device/idVendor") == "$vendor" && $(<"$device/idProduct") == "$product" ]] || continue
        realpath -- "$device"
        return 0
    done
    return 1
}

current_config() {
    tr -d '[:space:]' <"$1/bConfigurationValue"
}

# The same idiom tiny-dfr's and the native pin rule use: unconfigure first, so every
# interface of the old configuration is gone before the new one binds. `force`
# re-selects a configuration that is already active, which probes its drivers again.
# A failed write is reported rather than fatal, so callers can still fall back.
set_config() {
    local device=$1 wanted=$2 force=${3:-}
    if [[ $(current_config "$device") == "$wanted" && -z $force ]]; then
        return 0
    fi
    log "switching ${device##*/} to configuration $wanted"
    if ! { echo 0 >"$device/bConfigurationValue" && echo "$wanted" >"$device/bConfigurationValue"; }; then
        log "WARNING: could not select configuration $wanted"
        return 1
    fi
}

under() {
    [[ $(realpath -- "$1" 2>/dev/null) == "$2"/* ]]
}

native_ready() {
    local node
    for node in /sys/bus/hid/drivers/hid-appletb-kbd/*/mode; do
        [[ -e $node ]] && under "$node" "$1" && return 0
    done
    return 1
}

display_ready() {
    local card input driver have_card=false have_digitizer=false
    for card in /sys/class/drm/card*; do
        [[ ${card##*/} =~ ^card[0-9]+$ ]] || continue
        driver=$(realpath -- "$card/device/driver" 2>/dev/null || true)
        [[ ${driver##*/} == appletbdrm ]] && under "$card" "$1" && have_card=true
    done
    for input in /sys/class/input/input*; do
        [[ $(cat -- "$input/name" 2>/dev/null) == "$digitizer_name" ]] || continue
        under "$input" "$1" && have_digitizer=true
    done
    [[ $have_card == true && $have_digitizer == true ]]
}

# Waits count polls instead of reading SECONDS: a run frozen across a system sleep
# must not find its deadline expired the moment it thaws.
wait_until() {
    local polls=$(($1 * 10))
    shift
    until "$@"; do
        ((polls-- > 0)) || return 1
        sleep 0.1
    done
}

# Prints the device once the same USB device node has existed for `settle` seconds.
# On t2bce's no-state resume path the whole VHCI re-enumerates shortly after thaw;
# waiting it out avoids switching a device that is about to disappear.
wait_for_stable_device() {
    local polls=$(($1 * 5)) settle=$(($2 * 5))
    local device identity previous="" stable=0
    while ((polls-- > 0)); do
        if device=$(find_device) && identity=$(stat -L -c %i -- "$device" 2>/dev/null); then
            if [[ $identity != "$previous" ]]; then
                previous=$identity
                stable=0
            elif ((++stable >= settle)); then
                printf '%s\n' "$device"
                return 0
            fi
        else
            previous=""
        fi
        sleep 0.2
    done
    return 1
}

# The sleep hook is "activating" while it switches to native and "active" from
# then until its ExecStop after resume, which is also what starts the display
# switch again. sleep.target only becomes active once the hook has finished.
sleep_in_progress() {
    local unit state
    for unit in touchbar-t2-sleep.service sleep.target; do
        state=$(systemctl show "$unit" --property=ActiveState --value 2>/dev/null || true)
        [[ $state == active || $state == activating ]] && return 0
    done
    return 1
}

take_lock() {
    exec 9>"$lock"
    flock --timeout 60 9
}

activated() {
    [[ -f $marker ]]
}

cmd_native() {
    if ! take_lock; then
        log "ERROR: another mode switch did not finish within 60 s"
        return 1
    fi
    # touchbard must let go of the DRM card and digitizer before their drivers unbind.
    systemctl stop touchbar.service 2>/dev/null || true
    local device
    if ! device=$(find_device); then
        log "Touch Bar is not enumerated; nothing to switch"
        return 0
    fi
    set_config "$device" 1 || true
    if ! wait_until 10 native_ready "$device"; then
        log "ERROR: hid-appletb-kbd did not bind within 10 s"
        return 1
    fi
    udevadm settle --timeout=5 || true
    log "native mode ready"
}

cmd_display() {
    if ! activated; then
        log "TouchBar is not activated ($marker is absent); staying in native mode"
        return 0
    fi
    if ! take_lock; then
        log "another mode switch did not finish within 60 s"
        return 1
    fi
    # Checked again under the lock: a rollback may have run while this waited for it.
    if ! activated; then
        log "TouchBar was deactivated meanwhile; staying in native mode"
        return 0
    fi
    if sleep_in_progress; then
        log "system sleep in progress; the resume hook switches after wake-up"
        return 0
    fi
    local attempt device force=""
    for attempt in 1 2 3; do
        if ! device=$(wait_for_stable_device 30 2); then
            log "Touch Bar did not appear within 30 s"
            return 1
        fi
        # The marker may disappear while discovery waits (rollback), or the
        # sleep hook may start. Do not change USB configuration in either case.
        if ! activated || sleep_in_progress; then
            log "display switch cancelled before selecting the configuration"
            return 0
        fi
        # A modprobe.d blacklist only stops alias autoloading; loading by name still works,
        # so appletbdrm can stay blacklisted for everything but this switch.
        modprobe appletbdrm || log "WARNING: could not load appletbdrm"
        if set_config "$device" 2 "$force" && wait_until 10 display_ready "$device"; then
            udevadm settle --timeout=5 || true
            if ! activated || sleep_in_progress; then
                log "display switch cancelled before starting touchbard"
                return 0
            fi
            # udev starts touchbar.service when the card appears; this covers a lost event.
            systemctl --no-block start touchbar.service || true
            log "display mode ready"
            return 0
        fi
        # Selecting the active configuration again makes its drivers probe anew.
        force=yes
        log "display configuration did not come up (attempt $attempt)"
    done
    # Never leave the strip dark: fall back to the function row the T2 draws itself.
    if activated && ! sleep_in_progress && device=$(find_device); then
        set_config "$device" 1 || true
    fi
    log "ERROR: display mode failed; left the Touch Bar in native mode"
    return 1
}

cmd_status() {
    local device
    if ! device=$(find_device); then
        echo "device=absent"
        return 0
    fi
    echo "device=${device##*/} configuration=$(current_config "$device")"
    if native_ready "$device"; then
        echo "mode=native driver=hid-appletb-kbd"
    elif display_ready "$device"; then
        echo "mode=display driver=appletbdrm"
    else
        echo "mode=transitional"
    fi
    if [[ -f $marker ]]; then
        echo "activated=yes"
    else
        echo "activated=no"
    fi
}

action=${1:-}
case "$action" in
    native | display)
        if [[ $EUID -ne 0 ]]; then
            echo "touchbar-t2-mode $action must run as root" >&2
            exit 1
        fi
        "cmd_$action"
        ;;
    status) cmd_status ;;
    *)
        echo "usage: touchbar-t2-mode native|display|status" >&2
        exit 2
        ;;
esac
