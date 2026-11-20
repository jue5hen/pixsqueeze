import { useCallback, useEffect, useRef, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { api } from "./api";
import PreviewCanvas, { type PreviewCanvasHandle } from "./PreviewCanvas";
import {
  batchItemState,
  batchReportText,
  clampInt,
  dirName,
  pathKey,
  requestIsCurrent,
  savingsPercent,
} from "./logic";
import {
  DEFAULT_SETTINGS,
  humanSize,
  type BatchResult,
  type EstimateResult,
  type FullPreview,
  type ImageItem,
  type PreviewResult,
  type ProgressPayload,
  type Settings,
} from "./types";

const IMAGE_FILTER = {
  name: "图片",
  extensions: ["jpg", "jpeg", "png", "webp", "bmp", "gif", "tif", "tiff"],
};

interface ToastState {
  id: number;
  kind: "success" | "warn" | "error";
  title: string;
  detail?: string;
  folder?: string;
  actionLabel?: string;
  onAction?: () => void;
}

type StatusSource = "general" | "import" | "batch" | "settings" | "preview";

export default function App() {
  const [settings, setSettings] = useState<Settings>(DEFAULT_SETTINGS);
  const [ready, setReady] = useState(false);
  const [images, setImages] = useState<ImageItem[]>([]);
  const [selectedPath, setSelectedPath] = useState<string | null>(null);
  const [preview, setPreview] = useState<PreviewResult | null>(null);
  const [estimate, setEstimate] = useState<EstimateResult | null>(null);
  const [previewBusy, setPreviewBusy] = useState(false);
  const [previewError, setPreviewError] = useState("");
  const [fullPreview, setFullPreview] = useState<FullPreview | null>(null);
  const [fullLoading, setFullLoading] = useState(false);
  const [fullPreviewError, setFullPreviewError] = useState("");
  const [importing, setImporting] = useState(false);
  const [compressing, setCompressing] = useState(false);
  const [stopping, setStopping] = useState(false);
  const [progress, setProgress] = useState({ done: 0, total: 0 });
  const [status, setStatus] = useState("就绪");
  const [statusError, setStatusError] = useState(false);
  const [toast, setToast] = useState<ToastState | null>(null);
  const [kbText, setKbText] = useState("500");
  const [dragging, setDragging] = useState(false);
  const [batchResult, setBatchResult] = useState<BatchResult | null>(null);

  const settingsRef = useRef(settings);
  const imagesRef = useRef(images);
  const selectedRef = useRef(selectedPath);
  const compressingRef = useRef(false);
  const importingRef = useRef(false);
  const pendingCloseRef = useRef(false);
  const saveTimerRef = useRef<number | null>(null);
  const previewRef = useRef<PreviewCanvasHandle>(null);
  const addPathsRef = useRef<(paths: string[], includeOwnOutputs?: boolean) => Promise<void>>(
    async () => {},
  );
  const initRef = useRef(false);
  const toastTimerRef = useRef<number | null>(null);
  const fullKeyRef = useRef("");
  const settingsRevisionRef = useRef(0);
  const currentJobIdRef = useRef<string | null>(null);
  const estimateRequestRef = useRef(0);
  const fullPreviewRequestRef = useRef(0);
  const statusSourceRef = useRef<StatusSource>("general");

  settingsRef.current = settings;
  imagesRef.current = images;
  selectedRef.current = selectedPath;

  const selectedImage = images.find((i) => i.path === selectedPath) ?? null;

  const showToast = useCallback((t: Omit<ToastState, "id">) => {
    if (toastTimerRef.current) window.clearTimeout(toastTimerRef.current);
    const item = { ...t, id: Date.now() };
    setToast(item);
    toastTimerRef.current = window.setTimeout(() => setToast(null), 3600);
  }, []);

  const setStatusNotice = useCallback(
    (text: string, error: boolean, source: StatusSource) => {
      statusSourceRef.current = source;
      setStatus(text);
      setStatusError(error);
    },
    [],
  );

  const clearImportStatus = useCallback((text = "就绪") => {
    if (statusSourceRef.current !== "import") return;
    statusSourceRef.current = "general";
    setStatus(text);
    setStatusError(false);
  }, []);

  const update = useCallback((patch: Partial<Settings>) => {
    if (compressingRef.current) return;
    setSettings((prev) => ({ ...prev, ...patch }));
  }, []);

  // 设置落盘：副作用放在 setState updater 之外（StrictMode 下 updater 可能执行两次，
  // 会造成双份防抖任务），且失败必须显性化——静默吞掉会让用户以为设置已经保住
  // （PS-A-025）。
  useEffect(() => {
    if (!ready) return;
    if (saveTimerRef.current) window.clearTimeout(saveTimerRef.current);
    saveTimerRef.current = window.setTimeout(() => {
      saveTimerRef.current = null;
      const revision = ++settingsRevisionRef.current;
      api.saveSettings(settings, revision).catch((e) => {
        if (statusSourceRef.current !== "batch" && statusSourceRef.current !== "import") {
          setStatusNotice(`保存设置失败：${e}`, true, "settings");
        }
      });
    }, 300);
    return () => {
      if (saveTimerRef.current) {
        window.clearTimeout(saveTimerRef.current);
        saveTimerRef.current = null;
      }
    };
  }, [setStatusNotice, settings, ready]);

  const addPaths = useCallback(
    async (paths: string[], includeOwnOutputs = false) => {
      if (!paths.length || importingRef.current || compressingRef.current) return;
      importingRef.current = true;
      setImporting(true);
      setStatusNotice("正在导入图片…", false, "import");
      try {
        const res = await api.importPaths(paths, includeOwnOutputs);
        const skipped = res.skipped_outputs ?? [];
        // 路径归一化后再去重：Windows 下同一文件可能以不同大小写/斜杠写法重复进入，
        // 连续两次导入时 imagesRef 也可能还没刷新（PS-A-027）。
        const known = new Set(imagesRef.current.map((img) => pathKey(img.path)));
        const fresh = res.images.filter((img) => {
          const key = pathKey(img.path);
          if (known.has(key)) return false;
          known.add(key);
          return true;
        });
        if (fresh.length) {
          setImages((prev) => [...prev, ...fresh]);
          if (!selectedRef.current) setSelectedPath(fresh[0].path);
          const firstDir = dirName(fresh[0].path);
          if (firstDir) update({ last_open_dir: firstDir });
          setStatusNotice(
            skipped.length
              ? `已导入 ${fresh.length} 张，排除 ${skipped.length} 个疑似自身输出`
              : `已导入 ${fresh.length} 张图片`,
            false,
            "import",
          );
        } else {
          setStatusNotice(
            skipped.length
              ? `未导入：排除 ${skipped.length} 个疑似自身输出`
              : res.images.length
                ? "这些图片已在列表中"
                : "没有可导入的图片",
            false,
            "import",
          );
        }
        if (res.errors.length) {
          setStatusNotice(
            `导入完成：成功 ${fresh.length} 张，跳过 ${res.errors.length} 张`,
            true,
            "import",
          );
          showToast({
            kind: "warn",
            title: `已导入 ${fresh.length} 张，${res.errors.length} 张跳过`,
            detail: res.errors.slice(0, 3).join("\n"),
          });
        } else if (skipped.length) {
          showToast({
            kind: "warn",
            title: `已排除 ${skipped.length} 个疑似 PixSqueeze 输出`,
            detail: skipped
              .slice(0, 3)
              .map((s) => `${s.path.split(/[\\/]/).pop() || s.path}：${s.reason}`)
              .join("\n"),
            actionLabel: "仍然导入",
            onAction: () => {
              setToast(null);
              void addPathsRef.current(paths, true);
            },
          });
        }
      } catch (e) {
        setStatusNotice(`导入失败：${e}`, true, "import");
        showToast({ kind: "error", title: "导入失败", detail: String(e) });
      } finally {
        importingRef.current = false;
        setImporting(false);
      }
    },
    [setStatusNotice, showToast, update],
  );
  addPathsRef.current = addPaths;

  const pickFiles = useCallback(async () => {
    if (compressingRef.current || importingRef.current) return;
    try {
      const picked = await open({
        multiple: true,
        directory: false,
        title: "选择图片",
        defaultPath: settingsRef.current.last_open_dir || undefined,
        filters: [IMAGE_FILTER],
      });
      if (!picked) return;
      const paths = Array.isArray(picked) ? picked : [picked];
      const dir = paths[0] ? dirName(paths[0]) : "";
      if (dir) update({ last_open_dir: dir });
      await addPathsRef.current(paths);
    } catch (e) {
      showToast({ kind: "error", title: "打开图片失败", detail: String(e) });
    }
  }, [showToast, update]);

  const pickFolder = useCallback(async () => {
    if (compressingRef.current || importingRef.current) return;
    try {
      const picked = await open({
        multiple: false,
        directory: true,
        title: "导入当前文件夹（不含子文件夹）",
        defaultPath: settingsRef.current.last_folder_dir || undefined,
      });
      if (!picked) return;
      const folder = Array.isArray(picked) ? picked[0] : picked;
      update({ last_folder_dir: folder });
      await addPathsRef.current([folder]);
    } catch (e) {
      showToast({ kind: "error", title: "打开文件夹失败", detail: String(e) });
    }
  }, [showToast, update]);

  const pickOutputFolder = useCallback(async () => {
    if (compressingRef.current) return;
    try {
      const picked = await open({
        multiple: false,
        directory: true,
        title: "选择保存文件夹",
        defaultPath: settingsRef.current.last_out_dir || undefined,
      });
      if (!picked) return;
      const folder = Array.isArray(picked) ? picked[0] : picked;
      update({ last_out_dir: folder, out_mode: "folder" });
    } catch (e) {
      showToast({ kind: "error", title: "选择文件夹失败", detail: String(e) });
    }
  }, [showToast, update]);

  const init = useCallback(async () => {
    try {
      const loaded = await api.loadSettings();
      setSettings(loaded.settings);
      setKbText(String(loaded.settings.target_kb));
      setReady(true);
      if (loaded.warning) {
        setStatusNotice(`设置已降级：${loaded.warning}`, true, "settings");
      }
      const cli = await api.cliPaths();
      if (cli.length) await addPathsRef.current(cli);
    } catch (e) {
      setReady(true);
      setStatusNotice(`读取设置失败：${e}`, true, "settings");
    }
  }, [setStatusNotice]);

  useEffect(() => {
    if (initRef.current) return;
    initRef.current = true;
    void init();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    return () => {
      if (saveTimerRef.current) window.clearTimeout(saveTimerRef.current);
      if (toastTimerRef.current) window.clearTimeout(toastTimerRef.current);
    };
  }, []);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    void listen<ProgressPayload>("pix-progress", (event) => {
      if (event.payload.job_id !== currentJobIdRef.current) return;
      setProgress({ done: event.payload.done, total: event.payload.total });
    }).then((fn) => {
      unlisten = fn;
    });
    return () => unlisten?.();
  }, []);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    void getCurrentWebview()
      .onDragDropEvent(async (event) => {
        if (event.payload.type === "over") {
          setDragging(true);
        } else if (event.payload.type === "leave") {
          setDragging(false);
        } else if (event.payload.type === "drop") {
          setDragging(false);
          if (compressingRef.current || importingRef.current) return;
          const paths = event.payload.paths ?? [];
          if (paths.length) await addPathsRef.current(paths);
        }
      })
      .then((fn) => {
        unlisten = fn;
      });
    return () => unlisten?.();
    // 监听只注册一次：此前依赖 importing，每次导入开始/结束都会重注册，
      // 交接间隙的 drop 事件会丢（PS-A-026）。
  }, []);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    void getCurrentWindow()
      .onCloseRequested(async (event) => {
        event.preventDefault();
        try {
          if (saveTimerRef.current) window.clearTimeout(saveTimerRef.current);
          const revision = ++settingsRevisionRef.current;
          await api.saveSettings(settingsRef.current, revision);
        } catch {
          /* 关窗失败不应阻塞退出；迟到的保存由 revision 丢弃 */
        }
        if (compressingRef.current) {
          // 压缩中关窗：先请求停止，批量线程收尾后由 startCompress 负责销毁窗口
          // （PS-A-006：以前会一直等到全部处理完，用户没有任何反馈）。
          pendingCloseRef.current = true;
          const jobId = currentJobIdRef.current;
          if (jobId) void api.cancelBatch(jobId).catch(() => undefined);
          return;
        }
        await getCurrentWindow().destroy();
      })
      .then((fn) => {
        unlisten = fn;
      });
    return () => unlisten?.();
  }, []);

  const previewKey = JSON.stringify([
    settings.mode,
    settings.percent,
    settings.quality,
    settings.target_kb,
    settings.fmt,
    settings.keep_exif,
    settings.sharpen,
  ]);

  useEffect(() => {
    fullKeyRef.current = "";
    setFullPreview(null);
    setFullLoading(false);
    setFullPreviewError("");
  }, [selectedPath, previewKey]);

  const requestFullPreview = useCallback(() => {
    if (compressingRef.current) return;
    const path = selectedRef.current;
    if (!path) return;
    const key = `${path}|${previewKey}`;
    if (fullKeyRef.current === key) return;
    fullKeyRef.current = key;
    setFullLoading(true);
    setFullPreviewError("");
    const requestId = ++fullPreviewRequestRef.current;
    api
      .previewFull(path, settingsRef.current, requestId)
      .then((res) => {
        if (requestIsCurrent(requestId, fullPreviewRequestRef.current)) {
          setFullPreview(res);
          setFullPreviewError(res ? "" : "图片超过 3200 万像素，高清源已降级为预览源");
        }
      })
      .catch((e) => {
        if (requestIsCurrent(requestId, fullPreviewRequestRef.current)) {
          fullKeyRef.current = "";
          setFullPreviewError(String(e));
        }
      })
      .finally(() => {
        if (requestIsCurrent(requestId, fullPreviewRequestRef.current)) {
          setFullLoading(false);
        }
      });
  }, [previewKey]);

  useEffect(() => {
    if (!ready || !selectedPath || compressingRef.current) {
      setPreview(null);
      setEstimate(null);
      setPreviewBusy(false);
      return;
    }
    let cancelled = false;
    setPreview(null);
    setEstimate(null);
    setPreviewError("");
    setPreviewBusy(true);
    const timer = window.setTimeout(async () => {
      if (cancelled || compressingRef.current) return;
      try {
        const p = await api.makePreview(selectedPath, settingsRef.current);
        if (cancelled) return;
        setPreview(p);
        setPreviewBusy(false);
        // 批量压缩进行中不抢先跑全图估算：估算不是刚需，抢 CPU 只会拖慢真正的任务（PS-A-007）
        if (compressingRef.current) return;
        try {
          const requestId = ++estimateRequestRef.current;
          const est = await api.estimateFull(
            selectedPath,
            settingsRef.current,
            requestId,
          );
          if (cancelled) return;
          setEstimate(est);
        } catch (e) {
          if (!cancelled) setPreviewError(String(e));
        }
      } catch (e) {
        if (cancelled) return;
        setPreviewError(String(e));
        setPreviewBusy(false);
      }
    }, 400);
    return () => {
      cancelled = true;
      estimateRequestRef.current += 1;
      fullPreviewRequestRef.current += 1;
      window.clearTimeout(timer);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [ready, selectedPath, previewKey]);

  const removeImage = (path: string) => {
    if (compressingRef.current || importingRef.current) return;
    setImages((prev) => {
      const next = prev.filter((i) => i.path !== path);
      if (selectedRef.current === path) {
        setSelectedPath(next.length ? next[0].path : null);
      }
      return next;
    });
    clearImportStatus("已移除 1 张图片");
  };

  const clearWorkspace = () => {
    if (compressingRef.current || importingRef.current || !images.length) return;
    setImages([]);
    setSelectedPath(null);
    setPreview(null);
    setEstimate(null);
    statusSourceRef.current = "general";
    setStatus("就绪");
    setStatusError(false);
    setBatchResult(null);
  };

  const startCompress = async () => {
    if (!images.length || compressingRef.current || importingRef.current) return;
    if (settingsRef.current.out_mode === "folder" && !settingsRef.current.last_out_dir.trim()) {
      setStatusNotice("请先选择指定输出文件夹；不会静默写回源目录", true, "batch");
      showToast({ kind: "error", title: "输出目录为空", detail: "请先选择保存文件夹" });
      return;
    }
    const jobId = `${Date.now()}-${Math.random().toString(16).slice(2)}`;
    currentJobIdRef.current = jobId;
    compressingRef.current = true;
    setCompressing(true);
    setStopping(false);
    setPreviewBusy(false);
    setFullLoading(false);
    setBatchResult(null);
    setProgress({ done: 0, total: images.length });
    setStatusNotice("正在压缩…", false, "batch");
    try {
      const res = await api.compressBatch(
        images.map((i) => i.path),
        settingsRef.current,
        jobId,
      );
      setBatchResult(res);
      const summary = `已生成 ${res.success}/${res.total}，达标 ${res.reached_target}，未达标 ${res.not_reached}，失败 ${res.failed}，取消 ${res.cancelled_count}，警告 ${res.warning_count}`;
      setStatusNotice(
        res.cancelled ? `已停止：${summary}` : `完成：${summary}`,
        res.failed > 0,
        "batch",
      );
      if (res.cancelled) {
        showToast({
          kind: "warn",
          title: res.success > 0 ? `已停止：已生成 ${res.success} 张` : "已停止压缩",
          detail: summary,
          folder: res.success > 0 ? res.out_dir : undefined,
        });
      } else if (res.success > 0 || res.not_reached > 0 || res.warning_count > 0) {
        showToast({
          kind:
            res.failed > 0 || res.not_reached > 0 || res.warning_count > 0 ? "warn" : "success",
          title:
            res.not_reached > 0
              ? `已生成 ${res.success} 张，${res.not_reached} 张未达目标`
              : `已生成 ${res.success} 张，达标 ${res.reached_target} 张`,
          detail: res.first_error || summary,
          folder: res.out_dir,
        });
      } else {
        showToast({
          kind: "error",
          title: "压缩失败",
          detail: res.first_error || "未知错误",
        });
      }
    } catch (e) {
      setStatusNotice(`压缩失败：${e}`, true, "batch");
      showToast({ kind: "error", title: "压缩失败", detail: String(e) });
    } finally {
      if (currentJobIdRef.current === jobId) currentJobIdRef.current = null;
      compressingRef.current = false;
      setCompressing(false);
      setStopping(false);
      if (pendingCloseRef.current) {
        await getCurrentWindow().destroy();
      }
    }
  };

  const stopCompress = async () => {
    if (!compressingRef.current || stopping) return;
    const jobId = currentJobIdRef.current;
    if (!jobId) return;
    setStopping(true);
    setStatusNotice("正在停止…", false, "batch");
    try {
      await api.cancelBatch(jobId);
    } catch (e) {
      setStopping(false);
      setStatusNotice(`停止失败：${e}`, true, "batch");
    }
  };

  const commitKb = () => {
    const value = clampInt(kbText, settingsRef.current.target_kb, 10, 1_000_000);
    setKbText(String(value));
    update({ target_kb: value });
  };

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const tag = (document.activeElement as HTMLElement | null)?.tagName ?? "";
      const inInput = tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT";
      if (e.ctrlKey && e.key.toLowerCase() === "o") {
        e.preventDefault();
        void pickFiles();
        return;
      }
      if (e.ctrlKey && e.key === "0") {
        e.preventDefault();
        previewRef.current?.reset();
        return;
      }
      if (e.key === "Delete" && !inInput) {
        if (selectedRef.current) removeImage(selectedRef.current);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [pickFiles]);

  const totalBytes = images.reduce((s, i) => s + i.bytes, 0);
  const savings =
    estimate && selectedImage && selectedImage.bytes > 0
      ? savingsPercent(selectedImage.bytes, estimate.out_bytes)
      : null;
  const previewWarnings = Array.from(
    new Set([...(preview?.warnings ?? []), ...(estimate?.warnings ?? [])]),
  );
  if (
    selectedImage?.has_alpha &&
    settings.fmt === "jpeg" &&
    !previewWarnings.some((w) => w.includes("白底"))
  ) {
    previewWarnings.push("JPEG 不支持透明通道，透明/半透明区域将合成到白底");
  }

  return (
    <div className={"app" + (dragging ? " drag-over" : "")} aria-busy={importing || compressing}>
      <header className="topbar">
        <div className="brand">
          PixSqueeze
          <span className="brand-sub">图片批量压缩</span>
        </div>
        <button
          className="btn subtle clear-btn"
          disabled={!images.length || compressing || importing}
          onClick={clearWorkspace}
        >
          清空当前工具区
        </button>
      </header>

      <div className="workspace">
        <aside className="sidebar">
          <div className="sidebar-head">
            <span>图片列表</span>
            <span className="list-count">{images.length ? images.length : "（空）"}</span>
          </div>
          <div className="image-list">
            {images.map((img) => (
              <div
                key={img.path}
                className={
                  "image-row" +
                  (img.path === selectedPath ? " selected" : "") +
                  (compressing ? " locked" : "")
                }
                role="button"
                tabIndex={compressing ? -1 : 0}
                aria-pressed={img.path === selectedPath}
                aria-disabled={compressing}
                aria-label={`选择 ${img.name}`}
                onClick={() => {
                  if (!compressingRef.current) {
                    setSelectedPath(img.path);
                    clearImportStatus();
                  }
                }}
                onKeyDown={(e) => {
                  if (compressingRef.current) return;
                  if (e.key === "Enter" || e.key === " ") {
                    e.preventDefault();
                    setSelectedPath(img.path);
                    clearImportStatus();
                  }
                }}
                title={img.path}
              >
                <img className="thumb" src={img.thumb} alt="" draggable={false} />
                <div className="image-meta">
                  <div className="image-name">{img.name}</div>
                  <div className="image-sub">
                    {humanSize(img.bytes)} · {img.width}×{img.height}
                  </div>
                  {img.warnings[0] && (
                    <div className="image-warning" title={img.warnings.join("\n")}>
                      ⚠ {img.warnings[0]}
                    </div>
                  )}
                </div>
                <button
                  className="row-remove"
                  title="移除"
                  aria-label={`移除 ${img.name}`}
                  disabled={compressing || importing}
                  onClick={(e) => {
                    e.stopPropagation();
                    removeImage(img.path);
                  }}
                >
                  ×
                </button>
              </div>
            ))}
          </div>
          <div className="sidebar-foot">
            共 {images.length} 张 · {humanSize(totalBytes)}
          </div>
        </aside>

        <main className="center">
          <div
            className="preview-card"
            onClick={() => {
              if (!images.length && !importing) void pickFiles();
            }}
          >
            {images.length === 0 ? (
              <div className="empty-state">
                <div className="empty-title">开始压缩你的第一张图片</div>
                <button
                  className="empty-btn"
                  disabled={importing}
                  onClick={(e) => {
                    e.stopPropagation();
                    void pickFiles();
                  }}
                >
                  点击选择图片
                </button>
                <button
                  className="empty-btn"
                  disabled={importing}
                  onClick={(e) => {
                    e.stopPropagation();
                    void pickFolder();
                  }}
                >
                  导入当前文件夹
                </button>
                <div className="empty-hint">文件夹仅扫描当前层，不递归子文件夹</div>
                <div className="empty-hint">也可以直接把图片拖进来 · 支持批量</div>
                {importing && <div className="empty-hint">正在读取图片…</div>}
              </div>
            ) : preview ? (
              <PreviewCanvas
                ref={previewRef}
                origUrl={preview.orig_url}
                compUrl={preview.comp_url}
                baseW={preview.base_w}
                baseH={preview.base_h}
                fullOrigUrl={fullPreview?.orig_url}
                fullCompUrl={fullPreview?.comp_url}
                fullLoading={fullLoading}
                fullError={fullPreviewError}
                onNeedFull={requestFullPreview}
              />
            ) : (
              <div className="empty-state">
                <div className="empty-title">
                  {previewError
                    ? "预览生成失败"
                    : previewBusy
                      ? "正在生成预览…"
                      : "从左侧选择一张图片"}
                </div>
                {previewError && <div className="empty-hint error">{previewError}</div>}
              </div>
            )}
            {dragging && <div className="drop-hint">松开鼠标即可导入图片</div>}
          </div>
          <div className="preview-info">
            {selectedImage ? (
              <>
                <span>
                  {selectedImage.width}×{selectedImage.height} ·{" "}
                  {humanSize(selectedImage.bytes)}
                </span>
                <span className="arrow">→</span>
                {estimate ? (
                  <span>
                    {estimate.out_w}×{estimate.out_h} · {humanSize(estimate.out_bytes)}
                    {savings !== null && savings >= 0
                      ? `（节省 ${savings}%）`
                      : savings !== null
                        ? `（增大 ${Math.abs(savings)}%）`
                        : ""}
                  </span>
                ) : (
                  <span className="muted">
                    {previewError ? "无法估算" : "计算中…"}
                  </span>
                )}
                {estimate && settings.mode === "size" && !estimate.reached_target && (
                  <span className="warn-text">未达目标大小</span>
                )}
                {estimate && estimate.scaled && settings.mode === "size" && (
                  <span className="warn-text">输出将缩至上述尺寸</span>
                )}
                {(estimate?.format || preview?.format) && (
                  <span className="muted">实际格式 {estimate?.format ?? preview?.format}</span>
                )}
                {estimate?.budget_exhausted && (
                  <span className="warn-text">估算达到编码预算，结果可能为近似值</span>
                )}
                {previewWarnings.slice(0, 2).map((warning) => (
                  <span className="warn-text" key={warning}>
                    {warning}
                  </span>
                ))}
              </>
            ) : (
              <span className="muted">选择图片后在此显示原始与输出尺寸</span>
            )}
          </div>
        </main>

        <aside className="panel">
          <fieldset className="settings-fieldset" disabled={compressing} aria-label="压缩设置">
          <div className="section-label">压缩模式</div>
          <div className="chip-row">
            <button
              className={"chip mode-chip" + (settings.mode === "pixel" ? " active" : "")}
              aria-pressed={settings.mode === "pixel"}
              onClick={() => update({ mode: "pixel" })}
            >
              按像素缩放
            </button>
            <button
              className={"chip mode-chip" + (settings.mode === "size" ? " active" : "")}
              aria-pressed={settings.mode === "size"}
              onClick={() => update({ mode: "size" })}
            >
              按目标大小
            </button>
          </div>

          {settings.mode === "pixel" ? (
            <>
              <div className="slider-head">
                <span>缩放比例</span>
                <span className="value">{settings.percent}%</span>
              </div>
              <input
                className="slider"
                type="range"
                aria-label="缩放比例"
                min={10}
                max={100}
                value={settings.percent}
                onChange={(e) => update({ percent: Number(e.target.value) })}
              />
              <div className="chip-row">
                {[25, 50, 75, 100].map((v) => (
                  <button
                    key={v}
                    className={"chip small" + (settings.percent === v ? " active" : "")}
                    aria-pressed={settings.percent === v}
                    onClick={() => update({ percent: v })}
                  >
                    {v}
                  </button>
                ))}
              </div>
              <div className="slider-head quality-head">
                <span>图片质量</span>
                <span className="value">{settings.quality}</span>
              </div>
              <input
                className="slider"
                type="range"
                aria-label="图片质量"
                min={1}
                max={100}
                value={settings.quality}
                onChange={(e) => update({ quality: Number(e.target.value) })}
              />
              <div className="chip-row">
                {[25, 50, 75, 100].map((v) => (
                  <button
                    key={v}
                    className={"chip small" + (settings.quality === v ? " active" : "")}
                    aria-pressed={settings.quality === v}
                    onClick={() => update({ quality: v })}
                  >
                    {v}
                  </button>
                ))}
              </div>
              <div className="target-res">
                目标分辨率{" "}
                {selectedImage
                  ? `${Math.max(1, Math.round((selectedImage.width * settings.percent) / 100))} × ${Math.max(
                      1,
                      Math.round((selectedImage.height * settings.percent) / 100),
                    )}`
                  : "—"}
              </div>
            </>
          ) : (
            <div className="size-mode">
              <div className="section-label inline">目标大小（KB）</div>
              <input
                className="kb-input"
                aria-label="目标大小 KB"
                value={kbText}
                inputMode="numeric"
                onChange={(e) => setKbText(e.target.value.replace(/[^\d]/g, ""))}
                onBlur={commitKb}
                onKeyDown={(e) => {
                  if (e.key === "Enter") {
                    commitKb();
                    (e.target as HTMLInputElement).blur();
                  }
                }}
              />
              <div className="hint small">
                自动搜索「不超目标的最高质量」，
                <br />
                不够时再降分辨率，尽量贴近目标。
              </div>
              <div className="target-res">
                {selectedImage
                  ? `原图 ${selectedImage.width} × ${selectedImage.height}，自动搜索最优质量`
                  : "导入图片后自动搜索最优质量"}
              </div>
            </div>
          )}

          <div className="section-label spaced">输出格式</div>
          <div className="chip-row">
            {(
              [
                ["auto", "自动格式"],
                ["jpeg", "JPEG"],
                ["png", "PNG"],
                ["webp", "WebP"],
              ] as const
            ).map(([value, label]) => (
              <button
                key={value}
                className={"chip fmt" + (settings.fmt === value ? " active" : "")}
                aria-pressed={settings.fmt === value}
                onClick={() => update({ fmt: value })}
              >
                {label}
              </button>
            ))}
          </div>
          <div className="hint small">
            自动：JPEG/PNG/WebP 保持原格式族；BMP/GIF/TIFF 转为 PNG。
            <br />
            GIF/多页 TIFF 当前仅处理第 1 帧/页，导入时会提示。
          </div>

          <label className="switch-row">
            <span className={"switch" + (settings.sharpen ? " on" : "")}>
              <input
                type="checkbox"
                checked={settings.sharpen}
                onChange={(e) => update({ sharpen: e.target.checked })}
              />
              <i />
            </span>
            <span>缩放后轻度锐化（更清晰）</span>
          </label>
          <label className="switch-row">
            <span className={"switch" + (settings.keep_exif ? " on" : "")}>
              <input
                type="checkbox"
                checked={settings.keep_exif}
                onChange={(e) => update({ keep_exif: e.target.checked })}
              />
              <i />
            </span>
            <span>保留拍摄信息 EXIF</span>
          </label>
          <div className="hint small">ICC 色彩配置独立于 EXIF，会尽量随输出保留。</div>

          <div className="section-label spaced">保存位置</div>
          <div className="chip-row">
            <button
              className={"chip save-chip" + (settings.out_mode === "suffix" ? " active" : "")}
              aria-pressed={settings.out_mode === "suffix"}
              onClick={() => update({ out_mode: "suffix" })}
            >
              原目录 + 后缀
            </button>
            <button
              className={"chip save-chip" + (settings.out_mode === "folder" ? " active" : "")}
              aria-pressed={settings.out_mode === "folder"}
              onClick={() => update({ out_mode: "folder" })}
            >
              指定文件夹
            </button>
          </div>
          {settings.out_mode === "folder" && (
            <>
              <div className="folder-row">
                <div className="folder-path" title={settings.last_out_dir || "未选择"}>
                  {settings.last_out_dir || "未选择"}
                </div>
                <button className="btn tiny" onClick={pickOutputFolder}>
                  选择…
                </button>
              </div>
              {!settings.last_out_dir.trim() && (
                <div className="hint small error">必须选择输出目录；不会静默写回源目录。</div>
              )}
            </>
          )}
          </fieldset>

          {compressing ? (
            <button
              className="btn danger start-btn"
              disabled={stopping}
              onClick={stopCompress}
            >
              {stopping
                ? "正在停止…"
                : `停止压缩（已完成 ${progress.done}/${progress.total}）`}
            </button>
          ) : (
            <button
              className="btn primary start-btn"
              disabled={
                !images.length ||
                !selectedImage ||
                importing ||
                (settings.out_mode === "folder" && !settings.last_out_dir.trim())
              }
              onClick={startCompress}
            >
              {images.length ? `开始压缩（${images.length} 张）` : "开始压缩"}
            </button>
          )}

          {batchResult && (
            <details className="batch-report" open>
              <summary>
                本次结果：生成 {batchResult.success}/{batchResult.total} · 达标 {batchResult.reached_target} ·
                未达标 {batchResult.not_reached} · 失败 {batchResult.failed} · 取消{" "}
                {batchResult.cancelled_count} · 警告 {batchResult.warning_count}
              </summary>
              <div className="batch-report-actions">
                <button
                  className="btn tiny"
                  onClick={() => {
                    navigator.clipboard
                      .writeText(batchReportText(batchResult))
                      .then(() => showToast({ kind: "success", title: "已复制结果明细" }))
                      .catch((e) =>
                        showToast({ kind: "error", title: "复制失败", detail: String(e) }),
                      );
                  }}
                >
                  复制明细
                </button>
              </div>
              <div className="batch-list">
                {batchResult.results.map((item, index) => {
                  const state = batchItemState(item);
                  const name = item.src.split(/[\\/]/).pop() || item.src || `第 ${index + 1} 项`;
                  const label =
                    state === "success"
                      ? "达标"
                      : state === "below-target"
                        ? "未达标"
                        : state === "failed"
                          ? "失败"
                          : "取消";
                  return (
                    <div className={`batch-item ${state}`} key={`${item.src}-${index}`}>
                      <div className="batch-item-head">
                        <span className={`batch-badge ${state}`}>{label}</span>
                        <span className="batch-name" title={item.src}>
                          {name}
                        </span>
                        <button
                          className="btn tiny"
                          onClick={() => api.reveal(item.out || item.src).catch(() => undefined)}
                        >
                          定位
                        </button>
                      </div>
                      {item.ok ? (
                        <div className="batch-detail">
                          {item.out_w}×{item.out_h} · {humanSize(item.bytes)} · {item.format}
                          {item.budget_exhausted ? " · 编码预算耗尽" : ""}
                        </div>
                      ) : (
                        <div className="batch-detail error">{item.error || "未知错误"}</div>
                      )}
                      {item.warnings.map((warning) => (
                        <div className="batch-warning" key={warning}>
                          ⚠ {warning}
                        </div>
                      ))}
                    </div>
                  );
                })}
              </div>
            </details>
          )}
        </aside>
      </div>

      <footer className="statusbar">
        <span
          className={statusError ? "status-text error" : "status-text"}
          role="status"
          aria-live="polite"
        >
          {status}
        </span>
      </footer>

      {toast && (
        <div
          className={"toast " + toast.kind}
          role={toast.kind === "error" ? "alert" : "status"}
          aria-live={toast.kind === "error" ? "assertive" : "polite"}
        >
          <div className="toast-text">
            <div>{toast.title}</div>
            {toast.detail && <div className="toast-detail">{toast.detail}</div>}
          </div>
          {toast.folder && (
            <button
              className="btn tiny"
              onClick={() => {
                api.reveal(toast.folder!).catch(() => undefined);
              }}
            >
              打开文件夹
            </button>
          )}
          {toast.actionLabel && toast.onAction && (
            <button
              className="btn tiny"
              onClick={() => {
                toast.onAction?.();
              }}
            >
              {toast.actionLabel}
            </button>
          )}
        </div>
      )}
    </div>
  );
}
