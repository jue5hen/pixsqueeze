export type Mode = "pixel" | "size";
export type OutputMode = "suffix" | "folder";
export type Fmt = "auto" | "jpeg" | "png" | "webp";

export interface Settings {
  mode: Mode;
  percent: number;
  quality: number;
  target_kb: number;
  fmt: Fmt;
  keep_exif: boolean;
  sharpen: boolean;
  out_mode: OutputMode;
  last_open_dir: string;
  last_folder_dir: string;
  last_out_dir: string;
}

export const DEFAULT_SETTINGS: Settings = {
  mode: "pixel",
  percent: 100,
  quality: 82,
  target_kb: 500,
  fmt: "auto",
  keep_exif: false,
  sharpen: true,
  out_mode: "suffix",
  last_open_dir: "",
  last_folder_dir: "",
  last_out_dir: "",
};

export interface ImageItem {
  path: string;
  name: string;
  bytes: number;
  width: number;
  height: number;
  thumb: string;
  has_alpha: boolean;
  warnings: string[];
}

export interface SkippedInput {
  path: string;
  reason: string;
}

export interface ImportResult {
  images: ImageItem[];
  errors: string[];
  skipped_outputs: SkippedInput[];
}

export interface PreviewResult {
  orig_url: string;
  comp_url: string;
  base_w: number;
  base_h: number;
  comp_w: number;
  comp_h: number;
  format: string;
  transparent_flattened: boolean;
  warnings: string[];
}

export interface FullPreview {
  orig_url: string;
  comp_url: string;
  width: number;
  height: number;
  comp_w: number;
  comp_h: number;
  format: string;
  transparent_flattened: boolean;
  warnings: string[];
}

export interface EstimateResult {
  out_w: number;
  out_h: number;
  out_bytes: number;
  quality_used: number;
  scaled: boolean;
  reached_target: boolean;
  encode_attempts: number;
  budget_exhausted: boolean;
  format: string;
  transparent_flattened: boolean;
  warnings: string[];
}

export interface BatchItemResult {
  ok: boolean;
  cancelled: boolean;
  src: string;
  out: string;
  bytes: number;
  error: string;
  reached_target: boolean;
  budget_exhausted: boolean;
  out_w: number;
  out_h: number;
  format: string;
  transparent_flattened: boolean;
  warnings: string[];
}

export interface BatchResult {
  job_id: string;
  total: number;
  success: number;
  reached_target: number;
  not_reached: number;
  failed: number;
  cancelled_count: number;
  warning_count: number;
  results: BatchItemResult[];
  out_dir: string;
  first_error: string;
  cancelled: boolean;
}

export interface SettingsLoadResult {
  settings: Settings;
  warning: string | null;
}

export interface ProgressPayload {
  job_id: string;
  done: number;
  total: number;
  path: string;
}

export function humanSize(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / 1048576).toFixed(2)} MB`;
}
