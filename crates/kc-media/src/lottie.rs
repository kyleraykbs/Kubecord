//! Lottie animations (Discord's own stickers), drawn by rlottie.

use std::ffi::{CString, c_char, c_double};

#[repr(C)]
struct RawAnimation {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn lottie_animation_from_data(data: *const c_char, key: *const c_char, resource_path: *const c_char) -> *mut RawAnimation;
    fn lottie_animation_destroy(animation: *mut RawAnimation);
    fn lottie_animation_get_size(animation: *const RawAnimation, width: *mut usize, height: *mut usize);
    fn lottie_animation_get_totalframe(animation: *const RawAnimation) -> usize;
    fn lottie_animation_get_framerate(animation: *const RawAnimation) -> c_double;
    fn lottie_animation_render(
        animation: *mut RawAnimation,
        frame: usize,
        buffer: *mut u32,
        width: usize,
        height: usize,
        bytes_per_line: usize,
    );
}

/// A loaded animation.
pub struct Animation {
    raw: *mut RawAnimation,
}

impl Animation {
    /// Loads a Lottie animation's JSON; `key` names it in rlottie's cache
    /// (the same key must mean the same JSON).
    pub fn from_json(json: &str, key: &str) -> Option<Self> {
        let data = CString::new(json).ok()?;
        let key = CString::new(key).ok()?;
        let empty = CString::default();
        // SAFETY: valid NUL-terminated strings; rlottie copies what it keeps.
        let raw = unsafe { lottie_animation_from_data(data.as_ptr(), key.as_ptr(), empty.as_ptr()) };
        (!raw.is_null()).then_some(Self { raw })
    }

    /// Its own size in pixels.
    pub fn size(&self) -> (usize, usize) {
        let (mut width, mut height) = (0, 0);
        // SAFETY: `raw` is a live animation.
        unsafe { lottie_animation_get_size(self.raw, &mut width, &mut height) };
        (width, height)
    }

    /// How many frames it has (rlottie counts the last as well).
    pub fn frames(&self) -> usize {
        // SAFETY: `raw` is a live animation.
        unsafe { lottie_animation_get_totalframe(self.raw) }
    }

    pub fn framerate(&self) -> f64 {
        // SAFETY: `raw` is a live animation.
        unsafe { lottie_animation_get_framerate(self.raw) }
    }

    /// Draws `frame` at `width`×`height`: premultiplied ARGB, one native-
    /// endian `u32` a pixel (BGRA bytes on little-endian machines).
    pub fn render(&mut self, frame: usize, width: usize, height: usize) -> Vec<u32> {
        let mut pixels = vec![0u32; width * height];
        if width > 0 && height > 0 {
            // SAFETY: the buffer holds width×height pixels, `width * 4`
            // bytes a line.
            unsafe {
                lottie_animation_render(self.raw, frame, pixels.as_mut_ptr(), width, height, width * 4);
            }
        }
        pixels
    }
}

impl Drop for Animation {
    fn drop(&mut self) {
        // SAFETY: `raw` came from rlottie and is destroyed once.
        unsafe { lottie_animation_destroy(self.raw) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 100×100, 30-frame animation of a red square sliding right.
    const SQUARE: &str = r#"{"v":"5.6.2","fr":30,"ip":0,"op":30,"w":100,"h":100,"layers":[
        {"ddd":0,"ind":1,"ty":4,"nm":"square","sr":1,"ip":0,"op":30,"st":0,"ao":0,
         "ks":{"o":{"a":0,"k":100},"r":{"a":0,"k":0},"a":{"a":0,"k":[0,0,0]},"s":{"a":0,"k":[100,100,100]},
               "p":{"a":1,"k":[{"t":0,"s":[25,50,0],"i":{"x":[1],"y":[1]},"o":{"x":[0],"y":[0]}},{"t":29,"s":[75,50,0]}]}},
         "shapes":[{"ty":"gr","it":[
            {"ty":"rc","d":1,"s":{"a":0,"k":[40,40]},"p":{"a":0,"k":[0,0]},"r":{"a":0,"k":0}},
            {"ty":"fl","c":{"a":0,"k":[1,0,0,1]},"o":{"a":0,"k":100},"r":1},
            {"ty":"tr","p":{"a":0,"k":[0,0]},"a":{"a":0,"k":[0,0]},"s":{"a":0,"k":[100,100]},"r":{"a":0,"k":0},"o":{"a":0,"k":100}}
         ]}]}]}"#;

    #[test]
    fn draws_a_lottie_animation() {
        let mut animation = Animation::from_json(SQUARE, "test-square").expect("loads");
        assert_eq!(animation.size(), (100, 100));
        // rlottie counts both ends (frames 0 to 30).
        assert_eq!(animation.frames(), 31);
        assert!((animation.framerate() - 30.0).abs() < 0.01);
        let red_at = |pixels: &[u32], x: usize, y: usize| {
            let pixel = pixels[y * 100 + x];
            (pixel >> 24) == 0xff && ((pixel >> 16) & 0xff) > 0xf0 && (pixel & 0xff) < 0x10
        };
        let first = animation.render(0, 100, 100);
        assert!(red_at(&first, 25, 50), "square starts on the left");
        assert!(!red_at(&first, 80, 50));
        let last = animation.render(29, 100, 100);
        assert!(red_at(&last, 80, 50), "and ends on the right");
        assert!(Animation::from_json("not lottie", "bad").is_none());
    }
}
