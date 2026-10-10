//! AI assistant (All tools ▸ AI assistant): the user's own API key for Anthropic, OpenAI,
//! DeepSeek, xAI Grok or Google Gemini summarizes, answers, translates, edits the open document,
//! extracts tables and outlines presentations.
//!
//! Off by default: nothing is sent until the user presses Run, and the dialog says what goes to
//! which provider. The desktop app supplies the HTTPS transport ([`PdfCraftApp::ai_transport`]),
//! so this crate has no network code; without one (the web build, tests) the dialog says the
//! assistant needs the desktop app. Requests run on a worker thread. When the model edits, each
//! tool call it makes is sent back here and run on the app's own session between frames, so
//! every change lands in the document's undo history and nothing is saved until the user saves.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};

use egui::{Align, Layout};
use pdfcraft_ai::tasks::{self, Cell, Deck, Table, Workspace};
use pdfcraft_ai::{AiError, Assistant, Config, Provider, Step, ToolCall, ToolSpec, Transport, Usage};
use pdfcraft_engine::DocId;

use crate::theme::{self, Tokens};
use crate::{PdfCraftApp, widgets};

/// Errors and warnings (as in Combine files).
const DANGER: egui::Color32 = egui::Color32::from_rgb(0xD7, 0x37, 0x3F);

/// How the app reaches the providers (HTTPS on the desktop).
pub type AiTransport = Arc<dyn Transport + Send + Sync>;

/// What the assistant is asked to do.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AiTask {
    #[default]
    Summarize,
    Ask,
    Translate,
    Edit,
    Tables,
    Slides,
}

impl AiTask {
    pub const ALL: [AiTask; 6] = [AiTask::Summarize, AiTask::Ask, AiTask::Translate, AiTask::Edit, AiTask::Tables, AiTask::Slides];

    fn label(self) -> &'static str {
        match self {
            Self::Summarize => tl!("Summarize"),
            Self::Ask => tl!("Ask"),
            Self::Translate => tl!("Translate"),
            Self::Edit => tl!("Edit"),
            Self::Tables => tl!("Tables"),
            Self::Slides => tl!("Presentation"),
        }
    }

    /// The prompt for the text box, and whether it must be filled in.
    fn input_label(self) -> (&'static str, bool) {
        match self {
            Self::Summarize | Self::Slides => (tl!("Focus or audience (optional)"), false),
            Self::Ask => (tl!("Your question"), true),
            Self::Translate => (tl!("Translate into (language)"), true),
            Self::Edit => (tl!("What should be changed?"), true),
            Self::Tables => (tl!("Which tables (optional)"), false),
        }
    }
}

/// A finished run's result.
pub enum AiResult {
    /// A summary, answer or translation, with the title for a PDF made from it.
    Text {
        title: String,
        text: String,
    },
    Tables(Vec<Table>),
    Deck(Deck),
    /// The model's closing message after editing, and how many tool calls it made.
    Edited {
        text: String,
        steps: usize,
        finished: bool,
    },
}

/// Messages from the worker thread.
enum Msg {
    Step(String),
    Done(Result<AiResult, AiError>, Usage),
}

/// A tool call waiting to run on the UI thread, and where its result goes.
type ToolRequest = (ToolCall, Sender<Result<String, String>>);

pub(crate) struct Running {
    rx: Receiver<Msg>,
    tools: Option<Receiver<ToolRequest>>,
    cancel: Arc<AtomicBool>,
    /// The document the run works on, and the private folder an edit's tools are confined to.
    doc: DocId,
    sandbox: Option<std::path::PathBuf>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(dir) = &self.sandbox {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

type Map = std::collections::BTreeMap<String, String>;

/// The assistant's settings and the current run.
#[derive(Default)]
pub struct AiState {
    pub open: bool,
    pub provider: Provider,
    /// Per provider id: the model (empty: the provider's default) and endpoint (empty: its own).
    pub models: Map,
    pub endpoints: Map,
    /// API keys typed in, per provider id.
    pub keys: Map,
    /// Keep the keys in PdfCraft's settings on this computer (off: only for this session).
    pub remember_keys: bool,
    pub task: AiTask,
    pub input: String,
    /// Pages to send (`1-3,5`; empty: all).
    pub pages: String,
    /// The answer's language (empty: the request's or the document's).
    pub language: String,
    /// Number of content slides (0: the model chooses).
    pub slides: usize,
    pub(crate) running: Option<Running>,
    pub log: Vec<String>,
    pub result: Option<AiResult>,
    pub error: Option<String>,
    pub usage: Option<Usage>,
}

impl AiState {
    pub fn running(&self) -> bool {
        self.running.is_some()
    }

    fn entry(map: &mut Map, p: Provider) -> &mut String {
        map.entry(p.id().to_string()).or_default()
    }

    /// The key for the provider, and whether it came from the environment rather than the box.
    fn key(&self) -> Option<(String, bool)> {
        if let Some(k) = self.keys.get(self.provider.id()).map(|k| k.trim().to_string()).filter(|k| !k.is_empty()) {
            return Some((k, false));
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(k) = std::env::var(self.provider.key_env()).ok().map(|k| k.trim().to_string()).filter(|k| !k.is_empty()) {
            return Some((k, true));
        }
        None
    }

    /// The settings for a run.
    pub fn config(&self) -> Result<Config, String> {
        let (key, _) = self.key().ok_or_else(|| crate::i18n::fmt(tl!("Enter your {provider} API key."), &[("provider", self.provider.name())]))?;
        let mut cfg = Config::new(self.provider, key);
        if let Some(m) = self.models.get(self.provider.id()).map(|m| m.trim()).filter(|m| !m.is_empty()) {
            cfg.model = m.to_string();
        }
        if let Some(u) = self.endpoints.get(self.provider.id()).map(|u| u.trim()).filter(|u| !u.is_empty()) {
            cfg.base_url = u.to_string();
        }
        cfg.validate().map_err(|e| e.to_string())?;
        Ok(cfg)
    }

    /// Settings to keep (keys only when the user asked to remember them).
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "provider": self.provider.id(),
            "models": self.models,
            "endpoints": self.endpoints,
            "remember_keys": self.remember_keys,
            "keys": if self.remember_keys { serde_json::json!(self.keys) } else { serde_json::Value::Null },
        })
    }

    /// Restore [`Self::to_json`]. Settings are untrusted: odd values are ignored or cut short.
    pub fn restore(&mut self, v: &serde_json::Value) {
        if let Some(p) = v["provider"].as_str().and_then(Provider::parse) {
            self.provider = p;
        }
        let map = |v: &serde_json::Value| -> Map {
            v.as_object()
                .map(|o| {
                    o.iter().filter_map(|(k, v)| Some((Provider::parse(k)?.id().to_string(), v.as_str()?.chars().take(4096).collect()))).collect()
                })
                .unwrap_or_default()
        };
        self.models = map(&v["models"]);
        self.endpoints = map(&v["endpoints"]);
        self.remember_keys = v["remember_keys"].as_bool().unwrap_or(false);
        if self.remember_keys {
            self.keys = map(&v["keys"]);
        }
    }
}

/// The tool calls of an edit, run on the UI thread: each is sent there and its result awaited.
struct Relay {
    tools: Vec<ToolSpec>,
    tx: Sender<ToolRequest>,
}

impl Workspace for Relay {
    fn tools(&self) -> Vec<ToolSpec> {
        self.tools.clone()
    }

    fn call(&mut self, call: &ToolCall) -> Result<String, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.tx.send((call.clone(), tx)).map_err(|_| "the assistant was closed".to_string())?;
        rx.recv().map_err(|_| "the assistant was closed".to_string())?
    }
}

fn step_line(s: &Step) -> String {
    let mut input = s.input.to_string();
    if let Some((at, _)) = input.char_indices().nth(120) {
        input.truncate(at);
        input.push('…');
    }
    if s.ok { format!("✓ {} {input}", s.tool) } else { format!("✗ {} {input} → {}", s.tool, s.result) }
}

/// What a worker needs to run one task.
struct Job {
    task: AiTask,
    input: String,
    language: Option<String>,
    slides: Option<usize>,
    pages: Vec<usize>,
    count: usize,
    bytes: Arc<Vec<u8>>,
    password: Option<Arc<str>>,
    stem: String,
    doc: u64,
    tools: Vec<ToolSpec>,
}

fn work(assistant: &Assistant<'_>, job: Job, relay_tx: Sender<ToolRequest>, on_step: &mut dyn FnMut(&Step)) -> Result<AiResult, AiError> {
    let texts = || -> Vec<(usize, String)> {
        let zero: Vec<usize> = job.pages.iter().map(|p| p.saturating_sub(1)).collect();
        pdfcraft_automation::plain_texts(&job.bytes, job.password.clone(), &zero).into_iter().map(|(p, t)| (p + 1, t.unwrap_or_default())).collect()
    };
    let focus = Some(job.input.as_str()).filter(|s| !s.is_empty());
    let lang = job.language.as_deref();
    let stem = &job.stem;
    match job.task {
        AiTask::Summarize => Ok(AiResult::Text { title: format!("Summary of {stem}"), text: tasks::summarize(assistant, &texts(), lang, focus)? }),
        AiTask::Ask => Ok(AiResult::Text { title: format!("Answer about {stem}"), text: tasks::ask(assistant, &texts(), &job.input, lang)? }),
        AiTask::Translate => {
            Ok(AiResult::Text { title: format!("{stem} ({})", job.input), text: tasks::translate(assistant, &texts(), &job.input)? })
        }
        AiTask::Tables => Ok(AiResult::Tables(tasks::tables(assistant, &texts(), focus)?)),
        AiTask::Slides => Ok(AiResult::Deck(tasks::slides(assistant, &texts(), job.slides, lang, focus)?)),
        AiTask::Edit => {
            let mut relay = Relay { tools: job.tools, tx: relay_tx };
            let out = tasks::command(assistant, &mut relay, job.doc, job.count, &job.input, 60, on_step)?;
            Ok(AiResult::Edited { text: out.text, steps: out.steps.len(), finished: out.finished })
        }
    }
}

impl PdfCraftApp {
    /// Open the assistant on `task`.
    pub fn open_ai(&mut self, task: AiTask) {
        if !self.ai.running() {
            if self.ai.task != task {
                self.ai.input.clear();
            }
            self.ai.task = task;
        }
        self.ai.open = true;
    }

    /// Start the chosen task on the active document.
    pub fn start_ai(&mut self) {
        if self.ai.running() {
            return;
        }
        self.ai.result = None;
        self.ai.error = None;
        self.ai.usage = None;
        self.ai.log.clear();
        match self.start_ai_run() {
            Ok(run) => self.ai.running = Some(run),
            Err(e) => self.ai.error = Some(e),
        }
    }

    fn start_ai_run(&mut self) -> Result<Running, String> {
        let transport = self
            .ai_transport
            .clone()
            .ok_or_else(|| tl!("The AI assistant needs the desktop app: the web version can't reach AI providers.").to_string())?;
        let cfg = self.ai.config()?;
        let (_, id) = self.active_ids().ok_or_else(|| tl!("Open a document first.").to_string())?;
        let doc = self.session.get(id).ok_or_else(|| tl!("Open a document first.").to_string())?;
        let count = doc.info.pages.len();
        let pages = tasks::page_list(&self.ai.pages, count).map_err(|e| crate::i18n::fmt(tl!("Pages: {e}"), &[("e", &e)]))?;
        let task = self.ai.task;
        let (label, required) = task.input_label();
        let input = self.ai.input.trim().to_string();
        if required && input.is_empty() {
            return Err(crate::i18n::fmt(tl!("Fill in “{field}”."), &[("field", label)]));
        }
        let mut sandbox = None;
        let mut tools = Vec::new();
        if task == AiTask::Edit {
            // The model's tools may only touch an empty private folder.
            let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
            let dir = std::env::temp_dir().join(format!("pdfcraft-ai-{}-{nanos}", std::process::id()));
            std::fs::create_dir(&dir).map_err(|e| e.to_string())?;
            sandbox = Some(dir.canonicalize().unwrap_or(dir));
            tools = pdfcraft_automation::assistant_tools()
                .into_iter()
                .map(|t| ToolSpec { name: t.name.to_string(), description: t.description.to_string(), schema: t.input_schema })
                .collect();
        }
        let job = Job {
            task,
            input,
            language: Some(self.ai.language.trim().to_string()).filter(|l| !l.is_empty()),
            slides: (self.ai.slides > 0).then_some(self.ai.slides),
            pages,
            count,
            bytes: doc.bytes.clone(),
            password: doc.password.as_deref().map(Arc::from),
            stem: crate::files::strip_pdf(&doc.name).to_string(),
            doc: id.0,
            tools,
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        let (tool_tx, tool_rx) = std::sync::mpsc::channel();
        let ctx = self.ctx.clone();
        let flag = cancel.clone();
        std::thread::spawn(move || {
            let repaint = || {
                if let Some(c) = &ctx {
                    c.request_repaint();
                }
            };
            let (result, usage) = match Assistant::new(cfg, &*transport) {
                Ok(a) => {
                    let a = a.with_cancel(flag);
                    let mut on_step = |s: &Step| {
                        let _ = tx.send(Msg::Step(step_line(s)));
                        repaint();
                    };
                    let r = work(&a, job, tool_tx, &mut on_step);
                    (r, a.usage())
                }
                Err(e) => (Err(e), Usage::default()),
            };
            let _ = tx.send(Msg::Done(result, usage));
            repaint();
        });
        Ok(Running { rx, tools: (task == AiTask::Edit).then_some(tool_rx), cancel, doc: id, sandbox })
    }

    /// Stop the run before its next request (the request under way finishes first).
    pub fn cancel_ai(&mut self) {
        if let Some(r) = &self.ai.running {
            r.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Each frame: run the tool calls an edit asks for, and pick up progress and the result.
    pub(crate) fn poll_ai(&mut self) {
        let Some(run) = self.ai.running.as_mut() else { return };
        let mut calls = Vec::new();
        if let Some(tools) = &run.tools {
            while let Ok(req) = tools.try_recv() {
                calls.push(req);
            }
        }
        let (doc, sandbox) = (run.doc, run.sandbox.clone());
        let mut msgs = Vec::new();
        let mut gone = false;
        loop {
            match run.rx.try_recv() {
                Ok(m) => msgs.push(m),
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    gone = true;
                    break;
                }
            }
        }
        for (call, reply) in calls {
            let result = self.run_ai_tool(doc, sandbox.clone(), &call);
            let _ = reply.send(result);
        }
        for m in msgs {
            match m {
                Msg::Step(line) => self.ai.log.push(line),
                Msg::Done(result, usage) => {
                    self.ai.usage = Some(usage);
                    self.ai.running = None;
                    match result {
                        Ok(r) => {
                            if !self.ai.open {
                                self.notify_tr("The AI assistant has finished");
                            }
                            self.ai.result = Some(r);
                        }
                        Err(AiError::Cancelled) => self.ai.error = Some(tl!("Stopped.").to_string()),
                        Err(e) => self.ai.error = Some(e.to_string()),
                    }
                    return;
                }
            }
        }
        if gone {
            self.ai.running = None;
            self.ai.error = Some(tl!("The assistant stopped unexpectedly.").to_string());
        }
    }

    /// Run one tool call of an edit on the app's session, and refresh the views it changed.
    fn run_ai_tool(&mut self, doc: DocId, sandbox: Option<std::path::PathBuf>, call: &ToolCall) -> Result<String, String> {
        if pdfcraft_automation::ASSISTANT_WITHHELD.contains(&call.name.as_str()) {
            return Err(format!("{} isn't available to the assistant", call.name));
        }
        // Only the document the run started on.
        if call.input.get("doc").and_then(serde_json::Value::as_u64).is_some_and(|d| d != doc.0) {
            return Err(format!("only document {} may be changed", doc.0));
        }
        let before = self.session.get(doc).map(|d| d.edit_generation());
        let session = std::mem::take(&mut self.session);
        let mut auto = pdfcraft_automation::Automation::from_session(session, sandbox);
        let out = auto.call(&call.name, &call.input);
        self.session = auto.into_session();
        if let Some(d) = self.session.get(doc)
            && Some(d.edit_generation()) != before
        {
            for v in self.views.iter_mut().filter(|v| v.id == doc) {
                v.document_changed(&d.info);
            }
        }
        let content = out.map_err(|e| e.to_string())?;
        Ok(content
            .into_iter()
            .map(|c| match c {
                pdfcraft_automation::Content::Json(v) => v.to_string(),
                pdfcraft_automation::Content::Png { width, height, .. } => format!("[a {width}×{height} image, not shown]"),
            })
            .collect::<Vec<_>>()
            .join("\n"))
    }

    fn ai_stem(&self) -> String {
        self.active_ids().and_then(|(_, id)| self.session.get(id)).map_or_else(|| "document".into(), |d| crate::files::strip_pdf(&d.name).to_string())
    }

    /// Save the extracted tables as an Excel workbook.
    fn save_ai_tables(&mut self, tables: &[Table]) {
        use pdfcraft_export::office;
        let cell = |c: &Cell| match c {
            Cell::Empty => office::Cell::Empty,
            Cell::Text(x) => office::Cell::Text(x.clone()),
            Cell::Number(n) => office::Cell::Number(*n),
        };
        let sheets: Vec<office::Sheet> = tables
            .iter()
            .map(|t| office::Sheet { name: t.name.clone(), rows: t.rows.iter().map(|r| r.iter().map(cell).collect()).collect() })
            .collect();
        let stem = self.ai_stem();
        self.save_ai_file(&format!("{stem} - tables.xlsx"), office::xlsx(&sheets), "xlsx");
    }

    fn save_ai_deck(&mut self, deck: &Deck) {
        let slides: Vec<pdfcraft_export::office::Slide> =
            deck.slides.iter().map(|s| pdfcraft_export::office::Slide { title: s.title.clone(), bullets: s.bullets.clone() }).collect();
        let stem = self.ai_stem();
        let title = if deck.title.is_empty() { stem.clone() } else { deck.title.clone() };
        self.save_ai_file(&format!("{stem} - presentation.pptx"), pdfcraft_export::office::pptx(&title, &deck.subtitle, &slides), "pptx");
    }

    fn save_ai_file(&mut self, name: &str, bytes: Vec<u8>, ext: &'static str) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let write = move |app: &mut Self, path: std::path::PathBuf| match crate::editing::write_atomically(&path.to_string_lossy(), &bytes) {
                Ok(()) => app.notify_fmt("Saved {path}", &[("path", &path.display().to_string())]),
                Err(e) => app.notify_fmt("Couldn't write {name}: {e}", &[("name", &path.display().to_string()), ("e", &e.to_string())]),
            };
            match self.save_override.clone() {
                Some(p) => write(self, p.into()),
                None => {
                    let dialog =
                        rfd::AsyncFileDialog::new().set_title(tl!("Save").to_string()).add_filter(ext.to_uppercase(), &[ext]).set_file_name(name);
                    self.ask_one(crate::pickers::Ask::Save(dialog), None, write);
                }
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = ext;
            if let Err(e) = crate::editing::download(name, &bytes) {
                self.notify_error(e);
            }
        }
    }

    fn open_ai_text(&mut self, title: &str, text: &str) {
        match self.session.create_from_text(title, text) {
            Ok(bytes) => self.open_created(&format!("{title}.pdf"), bytes, "Created a PDF from the assistant's answer"),
            Err(e) => self.notify_fmt("Couldn't create the PDF: {e}", &[("e", &e.to_string())]),
        }
    }

    fn open_ai_deck(&mut self, deck: &Deck) {
        let stem = self.ai_stem();
        let title = if deck.title.is_empty() { stem.clone() } else { deck.title.clone() };
        let slides: Vec<pdfcraft_create::Slide> =
            deck.slides.iter().map(|s| pdfcraft_create::Slide { title: s.title.clone(), bullets: s.bullets.clone() }).collect();
        match self.session.create_from_slides(&title, &deck.subtitle, &slides) {
            Ok(bytes) => self.open_created(&format!("{stem} - presentation.pdf"), bytes, "Created a PDF from the assistant's answer"),
            Err(e) => self.notify_fmt("Couldn't create the PDF: {e}", &[("e", &e.to_string())]),
        }
    }
}

/// What the result area asks for.
enum Action {
    OpenText(String, String),
    Copy(String),
    SaveTables,
    SaveDeck,
    OpenDeck,
}

/// The settings and task controls.
fn controls(ui: &mut egui::Ui, s: &mut AiState, t: &Tokens) {
    egui::Grid::new("ai-settings").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
        ui.label(tl!("Provider"));
        egui::ComboBox::from_id_salt("ai-provider").selected_text(s.provider.name()).width(260.0).show_ui(ui, |ui| {
            for p in Provider::ALL {
                ui.selectable_value(&mut s.provider, p, p.name());
            }
        });
        ui.end_row();
        let p = s.provider;
        ui.label(tl!("API key"));
        let from_env = s.key().is_some_and(|(_, env)| env);
        ui.vertical(|ui| {
            ui.add(egui::TextEdit::singleline(AiState::entry(&mut s.keys, p)).password(true).desired_width(360.0).hint_text(p.key_env()));
            if from_env {
                ui.label(
                    egui::RichText::new(crate::i18n::fmt(tl!("Using {var} from the environment."), &[("var", p.key_env())]))
                        .small()
                        .color(t.text_muted),
                );
            }
        });
        ui.end_row();
        ui.label(tl!("Model"));
        ui.add(egui::TextEdit::singleline(AiState::entry(&mut s.models, p)).desired_width(260.0).hint_text(p.default_model()));
        ui.end_row();
    });
    ui.checkbox(&mut s.remember_keys, tl!("Remember API keys on this computer"));
    if s.remember_keys {
        ui.label(
            egui::RichText::new(tl!("They are kept unencrypted in PdfCraft's settings file. Leave this off on a shared computer."))
                .small()
                .color(t.text_muted),
        );
    }
    ui.collapsing(tl!("Advanced"), |ui| {
        ui.horizontal(|ui| {
            ui.label(tl!("Endpoint"));
            let p = s.provider;
            ui.add(egui::TextEdit::singleline(AiState::entry(&mut s.endpoints, p)).desired_width(420.0).hint_text(p.base_url()));
        });
        ui.label(
            egui::RichText::new(tl!("Another HTTPS address that speaks the same API, or http://localhost for a model on this computer."))
                .small()
                .color(t.text_muted),
        );
    });
    ui.add_space(6.0);
    ui.separator();
    ui.horizontal_wrapped(|ui| {
        for task in AiTask::ALL {
            if widgets::mode_tab(ui, task.label(), s.task == task).clicked() && s.task != task {
                s.task = task;
                s.input.clear();
            }
        }
    });
    ui.add_space(6.0);
    ui.label(s.task.input_label().0);
    let rows = if matches!(s.task, AiTask::Ask | AiTask::Edit) { 3 } else { 1 };
    ui.add(egui::TextEdit::multiline(&mut s.input).desired_rows(rows).desired_width(f32::INFINITY));
    ui.horizontal(|ui| {
        ui.label(tl!("Pages"));
        ui.add(egui::TextEdit::singleline(&mut s.pages).desired_width(110.0).hint_text(tl!("all")));
        if !matches!(s.task, AiTask::Translate | AiTask::Edit | AiTask::Tables) {
            ui.label(tl!("Answer language"));
            ui.add(egui::TextEdit::singleline(&mut s.language).desired_width(120.0).hint_text(tl!("automatic")));
        }
        if s.task == AiTask::Slides {
            ui.label(tl!("Slides"));
            ui.add(
                egui::DragValue::new(&mut s.slides)
                    .range(0..=50)
                    .custom_formatter(|n, _| if n < 0.5 { tl!("auto").to_string() } else { format!("{n:.0}") }),
            );
        }
    });
}

/// Progress, errors and the result; returns what the user asked to do with it.
fn outcome(ui: &mut egui::Ui, s: &AiState, t: &Tokens) -> Option<Action> {
    let mut action = None;
    if s.running.is_some() {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(tl!("Working…"));
        });
    }
    if !s.log.is_empty() {
        egui::ScrollArea::vertical().id_salt("ai-log").max_height(120.0).stick_to_bottom(true).show(ui, |ui| {
            for line in &s.log {
                ui.label(egui::RichText::new(line).small().monospace());
            }
        });
    }
    if let Some(e) = &s.error {
        ui.label(egui::RichText::new(e).color(DANGER));
    }
    match &s.result {
        Some(AiResult::Text { title, text }) => {
            egui::ScrollArea::vertical().id_salt("ai-text").max_height(300.0).show(ui, |ui| {
                ui.add(egui::Label::new(text.as_str()).selectable(true));
            });
            ui.horizontal(|ui| {
                if widgets::pill_button(ui, tl!("Copy"), false).clicked() {
                    action = Some(Action::Copy(text.clone()));
                }
                if widgets::pill_button(ui, tl!("Open as PDF"), false).clicked() {
                    action = Some(Action::OpenText(title.clone(), text.clone()));
                }
            });
        }
        Some(AiResult::Tables(tables)) if tables.is_empty() => {
            ui.label(tl!("No tables found."));
        }
        Some(AiResult::Tables(tables)) => {
            egui::ScrollArea::vertical().id_salt("ai-tables").max_height(200.0).show(ui, |ui| {
                for tb in tables {
                    let rows = tb.rows.len().saturating_sub(1).to_string();
                    let line = match tb.page {
                        Some(p) => crate::i18n::fmt(
                            tl!("{name} (page {page}): {rows} rows"),
                            &[("name", &tb.name), ("page", &p.to_string()), ("rows", &rows)],
                        ),
                        None => crate::i18n::fmt(tl!("{name}: {rows} rows"), &[("name", &tb.name), ("rows", &rows)]),
                    };
                    ui.label(line);
                }
            });
            if widgets::pill_button(ui, tl!("Save as Excel…"), true).clicked() {
                action = Some(Action::SaveTables);
            }
        }
        Some(AiResult::Deck(deck)) => {
            ui.label(egui::RichText::new(&deck.title).strong());
            egui::ScrollArea::vertical().id_salt("ai-deck").max_height(240.0).show(ui, |ui| {
                for (i, slide) in deck.slides.iter().enumerate() {
                    ui.label(egui::RichText::new(format!("{}. {}", i + 2, slide.title)).strong());
                    for b in &slide.bullets {
                        ui.label(format!("   • {b}"));
                    }
                }
            });
            ui.horizontal(|ui| {
                if widgets::pill_button(ui, tl!("Save as PowerPoint…"), true).clicked() {
                    action = Some(Action::SaveDeck);
                }
                if widgets::pill_button(ui, tl!("Open as PDF"), false).clicked() {
                    action = Some(Action::OpenDeck);
                }
            });
        }
        Some(AiResult::Edited { text, steps, finished }) => {
            ui.add(egui::Label::new(text.as_str()).selectable(true));
            ui.label(egui::RichText::new(crate::i18n::fmt(tl!("{n} tool calls."), &[("n", &steps.to_string())])).small().color(t.text_muted));
            if !finished {
                ui.label(egui::RichText::new(tl!("Stopped at the step limit before the model finished.")).color(DANGER));
            }
        }
        None => {}
    }
    if let Some(u) = s.usage.filter(|u| u.requests > 0) {
        ui.label(
            egui::RichText::new(crate::i18n::fmt(
                tl!("{requests} requests · {input} input and {output} output tokens"),
                &[("requests", &u.requests.to_string()), ("input", &u.input_tokens.to_string()), ("output", &u.output_tokens.to_string())],
            ))
            .small()
            .color(t.text_muted),
        );
    }
    action
}

/// The AI assistant dialog.
pub(crate) fn dialog(app: &mut PdfCraftApp, ctx: &egui::Context) {
    if !app.ai.open {
        return;
    }
    let t = Tokens::get(ctx);
    let mut close = false;
    let mut run = false;
    let mut stop = false;
    let mut action: Option<Action> = None;
    let has_transport = app.ai_transport.is_some();
    let has_doc = app.active_ids().and_then(|(_, id)| app.session.get(id)).is_some();
    let modal = egui::Modal::new(egui::Id::new("ai-assistant")).show(ctx, |ui| {
        ui.set_width(640.0);
        let w = &mut ui.visuals_mut().widgets;
        w.inactive.bg_stroke = egui::Stroke::new(1.0, t.border);
        w.inactive.weak_bg_fill = t.field;
        ui.horizontal(|ui| {
            ui.add(crate::icons::image("sparkles", 22.0, t.accent));
            ui.label(egui::RichText::new(tl!("AI assistant")).font(theme::semibold(18.0)));
        });
        ui.add_space(6.0);
        let s = &mut app.ai;
        let busy = s.running.is_some();
        ui.add_enabled_ui(!busy, |ui| controls(ui, s, &t));
        ui.add_space(4.0);
        let privacy = match s.task {
            AiTask::Edit => tl!(
                "Your instruction, and the parts of the document the model reads with its tools, are sent to {provider}. Changes go into the open document: undo them with Edit ▸ Undo; nothing is saved until you save."
            ),
            _ => tl!("The text of the chosen pages is sent to {provider}. Nothing is sent until you press Run."),
        };
        ui.label(egui::RichText::new(crate::i18n::fmt(privacy, &[("provider", s.provider.name())])).small().color(t.text_muted));
        if !has_transport {
            ui.label(egui::RichText::new(tl!("The AI assistant needs the desktop app: the web version can't reach AI providers.")).color(DANGER));
        } else if !has_doc {
            ui.label(egui::RichText::new(tl!("Open a document first.")).color(t.text_muted));
        }
        action = outcome(ui, s, &t);
        ui.add_space(10.0);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if busy {
                stop = widgets::pill_button(ui, tl!("Stop"), false).clicked();
            } else {
                run = ui.add_enabled_ui(has_transport && has_doc, |ui| widgets::pill_button(ui, tl!("Run"), true)).inner.clicked();
            }
            close = widgets::pill_button(ui, tl!("Close"), false).clicked();
        });
    });
    if modal.should_close() {
        close = true;
    }
    if run {
        app.start_ai();
    }
    if stop {
        app.cancel_ai();
    }
    match action {
        Some(Action::Copy(text)) => ctx.copy_text(text),
        Some(Action::OpenText(title, text)) => app.open_ai_text(&title, &text),
        Some(Action::SaveTables) => {
            if let Some(AiResult::Tables(tables)) = app.ai.result.take() {
                app.save_ai_tables(&tables);
                app.ai.result = Some(AiResult::Tables(tables));
            }
        }
        Some(a @ (Action::SaveDeck | Action::OpenDeck)) => {
            if let Some(AiResult::Deck(deck)) = app.ai.result.take() {
                if matches!(a, Action::SaveDeck) {
                    app.save_ai_deck(&deck);
                } else {
                    app.open_ai_deck(&deck);
                }
                app.ai.result = Some(AiResult::Deck(deck));
            }
        }
        None => {}
    }
    if close {
        // A run keeps going in the background; its result waits here.
        app.ai.open = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_kept_only_when_asked() {
        let mut s = AiState::default();
        s.keys.insert("openai".into(), "sk-secret".into());
        s.models.insert("openai".into(), "gpt-x".into());
        s.provider = Provider::OpenAi;
        assert!(!s.to_json().to_string().contains("sk-secret"));
        s.remember_keys = true;
        let saved = s.to_json();
        assert!(saved.to_string().contains("sk-secret"));
        let mut r = AiState::default();
        r.restore(&saved);
        assert_eq!(r.provider, Provider::OpenAi);
        assert_eq!(r.keys.get("openai").map(String::as_str), Some("sk-secret"));
        assert_eq!(r.models.get("openai").map(String::as_str), Some("gpt-x"));
        // Settings are untrusted: unknown providers and odd values are dropped.
        let mut r = AiState::default();
        r.restore(&serde_json::json!({ "provider": 5, "models": { "evil": "x", "grok": 7 }, "remember_keys": true, "keys": [1] }));
        assert_eq!(r.provider, Provider::Anthropic);
        assert!(r.models.is_empty() && r.keys.is_empty());
    }

    #[test]
    fn a_run_needs_a_usable_key_and_endpoint() {
        let mut s = AiState { provider: Provider::Gemini, ..AiState::default() };
        if std::env::var(Provider::Gemini.key_env()).is_err() {
            assert!(s.config().is_err());
        }
        s.keys.insert("gemini".into(), "AIza-test".into());
        assert_eq!(s.config().unwrap().model, Provider::Gemini.default_model());
        s.endpoints.insert("gemini".into(), "http://example.com".into());
        assert!(s.config().is_err(), "plain HTTP off this computer is refused");
    }
}
