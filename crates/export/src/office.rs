//! Excel workbooks and PowerPoint presentations built from data (not from page layout):
//! tables extracted from a document, and slide outlines. Minimal Office Open XML packages
//! (ISO/IEC 29500) that Excel, PowerPoint, LibreOffice and Google's importers open.

use crate::{Zip, esc};

/// One cell of a [`Sheet`].
#[derive(Clone, Debug, PartialEq)]
pub enum Cell {
    Empty,
    Text(String),
    /// Stored as a number, so formulas and sums work. Non-finite values are written as text.
    Number(f64),
}

/// One worksheet: its name and rows. The first row is set in bold (the header).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sheet {
    pub name: String,
    pub rows: Vec<Vec<Cell>>,
}

/// One slide of [`pptx`]: a title and its bullet points.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Slide {
    pub title: String,
    pub bullets: Vec<String>,
}

/// Excel's limits (rows, columns, characters in a cell, worksheet name length).
const MAX_ROWS: usize = 1_048_576;
const MAX_COLS: usize = 16_384;
const MAX_CELL_CHARS: usize = 32_767;
const MAX_SHEET_NAME: usize = 31;
/// Sheets and slides beyond this are refused by the callers' limits; this keeps the zip's
/// 16-bit entry count safe.
pub const MAX_PARTS: usize = 4_000;

/// The column letters of a 0-based column index (0 → A, 26 → AA).
fn column(mut i: usize) -> String {
    let mut s = Vec::new();
    loop {
        s.push(b'A' + (i % 26) as u8);
        if i < 26 {
            break;
        }
        i = i / 26 - 1;
    }
    s.reverse();
    String::from_utf8(s).unwrap_or_default()
}

/// Excel refuses `[]:*?/\` in sheet names, names over 31 characters, empty names and
/// duplicates (case-insensitively).
fn sheet_names(sheets: &[Sheet]) -> Vec<String> {
    let mut used: Vec<String> = Vec::new();
    sheets
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let clean: String = s.name.chars().filter(|c| !matches!(c, '[' | ']' | ':' | '*' | '?' | '/' | '\\') && !c.is_control()).collect();
            let clean = clean.trim().trim_matches('\'').to_string();
            let base: String = if clean.is_empty() { format!("Sheet{}", i + 1) } else { clean.chars().take(MAX_SHEET_NAME).collect() };
            let mut name = base.clone();
            let mut n = 2usize;
            while used.iter().any(|u| u.eq_ignore_ascii_case(&name)) {
                let suffix = format!(" ({n})");
                let keep = MAX_SHEET_NAME.saturating_sub(suffix.chars().count());
                name = format!("{}{suffix}", base.chars().take(keep).collect::<String>());
                n = n.saturating_add(1);
            }
            used.push(name.clone());
            name
        })
        .collect()
}

fn sheet_xml(sheet: &Sheet) -> String {
    let mut x = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><sheetData>",
    );
    for (r, row) in sheet.rows.iter().take(MAX_ROWS).enumerate() {
        let rn = r + 1;
        x.push_str(&format!("<row r=\"{rn}\">"));
        let style = if r == 0 { " s=\"1\"" } else { "" };
        for (c, cell) in row.iter().take(MAX_COLS).enumerate() {
            let at = format!("{}{rn}", column(c));
            match cell {
                Cell::Empty => {}
                Cell::Number(v) if v.is_finite() => x.push_str(&format!("<c r=\"{at}\"{style}><v>{v}</v></c>")),
                Cell::Number(v) => x.push_str(&format!("<c r=\"{at}\" t=\"inlineStr\"{style}><is><t>{v}</t></is></c>")),
                Cell::Text(t) => {
                    let t: String = t.chars().take(MAX_CELL_CHARS).collect();
                    x.push_str(&format!("<c r=\"{at}\" t=\"inlineStr\"{style}><is><t xml:space=\"preserve\">{}</t></is></c>", esc(&t)));
                }
            }
        }
        x.push_str("</row>");
    }
    x.push_str("</sheetData></worksheet>");
    x
}

const XLSX_STYLES: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
<styleSheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\">\
<fonts count=\"2\"><font><sz val=\"11\"/><name val=\"Calibri\"/></font><font><b/><sz val=\"11\"/><name val=\"Calibri\"/></font></fonts>\
<fills count=\"2\"><fill><patternFill patternType=\"none\"/></fill><fill><patternFill patternType=\"gray125\"/></fill></fills>\
<borders count=\"1\"><border><left/><right/><top/><bottom/><diagonal/></border></borders>\
<cellStyleXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\"/></cellStyleXfs>\
<cellXfs count=\"2\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\"/><xf numFmtId=\"0\" fontId=\"1\" fillId=\"0\" borderId=\"0\" xfId=\"0\" applyFont=\"1\"/></cellXfs>\
<cellStyles count=\"1\"><cellStyle name=\"Normal\" xfId=\"0\" builtinId=\"0\"/></cellStyles>\
</styleSheet>";

/// An Excel workbook (.xlsx), one worksheet per sheet (at least one, empty if none is given).
/// Rows and columns past Excel's limits, and sheets past [`MAX_PARTS`], are left out.
pub fn xlsx(sheets: &[Sheet]) -> Vec<u8> {
    let empty = [Sheet { name: "Sheet1".into(), rows: Vec::new() }];
    let sheets = if sheets.is_empty() { &empty[..] } else { &sheets[..sheets.len().min(MAX_PARTS)] };
    let names = sheet_names(sheets);
    let mut z = Zip::default();
    let mut types = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
<Default Extension=\"xml\" ContentType=\"application/xml\"/>\
<Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/>\
<Override PartName=\"/xl/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml\"/>",
    );
    let mut workbook = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><sheets>",
    );
    let mut rels = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">",
    );
    for (i, (sheet, name)) in sheets.iter().zip(&names).enumerate() {
        let n = i + 1;
        types.push_str(&format!(
            "<Override PartName=\"/xl/worksheets/sheet{n}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/>"
        ));
        workbook.push_str(&format!("<sheet name=\"{}\" sheetId=\"{n}\" r:id=\"rId{n}\"/>", esc(name)));
        rels.push_str(&format!(
            "<Relationship Id=\"rId{n}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"worksheets/sheet{n}.xml\"/>"
        ));
        z.add(&format!("xl/worksheets/sheet{n}.xml"), sheet_xml(sheet).as_bytes(), true);
    }
    rels.push_str(&format!(
        "<Relationship Id=\"rId{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\"/></Relationships>",
        sheets.len() + 1
    ));
    types.push_str("</Types>");
    workbook.push_str("</sheets></workbook>");
    z.add("[Content_Types].xml", types.as_bytes(), true);
    z.add(
        "_rels/.rels",
        b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"xl/workbook.xml\"/></Relationships>",
        true,
    );
    z.add("xl/workbook.xml", workbook.as_bytes(), true);
    z.add("xl/_rels/workbook.xml.rels", rels.as_bytes(), true);
    z.add("xl/styles.xml", XLSX_STYLES.as_bytes(), true);
    z.finish()
}

// ── PowerPoint ──────────────────────────────────────────────────────────────────────────────

const P_NS: &str = "xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\"";
const EMPTY_TREE: &str = "<p:nvGrpSpPr><p:cNvPr id=\"1\" name=\"\"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"0\" cy=\"0\"/><a:chOff x=\"0\" y=\"0\"/><a:chExt cx=\"0\" cy=\"0\"/></a:xfrm></p:grpSpPr>";
const CLR_MAP: &str = "<p:clrMap bg1=\"lt1\" tx1=\"dk1\" bg2=\"lt2\" tx2=\"dk2\" accent1=\"accent1\" accent2=\"accent2\" accent3=\"accent3\" accent4=\"accent4\" accent5=\"accent5\" accent6=\"accent6\" hlink=\"hlink\" folHlink=\"folHlink\"/>";
/// 16:9 widescreen, in EMU (13.333 × 7.5 in).
const SLIDE_CX: i64 = 12_192_000;
const SLIDE_CY: i64 = 6_858_000;
const REL_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

fn theme_xml() -> String {
    let fill = "<a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill>";
    let line = "<a:ln w=\"9525\"><a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill></a:ln>";
    let effect = "<a:effectStyle><a:effectLst/></a:effectStyle>";
    let colors = [
        ("dk1", "000000"),
        ("lt1", "FFFFFF"),
        ("dk2", "1F2A30"),
        ("lt2", "EEF2F1"),
        ("accent1", "0A7563"),
        ("accent2", "12A58A"),
        ("accent3", "E3A21A"),
        ("accent4", "4472C4"),
        ("accent5", "A5A5A5"),
        ("accent6", "C0504D"),
        ("hlink", "0563C1"),
        ("folHlink", "954F72"),
    ];
    let scheme: String = colors.iter().map(|(n, c)| format!("<a:{n}><a:srgbClr val=\"{c}\"/></a:{n}>")).collect();
    let font = |face: &str| format!("<a:latin typeface=\"{face}\"/><a:ea typeface=\"\"/><a:cs typeface=\"\"/>");
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<a:theme xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" name=\"PdfCraft\"><a:themeElements>\
<a:clrScheme name=\"PdfCraft\">{scheme}</a:clrScheme>\
<a:fontScheme name=\"PdfCraft\"><a:majorFont>{}</a:majorFont><a:minorFont>{}</a:minorFont></a:fontScheme>\
<a:fmtScheme name=\"PdfCraft\"><a:fillStyleLst>{fill}{fill}{fill}</a:fillStyleLst><a:lnStyleLst>{line}{line}{line}</a:lnStyleLst>\
<a:effectStyleLst>{effect}{effect}{effect}</a:effectStyleLst><a:bgFillStyleLst>{fill}{fill}{fill}</a:bgFillStyleLst></a:fmtScheme>\
</a:themeElements></a:theme>",
        font("Calibri Light"),
        font("Calibri")
    )
}

/// A text box shape: `paras` are (text, size in hundredths of a point, bold, bullet).
fn text_box(id: usize, name: &str, (x, y, cx, cy): (i64, i64, i64, i64), paras: &[(&str, u32, bool, bool)], center: bool) -> String {
    let mut body = String::new();
    for (text, size, bold, bullet) in paras {
        let algn = if center { " algn=\"ctr\"" } else { "" };
        let ppr = if *bullet {
            format!(
                "<a:pPr marL=\"342900\" indent=\"-342900\"{algn}><a:spcBef><a:spcPts val=\"600\"/></a:spcBef><a:buClr><a:schemeClr val=\"accent1\"/></a:buClr><a:buFont typeface=\"Arial\"/><a:buChar char=\"&#8226;\"/></a:pPr>"
            )
        } else {
            format!("<a:pPr{algn}><a:buNone/></a:pPr>")
        };
        let b = if *bold { " b=\"1\"" } else { "" };
        body.push_str(&format!("<a:p>{ppr}<a:r><a:rPr lang=\"en-US\" sz=\"{size}\"{b} dirty=\"0\"/><a:t>{}</a:t></a:r></a:p>", esc(text)));
    }
    if body.is_empty() {
        body.push_str("<a:p><a:endParaRPr lang=\"en-US\"/></a:p>");
    }
    format!(
        "<p:sp><p:nvSpPr><p:cNvPr id=\"{id}\" name=\"{name}\"/><p:cNvSpPr txBox=\"1\"/><p:nvPr/></p:nvSpPr>\
<p:spPr><a:xfrm><a:off x=\"{x}\" y=\"{y}\"/><a:ext cx=\"{cx}\" cy=\"{cy}\"/></a:xfrm><a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom><a:noFill/></p:spPr>\
<p:txBody><a:bodyPr wrap=\"square\" rtlCol=\"0\"><a:normAutofit/></a:bodyPr><a:lstStyle/>{body}</p:txBody></p:sp>"
    )
}

fn accent_bar(id: usize, (x, y, cx, cy): (i64, i64, i64, i64)) -> String {
    format!(
        "<p:sp><p:nvSpPr><p:cNvPr id=\"{id}\" name=\"Accent\"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>\
<p:spPr><a:xfrm><a:off x=\"{x}\" y=\"{y}\"/><a:ext cx=\"{cx}\" cy=\"{cy}\"/></a:xfrm><a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom><a:solidFill><a:schemeClr val=\"accent1\"/></a:solidFill><a:ln><a:noFill/></a:ln></p:spPr></p:sp>"
    )
}

fn slide_xml(shapes: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<p:sld {P_NS}><p:cSld><p:spTree>{EMPTY_TREE}{shapes}</p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sld>"
    )
}

fn rels(entries: &[(String, &str, String)]) -> String {
    let mut x = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">",
    );
    for (id, kind, target) in entries {
        x.push_str(&format!("<Relationship Id=\"{id}\" Type=\"{REL_NS}/{kind}\" Target=\"{target}\"/>"));
    }
    x.push_str("</Relationships>");
    x
}

/// A PowerPoint presentation (.pptx, 16:9): a title slide, then one slide per entry with its
/// title and bullets. Text boxes shrink their text to fit (PowerPoint's "shrink text on
/// overflow"). Slides past [`MAX_PARTS`] are left out.
pub fn pptx(title: &str, subtitle: &str, slides: &[Slide]) -> Vec<u8> {
    let slides = &slides[..slides.len().min(MAX_PARTS)];
    let margin = 609_600; // 0.667 in
    let width = SLIDE_CX - 2 * margin;
    let mut all = vec![slide_xml(&format!(
        "{}{}{}",
        text_box(2, "Title", (margin, 2_130_000, width, 1_470_000), &[(title, 4000, true, false)], true),
        text_box(3, "Subtitle", (margin, 3_700_000, width, 900_000), &[(subtitle, 2200, false, false)], true),
        accent_bar(4, (0, SLIDE_CY - 152_400, SLIDE_CX, 152_400)),
    ))];
    for s in slides {
        let bullets: Vec<(&str, u32, bool, bool)> = s.bullets.iter().map(|b| (b.as_str(), 2000, false, true)).collect();
        all.push(slide_xml(&format!(
            "{}{}{}",
            text_box(2, "Title", (margin, 380_000, width, 1_000_000), &[(s.title.as_str(), 3000, true, false)], false),
            accent_bar(3, (margin + 91_440, 1_400_000, 1_219_200, 50_800)),
            text_box(4, "Content", (margin, 1_600_000, width, SLIDE_CY - 1_600_000 - 500_000), &bullets, false),
        )));
    }
    let mut z = Zip::default();
    let mut types = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
<Default Extension=\"xml\" ContentType=\"application/xml\"/>\
<Override PartName=\"/ppt/presentation.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml\"/>\
<Override PartName=\"/ppt/slideMasters/slideMaster1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml\"/>\
<Override PartName=\"/ppt/slideLayouts/slideLayout1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml\"/>\
<Override PartName=\"/ppt/theme/theme1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.theme+xml\"/>",
    );
    let mut ids = String::new();
    let mut pres_rels = vec![
        ("rId1".to_string(), "slideMaster", "slideMasters/slideMaster1.xml".to_string()),
        ("rId2".to_string(), "theme", "theme/theme1.xml".to_string()),
    ];
    for (i, xml) in all.iter().enumerate() {
        let n = i + 1;
        types.push_str(&format!(
            "<Override PartName=\"/ppt/slides/slide{n}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slide+xml\"/>"
        ));
        ids.push_str(&format!("<p:sldId id=\"{}\" r:id=\"rId{}\"/>", 255 + n, n + 2));
        pres_rels.push((format!("rId{}", n + 2), "slide", format!("slides/slide{n}.xml")));
        z.add(&format!("ppt/slides/slide{n}.xml"), xml.as_bytes(), true);
        z.add(
            &format!("ppt/slides/_rels/slide{n}.xml.rels"),
            rels(&[("rId1".into(), "slideLayout", "../slideLayouts/slideLayout1.xml".into())]).as_bytes(),
            true,
        );
    }
    types.push_str("</Types>");
    z.add("[Content_Types].xml", types.as_bytes(), true);
    z.add("_rels/.rels", rels(&[("rId1".into(), "officeDocument", "ppt/presentation.xml".into())]).as_bytes(), true);
    z.add(
        "ppt/presentation.xml",
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<p:presentation {P_NS} saveSubsetFonts=\"1\">\
<p:sldMasterIdLst><p:sldMasterId id=\"2147483648\" r:id=\"rId1\"/></p:sldMasterIdLst><p:sldIdLst>{ids}</p:sldIdLst>\
<p:sldSz cx=\"{SLIDE_CX}\" cy=\"{SLIDE_CY}\"/><p:notesSz cx=\"6858000\" cy=\"9144000\"/></p:presentation>"
        )
        .as_bytes(),
        true,
    );
    z.add("ppt/_rels/presentation.xml.rels", rels(&pres_rels).as_bytes(), true);
    z.add(
        "ppt/slideMasters/slideMaster1.xml",
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<p:sldMaster {P_NS}><p:cSld><p:bg><p:bgRef idx=\"1001\"><a:schemeClr val=\"bg1\"/></p:bgRef></p:bg><p:spTree>{EMPTY_TREE}</p:spTree></p:cSld>{CLR_MAP}\
<p:sldLayoutIdLst><p:sldLayoutId id=\"2147483649\" r:id=\"rId1\"/></p:sldLayoutIdLst></p:sldMaster>"
        )
        .as_bytes(),
        true,
    );
    z.add(
        "ppt/slideMasters/_rels/slideMaster1.xml.rels",
        rels(&[("rId1".into(), "slideLayout", "../slideLayouts/slideLayout1.xml".into()), ("rId2".into(), "theme", "../theme/theme1.xml".into())])
            .as_bytes(),
        true,
    );
    z.add(
        "ppt/slideLayouts/slideLayout1.xml",
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<p:sldLayout {P_NS} type=\"blank\" preserve=\"1\"><p:cSld name=\"Blank\"><p:spTree>{EMPTY_TREE}</p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sldLayout>"
        )
        .as_bytes(),
        true,
    );
    z.add(
        "ppt/slideLayouts/_rels/slideLayout1.xml.rels",
        rels(&[("rId1".into(), "slideMaster", "../slideMasters/slideMaster1.xml".into())]).as_bytes(),
        true,
    );
    z.add("ppt/theme/theme1.xml", theme_xml().as_bytes(), true);
    z.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The text of a stored or deflated part of a zip written by [`Zip`].
    fn part(zip: &[u8], name: &str) -> String {
        crate::tests::part(zip, name)
    }

    #[test]
    fn column_letters_follow_excel() {
        assert_eq!(column(0), "A");
        assert_eq!(column(25), "Z");
        assert_eq!(column(26), "AA");
        assert_eq!(column(701), "ZZ");
        assert_eq!(column(702), "AAA");
        assert_eq!(column(MAX_COLS - 1), "XFD");
    }

    #[test]
    fn workbook_keeps_numbers_as_numbers_and_escapes_text() {
        let sheet = Sheet {
            name: "Receitas".into(),
            rows: vec![
                vec![Cell::Text("Mês".into()), Cell::Text("Valor".into())],
                vec![Cell::Text("Jan <&>".into()), Cell::Number(1234.56)],
                vec![Cell::Empty, Cell::Number(f64::NAN)],
            ],
        };
        let x = xlsx(&[sheet]);
        let s = part(&x, "xl/worksheets/sheet1.xml");
        assert!(s.contains("<c r=\"B2\"><v>1234.56</v></c>"), "{s}");
        assert!(s.contains("Jan &lt;&amp;&gt;"));
        assert!(s.contains("<c r=\"A1\" t=\"inlineStr\" s=\"1\">"), "header in bold");
        assert!(!s.contains("NaN</v>"), "a non-finite number is not written as a number");
        assert!(part(&x, "xl/workbook.xml").contains("name=\"Receitas\""));
    }

    #[test]
    fn sheet_names_are_made_valid_and_unique() {
        let names = sheet_names(&[
            Sheet { name: "a/b:c".into(), rows: vec![] },
            Sheet { name: "ABC".into(), rows: vec![] },
            Sheet { name: "abc".into(), rows: vec![] },
            Sheet { name: String::new(), rows: vec![] },
            Sheet { name: "x".repeat(40), rows: vec![] },
            Sheet { name: "x".repeat(40), rows: vec![] },
        ]);
        assert_eq!(names[0], "abc");
        assert_eq!(names[1], "ABC (2)");
        assert_eq!(names[2], "abc (3)");
        assert_eq!(names[3], "Sheet4");
        assert_eq!(names[4].chars().count(), 31);
        assert_eq!(names[5].chars().count(), 31);
        assert_ne!(names[4], names[5]);
    }

    #[test]
    fn an_empty_workbook_still_has_one_sheet() {
        let x = xlsx(&[]);
        assert!(part(&x, "xl/workbook.xml").contains("sheetId=\"1\""));
    }

    #[test]
    fn presentation_has_a_title_slide_and_one_slide_each() {
        let p = pptx("Relatório", "2026", &[Slide { title: "Receitas".into(), bullets: vec!["Alta de 8%".into(), "<R&D>".into()] }]);
        let pres = part(&p, "ppt/presentation.xml");
        assert_eq!(pres.matches("<p:sldId ").count(), 2);
        let s2 = part(&p, "ppt/slides/slide2.xml");
        assert!(s2.contains("Receitas") && s2.contains("&lt;R&amp;D&gt;"), "{s2}");
        assert!(part(&p, "[Content_Types].xml").contains("/ppt/slides/slide2.xml"));
    }
}
