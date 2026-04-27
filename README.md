# ncz-tools

Operator tooling for the nclawzero edge fleet. Cargo workspace housing the
Rust binaries that get installed on every nclawzero device and on the
operator workstations that drive them.

## Members

| Crate | Role |
|---|---|
| [`ncz/`](./ncz) | On-device umbrella CLI (status, set-agent, logs, providers, sandbox, integrity, update, channel, health, inspect, ...). Replaces the bash dispatcher previously shipped from `pi-gen-nclawzero` stage `06-install-ncz-cli`. |
| [`zterm/`](./zterm) | Light, thin, fast terminal REPL for the claw-family agentic daemons. Back to the 1970s. |

## Build

```bash
cargo build --release            # build all members
cargo build --release -p ncz     # ncz only
cargo build --release -p zterm   # zterm only
cargo test --workspace
```

## Distribution

`ncz` ships as a `.deb` in the `nclawzero-internal` apt repo on ARGOS and
is installed by `pi-gen-nclawzero` stage `06-install-ncz-cli`.

`zterm` is a developer/operator tool, installed via `cargo install`.

## License

Apache-2.0. See [LICENSE](./LICENSE) and [NOTICE](./NOTICE).
