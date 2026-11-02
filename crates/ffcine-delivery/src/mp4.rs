//! Chapters and cover art written into a finished MP4 with `mp4-atom`, without touching a frame.
//!
//! Chapters go in twice, as ffmpeg's muxer writes them: a Nero `moov/udta/chpl` list (ffmpeg,
//! VLC, mpv) and a QuickTime chapter track (a disabled `text` track that every other track
//! references with `tref/chap`; Apple players read only that one). The cover is an iTunes
//! `moov/udta/meta/ilst/covr` item (JPEG or PNG), which ffprobe lists as an attached picture.
//!
//! The file is rewritten in one pass: `ftyp`, the new `moov`, a small `mdat` holding the chapter
//! samples, then every other top-level box byte for byte (so the result is also `+faststart`).
//! Chunk offsets (`stco`, or `co64` past 4 GiB) are moved with their boxes. Anything a previous
//! run wrote (its chapter track, `chpl`, `covr`) is replaced, so tagging twice is the same as
//! tagging once.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read as _, Seek as _, SeekFrom, Write as _};
use std::path::Path;

use ffcine_core::{Error, Result};
use mp4_atom::{
    Chpl, ChplEntry, Co64, Covr, Decode as _, Dinf, Dref, DrefEntry, FourCC, Hdlr, Ilst, Mdhd,
    Meta, Mvhd, Stco, Stsc, StscEntry, Stsz, StszSamples, Stts, SttsEntry, Tkhd,
    TrackReferenceTypeBox, Tref, Udta, Url,
};

use crate::Chapter;

/// What [`write()`] puts in the file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tags {
    /// In order, the first at 0. Starts past the end are dropped; titles are cut to 255 bytes
    /// and the list to 255 entries (the `chpl` limits).
    pub chapters: Vec<Chapter>,
    /// A JPEG or PNG file's bytes.
    pub cover: Option<Vec<u8>>,
}

/// The cover's `data` type: 13 JPEG, 14 PNG.
fn image_type(data: &[u8]) -> Option<u32> {
    if data.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(13)
    } else if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(14)
    } else {
        None
    }
}

fn invalid(what: &'static str, value: impl Into<String>) -> Error {
    Error::Invalid {
        what,
        value: value.into(),
    }
}

fn atom_err(e: &mp4_atom::Error) -> Error {
    invalid("MP4", e.to_string())
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.to_owned(),
        source,
    }
}

/// One box in a buffer: its type, the whole box and its header length.
#[derive(Clone, Copy)]
struct Raw<'a> {
    kind: [u8; 4],
    bytes: &'a [u8],
    header: usize,
}

impl<'a> Raw<'a> {
    fn body(&self) -> &'a [u8] {
        &self.bytes[self.header..]
    }
}

/// The boxes in `buf`, in order.
fn children(mut buf: &[u8]) -> Result<Vec<Raw<'_>>> {
    let mut out = Vec::new();
    while buf.len() >= 8 {
        let size32 = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let kind = [buf[4], buf[5], buf[6], buf[7]];
        let (size, header) = match size32 {
            0 => (buf.len(), 8),
            1 => {
                let b = buf
                    .get(8..16)
                    .ok_or_else(|| invalid("MP4 box", "truncated"))?;
                let s = u64::from_be_bytes(b.try_into().unwrap_or_default());
                (usize::try_from(s).unwrap_or(usize::MAX), 16)
            }
            s => (s as usize, 8),
        };
        if size < header || size > buf.len() {
            return Err(invalid(
                "MP4 box",
                format!("{} of {size} bytes", String::from_utf8_lossy(&kind)),
            ));
        }
        out.push(Raw {
            kind,
            bytes: &buf[..size],
            header,
        });
        buf = &buf[size..];
    }
    Ok(out)
}

/// A box of `kind` around `body`.
#[allow(clippy::trivially_copy_pass_by_ref)] // called with `b"type"` literals
fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(body.len() + 16);
    if let Ok(size) = u32::try_from(body.len() + 8) {
        v.extend_from_slice(&size.to_be_bytes());
        v.extend_from_slice(kind);
    } else {
        v.extend_from_slice(&1u32.to_be_bytes());
        v.extend_from_slice(kind);
        v.extend_from_slice(&(body.len() as u64 + 16).to_be_bytes());
    }
    v.extend_from_slice(body);
    v
}

fn encode(atom: &impl mp4_atom::Encode) -> Result<Vec<u8>> {
    let mut v = Vec::new();
    atom.encode(&mut v).map_err(|e| atom_err(&e))?;
    Ok(v)
}

fn track_id(trak: &Raw) -> Result<u32> {
    let tkhd = children(trak.body())?
        .into_iter()
        .find(|c| &c.kind == b"tkhd")
        .ok_or_else(|| invalid("MP4", "trak without tkhd"))?;
    let t = Tkhd::decode(&mut { tkhd.bytes }).map_err(|e| atom_err(&e))?;
    Ok(t.track_id)
}

/// Track ids the `tref/chap` boxes of `traks` point at.
fn chapter_tracks(traks: &[Raw]) -> Result<Vec<u32>> {
    let mut ids = Vec::new();
    for t in traks {
        for c in children(t.body())?
            .into_iter()
            .filter(|c| &c.kind == b"tref")
        {
            let tref = Tref::decode(&mut { c.bytes }).map_err(|e| atom_err(&e))?;
            for r in tref.track_reference_type_boxes {
                if r.reference_type == FourCC::new(b"chap") {
                    ids.extend(r.track_ids);
                }
            }
        }
    }
    Ok(ids)
}

/// `container` rebuilt with its chunk offsets moved by `map` (`stco` becomes `co64` when an
/// offset no longer fits 32 bits).
fn remap(container: &Raw, map: &dyn Fn(u64) -> Result<u64>) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    for c in children(container.body())? {
        match &c.kind {
            b"mdia" | b"minf" | b"stbl" => body.extend(remap(&c, map)?),
            b"stco" | b"co64" => {
                let old: Vec<u64> = if &c.kind == b"stco" {
                    let s = Stco::decode(&mut { c.bytes }).map_err(|e| atom_err(&e))?;
                    s.entries.into_iter().map(u64::from).collect()
                } else {
                    Co64::decode(&mut { c.bytes })
                        .map_err(|e| atom_err(&e))?
                        .entries
                };
                let new = old.into_iter().map(map).collect::<Result<Vec<_>>>()?;
                body.extend(offsets(new)?);
            }
            _ => body.extend_from_slice(c.bytes),
        }
    }
    Ok(boxed(&container.kind, &body))
}

/// `stco`, or `co64` if an offset needs it.
fn offsets(entries: Vec<u64>) -> Result<Vec<u8>> {
    match entries
        .iter()
        .map(|o| u32::try_from(*o))
        .collect::<std::result::Result<Vec<_>, _>>()
    {
        Ok(entries) => encode(&Stco { entries }),
        Err(_) => encode(&Co64 { entries }),
    }
}

/// A `trak` with its chunk offsets remapped and its `tref/chap` pointing at `chap` (dropped
/// when `None`).
fn rebuild_trak(
    trak: &Raw,
    map: &dyn Fn(u64) -> Result<u64>,
    chap: Option<u32>,
) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    let mut tref_done = false;
    let new_tref = |old: Option<&Raw>| -> Result<Option<Vec<u8>>> {
        let mut t = match old {
            Some(r) => Tref::decode(&mut { r.bytes }).map_err(|e| atom_err(&e))?,
            None => Tref::default(),
        };
        t.track_reference_type_boxes
            .retain(|r| r.reference_type != FourCC::new(b"chap"));
        if let Some(id) = chap {
            t.track_reference_type_boxes.push(TrackReferenceTypeBox {
                reference_type: FourCC::new(b"chap"),
                track_ids: vec![id],
            });
        }
        if t.track_reference_type_boxes.is_empty() {
            Ok(None)
        } else {
            encode(&t).map(Some)
        }
    };
    for c in children(trak.body())? {
        match &c.kind {
            b"tkhd" => {
                body.extend_from_slice(c.bytes);
                if !tref_done && !children(trak.body())?.iter().any(|c| &c.kind == b"tref") {
                    body.extend(new_tref(None)?.unwrap_or_default());
                    tref_done = true;
                }
            }
            b"tref" => {
                body.extend(new_tref(Some(&c))?.unwrap_or_default());
                tref_done = true;
            }
            b"mdia" => body.extend(remap(&c, map)?),
            _ => body.extend_from_slice(c.bytes),
        }
    }
    Ok(boxed(b"trak", &body))
}

/// The QuickTime `text` sample entry ffmpeg writes for chapter tracks.
const TEXT_ENTRY: &[u8] = &[
    0x00, 0x00, 0x00, 0x4f, b't', b'e', b'x', b't', 0, 0, 0, 0, 0, 0, 0, 1, // data ref 1
    0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0x0d, b'f', b't', b'a', b'b', 0, 1, 0, 1, 0, 0, 0, 0, 0x14, b'b', b'r',
    b't', b't', 0, 0, 0, 0, 0, 0, 0, 0x2d, 0, 0, 0, 0x2d,
];

/// `gmhd` (`gmin` + `text`) as ffmpeg writes it for a text track.
const GMHD_BODY: &[u8] = &[
    0x00, 0x00, 0x00, 0x18, b'g', b'm', b'i', b'n', 0, 0, 0, 0, 0, 0x40, 0x80, 0, 0x80, 0, 0x80, 0,
    0, 0, 0, 0, 0x00, 0x00, 0x00, 0x2c, b't', b'e', b'x', b't', 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x40, 0, 0, 0,
];

/// A chapter sample: length-prefixed UTF-8 and an `encd` box saying so.
fn chapter_sample(title: &str) -> Vec<u8> {
    let t = truncate(title, 255);
    let mut v = Vec::with_capacity(t.len() + 14);
    v.extend_from_slice(&u16::try_from(t.len()).unwrap_or(0).to_be_bytes());
    v.extend_from_slice(t.as_bytes());
    v.extend_from_slice(&[0, 0, 0, 0x0c, b'e', b'n', b'c', b'd', 0, 0, 1, 0]);
    v
}

fn truncate(s: &str, max: usize) -> &str {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// The chapter `trak`: `starts_ms` and `durs_ms` per sample, samples at `offset`.
fn chapter_trak(
    id: u32,
    movie_duration: u64,
    sizes: Vec<u32>,
    durs_ms: &[u32],
    offset: u64,
) -> Result<Vec<u8>> {
    let total: u64 = durs_ms.iter().map(|d| u64::from(*d)).sum();
    let tkhd = encode(&Tkhd {
        track_id: id,
        duration: movie_duration,
        enabled: false,
        in_movie: true,
        ..Tkhd::default()
    })?;
    let mdhd = encode(&Mdhd {
        timescale: 1000,
        duration: total,
        language: "und".into(),
        ..Mdhd::default()
    })?;
    let hdlr = encode(&Hdlr {
        handler: FourCC::new(b"text"),
        name: "ChapterHandler".into(),
    })?;
    let dinf = encode(&Dinf {
        dref: Dref {
            entries: vec![DrefEntry::Url(Url::default())],
        },
    })?;
    let mut stsd_body = vec![0, 0, 0, 0, 0, 0, 0, 1];
    stsd_body.extend_from_slice(TEXT_ENTRY);
    let n = u32::try_from(sizes.len()).unwrap_or(u32::MAX);
    let stbl = [
        boxed(b"stsd", &stsd_body),
        encode(&Stts {
            entries: durs_ms
                .iter()
                .map(|d| SttsEntry {
                    sample_count: 1,
                    sample_delta: *d,
                })
                .collect(),
        })?,
        encode(&Stsc {
            entries: vec![StscEntry {
                first_chunk: 1,
                samples_per_chunk: n,
                sample_description_index: 1,
            }],
        })?,
        encode(&Stsz {
            samples: StszSamples::Different { sizes },
        })?,
        offsets(vec![offset])?,
    ]
    .concat();
    let minf = [boxed(b"gmhd", GMHD_BODY), dinf, boxed(b"stbl", &stbl)].concat();
    let mdia = [mdhd, hdlr, boxed(b"minf", &minf)].concat();
    Ok(boxed(b"trak", &[tkhd, boxed(b"mdia", &mdia)].concat()))
}

/// `udta` with `chpl` and `covr` replaced.
fn rebuild_udta(old: Option<&Raw>, chpl: Option<Chpl>, cover: Option<&[u8]>) -> Result<Vec<u8>> {
    let mut u = match old {
        Some(r) => Udta::decode(&mut { r.bytes }).map_err(|e| atom_err(&e))?,
        None => Udta::default(),
    };
    u.chpl = chpl;
    let item = cover.and_then(|c| {
        let ty = image_type(c)?;
        let mut data = Vec::with_capacity(c.len() + 16);
        data.extend_from_slice(&u32::try_from(c.len() + 16).unwrap_or(0).to_be_bytes());
        data.extend_from_slice(b"data");
        data.extend_from_slice(&ty.to_be_bytes());
        data.extend_from_slice(&[0; 4]);
        data.extend_from_slice(c);
        Some(Covr(data))
    });
    if let Some(meta) = &mut u.meta {
        match meta.get_mut::<Ilst>() {
            Some(ilst) => ilst.covr = item,
            None if item.is_some() => meta.push(Ilst {
                covr: item,
                ..Ilst::default()
            }),
            None => {}
        }
    } else if item.is_some() {
        let mut meta = Meta {
            hdlr: Hdlr {
                handler: FourCC::new(b"mdir"),
                name: String::new(),
            },
            items: Vec::new(),
        };
        meta.push(Ilst {
            covr: item,
            ..Ilst::default()
        });
        u.meta = Some(meta);
    }
    if u == Udta::default() {
        return Ok(Vec::new());
    }
    encode(&u)
}

/// A top-level box of the input file.
struct Top {
    kind: [u8; 4],
    offset: u64,
    size: u64,
}

fn top_level(f: &mut File, path: &Path) -> Result<Vec<Top>> {
    let len = f.metadata().map_err(io(path))?.len();
    let mut out = Vec::new();
    let mut at = 0;
    while at + 8 <= len {
        f.seek(SeekFrom::Start(at)).map_err(io(path))?;
        let mut h = [0u8; 16];
        f.read_exact(&mut h[..8]).map_err(io(path))?;
        let kind = [h[4], h[5], h[6], h[7]];
        let size = match u32::from_be_bytes([h[0], h[1], h[2], h[3]]) {
            0 => len - at,
            1 => {
                f.read_exact(&mut h[8..]).map_err(io(path))?;
                u64::from_be_bytes([h[8], h[9], h[10], h[11], h[12], h[13], h[14], h[15]])
            }
            s => u64::from(s),
        };
        if size < 8 || at + size > len {
            return Err(invalid(
                "MP4 box",
                format!(
                    "{}: {} at {at}",
                    path.display(),
                    String::from_utf8_lossy(&kind)
                ),
            ));
        }
        out.push(Top {
            kind,
            offset: at,
            size,
        });
        at += size;
    }
    Ok(out)
}

/// Writes `input` with `tags` to `output` (a different path; see [`write_in_place`]).
#[allow(clippy::too_many_lines)]
pub fn write(input: &Path, output: &Path, tags: &Tags) -> Result<()> {
    if let Some(c) = &tags.cover {
        if image_type(c).is_none() {
            return Err(invalid(
                "cover image (JPEG or PNG)",
                format!("{} bytes", c.len()),
            ));
        }
    }
    let mut f = File::open(input).map_err(io(input))?;
    let top = top_level(&mut f, input)?;
    let moov_top = top
        .iter()
        .find(|t| &t.kind == b"moov")
        .ok_or_else(|| invalid("MP4", format!("{}: no moov", input.display())))?;
    let mut moov = vec![0; usize::try_from(moov_top.size).unwrap_or(usize::MAX)];
    f.seek(SeekFrom::Start(moov_top.offset))
        .map_err(io(input))?;
    f.read_exact(&mut moov).map_err(io(input))?;
    let moov = children(&moov)?
        .into_iter()
        .next()
        .ok_or_else(|| invalid("MP4", "empty moov"))?;
    let kids = children(moov.body())?;

    let mvhd_raw = kids
        .iter()
        .find(|c| &c.kind == b"mvhd")
        .ok_or_else(|| invalid("MP4", "moov without mvhd"))?;
    let mvhd = Mvhd::decode(&mut { mvhd_raw.bytes }).map_err(|e| atom_err(&e))?;
    let traks: Vec<Raw> = kids
        .iter()
        .copied()
        .filter(|c| &c.kind == b"trak")
        .collect();
    let old_chap = chapter_tracks(&traks)?;
    let mut keep = Vec::new();
    for t in &traks {
        let id = track_id(t)?;
        if !old_chap.contains(&id) {
            keep.push((id, *t));
        }
    }

    // Chapters: start in ms, sample durations, and the samples.
    let dur_ms = if mvhd.timescale > 0 {
        let ts = u128::from(mvhd.timescale);
        u64::try_from((u128::from(mvhd.duration) * 1000 + ts / 2) / ts).unwrap_or(u64::MAX)
    } else {
        0
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let chapters: Vec<(u64, &str)> = tags
        .chapters
        .iter()
        .map(|c| ((c.start.max(0.0) * 1000.0).round() as u64, c.title.as_str()))
        .filter(|(s, _)| *s < dur_ms.max(1))
        .take(255)
        .collect();
    let mut durs = Vec::new();
    let mut samples = Vec::new();
    let mut sizes = Vec::new();
    for (i, (s, title)) in chapters.iter().enumerate() {
        let end = chapters.get(i + 1).map_or(dur_ms, |n| n.0);
        durs.push(u32::try_from(end.saturating_sub(*s).max(1)).unwrap_or(u32::MAX));
        let sample = chapter_sample(title);
        sizes.push(u32::try_from(sample.len()).unwrap_or(0));
        samples.extend(sample);
    }
    let chpl = (!chapters.is_empty()).then(|| Chpl {
        entries: chapters
            .iter()
            .map(|(s, t)| ChplEntry {
                start: s * 10_000,
                title: truncate(t, 255).to_owned(),
            })
            .collect(),
    });
    let chap_id =
        (!chapters.is_empty()).then(|| keep.iter().map(|(id, _)| *id).max().unwrap_or(0) + 1);

    // Output layout: ftyp, moov, chapter mdat, everything else as it was.
    let ftyp = top.iter().find(|t| &t.kind == b"ftyp");
    let rest: Vec<&Top> = top
        .iter()
        .filter(|t| !matches!(&t.kind, b"ftyp" | b"moov" | b"free" | b"skip" | b"wide"))
        .collect();
    let ftyp_len = ftyp.map_or(0, |t| t.size);
    let chap_mdat = if samples.is_empty() {
        Vec::new()
    } else {
        boxed(b"mdat", &samples)
    };
    let build = |moov_len: u64| -> Result<Vec<u8>> {
        let mut at = ftyp_len + moov_len + chap_mdat.len() as u64;
        let mut moved = Vec::with_capacity(rest.len());
        for t in &rest {
            moved.push((t.offset, t.size, at));
            at += t.size;
        }
        let map = |o: u64| -> Result<u64> {
            moved
                .iter()
                .find(|(from, size, _)| (*from..from + size).contains(&o))
                .map(|(from, _, to)| o - from + to)
                .ok_or_else(|| invalid("MP4 chunk offset", o.to_string()))
        };
        let mut body = Vec::new();
        for c in &kids {
            match &c.kind {
                b"mvhd" => {
                    let mut m = c.bytes.to_vec();
                    if let Some(id) = chap_id {
                        let n = m.len();
                        let next = u32::from_be_bytes([m[n - 4], m[n - 3], m[n - 2], m[n - 1]]);
                        m[n - 4..].copy_from_slice(&next.max(id + 1).to_be_bytes());
                    }
                    body.extend(m);
                }
                b"trak" => {
                    if let Some((_, t)) = keep
                        .iter()
                        .find(|(_, t)| t.bytes.as_ptr() == c.bytes.as_ptr())
                    {
                        body.extend(rebuild_trak(t, &map, chap_id)?);
                    }
                }
                b"udta" => {}
                _ => body.extend_from_slice(c.bytes),
            }
        }
        if let Some(id) = chap_id {
            body.extend(chapter_trak(
                id,
                mvhd.duration,
                sizes.clone(),
                &durs,
                ftyp_len + moov_len + 8,
            )?);
        }
        body.extend(rebuild_udta(
            kids.iter().find(|c| &c.kind == b"udta"),
            chpl.clone(),
            tags.cover.as_deref(),
        )?);
        Ok(boxed(b"moov", &body))
    };
    let mut new_moov = build(moov_top.size)?;
    for _ in 0..4 {
        let next = build(new_moov.len() as u64)?;
        if next.len() == new_moov.len() {
            new_moov = next;
            break;
        }
        new_moov = next;
    }

    let out_file = File::create(output).map_err(io(output))?;
    let mut w = BufWriter::with_capacity(1 << 20, out_file);
    let mut r = BufReader::with_capacity(1 << 20, f);
    let mut copy = |t: &Top, w: &mut BufWriter<File>| -> Result<()> {
        r.seek(SeekFrom::Start(t.offset)).map_err(io(input))?;
        let n = std::io::copy(&mut (&mut r).take(t.size), w).map_err(io(output))?;
        if n == t.size {
            Ok(())
        } else {
            Err(invalid("MP4", format!("{}: short read", input.display())))
        }
    };
    if let Some(t) = ftyp {
        copy(t, &mut w)?;
    }
    w.write_all(&new_moov).map_err(io(output))?;
    w.write_all(&chap_mdat).map_err(io(output))?;
    for t in &rest {
        copy(t, &mut w)?;
    }
    w.flush().map_err(io(output))?;
    Ok(())
}

/// [`write()`] to `<path>.partial.<ext>`, then renamed over `path`.
pub fn write_in_place(path: &Path, tags: &Tags) -> Result<()> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("mp4");
    let partial = path.with_extension(format!("tags.partial.{ext}"));
    if let Err(e) = write(path, &partial, tags) {
        let _ = std::fs::remove_file(&partial);
        return Err(e);
    }
    std::fs::rename(&partial, path).map_err(io(path))
}

/// Moves `path`'s `moov` ahead of its media (`+faststart`, for playback over HTTP before the
/// download ends), keeping its chapters and cover.
pub fn faststart(path: &Path) -> Result<()> {
    write_in_place(path, &read(path)?)
}

/// The chapters (`chpl`) and cover (`covr`) in an MP4, as [`write()`] puts them.
pub fn read(path: &Path) -> Result<Tags> {
    let mut f = File::open(path).map_err(io(path))?;
    let top = top_level(&mut f, path)?;
    let Some(m) = top.iter().find(|t| &t.kind == b"moov") else {
        return Err(invalid("MP4", format!("{}: no moov", path.display())));
    };
    let mut moov = vec![0; usize::try_from(m.size).unwrap_or(usize::MAX)];
    f.seek(SeekFrom::Start(m.offset)).map_err(io(path))?;
    f.read_exact(&mut moov).map_err(io(path))?;
    let mut tags = Tags::default();
    let moov = children(&moov)?;
    let Some(moov) = moov.first() else {
        return Ok(tags);
    };
    if let Some(u) = children(moov.body())?
        .into_iter()
        .find(|c| &c.kind == b"udta")
    {
        let u = Udta::decode(&mut { u.bytes }).map_err(|e| atom_err(&e))?;
        #[allow(clippy::cast_precision_loss)]
        if let Some(c) = u.chpl {
            tags.chapters = c
                .entries
                .into_iter()
                .map(|e| Chapter {
                    start: e.start as f64 / 1e7,
                    title: e.title,
                })
                .collect();
        }
        tags.cover = u
            .meta
            .as_ref()
            .and_then(|m| m.get::<Ilst>())
            .and_then(|i| i.covr.as_ref())
            .and_then(|c| c.0.get(16..).map(<[u8]>::to_vec));
    }
    Ok(tags)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny MP4 by hand: `ftyp`, `mdat` (two "frames"), then `moov` with one video `trak`
    /// whose `stco` points at them, 2 s long.
    fn synthetic() -> Vec<u8> {
        let ftyp = boxed(b"ftyp", b"isom\0\0\x02\0isomiso2mp41");
        let mdat = boxed(b"mdat", b"FRAME-ONEFRAME-TWO");
        let first = u32::try_from(ftyp.len() + 8).expect("small");
        let mvhd = encode(&Mvhd {
            timescale: 1000,
            duration: 2000,
            rate: 1.into(),
            volume: 1.into(),
            next_track_id: 2,
            ..Mvhd::default()
        })
        .expect("mvhd");
        let tkhd = encode(&Tkhd {
            track_id: 1,
            duration: 2000,
            enabled: true,
            in_movie: true,
            ..Tkhd::default()
        })
        .expect("tkhd");
        let stbl = [
            encode(&Stsc {
                entries: vec![StscEntry {
                    first_chunk: 1,
                    samples_per_chunk: 1,
                    sample_description_index: 1,
                }],
            })
            .expect("stsc"),
            encode(&Stsz {
                samples: StszSamples::Identical { count: 2, size: 9 },
            })
            .expect("stsz"),
            encode(&Stco {
                entries: vec![first, first + 9],
            })
            .expect("stco"),
        ]
        .concat();
        let mdia = [
            encode(&Mdhd {
                timescale: 1000,
                duration: 2000,
                language: "und".into(),
                ..Mdhd::default()
            })
            .expect("mdhd"),
            boxed(b"minf", &boxed(b"stbl", &stbl)),
        ]
        .concat();
        let trak = boxed(b"trak", &[tkhd, boxed(b"mdia", &mdia)].concat());
        let moov = boxed(b"moov", &[mvhd, trak].concat());
        [ftyp, mdat, moov].concat()
    }

    /// The bytes each chunk offset of track 1 points at.
    fn frames(file: &[u8]) -> Vec<Vec<u8>> {
        let top = children(file).expect("top");
        let moov = top.iter().find(|c| &c.kind == b"moov").expect("moov");
        for t in children(moov.body())
            .expect("moov")
            .iter()
            .filter(|c| &c.kind == b"trak")
        {
            if track_id(t).expect("id") != 1 {
                continue;
            }
            let mdia = children(t.body())
                .expect("t")
                .into_iter()
                .find(|c| &c.kind == b"mdia");
            let minf = children(mdia.expect("mdia").body())
                .expect("mdia")
                .into_iter()
                .find(|c| &c.kind == b"minf")
                .expect("minf");
            let stbl = children(minf.body()).expect("minf")[0];
            let stco = children(stbl.body())
                .expect("stbl")
                .into_iter()
                .find(|c| &c.kind == b"stco")
                .expect("stco");
            let s = Stco::decode(&mut { stco.bytes }).expect("stco");
            return s
                .entries
                .iter()
                .map(|o| file[*o as usize..*o as usize + 9].to_vec())
                .collect();
        }
        panic!("no track 1");
    }

    fn png() -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.extend_from_slice(b"not really a png");
        v
    }

    #[test]
    fn chapters_and_cover_round_trip() {
        let dir = std::env::temp_dir().join(format!("ffcine-mp4-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let src = dir.join("in.mp4");
        std::fs::write(&src, synthetic()).expect("write");
        assert_eq!(
            frames(&synthetic()),
            [b"FRAME-ONE".to_vec(), b"FRAME-TWO".to_vec()]
        );
        let tags = Tags {
            chapters: vec![
                Chapter {
                    start: 0.0,
                    title: "Opening".into(),
                },
                Chapter {
                    start: 1.25,
                    title: "Ünïcode".into(),
                },
                Chapter {
                    start: 9.0,
                    title: "past the end".into(),
                },
            ],
            cover: Some(png()),
        };
        let out = dir.join("out.mp4");
        write(&src, &out, &tags).expect("tag");
        let bytes = std::fs::read(&out).expect("read");
        let kinds: Vec<_> = children(&bytes)
            .expect("top")
            .iter()
            .map(|c| c.kind)
            .collect();
        assert_eq!(kinds, [*b"ftyp", *b"moov", *b"mdat", *b"mdat"]);
        assert_eq!(
            frames(&bytes),
            frames(&synthetic()),
            "chunk offsets follow the mdat"
        );
        let got = read(&out).expect("read tags");
        assert_eq!(got.cover, tags.cover);
        assert_eq!(got.chapters, tags.chapters[..2]);

        // Again over its own output: one chapter track, same offsets.
        write_in_place(&out, &tags).expect("retag");
        let again = std::fs::read(&out).expect("read");
        assert_eq!(frames(&again), frames(&synthetic()));
        let moov = children(&again).expect("top")[1];
        let traks = children(moov.body())
            .expect("moov")
            .into_iter()
            .filter(|c| &c.kind == b"trak")
            .count();
        assert_eq!(traks, 2);
        assert_eq!(read(&out).expect("tags"), got);

        // Clearing removes both.
        write_in_place(&out, &Tags::default()).expect("clear");
        assert_eq!(read(&out).expect("tags"), Tags::default());
        let cleared = std::fs::read(&out).expect("read");
        let moov = children(&cleared).expect("top")[1];
        assert_eq!(
            children(moov.body())
                .expect("moov")
                .iter()
                .filter(|c| &c.kind == b"trak")
                .count(),
            1
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn faststart_moves_moov_first() {
        let dir = std::env::temp_dir().join(format!("ffcine-mp4-fast-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("a.mp4");
        std::fs::write(&path, synthetic()).expect("write");
        faststart(&path).expect("faststart");
        let bytes = std::fs::read(&path).expect("read");
        let kinds: Vec<_> = children(&bytes)
            .expect("top")
            .iter()
            .map(|c| c.kind)
            .collect();
        assert_eq!(kinds, [*b"ftyp", *b"moov", *b"mdat"]);
        assert_eq!(frames(&bytes), frames(&synthetic()));
        assert_eq!(read(&path).expect("tags"), Tags::default());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_non_images() {
        let dir = std::env::temp_dir().join(format!("ffcine-mp4-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let src = dir.join("in.mp4");
        std::fs::write(&src, synthetic()).expect("write");
        let tags = Tags {
            cover: Some(b"GIF89a".to_vec()),
            ..Tags::default()
        };
        assert!(write(&src, &dir.join("out.mp4"), &tags).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn large_offsets_become_co64() {
        let v = offsets(vec![10, u64::from(u32::MAX) + 5]).expect("co64");
        assert_eq!(&v[4..8], b"co64");
        let v = offsets(vec![10]).expect("stco");
        assert_eq!(&v[4..8], b"stco");
    }
}
