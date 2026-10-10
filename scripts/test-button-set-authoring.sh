#!/bin/bash
set -euo pipefail
project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
# Fail visibly when a new package or campaign has not been connected.
for script in build-first-party-packs.sh test-packaging.sh; do
    for pack in app-actions omarchy; do
        rg -q "^for pack in .*\\b$pack\\b" "$project_dir/scripts/$script" || { echo "missing first-party build: $pack in $script" >&2; exit 1; }
    done
done
rg -Fq '"$project_dir/scripts/test-button-set-authoring.sh"' "$project_dir/scripts/test-packaging.sh" || { echo "missing authoring acceptance in packaging" >&2; exit 1; }
cargo build --quiet --release --manifest-path "$project_dir/Cargo.toml" \
    -p touchbar-button-set-logic-demo --target wasm32-wasip2
stage=$(mktemp -d "$project_dir/run/set-authoring.XXXXXX")
trap 'rm -r -- "$stage"' EXIT INT TERM
mkdir -p "$stage/package/component" "$stage/package/sets"
cp "$project_dir/examples/button-set-logic/touchbar-plugin.toml" "$stage/package/"
cp "$project_dir/examples/button-set-logic/sets/"*.toml "$stage/package/sets/"
cp "$project_dir/target/wasm32-wasip2/release/touchbar_button_set_logic_demo.wasm" "$stage/package/component/plugin.wasm"
cli="$project_dir/target/release/touchbarctl"
wasm_before=$(sha256sum "$stage/package/component/plugin.wasm")
"$cli" plugin check --package "$stage/package" >/dev/null
sed -i 's/Increment/Advance/' "$stage/package/sets/counter.toml"
"$cli" plugin check --package "$stage/package" >/dev/null
"$cli" plugin pack --package "$stage/package" --output "$stage/counter.touchbar" >/dev/null
[[ "$wasm_before" == "$(sha256sum "$stage/package/component/plugin.wasm")" ]]
[[ -s "$stage/counter.touchbar" ]]
echo 'layout_only_edit_needs_no_build=ok shared_wasm=true explicit_files=true'
# Sealing and legacy launches are covered by the production supervisor campaign.
