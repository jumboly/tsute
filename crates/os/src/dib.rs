//! Windows の Clipboard 画像（CF_DIB = BITMAPINFOHEADER + 画素）と PNG の相互変換（ADR-0003 Windows 節）。
//!
//! OS の画像 API（WIC）を使わず純 Rust で書くのは、変換ロジックを macOS 上の単体テストでも検証できるようにするため。
//! Clipboard の画像はスクリーンショット等の 24/32bpp がほとんどなので、対応形式を絞って単純に保つ。

use std::io::Cursor;

/// 1 辺の上限。壊れたヘッダで巨大なバッファを確保しないため（8K の 2 倍より十分大きい値）
const MAX_DIM: u32 = 32_768;

const BI_RGB: u32 = 0;
const BI_BITFIELDS: u32 = 3;
const BI_ALPHABITFIELDS: u32 = 6;

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
}
fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}
fn i32_at(b: &[u8], o: usize) -> Option<i32> {
    Some(i32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

/// ビットマスクで取り出した値を 8bit に広げる
fn channel(px: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let bits = (mask >> shift).count_ones();
    let v = (px & mask) >> shift;
    let max = (1u32 << bits) - 1;
    ((v * 255 + max / 2) / max) as u8
}

/// CF_DIB の中身を RGBA8 に展開する。未対応の形式（RLE 圧縮・JPEG 埋め込み等）は None
pub fn dib_to_rgba(dib: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let hsize = u32_at(dib, 0)? as usize;
    if hsize < 40 {
        return None;
    }
    let width = i32_at(dib, 4)?;
    let height = i32_at(dib, 8)?;
    let bpp = u16_at(dib, 14)?;
    let compression = u32_at(dib, 16)?;
    let clr_used = u32_at(dib, 32)?;
    if width <= 0 || height == 0 || width as u32 > MAX_DIM || height.unsigned_abs() > MAX_DIM {
        return None;
    }
    let (w, h) = (width as u32, height.unsigned_abs());
    // 高さが正なら下の行から並ぶ（bottom-up）
    let bottom_up = height > 0;

    // マスクは V4/V5 ヘッダなら中に、40 バイトのヘッダなら直後に置かれる
    let mut off = hsize;
    let (mut rm, mut gm, mut bm, mut am) = match bpp {
        32 => (0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 0),
        16 => (0x7c00, 0x03e0, 0x001f, 0),
        _ => (0, 0, 0, 0),
    };
    if compression == BI_BITFIELDS || compression == BI_ALPHABITFIELDS {
        if bpp != 16 && bpp != 32 {
            return None;
        }
        let base = if hsize >= 52 { 40 } else { hsize };
        rm = u32_at(dib, base)?;
        gm = u32_at(dib, base + 4)?;
        bm = u32_at(dib, base + 8)?;
        if hsize >= 56 {
            am = u32_at(dib, 52)?;
        } else if compression == BI_ALPHABITFIELDS {
            am = u32_at(dib, base + 12)?;
        }
        if hsize == 40 {
            off += if compression == BI_ALPHABITFIELDS { 16 } else { 12 };
        }
    } else if compression != BI_RGB {
        return None;
    }

    let palette: Vec<[u8; 4]> = if bpp <= 8 {
        let n = if clr_used == 0 {
            1usize << bpp
        } else {
            clr_used.min(256) as usize
        };
        let p = dib.get(off..off + n * 4)?;
        off += n * 4;
        p.as_chunks::<4>().0.iter().map(|c| [c[2], c[1], c[0], 255]).collect()
    } else {
        Vec::new()
    };

    let stride = ((w as usize * bpp as usize).div_ceil(32)) * 4;
    let pixels = dib.get(off..off.checked_add(stride.checked_mul(h as usize)?)?)?;
    let mut out = vec![0u8; w as usize * h as usize * 4];
    for y in 0..h as usize {
        let src_row = if bottom_up { h as usize - 1 - y } else { y };
        let row = &pixels[src_row * stride..src_row * stride + stride];
        for x in 0..w as usize {
            let rgba: [u8; 4] = match bpp {
                32 => {
                    let px = u32::from_le_bytes(row[x * 4..x * 4 + 4].try_into().ok()?);
                    let a = if am != 0 { channel(px, am) } else { (px >> 24) as u8 };
                    [channel(px, rm), channel(px, gm), channel(px, bm), a]
                }
                24 => [row[x * 3 + 2], row[x * 3 + 1], row[x * 3], 255],
                16 => {
                    let px = u16::from_le_bytes(row[x * 2..x * 2 + 2].try_into().ok()?) as u32;
                    [channel(px, rm), channel(px, gm), channel(px, bm), 255]
                }
                8 | 4 | 1 => {
                    let bit = x * bpp as usize;
                    let byte = row[bit / 8];
                    let idx = (byte >> (8 - bpp as usize - bit % 8)) & ((1u16 << bpp) - 1) as u8;
                    *palette.get(idx as usize)?
                }
                _ => return None,
            };
            out[(y * w as usize + x) * 4..][..4].copy_from_slice(&rgba);
        }
    }
    // 32bpp BI_RGB の 4 バイト目は「未使用（0）」のアプリが多い。全画素 0 なら不透明とみなす
    // （そのまま使うと全面透明な画像になるため。ブラウザ等の Clipboard 実装と同じ扱い）
    if bpp == 32 && am == 0 && out.as_chunks::<4>().0.iter().all(|p| p[3] == 0) {
        out.as_chunks_mut::<4>().0.iter_mut().for_each(|p| p[3] = 255);
    }
    Some((w, h, out))
}

pub fn rgba_to_png(w: u32, h: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    let mut enc = png::Encoder::new(&mut buf, w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut wr = enc.write_header().ok()?;
    wr.write_image_data(rgba).ok()?;
    wr.finish().ok()?;
    Some(buf)
}

pub fn dib_to_png(dib: &[u8]) -> Option<Vec<u8>> {
    let (w, h, rgba) = dib_to_rgba(dib)?;
    rgba_to_png(w, h, &rgba)
}

/// PNG を RGBA8 に展開する（パレット・グレースケール・16bit も 8bit RGBA にそろえる）
pub fn png_to_rgba(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut dec = png::Decoder::new(Cursor::new(data));
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = dec.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width, info.height);
    if w > MAX_DIM || h > MAX_DIM {
        return None;
    }
    let buf = &buf[..info.buffer_size()];
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => buf.to_vec(),
        png::ColorType::Rgb => buf
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => buf
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return None,
    };
    Some((w, h, rgba))
}

/// CF_DIB 用の 24bpp（bottom-up）DIB。透明部分は白と合成する。
/// 32bpp のアルファを正しく扱わないアプリが多く、黒く潰れるのを避けるため。アルファが必要なアプリは
/// 同時に載せる "PNG" 形式を使う（Office・ブラウザ等）
pub fn rgba_to_dib24(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
    let stride = (w as usize * 3).div_ceil(4) * 4;
    let mut out = Vec::with_capacity(40 + stride * h as usize);
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&BI_RGB.to_le_bytes());
    out.extend_from_slice(&((stride * h as usize) as u32).to_le_bytes());
    // 96 DPI 相当（2835 px/m）
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    let over_white = |c: u8, a: u8| ((c as u32 * a as u32 + 255 * (255 - a as u32) + 127) / 255) as u8;
    for y in (0..h as usize).rev() {
        let start = out.len();
        for x in 0..w as usize {
            let p = &rgba[(y * w as usize + x) * 4..][..4];
            out.extend_from_slice(&[over_white(p[2], p[3]), over_white(p[1], p[3]), over_white(p[0], p[3])]);
        }
        out.resize(start + stride, 0);
    }
    out
}

pub fn png_to_dib24(data: &[u8]) -> Option<Vec<u8>> {
    let (w, h, rgba) = png_to_rgba(data)?;
    Some(rgba_to_dib24(w, h, &rgba))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(w: i32, h: i32, bpp: u16, compression: u32) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&40u32.to_le_bytes());
        v.extend_from_slice(&w.to_le_bytes());
        v.extend_from_slice(&h.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&bpp.to_le_bytes());
        v.extend_from_slice(&compression.to_le_bytes());
        v.extend_from_slice(&[0u8; 20]);
        v
    }

    #[test]
    fn bottom_up_24bpp_with_row_padding() {
        // 1x2: 下の行（先に並ぶ）が青、上の行が赤。1 行 3 バイト + 1 バイトの詰め物
        let mut d = header(1, 2, 24, BI_RGB);
        d.extend_from_slice(&[255, 0, 0, 0]);
        d.extend_from_slice(&[0, 0, 255, 0]);
        let (w, h, px) = dib_to_rgba(&d).unwrap();
        assert_eq!((w, h), (1, 2));
        assert_eq!(px, vec![255, 0, 0, 255, 0, 0, 255, 255]);
    }

    #[test]
    fn top_down_32bpp_zero_alpha_is_opaque() {
        let mut d = header(2, -1, 32, BI_RGB);
        d.extend_from_slice(&[0, 255, 0, 0, 10, 20, 30, 0]);
        let (_, _, px) = dib_to_rgba(&d).unwrap();
        assert_eq!(px, vec![0, 255, 0, 255, 30, 20, 10, 255]);
    }

    #[test]
    fn bitfields_with_alpha_after_header() {
        let mut d = header(1, 1, 32, BI_ALPHABITFIELDS);
        for m in [0x00ff_0000u32, 0x0000_ff00, 0x0000_00ff, 0xff00_0000] {
            d.extend_from_slice(&m.to_le_bytes());
        }
        d.extend_from_slice(&[3, 2, 1, 128]);
        let (_, _, px) = dib_to_rgba(&d).unwrap();
        assert_eq!(px, vec![1, 2, 3, 128]);
    }

    #[test]
    fn palette_1bpp() {
        let mut d = header(2, 1, 1, BI_RGB);
        d.extend_from_slice(&[0, 0, 0, 0, 255, 255, 255, 0]);
        d.extend_from_slice(&[0b0100_0000, 0, 0, 0]);
        let (_, _, px) = dib_to_rgba(&d).unwrap();
        assert_eq!(px, vec![0, 0, 0, 255, 255, 255, 255, 255]);
    }

    #[test]
    fn rejects_truncated_and_compressed() {
        let mut d = header(4, 4, 24, BI_RGB);
        d.extend_from_slice(&[0; 10]);
        assert!(dib_to_rgba(&d).is_none());
        assert!(dib_to_rgba(&header(1, 1, 8, 1)).is_none());
        assert!(dib_to_rgba(&[0; 8]).is_none());
    }

    #[test]
    fn png_dib_roundtrip_composites_over_white() {
        let rgba = [255, 0, 0, 255, 0, 0, 0, 0, 0, 0, 255, 128];
        let png = rgba_to_png(3, 1, &rgba).unwrap();
        assert_eq!(crate::png_dimensions(&png), Some((3, 1)));
        let dib = png_to_dib24(&png).unwrap();
        let (w, h, back) = dib_to_rgba(&dib).unwrap();
        assert_eq!((w, h), (3, 1));
        // 透明は白、半透明の青は白と混ざる
        assert_eq!(back, vec![255, 0, 0, 255, 255, 255, 255, 255, 127, 127, 255, 255]);
    }
}
