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
    /// Which face of the file this is. Always 0 outside `ttcf` collections.
    pub face_index: u32,
    pub weight: String,
    pub style: String,
}

/// How many faces a font file holds. Plain sfnt files hold exactly one.
fn face_count(data: &[u8]) -> usize {
    if data.len() >= 12 && &data[0..4] == b"ttcf" {
        u32::from_be_bytes([data[8], data[9], data[10], data[11]]) as usize
    } else {
        1
    }
}

/// Byte offset of a face's table directory, or None when the file has no
/// such face.
fn face_offset(data: &[u8], index: usize) -> Option<usize> {
    if data.len() < 12 {
        return None;
    }
    if &data[0..4] != b"ttcf" {
        return if index == 0 { Some(0) } else { None };
    }
    let rec = 12 + index * 4;
    if rec + 4 > data.len() {
        return None;
    }
    Some(u32::from_be_bytes([data[rec], data[rec + 1], data[rec + 2], data[rec + 3]]) as usize)
}

fn sfnt_offset(data: &[u8]) -> Option<usize> {
    face_offset(data, 0)
}

/// Locate a table within one specific face's directory.
fn table_slice_in<'a>(data: &'a [u8], offset: usize, tag: &[u8; 4]) -> Option<&'a [u8]> {
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

fn read_is_monospace_in(data: &[u8], face: usize) -> bool {
    let table_slice = |tag: &[u8; 4]| table_slice_in(data, face, tag);
    if let Some(post) = table_slice(b"post")
        && post.len() >= 16
    {
        let is_fixed_pitch = u32::from_be_bytes([post[12], post[13], post[14], post[15]]);
        if is_fixed_pitch != 0 {
            return true;
        }
    }

    let Some(hhea) = table_slice(b"hhea") else {
        return false;
    };
    let Some(hmtx) = table_slice(b"hmtx") else {
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
fn read_advance_ratio_in(data: &[u8], face: usize) -> Option<f64> {
    let head = table_slice_in(data, face, b"head")?;
    if head.len() < 20 {
        return None;
    }
    let units_per_em = u16::from_be_bytes([head[18], head[19]]) as f64;
    if units_per_em == 0.0 {
        return None;
    }

    let hhea = table_slice_in(data, face, b"hhea")?;
    let hmtx = table_slice_in(data, face, b"hmtx")?;
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
    read_font_info_in(data, sfnt_offset(data)?)
}

fn read_font_info_in(data: &[u8], face: usize) -> Option<FontInfo> {
    let tbl = table_slice_in(data, face, b"name")?;
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
        let language = u16::from_be_bytes([tbl[rec + 4], tbl[rec + 5]]);
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
        // Name records repeat per language, and the localized ones are useless
        // to us: the macOS copies of Courier New call their bold face
        // "Negreta", which matches nothing downstream. Rank English first —
        // 0x0409 (en-US) on Windows records, 0 (English) on Mac ones.
        let lang_bonus: u8 =
            if (platform == 3 && language == 0x0409) || (platform == 1 && language == 0) {
                8
            } else {
                0
            };
        let priority = plat_bonus + typo_bonus + lang_bonus;

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
        is_monospace: read_is_monospace_in(data, face),
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

/// CSS weight/style for one face.
///
/// `head.macStyle` is the authority: two bits at a fixed offset that mean the
/// same thing in every language, where the subfamily string may be localized
/// (and so unmatchable) or missing. The string still gets a say, because it
/// distinguishes weights macStyle cannot and some fonts leave macStyle clear.
fn weight_style_in(data: &[u8], face: usize, subfamily: &str) -> (&'static str, &'static str) {
    let (mut bold, mut italic) = (false, false);
    if let Some(head) = table_slice_in(data, face, b"head")
        && head.len() >= 46
    {
        let mac_style = u16::from_be_bytes([head[44], head[45]]);
        bold = mac_style & 1 != 0;
        italic = mac_style & 2 != 0;
    }
    let (str_weight, str_style) = subfamily_to_weight_style(subfamily);
    match (
        bold || str_weight == "bold",
        italic || str_style == "italic",
    ) {
        (true, true) => ("bold", "italic"),
        (true, false) => ("bold", "normal"),
        (false, true) => ("normal", "italic"),
        (false, false) => ("normal", "normal"),
    }
}

/// Whether a font's own family name refers to the family being asked for.
/// Space-insensitive because file-scanned names and requested names disagree
/// on them ("PragmataPro Mono" vs "PragmataProMono").
fn family_matches(parsed: &str, requested: &str) -> bool {
    let a = parsed.to_lowercase();
    let b = requested.to_lowercase();
    a == b || a.replace(' ', "") == b.replace(' ', "")
}

/// Every face in one font file that belongs to `family`.
///
/// A `.ttc` collection holds several faces — on macOS that is how Menlo,
/// Courier and the SF families ship their bold and italic — so a file is one
/// candidate per face, not one candidate outright.
fn variants_in_file(path: &str, data: &[u8], family: &str) -> Vec<FontVariant> {
    let mut variants = Vec::new();
    let mut seen = BTreeSet::new();
    for face in 0..face_count(data).max(1) {
        let Some(offset) = face_offset(data, face) else {
            continue;
        };
        let Some(info) = read_font_info_in(data, offset) else {
            continue;
        };
        if !family_matches(&info.family, family) {
            continue;
        }
        let (weight, style) = weight_style_in(data, offset, &info.subfamily);
        // One face per CSS (weight, style) pair; a second claimant would only
        // shadow the first in the stylesheet anyway.
        if !seen.insert((weight, style)) {
            continue;
        }
        variants.push(FontVariant {
            path: path.to_owned(),
            face_index: face as u32,
            weight: weight.to_owned(),
            style: style.to_owned(),
        });
    }
    // Regular, bold, italic, bold-italic: the order a terminal needs them, and
    // so the order the payload budget in font_face_css spends itself in.
    variants.sort_by_key(|v| (v.weight == "bold", v.style == "italic"));
    variants
}

pub fn find_font_files(family: &str) -> Vec<FontVariant> {
    font_files_for_family(family)
        .into_iter()
        .flat_map(|(_, variants)| variants)
        .collect()
}

/// Candidate files for a family, each with its bytes and the faces inside it
/// that match. Reading happens once per file however many faces it yields.
fn font_files_for_family(family: &str) -> Vec<(Vec<u8>, Vec<FontVariant>)> {
    #[cfg(unix)]
    if let Some(paths) = paths_via_fc_match(family) {
        let results = read_matching_faces(paths, family);
        // Fontconfig answers every query with *something*, so an empty result
        // here means it had nothing of this family — fall through and scan.
        if !results.is_empty() {
            return results;
        }
    }
    let mut paths = Vec::new();
    for dir in &font_dirs() {
        collect_font_paths(dir, &mut paths);
    }
    read_matching_faces(paths, family)
}

fn read_matching_faces(paths: Vec<String>, family: &str) -> Vec<(Vec<u8>, Vec<FontVariant>)> {
    let mut results = Vec::new();
    let mut seen_paths = BTreeSet::new();
    for path in paths {
        if !seen_paths.insert(path.clone()) {
            continue;
        }
        let Ok(data) = std::fs::read(&path) else {
            continue;
        };
        let variants = variants_in_file(&path, &data, family);
        if !variants.is_empty() {
            results.push((data, variants));
        }
    }
    results
}

/// Font files fontconfig considers relevant to `family`, best match first.
/// The reported style is ignored: a collection reports one style per listing
/// but contains all of them, so the faces are enumerated from the file itself.
#[cfg(unix)]
fn paths_via_fc_match(family: &str) -> Option<Vec<String>> {
    let output = std::process::Command::new("fc-match")
        .args(["--format", "%{file}\n", "-a", family])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let paths: Vec<String> = text
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(|l| l.to_owned())
        .collect();
    if paths.is_empty() { None } else { Some(paths) }
}

fn collect_font_paths(dir: &str, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_font_paths(&path.to_string_lossy(), out);
            continue;
        }
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if matches!(ext, "ttf" | "otf" | "woff" | "woff2" | "ttc") {
            out.push(path.to_string_lossy().into_owned());
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

/// Ceiling on the raw font bytes one stylesheet will inline. Collections that
/// share outlines across faces (CJK families, above all) turn into one full
/// copy per extracted face, and a stylesheet is not the place to discover that
/// you have asked for 80 MB. Faces are ordered regular-first, so what a budget
/// this size drops is the italics of an enormous family, never the text face.
///
/// The first face is exempt, which is what makes that last sentence true: a
/// family whose regular face is *itself* over the cap would otherwise emit no
/// `@font-face` at all and fall back to a default font — a worse outcome than
/// inlining one large face, and one that large CJK text faces (15–30 MB
/// alone) reach routinely.
const MAX_CSS_FONT_BYTES: usize = 24 * 1024 * 1024;

/// Whether a face of `len` bytes gets inlined, given the remaining budget
/// and how many faces are already in the stylesheet. The first one always
/// does — see [`MAX_CSS_FONT_BYTES`].
fn face_fits(len: usize, budget: usize, emitted: usize) -> bool {
    emitted == 0 || len <= budget
}

pub fn font_face_css(family: &str) -> Option<String> {
    let files = font_files_for_family(family);
    if files.is_empty() {
        return None;
    }
    // Escape single quotes in the family name to prevent CSS injection.
    let safe_family = family.replace('\\', "\\\\").replace('\'', "\\'");
    let mut css = String::new();
    let mut budget = MAX_CSS_FONT_BYTES;
    let mut emitted = 0usize;
    for (data, variants) in &files {
        for variant in variants {
            let Some((bytes, mime)) = face_payload(data, variant) else {
                continue;
            };
            if !face_fits(bytes.len(), budget, emitted) {
                continue;
            }
            budget = budget.saturating_sub(bytes.len());
            emitted += 1;
            css.push_str(&format!(
                "@font-face {{ font-family: '{}'; font-weight: {}; font-style: {}; src: url('data:{};base64,{}'); }}\n",
                safe_family,
                variant.weight,
                variant.style,
                mime,
                base64_encode(&bytes),
            ));
        }
    }
    if css.is_empty() { None } else { Some(css) }
}

/// The bytes to serve for one variant, plus their MIME type.
///
/// Single-face files are served verbatim. A face out of a collection has to be
/// rebuilt as a standalone font first: browsers load only the first face of a
/// `ttcf`, so the bold and italic faces of a collection are unreachable
/// otherwise — and unreachable means the browser fakes them by smearing the
/// regular outlines, which is exactly the fat, blurry bold this avoids.
fn face_payload(data: &[u8], variant: &FontVariant) -> Option<(Vec<u8>, &'static str)> {
    if face_count(data) <= 1 {
        let ext = variant.path.rsplit('.').next().unwrap_or("ttf");
        let mime = match ext {
            "otf" => "font/otf",
            "woff" => "font/woff",
            "woff2" => "font/woff2",
            _ => "font/ttf",
        };
        return Some((data.to_vec(), mime));
    }
    let bytes = extract_face(data, variant.face_index)?;
    let mime = if bytes.starts_with(b"OTTO") {
        "font/otf"
    } else {
        "font/ttf"
    };
    Some((bytes, mime))
}

/// Sum of a table's bytes read as big-endian u32s, tail zero-padded — the
/// checksum every sfnt table directory record carries.
fn table_checksum(bytes: &[u8]) -> u32 {
    let mut sum = 0u32;
    for chunk in bytes.chunks(4) {
        let mut word = [0u8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        sum = sum.wrapping_add(u32::from_be_bytes(word));
    }
    sum
}

/// Rebuild one face of a font collection as a standalone sfnt file.
///
/// Collection faces share tables by reference, so a face is just a table
/// directory: copy the tables it points at into a fresh file and it stands on
/// its own. Offsets, checksums and the head checksum adjustment are recomputed
/// because they all describe positions that have just changed.
fn extract_face(data: &[u8], face_index: u32) -> Option<Vec<u8>> {
    let base = face_offset(data, face_index as usize)?;
    if base + 12 > data.len() {
        return None;
    }
    let num_tables = u16::from_be_bytes([data[base + 4], data[base + 5]]) as usize;
    if num_tables == 0 || base + 12 + num_tables * 16 > data.len() {
        return None;
    }

    let mut tables: Vec<([u8; 4], Vec<u8>)> = Vec::with_capacity(num_tables);
    for i in 0..num_tables {
        let rec = base + 12 + i * 16;
        let tag = [data[rec], data[rec + 1], data[rec + 2], data[rec + 3]];
        let offset =
            u32::from_be_bytes([data[rec + 8], data[rec + 9], data[rec + 10], data[rec + 11]])
                as usize;
        let length = u32::from_be_bytes([
            data[rec + 12],
            data[rec + 13],
            data[rec + 14],
            data[rec + 15],
        ]) as usize;
        let end = offset.checked_add(length)?;
        if end > data.len() {
            return None;
        }
        let mut bytes = data[offset..end].to_vec();
        // head.checkSumAdjustment is defined to be zero while checksums are
        // taken, and gets its real value once the file is whole.
        if &tag == b"head" && bytes.len() >= 12 {
            bytes[8..12].fill(0);
        }
        tables.push((tag, bytes));
    }
    // Directory records are in tag order.
    tables.sort_by_key(|(tag, _)| *tag);

    let entry_selector = (usize::BITS - 1 - num_tables.leading_zeros()) as u16;
    let search_range = 16u32 << entry_selector;
    let mut out = Vec::new();
    out.extend_from_slice(&data[base..base + 4]); // sfnt version
    out.extend_from_slice(&(num_tables as u16).to_be_bytes());
    out.extend_from_slice(&(search_range as u16).to_be_bytes());
    out.extend_from_slice(&entry_selector.to_be_bytes());
    out.extend_from_slice(
        &((num_tables as u32 * 16).wrapping_sub(search_range) as u16).to_be_bytes(),
    );

    let body_start = 12 + num_tables * 16;
    let mut body = Vec::new();
    let mut head_offset = None;
    for (tag, bytes) in &tables {
        let offset = body_start + body.len();
        if tag == b"head" {
            head_offset = Some(offset);
        }
        out.extend_from_slice(tag);
        out.extend_from_slice(&table_checksum(bytes).to_be_bytes());
        out.extend_from_slice(&(offset as u32).to_be_bytes());
        out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        body.extend_from_slice(bytes);
        while body.len() % 4 != 0 {
            body.push(0);
        }
    }
    out.extend_from_slice(&body);

    if let Some(head) = head_offset
        && head + 12 <= out.len()
    {
        let adjustment = 0xB1B0AFBAu32.wrapping_sub(table_checksum(&out));
        out[head + 8..head + 12].copy_from_slice(&adjustment.to_be_bytes());
    }
    Some(out)
}

/// Return the advance-width / units-per-em ratio for a font family's regular variant.
/// This is how native terminals compute cell width: `ratio * font_size_px`.
pub fn font_advance_ratio(family: &str) -> Option<f64> {
    let files = font_files_for_family(family);
    // Prefer the upright regular face; fall back to whatever reads.
    for want_regular in [true, false] {
        for (data, variants) in &files {
            for variant in variants {
                let is_regular = variant.style == "normal" && variant.weight == "normal";
                if want_regular && !is_regular {
                    continue;
                }
                if let Some(offset) = face_offset(data, variant.face_index as usize)
                    && let Some(ratio) = read_advance_ratio_in(data, offset)
                {
                    return Some(ratio);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {

    /// The budget sheds a huge family's italics; it must never shed the only
    /// face there is. A regular face over the cap used to be skipped, which
    /// left the stylesheet empty, `font_face_css` returning `None`, and the
    /// terminal on a fallback font — for families (large CJK text faces) that
    /// rendered fine before the cap existed.
    #[test]
    fn the_first_face_is_never_dropped() {
        let huge = MAX_CSS_FONT_BYTES + 1;
        assert!(
            face_fits(huge, MAX_CSS_FONT_BYTES, 0),
            "an oversized regular face is still better than no face"
        );
        // Once something is in the stylesheet, the budget rules again.
        assert!(!face_fits(huge, MAX_CSS_FONT_BYTES, 1));
        assert!(!face_fits(2, 1, 1));
        assert!(face_fits(1, 1, 1), "exactly filling the budget fits");
    }

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

    /// Wrap several single-face fonts into a `ttcf` collection, rewriting each
    /// face's table offsets to point at the shared body.
    fn build_test_ttc(faces: &[Vec<u8>]) -> Vec<u8> {
        let header_len = 12 + faces.len() * 4;
        let mut out = vec![0u8; header_len];
        out[0..4].copy_from_slice(b"ttcf");
        out[4..8].copy_from_slice(&[0, 1, 0, 0]);
        out[8..12].copy_from_slice(&(faces.len() as u32).to_be_bytes());
        for (i, face) in faces.iter().enumerate() {
            let base = out.len();
            out[12 + i * 4..16 + i * 4].copy_from_slice(&(base as u32).to_be_bytes());
            // Copy the face verbatim, then shift its table offsets by where it
            // landed — offsets in an sfnt are file-absolute.
            out.extend_from_slice(face);
            let num_tables = u16::from_be_bytes([face[4], face[5]]) as usize;
            for t in 0..num_tables {
                let rec = base + 12 + t * 16;
                let off =
                    u32::from_be_bytes([out[rec + 8], out[rec + 9], out[rec + 10], out[rec + 11]]);
                out[rec + 8..rec + 12].copy_from_slice(&(off + base as u32).to_be_bytes());
            }
        }
        out
    }

    /// Minimal `name` table carrying one family and one subfamily string.
    fn build_name_table(family: &str, subfamily: &str) -> Vec<u8> {
        let strings: Vec<Vec<u8>> = [family, subfamily]
            .iter()
            .map(|s| {
                s.encode_utf16()
                    .flat_map(|u| u.to_be_bytes())
                    .collect::<Vec<u8>>()
            })
            .collect();
        let count = strings.len();
        let storage = 6 + count * 12;
        let mut tbl = vec![0u8; storage];
        tbl[2..4].copy_from_slice(&(count as u16).to_be_bytes());
        tbl[4..6].copy_from_slice(&(storage as u16).to_be_bytes());
        let mut offset = 0usize;
        for (i, (name_id, bytes)) in [1u16, 2].iter().zip(strings.iter()).enumerate() {
            let rec = 6 + i * 12;
            tbl[rec..rec + 2].copy_from_slice(&3u16.to_be_bytes()); // Windows
            tbl[rec + 2..rec + 4].copy_from_slice(&1u16.to_be_bytes()); // UCS-2
            tbl[rec + 6..rec + 8].copy_from_slice(&name_id.to_be_bytes());
            tbl[rec + 8..rec + 10].copy_from_slice(&(bytes.len() as u16).to_be_bytes());
            tbl[rec + 10..rec + 12].copy_from_slice(&(offset as u16).to_be_bytes());
            offset += bytes.len();
        }
        for bytes in &strings {
            tbl.extend_from_slice(bytes);
        }
        tbl
    }

    fn face_with_names(family: &str, subfamily: &str) -> Vec<u8> {
        let mut head = vec![0u8; 54];
        head[18..20].copy_from_slice(&1000u16.to_be_bytes()); // unitsPerEm
        head[8..12].copy_from_slice(&0xDEADBEEFu32.to_be_bytes()); // checkSumAdjustment
        let mut hhea = vec![0u8; 36];
        hhea[34..36].copy_from_slice(&1u16.to_be_bytes());
        let mut hmtx = vec![0u8; 4];
        hmtx[0..2].copy_from_slice(&600u16.to_be_bytes());
        build_test_font(&[
            (b"head", head),
            (b"hhea", hhea),
            (b"hmtx", hmtx),
            (b"name", build_name_table(family, subfamily)),
        ])
    }

    #[test]
    fn enumerates_every_face_of_a_collection() {
        let ttc = build_test_ttc(&[
            face_with_names("Test Mono", "Regular"),
            face_with_names("Test Mono", "Bold"),
            face_with_names("Other", "Regular"),
        ]);
        let variants = variants_in_file("/x/test.ttc", &ttc, "Test Mono");
        assert_eq!(variants.len(), 2, "{variants:?}");
        assert_eq!(
            (variants[0].weight.as_str(), variants[0].face_index),
            ("normal", 0)
        );
        assert_eq!(
            (variants[1].weight.as_str(), variants[1].face_index),
            ("bold", 1)
        );
    }

    #[test]
    fn extracted_face_is_a_standalone_font() {
        let ttc = build_test_ttc(&[
            face_with_names("Test Mono", "Regular"),
            face_with_names("Test Mono", "Bold"),
        ]);
        let bold = extract_face(&ttc, 1).expect("extract");
        assert_eq!(face_count(&bold), 1);
        let info = read_font_info(&bold).expect("name table");
        assert_eq!(info.family, "Test Mono");
        assert_eq!(info.subfamily, "Bold");
        assert_eq!(read_advance_ratio_in(&bold, 0), Some(0.6));
        // Whole-file checksum must land on the magic constant, which is what
        // head.checkSumAdjustment exists to make true.
        assert_eq!(table_checksum(&bold), 0xB1B0AFBA);
    }

    #[test]
    fn extracted_face_directory_is_sorted_and_padded() {
        let ttc = build_test_ttc(&[face_with_names("Test Mono", "Regular")]);
        let face = extract_face(&ttc, 0).expect("extract");
        let num_tables = u16::from_be_bytes([face[4], face[5]]) as usize;
        let mut tags = Vec::new();
        for i in 0..num_tables {
            let rec = 12 + i * 16;
            tags.push(face[rec..rec + 4].to_vec());
            let off =
                u32::from_be_bytes([face[rec + 8], face[rec + 9], face[rec + 10], face[rec + 11]])
                    as usize;
            assert_eq!(off % 4, 0, "table {i} not 4-byte aligned");
        }
        let mut sorted = tags.clone();
        sorted.sort();
        assert_eq!(tags, sorted);
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
        assert!(read_is_monospace_in(&font, 0));
    }

    #[test]
    fn detects_monospace_from_uniform_hmtx_widths() {
        let mut hhea = vec![0u8; 36];
        hhea[34..36].copy_from_slice(&2u16.to_be_bytes());

        let mut hmtx = vec![0u8; 8];
        hmtx[0..2].copy_from_slice(&600u16.to_be_bytes());
        hmtx[4..6].copy_from_slice(&600u16.to_be_bytes());

        let font = build_test_font(&[(b"hhea", hhea), (b"hmtx", hmtx)]);
        assert!(read_is_monospace_in(&font, 0));
    }

    #[test]
    fn rejects_variable_width_fonts() {
        let mut hhea = vec![0u8; 36];
        hhea[34..36].copy_from_slice(&2u16.to_be_bytes());

        let mut hmtx = vec![0u8; 8];
        hmtx[0..2].copy_from_slice(&500u16.to_be_bytes());
        hmtx[4..6].copy_from_slice(&700u16.to_be_bytes());

        let font = build_test_font(&[(b"hhea", hhea), (b"hmtx", hmtx)]);
        assert!(!read_is_monospace_in(&font, 0));
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
        let slice = table_slice_in(&font, 0, b"test");
        assert_eq!(slice, Some(table_data.as_slice()));
    }

    #[test]
    fn table_slice_not_found() {
        let font = build_test_font(&[(b"aaaa", vec![0])]);
        assert_eq!(table_slice_in(&font, 0, b"zzzz"), None);
    }

    #[test]
    fn table_slice_empty_font() {
        let font = build_test_font(&[]);
        assert_eq!(table_slice_in(&font, 0, b"test"), None);
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
        let ratio = read_advance_ratio_in(&font, 0).unwrap();
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
        let ratio = read_advance_ratio_in(&font, 0).unwrap();
        assert!((ratio - 0.5).abs() < 1e-10);
    }

    #[test]
    fn advance_ratio_no_head_table() {
        let hhea = vec![0u8; 36];
        let hmtx = vec![0u8; 4];
        let font = build_test_font(&[(b"hhea", hhea), (b"hmtx", hmtx)]);
        assert!(read_advance_ratio_in(&font, 0).is_none());
    }

    #[test]
    fn advance_ratio_zero_units_per_em() {
        let head = vec![0u8; 20];
        let hhea = vec![0u8; 36];
        let hmtx = vec![0u8; 4];
        let font = build_test_font(&[(b"head", head), (b"hhea", hhea), (b"hmtx", hmtx)]);
        assert!(read_advance_ratio_in(&font, 0).is_none());
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
