# Contributing

Issues, small focused pull requests, documentation, and synthetic media fixtures
are welcome. Include your Rust, ffmpeg, libplacebo, GPU, and driver versions when
reporting a rendering issue. Reproduce it with a short clip you can legally share;
remove private metadata, credentials, and identifying footage first.

## Development

Rust 1.89 is the declared minimum. Use the pinned Nix shell for the matching
ffmpeg 9, libplacebo, Vulkan headers, font libraries, Clang, and Rust toolchain.
See [the design guide](docs/design.md#crates) for the crate map.

```sh
nix develop --command cargo fmt --all --check
nix develop --command nice -n 19 cargo nextest run --workspace --locked
nix develop --command cargo test --workspace --doc --locked
nix develop --command nice -n 19 cargo clippy --workspace --all-targets --locked -- -D warnings
RUSTDOCFLAGS="-D warnings" nix develop --command nice -n 19 cargo doc --workspace --no-deps --locked
```

Builds can be heavy. CI runs `nix flake check`; it need not be run locally.
Hardware tests skip themselves when a GPU is unavailable. Do not treat a passing
CPU test run as proof that a native GPU path works. Doctests cover portable API
examples; `no_run` examples compile but do not open media or render.

## Documentation and packages

Every crate links its API reference from its Cargo manifest. docs.rs disables
default features so that portable types and algorithms build without the pinned
native stack. Full backend docs are generated in the Nix shell above.

`ffcine-libplacebo-sys` uses `docs/bindings.rs` **only** when `DOCS_RS` is set.
This API 375 snapshot was generated from the revision in `nix/libplacebo.nix`.
When changing that pin, build normally in the Nix shell and replace the snapshot
with the generated `out/bindings.rs` under Cargo's build directory. Retain its
provenance comment. Native builds must continue to bind to their actual headers.

Verify packaging in the Nix shell with `cargo publish --workspace --dry-run --locked`.
The workspace README is inherited into each package. Keep its public links
absolute so they work on crates.io as well as GitHub.

## Releases

All workspace crates share a version. `release-plz.toml` keeps their versions in
sync and combines changes in the root `CHANGELOG.md`, with `ffcine-cli` owning the
`v<version>` tag and GitHub release. `release_always = false` publishes only after
a release PR is merged. Review version bumps and changelog entries and require
passing CI before merging. An existing release PR may need a maintainer to approve
its Actions run before checks start.

Release automation uses **release-plz** for both release PRs and publishing.
A crate's first publication needs a crates.io token; trusted publishing cannot
create it. `CARGO_REGISTRY_TOKEN` is configured as a repository Actions secret,
so release-plz can publish `ffcine-comp` and any future new crates alongside
the existing workspace packages.

Release PRs use the built-in `GITHUB_TOKEN` by default, with job-level
`contents: write` and `pull-requests: write` permissions. Enable **Allow GitHub
Actions to create and approve pull requests** in the organization and repository
Actions settings, then set repository variable `RELEASE_PR=true`. The setup
check prevents a failing updater from disturbing an existing release PR.

GitHub requires a maintainer to approve CI runs on PRs created or updated with
`GITHUB_TOKEN`; token-generated pushes do not start ordinary push workflows.
An optional `RELEASE_PLZ_TOKEN` (GitHub App token or PAT) can override the built-in
token to run PR CI without that approval and support workflow-file updates.
No personal GitHub token is required for the default setup.

For tokenless registry publication, configure every crate's trusted publisher
for owner `talaagent`, repository `ffcine`, workflow `release-plz.yml` (no
environment), then remove the registry secret. Release-plz obtains OIDC
credentials itself. New crates still need a token for first publication.

After publication, check each package version on crates.io and the docs.rs build
status. The published CLI and library may lag behind `master`; source-only crates
must be published in dependency order as part of the release.

## License

Contributions use Apache-2.0. Preserve fixture and shader notices. A built binary's
redistribution terms also depend on the ffmpeg configuration; see the root
[license notes](README.md#license).

Use `jiff` for date/time handling; do not add a direct `chrono` dependency.
The Nix dev shell supplies Rust, Cargo, Clippy, rustfmt, and cargo-nextest.

The optional `telemetry` feature uses upstream camera parsers that still depend
on Chrono and unmaintained YAML helpers. Those exceptions are limited in
`deny.toml`; the normal GPU/audio features do not enable telemetry. Application
code must use Jiff, and newly introduced Chrono edges fail cargo-deny.
