use anyhow::{Context, Result, anyhow, bail};
use arboard::{Clipboard, Error as ClipboardError};
use image::{DynamicImage, ImageFormat, RgbaImage};
use std::io::Cursor;

/// Read the current clipboard image and encode it as PNG.
///
/// Clipboard access is blocking and must be called from `spawn_blocking`.
pub fn read_image_png() -> Result<Vec<u8>> {
    let mut clipboard = Clipboard::new().context("cannot access the system clipboard")?;
    let image = match clipboard.get_image() {
        Ok(image) => image,
        Err(ClipboardError::ContentNotAvailable) => {
            bail!("the clipboard does not contain an image")
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
    let mut output = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(image)
        .write_to(&mut output, ImageFormat::Png)
        .context("cannot encode the clipboard image as PNG")?;
    Ok(output.into_inner())
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
}
