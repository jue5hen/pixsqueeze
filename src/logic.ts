// 纯函数工具：不依赖 React / Tauri，可直接被 node --test 覆盖
// （qa/logic.test.ts，PS-A-030：前端此前零逻辑测试）。

import type { BatchItemResult, BatchResult } from "./types";

/** Windows 路径归一化：统一斜杠、忽略大小写，用于判断「是不是同一张图」。 */
export function pathKey(p: string): string {
  return p.replace(/\\/g, "/").toLowerCase();
}

/** 把输入框文本夹到 [min,max] 的整数；解析失败时退回 fallback。 */
export function clampInt(
  raw: string,
  fallback: number,
  min: number,
  max: number,
): number {
  const parsed = Number.parseInt(raw, 10);
  const value = Number.isFinite(parsed) ? parsed : fallback;
  return Math.min(max, Math.max(min, value));
}

/** 相对原图的有符号体积变化百分比：正数为节省，负数为增大。 */
export function savingsPercent(origBytes: number, outBytes: number): number {
  if (!(origBytes > 0) || !(outBytes > 0)) return 0;
  return Math.round((1 - outBytes / origBytes) * 100);
}

/** 取父目录；Windows/Unix 分隔符都接受，没有分隔符时返回空串。 */
export function dirName(p: string): string {
  if (!p) return "";
  if (/^[A-Za-z]:[\\/]$/.test(p)) return p;
  const uncRoot = /^(?:\\\\|\/\/)[^\\/]+[\\/][^\\/]+[\\/]?$/.test(p);
  if (uncRoot) return p.replace(/[\\/]+$/, "");
  const clean = p.length > 1 ? p.replace(/[\\/]+$/, "") : p;
  const idx = Math.max(clean.lastIndexOf("\\"), clean.lastIndexOf("/"));
  if (idx < 0) return "";
  if (idx === 0) return clean.slice(0, 1);
  if (idx === 2 && /^[A-Za-z]:[\\/]/.test(clean)) return clean.slice(0, 3);
  return clean.slice(0, idx);
}

/** 异步请求是否仍是最新代际；用于失败/乱序时正确收尾 loading。 */
export function requestIsCurrent(requestId: number, latestRequestId: number): boolean {
  return requestId === latestRequestId;
}

export type BatchItemState = "success" | "below-target" | "failed" | "cancelled";

export function batchItemState(item: BatchItemResult): BatchItemState {
  if (item.cancelled) return "cancelled";
  if (!item.ok) return "failed";
  return item.reached_target ? "success" : "below-target";
}

function baseName(path: string): string {
  return path.split(/[\\/]/).pop() || path;
}

export function batchReportText(result: BatchResult): string {
  const lines = [
    `任务 ${result.job_id}`,
    `总数 ${result.total}，已生成 ${result.success}，达标 ${result.reached_target}，未达标 ${result.not_reached}，失败 ${result.failed}，取消 ${result.cancelled_count}，警告 ${result.warning_count}`,
  ];
  for (const item of result.results) {
    const state = batchItemState(item);
    const label =
      state === "success"
        ? "达标"
        : state === "below-target"
          ? "未达标"
          : state === "failed"
            ? "失败"
            : "取消";
    const details = [item.error, ...item.warnings].filter(Boolean).join("；");
    lines.push(`${label}\t${baseName(item.src)}${details ? `\t${details}` : ""}`);
  }
  return lines.join("\n");
}

export const IMAGE_EXTS = [
  "jpg",
  "jpeg",
  "png",
  "webp",
  "bmp",
  "gif",
  "tif",
  "tiff",
] as const;

/** 输入文件是否像图片（只看扩展名，与后端 is_image_file 保持一致）。 */
export function isImageFile(p: string): boolean {
  const lower = p.toLowerCase();
  const dot = lower.lastIndexOf(".");
  if (dot < 0) return false;
  return (IMAGE_EXTS as readonly string[]).includes(lower.slice(dot + 1));
}
