# Shared counter logic

`sets/counter.toml` and `sets/compact-counter.toml` explicitly share
`component/plugin.wasm` through `touchbar-plugin.toml`. Both bind the same
`increment` action, while each runtime instance owns its counter state.

From the repository root:

```sh
touchbarctl plugin build --package examples/button-set-logic
touchbarctl plugin check --package examples/button-set-logic
touchbarctl plugin test --package examples/button-set-logic
touchbarctl plugin dev --package examples/button-set-logic --item compact-counter
```

Once built, edit either layout or register another description using the same
contract, then run `check` and `pack` without rebuilding Wasm. Change Rust only
when adding behavior. See [the three layers](../../docs/design/button-sets.md).
