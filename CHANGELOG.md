# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0](https://github.com/macalinao/ffcine/compare/v0.2.0...v0.3.0) - 2026-11-02

### Added

- *(text)* slide/pop/typewriter/wipe animations, keyframes, title sequences and end screens ([#5](https://github.com/macalinao/ffcine/pull/5))

### Other

- *(cli)* comprehensive --help for every command ([#18](https://github.com/macalinao/ffcine/pull/18))
- Overlay rotation and anchor, blend modes, HDR layers ([#12](https://github.com/macalinao/ffcine/pull/12))

## [0.2.0](https://github.com/macalinao/ffcine/compare/v0.1.0...v0.2.0) - 2026-11-02

### Added

- denoise presets (night, snow), asynchronous NVOF, gyro stabilization ([#16](https://github.com/macalinao/ffcine/pull/16))
- *(breakdown)* shot list of a reference video ([#15](https://github.com/macalinao/ffcine/pull/15))
- *(color)* Rust LUT baker, HDR PQ/HLG deliverables, HDR camera transforms ([#10](https://github.com/macalinao/ffcine/pull/10))
- *(audio)* streaming mix, per-bus gain and sends, macOS ([#13](https://github.com/macalinao/ffcine/pull/13))
- *(delivery)* MP4 chapters and cover art, review renders from proxies ([#7](https://github.com/macalinao/ffcine/pull/7))
- vendor ArtCNN, load copyleft hooks by name, assemble HEIC tile grids ([#8](https://github.com/macalinao/ffcine/pull/8))
- *(reframe)* blurred contain fill, NLE crop/transform, reframe+stabilize panels and overlays ([#4](https://github.com/macalinao/ffcine/pull/4))

### Fixed

- *(assembly)* path-only ffcine-import dev-dependency ([#1](https://github.com/macalinao/ffcine/pull/1))

### Other

- rgb_gain, extra filters and temporal blend ([#9](https://github.com/macalinao/ffcine/pull/9))
- *(cli)* complete --help for every command ([#11](https://github.com/macalinao/ffcine/pull/11))
- fail on rustdoc warnings; fix broken intra-doc links ([#3](https://github.com/macalinao/ffcine/pull/3))
- Render pieces: ramp seek, denoiser context across cuts ([#6](https://github.com/macalinao/ffcine/pull/6))
