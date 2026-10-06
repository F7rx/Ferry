//! Turning peer-supplied file names into safe relative paths.
//!
//! The same (strictest, Windows-compatible) rules apply on every platform, so a
//! name that is safe on one device is safe on all of them and transfers between
//! platforms never produce surprises.

use unicode_general_category::{GeneralCategory, get_general_category};
use unicode_normalization::UnicodeNormalization;

/// Longest component we create, in UTF-8 bytes. Leaves room under the common
/// 255-byte limit for a ` (999)` suffix and the `.ferrypart` extension.
pub const MAX_COMPONENT_BYTES: usize = 200;

/// Deepest folder nesting accepted in one transfer.
pub const MAX_DEPTH: usize = 32;

/// Extensions longer than this are treated as part of the name when truncating.
const MAX_EXTENSION_BYTES: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    #[error("file name contains '..'")]
    Traversal,
    #[error("folders nested too deeply")]
    TooDeep,
}

/// A validated, sanitized relative path: at least one non-empty component,
/// none of them `.`/`..`, each safe to create on any OS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafeRelativePath {
    components: Vec<String>,
}

impl SafeRelativePath {
    pub fn components(&self) -> &[String] {
        &self.components
    }

    /// The final component (the file name).
    pub fn file_name(&self) -> &str {
        self.components.last().map(String::as_str).unwrap_or("untitled")
    }

    /// The folders leading to the file (may be empty).
    pub fn parents(&self) -> &[String] {
        &self.components[..self.components.len() - 1]
    }

    /// `/`-joined form, as shown in the UI and used in the protocol.
    pub fn display(&self) -> String {
        self.components.join("/")
    }
}

/// Validates and sanitizes a relative path as sent by a peer (`folder/sub/file.ext`).
///
/// Both `/` and `\` separate components. Empty and `.` components are dropped;
/// a `..` component rejects the whole path; legitimate senders never produce
/// one, so it signals an attack rather than something to repair.
pub fn sanitize_relative_path(raw: &str) -> Result<SafeRelativePath, PathError> {
    let mut components = Vec::new();
    for part in raw.split(['/', '\\']) {
        let normalized: String = part.nfc().collect();
        // Judge the component by what a user would see: invisible characters
        // must not smuggle a `..` past the check (".\u{200B}." is still "..").
        let visible: String = normalized.chars().filter(|c| !is_invisible_or_control(*c)).collect();
        let visible = visible.trim();
        if visible == ".." {
            return Err(PathError::Traversal);
        }
        if visible.is_empty() || visible == "." {
            continue;
        }
        components.push(sanitize_component(&normalized));
    }
    if components.len() > MAX_DEPTH {
        return Err(PathError::TooDeep);
    }
    if components.is_empty() {
        components.push("untitled".to_string());
    }
    Ok(SafeRelativePath { components })
}

/// Sanitizes a single file or folder name.
pub fn sanitize_component(raw: &str) -> String {
    let normalized: String = raw.nfc().collect();
    let mut out = String::with_capacity(normalized.len());
    for c in normalized.chars() {
        match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => out.push('_'),
            c if is_invisible_or_control(c) => {}
            c => out.push(c),
        }
    }

    // Windows silently strips trailing dots and spaces, which would make
    // "a.txt." and "a.txt" the same file; strip them ourselves.
    let trimmed = out.trim_start().trim_end_matches(|c: char| c == '.' || c.is_whitespace());
    let mut name = trimmed.to_string();

    if name.is_empty() || name.chars().all(|c| c == '.') {
        return "untitled".to_string();
    }

    if is_reserved_windows_name(&name) {
        name.insert(0, '_');
    }

    truncate_preserving_extension(&name, MAX_COMPONENT_BYTES)
}

/// Control characters, format characters (bidi overrides such as U+202E that
/// disguise `exe` as `txt`, zero-width joiners, BOMs, soft hyphens) and
/// line/paragraph separators never belong in a file name.
pub(crate) fn is_invisible_or_control(c: char) -> bool {
    matches!(
        get_general_category(c),
        GeneralCategory::Control
            | GeneralCategory::Format
            | GeneralCategory::LineSeparator
            | GeneralCategory::ParagraphSeparator
            | GeneralCategory::Surrogate
    )
}

/// DOS device names are reserved in every folder on Windows, with or without
/// an extension (`con.txt` opens the console). Compared on the part before the
/// first dot, ignoring trailing spaces, case-insensitively.
fn is_reserved_windows_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").trim_end().to_uppercase();
    const RESERVED: [&str; 7] = ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$", "CLOCK$"];
    if RESERVED.contains(&stem.as_str()) {
        return true;
    }
    for prefix in ["COM", "LPT"] {
        if let Some(rest) = stem.strip_prefix(prefix) {
            let mut chars = rest.chars();
            if let (Some(c), None) = (chars.next(), chars.next()) {
                // COM0-9, LPT0-9 and the superscript digits Windows also reserves.
                if c.is_ascii_digit() || matches!(c, '¹' | '²' | '³') {
                    return true;
                }
            }
        }
    }
    false
}

/// Shortens `name` to at most `max` UTF-8 bytes on a character boundary,
/// keeping a short extension intact (`very…long.pdf`, not `very…lo`).
fn truncate_preserving_extension(name: &str, max: usize) -> String {
    if name.len() <= max {
        return name.to_string();
    }
    let (stem, ext) = match name.rfind('.') {
        Some(dot) if dot > 0 && name.len() - dot - 1 <= MAX_EXTENSION_BYTES => (&name[..dot], &name[dot..]),
        _ => (name, ""),
    };
    let budget = max.saturating_sub(ext.len());
    let mut end = budget.min(stem.len());
    while end > 0 && !stem.is_char_boundary(end) {
        end -= 1;
    }
    let stem = stem[..end].trim_end_matches(|c: char| c == '.' || c.is_whitespace());
    format!("{stem}{ext}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(raw: &str) -> String {
        sanitize_relative_path(raw).unwrap().display()
    }

    #[test]
    fn keeps_ordinary_names() {
        assert_eq!(path("photo.jpg"), "photo.jpg");
        assert_eq!(path("Album/2026/IMG_0001.HEIC"), "Album/2026/IMG_0001.HEIC");
        assert_eq!(path("Résumé · final.pdf"), "Résumé · final.pdf");
        assert_eq!(path("日本語のファイル.txt"), "日本語のファイル.txt");
        assert_eq!(path(".bashrc"), ".bashrc");
    }

    #[test]
    fn rejects_traversal() {
        for raw in ["../etc/passwd", "a/../../b", "..", "a\\..\\b", "./../x", "a/ .. /b"] {
            assert_eq!(sanitize_relative_path(raw), Err(PathError::Traversal), "{raw}");
        }
    }

    #[test]
    fn rejects_traversal_hidden_behind_invisible_characters() {
        assert_eq!(sanitize_relative_path("a/.\u{200B}./b"), Err(PathError::Traversal));
    }

    #[test]
    fn strips_absolute_and_drive_prefixes() {
        assert_eq!(path("/etc/passwd"), "etc/passwd");
        assert_eq!(path("C:\\Windows\\win.ini"), "C_/Windows/win.ini");
        assert_eq!(path("\\\\server\\share\\x.txt"), "server/share/x.txt");
        assert_eq!(path("//./a"), "a");
    }

    #[test]
    fn neutralizes_alternate_data_streams_and_illegal_characters() {
        assert_eq!(path("report.txt:Zone.Identifier"), "report.txt_Zone.Identifier");
        assert_eq!(path("a<b>c|d?e*f\"g.txt"), "a_b_c_d_e_f_g.txt");
    }

    #[test]
    fn removes_bidi_and_zero_width_characters() {
        // "invoice\u{202E}fdp.exe" renders as "invoiceexe.pdf".
        assert_eq!(path("invoice\u{202E}fdp.exe"), "invoicefdp.exe");
        assert_eq!(path("pass\u{200B}word\u{FEFF}.txt"), "password.txt");
        assert_eq!(path("line\nbreak\t.txt"), "linebreak.txt");
    }

    #[test]
    fn maps_reserved_windows_names() {
        for raw in ["CON", "con.txt", "Nul.tar.gz", "COM1", "lpt9.log", "COM¹", "CONIN$", "aux .txt"] {
            let clean = path(raw);
            assert!(clean.starts_with('_'), "{raw} -> {clean}");
        }
        assert_eq!(path("CONSOLE.txt"), "CONSOLE.txt");
        assert_eq!(path("COM10"), "COM10");
    }

    #[test]
    fn trims_trailing_dots_and_spaces() {
        assert_eq!(path("notes.txt. . "), "notes.txt");
        assert_eq!(path("folder. /file"), "folder/file");
        assert_eq!(path("..."), "untitled");
    }

    #[test]
    fn empty_names_become_untitled() {
        assert_eq!(path(""), "untitled");
        assert_eq!(path("///"), "untitled");
        assert_eq!(path("\u{200B}"), "untitled");
    }

    #[test]
    fn truncates_long_names_but_keeps_extension() {
        let long = format!("{}.pdf", "a".repeat(400));
        let clean = path(&long);
        assert!(clean.len() <= MAX_COMPONENT_BYTES);
        assert!(clean.ends_with(".pdf"));

        let multibyte = format!("{}.txt", "é".repeat(300));
        let clean = path(&multibyte);
        assert!(clean.len() <= MAX_COMPONENT_BYTES);
        assert!(clean.ends_with(".txt"));
    }

    #[test]
    fn normalizes_to_nfc() {
        // "é" as e + combining acute must equal the precomposed form.
        assert_eq!(path("cafe\u{301}.txt"), "caf\u{e9}.txt");
    }

    #[test]
    fn limits_depth() {
        let deep = vec!["d"; MAX_DEPTH + 1].join("/");
        assert_eq!(sanitize_relative_path(&deep), Err(PathError::TooDeep));
        let ok = vec!["d"; MAX_DEPTH].join("/");
        assert!(sanitize_relative_path(&ok).is_ok());
    }
}
