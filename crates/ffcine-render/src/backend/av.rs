//! Thin RAII wrappers over the raw libav* objects the GPU path needs (hw device/frames contexts,
//! frames, codec contexts). `ffmpeg-next` covers demux/mux; these cover what it doesn't expose.

use std::ffi::{CStr, CString};
use std::os::raw::c_int;
use std::ptr::{self, NonNull};

use ffmpeg_next::ffi as ff;

use crate::{Error, Result};

/// Turns a negative libav return code into an error naming `what` failed.
pub(crate) fn check(ret: c_int, what: &str) -> Result<c_int> {
    if ret < 0 {
        Err(Error::Pipeline(format!(
            "{what}: {}",
            ffmpeg_next::Error::from(ret)
        )))
    } else {
        Ok(ret)
    }
}

pub(crate) fn gpu_err(msg: impl Into<String>) -> Error {
    Error::Pipeline(msg.into())
}

/// An owned `AVBufferRef` (device or frames context, pool buffer).
#[derive(Debug)]
pub(crate) struct BufferRef(NonNull<ff::AVBufferRef>);

// SAFETY: AVBufferRef refcounting is atomic; the contexts behind them are used from one thread at
// a time by the pipeline.
unsafe impl Send for BufferRef {}
unsafe impl Sync for BufferRef {}

impl BufferRef {
    /// Takes ownership of `p` (null is an error).
    pub(crate) unsafe fn from_raw(p: *mut ff::AVBufferRef, what: &str) -> Result<Self> {
        NonNull::new(p)
            .map(Self)
            .ok_or_else(|| gpu_err(format!("{what}: out of memory")))
    }

    pub(crate) fn as_ptr(&self) -> *mut ff::AVBufferRef {
        self.0.as_ptr()
    }

    /// A new reference, owned by the caller (e.g. to store in a codec context).
    pub(crate) fn new_ref(&self) -> *mut ff::AVBufferRef {
        // SAFETY: valid buffer.
        unsafe { ff::av_buffer_ref(self.0.as_ptr()) }
    }

    /// `data` cast to `T` (e.g. `AVHWDeviceContext`, `AVHWFramesContext`).
    pub(crate) fn data<T>(&self) -> *mut T {
        // SAFETY: valid buffer.
        unsafe { (*self.0.as_ptr()).data.cast() }
    }
}

impl Clone for BufferRef {
    fn clone(&self) -> Self {
        Self(NonNull::new(self.new_ref()).expect("av_buffer_ref"))
    }
}

impl Drop for BufferRef {
    fn drop(&mut self) {
        let mut p = self.0.as_ptr();
        // SAFETY: we own one reference.
        unsafe { ff::av_buffer_unref(&raw mut p) };
    }
}

/// An owned `AVFrame`.
#[derive(Debug)]
pub(crate) struct Frame(NonNull<ff::AVFrame>);

// SAFETY: frames are refcounted and handed between pipeline threads, never shared.
unsafe impl Send for Frame {}

impl Frame {
    pub(crate) fn new() -> Result<Self> {
        // SAFETY: plain allocation.
        NonNull::new(unsafe { ff::av_frame_alloc() })
            .map(Self)
            .ok_or_else(|| gpu_err("av_frame_alloc"))
    }

    /// A frame from the pool of hw frames context `frames`.
    pub(crate) fn hw(frames: &BufferRef) -> Result<Self> {
        let f = Self::new()?;
        // SAFETY: valid frame and frames context.
        check(
            unsafe { ff::av_hwframe_get_buffer(frames.as_ptr(), f.as_ptr(), 0) },
            "av_hwframe_get_buffer",
        )?;
        Ok(f)
    }

    pub(crate) fn as_ptr(&self) -> *mut ff::AVFrame {
        self.0.as_ptr()
    }

    pub(crate) fn get(&self) -> &ff::AVFrame {
        // SAFETY: valid for our lifetime.
        unsafe { self.0.as_ref() }
    }

    pub(crate) fn get_mut(&mut self) -> &mut ff::AVFrame {
        // SAFETY: valid and uniquely ours.
        unsafe { self.0.as_mut() }
    }
}

impl Clone for Frame {
    /// A new reference to the same data (`av_frame_clone`).
    fn clone(&self) -> Self {
        // SAFETY: valid frame.
        Self(NonNull::new(unsafe { ff::av_frame_clone(self.0.as_ptr()) }).expect("av_frame_clone"))
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        let mut p = self.0.as_ptr();
        // SAFETY: we own the frame.
        unsafe { ff::av_frame_free(&raw mut p) };
    }
}

/// An owned `AVCodecContext`.
#[derive(Debug)]
pub(crate) struct CodecCtx(NonNull<ff::AVCodecContext>);

// SAFETY: a codec context is only used by the thread that owns it.
unsafe impl Send for CodecCtx {}

impl CodecCtx {
    pub(crate) fn new(codec: *const ff::AVCodec) -> Result<Self> {
        // SAFETY: codec may be null (libav handles that).
        NonNull::new(unsafe { ff::avcodec_alloc_context3(codec) })
            .map(Self)
            .ok_or_else(|| gpu_err("avcodec_alloc_context3"))
    }

    pub(crate) fn as_ptr(&self) -> *mut ff::AVCodecContext {
        self.0.as_ptr()
    }

    pub(crate) fn get_mut(&mut self) -> &mut ff::AVCodecContext {
        // SAFETY: valid and uniquely ours.
        unsafe { self.0.as_mut() }
    }
}

impl Drop for CodecCtx {
    fn drop(&mut self) {
        let mut p = self.0.as_ptr();
        // SAFETY: we own the context.
        unsafe { ff::avcodec_free_context(&raw mut p) };
    }
}

/// An `AVDictionary` of options, freed on drop.
#[derive(Debug, Default)]
pub(crate) struct Dict(*mut ff::AVDictionary);

impl Dict {
    pub(crate) fn set(&mut self, key: &str, value: &str) -> Result<()> {
        let (k, v) = (cstr(key)?, cstr(value)?);
        // SAFETY: valid strings; the dictionary copies them.
        check(
            unsafe { ff::av_dict_set(&raw mut self.0, k.as_ptr(), v.as_ptr(), 0) },
            "av_dict_set",
        )?;
        Ok(())
    }

    pub(crate) fn as_mut_ptr(&mut self) -> *mut *mut ff::AVDictionary {
        &raw mut self.0
    }

    /// Keys libav didn't consume.
    pub(crate) fn leftover(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut e: *const ff::AVDictionaryEntry = ptr::null();
        loop {
            // SAFETY: iterating a valid (possibly null) dictionary.
            e = unsafe { ff::av_dict_get(self.0, c"".as_ptr(), e, ff::AV_DICT_IGNORE_SUFFIX) };
            if e.is_null() {
                break;
            }
            // SAFETY: entries have valid C strings.
            out.push(
                unsafe { CStr::from_ptr((*e).key) }
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        out
    }
}

impl Drop for Dict {
    fn drop(&mut self) {
        // SAFETY: we own the dictionary.
        unsafe { ff::av_dict_free(&raw mut self.0) };
    }
}

pub(crate) fn cstr(s: &str) -> Result<CString> {
    CString::new(s).map_err(|_| Error::Invalid {
        what: "string with NUL",
        value: s.to_owned(),
    })
}
