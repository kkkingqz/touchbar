#!/usr/bin/env bash
# Run a command with the supervisor's required cgroup and session bus.
# A graphical login supplies a user manager and bus, but its app cgroup need
# not be delegated. Use an unprivileged user scope when delegation is missing.
# Run this on the host: a container cannot supply the host's user manager or
# confinement merely by inheriting DBUS_SESSION_BUS_ADDRESS.
set -euo pipefail

prepare_cgroup=false
if [[ ${1:-} == --prepare-cgroup ]]; then
    prepare_cgroup=true
    shift
fi

if [[ $# -eq 0 ]]; then
    echo "usage: ci-session.sh COMMAND [ARGUMENTS...]" >&2
    exit 2
fi

have_session_bus() {
    [[ -n ${DBUS_SESSION_BUS_ADDRESS:-} ]]
}

cgroup_directory() {
    local relative
    relative=$(awk -F: '$1 == "0" { print $3 }' /proc/self/cgroup 2>/dev/null) || return 1
    [[ -n "$relative" ]] || return 1
    readlink -f -- "/sys/fs/cgroup$relative"
}

private_directory() {
    local owner mode
    [[ -d "$1" ]] || return 1
    owner=$(stat -c %u "$1" 2>/dev/null) || return 1
    mode=$(stat -c %a "$1" 2>/dev/null) || return 1
    [[ "$owner" == "$(id -u)" ]] || return 1
    (( 8#$mode & 8#022 )) && return 1
    return 0
}

# Match the supervisor's layout: the runner is in a leaf, while component and
# command groups are its siblings under an empty, delegated domain.
own_cgroup() {
    local directory parent controllers
    directory=$(cgroup_directory) || return 1
    [[ "${directory##*/}" == touchbar-supervisor ]] || return 1
    private_directory "$directory" || return 1
    parent=${directory%/*}
    private_directory "$parent" || return 1
    [[ $(cat "$parent/cgroup.type") == domain ]] || return 1
    [[ -z $(cat "$parent/cgroup.procs") ]] || return 1
    [[ -w "$parent/cgroup.subtree_control" ]] || return 1
    controllers=$(cat "$parent/cgroup.controllers" 2>/dev/null) || return 1
    [[ " $controllers " == *" pids "* ]] || return 1
    return 0
}

if "$prepare_cgroup"; then
    directory=$(cgroup_directory)
    private_directory "$directory"
    mkdir -m 700 -- "$directory/touchbar-supervisor"
    # This shell is the scope's only process. Move it before enabling any
    # domain controllers; subprocesses then inherit the supervisor leaf.
    printf '%s' "$$" > "$directory/touchbar-supervisor/cgroup.procs"
    if ! own_cgroup; then
        echo "cannot prepare an empty delegated supervisor cgroup" >&2
        exit 1
    fi
    exec "$@"
fi

command=("$@")
if ! have_session_bus; then
    command=(dbus-run-session -- "${command[@]}")
fi

if own_cgroup; then
    exec "${command[@]}"
fi

if ! systemctl --user show-environment >/dev/null 2>&1; then
    echo "supervisor tests need the host user systemd manager; run this wrapper from a user login session, outside the build container" >&2
    exit 1
fi

# A user scope preserves the caller's environment and exit status. Delegation
# makes its cgroup user-owned; no sudo or privileged system manager is needed.
exec systemd-run --user --quiet --scope --same-dir --collect \
    --property='Delegate=cpu memory pids' -- "${BASH_SOURCE[0]}" --prepare-cgroup "${command[@]}"
