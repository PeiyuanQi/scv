# Release and Compatibility

Status: final design for v0.1

Peon v0.1 supports the latest patch release of stable Rust 1.88 or newer on:

- macOS 13 or newer on Apple Silicon and x86-64;
- glibc-based Linux on x86-64 and ARM64.

The release workflow builds and tests four target archives:

- `peon-aarch64-apple-darwin.tar.gz`;
- `peon-x86_64-apple-darwin.tar.gz`;
- `peon-aarch64-unknown-linux-gnu.tar.gz`;
- `peon-x86_64-unknown-linux-gnu.tar.gz`.

Each archive contains `peon`, `peon-server`, `README.md`, `LICENSE`, and
`NOTICE`. Checksums are published beside the archives. Release builds use Cargo
locked mode. The project does not ship a curl-to-shell installer in v0.1.

Peon is licensed under the Apache License 2.0. The root `LICENSE` contains the
unmodified Apache 2.0 license text, `NOTICE` identifies Peon and any required
third-party notices, and the workspace and every published Cargo package set
`license = "Apache-2.0"`. Dependency license checks reject packages whose terms
are incompatible with Apache-2.0 distribution.

The root `README.md` is the installation and quick-start contract. It includes
prerequisites, provider configuration, source build, binary usage, safety
limits, extension entry points, development checks, architecture links,
contribution guidance, and license information.

## Compatibility policy

- The stdio protocol is versioned independently from the crate version.
- Additive object fields do not change the protocol version.
- Removing a field, changing its meaning, or changing message ordering requires
  a protocol version increase.
- Configuration rejects unknown keys in v0.x so misspellings do not silently
  weaken behavior.
- Rust traits are extension seams but do not promise a stable third-party ABI
  before 1.0.
- Linux and macOS are release-gated; other platforms are best effort until they
  join the CI matrix.
