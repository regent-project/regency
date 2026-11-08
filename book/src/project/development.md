# Development

regency is a Cargo workspace (Rust, edition 2024). Every build needs
`protoc` **36.1** exactly.

```bash
cargo build                                     # every service
cargo test                                      # unit tests
cargo test -- --include-ignored                 # + integration tests (Docker: RabbitMQ and CouchDB containers)
cargo clippy --workspace --exclude frontend --all-targets
```

The web client (`frontend/`) is built with [trunk](https://trunkrs.dev)
and needs the `wasm32-unknown-unknown` target:

```bash
cd frontend && trunk build --release     # → frontend/dist
```

## Local stack

{{#include ../../../dev/README.md:3:}}

## Repository layout

| Path | Content |
|---|---|
| `regencylib/` | shared library: messages, model, topology, configuration, security, logging |
| `<service>/` | one crate per service |
| `frontend/` | the web client |
| `deploy/` | Dockerfile, all-in-one configuration, compose, Helm chart |
| `packaging/` | systemd units, maintainer scripts, default configurations |
| `book/` | this book (`mdbook build book`) |
