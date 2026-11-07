//! A small ISO BMFF (MP4 / QuickTime MOV / CR3) metadata reader, without libav: the header
//! fields `ffcine catalog` needs (duration, tracks, codecs, frame rate, bit depth, chroma, `colr`
//! tags, maker atoms, Apple `mdta` keys, the first DJI `CAM meta` sample). Seeks over `mdat`, so
//! it reads a few hundred KB of a multi-GB clip, and runs anywhere `std` does (macOS offload).

#![allow(
    clippy::many_single_char_names,
    clippy::similar_names,
    clippy::cast_possible_truncation,
    clippy::doc_markdown
)] // binary parsing; box sizes are small; QuickTime, BMFF are names

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use crate::{Error, Rational, Result};

/// Seconds from 1904-01-01 (QuickTime epoch) to 1970-01-01.
const QT_EPOCH_OFFSET: i64 = 2_082_844_800;
/// Boxes larger than this are skipped rather than read into memory (cover art, thumbnails).
const MAX_READ: u64 = 4 << 20;

/// `colr` box contents (ITU-T H.273 code points).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colr {
    pub primaries: u16,
    pub transfer: u16,
    pub matrix: u16,
    /// `nclx` full-range flag; `None` for QuickTime `nclc`, which has none.
    pub full_range: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Track {
    /// `hdlr` type: `vide`, `soun`, `meta`, `tmcd`, ...
    pub handler: String,
    /// `hdlr` name (`CAM meta` on DJI).
    pub handler_name: String,
    /// Sample entry fourcc: `hvc1`, `avc1`, `apch`, `mp4a`, `djmd`, ...
    pub codec: String,
    pub duration: Option<f64>,
    pub width: u32,
    pub height: u32,
    pub frame_rate: Option<Rational>,
    pub samples: u64,
    pub bit_depth: Option<u8>,
    /// `chroma_format_idc`: 0 = 4:0:0, 1 = 4:2:0, 2 = 4:2:2, 3 = 4:4:4.
    pub chroma: Option<u8>,
    pub colr: Option<Colr>,
    /// A `dvcC`/`dvvC` Dolby Vision configuration box in the sample entry.
    pub dolby_vision: bool,
    pub sample_rate: Option<u32>,
    pub channels: Option<u16>,
    /// First sample's file offset and size (used for DJI's `CAM meta` header).
    pub first_sample: Option<(u64, u32)>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Mp4Info {
    /// `ftyp` major brand (`isom`, `qt  `, `crx `, ...).
    pub brand: String,
    /// `mvhd` creation time, Unix seconds. UTC on DJI and iPhone, local wall time on Canon.
    pub creation_time: Option<i64>,
    pub duration: Option<f64>,
    pub tracks: Vec<Track>,
    /// Text atoms from `moov/udta` (and `udta/meta/ilst`), keyed by fourcc: `©too`, `modl`,
    /// `manu`, `©mak`, `©mod`, ...
    pub udta: BTreeMap<String, String>,
    /// QuickTime `mdta` keys (`com.apple.quicktime.model`, `...creationdate`, ...).
    pub keys: BTreeMap<String, String>,
    /// Canon `CMT1` (TIFF IFD0) fields: `Make`, `Model`, `DateTime`.
    pub canon: BTreeMap<String, String>,
    /// First sample of DJI's `djmd` (`CAM meta`) track: a protobuf clip header.
    pub dji_meta: Option<Vec<u8>>,
}

impl Mp4Info {
    /// The first video track.
    pub fn video(&self) -> Option<&Track> {
        self.tracks.iter().find(|t| t.handler == "vide")
    }

    /// The first audio track.
    pub fn audio(&self) -> Option<&Track> {
        self.tracks.iter().find(|t| t.handler == "soun")
    }

    /// Camera make and model from whichever atoms the vendor writes.
    pub fn model(&self) -> Option<&str> {
        ["com.apple.quicktime.model"]
            .iter()
            .find_map(|k| self.keys.get(*k))
            .or_else(|| self.canon.get("Model"))
            .or_else(|| {
                ["modl", "©mod", "©too"]
                    .iter()
                    .find_map(|k| self.udta.get(*k))
            })
            .map(String::as_str)
    }
}

struct Reader {
    f: File,
    len: u64,
}

#[derive(Debug, Clone, Copy)]
struct BoxHdr {
    kind: [u8; 4],
    /// Payload start and end.
    start: u64,
    end: u64,
}

impl Reader {
    fn read_at(&mut self, pos: u64, n: usize) -> io::Result<Vec<u8>> {
        self.f.seek(SeekFrom::Start(pos))?;
        let mut buf = vec![0; n];
        self.f.read_exact(&mut buf)?;
        Ok(buf)
    }

    fn payload(&mut self, b: BoxHdr) -> io::Result<Vec<u8>> {
        let n = b.end - b.start;
        if n > MAX_READ {
            return Err(invalid("box too large", &fourcc(b.kind)));
        }
        #[allow(clippy::cast_possible_truncation)] // < MAX_READ
        self.read_at(b.start, n as usize)
    }

    /// Child boxes of `[start, end)`.
    fn children(&mut self, start: u64, end: u64) -> io::Result<Vec<BoxHdr>> {
        let mut out = Vec::new();
        let mut off = start;
        while off + 8 <= end {
            let h = self.read_at(off, 8)?;
            let mut size = u64::from(be32(&h[0..4]));
            let kind = [h[4], h[5], h[6], h[7]];
            let mut hdr = 8;
            if size == 1 {
                size = be64(&self.read_at(off + 8, 8)?);
                hdr = 16;
            } else if size == 0 {
                size = end - off;
            }
            if size < hdr || off + size > end {
                // Truncated tail (a copy that died mid-file): keep what parsed.
                break;
            }
            if &kind == b"uuid" {
                hdr += 16;
            }
            out.push(BoxHdr {
                kind,
                start: off + hdr,
                end: off + size,
            });
            off += size;
        }
        Ok(out)
    }
}

fn invalid(what: &'static str, value: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("{what} {value}"))
}

fn fourcc(k: [u8; 4]) -> String {
    k.iter().map(|&b| char::from(b)).collect()
}

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}
fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}
fn be64(b: &[u8]) -> u64 {
    u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

fn find(boxes: &[BoxHdr], kind: [u8; 4]) -> Option<BoxHdr> {
    boxes.iter().copied().find(|b| b.kind == kind)
}

/// Reads the metadata of an ISO BMFF file. Errors only if it isn't one (no `ftyp`/`moov`).
pub fn read(path: &Path) -> Result<Mp4Info> {
    read_io(path).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })
}

fn read_io(path: &Path) -> io::Result<Mp4Info> {
    let f = File::open(path)?;
    let len = f.metadata()?.len();
    let mut r = Reader { f, len };
    let top = r.children(0, r.len)?;
    let mut info = Mp4Info::default();
    if let Some(ftyp) = find(&top, *b"ftyp") {
        let p = r.read_at(ftyp.start, 4)?;
        info.brand = fourcc([p[0], p[1], p[2], p[3]]);
    }
    let moov = find(&top, *b"moov").ok_or_else(|| invalid("MP4/MOV (no moov box)", ""))?;
    let kids = r.children(moov.start, moov.end)?;
    for b in &kids {
        match &b.kind {
            b"mvhd" => mvhd(&mut r, *b, &mut info)?,
            b"trak" => {
                if let Some(t) = trak(&mut r, *b)? {
                    info.tracks.push(t);
                }
            }
            b"udta" => udta(&mut r, *b, &mut info)?,
            b"meta" => meta(&mut r, *b, &mut info)?,
            b"uuid" => canon_uuid(&mut r, *b, &mut info)?,
            _ => {}
        }
    }
    if let Some(t) = info.tracks.iter().find(|t| t.codec == "djmd")
        && let Some((off, size)) = t.first_sample
        && u64::from(size) <= MAX_READ
        && off + u64::from(size) <= r.len
    {
        info.dji_meta = Some(r.read_at(off, size as usize)?);
    }
    Ok(info)
}

#[allow(clippy::cast_precision_loss, clippy::cast_possible_wrap)]
fn mvhd(r: &mut Reader, b: BoxHdr, info: &mut Mp4Info) -> io::Result<()> {
    let p = r.payload(b)?;
    let (created, scale, dur) = if p.first() == Some(&1) && p.len() >= 32 {
        (be64(&p[4..12]), be32(&p[20..24]), be64(&p[24..32]))
    } else if p.len() >= 20 {
        (
            u64::from(be32(&p[4..8])),
            be32(&p[12..16]),
            u64::from(be32(&p[16..20])),
        )
    } else {
        return Ok(());
    };
    if created != 0 {
        info.creation_time = Some(created as i64 - QT_EPOCH_OFFSET);
    }
    if scale != 0 {
        info.duration = Some(dur as f64 / f64::from(scale));
    }
    Ok(())
}

#[allow(clippy::cast_precision_loss)]
fn trak(r: &mut Reader, b: BoxHdr) -> io::Result<Option<Track>> {
    let kids = r.children(b.start, b.end)?;
    let Some(mdia) = find(&kids, *b"mdia") else {
        return Ok(None);
    };
    let mk = r.children(mdia.start, mdia.end)?;
    let mut t = Track::default();
    let mut timescale = 0;
    if let Some(h) = find(&mk, *b"mdhd") {
        let p = r.payload(h)?;
        let (scale, dur) = if p.first() == Some(&1) && p.len() >= 32 {
            (be32(&p[20..24]), be64(&p[24..32]))
        } else if p.len() >= 20 {
            (be32(&p[12..16]), u64::from(be32(&p[16..20])))
        } else {
            (0, 0)
        };
        timescale = scale;
        if scale != 0 {
            t.duration = Some(dur as f64 / f64::from(scale));
        }
    }
    if let Some(h) = find(&mk, *b"hdlr") {
        let p = r.payload(h)?;
        if p.len() >= 12 {
            t.handler = fourcc([p[8], p[9], p[10], p[11]]);
        }
        if p.len() > 24 {
            // ISO: NUL-terminated; QuickTime: Pascal string. Keep printable bytes.
            t.handler_name.clone_from(
                &p[24..]
                    .iter()
                    .filter(|c| c.is_ascii_graphic() || **c == b' ')
                    .map(|&c| char::from(c))
                    .collect::<String>()
                    .trim()
                    .to_owned(),
            );
        }
    }
    let Some(minf) = find(&mk, *b"minf") else {
        return Ok(Some(t));
    };
    let mi = r.children(minf.start, minf.end)?;
    let Some(stbl) = find(&mi, *b"stbl") else {
        return Ok(Some(t));
    };
    let st = r.children(stbl.start, stbl.end)?;
    if let Some(sd) = find(&st, *b"stsd") {
        stsd(r, sd, &mut t);
    }
    if let Some(b) = find(&st, *b"stts") {
        stts(r, b, timescale, &mut t)?;
    }
    let first_size = match find(&st, *b"stsz") {
        Some(b) => {
            let p = r.read_at(b.start, 16.min((b.end - b.start) as usize))?;
            if p.len() >= 12 {
                let fixed = be32(&p[4..8]);
                if t.samples == 0 {
                    t.samples = u64::from(be32(&p[8..12]));
                }
                if fixed != 0 {
                    Some(fixed)
                } else if p.len() >= 16 {
                    Some(be32(&p[12..16]))
                } else {
                    None
                }
            } else {
                None
            }
        }
        None => None,
    };
    let first_off = if let Some(b) = find(&st, *b"stco") {
        let p = r.read_at(b.start, 12.min((b.end - b.start) as usize))?;
        (p.len() >= 12 && be32(&p[4..8]) > 0).then(|| u64::from(be32(&p[8..12])))
    } else if let Some(b) = find(&st, *b"co64") {
        let p = r.read_at(b.start, 16.min((b.end - b.start) as usize))?;
        (p.len() >= 16 && be32(&p[4..8]) > 0).then(|| be64(&p[8..16]))
    } else {
        None
    };
    if let (Some(o), Some(s)) = (first_off, first_size) {
        t.first_sample = Some((o, s));
    }
    Ok(Some(t))
}

fn stts(r: &mut Reader, b: BoxHdr, timescale: u32, t: &mut Track) -> io::Result<()> {
    let p = r.payload(b)?;
    if p.len() < 8 || timescale == 0 {
        return Ok(());
    }
    let n = be32(&p[4..8]) as usize;
    let (mut count, mut total) = (0u64, 0u64);
    // The dominant (longest) run sets the rate; a short last entry is common.
    let mut best = (0u64, 0u32);
    for e in p[8..].as_chunks::<8>().0.iter().take(n) {
        let (c, d) = (u64::from(be32(&e[0..4])), be32(&e[4..8]));
        count += c;
        total += c * u64::from(d);
        if c > best.0 {
            best = (c, d);
        }
    }
    t.samples = count;
    if t.handler == "vide" && best.1 != 0 {
        let g = gcd(timescale, best.1);
        if let (Ok(num), Ok(den)) = (i32::try_from(timescale / g), i32::try_from(best.1 / g)) {
            t.frame_rate = Some(Rational { num, den });
        }
    }
    let _ = total;
    Ok(())
}

fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a.max(1)
}

fn stsd(r: &mut Reader, b: BoxHdr, t: &mut Track) {
    let Ok(p) = r.payload(b) else {
        return;
    };
    if p.len() < 16 {
        return;
    }
    let entry = &p[8..];
    let size = (be32(&entry[0..4]) as usize).min(entry.len());
    t.codec = fourcc([entry[4], entry[5], entry[6], entry[7]]);
    let e = &entry[..size];
    match t.handler.as_str() {
        "vide" if e.len() >= 86 => {
            t.width = u32::from(be16(&e[32..34]));
            t.height = u32::from(be16(&e[34..36]));
            let depth = be16(&e[82..84]);
            sample_entry_boxes(&e[86..], t);
            if t.bit_depth.is_none() {
                t.bit_depth = prores_depth(&t.codec).or(match depth {
                    // QuickTime `depth` is bits per pixel (24 = 8-bit RGB); only trust 8-bit.
                    24 => Some(8),
                    _ => None,
                });
            }
            if t.chroma.is_none() {
                t.chroma = prores_chroma(&t.codec);
            }
        }
        "soun" if e.len() >= 28 => {
            t.channels = Some(be16(&e[16..18]));
            t.sample_rate = Some(be32(&e[24..28]) >> 16);
        }
        _ => {}
    }
}

fn prores_depth(codec: &str) -> Option<u8> {
    match codec {
        "apch" | "apcn" | "apcs" | "apco" => Some(10),
        "ap4h" | "ap4x" => Some(12),
        _ => None,
    }
}

fn prores_chroma(codec: &str) -> Option<u8> {
    match codec {
        "apch" | "apcn" | "apcs" | "apco" => Some(2),
        "ap4h" | "ap4x" => Some(3),
        _ => None,
    }
}

/// Boxes inside a visual sample entry: `hvcC`, `avcC`, `colr`, `dvcC`, ...
fn sample_entry_boxes(mut b: &[u8], t: &mut Track) {
    while b.len() >= 8 {
        let size = be32(&b[0..4]) as usize;
        if size < 8 || size > b.len() {
            break;
        }
        let (kind, body) = (&b[4..8], &b[8..size]);
        match kind {
            b"hvcC" if body.len() >= 19 => {
                t.chroma = Some(body[16] & 3);
                t.bit_depth = Some((body[17] & 7) + 8);
            }
            b"avcC" if body.len() >= 4 => avcc(body, t),
            b"colr" if body.len() >= 10 => {
                let full =
                    (&body[0..4] == b"nclx" && body.len() >= 11).then(|| body[10] & 0x80 != 0);
                if &body[0..4] == b"nclx" || &body[0..4] == b"nclc" {
                    t.colr = Some(Colr {
                        primaries: be16(&body[4..6]),
                        transfer: be16(&body[6..8]),
                        matrix: be16(&body[8..10]),
                        full_range: full,
                    });
                }
            }
            b"dvcC" | b"dvvC" | b"dvwC" => t.dolby_vision = true,
            _ => {}
        }
        b = &b[size..];
    }
}

/// `avcC`: High profiles carry chroma/bit depth after the parameter sets; others are 8-bit 4:2:0.
fn avcc(body: &[u8], t: &mut Track) {
    let profile = body[1];
    t.chroma = Some(1);
    t.bit_depth = Some(8);
    if !matches!(profile, 100 | 110 | 122 | 144 | 244) {
        return;
    }
    let mut i = 5;
    let Some(&nsps) = body.get(i) else { return };
    i += 1;
    for _ in 0..(nsps & 0x1f) {
        let Some(l) = body.get(i..i + 2) else { return };
        i += 2 + be16(l) as usize;
    }
    let Some(&npps) = body.get(i) else { return };
    i += 1;
    for _ in 0..npps {
        let Some(l) = body.get(i..i + 2) else { return };
        i += 2 + be16(l) as usize;
    }
    if let Some(ext) = body.get(i..i + 3) {
        t.chroma = Some(ext[0] & 3);
        t.bit_depth = Some((ext[1] & 7) + 8);
    } else if profile == 110 {
        t.bit_depth = Some(10);
    } else if profile == 122 {
        (t.chroma, t.bit_depth) = (Some(2), Some(10));
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b)
        .trim_matches(|c: char| c == '\0' || c.is_whitespace())
        .to_owned()
}

fn udta(r: &mut Reader, b: BoxHdr, info: &mut Mp4Info) -> io::Result<()> {
    for c in r.children(b.start, b.end)? {
        let key = fourcc(c.kind);
        if &c.kind == b"meta" {
            meta(r, c, info)?;
            continue;
        }
        if c.end - c.start > 1024 {
            continue;
        }
        let p = r.payload(c)?;
        let value = if key.starts_with('©') && p.len() >= 4 {
            // QuickTime international text: u16 length, u16 language, text.
            let n = (be16(&p[0..2]) as usize).min(p.len() - 4);
            text(&p[4..4 + n])
        } else if matches!(&c.kind, b"manu" | b"modl") && p.len() > 6 {
            // Canon: u32 version/flags, u16 language, text.
            text(&p[6..])
        } else if p
            .iter()
            .all(|&ch| ch == 0 || ch.is_ascii_graphic() || ch == b' ' || ch == b'=' || ch == b';')
        {
            text(&p)
        } else {
            continue;
        };
        if !value.is_empty() {
            info.udta.insert(key, value);
        }
    }
    Ok(())
}

/// `meta` box: either ISO (full box, then `hdlr`) or QuickTime (no version/flags).
fn meta(r: &mut Reader, b: BoxHdr, info: &mut Mp4Info) -> io::Result<()> {
    let head = r.read_at(b.start, 8.min((b.end - b.start) as usize))?;
    let start = if head.len() >= 8 && &head[4..8] == b"hdlr" {
        b.start
    } else {
        b.start + 4
    };
    let kids = r.children(start, b.end)?;
    let mut keys: Vec<String> = Vec::new();
    if let Some(k) = find(&kids, *b"keys") {
        let p = r.payload(k)?;
        let mut i = 8;
        while i + 8 <= p.len() {
            let size = be32(&p[i..i + 4]) as usize;
            if size < 8 || i + size > p.len() {
                break;
            }
            keys.push(text(&p[i + 8..i + size]));
            i += size;
        }
    }
    let Some(ilst) = find(&kids, *b"ilst") else {
        return Ok(());
    };
    for item in r.children(ilst.start, ilst.end)? {
        if item.end - item.start > 64 * 1024 {
            continue;
        }
        let p = r.payload(item)?;
        // `data` box: size, 'data', u32 type, u32 locale, value.
        if p.len() < 16 || &p[4..8] != b"data" {
            continue;
        }
        let dsize = (be32(&p[0..4]) as usize).min(p.len());
        let kind = be32(&p[8..12]) & 0x00ff_ffff;
        if kind != 1 {
            continue; // UTF-8 only
        }
        let value = text(&p[16..dsize]);
        let idx = be32(&[item.kind[0], item.kind[1], item.kind[2], item.kind[3]]) as usize;
        if !keys.is_empty() && idx >= 1 && idx <= keys.len() {
            info.keys.insert(keys[idx - 1].clone(), value);
        } else {
            info.udta.insert(fourcc(item.kind), value);
        }
    }
    Ok(())
}

/// Canon's `uuid` box (`85c0b687-820f-11e0-8111-f4ce462b6a48`) holds `CMT1`, a TIFF IFD0.
fn canon_uuid(r: &mut Reader, b: BoxHdr, info: &mut Mp4Info) -> io::Result<()> {
    const CANON: [u8; 16] = [
        0x85, 0xc0, 0xb6, 0x87, 0x82, 0x0f, 0x11, 0xe0, 0x81, 0x11, 0xf4, 0xce, 0x46, 0x2b, 0x6a,
        0x48,
    ];
    if r.read_at(b.start - 16, 16)? != CANON {
        return Ok(());
    }
    for c in r.children(b.start, b.end)? {
        if &c.kind == b"CMT1"
            && let Ok(p) = r.payload(c)
        {
            for (tag, name) in [(0x010f, "Make"), (0x0110, "Model"), (0x0132, "DateTime")] {
                if let Some(v) = tiff_ascii(&p, tag) {
                    info.canon.insert(name.to_owned(), v);
                }
            }
        }
    }
    Ok(())
}

/// An ASCII tag from IFD0 of a TIFF blob.
pub fn tiff_ascii(p: &[u8], tag: u16) -> Option<String> {
    let le = match p.get(0..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16_at = |i: usize| -> Option<u16> {
        let b = p.get(i..i + 2)?;
        Some(if le {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            u16::from_be_bytes([b[0], b[1]])
        })
    };
    let u32_at = |i: usize| -> Option<u32> {
        let b = p.get(i..i + 4)?;
        Some(if le {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        } else {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]])
        })
    };
    let ifd = u32_at(4)? as usize;
    let n = u16_at(ifd)? as usize;
    for i in 0..n {
        let e = ifd + 2 + i * 12;
        if u16_at(e)? != tag || u16_at(e + 2)? != 2 {
            continue;
        }
        let count = u32_at(e + 4)? as usize;
        let data = if count <= 4 {
            p.get(e + 8..e + 8 + count)?
        } else {
            let off = u32_at(e + 8)? as usize;
            p.get(off..off + count)?
        };
        return Some(text(data));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiff_ifd0_ascii() {
        // Little-endian TIFF, one entry: Model = "EOS R50" (8 bytes incl. NUL, out of line).
        let mut t = b"II*\0".to_vec();
        t.extend_from_slice(&8u32.to_le_bytes());
        t.extend_from_slice(&1u16.to_le_bytes());
        t.extend_from_slice(&0x0110u16.to_le_bytes());
        t.extend_from_slice(&2u16.to_le_bytes());
        t.extend_from_slice(&8u32.to_le_bytes());
        t.extend_from_slice(&26u32.to_le_bytes());
        t.extend_from_slice(&0u32.to_le_bytes());
        t.extend_from_slice(b"EOS R50\0");
        assert_eq!(tiff_ascii(&t, 0x0110).as_deref(), Some("EOS R50"));
        assert_eq!(tiff_ascii(&t, 0x010f), None);
    }
}

/// Real camera files under `FFCINE_SAMPLE_DIR`, when set (skipped otherwise).
#[cfg(test)]
mod samples {
    use std::path::{Path, PathBuf};

    use crate::profile::{self, PictureProfile};

    fn sample(rel: &str) -> Option<PathBuf> {
        let p = Path::new(&std::env::var_os("FFCINE_SAMPLE_DIR")?).join(rel);
        p.exists().then_some(p)
    }

    #[test]
    fn osmo_pocket_4_dlog_and_normal() {
        for (path, want) in [
            (
                "pocket4-dlog/DJI_20260704112401_0001_D.MP4",
                PictureProfile::Dlog,
            ),
            (
                "pocket4-normal/DCIM/DJI_001/DJI_20260605164744_0001_D.MP4",
                PictureProfile::Rec709,
            ),
        ] {
            let Some(p) = sample(path) else { continue };
            let info = super::read(&p).unwrap();
            let v = info.video().unwrap();
            assert_eq!((v.codec.as_str(), v.width, v.height), ("hvc1", 3840, 2160));
            assert_eq!((v.bit_depth, v.chroma), (Some(10), Some(1)));
            assert!(v.frame_rate.is_some());
            assert_eq!(profile::detect(&info).profile, want, "{path}");
        }
    }

    #[test]
    fn canon_r10_mp4() {
        let Some(p) = sample("r10/MVI_2884.MP4") else {
            return;
        };
        let info = super::read(&p).unwrap();
        assert_eq!(info.model(), Some("Canon EOS R10"));
        assert_eq!(info.video().unwrap().bit_depth, Some(8));
        assert_eq!(profile::detect(&info).profile, PictureProfile::Rec709);
    }
}
