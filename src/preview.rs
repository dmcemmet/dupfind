use std::fs::File;
use std::io::Read;
use std::path::Path;

use image::DynamicImage;
use ratatui::layout::Rect;
use ratatui_image::Resize;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::Protocol;

const TEXT_PREVIEW_BYTES: usize = 4096;
const THUMB_MAX_DIM: u32 = 1000; // max pixels for thumbnail decode (fills preview pane)

const IMAGE_EXTS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "bmp", "webp", "tiff", "tif", "ico",
];
const TEXT_EXTS: &[&str] = &[
    "txt", "md", "rs", "py", "js", "ts", "json", "toml", "yaml", "yml", "xml", "html", "css",
    "csv", "log", "sh", "bash", "c", "cpp", "h", "hpp", "java", "go", "rb", "php", "sql", "conf",
    "cfg", "ini",
];

/// Returns true if the file extension indicates an image format.
pub fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMAGE_EXTS.contains(&e.to_lowercase().as_str()))
}

/// Returns true if the file extension indicates a text format.
pub fn is_text(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| TEXT_EXTS.contains(&e.to_lowercase().as_str()))
}

/// Loads the first 4KB of a text file as a UTF-8 string preview.
pub fn load_text_preview(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let mut buf = vec![0u8; TEXT_PREVIEW_BYTES];
    let n = file.read(&mut buf).ok()?;
    buf.truncate(n);
    // Check if content looks like valid UTF-8 text
    String::from_utf8(buf).ok()
}

/// Load image as a thumbnail. Reads full file but resizes immediately to save memory.
/// For network drives this is the minimum we can do without format-specific thumbnail extraction.
pub fn load_image_thumbnail(path: &Path) -> Option<DynamicImage> {
    let img = image::open(path).ok()?;
    // Resize to thumbnail to keep memory low
    Some(img.thumbnail(THUMB_MAX_DIM, THUMB_MAX_DIM))
}

/// Creates a ratatui-image protocol for rendering an image in the terminal.
pub fn make_image_protocol(
    picker: &mut Picker,
    img: &DynamicImage,
    area: Rect,
) -> Option<Protocol> {
    picker
        .new_protocol(img.clone(), area, Resize::Fit(None))
        .ok()
}
