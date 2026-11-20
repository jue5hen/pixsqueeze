import {
  forwardRef,
  useEffect,
  useImperativeHandle,
  useRef,
  useState,
} from "react";

/** 读取统一设计 token 的运行时值；取不到时回退到内置默认色。 */
const cssVar = (name: string, fallback: string): string =>
  getComputedStyle(document.documentElement).getPropertyValue(name).trim() || fallback;

export interface PreviewCanvasHandle {
  reset: () => void;
}

interface Props {
  origUrl: string;
  compUrl: string;
  baseW: number;
  baseH: number;
  fullOrigUrl?: string;
  fullCompUrl?: string;
  fullLoading?: boolean;
  fullError?: string;
  onNeedFull?: () => void;
  busy?: boolean;
}

interface ViewState {
  zoom: number;
  panX: number;
  panY: number;
  split: number;
}

const MIN_ZOOM = 1;
const MAX_ZOOM = 8;

const PreviewCanvas = forwardRef<PreviewCanvasHandle, Props>(
  (props, ref) => {
    const { origUrl, compUrl, baseW, baseH } = props;
    const canvasRef = useRef<HTMLCanvasElement>(null);
    const wrapRef = useRef<HTMLDivElement>(null);
    const originalRef = useRef<HTMLImageElement | null>(null);
    const compressedRef = useRef<HTMLImageElement | null>(null);
    const fullOrigRef = useRef<HTMLImageElement | null>(null);
    const fullCompRef = useRef<HTMLImageElement | null>(null);
    const propsRef = useRef(props);
    propsRef.current = props;
    const needFullRef = useRef(false);
    const view = useRef<ViewState>({ zoom: 1, panX: 0, panY: 0, split: 0.5 });
    const [zoomText, setZoomText] = useState(100);
    const [imgError, setImgError] = useState(false);
    const [reloadKey, setReloadKey] = useState(0);
    const prevFullLoading = useRef(false);
    const dragMode = useRef<"none" | "pan" | "split">("none");
    const lastPointer = useRef({ x: 0, y: 0 });

    const reset = () => {
      view.current = { zoom: 1, panX: 0, panY: 0, split: 0.5 };
      setZoomText(100);
      draw();
    };

    useImperativeHandle(ref, () => ({ reset }));

    const draw = () => {
      const canvas = canvasRef.current;
      const wrap = wrapRef.current;
      if (!canvas || !wrap) return;
      const dpr = window.devicePixelRatio || 1;
      const cw = wrap.clientWidth;
      const ch = wrap.clientHeight;
      if (cw <= 0 || ch <= 0) return;
      if (canvas.width !== Math.round(cw * dpr) || canvas.height !== Math.round(ch * dpr)) {
        canvas.width = Math.round(cw * dpr);
        canvas.height = Math.round(ch * dpr);
        canvas.style.width = `${cw}px`;
        canvas.style.height = `${ch}px`;
      }
      const ctx = canvas.getContext("2d");
      if (!ctx) return;
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      ctx.clearRect(0, 0, cw, ch);
      ctx.fillStyle = cssVar("--color-bg-subtle", "#eef0f4");
      ctx.fillRect(0, 0, cw, ch);

      const v = view.current;
      const fit = Math.min((cw - 24) / baseW, (ch - 24) / baseH);
      const scale = fit * v.zoom;
      const dw = baseW * scale;
      const dh = baseH * scale;
      const dx = (cw - dw) / 2 + v.panX;
      const dy = (ch - dh) / 2 + v.panY;
      const splitX = Math.max(0, Math.min(cw, v.split * cw));
      ctx.imageSmoothingEnabled = v.zoom < 3;
      ctx.imageSmoothingQuality = "high";

      const p = propsRef.current;
      const wantFull = v.zoom >= 3;
      const fullReady = Boolean(
        fullOrigRef.current?.complete &&
          fullOrigRef.current.naturalWidth > 0 &&
          fullCompRef.current?.complete &&
          fullCompRef.current.naturalWidth > 0,
      );
      if (wantFull && !fullReady && !p.fullOrigUrl && !p.fullLoading && !needFullRef.current) {
        needFullRef.current = true;
        p.onNeedFull?.();
      }
      const original = wantFull && fullReady ? fullOrigRef.current : originalRef.current;
      if (original && original.complete && original.naturalWidth > 0) {
        ctx.save();
        ctx.beginPath();
        ctx.rect(0, 0, splitX, ch);
        ctx.clip();
        ctx.drawImage(original, dx, dy, dw, dh);
        ctx.restore();
      }
      const compressed = wantFull && fullReady ? fullCompRef.current : compressedRef.current;
      if (compressed && compressed.complete && compressed.naturalWidth > 0) {
        ctx.save();
        ctx.beginPath();
        ctx.rect(splitX, 0, cw - splitX, ch);
        ctx.clip();
        ctx.drawImage(compressed, dx, dy, dw, dh);
        ctx.restore();
      }

      const accent = cssVar("--color-accent", "#4f5fe7");
      ctx.fillStyle = accent;
      ctx.fillRect(splitX - 1, 0, 2, ch);
      ctx.globalAlpha = 0.9;
      ctx.fillStyle = accent;
      ctx.beginPath();
      ctx.arc(splitX, ch / 2, 7, 0, Math.PI * 2);
      ctx.fill();
      ctx.globalAlpha = 1;
      ctx.strokeStyle = cssVar("--color-text-on-accent", "#ffffff");
      ctx.lineWidth = 2;
      ctx.beginPath();
      ctx.moveTo(splitX - 3, ch / 2 - 5);
      ctx.lineTo(splitX - 3, ch / 2 + 5);
      ctx.moveTo(splitX + 3, ch / 2 - 5);
      ctx.lineTo(splitX + 3, ch / 2 + 5);
      ctx.stroke();
    };

    useEffect(() => {
      const original = new Image();
      const compressed = new Image();
      setImgError(false);
      original.onload = draw;
      compressed.onload = draw;
      // 图源加载失败时给用户一个明确出口，而不是永远空白的画布（PS-A-024）。
      original.onerror = () => setImgError(true);
      compressed.onerror = () => setImgError(true);
      original.src = origUrl;
      compressed.src = compUrl;
      originalRef.current = original;
      compressedRef.current = compressed;
      fullOrigRef.current = null;
      fullCompRef.current = null;
      needFullRef.current = false;
      view.current = { zoom: 1, panX: 0, panY: 0, split: 0.5 };
      setZoomText(100);
      draw();
      // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [origUrl, compUrl, baseW, baseH, reloadKey]);

    useEffect(() => {
      fullOrigRef.current = null;
      fullCompRef.current = null;
      needFullRef.current = false;
      if (!props.fullOrigUrl || !props.fullCompUrl) {
        draw();
        return;
      }
      let alive = true;
      let loaded = 0;
      const a = new Image();
      const b = new Image();
      const ready = () => {
        if (!alive) return;
        loaded += 1;
        if (loaded >= 2) {
          fullOrigRef.current = a;
          fullCompRef.current = b;
          draw();
        }
      };
      a.onload = ready;
      b.onload = ready;
      a.src = props.fullOrigUrl;
      b.src = props.fullCompUrl;
      return () => {
        alive = false;
        a.onload = null;
        b.onload = null;
      };
      // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [props.fullOrigUrl, props.fullCompUrl]);

    // 高清源请求结束却没拿到图：复位「已请求」标记，让下次缩放还能再试
    // （PS-A-023：此前失败一次就永久停在预览源）。
    useEffect(() => {
      const was = prevFullLoading.current;
      prevFullLoading.current = Boolean(props.fullLoading);
      if (was && !props.fullLoading && !props.fullOrigUrl) {
        needFullRef.current = false;
      }
    }, [props.fullLoading, props.fullOrigUrl]);

    useEffect(() => {
      const wrap = wrapRef.current;
      if (!wrap) return;
      const ro = new ResizeObserver(() => draw());
      ro.observe(wrap);
      return () => ro.disconnect();
      // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [baseW, baseH]);

    // 跟随系统深浅色切换重绘（画布底色与分割线取自 token）。
    useEffect(() => {
      const mq = window.matchMedia("(prefers-color-scheme: dark)");
      const onTheme = () => draw();
      mq.addEventListener("change", onTheme);
      return () => mq.removeEventListener("change", onTheme);
      // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [baseW, baseH]);

    useEffect(() => {
      const canvas = canvasRef.current;
      if (!canvas) return;
      const onWheel = (e: WheelEvent) => {
        e.preventDefault();
        if (dragMode.current !== "none") return;
        const rect = canvas.getBoundingClientRect();
        const cx = e.clientX - rect.left;
        const cy = e.clientY - rect.top;
        const v = view.current;
        const oldZoom = v.zoom;
        const factor = e.deltaY < 0 ? 1.2 : 1 / 1.2;
        const newZoom = Math.min(MAX_ZOOM, Math.max(MIN_ZOOM, oldZoom * factor));
        if (newZoom === oldZoom) return;
        const cw = rect.width;
        const ch = rect.height;
        const fit = Math.min((cw - 24) / baseW, (ch - 24) / baseH);
        const oldScale = fit * oldZoom;
        const newScale = fit * newZoom;
        const oldDx = (cw - baseW * oldScale) / 2 + v.panX;
        const oldDy = (ch - baseH * oldScale) / 2 + v.panY;
        const imgX = (cx - oldDx) / oldScale;
        const imgY = (cy - oldDy) / oldScale;
        const newDx = cx - imgX * newScale;
        const newDy = cy - imgY * newScale;
        v.panX = newDx - (cw - baseW * newScale) / 2;
        v.panY = newDy - (ch - baseH * newScale) / 2;
        v.zoom = newZoom;
        clampPan();
        setZoomText(Math.round(newZoom * 100));
        draw();
      };
      canvas.addEventListener("wheel", onWheel, { passive: false });
      return () => canvas.removeEventListener("wheel", onWheel);
      // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [baseW, baseH]);

    const clampPan = () => {
      const wrap = wrapRef.current;
      if (!wrap) return;
      const cw = wrap.clientWidth;
      const ch = wrap.clientHeight;
      const v = view.current;
      const fit = Math.min((cw - 24) / baseW, (ch - 24) / baseH);
      const scale = fit * v.zoom;
      const dw = baseW * scale;
      const dh = baseH * scale;
      const maxX = Math.max(0, (dw - cw) / 2 + 40);
      const maxY = Math.max(0, (dh - ch) / 2 + 40);
      v.panX = Math.max(-maxX, Math.min(maxX, v.panX));
      v.panY = Math.max(-maxY, Math.min(maxY, v.panY));
    };

    const onPointerDown = (e: React.PointerEvent<HTMLCanvasElement>) => {
      const canvas = canvasRef.current;
      if (!canvas) return;
      const rect = canvas.getBoundingClientRect();
      const x = e.clientX - rect.left;
      const splitX = view.current.split * rect.width;
      if (e.button === 0 && Math.abs(x - splitX) <= 9) {
        dragMode.current = "split";
      } else if (e.button === 0 || e.button === 1 || e.altKey) {
        dragMode.current = "pan";
      } else {
        return;
      }
      e.preventDefault();
      canvas.setPointerCapture(e.pointerId);
      lastPointer.current = { x: e.clientX, y: e.clientY };
    };

    const onPointerMove = (e: React.PointerEvent<HTMLCanvasElement>) => {
      if (dragMode.current === "none") return;
      const canvas = canvasRef.current;
      if (!canvas) return;
      const rect = canvas.getBoundingClientRect();
      if (dragMode.current === "split") {
        view.current.split = Math.min(
          0.95,
          Math.max(0.05, (e.clientX - rect.left) / rect.width),
        );
      } else {
        view.current.panX += e.clientX - lastPointer.current.x;
        view.current.panY += e.clientY - lastPointer.current.y;
        clampPan();
      }
      lastPointer.current = { x: e.clientX, y: e.clientY };
      draw();
    };

    const endDrag = (e: React.PointerEvent<HTMLCanvasElement>) => {
      if (dragMode.current === "none") return;
      dragMode.current = "none";
      try {
        canvasRef.current?.releasePointerCapture(e.pointerId);
      } catch {
        /* ignore */
      }
    };

    return (
      <div className="preview-canvas-wrap" ref={wrapRef}>
        <canvas
          ref={canvasRef}
          onPointerDown={onPointerDown}
          onPointerMove={onPointerMove}
          onPointerUp={endDrag}
          onPointerCancel={endDrag}
          onDoubleClick={reset}
        />
        <div className="corner-label corner-orig">原图</div>
        <div className="corner-label corner-comp">压缩后</div>
        {imgError && (
          <button
            className="preview-error"
            onClick={() => {
              setImgError(false);
              needFullRef.current = false;
              setReloadKey((n) => n + 1);
            }}
          >
            预览加载失败（点此重试）
          </button>
        )}
        <div className="zoom-caption">
          {zoomText}% · 滚轮缩放 · 拖动平移 · 双击复位
          {zoomText >= 300
            ? props.fullOrigUrl
              ? " · 高清源"
              : props.fullLoading
                ? " · 高清源加载中…"
                : props.fullError
                  ? ` · 高清源失败：${props.fullError}`
                : " · 基于预览源"
            : ""}
        </div>
      </div>
    );
  },
);

PreviewCanvas.displayName = "PreviewCanvas";
export default PreviewCanvas;
