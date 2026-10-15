//! MJPEG cards (#17, the MS2109 class): each frame is a JPEG, decoded on the
//! CPU with zune-jpeg (pure Rust, so it builds for Windows too, #9) into the
//! YUYV the convert pass already takes. The YCbCr values are kept as the
//! JPEG holds them; their range and matrix are picture.rs's `Coding::Jpeg`.

use zune_jpeg::JpegDecoder;
use zune_jpeg::zune_core::bytestream::ZCursor;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;

/// The OpenDML marker for MJPEG with the Huffman tables left out: an APP0
/// segment reading "AVI1". Inserted after SOI when a frame has no DHT, so the
/// decoder fills in the standard tables (JPEG Annex K.3), as ffmpeg's mjpeg
/// decoder always does. UVC cards may send frames without them.
const AVI1: [u8; 9] = [0xFF, 0xE0, 0x00, 0x07, b'A', b'V', b'I', b'1', 0x00];

pub struct Decoder {
    width: usize,
    height: usize,
    /// The decoded frame, 3 bytes (Y, Cb, Cr) per pixel.
    ycc: Vec<u8>,
    /// A frame with AVI1 put in.
    patched: Vec<u8>,
}

impl Decoder {
    /// For frames of `width` x `height` (an even width, as YUYV needs).
    pub fn new(width: u32, height: u32) -> Self {
        let (width, height) = (width as usize, height as usize);
        Self { width, height, ycc: vec![0; width * height * 3], patched: Vec::new() }
    }

    /// Decodes one JPEG into `out`, a YUYV frame of the decoder's size. An
    /// error means the frame is unusable (cut short, another size, not
    /// YCbCr); `out` is then undefined.
    pub fn decode(&mut self, jpeg: &[u8], out: &mut [u8]) -> Result<(), String> {
        let data = if has_tables(jpeg)? {
            jpeg
        } else {
            self.patched.clear();
            self.patched.extend_from_slice(&jpeg[..2]);
            self.patched.extend_from_slice(&AVI1);
            self.patched.extend_from_slice(&jpeg[2..]);
            &self.patched
        };
        let options = DecoderOptions::default()
            .jpeg_set_out_colorspace(ColorSpace::YCbCr)
            .set_max_width(self.width)
            .set_max_height(self.height)
            .set_strict_mode(false);
        let mut d = JpegDecoder::new_with_options(ZCursor::new(data), options);
        d.decode_headers().map_err(|e| format!("JPEG headers: {e}"))?;
        let size = d.dimensions().ok_or("JPEG headers: no frame size")?;
        if size != (self.width, self.height) {
            return Err(format!("the JPEG is {}x{}, not {}x{} (RES)", size.0, size.1, self.width, self.height));
        }
        // Luma-only, RGB or CMYK frames would decode to something else.
        match d.input_colorspace() {
            Some(ColorSpace::YCbCr) => {}
            other => return Err(format!("the JPEG is {other:?}, not YCbCr")),
        }
        d.decode_into(&mut self.ycc).map_err(|e| format!("JPEG: {e}"))?;
        pack_yuyv(&self.ycc, out);
        Ok(())
    }
}

/// Whether the JPEG defines Huffman tables (a DHT before its first SOS).
fn has_tables(jpeg: &[u8]) -> Result<bool, String> {
    if jpeg.len() < 4 || jpeg[..2] != [0xFF, 0xD8] {
        return Err("not a JPEG (no SOI)".into());
    }
    let mut at = 2;
    loop {
        // Fill bytes (0xFF) may come before a marker.
        while jpeg.get(at) == Some(&0xFF) && jpeg.get(at + 1) == Some(&0xFF) {
            at += 1;
        }
        let (Some(&0xFF), Some(&marker)) = (jpeg.get(at), jpeg.get(at + 1)) else {
            return Err(format!("JPEG: no marker at byte {at}"));
        };
        match marker {
            0xC4 => return Ok(true),
            0xDA => return Ok(false),
            // TEM and RSTn stand alone; nothing else may come before SOS.
            0x01 | 0xD0..=0xD7 => at += 2,
            0xD8 | 0xD9 => return Err(format!("JPEG: marker {marker:#04x} before SOS")),
            _ => {
                let len = jpeg.get(at + 2..at + 4).ok_or("JPEG: cut short in its headers")?;
                at += 2 + usize::from(u16::from_be_bytes([len[0], len[1]]));
            }
        }
    }
}

/// 4:4:4 (Y, Cb, Cr) to YUYV: Y0 Cb Y1 Cr per pixel pair, the chroma
/// averaged over the pair. A 4:2:2 JPEG's chroma sample sits between the two
/// pixels (JFIF), so the average of its upsampled pair is near that sample.
fn pack_yuyv(ycc: &[u8], out: &mut [u8]) {
    for (pair, o) in ycc.as_chunks::<6>().0.iter().zip(out.as_chunks_mut::<4>().0) {
        let avg = |a: u8, b: u8| (u16::from(a) + u16::from(b)).div_ceil(2) as u8;
        o[0] = pair[0];
        o[1] = avg(pair[1], pair[4]);
        o[2] = pair[3];
        o[3] = avg(pair[2], pair[5]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jpeg_encoder::{ColorType, Encoder, SamplingFactor};
    use std::time::Instant;

    /// A JPEG as the card sends one: 4:2:2, the standard tables.
    fn jpeg(width: u16, height: u16, ycc: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut e = Encoder::new(&mut out, 90);
        e.set_sampling_factor(SamplingFactor::F_2_1);
        e.encode(ycc, width, height, ColorType::Ycbcr).unwrap();
        out
    }

    /// Every segment of one marker type taken out.
    fn without(jpeg: &[u8], marker: u8) -> Vec<u8> {
        let mut out = jpeg[..2].to_vec();
        let mut at = 2;
        while jpeg[at + 1] != 0xDA {
            let len = 2 + usize::from(u16::from_be_bytes([jpeg[at + 2], jpeg[at + 3]]));
            if jpeg[at + 1] != marker {
                out.extend_from_slice(&jpeg[at..at + len]);
            }
            at += len;
        }
        out.extend_from_slice(&jpeg[at..]);
        out
    }

    /// Bars of known (Y, Cb, Cr), each 16 pixels wide: a JPEG keeps flat
    /// areas nearly exact.
    const BARS: [[u8; 3]; 4] = [[16, 128, 128], [235, 128, 128], [81, 90, 240], [145, 54, 34]];

    fn bars(width: usize, height: usize) -> Vec<u8> {
        (0..width * height).flat_map(|i| BARS[(i % width) / 16 % BARS.len()]).collect()
    }

    #[test]
    fn decodes_to_yuyv_with_the_values_the_jpeg_holds() {
        let (w, h) = (128, 32);
        let mut d = Decoder::new(w, h);
        let mut out = vec![0; (w * h * 2) as usize];
        d.decode(&jpeg(w as u16, h as u16, &bars(w as usize, h as usize)), &mut out).unwrap();
        // The middle of each bar, away from the edges' ringing.
        for (i, want) in BARS.iter().enumerate() {
            let x = i * 16 + 8;
            let at = (16 * w as usize + x) * 2;
            let got = [out[at], out[at + 1], out[at + 3]];
            for c in 0..3 {
                assert!((i32::from(got[c]) - i32::from(want[c])).abs() <= 2, "bar {i}: {got:?}, want {want:?}");
            }
        }
    }

    #[test]
    fn frames_without_huffman_tables_decode_with_the_standard_ones() {
        let (w, h) = (64, 16);
        let full = jpeg(w as u16, h as u16, &bars(w as usize, h as usize));
        let bare = without(&full, 0xC4);
        assert!(has_tables(&full).unwrap() && !has_tables(&bare).unwrap());
        let mut d = Decoder::new(w, h);
        let (mut a, mut b) = (vec![0; (w * h * 2) as usize], vec![0; (w * h * 2) as usize]);
        d.decode(&full, &mut a).unwrap();
        d.decode(&bare, &mut b).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn unusable_frames_are_errors() {
        let (w, h) = (64, 16);
        let good = jpeg(w as u16, h as u16, &bars(w as usize, h as usize));
        let mut out = vec![0; (w * h * 2) as usize];
        let mut d = Decoder::new(w, h);
        assert!(d.decode(&[0, 0, 0, 0], &mut out).unwrap_err().contains("no SOI"));
        assert!(d.decode(&good[..20], &mut out).is_err(), "cut short in the headers");
        let e = Decoder::new(128, 16).decode(&good, &mut vec![0; 128 * 16 * 2]).unwrap_err();
        assert!(e.contains("64x16, not 128x16"), "{e}");
        let grey = {
            let mut out = Vec::new();
            Encoder::new(&mut out, 90).encode(&vec![128; (w * h) as usize], w as u16, h as u16, ColorType::Luma).unwrap();
            out
        };
        assert!(d.decode(&grey, &mut out).unwrap_err().contains("not YCbCr"));
    }

    /// The CPU cost per frame at the MS2109's two modes, in a release
    /// build: `cargo test --release mjpeg::tests::decode_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "a measurement, not a check"]
    fn decode_cost() {
        for (w, h) in [(1280u32, 720u32), (1920, 1080)] {
            // A picture with detail, so the entropy decoding has work.
            let ycc: Vec<u8> = (0..(w * h) as usize)
                .flat_map(|i| {
                    let (x, y) = (i % w as usize, i / w as usize);
                    [((x ^ y) & 0xFF) as u8, (x * 255 / w as usize) as u8, (y * 255 / h as usize) as u8]
                })
                .collect();
            let frame = jpeg(w as u16, h as u16, &ycc);
            let mut d = Decoder::new(w, h);
            let mut out = vec![0; (w * h * 2) as usize];
            for _ in 0..10 {
                d.decode(&frame, &mut out).unwrap();
            }
            let n = 200;
            let mut times: Vec<f64> = (0..n)
                .map(|_| {
                    let t = Instant::now();
                    d.decode(&frame, &mut out).unwrap();
                    t.elapsed().as_secs_f64() * 1000.0
                })
                .collect();
            times.sort_by(f64::total_cmp);
            let t = Instant::now();
            for _ in 0..n {
                pack_yuyv(&d.ycc, &mut out);
            }
            println!(
                "{w}x{h} ({} KiB JPEG): median {:.2} ms, p99 {:.2} ms, of which the YUYV packing {:.2} ms",
                frame.len() / 1024,
                times[n / 2],
                times[n * 99 / 100],
                t.elapsed().as_secs_f64() * 1000.0 / n as f64
            );
        }
    }
}
