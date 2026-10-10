//! `pdfcraft-cli ai`: the opt-in AI assistant, with the user's own API key.
//!
//! Nothing is sent anywhere unless this command runs. Before sending, it says on stderr what
//! goes to which provider. The document is opened from a copy in a private temporary folder,
//! which is also the only folder the model's tools can read or write (`Automation::with_root`);
//! the original file is never changed, and an edited result is written to a new file.

use std::path::{Path, PathBuf};
use std::time::Duration;

use pdfcraft_ai::tasks::{self, Cell, Table, Workspace};
use pdfcraft_ai::{AiError, Assistant, Config, HttpTransport, Provider, Step, ToolCall, ToolSpec};
use pdfcraft_automation::{Automation, Content};
use serde_json::{Value, json};

use crate::{CliError, flag, positional, stdout_line};

pub const USAGE: &str = "\
pdfcraft-cli ai providers                                 the AI providers, default models and key variables
pdfcraft-cli ai summarize <file.pdf> [--focus TEXT] [--out summary.pdf|.txt|.md]
pdfcraft-cli ai ask       <file.pdf> \"question\"
pdfcraft-cli ai translate <file.pdf> --to LANGUAGE [--out translation.pdf|.txt|.md]
pdfcraft-cli ai edit      <file.pdf> \"instruction\" [--out new.pdf] [--max-steps 40]
pdfcraft-cli ai tables    <file.pdf> [--which TEXT] [--out tables.xlsx|.csv] [--sep ';']
pdfcraft-cli ai slides    <file.pdf> [--slides N] [--focus TEXT] [--out slides.pptx|.pdf]
  every ai command: [--provider anthropic|openai|deepseek|grok|gemini] [--model M] [--base-url URL]
                    [--key-file FILE] [--pages 1-3,5] [--lang pt-BR] [--password PW]
  The key comes from --key-file, PDFCRAFT_AI_KEY or the provider's variable (ANTHROPIC_API_KEY,
  OPENAI_API_KEY, DEEPSEEK_API_KEY, XAI_API_KEY, GEMINI_API_KEY). The provider is --provider,
  PDFCRAFT_AI_PROVIDER, or the first one whose variable is set. The document's text is sent to
  that provider; the original file is never changed.";

/// Deletes the private folder when the command ends, however it ends.
struct Sandbox(PathBuf);

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn sandbox() -> Result<Sandbox, String> {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let dir = std::env::temp_dir().join(format!("pdfcraft-ai-{}-{nanos}", std::process::id()));
    std::fs::create_dir(&dir).map_err(|e| format!("couldn't create a temporary folder: {e}"))?;
    Ok(Sandbox(dir))
}

fn say(line: std::fmt::Arguments<'_>) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr().lock(), "{line}");
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// The provider, key and model from flags and the environment.
fn config(args: &[String]) -> Result<Config, String> {
    let provider = match flag(args, "--provider").map(str::to_string).or_else(|| env("PDFCRAFT_AI_PROVIDER")) {
        Some(p) => Provider::parse(&p).ok_or(format!("unknown provider {p:?}; use anthropic, openai, deepseek, grok or gemini"))?,
        None => Provider::ALL
            .into_iter()
            .find(|p| env(p.key_env()).is_some())
            .ok_or("no AI provider chosen: pass --provider and set its API key (see `pdfcraft-cli ai providers`)")?,
    };
    let key = match flag(args, "--key-file") {
        Some(file) => {
            let text = std::fs::read_to_string(file).map_err(|e| format!("{file}: {e}"))?;
            text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").to_string()
        }
        None => env("PDFCRAFT_AI_KEY").or_else(|| env(provider.key_env())).unwrap_or_default(),
    };
    let mut cfg = Config::new(provider, key);
    if let Some(m) = flag(args, "--model").map(str::to_string).or_else(|| env("PDFCRAFT_AI_MODEL")) {
        cfg.model = m;
    }
    if let Some(u) = flag(args, "--base-url") {
        cfg.base_url = u.to_string();
    }
    cfg.validate().map_err(|e| e.to_string())?;
    Ok(cfg)
}

/// The JSON of a tool result (the first JSON content).
fn json_of(content: Vec<Content>) -> Value {
    content
        .into_iter()
        .find_map(|c| match c {
            Content::Json(v) => Some(v),
            Content::Png { .. } => None,
        })
        .unwrap_or(Value::Null)
}

/// The document, opened from a private copy.
struct Opened {
    auto: Automation,
    pages: usize,
    _sandbox: Sandbox,
    dir: PathBuf,
    stem: String,
    source: PathBuf,
}

fn open(args: &[String], file: &str) -> Result<Opened, String> {
    let source = PathBuf::from(file);
    let name = source.file_name().and_then(|n| n.to_str()).ok_or(format!("{file}: not a file name"))?.to_string();
    let stem = source.file_stem().and_then(|n| n.to_str()).unwrap_or("document").to_string();
    let sb = sandbox()?;
    let dir = sb.0.clone();
    std::fs::copy(&source, dir.join(&name)).map_err(|e| format!("{file}: {e}"))?;
    let mut auto = Automation::new().with_root(&dir).map_err(|e| e.to_string())?;
    let mut open_args = json!({ "path": name });
    if let Some(pw) = flag(args, "--password") {
        open_args["password"] = json!(pw);
    }
    let info = json_of(auto.call("doc_open", &open_args).map_err(|e| format!("{file}: {e}"))?);
    let pages = info["pages"].as_u64().and_then(|n| usize::try_from(n).ok()).ok_or(format!("{file}: no page count"))?;
    Ok(Opened { auto, pages, _sandbox: sb, dir, stem, source })
}

impl Opened {
    /// The text of the chosen pages.
    fn texts(&mut self, args: &[String]) -> Result<Vec<(usize, String)>, String> {
        let pages = match flag(args, "--pages") {
            Some(spec) => tasks::page_list(spec, self.pages).map_err(|e| format!("--pages: {e}"))?,
            None => (1..=self.pages).collect(),
        };
        let v = json_of(self.auto.call("text_extract", &json!({ "doc": 1, "pages": pages })).map_err(|e| e.to_string())?);
        let list = v["pages"].as_array().cloned().unwrap_or_default();
        Ok(list.iter().filter_map(|p| Some((usize::try_from(p["page"].as_u64()?).ok()?, p["text"].as_str().unwrap_or("").to_string()))).collect())
    }

    /// `--out`, or `<stem><suffix>` next to the original; never the original itself.
    fn output(&self, args: &[String], suffix: &str) -> Result<PathBuf, String> {
        let out = match flag(args, "--out") {
            Some(o) => PathBuf::from(o),
            None => self.source.with_file_name(format!("{}{suffix}", self.stem)),
        };
        let same = match (std::fs::canonicalize(&self.source), std::fs::canonicalize(&out)) {
            (Ok(a), Ok(b)) => a == b,
            _ => out == self.source,
        };
        if same {
            return Err("--out is the original file; the assistant writes its result to a new file".into());
        }
        Ok(out)
    }
}

fn extension(path: &Path) -> String {
    path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase()
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    std::fs::write(path, bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    say(format_args!("Saved {}", path.display()));
    Ok(())
}

fn announce(cfg: &Config, what: &str) {
    say(format_args!("Sending {what} to {} (model {}).", cfg.provider.name(), cfg.model));
}

fn report(a: &Assistant<'_>) {
    let u = a.usage();
    say(format_args!("{} request(s): {} input and {} output tokens.", u.requests, u.input_tokens, u.output_tokens));
}

fn chars(texts: &[(usize, String)]) -> usize {
    texts.iter().map(|(_, t)| t.chars().count()).sum()
}

pub fn run(args: &[String]) -> Result<(), CliError> {
    let sub = args.first().map(String::as_str);
    if sub == Some("providers") {
        for p in Provider::ALL {
            let set = if env(p.key_env()).is_some() { "key set" } else { "no key" };
            stdout_line(format_args!("{:<9} {:<20} model {:<18} key in {} ({set})", p.id(), p.name(), p.default_model(), p.key_env()))?;
        }
        return Ok(());
    }
    let rest = args.get(1..).unwrap_or(&[]);
    let pos = positional(rest);
    let file = *pos.first().ok_or(format!("ai: missing file\n{USAGE}"))?;
    let cfg = config(rest)?;
    let transport = HttpTransport::new(Duration::from_secs(600)).map_err(|e| e.to_string())?;
    let a = Assistant::new(cfg.clone(), &transport).map_err(|e| e.to_string())?;
    let lang = flag(rest, "--lang");
    let mut doc = open(rest, file)?;
    let result = match sub {
        Some("summarize") => summarize(&a, &mut doc, rest, lang),
        Some("ask") => {
            let question = pos.get(1).ok_or("ai ask: missing question")?;
            let texts = doc.texts(rest)?;
            announce(&cfg, &format!("the text of {} page(s) ({} characters) and your question", texts.len(), chars(&texts)));
            let answer = tasks::ask(&a, &texts, question, lang).map_err(|e| e.to_string())?;
            stdout_line(format_args!("{answer}"))
        }
        Some("translate") => translate(&a, &mut doc, rest),
        Some("edit") => edit(&a, &mut doc, rest, pos.get(1).copied()),
        Some("tables") => tables(&a, &mut doc, rest),
        Some("slides") => slides(&a, &mut doc, rest, lang),
        _ => Err(CliError::Message(format!("ai: unknown command\n{USAGE}"))),
    };
    report(&a);
    result
}

fn summarize(a: &Assistant<'_>, doc: &mut Opened, args: &[String], lang: Option<&str>) -> Result<(), CliError> {
    let texts = doc.texts(args)?;
    announce(a.config(), &format!("the text of {} page(s) ({} characters)", texts.len(), chars(&texts)));
    let summary = tasks::summarize(a, &texts, lang, flag(args, "--focus")).map_err(|e| e.to_string())?;
    text_result(doc, args, &format!("Summary of {}", doc.stem), &summary)
}

/// Print `text`, or write it to `--out` (a new PDF for `.pdf`, the text otherwise).
fn text_result(doc: &Opened, args: &[String], title: &str, text: &str) -> Result<(), CliError> {
    let Some(out) = flag(args, "--out").map(|_| doc.output(args, "")).transpose()? else {
        return stdout_line(format_args!("{text}"));
    };
    if extension(&out) == "pdf" {
        let bytes = doc.auto.session().create_from_text(title, text).map_err(|e| e.to_string())?;
        write(&out, &bytes)?;
    } else {
        write(&out, text.as_bytes())?;
    }
    Ok(())
}

fn translate(a: &Assistant<'_>, doc: &mut Opened, args: &[String]) -> Result<(), CliError> {
    let target = flag(args, "--to").ok_or("ai translate: missing --to LANGUAGE")?;
    let texts = doc.texts(args)?;
    announce(a.config(), &format!("the text of {} page(s) ({} characters)", texts.len(), chars(&texts)));
    let text = tasks::translate(a, &texts, target).map_err(|e| e.to_string())?;
    text_result(doc, args, &format!("{} ({target})", doc.stem), &text)
}

/// The automation tools, minus those withheld, run on the open document.
struct Tools<'a> {
    auto: &'a mut Automation,
}

impl Workspace for Tools<'_> {
    fn tools(&self) -> Vec<ToolSpec> {
        pdfcraft_automation::assistant_tools()
            .into_iter()
            .map(|t| ToolSpec { name: t.name.to_string(), description: t.description.to_string(), schema: t.input_schema.clone() })
            .collect()
    }

    fn call(&mut self, call: &ToolCall) -> Result<String, String> {
        if pdfcraft_automation::ASSISTANT_WITHHELD.contains(&call.name.as_str()) {
            return Err(format!("{} isn't available to the assistant", call.name));
        }
        let content = self.auto.call(&call.name, &call.input).map_err(|e| e.to_string())?;
        let parts: Vec<String> = content
            .into_iter()
            .map(|c| match c {
                Content::Json(v) => v.to_string(),
                Content::Png { width, height, .. } => format!("[a {width}×{height} image, not shown]"),
            })
            .collect();
        Ok(parts.join("\n"))
    }
}

fn edit(a: &Assistant<'_>, doc: &mut Opened, args: &[String], instruction: Option<&str>) -> Result<(), CliError> {
    let instruction = instruction.map(str::trim).filter(|i| !i.is_empty()).ok_or("ai edit: missing instruction")?;
    let out = doc.output(args, "-ai.pdf")?;
    let max_steps = match flag(args, "--max-steps") {
        Some(n) => n.parse::<usize>().ok().filter(|n| (1..=500).contains(n)).ok_or("--max-steps must be 1 to 500")?,
        None => 40,
    };
    announce(a.config(), "your instruction, and the parts of the document the model reads with its tools,");
    let pages = doc.pages;
    let mut tools = Tools { auto: &mut doc.auto };
    let mut log = |s: &Step| {
        let input = clip(&s.input.to_string(), 160);
        say(format_args!("  {} {} {input}{}", if s.ok { "✓" } else { "✗" }, s.tool, if s.ok { String::new() } else { format!(" → {}", s.result) }));
    };
    let outcome = tasks::command(a, &mut tools, 1, pages, instruction, max_steps, &mut log).map_err(|e: AiError| e.to_string())?;
    if !outcome.text.trim().is_empty() {
        stdout_line(format_args!("{}", outcome.text.trim()))?;
    }
    if !outcome.finished {
        say(format_args!("Stopped after {max_steps} steps (raise --max-steps to allow more)."));
    }
    let info = json_of(doc.auto.call("doc_info", &json!({ "doc": 1 })).map_err(|e| e.to_string())?);
    if info["document"]["dirty"].as_bool() != Some(true) {
        say(format_args!("The document was not changed; nothing saved."));
        return Ok(());
    }
    doc.auto.call("doc_save", &json!({ "doc": 1, "path": "pdfcraft-ai-result.pdf" })).map_err(|e| e.to_string())?;
    let bytes = std::fs::read(doc.dir.join("pdfcraft-ai-result.pdf")).map_err(|e| e.to_string())?;
    write(&out, &bytes)?;
    Ok(())
}

fn clip(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", s.get(..at).unwrap_or("")),
        None => s.to_string(),
    }
}

/// A CSV field: quoted when it holds the separator, a quote or a line break.
fn csv_field(s: &str, sep: char) -> String {
    if s.contains([sep, '"', '\n', '\r']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s.to_string() }
}

fn csv(tables: &[Table], sep: char) -> String {
    // A semicolon separator is what spreadsheet apps expect where the comma is the decimal mark.
    let decimal_comma = sep == ';';
    let mut s = String::from("\u{feff}");
    for (i, t) in tables.iter().enumerate() {
        if tables.len() > 1 {
            if i > 0 {
                s.push_str("\r\n");
            }
            s.push_str(&csv_field(&t.name, sep));
            s.push_str("\r\n");
        }
        for row in &t.rows {
            let cells: Vec<String> = row
                .iter()
                .map(|c| match c {
                    Cell::Empty => String::new(),
                    Cell::Text(x) => csv_field(x, sep),
                    Cell::Number(n) if decimal_comma => n.to_string().replace('.', ","),
                    Cell::Number(n) => n.to_string(),
                })
                .collect();
            s.push_str(&cells.join(&sep.to_string()));
            s.push_str("\r\n");
        }
    }
    s
}

fn tables(a: &Assistant<'_>, doc: &mut Opened, args: &[String]) -> Result<(), CliError> {
    let out = doc.output(args, "-tables.xlsx")?;
    let texts = doc.texts(args)?;
    announce(a.config(), &format!("the text of {} page(s) ({} characters)", texts.len(), chars(&texts)));
    let found = tasks::tables(a, &texts, flag(args, "--which")).map_err(|e| e.to_string())?;
    if found.is_empty() {
        stdout_line(format_args!("No tables found."))?;
        return Ok(());
    }
    for t in &found {
        let page = t.page.map_or(String::new(), |p| format!(" (page {p})"));
        stdout_line(format_args!("{}{page}: {} row(s)", t.name, t.rows.len().saturating_sub(1)))?;
    }
    if extension(&out) == "csv" {
        let sep = match flag(args, "--sep") {
            None | Some(",") => ',',
            Some(";") => ';',
            Some("tab" | "\\t") => '\t',
            Some(other) => return Err(format!("--sep {other:?}: use , ; or tab").into()),
        };
        write(&out, csv(&found, sep).as_bytes())?;
    } else {
        use pdfcraft_export::office;
        let sheets: Vec<office::Sheet> = found
            .iter()
            .map(|t| office::Sheet {
                name: t.name.clone(),
                rows: t
                    .rows
                    .iter()
                    .map(|r| {
                        r.iter()
                            .map(|c| match c {
                                Cell::Empty => office::Cell::Empty,
                                Cell::Text(x) => office::Cell::Text(x.clone()),
                                Cell::Number(n) => office::Cell::Number(*n),
                            })
                            .collect()
                    })
                    .collect(),
            })
            .collect();
        write(&out, &office::xlsx(&sheets))?;
    }
    Ok(())
}

fn slides(a: &Assistant<'_>, doc: &mut Opened, args: &[String], lang: Option<&str>) -> Result<(), CliError> {
    let out = doc.output(args, "-slides.pptx")?;
    let count = match flag(args, "--slides") {
        Some(n) => Some(n.parse::<usize>().ok().filter(|n| (1..=100).contains(n)).ok_or("--slides must be 1 to 100")?),
        None => None,
    };
    let texts = doc.texts(args)?;
    announce(a.config(), &format!("the text of {} page(s) ({} characters)", texts.len(), chars(&texts)));
    let deck = tasks::slides(a, &texts, count, lang, flag(args, "--focus")).map_err(|e| e.to_string())?;
    let title = if deck.title.is_empty() { doc.stem.clone() } else { deck.title.clone() };
    stdout_line(format_args!("{title}: {} slide(s)", deck.slides.len() + 1))?;
    if extension(&out) == "pdf" {
        let s: Vec<pdfcraft_create::Slide> =
            deck.slides.iter().map(|s| pdfcraft_create::Slide { title: s.title.clone(), bullets: s.bullets.clone() }).collect();
        let bytes = doc.auto.session().create_from_slides(&title, &deck.subtitle, &s).map_err(|e| e.to_string())?;
        write(&out, &bytes)?;
    } else {
        let s: Vec<pdfcraft_export::office::Slide> =
            deck.slides.iter().map(|s| pdfcraft_export::office::Slide { title: s.title.clone(), bullets: s.bullets.clone() }).collect();
        write(&out, &pdfcraft_export::office::pptx(&title, &deck.subtitle, &s))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_quotes_and_follows_the_separator() {
        let t = Table {
            name: "T".into(),
            page: None,
            rows: vec![vec![Cell::Text("a;b".into()), Cell::Number(1234.5), Cell::Empty, Cell::Text("x\"y".into())]],
        };
        assert_eq!(csv(std::slice::from_ref(&t), ';'), "\u{feff}\"a;b\";1234,5;;\"x\"\"y\"\r\n");
        assert_eq!(csv(std::slice::from_ref(&t), ','), "\u{feff}a;b,1234.5,,\"x\"\"y\"\r\n");
        let two = csv(&[t.clone(), t], ',');
        assert!(two.starts_with("\u{feff}T\r\n") && two.matches("T\r\n").count() == 2);
    }

    #[test]
    fn editing_never_runs_a_withheld_tool() {
        let mut auto = Automation::new();
        assert_eq!(Tools { auto: &mut auto }.tools().len(), pdfcraft_automation::assistant_tools().len());
        let call = ToolCall { id: "1".into(), name: "doc_save".into(), input: json!({}), invalid: None };
        assert!(Tools { auto: &mut auto }.call(&call).is_err());
    }
}
