use anyhow::{Context, Result, anyhow, bail};
use arboard::{Clipboard, Error as ClipboardError};
use image::{DynamicImage, ImageFormat, RgbaImage};
use std::io::Cursor;

/// Read the current clipboard image and encode it as PNG.
///
/// Clipboard access is blocking and must be called from `spawn_blocking`.
pub fn read_image_png() -> Result<Vec<u8>> {
    #[cfg(windows)]
    if let Ok(bytes) = windows::read_image_png() {
        return Ok(bytes);
    }
    read_arboard_image_png()
}

fn read_arboard_image_png() -> Result<Vec<u8>> {
    let mut clipboard = Clipboard::new().context("cannot access the system clipboard")?;
    let image = match clipboard.get_image() {
        Ok(image) => image,
        Err(ClipboardError::ContentNotAvailable) => {
            bail!("the clipboard does not contain an image")
        }
        Err(ClipboardError::ConversionFailure) => {
            return Err(anyhow!("the clipboard image could not be decoded"));
        }
        Err(error) => {
            return Err(anyhow!(error)).context("cannot read an image from the clipboard");
        }
    };
    encode_png(image.width, image.height, image.bytes.into_owned())
}

fn encode_png(width: usize, height: usize, bytes: Vec<u8>) -> Result<Vec<u8>> {
    let width = u32::try_from(width).context("clipboard image width is too large")?;
    let height = u32::try_from(height).context("clipboard image height is too large")?;
    let image = RgbaImage::from_raw(width, height, bytes)
        .ok_or_else(|| anyhow!("clipboard image buffer size does not match its dimensions"))?;
    write_dynamic_png(DynamicImage::ImageRgba8(image))
}

fn write_dynamic_png(image: DynamicImage) -> Result<Vec<u8>> {
    let mut output = Cursor::new(Vec::new());
    image
        .write_to(&mut output, ImageFormat::Png)
        .context("cannot encode the clipboard image as PNG")?;
    Ok(output.into_inner())
}

#[cfg(windows)]
fn encoded_bytes_to_png(bytes: &[u8]) -> Result<Vec<u8>> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Ok(bytes.to_vec());
    }
    let image = image::load_from_memory(bytes).context("cannot decode clipboard image bytes")?;
    write_dynamic_png(image)
}

#[cfg(windows)]
fn dib_bytes_to_png(dib: &[u8]) -> Result<Vec<u8>> {
    if let Ok(decoder) = image::codecs::bmp::BmpDecoder::new_without_file_header(Cursor::new(dib))
        && let Ok(image) = DynamicImage::from_decoder(decoder)
    {
        return write_dynamic_png(image);
    }
    let bmp = wrap_dib_as_bmp(dib)?;
    let image = image::load_from_memory_with_format(&bmp, ImageFormat::Bmp)
        .context("cannot decode clipboard DIB")?;
    write_dynamic_png(image)
}

#[cfg(windows)]
fn wrap_dib_as_bmp(dib: &[u8]) -> Result<Vec<u8>> {
    if dib.starts_with(b"BM") {
        return Ok(dib.to_vec());
    }
    let pixel_offset = 14u32
        .checked_add(dib_pixel_data_offset(dib)?)
        .ok_or_else(|| anyhow!("clipboard DIB is too large"))?;
    let file_size = 14u32
        .checked_add(u32::try_from(dib.len()).context("clipboard DIB is too large")?)
        .ok_or_else(|| anyhow!("clipboard DIB is too large"))?;
    let mut bmp = Vec::with_capacity(14 + dib.len());
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&file_size.to_le_bytes());
    bmp.extend_from_slice(&0u32.to_le_bytes());
    bmp.extend_from_slice(&pixel_offset.to_le_bytes());
    bmp.extend_from_slice(dib);
    Ok(bmp)
}

#[cfg(windows)]
fn dib_pixel_data_offset(dib: &[u8]) -> Result<u32> {
    if dib.len() < 4 {
        bail!("clipboard DIB is too small");
    }
    let header_size = u32::from_le_bytes(dib[0..4].try_into().expect("header size is 4 bytes"));
    if header_size < 12 || header_size as usize > dib.len() {
        bail!("clipboard DIB header is invalid");
    }

    let extra = if header_size == 12 {
        if dib.len() < 12 {
            bail!("clipboard DIB header is invalid");
        }
        let bit_count = u16::from_le_bytes(dib[10..12].try_into().expect("bit count is 2 bytes"));
        if bit_count <= 8 {
            (1u32 << bit_count).saturating_mul(3)
        } else {
            0
        }
    } else {
        if dib.len() < 20 {
            bail!("clipboard DIB header is invalid");
        }
        let bit_count = u16::from_le_bytes(dib[14..16].try_into().expect("bit count is 2 bytes"));
        let compression =
            u32::from_le_bytes(dib[16..20].try_into().expect("compression is 4 bytes"));
        let clr_used = if header_size >= 36 && dib.len() >= 36 {
            u32::from_le_bytes(dib[32..36].try_into().expect("color table size is 4 bytes"))
        } else {
            0
        };
        const BI_BITFIELDS: u32 = 3;
        if header_size == 40 && compression == BI_BITFIELDS {
            12
        } else if bit_count <= 8 {
            let entries = if clr_used == 0 {
                1u32 << bit_count
            } else {
                clr_used
            };
            entries.saturating_mul(4)
        } else {
            clr_used.saturating_mul(4)
        }
    };

    header_size
        .checked_add(extra)
        .filter(|&offset| offset as usize <= dib.len())
        .ok_or_else(|| anyhow!("clipboard DIB pixel offset is invalid"))
}

#[cfg(windows)]
mod windows {
    use super::{dib_bytes_to_png, encoded_bytes_to_png};
    use anyhow::{Context, Result, anyhow, bail};
    use clipboard_win::{Clipboard, Getter, formats, raw, register_format};

    /// Read Windows clipboard image formats that `arboard` misses or aborts on.
    ///
    /// Win+Shift+S, browsers, and many screenshot tools often publish `PNG` or
    /// `CF_DIB` without a usable `CF_DIBV5`. `arboard` treats a failed PNG decode
    /// as terminal and does not fall back to DIB.
    pub(super) fn read_image_png() -> Result<Vec<u8>> {
        let _clipboard = Clipboard::new_attempts(10)
            .map_err(|error| anyhow!(error.to_string()))
            .context("cannot access the system clipboard")?;

        for name in ["PNG", "image/png"] {
            if let Some(bytes) = read_registered_format(name)
                && let Ok(png) = encoded_bytes_to_png(&bytes)
            {
                return Ok(png);
            }
        }

        for format in [formats::CF_DIB, formats::CF_DIBV5] {
            if let Some(dib) = read_raw_format(format)
                && let Ok(png) = dib_bytes_to_png(&dib)
            {
                return Ok(png);
            }
        }

        if raw::is_format_avail(formats::CF_BITMAP) {
            let mut bmp = Vec::new();
            if formats::Bitmap.read_clipboard(&mut bmp).is_ok()
                && let Ok(png) = encoded_bytes_to_png(&bmp)
            {
                return Ok(png);
            }
        }

        bail!("the clipboard does not contain an image")
    }

    fn read_registered_format(name: &str) -> Option<Vec<u8>> {
        read_raw_format(register_format(name)?.get())
    }

    fn read_raw_format(format: u32) -> Option<Vec<u8>> {
        if !raw::is_format_avail(format) {
            return None;
        }
        let mut data = Vec::new();
        raw::get_vec(format, &mut data).ok()?;
        (!data.is_empty()).then_some(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgba_clipboard_data_is_encoded_as_png() {
        let png = encode_png(1, 1, vec![12, 34, 56, 255]).unwrap();
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        let decoded = image::load_from_memory_with_format(&png, ImageFormat::Png).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (1, 1));
    }

    #[test]
    fn mismatched_clipboard_buffer_is_rejected() {
        let error = encode_png(2, 1, vec![0; 4]).unwrap_err();
        assert!(format!("{error:#}").contains("buffer size"));
    }

    #[cfg(windows)]
    #[test]
    fn valid_png_bytes_are_returned_unchanged() {
        let png = encode_png(1, 1, vec![12, 34, 56, 255]).unwrap();
        assert_eq!(encoded_bytes_to_png(&png).unwrap(), png);
    }

    #[cfg(windows)]
    #[test]
    fn thirty_two_bit_dib_is_decoded_as_png() {
        let dib = packed_dib32(1, 1, &[56, 34, 12, 255]);
        let png = dib_bytes_to_png(&dib).unwrap();
        let decoded = image::load_from_memory_with_format(&png, ImageFormat::Png)
            .unwrap()
            .to_rgba8();
        assert_eq!((decoded.width(), decoded.height()), (1, 1));
        assert_eq!(&decoded.into_raw()[..3], &[12, 34, 56]);
    }

    #[cfg(windows)]
    #[test]
    fn wrapped_dib_keeps_a_bmp_file_header() {
        let dib = packed_dib32(1, 1, &[56, 34, 12, 255]);
        let bmp = wrap_dib_as_bmp(&dib).unwrap();
        assert!(bmp.starts_with(b"BM"));
        assert_eq!(&bmp[14..], dib.as_slice());
        assert_eq!(&bmp[10..14], &54u32.to_le_bytes());
    }

    #[cfg(windows)]
    #[test]
    fn invalid_dib_is_rejected() {
        let error = dib_bytes_to_png(&[1, 2, 3, 4]).unwrap_err();
        assert!(format!("{error:#}").contains("DIB"));
    }

    #[cfg(windows)]
    fn packed_dib32(width: i32, height: i32, pixels_bgra: &[u8]) -> Vec<u8> {
        let mut dib = vec![0_u8; 40];
        dib[0..4].copy_from_slice(&40u32.to_le_bytes());
        dib[4..8].copy_from_slice(&width.to_le_bytes());
        dib[8..12].copy_from_slice(&height.to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&32u16.to_le_bytes());
        dib[20..24].copy_from_slice(&(pixels_bgra.len() as u32).to_le_bytes());
        dib.extend_from_slice(pixels_bgra);
        dib
    }
}
