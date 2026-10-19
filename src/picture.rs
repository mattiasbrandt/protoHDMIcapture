//! The picture controls: LEVELS (the range of the card's frames) and the
//! colour values BRIGHTNESS, CONTRAST, GAMMA and SATURATION. They are applied
//! in the YUYV -> RGB pass with the formulas mpv's gpu-next uses, so a saved
//! value looks the same in both viewers:
//! - the values: mpv 0.41 `mp_csp_copy_equalizer_values` (video/csputils.c);
//! - the matrix, range, contrast, saturation and brightness: libplacebo 7.360
//!   `pl_color_repr_decode` (src/colorspace.c);
//! - gamma, after the matrix in non-linear light: libplacebo
//!   `pl_shader_decode_color` (src/shaders/colorspace.c).
//!
//! What LEVELS=auto means and which matrix applies depend on how mpv gets
//! the frames (`Coding`).

/// How the frames' colour is to be read, as ffmpeg tags them for mpv.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coding {
    /// Raw YUYV from V4L2: untagged, so mpv guesses the matrix from the size
    /// and libplacebo takes limited range.
    Raw,
    /// MJPEG (#17): ffmpeg's mjpeg decoder tags every frame BT.601 (BT470BG)
    /// and full range (yuvj, JFIF), unless a comment says "CS=ITU601"
    /// (FFmpeg libavcodec/mjpegdec.c). Whether a card's encoder really puts
    /// full-range values in is not known (the MS2109's is unverified), so
    /// LEVELS still overrides the range.
    Jpeg,
}

/// LEVELS, in the order the overlay's l cycles them (settings.rows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Levels {
    /// What mpv makes of the card's frames: ffmpeg's V4L2 input tags no
    /// range on raw frames, so libplacebo guesses limited for YCbCr
    /// (measured on the MS2130 at 1080p: mpv's video-out-params say bt.709
    /// limited); JPEG frames are tagged full (`Coding::Jpeg`).
    Auto,
    Limited,
    Full,
}

impl Levels {
    /// LEVELS, as the settings spell it.
    pub fn from_settings(v: &str) -> Result<Self, String> {
        Ok(match v {
            "auto" => Self::Auto,
            "limited" => Self::Limited,
            "full" => Self::Full,
            other => return Err(format!("LEVELS={other} is not auto, limited or full")),
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Limited => "limited",
            Self::Full => "full",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Auto => Self::Limited,
            Self::Limited => Self::Full,
            Self::Full => Self::Auto,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Colour {
    Contrast,
    Brightness,
    Gamma,
    Saturation,
}

/// The live values, as the settings spell them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Picture {
    pub levels: Levels,
    pub contrast: i32,
    pub brightness: i32,
    pub gamma: i32,
    pub saturation: i32,
}

/// The uniform as the GPU takes it.
pub fn bytes(u: &[[f32; 4]; 4]) -> Vec<u8> {
    u.as_flattened().iter().flat_map(|v| v.to_ne_bytes()).collect()
}

/// The colour values' range, mpv's and settings.rows'.
const COLOUR_RANGE: std::ops::RangeInclusive<i32> = -100..=100;

impl Picture {
    /// Every setting at its default.
    pub const DEFAULT: Self = Self { levels: Levels::Auto, contrast: 0, brightness: 0, gamma: 0, saturation: 0 };

    /// mpv's 1..8 keys: `add <colour> ±1`, clamped to -100..100.
    pub fn adjust(&mut self, colour: Colour, by: i32) {
        let v = match colour {
            Colour::Contrast => &mut self.contrast,
            Colour::Brightness => &mut self.brightness,
            Colour::Gamma => &mut self.gamma,
            Colour::Saturation => &mut self.saturation,
        };
        *v = (*v + by).clamp(*COLOUR_RANGE.start(), *COLOUR_RANGE.end());
    }

    /// The overlay's r: the four colour values to 0; LEVELS stays.
    pub fn reset_colour(&mut self) {
        *self = Self { levels: self.levels, ..Self::DEFAULT };
    }

    /// The overlay's l.
    pub fn cycle_levels(&mut self) {
        self.levels = self.levels.next();
    }

    /// The convert pass's uniform: three rows `[m0, m1, m2, offset]` taking
    /// the texture's (Y, Cb, Cr) to RGB, then `[1/gamma, 0, 0, 0]`.
    pub fn uniform(&self, width: u32, height: u32, coding: Coding) -> [[f32; 4]; 4] {
        // mp_csp_copy_equalizer_values (gamma-factor stays at its default, 1).
        let brightness = f64::from(self.brightness) / 100.0;
        let contrast = (f64::from(self.contrast) + 100.0) / 100.0;
        let saturation = (f64::from(self.saturation) + 100.0) / 100.0;
        let gamma = (8f64.ln() * f64::from(self.gamma) / 100.0).exp();

        // mp_csp_guess_colorspace for raw frames, which carry no matrix.
        let bt709 = coding == Coding::Raw && (width >= 1280 || height > 576);
        let (lr, lg, lb) = if bt709 { (0.2126, 0.7152, 0.0722) } else { (0.2990, 0.5870, 0.1140) };
        // luma_coeffs, then apply_hue_sat with hue 0: saturation scales the
        // Cb and Cr columns.
        let mut m = [
            [1.0, 0.0, 2.0 * (1.0 - lr)],
            [1.0, -2.0 * (1.0 - lb) * lb / lg, -2.0 * (1.0 - lr) * lr / lg],
            [1.0, 2.0 * (1.0 - lb), 0.0],
        ];
        for row in &mut m {
            row[1] *= saturation;
            row[2] *= saturation;
        }

        // 8-bit levels; libplacebo's x/256 * 256/255 is x/255.
        let levels = match (self.levels, coding) {
            (Levels::Auto, Coding::Raw) => Levels::Limited,
            (Levels::Auto, Coding::Jpeg) => Levels::Full,
            (l, _) => l,
        };
        let (ymin, ymul, cmid, cmul) = match levels {
            Levels::Auto | Levels::Limited => (16.0 / 255.0, 255.0 / 219.0, 128.0 / 255.0, 0.5 / (112.0 / 255.0)),
            // Chroma's middle is 128/255, not 0.5, as libplacebo has it.
            Levels::Full => (0.0, 1.0, 128.0 / 255.0, 0.5 / (1.0 - 128.0 / 255.0)),
        };
        // Contrast scales every input column (chroma too); brightness is an
        // offset on the output. Then black keeps mapping to black.
        let mul = [ymul * contrast, cmul * contrast, cmul * contrast];
        let black = [ymin, cmid, cmid];
        let mut out = [[0f32; 4]; 4];
        for (i, row) in m.iter().enumerate() {
            let mut c = brightness;
            for j in 0..3 {
                let v = row[j] * mul[j];
                out[i][j] = v as f32;
                c -= v * black[j];
            }
            out[i][3] = c as f32;
        }
        out[3][0] = (1.0 / gamma) as f32;
        out
    }
}

/// The WGSL side of `Picture::uniform`.
pub const WGSL: &str = r#"
struct Picture {
    r: vec4f,
    g: vec4f,
    b: vec4f,
    inv_gamma: vec4f,
};

fn picture(pic: Picture, yuv: vec3f) -> vec3f {
    var rgb = vec3f(dot(pic.r.xyz, yuv) + pic.r.w, dot(pic.g.xyz, yuv) + pic.g.w, dot(pic.b.xyz, yuv) + pic.b.w);
    // GPUs compute pow as exp2(y * log2(x)), which need not return x for
    // y = 1; skip it at the neutral value.
    if (pic.inv_gamma.x != 1.0) {
        rgb = pow(max(rgb, vec3f(0.0)), vec3f(pic.inv_gamma.x));
    }
    return clamp(rgb, vec3f(0.0), vec3f(1.0));
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn neutral(levels: Levels) -> Picture {
        Picture { levels, contrast: 0, brightness: 0, gamma: 0, saturation: 0 }
    }

    /// The shader's arithmetic on 8-bit (Y, Cb, Cr), unclamped.
    fn rgb(p: &Picture, w: u32, h: u32, yuv: [f32; 3]) -> [f32; 3] {
        rgb_coded(p, w, h, Coding::Raw, yuv)
    }

    fn rgb_coded(p: &Picture, w: u32, h: u32, coding: Coding, yuv: [f32; 3]) -> [f32; 3] {
        let u = p.uniform(w, h, coding);
        let yuv = yuv.map(|v| v / 255.0);
        let mut out = [0.0; 3];
        for i in 0..3 {
            let v = u[i][0] * yuv[0] + u[i][1] * yuv[1] + u[i][2] * yuv[2] + u[i][3];
            out[i] = if u[3][0] == 1.0 { v } else { v.max(0.0).powf(u[3][0]) };
        }
        out
    }

    fn close(a: [f32; 3], b: [f32; 3]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-4)
    }

    #[test]
    fn neutral_limited_709_is_the_feed_before_the_controls() {
        let p = neutral(Levels::Auto);
        assert_eq!(p.uniform(1920, 1080, Coding::Raw), neutral(Levels::Limited).uniform(1920, 1080, Coding::Raw));
        assert!(close(rgb(&p, 1920, 1080, [16.0, 128.0, 128.0]), [0.0; 3]));
        assert!(close(rgb(&p, 1920, 1080, [235.0, 128.0, 128.0]), [1.0; 3]));
        // The constants the shader had before #12: Cr's weight in R.
        let u = p.uniform(1920, 1080, Coding::Raw);
        assert!((u[0][2] - 1.5748 * 255.0 / 224.0).abs() < 1e-5);
        assert!((u[2][1] - 1.8556 * 255.0 / 224.0).abs() < 1e-5);
    }

    #[test]
    fn full_range_maps_0_and_255() {
        let p = neutral(Levels::Full);
        assert!(close(rgb(&p, 1920, 1080, [0.0, 128.0, 128.0]), [0.0; 3]));
        assert!(close(rgb(&p, 1920, 1080, [255.0, 128.0, 128.0]), [1.0; 3]));
        // Limited black, relabelled full, is lifted: the washed-out look.
        assert!(rgb(&p, 1920, 1080, [16.0, 128.0, 128.0])[0] > 0.06);
    }

    #[test]
    fn the_matrix_is_guessed_from_the_size_as_mpv_does() {
        let p = neutral(Levels::Limited);
        let r_from_cr = |w, h| p.uniform(w, h, Coding::Raw)[0][2];
        let bt709 = 2.0 * (1.0 - 0.2126) * 255.0 / 224.0;
        let bt601 = 2.0 * (1.0 - 0.2990) * 255.0 / 224.0;
        assert!((r_from_cr(1920, 1080) - bt709).abs() < 1e-5);
        assert!((r_from_cr(1280, 720) - bt709).abs() < 1e-5);
        assert!((r_from_cr(720, 577) - bt709).abs() < 1e-5);
        assert!((r_from_cr(720, 576) - bt601).abs() < 1e-5);
        assert!((r_from_cr(640, 480) - bt601).abs() < 1e-5);
    }

    #[test]
    fn jpeg_frames_are_bt601_full_range_on_auto() {
        let auto = neutral(Levels::Auto);
        // Full range: 0 and 255 are black and white, at any size.
        for (w, h) in [(1920, 1080), (1280, 720), (640, 480)] {
            assert!(close(rgb_coded(&auto, w, h, Coding::Jpeg, [0.0, 128.0, 128.0]), [0.0; 3]));
            assert!(close(rgb_coded(&auto, w, h, Coding::Jpeg, [255.0, 128.0, 128.0]), [1.0; 3]));
            let bt601 = 2.0 * (1.0 - 0.2990) * 255.0 / 254.0;
            assert!((auto.uniform(w, h, Coding::Jpeg)[0][2] - bt601).abs() < 1e-5, "{w}x{h}: BT.601");
        }
        // LEVELS still decides the range when set; the matrix stays BT.601.
        let limited = neutral(Levels::Limited);
        assert!(close(rgb_coded(&limited, 1280, 720, Coding::Jpeg, [16.0, 128.0, 128.0]), [0.0; 3]));
        assert!((limited.uniform(1280, 720, Coding::Jpeg)[0][2] - 2.0 * (1.0 - 0.2990) * 255.0 / 224.0).abs() < 1e-5);
        assert_eq!(neutral(Levels::Full).uniform(1280, 720, Coding::Jpeg), auto.uniform(1280, 720, Coding::Jpeg));
    }

    #[test]
    fn colour_values_as_mpv_scales_them() {
        let red = [81.0, 90.0, 240.0];
        let mut p = neutral(Levels::Limited);
        p.contrast = -100;
        assert!(close(rgb(&p, 1920, 1080, red), [0.0; 3]), "contrast -100 is black");
        p = neutral(Levels::Limited);
        p.brightness = 100;
        assert!(rgb(&p, 1920, 1080, [16.0, 128.0, 128.0]).iter().all(|v| (v - 1.0).abs() < 1e-4), "brightness 100 lifts black to white");
        p = neutral(Levels::Limited);
        p.saturation = -100;
        let grey = rgb(&p, 1920, 1080, red);
        assert!((grey[0] - grey[1]).abs() < 1e-5 && (grey[1] - grey[2]).abs() < 1e-5, "saturation -100 is grey");
        p = neutral(Levels::Limited);
        p.gamma = 100;
        assert!((p.uniform(1920, 1080, Coding::Raw)[3][0] - 1.0 / 8.0).abs() < 1e-6, "gamma 100 is 8");
        p.gamma = -50;
        assert!((p.uniform(1920, 1080, Coding::Raw)[3][0] - 8f32.sqrt()).abs() < 1e-5);
        // Contrast scales chroma too: a colour's distance from grey grows.
        p = neutral(Levels::Limited);
        p.contrast = 100;
        let c = rgb(&p, 1920, 1080, red);
        let n = rgb(&neutral(Levels::Limited), 1920, 1080, red);
        assert!(((c[0] - c[1]) - 2.0 * (n[0] - n[1])).abs() < 1e-4);
    }

    #[test]
    fn keys_clamp_reset_and_cycle() {
        let mut p = Picture { levels: Levels::Full, contrast: 99, brightness: -3, gamma: 0, saturation: 5 };
        p.adjust(Colour::Contrast, 1);
        p.adjust(Colour::Contrast, 1);
        assert_eq!(p.contrast, 100);
        p.adjust(Colour::Brightness, -1);
        assert_eq!(p.brightness, -4);
        p.reset_colour();
        assert_eq!(p, neutral(Levels::Full));
        p.cycle_levels();
        assert_eq!(p.levels, Levels::Auto);
        p.cycle_levels();
        assert_eq!(p.levels, Levels::Limited);
        assert_eq!(Levels::from_settings("full"), Ok(Levels::Full));
        assert!(Levels::from_settings("tv").is_err());
    }

    #[test]
    fn levels_and_ranges_match_lib_settings_sh() {
        let rows = crate::settings::rows();
        let values = |n: &str| rows.iter().find(|r| r.name == n).unwrap().values.clone();
        let order: Vec<&str> = [Levels::Auto, Levels::Auto.next(), Levels::Auto.next().next()].map(Levels::as_str).to_vec();
        assert_eq!(values("LEVELS"), format!("enum:{}", order.join(",")));
        assert_eq!(Levels::Auto.next().next().next(), Levels::Auto);
        for n in ["BRIGHTNESS", "CONTRAST", "SATURATION", "GAMMA"] {
            assert_eq!(values(n), format!("int:{}..{}", COLOUR_RANGE.start(), COLOUR_RANGE.end()));
        }
    }
}
