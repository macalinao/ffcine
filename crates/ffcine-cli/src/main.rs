//! `ffcine`: GPU video finishing on ffmpeg + libplacebo, plus card ingest and the clip catalog
//! (which build without the GPU feature, for the Mac).

#[cfg(feature = "audio")]
mod audio;
#[cfg(feature = "gpu")]
mod gpu;
mod publish;
mod triage;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use anyhow::Context as _;
use bpaf::Bpaf;
use ffcine::ingest::{
    self, CatalogOptions, ClockConfig, Event, Histogram, IngestOptions, Zone, catalog,
};

#[derive(Debug, Clone, Bpaf)]
#[bpaf(options, version)]
#[allow(clippy::large_enum_variant, clippy::doc_markdown)] // parsed once; help text
/// GPU video finishing: decode, grade and encode with frames kept in VRAM.
enum Cmd {
    #[cfg(feature = "gpu")]
    Gpu(#[bpaf(external(gpu::cmd))] gpu::Cmd),
    /// Upload renders to an Immich album (created if missing), e.g. review copies for a phone.
    /// Server and API key file: ~/.config/ffcine/config.json `{"immich": {"url", "api_key_file"}}`
    /// or FFCINE_IMMICH_URL / FFCINE_IMMICH_API_KEY_FILE.
    #[bpaf(command)]
    Publish {
        /// Album name.
        #[bpaf(argument("ALBUM"))]
        immich: String,
        /// Config file (default: $XDG_CONFIG_HOME/ffcine/config.json).
        #[bpaf(argument("FILE"))]
        config: Option<PathBuf>,
        #[bpaf(positional("FILE"), some("a file to upload"))]
        files: Vec<PathBuf>,
    },
    /// Offload a camera card with verified copies, a manifest and an optional backup.
    ///
    /// Copies into DEST/01_footage/<camera>/<day>/ (audio into 02_audio/), checks each copy
    /// against the card with BLAKE3, logs it in DEST/.ffcine/ingest.jsonl, then catalogs DEST.
    /// Never writes to the card; re-running skips what is already copied.
    #[bpaf(command)]
    Ingest(#[bpaf(external(ingest_args))] IngestArgs),
    /// Index clips under DIR into DIR/catalog.json, with picture profile and fixed clocks.
    ///
    /// Per clip: codec, resolution, exact frame rate, bit depth, chroma, colour tags, picture
    /// profile (DJI D-Log / D-Log M / D-Log2, HLG, PQ, C-Log3, Apple Log, Rec.709) with its
    /// confidence and camera LUT, and the clock after the per-camera fixes. Flags mixed frame
    /// rates and cameras whose clocks disagree.
    #[bpaf(command)]
    Catalog(#[bpaf(external(catalog_args))] CatalogArgs),
    /// Check the GPU path; exits non-zero if anything the render needs is missing.
    ///
    /// Opens CUDA and Vulkan devices, an NVENC session, NVDEC configs and NVOF, and checks the
    /// linked libav and the `ffmpeg` on PATH for libplacebo and nlmeans_vulkan.
    #[bpaf(command)]
    Doctor,
    /// Find words in the triage transcripts: clip and timecode per hit.
    #[bpaf(command)]
    Search(#[bpaf(external(triage::search_args))] triage::SearchArgs),
    /// Move a proposed in/out onto word boundaries and silence.
    #[bpaf(command)]
    Snap(#[bpaf(external(triage::snap_args))] triage::SnapArgs),
    /// Line up recorder WAVs with the camera clips by their scratch audio.
    ///
    /// Writes the recorder audio as clips on its own track, with offset, drift and confidence
    /// per clip. Needs no GPU.
    #[cfg(feature = "audio")]
    #[bpaf(command)]
    Sync(#[bpaf(external(audio::sync_args))] audio::SyncArgs),
    /// Render the timeline's audio mix to a WAV, without touching the video.
    ///
    /// Clean chain, R128 levelling, dialogue/music/ambience/SFX buses with ducking, master at
    /// -14 LUFS under a true-peak limiter.
    #[cfg(feature = "audio")]
    #[bpaf(command)]
    Audio(#[bpaf(external(audio::audio_args))] audio::AudioArgs),
    /// Tempo and beat times of a music file (librosa's beat tracker), cached.
    #[cfg(feature = "audio")]
    #[bpaf(command)]
    Beats(#[bpaf(external(audio::beats_args))] audio::BeatsArgs),
}

#[derive(Debug, Clone, Bpaf)]
#[allow(clippy::doc_markdown)]
struct ClockArgs {
    /// Clock config (YAML: zone, cameras.<id>.shift/zone, sync); default DEST/clocks.yaml.
    #[bpaf(argument("FILE"))]
    clocks: Option<PathBuf>,
    /// Per-camera fix, CAMERA=SHIFT[@ZONE], e.g. r10=-2m, pocket=+0h@+00:00. Repeatable.
    #[bpaf(argument("CAM=SHIFT"), many)]
    clock: Vec<String>,
    /// Zone for cameras whose files don't record one (+HH:MM, e.g. +00:00 for UTC).
    #[bpaf(argument("ZONE"))]
    zone: Option<String>,
}

impl ClockArgs {
    fn config(&self, root: &Path) -> anyhow::Result<ClockConfig> {
        let default = root.join("clocks.yaml");
        let mut cfg = match &self.clocks {
            Some(p) => ClockConfig::from_file(p)?,
            None if default.exists() => ClockConfig::from_file(&default)?,
            None => ClockConfig::default(),
        };
        for c in &self.clock {
            cfg.set(c)?;
        }
        if let Some(z) = &self.zone {
            cfg.zone = Some(z.parse::<Zone>()?);
        }
        Ok(cfg)
    }
}

#[derive(Debug, Clone, Bpaf)]
#[allow(clippy::struct_excessive_bools, clippy::doc_markdown)]
struct IngestArgs {
    /// Destination project root.
    #[bpaf(argument("DEST"))]
    to: PathBuf,
    /// Second verified copy, same layout (e.g. /media/backup/projects/trip).
    #[bpaf(argument("DIR"))]
    backup: Option<PathBuf>,
    /// Force the camera id for every file on this card (default: from the files' metadata).
    #[bpaf(argument("ID"))]
    camera: Option<String>,
    #[bpaf(external(clock_args))]
    clocks: ClockArgs,
    /// Show what would be copied; copy nothing.
    dry_run: bool,
    /// Re-hash already-ingested copies instead of trusting name, size and mtime.
    verify_existing: bool,
    /// Don't rebuild DEST/catalog.json afterwards.
    no_catalog: bool,
    /// Card root (the mounted volume or its DCIM folder).
    #[bpaf(positional("CARD_DIR"))]
    card: PathBuf,
}

#[derive(Debug, Clone, Bpaf)]
struct CatalogArgs {
    #[bpaf(external(clock_args))]
    clocks: ClockArgs,
    /// Frame-statistics profile check via the ffmpeg CLI: auto (when metadata isn't
    /// conclusive), always, never.
    #[bpaf(argument("WHEN"), fallback(Histogram::Auto))]
    histogram: Histogram,
    /// Re-probe every file (default: reuse unchanged entries).
    full: bool,
    #[bpaf(positional("DIR"))]
    dir: PathBuf,
}

fn gib(b: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let g = b as f64 / f64::from(1u32 << 30);
    format!("{g:.2} GiB")
}

fn print_catalog(path: &Path, c: &catalog::Catalog) {
    println!("{} clips -> {}", c.clips.len(), path.display());
    for (cam, s) in &c.cameras {
        let fps: Vec<String> = s.fps.iter().map(|(r, n)| format!("{r} x{n}")).collect();
        let prof: Vec<String> = s
            .profiles
            .iter()
            .map(|(p, n)| format!("{p} x{n}"))
            .collect();
        println!(
            "  {cam}: {} clips, {:.1} min, {}, days {}{}{}",
            s.clips,
            s.seconds / 60.0,
            gib(s.bytes),
            s.days.iter().cloned().collect::<Vec<_>>().join(" "),
            if fps.is_empty() {
                String::new()
            } else {
                format!(", fps {}", fps.join(" "))
            },
            if prof.is_empty() {
                String::new()
            } else {
                format!(", {}", prof.join(" "))
            },
        );
    }
    for w in &c.warnings {
        println!("warning: {w}");
    }
    for i in &c.clock_issues {
        match i.suggest_shift {
            Some(s) => println!("clock: {}: {} (set {}: {s})", i.camera, i.message, i.camera),
            None => println!("clock: {}: {}", i.camera, i.message),
        }
    }
    if !c.overlaps.is_empty() {
        println!(
            "{} cross-camera overlaps (audio sync candidates)",
            c.overlaps.len()
        );
    }
}

fn run_catalog(
    dir: &Path,
    clocks: ClockConfig,
    histogram: Histogram,
    full: bool,
) -> anyhow::Result<()> {
    let opts = CatalogOptions {
        clocks,
        histogram,
        full,
    };
    let c = catalog::build(dir, &opts, ingest::now_utc())
        .with_context(|| format!("cataloguing {}", dir.display()))?;
    let path = catalog::write(dir, &c)?;
    print_catalog(&path, &c);
    Ok(())
}

fn run_ingest(a: IngestArgs) -> anyhow::Result<()> {
    let clocks = a.clocks.config(&a.to)?;
    let opts = IngestOptions {
        card: a.card,
        dest: a.to.clone(),
        backup: a.backup,
        clocks: clocks.clone(),
        camera: a.camera,
        dry_run: a.dry_run,
        verify_existing: a.verify_existing,
    };
    let (n, bytes) = (AtomicUsize::new(0), AtomicU64::new(0));
    let on_event = |e: Event<'_>| match e {
        Event::Copied { rel, bytes: b } => {
            let k = n.fetch_add(1, Ordering::Relaxed) + 1;
            let total = bytes.fetch_add(b, Ordering::Relaxed) + b;
            eprintln!("copied   {rel} ({k} files, {})", gib(total));
        }
        Event::Skipped { rel } => eprintln!("have     {rel}"),
        Event::BackedUp { rel } => eprintln!("backup   {rel}"),
        Event::Failed { path, error } => eprintln!("FAILED   {}: {error}", path.display()),
    };
    let r = ingest::ingest(&opts, &on_event)?;
    for (src, rel) in &r.planned {
        println!("{} -> {rel}", src.display());
    }
    println!(
        "{} copied ({}), {} already present, {} backed up, {} ignored",
        r.copied,
        gib(r.bytes),
        r.skipped,
        r.backed_up,
        r.ignored.len()
    );
    for p in &r.ignored {
        eprintln!("ignored  {}", p.display());
    }
    if !r.ok() {
        for (p, e) in &r.failed {
            eprintln!("FAILED   {}: {e}", p.display());
        }
        anyhow::bail!(
            "{} file(s) failed; the card is untouched, re-run to retry",
            r.failed.len()
        );
    }
    if !a.dry_run && !a.no_catalog {
        run_catalog(&a.to, clocks, Histogram::Auto, false)?;
    }
    Ok(())
}

/// Output of `ffmpeg ARGS` from `PATH`, if it runs.
fn ffmpeg_out(args: &[&str]) -> Option<String> {
    std::process::Command::new("ffmpeg")
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// Checks on the `ffmpeg` binary on `PATH` (the prototype's renderer, and the catalog's frame
/// statistics): `(name, result)`.
fn ffmpeg_cli_checks() -> Vec<(String, Result<String, String>)> {
    let Some(version) = ffmpeg_out(&["-hide_banner", "-version"]) else {
        return vec![("ffmpeg cli".into(), Err("not on PATH".into()))];
    };
    let mut out = vec![(
        "ffmpeg cli".into(),
        Ok(version.lines().next().unwrap_or_default().to_owned()),
    )];
    let list = |args: &[&str]| ffmpeg_out(args).unwrap_or_default();
    let has = |text: &str, name: &str| {
        text.lines()
            .any(|l| l.split_whitespace().any(|w| w == name))
    };
    let filters = list(&["-hide_banner", "-filters"]);
    for f in ["libplacebo", "nlmeans_vulkan"] {
        out.push((
            format!("ffmpeg cli filter {f}"),
            if has(&filters, f) {
                Ok("available".into())
            } else {
                Err("missing".into())
            },
        ));
    }
    let hw = list(&["-hide_banner", "-hwaccels"]);
    for h in ["cuda", "vulkan"] {
        out.push((
            format!("ffmpeg cli hwaccel {h}"),
            if has(&hw, h) {
                Ok("available".into())
            } else {
                Err("missing".into())
            },
        ));
    }
    let enc = list(&["-hide_banner", "-encoders"]);
    out.push((
        "ffmpeg cli encoder hevc_nvenc".into(),
        if has(&enc, "hevc_nvenc") {
            Ok("available".into())
        } else {
            Err("missing".into())
        },
    ));
    out
}

fn doctor() -> anyhow::Result<()> {
    let mut failed = Vec::new();
    for (name, r) in ffmpeg_cli_checks() {
        match r {
            Ok(d) => println!("ok    {name}: {d}"),
            Err(e) if cfg!(feature = "gpu") => {
                println!("FAIL  {name}: {e}");
                failed.push(name);
            }
            Err(e) => println!("info  {name}: {e}"),
        }
    }
    gpu_doctor(&mut failed);
    if !failed.is_empty() {
        anyhow::bail!(
            "{} check(s) failed: {}; renders would not run (fully) on the GPU",
            failed.len(),
            failed.join(", ")
        );
    }
    Ok(())
}

#[cfg(feature = "gpu")]
fn gpu_doctor(failed: &mut Vec<String>) {
    let mut checks = ffcine::backend::doctor::checks();
    checks.push(ffcine::backend::doctor::Check {
        name: "nvof",
        result: ffcine::stabilize::nvof::max_api_version()
            .map(|(a, b)| format!("optical flow API {a}.{b}"))
            .map_err(|e| e.to_string()),
        needed_for: "stabilize detect",
    });
    for c in &checks {
        match &c.result {
            Ok(d) => println!("ok    {}: {d}", c.name),
            Err(e) => {
                println!("FAIL  {}: {e} (needed for {})", c.name, c.needed_for);
                failed.push(c.name.to_owned());
            }
        }
    }
}

#[cfg(not(feature = "gpu"))]
fn gpu_doctor(_: &mut Vec<String>) {
    println!(
        "info  gpu: not built (ingest/catalog build); grade and stabilize need the gpu feature"
    );
}

fn main() -> anyhow::Result<()> {
    match cmd().run() {
        #[cfg(feature = "gpu")]
        Cmd::Gpu(c) => gpu::run(c),
        Cmd::Ingest(a) => run_ingest(a),
        Cmd::Catalog(a) => {
            let clocks = a.clocks.config(&a.dir)?;
            run_catalog(&a.dir, clocks, a.histogram, a.full)
        }
        Cmd::Doctor => doctor(),
        Cmd::Publish {
            immich,
            config,
            files,
        } => publish::Immich::from_config(config.as_deref())?
            .publish(&immich, &files)
            .map(drop),
        Cmd::Search(a) => a.run(),
        Cmd::Snap(a) => a.run(),
        #[cfg(feature = "audio")]
        Cmd::Sync(a) => a.run(),
        #[cfg(feature = "audio")]
        Cmd::Audio(a) => a.run(),
        #[cfg(feature = "audio")]
        Cmd::Beats(a) => a.run(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_invariants() {
        cmd().check_invariants(false);
    }

    #[test]
    fn ingest_args() {
        let Cmd::Ingest(a) = cmd()
            .run_inner(&[
                "ingest",
                "--to",
                "/proj",
                "--backup",
                "/backup/x",
                "--clock",
                "r10=-2m",
                "--clock",
                "pocket=+0h",
                "/Volumes/CARD",
            ])
            .unwrap()
        else {
            panic!("expected ingest");
        };
        assert_eq!(a.card, PathBuf::from("/Volumes/CARD"));
        assert_eq!(a.clocks.clock.len(), 2);
        let cfg = a.clocks.config(Path::new("/nonexistent")).unwrap();
        assert!((cfg.cameras["r10"].shift.0 + 120.0).abs() < 1e-9);
    }
}
