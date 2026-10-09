//! pdfcraft-fonts — font metrics and encodings for generated appearances (L2).
//!
//! See the README: the metrics are approximations by character class (no vendor metrics files
//! are bundled). The full font subsystem lands in M2.2/M7.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod craft;
mod encodings;
pub mod pdf;
mod script;
pub use craft::{
    CRAFT_FONTS, CraftFont, SHIPPORI_MINCHO, document_japanese_font, document_japanese_font_for_style, ui_arabic_fonts, ui_chinese_fonts,
    ui_cjk_fonts, ui_japanese_fonts, ui_telugu_fonts,
};
pub use script::{GlyphError, GlyphOutline, MAX_SIGNATURE_CHARS, ScriptOutline, japanese_glyph, japanese_glyph_from, script_outline};

/// Embedded Latin UI faces from `assets/fonts/`, also used as renderer fallbacks.
pub const INTER_REGULAR: &[u8] = include_bytes!("../../../assets/fonts/Inter-Regular.ttf");
pub const INTER_SEMIBOLD: &[u8] = include_bytes!("../../../assets/fonts/Inter-SemiBold.ttf");
pub const JETBRAINS_MONO: &[u8] = include_bytes!("../../../assets/fonts/JetBrainsMono-Regular.ttf");

/// Approximate advance of `s` in Helvetica (or Arial) at `size` points.
pub fn helvetica_width(s: &str, size: f64) -> f64 {
    let units: f64 = s
        .chars()
        .map(|c| match c {
            ' ' | 'i' | 'j' | 'l' | '\'' | '!' | '|' | '.' | ',' | ':' | ';' | 'I' => 260.0,
            'f' | 't' | 'r' | '(' | ')' | '[' | ']' | '/' | '-' | '"' => 333.0,
            'm' => 833.0,
            'w' => 722.0,
            'M' => 833.0,
            'W' => 944.0,
            'J' | 'c' | 'k' | 's' | 'v' | 'x' | 'y' | 'z' => 500.0,
            '0'..='9' | 'a'..='z' | '$' | '#' | '?' | '_' => 556.0,
            'A'..='Z' => 680.0,
            '@' => 1015.0,
            _ if c.is_whitespace() => 260.0,
            _ => 584.0,
        })
        .sum();
    units * size / 1000.0
}

/// Greedy line breaking within `width` points (paragraphs split on newlines; words longer
/// than a line are broken by character).
pub fn wrap(text: &str, size: f64, width: f64) -> Vec<String> {
    let mut lines = Vec::new();
    for para in text.split(['\n', '\r']) {
        let mut line = String::new();
        for word in para.split(' ') {
            let candidate = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
            if helvetica_width(&candidate, size) <= width || line.is_empty() && helvetica_width(word, size) <= width {
                line = candidate;
                continue;
            }
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
            }
            for ch in word.chars() {
                if !line.is_empty() && helvetica_width(&format!("{line}{ch}"), size) > width {
                    lines.push(std::mem::take(&mut line));
                }
                line.push(ch);
            }
        }
        lines.push(line);
    }
    lines
}

/// Encode text in WinAnsiEncoding (ISO 32000-2 Annex D); unmappable characters become `?`.
pub fn win_ansi(s: &str) -> Vec<u8> {
    s.chars()
        .map(|c| match c {
            '\u{20}'..='\u{7e}' => c as u8,
            '\u{a0}'..='\u{ff}' => c as u32 as u8,
            '€' => 0x80,
            '‚' => 0x82,
            'ƒ' => 0x83,
            '„' => 0x84,
            '…' => 0x85,
            '†' => 0x86,
            '‡' => 0x87,
            'ˆ' => 0x88,
            '‰' => 0x89,
            'Š' => 0x8a,
            '‹' => 0x8b,
            'Œ' => 0x8c,
            'Ž' => 0x8e,
            '‘' => 0x91,
            '’' => 0x92,
            '“' => 0x93,
            '”' => 0x94,
            '•' => 0x95,
            '–' => 0x96,
            '—' => 0x97,
            '˜' => 0x98,
            '™' => 0x99,
            'š' => 0x9a,
            '›' => 0x9b,
            'œ' => 0x9c,
            'ž' => 0x9e,
            'Ÿ' => 0x9f,
            '\t' => b' ',
            _ => b'?',
        })
        .collect()
}

/// Returns the Adobe Glyph List name for a Unicode character if known.
pub fn unicode_to_adobe_glyph_name(c: char) -> Option<&'static str> {
    match c {
        // Romanian diacritics
        'ă' => Some("abreve"),
        'Ă' => Some("Abreve"),
        'ș' => Some("scommaaccent"),
        'Ș' => Some("Scommaaccent"),
        'ț' => Some("tcommaaccent"),
        'Ț' => Some("Tcommaaccent"),
        'ş' => Some("scedilla"),
        'Ş' => Some("Scedilla"),
        'ţ' => Some("tcedilla"),
        'Ţ' => Some("Tcedilla"),

        // Central & Eastern European, Baltic, Turkish
        'ā' => Some("amacron"),
        'Ā' => Some("Amacron"),
        'ą' => Some("aogonek"),
        'Ą' => Some("Aogonek"),
        'ć' => Some("cacute"),
        'Ć' => Some("Cacute"),
        'č' => Some("ccaron"),
        'Č' => Some("Ccaron"),
        'ď' => Some("dcaron"),
        'Ď' => Some("Dcaron"),
        'đ' => Some("dcroat"),
        'Đ' => Some("Dcroat"),
        'ē' => Some("emacron"),
        'Ē' => Some("Emacron"),
        'ė' => Some("edotaccent"),
        'Ė' => Some("Edotaccent"),
        'ę' => Some("eogonek"),
        'Ę' => Some("Eogonek"),
        'ě' => Some("ecaron"),
        'Ě' => Some("Ecaron"),
        'ğ' => Some("gbreve"),
        'Ğ' => Some("Gbreve"),
        'ģ' => Some("gcommaaccent"),
        'Ģ' => Some("Gcommaaccent"),
        'ī' => Some("imacron"),
        'Ī' => Some("Imacron"),
        'ı' => Some("dotlessi"),
        'İ' => Some("Idotaccent"),
        'į' => Some("iogonek"),
        'Į' => Some("Iogonek"),
        'ķ' => Some("kcommaaccent"),
        'Ķ' => Some("Kcommaaccent"),
        'ĺ' => Some("lacute"),
        'Ĺ' => Some("Lacute"),
        'ľ' => Some("lcaron"),
        'Ľ' => Some("Lcaron"),
        'ļ' => Some("lcommaaccent"),
        'Ļ' => Some("Lcommaaccent"),
        'ł' => Some("lslash"),
        'Ł' => Some("Lslash"),
        'ń' => Some("nacute"),
        'Ń' => Some("Nacute"),
        'ň' => Some("ncaron"),
        'Ň' => Some("Ncaron"),
        'ņ' => Some("ncommaaccent"),
        'Ņ' => Some("Ncommaaccent"),
        'ō' => Some("omacron"),
        'Ō' => Some("Omacron"),
        'ő' => Some("ohungarumlaut"),
        'Ő' => Some("Ohungarumlaut"),
        'ŕ' => Some("racute"),
        'Ŕ' => Some("Racute"),
        'ř' => Some("rcaron"),
        'Ř' => Some("Rcaron"),
        'ŗ' => Some("rcommaaccent"),
        'Ŗ' => Some("Rcommaaccent"),
        'ś' => Some("sacute"),
        'Ś' => Some("Sacute"),
        'ť' => Some("tcaron"),
        'Ť' => Some("Tcaron"),
        'ū' => Some("umacron"),
        'Ū' => Some("Umacron"),
        'ů' => Some("uring"),
        'Ů' => Some("Uring"),
        'ű' => Some("uhungarumlaut"),
        'Ű' => Some("Uhungarumlaut"),
        'ų' => Some("uogonek"),
        'Ų' => Some("Uogonek"),
        'ź' => Some("zacute"),
        'Ź' => Some("Zacute"),
        'ż' => Some("zdotaccent"),
        'Ż' => Some("Zdotaccent"),
        _ => None,
    }
}

/// Returns the Unicode character for an extended Adobe Glyph List name if known.
pub fn adobe_glyph_name_to_unicode(name: &str) -> Option<char> {
    match name {
        "abreve" => Some('ă'),
        "Abreve" => Some('Ă'),
        "scommaaccent" => Some('ș'),
        "Scommaaccent" => Some('Ș'),
        "tcommaaccent" => Some('ț'),
        "Tcommaaccent" => Some('Ț'),
        "scedilla" => Some('ş'),
        "Scedilla" => Some('Ş'),
        "tcedilla" => Some('ţ'),
        "Tcedilla" => Some('Ţ'),
        "amacron" => Some('ā'),
        "Amacron" => Some('Ā'),
        "aogonek" => Some('ą'),
        "Aogonek" => Some('Ą'),
        "cacute" => Some('ć'),
        "Cacute" => Some('Ć'),
        "ccaron" => Some('č'),
        "Ccaron" => Some('Č'),
        "dcaron" => Some('ď'),
        "Dcaron" => Some('Ď'),
        "dcroat" => Some('đ'),
        "Dcroat" => Some('Đ'),
        "emacron" => Some('ē'),
        "Emacron" => Some('Ē'),
        "edotaccent" => Some('ė'),
        "Edotaccent" => Some('Ė'),
        "eogonek" => Some('ę'),
        "Eogonek" => Some('Ę'),
        "ecaron" => Some('ě'),
        "Ecaron" => Some('Ě'),
        "gbreve" => Some('ğ'),
        "Gbreve" => Some('Ğ'),
        "gcommaaccent" => Some('ģ'),
        "Gcommaaccent" => Some('Ģ'),
        "imacron" => Some('ī'),
        "Imacron" => Some('Ī'),
        "dotlessi" => Some('ı'),
        "Idotaccent" => Some('İ'),
        "iogonek" => Some('į'),
        "Iogonek" => Some('Į'),
        "kcommaaccent" => Some('ķ'),
        "Kcommaaccent" => Some('Ķ'),
        "lacute" => Some('ĺ'),
        "Lacute" => Some('Ĺ'),
        "lcaron" => Some('ľ'),
        "Lcaron" => Some('Ľ'),
        "lcommaaccent" => Some('ļ'),
        "Lcommaaccent" => Some('Ļ'),
        "lslash" => Some('ł'),
        "Lslash" => Some('Ł'),
        "nacute" => Some('ń'),
        "Nacute" => Some('Ń'),
        "ncaron" => Some('ň'),
        "Ncaron" => Some('Ň'),
        "ncommaaccent" => Some('ņ'),
        "Ncommaaccent" => Some('Ņ'),
        "omacron" => Some('ō'),
        "Omacron" => Some('Ō'),
        "ohungarumlaut" => Some('ő'),
        "Ohungarumlaut" => Some('Ő'),
        "racute" => Some('ŕ'),
        "Racute" => Some('Ŕ'),
        "rcaron" => Some('ř'),
        "Rcaron" => Some('Ř'),
        "rcommaaccent" => Some('ŗ'),
        "Rcommaaccent" => Some('Ŗ'),
        "sacute" => Some('ś'),
        "Sacute" => Some('Ś'),
        "tcaron" => Some('ť'),
        "Tcaron" => Some('Ť'),
        "umacron" => Some('ū'),
        "Umacron" => Some('Ū'),
        "uring" => Some('ů'),
        "Uring" => Some('Ů'),
        "uhungarumlaut" => Some('ű'),
        "Uhungarumlaut" => Some('Ű'),
        "uogonek" => Some('ų'),
        "Uogonek" => Some('Ų'),
        "zacute" => Some('ź'),
        "Zacute" => Some('Ź'),
        "zdotaccent" => Some('ż'),
        "Zdotaccent" => Some('Ż'),
        _ => None,
    }
}

/// Bytes as a PDF literal string, `(` … `)`, with delimiters escaped.
pub fn literal(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 2);
    out.push(b'(');
    for &b in bytes {
        match b {
            b'(' | b')' | b'\\' => out.extend_from_slice(&[b'\\', b]),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\n' => out.extend_from_slice(b"\\n"),
            1..=31 => {
                let mut buf = *b"\\000";
                buf[1] = b'0' + (b >> 6);
                buf[2] = b'0' + ((b >> 3) & 7);
                buf[3] = b'0' + (b & 7);
                out.extend_from_slice(&buf);
            }
            _ => out.push(b),
        }
    }
    out.push(b')');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths_wrap_and_encode() {
        assert!(helvetica_width("MMMM", 10.0) > helvetica_width("iiii", 10.0) * 2.0);
        assert_eq!(helvetica_width("", 12.0), 0.0);
        let lines = wrap("the quick brown fox jumps over the lazy dog", 12.0, 80.0);
        assert!(lines.len() > 2 && lines.iter().all(|l| helvetica_width(l, 12.0) <= 80.0));
        assert_eq!(wrap("a\nb", 12.0, 100.0), ["a", "b"]);
        let long = wrap("Supercalifragilisticexpialidocious", 12.0, 40.0);
        assert!(long.len() > 3 && long.concat() == "Supercalifragilisticexpialidocious");
        assert_eq!(win_ansi("Café — 5€ ☃"), b"Caf\xe9 \x97 5\x80 ?");
        assert_eq!(literal(b"a(b)\\c"), b"(a\\(b\\)\\\\c)");
    }
}
