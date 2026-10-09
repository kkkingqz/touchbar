#!/usr/bin/env bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
build_dir="$project_dir/target/drawing-component-build"
package_dir="$project_dir/target/drawing-component-demo"

cargo build --locked --release --target wasm32-wasip2 --target-dir "$build_dir" \
    --manifest-path "$project_dir/examples/drawing-component-plugin/Cargo.toml"
install -d -m 700 "$package_dir/component"
install -m 0644 "$build_dir/wasm32-wasip2/release/touchbar_drawing_component_demo.wasm" \
    "$package_dir/component/plugin.wasm"
install -m 0644 "$project_dir/examples/drawing-component-plugin/touchbar-plugin.toml" \
    "$package_dir/touchbar-plugin.toml"
echo "$package_dir"
