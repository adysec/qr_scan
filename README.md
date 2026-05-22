# qr_scan

发票二维码批量识别工具，包含两种运行方式：

1. Tauri GUI 桌面版
2. Rust CLI 命令行版

项目已支持 PDF、常见图片格式和 Word 文档（docx 原生解析）。

## 功能概览

1. 批量识别输入文件中的二维码
2. 发票二维码按固定 8 段字段解析
3. 多二维码时优先返回符合发票格式的内容
4. GUI 支持逐文件扫描与行级回显
5. 支持导出 CSV（中文列名）
6. 默认单文件超时为 5 秒

## 支持输入格式

1. PDF：pdf
2. 图片：png、jpg、jpeg、bmp、webp、tif、tiff
3. Word：docx（原生解析）

## 目录结构

1. src/main.rs：Tauri GUI 后端
2. src/bin/qr_scan_cli.rs：CLI 程序
3. ui/：前端页面（index.html、main.js、styles.css）
4. tauri.conf.json：Tauri 配置
5. icons/：应用图标资源

## 快速启动（GUI）

在项目根目录执行：

cargo tauri dev

## CLI 用法

基础命令：

cargo run --bin qr_scan_cli -- <文件或目录> --timeout 5

导出 CSV：

cargo run --bin qr_scan_cli -- <文件或目录> --timeout 5 --csv result.csv

示例：

cargo run --bin qr_scan_cli -- pdf/ --timeout 5

## GUI 发布构建

### Windows GNU（已验证可用）

cargo tauri build --target x86_64-pc-windows-gnu --no-bundle

产物：

target/x86_64-pc-windows-gnu/release/qr_scan.exe

### Linux GNU（已验证可用）

cargo tauri build --target x86_64-unknown-linux-gnu --no-bundle

产物：

target/x86_64-unknown-linux-gnu/release/qr_scan
