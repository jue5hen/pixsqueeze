#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod compress;
mod image_util;
mod resources;
mod settings;

use compress::{
    compress_preview_with_settings_cancellable, compress_with_settings,
    compress_with_settings_cancellable, out_path_for, resolve_out_fmt, unique_part_path,
    CANCELLED_ERROR, OUTPUT_SUFFIX,
};
use image_util::{
    clear_cache, data_url, encode_image, encode_thumb, file_matches_format, has_alpha, load_image,
    load_image_with_options, preview_base, probe_dimensions, smart_resize, ImageMetadata,
    LoadOptions, OutFormat, OutputMetadata, WEBP_MAX_DIMENSION,
};
use resources::{WorkShape, PROCESS_WORKING_BUDGET_BYTES};
use serde::Serialize;
use settings::{log_error, Settings};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter};

const INPUT_EXTS: [&str; 8] = ["jpg", "jpeg", "png", "webp", "bmp", "gif", "tif", "tiff"];
const ESTIMATE_ENCODE_BUDGET: u32 = 8;
const INTERACTIVE_FULL_MAX_PIXELS: u64 = 32_000_000;

/// 单实例前端只允许一个批任务。job_id 由前端生成，取消按 id 精确命中，
/// 避免“刚点开始就点停止”时旧全局 bool 被后启动的任务重新清零。
static ACTIVE_JOB: Mutex<Option<String>> = Mutex::new(None);
static CANCEL_JOB: Mutex<Option<String>> = Mutex::new(None);
static LATEST_ESTIMATE_REQUEST: AtomicU64 = AtomicU64::new(0);
static LATEST_FULL_PREVIEW_REQUEST: AtomicU64 = AtomicU64::new(0);

struct ActiveJobGuard {
    job_id: String,
}

impl ActiveJobGuard {
    fn begin(job_id: &str) -> Result<Self, String> {
        let job_id = job_id.trim();
        if job_id.is_empty() {
            return Err("任务 ID 不能为空".into());
        }
        let mut active = ACTIVE_JOB
            .lock()
            .map_err(|_| "任务状态锁已损坏".to_string())?;
        if let Some(existing) = active.as_deref() {
            return Err(format!("已有压缩任务正在运行（{existing}）"));
        }
        *active = Some(job_id.to_string());
        Ok(Self {
            job_id: job_id.to_string(),
        })
    }
}

impl Drop for ActiveJobGuard {
    fn drop(&mut self) {
        if let Ok(mut active) = ACTIVE_JOB.lock() {
            if active.as_deref() == Some(self.job_id.as_str()) {
                *active = None;
            }
        }
        if let Ok(mut cancel) = CANCEL_JOB.lock() {
            if cancel.as_deref() == Some(self.job_id.as_str()) {
                *cancel = None;
            }
        }
    }
}

fn request_cancel(job_id: &str) {
    let active = ACTIVE_JOB.lock().ok();
    if active
        .as_ref()
        .is_some_and(|active| active.as_deref().is_none_or(|id| id == job_id))
    {
        if let Ok(mut cancel) = CANCEL_JOB.lock() {
            *cancel = Some(job_id.to_string());
        }
    }
}

fn job_is_cancelled(job_id: &str) -> bool {
    CANCEL_JOB
        .lock()
        .ok()
        .is_some_and(|cancel| cancel.as_deref() == Some(job_id))
}

fn import_worker_count(paths: &[PathBuf], total: usize, cap: usize) -> usize {
    let peak = paths
        .iter()
        .filter_map(|p| probe_dimensions(&p.to_string_lossy()))
        .filter_map(|(w, h)| resources::estimate_import_peak_bytes(w, h).ok())
        .max()
        .unwrap_or(0);
    resources::workers_for_peak(peak, total, cap, PROCESS_WORKING_BUDGET_BYTES)
}

fn estimated_work_peak(p: &str, settings: &Settings) -> Option<u64> {
    let (w, h) = probe_dimensions(p)?;
    let (out_w, out_h) = if settings.mode == "pixel" {
        let percent = settings.percent.clamp(10, 100) as u32;
        (
            ((w as f64 * percent as f64 / 100.0).round() as u32).max(1),
            ((h as f64 * percent as f64 / 100.0).round() as u32).max(1),
        )
    } else {
        (w, h)
    };
    let ext = Path::new(p)
        .extension()
        .map(|v| v.to_string_lossy().to_string())
        .unwrap_or_default();
    let fmt = resolve_out_fmt(&ext, &settings.fmt);
    resources::estimate_work_peak_bytes(
        w,
        h,
        out_w,
        out_h,
        WorkShape {
            scaled: (out_w, out_h) != (w, h),
            sharpen: (out_w, out_h) != (w, h) && settings.sharpen,
            quantize: fmt == OutFormat::Png,
            flatten_rgb: fmt == OutFormat::Jpeg,
        },
    )
    .ok()
}

fn batch_worker_count(paths: &[String], settings: &Settings, total: usize, cap: usize) -> usize {
    let peak = paths
        .iter()
        .filter_map(|p| estimated_work_peak(p, settings))
        .max()
        .unwrap_or(0);
    resources::workers_for_peak(peak, total, cap, PROCESS_WORKING_BUDGET_BYTES)
}

#[derive(Serialize, Clone)]
struct ImportedImage {
    path: String,
    name: String,
    bytes: u64,
    width: u32,
    height: u32,
    thumb: String,
    has_alpha: bool,
    warnings: Vec<String>,
}

#[derive(Serialize, Clone, Debug)]
struct SkippedInput {
    path: String,
    reason: String,
}

#[derive(Serialize, Clone)]
struct ImportResult {
    images: Vec<ImportedImage>,
    errors: Vec<String>,
    skipped_outputs: Vec<SkippedInput>,
}

#[derive(Serialize, Clone)]
struct PreviewResult {
    orig_url: String,
    comp_url: String,
    base_w: u32,
    base_h: u32,
    comp_w: u32,
    comp_h: u32,
    format: String,
    transparent_flattened: bool,
    warnings: Vec<String>,
}

#[derive(Serialize, Clone)]
struct EstimateResult {
    out_w: u32,
    out_h: u32,
    out_bytes: u64,
    quality_used: u32,
    scaled: bool,
    reached_target: bool,
    encode_attempts: u32,
    budget_exhausted: bool,
    format: String,
    transparent_flattened: bool,
    warnings: Vec<String>,
}

#[derive(Serialize, Clone)]
struct BatchItemResult {
    ok: bool,
    cancelled: bool,
    src: String,
    out: String,
    bytes: u64,
    error: String,
    reached_target: bool,
    budget_exhausted: bool,
    out_w: u32,
    out_h: u32,
    format: String,
    transparent_flattened: bool,
    warnings: Vec<String>,
}

#[derive(Serialize, Clone)]
struct FullPreview {
    orig_url: String,
    comp_url: String,
    width: u32,
    height: u32,
    comp_w: u32,
    comp_h: u32,
    format: String,
    transparent_flattened: bool,
    warnings: Vec<String>,
}

#[derive(Serialize, Clone)]
struct BatchResult {
    job_id: String,
    total: usize,
    success: usize,
    reached_target: usize,
    not_reached: usize,
    failed: usize,
    cancelled_count: usize,
    warning_count: usize,
    cancelled: bool,
    results: Vec<BatchItemResult>,
    out_dir: String,
    first_error: String,
}

#[derive(Serialize, Clone)]
struct SettingsLoadResult {
    settings: Settings,
    warning: Option<String>,
}

#[derive(Serialize, Clone)]
struct ProgressPayload {
    job_id: String,
    done: usize,
    total: usize,
    path: String,
}

fn is_image_file(p: &Path) -> bool {
    p.is_file()
        && p.extension()
            .map(|e| INPUT_EXTS.contains(&e.to_string_lossy().to_ascii_lowercase().as_str()))
            .unwrap_or(false)
}

const OUTPUT_EXTS: [&str; 4] = ["jpg", "jpeg", "png", "webp"];

fn split_output_sequence(stem: &str) -> (&str, Option<u32>) {
    let Some(open) = stem.rfind('(') else {
        return (stem, None);
    };
    if !stem.ends_with(')') {
        return (stem, None);
    }
    let Ok(n) = stem[open + 1..stem.len() - 1].parse::<u32>() else {
        return (stem, None);
    };
    if n >= 2 {
        (&stem[..open], Some(n))
    } else {
        (stem, None)
    }
}

fn source_with_stem(
    dir: &Path,
    base: &str,
    current: &Path,
    different_ext: bool,
    source_non_output: bool,
) -> Option<PathBuf> {
    let current_ext = current
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .find_map(|entry| {
            let candidate = entry.path();
            if candidate == current || !is_image_file(&candidate) {
                return None;
            }
            let stem = candidate.file_stem()?.to_string_lossy();
            if !stem.eq_ignore_ascii_case(base) {
                return None;
            }
            let ext = candidate
                .extension()
                .map(|e| e.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            if different_ext && ext == current_ext {
                return None;
            }
            if source_non_output && OUTPUT_EXTS.contains(&ext.as_str()) {
                return None;
            }
            Some(candidate)
        })
}

/// 识别本程序可生成的完整命名语法。文件名本身仍不能证明来源，因此目录导入
/// 只跳过“疑似产物”并提供用户覆盖入口，而不是宣称零误判。
fn own_output_reason(p: &Path) -> Option<String> {
    let name = p.file_name()?;
    if name.to_string_lossy().ends_with(".part") {
        return Some("PixSqueeze 临时文件".into());
    }
    let ext = p
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if !OUTPUT_EXTS.contains(&ext.as_str()) {
        return None;
    }
    let stem = p.file_stem()?;
    let stem = stem.to_string_lossy();
    let dir = p.parent()?;
    let (without_seq, seq) = split_output_sequence(&stem);
    if let Some(base) = without_seq.strip_suffix(OUTPUT_SUFFIX) {
        if !base.is_empty() && source_with_stem(dir, base, p, false, false).is_some() {
            return Some("疑似 PixSqueeze 后缀输出（同目录存在对应源图）".into());
        }
    }
    if !without_seq.is_empty() {
        let different_ext = source_with_stem(dir, without_seq, p, true, true);
        if different_ext.is_some() {
            return Some("疑似 PixSqueeze 指定目录输出（同目录存在同名跨格式源图）".into());
        }
        if seq.is_some() && source_with_stem(dir, without_seq, p, false, false).is_some() {
            return Some("疑似 PixSqueeze 指定目录编号输出（同目录存在同名词干源图）".into());
        }
    }
    None
}

/// 自然序：`图2.png` 排在 `图10.png` 前面（字典序会把 10 排到 2 前面）。
fn natural_cmp(a: &Path, b: &Path) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (x, y) = (a.to_string_lossy(), b.to_string_lossy());
    let (xb, yb) = (x.as_bytes(), y.as_bytes());
    let (mut i, mut j) = (0usize, 0usize);
    while i < xb.len() && j < yb.len() {
        if xb[i].is_ascii_digit() && yb[j].is_ascii_digit() {
            let si = i;
            while i < xb.len() && xb[i].is_ascii_digit() {
                i += 1;
            }
            let sj = j;
            while j < yb.len() && yb[j].is_ascii_digit() {
                j += 1;
            }
            let nx: u128 = x[si..i].parse().unwrap_or(0);
            let ny: u128 = y[sj..j].parse().unwrap_or(0);
            match nx.cmp(&ny) {
                Ordering::Equal => {}
                ord => return ord,
            }
        } else {
            match xb[i].cmp(&yb[j]) {
                Ordering::Equal => {
                    i += 1;
                    j += 1;
                }
                ord => return ord,
            }
        }
    }
    (xb.len() - i).cmp(&(yb.len() - j))
}

fn collect_input_paths(
    inputs: &[String],
    include_own_outputs: bool,
) -> (Vec<PathBuf>, Vec<String>, Vec<SkippedInput>) {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut errors = Vec::new();
    let mut skipped = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let push =
        |p: PathBuf, out: &mut Vec<PathBuf>, seen: &mut std::collections::HashSet<String>| {
            let key = p.to_string_lossy().replace('\\', "/").to_lowercase();
            if seen.insert(key) {
                out.push(p);
            }
        };
    for raw in inputs {
        let p = PathBuf::from(raw);
        if p.is_dir() {
            match std::fs::read_dir(&p) {
                Ok(rd) => {
                    let mut files = Vec::new();
                    for entry in rd.filter_map(|e| e.ok()) {
                        let candidate = entry.path();
                        if !is_image_file(&candidate) {
                            continue;
                        }
                        if !include_own_outputs {
                            if let Some(reason) = own_output_reason(&candidate) {
                                skipped.push(SkippedInput {
                                    path: candidate.to_string_lossy().to_string(),
                                    reason,
                                });
                                continue;
                            }
                        }
                        files.push(candidate);
                    }
                    files.sort_by(|a, b| natural_cmp(a, b));
                    for f in files {
                        push(f, &mut out, &mut seen);
                    }
                }
                Err(e) => errors.push(format!("无法读取目录 {}：{e}", p.display())),
            }
        } else if is_image_file(&p) {
            // 显式选中的文件不按 `_压缩` 过滤：用户可能就是要再压一次
            push(p, &mut out, &mut seen);
        } else if p.exists() {
            errors.push(format!("不支持的图片格式：{}", p.display()));
        } else {
            errors.push(format!("路径不存在：{}", p.display()));
        }
    }
    (out, errors, skipped)
}

#[tauri::command]
async fn load_settings() -> SettingsLoadResult {
    let (settings, warning) = settings::load_settings();
    SettingsLoadResult { settings, warning }
}

#[tauri::command]
async fn save_settings(s: Settings, revision: u64) -> Result<(), String> {
    settings::save_settings(&s, revision).inspect_err(|e| log_error("保存设置", e))
}

#[tauri::command]
async fn import_paths(paths: Vec<String>, include_own_outputs: bool) -> ImportResult {
    tauri::async_runtime::spawn_blocking(move || {
        let (files, mut errors, skipped_outputs) = collect_input_paths(&paths, include_own_outputs);
        let total = files.len();
        let workers = import_worker_count(&files, total, 4);
        let mut slots: Vec<Option<Result<ImportedImage, String>>> =
            (0..total).map(|_| None).collect();
        {
            let next = AtomicUsize::new(0);
            let slots = Mutex::new(&mut slots);
            let files = &files;
            std::thread::scope(|scope| {
                for _ in 0..workers {
                    scope.spawn(|| loop {
                        let i = next.fetch_add(1, Ordering::SeqCst);
                        if i >= total {
                            break;
                        }
                        let p = &files[i];
                        let path_s = p.to_string_lossy().to_string();
                        let name = p
                            .file_name()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_default();
                        let bytes = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
                        let item = match load_image_with_options(
                            &path_s,
                            LoadOptions {
                                keep_exif: false,
                                keep_icc: false,
                            },
                        ) {
                            Ok((img, metadata)) => match encode_thumb(&img) {
                                Ok(thumb) => Ok(ImportedImage {
                                    path: path_s,
                                    name,
                                    bytes,
                                    width: img.width(),
                                    height: img.height(),
                                    thumb,
                                    has_alpha: has_alpha(&img),
                                    warnings: metadata.warnings,
                                }),
                                Err(e) => Err(format!("{name}：{e}")),
                            },
                            Err(e) => Err(format!("{name}：{e}")),
                        };
                        slots.lock().unwrap()[i] = Some(item);
                    });
                }
            });
        }
        let mut images = Vec::with_capacity(total);
        for slot in slots.into_iter().flatten() {
            match slot {
                Ok(img) => images.push(img),
                Err(e) => errors.push(e),
            }
        }
        clear_cache();
        for error in &errors {
            log_error("导入失败", error);
        }
        ImportResult {
            images,
            errors,
            skipped_outputs,
        }
    })
    .await
    .unwrap_or_else(|e| ImportResult {
        images: vec![],
        errors: vec![format!("导入失败：{e}")],
        skipped_outputs: vec![],
    })
}

fn preview_original_url(base: &image::RgbaImage, icc: Option<&[u8]>) -> Result<String, String> {
    let metadata = OutputMetadata { exif: None, icc };
    if has_alpha(base) {
        // PNG q95 里的 95 是无意义参数；有 alpha 时用 WebP q80：同样无 alpha 损失，体积小一个数量级（PS-A-050）
        let bytes = encode_image(base, OutFormat::Webp, 80, metadata)?;
        Ok(data_url(&bytes, "image/webp"))
    } else {
        let bytes = encode_image(base, OutFormat::Jpeg, 92, metadata)?;
        Ok(data_url(&bytes, "image/jpeg"))
    }
}

fn output_metadata(metadata: &ImageMetadata, keep_exif: bool) -> OutputMetadata<'_> {
    OutputMetadata {
        exif: keep_exif.then_some(metadata.exif.as_deref()).flatten(),
        icc: metadata.icc.as_deref(),
    }
}

#[tauri::command]
async fn make_preview(path: String, settings: Settings) -> Result<PreviewResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let result = (|| {
            let (img, metadata) = load_image_with_options(
                &path,
                LoadOptions {
                    keep_exif: settings.keep_exif,
                    keep_icc: true,
                },
            )?;
            let base = preview_base(&img, 900);
            let ext = Path::new(&path)
                .extension()
                .map(|e| e.to_string_lossy().to_string())
                .unwrap_or_default();
            let fmt = resolve_out_fmt(&ext, &settings.fmt);
            let metadata_ref = output_metadata(&metadata, settings.keep_exif);
            let mut outcome = compress_preview_with_settings_cancellable(
                &base,
                img.width(),
                img.height(),
                fmt,
                &settings,
                metadata_ref,
                Some(5),
                None,
            )?;
            outcome.meta.warnings.extend(metadata.warnings);
            let orig_url = preview_original_url(&base, metadata.icc.as_deref())?;
            let comp_url = data_url(&outcome.data, fmt.mime());
            Ok(PreviewResult {
                orig_url,
                comp_url,
                base_w: base.width(),
                base_h: base.height(),
                comp_w: outcome.meta.out_w,
                comp_h: outcome.meta.out_h,
                format: fmt.ext().to_string(),
                transparent_flattened: outcome.meta.transparent_flattened,
                warnings: outcome.meta.warnings,
            })
        })();
        if let Err(e) = &result {
            log_error("生成预览", &format!("{path}: {e}"));
        }
        result
    })
    .await
    .map_err(|e| format!("预览失败：{e}"))?
}

#[tauri::command]
async fn preview_full(
    request_id: u64,
    path: String,
    settings: Settings,
) -> Result<Option<FullPreview>, String> {
    LATEST_FULL_PREVIEW_REQUEST.store(request_id, Ordering::Release);
    tauri::async_runtime::spawn_blocking(move || {
        let result = (|| -> Result<Option<FullPreview>, String> {
            let superseded = || LATEST_FULL_PREVIEW_REQUEST.load(Ordering::Acquire) != request_id;
            if probe_dimensions(&path)
                .is_some_and(|(w, h)| (w as u64) * (h as u64) > INTERACTIVE_FULL_MAX_PIXELS)
            {
                return Ok(None);
            }
            let (img, metadata) = load_image_with_options(
                &path,
                LoadOptions {
                    keep_exif: settings.keep_exif,
                    keep_icc: true,
                },
            )?;
            let width = img.width();
            let height = img.height();
            let ext = Path::new(&path)
                .extension()
                .map(|e| e.to_string_lossy().to_string())
                .unwrap_or_default();
            let fmt = resolve_out_fmt(&ext, &settings.fmt);
            let metadata_ref = output_metadata(&metadata, settings.keep_exif);
            // 估算只求量级，给 5 次编码就够；用户拖着滑块时不再叠加全图压缩（PS-A-007）
            let mut outcome = compress::compress_with_settings_cancellable(
                &img,
                fmt,
                &settings,
                metadata_ref,
                Some(5),
                Some(&superseded),
            )?;
            outcome.meta.warnings.extend(metadata.warnings);
            // 高清对比只需要「够看清细节」：把原图一侧限制到 4096 长边，避免几十 MB 的
            // base64 穿过 IPC；压缩侧保持真实产物（PS-A-008）。
            const FULL_PREVIEW_MAX_SIDE: u32 = 4096;
            let orig_url = if width.max(height) > FULL_PREVIEW_MAX_SIDE {
                let k = FULL_PREVIEW_MAX_SIDE as f64 / width.max(height) as f64;
                let w = (width as f64 * k).round().max(1.0) as u32;
                let h = (height as f64 * k).round().max(1.0) as u32;
                let reduced = smart_resize(&img, w, h);
                preview_original_url(&reduced, metadata.icc.as_deref())?
            } else {
                preview_original_url(&img, metadata.icc.as_deref())?
            };
            let comp_url = data_url(&outcome.data, fmt.mime());
            Ok(Some(FullPreview {
                orig_url,
                comp_url,
                width,
                height,
                comp_w: outcome.meta.out_w,
                comp_h: outcome.meta.out_h,
                format: fmt.ext().to_string(),
                transparent_flattened: outcome.meta.transparent_flattened,
                warnings: outcome.meta.warnings,
            }))
        })();
        if let Err(e) = &result {
            log_error("高清预览", &format!("{path}: {e}"));
        }
        result
    })
    .await
    .map_err(|e| format!("高清预览失败：{e}"))?
}

#[tauri::command]
async fn estimate_full(
    request_id: u64,
    path: String,
    settings: Settings,
) -> Result<EstimateResult, String> {
    LATEST_ESTIMATE_REQUEST.store(request_id, Ordering::Release);
    tauri::async_runtime::spawn_blocking(move || {
        let result = (|| {
            let superseded = || LATEST_ESTIMATE_REQUEST.load(Ordering::Acquire) != request_id;
            if probe_dimensions(&path)
                .is_some_and(|(w, h)| (w as u64) * (h as u64) > INTERACTIVE_FULL_MAX_PIXELS)
            {
                return Err(format!(
                    "图片超过 {} 万像素，已跳过全图估算；仍可直接按当前设置压缩",
                    INTERACTIVE_FULL_MAX_PIXELS / 10_000
                ));
            }
            let (img, metadata) = load_image_with_options(
                &path,
                LoadOptions {
                    keep_exif: settings.keep_exif,
                    keep_icc: true,
                },
            )?;
            let ext = Path::new(&path)
                .extension()
                .map(|e| e.to_string_lossy().to_string())
                .unwrap_or_default();
            let fmt = resolve_out_fmt(&ext, &settings.fmt);
            let metadata_ref = output_metadata(&metadata, settings.keep_exif);
            let mut outcome = compress_with_settings_cancellable(
                &img,
                fmt,
                &settings,
                metadata_ref,
                Some(ESTIMATE_ENCODE_BUDGET),
                Some(&superseded),
            )?;
            outcome.meta.warnings.extend(metadata.warnings);
            Ok(EstimateResult {
                out_w: outcome.meta.out_w,
                out_h: outcome.meta.out_h,
                out_bytes: outcome.meta.out_bytes,
                quality_used: outcome.meta.quality_used,
                scaled: outcome.meta.scaled,
                reached_target: outcome.meta.reached_target,
                encode_attempts: outcome.meta.encode_attempts,
                budget_exhausted: outcome.meta.budget_exhausted,
                format: fmt.ext().to_string(),
                transparent_flattened: outcome.meta.transparent_flattened,
                warnings: outcome.meta.warnings,
            })
        })();
        if let Err(e) = &result {
            log_error("估算", &format!("{path}: {e}"));
        }
        result
    })
    .await
    .map_err(|e| format!("估算失败：{e}"))?
}

fn process_one(
    src: &str,
    settings: &Settings,
    job_id: &str,
    cancel: &dyn Fn() -> bool,
) -> BatchItemResult {
    let result = (|| -> Result<(PathBuf, u64, compress::CompressMeta, String), String> {
        let (img, metadata) = load_image_with_options(
            src,
            LoadOptions {
                keep_exif: settings.keep_exif,
                keep_icc: true,
            },
        )?;
        let ext = Path::new(src)
            .extension()
            .map(|e| e.to_string_lossy().to_string())
            .unwrap_or_default();
        let fmt = resolve_out_fmt(&ext, &settings.fmt);
        let metadata_ref = output_metadata(&metadata, settings.keep_exif);
        let mut outcome = compress_with_settings_cancellable(
            &img,
            fmt,
            settings,
            metadata_ref,
            None,
            Some(cancel),
        )?;
        outcome.meta.warnings.extend(metadata.warnings);

        let mut collision_error = None;
        for _ in 0..64 {
            let out = out_path_for(
                src,
                &settings.fmt,
                &settings.out_mode,
                &settings.last_out_dir,
            )?;
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("创建输出目录失败（{}）：{e}", parent.display()))?;
            }
            // 唯一任务临时名 + create_new：两个实例/残留 .part 不会争抢同一临时文件。
            let tmp = unique_part_path(&out, job_id);
            let mut file = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&tmp)
                .map_err(|e| format!("创建临时文件失败（{}）：{e}", tmp.display()))?;
            if let Err(e) = file.write_all(&outcome.data).and_then(|_| file.sync_all()) {
                drop(file);
                let _ = std::fs::remove_file(&tmp);
                return Err(format!("写入文件失败：{e}"));
            }
            drop(file);
            match std::fs::rename(&tmp, &out) {
                Ok(()) => {
                    let bytes = std::fs::metadata(&out)
                        .map_err(|e| format!("读取最终产物大小失败：{e}"))?
                        .len();
                    return Ok((out, bytes, outcome.meta, fmt.ext().to_string()));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let _ = std::fs::remove_file(&tmp);
                    collision_error = Some(e);
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(format!("提交输出文件失败（{}）：{e}", out.display()));
                }
            }
        }
        Err(format!(
            "输出文件名连续冲突，已停止提交{}",
            collision_error
                .map(|e| format!("：{e}"))
                .unwrap_or_default()
        ))
    })();
    match result {
        Ok((out, bytes, meta, format)) => BatchItemResult {
            ok: true,
            cancelled: false,
            src: src.to_string(),
            out: out.to_string_lossy().to_string(),
            bytes,
            error: String::new(),
            reached_target: meta.reached_target,
            budget_exhausted: meta.budget_exhausted,
            out_w: meta.out_w,
            out_h: meta.out_h,
            format,
            transparent_flattened: meta.transparent_flattened,
            warnings: meta.warnings,
        },
        Err(e) if e == CANCELLED_ERROR => BatchItemResult {
            ok: false,
            cancelled: true,
            src: src.to_string(),
            out: String::new(),
            bytes: 0,
            error: "已取消".into(),
            reached_target: false,
            budget_exhausted: false,
            out_w: 0,
            out_h: 0,
            format: String::new(),
            transparent_flattened: false,
            warnings: vec![],
        },
        Err(e) => {
            log_error("压缩失败", &format!("{src}: {e}"));
            BatchItemResult {
                ok: false,
                cancelled: false,
                src: src.to_string(),
                out: String::new(),
                bytes: 0,
                error: e,
                reached_target: false,
                budget_exhausted: false,
                out_w: 0,
                out_h: 0,
                format: String::new(),
                transparent_flattened: false,
                warnings: vec![],
            }
        }
    }
}

#[tauri::command]
async fn compress_batch(
    app: AppHandle,
    job_id: String,
    paths: Vec<String>,
    settings: Settings,
) -> Result<BatchResult, String> {
    let active_job = ActiveJobGuard::begin(&job_id)?;
    tauri::async_runtime::spawn_blocking(move || {
        let _active_job = active_job;
        let total = paths.len();
        compress::clear_reserved_outputs();
        let results: Arc<Mutex<Vec<Option<BatchItemResult>>>> =
            Arc::new(Mutex::new(vec![None; total]));
        let next = Arc::new(AtomicUsize::new(0));
        let done = Arc::new(AtomicUsize::new(0));
        let workers = batch_worker_count(&paths, &settings, total, 3);
        let settings = Arc::new(settings);
        std::thread::scope(|scope| {
            for _ in 0..workers {
                let results = results.clone();
                let next = next.clone();
                let done = done.clone();
                let paths = &paths;
                let settings = settings.clone();
                let app = app.clone();
                let worker_job_id = job_id.clone();
                scope.spawn(move || {
                    let cancelled = || job_is_cancelled(&worker_job_id);
                    loop {
                        let i = next.fetch_add(1, Ordering::SeqCst);
                        if i >= total || cancelled() {
                            break;
                        }
                        let r = process_one(&paths[i], &settings, &worker_job_id, &cancelled);
                        let d = done.fetch_add(1, Ordering::SeqCst) + 1;
                        let _ = app.emit(
                            "pix-progress",
                            ProgressPayload {
                                job_id: worker_job_id.clone(),
                                done: d,
                                total,
                                path: paths[i].clone(),
                            },
                        );
                        results.lock().unwrap()[i] = Some(r);
                    }
                });
            }
        });
        clear_cache();
        let requested_cancel = job_is_cancelled(&job_id);
        let final_results: Vec<BatchItemResult> = results
            .lock()
            .unwrap()
            .iter()
            .enumerate()
            .map(|(i, r)| {
                r.clone().unwrap_or(BatchItemResult {
                    ok: false,
                    cancelled: true,
                    src: paths.get(i).cloned().unwrap_or_default(),
                    out: String::new(),
                    bytes: 0,
                    error: "已取消".into(),
                    reached_target: false,
                    budget_exhausted: false,
                    out_w: 0,
                    out_h: 0,
                    format: String::new(),
                    transparent_flattened: false,
                    warnings: vec![],
                })
            })
            .collect();
        let cancelled = requested_cancel || final_results.iter().any(|r| r.cancelled);
        let success = final_results.iter().filter(|r| r.ok).count();
        let reached_target = final_results
            .iter()
            .filter(|r| r.ok && r.reached_target)
            .count();
        let not_reached = final_results
            .iter()
            .filter(|r| r.ok && !r.reached_target)
            .count();
        let failed = final_results
            .iter()
            .filter(|r| !r.ok && !r.cancelled)
            .count();
        let cancelled_count = final_results.iter().filter(|r| r.cancelled).count();
        let warning_count = final_results.iter().map(|r| r.warnings.len()).sum();
        let first_error = final_results
            .iter()
            .find(|r| !r.ok && !r.cancelled)
            .map(|r| r.error.clone())
            .unwrap_or_default();
        let out_dir = final_results
            .iter()
            .find(|r| r.ok)
            .and_then(|r| {
                Path::new(&r.out)
                    .parent()
                    .map(|p| p.to_string_lossy().to_string())
            })
            .or_else(|| {
                paths.first().and_then(|p| {
                    Path::new(p)
                        .parent()
                        .map(|p| p.to_string_lossy().to_string())
                })
            })
            .unwrap_or_default();
        Ok(BatchResult {
            job_id,
            total,
            success,
            reached_target,
            not_reached,
            failed,
            cancelled_count,
            warning_count,
            cancelled,
            results: final_results,
            out_dir,
            first_error,
        })
    })
    .await
    .map_err(|e| format!("批量压缩失败：{e}"))?
}

#[tauri::command]
fn cancel_batch(job_id: String) {
    request_cancel(&job_id);
}

#[tauri::command]
fn reveal(path: String) {
    let p = Path::new(&path);
    let target = if p.is_dir() {
        path.clone()
    } else if p.exists() {
        // 带引号，避免路径中的空格/逗号被 explorer 当成参数分隔
        format!("/select,\"{path}\"")
    } else if let Some(parent) = p.parent() {
        parent.to_string_lossy().to_string()
    } else {
        return;
    };
    if let Err(e) = std::process::Command::new("explorer.exe")
        .arg(&target)
        .spawn()
    {
        log_error("REVEAL", &format!("打开资源管理器失败（{target}）：{e}"));
    }
}

#[tauri::command]
fn cli_paths() -> Vec<String> {
    std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .filter(|a| Path::new(a).exists())
        .collect()
}

fn run_self_test() -> Result<(), String> {
    let started = std::time::Instant::now();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let at = args
        .iter()
        .position(|a| a == "--self-test")
        .ok_or("缺少 --self-test")?;
    let input = args.get(at + 1).ok_or("缺少输入图片")?;
    let out_dir = PathBuf::from(args.get(at + 2).ok_or("缺少输出目录")?);
    let requested_fmt = args.get(at + 3).map(String::as_str).unwrap_or("auto");
    std::fs::create_dir_all(&out_dir).map_err(|e| format!("无法创建输出目录：{e}"))?;

    let (image, metadata) = load_image_with_options(input, LoadOptions::default())?;
    let ext = Path::new(input)
        .extension()
        .map(|e| e.to_string_lossy().to_string())
        .unwrap_or_default();
    let fmt = resolve_out_fmt(&ext, requested_fmt);
    let source_w = image.width();
    let source_h = image.height();

    let pixel_settings = Settings {
        mode: "pixel".into(),
        percent: 50,
        quality: 82,
        ..Settings::default()
    };
    let ext = fmt.ext();
    let pixel_meta = output_metadata(&metadata, pixel_settings.keep_exif);
    let pixel = compress_with_settings(&image, fmt, &pixel_settings, pixel_meta)?;
    let pixel_path = out_dir.join(format!("self-test-pixel.{ext}"));
    std::fs::write(&pixel_path, &pixel.data).map_err(|e| e.to_string())?;

    let size_settings = Settings {
        mode: "size".into(),
        target_kb: 100,
        quality: 82,
        ..Settings::default()
    };
    let size_meta = output_metadata(&metadata, size_settings.keep_exif);
    let size_result = compress_with_settings(&image, fmt, &size_settings, size_meta)?;
    if !size_result.meta.reached_target {
        return Err(format!(
            "按目标大小自检未达标：目标 {} KB，实际 {} 字节，预算耗尽={}，尝试={}",
            size_settings.target_kb,
            size_result.meta.out_bytes,
            size_result.meta.budget_exhausted,
            size_result.meta.encode_attempts
        ));
    }
    let size_path = out_dir.join(format!("self-test-size.{ext}"));
    std::fs::write(&size_path, &size_result.data).map_err(|e| e.to_string())?;

    let preview = preview_base(&image, 900);
    let preview_meta = output_metadata(&metadata, size_settings.keep_exif);
    let preview_result = compress_preview_with_settings_cancellable(
        &preview,
        source_w,
        source_h,
        fmt,
        &size_settings,
        preview_meta,
        Some(5),
        None,
    )?;
    let preview_path = out_dir.join(format!("self-test-preview.{}", fmt.ext()));
    std::fs::write(&preview_path, &preview_result.data).map_err(|e| e.to_string())?;

    for path in [&pixel_path, &size_path, &preview_path] {
        let (loaded, _) = load_image(&path.to_string_lossy())?;
        if loaded.width() == 0 || loaded.height() == 0 {
            return Err(format!("{} 无法读回", path.display()));
        }
        // 扩展名必须与实际编码格式一致，否则「换个解码器」就是下一次事故
        if !file_matches_format(path, fmt) {
            return Err(format!("{} 的扩展名与实际格式不一致", path.display()));
        }
    }
    for (path, expected) in [
        (&pixel_path, pixel.meta.out_bytes),
        (&size_path, size_result.meta.out_bytes),
        (&preview_path, preview_result.meta.out_bytes),
    ] {
        let actual = std::fs::metadata(path)
            .map_err(|e| format!("无法读取自检产物大小 {}：{e}", path.display()))?
            .len();
        if actual != expected {
            return Err(format!(
                "{} 最终字节数与结果元数据不一致：{} != {}",
                path.display(),
                actual,
                expected
            ));
        }
    }
    let too_wide = image::RgbaImage::from_raw(
        WEBP_MAX_DIMENSION + 1,
        1,
        vec![255; (WEBP_MAX_DIMENSION as usize + 1) * 4],
    )
    .ok_or("无法创建 WebP 边界自检图")?;
    let boundary_error = encode_image(&too_wide, OutFormat::Webp, 82, OutputMetadata::default())
        .expect_err("WebP 超单边上限必须返回错误而不是 panic");
    if !boundary_error.contains(&WEBP_MAX_DIMENSION.to_string()) {
        return Err(format!("WebP 边界错误信息不明确：{boundary_error}"));
    }
    let scaled_boundary = compress_with_settings(
        &too_wide,
        OutFormat::Webp,
        &Settings {
            mode: "pixel".into(),
            percent: 50,
            ..Settings::default()
        },
        OutputMetadata::default(),
    )?;
    if scaled_boundary.meta.out_w > WEBP_MAX_DIMENSION {
        return Err("WebP 边界图缩放后仍超过单边上限".into());
    }
    let _ = encode_thumb(&image);
    clear_cache();

    println!(
        "PixSqueeze self-test PASS: {}x{} [{}] -> {} B pixel / {} B size (target reached) / {} B preview, encodes {}/{}/{}, elapsed {} ms",
        source_w,
        source_h,
        fmt.ext(),
        pixel.data.len(),
        size_result.data.len(),
        preview_result.data.len(),
        pixel.meta.encode_attempts,
        size_result.meta.encode_attempts,
        preview_result.meta.encode_attempts,
        started.elapsed().as_millis()
    );
    Ok(())
}

#[cfg(windows)]
mod single_instance {
    use std::ffi::c_void;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONINFORMATION, MB_OK};

    pub struct InstanceGuard(HANDLE);

    impl Drop for InstanceGuard {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn acquire(name: &str) -> Result<Option<InstanceGuard>, String> {
        let name = wide(name);
        let handle = unsafe { CreateMutexW(std::ptr::null::<c_void>() as _, 1, name.as_ptr()) };
        if handle.is_null() {
            return Err(format!(
                "创建单实例互斥体失败：{}",
                std::io::Error::last_os_error()
            ));
        }
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe {
                CloseHandle(handle);
            }
            return Ok(None);
        }
        Ok(Some(InstanceGuard(handle)))
    }

    pub fn notify_already_running() {
        let text = wide("PixSqueeze 已在运行，请切换到已打开的窗口。\n\n同一用户会话只允许一个实例，以避免设置相互覆盖。");
        let title = wide("PixSqueeze");
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                text.as_ptr(),
                title.as_ptr(),
                MB_OK | MB_ICONINFORMATION,
            );
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn named_mutex_blocks_second_holder_until_drop() {
            let name = format!(
                "Local\\PixSqueeze-test-{}-{}",
                std::process::id(),
                chrono::Local::now()
                    .timestamp_nanos_opt()
                    .unwrap_or_default()
            );
            let first = acquire(&name).unwrap().expect("first holder");
            assert!(acquire(&name).unwrap().is_none());
            drop(first);
            let third = acquire(&name).unwrap().expect("holder after drop");
            drop(third);
        }
    }
}

fn main() {
    // GUI 子系统没有控制台；未捕获 panic 也必须落到便携目录的错误日志里。
    std::panic::set_hook(Box::new(|info| {
        log_error("程序崩溃", &info.to_string());
    }));
    if std::env::args().any(|a| a == "--self-test") {
        match run_self_test() {
            Ok(()) => std::process::exit(0),
            Err(error) => {
                eprintln!("PixSqueeze self-test FAIL: {error}");
                std::process::exit(1);
            }
        }
    }

    #[cfg(windows)]
    let _single_instance = match single_instance::acquire("Local\\com.dsh.pixsqueeze") {
        Ok(Some(guard)) => guard,
        Ok(None) => {
            single_instance::notify_already_running();
            std::process::exit(0);
        }
        Err(error) => {
            log_error("单实例", &error);
            std::process::exit(1);
        }
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            load_settings,
            save_settings,
            import_paths,
            make_preview,
            preview_full,
            estimate_full,
            compress_batch,
            cancel_batch,
            reveal,
            cli_paths
        ])
        .run(tauri::generate_context!())
        .expect("error while running PixSqueeze");
}
#[cfg(test)]
mod performance_tests {
    use super::*;
    use crate::compress::compress_pixel;
    use image::RgbaImage;
    use std::fs;
    use std::time::{Duration, Instant};

    /// 分阶段计时 + 中位数：单次冷启动计时会把「机器此刻忙」当成「算法退化」，
    /// 也说不清慢在哪一段（PS-A-051）。预热 1 次，再测 3 次取中位数。
    #[test]
    #[ignore = "需要真实机器的性能基准，手动通过 qa\\test-perf.cmd 运行"]
    fn decode_and_compress_24mp_photo_within_budget() {
        const TOTAL_BUDGET: Duration = Duration::from_millis(900);
        const DECODE_BUDGET: Duration = Duration::from_millis(600);

        let dir = std::env::temp_dir().join(format!("pix-perf-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("6000x4000.jpg");
        // 纯灰图会把 JPEG 编解码压到失真低位，无法代表真实照片。
        // 用确定性纹理 + 平滑渐变，既不依赖外部 fixture，也保留足够高频细节。
        let width = 6000u32;
        let height = 4000u32;
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        let mut state = 0x9e37_79b9u32;
        for y in 0..height {
            for x in 0..width {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                let noise = ((state >> 24) as i32 & 0x1f) - 16;
                let texture = ((x / 23 + y / 19) % 37) as i32 - 18;
                let base_r = (x * 180 / width + y * 40 / height) as i32;
                let base_g = (y * 170 / height) as i32;
                let base_b = ((x + y) * 150 / (width + height)) as i32;
                let r = (base_r + noise + texture).clamp(0, 255);
                let g = (base_g + 55 + noise / 2 + texture / 2).clamp(0, 255);
                let b = (base_b + 35 + noise / 3).clamp(0, 255);
                pixels.extend_from_slice(&[r as u8, g as u8, b as u8, 255]);
            }
        }
        let source = RgbaImage::from_raw(width, height, pixels).unwrap();
        let encoded =
            encode_image(&source, OutFormat::Jpeg, 95, OutputMetadata::default()).unwrap();
        fs::write(&path, encoded).unwrap();
        drop(source);

        let settings = Settings::default();
        let fmt = resolve_out_fmt("jpg", &settings.fmt);
        let mut samples: Vec<(Duration, Duration, usize)> = Vec::new();
        for round in 0..4 {
            clear_cache();
            let t_decode = Instant::now();
            let (image, exif) = load_image(&path.to_string_lossy()).unwrap();
            let decode = t_decode.elapsed();
            let t_work = Instant::now();
            let result = compress_pixel(
                &image,
                fmt,
                50,
                82,
                true,
                OutputMetadata {
                    exif: if settings.keep_exif {
                        exif.as_deref()
                    } else {
                        None
                    },
                    icc: None,
                },
            )
            .unwrap();
            let work = t_work.elapsed();
            drop(image);
            assert_eq!(result.meta.out_w, 3000);
            assert_eq!(result.meta.out_h, 2000);
            if round > 0 {
                samples.push((decode, work, result.data.len()));
            }
        }
        clear_cache();
        let _ = fs::remove_dir_all(&dir);

        let median = |mut v: Vec<Duration>| {
            v.sort();
            v[v.len() / 2]
        };
        let decode_med = median(samples.iter().map(|s| s.0).collect());
        let work_med = median(samples.iter().map(|s| s.1).collect());
        let total_med = median(samples.iter().map(|s| s.0 + s.1).collect());
        println!(
            "PixSqueeze perf: 6000x4000 JPEG -> 50% q82 sharpened = {total_med:?} total (decode {decode_med:?} + compress {work_med:?}), output {} bytes, n=3",
            samples[1].2
        );
        assert!(
            total_med < TOTAL_BUDGET,
            "24MP 缩放压缩中位数 {total_med:?} 超出预算 {TOTAL_BUDGET:?}（decode {decode_med:?} + {work_med:?}）"
        );
        assert!(
            decode_med < DECODE_BUDGET,
            "24MP 解码中位数 {decode_med:?} 超出预算 {DECODE_BUDGET:?}"
        );
    }
}

#[cfg(test)]
mod path_tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn natural_order_sorts_numbers_by_value() {
        let mut v = vec![
            PathBuf::from("a10.jpg"),
            PathBuf::from("a2.jpg"),
            PathBuf::from("a1.jpg"),
        ];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            v,
            vec![
                PathBuf::from("a1.jpg"),
                PathBuf::from("a2.jpg"),
                PathBuf::from("a10.jpg")
            ]
        );
    }

    #[test]
    fn own_output_filter_is_narrow_and_has_override() {
        let dir = std::env::temp_dir().join(format!("pix-path-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.jpg"), b"x").unwrap();
        fs::write(dir.join("a_压缩.jpg"), b"x").unwrap();
        fs::write(dir.join("logo.jpg"), b"x").unwrap();
        fs::write(dir.join("logo_压缩(2).png"), b"x").unwrap();
        fs::write(dir.join("logo_压缩说明.png"), b"x").unwrap();
        fs::write(dir.join("cross.bmp"), b"x").unwrap();
        fs::write(dir.join("cross_压缩.png"), b"x").unwrap();
        fs::write(dir.join("folder.jpg"), b"x").unwrap();
        fs::write(dir.join("folder.png"), b"x").unwrap();
        fs::write(dir.join("folderbmp.bmp"), b"x").unwrap();
        fs::write(dir.join("folderbmp.png"), b"x").unwrap();
        fs::write(dir.join("orphan_压缩.png"), b"x").unwrap();

        assert!(own_output_reason(&dir.join("a_压缩.jpg")).is_some());
        assert!(own_output_reason(&dir.join("logo_压缩(2).png")).is_some());
        assert!(own_output_reason(&dir.join("cross_压缩.png")).is_some());
        assert!(own_output_reason(&dir.join("folder.png")).is_none());
        assert!(own_output_reason(&dir.join("folderbmp.png")).is_some());
        assert!(own_output_reason(&dir.join("logo_压缩说明.png")).is_none());
        assert!(own_output_reason(&dir.join("orphan_压缩.png")).is_none());
        assert!(own_output_reason(&dir.join("whatever.part")).is_some());

        let (collected, errors, skipped) =
            collect_input_paths(&[dir.to_string_lossy().to_string()], false);
        assert!(errors.is_empty(), "目录导入不应产生错误：{errors:?}");
        assert_eq!(skipped.len(), 4, "排除项：{skipped:?}");
        let names: Vec<String> = collected
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"a.jpg".to_string()));
        assert!(!names.contains(&"a_压缩.jpg".to_string()));
        assert!(names.contains(&"logo_压缩说明.png".to_string()));
        assert!(names.contains(&"orphan_压缩.png".to_string()));

        let explicit = dir.join("a_压缩.jpg").to_string_lossy().to_string();
        let (explicit_files, _, _) = collect_input_paths(&[explicit], false);
        assert_eq!(explicit_files.len(), 1, "显式选择必须允许再次处理");

        let (with_outputs, _, skipped_with_outputs) =
            collect_input_paths(&[dir.to_string_lossy().to_string()], true);
        assert!(skipped_with_outputs.is_empty());
        assert!(with_outputs.iter().any(|p| p.ends_with("a_压缩.jpg")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn worker_count_follows_process_budget() {
        assert_eq!(
            resources::workers_for_peak(300 * 1024 * 1024, 10, 4, PROCESS_WORKING_BUDGET_BYTES),
            4
        );
        assert_eq!(
            resources::workers_for_peak(700 * 1024 * 1024, 10, 4, PROCESS_WORKING_BUDGET_BYTES),
            2
        );
        assert_eq!(
            resources::workers_for_peak(2_000 * 1024 * 1024, 10, 4, PROCESS_WORKING_BUDGET_BYTES),
            1
        );
        assert_eq!(
            resources::workers_for_peak(1, 0, 4, PROCESS_WORKING_BUDGET_BYTES),
            0
        );
    }

    #[test]
    fn invalid_import_paths_are_reported() {
        let missing = std::env::temp_dir().join("pix-squeeze-definitely-missing-1.8");
        let (paths, errors, skipped) =
            collect_input_paths(&[missing.to_string_lossy().to_string()], false);
        assert!(paths.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(skipped.is_empty());
        assert!(errors[0].contains("路径不存在"));
    }

    #[test]
    fn webp_long_edge_failure_is_per_file_and_mixed_batch_continues() {
        let dir = std::env::temp_dir().join(format!("pix-webp-edge-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let out_dir = dir.join("out");
        let long_path = dir.join("long.png");
        let normal_path = dir.join("normal.png");
        let long = image::RgbaImage::from_raw(
            WEBP_MAX_DIMENSION + 1,
            1,
            vec![255; (WEBP_MAX_DIMENSION as usize + 1) * 4],
        )
        .unwrap();
        fs::write(
            &long_path,
            encode_image(&long, OutFormat::Png, 82, OutputMetadata::default()).unwrap(),
        )
        .unwrap();
        let normal = image::RgbaImage::from_raw(8, 8, vec![80; 8 * 8 * 4]).unwrap();
        fs::write(
            &normal_path,
            encode_image(&normal, OutFormat::Png, 82, OutputMetadata::default()).unwrap(),
        )
        .unwrap();
        let settings = Settings {
            fmt: "webp".into(),
            out_mode: "folder".into(),
            last_out_dir: out_dir.to_string_lossy().to_string(),
            ..Settings::default()
        };
        compress::clear_reserved_outputs();
        let results = [
            process_one(&long_path.to_string_lossy(), &settings, "edge-job", &|| {
                false
            }),
            process_one(
                &normal_path.to_string_lossy(),
                &settings,
                "edge-job",
                &|| false,
            ),
        ];
        assert!(!results[0].ok);
        assert!(results[0].error.contains(&WEBP_MAX_DIMENSION.to_string()));
        assert!(results[1].ok, "正常文件必须继续：{}", results[1].error);
        assert_eq!(results[1].format, "webp");
        assert!(fs::metadata(&results[1].out).is_ok());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn folder_mode_empty_directory_is_an_error_not_source_fallback() {
        let dir = std::env::temp_dir().join(format!("pix-empty-folder-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let src = dir.join("photo.png");
        let image = image::RgbaImage::from_raw(8, 8, vec![80; 8 * 8 * 4]).unwrap();
        fs::write(
            &src,
            encode_image(&image, OutFormat::Png, 82, OutputMetadata::default()).unwrap(),
        )
        .unwrap();
        let settings = Settings {
            out_mode: "folder".into(),
            last_out_dir: String::new(),
            ..Settings::default()
        };
        let result = process_one(&src.to_string_lossy(), &settings, "empty-folder", &|| false);
        assert!(!result.ok);
        assert!(result.error.contains("输出目录为空"), "{}", result.error);
        assert!(!dir.join("photo_压缩.png").exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
