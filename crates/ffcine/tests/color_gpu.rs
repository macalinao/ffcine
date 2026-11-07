//! Colour checks on the GPU with synthetic 10-bit 4:2:2 HEVC fixtures (`cargo test -p ffcine
//! --test color_gpu -- --ignored`; needs a Vulkan GPU and `ffmpeg` with libx265 on PATH, as in
//! the dev shell):
//!
//! - Canon Log 3 (BT.2020, untagged transfer, `CAEP` brand): detected as `clog3`, and the
//!   analytic camera LUT on the GPU matches ffmpeg's CPU `lut3d` with the same LUT;
//! - HDR10 PQ in, PQ out at the same peak: passes through, and the output carries the BT.2020 PQ
//!   tags and our mastering display and content light metadata;
//! - PQ in, SDR out: tone-mapped to BT.709;
//! - a camera LUT plus a grade LUT render the same as the one LUT the baker makes of them.

use std::path::{Path, PathBuf};
use std::process::Command;

use ffcine::camera::{Profile, ProfileChoice};
use ffcine::shaders::LookParams;
use ffcine::{
    Backend as _, Bake, Codec, ColorSpace, EncoderSettings, Grade, HdrMetadata, LutStage, LutType,
    PixelFormat, RateControl, backend,
};

const N: u64 = 4;
/// Against a CPU reference (the 4:2:2 source goes out 4:2:0).
const MIN_DB: f64 = 40.0;

fn dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("ffcine-color-gpu-{}", std::process::id()));
    std::fs::create_dir_all(&d).expect("temp dir");
    d
}

fn ffmpeg(args: &[&str]) -> Vec<u8> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-y"])
        .args(args)
        .output()
        .expect("ffmpeg on PATH");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn ffprobe(path: &Path, args: &[&str]) -> String {
    let out = Command::new("ffprobe")
        .args(["-v", "error"])
        .args(args)
        .arg(path)
        .output()
        .expect("ffprobe on PATH");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A lossless 10-bit 4:2:2 HEVC of smooth colour gradients squeezed into a log-like 25-70 %
/// code range, with x265 VUI `params` (`colorprim=...`).
fn fixture(path: &Path, params: &str, extra: &[&str]) {
    let x265 = format!("log-level=error:lossless=1:range=limited:{params}");
    let mut args = vec![
        "-f",
        "lavfi",
        "-i",
        "gradients=size=1280x720:rate=25:speed=0.05",
        "-vf",
        "format=gbrp,lutrgb=r=val*0.45+64:g=val*0.45+64:b=val*0.45+64",
        "-frames:v",
        "4",
        "-pix_fmt",
        "yuv422p10le",
        "-c:v",
        "libx265",
        "-x265-params",
        &x265,
    ];
    args.extend_from_slice(extra);
    args.push(path.to_str().expect("utf-8"));
    ffmpeg(&args);
}

/// The first `N` frames as full-range 16-bit RGB: limited-range YCbCr in `matrix`, then `post`
/// (filters on that RGB).
fn rgb48(path: &Path, matrix: &str, post: &str) -> Vec<u16> {
    let p = path.to_str().expect("utf-8");
    let vf = format!(
        "trim=end_frame={N},scale=in_color_matrix={matrix}:in_range=tv:out_range=pc,\
         format=gbrp16le,{post}format=rgb48le"
    );
    let b = ffmpeg(&["-i", p, "-vf", &vf, "-f", "rawvideo", "-"]);
    b.as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect()
}

#[allow(clippy::cast_precision_loss)]
fn psnr(a: &[u16], b: &[u16]) -> f64 {
    assert_eq!(a.len(), b.len());
    let se: f64 = a
        .iter()
        .zip(b)
        .map(|(x, y)| (f64::from(*x) - f64::from(*y)).powi(2))
        .sum();
    10.0 * (65535.0_f64.powi(2) / (se / a.len() as f64).max(1e-9)).log10()
}

fn x265_lossless(color: ColorSpace) -> EncoderSettings {
    EncoderSettings {
        codec: Codec::Libx265,
        preset: Some("ultrafast".into()),
        profile: Some("main10".into()),
        rate: RateControl::Crf(0.0),
        pixel_format: PixelFormat::Yuv420p10,
        color,
        extra: vec![("x265-params".into(), "lossless=1:log-level=error".into())],
        ..EncoderSettings::x264_preview()
    }
}

fn render(ff: &backend::Ffmpeg, src: &Path, out: &Path, grade: Grade, enc: EncoderSettings) {
    ffcine::Pipeline::new(src, out)
        .range(std::time::Duration::ZERO, Some(N))
        .grade(grade)
        .encoder(enc)
        .run(ff)
        .expect("render");
}

fn plain() -> Grade {
    Grade {
        dither: None,
        peak_detect: false,
        ..Grade::default()
    }
}

#[test]
#[ignore = "needs a Vulkan GPU and ffmpeg with libx265"]
#[allow(clippy::too_many_lines, clippy::many_single_char_names)]
fn clog3_pq_and_bake() {
    let d = dir();
    let ff = backend::Ffmpeg::new().expect("backend");

    // Canon Log 3, 4:2:2 10-bit: detection, decode, camera LUT against ffmpeg's lut3d.
    let clog = d.join("clog3.mp4");
    fixture(
        &clog,
        "colorprim=bt2020:colormatrix=bt2020nc",
        &["-brand", "CAEP"],
    );
    let info = ff.probe(&clog).expect("probe");
    assert_eq!(ProfileChoice::Auto.resolve(&info), Profile::CanonClog3);
    let cam = Profile::CanonClog3
        .camera_lut(&[])
        .expect("lut")
        .expect("analytic");
    let cube = d.join("clog3.cube");
    cam.lut.write_cube(&cube).expect("cube");
    let mut g = plain();
    g.input_color.primaries = Some(cam.primaries);
    g.input_color.transfer = Some(cam.transfer);
    g.input_lut = Some(LutStage {
        lut: cam.lut.clone(),
        kind: LutType::Normalized,
    });
    let gpu = d.join("clog3_sdr.mkv");
    render(&ff, &clog, &gpu, g, x265_lossless(ColorSpace::BT709_TV));
    let lut3d = format!("lut3d=file={}:interp=trilinear,", cube.display());
    let p = psnr(&rgb48(&gpu, "bt709", ""), &rgb48(&clog, "bt2020", &lut3d));
    eprintln!("clog3 camera LUT vs lut3d: {p:.1} dB");
    assert!(p > MIN_DB, "clog3 {p:.1} dB");

    // HDR10 PQ in, PQ out at the source's peak: a passthrough, with our metadata.
    let pq = d.join("pq.mp4");
    fixture(
        &pq,
        "colorprim=bt2020:colormatrix=bt2020nc:transfer=smpte2084:hdr10=1:\
         master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,1):\
         max-cll=1000,400",
        &[],
    );
    let out = d.join("pq_pq.mkv");
    let g = Grade {
        output: ColorSpace::BT2020_PQ,
        output_hdr: Some(HdrMetadata::P3_D65_1000),
        ..plain()
    };
    render(&ff, &pq, &out, g, x265_lossless(ColorSpace::BT2020_PQ));
    let tags = ffprobe(
        &out,
        &[
            "-show_entries",
            "stream=color_transfer,color_primaries,color_space",
            "-of",
            "compact",
        ],
    );
    assert!(tags.contains("color_transfer=smpte2084"), "{tags}");
    assert!(tags.contains("color_primaries=bt2020"), "{tags}");
    let side = ffprobe(
        &out,
        &[
            "-read_intervals",
            "%+#1",
            "-show_frames",
            "-show_entries",
            "frame_side_data",
        ],
    );
    assert!(side.contains("Mastering display metadata"), "{side}");
    assert!(side.contains("max_luminance=10000000/10000"), "{side}");
    assert!(side.contains("max_content=1000"), "{side}");
    let p = psnr(&rgb48(&out, "bt2020", ""), &rgb48(&pq, "bt2020", ""));
    eprintln!("PQ -> PQ passthrough: {p:.1} dB");
    assert!(p > MIN_DB, "PQ passthrough {p:.1} dB");

    // PQ in, SDR out: tone-mapped, tagged BT.709.
    let sdr = d.join("pq_sdr.mkv");
    render(&ff, &pq, &sdr, plain(), x265_lossless(ColorSpace::BT709_TV));
    let tags = ffprobe(
        &sdr,
        &[
            "-show_entries",
            "stream=color_transfer,color_primaries",
            "-of",
            "compact",
        ],
    );
    assert!(
        tags.contains("color_transfer=bt709") && tags.contains("color_primaries=bt709"),
        "{tags}"
    );

    // Camera + grade LUT (merged in the backend) == the baker's one LUT.
    let mut grade_lut = ffcine::Lut::identity(17);
    for e in &mut grade_lut.data {
        *e = [e[0].powf(0.8), e[1], e[2].powf(1.2)];
    }
    let split = Grade {
        input_lut: Some(LutStage {
            lut: cam.lut.clone(),
            kind: LutType::Normalized,
        }),
        lut: Some(LutStage {
            lut: grade_lut.clone(),
            kind: LutType::Normalized,
        }),
        input_color: ffcine::ColorOverride {
            primaries: Some(cam.primaries),
            transfer: Some(cam.transfer),
            ..Default::default()
        },
        ..plain()
    };
    let baked = Bake {
        camera: Some(&cam.lut),
        lut: Some(&grade_lut),
        look: LookParams::default(),
    }
    .bake(65);
    let one = Grade {
        input_lut: Some(LutStage {
            lut: baked,
            kind: LutType::Normalized,
        }),
        lut: None,
        ..split.clone()
    };
    let (a, b) = (d.join("split.mkv"), d.join("baked.mkv"));
    render(&ff, &clog, &a, split, x265_lossless(ColorSpace::BT709_TV));
    render(&ff, &clog, &b, one, x265_lossless(ColorSpace::BT709_TV));
    let p = psnr(&rgb48(&a, "bt709", ""), &rgb48(&b, "bt709", ""));
    eprintln!("camera + grade LUT vs baked: {p:.1} dB");
    assert!(p > 45.0, "bake {p:.1} dB");
    let _ = std::fs::remove_dir_all(&d);
}
