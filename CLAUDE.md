# ffcine

GPU video finishing in Rust: ffmpeg (libav*, `ffmpeg-next`) for I/O and NVDEC/NVENC, libplacebo
(Vulkan, `crates/libplacebo-sys`) for grading, frames kept in VRAM. Design: `docs/design.md`.
It replaces a Python prototype (`gpu_grade.py` and the GPU path in `render.py`); `grade` flags and
defaults should keep matching it.

## Layout

- `crates/ffcine-*`: the library, one crate per concern (map and dependency graph:
  `docs/design.md#crates`). `crates/ffcine` is a facade re-exporting them. Public types stay
  backend-neutral; anything touching ffmpeg or libplacebo lives in `ffcine-render/src/backend/`
  (or behind a feature, like `ffcine-stabilize`'s `nvof`).
- `crates/ffcine-cli`: `ffcine` binary, bpaf derive (not clap).
- `crates/libplacebo-sys`: bindgen over `wrapper.h`; needs libplacebo >= 7.365.

## Commands

Builds are heavy: `nice -n 19` is recommended. System libs come from the dev shell.


```sh
nix develop -c nice -n 19 cargo build
nix develop -c nice -n 19 cargo test
nix develop -c nice -n 19 cargo clippy --workspace --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" nix develop -c nice -n 19 cargo doc --no-deps --workspace
nix fmt
```

Don't run `nix flake check` locally (CI does). Clippy also runs as a pre-push hook.
