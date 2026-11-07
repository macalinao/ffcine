{
  description = "ffcine: GPU video finishing on ffmpeg + libplacebo (Vulkan)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-parts = {
      url = "github:hercules-ci/flake-parts";
      inputs.nixpkgs-lib.follows = "nixpkgs";
    };
    treefmt-nix = {
      url = "github:numtide/treefmt-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    git-hooks = {
      url = "github:cachix/git-hooks.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    inputs@{ flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      # Vulkan + NVENC/VAAPI: the full build is Linux only. macOS has the audio dev shell.
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];

      imports = [
        inputs.treefmt-nix.flakeModule
        inputs.git-hooks.flakeModule
      ];

      perSystem =
        { config, pkgs, ... }:
        let
          libplacebo = pkgs.callPackage ./nix/libplacebo.nix { };

          # The GPU ffmpeg (the same overrides as the Python prototype's
          # dev shell): ffmpeg-full (NVENC/NVDEC, CUDA,

          # Vulkan) on the libplacebo above, with glslang at build time, else
          # configure drops spirv_compiler and every *_vulkan filter
          # (nlmeans_vulkan, blend_vulkan, ...).
          ffmpeg-gpu = (pkgs.ffmpeg-full.override { inherit libplacebo; }).overrideAttrs (o: {
            pname = "ffmpeg-full-gpu";
            nativeBuildInputs = o.nativeBuildInputs ++ [ pkgs.glslang ];
            doCheck = false;
          });

          # Native libraries the crates link: libav* (ffmpeg-sys-next) and
          # libplacebo + Vulkan (libplacebo-sys). Shared by the package and
          # the dev shell so `cargo build` sees the same versions as `nix build`.
          nativeLibs = [
            ffmpeg-gpu
            libplacebo
            pkgs.vulkan-loader
            pkgs.vulkan-headers
            pkgs.shaderc
          ];

          # `sync`, `audio` and `ffcine-audio`'s tests: libav* only, so this one builds on macOS.
          audioShell = pkgs.mkShell {
            name = "ffcine-audio";
            nativeBuildInputs = [
              pkgs.pkg-config
              pkgs.rustPlatform.bindgenHook
            ];
            buildInputs = [ pkgs.ffmpeg ] ++ pkgs.lib.optional pkgs.stdenv.hostPlatform.isDarwin pkgs.libiconv;
            packages = [
              pkgs.cargo
              pkgs.cargo-nextest
              pkgs.cargo-deny
              pkgs.rustc
              pkgs.clippy
              pkgs.rustfmt
            ];
          };

          ffcine = pkgs.rustPlatform.buildRustPackage {
            pname = "ffcine";
            version = (pkgs.lib.importTOML ./Cargo.toml).workspace.package.version;
            src = pkgs.lib.fileset.toSource {
              root = ./.;
              fileset = pkgs.lib.fileset.unions [
                ./Cargo.toml
                ./Cargo.lock
                ./README.md
                ./LICENSE
                ./crates
              ];
            };
            cargoLock.lockFile = ./Cargo.lock;
            useNextest = true;
            postCheck = ''
              cargo test --workspace --all-features --doc --offline --release
            '';
            cargoBuildFlags = [
              "--workspace"
              "--all-features"
            ];
            cargoTestFlags = [
              "--workspace"
              "--all-features"
            ];
            # bindgenHook points bindgen (ffmpeg-sys-next, libplacebo-sys) at libclang.
            nativeBuildInputs = [
              pkgs.pkg-config
              pkgs.rustPlatform.bindgenHook
              pkgs.autoAddDriverRunpath
            ];
            buildInputs = nativeLibs;
            # ffcine-text's layout tests need a font: the sandbox has none,
            # and cosmic-text panics shaping without one.
            FONTCONFIG_FILE = pkgs.makeFontsConf { fontDirectories = [ pkgs.dejavu_fonts ]; };
            # autoAddDriverRunpath: /run/opengl-driver/lib on the RUNPATH, so the
            # dlopen'd libcuda / libnvidia-opticalflow resolve in sandboxes too.
            # Tests that need a GPU skip themselves; the rest run in the sandbox.
            meta.mainProgram = "ffcine";
          };
        in
        {
          packages = pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
            inherit ffcine libplacebo ffmpeg-gpu;
            default = ffcine;
          };

          checks = pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux { inherit ffcine; };

          # treefmt-nix: `nix fmt` formats Nix, Rust and TOML.
          treefmt = {
            projectRootFile = "flake.nix";
            programs.nixfmt.enable = true;
            programs.rustfmt.enable = true;
            programs.taplo.enable = true;
          };

          # git-hooks.nix: format on commit, clippy on push.
          pre-commit.settings.hooks = {
            treefmt = {
              enable = true;
              package = config.treefmt.build.wrapper;
            };
            # Clippy typechecks the whole workspace (and runs bindgen): too
            # heavy per commit, so pre-push. Same cargo/clippy as the dev shell.
            clippy = {
              enable = true;
              stages = [ "pre-push" ];
              packageOverrides = {
                cargo = pkgs.cargo;
                clippy = pkgs.clippy;
              };
              settings = {
                extraArgs = "--workspace --all-targets";
                denyWarnings = true;
              };
            };
          };

          devShells.audio = audioShell;

          devShells.default = pkgs.mkShell {
            name = "ffcine";
            inputsFrom = [ ffcine ];
            packages = [
              pkgs.cargo
              pkgs.cargo-nextest
              pkgs.cargo-deny
              pkgs.rustc
              pkgs.clippy
              pkgs.rustfmt
              pkgs.rust-analyzer
              # CLI ffmpeg/ffprobe for comparing against the libav* path.
              ffmpeg-gpu
              pkgs.vulkan-tools
              config.treefmt.build.wrapper
            ];
            shellHook = config.pre-commit.installationScript;
            RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
            # The Vulkan loader is dlopen'd at runtime; NixOS puts the ICDs
            # (driver JSONs) under /run/opengl-driver.
            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [ pkgs.vulkan-loader ];
          };
        };
    };
}
