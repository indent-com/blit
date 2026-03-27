use std::collections::BTreeSet;

pub fn font_dirs() -> Vec<String> {
    let mut dirs = Vec::new();
    // BLIT_FONT_DIRS: colon-separated list of extra directories to search first
    if let Ok(extra) = std::env::var("BLIT_FONT_DIRS") {
        for d in extra.split(':') {
            let d = d.trim();
            if !d.is_empty() {
                dirs.push(d.to_owned());
            }
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy();
        dirs.push(format!("{home}/Library/Fonts"));
        dirs.push(format!("{home}/.local/share/fonts"));
        dirs.push(format!("{home}/.fonts"));
    }
    dirs.push("/Library/Fonts".into());
    dirs.push("/System/Library/Fonts".into());
    dirs.push("/usr/share/fonts".into());
    dirs.push("/usr/local/share/fonts".into());
    dirs
}

#[derive(Debug, Clone)]
pub struct FontInfo {
    pub family: String,
    pub subfamily: String,
}

#[derive(Debug, Clone)]
pub struct FontVariant {
    pub path: String,
    pub weight: String,
    pub style: String,
}

/// Read font family and subfamily from a TTF/OTF/TTC file's `name` table.
fn read_font_info(data: &[u8]) -> Option<FontInfo> {
    if data.len() < 12 {
        return None;
    }

    let offset = if &data[0..4] == b"ttcf" {
        if data.len() < 16 {
            return None;
        }
        u32::from_be_bytes([data[12], data[13], data[14], data[15]]) as usize
    } else {
        0
    };

    if offset + 12 > data.len() {
        return None;
    }
    let num_tables = u16::from_be_bytes([data[offset + 4], data[offset + 5]]) as usize;
    if offset + 12 + num_tables * 16 > data.len() {
        return None;
    }

    let mut name_offset = 0usize;
    let mut name_length = 0usize;
    for i in 0..num_tables {
        let rec = offset + 12 + i * 16;
        if &data[rec..rec + 4] == b"name" {
            name_offset =
                u32::from_be_bytes([data[rec + 8], data[rec + 9], data[rec + 10], data[rec + 11]])
                    as usize;
            name_length = u32::from_be_bytes([
                data[rec + 12],
                data[rec + 13],
                data[rec + 14],
                data[rec + 15],
            ]) as usize;
            break;
        }
    }
    if name_offset == 0 || name_offset + name_length > data.len() {
        return None;
    }

    let tbl = &data[name_offset..];
    if tbl.len() < 6 {
        return None;
    }
    let count = u16::from_be_bytes([tbl[2], tbl[3]]) as usize;
    let string_offset = u16::from_be_bytes([tbl[4], tbl[5]]) as usize;
    if tbl.len() < 6 + count * 12 {
        return None;
    }

    // Collect candidates for name IDs 1 (family), 2 (subfamily), 16 (typo family), 17 (typo subfamily).
    // Prefer platform 3 (Windows UTF-16) over 1 (Mac).
    // Prefer typo (16/17) over legacy (1/2).
    let mut family: Option<String> = None;
    let mut family_pri = 0u8;
    let mut subfamily: Option<String> = None;
    let mut subfamily_pri = 0u8;

    for i in 0..count {
        let rec = 6 + i * 12;
        let platform = u16::from_be_bytes([tbl[rec], tbl[rec + 1]]);
        let name_id = u16::from_be_bytes([tbl[rec + 6], tbl[rec + 7]]);
        let length = u16::from_be_bytes([tbl[rec + 8], tbl[rec + 9]]) as usize;
        let str_off = u16::from_be_bytes([tbl[rec + 10], tbl[rec + 11]]) as usize;

        let is_family = name_id == 1 || name_id == 16;
        let is_subfamily = name_id == 2 || name_id == 17;
        if !is_family && !is_subfamily {
            continue;
        }

        let plat_bonus: u8 = if platform == 3 {
            2
        } else if platform == 1 {
            1
        } else {
            0
        };
        if plat_bonus == 0 {
            continue;
        }
        let typo_bonus: u8 = if name_id >= 16 { 4 } else { 0 };
        let priority = plat_bonus + typo_bonus;

        let start = string_offset + str_off;
        if start + length > tbl.len() {
            continue;
        }
        let raw = &tbl[start..start + length];

        let decoded = if platform == 3 {
            let chars: Vec<u16> = raw
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16_lossy(&chars)
        } else {
            String::from_utf8_lossy(raw).into_owned()
        };
        let decoded = decoded.trim().to_owned();
        if decoded.is_empty() {
            continue;
        }

        if is_family && priority > family_pri {
            family = Some(decoded);
            family_pri = priority;
        } else if is_subfamily && priority > subfamily_pri {
            subfamily = Some(decoded);
            subfamily_pri = priority;
        }
    }

    Some(FontInfo {
        family: family?,
        subfamily: subfamily.unwrap_or_else(|| "Regular".to_owned()),
    })
}

fn subfamily_to_weight_style(subfamily: &str) -> (&'static str, &'static str) {
    let s = subfamily.to_lowercase();
    let bold = s.contains("bold") || s.contains("heavy") || s.contains("black");
    let italic = s.contains("italic") || s.contains("oblique");
    match (bold, italic) {
        (true, true) => ("bold", "italic"),
        (true, false) => ("bold", "normal"),
        (false, true) => ("normal", "italic"),
        (false, false) => ("normal", "normal"),
    }
}

pub fn find_font_files(family: &str) -> Vec<FontVariant> {
    if let Some(results) = find_via_fc_match(family) {
        if !results.is_empty() {
            return results;
        }
    }
    let dirs = font_dirs();
    let family_lower = family.to_lowercase();
    let family_nospace = family_lower.replace(' ', "");
    let mut results = Vec::new();
    for dir in &dirs {
        find_in_dir_recursive(dir, &family_lower, &family_nospace, &mut results);
    }
    results
}

fn find_via_fc_match(family: &str) -> Option<Vec<FontVariant>> {
    let output = std::process::Command::new("fc-match")
        .args(["--format", "%{file}\n%{style}\n", "-a", family])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = text.lines().collect();
    let mut results = Vec::new();
    let mut seen = BTreeSet::new();
    for pair in lines.chunks(2) {
        if pair.len() < 2 {
            break;
        }
        let path = pair[0].trim();
        let style_str = pair[1].trim();
        if path.is_empty() || !seen.insert(path.to_owned()) {
            continue;
        }
        if let Ok(data) = std::fs::read(path) {
            if let Some(info) = read_font_info(&data) {
                if !info.family.eq_ignore_ascii_case(family) {
                    continue;
                }
                let (weight, style) = subfamily_to_weight_style(style_str);
                results.push(FontVariant {
                    path: path.to_owned(),
                    weight: weight.to_owned(),
                    style: style.to_owned(),
                });
            }
        }
    }
    if results.is_empty() {
        None
    } else {
        Some(results)
    }
}

fn find_in_dir_recursive(
    dir: &str,
    family_lower: &str,
    family_nospace: &str,
    results: &mut Vec<FontVariant>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            find_in_dir_recursive(
                &path.to_string_lossy(),
                family_lower,
                family_nospace,
                results,
            );
            continue;
        }
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !matches!(ext, "ttf" | "otf" | "woff" | "woff2" | "ttc") {
            continue;
        }

        if let Ok(data) = std::fs::read(&path) {
            if let Some(info) = read_font_info(&data) {
                let parsed_lower = info.family.to_lowercase();
                if parsed_lower != family_lower && parsed_lower.replace(' ', "") != family_nospace {
                    continue;
                }
                let (weight, style) = subfamily_to_weight_style(&info.subfamily);
                results.push(FontVariant {
                    path: path.to_string_lossy().into_owned(),
                    weight: weight.to_owned(),
                    style: style.to_owned(),
                });
            }
        }
    }
}

pub fn list_font_families() -> Vec<String> {
    if let Some(families) = list_via_fc_list() {
        return families;
    }
    list_via_name_tables()
}

fn list_via_fc_list() -> Option<Vec<String>> {
    let output = std::process::Command::new("fc-list")
        .args(["--format", "%{family}\n"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut families = BTreeSet::new();
    for line in text.lines() {
        for name in line.split(',') {
            let name = name.trim();
            if !name.is_empty() {
                families.insert(name.to_owned());
            }
        }
    }
    if families.is_empty() {
        return None;
    }
    Some(families.into_iter().collect())
}

fn list_via_name_tables() -> Vec<String> {
    let dirs = font_dirs();
    let mut families = BTreeSet::new();
    for dir in &dirs {
        scan_dir_recursive(dir, &mut families);
    }
    families.into_iter().collect()
}

fn scan_dir_recursive(dir: &str, families: &mut BTreeSet<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_dir_recursive(&path.to_string_lossy(), families);
            continue;
        }
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !matches!(ext, "ttf" | "otf" | "woff" | "woff2" | "ttc") {
            continue;
        }
        if let Ok(data) = std::fs::read(&path) {
            if let Some(info) = read_font_info(&data) {
                families.insert(info.family);
            }
        }
    }
}

pub fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(CHARS[(n >> 18 & 63) as usize] as char);
        out.push(CHARS[(n >> 12 & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(CHARS[(n >> 6 & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(CHARS[(n & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

pub fn font_face_css(family: &str) -> Option<String> {
    let files = find_font_files_with_data(family);
    if files.is_empty() {
        return None;
    }
    let mut css = String::new();
    for (variant, data) in &files {
        let ext = variant.path.rsplit('.').next().unwrap_or("ttf");
        let mime = match ext {
            "otf" => "font/otf",
            "woff" => "font/woff",
            "woff2" => "font/woff2",
            _ => "font/ttf",
        };
        let b64 = base64_encode(data);
        css.push_str(&format!(
            "@font-face {{ font-family: '{}'; font-weight: {}; font-style: {}; src: url('data:{};base64,{}'); }}\n",
            family, variant.weight, variant.style, mime, b64,
        ));
    }
    if css.is_empty() {
        None
    } else {
        Some(css)
    }
}

/// Like `find_font_files` but returns the file data alongside each variant,
/// avoiding a second read in `font_face_css`.
fn find_font_files_with_data(family: &str) -> Vec<(FontVariant, Vec<u8>)> {
    let variants = find_font_files(family);
    variants
        .into_iter()
        .filter_map(|v| {
            let data = std::fs::read(&v.path).ok()?;
            Some((v, data))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_font_info_from_system_fonts() {
        let families = list_font_families();
        assert!(!families.is_empty(), "no fonts found on system");
        for f in &families {
            assert!(!f.is_empty());
            assert!(!f.contains('\0'));
        }
    }

    #[test]
    fn subfamily_parsing() {
        assert_eq!(subfamily_to_weight_style("Regular"), ("normal", "normal"));
        assert_eq!(subfamily_to_weight_style("Bold"), ("bold", "normal"));
        assert_eq!(subfamily_to_weight_style("Italic"), ("normal", "italic"));
        assert_eq!(subfamily_to_weight_style("Bold Italic"), ("bold", "italic"));
        assert_eq!(
            subfamily_to_weight_style("Bold Oblique"),
            ("bold", "italic")
        );
    }
}
