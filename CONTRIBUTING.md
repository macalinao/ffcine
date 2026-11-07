# Contributing

Issues, small focused pull requests, documentation, and synthetic media fixtures
are welcome. Include your Rust, ffmpeg, libplacebo, GPU, and driver versions when
reporting a rendering issue. Reproduce it with a short clip you can legally share;
remove private metadata, credentials, and identifying footage first.

## Development

Rust 1.85 is the declared minimum. Use the pinned Nix shell for the matching
ffmpeg 9, libplacebo, Vulkan headers, font libraries, Clang, and Rust toolchain.
See [the design guide](docs/design.md#crates) for the crate map.

```sh
nix develop --command cargo fmt --all --check
nix develop --command nice -n 19 cargo test --workspace --locked
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

A crate's first publication needs a crates.io token; trusted publishing cannot
create it. `ffcine-comp` still needs its first publication. The workflow supports
`CARGO_REGISTRY_TOKEN` for bootstrap: when any crate is missing, OIDC is skipped
and the registry secret is used. For existing crates it uses OIDC, falling back
to the registry secret when OIDC is unavailable.
Never commit registry tokens. Configure each crate's trusted publisher for owner
`talaagent`, repository `ffcine`, workflow `release-plz.yml` (no environment).
The release PR updater requires `RELEASE_PLZ_TOKEN`: a GitHub App token or PAT
with repository contents, pull-request, and workflow write permissions. The
built-in `GITHUB_TOKEN` failed to create a release branch after workflow edits;
the updater now stops early without closing the existing PR when this secret
is absent. A user/App token also allows release PR updates to trigger CI.
Publication uses the separate registry credentials described above.

After publication, check each package version on crates.io and the docs.rs build
status. The published CLI and library may lag behind `master`; source-only crates
must be published in dependency order as part of the release.

## License

Contributions use Apache-2.0. Preserve fixture and shader notices. A built binary's
redistribution terms also depend on the ffmpeg configuration; see the root
[license notes](README.md#license).
