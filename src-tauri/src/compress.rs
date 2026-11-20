use crate::image_util::{
    encode_image, encode_png_paletted, has_alpha, metadata_warnings, smart_resize, unsharp_mask,
    OutFormat, OutputMetadata,
};
use crate::resources::{self, ResourcePolicy, WorkShape};
use crate::settings::Settings;
use image::RgbaImage;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

pub const OUTPUT_SUFFIX: &str = "_压缩";
pub const CANCELLED_ERROR: &str = "任务已取消";
const JPEG_Q_MIN: u32 = 30;
const JPEG_Q_MAX: u32 = 95;
const SEARCH_BUDGET: u32 = 8;
const SCALE_ROUNDS: u32 = 3;
const PNG_LEVELS: [u32; 6] = [8, 16, 32, 64, 128, 256];

pub(crate) type CancelCheck<'a> = Option<&'a dyn Fn() -> bool>;

#[derive(Clone, Debug)]
pub struct CompressMeta {
    pub out_w: u32,
    pub out_h: u32,
    pub out_bytes: u64,
    pub quality_used: u32,
    pub scaled: bool,
    pub reached_target: bool,
    pub encode_attempts: u32,
    pub budget_exhausted: bool,
    pub transparent_flattened: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug)]
pub struct CompressOutcome {
    pub data: Vec<u8>,
    pub meta: CompressMeta,
}

struct EncodeBudget {
    limit: u32,
    used: u32,
}

impl EncodeBudget {
    fn new(limit: u32) -> Self {
        Self {
            limit: limit.max(1),
            used: 0,
        }
    }

    fn take(&mut self) -> bool {
        if self.used >= self.limit {
            return false;
        }
        self.used += 1;
        true
    }

    fn remaining(&self) -> u32 {
        self.limit.saturating_sub(self.used)
    }

    fn exhausted(&self) -> bool {
        self.used >= self.limit
    }
}

fn check_cancel(cancel: CancelCheck<'_>) -> Result<(), String> {
    if cancel.is_some_and(|is_cancelled| is_cancelled()) {
        Err(CANCELLED_ERROR.into())
    } else {
        Ok(())
    }
}

pub fn resolve_out_fmt(in_ext: &str, setting: &str) -> OutFormat {
    if setting == "jpeg" {
        return OutFormat::Jpeg;
    }
    if setting == "png" {
        return OutFormat::Png;
    }
    if setting == "webp" {
        return OutFormat::Webp;
    }
    match in_ext.trim_start_matches('.').to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => OutFormat::Jpeg,
        "png" => OutFormat::Png,
        "webp" => OutFormat::Webp,
        _ => OutFormat::Png,
    }
}

fn target_size(w: u32, h: u32, percent: u32) -> (u32, u32) {
    if percent >= 100 {
        (w, h)
    } else {
        (
            ((w as f64 * percent as f64 / 100.0).round() as u32).max(1),
            ((h as f64 * percent as f64 / 100.0).round() as u32).max(1),
        )
    }
}

fn encode_regular(
    img: &RgbaImage,
    fmt: OutFormat,
    quality: u32,
    metadata: OutputMetadata<'_>,
) -> Result<Vec<u8>, String> {
    encode_image(img, fmt, quality, metadata)
}

fn encode_work(
    img: &RgbaImage,
    fmt: OutFormat,
    quality: u32,
    metadata: OutputMetadata<'_>,
) -> Result<Vec<u8>, String> {
    if fmt == OutFormat::Png && quality < 90 {
        // 调色板档位随质量单调递减；抖动按档位收敛（抖动越猛，体积越大）——PS-A-019
        let (colors, dithering) = if quality >= 70 {
            (256, 0.6)
        } else if quality >= 40 {
            (128, 0.3)
        } else {
            (64, 0.0)
        };
        encode_png_paletted(img, colors, dithering, metadata)
    } else {
        encode_regular(img, fmt, quality, metadata)
    }
}

pub fn compress_pixel(
    src: &RgbaImage,
    fmt: OutFormat,
    percent: u32,
    quality: u32,
    sharpen: bool,
    metadata: OutputMetadata<'_>,
) -> Result<CompressOutcome, String> {
    let (nw, nh) = target_size(src.width(), src.height(), percent);
    let scaled = (nw, nh) != (src.width(), src.height());
    let shape = WorkShape {
        scaled,
        sharpen: scaled && sharpen,
        quantize: fmt == OutFormat::Png && quality < 90,
        flatten_rgb: fmt == OutFormat::Jpeg,
    };
    resources::check_work(
        ResourcePolicy::default(),
        src.width(),
        src.height(),
        nw,
        nh,
        shape,
    )?;
    // 不缩放就不复制：24 MP 的一张 clone 是 96 MB 的纯浪费（PS-A-017）
    let mut work: std::borrow::Cow<'_, RgbaImage> = if scaled {
        std::borrow::Cow::Owned(smart_resize(src, nw, nh))
    } else {
        std::borrow::Cow::Borrowed(src)
    };
    if scaled && sharpen {
        work = std::borrow::Cow::Owned(unsharp_mask(work.as_ref()));
    }
    let data = encode_work(work.as_ref(), fmt, quality, metadata)?;
    let transparent_flattened = fmt == OutFormat::Jpeg && has_alpha(src);
    let mut warnings = metadata_warnings(fmt, metadata);
    if transparent_flattened {
        warnings.push("JPEG 不支持透明通道，透明/半透明区域已合成到白底".into());
    }
    Ok(CompressOutcome {
        meta: CompressMeta {
            out_w: work.width(),
            out_h: work.height(),
            out_bytes: data.len() as u64,
            quality_used: quality,
            scaled,
            reached_target: true,
            encode_attempts: 1,
            budget_exhausted: false,
            transparent_flattened,
            warnings,
        },
        data,
    })
}

/// 质量搜索只使用同一份全局预算：跨质量二分与跨缩放轮次都算入 `limit`。
/// 预算耗尽时返回当前最小的有效编码结果，而不是额外偷跑一次兜底编码。
fn search_quality(
    work: &RgbaImage,
    fmt: OutFormat,
    target: u64,
    metadata: OutputMetadata<'_>,
    budget: &mut EncodeBudget,
    cancel: CancelCheck<'_>,
) -> Result<(Vec<u8>, u32, bool), String> {
    check_cancel(cancel)?;
    let mut smallest: Option<(Vec<u8>, u32)>;

    if fmt == OutFormat::Png {
        if !budget.take() {
            return Err("编码预算已用尽".into());
        }
        let first = encode_png_paletted(work, PNG_LEVELS[5], 0.6, metadata)?;
        if first.len() as u64 <= target {
            return Ok((first, PNG_LEVELS[5], true));
        }
        smallest = Some((first, PNG_LEVELS[5]));

        let (mut lo, mut hi) = (0usize, 5usize);
        let mut best: Option<(Vec<u8>, u32)> = None;
        while lo <= hi && budget.remaining() > 0 {
            check_cancel(cancel)?;
            if !budget.take() {
                break;
            }
            let mid = (lo + hi) / 2;
            let d = encode_png_paletted(work, PNG_LEVELS[mid], 0.6, metadata)?;
            if smallest.as_ref().is_none_or(|(s, _)| d.len() < s.len()) {
                smallest = Some((d.clone(), PNG_LEVELS[mid]));
            }
            if d.len() as u64 <= target {
                best = Some((d, PNG_LEVELS[mid]));
                lo = mid + 1;
            } else if mid == 0 {
                break;
            } else {
                hi = mid - 1;
            }
        }
        if let Some((d, c)) = best {
            return Ok((d, c, true));
        }
        let (d, c) = smallest.ok_or_else(|| "PNG 编码预算不足".to_string())?;
        return Ok((d, c, false));
    }

    if !budget.take() {
        return Err("编码预算已用尽".into());
    }
    let first = encode_regular(work, fmt, JPEG_Q_MAX, metadata)?;
    if first.len() as u64 <= target {
        return Ok((first, JPEG_Q_MAX, true));
    }
    smallest = Some((first, JPEG_Q_MAX));

    if budget.remaining() == 0 {
        let (d, q) = smallest.expect("smallest exists");
        return Ok((d, q, false));
    }

    check_cancel(cancel)?;
    budget.take();
    let probe = encode_regular(work, fmt, 60, metadata)?;
    if smallest.as_ref().is_none_or(|(s, _)| probe.len() < s.len()) {
        smallest = Some((probe.clone(), 60));
    }
    let mut best: Option<(Vec<u8>, u32)> = None;
    let (mut lo, mut hi) = if (probe.len() as u64) <= target {
        best = Some((probe, 60));
        (61u32, JPEG_Q_MAX - 1)
    } else {
        let ratio = target as f64 / (probe.len().max(1) as f64);
        let guess = ((60.0 * ratio).round() as u32).clamp(JPEG_Q_MIN, 59);
        (JPEG_Q_MIN, guess.max(JPEG_Q_MIN))
    };

    while lo <= hi && budget.remaining() > 0 {
        check_cancel(cancel)?;
        if !budget.take() {
            break;
        }
        let mid = (lo + hi) / 2;
        let d = encode_regular(work, fmt, mid, metadata)?;
        if smallest.as_ref().is_none_or(|(s, _)| d.len() < s.len()) {
            smallest = Some((d.clone(), mid));
        }
        if d.len() as u64 <= target {
            best = Some((d, mid));
            lo = mid + 1;
        } else {
            hi = mid.saturating_sub(1);
        }
    }

    if let Some((d, q)) = best {
        let reached = d.len() as u64 <= target;
        return Ok((d, q, reached));
    }
    let (d, q) = smallest.ok_or_else(|| "JPEG 编码预算不足".to_string())?;
    Ok((d, q, false))
}

#[cfg(test)]
pub fn compress_to_target(
    src: &RgbaImage,
    fmt: OutFormat,
    target_kb: i64,
    sharpen: bool,
    metadata: OutputMetadata<'_>,
    max_encodes: Option<u32>,
) -> Result<CompressOutcome, String> {
    compress_to_target_cancellable(src, fmt, target_kb, sharpen, metadata, max_encodes, None)
}

fn compress_to_target_cancellable(
    src: &RgbaImage,
    fmt: OutFormat,
    target_kb: i64,
    sharpen: bool,
    metadata: OutputMetadata<'_>,
    max_encodes: Option<u32>,
    cancel: CancelCheck<'_>,
) -> Result<CompressOutcome, String> {
    check_cancel(cancel)?;
    let target = (target_kb.max(1) as u64) * 1024;
    let policy = ResourcePolicy::default();
    let full_shape = WorkShape {
        scaled: false,
        sharpen: false,
        quantize: fmt == OutFormat::Png,
        flatten_rgb: fmt == OutFormat::Jpeg,
    };
    let full_peak = resources::estimate_work_peak_bytes(
        src.width(),
        src.height(),
        src.width(),
        src.height(),
        full_shape,
    )?;
    let mut scale = if full_peak > policy.single_image_bytes {
        (policy.single_image_bytes as f64 / full_peak as f64)
            .sqrt()
            .clamp(0.1, 0.95)
    } else {
        1.0
    };
    let resource_warning = (scale < 1.0).then(|| {
        format!(
            "为满足单图内存预算，目标体积搜索从约 {}% 尺寸开始",
            (scale * 100.0).round() as u32
        )
    });
    let mut smallest: Option<(Vec<u8>, u32, f64, u32, u32)> = None;
    let mut budget = EncodeBudget::new(max_encodes.unwrap_or(SEARCH_BUDGET));

    for _ in 0..SCALE_ROUNDS {
        check_cancel(cancel)?;
        if budget.remaining() == 0 {
            break;
        }
        let scaled = scale < 1.0;
        let (w, h) = if scaled {
            (
                ((src.width() as f64 * scale).round() as u32).max(1),
                ((src.height() as f64 * scale).round() as u32).max(1),
            )
        } else {
            (src.width(), src.height())
        };
        let mut work: std::borrow::Cow<'_, RgbaImage> = if scaled {
            std::borrow::Cow::Owned(smart_resize(src, w, h))
        } else {
            std::borrow::Cow::Borrowed(src)
        };
        let shape = WorkShape {
            scaled,
            sharpen: scaled && sharpen,
            quantize: fmt == OutFormat::Png,
            flatten_rgb: fmt == OutFormat::Jpeg,
        };
        resources::check_work(policy, src.width(), src.height(), w, h, shape)?;
        if scaled && sharpen {
            work = std::borrow::Cow::Owned(unsharp_mask(work.as_ref()));
        }
        let (data, q, ok) =
            search_quality(work.as_ref(), fmt, target, metadata, &mut budget, cancel)?;
        let len = data.len();
        if smallest.as_ref().is_none_or(|s| len < s.0.len()) {
            smallest = Some((data.clone(), q, scale, work.width(), work.height()));
        }
        if ok {
            let mut warnings = metadata_warnings(fmt, metadata);
            if fmt == OutFormat::Jpeg && has_alpha(src) {
                warnings.push("JPEG 不支持透明通道，透明/半透明区域已合成到白底".into());
            }
            if let Some(warning) = &resource_warning {
                warnings.push(warning.clone());
            }
            return Ok(CompressOutcome {
                meta: CompressMeta {
                    out_w: work.width(),
                    out_h: work.height(),
                    out_bytes: len as u64,
                    quality_used: q,
                    scaled,
                    reached_target: true,
                    encode_attempts: budget.used,
                    budget_exhausted: false,
                    transparent_flattened: fmt == OutFormat::Jpeg && has_alpha(src),
                    warnings,
                },
                data,
            });
        }
        if budget.exhausted() {
            break;
        }
        let ratio = (target as f64 / len.max(1) as f64).sqrt();
        scale *= ratio.clamp(0.4, 0.95);
    }

    let (data, q, s, w, h) = smallest.ok_or_else(|| "目标体积搜索未产生有效编码".to_string())?;
    let mut warnings = metadata_warnings(fmt, metadata);
    if fmt == OutFormat::Jpeg && has_alpha(src) {
        warnings.push("JPEG 不支持透明通道，透明/半透明区域已合成到白底".into());
    }
    if let Some(warning) = resource_warning {
        warnings.push(warning);
    }
    Ok(CompressOutcome {
        meta: CompressMeta {
            out_w: w,
            out_h: h,
            out_bytes: data.len() as u64,
            quality_used: q,
            scaled: s < 1.0,
            reached_target: false,
            encode_attempts: budget.used,
            budget_exhausted: budget.exhausted(),
            transparent_flattened: fmt == OutFormat::Jpeg && has_alpha(src),
            warnings,
        },
        data,
    })
}

pub fn compress_with_settings(
    src: &RgbaImage,
    fmt: OutFormat,
    settings: &Settings,
    metadata: OutputMetadata<'_>,
) -> Result<CompressOutcome, String> {
    compress_with_settings_budget(src, fmt, settings, metadata, None)
}

pub fn compress_with_settings_budget(
    src: &RgbaImage,
    fmt: OutFormat,
    settings: &Settings,
    metadata: OutputMetadata<'_>,
    max_encodes: Option<u32>,
) -> Result<CompressOutcome, String> {
    compress_with_settings_cancellable(src, fmt, settings, metadata, max_encodes, None)
}

pub(crate) fn compress_with_settings_cancellable(
    src: &RgbaImage,
    fmt: OutFormat,
    settings: &Settings,
    metadata: OutputMetadata<'_>,
    max_encodes: Option<u32>,
    cancel: CancelCheck<'_>,
) -> Result<CompressOutcome, String> {
    check_cancel(cancel)?;
    if settings.mode == "size" {
        compress_to_target_cancellable(
            src,
            fmt,
            settings.target_kb,
            settings.sharpen,
            metadata,
            max_encodes,
            cancel,
        )
    } else {
        compress_pixel(
            src,
            fmt,
            settings.percent.clamp(10, 100) as u32,
            settings.quality.clamp(1, 100) as u32,
            settings.sharpen,
            metadata,
        )
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn compress_preview_with_settings_cancellable(
    base: &RgbaImage,
    full_w: u32,
    full_h: u32,
    fmt: OutFormat,
    settings: &Settings,
    metadata: OutputMetadata<'_>,
    max_encodes: Option<u32>,
    cancel: CancelCheck<'_>,
) -> Result<CompressOutcome, String> {
    let mut preview_settings = settings.clone();
    if settings.mode == "size" {
        let full_px = (full_w as f64) * (full_h as f64);
        let base_px = (base.width() as f64) * (base.height() as f64);
        if full_px > 0.0 {
            preview_settings.target_kb =
                ((settings.target_kb as f64 * base_px / full_px).round() as i64).max(1);
        }
    }
    compress_with_settings_cancellable(base, fmt, &preview_settings, metadata, max_encodes, cancel)
}

/// 本进程内已「发放」给批处理的输出路径。批量压缩是多线程的，
/// 只靠 `Path::exists()` 判断会在「选中 → 写盘」之间被其他线程抢占同一路径，造成静默覆盖。
static RESERVED_OUT: Mutex<Vec<String>> = Mutex::new(Vec::new());
static PART_SEQ: AtomicU64 = AtomicU64::new(1);

pub fn clear_reserved_outputs() {
    if let Ok(mut list) = RESERVED_OUT.lock() {
        list.clear();
    }
}

fn reservation_key(p: &Path) -> String {
    let parent = p
        .parent()
        .and_then(|v| fs::canonicalize(v).ok())
        .unwrap_or_else(|| p.parent().unwrap_or(Path::new("")).to_path_buf());
    let name = p
        .file_name()
        .map(|v| v.to_string_lossy().trim_end_matches([' ', '.']).to_string())
        .unwrap_or_default();
    format!("{}/{}", parent.to_string_lossy().replace('\\', "/"), name).to_lowercase()
}

fn try_reserve(p: &Path) -> bool {
    let Ok(mut list) = RESERVED_OUT.lock() else {
        return false;
    };
    let key = reservation_key(p);
    if list.iter().any(|x| x == &key) {
        false
    } else {
        list.push(key);
        true
    }
}

fn dedupe_out(cand: PathBuf, src: &Path) -> Result<PathBuf, String> {
    let same = |a: &Path| -> bool {
        let ca = fs::canonicalize(a).ok();
        let cs = fs::canonicalize(src).ok();
        ca.is_some() && ca == cs
    };
    let stem = cand
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "output".into());
    let suffix = cand
        .extension()
        .map(|s| format!(".{}", s.to_string_lossy()))
        .unwrap_or_default();
    let parent = cand.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut i = 1u32;
    loop {
        let c = if i == 1 {
            cand.clone()
        } else {
            parent.join(format!("{stem}({i}){suffix}"))
        };
        if !c.exists() && !same(&c) && try_reserve(&c) {
            return Ok(c);
        }
        i += 1;
        if i == u32::MAX {
            return Err("无法分配唯一输出文件名".into());
        }
    }
}

pub fn out_path_for(
    src_path: &str,
    fmt_setting: &str,
    out_mode: &str,
    out_dir: &str,
) -> Result<PathBuf, String> {
    let p = Path::new(src_path);
    let src_ext = p
        .extension()
        .map(|e| e.to_string_lossy().to_string())
        .unwrap_or_default();
    let fmt = resolve_out_fmt(&src_ext, fmt_setting);
    // auto 模式下只有「源扩展名与实际格式同族」才沿用原书写（.jpeg 不会被改成 .jpg），
    // 否则 bmp/gif/tif 等会被编码成 PNG 却仍叫 .bmp，造成后续按扩展名解码失败。
    let ext = if fmt_setting == "auto" && fmt.matches_ext(&src_ext) {
        format!(".{}", src_ext)
    } else {
        format!(".{}", fmt.ext())
    };
    let cand = if out_mode == "folder" {
        if out_dir.trim().is_empty() {
            return Err("已选择“指定文件夹”，但输出目录为空；请先选择目录".into());
        }
        let base = PathBuf::from(out_dir);
        fs::create_dir_all(&base)
            .map_err(|e| format!("创建输出目录失败（{}）：{e}", base.display()))?;
        base.join(format!(
            "{}{}",
            p.file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default(),
            ext
        ))
    } else {
        let name = format!(
            "{}{}{}",
            p.file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default(),
            OUTPUT_SUFFIX,
            ext
        );
        p.with_file_name(name)
    };
    dedupe_out(cand, p)
}

pub fn unique_part_path(out: &Path, task_id: &str) -> PathBuf {
    let seq = PART_SEQ.fetch_add(1, Ordering::Relaxed);
    let safe_task = task_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(48)
        .collect::<String>();
    let name = out
        .file_name()
        .map(|v| v.to_string_lossy().to_string())
        .unwrap_or_else(|| "output".into());
    out.with_file_name(format!(
        ".{name}.{}.{seq}.{}.part",
        std::process::id(),
        if safe_task.is_empty() {
            "job"
        } else {
            &safe_task
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static SEQ: AtomicU32 = AtomicU32::new(1);

    fn unique_dir(tag: &str) -> PathBuf {
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "pix-squeeze-test-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn gradient(w: u32, h: u32) -> RgbaImage {
        let mut pixels = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                pixels.extend_from_slice(&[
                    (x as u8).wrapping_mul(3),
                    (y as u8).wrapping_mul(2),
                    ((x + y) as u8).wrapping_add(30),
                    if y % 10 == 0 { 120 } else { 255 },
                ]);
            }
        }
        RgbaImage::from_raw(w, h, pixels).expect("test image")
    }

    #[test]
    fn sharpen_changes_encoded_output() {
        let img = gradient(420, 320);
        for (label, fmt) in [("jpeg", OutFormat::Jpeg), ("webp", OutFormat::Webp)] {
            let plain =
                compress_pixel(&img, fmt, 50, 75, false, OutputMetadata::default()).expect("plain");
            let sharp =
                compress_pixel(&img, fmt, 50, 75, true, OutputMetadata::default()).expect("sharp");
            assert_ne!(plain.data, sharp.data, "{label} 锐化没有改变输出");
        }
    }

    #[test]
    fn transparent_jpeg_is_flagged_as_white_flattened() {
        let mut raw = vec![255u8; 20 * 10 * 4];
        for alpha in raw.iter_mut().skip(3).step_by(4) {
            *alpha = 64;
        }
        let img = RgbaImage::from_raw(20, 10, raw).unwrap();
        let outcome = compress_pixel(
            &img,
            OutFormat::Jpeg,
            100,
            80,
            false,
            OutputMetadata::default(),
        )
        .unwrap();
        assert!(outcome.meta.transparent_flattened);
        assert!(outcome.meta.warnings.iter().any(|w| w.contains("白底")));
    }

    #[test]
    fn output_format_resolution_matches_original_rules() {
        assert_eq!(resolve_out_fmt("jpg", "auto"), OutFormat::Jpeg);
        assert_eq!(resolve_out_fmt("webp", "auto"), OutFormat::Webp);
        assert_eq!(resolve_out_fmt("bmp", "auto"), OutFormat::Png);
        assert_eq!(resolve_out_fmt("png", "jpeg"), OutFormat::Jpeg);
    }

    #[test]
    fn auto_output_extension_follows_actual_format() {
        let dir = unique_dir("ext");
        for (src_name, expect) in [
            ("a.bmp", "a_压缩.png"),
            ("b.tif", "b_压缩.png"),
            ("c.gif", "c_压缩.png"),
            ("d.jpeg", "d_压缩.jpeg"),
            ("e.PNG", "e_压缩.PNG"),
            ("f.webp", "f_压缩.webp"),
        ] {
            let src = dir.join(src_name);
            fs::write(&src, b"x").unwrap();
            let out = out_path_for(&src.to_string_lossy(), "auto", "suffix", "").unwrap();
            assert_eq!(
                out.file_name().unwrap().to_string_lossy(),
                expect,
                "auto 输出的扩展名必须等于实际编码格式（{src_name}）"
            );
        }
    }

    #[test]
    fn concurrent_same_stem_reserves_distinct_outputs() {
        let out_dir = unique_dir("race");
        let dirs: Vec<PathBuf> = (0..3)
            .map(|i| {
                let d = unique_dir(&format!("race-src-{i}"));
                fs::write(d.join("photo.jpg"), b"x").unwrap();
                d
            })
            .collect();
        let handles: Vec<_> = dirs
            .into_iter()
            .map(|d| {
                let out = out_dir.clone();
                std::thread::spawn(move || {
                    out_path_for(
                        &d.join("photo.jpg").to_string_lossy(),
                        "jpeg",
                        "folder",
                        &out.to_string_lossy(),
                    )
                })
            })
            .collect();
        let paths: Vec<PathBuf> = handles
            .into_iter()
            .map(|h| h.join().unwrap().unwrap())
            .collect();
        let unique: std::collections::HashSet<_> = paths.iter().collect();
        assert_eq!(
            unique.len(),
            3,
            "并发同名输入必须拿到不同输出路径：{paths:?}"
        );
    }

    #[test]
    fn suffix_output_does_not_overwrite_existing_file() {
        let dir = unique_dir("suffix");
        let src = dir.join("照片.jpg");
        fs::write(&src, b"source").unwrap();
        let first = out_path_for(&src.to_string_lossy(), "auto", "suffix", "").unwrap();
        assert_eq!(
            first.file_name().unwrap().to_string_lossy(),
            "照片_压缩.jpg"
        );
        fs::write(&first, b"existing").unwrap();
        let second = out_path_for(&src.to_string_lossy(), "auto", "suffix", "").unwrap();
        assert_eq!(
            second.file_name().unwrap().to_string_lossy(),
            "照片_压缩(2).jpg"
        );
    }

    #[test]
    fn folder_output_uses_requested_directory() {
        let source_dir = unique_dir("folder-source");
        let out_dir = unique_dir("folder-dest");
        let src = source_dir.join("image.bmp");
        fs::write(&src, b"source").unwrap();
        let out = out_path_for(
            &src.to_string_lossy(),
            "jpeg",
            "folder",
            &out_dir.to_string_lossy(),
        )
        .unwrap();
        assert_eq!(out.parent().unwrap(), out_dir);
        assert_eq!(out.extension().unwrap(), "jpeg");
    }

    #[test]
    fn reservation_is_case_insensitive_and_part_names_are_unique() {
        clear_reserved_outputs();
        let out_dir = unique_dir("case-reserve");
        let a_dir = unique_dir("case-a");
        let b_dir = unique_dir("case-b");
        let a = a_dir.join("Photo.jpg");
        let b = b_dir.join("photo.jpg");
        fs::write(&a, b"a").unwrap();
        fs::write(&b, b"b").unwrap();
        let first = out_path_for(
            &a.to_string_lossy(),
            "jpeg",
            "folder",
            &out_dir.to_string_lossy(),
        )
        .unwrap();
        let second = out_path_for(
            &b.to_string_lossy(),
            "jpeg",
            "folder",
            &out_dir.to_string_lossy(),
        )
        .unwrap();
        assert_ne!(
            reservation_key(&first),
            reservation_key(&second),
            "大小写等价候选名不能共享预约键"
        );
        assert_ne!(
            unique_part_path(&first, "job-a"),
            unique_part_path(&first, "job-a")
        );
        assert!(unique_part_path(&first, "job-a")
            .to_string_lossy()
            .ends_with(".part"));
    }

    #[test]
    fn pixel_mode_scales_and_reports_size() {
        let img = gradient(200, 100);
        let result = compress_pixel(
            &img,
            OutFormat::Jpeg,
            50,
            80,
            false,
            OutputMetadata::default(),
        )
        .unwrap();
        assert_eq!(result.meta.out_w, 100);
        assert_eq!(result.meta.out_h, 50);
        assert_eq!(result.meta.out_bytes, result.data.len() as u64);
        assert!(result.meta.scaled);
    }

    #[test]
    fn preview_uses_real_percent_and_sharpen_semantics() {
        let base = gradient(200, 100);
        let settings = Settings {
            mode: "pixel".into(),
            percent: 50,
            quality: 80,
            sharpen: true,
            ..Settings::default()
        };
        let sharp = compress_preview_with_settings_cancellable(
            &base,
            200,
            100,
            OutFormat::Jpeg,
            &settings,
            OutputMetadata::default(),
            Some(5),
            None,
        )
        .unwrap();
        assert_eq!((sharp.meta.out_w, sharp.meta.out_h), (100, 50));
        let plain = compress_pixel(
            &base,
            OutFormat::Jpeg,
            50,
            80,
            false,
            OutputMetadata::default(),
        )
        .unwrap();
        assert_ne!(sharp.data, plain.data, "预览必须应用缩放后的锐化");
    }

    #[test]
    fn size_mode_obeys_target_and_is_not_empty() {
        let img = gradient(1000, 800);
        let result = compress_to_target(
            &img,
            OutFormat::Jpeg,
            200,
            true,
            OutputMetadata::default(),
            None,
        )
        .unwrap();
        assert!(!result.data.is_empty());
        assert_eq!(result.meta.out_bytes, result.data.len() as u64);
        assert!(result.meta.out_bytes <= 200 * 1024);
        assert!(result.meta.reached_target);
    }

    #[test]
    fn size_mode_hard_budget_caps_total_encodes() {
        let img = gradient(1000, 800);
        let settings = Settings {
            mode: "size".into(),
            target_kb: 1,
            ..Settings::default()
        };
        let result = compress_with_settings_budget(
            &img,
            OutFormat::Jpeg,
            &settings,
            OutputMetadata::default(),
            Some(2),
        )
        .unwrap();
        assert!(
            result.meta.encode_attempts <= 2,
            "encode budget leaked: {}",
            result.meta.encode_attempts
        );
        assert!(result.meta.budget_exhausted);
        assert!(!result.meta.reached_target);
    }

    #[test]
    fn cooperative_cancel_stops_before_encoding() {
        let img = gradient(100, 80);
        let settings = Settings {
            mode: "size".into(),
            ..Settings::default()
        };
        let cancel = || true;
        let err = compress_with_settings_cancellable(
            &img,
            OutFormat::Jpeg,
            &settings,
            OutputMetadata::default(),
            Some(8),
            Some(&cancel),
        )
        .unwrap_err();
        assert_eq!(err, CANCELLED_ERROR);
    }

    #[test]
    fn paletted_png_keeps_exif_and_alpha() {
        let img = gradient(40, 30);
        let exif = b"Exif\0\0II*\0\x08\0\0\0\x01\0\x03\0\x01\0\0\0\x01\0\0\0\0\0\0\0";
        let png = encode_png_paletted(
            &img,
            256,
            0.6,
            OutputMetadata {
                exif: Some(exif),
                icc: None,
            },
        )
        .unwrap();
        assert_eq!(&png[0..8], b"\x89PNG\r\n\x1a\n");
        assert!(png.windows(4).any(|chunk| chunk == b"eXIf"));
        let decoded = image::load_from_memory(&png).unwrap().to_rgba8();
        assert!(decoded.pixels().any(|p| p[3] < 255));
    }
}
