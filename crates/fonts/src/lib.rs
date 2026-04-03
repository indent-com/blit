use std::collections::BTreeSet;

pub fn font_dirs() -> Vec<String> {
    let mut dirs = Vec::new();
    if let Ok(extra) = std::env::var("BLIT_FONT_DIRS") {
        let sep = if cfg!(windows) { ';' } else { ':' };
        for d in extra.split(sep) {
            let d = d.trim();
            if !d.is_empty() {
                dirs.push(d.to_owned());
            }
        }
    }
    #[cfg(unix)]
    {
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
    }
    #[cfg(windows)]
    {
        if let Ok(windir) = std::env::var("SYSTEMROOT") {
            dirs.push(format!("{windir}\\Fonts"));
        } else {
            dirs.push(r"C:\Windows\Fonts".into());
        }
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            dirs.push(format!(r"{local}\Microsoft\Windows\Fonts"));
        }
    }
    dirs
}

#[derive(Debug, Clone)]
pub struct FontInfo {
    pub family: String,
    pub subfamily: String,
    pub is_monospace: bool,
}

#[derive(Debug, Clone)]
pub struct FontVariant {
    pub path: String,
    pub weight: String,
    pub style: String,
}

fn sfnt_offset(data: &[u8]) -> Option<usize> {
    if data.len() < 12 {
        return None;
    }
    if &data[0..4] == b"ttcf" {
        if data.len() < 16 {
            return None;
        }
        Some(u32::from_be_bytes([data[12], data[13], data[14], data[15]]) as usize)
    } else {
        Some(0)
    }
}

fn table_slice<'a>(data: &'a [u8], tag: &[u8; 4]) -> Option<&'a [u8]> {
    let offset = sfnt_offset(data)?;
    if offset + 12 > data.len() {
        return None;
    }
    let num_tables = u16::from_be_bytes([data[offset + 4], data[offset + 5]]) as usize;
    if offset + 12 + num_tables * 16 > data.len() {
        return None;
    }
    for i in 0..num_tables {
        let rec = offset + 12 + i * 16;
        if &data[rec..rec + 4] == tag {
            let table_offset =
                u32::from_be_bytes([data[rec + 8], data[rec + 9], data[rec + 10], data[rec + 11]])
                    as usize;
            let table_length = u32::from_be_bytes([
                data[rec + 12],
                data[rec + 13],
                data[rec + 14],
                data[rec + 15],
            ]) as usize;
            let table_end = table_offset.checked_add(table_length)?;
            if table_end > data.len() {
                return None;
            }
            return Some(&data[table_offset..table_end]);
        }
    }
    None
}

fn read_is_monospace(data: &[u8]) -> bool {
    if let Some(post) = table_slice(data, b"post")
        && post.len() >= 16
    {
        let is_fixed_pitch = u32::from_be_bytes([post[12], post[13], post[14], post[15]]);
        if is_fixed_pitch != 0 {
            return true;
        }
    }

    let Some(hhea) = table_slice(data, b"hhea") else {
        return false;
    };
    let Some(hmtx) = table_slice(data, b"hmtx") else {
        return false;
    };
    if hhea.len() < 36 {
        return false;
    }
    let num_long_metrics = u16::from_be_bytes([hhea[34], hhea[35]]) as usize;
    if num_long_metrics == 0 {
        return false;
    }
    let Some(metrics_len) = num_long_metrics.checked_mul(4) else {
        return false;
    };
    if hmtx.len() < metrics_len {
        return false;
    }

    let mut reference_width: Option<u16> = None;
    for i in 0..num_long_metrics {
        let idx = i * 4;
        let advance = u16::from_be_bytes([hmtx[idx], hmtx[idx + 1]]);
        if advance == 0 {
            continue;
        }
        match reference_width {
            Some(width) if width != advance => return false,
            Some(_) => {}
            None => reference_width = Some(advance),
        }
    }

    reference_width.is_some()
}

/// Read the monospace advance width as a fraction of the em square.
/// Returns `advance_width / units_per_em` for the first non-zero advance in hmtx,
/// matching how native terminals (Ghostty, kitty) compute cell width.
fn read_advance_ratio(data: &[u8]) -> Option<f64> {
    let head = table_slice(data, b"head")?;
    if head.len() < 20 {
        return None;
    }
    let units_per_em = u16::from_be_bytes([head[18], head[19]]) as f64;
    if units_per_em == 0.0 {
        return None;
    }

    let hhea = table_slice(data, b"hhea")?;
    let hmtx = table_slice(data, b"hmtx")?;
    if hhea.len() < 36 {
        return None;
    }
    let num_long_metrics = u16::from_be_bytes([hhea[34], hhea[35]]) as usize;
    if num_long_metrics == 0 || hmtx.len() < num_long_metrics * 4 {
        return None;
    }

    for i in 0..num_long_metrics {
        let idx = i * 4;
        let advance = u16::from_be_bytes([hmtx[idx], hmtx[idx + 1]]);
        if advance > 0 {
            return Some(advance as f64 / units_per_em);
        }
    }
    None
}

/// Read font family and subfamily from a TTF/OTF/TTC file's `name` table.
fn read_font_info(data: &[u8]) -> Option<FontInfo> {
    let tbl = table_slice(data, b"name")?;
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
        is_monospace: read_is_monospace(data),
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
    #[cfg(unix)]
    if let Some(results) = find_via_fc_match(family)
        && !results.is_empty()
    {
        return results;
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

#[cfg(unix)]
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
    let mut results = Vec::with_capacity(lines.len() / 2);
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
        if let Ok(data) = std::fs::read(path)
            && let Some(info) = read_font_info(&data)
        {
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

        if let Ok(data) = std::fs::read(&path)
            && let Some(info) = read_font_info(&data)
        {
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

pub fn list_font_families() -> Vec<String> {
    #[cfg(unix)]
    if let Some(families) = list_via_fc_list() {
        return families;
    }
    list_via_name_tables()
}

pub fn list_monospace_font_families() -> Vec<String> {
    #[cfg(unix)]
    if let Some(families) = list_monospace_via_fc_list() {
        return families;
    }
    list_monospace_via_name_tables()
}

#[cfg(unix)]
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

#[cfg(unix)]
fn list_monospace_via_fc_list() -> Option<Vec<String>> {
    let output = std::process::Command::new("fc-list")
        .args(["--format", "%{file}\n"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut families = BTreeSet::new();
    let mut seen_paths = BTreeSet::new();
    for line in text.lines() {
        let path = line.trim();
        if path.is_empty() || !seen_paths.insert(path.to_owned()) {
            continue;
        }
        let Ok(data) = std::fs::read(path) else {
            continue;
        };
        let Some(info) = read_font_info(&data) else {
            continue;
        };
        if !info.is_monospace {
            continue;
        }
        // Use the name table family so the name matches what find_font_files expects.
        families.insert(info.family);
    }
    if families.is_empty() {
        return None;
    }
    Some(families.into_iter().collect())
}

fn list_monospace_via_name_tables() -> Vec<String> {
    let dirs = font_dirs();
    let mut families = BTreeSet::new();
    for dir in &dirs {
        scan_monospace_dir_recursive(dir, &mut families);
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
        if let Ok(data) = std::fs::read(&path)
            && let Some(info) = read_font_info(&data)
        {
            families.insert(info.family);
        }
    }
}

fn scan_monospace_dir_recursive(dir: &str, families: &mut BTreeSet<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_monospace_dir_recursive(&path.to_string_lossy(), families);
            continue;
        }
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !matches!(ext, "ttf" | "otf" | "woff" | "woff2" | "ttc") {
            continue;
        }
        if let Ok(data) = std::fs::read(&path)
            && let Some(info) = read_font_info(&data)
            && info.is_monospace
        {
            families.insert(info.family);
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
    if css.is_empty() { None } else { Some(css) }
}

/// Return the advance-width / units-per-em ratio for a font family's regular variant.
/// This is how native terminals compute cell width: `ratio * font_size_px`.
pub fn font_advance_ratio(family: &str) -> Option<f64> {
    let files = find_font_files_with_data(family);
    // Prefer the "normal" weight regular variant
    for (variant, data) in &files {
        if variant.style == "normal"
            && (variant.weight == "400" || variant.weight == "normal")
            && let Some(ratio) = read_advance_ratio(data)
        {
            return Some(ratio);
        }
    }
    // Fall back to any variant
    for (_variant, data) in &files {
        if let Some(ratio) = read_advance_ratio(data) {
            return Some(ratio);
        }
    }
    None
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

    fn build_test_font(tables: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
        let header_len = 12 + tables.len() * 16;
        let mut data = vec![0u8; header_len];
        data[0..4].copy_from_slice(&[0, 1, 0, 0]);
        data[4..6].copy_from_slice(&(tables.len() as u16).to_be_bytes());

        let mut offset = header_len;
        for (i, (tag, table)) in tables.iter().enumerate() {
            let rec = 12 + i * 16;
            data[rec..rec + 4].copy_from_slice(*tag);
            data[rec + 8..rec + 12].copy_from_slice(&(offset as u32).to_be_bytes());
            data[rec + 12..rec + 16].copy_from_slice(&(table.len() as u32).to_be_bytes());
            data.extend_from_slice(table);
            offset += table.len();
        }

        data
    }

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

    #[test]
    fn detects_monospace_from_post_table() {
        let mut post = vec![0u8; 32];
        post[12..16].copy_from_slice(&1u32.to_be_bytes());
        let font = build_test_font(&[(b"post", post)]);
        assert!(read_is_monospace(&font));
    }

    #[test]
    fn detects_monospace_from_uniform_hmtx_widths() {
        let mut hhea = vec![0u8; 36];
        hhea[34..36].copy_from_slice(&2u16.to_be_bytes());

        let mut hmtx = vec![0u8; 8];
        hmtx[0..2].copy_from_slice(&600u16.to_be_bytes());
        hmtx[4..6].copy_from_slice(&600u16.to_be_bytes());

        let font = build_test_font(&[(b"hhea", hhea), (b"hmtx", hmtx)]);
        assert!(read_is_monospace(&font));
    }

    #[test]
    fn rejects_variable_width_fonts() {
        let mut hhea = vec![0u8; 36];
        hhea[34..36].copy_from_slice(&2u16.to_be_bytes());

        let mut hmtx = vec![0u8; 8];
        hmtx[0..2].copy_from_slice(&500u16.to_be_bytes());
        hmtx[4..6].copy_from_slice(&700u16.to_be_bytes());

        let font = build_test_font(&[(b"hhea", hhea), (b"hmtx", hmtx)]);
        assert!(!read_is_monospace(&font));
    }

    // ── base64_encode ──

    #[test]
    fn base64_empty() {
        assert_eq!(base64_encode(b""), "");
    }

    #[test]
    fn base64_one_byte() {
        assert_eq!(base64_encode(b"M"), "TQ==");
    }

    #[test]
    fn base64_two_bytes() {
        assert_eq!(base64_encode(b"Ma"), "TWE=");
    }

    #[test]
    fn base64_three_bytes() {
        assert_eq!(base64_encode(b"Man"), "TWFu");
    }

    #[test]
    fn base64_rfc4648_vectors() {
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    // ── sfnt_offset ──

    #[test]
    fn sfnt_offset_too_short() {
        assert_eq!(sfnt_offset(b"abc"), None);
    }

    #[test]
    fn sfnt_offset_non_ttc() {
        let font = build_test_font(&[]);
        assert_eq!(sfnt_offset(&font), Some(0));
    }

    #[test]
    fn sfnt_offset_ttc_header() {
        let mut data = vec![0u8; 20];
        data[0..4].copy_from_slice(b"ttcf");
        data[12..16].copy_from_slice(&100u32.to_be_bytes());
        assert_eq!(sfnt_offset(&data), Some(100));
    }

    #[test]
    fn sfnt_offset_ttc_too_short() {
        let mut data = vec![0u8; 14];
        data[0..4].copy_from_slice(b"ttcf");
        assert_eq!(sfnt_offset(&data), None);
    }

    // ── table_slice ──

    #[test]
    fn table_slice_found() {
        let table_data = vec![1, 2, 3, 4];
        let font = build_test_font(&[(b"test", table_data.clone())]);
        let slice = table_slice(&font, b"test");
        assert_eq!(slice, Some(table_data.as_slice()));
    }

    #[test]
    fn table_slice_not_found() {
        let font = build_test_font(&[(b"aaaa", vec![0])]);
        assert_eq!(table_slice(&font, b"zzzz"), None);
    }

    #[test]
    fn table_slice_empty_font() {
        let font = build_test_font(&[]);
        assert_eq!(table_slice(&font, b"test"), None);
    }

    // ── read_advance_ratio ──

    #[test]
    fn advance_ratio_basic() {
        let mut head = vec![0u8; 20];
        head[18..20].copy_from_slice(&1000u16.to_be_bytes());

        let mut hhea = vec![0u8; 36];
        hhea[34..36].copy_from_slice(&1u16.to_be_bytes());

        let mut hmtx = vec![0u8; 4];
        hmtx[0..2].copy_from_slice(&600u16.to_be_bytes());

        let font = build_test_font(&[(b"head", head), (b"hhea", hhea), (b"hmtx", hmtx)]);
        let ratio = read_advance_ratio(&font).unwrap();
        assert!((ratio - 0.6).abs() < 1e-10);
    }

    #[test]
    fn advance_ratio_skips_zero_advances() {
        let mut head = vec![0u8; 20];
        head[18..20].copy_from_slice(&1000u16.to_be_bytes());

        let mut hhea = vec![0u8; 36];
        hhea[34..36].copy_from_slice(&2u16.to_be_bytes());

        let mut hmtx = vec![0u8; 8];
        hmtx[0..2].copy_from_slice(&0u16.to_be_bytes());
        hmtx[4..6].copy_from_slice(&500u16.to_be_bytes());

        let font = build_test_font(&[(b"head", head), (b"hhea", hhea), (b"hmtx", hmtx)]);
        let ratio = read_advance_ratio(&font).unwrap();
        assert!((ratio - 0.5).abs() < 1e-10);
    }

    #[test]
    fn advance_ratio_no_head_table() {
        let hhea = vec![0u8; 36];
        let hmtx = vec![0u8; 4];
        let font = build_test_font(&[(b"hhea", hhea), (b"hmtx", hmtx)]);
        assert!(read_advance_ratio(&font).is_none());
    }

    #[test]
    fn advance_ratio_zero_units_per_em() {
        let head = vec![0u8; 20];
        let hhea = vec![0u8; 36];
        let hmtx = vec![0u8; 4];
        let font = build_test_font(&[(b"head", head), (b"hhea", hhea), (b"hmtx", hmtx)]);
        assert!(read_advance_ratio(&font).is_none());
    }

    // ── subfamily_to_weight_style (extra cases) ──

    #[test]
    fn subfamily_heavy() {
        assert_eq!(subfamily_to_weight_style("Heavy"), ("bold", "normal"));
    }

    #[test]
    fn subfamily_black() {
        assert_eq!(subfamily_to_weight_style("Black"), ("bold", "normal"));
    }

    #[test]
    fn subfamily_oblique() {
        assert_eq!(subfamily_to_weight_style("Oblique"), ("normal", "italic"));
    }

    #[test]
    fn subfamily_case_insensitive() {
        assert_eq!(subfamily_to_weight_style("BOLD ITALIC"), ("bold", "italic"));
        assert_eq!(subfamily_to_weight_style("bold italic"), ("bold", "italic"));
    }

    #[test]
    fn subfamily_unrecognized() {
        assert_eq!(subfamily_to_weight_style("Light"), ("normal", "normal"));
        assert_eq!(subfamily_to_weight_style("Thin"), ("normal", "normal"));
    }
}
