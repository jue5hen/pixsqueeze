//! Single-image and process-wide resource accounting.
//!
//! Pixel-count limits alone are not enough: rotation, resizing, sharpening,
//! quantization and encoding can keep several full-image buffers alive at the
//! same time. The estimators below are intentionally conservative and are
//! checked before the expensive allocation/encoding starts.

pub const MAX_PIXELS: u64 = 200_000_000;
pub const SINGLE_IMAGE_BUDGET_BYTES: u64 = 1_500_000_000;
pub const PROCESS_WORKING_BUDGET_BYTES: u64 = 1_500_000_000;

const MIB: u64 = 1024 * 1024;
const FIXED_OVERHEAD: u64 = 32 * MIB;

#[derive(Clone, Copy, Debug)]
pub struct ResourcePolicy {
    pub max_pixels: u64,
    pub single_image_bytes: u64,
}

impl Default for ResourcePolicy {
    fn default() -> Self {
        Self {
            max_pixels: MAX_PIXELS,
            single_image_bytes: SINGLE_IMAGE_BUDGET_BYTES,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct WorkShape {
    pub scaled: bool,
    pub sharpen: bool,
    pub quantize: bool,
    pub flatten_rgb: bool,
}

fn checked_pixels(w: u32, h: u32) -> Result<u64, String> {
    (w as u64)
        .checked_mul(h as u64)
        .filter(|p| *p > 0)
        .ok_or_else(|| "图片尺寸无效或像素数溢出".to_string())
}

fn checked_bytes(pixels: u64, bytes_per_pixel: u64) -> Result<u64, String> {
    pixels
        .checked_mul(bytes_per_pixel)
        .ok_or_else(|| "图片内存估算溢出".to_string())
}

fn checked_add(total: &mut u64, value: u64) -> Result<(), String> {
    *total = total
        .checked_add(value)
        .ok_or_else(|| "图片内存估算溢出".to_string())?;
    Ok(())
}

/// Decode may keep the decoder output and the oriented RGBA image at once.
/// Import also creates a thumbnail, but that is small compared with the image.
pub fn estimate_import_peak_bytes(w: u32, h: u32) -> Result<u64, String> {
    let pixels = checked_pixels(w, h)?;
    let mut total = FIXED_OVERHEAD;
    // Decoder output + RGBA conversion + orientation/copy headroom.
    checked_add(&mut total, checked_bytes(pixels, 12)?)?;
    Ok(total)
}

/// Conservative estimate for one compression attempt. It covers the source
/// RGBA buffer, resized intermediate/final buffers, blur + sharpened copy,
/// imagequant working memory, RGB flattening and a few encoded candidates kept
/// by the target-size search.
pub fn estimate_work_peak_bytes(
    src_w: u32,
    src_h: u32,
    out_w: u32,
    out_h: u32,
    shape: WorkShape,
) -> Result<u64, String> {
    let src_pixels = checked_pixels(src_w, src_h)?;
    let out_pixels = checked_pixels(out_w, out_h)?;
    let src_rgba = checked_bytes(src_pixels, 4)?;
    let out_rgba = checked_bytes(out_pixels, 4)?;
    let mut total = FIXED_OVERHEAD;
    checked_add(&mut total, src_rgba)?;
    checked_add(&mut total, out_rgba)?;

    if shape.scaled {
        // smart_resize may hold the previous and next stage simultaneously.
        checked_add(&mut total, out_rgba)?;
    }
    if shape.sharpen {
        // image::blur buffer + sharpened RGBA clone.
        checked_add(&mut total, out_rgba)?;
        checked_add(&mut total, out_rgba)?;
    }
    if shape.quantize {
        // imagequant RGBA input, remapped indices and palette/index output.
        checked_add(&mut total, out_rgba)?;
        checked_add(&mut total, out_pixels)?;
        checked_add(&mut total, out_pixels)?;
    }
    if shape.flatten_rgb {
        checked_add(&mut total, checked_bytes(out_pixels, 3)?)?;
    }
    // Encoded bytes and the best/smallest candidates retained by the search.
    checked_add(&mut total, out_rgba)?;
    Ok(total)
}

fn enforce(policy: ResourcePolicy, w: u32, h: u32, peak: u64) -> Result<u64, String> {
    let pixels = checked_pixels(w, h)?;
    if pixels > policy.max_pixels {
        return Err(format!(
            "图片像素数 {pixels} 超过上限 {}，已拒绝处理",
            policy.max_pixels
        ));
    }
    if peak > policy.single_image_bytes {
        return Err(format!(
            "单张图片预计峰值内存约 {:.1} MB，超过单图预算 {:.1} MB；请先缩小图片或降低输出复杂度",
            peak as f64 / MIB as f64,
            policy.single_image_bytes as f64 / MIB as f64
        ));
    }
    Ok(peak)
}

pub fn check_import(policy: ResourcePolicy, w: u32, h: u32) -> Result<u64, String> {
    let peak = estimate_import_peak_bytes(w, h)?;
    enforce(policy, w, h, peak)
}

pub fn check_work(
    policy: ResourcePolicy,
    src_w: u32,
    src_h: u32,
    out_w: u32,
    out_h: u32,
    shape: WorkShape,
) -> Result<u64, String> {
    let peak = estimate_work_peak_bytes(src_w, src_h, out_w, out_h, shape)?;
    enforce(policy, src_w, src_h, peak)
}

pub fn workers_for_peak(peak_bytes: u64, total: usize, cap: usize, process_budget: u64) -> usize {
    if total == 0 || cap == 0 {
        return 0;
    }
    let by_memory = process_budget
        .checked_div(peak_bytes)
        .unwrap_or(cap as u64)
        .max(1) as usize;
    total.clamp(1, cap.min(by_memory).max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_budget_accepts_24mp_and_rejects_200mp_import() {
        let policy = ResourcePolicy::default();
        assert!(check_import(policy, 6000, 4000).is_ok());
        let err = check_import(policy, 20000, 10000).unwrap_err();
        assert!(err.contains("单图预算"), "{err}");
    }

    #[test]
    fn small_injected_budget_rejects_before_work_starts() {
        let policy = ResourcePolicy {
            max_pixels: MAX_PIXELS,
            single_image_bytes: 8 * 1024 * 1024,
        };
        let err = check_import(policy, 4000, 3000).unwrap_err();
        assert!(err.contains("单图预算"), "{err}");
    }

    #[test]
    fn quantized_png_estimates_more_than_plain_jpeg() {
        let jpeg = estimate_work_peak_bytes(
            4000,
            3000,
            4000,
            3000,
            WorkShape {
                flatten_rgb: true,
                ..WorkShape::default()
            },
        )
        .unwrap();
        let png = estimate_work_peak_bytes(
            4000,
            3000,
            4000,
            3000,
            WorkShape {
                quantize: true,
                ..WorkShape::default()
            },
        )
        .unwrap();
        assert!(png > jpeg);
    }

    #[test]
    fn worker_count_uses_process_budget() {
        assert_eq!(workers_for_peak(300 * MIB, 10, 4, 1500 * MIB), 4);
        assert_eq!(workers_for_peak(700 * MIB, 10, 4, 1500 * MIB), 2);
        assert_eq!(workers_for_peak(2_000 * MIB, 10, 4, 1500 * MIB), 1);
        assert_eq!(workers_for_peak(1, 0, 4, 1500 * MIB), 0);
    }
}
