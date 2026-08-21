//! Build-only FIGlet expansion for Dustnet static sites.
//!
//! This package is deliberately excluded from the production workspace. The
//! client, server, and authoring CLI never link font parsing or banner
//! generation; `dustnetd` serves only this tool's static output directory.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::Path;

const MARKER: &str = "{{figlet:";
const DEFAULT_FONT: &str = "ANSI_Shadow";
const MAX_FONT_BYTES: usize = 1_048_576;
const MAX_FONT_HEIGHT: usize = 64;
const MAX_GLYPH_ROW_BYTES: usize = 4096;
const MAX_MARKERS_PER_PAGE: usize = 128;
const MAX_BANNER_TEXT_BYTES: usize = 256;
// Mirrors dustnet_core::protocol::MAX_PAGE_MESSAGE_SIZE without adding the
// build-only tool to the production workspace dependency graph.
const MAX_PAGE_BYTES: usize = 1_048_576;

#[derive(Debug)]
pub struct BuildError(String);

impl BuildError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BuildError {}

#[derive(Debug)]
struct FigletFont {
    height: usize,
    glyphs: HashMap<char, Vec<String>>,
}

impl FigletFont {
    fn parse(data: &str) -> Result<Self, BuildError> {
        if data.len() > MAX_FONT_BYTES {
            return Err(BuildError::new("font exceeds the 1 MiB build limit"));
        }
        let mut lines = data.lines();
        let header_line = lines
            .next()
            .ok_or_else(|| BuildError::new("font is empty"))?;
        let header = header_line.split_whitespace().collect::<Vec<_>>();
        if header.len() < 6 || !header[0].starts_with("flf2a") {
            return Err(BuildError::new("invalid FIGlet 2 font header"));
        }
        let hardblank = header[0]
            .chars()
            .last()
            .ok_or_else(|| BuildError::new("font header has no hardblank"))?;
        let height = header[1]
            .parse::<usize>()
            .map_err(|_| BuildError::new("invalid font height"))?;
        if height == 0 || height > MAX_FONT_HEIGHT {
            return Err(BuildError::new("font height is outside 1..=64"));
        }
        let comments = header[5]
            .parse::<usize>()
            .map_err(|_| BuildError::new("invalid font comment count"))?;
        for _ in 0..comments {
            lines
                .next()
                .ok_or_else(|| BuildError::new("truncated font comments"))?;
        }

        let mut glyphs = HashMap::with_capacity(95);
        for ascii in 32u8..=126 {
            let mut rows = Vec::with_capacity(height);
            for _ in 0..height {
                let row = lines
                    .next()
                    .ok_or_else(|| BuildError::new("truncated ASCII glyph table"))?;
                if row.len() > MAX_GLYPH_ROW_BYTES {
                    return Err(BuildError::new("font glyph row exceeds 4096 bytes"));
                }
                let cleaned = row.trim_end_matches('@').replace(hardblank, " ");
                rows.push(cleaned);
            }
            glyphs.insert(char::from(ascii), rows);
        }
        Ok(Self { height, glyphs })
    }

    fn render(&self, text: &str) -> Result<String, BuildError> {
        if text.is_empty() || text.len() > MAX_BANNER_TEXT_BYTES || !text.is_ascii() {
            return Err(BuildError::new(
                "banner text must be 1..=256 printable ASCII bytes",
            ));
        }
        if !text.bytes().all(|byte| (32..=126).contains(&byte)) {
            return Err(BuildError::new("banner text contains a control character"));
        }
        let mut output = vec![String::new(); self.height];
        for character in text.chars() {
            let glyph = self
                .glyphs
                .get(&character)
                .or_else(|| self.glyphs.get(&character.to_ascii_uppercase()))
                .ok_or_else(|| BuildError::new("font is missing an ASCII glyph"))?;
            for (target, row) in output.iter_mut().zip(glyph) {
                let new_len = target
                    .len()
                    .checked_add(row.len())
                    .ok_or_else(|| BuildError::new("banner size overflow"))?;
                if new_len > MAX_PAGE_BYTES {
                    return Err(BuildError::new("banner exceeds the page-size limit"));
                }
                target.push_str(row);
            }
        }
        while output.last().is_some_and(|row| row.trim_end().is_empty()) && output.len() > 1 {
            output.pop();
        }
        Ok(output
            .iter()
            .map(|row| row.trim_end())
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Alignment {
    Left,
    Center,
    Right,
}

impl Alignment {
    fn parse(value: &str) -> Result<Self, BuildError> {
        match value {
            "left" => Ok(Self::Left),
            "center" => Ok(Self::Center),
            "right" => Ok(Self::Right),
            _ => Err(BuildError::new("align must be left, center, or right")),
        }
    }
}

#[derive(Debug)]
struct MarkerSpec<'a> {
    font: &'a str,
    alignment: Alignment,
    text: &'a str,
}

fn parse_marker(value: &str) -> Result<MarkerSpec<'_>, BuildError> {
    let mut font = DEFAULT_FONT;
    let mut alignment = Alignment::Left;
    let mut saw_font = false;
    let mut saw_alignment = false;
    let mut text_offset = None;

    for part in value.split_whitespace() {
        let offset = part.as_ptr() as usize - value.as_ptr() as usize;
        if let Some(candidate) = part.strip_prefix("font=") {
            if saw_font {
                return Err(BuildError::new("duplicate font option"));
            }
            if candidate.is_empty()
                || !candidate
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
            {
                return Err(BuildError::new("font name must match [A-Za-z0-9_-]+"));
            }
            font = candidate;
            saw_font = true;
        } else if let Some(candidate) = part.strip_prefix("align=") {
            if saw_alignment {
                return Err(BuildError::new("duplicate align option"));
            }
            alignment = Alignment::parse(candidate)?;
            saw_alignment = true;
        } else if part.contains('=') {
            return Err(BuildError::new(format!("unknown FIGlet option: {part}")));
        } else {
            text_offset = Some(offset);
            break;
        }
    }
    let text = text_offset
        .map(|offset| value[offset..].trim())
        .filter(|text| !text.is_empty())
        .ok_or_else(|| BuildError::new("FIGlet marker has no banner text"))?;
    Ok(MarkerSpec {
        font,
        alignment,
        text,
    })
}

fn expand_aml(
    input: &str,
    font_dir: &Path,
    fonts: &mut HashMap<String, FigletFont>,
) -> Result<String, BuildError> {
    let mut output = String::with_capacity(input.len());
    let mut remainder = input;
    let mut marker_count = 0usize;
    while let Some(start) = remainder.find(MARKER) {
        marker_count += 1;
        if marker_count > MAX_MARKERS_PER_PAGE {
            return Err(BuildError::new(
                "page contains more than 128 FIGlet markers",
            ));
        }
        output.push_str(&remainder[..start]);
        let marker_body = &remainder[start + MARKER.len()..];
        let end = marker_body
            .find("}}")
            .ok_or_else(|| BuildError::new("unterminated FIGlet marker"))?;
        let spec = parse_marker(&marker_body[..end])?;
        if !fonts.contains_key(spec.font) {
            let path = font_dir.join(format!("{}.flf", spec.font));
            let data = fs::read_to_string(&path)
                .map_err(|error| BuildError::new(format!("read {}: {error}", path.display())))?;
            fonts.insert(spec.font.to_owned(), FigletFont::parse(&data)?);
        }
        let art = fonts
            .get(spec.font)
            .expect("font inserted above")
            .render(spec.text)?;
        match spec.alignment {
            Alignment::Left => output.push_str("[pre]"),
            Alignment::Center => output.push_str("[pre align=center]"),
            Alignment::Right => output.push_str("[pre align=right]"),
        }
        output.push_str(&art);
        output.push_str("[/pre]");
        if output.len() > MAX_PAGE_BYTES {
            return Err(BuildError::new(
                "generated AML exceeds the 1 MiB page limit",
            ));
        }
        remainder = &marker_body[end + 2..];
    }
    output.push_str(remainder);
    if output.contains("{{figlet") {
        return Err(BuildError::new("malformed FIGlet marker remains in output"));
    }
    if output.len() > MAX_PAGE_BYTES {
        return Err(BuildError::new(
            "generated AML exceeds the 1 MiB page limit",
        ));
    }
    Ok(output)
}

fn copy_tree(
    source: &Path,
    destination: &Path,
    font_dir: &Path,
    fonts: &mut HashMap<String, FigletFont>,
) -> Result<(), BuildError> {
    fs::create_dir_all(destination)
        .map_err(|error| BuildError::new(format!("create {}: {error}", destination.display())))?;
    let mut entries = fs::read_dir(source)
        .map_err(|error| BuildError::new(format!("read {}: {error}", source.display())))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| BuildError::new(format!("read {}: {error}", source.display())))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name();
        let name_text = name.to_string_lossy();
        if name_text.starts_with('.') || name_text == "fonts" || name_text == "dist" {
            continue;
        }
        let file_type = entry.file_type().map_err(|error| {
            BuildError::new(format!("inspect {}: {error}", entry.path().display()))
        })?;
        if file_type.is_symlink() {
            return Err(BuildError::new(format!(
                "site build refuses symlink {}",
                entry.path().display()
            )));
        }
        let target = destination.join(&name);
        if file_type.is_dir() {
            copy_tree(&entry.path(), &target, font_dir, fonts)?;
        } else if file_type.is_file() && entry.path().extension().is_some_and(|ext| ext == "aml") {
            let input = fs::read_to_string(entry.path()).map_err(|error| {
                BuildError::new(format!("read {}: {error}", entry.path().display()))
            })?;
            let expanded = expand_aml(&input, font_dir, fonts)
                .map_err(|error| BuildError::new(format!("{}: {error}", entry.path().display())))?;
            fs::write(&target, expanded)
                .map_err(|error| BuildError::new(format!("write {}: {error}", target.display())))?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), &target).map_err(|error| {
                BuildError::new(format!(
                    "copy {} to {}: {error}",
                    entry.path().display(),
                    target.display()
                ))
            })?;
        }
    }
    Ok(())
}

/// Build `source` into a clean static `output` tree. Fonts, hidden state, and
/// raw template markers never enter the served directory.
pub fn build_site(source: &Path, output: &Path) -> Result<(), BuildError> {
    let source = source
        .canonicalize()
        .map_err(|error| BuildError::new(format!("read {}: {error}", source.display())))?;
    let output_name = output
        .file_name()
        .ok_or_else(|| BuildError::new("output must name a directory"))?;
    let output_parent = output
        .parent()
        .ok_or_else(|| BuildError::new("output must have a parent directory"))?;
    fs::create_dir_all(output_parent)
        .map_err(|error| BuildError::new(format!("create {}: {error}", output_parent.display())))?;
    let output_parent = output_parent.canonicalize().map_err(|error| {
        BuildError::new(format!("resolve {}: {error}", output_parent.display()))
    })?;
    let output = output_parent.join(output_name);
    if output == source || output.starts_with(&source) {
        return Err(BuildError::new("output must be outside the source tree"));
    }

    let staging = output_parent.join(format!(
        ".dustnet-site-build-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| BuildError::new("system clock precedes Unix epoch"))?
            .as_nanos()
    ));
    let result = copy_tree(
        &source,
        &staging,
        &source.join("fonts"),
        &mut HashMap::new(),
    );
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    if output.exists() {
        fs::remove_dir_all(&output)
            .map_err(|error| BuildError::new(format!("remove {}: {error}", output.display())))?;
    }
    fs::rename(&staging, &output).map_err(|error| {
        BuildError::new(format!(
            "install {} as {}: {error}",
            staging.display(),
            output.display()
        ))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_row_font() -> String {
        let mut font = String::from("flf2a$ 1 1 1 0 0\n");
        for ascii in 32u8..=126 {
            font.push(char::from(ascii));
            font.push_str("@\n");
        }
        font
    }

    #[test]
    fn expands_left_and_center_markers_to_static_pre() {
        let dir = std::env::temp_dir().join(format!("dustnet-figlet-font-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("ANSI_Shadow.flf"), one_row_font()).unwrap();
        let mut fonts = HashMap::new();
        let output = expand_aml(
            "[page]{{figlet:ABC}} {{figlet:align=center XYZ}}[/page]",
            &dir,
            &mut fonts,
        )
        .unwrap();
        assert_eq!(
            output,
            "[page][pre]ABC[/pre] [pre align=center]XYZ[/pre][/page]"
        );
        assert!(!output.contains(MARKER));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_untrusted_options_and_malformed_fonts() {
        assert!(parse_marker("font=../secret TITLE").is_err());
        assert!(parse_marker("align=justify TITLE").is_err());
        assert!(parse_marker("color=red TITLE").is_err());
        assert!(FigletFont::parse("not a font").is_err());
        assert!(FigletFont::parse("flf2a$ 65 1 1 0 0\n").is_err());
    }

    #[test]
    fn site_build_is_clean_deterministic_and_excludes_fonts_and_hidden_state() {
        let root = std::env::temp_dir().join(format!("dustnet-site-build-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let source = root.join("source");
        let output = root.join("output");
        fs::create_dir_all(source.join("fonts")).unwrap();
        fs::create_dir_all(source.join(".auth")).unwrap();
        fs::write(source.join("fonts/ANSI_Shadow.flf"), one_row_font()).unwrap();
        fs::write(source.join(".auth/state.tsv"), "secret").unwrap();
        fs::write(
            source.join("index.aml"),
            "[page mode=document]{{figlet:TITLE}}[/page]",
        )
        .unwrap();
        fs::write(source.join("effect.wasm"), b"wasm").unwrap();

        build_site(&source, &output).unwrap();
        let first = fs::read(output.join("index.aml")).unwrap();
        assert_eq!(first, b"[page mode=document][pre]TITLE[/pre][/page]");
        assert_eq!(fs::read(output.join("effect.wasm")).unwrap(), b"wasm");
        assert!(!output.join("fonts").exists());
        assert!(!output.join(".auth").exists());
        build_site(&source, &output).unwrap();
        assert_eq!(fs::read(output.join("index.aml")).unwrap(), first);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_build_preserves_the_previous_output() {
        let root = std::env::temp_dir().join(format!("dustnet-site-fail-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let source = root.join("source");
        let output = root.join("output");
        fs::create_dir_all(source.join("fonts")).unwrap();
        fs::create_dir_all(&output).unwrap();
        fs::write(output.join("index.aml"), "old").unwrap();
        fs::write(source.join("index.aml"), "{{figlet:unterminated").unwrap();

        assert!(build_site(&source, &output).is_err());
        assert_eq!(fs::read_to_string(output.join("index.aml")).unwrap(), "old");
        fs::remove_dir_all(root).unwrap();
    }
}
