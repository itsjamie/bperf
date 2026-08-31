use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::{Context, Result, bail};
use bperf_browser::lab::Engine;
use bperf_decision::{
    comparison::{ComparisonSummary, MetricSummary},
    lineage::{self, HistoryIndexEntry},
};
use bperf_measurement::sampling::RunBudget;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, StatefulWidget, Wrap},
};

use crate::terminal_ui::{
    self, AMBER, BG, BLUE, CYAN, ControlFlow, FAINT, FOCUS, GREEN, MUTED, RED, SELECTED, SURFACE,
    TEXT, chrome_block, clip, fit_sides, relative_age,
};

const MIN_WIDTH: u16 = 100;
const MIN_HEIGHT: u16 = 28;
const HORIZONTAL_LAYOUT: u16 = 136;

pub(crate) struct Options {
    pub(crate) directory: PathBuf,
    pub(crate) message: Option<String>,
    pub(crate) budget: RunBudget,
    pub(crate) lineage_root: PathBuf,
}

pub(crate) struct Selection {
    pub(crate) benchmark: PathBuf,
    pub(crate) message: Option<String>,
    pub(crate) budget: RunBudget,
}

pub(crate) fn select(options: Options) -> Result<Option<Selection>> {
    let mut app = App::load(options)?;
    terminal_ui::run("run picker", &mut app, render, App::handle_key)?;
    Ok(app.selection)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EditorKind {
    Filter,
    Message,
    Directory,
    Budget,
}

#[derive(Clone, Debug)]
struct Editor {
    kind: EditorKind,
    value: String,
    cursor: usize,
    original_filter: String,
    error: Option<String>,
}

impl Editor {
    fn new(kind: EditorKind, value: String, original_filter: String) -> Self {
        let cursor = value.chars().count();
        Self {
            kind,
            value,
            cursor,
            original_filter,
            error: None,
        }
    }

    fn insert(&mut self, character: char) {
        let offset = char_offset(&self.value, self.cursor);
        self.value.insert(offset, character);
        self.cursor += 1;
        self.error = None;
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let end = char_offset(&self.value, self.cursor);
        let start = char_offset(&self.value, self.cursor - 1);
        self.value.replace_range(start..end, "");
        self.cursor -= 1;
        self.error = None;
    }

    fn delete(&mut self) {
        if self.cursor >= self.value.chars().count() {
            return;
        }
        let start = char_offset(&self.value, self.cursor);
        let end = char_offset(&self.value, self.cursor + 1);
        self.value.replace_range(start..end, "");
        self.error = None;
    }
}

struct App {
    workspace_root: PathBuf,
    history: HashMap<String, HistoryIndexEntry>,
    catalog: Catalog,
    selected_path: Option<PathBuf>,
    filter: String,
    message: Option<String>,
    budget: RunBudget,
    editor: Option<Editor>,
    notice: Option<String>,
    selection: Option<Selection>,
}

impl App {
    fn load(options: Options) -> Result<Self> {
        let workspace_root = fs::canonicalize(std::env::current_dir()?)
            .context("failed to resolve the benchmark workspace")?;
        let history = load_history(&options.lineage_root)?;
        let catalog = Catalog::scan(&workspace_root, &options.directory, &history)?;
        let selected_path = catalog.modules.first().map(|module| module.path.clone());
        Ok(Self {
            workspace_root,
            history,
            catalog,
            selected_path,
            filter: String::new(),
            message: options.message,
            budget: options.budget,
            editor: None,
            notice: None,
            selection: None,
        })
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<ControlFlow> {
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'C'))
        {
            return Ok(ControlFlow::Quit);
        }
        if self.editor.is_some() {
            return self.handle_editor_key(key);
        }
        match key.code {
            KeyCode::Char('q' | 'Q') | KeyCode::Esc => return Ok(ControlFlow::Quit),
            KeyCode::Up | KeyCode::Char('k' | 'K') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j' | 'J') => self.move_selection(1),
            KeyCode::Home => self.select_edge(false),
            KeyCode::End => self.select_edge(true),
            KeyCode::Char('/') => self.open_editor(EditorKind::Filter),
            KeyCode::Char('m' | 'M') | KeyCode::Enter => {
                if self.selected_module().is_some() {
                    self.open_editor(EditorKind::Message);
                } else {
                    self.notice = Some("no benchmark module is selected".to_owned());
                }
            }
            KeyCode::Char('b' | 'B') => self.open_editor(EditorKind::Budget),
            KeyCode::Char('d' | 'D') => self.open_editor(EditorKind::Directory),
            KeyCode::Char('e' | 'E') => {
                self.notice =
                    Some("managed runs capture chromium, firefox, and webkit together".to_owned());
            }
            _ => {}
        }
        Ok(ControlFlow::Continue)
    }

    fn handle_editor_key(&mut self, key: KeyEvent) -> Result<ControlFlow> {
        match key.code {
            KeyCode::Esc => {
                if let Some(editor) = self.editor.take()
                    && editor.kind == EditorKind::Filter
                {
                    self.filter = editor.original_filter;
                    self.ensure_selection();
                }
            }
            KeyCode::Enter => return self.submit_editor(),
            KeyCode::Left => {
                if let Some(editor) = &mut self.editor {
                    editor.cursor = editor.cursor.saturating_sub(1);
                }
            }
            KeyCode::Right => {
                if let Some(editor) = &mut self.editor {
                    editor.cursor = (editor.cursor + 1).min(editor.value.chars().count());
                }
            }
            KeyCode::Home => {
                if let Some(editor) = &mut self.editor {
                    editor.cursor = 0;
                }
            }
            KeyCode::End => {
                if let Some(editor) = &mut self.editor {
                    editor.cursor = editor.value.chars().count();
                }
            }
            KeyCode::Backspace => {
                if let Some(editor) = &mut self.editor {
                    editor.backspace();
                }
                self.sync_filter_editor();
            }
            KeyCode::Delete => {
                if let Some(editor) = &mut self.editor {
                    editor.delete();
                }
                self.sync_filter_editor();
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Some(editor) = &mut self.editor {
                    editor.insert(character);
                }
                self.sync_filter_editor();
            }
            _ => {}
        }
        Ok(ControlFlow::Continue)
    }

    fn open_editor(&mut self, kind: EditorKind) {
        let value = match kind {
            EditorKind::Filter => self.filter.clone(),
            EditorKind::Message => self.message.clone().unwrap_or_default(),
            EditorKind::Directory => display_path(&self.catalog.root, &self.workspace_root),
            EditorKind::Budget => self.budget.to_string(),
        };
        self.editor = Some(Editor::new(kind, value, self.filter.clone()));
        self.notice = None;
    }

    fn submit_editor(&mut self) -> Result<ControlFlow> {
        let Some(editor) = self.editor.take() else {
            return Ok(ControlFlow::Continue);
        };
        match editor.kind {
            EditorKind::Filter => {
                self.filter = editor.value;
                self.ensure_selection();
            }
            EditorKind::Message => {
                let Some(benchmark) = self.selected_module().map(|module| module.path.clone())
                else {
                    self.notice = Some("no benchmark module is selected".to_owned());
                    return Ok(ControlFlow::Continue);
                };
                let message = non_empty(editor.value);
                self.message = message.clone();
                self.selection = Some(Selection {
                    benchmark,
                    message,
                    budget: self.budget,
                });
                return Ok(ControlFlow::Quit);
            }
            EditorKind::Directory => {
                let requested = PathBuf::from(editor.value.trim());
                match Catalog::scan(&self.workspace_root, &requested, &self.history) {
                    Ok(catalog) => {
                        self.catalog = catalog;
                        self.filter.clear();
                        self.selected_path = self
                            .catalog
                            .modules
                            .first()
                            .map(|module| module.path.clone());
                        self.notice = None;
                    }
                    Err(error) => {
                        let mut editor =
                            Editor::new(EditorKind::Directory, editor.value, self.filter.clone());
                        editor.error = Some(format!("{error:#}"));
                        self.editor = Some(editor);
                    }
                }
            }
            EditorKind::Budget => match RunBudget::from_str(editor.value.trim()) {
                Ok(budget) => {
                    self.budget = budget;
                    self.notice = Some(format!("measurement budget set to {budget}"));
                }
                Err(error) => {
                    let mut editor =
                        Editor::new(EditorKind::Budget, editor.value, self.filter.clone());
                    editor.error = Some(error);
                    self.editor = Some(editor);
                }
            },
        }
        Ok(ControlFlow::Continue)
    }

    fn sync_filter_editor(&mut self) {
        let Some(editor) = &self.editor else {
            return;
        };
        if editor.kind == EditorKind::Filter {
            self.filter.clone_from(&editor.value);
            self.ensure_selection();
        }
    }

    fn visible_indices(&self) -> Vec<usize> {
        let needle = self.filter.trim().to_ascii_lowercase();
        self.catalog
            .modules
            .iter()
            .enumerate()
            .filter(|(_, module)| {
                needle.is_empty()
                    || module.id.to_ascii_lowercase().contains(&needle)
                    || module.relative_path.to_ascii_lowercase().contains(&needle)
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn selected_module(&self) -> Option<&BenchmarkModule> {
        let selected = self.selected_path.as_ref()?;
        self.catalog
            .modules
            .iter()
            .find(|module| module.path == *selected)
    }

    fn ensure_selection(&mut self) {
        let visible = self.visible_indices();
        let selected_is_visible = self.selected_path.as_ref().is_some_and(|selected| {
            visible
                .iter()
                .any(|index| self.catalog.modules[*index].path == *selected)
        });
        if !selected_is_visible {
            self.selected_path = visible
                .first()
                .map(|index| self.catalog.modules[*index].path.clone());
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let visible = self.visible_indices();
        if visible.is_empty() {
            self.selected_path = None;
            return;
        }
        let current = self
            .selected_path
            .as_ref()
            .and_then(|selected| {
                visible
                    .iter()
                    .position(|index| self.catalog.modules[*index].path == *selected)
            })
            .unwrap_or_default();
        let next = offset_index(current, delta, visible.len());
        self.selected_path = Some(self.catalog.modules[visible[next]].path.clone());
        self.notice = None;
    }

    fn select_edge(&mut self, end: bool) {
        let visible = self.visible_indices();
        let selected = if end { visible.last() } else { visible.first() };
        self.selected_path = selected.map(|index| self.catalog.modules[*index].path.clone());
        self.notice = None;
    }
}

struct Catalog {
    root: PathBuf,
    modules: Vec<BenchmarkModule>,
    nested_count: usize,
    known_case_count: usize,
}

impl Catalog {
    fn scan(
        workspace_root: &Path,
        requested: &Path,
        history: &HashMap<String, HistoryIndexEntry>,
    ) -> Result<Self> {
        let requested = if requested.is_absolute() {
            requested.to_owned()
        } else {
            workspace_root.join(requested)
        };
        let root = fs::canonicalize(&requested).with_context(|| {
            format!(
                "failed to resolve benchmark directory {}",
                requested.display()
            )
        })?;
        if !root.is_dir() {
            bail!(
                "benchmark selection requires a directory: {}",
                root.display()
            );
        }
        if !root.starts_with(workspace_root) {
            bail!(
                "benchmark directory {} is outside the current workspace {}",
                root.display(),
                workspace_root.display()
            );
        }
        let paths = benchmark_paths(&root)?;
        let nested_count = paths
            .iter()
            .filter(|path| path.parent() != Some(root.as_path()))
            .count();
        let mut modules = paths
            .into_iter()
            .map(|path| BenchmarkModule::load(workspace_root, path, history))
            .collect::<Result<Vec<_>>>()?;
        modules.sort_by(|left, right| {
            right
                .history
                .as_ref()
                .map(|history| history.latest_recorded_at_unix_ms)
                .unwrap_or_default()
                .cmp(
                    &left
                        .history
                        .as_ref()
                        .map(|history| history.latest_recorded_at_unix_ms)
                        .unwrap_or_default(),
                )
                .then_with(|| left.id.cmp(&right.id))
        });
        let known_case_count = modules.iter().map(|module| module.cases.len()).sum();
        Ok(Self {
            root,
            modules,
            nested_count,
            known_case_count,
        })
    }
}

struct BenchmarkModule {
    path: PathBuf,
    relative_path: String,
    id: String,
    cases: Vec<CasePreview>,
    fixtures: Vec<FixturePreview>,
    file_size: u64,
    metadata_note: Option<String>,
    history: Option<HistoryIndexEntry>,
}

impl BenchmarkModule {
    fn load(
        workspace_root: &Path,
        path: PathBuf,
        history: &HashMap<String, HistoryIndexEntry>,
    ) -> Result<Self> {
        let source = fs::read_to_string(&path)
            .with_context(|| format!("failed to read benchmark module {}", path.display()))?;
        let parsed = parse_benchmark_source(&source);
        let fallback_id = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".bench.ts"))
            .unwrap_or("benchmark")
            .to_owned();
        let id = parsed.id.unwrap_or_else(|| fallback_id.clone());
        let history = history
            .get(&id)
            .or_else(|| history.get(&fallback_id))
            .cloned();
        let relative_path = display_path(&path, workspace_root);
        let fixtures = parsed
            .fixtures
            .into_iter()
            .map(|source| FixturePreview::load(&path, source))
            .collect();
        let metadata_note = if parsed.literal_definition {
            None
        } else {
            Some("metadata is dynamic; bperf validates it before measurement".to_owned())
        };
        Ok(Self {
            file_size: fs::metadata(&path)?.len(),
            path,
            relative_path,
            id,
            cases: parsed.cases,
            fixtures,
            metadata_note,
            history,
        })
    }
}

struct CasePreview {
    id: String,
    setup: bool,
    exact: bool,
}

struct FixturePreview {
    source: String,
    size: Option<u64>,
}

impl FixturePreview {
    fn load(benchmark: &Path, source: String) -> Self {
        let size = benchmark
            .parent()
            .map(|parent| parent.join(&source))
            .and_then(|path| fs::metadata(path).ok())
            .map(|metadata| metadata.len());
        Self { source, size }
    }
}

fn load_history(root: &Path) -> Result<HashMap<String, HistoryIndexEntry>> {
    let Some(index) = lineage::history_index_if_present(root)? else {
        return Ok(HashMap::new());
    };
    Ok(index
        .benchmarks
        .into_iter()
        .map(|entry| (entry.benchmark_id.clone(), entry))
        .collect())
}

fn benchmark_paths(root: &Path) -> Result<Vec<PathBuf>> {
    let mut pending = vec![root.to_owned()];
    let mut paths = Vec::new();
    while let Some(directory) = pending.pop() {
        let mut entries = fs::read_dir(&directory)
            .with_context(|| format!("failed to scan benchmark directory {}", directory.display()))?
            .collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries.into_iter().rev() {
            let file_type = entry.file_type()?;
            if file_type.is_dir() && !file_type.is_symlink() {
                pending.push(entry.path());
            } else if file_type.is_file()
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.ends_with(".bench.ts"))
            {
                paths.push(entry.path());
            }
        }
    }
    paths.sort();
    Ok(paths)
}

struct ParsedBenchmark {
    id: Option<String>,
    cases: Vec<CasePreview>,
    fixtures: Vec<String>,
    literal_definition: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Token {
    Identifier(String),
    String(String),
    Punct(char),
}

fn parse_benchmark_source(source: &str) -> ParsedBenchmark {
    let tokens = tokenize_typescript(source);
    let mut fixtures = Vec::new();
    for index in 0..tokens.len().saturating_sub(2) {
        if matches!(&tokens[index], Token::Identifier(name) if name == "fixture")
            && tokens.get(index + 1) == Some(&Token::Punct('('))
            && let Some(Token::String(source)) = tokens.get(index + 2)
            && !fixtures.contains(source)
        {
            fixtures.push(source.clone());
        }
    }

    let Some(call) = tokens.iter().enumerate().position(|(index, token)| {
        matches!(token, Token::Identifier(name) if name == "defineBrowserBenchmark")
            && tokens.get(index + 1) == Some(&Token::Punct('('))
            && tokens.get(index + 2) == Some(&Token::Punct('{'))
    }) else {
        return ParsedBenchmark {
            id: None,
            cases: Vec::new(),
            fixtures,
            literal_definition: false,
        };
    };
    let open = call + 2;
    let Some(close) = matching_delimiter(&tokens, open, '{', '}') else {
        return ParsedBenchmark {
            id: None,
            cases: Vec::new(),
            fixtures,
            literal_definition: false,
        };
    };
    let id = string_property(&tokens, open + 1, close, "id");
    let cases = array_property(&tokens, open + 1, close, "cases")
        .map(|(start, end)| parse_cases(&tokens, start, end))
        .unwrap_or_default();
    ParsedBenchmark {
        literal_definition: id.is_some(),
        id,
        cases,
        fixtures,
    }
}

fn parse_cases(tokens: &[Token], start: usize, end: usize) -> Vec<CasePreview> {
    let mut cases = Vec::new();
    let mut index = start;
    while index < end {
        if tokens[index] != Token::Punct('{') {
            index += 1;
            continue;
        }
        let Some(close) = matching_delimiter(tokens, index, '{', '}') else {
            break;
        };
        if close > end {
            break;
        }
        if let Some(id) = string_property(tokens, index + 1, close, "id") {
            cases.push(CasePreview {
                id,
                setup: has_member(tokens, index + 1, close, "setup"),
                exact: property_calls(tokens, index + 1, close, "expect", "exact"),
            });
        }
        index = close + 1;
    }
    cases
}

fn string_property(tokens: &[Token], start: usize, end: usize, name: &str) -> Option<String> {
    property_value_index(tokens, start, end, name).and_then(|index| match tokens.get(index) {
        Some(Token::String(value)) => Some(value.clone()),
        _ => None,
    })
}

fn array_property(
    tokens: &[Token],
    start: usize,
    end: usize,
    name: &str,
) -> Option<(usize, usize)> {
    let open = property_value_index(tokens, start, end, name)?;
    if tokens.get(open) != Some(&Token::Punct('[')) {
        return None;
    }
    let close = matching_delimiter(tokens, open, '[', ']')?;
    (close <= end).then_some((open + 1, close))
}

fn has_member(tokens: &[Token], start: usize, end: usize, name: &str) -> bool {
    if property_value_index(tokens, start, end, name).is_some() {
        return true;
    }
    let mut depths = [0_i32; 3];
    let mut index = start;
    while index + 1 < end {
        match tokens[index] {
            Token::Punct('{') => depths[0] += 1,
            Token::Punct('}') => depths[0] -= 1,
            Token::Punct('[') => depths[1] += 1,
            Token::Punct(']') => depths[1] -= 1,
            Token::Punct('(') => depths[2] += 1,
            Token::Punct(')') => depths[2] -= 1,
            _ => {}
        }
        if depths == [0, 0, 0]
            && token_name(&tokens[index]) == Some(name)
            && tokens.get(index + 1) == Some(&Token::Punct('('))
        {
            return true;
        }
        index += 1;
    }
    false
}

fn property_calls(
    tokens: &[Token],
    start: usize,
    end: usize,
    property: &str,
    function: &str,
) -> bool {
    let Some(value) = property_value_index(tokens, start, end, property) else {
        return false;
    };
    matches!(tokens.get(value), Some(Token::Identifier(name)) if name == function)
        && tokens.get(value + 1) == Some(&Token::Punct('('))
}

fn property_value_index(tokens: &[Token], start: usize, end: usize, name: &str) -> Option<usize> {
    let mut depths = [0_i32; 3];
    let mut index = start;
    while index + 2 < end {
        match tokens[index] {
            Token::Punct('{') => depths[0] += 1,
            Token::Punct('}') => depths[0] -= 1,
            Token::Punct('[') => depths[1] += 1,
            Token::Punct(']') => depths[1] -= 1,
            Token::Punct('(') => depths[2] += 1,
            Token::Punct(')') => depths[2] -= 1,
            _ => {}
        }
        if depths == [0, 0, 0]
            && token_name(&tokens[index]) == Some(name)
            && tokens.get(index + 1) == Some(&Token::Punct(':'))
        {
            return Some(index + 2);
        }
        index += 1;
    }
    None
}

fn token_name(token: &Token) -> Option<&str> {
    match token {
        Token::Identifier(name) | Token::String(name) => Some(name),
        Token::Punct(_) => None,
    }
}

fn matching_delimiter(tokens: &[Token], open: usize, left: char, right: char) -> Option<usize> {
    let mut depth = 0_u32;
    for (index, token) in tokens.iter().enumerate().skip(open) {
        if *token == Token::Punct(left) {
            depth += 1;
        } else if *token == Token::Punct(right) {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                return Some(index);
            }
        }
    }
    None
}

fn tokenize_typescript(source: &str) -> Vec<Token> {
    let characters = source.chars().collect::<Vec<_>>();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < characters.len() {
        let character = characters[index];
        if character.is_whitespace() {
            index += 1;
        } else if character == '/' && characters.get(index + 1) == Some(&'/') {
            index += 2;
            while index < characters.len() && characters[index] != '\n' {
                index += 1;
            }
        } else if character == '/' && characters.get(index + 1) == Some(&'*') {
            index += 2;
            while index + 1 < characters.len()
                && !(characters[index] == '*' && characters[index + 1] == '/')
            {
                index += 1;
            }
            index = (index + 2).min(characters.len());
        } else if matches!(character, '\'' | '"') {
            let quote = character;
            index += 1;
            let mut value = String::new();
            while index < characters.len() {
                let character = characters[index];
                index += 1;
                if character == quote {
                    break;
                }
                if character == '\\' && index < characters.len() {
                    value.push(characters[index]);
                    index += 1;
                } else {
                    value.push(character);
                }
            }
            tokens.push(Token::String(value));
        } else if character == '`' {
            index += 1;
            while index < characters.len() {
                let character = characters[index];
                index += 1;
                if character == '\\' {
                    index = (index + 1).min(characters.len());
                } else if character == '`' {
                    break;
                }
            }
        } else if character.is_ascii_alphabetic() || matches!(character, '_' | '$') {
            let start = index;
            index += 1;
            while index < characters.len()
                && (characters[index].is_ascii_alphanumeric()
                    || matches!(characters[index], '_' | '$'))
            {
                index += 1;
            }
            tokens.push(Token::Identifier(characters[start..index].iter().collect()));
        } else {
            tokens.push(Token::Punct(character));
            index += 1;
        }
    }
    tokens
}

fn char_offset(value: &str, character_index: usize) -> usize {
    value
        .char_indices()
        .nth(character_index)
        .map_or(value.len(), |(offset, _)| offset)
}

fn non_empty(value: String) -> Option<String> {
    let value = value.trim().to_owned();
    (!value.is_empty()).then_some(value)
}

fn offset_index(index: usize, delta: isize, count: usize) -> usize {
    if delta < 0 {
        index.saturating_sub(delta.unsigned_abs())
    } else {
        index
            .saturating_add(delta as usize)
            .min(count.saturating_sub(1))
    }
}

fn display_path(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn render(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    frame.render_widget(Block::new().style(Style::default().bg(BG)), area);
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        render_too_small(frame, area);
        return;
    }

    let sections = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(2),
        Constraint::Min(10),
        Constraint::Length(2),
        Constraint::Length(3),
    ])
    .split(area);
    render_header(frame, sections[0], app);
    render_scan_line(frame, sections[1], app);
    render_content(frame, sections[2], app);
    render_context(frame, sections[3], app);
    render_footer(frame, sections[4], app);
}

fn render_too_small(frame: &mut Frame<'_>, area: Rect) {
    let block = chrome_block();
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                "bperf run",
                Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(format!(
                "Terminal is {}x{}; resize to at least {MIN_WIDTH}x{MIN_HEIGHT}.",
                area.width, area.height
            )),
            Line::from(""),
            Line::from(Span::styled("q quit", Style::default().fg(MUTED))),
        ])
        .style(Style::default().fg(TEXT).bg(BG))
        .alignment(Alignment::Center),
        inner,
    );
}

fn render_header(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let block = chrome_block().style(Style::default().bg(SURFACE));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let columns = Layout::horizontal([Constraint::Min(40), Constraint::Length(24)]).split(inner);
    let root = display_path(&app.catalog.root, &app.workspace_root);
    let line = Line::from(vec![
        Span::styled(
            " bperf ",
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
        ),
        Span::styled("run  ", Style::default().fg(MUTED)),
        Span::styled(
            format!("{root}/"),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  │  budget  ", Style::default().fg(FAINT)),
        Span::styled(
            app.budget.to_string(),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  │  engines  ", Style::default().fg(FAINT)),
        Span::styled(
            "chromium firefox webkit",
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(line).style(Style::default().bg(SURFACE)),
        columns[0],
    );
    frame.render_widget(
        Paragraph::new(format!("00:00 / {} ", budget_clock(app.budget)))
            .style(Style::default().fg(MUTED).bg(SURFACE))
            .alignment(Alignment::Right),
        columns[1],
    );
}

fn render_scan_line(frame: &mut Frame<'_>, area: Rect, app: &App) {
    frame.render_widget(
        Block::new()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(BORDER_COLOR))
            .style(Style::default().bg(BG)),
        area,
    );
    let line_area = Rect::new(area.x, area.y, area.width, 1);
    let columns = Layout::horizontal([Constraint::Percentage(48), Constraint::Percentage(52)])
        .split(line_area);
    let selected = app
        .selected_module()
        .map(|module| module.relative_path.as_str())
        .unwrap_or_else(|| app.catalog.root.to_str().unwrap_or("benchmarks"));
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" $  ", Style::default().fg(FAINT)),
            Span::styled("bperf run ", Style::default().fg(TEXT)),
            Span::styled(
                clip(selected, usize::from(columns[0].width.saturating_sub(15))),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
        ])),
        columns[0],
    );
    let case_label = if app
        .catalog
        .modules
        .iter()
        .any(|module| module.cases.is_empty())
    {
        format!("{} known cases", app.catalog.known_case_count)
    } else {
        format!("{} cases", app.catalog.known_case_count)
    };
    frame.render_widget(
        Paragraph::new(format!(
            "scanning {}/**/*.bench.ts · {} modules · {case_label} · {} nested ",
            display_path(&app.catalog.root, &app.workspace_root),
            app.catalog.modules.len(),
            app.catalog.nested_count
        ))
        .style(Style::default().fg(MUTED))
        .alignment(Alignment::Right),
        columns[1],
    );
}

const BORDER_COLOR: ratatui::style::Color = crate::terminal_ui::BORDER;

fn render_content(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if area.width >= HORIZONTAL_LAYOUT {
        let columns = Layout::horizontal([Constraint::Percentage(43), Constraint::Percentage(57)])
            .split(area);
        render_module_list(frame, columns[0], app);
        if area.height >= 30 {
            render_wide_detail(frame, columns[1], app);
        } else {
            render_compact_detail(frame, columns[1], app);
        }
    } else {
        let rows =
            Layout::vertical([Constraint::Percentage(44), Constraint::Percentage(56)]).split(area);
        render_module_list(frame, rows[0], app);
        render_compact_detail(frame, rows[1], app);
    }
}

fn render_module_list(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let visible = app.visible_indices();
    let title_width = usize::from(area.width.saturating_sub(2));
    let title = fit_sides(
        " BENCHMARK MODULES",
        "runs · last verdict · when ",
        title_width,
    );
    let block = chrome_block().title(Line::from(vec![
        Span::styled(
            clip(" BENCHMARK MODULES", title_width),
            Style::default().fg(AMBER),
        ),
        Span::styled(
            title
                .strip_prefix(" BENCHMARK MODULES")
                .unwrap_or_default()
                .to_owned(),
            Style::default().fg(MUTED),
        ),
    ]));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if visible.is_empty() {
        let message = if app.catalog.modules.is_empty() {
            "No .bench.ts modules found. Press d to change directory."
        } else {
            "No benchmark modules match the filter. Press / to edit it."
        };
        frame.render_widget(
            Paragraph::new(message)
                .style(Style::default().fg(MUTED))
                .wrap(Wrap { trim: true }),
            inner,
        );
        return;
    }

    let items = visible
        .iter()
        .map(|index| module_item(&app.catalog.modules[*index], inner.width))
        .collect::<Vec<_>>();
    let selected = app.selected_path.as_ref().and_then(|selected| {
        visible
            .iter()
            .position(|index| app.catalog.modules[*index].path == *selected)
    });
    let mut state = ListState::default();
    state.select(selected);
    let list = List::new(items)
        .style(Style::default().fg(TEXT).bg(BG))
        .highlight_style(Style::default().bg(FOCUS).add_modifier(Modifier::BOLD))
        .highlight_symbol("› ");
    StatefulWidget::render(list, inner, frame.buffer_mut(), &mut state);
}

fn module_item(module: &BenchmarkModule, width: u16) -> ListItem<'static> {
    let available = usize::from(width.saturating_sub(3));
    let history = module.history.as_ref();
    let run_summary = history.map_or_else(
        || "no runs  ·  never".to_owned(),
        |history| {
            format!(
                "{} {}  {}  {}",
                history.cycle_count,
                if history.cycle_count == 1 {
                    "run"
                } else {
                    "runs"
                },
                outcome_mark(&history.latest_outcome),
                relative_age(history.latest_recorded_at_unix_ms)
            )
        },
    );
    let path_space =
        available.saturating_sub(module.id.chars().count() + run_summary.chars().count() + 3);
    let mut first = vec![
        Span::styled(
            module.id.clone(),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("{:<path_space$}", clip(&module.relative_path, path_space)),
            Style::default().fg(MUTED),
        ),
    ];
    if let Some(history) = history {
        first.extend([
            Span::styled(
                format!(
                    "{} {}  ",
                    history.cycle_count,
                    if history.cycle_count == 1 {
                        "run"
                    } else {
                        "runs"
                    }
                ),
                Style::default().fg(TEXT),
            ),
            Span::styled(
                outcome_mark(&history.latest_outcome).to_string(),
                Style::default()
                    .fg(outcome_color(&history.latest_outcome))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {}", relative_age(history.latest_recorded_at_unix_ms)),
                Style::default().fg(MUTED),
            ),
        ]);
    } else {
        first.push(Span::styled(run_summary, Style::default().fg(MUTED)));
    }
    let first = Line::from(first);
    let cases = if module.cases.is_empty() {
        "cases unknown".to_owned()
    } else {
        format!("{} cases", module.cases.len())
    };
    let baseline = history
        .and_then(|history| history.current_baseline_label.as_deref())
        .map_or_else(
            || "baseline none".to_owned(),
            |label| format!("baseline {label}"),
        );
    let walls = history
        .and_then(|history| history.latest_comparison.as_ref())
        .map(compact_wall_values)
        .unwrap_or_else(|| "never measured".to_owned());
    let summary = format!("{cases} · {baseline} · {walls}");
    let graph = history
        .and_then(|history| history.wall_history_ms.get(&Engine::Chromium))
        .map(|values| sparkline(values, 10))
        .unwrap_or_default();
    let graph_width = graph.chars().count();
    let second_width = available.saturating_sub(graph_width + usize::from(!graph.is_empty()));
    let second = Line::from(vec![
        Span::styled(
            format!("{:<second_width$}", clip(&summary, second_width)),
            Style::default().fg(MUTED),
        ),
        Span::styled(graph, Style::default().fg(CYAN)),
    ]);
    ListItem::new(vec![first, second, Line::from("")])
}

fn render_wide_detail(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(module) = app.selected_module() else {
        render_empty_detail(frame, area);
        return;
    };
    let rows = Layout::vertical([
        Constraint::Length(4),
        Constraint::Length(11),
        Constraint::Length(8),
        Constraint::Min(7),
    ])
    .split(area);
    render_module_summary(frame, rows[0], module);
    render_cases_and_medians(frame, rows[1], module);
    render_trend(frame, rows[2], module);
    render_action(frame, rows[3], app, module);
}

fn render_compact_detail(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(module) = app.selected_module() else {
        render_empty_detail(frame, area);
        return;
    };
    let block = chrome_block().title(Line::from(vec![
        Span::styled(
            format!(" {} ", module.id),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{} ", module.relative_path),
            Style::default().fg(MUTED),
        ),
    ]));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let history = module.history.as_ref();
    let mut lines = vec![
        Line::from(Span::styled(
            history.map_or_else(
                || "no recorded runs".to_owned(),
                |history| {
                    format!(
                        "{} runs · {} accepted · last {}",
                        history.cycle_count,
                        history.accepted_count,
                        relative_age(history.latest_recorded_at_unix_ms)
                    )
                },
            ),
            Style::default().fg(MUTED),
        )),
        Line::from(""),
    ];
    for case in module.cases.iter().take(3) {
        lines.push(case_line(case, usize::from(inner.width)));
    }
    if module.cases.is_empty() {
        lines.push(Line::from(Span::styled(
            "Cases are discovered during run preflight.",
            Style::default().fg(MUTED),
        )));
    }
    lines.push(Line::from(""));
    if let Some(history) = history {
        lines.push(Line::from(vec![
            Span::styled("baseline  ", Style::default().fg(MUTED)),
            Span::styled(
                history.current_baseline_label.as_deref().unwrap_or("none"),
                Style::default().fg(TEXT),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::styled("last msg  ", Style::default().fg(MUTED)),
            Span::styled(
                clip(
                    history.latest_message.as_deref().unwrap_or("(no message)"),
                    usize::from(inner.width.saturating_sub(10)),
                ),
                Style::default().fg(TEXT),
            ),
        ]));
    }
    lines.push(Line::from(""));
    lines.extend(editor_or_action_lines(
        app,
        module,
        usize::from(inner.width),
    ));
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(BG))
            .wrap(Wrap { trim: true }),
        inner,
    );
}

fn render_empty_detail(frame: &mut Frame<'_>, area: Rect) {
    let block = chrome_block().title(" RUN ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new("Select a benchmark module or change the scan directory.")
            .style(Style::default().fg(MUTED))
            .alignment(Alignment::Center),
        inner,
    );
}

fn render_module_summary(frame: &mut Frame<'_>, area: Rect, module: &BenchmarkModule) {
    let block = chrome_block();
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let history = module.history.as_ref();
    let outcome = history.map(|history| history.latest_outcome.as_str());
    let right = outcome
        .map(str::to_ascii_uppercase)
        .unwrap_or_else(|| "NEVER RUN".to_owned());
    let left_width = usize::from(inner.width).saturating_sub(right.chars().count() + 1);
    let left = format!(
        "{:<left_width$}",
        clip(
            &format!(
                "{}  {}  {}",
                module.id,
                module.relative_path,
                human_bytes(module.file_size)
            ),
            left_width
        )
    );
    let detail = history.map_or_else(
        || "no runs · no promoted baseline".to_owned(),
        |history| {
            format!(
                "{} runs · {} accepted · last {}",
                history.cycle_count,
                history.accepted_count,
                relative_age(history.latest_recorded_at_unix_ms)
            )
        },
    );
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(left, Style::default().fg(TEXT).add_modifier(Modifier::BOLD)),
                Span::styled(
                    right,
                    Style::default()
                        .fg(outcome.map_or(MUTED, outcome_color))
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(Span::styled(detail, Style::default().fg(MUTED))),
        ]),
        inner,
    );
}

fn render_cases_and_medians(frame: &mut Frame<'_>, area: Rect, module: &BenchmarkModule) {
    let columns =
        Layout::horizontal([Constraint::Percentage(65), Constraint::Percentage(35)]).split(area);
    let cases_block = chrome_block().title(Span::styled(" CASES ", Style::default().fg(AMBER)));
    let cases_inner = cases_block.inner(columns[0]);
    frame.render_widget(cases_block, columns[0]);
    let mut lines = module
        .cases
        .iter()
        .take(4)
        .map(|case| case_line(case, usize::from(cases_inner.width)))
        .collect::<Vec<_>>();
    if module.cases.is_empty() {
        lines.push(Line::from(Span::styled(
            "Discovered during run preflight.",
            Style::default().fg(MUTED),
        )));
    }
    if !module.fixtures.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "FIXTURES",
            Style::default().fg(AMBER),
        )));
        for fixture in module.fixtures.iter().take(2) {
            let size = fixture
                .size
                .map(human_bytes)
                .unwrap_or_else(|| "missing".to_owned());
            lines.push(Line::from(vec![
                Span::styled(fixture.source.clone(), Style::default().fg(TEXT)),
                Span::styled(format!("  {size}"), Style::default().fg(MUTED)),
            ]));
        }
    }
    frame.render_widget(Paragraph::new(lines), cases_inner);

    let medians_block =
        chrome_block().title(Span::styled(" LAST MEDIANS ", Style::default().fg(AMBER)));
    let medians_inner = medians_block.inner(columns[1]);
    frame.render_widget(medians_block, columns[1]);
    let lines = module
        .history
        .as_ref()
        .and_then(|history| history.latest_comparison.as_ref())
        .map(|comparison| median_lines(comparison, usize::from(medians_inner.width)))
        .unwrap_or_else(|| {
            vec![Line::from(Span::styled(
                "No comparable run yet.",
                Style::default().fg(MUTED),
            ))]
        });
    frame.render_widget(Paragraph::new(lines), medians_inner);
}

fn case_line(case: &CasePreview, width: usize) -> Line<'static> {
    let mut contract = if case.exact {
        "expect exact()".to_owned()
    } else {
        "expect dynamic".to_owned()
    };
    if case.setup {
        contract.push_str(" · setup()");
    }
    Line::from(vec![
        Span::styled(
            format!(
                "{:<id_width$}",
                clip(&case.id, width.saturating_sub(contract.len() + 1)),
                id_width = width.saturating_sub(contract.len() + 1)
            ),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(contract, Style::default().fg(MUTED)),
    ])
}

fn median_lines(comparison: &ComparisonSummary, width: usize) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(fit_sides(
        "",
        "wall       cpu       heap      alloc",
        width,
    ))];
    for engine in Engine::ALL {
        let summary = comparison
            .engines
            .iter()
            .find(|summary| summary.engine == engine);
        let wall = summary
            .and_then(|summary| candidate_metric(summary.metrics.get("workload.wall_ms")))
            .map(format_milliseconds)
            .unwrap_or_else(|| "n/a".to_owned());
        let cpu = summary
            .and_then(|summary| {
                candidate_metric(summary.metrics.get("browser.cpu_profile.active_ms"))
            })
            .map(format_milliseconds)
            .unwrap_or_else(|| "n/a".to_owned());
        let heap = summary
            .and_then(|summary| candidate_metric(summary.metrics.get("browser.js_heap.live_bytes")))
            .map(format_bytes)
            .unwrap_or_else(|| "n/a".to_owned());
        let alloc = summary
            .and_then(|summary| {
                candidate_metric(summary.metrics.get("browser.js_heap.allocated_bytes"))
            })
            .map(format_bytes)
            .unwrap_or_else(|| "n/a".to_owned());
        lines.push(Line::from(fit_sides(
            &engine.to_string(),
            &format!("{wall:>9} {cpu:>9} {heap:>9} {alloc:>9}"),
            width,
        )));
    }
    lines
}

fn render_trend(frame: &mut Frame<'_>, area: Rect, module: &BenchmarkModule) {
    let block = chrome_block().title(Line::from(vec![
        Span::styled(" TREND ", Style::default().fg(AMBER)),
        Span::styled(" wall ms, chromium ", Style::default().fg(MUTED)),
    ]));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let Some(history) = module.history.as_ref() else {
        frame.render_widget(
            Paragraph::new("No run history yet. The first run establishes measured evidence.")
                .style(Style::default().fg(MUTED)),
            inner,
        );
        return;
    };
    let values = history
        .wall_history_ms
        .get(&Engine::Chromium)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let endpoints = values
        .first()
        .zip(values.last())
        .map(|(first, last)| format!("{} → {}", format_number(*first), format_number(*last)))
        .unwrap_or_else(|| "values unavailable".to_owned());
    let trend = format!(
        "{} cycles · {endpoints}  {}",
        values.len(),
        sparkline(values, usize::from(inner.width.saturating_sub(28)))
    );
    let baseline = history.current_baseline_label.as_deref().unwrap_or("none");
    let message = history.latest_message.as_deref().unwrap_or("(no message)");
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(trend, Style::default().fg(CYAN))),
            Line::from(vec![
                Span::styled("baseline   ", Style::default().fg(MUTED)),
                Span::styled(baseline.to_owned(), Style::default().fg(TEXT)),
            ]),
            Line::from(vec![
                Span::styled("last msg   ", Style::default().fg(MUTED)),
                Span::styled(
                    clip(message, usize::from(inner.width.saturating_sub(11))),
                    Style::default().fg(TEXT),
                ),
            ]),
        ]),
        inner,
    );
}

fn render_action(frame: &mut Frame<'_>, area: Rect, app: &App, module: &BenchmarkModule) {
    let block = chrome_block();
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if app.editor.is_none() {
        frame.render_widget(
            Paragraph::new(Span::styled("next", Style::default().fg(MUTED))),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
        if inner.height >= 4 {
            let label = format!("enter  record a message and run {}", module.id);
            let button_width = u16::try_from(label.chars().count() + 4)
                .unwrap_or(inner.width)
                .min(inner.width);
            let button = Rect::new(inner.x, inner.y + 2, button_width, 3);
            let button_block = Block::new()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(AMBER))
                .style(Style::default().bg(BG));
            let button_inner = button_block.inner(button);
            frame.render_widget(button_block, button);
            frame.render_widget(
                Paragraph::new(label)
                    .style(
                        Style::default()
                            .fg(TEXT)
                            .bg(BG)
                            .add_modifier(Modifier::BOLD),
                    )
                    .alignment(Alignment::Center),
                button_inner,
            );
        }
        if inner.height >= 7 {
            let mut lines = vec![Line::from(vec![
                Span::styled(
                    "The message becomes this cycle's hypothesis in ",
                    Style::default().fg(MUTED),
                ),
                Span::styled("bperf history", Style::default().fg(CYAN)),
                Span::styled(".", Style::default().fg(MUTED)),
            ])];
            if let Some(note) = &module.metadata_note {
                lines.push(Line::from(Span::styled(
                    clip(note, usize::from(inner.width)),
                    Style::default().fg(FAINT),
                )));
            }
            frame.render_widget(
                Paragraph::new(lines).wrap(Wrap { trim: true }),
                Rect::new(
                    inner.x,
                    inner.y + 6,
                    inner.width,
                    inner.height.saturating_sub(6),
                ),
            );
        }
        return;
    }
    frame.render_widget(
        Paragraph::new(editor_or_action_lines(
            app,
            module,
            usize::from(inner.width),
        ))
        .style(Style::default().bg(BG))
        .wrap(Wrap { trim: true }),
        inner,
    );
}

fn editor_or_action_lines(app: &App, module: &BenchmarkModule, width: usize) -> Vec<Line<'static>> {
    let Some(editor) = &app.editor else {
        let mut lines = vec![
            Line::from(Span::styled("next", Style::default().fg(MUTED))),
            Line::from(""),
            Line::from(Span::styled(
                clip(
                    &format!(" enter  record a message and run {} ", module.id),
                    width,
                ),
                Style::default()
                    .fg(TEXT)
                    .bg(SELECTED)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    "The message becomes this cycle's hypothesis in ",
                    Style::default().fg(MUTED),
                ),
                Span::styled("bperf history", Style::default().fg(CYAN)),
                Span::styled(".", Style::default().fg(MUTED)),
            ]),
        ];
        if let Some(note) = &module.metadata_note {
            lines.push(Line::from(Span::styled(
                clip(note, width),
                Style::default().fg(FAINT),
            )));
        }
        return lines;
    };

    let (label, hint) = match editor.kind {
        EditorKind::Filter => ("FILTER MODULES", "enter apply · esc restore"),
        EditorKind::Message => ("MESSAGE", "enter start benchmark · esc cancel"),
        EditorKind::Directory => ("BENCHMARK DIRECTORY", "enter scan · esc cancel"),
        EditorKind::Budget => ("MEASUREMENT BUDGET", "enter apply · esc cancel"),
    };
    let value = input_with_cursor(&editor.value, editor.cursor);
    let mut lines = vec![
        Line::from(Span::styled(label, Style::default().fg(AMBER))),
        Line::from(""),
        Line::from(vec![
            Span::styled("> ", Style::default().fg(CYAN)),
            Span::styled(
                clip(&value, width.saturating_sub(2)),
                Style::default().fg(TEXT),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(hint, Style::default().fg(MUTED))),
    ];
    if let Some(error) = &editor.error {
        lines.push(Line::from(Span::styled(
            clip(error, width),
            Style::default().fg(RED),
        )));
    }
    lines
}

fn render_context(frame: &mut Frame<'_>, area: Rect, app: &App) {
    frame.render_widget(
        Block::new()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(BORDER_COLOR))
            .style(Style::default().bg(BG)),
        area,
    );
    let line_area = Rect::new(area.x, area.y + 1, area.width, 1);
    if let Some(notice) = &app.notice {
        frame.render_widget(
            Paragraph::new(format!("  {notice}")).style(Style::default().fg(AMBER)),
            line_area,
        );
        return;
    }
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "  d ",
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("change directory", Style::default().fg(MUTED)),
            Span::styled(
                "    / ",
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("filter modules", Style::default().fg(MUTED)),
            Span::styled(
                "    enter ",
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("pick benchmark", Style::default().fg(MUTED)),
        ])),
        line_area,
    );
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, _app: &App) {
    let block = chrome_block().style(Style::default().bg(SURFACE));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let shortcuts = [
        ("↑↓ / j k", "move"),
        ("enter", "select / start"),
        ("m", "message"),
        ("esc", "back / cancel"),
        ("b", "budget"),
        ("e", "engines"),
        ("d", "directory"),
        ("q", "quit"),
    ];
    let mut spans = Vec::new();
    for (index, (key, label)) in shortcuts.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("   "));
        }
        spans.push(Span::styled(
            format!("{key} "),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(label, Style::default().fg(MUTED)));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(SURFACE)),
        inner,
    );
}

fn compact_wall_values(comparison: &ComparisonSummary) -> String {
    let values = Engine::ALL
        .into_iter()
        .map(|engine| {
            comparison
                .engines
                .iter()
                .find(|summary| summary.engine == engine)
                .and_then(|summary| summary.metrics.get("workload.wall_ms"))
                .and_then(|metric| metric.candidate_value)
                .map(format_number)
                .unwrap_or_else(|| "n/a".to_owned())
        })
        .collect::<Vec<_>>();
    format!("wall {}ms", values.join("/"))
}

fn candidate_metric(metric: Option<&MetricSummary>) -> Option<f64> {
    metric.and_then(|metric| metric.candidate_value)
}

fn format_milliseconds(value: f64) -> String {
    if value < 1.0 {
        format!("{:.1} us", value * 1_000.0)
    } else {
        format!("{} ms", format_number(value))
    }
}

fn format_bytes(value: f64) -> String {
    if value >= 1_048_576.0 {
        format!("{:.1} MB", value / 1_048_576.0)
    } else if value >= 1_024.0 {
        format!("{:.1} KB", value / 1_024.0)
    } else {
        format!("{value:.0} B")
    }
}

fn format_number(value: f64) -> String {
    if value.abs() >= 100.0 {
        format!("{value:.0}")
    } else if value.abs() >= 10.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.2}")
    }
}

fn human_bytes(bytes: u64) -> String {
    if bytes >= 1_048_576 {
        format!("{:.1} MiB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1_024 {
        format!("{:.1} KiB", bytes as f64 / 1_024.0)
    } else {
        format!("{bytes} B")
    }
}

fn sparkline(values: &[f64], width: usize) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    if values.is_empty() || width == 0 {
        return String::new();
    }
    let start = values.len().saturating_sub(width);
    let values = &values[start..];
    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let span = (max - min).max(f64::EPSILON);
    values
        .iter()
        .map(|value| {
            let level = (((value - min) / span) * 7.0).round() as usize;
            BARS[level.min(BARS.len() - 1)]
        })
        .collect()
}

fn outcome_mark(outcome: &str) -> char {
    match outcome {
        "positive" => 'P',
        "equivalent" => 'E',
        "inconclusive" => 'I',
        "negative" => 'N',
        _ => '·',
    }
}

fn outcome_color(outcome: &str) -> ratatui::style::Color {
    match outcome {
        "positive" => GREEN,
        "equivalent" => BLUE,
        "inconclusive" => AMBER,
        "negative" => RED,
        _ => MUTED,
    }
}

fn input_with_cursor(value: &str, cursor: usize) -> String {
    let offset = char_offset(value, cursor);
    format!("{}█{}", &value[..offset], &value[offset..])
}

fn budget_clock(budget: RunBudget) -> String {
    let seconds = budget.milliseconds() / 1_000;
    let minutes = seconds / 60;
    let seconds = seconds % 60;
    format!("{minutes:02}:{seconds:02}")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use bperf_decision::comparison::EngineSummary;
    use crossterm::event::KeyEvent;
    use ratatui::{Terminal, backend::TestBackend};
    use tempfile::{TempDir, tempdir};

    use super::*;

    const BENCHMARK_SOURCE: &str = r#"
import {
  defineBrowserBenchmark,
  exact,
  fixture,
} from "bperf/browser";

const stream = fixture("./stream.bin");

export default defineBrowserBenchmark({
  id: "ts-demuxer",
  cases: [
    {
      id: "ts-188",
      setup() {},
      expect: exact({ packets: 12 }),
      measure() {},
    },
    {
      id: "aac-adts",
      setup: () => {},
      expect: exact({ frames: 4 }),
      measure() {},
    },
  ],
});
"#;

    fn comparison() -> ComparisonSummary {
        ComparisonSummary {
            comparison_id: "comparison-23".to_owned(),
            report_path: None,
            baseline_measurement_set: "measure-baseline".to_owned(),
            candidate_measurement_set: "measure-candidate".to_owned(),
            environment_fingerprint: Some("environment".to_owned()),
            policy: "strict_all".to_owned(),
            verdict: "positive".to_owned(),
            engines: Engine::ALL
                .into_iter()
                .enumerate()
                .map(|(index, engine)| EngineSummary {
                    engine,
                    verdict: "positive".to_owned(),
                    correctness: "pass".to_owned(),
                    anchor: None,
                    metrics: BTreeMap::from([
                        (
                            "workload.wall_ms".to_owned(),
                            metric(240.0 + index as f64, 186.4 + index as f64),
                        ),
                        (
                            "browser.cpu_profile.active_ms".to_owned(),
                            metric(220.0 + index as f64, 178.1 + index as f64),
                        ),
                        (
                            "browser.js_heap.live_bytes".to_owned(),
                            metric(80_000_000.0, 74_800_000.0 + index as f64),
                        ),
                    ]),
                })
                .collect(),
            warnings: Vec::new(),
        }
    }

    fn metric(baseline: f64, candidate: f64) -> MetricSummary {
        MetricSummary {
            improvement_pct: Some((baseline - candidate) / baseline * 100.0),
            ci_pct: Some([5.0, 8.0]),
            classification: "improved".to_owned(),
            guardrail_regressed: false,
            baseline_value: Some(baseline),
            candidate_value: Some(candidate),
            unsupported_reason: None,
        }
    }

    fn history() -> HistoryIndexEntry {
        HistoryIndexEntry {
            benchmark_id: "ts-demuxer".to_owned(),
            cycle_count: 23,
            accepted_count: 7,
            latest_recorded_at_unix_ms: crate::terminal_ui::now_unix_ms() - 6 * 24 * 60 * 60_000,
            latest_outcome: "positive".to_owned(),
            latest_message: Some("Skip the PES header re-scan".to_owned()),
            current_baseline_label: Some("b-07".to_owned()),
            latest_comparison: Some(comparison()),
            wall_history_ms: BTreeMap::from([
                (Engine::Chromium, vec![240.0, 221.0, 206.0, 198.0, 186.4]),
                (Engine::Firefox, vec![270.0, 258.0, 249.0, 241.7]),
                (Engine::Webkit, vec![260.0, 247.0, 233.2]),
            ]),
        }
    }

    fn test_app() -> (TempDir, App) {
        let temporary = tempdir().unwrap();
        let workspace_root = fs::canonicalize(temporary.path()).unwrap();
        let benchmark_root = workspace_root.join("benchmarks");
        fs::create_dir_all(benchmark_root.join("nested")).unwrap();
        fs::write(benchmark_root.join("demuxer.bench.ts"), BENCHMARK_SOURCE).unwrap();
        fs::write(
            benchmark_root.join("nested/playlist.bench.ts"),
            r#"export default defineBrowserBenchmark({ id: "playlist", cases: [] });"#,
        )
        .unwrap();
        fs::write(benchmark_root.join("ignored.ts"), BENCHMARK_SOURCE).unwrap();
        fs::write(benchmark_root.join("stream.bin"), [0_u8; 1_024]).unwrap();
        let history = HashMap::from([("ts-demuxer".to_owned(), history())]);
        let catalog = Catalog::scan(&workspace_root, Path::new("benchmarks"), &history).unwrap();
        let selected_path = catalog.modules.first().map(|module| module.path.clone());
        (
            temporary,
            App {
                workspace_root,
                history,
                catalog,
                selected_path,
                filter: String::new(),
                message: None,
                budget: RunBudget::from_str("5m").unwrap(),
                editor: None,
                notice: None,
                selection: None,
            },
        )
    }

    fn render_screen(app: &App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        let mut output = String::new();
        for row in 0..height {
            for column in 0..width {
                output.push_str(buffer[(column, row)].symbol());
            }
            output.push('\n');
        }
        output
    }

    #[test]
    fn static_preview_ignores_imports_and_reads_literal_benchmark_metadata() {
        let parsed = parse_benchmark_source(BENCHMARK_SOURCE);
        assert_eq!(parsed.id.as_deref(), Some("ts-demuxer"));
        assert_eq!(parsed.fixtures, ["./stream.bin"]);
        assert_eq!(parsed.cases.len(), 2);
        assert_eq!(parsed.cases[0].id, "ts-188");
        assert!(parsed.cases[0].setup);
        assert!(parsed.cases[0].exact);
        assert!(parsed.cases[1].setup);
        assert!(parsed.literal_definition);
    }

    #[test]
    fn catalog_recurses_for_benchmark_modules_and_joins_history_by_id() {
        let (_temporary, app) = test_app();
        assert_eq!(app.catalog.modules.len(), 2);
        assert_eq!(app.catalog.nested_count, 1);
        assert_eq!(app.catalog.known_case_count, 2);
        assert_eq!(app.catalog.modules[0].id, "ts-demuxer");
        assert_eq!(
            app.catalog.modules[0]
                .history
                .as_ref()
                .map(|history| history.cycle_count),
            Some(23)
        );
        assert_eq!(
            app.catalog.modules[1].relative_path,
            "benchmarks/nested/playlist.bench.ts"
        );
    }

    #[test]
    fn wide_screen_contains_the_reference_information_hierarchy_and_shortcuts() {
        let (_temporary, app) = test_app();
        let screen = render_screen(&app, 200, 56);
        for expected in [
            "bperf",
            "run",
            "budget",
            "chromium firefox webkit",
            "BENCHMARK MODULES",
            "ts-demuxer",
            "23 runs",
            "POSITIVE",
            "CASES",
            "ts-188",
            "FIXTURES",
            "LAST MEDIANS",
            "TREND",
            "Skip the PES header re-scan",
            "record a message and run ts-demuxer",
            "select / start",
            "message",
            "directory",
            "quit",
        ] {
            assert!(
                screen.contains(expected),
                "run picker omitted {expected:?}:\n{screen}"
            );
        }
    }

    #[test]
    fn enter_collects_a_message_and_returns_the_selected_benchmark() {
        let (_temporary, mut app) = test_app();
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .unwrap(),
            ControlFlow::Continue
        );
        assert_eq!(
            app.editor.as_ref().map(|editor| editor.kind),
            Some(EditorKind::Message)
        );
        for character in "Avoid temporary strings".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .unwrap();
        }
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .unwrap(),
            ControlFlow::Quit
        );
        let selection = app.selection.unwrap();
        assert!(selection.benchmark.ends_with("benchmarks/demuxer.bench.ts"));
        assert_eq!(
            selection.message.as_deref(),
            Some("Avoid temporary strings")
        );
        assert_eq!(selection.budget, RunBudget::from_str("5m").unwrap());
    }

    #[test]
    fn filter_and_budget_shortcuts_update_picker_state() {
        let (_temporary, mut app) = test_app();
        app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))
            .unwrap();
        for character in "playlist".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .unwrap();
        }
        assert_eq!(
            app.selected_module().map(|module| module.id.as_str()),
            Some("playlist")
        );
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE))
            .unwrap();
        for _ in 0.."5m".chars().count() {
            app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE))
                .unwrap();
        }
        for character in "30s".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .unwrap();
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.budget, RunBudget::from_str("30s").unwrap());
    }
}
