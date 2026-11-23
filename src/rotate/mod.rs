//! Lossless 90°/270° JPEG rotation via libturbojpeg's `tjTransform`.
//!
//! libcamera (Bookworm+) can only rotate 0/180 — the ISP can't transpose — so
//! quarter turns are done here, per frame, in the DCT domain (no decode /
//! re-encode, no quality loss). Measured on a Zero 2 W at 1640x1232: ~94 ms per
//! frame on one core, i.e. ~10 fps max.
//!
//! `libturbojpeg.so.0` (deb `libturbojpeg0`) is `dlopen`ed at runtime rather than
//! linked, so the zigbuild cross-compile needs no ARM copy of the library. If it
//! can't be loaded, frames pass through unrotated with a logged error.

use std::ffi::{c_char, c_int, c_ulong, c_void, CStr};
use std::sync::Mutex;

const TJXOP_ROT90: c_int = 5;
const TJXOP_ROT270: c_int = 7;
/// Drop partial edge MCUs that can't be transposed losslessly.
const TJXOPT_TRIM: c_int = 2;
const RTLD_NOW: c_int = 2;

#[repr(C)]
struct TjRegion {
    x: c_int,
    y: c_int,
    w: c_int,
    h: c_int,
}

#[repr(C)]
struct TjTransform {
    r: TjRegion,
    op: c_int,
    options: c_int,
    data: *mut c_void,
    custom_filter: *mut c_void,
}

type InitFn = unsafe extern "C" fn() -> *mut c_void;
type TransformFn = unsafe extern "C" fn(
    *mut c_void, *const u8, c_ulong, c_int, *mut *mut u8, *mut c_ulong, *mut TjTransform, c_int,
) -> c_int;
type FreeFn = unsafe extern "C" fn(*mut u8);
type ErrFn = unsafe extern "C" fn(*mut c_void) -> *const c_char;

#[link(name = "dl")]
extern "C" {
    fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

struct Tj {
    handle: *mut c_void,
    transform: TransformFn,
    free: FreeFn,
    err: ErrFn,
}

// The tjhandle is only ever used under the Mutex in `Rotator`.
unsafe impl Send for Tj {}

pub struct Rotator(Mutex<Tj>);

impl Rotator {
    /// Load libturbojpeg and create a transform handle. `None` (with an error
    /// logged) if the library is missing.
    pub fn load() -> Option<Self> {
        unsafe {
            let lib = dlopen(c"libturbojpeg.so.0".as_ptr(), RTLD_NOW);
            if lib.is_null() {
                log::error!("rotation 90/270 needs libturbojpeg.so.0 (apt install libturbojpeg0); frames will NOT be rotated");
                return None;
            }
            let sym = |name: &CStr| {
                let p = dlsym(lib, name.as_ptr());
                (!p.is_null()).then_some(p)
            };
            let init: InitFn = std::mem::transmute(sym(c"tjInitTransform")?);
            let transform: TransformFn = std::mem::transmute(sym(c"tjTransform")?);
            let free: FreeFn = std::mem::transmute(sym(c"tjFree")?);
            let err: ErrFn = std::mem::transmute(sym(c"tjGetErrorStr2")?);
            let handle = init();
            if handle.is_null() {
                log::error!("tjInitTransform failed; frames will NOT be rotated");
                return None;
            }
            Some(Self(Mutex::new(Tj { handle, transform, free, err })))
        }
    }

    /// Rotate a JPEG clockwise by `degrees` (90 or 270). `None` on failure or
    /// any other angle.
    pub fn rotate(&self, jpeg: &[u8], degrees: u16) -> Option<Vec<u8>> {
        let op = match degrees {
            90 => TJXOP_ROT90,
            270 => TJXOP_ROT270,
            _ => return None,
        };
        let tj = self.0.lock().unwrap();
        let mut xf = TjTransform {
            r: TjRegion { x: 0, y: 0, w: 0, h: 0 },
            op,
            options: TJXOPT_TRIM,
            data: std::ptr::null_mut(),
            custom_filter: std::ptr::null_mut(),
        };
        let mut out: *mut u8 = std::ptr::null_mut();
        let mut out_len: c_ulong = 0;
        unsafe {
            let rc = (tj.transform)(
                tj.handle, jpeg.as_ptr(), jpeg.len() as c_ulong, 1, &mut out, &mut out_len, &mut xf, 0,
            );
            if rc != 0 || out.is_null() {
                let msg = CStr::from_ptr((tj.err)(tj.handle)).to_string_lossy();
                log::warn!("tjTransform failed: {msg}");
                if !out.is_null() {
                    (tj.free)(out);
                }
                return None;
            }
            let v = std::slice::from_raw_parts(out, out_len as usize).to_vec();
            (tj.free)(out);
            Some(v)
        }
    }
}
