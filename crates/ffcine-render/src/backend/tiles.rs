//! HEIF tile grids (iPhone HEICs are 512x512 HEVC tiles in a `grid` item): libavformat exposes
//! each tile as its own stream plus an `AV_STREAM_GROUP_PARAMS_TILE_GRID` stream group with the
//! tile offsets, the crop (`clap`) and the rotation (`irot`, as a display matrix). Decoding the
//! "best" video stream alone gives one tile; [`decode`] assembles the picture.

// Pixel and plane arithmetic on image-sized values.
#![allow(
    clippy::many_single_char_names,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

use std::os::raw::c_int;
use std::ptr;

use ffmpeg_next::ffi as ff;

/// A file's first tile grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TileGrid {
    /// Stream index and top-left offset in the coded canvas, per tile.
    pub tiles: Vec<(c_int, i32, i32)>,
    /// The visible rectangle of the canvas: x, y, width, height.
    pub crop: (i32, i32, i32, i32),
    /// Clockwise rotation to display it upright: 0, 90, 180 or 270.
    pub rotation: u32,
}

impl TileGrid {
    /// The first tile grid stream group of `fmt`, if any.
    ///
    /// # Safety
    /// `fmt` must be an opened format context (after `avformat_find_stream_info`).
    pub(crate) unsafe fn find(fmt: *const ff::AVFormatContext) -> Option<Self> {
        // SAFETY: libavformat keeps these arrays valid for the context's life.
        unsafe {
            for i in 0..(*fmt).nb_stream_groups as usize {
                let g = *(*fmt).stream_groups.add(i);
                if (*g).type_ != ff::AVStreamGroupParamsType::AV_STREAM_GROUP_PARAMS_TILE_GRID {
                    continue;
                }
                let t = (*g).params.tile_grid;
                let tiles = (0..(*t).nb_tiles as usize)
                    .map(|k| {
                        let o = *(*t).offsets.add(k);
                        let st = *(*g).streams.add(o.idx as usize);
                        ((*st).index, o.horizontal, o.vertical)
                    })
                    .collect();
                let sd = ff::av_packet_side_data_get(
                    (*t).coded_side_data,
                    (*t).nb_coded_side_data,
                    ff::AVPacketSideDataType::AV_PKT_DATA_DISPLAYMATRIX,
                );
                let rotation = if sd.is_null() || (*sd).size < 36 {
                    0
                } else {
                    clockwise(ff::av_display_rotation_get((*sd).data.cast::<i32>()))
                };
                return Some(Self {
                    tiles,
                    crop: (
                        (*t).horizontal_offset,
                        (*t).vertical_offset,
                        (*t).width,
                        (*t).height,
                    ),
                    rotation,
                });
            }
            None
        }
    }

    /// Displayed width and height (after the crop and rotation).
    pub(crate) fn size(&self) -> (u32, u32) {
        let (_, _, w, h) = self.crop;
        let (w, h) = (w.unsigned_abs(), h.unsigned_abs());
        if self.rotation % 180 == 90 {
            (h, w)
        } else {
            (w, h)
        }
    }

    pub(crate) fn has_stream(&self, index: c_int) -> bool {
        self.tiles.iter().any(|t| t.0 == index)
    }
}

/// HEIF image brands (`ftyp` major brand): files whose primary item is a still.
const HEIF_BRANDS: [&str; 6] = ["heic", "heix", "heim", "heis", "mif1", "avif"];

/// The clockwise rotation (`irot`, as the stream's display matrix) of a single-image (untiled)
/// HEIF item in `stream`; 0 for anything else, so videos keep ignoring their display matrices.
///
/// # Safety
/// `fmt` must be an opened format context (after `avformat_find_stream_info`).
pub(crate) unsafe fn item_rotation(fmt: *const ff::AVFormatContext, stream: c_int) -> u32 {
    // SAFETY: libavformat keeps the metadata and streams valid for the context's life.
    unsafe {
        let brand = ff::av_dict_get((*fmt).metadata, c"major_brand".as_ptr(), ptr::null(), 0);
        if brand.is_null() {
            return 0;
        }
        let brand = std::ffi::CStr::from_ptr((*brand).value).to_string_lossy();
        if !HEIF_BRANDS.contains(&brand.trim()) {
            return 0;
        }
        let Ok(i) = usize::try_from(stream) else {
            return 0;
        };
        if i >= (*fmt).nb_streams as usize {
            return 0;
        }
        let par = (*(*(*fmt).streams.add(i))).codecpar;
        let sd = ff::av_packet_side_data_get(
            (*par).coded_side_data,
            (*par).nb_coded_side_data,
            ff::AVPacketSideDataType::AV_PKT_DATA_DISPLAYMATRIX,
        );
        if sd.is_null() || (*sd).size < 36 {
            0
        } else {
            clockwise(ff::av_display_rotation_get((*sd).data.cast::<i32>()))
        }
    }
}

/// `av_display_rotation_get`'s counter-clockwise degrees as a clockwise quarter turn.
fn clockwise(ccw: f64) -> u32 {
    if ccw.is_nan() {
        return 0;
    }
    let q = (-ccw / 90.0).round() as i64;
    u32::try_from(q.rem_euclid(4) * 90).unwrap_or(0)
}

#[cfg(feature = "placebo")]
pub(crate) use assemble::{decode, rotate};

#[cfg(feature = "placebo")]
mod assemble {
    use std::os::raw::c_int;
    use std::ptr;

    use ffmpeg_next::ffi as ff;

    use super::TileGrid;
    use crate::Result;
    use crate::backend::av::{CodecCtx, Frame, check, gpu_err};

    const AVERROR_EOF: c_int = -0x2046_4F45;
    const EAGAIN: c_int = -11;

    /// Decodes every tile of `grid` (software decode) and returns the cropped, upright picture
    /// as one software frame (the tiles' pixel format, pts 0).
    ///
    /// # Safety
    /// `fmt` must be an opened format context nothing has read packets from yet.
    pub(crate) unsafe fn decode(fmt: *mut ff::AVFormatContext, grid: &TileGrid) -> Result<Frame> {
        if grid.tiles.is_empty() {
            return Err(gpu_err("tile grid: no tiles"));
        }
        let (_, _, cw, ch) = grid.crop;
        let fits = u32::try_from(cw)
            .ok()
            .zip(u32::try_from(ch).ok())
            .is_some_and(|(w, h)| crate::layer::still_fits(w, h));
        if !fits {
            return Err(gpu_err(format!(
                "tile grid: {cw}x{ch} px (at most {} px a side, {} in all)",
                crate::layer::STILL_SIDE,
                crate::layer::STILL_PIXELS
            )));
        }
        // SAFETY: standard decode sequence on valid contexts; frames and contexts are owned.
        unsafe {
            let mut decs = Vec::with_capacity(grid.tiles.len());
            for &(idx, _, _) in &grid.tiles {
                let st = *(*fmt).streams.add(usize::try_from(idx).unwrap_or(0));
                let codec = ff::avcodec_find_decoder((*(*st).codecpar).codec_id);
                if codec.is_null() {
                    return Err(gpu_err("tile grid: no decoder for the tile codec"));
                }
                let mut d = CodecCtx::new(codec)?;
                check(
                    ff::avcodec_parameters_to_context(d.get_mut(), (*st).codecpar),
                    "tile decoder params",
                )?;
                check(
                    ff::avcodec_open2(d.as_ptr(), codec, ptr::null_mut()),
                    "open tile decoder",
                )?;
                decs.push(d);
            }
            for i in 0..(*fmt).nb_streams as usize {
                let s = *(*fmt).streams.add(i);
                (*s).discard = if grid.has_stream((*s).index) {
                    ff::AVDiscard::AVDISCARD_DEFAULT
                } else {
                    ff::AVDiscard::AVDISCARD_ALL
                };
            }

            let mut frames: Vec<Option<Frame>> = grid.tiles.iter().map(|_| None).collect();
            let pkt = ff::av_packet_alloc();
            let mut read = || -> Result<()> {
                while frames.iter().any(Option::is_none) {
                    let r = ff::av_read_frame(fmt, pkt);
                    if r == AVERROR_EOF {
                        break;
                    }
                    check(r, "read tile packet")?;
                    let k = grid.tiles.iter().position(|t| t.0 == (*pkt).stream_index);
                    if let Some(k) = k.filter(|&k| frames[k].is_none()) {
                        let s = ff::avcodec_send_packet(decs[k].as_ptr(), pkt);
                        ff::av_packet_unref(pkt);
                        check(s, "send tile packet")?;
                        frames[k] = receive(&decs[k])?;
                    } else {
                        ff::av_packet_unref(pkt);
                    }
                }
                for (k, f) in frames.iter_mut().enumerate().filter(|(_, f)| f.is_none()) {
                    ff::avcodec_send_packet(decs[k].as_ptr(), ptr::null());
                    *f = receive(&decs[k])?;
                }
                Ok(())
            };
            let r = read();
            let mut pkt = pkt;
            ff::av_packet_free(&raw mut pkt);
            r?;
            let frames = frames
                .into_iter()
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| gpu_err("tile grid: a tile didn't decode"))?;
            compose(grid, &frames)
        }
    }

    /// One frame from `d`, or `None` if it wants more input.
    unsafe fn receive(d: &CodecCtx) -> Result<Option<Frame>> {
        let f = Frame::new()?;
        // SAFETY: valid decoder and frame.
        let r = unsafe { ff::avcodec_receive_frame(d.as_ptr(), f.as_ptr()) };
        if r == EAGAIN || r == AVERROR_EOF {
            return Ok(None);
        }
        check(r, "decode tile")?;
        Ok(Some(f))
    }

    /// One plane of a frame: its pixels as `px`-byte cells.
    struct Plane {
        data: *mut u8,
        stride: isize,
        w: usize,
        h: usize,
        px: usize,
        /// log2 subsampling.
        sx: u8,
        sy: u8,
    }

    impl Plane {
        /// Byte offset of pixel (x, y).
        fn at(&self, x: usize, y: usize) -> isize {
            y as isize * self.stride + (x * self.px) as isize
        }
    }

    /// The planes of `f` (a `w` x `h` software frame), with each plane's size and cell width.
    unsafe fn planes(f: &ff::AVFrame, w: usize, h: usize) -> Result<Vec<Plane>> {
        // SAFETY: `format` is a valid pixel format of a decoded frame.
        unsafe {
            let fmt = std::mem::transmute::<i32, ff::AVPixelFormat>(f.format);
            let d = ff::av_pix_fmt_desc_get(fmt);
            if d.is_null()
                || (*d).flags & (ff::AV_PIX_FMT_FLAG_HWACCEL | ff::AV_PIX_FMT_FLAG_BITSTREAM) as u64
                    != 0
            {
                return Err(gpu_err("tile grid: tiles aren't a software pixel format"));
            }
            let n = usize::try_from(ff::av_pix_fmt_count_planes(fmt)).unwrap_or(0);
            let d = &*d;
            let comps = &d.comp[..usize::from(d.nb_components)];
            let rgb = d.flags & ff::AV_PIX_FMT_FLAG_RGB as u64 != 0;
            (0..n)
                .map(|p| {
                    let c = comps
                        .iter()
                        .enumerate()
                        .find(|(_, c)| usize::try_from(c.plane).ok() == Some(p))
                        .ok_or_else(|| gpu_err("tile grid: odd pixel format"))?;
                    // Chroma components (1 and 2 of a YUV format) are subsampled.
                    let chroma = !rgb && (c.0 == 1 || c.0 == 2);
                    let (sx, sy) = if chroma {
                        (d.log2_chroma_w, d.log2_chroma_h)
                    } else {
                        (0, 0)
                    };
                    Ok(Plane {
                        data: f.data[p],
                        stride: f.linesize[p] as isize,
                        w: w.div_ceil(1 << sx),
                        h: h.div_ceil(1 << sy),
                        px: usize::try_from(c.1.step).unwrap_or(1),
                        sx,
                        sy,
                    })
                })
                .collect()
        }
    }

    /// A new software frame of `format`, `w` x `h`.
    fn alloc(format: c_int, w: usize, h: usize) -> Result<Frame> {
        let mut f = Frame::new()?;
        let fr = f.get_mut();
        fr.format = format;
        fr.width = c_int::try_from(w).unwrap_or(0);
        fr.height = c_int::try_from(h).unwrap_or(0);
        // SAFETY: valid frame with format and size set.
        check(
            unsafe { ff::av_frame_get_buffer(f.as_ptr(), 0) },
            "tile grid frame",
        )?;
        Ok(f)
    }

    /// Places `tiles` on the canvas, crops it to `grid.crop` and turns it upright.
    pub(super) fn compose(grid: &TileGrid, tiles: &[Frame]) -> Result<Frame> {
        let first = tiles
            .first()
            .ok_or_else(|| gpu_err("tile grid: no tiles"))?
            .get();
        if tiles.len() != grid.tiles.len() {
            return Err(gpu_err("tile grid: a tile is missing"));
        }
        let (cx, cy, cw, ch) = grid.crop;
        // The canvas the tiles cover: the crop must lie inside it.
        let (mut ew, mut eh) = (0i64, 0i64);
        for (t, &(_, tx, ty)) in tiles.iter().zip(&grid.tiles) {
            let tf = t.get();
            if tx < 0 || ty < 0 {
                return Err(gpu_err("tile grid: a tile is off the canvas"));
            }
            ew = ew.max(i64::from(tx) + i64::from(tf.width));
            eh = eh.max(i64::from(ty) + i64::from(tf.height));
        }
        let inside = cx >= 0
            && cy >= 0
            && cw > 0
            && ch > 0
            && i64::from(cx) + i64::from(cw) <= ew
            && i64::from(cy) + i64::from(ch) <= eh;
        let fits = crate::layer::still_fits(cw.unsigned_abs(), ch.unsigned_abs());
        if !inside || !fits {
            return Err(gpu_err(format!(
                "tile grid: bad crop {cw}x{ch}+{cx}+{cy} for {ew}x{eh} px of tiles"
            )));
        }
        let (cw, ch) = (cw as usize, ch as usize);
        let mut canvas = alloc(first.format, cw, ch)?;
        // SAFETY: plane pointers and sizes come from libav's own frames; every copy is
        // clipped to both the tile and the canvas.
        unsafe {
            let dst = planes(canvas.get(), cw, ch)?;
            for (t, &(_, tx, ty)) in tiles.iter().zip(&grid.tiles) {
                let tf = t.get();
                if tf.format != first.format {
                    return Err(gpu_err("tile grid: tiles differ in pixel format"));
                }
                let (tw, th) = (tf.width as usize, tf.height as usize);
                let src = planes(tf, tw, th)?;
                for (s, d) in src.iter().zip(&dst) {
                    // Tile origin relative to the crop, in this plane's samples.
                    let ox = (i64::from(tx) - i64::from(cx)) >> s.sx;
                    let oy = (i64::from(ty) - i64::from(cy)) >> s.sy;
                    let x0 = ox.max(0);
                    let x1 = (ox + s.w as i64).min(d.w as i64);
                    let y0 = oy.max(0);
                    let y1 = (oy + s.h as i64).min(d.h as i64);
                    if x0 >= x1 || y0 >= y1 {
                        continue;
                    }
                    let bytes = (x1 - x0) as usize * d.px;
                    for y in y0..y1 {
                        let sp = s.data.offset(s.at((x0 - ox) as usize, (y - oy) as usize));
                        let dp = d.data.offset(d.at(x0 as usize, y as usize));
                        ptr::copy_nonoverlapping(sp, dp, bytes);
                    }
                }
            }
            check(
                ff::av_frame_copy_props(canvas.as_ptr(), tiles[0].as_ptr()),
                "tile grid props",
            )?;
        }
        canvas.get_mut().pts = 0;
        if grid.rotation == 0 {
            return Ok(canvas);
        }
        rotate(&canvas, grid.rotation)
    }

    /// `f` (a software frame) turned `rotation` degrees clockwise (90, 180 or 270).
    pub(crate) fn rotate(f: &Frame, rotation: u32) -> Result<Frame> {
        let fr = f.get();
        let (w, h) = (
            usize::try_from(fr.width).unwrap_or(0),
            usize::try_from(fr.height).unwrap_or(0),
        );
        let (ow, oh) = if rotation == 180 { (w, h) } else { (h, w) };
        let out = alloc(fr.format, ow, oh)?;
        // SAFETY: as in `compose`; every index is within its plane.
        unsafe {
            let src = planes(fr, w, h)?;
            let dst = planes(out.get(), ow, oh)?;
            for (s, d) in src.iter().zip(&dst) {
                if rotation != 180 && (s.w, s.h) != (d.h, d.w) {
                    return Err(gpu_err(
                        "tile grid: can't turn a picture with unequal horizontal and vertical chroma subsampling",
                    ));
                }
                for oy in 0..d.h {
                    for ox in 0..d.w {
                        let (ix, iy) = match rotation {
                            90 => (oy, s.h - 1 - ox),
                            180 => (s.w - 1 - ox, s.h - 1 - oy),
                            _ => (s.w - 1 - oy, ox),
                        };
                        ptr::copy_nonoverlapping(
                            s.data.offset(s.at(ix, iy)),
                            d.data.offset(d.at(ox, oy)),
                            s.px,
                        );
                    }
                }
            }
            check(
                ff::av_frame_copy_props(out.as_ptr(), f.as_ptr()),
                "tile grid props",
            )?;
        }
        Ok(out)
    }
}

#[cfg(all(test, feature = "placebo"))]
mod tests {
    use std::path::Path;
    use std::ptr;

    use ffmpeg_next::ffi as ff;

    use super::*;
    use crate::backend::av::{Frame, cstr};

    fn open(name: &str) -> *mut ff::AVFormatContext {
        let p = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("testdata/heic")
            .join(name);
        let p = cstr(&p.to_string_lossy()).expect("path");
        let mut fmt = ptr::null_mut();
        // SAFETY: plain open; closed by the caller.
        unsafe {
            assert!(
                ff::avformat_open_input(&raw mut fmt, p.as_ptr(), ptr::null(), ptr::null_mut())
                    >= 0
            );
            assert!(ff::avformat_find_stream_info(fmt, ptr::null_mut()) >= 0);
        }
        fmt
    }

    fn grid(name: &str) -> (TileGrid, Frame) {
        let mut fmt = open(name);
        // SAFETY: freshly opened context.
        unsafe {
            let g = TileGrid::find(fmt).expect("a tile grid");
            let f = decode(fmt, &g).expect("decode");
            ff::avformat_close_input(&raw mut fmt);
            (g, f)
        }
    }

    /// Luma of the single-image (untiled) encode of the same picture.
    fn whole_luma() -> (usize, usize, Vec<u8>) {
        let mut fmt = open("whole.heic");
        // SAFETY: decode one frame of stream 0 in software.
        unsafe {
            assert!(TileGrid::find(fmt).is_none());
            let st = *(*fmt).streams;
            let codec = ff::avcodec_find_decoder((*(*st).codecpar).codec_id);
            let d = ff::avcodec_alloc_context3(codec);
            ff::avcodec_parameters_to_context(d, (*st).codecpar);
            assert!(ff::avcodec_open2(d, codec, ptr::null_mut()) >= 0);
            let pkt = ff::av_packet_alloc();
            assert!(ff::av_read_frame(fmt, pkt) >= 0);
            ff::avcodec_send_packet(d, pkt);
            ff::avcodec_send_packet(d, ptr::null());
            let f = Frame::new().expect("frame");
            assert!(ff::avcodec_receive_frame(d, f.as_ptr()) >= 0);
            let out = luma(&f);
            let mut pkt = pkt;
            let mut d = d;
            ff::av_packet_free(&raw mut pkt);
            ff::avcodec_free_context(&raw mut d);
            ff::avformat_close_input(&raw mut fmt);
            out
        }
    }

    fn luma(f: &Frame) -> (usize, usize, Vec<u8>) {
        let fr = f.get();
        let (w, h) = (fr.width as usize, fr.height as usize);
        let mut v = Vec::with_capacity(w * h);
        for y in 0..h {
            // SAFETY: an 8-bit luma row of `w` bytes.
            let row = unsafe {
                std::slice::from_raw_parts(
                    fr.data[0].offset(y as isize * fr.linesize[0] as isize),
                    w,
                )
            };
            v.extend_from_slice(row);
        }
        (w, h, v)
    }

    #[test]
    fn rotation_from_display_matrix() {
        assert_eq!(
            [0.0, -90.0, 90.0, 180.0, -180.0, 270.0].map(clockwise),
            [0, 90, 270, 180, 180, 90]
        );
    }

    #[test]
    fn assembles_tiles_with_crop() {
        // 200x120 cut into 64x64 tiles: a 4x2 grid on a 256x128 canvas, cropped.
        let (g, f) = grid("grid.heic");
        assert_eq!(g.tiles.len(), 8);
        assert_eq!(
            (g.crop, g.rotation, g.size()),
            ((0, 0, 200, 120), 0, (200, 120))
        );
        let (w, h, y) = luma(&f);
        assert_eq!((w, h), (200, 120));
        // Same picture as the untiled encode, up to coding noise; a misplaced tile is far off.
        let (ww, wh, wy) = whole_luma();
        assert_eq!((ww, wh), (w, h));
        let mad = y
            .iter()
            .zip(&wy)
            .map(|(a, b)| u64::from(a.abs_diff(*b)))
            .sum::<u64>()
            / y.len() as u64;
        assert!(mad <= 3, "mean abs luma difference {mad}");
    }

    #[test]
    fn turns_upright() {
        // Same tiles with `irot` (90 degrees clockwise to display).
        let (_, f) = grid("grid.heic");
        let (g, r) = grid("grid-rot.heic");
        assert_eq!((g.rotation, g.size()), (90, (120, 200)));
        let (w, h, y) = luma(&f);
        let (rw, rh, ry) = luma(&r);
        assert_eq!((rw, rh), (h, w));
        for oy in 0..rh {
            for ox in 0..rw {
                assert_eq!(ry[oy * rw + ox], y[(h - 1 - ox) * w + oy], "({ox}, {oy})");
            }
        }
        // Chroma is turned too.
        let (fc, rc) = (f.get(), r.get());
        // SAFETY: 4:2:0 chroma planes, 100x60 and 60x100.
        unsafe {
            let at = |fr: &ff::AVFrame, x: usize, y: usize| {
                *fr.data[1].offset(y as isize * fr.linesize[1] as isize + x as isize)
            };
            assert_eq!(at(rc, 0, 0), at(fc, 0, 59));
            assert_eq!(at(rc, 59, 99), at(fc, 99, 0));
        }
    }

    #[test]
    fn single_items_turn_by_their_display_matrix() {
        for (name, want) in [("whole.heic", 0), ("whole-rot.heic", 90)] {
            let mut fmt = open(name);
            // SAFETY: freshly opened context, closed here.
            unsafe {
                assert!(TileGrid::find(fmt).is_none());
                assert_eq!(item_rotation(fmt, 0), want, "{name}");
                assert_eq!(item_rotation(fmt, 7), 0, "{name}: no such stream");
                ff::avformat_close_input(&raw mut fmt);
            }
        }
        // Probed at the displayed size.
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/heic/whole-rot.heic");
        let ff = crate::backend::Ffmpeg::new().expect("backend");
        let info = crate::Backend::probe(&ff, &p).expect("probe");
        let (_, v) = info.video().expect("video");
        assert_eq!((v.width, v.height), (120, 200));
    }

    #[test]
    fn bad_grids_are_errors() {
        // No tiles, a crop past the tiles, a crop over the still cap: errors, not panics.
        let empty = TileGrid {
            tiles: Vec::new(),
            crop: (0, 0, 10, 10),
            rotation: 0,
        };
        assert!(super::assemble::compose(&empty, &[]).is_err());
        for crop in [(0, 0, 300, 120), (100, 0, 200, 120), (0, 0, 20000, 20000)] {
            let mut fmt = open("grid.heic");
            // SAFETY: freshly opened context, closed here.
            unsafe {
                let mut g = TileGrid::find(fmt).expect("a tile grid");
                g.crop = crop;
                assert!(decode(fmt, &g).is_err(), "{crop:?}");
                ff::avformat_close_input(&raw mut fmt);
            }
        }
    }
}
