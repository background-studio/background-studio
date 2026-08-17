use std::path::{Path, PathBuf};

use image::GenericImageView;

use crate::core::{SharedMediaKind, ThumbnailSource};

pub const THUMBNAIL_WIDTH: u32 = 160;
pub const THUMBNAIL_HEIGHT: u32 = 104;
pub const TEXTURE_CACHE_BYTES: usize = 64 * 1024 * 1024;

pub struct ThumbnailPixels {
    pub key: String,
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

pub fn generate(source: &ThumbnailSource, cache_dir: &Path) -> Result<ThumbnailPixels, String> {
    std::fs::create_dir_all(cache_dir).map_err(|error| error.to_string())?;
    let key = cache_key(source);
    let cache_path = cache_dir.join(format!("{key}.png"));
    let image = if cache_path.is_file() {
        image::open(&cache_path).map_err(|error| error.to_string())?
    } else {
        let image = match source.kind {
            SharedMediaKind::Image => {
                image::open(&source.path).map_err(|error| format!("读取图片缩略图失败：{error}"))?
            }
            SharedMediaKind::Video => shell_thumbnail(&source.path)
                .ok_or_else(|| "Windows 无法生成视频首帧。".to_string())?,
        };
        let image = image.thumbnail(THUMBNAIL_WIDTH, THUMBNAIL_HEIGHT);
        image
            .save_with_format(&cache_path, image::ImageFormat::Png)
            .map_err(|error| format!("写入缩略图缓存失败：{error}"))?;
        image
    };
    let (width, height) = image.dimensions();
    Ok(ThumbnailPixels {
        key,
        width: width as usize,
        height: height as usize,
        rgba: image.into_rgba8().into_raw(),
    })
}

pub fn cache_key(source: &ThumbnailSource) -> String {
    format!(
        "{}-{}x{}",
        source.cache_key, THUMBNAIL_WIDTH, THUMBNAIL_HEIGHT
    )
}

#[cfg(windows)]
fn shell_thumbnail(path: &Path) -> Option<image::DynamicImage> {
    use std::{ffi::c_void, mem::size_of};

    use windows::{
        core::HSTRING,
        Win32::{
            Foundation::SIZE,
            Graphics::Gdi::{
                DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO,
                BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HGDIOBJ,
            },
            System::Com::{CoInitializeEx, CoUninitialize, IBindCtx, COINIT_MULTITHREADED},
            UI::Shell::{
                IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_BIGGERSIZEOK,
                SIIGBF_SCALEUP, SIIGBF_THUMBNAILONLY,
            },
        },
    };

    unsafe {
        let initialized = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
        let path = HSTRING::from(path.as_os_str());
        let factory: IShellItemImageFactory =
            SHCreateItemFromParsingName(&path, None::<&IBindCtx>).ok()?;
        let bitmap = factory
            .GetImage(
                SIZE {
                    cx: THUMBNAIL_WIDTH as i32,
                    cy: THUMBNAIL_HEIGHT as i32,
                },
                SIIGBF_THUMBNAILONLY | SIIGBF_BIGGERSIZEOK | SIIGBF_SCALEUP,
            )
            .ok()?;

        let mut object = BITMAP::default();
        let read = GetObjectW(
            HGDIOBJ(bitmap.0),
            size_of::<BITMAP>() as i32,
            Some((&mut object as *mut BITMAP).cast::<c_void>()),
        );
        if read == 0 || object.bmWidth <= 0 || object.bmHeight == 0 {
            let _ = DeleteObject(HGDIOBJ(bitmap.0));
            if initialized {
                CoUninitialize();
            }
            return None;
        }
        let width = object.bmWidth as u32;
        let height = object.bmHeight.unsigned_abs();
        let mut pixels = vec![0u8; width as usize * height as usize * 4];
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                biSizeImage: (pixels.len()) as u32,
                ..Default::default()
            },
            ..Default::default()
        };
        let hdc = GetDC(None);
        let copied = GetDIBits(
            hdc,
            bitmap,
            0,
            height,
            Some(pixels.as_mut_ptr().cast::<c_void>()),
            &mut info,
            DIB_RGB_COLORS,
        );
        let _ = ReleaseDC(None, hdc);
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        if initialized {
            CoUninitialize();
        }
        if copied == 0 {
            return None;
        }
        for pixel in pixels.chunks_exact_mut(4) {
            pixel.swap(0, 2);
            pixel[3] = 255;
        }
        image::RgbaImage::from_raw(width, height, pixels).map(image::DynamicImage::ImageRgba8)
    }
}

#[cfg(not(windows))]
fn shell_thumbnail(_path: &Path) -> Option<image::DynamicImage> {
    None
}

pub fn cache_directory(data_dir: &Path) -> PathBuf {
    data_dir.join("thumbnail-cache")
}
