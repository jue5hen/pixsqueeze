import { invoke } from "@tauri-apps/api/core";
import type {
  BatchResult,
  EstimateResult,
  FullPreview,
  ImportResult,
  PreviewResult,
  Settings,
  SettingsLoadResult,
} from "./types";

export const api = {
  loadSettings: () => invoke<SettingsLoadResult>("load_settings"),
  saveSettings: (s: Settings, revision: number) =>
    invoke<void>("save_settings", { s, revision }),
  importPaths: (paths: string[], includeOwnOutputs = false) =>
    invoke<ImportResult>("import_paths", { paths, includeOwnOutputs }),
  makePreview: (path: string, settings: Settings) =>
    invoke<PreviewResult>("make_preview", { path, settings }),
  previewFull: (path: string, settings: Settings, requestId: number) =>
    invoke<FullPreview | null>("preview_full", { requestId, path, settings }),
  estimateFull: (path: string, settings: Settings, requestId: number) =>
    invoke<EstimateResult>("estimate_full", { requestId, path, settings }),
  compressBatch: (paths: string[], settings: Settings, jobId: string) =>
    invoke<BatchResult>("compress_batch", { jobId, paths, settings }),
  cancelBatch: (jobId: string) => invoke<void>("cancel_batch", { jobId }),
  reveal: (path: string) => invoke<void>("reveal", { path }),
  cliPaths: () => invoke<string[]>("cli_paths"),
};
