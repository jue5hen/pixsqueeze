use base64::{engine::general_purpose::STANDARD, Engine as _};
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use image::imageops::FilterType;
use image::{DynamicImage, ImageDecoder, ImageReader, RgbaImage};
use std::fs::{self, File};
use std::io::{BufReader, Cursor, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use crate::resources::{self, ResourcePolicy};

pub const WEBP_MAX_DIMENSION: u32 = 16_383;
/// 扫描 EXIF 时最多读取的字节数，防止病态文件把整盘读满。
const EXIF_SCAN_LIMIT: u64 = 16 * 1024 * 1024;
const EXIF_PAYLOAD_LIMIT: usize = 16 * 1024 * 1024;
const ICC_PAYLOAD_LIMIT: usize = 16 * 1024 * 1024;
const EXIF_PREFIX: &[u8] = b"Exif\0\0";
const ICC_PREFIX: &[u8] = b"ICC_PROFILE\0";
pub const MAX_JPEG_ICC_BYTES: usize = 255 * (0xFFFF - 2 - ICC_PREFIX.len() - 2);
/// 解码缓存的内存预算：按字节而不是按张数淘汰（PS-A-009）。
const MAX_CACHE_BYTES: usize = 256 * 1024 * 1024;

#[derive(Clone, Debug, Default)]
pub struct ImageMetadata {
    pub exif: Option<Vec<u8>>,
    pub icc: Option<Vec<u8>>,
    pub warnings: Vec<String>,
    pub frames: Option<u32>,
    pub pages: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct OutputMetadata<'a> {
    pub exif: Option<&'a [u8]>,
    pub icc: Option<&'a [u8]>,
}

pub fn metadata_warnings(fmt: OutFormat, metadata: OutputMetadata<'_>) -> Vec<String> {
    let mut warnings = Vec::new();
    if let Some(exif) = metadata.exif {
        if exif.len() < 6 || !exif.starts_with(EXIF_PREFIX) {
            warnings.push("EXIF 数据格式无效，像素已保存但 EXIF 未写入".into());
        } else if fmt == OutFormat::Jpeg && exif.len() + 2 > 0xFFFF {
            warnings.push(format!(
                "EXIF 段 {} 字节超过 JPEG APP1 64 KB 上限；像素已保存但 EXIF 未完整保留",
                exif.len()
            ));
        }
    }
    if let Some(icc) = metadata.icc {
        if fmt == OutFormat::Jpeg && icc.len() > MAX_JPEG_ICC_BYTES {
            warnings.push(format!(
                "ICC 配置 {} 字节超过 JPEG APP2 分片上限；像素已保存但 ICC 未保留",
                icc.len()
            ));
        }
    }
    warnings
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadOptions {
    pub keep_exif: bool,
    pub keep_icc: bool,
}

impl Default for LoadOptions {
    fn default() -> Self {
        Self {
            keep_exif: true,
            keep_icc: true,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct MetadataLimits {
    pub scan_bytes: u64,
    pub exif_bytes: usize,
    pub icc_bytes: usize,
}

impl Default for MetadataLimits {
    fn default() -> Self {
        Self {
            scan_bytes: EXIF_SCAN_LIMIT,
            exif_bytes: EXIF_PAYLOAD_LIMIT,
            icc_bytes: ICC_PAYLOAD_LIMIT,
        }
    }
}

struct Cached {
    path: String,
    mtime: Option<SystemTime>,
    image: Arc<RgbaImage>,
    metadata: ImageMetadata,
    options: LoadOptions,
}

fn cache_bytes(cache: &[Cached]) -> usize {
    cache
        .iter()
        .map(|c| {
            (c.image.width() as usize)
                .saturating_mul(c.image.height() as usize)
                .saturating_mul(4)
                .saturating_add(c.metadata.exif.as_ref().map_or(0, Vec::len))
                .saturating_add(c.metadata.icc.as_ref().map_or(0, Vec::len))
        })
        .fold(0usize, usize::saturating_add)
}

static CACHE: Mutex<Vec<Cached>> = Mutex::new(Vec::new());

pub fn clear_cache() {
    CACHE.lock().unwrap().clear();
}

pub fn load_image(path: &str) -> Result<(Arc<RgbaImage>, Option<Vec<u8>>), String> {
    load_image_with_options(path, LoadOptions::default())
        .map(|(image, metadata)| (image, metadata.exif))
}

pub fn load_image_with_options(
    path: &str,
    options: LoadOptions,
) -> Result<(Arc<RgbaImage>, ImageMetadata), String> {
    load_image_with_policy(path, options, ResourcePolicy::default())
}

pub(crate) fn load_image_with_policy(
    path: &str,
    options: LoadOptions,
    policy: ResourcePolicy,
) -> Result<(Arc<RgbaImage>, ImageMetadata), String> {
    let p = Path::new(path);
    let mtime = fs::metadata(p).ok().and_then(|m| m.modified().ok());
    let path_key = path.replace('\\', "/").to_lowercase();
    {
        let cache = CACHE.lock().unwrap();
        for item in cache.iter() {
            if item.path == path_key && item.mtime == mtime && item.options == options {
                return Ok((item.image.clone(), item.metadata.clone()));
            }
        }
    }
    let reader = ImageReader::open(p)
        .map_err(|e| format!("无法打开图片：{e}"))?
        .with_guessed_format()
        .map_err(|e| format!("无法识别图片格式：{e}"))?;
    // 像素上限预检必须与真正解码使用同一套格式判定（都按内容猜），
    // 否则扩展名与内容不符的文件会在这里按扩展名选错解码器而失败。
    let (w, h) = ImageReader::open(p)
        .map_err(|e| format!("无法打开图片：{e}"))?
        .with_guessed_format()
        .map_err(|e| format!("无法识别图片格式：{e}"))?
        .into_dimensions()
        .map_err(|e| format!("无法读取图片信息：{e}"))?;
    resources::check_import(policy, w, h)?;
    let mut metadata = read_metadata_with_limits(p, options, MetadataLimits::default());
    inspect_multiframe(p, &mut metadata);
    let mut decoder = reader
        .into_decoder()
        .map_err(|e| format!("无法解码图片：{e}"))?;
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut dynimg =
        DynamicImage::from_decoder(decoder).map_err(|e| format!("无法解码图片：{e}"))?;
    dynimg.apply_orientation(orientation);
    let image = Arc::new(dynimg.into_rgba8());
    let image_bytes = (image.width() as usize)
        .saturating_mul(image.height() as usize)
        .saturating_mul(4);
    {
        let mut cache = CACHE.lock().unwrap();
        cache.retain(|c| c.path != path_key);
        // A single oversized image must not bypass the cache budget forever.
        if image_bytes <= MAX_CACHE_BYTES {
            cache.push(Cached {
                path: path_key,
                mtime,
                image: image.clone(),
                metadata: metadata.clone(),
                options,
            });
            // 按「字节预算」淘汰，而不是按张数：三张 24MP 就接近 300 MB（PS-A-009）。
            while cache.len() > 1 && cache_bytes(&cache) > MAX_CACHE_BYTES {
                cache.remove(0);
            }
        }
    }
    Ok((image, metadata))
}

pub fn smart_resize(img: &RgbaImage, nw: u32, nh: u32) -> RgbaImage {
    let (w, h) = (img.width(), img.height());
    if nw == 0 || nh == 0 {
        return RgbaImage::new(1, 1);
    }
    if (nw, nh) == (w, h) {
        return img.clone();
    }
    // 折半阶段不再先克隆整张图：第一次真正缩小才产生新缓冲（PS-A-017）。
    let mut work: Option<RgbaImage> = None;
    loop {
        let (cw, ch) = match &work {
            Some(buf) => (buf.width(), buf.height()),
            None => (w, h),
        };
        if cw < nw.saturating_mul(2) || ch < nh.saturating_mul(2) {
            break;
        }
        let half_w = (cw / 2).max(1);
        let half_h = (ch / 2).max(1);
        let next = {
            let src: &RgbaImage = work.as_ref().unwrap_or(img);
            image::imageops::resize(src, half_w, half_h, FilterType::Triangle)
        };
        work = Some(next);
    }
    match work {
        // 折半后正好是目标尺寸时别再跑一遍 Lanczos（PS-A-043：50% 场景纯空转）。
        Some(buf) if (buf.width(), buf.height()) == (nw.max(1), nh.max(1)) => buf,
        Some(buf) => image::imageops::resize(&buf, nw.max(1), nh.max(1), FilterType::Lanczos3),
        None => image::imageops::resize(img, nw.max(1), nh.max(1), FilterType::Lanczos3),
    }
}

pub fn preview_base(img: &RgbaImage, max_side: u32) -> RgbaImage {
    let (w, h) = (img.width(), img.height());
    if w.max(h) <= max_side {
        return img.clone();
    }
    let k = max_side as f64 / w.max(h) as f64;
    smart_resize(
        img,
        ((w as f64 * k).round() as u32).max(1),
        ((h as f64 * k).round() as u32).max(1),
    )
}

pub fn unsharp_mask(img: &RgbaImage) -> RgbaImage {
    let blurred = image::imageops::blur(img, 1.0);
    let mut out = img.clone();
    for (x, y, px) in out.enumerate_pixels_mut() {
        let b = blurred.get_pixel(x, y);
        for c in 0..3 {
            let orig = px[c] as i32;
            let diff = orig - b[c] as i32;
            if diff.abs() > 2 {
                px[c] = (orig + diff * 70 / 100).clamp(0, 255) as u8;
            }
        }
    }
    out
}

fn flatten_white(img: &RgbaImage) -> Vec<u8> {
    let capacity = (img.width() as usize)
        .saturating_mul(img.height() as usize)
        .saturating_mul(3);
    let mut rgb = Vec::with_capacity(capacity);
    for p in img.pixels() {
        let a = p[3] as u32;
        for c in 0..3 {
            let v = (p[c] as u32 * a + 255 * (255 - a)) / 255;
            rgb.push(v as u8);
        }
    }
    rgb
}

fn encode_jpeg(
    img: &RgbaImage,
    quality: u32,
    metadata: OutputMetadata<'_>,
) -> Result<Vec<u8>, String> {
    let rgb = flatten_white(img);
    let mut out = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(
        Cursor::new(&mut out),
        quality.clamp(1, 100) as u8,
    );
    encoder
        .encode(
            &rgb,
            img.width(),
            img.height(),
            image::ExtendedColorType::Rgb8,
        )
        .map_err(|e| format!("JPEG 编码失败：{e}"))?;
    if let Some(exif) = metadata.exif {
        let mut exif = exif.to_vec();
        patch_exif_orientation(&mut exif);
        out = insert_jpeg_exif(out, &exif);
    }
    if let Some(icc) = metadata.icc {
        out = insert_jpeg_icc(out, icc);
    }
    Ok(out)
}

fn encode_png_rgba(
    img: &RgbaImage,
    quality: u32,
    metadata: OutputMetadata<'_>,
) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, img.width(), img.height());
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        // 原来的 Best 在 24 MP 上要十几秒、换来的体积收益却只有个位数百分比（PS-A-018）
        encoder.set_compression(png_compression(quality));
        let mut writer = encoder
            .write_header()
            .map_err(|e| format!("PNG 编码失败：{e}"))?;
        writer
            .write_image_data(img.as_raw())
            .map_err(|e| format!("PNG 编码失败：{e}"))?;
    }
    if let Some(icc) = metadata.icc {
        out = insert_png_icc(out, icc)?;
    }
    if let Some(ex) = metadata.exif {
        let mut normalized = ex.to_vec();
        patch_exif_orientation(&mut normalized);
        out = insert_png_exif(out, &normalized)?;
    }
    Ok(out)
}

/// 质量滑块 ⇄ PNG 压缩档位的单调映射：高质量用默认档（体积优先），低质量用快速档。
fn png_compression(quality: u32) -> png::Compression {
    if quality >= 60 {
        png::Compression::Default
    } else {
        png::Compression::Fast
    }
}

pub fn encode_png_paletted(
    img: &RgbaImage,
    colors: u32,
    dithering: f32,
    metadata: OutputMetadata<'_>,
) -> Result<Vec<u8>, String> {
    let pixels: Vec<imagequant::RGBA> = img
        .pixels()
        .map(|p| imagequant::RGBA::new(p[0], p[1], p[2], p[3]))
        .collect();
    let mut attr = imagequant::Attributes::new();
    attr.set_max_colors(colors.clamp(2, 256))
        .map_err(|e| format!("PNG 调色板量化失败：{e}"))?;
    attr.set_quality(0, 100)
        .map_err(|e| format!("PNG 调色板量化失败：{e}"))?;
    let mut liq = attr
        .new_image(pixels, img.width() as usize, img.height() as usize, 0.0)
        .map_err(|e| format!("PNG 调色板量化失败：{e}"))?;
    let mut result = attr
        .quantize(&mut liq)
        .map_err(|e| format!("PNG 调色板量化失败：{e}"))?;
    let _ = result.set_dithering_level(dithering.clamp(0.0, 1.0));
    let (palette, indices) = result
        .remapped(&mut liq)
        .map_err(|e| format!("PNG 调色板生成失败：{e}"))?;
    let has_alpha = palette.iter().any(|c| c.a < 255);
    let mut rgb_palette = Vec::with_capacity(palette.len() * 3);
    for c in &palette {
        rgb_palette.extend_from_slice(&[c.r, c.g, c.b]);
    }
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, img.width(), img.height());
        encoder.set_color(png::ColorType::Indexed);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_palette(rgb_palette);
        if has_alpha {
            encoder.set_trns(palette.iter().map(|c| c.a).collect::<Vec<u8>>());
        }
        let mut writer = encoder
            .write_header()
            .map_err(|e| format!("PNG 编码失败：{e}"))?;
        writer
            .write_image_data(&indices)
            .map_err(|e| format!("PNG 编码失败：{e}"))?;
    }
    if let Some(icc) = metadata.icc {
        out = insert_png_icc(out, icc)?;
    }
    if let Some(ex) = metadata.exif {
        // 调色板路径绕过 encode_image 的 PNG 分支，必须在这里做同样的一次性方向归一；
        // 否则像素已旋转、eXIf 仍写着 6/8，用户再打开就会被二次旋转。
        let mut normalized = ex.to_vec();
        patch_exif_orientation(&mut normalized);
        out = insert_png_exif(out, &normalized)?;
    }
    Ok(out)
}

fn encode_webp(img: &RgbaImage, quality: u32) -> Result<Vec<u8>, String> {
    if img.width() > WEBP_MAX_DIMENSION || img.height() > WEBP_MAX_DIMENSION {
        return Err(format!(
            "WebP 单边上限为 {} 像素，当前待编码尺寸为 {}×{}",
            WEBP_MAX_DIMENSION,
            img.width(),
            img.height()
        ));
    }
    let encoder = webp::Encoder::from_rgba(img.as_raw(), img.width(), img.height());
    let mem = encoder
        .encode_simple(false, quality.clamp(1, 100) as f32)
        .map_err(|e| format!("WebP 编码失败：{e:?}"))?;
    Ok(mem.to_vec())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutFormat {
    Jpeg,
    Png,
    Webp,
}

impl OutFormat {
    pub fn mime(self) -> &'static str {
        match self {
            OutFormat::Jpeg => "image/jpeg",
            OutFormat::Png => "image/png",
            OutFormat::Webp => "image/webp",
        }
    }
    pub fn ext(self) -> &'static str {
        match self {
            OutFormat::Jpeg => "jpeg",
            OutFormat::Png => "png",
            OutFormat::Webp => "webp",
        }
    }

    /// 源扩展名是否与该格式同族。JPEG 的 jpg/jpeg 视为同族；其余严格相等。
    /// 输出文件名的扩展名只能由「实际编码格式」决定，不能用源扩展名猜。
    pub fn matches_ext(self, ext: &str) -> bool {
        let e = ext.trim_start_matches('.').to_ascii_lowercase();
        match self {
            OutFormat::Jpeg => e == "jpg" || e == "jpeg",
            OutFormat::Png => e == "png",
            OutFormat::Webp => e == "webp",
        }
    }

    pub fn validate_dimensions(self, w: u32, h: u32) -> Result<(), String> {
        if self == OutFormat::Webp && (w > WEBP_MAX_DIMENSION || h > WEBP_MAX_DIMENSION) {
            return Err(format!(
                "WebP 单边上限为 {} 像素，当前待编码尺寸为 {}×{}",
                WEBP_MAX_DIMENSION, w, h
            ));
        }
        Ok(())
    }
}

pub fn encode_image(
    img: &RgbaImage,
    fmt: OutFormat,
    quality: u32,
    metadata: OutputMetadata<'_>,
) -> Result<Vec<u8>, String> {
    fmt.validate_dimensions(img.width(), img.height())?;
    match fmt {
        OutFormat::Jpeg => encode_jpeg(img, quality, metadata),
        // 三条输出路径共用同一套 EXIF 语义：像素在 load_image 里已按 orientation 摆正，
        // 因此写出的 orientation 必须归一为 1，否则看图软件会二次旋转。
        OutFormat::Png => encode_png_rgba(img, quality, metadata),
        OutFormat::Webp => {
            let mut data = encode_webp(img, quality)?;
            let mut exif = metadata.exif.map(|ex| ex.to_vec());
            if let Some(ex) = exif.as_mut() {
                patch_exif_orientation(ex);
            }
            data = insert_webp_metadata(
                data,
                exif.as_deref(),
                metadata.icc,
                img.width(),
                img.height(),
            )?;
            Ok(data)
        }
    }
}

/// 给简单 WebP（VP8/VP8L）补 EXIF/ICC：必要时插入 VP8X 扩展头并设置标志位。
fn insert_webp_metadata(
    webp: Vec<u8>,
    exif: Option<&[u8]>,
    icc: Option<&[u8]>,
    w: u32,
    h: u32,
) -> Result<Vec<u8>, String> {
    let tiff = exif.map(|exif| exif.strip_prefix(EXIF_PREFIX).unwrap_or(exif));
    let tiff = tiff.filter(|v| !v.is_empty());
    let icc = icc.filter(|v| !v.is_empty());
    if (tiff.is_none() && icc.is_none())
        || webp.len() < 12
        || &webp[0..4] != b"RIFF"
        || &webp[8..12] != b"WEBP"
    {
        return Ok(webp);
    }
    let mut chunks: Vec<([u8; 4], Vec<u8>)> = Vec::new();
    let mut pos = 12usize;
    while pos + 8 <= webp.len() {
        let size = u32::from_le_bytes([webp[pos + 4], webp[pos + 5], webp[pos + 6], webp[pos + 7]])
            as usize;
        let end = pos
            .checked_add(8)
            .and_then(|v| v.checked_add(size))
            .ok_or_else(|| "WebP 块长度溢出".to_string())?;
        if end > webp.len() {
            return Err("WebP 块声明长度超过文件实际长度".into());
        }
        let mut four = [0u8; 4];
        four.copy_from_slice(&webp[pos..pos + 4]);
        if &four != b"EXIF" && &four != b"ICCP" {
            chunks.push((four, webp[pos + 8..pos + 8 + size].to_vec()));
        }
        pos = end
            .checked_add(size & 1)
            .ok_or_else(|| "WebP 块长度溢出".to_string())?;
    }
    if chunks.is_empty() {
        return Ok(webp);
    }
    let mut flags = 0u8;
    if tiff.is_some() {
        flags |= 0x08;
    }
    if icc.is_some() {
        flags |= 0x20;
    }
    if chunks[0].0 == *b"VP8X" && chunks[0].1.len() >= 10 {
        chunks[0].1[0] |= flags;
    } else {
        let mut vp8x = vec![flags, 0, 0, 0];
        let cw = w.max(1) - 1;
        let ch = h.max(1) - 1;
        vp8x.extend_from_slice(&[cw as u8, (cw >> 8) as u8, (cw >> 16) as u8]);
        vp8x.extend_from_slice(&[ch as u8, (ch >> 8) as u8, (ch >> 16) as u8]);
        chunks.insert(0, (*b"VP8X", vp8x));
    }
    if let Some(tiff) = tiff {
        chunks.push((*b"EXIF", tiff.to_vec()));
    }
    if let Some(icc) = icc {
        chunks.push((*b"ICCP", icc.to_vec()));
    }
    let payload_bytes = chunks.iter().try_fold(0usize, |total, (_, payload)| {
        total
            .checked_add(8)
            .and_then(|v| v.checked_add(payload.len()))
            .and_then(|v| v.checked_add(payload.len() & 1))
            .ok_or_else(|| "WebP 元数据容量溢出".to_string())
    })?;
    let mut body = Vec::with_capacity(payload_bytes.saturating_add(4));
    body.extend_from_slice(b"WEBP");
    for (four, payload) in &chunks {
        let size = u32::try_from(payload.len()).map_err(|_| "WebP 块过大".to_string())?;
        body.extend_from_slice(four);
        body.extend_from_slice(&size.to_le_bytes());
        body.extend_from_slice(payload);
        if payload.len() % 2 == 1 {
            body.push(0);
        }
    }
    let riff_size = u32::try_from(body.len()).map_err(|_| "WebP 文件过大".to_string())?;
    let mut out = Vec::with_capacity(body.len() + 8);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&riff_size.to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

pub fn data_url(bytes: &[u8], mime: &str) -> String {
    format!("data:{mime};base64,{}", STANDARD.encode(bytes))
}

/// 只读文件头拿尺寸（按内容判定格式），用于批量前的内存风险预判。
pub fn probe_dimensions(path: &str) -> Option<(u32, u32)> {
    ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// 校验文件头魔数是否与给定输出格式一致。
/// 自检回读时用它把「扩展名 / 内容」的一致性变成硬断言。
pub fn file_matches_format(path: &Path, fmt: OutFormat) -> bool {
    let Ok(mut f) = File::open(path) else {
        return false;
    };
    let mut head = [0u8; 12];
    if f.read_exact(&mut head).is_err() {
        return false;
    }
    matches!(
        (sniff_container(&head), fmt),
        (Some(ExifContainer::Jpeg), OutFormat::Jpeg)
            | (Some(ExifContainer::Png), OutFormat::Png)
            | (Some(ExifContainer::Webp), OutFormat::Webp)
    )
}

pub fn has_alpha(img: &RgbaImage) -> bool {
    img.pixels().any(|p| p[3] < 255)
}

pub fn encode_thumb(img: &RgbaImage) -> Result<String, String> {
    let (w, h) = (img.width(), img.height());
    let k = 96.0 / w.max(h) as f64;
    let thumb = if w.max(h) <= 96 {
        img.clone()
    } else {
        smart_resize(
            img,
            ((w as f64 * k).round() as u32).max(1),
            ((h as f64 * k).round() as u32).max(1),
        )
    };
    let bytes = if has_alpha(&thumb) {
        encode_png_rgba(&thumb, 60, OutputMetadata::default())?
    } else {
        encode_jpeg(&thumb, 85, OutputMetadata::default())?
    };
    Ok(data_url(
        &bytes,
        if has_alpha(&thumb) {
            "image/png"
        } else {
            "image/jpeg"
        },
    ))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExifContainer {
    Jpeg,
    Png,
    Webp,
}

fn sniff_container(head: &[u8]) -> Option<ExifContainer> {
    if head.len() < 12 {
        return None;
    }
    if head[0] == 0xFF && head[1] == 0xD8 {
        return Some(ExifContainer::Jpeg);
    }
    if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(ExifContainer::Png);
    }
    if &head[0..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        return Some(ExifContainer::Webp);
    }
    None
}

/// 把容器里的原始 TIFF 流规范成内部契约：带 `Exif\0\0` 前缀。
fn with_exif_prefix(payload: Vec<u8>) -> Vec<u8> {
    if payload.starts_with(EXIF_PREFIX) {
        payload
    } else {
        let mut out = EXIF_PREFIX.to_vec();
        out.extend_from_slice(&payload);
        out
    }
}

/// Read EXIF/ICC from the source container with checked lengths and a bounded
/// scan budget. A malformed metadata block only produces a warning; the image
/// itself remains decodable.
fn read_metadata_with_limits(
    path: &Path,
    options: LoadOptions,
    limits: MetadataLimits,
) -> ImageMetadata {
    let mut metadata = ImageMetadata::default();
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) => {
            metadata.warnings.push(format!("无法读取元数据：{e}"));
            return metadata;
        }
    };
    let mut reader = BufReader::new(file);
    let mut head = [0u8; 12];
    if let Err(e) = reader.read_exact(&mut head) {
        metadata
            .warnings
            .push(format!("无法读取图片头，元数据未检查：{e}"));
        return metadata;
    }
    match sniff_container(&head) {
        Some(ExifContainer::Jpeg) => {
            read_jpeg_metadata(&mut reader, options, limits, &mut metadata)
        }
        Some(ExifContainer::Png) => read_png_metadata(&mut reader, options, limits, &mut metadata),
        Some(ExifContainer::Webp) => {
            read_webp_metadata(&mut reader, options, limits, &mut metadata)
        }
        None => metadata
            .warnings
            .push("无法识别元数据容器，EXIF/ICC 未检查".into()),
    }
    metadata
}

fn read_file_len<R: Read + Seek>(reader: &mut R) -> Option<u64> {
    let pos = reader.stream_position().ok()?;
    let len = reader.seek(SeekFrom::End(0)).ok()?;
    reader.seek(SeekFrom::Start(pos)).ok()?;
    Some(len)
}

fn scan_exceeded(metadata: &mut ImageMetadata, kind: &str, limit: u64) {
    metadata.warnings.push(format!(
        "{kind} 元数据扫描超过 {limit} 字节预算，已停止检查"
    ));
}

fn assemble_icc_chunks(
    chunks: Vec<(u8, u8, Vec<u8>)>,
    limit: usize,
    metadata: &mut ImageMetadata,
) -> Option<Vec<u8>> {
    if chunks.is_empty() {
        return None;
    }
    let count = chunks[0].1;
    if count == 0
        || chunks
            .iter()
            .any(|(seq, declared, _)| *seq == 0 || *declared != count)
    {
        metadata
            .warnings
            .push("JPEG ICC 分片序号无效，ICC 已忽略".into());
        return None;
    }
    let mut ordered = vec![None; count as usize];
    for (seq, _, data) in chunks {
        let idx = seq as usize - 1;
        if idx >= ordered.len() || ordered[idx].is_some() {
            metadata
                .warnings
                .push("JPEG ICC 分片重复或越界，ICC 已忽略".into());
            return None;
        }
        ordered[idx] = Some(data);
    }
    if ordered.iter().any(Option::is_none) {
        metadata
            .warnings
            .push("JPEG ICC 分片不完整，ICC 已忽略".into());
        return None;
    }
    let total = ordered
        .iter()
        .flatten()
        .try_fold(0usize, |sum, data| sum.checked_add(data.len()));
    let Some(total) = total else {
        metadata.warnings.push("ICC 长度溢出，已忽略".into());
        return None;
    };
    if total > limit {
        metadata.warnings.push(format!(
            "ICC 载荷 {total} 字节超过 {limit} 字节预算，已忽略"
        ));
        return None;
    }
    let mut out = Vec::with_capacity(total);
    for data in ordered.into_iter().flatten() {
        out.extend_from_slice(&data);
    }
    Some(out)
}

fn read_jpeg_metadata<R: Read + Seek>(
    reader: &mut R,
    options: LoadOptions,
    limits: MetadataLimits,
    metadata: &mut ImageMetadata,
) {
    let Some(file_len) = read_file_len(reader) else {
        metadata.warnings.push("无法读取 JPEG 文件长度".into());
        return;
    };
    if reader.seek(SeekFrom::Start(2)).is_err() {
        return;
    }
    let mut consumed = 2u64;
    let mut marker = [0u8; 2];
    let mut len_bytes = [0u8; 2];
    let mut icc_chunks: Vec<(u8, u8, Vec<u8>)> = Vec::new();
    loop {
        let marker_start = consumed;
        if marker_start.saturating_add(2) > limits.scan_bytes {
            scan_exceeded(metadata, "JPEG", limits.scan_bytes);
            break;
        }
        if reader.read_exact(&mut marker).is_err() || marker[0] != 0xFF {
            break;
        }
        let tag = marker[1];
        if tag == 0xD8 || tag == 0x01 || (0xD0..=0xD7).contains(&tag) || tag == 0xFF {
            consumed = marker_start.saturating_add(2);
            continue;
        }
        if reader.read_exact(&mut len_bytes).is_err() {
            metadata.warnings.push("JPEG 元数据长度字段截断".into());
            break;
        }
        let len = ((len_bytes[0] as usize) << 8) | len_bytes[1] as usize;
        if len < 2 {
            metadata.warnings.push("JPEG 元数据段长度小于 2".into());
            break;
        }
        let next = marker_start.saturating_add(2 + len as u64);
        if next > limits.scan_bytes {
            scan_exceeded(metadata, "JPEG", limits.scan_bytes);
            break;
        }
        if next > file_len {
            metadata
                .warnings
                .push("JPEG 元数据段声明长度超过文件实际长度".into());
            break;
        }
        let payload_len = len - 2;
        if tag == 0xDA {
            break;
        }
        if tag == 0xE1 && payload_len >= EXIF_PREFIX.len() && options.keep_exif {
            if payload_len > limits.exif_bytes {
                metadata.warnings.push(format!(
                    "EXIF 载荷 {payload_len} 字节超过 {} 字节预算，已忽略",
                    limits.exif_bytes
                ));
                if reader.seek(SeekFrom::Current(payload_len as i64)).is_err() {
                    break;
                }
            } else {
                let mut payload = vec![0u8; payload_len];
                if reader.read_exact(&mut payload).is_err() {
                    metadata.warnings.push("JPEG EXIF 载荷截断".into());
                    break;
                }
                if payload.starts_with(EXIF_PREFIX) && metadata.exif.is_none() {
                    metadata.exif = Some(payload);
                }
            }
        } else if tag == 0xE2 && payload_len >= 14 && options.keep_icc {
            if payload_len > limits.icc_bytes {
                metadata.warnings.push(format!(
                    "ICC 分片 {payload_len} 字节超过 {} 字节预算，已忽略",
                    limits.icc_bytes
                ));
                if reader.seek(SeekFrom::Current(payload_len as i64)).is_err() {
                    break;
                }
            } else {
                let mut payload = vec![0u8; payload_len];
                if reader.read_exact(&mut payload).is_err() {
                    metadata.warnings.push("JPEG ICC 载荷截断".into());
                    break;
                }
                if payload.starts_with(ICC_PREFIX) {
                    icc_chunks.push((payload[12], payload[13], payload[14..].to_vec()));
                }
            }
        } else if reader.seek(SeekFrom::Current(payload_len as i64)).is_err() {
            break;
        }
        consumed = next;
    }
    if options.keep_icc {
        metadata.icc = assemble_icc_chunks(icc_chunks, limits.icc_bytes, metadata);
    }
}

fn read_png_metadata<R: Read + Seek>(
    reader: &mut R,
    options: LoadOptions,
    limits: MetadataLimits,
    metadata: &mut ImageMetadata,
) {
    let Some(file_len) = read_file_len(reader) else {
        metadata.warnings.push("无法读取 PNG 文件长度".into());
        return;
    };
    if reader.seek(SeekFrom::Start(8)).is_err() {
        return;
    }
    let mut consumed = 8u64;
    let mut header = [0u8; 8];
    loop {
        if consumed.saturating_add(8) > limits.scan_bytes {
            scan_exceeded(metadata, "PNG", limits.scan_bytes);
            break;
        }
        if reader.read_exact(&mut header).is_err() {
            break;
        }
        let len = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let chunk_total = match len.checked_add(12) {
            Some(v) => v as u64,
            None => {
                metadata.warnings.push("PNG 块长度溢出".into());
                break;
            }
        };
        let next = consumed.saturating_add(chunk_total);
        if next > limits.scan_bytes {
            scan_exceeded(metadata, "PNG", limits.scan_bytes);
            break;
        }
        if next > file_len {
            metadata
                .warnings
                .push("PNG 块声明长度超过文件实际长度".into());
            break;
        }
        let kind = &header[4..8];
        if kind == b"IEND" {
            break;
        }
        let mut payload_read = false;
        if kind == b"eXIf" && options.keep_exif {
            if len > limits.exif_bytes {
                metadata.warnings.push(format!(
                    "EXIF 载荷 {len} 字节超过 {} 字节预算，已忽略",
                    limits.exif_bytes
                ));
            } else {
                let mut payload = vec![0u8; len];
                if reader.read_exact(&mut payload).is_err() {
                    metadata.warnings.push("PNG EXIF 载荷截断".into());
                    break;
                }
                payload_read = true;
                if metadata.exif.is_none() {
                    metadata.exif = Some(with_exif_prefix(payload));
                }
            }
        } else if kind == b"iCCP" && options.keep_icc {
            if len > limits.icc_bytes {
                metadata.warnings.push(format!(
                    "iCCP 载荷 {len} 字节超过 {} 字节预算，已忽略",
                    limits.icc_bytes
                ));
            } else {
                let mut payload = vec![0u8; len];
                if reader.read_exact(&mut payload).is_err() {
                    metadata.warnings.push("PNG iCCP 载荷截断".into());
                    break;
                }
                payload_read = true;
                metadata.icc = decode_iccp(&payload, limits.icc_bytes, metadata);
            }
        }
        let skip = if payload_read { 4 } else { len + 4 };
        if reader.seek(SeekFrom::Current(skip as i64)).is_err() {
            break;
        }
        consumed = next;
    }
}

fn decode_iccp(payload: &[u8], limit: usize, metadata: &mut ImageMetadata) -> Option<Vec<u8>> {
    let Some(name_end) = payload.iter().position(|b| *b == 0) else {
        metadata
            .warnings
            .push("PNG iCCP 缺少配置名终止符，已忽略".into());
        return None;
    };
    let Some(method) = payload.get(name_end + 1) else {
        metadata
            .warnings
            .push("PNG iCCP 压缩方法缺失，已忽略".into());
        return None;
    };
    if *method != 0 {
        metadata
            .warnings
            .push(format!("PNG iCCP 使用不支持的压缩方法 {method}，已忽略"));
        return None;
    }
    let compressed = payload.get(name_end + 2..).unwrap_or_default();
    let mut decoder = ZlibDecoder::new(compressed);
    let mut out = Vec::new();
    if decoder
        .by_ref()
        .take(limit as u64 + 1)
        .read_to_end(&mut out)
        .is_err()
    {
        metadata.warnings.push("PNG iCCP 解压失败，已忽略".into());
        return None;
    }
    if out.len() > limit {
        metadata
            .warnings
            .push(format!("ICC 解压后超过 {limit} 字节预算，已忽略"));
        return None;
    }
    Some(out)
}

fn read_webp_metadata<R: Read + Seek>(
    reader: &mut R,
    options: LoadOptions,
    limits: MetadataLimits,
    metadata: &mut ImageMetadata,
) {
    let Some(file_len) = read_file_len(reader) else {
        metadata.warnings.push("无法读取 WebP 文件长度".into());
        return;
    };
    if file_len < 12 || reader.seek(SeekFrom::Start(12)).is_err() {
        return;
    }
    let mut consumed = 12u64;
    let mut header = [0u8; 8];
    loop {
        if consumed.saturating_add(8) > limits.scan_bytes {
            scan_exceeded(metadata, "WebP", limits.scan_bytes);
            break;
        }
        if reader.read_exact(&mut header).is_err() {
            break;
        }
        let len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
        let chunk_total = match len.checked_add(8).and_then(|v| v.checked_add(len & 1)) {
            Some(v) => v as u64,
            None => {
                metadata.warnings.push("WebP 块长度溢出".into());
                break;
            }
        };
        let next = consumed.saturating_add(chunk_total);
        if next > limits.scan_bytes {
            scan_exceeded(metadata, "WebP", limits.scan_bytes);
            break;
        }
        if next > file_len {
            metadata
                .warnings
                .push("WebP 块声明长度超过文件实际长度".into());
            break;
        }
        let kind = &header[0..4];
        let mut payload_read = false;
        if kind == b"EXIF" && options.keep_exif {
            if len > limits.exif_bytes {
                metadata.warnings.push(format!(
                    "EXIF 载荷 {len} 字节超过 {} 字节预算，已忽略",
                    limits.exif_bytes
                ));
            } else {
                let mut payload = vec![0u8; len];
                if reader.read_exact(&mut payload).is_err() {
                    metadata.warnings.push("WebP EXIF 载荷截断".into());
                    break;
                }
                payload_read = true;
                if metadata.exif.is_none() {
                    metadata.exif = Some(with_exif_prefix(payload));
                }
            }
        } else if kind == b"ICCP" && options.keep_icc {
            if len > limits.icc_bytes {
                metadata.warnings.push(format!(
                    "ICCP 载荷 {len} 字节超过 {} 字节预算，已忽略",
                    limits.icc_bytes
                ));
            } else {
                let mut payload = vec![0u8; len];
                if reader.read_exact(&mut payload).is_err() {
                    metadata.warnings.push("WebP ICC 载荷截断".into());
                    break;
                }
                payload_read = true;
                metadata.icc = Some(payload);
            }
        }
        let skip = if payload_read {
            len & 1
        } else {
            len + (len & 1)
        };
        if reader.seek(SeekFrom::Current(skip as i64)).is_err() {
            break;
        }
        consumed = next;
    }
}

fn inspect_multiframe(path: &Path, metadata: &mut ImageMetadata) {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if ext == "gif" {
        match gif_frame_count(path) {
            Ok(Some(count)) => {
                metadata.frames = Some(count);
                if count > 1 {
                    metadata.warnings.push(format!(
                        "检测到动画 GIF（{count} 帧）；当前仅处理第 1 帧，其他帧不会保留"
                    ));
                }
            }
            Ok(None) => {}
            Err(e) => metadata
                .warnings
                .push(format!("无法检查 GIF 帧数，可能只处理第 1 帧：{e}")),
        }
    } else if ext == "tif" || ext == "tiff" {
        match tiff_page_count(path) {
            Ok(Some(count)) => {
                metadata.pages = Some(count);
                if count > 1 {
                    metadata.warnings.push(format!(
                        "检测到多页 TIFF（{count} 页）；当前仅处理第 1 页，其他页不会保留"
                    ));
                }
            }
            Ok(None) => {}
            Err(e) => metadata
                .warnings
                .push(format!("无法检查 TIFF 页数，可能只处理第 1 页：{e}")),
        }
    }
}

fn gif_frame_count(path: &Path) -> Result<Option<u32>, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mut options = gif::DecodeOptions::new();
    options.skip_frame_decoding(true);
    let decoder = options.read_info(file).map_err(|e| e.to_string())?;
    let mut count = 0u32;
    for frame in decoder {
        frame.map_err(|e| e.to_string())?;
        count = count.saturating_add(1);
        if count >= 10_000 {
            break;
        }
    }
    Ok(Some(count))
}

fn tiff_page_count(path: &Path) -> Result<Option<u32>, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mut decoder =
        tiff::decoder::Decoder::new(BufReader::new(file)).map_err(|e| e.to_string())?;
    let mut count = 1u32;
    while decoder.more_images() {
        decoder.next_image().map_err(|e| e.to_string())?;
        count = count.saturating_add(1);
        if count >= 10_000 {
            break;
        }
    }
    Ok(Some(count))
}

fn patch_exif_orientation(raw: &mut [u8]) {
    if raw.len() < 14 || &raw[0..6] != b"Exif\0\0" {
        return;
    }
    let tiff = 6usize;
    let little = match &raw[tiff..tiff + 4] {
        [0x49, 0x49, 0x2A, 0x00] => true,
        [0x4D, 0x4D, 0x00, 0x2A] => false,
        _ => return,
    };
    let read_u32 = |b: &[u8], off: usize| -> Option<u32> {
        if off + 4 > b.len() {
            return None;
        }
        Some(if little {
            u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
        } else {
            u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
        })
    };
    let Some(ifd_off) = read_u32(raw, tiff + 4) else {
        return;
    };
    let Some(ifd) = tiff.checked_add(ifd_off as usize) else {
        return;
    };
    if ifd + 2 > raw.len() {
        return;
    }
    let count = if little {
        u16::from_le_bytes([raw[ifd], raw[ifd + 1]])
    } else {
        u16::from_be_bytes([raw[ifd], raw[ifd + 1]])
    } as usize;
    for i in 0..count {
        let Some(e) = ifd
            .checked_add(2)
            .and_then(|v| i.checked_mul(12).and_then(|n| v.checked_add(n)))
        else {
            return;
        };
        if e + 12 > raw.len() {
            return;
        }
        let tag = if little {
            u16::from_le_bytes([raw[e], raw[e + 1]])
        } else {
            u16::from_be_bytes([raw[e], raw[e + 1]])
        };
        if tag == 0x0112 {
            if little {
                raw[e + 8] = 1;
                raw[e + 9] = 0;
            } else {
                raw[e + 8] = 0;
                raw[e + 9] = 1;
            }
            return;
        }
    }
}

fn insert_jpeg_exif(mut jpeg: Vec<u8>, payload: &[u8]) -> Vec<u8> {
    if payload.len() < 6 || jpeg.len() < 4 {
        return jpeg;
    }
    let seg_len = payload.len() + 2;
    if seg_len > 0xFFFF {
        return jpeg;
    }
    // JFIF 规范要求 APP0 在最前，因此插到紧随 SOI 的 APP0 段之后。
    let mut at = 2usize;
    if jpeg.len() >= 6 && jpeg[2] == 0xFF && jpeg[3] == 0xE0 {
        let app0_len = ((jpeg[4] as usize) << 8) | jpeg[5] as usize;
        if app0_len >= 2 && 4 + app0_len <= jpeg.len() {
            at = 4 + app0_len;
        }
    }
    let mut app1 = Vec::with_capacity(payload.len() + 4);
    app1.push(0xFF);
    app1.push(0xE1);
    app1.push((seg_len >> 8) as u8);
    app1.push((seg_len & 0xFF) as u8);
    app1.extend_from_slice(payload);
    jpeg.splice(at..at, app1);
    jpeg
}

fn insert_jpeg_icc(mut jpeg: Vec<u8>, icc: &[u8]) -> Vec<u8> {
    if icc.is_empty() || icc.len() > MAX_JPEG_ICC_BYTES || jpeg.len() < 4 {
        return jpeg;
    }
    let per_chunk = 0xFFFF - 2 - ICC_PREFIX.len() - 2;
    let count = icc.len().div_ceil(per_chunk);
    if count == 0 || count > u8::MAX as usize {
        return jpeg;
    }
    let mut at = 2usize;
    if jpeg.len() >= 6 && jpeg[2] == 0xFF && jpeg[3] == 0xE0 {
        let app0_len = ((jpeg[4] as usize) << 8) | jpeg[5] as usize;
        if app0_len >= 2 && 4 + app0_len <= jpeg.len() {
            at = 4 + app0_len;
        }
    }
    let mut chunks = Vec::with_capacity(icc.len() + count * (ICC_PREFIX.len() + 6));
    for (idx, part) in icc.chunks(per_chunk).enumerate() {
        let payload_len = ICC_PREFIX.len() + 2 + part.len();
        let seg_len = payload_len + 2;
        chunks.extend_from_slice(&[0xFF, 0xE2, (seg_len >> 8) as u8, seg_len as u8]);
        chunks.extend_from_slice(ICC_PREFIX);
        chunks.push((idx + 1) as u8);
        chunks.push(count as u8);
        chunks.extend_from_slice(part);
    }
    jpeg.splice(at..at, chunks);
    jpeg
}

fn insert_png_chunk(mut png: Vec<u8>, kind: &[u8; 4], payload: &[u8]) -> Result<Vec<u8>, String> {
    if payload.len() > u32::MAX as usize {
        return Err("PNG 元数据块过大".into());
    }
    let mut pos = 8usize;
    let mut iend = None;
    while pos.checked_add(8).is_some_and(|v| v <= png.len()) {
        let len = u32::from_be_bytes([png[pos], png[pos + 1], png[pos + 2], png[pos + 3]]) as usize;
        let next = pos
            .checked_add(12)
            .and_then(|v| v.checked_add(len))
            .ok_or_else(|| "PNG 块长度溢出".to_string())?;
        if next > png.len() {
            return Err("PNG 块声明长度超过文件实际长度".into());
        }
        if &png[pos + 4..pos + 8] == b"IEND" {
            iend = Some(pos);
            break;
        }
        pos = next;
    }
    let Some(at) = iend else {
        return Ok(png);
    };
    let mut chunk = Vec::with_capacity(payload.len() + 12);
    chunk.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    chunk.extend_from_slice(kind);
    chunk.extend_from_slice(payload);
    let mut crc = crc32fast::Hasher::new();
    crc.update(kind);
    crc.update(payload);
    chunk.extend_from_slice(&crc.finalize().to_be_bytes());
    png.splice(at..at, chunk);
    Ok(png)
}

fn insert_png_icc(png: Vec<u8>, icc: &[u8]) -> Result<Vec<u8>, String> {
    if icc.is_empty() {
        return Ok(png);
    }
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(icc)
        .map_err(|e| format!("ICC 压缩失败：{e}"))?;
    let compressed = encoder.finish().map_err(|e| format!("ICC 压缩失败：{e}"))?;
    let mut payload = Vec::with_capacity(b"PixSqueeze ICC".len() + 2 + compressed.len());
    payload.extend_from_slice(b"PixSqueeze ICC\0");
    payload.push(0);
    payload.extend_from_slice(&compressed);
    insert_png_chunk(png, b"iCCP", &payload)
}

fn insert_png_exif(png: Vec<u8>, exif: &[u8]) -> Result<Vec<u8>, String> {
    let tiff = if exif.starts_with(b"Exif\0\0") {
        &exif[6..]
    } else {
        exif
    };
    if tiff.is_empty() {
        return Ok(png);
    }
    insert_png_chunk(png, b"eXIf", tiff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alpha_detection_and_resize_keep_expected_shape() {
        let opaque = RgbaImage::from_raw(8, 4, vec![255; 8 * 4 * 4]).unwrap();
        assert!(!has_alpha(&opaque));
        let resized = smart_resize(&opaque, 4, 2);
        assert_eq!((resized.width(), resized.height()), (4, 2));

        let mut raw = opaque.into_raw();
        raw[3] = 10;
        let alpha = RgbaImage::from_raw(8, 4, raw).unwrap();
        assert!(has_alpha(&alpha));
    }

    /// 最小 EXIF：II 字节序，IFD0 只有一个 Orientation 条目，值为 6（顺时针 90°）。
    fn orientation6_exif() -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(EXIF_PREFIX);
        v.extend_from_slice(b"II\x2a\x00");
        v.extend_from_slice(&8u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&0x0112u16.to_le_bytes());
        v.extend_from_slice(&3u16.to_le_bytes());
        v.extend_from_slice(&1u32.to_le_bytes());
        v.extend_from_slice(&6u16.to_le_bytes());
        v.extend_from_slice(&[0u8, 0]);
        v.extend_from_slice(&0u32.to_le_bytes());
        v
    }

    fn orientation_of(raw: &[u8]) -> Option<u16> {
        let tiff = raw.strip_prefix(EXIF_PREFIX).unwrap_or(raw);
        if tiff.len() < 14 || &tiff[0..2] != b"II" {
            return None;
        }
        let ifd = u32::from_le_bytes([tiff[4], tiff[5], tiff[6], tiff[7]]) as usize;
        let count = u16::from_le_bytes([tiff[ifd], tiff[ifd + 1]]) as usize;
        for i in 0..count {
            let e = ifd + 2 + i * 12;
            if e + 12 > tiff.len() {
                return None;
            }
            if u16::from_le_bytes([tiff[e], tiff[e + 1]]) == 0x0112 {
                return Some(u16::from_le_bytes([tiff[e + 8], tiff[e + 9]]));
            }
        }
        None
    }

    fn png_exif_chunk(png: &[u8]) -> Option<&[u8]> {
        let mut pos = 8usize;
        while pos + 8 <= png.len() {
            let len =
                u32::from_be_bytes([png[pos], png[pos + 1], png[pos + 2], png[pos + 3]]) as usize;
            if &png[pos + 4..pos + 8] == b"eXIf" {
                return png.get(pos + 8..pos + 8 + len);
            }
            if &png[pos + 4..pos + 8] == b"IEND" {
                return None;
            }
            pos += 12 + len;
        }
        None
    }

    fn unique_dir(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(1);
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "pix-squeeze-img-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn png_output_clears_orientation_when_pixels_rotated() {
        let img = RgbaImage::from_raw(4, 2, vec![128; 4 * 2 * 4]).unwrap();
        let exif = orientation6_exif();
        let png = encode_image(
            &img,
            OutFormat::Png,
            82,
            OutputMetadata {
                exif: Some(&exif),
                icc: None,
            },
        )
        .unwrap();
        let chunk = png_exif_chunk(&png).expect("eXIf chunk 应存在");
        assert_eq!(orientation_of(chunk), Some(1), "PNG 输出必须把方向归一为 1");
    }

    #[test]
    fn paletted_png_clears_orientation_when_pixels_rotated() {
        let img = RgbaImage::from_raw(4, 2, vec![128; 4 * 2 * 4]).unwrap();
        let exif = orientation6_exif();
        let png = encode_png_paletted(
            &img,
            256,
            0.6,
            OutputMetadata {
                exif: Some(&exif),
                icc: None,
            },
        )
        .unwrap();
        let chunk = png_exif_chunk(&png).expect("eXIf chunk 应存在");
        assert_eq!(
            orientation_of(chunk),
            Some(1),
            "调色板 PNG 也必须把方向归一为 1"
        );
    }

    #[test]
    fn keeps_exif_from_png_and_webp_sources() {
        let dir = unique_dir("exif-src");
        let img = RgbaImage::from_raw(3, 3, vec![200; 3 * 3 * 4]).unwrap();
        let exif = orientation6_exif();

        let png_path = dir.join("src.png");
        fs::write(
            &png_path,
            encode_image(
                &img,
                OutFormat::Png,
                82,
                OutputMetadata {
                    exif: Some(&exif),
                    icc: None,
                },
            )
            .unwrap(),
        )
        .unwrap();
        let from_png =
            read_metadata_with_limits(&png_path, LoadOptions::default(), MetadataLimits::default())
                .exif
                .expect("PNG eXIf 应能读回");
        assert!(from_png.starts_with(EXIF_PREFIX));
        assert_eq!(orientation_of(&from_png), Some(1));

        let webp_path = dir.join("src.webp");
        fs::write(
            &webp_path,
            encode_image(
                &img,
                OutFormat::Webp,
                82,
                OutputMetadata {
                    exif: Some(&exif),
                    icc: None,
                },
            )
            .unwrap(),
        )
        .unwrap();
        let from_webp = read_metadata_with_limits(
            &webp_path,
            LoadOptions::default(),
            MetadataLimits::default(),
        )
        .exif
        .expect("WebP EXIF 应能读回");
        assert!(from_webp.starts_with(EXIF_PREFIX));
        assert_eq!(orientation_of(&from_webp), Some(1));
    }

    #[test]
    fn loads_png_bytes_with_jpg_extension() {
        let dir = unique_dir("mismatch");
        let img = RgbaImage::from_raw(6, 4, vec![255; 6 * 4 * 4]).unwrap();
        let png = encode_image(&img, OutFormat::Png, 82, OutputMetadata::default()).unwrap();
        let fake = dir.join("伪装.jpg");
        fs::write(&fake, &png).unwrap();
        let (loaded, _) = load_image(&fake.to_string_lossy()).expect("按内容识别后应能解码");
        assert_eq!((loaded.width(), loaded.height()), (6, 4));
        assert!(file_matches_format(&fake, OutFormat::Png));
        assert!(!file_matches_format(&fake, OutFormat::Jpeg));
    }

    #[test]
    fn preview_base_keeps_aspect_and_small_images() {
        let image = RgbaImage::from_raw(1200, 800, vec![0; 1200 * 800 * 4]).unwrap();
        let base = preview_base(&image, 900);
        assert_eq!((base.width(), base.height()), (900, 600));
        let small = RgbaImage::from_raw(100, 80, vec![0; 100 * 80 * 4]).unwrap();
        let unchanged = preview_base(&small, 900);
        assert_eq!((unchanged.width(), unchanged.height()), (100, 80));
    }

    #[test]
    fn png_declared_length_is_checked_before_payload_allocation() {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&0x7fff_ffffu32.to_be_bytes());
        png.extend_from_slice(b"eXIf");
        let mut reader = Cursor::new(png);
        let mut metadata = ImageMetadata::default();
        read_png_metadata(
            &mut reader,
            LoadOptions::default(),
            MetadataLimits {
                scan_bytes: u64::MAX,
                exif_bytes: 64,
                icc_bytes: 64,
            },
            &mut metadata,
        );
        assert!(metadata.exif.is_none());
        assert!(
            metadata
                .warnings
                .iter()
                .any(|warning| warning.contains("实际长度")),
            "warnings: {:?}",
            metadata.warnings
        );
    }

    #[test]
    fn metadata_payload_budget_rejects_before_copy() {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&64u32.to_be_bytes());
        png.extend_from_slice(b"eXIf");
        png.extend_from_slice(&[0u8; 64]);
        png.extend_from_slice(&[0u8; 4]);
        let mut reader = Cursor::new(png);
        let mut metadata = ImageMetadata::default();
        read_png_metadata(
            &mut reader,
            LoadOptions::default(),
            MetadataLimits {
                scan_bytes: 1024,
                exif_bytes: 16,
                icc_bytes: 16,
            },
            &mut metadata,
        );
        assert!(metadata.exif.is_none());
        assert!(metadata
            .warnings
            .iter()
            .any(|warning| warning.contains("预算")));
    }

    #[test]
    fn icc_and_exif_roundtrip_across_output_formats() {
        let dir = unique_dir("icc-roundtrip");
        let image = RgbaImage::from_raw(8, 6, vec![80; 8 * 6 * 4]).unwrap();
        let exif = orientation6_exif();
        let icc = b"PixSqueeze test ICC profile payload".to_vec();
        let metadata = OutputMetadata {
            exif: Some(&exif),
            icc: Some(&icc),
        };
        for (fmt, ext) in [
            (OutFormat::Jpeg, "jpg"),
            (OutFormat::Png, "png"),
            (OutFormat::Webp, "webp"),
        ] {
            let bytes = encode_image(&image, fmt, 82, metadata).unwrap();
            let path = dir.join(format!("roundtrip.{ext}"));
            fs::write(&path, bytes).unwrap();
            assert!(file_matches_format(&path, fmt));
            let parsed =
                read_metadata_with_limits(&path, LoadOptions::default(), MetadataLimits::default());
            assert_eq!(parsed.icc.as_deref(), Some(icc.as_slice()), "{ext} ICC");
            assert!(parsed.exif.is_some(), "{ext} EXIF");
            assert_eq!(orientation_of(parsed.exif.as_deref().unwrap()), Some(1));
        }
    }

    #[test]
    fn oversized_jpeg_exif_is_reported_not_silently_ignored() {
        let image = RgbaImage::from_raw(4, 4, vec![128; 4 * 4 * 4]).unwrap();
        let mut exif = EXIF_PREFIX.to_vec();
        exif.extend(std::iter::repeat_n(0u8, 70_000));
        let warnings = metadata_warnings(
            OutFormat::Jpeg,
            OutputMetadata {
                exif: Some(&exif),
                icc: None,
            },
        );
        assert!(warnings.iter().any(|warning| warning.contains("APP1")));
        let encoded = encode_image(
            &image,
            OutFormat::Jpeg,
            80,
            OutputMetadata {
                exif: Some(&exif),
                icc: None,
            },
        )
        .unwrap();
        assert!(image::load_from_memory(&encoded).is_ok());
    }

    #[test]
    fn webp_rejects_both_long_edge_orientations_without_panic() {
        for (w, h) in [(WEBP_MAX_DIMENSION + 1, 1), (1, WEBP_MAX_DIMENSION + 1)] {
            let image = RgbaImage::from_raw(w, h, vec![255; w as usize * h as usize * 4]).unwrap();
            let error =
                encode_image(&image, OutFormat::Webp, 82, OutputMetadata::default()).unwrap_err();
            assert!(error.contains(&WEBP_MAX_DIMENSION.to_string()), "{error}");
        }
        let boundary = RgbaImage::from_raw(
            WEBP_MAX_DIMENSION,
            1,
            vec![255; WEBP_MAX_DIMENSION as usize * 4],
        )
        .unwrap();
        assert!(encode_image(&boundary, OutFormat::Webp, 82, OutputMetadata::default()).is_ok());
    }

    #[test]
    fn animated_gif_and_multipage_tiff_are_detected() {
        let dir = unique_dir("multiframe");

        let gif_path = dir.join("animated.gif");
        let mut gif_bytes = Vec::new();
        {
            let mut encoder = gif::Encoder::new(&mut gif_bytes, 2, 2, &[]).unwrap();
            encoder.set_repeat(gif::Repeat::Infinite).unwrap();
            for color in [[255u8, 0, 0, 255], [0, 255, 0, 255]] {
                let mut rgba = color.repeat(4);
                let frame = gif::Frame::from_rgba_speed(2, 2, &mut rgba, 10);
                encoder.write_frame(&frame).unwrap();
            }
        }
        fs::write(&gif_path, gif_bytes).unwrap();
        assert_eq!(gif_frame_count(&gif_path).unwrap(), Some(2));
        let mut gif_metadata = ImageMetadata::default();
        inspect_multiframe(&gif_path, &mut gif_metadata);
        assert!(gif_metadata.warnings.iter().any(|w| w.contains("动画 GIF")));

        let tiff_path = dir.join("pages.tiff");
        let file = File::create(&tiff_path).unwrap();
        let mut encoder = tiff::encoder::TiffEncoder::new(file).unwrap();
        let rgb = [10u8, 20, 30];
        encoder
            .write_image::<tiff::encoder::colortype::RGB8>(1, 1, &rgb)
            .unwrap();
        encoder
            .write_image::<tiff::encoder::colortype::RGB8>(1, 1, &rgb)
            .unwrap();
        assert_eq!(tiff_page_count(&tiff_path).unwrap(), Some(2));
        let mut tiff_metadata = ImageMetadata::default();
        inspect_multiframe(&tiff_path, &mut tiff_metadata);
        assert!(tiff_metadata
            .warnings
            .iter()
            .any(|w| w.contains("多页 TIFF")));
    }
}
