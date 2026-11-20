use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    pub mode: String,
    pub percent: i64,
    pub quality: i64,
    pub target_kb: i64,
    pub fmt: String,
    pub keep_exif: bool,
    pub sharpen: bool,
    pub out_mode: String,
    pub last_open_dir: String,
    pub last_folder_dir: String,
    pub last_out_dir: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: "pixel".into(),
            percent: 100,
            quality: 82,
            target_kb: 500,
            fmt: "auto".into(),
            keep_exif: false,
            sharpen: true,
            out_mode: "suffix".into(),
            last_open_dir: String::new(),
            last_folder_dir: String::new(),
            last_out_dir: String::new(),
        }
    }
}

impl Settings {
    pub fn cleaned(v: Value) -> Self {
        let mut out = Self::default();
        let Some(obj) = v.as_object() else {
            return out;
        };
        if let Some(s) = obj.get("mode").and_then(Value::as_str) {
            if s == "pixel" || s == "size" {
                out.mode = s.to_string();
            }
        }
        if let Some(s) = obj.get("fmt").and_then(Value::as_str) {
            if ["auto", "jpeg", "png", "webp"].contains(&s) {
                out.fmt = s.to_string();
            }
        }
        if let Some(s) = obj.get("out_mode").and_then(Value::as_str) {
            if s == "suffix" || s == "folder" {
                out.out_mode = s.to_string();
            }
        }
        for key in ["keep_exif", "sharpen"] {
            if let Some(b) = obj.get(key).and_then(Value::as_bool) {
                if key == "keep_exif" {
                    out.keep_exif = b;
                } else {
                    out.sharpen = b;
                }
            }
        }
        for (key, lo, hi) in [
            ("percent", 10i64, 100i64),
            ("quality", 1, 100),
            ("target_kb", 10, 1_000_000),
        ] {
            if let Some(n) = obj.get(key) {
                if let Some(i) = n
                    .as_i64()
                    .or_else(|| n.as_str().and_then(|s| s.parse::<i64>().ok()))
                {
                    let clamped = i.clamp(lo, hi);
                    match key {
                        "percent" => out.percent = clamped,
                        "quality" => out.quality = clamped,
                        _ => out.target_kb = clamped,
                    }
                }
            }
        }
        for key in ["last_open_dir", "last_folder_dir", "last_out_dir"] {
            if let Some(s) = obj.get(key).and_then(Value::as_str) {
                let v = s.chars().take(500).collect::<String>();
                match key {
                    "last_open_dir" => out.last_open_dir = v,
                    "last_folder_dir" => out.last_folder_dir = v,
                    _ => out.last_out_dir = v,
                }
            }
        }
        out
    }
}

pub fn base_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn settings_path() -> PathBuf {
    base_dir().join("settings.json")
}

static SETTINGS_WRITE_LOCK: Mutex<()> = Mutex::new(());
static SETTINGS_REVISION: AtomicU64 = AtomicU64::new(0);
static SETTINGS_TEMP_SEQ: AtomicU64 = AtomicU64::new(1);
static ERROR_LOG_LOCK: Mutex<()> = Mutex::new(());

pub fn load_settings() -> (Settings, Option<String>) {
    let (settings, warning) = load_settings_from(&settings_path());
    if let Some(warning) = &warning {
        log_error("读取设置", warning);
    }
    (settings, warning)
}

fn load_settings_from(path: &Path) -> (Settings, Option<String>) {
    match fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(v) => (Settings::cleaned(v), None),
            Err(e) => (
                Settings::default(),
                Some(format!("settings.json 损坏，已使用默认值：{e}")),
            ),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Settings::default(), None),
        Err(e) => (
            Settings::default(),
            Some(format!("无法读取 settings.json，已使用默认值：{e}")),
        ),
    }
}

/// `revision` 必须由前端单调递增。前后端都可能因防抖/关窗触发并发保存，
/// 这里用 revision 丢弃迟到写入，避免较旧设置覆盖较新设置。
pub fn save_settings(s: &Settings, revision: u64) -> Result<(), String> {
    let _guard = SETTINGS_WRITE_LOCK
        .lock()
        .map_err(|_| "设置写锁已损坏".to_string())?;
    if revision < SETTINGS_REVISION.load(Ordering::Acquire) {
        return Ok(());
    }
    let path = settings_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建设置目录失败：{e}"))?;
    }
    let text = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    // 先写唯一临时文件、落盘、再原子替换：并发调用不会争抢同一个 .tmp。
    let seq = SETTINGS_TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("json.tmp.{}.{}", std::process::id(), seq));
    fs::write(&tmp, &text).map_err(|e| format!("写入设置失败：{e}"))?;
    if let Ok(file) = File::open(&tmp) {
        let _ = file.sync_all();
    }
    if let Err(e) = fs::rename(&tmp, &path) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("保存设置失败：{e}"));
    }
    SETTINGS_REVISION.store(revision, Ordering::Release);
    Ok(())
}

pub fn log_error(kind: &str, msg: &str) {
    log_error_at(&base_dir().join("错误日志.txt"), kind, msg);
}

fn log_error_at(path: &Path, kind: &str, msg: &str) {
    let Ok(_guard) = ERROR_LOG_LOCK.lock() else {
        return;
    };
    // 日志上限 256 KB，超限后只保留尾部 128 KB，并在第一条换行处截断：
    // 否则按字节切会把一行（或一个 UTF-8 汉字）拦腰切断，留下乱码（PS-A-024 清理项）。
    const LIMIT: u64 = 256 * 1024;
    const KEEP: u64 = 128 * 1024;
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    let line = format!("[{now}] {kind}: {msg}\n");
    let _ = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| f.write_all(line.as_bytes()));
    let Ok(meta) = fs::metadata(path) else {
        return;
    };
    if meta.len() <= LIMIT {
        return;
    }
    let Ok(mut f) = File::open(path) else { return };
    if f.seek(SeekFrom::Start(meta.len() - KEEP)).is_err() {
        return;
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return;
    }
    let start = buf.iter().position(|b| *b == b'\n').map_or(0, |i| i + 1);
    let body = String::from_utf8_lossy(&buf[start..]);
    let text = format!("…（更早的日志已截断，仅保留最近 128 KB）\n{body}");
    let tmp = path.with_extension(format!("txt.tmp.{}", std::process::id()));
    if fs::write(&tmp, text).is_ok() {
        let _ = fs::rename(tmp, path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "pix-settings-{}-{}-{}.json",
            std::process::id(),
            tag,
            chrono::Local::now()
                .timestamp_nanos_opt()
                .unwrap_or_default()
        ))
    }

    #[test]
    fn missing_settings_is_not_an_error_but_corruption_is_reported() {
        let missing = temp_path("missing");
        let _ = fs::remove_file(&missing);
        let (_, warning) = load_settings_from(&missing);
        assert!(warning.is_none());

        let corrupt = temp_path("corrupt");
        fs::write(&corrupt, b"{not json").unwrap();
        let (settings, warning) = load_settings_from(&corrupt);
        assert_eq!(settings.mode, Settings::default().mode);
        assert!(warning.unwrap().contains("损坏"));
        let _ = fs::remove_file(corrupt);
    }

    #[test]
    fn log_rotation_keeps_valid_utf8_tail() {
        let path = temp_path("log").with_extension("txt");
        let _ = fs::remove_file(&path);
        for _ in 0..6000 {
            log_error_at(&path, "测试", "图片导入/预览/估算错误日志轮转验证");
        }
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("更早的日志已截断"));
        assert!(text.contains("图片导入/预览/估算错误日志轮转验证"));
        assert!(fs::metadata(&path).unwrap().len() < 256 * 1024);
        let _ = fs::remove_file(path);
    }
}
