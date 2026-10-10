#!/bin/bash
set -euo pipefail
project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
stage=$(mktemp -d "$project_dir/run/supervised-sets.XXXXXX")
cleanup() {
    result=$?
    if (( result != 0 )); then
        for log in "$stage"/*.log; do [[ ! -f "$log" ]] || cat "$log" >&2; done
    fi
    rm -r -- "$stage"
}
trap cleanup EXIT INT TERM
profile=${TOUCHBAR_TEST_BUILD_PROFILE:-release}
build_flags=()
[[ "$profile" != release ]] || build_flags+=(--release)
cargo build --quiet --manifest-path "$project_dir/Cargo.toml" "${build_flags[@]}" \
    -p touchbar-plugin-host -p touchbar-plugin-supervisor
cargo build --quiet --release --manifest-path "$project_dir/Cargo.toml" \
    -p touchbar-button-set-logic-demo -p touchbar-broker-component-demo --target wasm32-wasip2
bin="$project_dir/target/$profile"
package="$stage/package"
mkdir -p "$package/component" "$package/sets" "$stage/state"
chmod 700 "$stage" "$stage/state"
cp "$project_dir/target/wasm32-wasip2/release/touchbar_button_set_logic_demo.wasm" "$package/component/plugin.wasm"
cat > "$package/touchbar-plugin.toml" <<'EOF'
manifest_version=1
[plugin]
name="Counter"
version="0.1.0"
description="Sealed set test"
license="MIT"
source="github:example/counter"
api="^1.0"
[runtime]
kind="button-set-logic"
entrypoint="component/plugin.wasm"
world="touchbar:plugin/button-set-logic@1.0.0"
[[sets]]
id="counter"
file="sets/counter.toml"
EOF
cat > "$package/sets/counter.toml" <<'EOF'
version=1
id="counter"
label="Counter"
[layout]
kind="button"
widget_id=10
label="Increment"
on_activate="increment"
EOF
wasm_digest="sha256:$(sha256sum "$package/component/plugin.wasm" | awk '{print $1}')"
set_digest="sha256:$(sha256sum "$package/sets/counter.toml" | awk '{print $1}')"
# Mutate the source after supervisor sealing and before the trusted host opens.
cat > "$stage/after-sealing" <<EOF
#!/bin/bash
/usr/bin/sed -i 's/Increment/Changed/' "\$1/sets/counter.toml"
exec "$bin/touchbar-plugin-host" "\$@"
EOF
chmod 700 "$stage/after-sealing"
supervisor_args=("$package" --host "$stage/after-sealing" --source github:example/counter \
    --version 0.1.0 --digest "$wasm_digest" --provenance local-development --state "$stage/state")
timeout 30 "$bin/touchbar-plugin-supervisor" "${supervisor_args[@]}" \
    --set-digest "sets/counter.toml=$set_digest" -- counter 160 10 > "$stage/sealed.log" 2>&1
rg -q 'Button "Increment"' "$stage/sealed.log"
rg -q 'activation 10: rerender=true' "$stage/sealed.log"
! rg -q 'Button "Changed"' "$stage/sealed.log"
rg -q 'label="Changed"' "$package/sets/counter.toml"
if "$bin/touchbar-plugin-supervisor" "${supervisor_args[@]}" \
    --set-digest "sets/counter.toml=$set_digest" -- counter 160 > "$stage/tampered.log" 2>&1; then
    echo "tampered set was accepted" >&2; exit 1
fi
rg -q 'set.*digest.*does not match' "$stage/tampered.log"
if "$bin/touchbar-plugin-supervisor" "${supervisor_args[@]}" -- counter 160 > "$stage/missing.log" 2>&1; then
    echo "missing set digests were accepted" >&2; exit 1
fi
rg -q 'set digests do not exactly match' "$stage/missing.log"

cat > "$stage/no-set-bundle" <<EOF
#!/bin/bash
unset TOUCHBAR_SET_BUNDLE_FD
exec "$bin/touchbar-plugin-host" "\$@"
EOF
chmod 700 "$stage/no-set-bundle"
changed_digest="sha256:$(sha256sum "$package/sets/counter.toml" | awk '{print $1}')"
if "$bin/touchbar-plugin-supervisor" "${supervisor_args[@]}" --host "$stage/no-set-bundle" \
    --set-digest "sets/counter.toml=$changed_digest" -- counter 160 > "$stage/host-missing.log" 2>&1; then
    echo "host accepted a missing sealed set bundle" >&2; exit 1
fi
rg -q 'require a sealed set bundle' "$stage/host-missing.log"

# A FIFO is rejected before any blocking read or guest execution.
rm "$package/sets/counter.toml"
mkfifo "$package/sets/counter.toml"
fifo_status=0
timeout 3 "$bin/touchbar-plugin-supervisor" "${supervisor_args[@]}" \
    --set-digest "sets/counter.toml=$set_digest" -- counter 160 > "$stage/fifo.log" 2>&1 || fifo_status=$?
if [[ "$fifo_status" == 0 || "$fifo_status" == 124 ]]; then
    echo "nonregular set was accepted or blocked its loader" >&2; exit 1
fi
rg -q 'bounded single-link regular file' "$stage/fifo.log"

# The old component ABI continues to launch without any set descriptor.
cp "$project_dir/examples/broker-component-plugin/touchbar-plugin.toml" "$package/touchbar-plugin.toml"
cp "$project_dir/target/wasm32-wasip2/release/touchbar_broker_component_demo.wasm" "$package/component/plugin.wasm"
legacy_digest="sha256:$(sha256sum "$package/component/plugin.wasm" | awk '{print $1}')"
legacy_args=("$package" --host "$bin/touchbar-plugin-host" \
    --source github:cameroncooper/touchbar-broker-component-demo --version 0.1.0 \
    --digest "$legacy_digest" --provenance local-development --state "$stage/state")
timeout 30 "$bin/touchbar-plugin-supervisor" "${legacy_args[@]}" -- media-broker 160 > "$stage/legacy.log" 2>&1
rg -q 'item: media-broker' "$stage/legacy.log"
if "$bin/touchbar-plugin-supervisor" "${legacy_args[@]}" --set-digest "sets/counter.toml=$set_digest" \
    -- media-broker 160 > "$stage/unexpected.log" 2>&1; then
    echo "legacy package accepted unsolicited set bytes" >&2; exit 1
fi
rg -q 'set digests do not exactly match' "$stage/unexpected.log"
echo "supervised-button-sets=ok sealed=true tampering=rejected legacy=compatible"
