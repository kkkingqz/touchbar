#!/bin/bash
set -euo pipefail

if [[ $EUID -ne 0 || -z ${PKEXEC_UID:-} || ! ${PKEXEC_UID} =~ ^[0-9]+$ ]]; then
    echo "service-admin-root.sh must be invoked through pkexec by a logged-in user" >&2
    exit 1
fi

action=${1:-}
case "$action" in
    activate | rollback) ;;
    *)
        echo "usage: service-admin-root.sh activate|rollback" >&2
        exit 2
        ;;
esac

readonly admin_lock=/run/touchbar-service-admin.lock

take_admin_lock() {
    exec 8>"$admin_lock"
    flock --timeout 120 8
}

# Activation has work after the mode helper returns, including a daemon start.
# Serialize the whole action with rollback; the mode helpers only use fd 9's
# separate lock, so they can run while this administrative lock is held.
if ! take_admin_lock; then
    echo "another TouchBar activation or rollback did not finish within 120 s" >&2
    exit 1
fi

hardware_unit=/usr/lib/systemd/system/touchbar.service
hardware_binary=/usr/lib/touchbar/touchbard
t2_mode=/usr/lib/touchbar/touchbar-t2-mode
t2_lock=/run/touchbar-t2-mode.lock
t2_units=(touchbar-t2-display.service touchbar-t2-sleep.service)

tiny_dfr_installed() {
    [[ -f /usr/lib/systemd/system/tiny-dfr.service || -f /etc/systemd/system/tiny-dfr.service ]]
}

t2_touchbar_present() {
    local device
    for device in /sys/bus/usb/devices/*; do
        [[ -f $device/idVendor && -f $device/idProduct ]] || continue
        [[ $(<"$device/idVendor") == 05ac && $(<"$device/idProduct") == 8302 ]] && return 0
    done
    return 1
}

# An Intel T2 Touch Bar has a native configuration in which the T2 draws the
# function row itself, so it needs no userspace owner to fall back to. The
# device is briefly missing while it re-enumerates after a resume, so an
# installation already activated for T2 keeps counting as one.
t2=false
if [[ -x $t2_mode ]] \
    && { t2_touchbar_present || systemctl is-enabled --quiet touchbar-t2-sleep.service 2>/dev/null; }; then
    t2=true
fi

# The owner the strip is handed back to. tiny-dfr keeps that role wherever it
# is installed; without it only the T2's native configuration qualifies.
if tiny_dfr_installed; then
    fallback="tiny-dfr"
elif [[ $t2 == true ]]; then
    fallback="t2-native"
else
    echo "no rollback target: install tiny-dfr, or use an Intel T2 Touch Bar with touchbar-t2-mode" >&2
    exit 1
fi

wait_hardware_stopped() {
    local state
    for _ in $(seq 1 100); do
        state=$(systemctl show touchbar.service --property=ActiveState --value 2>/dev/null || true)
        case "$state" in
            "" | inactive | failed) return 0 ;;
        esac
        sleep 0.05
    done
    echo "refusing to hand the Touch Bar to $fallback while touchbar.service state is ${state:-unknown}" >&2
    return 1
}

# Called only once touchbar.service is proven stopped.
hand_back() {
    case "$fallback" in
        tiny-dfr)
            systemctl unmask tiny-dfr.service
            systemctl start tiny-dfr.service
            systemctl is-active --quiet tiny-dfr.service
            ;;
        t2-native)
            # The native helper acquires the same lock itself. Display callers
            # waiting for it will re-check the now absent activation marker.
            exec 9>&-
            "$t2_mode" native
            ;;
    esac
}

release_hardware() {
    rm -f -- /etc/touchbar/enabled
    systemctl disable touchbar.service 2>/dev/null || true
    # The T2 units are disabled unconditionally; that is harmless where they
    # were never enabled.
    systemctl disable "${t2_units[@]}" 2>/dev/null || true
    if [[ $t2 == true ]]; then
        # ExecStop on the sleep hook queues a display restart. Stop that hook
        # first, then drain the display unit, with the marker already absent.
        # A failed stop must never permit another owner to be started.
        systemctl stop touchbar-t2-sleep.service || return 1
        systemctl stop touchbar-t2-display.service || return 1
        # Direct invocations are outside the display unit's cgroup. Drain them
        # too, and keep the lock through hardware shutdown and tiny-dfr handback.
        exec 9>"$t2_lock"
        flock --timeout 60 9 || return 1
    fi
    systemctl stop touchbar.service 2>/dev/null || true
}

trigger_touchbar_display() {
    local count=0
    local driver_path
    local driver
    local name
    local sysfs_card
    for sysfs_card in /sys/class/drm/card*; do
        [[ -e "$sysfs_card" ]] || continue
        name=${sysfs_card##*/}
        [[ "$name" =~ ^card[0-9]+$ ]] || continue
        driver_path=$(readlink -f -- "$sysfs_card/device/driver" 2>/dev/null || true)
        driver=${driver_path##*/}
        case "$driver" in
            adp | appletbdrm)
                udevadm trigger --action=change "$sysfs_card"
                count=$((count + 1))
                ;;
        esac
    done
    if ((count != 1)); then
        echo "expected exactly one ADP Touch Bar DRM card, found $count" >&2
        return 1
    fi
    udevadm settle --timeout=10
    for _ in $(seq 1 100); do
        systemctl is-active --quiet dev-touchbar_display.device && return 0
        sleep 0.05
    done
    echo "Touch Bar systemd device alias did not become active" >&2
    return 1
}

if [[ "$action" == "rollback" ]]; then
    release_hardware
    wait_hardware_stopped
    hand_back
    echo "hardware-service=rolled-back replacement=$fallback"
    exit 0
fi

if [[ ! -f "$hardware_unit" || ! -x "$hardware_binary" ]]; then
    echo "TouchBar system files are not installed" >&2
    exit 1
fi

restore_fallback() {
    status=${1:-1}
    trap - EXIT INT TERM HUP
    if [[ $status -ne 0 ]]; then
        if release_hardware && wait_hardware_stopped; then
            hand_back 2>/dev/null || status=1
        else
            echo "WARNING: $fallback was not restored because hardware ownership could not be released" >&2
            status=1
        fi
    fi
    exit "$status"
}
trap 'restore_fallback $?' EXIT
trap 'restore_fallback 130' INT TERM HUP

if [[ $fallback == tiny-dfr ]]; then
    # tiny-dfr is also started directly by its udev rules. Disabling it is not
    # sufficient: masking is the deterministic ownership switch and is reversible.
    systemctl mask --now tiny-dfr.service
fi
install -d -o root -g root -m 0755 /etc/touchbar
install -o root -g root -m 0644 /dev/null /etc/touchbar/enabled
systemctl daemon-reload
if [[ $t2 == true ]]; then
    udevadm control --reload-rules
    # Native across every sleep, display again after each resume and at boot.
    systemctl enable "${t2_units[@]}"
    # touchbard needs the appletbdrm card, which only the display configuration has.
    "$t2_mode" display
fi
trigger_touchbar_display
if [[ $t2 == true ]]; then
    # udev starts touchbard whenever the display configuration's card appears.
    # Enabling it for multi-user.target would make every boot wait for a card
    # that the native configuration the T2 boots in does not have.
    systemctl disable touchbar.service 2>/dev/null || true
    systemctl start touchbar.service
else
    systemctl enable --now touchbar.service
fi
systemctl is-active --quiet touchbar.service
trap - EXIT INT TERM HUP
echo "hardware-service=activated replacement=touchbar.service fallback=$fallback"
