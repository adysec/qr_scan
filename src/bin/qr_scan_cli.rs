use std::collections::HashSet;
use std::env;
use std::fs;
use std::ffi::OsStr;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use csv::WriterBuilder;
use image::{imageops::FilterType, DynamicImage, GrayImage, ImageBuffer, Luma, Rgb, RgbImage};
use lopdf::content::Content;
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;
use zip::ZipArchive;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct QrRow {
    file: String,
    page: usize,
    qr_index: usize,
    content: String,
    invoice_type_tag: String,
    invoice_kind_code: String,
    invoice_code: String,
    invoice_number: String,
    issue_amount: String,
    issue_date: String,
    invoice_check_code: String,
    encrypted_text: String,
}

#[derive(Default)]
struct InvoiceFields {
    invoice_type_tag: String,
    invoice_kind_code: String,
    invoice_code: String,
    invoice_number: String,
    issue_amount: String,
    issue_date: String,
    invoice_check_code: String,
    encrypted_text: String,
}

#[derive(Debug, Clone)]
struct ScanImage {
    page: usize,
    image: DynamicImage,
}

#[derive(Clone, Debug)]
enum ColorSpec {
    Gray,
    Rgb,
    Cmyk,
    Indexed { hival: u16 },
}

#[derive(Clone, Copy)]
struct PredictorParams {
    predictor: u32,
    colors: u32,
    columns: u32,
    bpc: u8,
}

const MAX_IMAGES_PER_PAGE: usize = 14;
const MAX_IMAGES_PER_FILE: usize = 64;
const MAX_SCAN_IMAGES_PER_FILE: usize = 36;
const MAX_DECODE_ATTEMPTS_FAST: usize = 20;
const MAX_DECODE_ATTEMPTS_AGGRESSIVE: usize = 80;
const MAX_EXTRACTION_IMAGES: usize = 96;
const MAX_IMAGE_SIDE: u32 = 9000;
const MAX_IMAGE_AREA: u64 = 40_000_000;
const MAX_ESTIMATED_RAW_BYTES: u64 = 64_000_000;
const MAX_WORD_IMAGES_PER_FILE: usize = 16;
const MAX_WORD_IMAGE_BYTES: u64 = 12_000_000;
const PRIMARY_SCAN_SOFT_TIMEOUT_SECS: u64 = 2;

fn print_usage() {
    eprintln!("用法:");
    eprintln!("  qr_scan_cli <文件或目录...> [--csv <输出.csv>] [--timeout <秒>]");
    eprintln!("示例:");
    eprintln!("  qr_scan_cli pdf/");
    eprintln!("  qr_scan_cli invoice1.pdf photo.jpg report.docx --csv result.csv --timeout 45");
}

fn parse_args() -> Result<(Vec<PathBuf>, Option<PathBuf>, u64)> {
    let mut inputs = Vec::new();
    let mut csv_output = None;
    let mut timeout_secs: u64 = 5;
    let mut iter = env::args().skip(1);

    while let Some(arg) = iter.next() {
        if arg == "--csv" {
            let Some(path) = iter.next() else {
                return Err(anyhow!("--csv 缺少输出文件路径"));
            };
            csv_output = Some(PathBuf::from(path));
            continue;
        }
        if arg == "--timeout" || arg == "-t" {
            let Some(v) = iter.next() else {
                return Err(anyhow!("--timeout 缺少秒数"));
            };
            timeout_secs = v
                .parse::<u64>()
                .with_context(|| format!("无效超时秒数: {v}"))?
                .max(1);
            continue;
        }
        if arg == "-h" || arg == "--help" {
            print_usage();
            std::process::exit(0);
        }
        inputs.push(PathBuf::from(arg));
    }

    if inputs.is_empty() {
        print_usage();
        return Err(anyhow!("未提供输入文件或目录"));
    }

    Ok((inputs, csv_output, timeout_secs))
}

fn expand_inputs(inputs: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for input in inputs {
        if input.is_file() {
            if is_supported_input_path(input) {
                out.push(input.clone());
            }
            continue;
        }

        if input.is_dir() {
            for entry in WalkDir::new(input).into_iter().flatten() {
                let path = entry.path();
                if path.is_file() && is_supported_input_path(path) {
                    out.push(path.to_path_buf());
                }
            }
        }
    }

    out.sort();
    out.dedup();
    out
}

enum ScanOutcome {
    Completed(Vec<QrRow>),
    TimedOut,
}

fn scan_file_with_timeout(path: PathBuf, timeout: Duration) -> Result<ScanOutcome> {
    let (tx, rx) = mpsc::channel();

    if matches!(extension_lower(&path).as_deref(), Some("pdf")) {
        // Text fallback is very cheap; try it first to avoid unnecessary heavy image scans.
        let text_rows = scan_pdf_text_rows(&path)?;
        if !text_rows.is_empty() {
            return Ok(ScanOutcome::Completed(text_rows));
        }
    }
    if matches!(extension_lower(&path).as_deref(), Some(ext) if is_supported_word_ext(ext)) {
        let text_rows = scan_word_text_rows(&path)?;
        if !text_rows.is_empty() {
            return Ok(ScanOutcome::Completed(text_rows));
        }
    }

    let path_clone = path.clone();
    let _jh = thread::spawn(move || {
        let result = (|| -> Result<Vec<QrRow>> {
            let images = collect_file_images(&path_clone)?;
            Ok(scan_images(&path_clone, images))
        })();
        let _ = tx.send(result);
    });

    let soft_timeout = Duration::from_secs(PRIMARY_SCAN_SOFT_TIMEOUT_SECS).min(timeout);
    match rx.recv_timeout(soft_timeout) {
        Ok(result) => {
            let rows = result?;
            if !rows.is_empty() || !matches!(extension_lower(&path).as_deref(), Some("pdf")) {
                return Ok(ScanOutcome::Completed(rows));
            }

            let render_rows = scan_pdf_render_rows(&path)?;
            if !render_rows.is_empty() {
                return Ok(ScanOutcome::Completed(render_rows));
            }

            return Ok(ScanOutcome::Completed(rows));
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            return Err(anyhow!("扫描线程异常: 通道已断开"));
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {}
    }

    // If primary scan is still running, attempt render fallback immediately for PDFs.
    if matches!(extension_lower(&path).as_deref(), Some("pdf")) {
        let render_rows = scan_pdf_render_rows(&path)?;
        if !render_rows.is_empty() {
            return Ok(ScanOutcome::Completed(render_rows));
        }
    }

    let remaining = timeout.saturating_sub(soft_timeout);
    if remaining.is_zero() {
        return Ok(ScanOutcome::TimedOut);
    }

    match rx.recv_timeout(remaining) {
        Ok(result) => {
            let rows = result?;
            if !rows.is_empty() || !matches!(extension_lower(&path).as_deref(), Some("pdf")) {
                return Ok(ScanOutcome::Completed(rows));
            }

            let render_rows = scan_pdf_render_rows(&path)?;
            if !render_rows.is_empty() {
                return Ok(ScanOutcome::Completed(render_rows));
            }

            Ok(ScanOutcome::Completed(rows))
        }
        Err(mpsc::RecvTimeoutError::Timeout) => Ok(ScanOutcome::TimedOut),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(anyhow!("扫描线程异常: 通道已断开")),
    }
}

fn run_cli(files: &[PathBuf], timeout_secs: u64) -> Result<Vec<QrRow>> {
    let mut rows = Vec::new();
    let total = files.len();
    let timeout = Duration::from_secs(timeout_secs);

    for (idx, path) in files.iter().enumerate() {
        let started = Instant::now();
        let before = rows.len();

        match scan_file_with_timeout(path.to_path_buf(), timeout) {
            Ok(ScanOutcome::Completed(file_rows)) => {
                if file_rows.is_empty() {
                    rows.push(build_failed_row(path, "识别失败"));
                } else {
                    rows.extend(file_rows);
                }
                let found = rows.len().saturating_sub(before);
                eprintln!(
                    "[{}/{}] {} -> 本文件识别 {} 条 (耗时: {:?})",
                    idx + 1,
                    total,
                    path.display(),
                    found,
                    started.elapsed()
                );
            }
            Ok(ScanOutcome::TimedOut) => {
                rows.push(build_failed_row(path, &format_scan_error_marker("超时")));
                eprintln!(
                    "[{}/{}] {} -> 超时跳过 (>{}s)",
                    idx + 1,
                    total,
                    path.display(),
                    timeout_secs
                );
            }
            Err(err) => {
                let marker = format_scan_error_marker(&err.to_string());
                rows.push(build_failed_row(path, &marker));
                eprintln!(
                    "[{}/{}] {} -> 失败: {} (耗时: {:?})",
                    idx + 1,
                    total,
                    path.display(),
                    err,
                    started.elapsed()
                );
            }
        }
    }

    Ok(rows)
}

fn scan_pdf_text_rows(path: &Path) -> Result<Vec<QrRow>> {
    let document = Document::load(path)
        .with_context(|| format!("无法读取 PDF 文本流: {}", path.display()))?;

    let mut candidates: Vec<(usize, String)> = Vec::new();
    for (page_no_u32, page_id) in document.get_pages() {
        let page_no = page_no_u32 as usize;
        let mut payloads: Vec<String> = extract_payloads_from_page_text(&document, page_id)
            .into_iter()
            .collect();
        payloads.sort();
        for payload in payloads {
            candidates.push((page_no, payload));
        }
    }

    let mut rows = Vec::new();
    if let Some((page, content)) = select_preferred_payload(&candidates) {
        rows.push(build_qr_row(path, page, 1, content));
    }

    Ok(rows)
}

fn scan_word_text_rows(path: &Path) -> Result<Vec<QrRow>> {
    let mut candidates: Vec<(usize, String)> = Vec::new();
    for payload in extract_payloads_from_docx_xml(path)? {
        candidates.push((1, payload));
    }

    let mut rows = Vec::new();
    if let Some((page, content)) = select_preferred_payload(&candidates) {
        rows.push(build_qr_row(path, page, 1, content));
    }

    Ok(rows)
}

fn extract_payloads_from_docx_xml(path: &Path) -> Result<HashSet<String>> {
    match extension_lower(path).as_deref() {
        Some("docx") => {}
        Some("doc") => {
            return Err(anyhow!(
                "原生模式暂不支持 .doc，请先另存为 .docx: {}",
                path.display()
            ));
        }
        _ => return Ok(HashSet::new()),
    }

    let file = File::open(path).with_context(|| format!("无法打开 Word 文件: {}", path.display()))?;
    let mut archive =
        ZipArchive::new(file).with_context(|| format!("无法解析 docx 压缩结构: {}", path.display()))?;

    let mut out = HashSet::new();
    for i in 0..archive.len() {
        let mut entry = match archive.by_index(i) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let name = entry.name().to_string();
        if !name.starts_with("word/") || !name.ends_with(".xml") {
            continue;
        }

        let mut xml = String::new();
        if entry.read_to_string(&mut xml).is_err() {
            continue;
        }

        for payload in extract_qr_candidates(&xml) {
            out.insert(payload);
        }
    }

    Ok(out)
}

fn scan_pdf_render_rows(path: &Path) -> Result<Vec<QrRow>> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let temp_dir = env::temp_dir().join(format!("qr_scan_render_{}_{}", std::process::id(), ts));
    fs::create_dir_all(&temp_dir)
        .with_context(|| format!("无法创建临时目录: {}", temp_dir.display()))?;

    let mut candidates: Vec<(usize, String)> = Vec::new();
    let mut seen = HashSet::new();
    let dpi_candidates = [200_u32, 300_u32, 400_u32];

    for dpi in dpi_candidates {
        let dpi_dir = temp_dir.join(format!("dpi_{dpi}"));
        fs::create_dir_all(&dpi_dir)?;

        let prefix = dpi_dir.join("page");
        let status = Command::new("pdftoppm")
            .arg("-r")
            .arg(dpi.to_string())
            .arg("-png")
            .arg(path)
            .arg(&prefix)
            .status();

        let render_ok = matches!(status, Ok(s) if s.success());
        if !render_ok {
            continue;
        }

        let mut pngs: Vec<(usize, PathBuf)> = Vec::new();
        for entry in fs::read_dir(&dpi_dir)? {
            let entry = match entry {
                Ok(v) => v,
                Err(_) => continue,
            };
            let p = entry.path();
            if !matches!(extension_lower(&p).as_deref(), Some("png")) {
                continue;
            }
            let page_no = parse_render_page_no(&p).unwrap_or(1);
            pngs.push((page_no, p));
        }
        pngs.sort_by_key(|(page, _)| *page);

        for (page_no, png_path) in pngs {
            let image = match image::open(&png_path) {
                Ok(img) => img,
                Err(_) => continue,
            };

            let mut payloads = decode_qr_payloads_from_dynamic(&image);
            if payloads.is_empty() {
                payloads = decode_qr_payloads_from_rendered(&image);
            }

            for payload in payloads {
                let normalized = payload.trim().to_string();
                if normalized.is_empty() {
                    continue;
                }
                if seen.insert(normalized.clone()) {
                    candidates.push((page_no, normalized));
                }
            }
        }

        if select_preferred_payload(&candidates).is_some() {
            break;
        }
    }

    let mut rows = Vec::new();
    if let Some((page, content)) = select_preferred_payload(&candidates) {
        rows.push(build_qr_row(path, page, 1, content));
    }

    let _ = fs::remove_dir_all(&temp_dir);
    Ok(rows)
}

fn decode_qr_payloads_from_rendered(image: &DynamicImage) -> Vec<String> {
    let mut found = HashSet::new();
    let gray = image.to_luma8();

    let mut variants = vec![gray.clone(), image::imageops::blur(&gray, 0.7), sharpen(&gray)];
    for scale in [2.0_f32, 3.0_f32] {
        let w = ((gray.width() as f32 * scale).round() as u32).clamp(1, 7000);
        let h = ((gray.height() as f32 * scale).round() as u32).clamp(1, 7000);
        variants.push(image::imageops::resize(&gray, w, h, FilterType::CatmullRom));
    }

    for variant in variants {
        for payload in decode_qr_payloads_with(&normalize_gray_input(&variant), true) {
            found.insert(payload);
        }
        if !found.is_empty() {
            break;
        }
    }

    let mut out: Vec<String> = found.into_iter().collect();
    out.sort();
    out
}

fn parse_render_page_no(path: &Path) -> Option<usize> {
    let stem = path.file_stem()?.to_str()?;
    let (_, tail) = stem.rsplit_once('-')?;
    tail.parse::<usize>().ok()
}

fn extract_payloads_from_page_text(document: &Document, page_id: ObjectId) -> HashSet<String> {
    let mut out = HashSet::new();
    let content_data = match document.get_page_content(page_id) {
        Ok(v) => v,
        Err(_) => return out,
    };

    let content = match Content::decode(&content_data) {
        Ok(c) => c,
        Err(_) => return out,
    };

    for op in content.operations {
        for operand in op.operands {
            collect_strings_from_object(&operand, &mut out);
        }
    }

    // Fallback for streams that fail operation-level decoding or split payload fragments.
    for payload in extract_payloads_from_raw_page_streams(document, page_id) {
        out.insert(payload);
    }

    out
}

fn extract_payloads_from_raw_page_streams(document: &Document, page_id: ObjectId) -> HashSet<String> {
    let mut out = HashSet::new();
    let stream_ids = document.get_page_contents(page_id);

    for stream_id in stream_ids {
        let obj = match document.get_object(stream_id) {
            Ok(o) => o,
            Err(_) => continue,
        };
        let stream = match obj {
            Object::Stream(s) => s,
            _ => continue,
        };

        let bytes = match stream.decompressed_content() {
            Ok(v) => v,
            Err(_) => continue,
        };

        for text in decode_pdf_text_bytes(&bytes) {
            for payload in extract_qr_candidates(&text) {
                out.insert(payload);
            }
        }
    }

    out
}

fn collect_strings_from_object(object: &Object, out: &mut HashSet<String>) {
    match object {
        Object::String(bytes, _) => {
            for text in decode_pdf_text_bytes(bytes) {
                for payload in extract_qr_candidates(&text) {
                    out.insert(payload);
                }
            }
        }
        Object::Array(arr) => {
            for item in arr {
                collect_strings_from_object(item, out);
            }
        }
        _ => {}
    }
}

fn decode_pdf_text_bytes(bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();

    let utf8 = String::from_utf8_lossy(bytes).to_string();
    if !utf8.is_empty() {
        out.push(utf8);
    }

    if bytes.len() >= 2 {
        if bytes[0] == 0xFE && bytes[1] == 0xFF {
            let mut u16s = Vec::new();
            let mut i = 2;
            while i + 1 < bytes.len() {
                u16s.push(u16::from_be_bytes([bytes[i], bytes[i + 1]]));
                i += 2;
            }
            if let Ok(s) = String::from_utf16(&u16s) {
                out.push(s);
            }
        } else if bytes[0] == 0xFF && bytes[1] == 0xFE {
            let mut u16s = Vec::new();
            let mut i = 2;
            while i + 1 < bytes.len() {
                u16s.push(u16::from_le_bytes([bytes[i], bytes[i + 1]]));
                i += 2;
            }
            if let Ok(s) = String::from_utf16(&u16s) {
                out.push(s);
            }
        } else {
            let mut zero_count = 0usize;
            for &b in bytes {
                if b == 0 {
                    zero_count += 1;
                }
            }
            if zero_count * 3 > bytes.len() {
                let mut u16s = Vec::new();
                let mut i = 0;
                while i + 1 < bytes.len() {
                    u16s.push(u16::from_be_bytes([bytes[i], bytes[i + 1]]));
                    i += 2;
                }
                if let Ok(s) = String::from_utf16(&u16s) {
                    out.push(s);
                }
            }
        }
    }

    out
}

fn extract_qr_candidates(text: &str) -> Vec<String> {
    let normalized = text
        .replace('\u{0}', "")
        .replace('\r', "")
        .replace('\n', "")
        .replace('\t', "")
        .replace('，', ",")
        .replace(' ', "");

    let mut out = Vec::new();
    let mut search_start = 0usize;

    while let Some(rel) = normalized[search_start..].find("01,32") {
        let start = search_start + rel;
        let mut end = start;
        for (off, ch) in normalized[start..].char_indices() {
            let ok = ch.is_ascii_alphanumeric() || ch == ',' || ch == '.' || ch == '-';
            if !ok {
                break;
            }
            end = start + off + ch.len_utf8();
            if end - start > 220 {
                break;
            }
        }

        if end > start {
            let candidate = &normalized[start..end];
            if let Some(payload) = normalize_qr_like_text(candidate) {
                out.push(payload);
            }
        }

        search_start = start + 5;
        if search_start >= normalized.len() {
            break;
        }
    }

    out
}

fn normalize_qr_like_text(text: &str) -> Option<String> {
    let s = text
        .replace('\u{0}', "")
        .replace('\r', "")
        .replace('\n', "")
        .replace('，', ",")
        .replace(" ", "");

    if !s.contains("01,32") {
        return None;
    }
    if s.matches(',').count() < 6 {
        return None;
    }
    if s.len() < 20 {
        return None;
    }

    Some(s)
}

fn is_invoice_payload(text: &str) -> bool {
    let parts: Vec<&str> = text.split(',').collect();
    if parts.len() < 8 {
        return false;
    }

    let head = parts[0].trim();
    let kind = parts[1].trim();
    !head.is_empty()
        && !kind.is_empty()
        && head.chars().all(|c| c.is_ascii_alphanumeric())
        && kind.chars().all(|c| c.is_ascii_alphanumeric())
}

fn select_preferred_payload(candidates: &[(usize, String)]) -> Option<(usize, String)> {
    let mut first_any: Option<(usize, String)> = None;
    for (page, payload) in candidates {
        if first_any.is_none() {
            first_any = Some((*page, payload.clone()));
        }
        if is_invoice_payload(payload) {
            return Some((*page, payload.clone()));
        }
    }
    first_any
}

fn build_qr_row(path: &Path, page: usize, qr_index: usize, content: String) -> QrRow {
    let fields = parse_invoice_fields(&content);
    QrRow {
        file: path.display().to_string(),
        page,
        qr_index,
        content,
        invoice_type_tag: fields.invoice_type_tag,
        invoice_kind_code: fields.invoice_kind_code,
        invoice_code: fields.invoice_code,
        invoice_number: fields.invoice_number,
        issue_amount: fields.issue_amount,
        issue_date: fields.issue_date,
        invoice_check_code: fields.invoice_check_code,
        encrypted_text: fields.encrypted_text,
    }
}

fn build_failed_row(path: &Path, marker: &str) -> QrRow {
    QrRow {
        file: path.display().to_string(),
        page: 0,
        qr_index: 0,
        content: marker.to_string(),
        invoice_type_tag: marker.to_string(),
        invoice_kind_code: marker.to_string(),
        invoice_code: marker.to_string(),
        invoice_number: marker.to_string(),
        issue_amount: marker.to_string(),
        issue_date: marker.to_string(),
        invoice_check_code: marker.to_string(),
        encrypted_text: marker.to_string(),
    }
}

fn format_scan_error_marker(message: &str) -> String {
    let compact = message.replace('\n', " ").replace('\r', " ");
    let trimmed = compact.trim();
    if trimmed.is_empty() {
        return "识别失败".to_string();
    }

    let short: String = trimmed.chars().take(32).collect();
    format!("识别失败: {short}")
}

fn parse_invoice_fields(content: &str) -> InvoiceFields {
    let mut fields = InvoiceFields::default();
    if !is_invoice_payload(content) {
        fields.invoice_type_tag = content.to_string();
        fields.invoice_kind_code = content.to_string();
        fields.invoice_code = content.to_string();
        fields.invoice_number = content.to_string();
        fields.issue_amount = content.to_string();
        fields.issue_date = content.to_string();
        fields.invoice_check_code = content.to_string();
        fields.encrypted_text = content.to_string();
        return fields;
    }

    let parts: Vec<String> = content.split(',').map(|s| s.trim().to_string()).collect();
    fields.invoice_type_tag = parts.get(0).cloned().unwrap_or_default();
    fields.invoice_kind_code = parts.get(1).cloned().unwrap_or_default();
    fields.invoice_code = parts.get(2).cloned().unwrap_or_default();
    fields.invoice_number = parts.get(3).cloned().unwrap_or_default();
    fields.issue_amount = parts.get(4).cloned().unwrap_or_default();
    fields.issue_date = parts.get(5).cloned().unwrap_or_default();
    fields.invoice_check_code = parts.get(6).cloned().unwrap_or_default();
    fields.encrypted_text = parts.get(7).cloned().unwrap_or_default();

    fields
}

fn print_rows(rows: &[QrRow]) {
    if rows.is_empty() {
        println!("未识别到二维码");
        return;
    }

    println!("共识别 {} 条二维码:", rows.len());
    for row in rows {
        println!(
            "file={} page={} qr_index={} content={} 发票类型标识={} 发票种类代码={} 发票代码={} 发票号码={} 开票金额={} 开票日期={} 发票校验码={} 加密字符={}",
            row.file,
            row.page,
            row.qr_index,
            row.content,
            row.invoice_type_tag,
            row.invoice_kind_code,
            row.invoice_code,
            row.invoice_number,
            row.issue_amount,
            row.issue_date,
            row.invoice_check_code,
            row.encrypted_text
        );
    }
}

fn collect_file_images(path: &Path) -> Result<Vec<ScanImage>> {
    match extension_lower(path).as_deref() {
        Some(ext) if is_supported_image_ext(ext) => {
            let image = image::open(path)
                .with_context(|| format!("无法打开图片文件: {}", path.display()))?;
            Ok(vec![ScanImage { page: 1, image }])
        }
        Some("pdf") => collect_pdf_images(path),
        Some(ext) if is_supported_word_ext(ext) => collect_word_images(path),
        _ => Ok(Vec::new()),
    }
}

fn collect_word_images(path: &Path) -> Result<Vec<ScanImage>> {
    match extension_lower(path).as_deref() {
        Some("docx") => {}
        Some("doc") => {
            return Err(anyhow!(
                "原生模式暂不支持 .doc，请先另存为 .docx: {}",
                path.display()
            ));
        }
        _ => return Ok(Vec::new()),
    }

    let file = File::open(path).with_context(|| format!("无法打开 Word 文件: {}", path.display()))?;
    let mut archive =
        ZipArchive::new(file).with_context(|| format!("无法解析 docx 压缩结构: {}", path.display()))?;

    let mut out = Vec::new();
    for i in 0..archive.len() {
        let mut entry = match archive.by_index(i) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let name = entry.name().to_ascii_lowercase();
        if !name.starts_with("word/media/") {
            continue;
        }
        if entry.size() > MAX_WORD_IMAGE_BYTES {
            continue;
        }

        let format = match name.rsplit('.').next() {
            Some("png") => image::ImageFormat::Png,
            Some("jpg") | Some("jpeg") => image::ImageFormat::Jpeg,
            Some("bmp") => image::ImageFormat::Bmp,
            Some("webp") => image::ImageFormat::WebP,
            Some("tif") | Some("tiff") => image::ImageFormat::Tiff,
            _ => continue,
        };

        let mut buf = Vec::new();
        if entry.read_to_end(&mut buf).is_err() {
            continue;
        }

        let image = match image::load_from_memory_with_format(&buf, format) {
            Ok(v) => v,
            Err(_) => continue,
        };

        out.push(ScanImage { page: 1, image });
        if out.len() >= MAX_WORD_IMAGES_PER_FILE {
            break;
        }
    }

    Ok(out)
}

fn collect_pdf_images(path: &Path) -> Result<Vec<ScanImage>> {
    let document = Document::load(path)
        .with_context(|| format!("无法读取 PDF 文件: {}", path.display()))?;

    let mut out = Vec::new();
    for (page_no_u32, page_id) in document.get_pages() {
        let page_no = page_no_u32 as usize;
        let mut page_images = extract_page_images(&document, page_id)
            .with_context(|| format!("提取 PDF 图片失败: {} 第 {} 页", path.display(), page_no))?;
        page_images.extend(
            extract_annotation_images(&document, page_id)
                .with_context(|| format!("提取 PDF 注释图片失败: {} 第 {} 页", path.display(), page_no))?,
        );

        page_images = prioritize_images(page_images);
        if page_images.len() > MAX_IMAGES_PER_PAGE {
            page_images.truncate(MAX_IMAGES_PER_PAGE);
        }

        for image in page_images {
            out.push(ScanImage {
                page: page_no,
                image,
            });

            if out.len() >= MAX_IMAGES_PER_FILE {
                return Ok(out);
            }
        }
    }

    if out.is_empty() {
        let mut fallback = prioritize_images(extract_document_images(&document));
        if fallback.len() > MAX_IMAGES_PER_FILE {
            fallback.truncate(MAX_IMAGES_PER_FILE);
        }
        for image in fallback {
            out.push(ScanImage { page: 1, image });
        }
    }

    Ok(out)
}

fn scan_images(path: &Path, images: Vec<ScanImage>) -> Vec<QrRow> {
    let mut candidates: Vec<(usize, String)> = Vec::new();
    let mut seen = HashSet::new();

    for (idx, scan_image) in images.into_iter().enumerate() {
        if idx >= MAX_SCAN_IMAGES_PER_FILE {
            break;
        }
        let page = scan_image.page.max(1);
        for payload in decode_qr_payloads_from_dynamic(&scan_image.image) {
            let normalized = payload.trim().to_string();
            if normalized.is_empty() {
                continue;
            }
            if seen.insert(normalized.clone()) {
                let invoice_like = is_invoice_payload(&normalized);
                candidates.push((page, normalized));
                if invoice_like {
                    break;
                }
            }
        }

        if candidates
            .last()
            .map(|(_, payload)| is_invoice_payload(payload))
            .unwrap_or(false)
        {
            break;
        }
    }

    let mut rows = Vec::new();
    if let Some((page, content)) = select_preferred_payload(&candidates) {
        rows.push(build_qr_row(path, page, 1, content));
    }

    rows
}

fn image_priority(image: &DynamicImage) -> i64 {
    let w = image.width() as i64;
    let h = image.height() as i64;
    let area = w.saturating_mul(h);
    if area <= 0 {
        return i64::MIN;
    }

    let ratio_penalty = (w - h).abs() * 1000 / w.max(h);
    let area_score = (area.min(5_000_000) / 10_000).max(1);
    area_score - ratio_penalty
}

fn prioritize_images(mut images: Vec<DynamicImage>) -> Vec<DynamicImage> {
    images.sort_by_key(|img| std::cmp::Reverse(image_priority(img)));
    images
}

fn extract_page_images(document: &Document, page_id: ObjectId) -> Result<Vec<DynamicImage>> {
    let (resources_opt, _) = document.get_page_resources(page_id);
    let Some(resources) = resources_opt else {
        return Ok(Vec::new());
    };

    let mut visited = HashSet::new();
    let mut out = Vec::new();
    extract_images_from_resources(document, resources, &mut visited, &mut out)?;
    if out.len() > MAX_IMAGES_PER_PAGE * 2 {
        out.truncate(MAX_IMAGES_PER_PAGE * 2);
    }
    Ok(out)
}

fn extract_annotation_images(document: &Document, page_id: ObjectId) -> Result<Vec<DynamicImage>> {
    let page_obj = document.get_object(page_id)?;
    let page_dict = match page_obj {
        Object::Dictionary(dict) => dict,
        _ => return Ok(Vec::new()),
    };

    let annots_obj = match page_dict.get(b"Annots") {
        Ok(obj) => obj,
        Err(_) => return Ok(Vec::new()),
    };

    let mut visited = HashSet::new();
    let mut out = Vec::new();
    collect_images_from_object(document, annots_obj, &mut visited, &mut out)?;
    Ok(out)
}

fn collect_images_from_object(
    document: &Document,
    object: &Object,
    visited: &mut HashSet<ObjectId>,
    out: &mut Vec<DynamicImage>,
) -> Result<()> {
    if out.len() >= MAX_EXTRACTION_IMAGES {
        return Ok(());
    }

    match object {
        Object::Reference(id) => {
            if visited.insert(*id) {
                let obj = document.get_object(*id)?;
                collect_images_from_object(document, obj, visited, out)?;
            }
        }
        Object::Array(arr) => {
            for item in arr {
                collect_images_from_object(document, item, visited, out)?;
            }
        }
        Object::Dictionary(dict) => {
            if dict.has(b"XObject") {
                extract_images_from_resources(document, dict, visited, out)?;
            }
            for key in [b"AP" as &[u8], b"N", b"R", b"D"] {
                if let Ok(obj) = dict.get(key) {
                    collect_images_from_object(document, obj, visited, out)?;
                }
            }
        }
        Object::Stream(stream) => {
            match stream.dict.get(b"Subtype") {
                Ok(Object::Name(name)) if name.as_slice() == b"Image" => {
                    if should_decode_stream_image(stream) {
                        if let Some(image) = decode_pdf_image_stream(document, stream)? {
                            out.push(image);
                        }
                    }
                }
                Ok(Object::Name(name)) if name.as_slice() == b"Form" => {
                    if let Ok(resources_obj) = stream.dict.get(b"Resources") {
                        let resources = resolve_dictionary(document, resources_obj)?;
                        extract_images_from_resources(document, &resources, visited, out)?;
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }

    Ok(())
}

fn extract_document_images(document: &Document) -> Vec<DynamicImage> {
    let mut out = Vec::new();
    for object in document.objects.values() {
        if out.len() >= MAX_EXTRACTION_IMAGES {
            break;
        }

        if let Object::Stream(stream) = object {
            if matches!(
                stream.dict.get(b"Subtype"),
                Ok(Object::Name(name)) if name.as_slice() == b"Image"
            ) {
                if !should_decode_stream_image(stream) {
                    continue;
                }
                if let Ok(Some(image)) = decode_pdf_image_stream(document, stream) {
                        out.push(image);
                    }
                }
        }
    }
    out
}

fn extract_images_from_resources(
    document: &Document,
    resources: &Dictionary,
    visited: &mut HashSet<ObjectId>,
    out: &mut Vec<DynamicImage>,
) -> Result<()> {
    if out.len() >= MAX_EXTRACTION_IMAGES {
        return Ok(());
    }

    let xobjects = match resources.get(b"XObject") {
        Ok(obj) => resolve_dictionary(document, obj)?,
        Err(_) => return Ok(()),
    };

    for (_, object) in xobjects.iter() {
        let obj_id = match object {
            Object::Reference(id) => *id,
            _ => continue,
        };
        if !visited.insert(obj_id) {
            continue;
        }

        let obj = document.get_object(obj_id)?;
        let stream = match obj {
            Object::Stream(stream) => stream,
            _ => continue,
        };

        match stream.dict.get(b"Subtype") {
            Ok(Object::Name(name)) if name.as_slice() == b"Image" => {
                if should_decode_stream_image(stream) {
                    if let Some(image) = decode_pdf_image_stream(document, stream)? {
                        out.push(image);
                    }
                }
            }
            Ok(Object::Name(name)) if name.as_slice() == b"Form" => {
                if let Ok(form_resources_obj) = stream.dict.get(b"Resources") {
                    let form_resources = resolve_dictionary(document, form_resources_obj)?;
                    extract_images_from_resources(document, &form_resources, visited, out)?;
                }
            }
            _ => {}
        }
    }

    Ok(())
}

fn resolve_object(document: &Document, object: &Object) -> Result<Object> {
    let mut current = object.clone();
    for _ in 0..16 {
        match current {
            Object::Reference(id) => current = document.get_object(id)?.clone(),
            _ => return Ok(current),
        }
    }
    Err(anyhow!("对象引用层级过深"))
}

fn resolve_dictionary(document: &Document, object: &Object) -> Result<Dictionary> {
    match resolve_object(document, object)? {
        Object::Dictionary(dict) => Ok(dict),
        _ => Err(anyhow!("对象不是 Dictionary")),
    }
}

fn decode_pdf_image_stream(document: &Document, stream: &Stream) -> Result<Option<DynamicImage>> {
    if let Ok(image) = image::load_from_memory(&stream.content) {
        return Ok(Some(image));
    }

    let mut decoded = match stream.decompressed_content() {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };

    let width = match stream.dict.get(b"Width").and_then(|o| o.as_i64()) {
        Ok(v) if v > 0 => v as u32,
        _ => return Ok(None),
    };
    let height = match stream.dict.get(b"Height").and_then(|o| o.as_i64()) {
        Ok(v) if v > 0 => v as u32,
        _ => return Ok(None),
    };
    let bpc = stream
        .dict
        .get(b"BitsPerComponent")
        .and_then(|o| o.as_i64())
        .unwrap_or(8);

    decoded = apply_predictor_if_needed(document, stream, decoded, width, height, bpc as u8);

    if let Ok(image) = image::load_from_memory(&decoded) {
        return Ok(Some(image));
    }

    let color_spec = stream
        .dict
        .get(b"ColorSpace")
        .ok()
        .and_then(|obj| parse_color_spec(document, obj));

    if bpc == 1 {
        if matches!(color_spec, Some(ColorSpec::Gray) | None) {
            return Ok(
                decode_1bit_gray_image(width, height, &decoded, should_invert_monochrome(stream))
                    .map(DynamicImage::ImageLuma8),
            );
        }
        if let Some(ColorSpec::Indexed { hival }) = color_spec {
            return Ok(decode_indexed_to_gray(width, height, &decoded, 1, hival)
                .map(DynamicImage::ImageLuma8));
        }
        return Ok(None);
    }

    if let Some(ColorSpec::Indexed { hival }) = color_spec {
        let indexed_bpc = match bpc {
            2 => Some(2_u8),
            4 => Some(4_u8),
            8 => Some(8_u8),
            _ => None,
        };
        if let Some(bits) = indexed_bpc {
            return Ok(decode_indexed_to_gray(width, height, &decoded, bits, hival)
                .map(DynamicImage::ImageLuma8));
        }
        return Ok(None);
    }

    let channels = color_spec
        .as_ref()
        .and_then(|s| match s {
            ColorSpec::Gray => Some(1_usize),
            ColorSpec::Rgb => Some(3_usize),
            ColorSpec::Cmyk => Some(4_usize),
            ColorSpec::Indexed { .. } => Some(1_usize),
        })
        .or_else(|| infer_channels(width, height, decoded.len()));

    if bpc == 16 {
        return match channels {
            Some(1) => Ok(decode_16bit_gray(width, height, &decoded).map(DynamicImage::ImageLuma8)),
            Some(3) => Ok(decode_16bit_rgb(width, height, &decoded).map(DynamicImage::ImageRgb8)),
            Some(4) => Ok(decode_16bit_cmyk(width, height, &decoded).map(DynamicImage::ImageRgb8)),
            _ => Ok(None),
        };
    }

    if bpc != 8 {
        return Ok(None);
    }

    match channels {
        Some(1) => Ok(GrayImage::from_raw(width, height, decoded).map(DynamicImage::ImageLuma8)),
        Some(3) => Ok(RgbImage::from_raw(width, height, decoded).map(DynamicImage::ImageRgb8)),
        Some(4) => Ok(decode_cmyk_image(width, height, &decoded).map(DynamicImage::ImageRgb8)),
        _ => Ok(None),
    }
}

fn should_decode_stream_image(stream: &Stream) -> bool {
    let width = stream
        .dict
        .get(b"Width")
        .ok()
        .and_then(|o| o.as_i64().ok())
        .unwrap_or(0);
    let height = stream
        .dict
        .get(b"Height")
        .ok()
        .and_then(|o| o.as_i64().ok())
        .unwrap_or(0);

    if width <= 0 || height <= 0 {
        return false;
    }

    let w = width as u32;
    let h = height as u32;
    if w > MAX_IMAGE_SIDE || h > MAX_IMAGE_SIDE {
        return false;
    }

    let area = u64::from(w) * u64::from(h);
    if area > MAX_IMAGE_AREA {
        return false;
    }

    let bpc = stream
        .dict
        .get(b"BitsPerComponent")
        .ok()
        .and_then(|o| o.as_i64().ok())
        .unwrap_or(8)
        .clamp(1, 16) as u64;
    let estimated_raw = area
        .saturating_mul(4)
        .saturating_mul(bpc)
        .saturating_div(8);
    if estimated_raw > MAX_ESTIMATED_RAW_BYTES {
        return false;
    }

    let filters = extract_filter_names(stream);
    if filters.iter().any(|f| f == "FlateDecode") {
        if stream.content.len() > 600_000 || area > 6_000_000 {
            return false;
        }
    }

    is_safe_filter_chain(stream)
}

fn is_safe_filter_chain(stream: &Stream) -> bool {
    let filters = extract_filter_names(stream);
    if filters.is_empty() {
        return true;
    }

    for f in &filters {
        match f.as_str() {
            "DCTDecode"
            | "JPXDecode"
            | "FlateDecode"
            | "LZWDecode"
            | "RunLengthDecode"
            | "CCITTFaxDecode"
            | "JBIG2Decode"
            | "ASCII85Decode"
            | "ASCIIHexDecode" => {}
            _ => return false,
        }
    }

    true
}

fn extract_filter_names(stream: &Stream) -> Vec<String> {
    let mut out = Vec::new();
    let filter_obj = match stream.dict.get(b"Filter") {
        Ok(v) => v,
        Err(_) => return out,
    };

    match filter_obj {
        Object::Name(name) => out.push(String::from_utf8_lossy(name).to_string()),
        Object::Array(arr) => {
            for item in arr {
                if let Object::Name(name) = item {
                    out.push(String::from_utf8_lossy(name).to_string());
                }
            }
        }
        _ => {}
    }

    out
}

fn parse_color_spec(document: &Document, object: &Object) -> Option<ColorSpec> {
    match resolve_object(document, object).ok()? {
        Object::Name(name) => match name.as_slice() {
            b"DeviceGray" | b"CalGray" => Some(ColorSpec::Gray),
            b"DeviceRGB" | b"CalRGB" | b"Lab" => Some(ColorSpec::Rgb),
            b"DeviceCMYK" => Some(ColorSpec::Cmyk),
            _ => None,
        },
        Object::Array(arr) => {
            let first = arr.first()?;
            let op = match resolve_object(document, first).ok()? {
                Object::Name(name) => name,
                _ => return None,
            };
            match op.as_slice() {
                b"ICCBased" => {
                    let profile = arr.get(1)?;
                    let profile_obj = resolve_object(document, profile).ok()?;
                    match profile_obj {
                        Object::Stream(s) => {
                            let n = s.dict.get(b"N").ok()?.as_i64().ok()?;
                            match n {
                                1 => Some(ColorSpec::Gray),
                                3 => Some(ColorSpec::Rgb),
                                4 => Some(ColorSpec::Cmyk),
                                _ => None,
                            }
                        }
                        _ => None,
                    }
                }
                b"Indexed" => {
                    let hival = arr.get(2)?.as_i64().ok()?;
                    if hival >= 0 {
                        Some(ColorSpec::Indexed { hival: hival as u16 })
                    } else {
                        None
                    }
                }
                b"Separation" => Some(ColorSpec::Gray),
                b"DeviceN" => {
                    let names = match arr.get(1) {
                        Some(Object::Array(values)) => values.len(),
                        _ => 0,
                    };
                    match names {
                        1 => Some(ColorSpec::Gray),
                        3 => Some(ColorSpec::Rgb),
                        4 => Some(ColorSpec::Cmyk),
                        _ => None,
                    }
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn infer_channels(width: u32, height: u32, len: usize) -> Option<usize> {
    let px = (width as usize).checked_mul(height as usize)?;
    if px == 0 {
        return None;
    }
    if len == px {
        return Some(1);
    }
    if len == px * 3 {
        return Some(3);
    }
    if len == px * 4 {
        return Some(4);
    }
    None
}

fn should_invert_monochrome(stream: &Stream) -> bool {
    let decode = match stream.dict.get(b"Decode") {
        Ok(Object::Array(arr)) if arr.len() >= 2 => arr,
        _ => return false,
    };

    let a = decode[0].as_f32().ok();
    let b = decode[1].as_f32().ok();
    matches!((a, b), (Some(1.0), Some(0.0)))
}

fn decode_1bit_gray_image(width: u32, height: u32, data: &[u8], invert: bool) -> Option<GrayImage> {
    let row_bytes = width.div_ceil(8) as usize;
    if data.len() < row_bytes * height as usize {
        return None;
    }

    let image = ImageBuffer::from_fn(width, height, |x, y| {
        let byte = data[(y as usize * row_bytes) + (x as usize / 8)];
        let bit = 7 - (x as usize % 8);
        let mut on = ((byte >> bit) & 1) == 1;
        if invert {
            on = !on;
        }
        Luma([if on { 255 } else { 0 }])
    });
    Some(image)
}

fn decode_indexed_to_gray(
    width: u32,
    height: u32,
    data: &[u8],
    bpc: u8,
    hival: u16,
) -> Option<GrayImage> {
    let total_pixels = (width as usize).checked_mul(height as usize)?;
    if total_pixels == 0 || !matches!(bpc, 1 | 2 | 4 | 8) {
        return None;
    }

    let mut indices = Vec::with_capacity(total_pixels);
    if bpc == 8 {
        if data.len() < total_pixels {
            return None;
        }
        indices.extend_from_slice(&data[..total_pixels]);
    } else {
        for &byte in data {
            let mut shift: u8 = 8 - bpc;
            loop {
                let mask = ((1_u16 << bpc) - 1) as u8;
                indices.push((byte >> shift) & mask);
                if indices.len() == total_pixels {
                    break;
                }
                if shift == 0 {
                    break;
                }
                shift = shift.saturating_sub(bpc);
            }
            if indices.len() == total_pixels {
                break;
            }
        }
        if indices.len() < total_pixels {
            return None;
        }
    }

    let scale = if hival == 0 { 255.0 } else { 255.0 / (hival as f32) };
    Some(ImageBuffer::from_fn(width, height, |x, y| {
        let idx = indices[(y * width + x) as usize] as f32;
        let v = (idx * scale).round().clamp(0.0, 255.0) as u8;
        Luma([v])
    }))
}

fn decode_cmyk_image(width: u32, height: u32, data: &[u8]) -> Option<RgbImage> {
    if data.len() != (width as usize) * (height as usize) * 4 {
        return None;
    }

    Some(ImageBuffer::from_fn(width, height, |x, y| {
        let idx = ((y * width + x) * 4) as usize;
        let c = data[idx] as f32 / 255.0;
        let m = data[idx + 1] as f32 / 255.0;
        let yv = data[idx + 2] as f32 / 255.0;
        let k = data[idx + 3] as f32 / 255.0;

        let r = ((1.0 - (c * (1.0 - k) + k)) * 255.0).round().clamp(0.0, 255.0) as u8;
        let g = ((1.0 - (m * (1.0 - k) + k)) * 255.0).round().clamp(0.0, 255.0) as u8;
        let b = ((1.0 - (yv * (1.0 - k) + k)) * 255.0).round().clamp(0.0, 255.0) as u8;
        Rgb([r, g, b])
    }))
}

fn decode_16bit_gray(width: u32, height: u32, data: &[u8]) -> Option<GrayImage> {
    if data.len() != (width as usize) * (height as usize) * 2 {
        return None;
    }
    Some(ImageBuffer::from_fn(width, height, |x, y| {
        let idx = ((y * width + x) * 2) as usize;
        Luma([data[idx]])
    }))
}

fn decode_16bit_rgb(width: u32, height: u32, data: &[u8]) -> Option<RgbImage> {
    if data.len() != (width as usize) * (height as usize) * 6 {
        return None;
    }
    Some(ImageBuffer::from_fn(width, height, |x, y| {
        let idx = ((y * width + x) * 6) as usize;
        Rgb([data[idx], data[idx + 2], data[idx + 4]])
    }))
}

fn decode_16bit_cmyk(width: u32, height: u32, data: &[u8]) -> Option<RgbImage> {
    if data.len() != (width as usize) * (height as usize) * 8 {
        return None;
    }

    Some(ImageBuffer::from_fn(width, height, |x, y| {
        let idx = ((y * width + x) * 8) as usize;
        let c = data[idx] as f32 / 255.0;
        let m = data[idx + 2] as f32 / 255.0;
        let yv = data[idx + 4] as f32 / 255.0;
        let k = data[idx + 6] as f32 / 255.0;

        let r = ((1.0 - (c * (1.0 - k) + k)) * 255.0).round().clamp(0.0, 255.0) as u8;
        let g = ((1.0 - (m * (1.0 - k) + k)) * 255.0).round().clamp(0.0, 255.0) as u8;
        let b = ((1.0 - (yv * (1.0 - k) + k)) * 255.0).round().clamp(0.0, 255.0) as u8;
        Rgb([r, g, b])
    }))
}

fn apply_predictor_if_needed(
    document: &Document,
    stream: &Stream,
    decoded: Vec<u8>,
    width: u32,
    height: u32,
    default_bpc: u8,
) -> Vec<u8> {
    let Some(params) = parse_predictor_params(document, stream, width, default_bpc) else {
        return decoded;
    };

    if params.predictor <= 1 {
        return decoded;
    }

    if (10..=15).contains(&params.predictor) {
        return decode_png_predictor(decoded.clone(), height, params).unwrap_or(decoded);
    }

    decoded
}

fn parse_predictor_params(
    document: &Document,
    stream: &Stream,
    width: u32,
    default_bpc: u8,
) -> Option<PredictorParams> {
    let decode_parms_obj = stream.dict.get(b"DecodeParms").ok()?;
    let decoded = resolve_object(document, decode_parms_obj).ok()?;
    let dict = match decoded {
        Object::Dictionary(d) => Some(d),
        Object::Array(arr) => arr
            .into_iter()
            .find_map(|obj| match resolve_object(document, &obj).ok() {
                Some(Object::Dictionary(d)) => Some(d),
                _ => None,
            }),
        _ => None,
    }?;

    let predictor = dict
        .get(b"Predictor")
        .ok()
        .and_then(|o| o.as_i64().ok())
        .unwrap_or(1)
        .max(1) as u32;
    let colors = dict
        .get(b"Colors")
        .ok()
        .and_then(|o| o.as_i64().ok())
        .unwrap_or(1)
        .max(1) as u32;
    let columns = dict
        .get(b"Columns")
        .ok()
        .and_then(|o| o.as_i64().ok())
        .unwrap_or(width as i64)
        .max(1) as u32;
    let bpc = dict
        .get(b"BitsPerComponent")
        .ok()
        .and_then(|o| o.as_i64().ok())
        .unwrap_or(default_bpc as i64)
        .clamp(1, 16) as u8;

    Some(PredictorParams {
        predictor,
        colors,
        columns,
        bpc,
    })
}

fn decode_png_predictor(data: Vec<u8>, rows: u32, params: PredictorParams) -> Option<Vec<u8>> {
    let bytes_per_pixel = ((params.colors * params.bpc as u32) as f32 / 8.0).ceil() as usize;
    let row_len = ((params.colors * params.columns * params.bpc as u32) as f32 / 8.0).ceil() as usize;
    if row_len == 0 || bytes_per_pixel == 0 || rows == 0 {
        return None;
    }

    if data.len() == row_len * rows as usize {
        return Some(data);
    }

    if data.len() < (row_len + 1) * rows as usize {
        return None;
    }

    let mut out = vec![0_u8; row_len * rows as usize];
    let mut in_pos = 0_usize;

    for row in 0..rows as usize {
        let filter = data[in_pos];
        in_pos += 1;
        let src = &data[in_pos..in_pos + row_len];
        in_pos += row_len;

        let prev = if row == 0 {
            None
        } else {
            Some(out[(row - 1) * row_len..row * row_len].to_vec())
        };
        let dst_row = &mut out[row * row_len..(row + 1) * row_len];

        match filter {
            0 => dst_row.copy_from_slice(src),
            1 => {
                for i in 0..row_len {
                    let left = if i >= bytes_per_pixel { dst_row[i - bytes_per_pixel] } else { 0 };
                    dst_row[i] = src[i].wrapping_add(left);
                }
            }
            2 => {
                for i in 0..row_len {
                    let up = prev.as_ref().map(|r| r[i]).unwrap_or(0);
                    dst_row[i] = src[i].wrapping_add(up);
                }
            }
            3 => {
                for i in 0..row_len {
                    let left = if i >= bytes_per_pixel { dst_row[i - bytes_per_pixel] } else { 0 };
                    let up = prev.as_ref().map(|r| r[i]).unwrap_or(0);
                    let avg = ((left as u16 + up as u16) / 2) as u8;
                    dst_row[i] = src[i].wrapping_add(avg);
                }
            }
            4 => {
                for i in 0..row_len {
                    let a = if i >= bytes_per_pixel { dst_row[i - bytes_per_pixel] } else { 0 };
                    let b = prev.as_ref().map(|r| r[i]).unwrap_or(0);
                    let c = if i >= bytes_per_pixel {
                        prev.as_ref().map(|r| r[i - bytes_per_pixel]).unwrap_or(0)
                    } else {
                        0
                    };
                    dst_row[i] = src[i].wrapping_add(paeth_predictor(a, b, c));
                }
            }
            _ => return None,
        }
    }

    Some(out)
}

fn paeth_predictor(a: u8, b: u8, c: u8) -> u8 {
    let a = a as i32;
    let b = b as i32;
    let c = c as i32;
    let p = a + b - c;
    let pa = (p - a).abs();
    let pb = (p - b).abs();
    let pc = (p - c).abs();

    if pa <= pb && pa <= pc {
        a as u8
    } else if pb <= pc {
        b as u8
    } else {
        c as u8
    }
}

fn decode_qr_payloads_from_dynamic(image: &DynamicImage) -> Vec<String> {
    let base = normalize_gray_input(&image.to_luma8());
    let primary = decode_qr_payloads(&base);
    if !primary.is_empty() {
        return primary;
    }

    let mut out = HashSet::new();
    for gray in dynamic_gray_candidates(image) {
        for payload in decode_qr_payloads(&gray) {
            out.insert(payload);
        }
        if !out.is_empty() {
            break;
        }
    }

    let mut sorted: Vec<String> = out.into_iter().collect();
    sorted.sort();
    sorted
}

fn dynamic_gray_candidates(image: &DynamicImage) -> Vec<GrayImage> {
    let rgb = image.to_rgb8();
    let (w, h) = rgb.dimensions();

    let red = ImageBuffer::from_fn(w, h, |x, y| Luma([rgb.get_pixel(x, y)[0]]));
    let green = ImageBuffer::from_fn(w, h, |x, y| Luma([rgb.get_pixel(x, y)[1]]));
    let max_ch = ImageBuffer::from_fn(w, h, |x, y| {
        let p = rgb.get_pixel(x, y);
        Luma([p[0].max(p[1]).max(p[2])])
    });

    vec![
        normalize_gray_input(&red),
        normalize_gray_input(&green),
        normalize_gray_input(&max_ch),
    ]
}

fn decode_qr_payloads(gray: &GrayImage) -> Vec<String> {
    let normalized = normalize_gray_input(gray);
    let fast = decode_qr_payloads_with(&normalized, false);
    if !fast.is_empty() {
        return fast;
    }
    decode_qr_payloads_with(&normalized, true)
}

fn decode_qr_payloads_with(gray: &GrayImage, aggressive: bool) -> Vec<String> {
    let mut found = HashSet::new();
    let mut attempts = 0_usize;
    let max_attempts = if aggressive {
        MAX_DECODE_ATTEMPTS_AGGRESSIVE
    } else {
        MAX_DECODE_ATTEMPTS_FAST
    };
    let scales: &[f32] = if aggressive {
        &[1.0, 1.5, 2.0]
    } else {
        &[1.0, 1.5]
    };

    let variants = if aggressive {
        build_gray_variants(gray)
    } else {
        build_fast_gray_variants(gray)
    };

    for base in variants {
        let orientations = oriented_variants(&base, aggressive);

        for oriented in &orientations {
            for &scale in scales {
                let candidate = resize_with_scale(oriented, scale);
                for payload in run_decoders(&candidate, aggressive) {
                    found.insert(payload);
                }
                attempts += 1;
                if !found.is_empty() {
                    break;
                }
                if attempts >= max_attempts {
                    let mut result: Vec<String> = found.into_iter().collect();
                    result.sort();
                    return result;
                }
            }
            if !found.is_empty() {
                break;
            }
        }

        if aggressive && found.is_empty() {
            for oriented in &orientations {
                for crop in crop_variants(oriented) {
                    for tile in tile_variants(&crop) {
                        for payload in run_decoders(&tile, true) {
                            found.insert(payload);
                        }
                        attempts += 1;
                        if !found.is_empty() {
                            break;
                        }
                        if attempts >= max_attempts {
                            let mut result: Vec<String> = found.into_iter().collect();
                            result.sort();
                            return result;
                        }
                    }
                    if !found.is_empty() {
                        break;
                    }
                }
                if !found.is_empty() {
                    break;
                }
            }
        }

        if !found.is_empty() {
            break;
        }
    }

    let mut result: Vec<String> = found.into_iter().collect();
    result.sort();
    result
}

fn run_decoders(gray: &GrayImage, aggressive: bool) -> Vec<String> {
    let mut out = HashSet::new();

    for text in decode_single_pass(gray) {
        out.insert(text);
    }
    if aggressive {
        for text in decode_rqrr_pass(gray) {
            out.insert(text);
        }
    }

    out.into_iter().collect()
}

fn decode_single_pass(gray: &GrayImage) -> Vec<String> {
    let width = gray.width() as usize;
    let height = gray.height() as usize;
    let bytes = gray.as_raw();

    let mut decoder = quircs::Quirc::default();
    let mut result = Vec::new();

    for code in decoder.identify(width, height, bytes) {
        if let Ok(code) = code {
            if let Ok(decoded) = code.decode() {
                result.push(String::from_utf8_lossy(&decoded.payload).to_string());
            }
        }
    }

    result
}

fn decode_rqrr_pass(gray: &GrayImage) -> Vec<String> {
    let mut prepared = rqrr::PreparedImage::prepare(gray.clone());
    let grids = prepared.detect_grids();
    let mut result = Vec::new();

    for grid in grids {
        if let Ok((_meta, content)) = grid.decode() {
            if !content.is_empty() {
                result.push(content);
            }
        }
    }

    result
}

fn build_fast_gray_variants(gray: &GrayImage) -> Vec<GrayImage> {
    let stretched = stretch_contrast(gray);
    let threshold = otsu_threshold(&stretched);
    vec![gray.clone(), stretched.clone(), binarize(&stretched, threshold, false)]
}

fn build_gray_variants(gray: &GrayImage) -> Vec<GrayImage> {
    let stretched = stretch_contrast(gray);
    let threshold = otsu_threshold(&stretched);
    vec![
        gray.clone(),
        stretched.clone(),
        gamma_correct(&stretched, 0.85),
        binarize(&stretched, threshold, false),
        binarize(&stretched, threshold, true),
        adaptive_binarize(&stretched, 24, 7),
        sharpen(&stretched),
    ]
}

fn normalize_gray_input(gray: &GrayImage) -> GrayImage {
    let width = gray.width();
    let height = gray.height();
    let max_dim = width.max(height);
    let area = u64::from(width) * u64::from(height);

    if max_dim <= 2400 && area <= 3_600_000 {
        return gray.clone();
    }

    let scale = (2400.0_f32 / max_dim as f32)
        .min((3_600_000.0_f32 / area as f32).sqrt())
        .clamp(0.2, 1.0);
    resize_with_scale(gray, scale)
}

fn resize_with_scale(gray: &GrayImage, scale: f32) -> GrayImage {
    if (scale - 1.0).abs() < f32::EPSILON {
        return gray.clone();
    }

    let w = ((gray.width() as f32 * scale).round() as u32).clamp(1, 8000);
    let h = ((gray.height() as f32 * scale).round() as u32).clamp(1, 8000);
    image::imageops::resize(gray, w, h, FilterType::Lanczos3)
}

fn oriented_variants(gray: &GrayImage, aggressive: bool) -> Vec<GrayImage> {
    let mut variants = vec![gray.clone()];
    if aggressive {
        variants.push(image::imageops::rotate90(gray));
        variants.push(image::imageops::rotate180(gray));
        variants.push(image::imageops::rotate270(gray));
        variants.push(image::imageops::flip_horizontal(gray));
    }
    variants
}

fn crop_variants(gray: &GrayImage) -> Vec<GrayImage> {
    let mut variants = vec![gray.clone()];
    if gray.width() < 200 || gray.height() < 200 {
        return variants;
    }

    let margin_x = gray.width() / 10;
    let margin_y = gray.height() / 10;
    let inner_w = gray.width().saturating_sub(margin_x * 2);
    let inner_h = gray.height().saturating_sub(margin_y * 2);
    if inner_w > 64 && inner_h > 64 {
        variants.push(image::imageops::crop_imm(gray, margin_x, margin_y, inner_w, inner_h).to_image());
    }

    let half_w = (gray.width() / 2).max(1);
    let half_h = (gray.height() / 2).max(1);
    variants.push(image::imageops::crop_imm(gray, 0, 0, half_w, half_h).to_image());
    variants.push(image::imageops::crop_imm(gray, gray.width() - half_w, 0, half_w, half_h).to_image());
    variants.push(image::imageops::crop_imm(gray, 0, gray.height() - half_h, half_w, half_h).to_image());
    variants.push(image::imageops::crop_imm(gray, gray.width() - half_w, gray.height() - half_h, half_w, half_h).to_image());

    variants
}

fn tile_variants(gray: &GrayImage) -> Vec<GrayImage> {
    let mut variants = vec![gray.clone()];
    if gray.width() < 240 || gray.height() < 240 {
        return variants;
    }

    let tile_w = (gray.width() * 2 / 3).max(64);
    let tile_h = (gray.height() * 2 / 3).max(64);
    let xs = [0, (gray.width().saturating_sub(tile_w)) / 2, gray.width().saturating_sub(tile_w)];
    let ys = [0, (gray.height().saturating_sub(tile_h)) / 2, gray.height().saturating_sub(tile_h)];

    for &x in &xs {
        for &y in &ys {
            variants.push(image::imageops::crop_imm(gray, x, y, tile_w, tile_h).to_image());
        }
    }

    if variants.len() > 5 {
        variants.truncate(5);
    }

    variants
}

fn stretch_contrast(gray: &GrayImage) -> GrayImage {
    let mut min = u8::MAX;
    let mut max = u8::MIN;

    for p in gray.pixels() {
        let v = p[0];
        if v < min {
            min = v;
        }
        if v > max {
            max = v;
        }
    }

    if max <= min {
        return gray.clone();
    }

    let range = u16::from(max - min);
    ImageBuffer::from_fn(gray.width(), gray.height(), |x, y| {
        let v = gray.get_pixel(x, y)[0];
        let scaled = (u16::from(v.saturating_sub(min)) * 255) / range;
        Luma([scaled as u8])
    })
}

fn otsu_threshold(gray: &GrayImage) -> u8 {
    let mut hist = [0_u32; 256];
    for p in gray.pixels() {
        hist[p[0] as usize] += 1;
    }

    let total = (gray.width() * gray.height()) as f64;
    if total == 0.0 {
        return 127;
    }

    let mut sum = 0.0;
    for (i, count) in hist.iter().enumerate() {
        sum += (i as f64) * (*count as f64);
    }

    let mut sum_bg = 0.0;
    let mut weight_bg = 0.0;
    let mut best_threshold = 127_u8;
    let mut best_variance = -1.0_f64;

    for (t, count) in hist.iter().enumerate() {
        weight_bg += *count as f64;
        if weight_bg == 0.0 {
            continue;
        }

        let weight_fg = total - weight_bg;
        if weight_fg == 0.0 {
            break;
        }

        sum_bg += (t as f64) * (*count as f64);
        let mean_bg = sum_bg / weight_bg;
        let mean_fg = (sum - sum_bg) / weight_fg;
        let var_between = weight_bg * weight_fg * (mean_bg - mean_fg).powi(2);

        if var_between > best_variance {
            best_variance = var_between;
            best_threshold = t as u8;
        }
    }

    best_threshold
}

fn binarize(gray: &GrayImage, threshold: u8, invert: bool) -> GrayImage {
    ImageBuffer::from_fn(gray.width(), gray.height(), |x, y| {
        let v = gray.get_pixel(x, y)[0];
        let mut out = if v >= threshold { 255 } else { 0 };
        if invert {
            out = 255 - out;
        }
        Luma([out])
    })
}

fn gamma_correct(gray: &GrayImage, gamma: f32) -> GrayImage {
    if gamma <= 0.01 {
        return gray.clone();
    }

    let inv = 1.0 / gamma;
    ImageBuffer::from_fn(gray.width(), gray.height(), |x, y| {
        let v = gray.get_pixel(x, y)[0] as f32 / 255.0;
        let out = (v.powf(inv) * 255.0).round().clamp(0.0, 255.0) as u8;
        Luma([out])
    })
}

fn sharpen(gray: &GrayImage) -> GrayImage {
    let kernel: [f32; 9] = [0.0, -1.0, 0.0, -1.0, 5.2, -1.0, 0.0, -1.0, 0.0];
    image::imageops::filter3x3(gray, &kernel)
}

fn adaptive_binarize(gray: &GrayImage, radius: u32, bias: i16) -> GrayImage {
    let integral = build_integral(gray);
    let stride = gray.width() as usize + 1;

    ImageBuffer::from_fn(gray.width(), gray.height(), |x, y| {
        let x0 = x.saturating_sub(radius);
        let y0 = y.saturating_sub(radius);
        let x1 = (x + radius).min(gray.width() - 1);
        let y1 = (y + radius).min(gray.height() - 1);

        let area = ((x1 - x0 + 1) * (y1 - y0 + 1)) as u32;
        let sum = rect_sum(&integral, stride, x0, y0, x1, y1);
        let mean = (sum / area) as i16;
        let value = gray.get_pixel(x, y)[0] as i16;
        Luma([if value >= mean - bias { 255 } else { 0 }])
    })
}

fn build_integral(gray: &GrayImage) -> Vec<u32> {
    let width = gray.width() as usize;
    let height = gray.height() as usize;
    let stride = width + 1;
    let mut integral = vec![0_u32; (width + 1) * (height + 1)];

    for y in 0..height {
        let mut row_sum = 0_u32;
        for x in 0..width {
            row_sum += gray.get_pixel(x as u32, y as u32)[0] as u32;
            let idx = (y + 1) * stride + (x + 1);
            integral[idx] = integral[y * stride + (x + 1)] + row_sum;
        }
    }

    integral
}

fn rect_sum(integral: &[u32], stride: usize, x0: u32, y0: u32, x1: u32, y1: u32) -> u32 {
    let x0 = x0 as usize;
    let y0 = y0 as usize;
    let x1 = x1 as usize + 1;
    let y1 = y1 as usize + 1;

    integral[y1 * stride + x1] + integral[y0 * stride + x0]
        - integral[y0 * stride + x1]
        - integral[y1 * stride + x0]
}

fn extension_lower(path: &Path) -> Option<String> {
    path.extension()
        .and_then(OsStr::to_str)
        .map(|s| s.to_ascii_lowercase())
}

fn is_supported_image_ext(ext: &str) -> bool {
    matches!(ext, "png" | "jpg" | "jpeg" | "bmp" | "webp" | "tif" | "tiff")
}

fn is_supported_word_ext(ext: &str) -> bool {
    matches!(ext, "doc" | "docx")
}

fn is_supported_input_path(path: &Path) -> bool {
    matches!(
        extension_lower(path).as_deref(),
        Some("pdf")
            | Some("png")
            | Some("jpg")
            | Some("jpeg")
            | Some("bmp")
            | Some("webp")
            | Some("tif")
            | Some("tiff")
            | Some("doc")
            | Some("docx")
    )
}

fn write_csv(output: &Path, rows: &[QrRow]) -> Result<()> {
    let file = File::create(output)
        .with_context(|| format!("无法创建输出文件: {}", output.display()))?;

    let mut wtr = WriterBuilder::new().from_writer(file);
    wtr.write_record([
        "文件",
        "发票类型标识",
        "发票种类代码",
        "发票代码",
        "发票号码",
        "开票金额",
        "开票日期",
        "发票校验码",
        "加密字符",
    ])?;

    for row in rows {
        wtr.write_record([
            row.file.as_str(),
            row.invoice_type_tag.as_str(),
            row.invoice_kind_code.as_str(),
            row.invoice_code.as_str(),
            row.invoice_number.as_str(),
            row.issue_amount.as_str(),
            row.issue_date.as_str(),
            row.invoice_check_code.as_str(),
            row.encrypted_text.as_str(),
        ])?;
    }

    wtr.flush()?;
    Ok(())
}

fn main() {
    if let Err(err) = run_main() {
        eprintln!("错误: {err}");
        std::process::exit(1);
    }
}

fn run_main() -> Result<()> {
    let (inputs, csv_output, timeout_secs) = parse_args()?;
    let files = expand_inputs(&inputs);
    if files.is_empty() {
        return Err(anyhow!("未找到可识别的 PDF/图片/Word 文件"));
    }

    let rows = run_cli(&files, timeout_secs)?;
    print_rows(&rows);

    if let Some(output) = csv_output {
        write_csv(&output, &rows)?;
        eprintln!("已导出 CSV: {}", output.display());
    }

    Ok(())
}
