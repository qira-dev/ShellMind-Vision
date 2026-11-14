#![allow(clippy::too_many_arguments, clippy::type_complexity)]

use image::GenericImageView;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;

const MAX_SVG_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_IMAGE_FILE_BYTES: u64 = 64 * 1024 * 1024;

fn resolve_ocr_paths(requested_lang: Option<&str>) -> (String, String, String, String) {
    let root = env::var("SHELLMIND_VISION_ROOT")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            env::current_exe().ok().and_then(|exe| {
                exe.parent()
                    .and_then(|bin_dir| bin_dir.parent())
                    .map(|r| r.to_string_lossy().to_string())
            })
        })
        .unwrap_or_else(|| ".".to_string());

    let tess_bin = format!("{}/native_ocr/root/usr/bin/tesseract", root);
    let tess_lib = format!("{}/native_ocr/root/usr/lib/x86_64-linux-gnu", root);
    let tess_data = format!(
        "{}/native_ocr/root/usr/share/tesseract-ocr/5/tessdata",
        root
    );

    let mut installed: Vec<String> = Vec::new();
    if let Ok(entries) = fs::read_dir(&tess_data) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("traineddata")
                && path.metadata().map(|m| m.len() >= 10_000).unwrap_or(false)
                && let Some(stem) = path.file_stem().and_then(|st| st.to_str())
                && stem != "osd"
                && stem != "equ"
            {
                installed.push(stem.to_string());
            }
        }
    }
    installed.sort();

    let lang_combo = if let Some(req) =
        requested_lang.filter(|s| !s.is_empty() && *s != "auto" && *s != "ben+eng")
    {
        let valid: Vec<String> = req
            .split('+')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
            .take(4)
            .map(|s| s.to_string())
            .collect();
        if valid.is_empty() {
            "eng".to_string()
        } else {
            valid.join("+")
        }
    } else {
        // Cap automatic language combo to at most 3 models (prioritizing ben + eng) so `lang add all` never degrades OCR
        let mut chosen = Vec::new();
        if installed.iter().any(|l| l == "ben") {
            chosen.push("ben".to_string());
        }
        for l in &installed {
            if l != "ben" && l != "eng" && chosen.len() < 2 {
                chosen.push(l.clone());
            }
        }
        chosen.push("eng".to_string());
        chosen.join("+")
    };

    (tess_bin, tess_lib, tess_data, lang_combo)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BBox {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub x2: i32,
    pub y2: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WordSpan {
    pub text: String,
    pub bbox: BBox,
    pub center: Point,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OcrBlock {
    pub index: usize,
    pub text: String,
    pub text_raw: String,
    pub confidence: f64,
    pub bbox: BBox,
    pub center: Point,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub words: Vec<WordSpan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub match_bbox: Option<BBox>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub match_center: Option<Point>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiBox {
    pub box_index: usize,
    pub kind: String,
    pub bbox: Value,
    pub center: Point,
    pub label: Option<String>,
}

// ============================================================================
// 1. PURE-RUST DETERMINISTIC SVG STRUCTURAL PARSER
// ============================================================================

/// Finds the closing `>` of an XML/SVG tag while skipping `>` inside single or double quotes.
fn find_tag_end_quoted(s: &str) -> Option<usize> {
    let mut in_single = false;
    let mut in_double = false;
    for (i, ch) in s.char_indices() {
        match ch {
            '"' if !in_single => in_double = !in_double,
            '\'' if !in_double => in_single = !in_single,
            '>' if !in_single && !in_double => return Some(i),
            _ => {}
        }
    }
    None
}

/// Extracts an XML attribute value, tolerating optional whitespace around `=`.
fn extract_attr(tag_str: &str, attr_name: &str) -> Option<String> {
    let bytes = tag_str.as_bytes();
    let n = bytes.len();
    let target = attr_name.as_bytes();
    let tlen = target.len();
    if tlen == 0 || n < tlen + 3 {
        return None;
    }

    let mut i = 0;
    let mut in_quote: Option<u8> = None;
    while i + tlen < n {
        let b = bytes[i];
        if let Some(q) = in_quote {
            if b == q {
                in_quote = None;
            }
            i += 1;
            continue;
        }
        if b == b'"' || b == b'\'' {
            in_quote = Some(b);
            i += 1;
            continue;
        }
        let boundary_ok = if i == 0 {
            true
        } else {
            let prev = bytes[i - 1] as char;
            !prev.is_alphanumeric() && prev != '-' && prev != '_' && prev != ':'
        };
        if boundary_ok && bytes[i..i + tlen].eq_ignore_ascii_case(target) {
            let mut j = i + tlen;
            while j < n
                && (bytes[j] == b' ' || bytes[j] == b'\t' || bytes[j] == b'\n' || bytes[j] == b'\r')
            {
                j += 1;
            }
            if j < n && bytes[j] == b'=' {
                j += 1;
                while j < n
                    && (bytes[j] == b' '
                        || bytes[j] == b'\t'
                        || bytes[j] == b'\n'
                        || bytes[j] == b'\r')
                {
                    j += 1;
                }
                if j < n && (bytes[j] == b'"' || bytes[j] == b'\'') {
                    let q = bytes[j];
                    let start = j + 1;
                    if let Some(end_rel) = tag_str[start..].find(q as char) {
                        return Some(decode_xml_entities(&tag_str[start..start + end_rel]));
                    }
                }
            }
        }
        i += 1;
    }
    None
}

fn decode_xml_entities(input: &str) -> String {
    let mut s = input.to_string();
    if let Some(cdata_start) = s.find("<![CDATA[")
        && let Some(cdata_end) = s[cdata_start + 9..].find("]]>")
    {
        let inner = s[cdata_start + 9..cdata_start + 9 + cdata_end].to_string();
        s.replace_range(cdata_start..cdata_start + 9 + cdata_end + 3, &inner);
    }
    s.replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn parse_f64(s: Option<String>) -> f64 {
    let Some(val) = s else { return 0.0 };
    let trimmed = val.trim();
    let nums = parse_numbers_list(trimmed);
    nums.first().copied().unwrap_or(0.0)
}

fn parse_numbers_list(s: &str) -> Vec<f64> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let n = bytes.len();
    let mut i = 0;

    while i < n {
        let b = bytes[i];
        if b.is_ascii_digit() || b == b'-' || b == b'+' || b == b'.' {
            let start = i;
            if b == b'-' || b == b'+' {
                i += 1;
            }
            let mut has_dot = false;
            while i < n {
                if bytes[i].is_ascii_digit() {
                    i += 1;
                } else if bytes[i] == b'.' && !has_dot {
                    has_dot = true;
                    i += 1;
                } else {
                    break;
                }
            }
            if i < n && (bytes[i] == b'e' || bytes[i] == b'E') {
                i += 1;
                if i < n && (bytes[i] == b'+' || bytes[i] == b'-') {
                    i += 1;
                }
                while i < n && bytes[i].is_ascii_digit() {
                    i += 1;
                }
            }
            if let Ok(v) = s[start..i].parse::<f64>()
                && v.is_finite()
            {
                out.push(v);
            }
        } else {
            i += 1;
        }
    }
    out
}

#[derive(Clone, Copy, Debug)]
struct Affine2D {
    a: f64,
    b: f64,
    c: f64,
    d: f64,
    e: f64,
    f: f64,
}

impl Affine2D {
    fn identity() -> Self {
        Self {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: 0.0,
            f: 0.0,
        }
    }

    fn multiply(self, rhs: Self) -> Self {
        Self {
            a: self.a * rhs.a + self.c * rhs.b,
            b: self.b * rhs.a + self.d * rhs.b,
            c: self.a * rhs.c + self.c * rhs.d,
            d: self.b * rhs.c + self.d * rhs.d,
            e: self.a * rhs.e + self.c * rhs.f + self.e,
            f: self.b * rhs.e + self.d * rhs.f + self.f,
        }
    }

    fn apply(self, x: f64, y: f64) -> (f64, f64) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }

    fn apply_rect(self, x: f64, y: f64, w: f64, h: f64) -> (f64, f64, f64, f64) {
        let corners = [
            self.apply(x, y),
            self.apply(x + w, y),
            self.apply(x, y + h),
            self.apply(x + w, y + h),
        ];
        let min_x = corners.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
        let max_x = corners
            .iter()
            .map(|p| p.0)
            .fold(f64::NEG_INFINITY, f64::max);
        let min_y = corners.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
        let max_y = corners
            .iter()
            .map(|p| p.1)
            .fold(f64::NEG_INFINITY, f64::max);
        (
            min_x,
            min_y,
            (max_x - min_x).max(0.0),
            (max_y - min_y).max(0.0),
        )
    }
}

fn parse_svg_transform(raw: Option<String>) -> Affine2D {
    let Some(s) = raw else {
        return Affine2D::identity();
    };
    let mut acc = Affine2D::identity();
    let mut rest = s.as_str();
    while let Some(open) = rest.find('(') {
        let name = rest[..open].trim().to_ascii_lowercase();
        let func_name = name.split_whitespace().last().unwrap_or("");
        let Some(close_rel) = rest[open + 1..].find(')') else {
            break;
        };
        let args = parse_numbers_list(&rest[open + 1..open + 1 + close_rel]);
        let m = match func_name {
            "translate" => {
                let tx = args.first().copied().unwrap_or(0.0);
                let ty = args.get(1).copied().unwrap_or(0.0);
                Affine2D {
                    a: 1.0,
                    b: 0.0,
                    c: 0.0,
                    d: 1.0,
                    e: tx,
                    f: ty,
                }
            }
            "scale" => {
                let sx = args.first().copied().unwrap_or(1.0);
                let sy = args.get(1).copied().unwrap_or(sx);
                Affine2D {
                    a: sx,
                    b: 0.0,
                    c: 0.0,
                    d: sy,
                    e: 0.0,
                    f: 0.0,
                }
            }
            "matrix" if args.len() >= 6 => Affine2D {
                a: args[0],
                b: args[1],
                c: args[2],
                d: args[3],
                e: args[4],
                f: args[5],
            },
            _ => Affine2D::identity(),
        };
        acc = acc.multiply(m);
        rest = &rest[open + 1 + close_rel + 1..];
    }
    acc
}

fn round2(v: f64) -> f64 {
    if !v.is_finite() {
        0.0
    } else {
        (v * 100.0).round() / 100.0
    }
}

fn bbox_json(x: f64, y: f64, w: f64, h: f64) -> Value {
    let w = w.max(0.0);
    let h = h.max(0.0);
    json!({
        "x": round2(x),
        "y": round2(y),
        "width": round2(w),
        "height": round2(h),
        "center_x": round2(x + w / 2.0),
        "center_y": round2(y + h / 2.0),
    })
}

fn compute_path_bbox(d_str: &str, tf: Affine2D) -> Option<Value> {
    let mut min_x = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    let mut cur_x = 0.0;
    let mut cur_y = 0.0;
    let mut start_x = 0.0;
    let mut start_y = 0.0;
    let mut cmd = 'M';

    let mut include_pt = |x: f64, y: f64| {
        let (tx, ty) = tf.apply(x, y);
        if tx < min_x {
            min_x = tx;
        }
        if tx > max_x {
            max_x = tx;
        }
        if ty < min_y {
            min_y = ty;
        }
        if ty > max_y {
            max_y = ty;
        }
    };

    // Tokenize path commands and numbers
    let mut tokens: Vec<Result<char, f64>> = Vec::new();
    let bytes = d_str.as_bytes();
    let n = bytes.len();
    let mut i = 0;
    while i < n {
        let c = bytes[i] as char;
        if c.is_ascii_alphabetic() && c != 'e' && c != 'E' {
            tokens.push(Ok(c));
            i += 1;
        } else if c.is_ascii_digit() || c == '-' || c == '+' || c == '.' {
            let start = i;
            if c == '-' || c == '+' {
                i += 1;
            }
            let mut has_dot = false;
            while i < n {
                let b = bytes[i];
                if b.is_ascii_digit() {
                    i += 1;
                } else if b == b'.' && !has_dot {
                    has_dot = true;
                    i += 1;
                } else {
                    break;
                }
            }
            if i < n && (bytes[i] == b'e' || bytes[i] == b'E') {
                i += 1;
                if i < n && (bytes[i] == b'+' || bytes[i] == b'-') {
                    i += 1;
                }
                while i < n && bytes[i].is_ascii_digit() {
                    i += 1;
                }
            }
            if let Ok(v) = d_str[start..i].parse::<f64>()
                && v.is_finite()
            {
                tokens.push(Err(v));
            }
        } else {
            i += 1;
        }
    }

    let mut idx = 0;
    let next_num = |tokens: &[Result<char, f64>], idx: &mut usize| -> Option<f64> {
        if let Some(Err(v)) = tokens.get(*idx) {
            *idx += 1;
            Some(*v)
        } else {
            None
        }
    };

    while idx < tokens.len() {
        if let Ok(c) = tokens[idx] {
            cmd = c;
            idx += 1;
            if cmd == 'Z' || cmd == 'z' {
                cur_x = start_x;
                cur_y = start_y;
                continue;
            }
        }
        match cmd {
            'M' | 'L' | 'T' => {
                let Some(x) = next_num(&tokens, &mut idx) else {
                    break;
                };
                let Some(y) = next_num(&tokens, &mut idx) else {
                    break;
                };
                cur_x = x;
                cur_y = y;
                if cmd == 'M' {
                    start_x = cur_x;
                    start_y = cur_y;
                    cmd = 'L';
                }
                include_pt(cur_x, cur_y);
            }
            'm' | 'l' | 't' => {
                let Some(dx) = next_num(&tokens, &mut idx) else {
                    break;
                };
                let Some(dy) = next_num(&tokens, &mut idx) else {
                    break;
                };
                cur_x += dx;
                cur_y += dy;
                if cmd == 'm' {
                    start_x = cur_x;
                    start_y = cur_y;
                    cmd = 'l';
                }
                include_pt(cur_x, cur_y);
            }
            'H' => {
                let Some(x) = next_num(&tokens, &mut idx) else {
                    break;
                };
                cur_x = x;
                include_pt(cur_x, cur_y);
            }
            'h' => {
                let Some(dx) = next_num(&tokens, &mut idx) else {
                    break;
                };
                cur_x += dx;
                include_pt(cur_x, cur_y);
            }
            'V' => {
                let Some(y) = next_num(&tokens, &mut idx) else {
                    break;
                };
                cur_y = y;
                include_pt(cur_x, cur_y);
            }
            'v' => {
                let Some(dy) = next_num(&tokens, &mut idx) else {
                    break;
                };
                cur_y += dy;
                include_pt(cur_x, cur_y);
            }
            'C' => {
                let (Some(x1), Some(y1), Some(x2), Some(y2), Some(x), Some(y)) = (
                    next_num(&tokens, &mut idx),
                    next_num(&tokens, &mut idx),
                    next_num(&tokens, &mut idx),
                    next_num(&tokens, &mut idx),
                    next_num(&tokens, &mut idx),
                    next_num(&tokens, &mut idx),
                ) else {
                    break;
                };
                include_pt(x1, y1);
                include_pt(x2, y2);
                include_pt(x, y);
                cur_x = x;
                cur_y = y;
            }
            'c' => {
                let (Some(dx1), Some(dy1), Some(dx2), Some(dy2), Some(dx), Some(dy)) = (
                    next_num(&tokens, &mut idx),
                    next_num(&tokens, &mut idx),
                    next_num(&tokens, &mut idx),
                    next_num(&tokens, &mut idx),
                    next_num(&tokens, &mut idx),
                    next_num(&tokens, &mut idx),
                ) else {
                    break;
                };
                include_pt(cur_x + dx1, cur_y + dy1);
                include_pt(cur_x + dx2, cur_y + dy2);
                cur_x += dx;
                cur_y += dy;
                include_pt(cur_x, cur_y);
            }
            'S' | 'Q' => {
                let (Some(x1), Some(y1), Some(x), Some(y)) = (
                    next_num(&tokens, &mut idx),
                    next_num(&tokens, &mut idx),
                    next_num(&tokens, &mut idx),
                    next_num(&tokens, &mut idx),
                ) else {
                    break;
                };
                include_pt(x1, y1);
                include_pt(x, y);
                cur_x = x;
                cur_y = y;
            }
            's' | 'q' => {
                let (Some(dx1), Some(dy1), Some(dx), Some(dy)) = (
                    next_num(&tokens, &mut idx),
                    next_num(&tokens, &mut idx),
                    next_num(&tokens, &mut idx),
                    next_num(&tokens, &mut idx),
                ) else {
                    break;
                };
                include_pt(cur_x + dx1, cur_y + dy1);
                cur_x += dx;
                cur_y += dy;
                include_pt(cur_x, cur_y);
            }
            'A' => {
                let (Some(_rx), Some(_ry), Some(_rot), Some(_laf), Some(_sf), Some(x), Some(y)) = (
                    next_num(&tokens, &mut idx),
                    next_num(&tokens, &mut idx),
                    next_num(&tokens, &mut idx),
                    next_num(&tokens, &mut idx),
                    next_num(&tokens, &mut idx),
                    next_num(&tokens, &mut idx),
                    next_num(&tokens, &mut idx),
                ) else {
                    break;
                };
                cur_x = x;
                cur_y = y;
                include_pt(cur_x, cur_y);
            }
            'a' => {
                let (Some(_rx), Some(_ry), Some(_rot), Some(_laf), Some(_sf), Some(dx), Some(dy)) = (
                    next_num(&tokens, &mut idx),
                    next_num(&tokens, &mut idx),
                    next_num(&tokens, &mut idx),
                    next_num(&tokens, &mut idx),
                    next_num(&tokens, &mut idx),
                    next_num(&tokens, &mut idx),
                    next_num(&tokens, &mut idx),
                ) else {
                    break;
                };
                cur_x += dx;
                cur_y += dy;
                include_pt(cur_x, cur_y);
            }
            _ => break,
        }
    }

    if !min_x.is_finite() || !min_y.is_finite() {
        return None;
    }
    Some(bbox_json(min_x, min_y, max_x - min_x, max_y - min_y))
}

fn strip_xml_tags(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    let mut last_was_space = false;
    for ch in s.chars() {
        if ch == '<' {
            if !in_tag && !out.is_empty() && !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
            in_tag = true;
        } else if ch == '>' {
            in_tag = false;
        } else if !in_tag {
            out.push(ch);
            last_was_space = ch.is_whitespace();
        }
    }
    decode_xml_entities(out.trim())
}

pub fn inspect_svg(file_path_or_xml: &str, include_paths: bool, max_elements: usize) -> Value {
    let trimmed = file_path_or_xml.trim_start();
    let (raw_xml, source_label) = if trimmed == "-" || trimmed == "stdin" {
        let mut buf = String::new();
        if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
            return json!({ "ok": false, "error": format!("Failed to read SVG from stdin: {}", e) });
        }
        (buf, "<stdin_svg>".to_string())
    } else if trimmed.starts_with('<') {
        (trimmed.to_string(), "<inline_svg>".to_string())
    } else {
        let p = Path::new(file_path_or_xml);
        let Ok(meta) = fs::metadata(p) else {
            return json!({ "ok": false, "error": format!("SVG file not found: {}", file_path_or_xml) });
        };
        if !meta.is_file() || meta.len() > MAX_SVG_FILE_BYTES {
            return json!({ "ok": false, "error": format!("Refusing non-regular or oversized SVG file (>32 MiB): {}", file_path_or_xml) });
        }
        match fs::read_to_string(p) {
            Ok(c) => (c, p.display().to_string()),
            Err(e) => return json!({ "ok": false, "error": format!("Failed to read SVG: {}", e) }),
        }
    };

    if !raw_xml.contains("<svg") && !raw_xml.contains("<SVG") {
        return json!({ "ok": false, "error": "Invalid SVG content: missing <svg> root element" });
    }

    let mut view_box = String::new();
    let mut width = String::new();
    let mut height = String::new();
    let mut root_svg_seen = false;
    let mut texts: Vec<Value> = Vec::new();
    let mut elements: Vec<Value> = Vec::new();
    let mut defs_ids: Vec<String> = Vec::new();
    let mut defs_bboxes: HashMap<String, (f64, f64, f64, f64)> = HashMap::new();

    let mut pos = 0;
    let len = raw_xml.len();
    let mut skip_depth: usize = 0;
    let mut tf_stack: Vec<(String, Affine2D)> = vec![("root".into(), Affine2D::identity())];
    let mut parent_text_xy: Option<(f64, f64)> = None;

    while pos < len {
        let Some(lt_rel) = raw_xml[pos..].find('<') else {
            break;
        };
        let lt = pos + lt_rel;
        if lt + 1 >= len {
            break;
        }
        if raw_xml[lt..].starts_with("<!--") {
            if let Some(end_c) = raw_xml[lt + 4..].find("-->") {
                pos = lt + 4 + end_c + 3;
                continue;
            } else {
                break;
            }
        }
        if raw_xml[lt..].starts_with("<![CDATA[") {
            if let Some(end_cd) = raw_xml[lt + 9..].find("]]>") {
                pos = lt + 9 + end_cd + 3;
                continue;
            } else {
                break;
            }
        }
        let Some(gt_rel) = find_tag_end_quoted(&raw_xml[lt..]) else {
            break;
        };
        let gt = lt + gt_rel;
        let tag_header = &raw_xml[lt + 1..gt];
        pos = gt + 1;

        let tag_trimmed = tag_header.trim();
        if tag_trimmed.is_empty() || tag_trimmed.starts_with('?') || tag_trimmed.starts_with('!') {
            continue;
        }
        if let Some(stripped) = tag_trimmed.strip_prefix('/') {
            let close_raw = stripped.split_whitespace().next().unwrap_or("");
            let close_name = close_raw
                .rsplit(':')
                .next()
                .unwrap_or(close_raw)
                .to_lowercase();
            if matches!(
                close_name.as_str(),
                "defs"
                    | "symbol"
                    | "clippath"
                    | "mask"
                    | "marker"
                    | "pattern"
                    | "lineargradient"
                    | "radialgradient"
            ) {
                skip_depth = skip_depth.saturating_sub(1);
            }
            if close_name == "text" {
                parent_text_xy = None;
            }
            if tf_stack.len() > 1
                && tf_stack
                    .last()
                    .map(|(n, _)| n == &close_name)
                    .unwrap_or(false)
            {
                tf_stack.pop();
            }
            continue;
        }

        let is_self_closing = tag_trimmed.ends_with('/');
        let raw_name = tag_trimmed
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or("");
        let tag_name = raw_name
            .rsplit(':')
            .next()
            .unwrap_or(raw_name)
            .to_lowercase();

        let cur_tf = tf_stack
            .last()
            .map(|(_, t)| *t)
            .unwrap_or_else(Affine2D::identity);
        let local_tf = parse_svg_transform(extract_attr(tag_trimmed, "transform"));
        let combined_tf = cur_tf.multiply(local_tf);

        if !is_self_closing && matches!(tag_name.as_str(), "g" | "svg" | "a") {
            tf_stack.push((tag_name.clone(), combined_tf));
        }

        if tag_name == "svg" {
            if !root_svg_seen {
                root_svg_seen = true;
                view_box = extract_attr(tag_trimmed, "viewBox")
                    .or_else(|| extract_attr(tag_trimmed, "viewbox"))
                    .unwrap_or_default();
                width = extract_attr(tag_trimmed, "width").unwrap_or_default();
                height = extract_attr(tag_trimmed, "height").unwrap_or_default();
            }
            continue;
        }

        if matches!(
            tag_name.as_str(),
            "defs"
                | "symbol"
                | "clippath"
                | "mask"
                | "marker"
                | "pattern"
                | "lineargradient"
                | "radialgradient"
        ) {
            if let Some(id) = extract_attr(tag_trimmed, "id") {
                defs_ids.push(format!("{}#{}", tag_name, id));
                let w = parse_f64(extract_attr(tag_trimmed, "width"));
                let h = parse_f64(extract_attr(tag_trimmed, "height"));
                if w > 0.0 || h > 0.0 {
                    defs_bboxes.insert(id, (0.0, 0.0, w, h));
                }
            }
            if !is_self_closing {
                skip_depth += 1;
            }
            continue;
        }

        if skip_depth > 0 {
            if let Some(id) = extract_attr(tag_trimmed, "id") {
                defs_ids.push(format!("{}#{}", tag_name, id));
                let x = parse_f64(extract_attr(tag_trimmed, "x"));
                let y = parse_f64(extract_attr(tag_trimmed, "y"));
                let w = parse_f64(extract_attr(tag_trimmed, "width"));
                let h = parse_f64(extract_attr(tag_trimmed, "height"));
                if w > 0.0 || h > 0.0 {
                    defs_bboxes.insert(id, (x, y, w, h));
                }
            }
            continue;
        }

        if tag_name == "text" || tag_name == "tspan" || tag_name == "textpath" {
            if !is_self_closing {
                let close_pat = format!("</{}", raw_name);
                if let Some(close_rel) = raw_xml[pos..].find(&close_pat) {
                    let inner = &raw_xml[pos..pos + close_rel];
                    let has_child_tspan = tag_name == "text"
                        && (inner.contains("<tspan") || inner.contains("<TSPAN"));
                    let raw_x = extract_attr(tag_trimmed, "x");
                    let raw_y = extract_attr(tag_trimmed, "y");
                    let base_x = raw_x
                        .as_ref()
                        .map(|v| parse_f64(Some(v.clone())))
                        .or_else(|| parent_text_xy.map(|p| p.0))
                        .unwrap_or(0.0);
                    let base_y = raw_y
                        .as_ref()
                        .map(|v| parse_f64(Some(v.clone())))
                        .or_else(|| parent_text_xy.map(|p| p.1))
                        .unwrap_or(0.0);
                    if tag_name == "text" {
                        parent_text_xy = Some((base_x, base_y));
                    }
                    if !has_child_tspan {
                        let clean_txt = strip_xml_tags(inner);
                        if !clean_txt.is_empty() {
                            let (tx, ty) = combined_tf.apply(base_x, base_y);
                            texts.push(json!({
                                "index": texts.len(),
                                "tag": tag_name,
                                "text": clean_txt,
                                "id": extract_attr(tag_trimmed, "id"),
                                "class": extract_attr(tag_trimmed, "class"),
                                "x": round2(tx),
                                "y": round2(ty),
                                "font_size": extract_attr(tag_trimmed, "font-size"),
                                "fill": extract_attr(tag_trimmed, "fill"),
                                "transform": extract_attr(tag_trimmed, "transform"),
                            }));
                        }
                        if tag_name == "text" {
                            pos += close_rel + close_pat.len();
                            parent_text_xy = None;
                        }
                    }
                }
            }
            continue;
        }

        let is_shape = matches!(
            tag_name.as_str(),
            "rect"
                | "circle"
                | "ellipse"
                | "line"
                | "polyline"
                | "polygon"
                | "path"
                | "image"
                | "use"
                | "a"
                | "g"
                | "button"
                | "foreignobject"
        );
        if is_shape && elements.len() < max_elements {
            let elem_id = extract_attr(tag_trimmed, "id");
            let elem_class = extract_attr(tag_trimmed, "class");
            let aria_label = extract_attr(tag_trimmed, "aria-label")
                .or_else(|| extract_attr(tag_trimmed, "name"));
            let mut title_text = extract_attr(tag_trimmed, "title");

            if !is_self_closing {
                let close_pat = format!("</{}", raw_name);
                if let Some(close_rel) = raw_xml[pos..].find(&close_pat) {
                    let inner = &raw_xml[pos..pos + close_rel];
                    if let Some(t_start) = inner.find("<title>")
                        && let Some(t_end) = inner[t_start + 7..].find("</title>")
                    {
                        title_text = Some(decode_xml_entities(
                            inner[t_start + 7..t_start + 7 + t_end].trim(),
                        ));
                    }
                }
            }

            if tag_name == "g"
                && elem_id.is_none()
                && elem_class.is_none()
                && aria_label.is_none()
                && title_text.is_none()
            {
                continue;
            }
            if tag_name == "path"
                && !include_paths
                && elem_id.is_none()
                && elem_class.is_none()
                && aria_label.is_none()
            {
                continue;
            }

            let bbox = match tag_name.as_str() {
                "rect" | "image" | "foreignobject" => {
                    let x = parse_f64(extract_attr(tag_trimmed, "x"));
                    let y = parse_f64(extract_attr(tag_trimmed, "y"));
                    let w = parse_f64(extract_attr(tag_trimmed, "width"));
                    let h = parse_f64(extract_attr(tag_trimmed, "height"));
                    let (tx, ty, tw, th) = combined_tf.apply_rect(x, y, w, h);
                    Some(bbox_json(tx, ty, tw, th))
                }
                "use" => {
                    let x = parse_f64(extract_attr(tag_trimmed, "x"));
                    let y = parse_f64(extract_attr(tag_trimmed, "y"));
                    let mut w = parse_f64(extract_attr(tag_trimmed, "width"));
                    let mut h = parse_f64(extract_attr(tag_trimmed, "height"));
                    if w == 0.0
                        && h == 0.0
                        && let Some(href) = extract_attr(tag_trimmed, "href")
                            .or_else(|| extract_attr(tag_trimmed, "xlink:href"))
                    {
                        let ref_id = href.trim_start_matches('#');
                        if let Some(&(_, _, dw, dh)) = defs_bboxes.get(ref_id) {
                            w = dw;
                            h = dh;
                        }
                    }
                    let (tx, ty, tw, th) = combined_tf.apply_rect(x, y, w, h);
                    Some(bbox_json(tx, ty, tw, th))
                }
                "circle" => {
                    let cx = parse_f64(extract_attr(tag_trimmed, "cx"));
                    let cy = parse_f64(extract_attr(tag_trimmed, "cy"));
                    let r = parse_f64(extract_attr(tag_trimmed, "r"));
                    let (tx, ty, tw, th) = combined_tf.apply_rect(cx - r, cy - r, r * 2.0, r * 2.0);
                    Some(bbox_json(tx, ty, tw, th))
                }
                "ellipse" => {
                    let cx = parse_f64(extract_attr(tag_trimmed, "cx"));
                    let cy = parse_f64(extract_attr(tag_trimmed, "cy"));
                    let rx = parse_f64(extract_attr(tag_trimmed, "rx"));
                    let ry = parse_f64(extract_attr(tag_trimmed, "ry"));
                    let (tx, ty, tw, th) =
                        combined_tf.apply_rect(cx - rx, cy - ry, rx * 2.0, ry * 2.0);
                    Some(bbox_json(tx, ty, tw, th))
                }
                "line" => {
                    let x1 = parse_f64(extract_attr(tag_trimmed, "x1"));
                    let y1 = parse_f64(extract_attr(tag_trimmed, "y1"));
                    let x2 = parse_f64(extract_attr(tag_trimmed, "x2"));
                    let y2 = parse_f64(extract_attr(tag_trimmed, "y2"));
                    let (tx1, ty1) = combined_tf.apply(x1, y1);
                    let (tx2, ty2) = combined_tf.apply(x2, y2);
                    let min_x = tx1.min(tx2);
                    let min_y = ty1.min(ty2);
                    let w = (tx1 - tx2).abs();
                    let h = (ty1 - ty2).abs();
                    Some(bbox_json(min_x, min_y, w, h))
                }
                "path" | "polygon" | "polyline" => {
                    let d = extract_attr(tag_trimmed, "d")
                        .or_else(|| extract_attr(tag_trimmed, "points"))
                        .unwrap_or_default();
                    compute_path_bbox(&d, combined_tf)
                }
                _ => None,
            };

            let mut obj = serde_json::Map::new();
            obj.insert("index".to_string(), json!(elements.len()));
            obj.insert("tag".to_string(), json!(tag_name));
            if let Some(v) = elem_id {
                obj.insert("id".to_string(), json!(v));
            }
            if let Some(v) = elem_class {
                obj.insert("class".to_string(), json!(v));
            }
            if let Some(v) = aria_label {
                obj.insert("label".to_string(), json!(v));
            }
            if let Some(v) = title_text {
                obj.insert("title".to_string(), json!(v));
            }
            if let Some(v) = extract_attr(tag_trimmed, "fill") {
                obj.insert("fill".to_string(), json!(v));
            }
            if let Some(v) = extract_attr(tag_trimmed, "stroke") {
                obj.insert("stroke".to_string(), json!(v));
            }
            if let Some(v) = bbox {
                obj.insert("bbox".to_string(), v);
            }
            elements.push(Value::Object(obj));
        }
    }

    let all_text_concat: String = texts
        .iter()
        .filter_map(|t| t.get("text").and_then(|v| v.as_str()))
        .collect::<Vec<_>>()
        .join(" | ");

    json!({
        "ok": true,
        "engine": "desktop-vision-rs (Pure Rust)",
        "format": "SVG",
        "source": source_label,
        "viewBox": view_box,
        "width": width,
        "height": height,
        "summary": {
            "text_count": texts.len(),
            "element_count": elements.len(),
            "defs_count": defs_ids.len(),
            "all_text_concat": all_text_concat,
        },
        "texts": texts,
        "elements": elements,
        "defs": defs_ids,
    })
}

// ============================================================================
// 2. PURE-RUST UI CONTOUR & COMPONENT BOX DETECTOR
// ============================================================================

pub fn detect_ui_boxes_rust(
    gray: &[u8],
    w: usize,
    h: usize,
    ocr_blocks: &[OcrBlock],
    max_boxes: usize,
) -> Vec<UiBox> {
    if w < 16 || h < 16 {
        return Vec::new();
    }
    let scale_factor = ((h as f64) / 1080.0).clamp(0.75, 2.5);
    let cw = w.div_ceil(4);
    let ch = h.div_ceil(4);
    let mut edge_grid = vec![0u8; cw * ch];

    for y in 1..(h - 1) {
        let row = y * w;
        let gy = y / 4;
        for x in 1..(w - 1) {
            let gx = (gray[row + x + 1] as i32 - gray[row + x - 1] as i32).abs();
            let gy_val = (gray[row + w + x] as i32 - gray[row - w + x] as i32).abs();
            if gx + gy_val > 42 {
                edge_grid[gy * cw + (x / 4)] = 1;
            }
        }
    }

    let mut visited = vec![false; cw * ch];
    let mut raw_boxes: Vec<(i32, i32, i32, i32, i32)> = Vec::new();
    let max_area = ((w * h) as f64 * 0.88) as i32;

    let mut queue = Vec::with_capacity(1024);
    for gy in 0..ch {
        for gx in 0..cw {
            let idx = gy * cw + gx;
            if edge_grid[idx] == 0 || visited[idx] {
                continue;
            }
            visited[idx] = true;
            queue.clear();
            queue.push((gx, gy));
            let mut min_gx = gx;
            let mut max_gx = gx;
            let mut min_gy = gy;
            let mut max_gy = gy;
            let mut head = 0;

            while head < queue.len() {
                let (cx, cy) = queue[head];
                head += 1;
                if cx < min_gx {
                    min_gx = cx;
                }
                if cx > max_gx {
                    max_gx = cx;
                }
                if cy < min_gy {
                    min_gy = cy;
                }
                if cy > max_gy {
                    max_gy = cy;
                }

                let y_start = cy.saturating_sub(1);
                let y_end = (cy + 1).min(ch - 1);
                let x_start = cx.saturating_sub(2);
                let x_end = (cx + 2).min(cw - 1);

                for ny in y_start..=y_end {
                    for nx in x_start..=x_end {
                        let nidx = ny * cw + nx;
                        if edge_grid[nidx] == 1 && !visited[nidx] {
                            visited[nidx] = true;
                            queue.push((nx, ny));
                        }
                    }
                }
            }

            let bx = (min_gx * 4) as i32;
            let by = (min_gy * 4) as i32;
            let bw = (((max_gx - min_gx + 1) * 4) as i32)
                .min(w as i32 - bx)
                .max(0);
            let bh = (((max_gy - min_gy + 1) * 4) as i32)
                .min(h as i32 - by)
                .max(0);
            let area = bw * bh;

            if bw >= 16 && bh >= 12 && area >= 220 && area <= max_area {
                let aspect = bw as f64 / (bh as f64);
                if (0.08..=38.0).contains(&aspect) {
                    raw_boxes.push((bx, by, bw, bh, area));
                }
            }
        }
    }

    for ob in ocr_blocks {
        let bx = (ob.bbox.x - 4).max(0);
        let by = (ob.bbox.y - 2).max(0);
        let bw = (ob.bbox.width + 8).min(w as i32 - bx).max(0);
        let bh = (ob.bbox.height + 4).min(h as i32 - by).max(0);
        let area = bw * bh;
        if bw >= 16 && bh >= 12 && area >= 200 {
            raw_boxes.push((bx, by, bw, bh, area));
        }
    }

    raw_boxes.sort_by_key(|b| (b.1 / 12, b.0));

    let mut deduped: Vec<(i32, i32, i32, i32, i32)> = Vec::new();
    for b in raw_boxes {
        let (x1, y1, w1, h1, a1) = b;
        let mut is_dup = false;
        for &(dx, dy, dw, dh, da) in &deduped {
            let ix1 = x1.max(dx);
            let iy1 = y1.max(dy);
            let ix2 = (x1 + w1).min(dx + dw);
            let iy2 = (y1 + h1).min(dy + dh);
            if ix2 > ix1 && iy2 > iy1 {
                let inter = (ix2 - ix1) * (iy2 - iy1);
                let union = a1 + da - inter;
                if union > 0 && (inter as f64 / union as f64) > 0.78 {
                    is_dup = true;
                    break;
                }
            }
        }
        if !is_dup {
            deduped.push(b);
            if deduped.len() >= max_boxes {
                break;
            }
        }
    }

    let max_btn_h = (64.0 * scale_factor).round() as i32;
    let max_btn_w = (550.0 * scale_factor).round() as i32;
    let max_icon = (52.0 * scale_factor).round() as i32;
    let max_input_h = (54.0 * scale_factor).round() as i32;

    let mut ui_boxes = Vec::with_capacity(deduped.len());
    for (idx, (x, y, bw, bh, _)) in deduped.into_iter().enumerate() {
        let cx = x + bw / 2;
        let cy = y + bh / 2;
        let mut labels = Vec::new();
        for ob in ocr_blocks {
            if ob.center.x >= x
                && ob.center.x <= x + bw
                && ob.center.y >= y
                && ob.center.y <= y + bh
            {
                labels.push(ob.text.clone());
            }
        }
        let kind = if bh <= max_btn_h && bw >= 36 && bw <= max_btn_w && !labels.is_empty() {
            "button_or_input"
        } else if bw >= 14 && bw <= max_icon && bh >= 14 && bh <= max_icon && (bw - bh).abs() <= 14
        {
            "icon_or_checkbox"
        } else if bh <= max_input_h && bw >= 110 && labels.is_empty() {
            "input_field"
        } else {
            "container"
        };

        ui_boxes.push(UiBox {
            box_index: idx,
            kind: kind.to_string(),
            bbox: json!({ "x": x, "y": y, "width": bw, "height": bh }),
            center: Point { x: cx, y: cy },
            label: if labels.is_empty() {
                None
            } else {
                Some(labels.join(" "))
            },
        });
    }
    ui_boxes
}

// ============================================================================
// 3. PURE-RUST IMAGE PREPROCESSING & SEPARABLE BOX BLUR
// ============================================================================

fn preprocess_crop_to_pgm(
    gray: &[u8],
    img_w: usize,
    img_h: usize,
    x0: usize,
    y0: usize,
    crop_w: usize,
    crop_h: usize,
    scale: f64,
    is_dark_mode: bool,
    gamma: f64,
) -> Vec<u8> {
    if crop_w == 0 || crop_h == 0 || img_w == 0 || img_h == 0 {
        return Vec::new();
    }
    let mut lut = [0u8; 256];
    if (gamma - 1.0).abs() < 0.01 {
        for (i, item) in lut.iter_mut().enumerate() {
            *item = if is_dark_mode {
                255 - (i as u8)
            } else {
                i as u8
            };
        }
    } else {
        let mut min_v = 255u8;
        let mut max_v = 0u8;
        for y in y0..(y0 + crop_h).min(img_h) {
            let row = y * img_w;
            for x in x0..(x0 + crop_w).min(img_w) {
                let v = gray[row + x];
                if v < min_v {
                    min_v = v;
                }
                if v > max_v {
                    max_v = v;
                }
            }
        }
        let range = (max_v.saturating_sub(min_v)).max(1) as f64;
        for (i, item) in lut.iter_mut().enumerate() {
            let norm = ((i as u8).saturating_sub(min_v) as f64 / range).clamp(0.0, 1.0);
            let lifted = norm.powf(gamma) * 255.0;
            let v = if is_dark_mode {
                (255.0 - lifted).round().clamp(0.0, 255.0) as u8
            } else {
                lifted.round().clamp(0.0, 255.0) as u8
            };
            *item = v;
        }
    }

    let out_w = (((crop_w as f64) * scale).round() as usize).max(1);
    let out_h = (((crop_h as f64) * scale).round() as usize).max(1);
    let header = format!("P5\n{} {}\n255\n", out_w, out_h);
    let mut pgm = Vec::with_capacity(header.len() + out_w * out_h);
    pgm.extend_from_slice(header.as_bytes());

    for oy in 0..out_h {
        let sy = (oy as f64) / scale;
        let y_low = (sy.floor() as usize).min(crop_h - 1);
        let y_high = (y_low + 1).min(crop_h - 1);
        let wy = sy - (y_low as f64);

        let r_low = (y0 + y_low) * img_w;
        let r_high = (y0 + y_high) * img_w;

        for ox in 0..out_w {
            let sx = (ox as f64) / scale;
            let x_low = (sx.floor() as usize).min(crop_w - 1);
            let x_high = (x_low + 1).min(crop_w - 1);
            let wx = sx - (x_low as f64);

            let p00 = lut[gray[r_low + x0 + x_low] as usize] as f64;
            let p10 = lut[gray[r_low + x0 + x_high] as usize] as f64;
            let p01 = lut[gray[r_high + x0 + x_low] as usize] as f64;
            let p11 = lut[gray[r_high + x0 + x_high] as usize] as f64;

            let top = p00 * (1.0 - wx) + p10 * wx;
            let bot = p01 * (1.0 - wx) + p11 * wx;
            let val = (top * (1.0 - wy) + bot * wy).round().clamp(0.0, 255.0) as u8;
            pgm.push(val);
        }
    }
    // Fast Pure-Rust 3x3 Unsharp Mask (Laplacian Sharpening) for crisp tiny fonts & monospace glyphs
    let hdr_len = header.len();
    if out_w >= 4 && out_h >= 4 && pgm.len() == hdr_len + out_w * out_h {
        let src_px = pgm[hdr_len..].to_vec();
        let dst_px = &mut pgm[hdr_len..];
        for y in 1..(out_h - 1) {
            let row = y * out_w;
            for x in 1..(out_w - 1) {
                let c = src_px[row + x] as i32;
                let n = src_px[row - out_w + x] as i32;
                let s = src_px[row + out_w + x] as i32;
                let l = src_px[row + x - 1] as i32;
                let r = src_px[row + x + 1] as i32;
                let lap = 4 * c - (n + s + l + r);
                dst_px[row + x] = (c + (lap * 3) / 8).clamp(0, 255) as u8;
            }
        }
    }
    pgm
}

fn fast_box_blur_u8(src: &[u8], w: usize, h: usize, radius: usize) -> Vec<u8> {
    if radius == 0 || w <= 1 || h <= 1 {
        return src.to_vec();
    }
    let radius = radius.min(w.saturating_sub(1)).min(h.saturating_sub(1));
    if radius == 0 {
        return src.to_vec();
    }
    let mut temp = vec![0u8; w * h];
    let mut dst = vec![0u8; w * h];
    let win_size = (2 * radius + 1) as u32;

    for y in 0..h {
        let row_offset = y * w;
        let mut sum: u32 = (src[row_offset] as u32) * (radius as u32);
        for x in 0..=radius {
            sum += src[row_offset + x.min(w - 1)] as u32;
        }
        for x in 0..w {
            temp[row_offset + x] = (sum / win_size) as u8;
            let left_out = src[row_offset + x.saturating_sub(radius)] as u32;
            let right_in = src[row_offset + (x + radius + 1).min(w - 1)] as u32;
            sum = sum + right_in - left_out;
        }
    }

    for x in 0..w {
        let mut sum: u32 = (temp[x] as u32) * (radius as u32);
        for y in 0..=radius {
            sum += temp[y.min(h - 1) * w + x] as u32;
        }
        for y in 0..h {
            dst[y * w + x] = (sum / win_size) as u8;
            let top_out = temp[y.saturating_sub(radius) * w + x] as u32;
            let bot_in = temp[(y + radius + 1).min(h - 1) * w + x] as u32;
            sum = sum + bot_in - top_out;
        }
    }
    dst
}

fn preprocess_crop_adaptive_to_pgm(
    gray: &[u8],
    img_w: usize,
    img_h: usize,
    x0: usize,
    y0: usize,
    crop_w: usize,
    crop_h: usize,
    scale: f64,
) -> Vec<u8> {
    if crop_w == 0 || crop_h == 0 || img_w == 0 || img_h == 0 {
        return Vec::new();
    }
    let mut cropped = vec![0u8; crop_w * crop_h];
    for y in 0..crop_h {
        let sy = (y0 + y).min(img_h - 1);
        let src_row = sy * img_w;
        let dst_row = y * crop_w;
        for x in 0..crop_w {
            let sx = (x0 + x).min(img_w - 1);
            cropped[dst_row + x] = gray[src_row + sx];
        }
    }

    let blurred = fast_box_blur_u8(&cropped, crop_w, crop_h, 16);
    let mut local_enhanced = vec![255u8; crop_w * crop_h];
    for i in 0..(crop_w * crop_h) {
        let diff = (cropped[i] as i32 - blurred[i] as i32).abs();
        local_enhanced[i] = (255 - (diff * 5)).clamp(0, 255) as u8;
    }

    let out_w = (((crop_w as f64) * scale).round() as usize).max(1);
    let out_h = (((crop_h as f64) * scale).round() as usize).max(1);
    let header = format!("P5\n{} {}\n255\n", out_w, out_h);
    let mut pgm = Vec::with_capacity(header.len() + out_w * out_h);
    pgm.extend_from_slice(header.as_bytes());

    for oy in 0..out_h {
        let sy = (oy as f64) / scale;
        let y_low = (sy.floor() as usize).min(crop_h - 1);
        let y_high = (y_low + 1).min(crop_h - 1);
        let wy = sy - (y_low as f64);

        let r_low = y_low * crop_w;
        let r_high = y_high * crop_w;

        for ox in 0..out_w {
            let sx = (ox as f64) / scale;
            let x_low = (sx.floor() as usize).min(crop_w - 1);
            let x_high = (x_low + 1).min(crop_w - 1);
            let wx = sx - (x_low as f64);

            let p00 = local_enhanced[r_low + x_low] as f64;
            let p10 = local_enhanced[r_low + x_high] as f64;
            let p01 = local_enhanced[r_high + x_low] as f64;
            let p11 = local_enhanced[r_high + x_high] as f64;

            let top = p00 * (1.0 - wx) + p10 * wx;
            let bot = p01 * (1.0 - wx) + p11 * wx;
            let val = (top * (1.0 - wy) + bot * wy).round().clamp(0.0, 255.0) as u8;
            pgm.push(val);
        }
    }
    pgm
}

// ============================================================================
// 4. UNIVERSAL UNICODE & SCRIPT-AWARE NORMALIZER
// ============================================================================

fn is_bengali_letter(c: char) -> bool {
    let u = c as u32;
    (0x0980..=0x09E5).contains(&u) || (0x09F0..=0x09FF).contains(&u)
}

fn normalize_context_digits(s: &str) -> String {
    let has_bn_letters = s.chars().any(is_bengali_letter);
    let has_ascii_alpha = s.chars().any(|c| c.is_ascii_alphabetic());

    if has_ascii_alpha && !has_bn_letters {
        return s
            .chars()
            .map(|c| match c {
                '০' => '0',
                '১' => '1',
                '২' => '2',
                '৩' => '3',
                '৪' => '4',
                '৫' => '5',
                '৬' => '6',
                '৭' => '7',
                '৮' => '8',
                '৯' => '9',
                '।' => '.',
                _ => c,
            })
            .collect();
    }

    let mut out = Vec::new();
    let chars: Vec<char> = s.chars().collect();
    for i in 0..chars.len() {
        let c = chars[i];
        let prev_ascii = i > 0
            && (chars[i - 1].is_ascii_alphanumeric() || chars[i - 1] == '.' || chars[i - 1] == ':');
        let next_ascii = i + 1 < chars.len()
            && (chars[i + 1].is_ascii_alphanumeric() || chars[i + 1] == '.' || chars[i + 1] == ':');
        if prev_ascii || next_ascii {
            let mapped = match c {
                '০' => '0',
                '১' => '1',
                '২' => '2',
                '৩' => '3',
                '৪' => '4',
                '৫' => '5',
                '৬' => '6',
                '৭' => '7',
                '৮' => '8',
                '৯' => '9',
                _ => c,
            };
            out.push(mapped);
        } else {
            out.push(c);
        }
    }
    out.into_iter().collect()
}

fn normalize_bengali_unicode(input: &str) -> String {
    input.replace("ড\u{09bc}", "ড়").replace("ঢ\u{09bc}", "ঢ়")
}

fn normalize_for_search(input: &str) -> String {
    input
        .to_lowercase()
        .replace("ড\u{09bc}", "ড়")
        .replace("ঢ\u{09bc}", "ঢ়")
        .replace("য\u{09bc}", "য়")
}

fn normalize_ocr_line(raw: &str, optimize_cli_paths: bool) -> String {
    let digit_fixed = normalize_context_digits(raw.trim());
    let mut s = normalize_bengali_unicode(&digit_fixed);
    if optimize_cli_paths {
        let rules = [
            ("Jopt/", "/opt/"),
            ("Jusr/", "/usr/"),
            ("Jbin/", "/bin/"),
            ("Jtmp/", "/tmp/"),
            ("Jsnap/", "/snap/"),
            ("Jvar/", "/var/"),
            ("Jetc/", "/etc/"),
            ("Jhome/", "/home/"),
            ("Jdev/", "/dev/"),
        ];
        for (pat, rep) in &rules {
            if s.contains(pat) {
                s = s.replace(pat, rep);
            }
        }
    }
    s
}

/// Computes exact word-span bounding box and center when `query_norm` matches a sub-span of `block.words`.
fn compute_word_span_match(block: &OcrBlock, query_norm: &str) -> (BBox, Point) {
    if block.words.is_empty() || query_norm.is_empty() {
        return (block.bbox.clone(), block.center.clone());
    }
    let n = block.words.len();
    for start in 0..n {
        let mut combined = String::new();
        let mut min_x = i32::MAX;
        let mut min_y = i32::MAX;
        let mut max_x = i32::MIN;
        let mut max_y = i32::MIN;
        for end in start..n {
            if !combined.is_empty() {
                combined.push(' ');
            }
            let w = &block.words[end];
            combined.push_str(&normalize_for_search(&w.text));
            min_x = min_x.min(w.bbox.x);
            min_y = min_y.min(w.bbox.y);
            max_x = max_x.max(w.bbox.x2);
            max_y = max_y.max(w.bbox.y2);

            if combined.contains(query_norm) {
                let mut s_idx = start;
                while s_idx < end {
                    let narrower: String = block.words[s_idx + 1..=end]
                        .iter()
                        .map(|item| normalize_for_search(&item.text))
                        .collect::<Vec<_>>()
                        .join(" ");
                    if narrower.contains(query_norm) {
                        s_idx += 1;
                    } else {
                        break;
                    }
                }
                let mut nx1 = i32::MAX;
                let mut ny1 = i32::MAX;
                let mut nx2 = i32::MIN;
                let mut ny2 = i32::MIN;
                for item in &block.words[s_idx..=end] {
                    nx1 = nx1.min(item.bbox.x);
                    ny1 = ny1.min(item.bbox.y);
                    nx2 = nx2.max(item.bbox.x2);
                    ny2 = ny2.max(item.bbox.y2);
                }
                let bw = (nx2 - nx1).max(1);
                let bh = (ny2 - ny1).max(1);
                return (
                    BBox {
                        x: nx1,
                        y: ny1,
                        width: bw,
                        height: bh,
                        x2: nx2,
                        y2: ny2,
                    },
                    Point {
                        x: nx1 + bw / 2,
                        y: ny1 + bh / 2,
                    },
                );
            }
        }
    }
    (block.bbox.clone(), block.center.clone())
}

fn run_tesseract_tsv_region(
    region_id: usize,
    x_offset: i32,
    y_offset: i32,
    scale: f64,
    pgm_bytes: Vec<u8>,
    lang: &str,
    psm: &str,
    optimize_cli_paths: bool,
) -> Result<Vec<OcrBlock>, String> {
    let (tess_bin, tess_lib, tess_data, dyn_lang) = resolve_ocr_paths(Some(lang));
    let effective_lang = if lang == "auto" || lang == "ben+eng" {
        dyn_lang.as_str()
    } else {
        lang
    };
    let run_once = |bin: &str, use_bundled_lib: bool| -> Result<std::process::Output, String> {
        let mut cmd = Command::new("/usr/bin/nice");
        cmd.args([
            "-n",
            "15",
            bin,
            "stdin",
            "stdout",
            "-l",
            effective_lang,
            "--oem",
            "1",
            "--psm",
            psm,
            "tsv",
        ])
        .env("TESSDATA_PREFIX", &tess_data)
        .env("OMP_THREAD_LIMIT", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        if use_bundled_lib {
            cmd.env("LD_LIBRARY_PATH", &tess_lib);
        } else {
            cmd.env_remove("LD_LIBRARY_PATH");
        }
        let mut child = cmd.spawn().map_err(|e| format!("spawn {}: {}", bin, e))?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(&pgm_bytes);
        }
        let out = child
            .wait_with_output()
            .map_err(|e| format!("wait {}: {}", bin, e))?;
        if out.status.success() {
            Ok(out)
        } else {
            let err_txt = String::from_utf8_lossy(&out.stderr);
            Err(format!(
                "{} exited {}: {}",
                bin,
                out.status,
                err_txt.chars().take(300).collect::<String>().trim()
            ))
        }
    };

    let output = match run_once(&tess_bin, true) {
        Ok(o) => o,
        Err(e1) => match run_once("tesseract", false) {
            Ok(o2) => o2,
            Err(e2) => return Err(format!("bundled ({}) & system ({})", e1, e2)),
        },
    };

    let tsv = String::from_utf8_lossy(&output.stdout);
    let mut segment_trackers: BTreeMap<(usize, i32, i32, i32), (usize, i32)> = BTreeMap::new();
    let mut lines_map: BTreeMap<
        (usize, i32, i32, i32, usize),
        (Vec<String>, Vec<f64>, Vec<WordSpan>, i32, i32, i32, i32),
    > = BTreeMap::new();

    for line in tsv.lines().skip(1) {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 12 {
            continue;
        }
        let level = parts[0].parse::<i32>().unwrap_or(0);
        if level != 5 {
            continue;
        }
        let word = parts[11].trim();
        let conf = parts[10].parse::<f64>().unwrap_or(-1.0);
        if word.is_empty() || conf < 28.0 {
            continue;
        }
        let block_num = parts[2].parse::<i32>().unwrap_or(0);
        let par_num = parts[3].parse::<i32>().unwrap_or(0);
        let line_num = parts[4].parse::<i32>().unwrap_or(0);

        let left = ((parts[6].parse::<f64>().unwrap_or(0.0) / scale).round() as i32) + x_offset;
        let top = ((parts[7].parse::<f64>().unwrap_or(0.0) / scale).round() as i32) + y_offset;
        let w = ((parts[8].parse::<f64>().unwrap_or(1.0) / scale).round() as i32).max(1);
        let h = ((parts[9].parse::<f64>().unwrap_or(1.0) / scale).round() as i32).max(1);

        let line_key = (region_id, block_num, par_num, line_num);
        let tracker = segment_trackers.entry(line_key).or_insert((0, left));
        let gap = left - tracker.1;
        if gap > 30 && gap > 2 * h {
            tracker.0 += 1;
        }
        let seg_id = tracker.0;
        tracker.1 = left + w;

        let full_key = (region_id, block_num, par_num, line_num, seg_id);
        let entry = lines_map.entry(full_key).or_insert_with(|| {
            (
                Vec::new(),
                Vec::new(),
                Vec::new(),
                left,
                top,
                left + w,
                top + h,
            )
        });
        entry.0.push(word.to_string());
        entry.1.push(conf);
        entry.2.push(WordSpan {
            text: word.to_string(),
            bbox: BBox {
                x: left,
                y: top,
                width: w,
                height: h,
                x2: left + w,
                y2: top + h,
            },
            center: Point {
                x: left + w / 2,
                y: top + h / 2,
            },
        });
        if left < entry.3 {
            entry.3 = left;
        }
        if top < entry.4 {
            entry.4 = top;
        }
        if left + w > entry.5 {
            entry.5 = left + w;
        }
        if top + h > entry.6 {
            entry.6 = top + h;
        }
    }

    let mut blocks = Vec::new();
    for (_key, (mut words, mut confs, mut word_spans, x1, y1, x2, y2)) in lines_map {
        let is_list_marker = words
            .first()
            .map(|w| {
                let s = w.as_str();
                s.ends_with('.') || s.ends_with(')') || s == "-" || s == "•"
            })
            .unwrap_or(false);
        if words.len() >= 2
            && !is_list_marker
            && confs[0] < 46.0
            && confs[1] >= 65.0
            && words[0].chars().count() <= 3
        {
            words.remove(0);
            confs.remove(0);
            if !word_spans.is_empty() {
                word_spans.remove(0);
            }
        }
        let raw_joined = words.join(" ");
        let joined = normalize_ocr_line(&raw_joined, optimize_cli_paths);
        let alnum_chars: Vec<char> = joined
            .chars()
            .filter(|c| c.is_alphanumeric() || is_bengali_letter(*c))
            .collect();
        let alnum_count = alnum_chars.len();
        if alnum_count == 0 {
            continue;
        }
        let avg_conf =
            (confs.iter().sum::<f64>() / (confs.len() as f64) / 100.0).clamp(0.01, 0.9999);

        if avg_conf < 0.42 {
            let all_same = alnum_chars.len() >= 2
                && alnum_chars
                    .iter()
                    .all(|&c| c.eq_ignore_ascii_case(&alnum_chars[0]));
            if all_same || alnum_count <= 2 {
                continue;
            }
        }

        let bw = (x2 - x1).max(1);
        let bh = (y2 - y1).max(1);
        blocks.push(OcrBlock {
            index: 0,
            text: joined,
            text_raw: raw_joined,
            confidence: (avg_conf * 10000.0).round() / 10000.0,
            bbox: BBox {
                x: x1,
                y: y1,
                width: bw,
                height: bh,
                x2,
                y2,
            },
            center: Point {
                x: x1 + bw / 2,
                y: y1 + bh / 2,
            },
            words: word_spans,
            match_bbox: None,
            match_center: None,
        });
    }
    Ok(blocks)
}

fn bbox_overlap(b1: &BBox, b2: &BBox) -> f64 {
    let x_left = b1.x.max(b2.x);
    let y_top = b1.y.max(b2.y);
    let x_right = b1.x2.min(b2.x2);
    let y_bottom = b1.y2.min(b2.y2);

    if x_right <= x_left || y_bottom <= y_top {
        return 0.0;
    }

    let inter_area = ((x_right - x_left) * (y_bottom - y_top)) as f64;
    let area1 = (b1.width * b1.height).max(1) as f64;
    let area2 = (b2.width * b2.height).max(1) as f64;
    inter_area / area1.min(area2)
}

pub fn analyze_screen_semantics_dynamic(
    ocr_blocks: &[OcrBlock],
    ui_boxes: &[UiBox],
    w: usize,
    h: usize,
) -> Value {
    let mut header = Vec::new();
    let mut sidebar = Vec::new();
    let mut timeline = Vec::new();
    let mut composer = Vec::new();
    let mut dock = Vec::new();

    let sidebar_x_limit = ((w as f64) * 0.25) as i32;
    let header_y_limit = 58;
    let dock_y_limit = (h as i32) - 46;
    let composer_y_limit = (h as i32) - 130;

    for b in ocr_blocks {
        let cx = b.center.x;
        let cy = b.center.y;
        if cy < header_y_limit {
            header.push(b);
        } else if cy >= dock_y_limit {
            dock.push(b);
        } else if cx < sidebar_x_limit {
            sidebar.push(b);
        } else if cy >= composer_y_limit {
            composer.push(b);
        } else {
            timeline.push(b);
        }
    }

    let window_title = header
        .iter()
        .filter(|b| b.center.y >= 22 && b.text.len() > 8)
        .max_by_key(|b| b.text.len())
        .map(|b| b.text.trim())
        .or_else(|| {
            header
                .iter()
                .max_by_key(|b| b.text.len())
                .map(|b| b.text.trim())
        })
        .unwrap_or("Active Desktop Window")
        .to_string();

    json!({
        "ok": true,
        "engine": "desktop-vision-rs",
        "window_title": window_title,
        "total_ocr_blocks": ocr_blocks.len(),
        "total_ui_boxes": ui_boxes.len(),
        "regions_detected": {
            "header_items": header.len(),
            "sidebar_items": sidebar.len(),
            "timeline_steps": timeline.len(),
            "composer_controls": composer.len(),
            "dock_apps": dock.len(),
        }
    })
}

// ============================================================================
// 5. FAST 4-CORE OVERLAPPING HORIZONTAL BAND + ADAPTIVE CONTRAST ORCHESTRATOR
// ============================================================================

pub fn inspect_image_rust(
    file_path: &str,
    run_ocr: bool,
    detect_ui_boxes: bool,
    query_text: Option<String>,
    optimize_cli_paths: bool,
) -> Value {
    let is_stdin = file_path == "-" || file_path == "stdin";
    let p = Path::new(file_path);
    if !is_stdin {
        let Ok(meta) = fs::metadata(p) else {
            return json!({ "ok": false, "error": format!("File not found: {}", file_path) });
        };
        if !meta.is_file() || meta.len() > MAX_IMAGE_FILE_BYTES {
            return json!({ "ok": false, "error": format!("Refusing non-regular or oversized image file (>64 MiB): {}", file_path) });
        }
        if p.extension()
            .and_then(|e| e.to_str())
            .map(|s| s.eq_ignore_ascii_case("svg"))
            .unwrap_or(false)
        {
            return inspect_svg(file_path, true, 500);
        }
    }

    let dyn_img = if is_stdin {
        let mut buf = Vec::new();
        if let Err(e) = std::io::stdin().read_to_end(&mut buf) {
            return json!({ "ok": false, "error": format!("Failed to read image from stdin: {}", e) });
        }
        match image::load_from_memory(&buf) {
            Ok(img) => img,
            Err(e) => {
                return json!({ "ok": false, "error": format!("Failed to decode image from memory: {}", e) });
            }
        }
    } else {
        match image::open(p) {
            Ok(img) => img,
            Err(e) => {
                return json!({ "ok": false, "error": format!("Failed to open image: {}", e) });
            }
        }
    };

    let (w_u32, h_u32) = dyn_img.dimensions();
    let w = w_u32 as usize;
    let h = h_u32 as usize;
    let gray_img = dyn_img.to_luma8();
    let gray = gray_img.as_raw();

    let sum_luma: u64 = gray.iter().map(|&v| v as u64).sum();
    let mean_luma = (sum_luma as f64) / ((w * h).max(1) as f64);
    let is_dark_mode = mean_luma < 125.0;

    let mut ocr_blocks: Vec<OcrBlock> = Vec::new();
    let mut pass_errors: Vec<String> = Vec::new();

    if run_ocr {
        let base_scale = if w <= 1024 {
            1.68
        } else if w <= 1440 {
            1.42
        } else {
            1.22
        };

        let left_w = (((w as f64) * 0.58).round() as usize).max(1);
        let right_x = ((w as f64) * 0.22).round() as usize;
        let right_w = w.saturating_sub(right_x).max(1);
        let g_normal = if is_dark_mode { 0.58 } else { 1.0 };

        let prepared: Vec<(usize, i32, i32, f64, Vec<u8>, &'static str, &'static str)> = vec![
            (
                0,
                0,
                0,
                base_scale,
                preprocess_crop_to_pgm(gray, w, h, 0, 0, w, h, base_scale, is_dark_mode, g_normal),
                "ben+eng",
                "11",
            ),
            (
                1,
                0,
                0,
                base_scale * 1.04,
                preprocess_crop_to_pgm(
                    gray,
                    w,
                    h,
                    0,
                    0,
                    left_w.min(w),
                    h,
                    base_scale * 1.04,
                    is_dark_mode,
                    1.0,
                ),
                "ben+eng",
                "6",
            ),
            (
                2,
                right_x as i32,
                0,
                base_scale * 1.04,
                preprocess_crop_to_pgm(
                    gray,
                    w,
                    h,
                    right_x,
                    0,
                    right_w.min(w),
                    h,
                    base_scale * 1.04,
                    is_dark_mode,
                    g_normal,
                ),
                "ben+eng",
                "6",
            ),
            (
                3,
                0,
                0,
                base_scale * 1.06,
                preprocess_crop_adaptive_to_pgm(gray, w, h, 0, 0, w, h, base_scale * 1.06),
                "eng",
                "11",
            ),
        ];

        let mut iter = prepared.into_iter();
        loop {
            let mut wave = Vec::with_capacity(2);
            if let Some(item1) = iter.next() {
                wave.push(item1);
            }
            if let Some(item2) = iter.next() {
                wave.push(item2);
            }
            if wave.is_empty() {
                break;
            }
            let wave_results: Vec<Result<Vec<OcrBlock>, String>> = thread::scope(|s| {
                let mut handles = Vec::new();
                for (rid, rx, ry, sc, pgm, lang, psm) in wave {
                    handles.push(s.spawn(move || {
                        run_tesseract_tsv_region(
                            rid,
                            rx,
                            ry,
                            sc,
                            pgm,
                            lang,
                            psm,
                            optimize_cli_paths,
                        )
                    }));
                }
                handles
                    .into_iter()
                    .map(|h| {
                        h.join()
                            .unwrap_or_else(|_| Err("OCR worker thread panicked".into()))
                    })
                    .collect()
            });
            for res in wave_results {
                match res {
                    Ok(rlist) => ocr_blocks.extend(rlist),
                    Err(e) => pass_errors.push(e),
                }
            }
        }

        if pass_errors.len() == 4 && ocr_blocks.is_empty() {
            return json!({
                "ok": false,
                "error_code": "OCR_ENGINE_FAILED",
                "error": format!("All 4 OCR passes failed: {}", pass_errors.join(" | ")),
            });
        }

        ocr_blocks.sort_by_key(|b| (b.bbox.y / 12, b.bbox.x));
        let mut deduped_ocr: Vec<OcrBlock> = Vec::with_capacity(ocr_blocks.len());
        for b in ocr_blocks {
            let mut replaced = false;
            let mut dup = false;
            for d in &mut deduped_ocr {
                let overlap = bbox_overlap(&b.bbox, &d.bbox);
                let dy = (b.center.y - d.center.y).abs();
                let dx = (b.center.x - d.center.x).abs();

                if overlap >= 0.55 || (dy <= 12 && dx <= 28) {
                    let b_bn = b.text.chars().any(is_bengali_letter) && b.confidence >= 0.60;
                    let d_bn = d.text.chars().any(is_bengali_letter) && d.confidence >= 0.60;
                    if b_bn && !d_bn {
                        *d = b.clone();
                        replaced = true;
                        break;
                    } else if d_bn && !b_bn {
                        dup = true;
                        break;
                    }
                    if b.text == d.text {
                        if b.confidence > d.confidence {
                            *d = b.clone();
                        }
                        dup = true;
                        break;
                    } else if b.text.contains(&d.text) && b.text.len() > d.text.len() {
                        let char_diff = b
                            .text
                            .chars()
                            .count()
                            .saturating_sub(d.text.chars().count());
                        if char_diff <= 3
                            && d.confidence > b.confidence + 0.15
                            && d.text.chars().count() >= 4
                        {
                            dup = true;
                            break;
                        }
                        *d = b.clone();
                        replaced = true;
                        break;
                    } else if d.text.contains(&b.text) {
                        let char_diff = d
                            .text
                            .chars()
                            .count()
                            .saturating_sub(b.text.chars().count());
                        if char_diff <= 3
                            && b.confidence > d.confidence + 0.15
                            && b.text.chars().count() >= 4
                        {
                            *d = b.clone();
                            replaced = true;
                            break;
                        }
                        dup = true;
                        break;
                    } else if overlap >= 0.70 {
                        if b.text.len() > d.text.len() || b.confidence > d.confidence {
                            *d = b.clone();
                            replaced = true;
                            break;
                        } else {
                            dup = true;
                            break;
                        }
                    }
                }
            }
            if !dup && !replaced {
                deduped_ocr.push(b);
            }
        }
        for (i, b) in deduped_ocr.iter_mut().enumerate() {
            b.index = i;
        }
        ocr_blocks = deduped_ocr;
    }

    let ui_boxes = if detect_ui_boxes {
        detect_ui_boxes_rust(gray, w, h, &ocr_blocks, 80)
    } else {
        Vec::new()
    };

    let matches: Option<Vec<OcrBlock>> = query_text.as_ref().map(|q| {
        let ql = normalize_for_search(q);
        ocr_blocks
            .iter()
            .filter(|b| {
                normalize_for_search(&b.text).contains(&ql)
                    || normalize_for_search(&b.text_raw).contains(&ql)
            })
            .map(|b| {
                let mut cloned = b.clone();
                let (mb, mc) = compute_word_span_match(&cloned, &ql);
                cloned.match_bbox = Some(mb);
                cloned.match_center = Some(mc);
                cloned
            })
            .collect()
    });

    let full_text = ocr_blocks
        .iter()
        .map(|b| b.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    let semantics = analyze_screen_semantics_dynamic(&ocr_blocks, &ui_boxes, w, h);

    let fmt_str = p
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("PNG")
        .to_uppercase();

    json!({
        "ok": true,
        "engine": "desktop-vision-rs (Pure Rust + Native C++ AVX2)",
        "source": if is_stdin { "<in_memory_stream>".to_string() } else { p.display().to_string() },
        "format": fmt_str,
        "mode": if is_dark_mode { "RGB (Dark-Mode Auto-Enhanced)" } else { "RGB" },
        "dimensions": { "width": w, "height": h },
        "metadata": {
            "ocr_passes_failed": pass_errors.len(),
        },
        "summary": {
            "ocr_block_count": ocr_blocks.len(),
            "ui_box_count": ui_boxes.len(),
            "match_count": matches.as_ref().map(|m| m.len()),
            "found": matches.as_ref().map(|m| !m.is_empty()),
            "full_text": full_text,
        },
        "semantics": semantics,
        "matches": matches,
        "ocr_blocks": ocr_blocks,
        "ui_boxes": ui_boxes,
    })
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        println!(
            "{}",
            json!({ "ok": false, "error": "Usage: desktop-vision-rs <inspect_image|inspect_svg|find_text> <target> [json_opts]" })
        );
        std::process::exit(1);
    }

    let cmd = &args[1];
    let target = &args[2];
    let opts: Value = if args.len() > 3 {
        serde_json::from_str(&args[3]).unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };

    let res = match cmd.as_str() {
        "inspect_svg" => {
            let inc_paths = opts
                .get("include_paths")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let max_el = opts
                .get("max_elements")
                .and_then(|v| v.as_u64())
                .unwrap_or(500) as usize;
            inspect_svg(target, inc_paths, max_el)
        }
        "inspect_image" => {
            let run_ocr = opts
                .get("run_ocr")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let det_boxes = opts
                .get("detect_ui_boxes")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let opt_paths = opts
                .get("optimize_for_cli_paths")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let q = opts
                .get("query_text")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            inspect_image_rust(target, run_ocr, det_boxes, q, opt_paths)
        }
        "find_text" => {
            let q = opts
                .get("query_text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let opt_paths = opts
                .get("optimize_for_cli_paths")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            inspect_image_rust(target, true, false, Some(q), opt_paths)
        }
        _ => json!({ "ok": false, "error": format!("Unknown command: {}", cmd) }),
    };

    println!(
        "{}",
        serde_json::to_string(&res).unwrap_or_else(|_| "{\"ok\":false}".to_string())
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_svg_transforms_defs_tspans_entities_and_paths() {
        let svg = r##"
        <svg viewBox="0 0 200 100" width = "200" height = "100">
          <defs id="d1" />
          <defs><rect id="btn_tpl" width="40" height="20"/></defs>
          <g transform="translate(100, 50)">
            <rect id="r1" x = "10" y = "10" width="30" height="20" title="Click &amp; Go"/>
            <text x="5" y="15"><tspan x="10" y="20">Line1</tspan><tspan x="10" y="40">Line2</tspan></text>
          </g>
          <path id="p1" d="M 10 10 h 50 v 20 z" />
          <use id="u1" href="#btn_tpl" x="15" y="25" />
        </svg>
        "##;
        let res = inspect_svg(svg, true, 500);
        assert_eq!(res["ok"], true);
        assert_eq!(res["width"], "200");

        let texts = res["texts"].as_array().unwrap();
        assert_eq!(
            texts.len(),
            2,
            "Each tspan must be emitted separately: {:?}",
            texts
        );
        assert_eq!(texts[0]["text"], "Line1");
        assert_eq!(texts[0]["x"], 110.0);
        assert_eq!(texts[0]["y"], 70.0);
        assert_eq!(texts[1]["text"], "Line2");
        assert_eq!(texts[1]["y"], 90.0);

        let elems = res["elements"].as_array().unwrap();
        let r1 = elems.iter().find(|e| e["id"] == "r1").unwrap();
        assert_eq!(r1["title"], "Click & Go");
        assert_eq!(r1["bbox"]["x"], 110.0);
        assert_eq!(r1["bbox"]["y"], 60.0);

        let p1 = elems.iter().find(|e| e["id"] == "p1").unwrap();
        assert_eq!(p1["bbox"]["width"], 50.0);
        assert_eq!(p1["bbox"]["height"], 20.0);

        let u1 = elems.iter().find(|e| e["id"] == "u1").unwrap();
        assert_eq!(u1["bbox"]["width"], 40.0);
        assert_eq!(u1["bbox"]["height"], 20.0);
    }

    #[test]
    fn test_word_span_match_center_targets_queried_word() {
        let block = OcrBlock {
            index: 0,
            text: "Cancel Submit Help".into(),
            text_raw: "Cancel Submit Help".into(),
            confidence: 0.95,
            bbox: BBox {
                x: 10,
                y: 10,
                width: 300,
                height: 20,
                x2: 310,
                y2: 30,
            },
            center: Point { x: 160, y: 20 },
            words: vec![
                WordSpan {
                    text: "Cancel".into(),
                    bbox: BBox {
                        x: 10,
                        y: 10,
                        width: 60,
                        height: 20,
                        x2: 70,
                        y2: 30,
                    },
                    center: Point { x: 40, y: 20 },
                },
                WordSpan {
                    text: "Submit".into(),
                    bbox: BBox {
                        x: 120,
                        y: 10,
                        width: 60,
                        height: 20,
                        x2: 180,
                        y2: 30,
                    },
                    center: Point { x: 150, y: 20 },
                },
                WordSpan {
                    text: "Help".into(),
                    bbox: BBox {
                        x: 250,
                        y: 10,
                        width: 60,
                        height: 20,
                        x2: 310,
                        y2: 30,
                    },
                    center: Point { x: 280, y: 20 },
                },
            ],
            match_bbox: None,
            match_center: None,
        };
        let (mb, mc) = compute_word_span_match(&block, "submit");
        assert_eq!(mb.x, 120);
        assert_eq!(mb.width, 60);
        assert_eq!(mc.x, 150);
    }
}
