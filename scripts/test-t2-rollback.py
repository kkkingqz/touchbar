#!/usr/bin/env python3
"""Exercise the installed scripts' functions without touching services or sysfs.

The subprocess runs real Bash control flow and flock against temporary files.
Only the systemd/USB boundaries and the device-discovery wait are substituted.
"""

import os
import pathlib
import re
import signal
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parent.parent


def functions(path):
    return "\n".join(re.findall(
        r"^\w+\(\) \{\n.*?^\}", (ROOT / path).read_text(), re.M | re.S
    ))


class RollbackTests(unittest.TestCase):
    def run_case(self, body):
        with tempfile.TemporaryDirectory(prefix="touchbar-t2-test-") as directory:
            script = "set -euo pipefail\n" + functions("scripts/touchbar-t2-mode.sh")
            script += "\n" + functions("scripts/service-admin-root.sh")
            script += r'''
state_dir=$1
marker=$state_dir/enabled
lock=$state_dir/mode.lock
t2_lock=$lock
admin_lock=$state_dir/admin.lock
t2=true
t2_units=(touchbar-t2-display.service touchbar-t2-sleep.service)
fallback=tiny-dfr
t2_mode=native_mode
display_pid=""
stop_failure=false
printf yes >"$marker"
printf native >"$state_dir/config"
printf tiny-dfr >"$state_dir/owner"
log() { :; }
sleep_in_progress() { return 1; }
find_device() { printf '%s' "$state_dir/usb"; }
wait_for_stable_device() {
    touch "$state_dir/waiting"
    until [[ -f $state_dir/continue ]]; do sleep 0.01; done
    find_device
}
modprobe() { :; }
set_config() { printf '%s' "$2" >"$state_dir/config"; }
wait_until() { :; }
udevadm() { :; }
native_mode() { (cmd_native); }
flock() {
    if [[ ${role:-} == rollback && ${*: -1} == 8 ]]; then
        touch "$state_dir/admin-lock-requested"
    fi
    if [[ ${role:-} == rollback ]]; then touch "$state_dir/acquiring-lock"; fi
    command flock "$@"
}
rm() {
    if [[ ${*: -1} == /etc/touchbar/enabled ]]; then
        command rm -f "$marker"
    else
        command rm "$@"
    fi
}
systemctl() {
    local args=" $* "
    if [[ $args == *' stop touchbar-t2-sleep.service '* ]]; then
        command rm -f "$state_dir/sleep-active"
        # Its ExecStop queues a display restart, even during rollback.
        touch "$state_dir/pending-display"
    fi
    if [[ $args == *' stop touchbar-t2-display.service '* ]]; then
        [[ $stop_failure == false ]] || return 1
        command rm -f "$state_dir/pending-display"
        if [[ -n $display_pid ]]; then
            touch "$state_dir/continue"
            wait "$display_pid"
            display_pid=""
        fi
    fi
    if [[ $args == *' stop touchbar.service '* ]]; then
        printf none >"$state_dir/owner"
    elif [[ $args == *' start touchbar.service '* ]]; then
        if [[ ! -f $marker ]]; then touch "$state_dir/late-start"; fi
        printf touchbar >"$state_dir/owner"
    elif [[ $args == *' start tiny-dfr.service '* ]]; then
        touch "$state_dir/handback-entered"
        if [[ -f $state_dir/usb-inflight ]]; then
            touch "$state_dir/premature-handback"
        fi
        printf tiny-dfr >"$state_dir/owner"
    elif [[ $args == *' show touchbar.service '* ]]; then
        printf inactive
    fi
}
rollback() {
    release_hardware
    wait_hardware_stopped
    hand_back
}
declare -F take_admin_lock >/dev/null || take_admin_lock() { :; }
await_waiting() {
    until [[ -f $state_dir/waiting ]]; do sleep 0.01; done
}
'''
            script += "\n" + body
            with subprocess.Popen(
                ["bash", "-s", "--", directory], text=True,
                stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                stderr=subprocess.PIPE, start_new_session=True,
            ) as process:
                try:
                    stdout, stderr = process.communicate(script, timeout=10)
                except subprocess.TimeoutExpired:
                    # A deadlocked background switch must not escape the test.
                    os.killpg(process.pid, signal.SIGKILL)
                    stdout, stderr = process.communicate()
                    self.fail("mode switch deadlocked: " + stdout + stderr)
                self.assertEqual(process.returncode, 0, stdout + stderr)

    def test_running_display_cannot_restart_touchbar_after_handback(self):
        # Missing draining/revalidation lets the old switch replace tiny-dfr.
        self.run_case(r'''
(cmd_display) &
display_pid=$!
await_waiting
rollback
touch "$state_dir/continue"
if [[ -n $display_pid ]]; then wait "$display_pid"; fi
[[ $(cat "$state_dir/owner") == tiny-dfr ]] || {
    echo 'display switch restarted touchbar after rollback'; exit 1;
}
''')

    def test_direct_switch_is_drained_before_tiny_dfr_handback(self):
        # Stopping the oneshot alone cannot drain a directly invoked mode script.
        self.run_case(r'''
(cmd_display) &
direct_pid=$!
await_waiting
(rollback) &
rollback_pid=$!
until [[ ! -f $marker ]]; do sleep 0.01; done
touch "$state_dir/continue"
wait "$direct_pid"
wait "$rollback_pid"
[[ $(cat "$state_dir/owner") == tiny-dfr ]] || {
    echo 'direct switch replaced the restored owner'; exit 1;
}
[[ $(cat "$state_dir/config") == native ]] || {
    echo 'deactivated direct switch changed the USB configuration'; exit 1;
}
''')

    def test_display_stop_failure_prevents_handback(self):
        # Ignoring a failed stop permits fallback while a switch is still alive.
        self.run_case(r'''
stop_failure=true
printf touchbar >"$state_dir/owner"
if release_hardware; then
    wait_hardware_stopped
    hand_back
    echo 'rollback succeeded despite display stop failure'; exit 1
fi
[[ $(cat "$state_dir/owner") != tiny-dfr ]]
''')

    def test_handback_waits_for_an_inflight_direct_usb_write(self):
        # Marker checks cannot cancel a USB operation that has already begun.
        self.run_case(r'''
touch "$state_dir/continue"
set_config() {
    touch "$state_dir/usb-inflight"
    until [[ -f $state_dir/usb-done ]]; do sleep 0.01; done
    printf '%s' "$2" >"$state_dir/config"
    command rm -f "$state_dir/usb-inflight"
}
(role=display; cmd_display) &
direct_pid=$!
until [[ -f $state_dir/usb-inflight ]]; do sleep 0.01; done
(role=rollback; rollback) &
rollback_pid=$!
until [[ -f $state_dir/acquiring-lock || -f $state_dir/handback-entered ]]; do
    sleep 0.01
done
touch "$state_dir/usb-done"
wait "$direct_pid"
wait "$rollback_pid"
[[ ! -f $state_dir/premature-handback ]] || {
    echo 'fallback started before the direct USB switch finished'; exit 1;
}
[[ $(cat "$state_dir/owner") == tiny-dfr ]]
''')

    def test_native_switch_fails_without_the_mode_lock(self):
        self.run_case(r'''
printf 2 >"$state_dir/config"
flock() { return 1; }
if cmd_native; then
    echo 'native switch succeeded without acquiring the mode lock'; exit 1
fi
[[ $(cat "$state_dir/config") == 2 ]]
''')

    def test_sleep_hook_restart_is_drained_before_handback(self):
        # Stopping sleep after display can enqueue a new display switch.
        self.run_case(r'''
touch "$state_dir/sleep-active"
rollback
[[ ! -f $state_dir/sleep-active ]] || {
    echo 'rollback left the sleep hook active'; exit 1;
}
[[ ! -f $state_dir/pending-display ]] || {
    echo 'sleep hook left a pending display switch after rollback'; exit 1;
}
''')

    def test_display_rechecks_activation_after_device_readiness(self):
        # Deactivation during the driver wait must suppress the late start.
        self.run_case(r'''
touch "$state_dir/continue"
wait_until() {
    touch "$state_dir/driver-waiting"
    until [[ -f $state_dir/driver-ready ]]; do sleep 0.01; done
}
(cmd_display) &
direct_pid=$!
until [[ -f $state_dir/driver-waiting ]]; do sleep 0.01; done
command rm -f "$marker"
touch "$state_dir/driver-ready"
wait "$direct_pid"
[[ ! -f $state_dir/late-start ]] || {
    echo 'display started touchbar after activation was removed'; exit 1;
}
''')

    def test_native_handback_does_not_deadlock_on_the_shared_lock(self):
        self.run_case(r'''
fallback=t2-native
printf 2 >"$state_dir/config"
rollback
[[ $(cat "$state_dir/config") == 1 ]]
''')

    def test_admin_rollback_waits_for_the_whole_activation_action(self):
        # Locking only the mode helper leaves activation's final start outside it.
        self.run_case(r'''
(
    take_admin_lock
    (cmd_display)
    # service-admin's activation also starts the daemon after the mode helper.
    systemctl start touchbar.service
) &
activation_pid=$!
await_waiting
(
    role=rollback
    take_admin_lock
    touch "$state_dir/admin-rollback-entered"
    rollback
) &
rollback_pid=$!
until [[ -f $state_dir/admin-lock-requested || -f $state_dir/admin-rollback-entered ]]; do
    sleep 0.01
done
if [[ -f $state_dir/admin-rollback-entered ]]; then
    touch "$state_dir/overlapped-admin-actions"
fi
touch "$state_dir/continue"
wait "$activation_pid"
wait "$rollback_pid"
[[ ! -f $state_dir/overlapped-admin-actions ]] || {
    echo 'rollback entered while activation could still start touchbar'; exit 1;
}
[[ $(cat "$state_dir/owner") == tiny-dfr ]]
''')


if __name__ == "__main__":
    unittest.main()
