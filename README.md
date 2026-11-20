# PixSqueeze

图片批量压缩：按比例或目标体积压缩 JPEG / PNG / WebP，尽量保留 EXIF 与 ICC 信息。

## 功能

- 批量导入图片或整个目录，逐项显示压缩前后体积与状态
- 两种模式：按比例压缩、按目标体积压缩
- 支持 JPEG、PNG、WebP、GIF、BMP、TIFF 输入，输出扩展名与真实编码格式一致
- 可选保留 EXIF 与 ICC 色彩配置
- 大图自动收紧并发，避免内存峰值过高
- 单实例运行，设置自动保存，失败原因写入错误日志

## 下载

安装包见 [Releases](https://github.com/jue5hen/pixsqueeze/releases/latest)：

- `PixSqueeze_1.0.0_x64-setup.exe` — 按当前用户安装，默认不需要管理员权限

系统要求：Windows 10/11 x64，Microsoft Edge WebView2 Runtime（Windows 11 已内置）。

## 从源码构建

前置：Node.js 20+、Rust stable、Windows 上可用的 MSVC 或 GNU 工具链、WebView2 Runtime。

```powershell
npm ci
npm run tauri build
```

产物在 `src-tauri/target/release/bundle/nsis/`。

## 许可证

[MIT](LICENSE)
