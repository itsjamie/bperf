use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use bperf_browser::lab::Engine;
use bperf_decision::{
    comparison::{EngineSummary, MetricSummary},
    lineage::{
        HistoryArtifact, HistoryArtifactKind, HistoryCycle, HistoryCycleSummary, HistoryIndex,
        HistoryOverview, HistoryReader, promotable_outcome,
    },
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, List, ListItem, ListState, Paragraph, StatefulWidget, Wrap},
};
use time::{OffsetDateTime, UtcOffset, macros::format_description};

use crate::terminal_ui::{
    self, AMBER, BG, BLUE, CYAN, ControlFlow, FAINT, GREEN, MUTED, RED, SELECTED, SURFACE, TEXT,
    centered_rect, chrome_block, clip, fit_sides,
};

const MIN_WIDTH: u16 = 92;
const MIN_HEIGHT: u16 = 25;
const WIDE_LAYOUT: u16 = 160;
const HORIZONTAL_LAYOUT: u16 = 140;
const FULL_FILTERS: u16 = 176;

/// Runs the interactive history view and restores the terminal on every exit
/// path. The lineage directory is read-only; artifact opening delegates one
/// validated retained path to the platform's file handler.
pub fn run(lineage_root: PathBuf) -> Result<()> {
    let mut app = App::load(lineage_root)?;
    terminal_ui::run("history", &mut app, render, App::handle_key)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GraphScope {
    AllCycles,
    VisibleCycles,
}

impl GraphScope {
    fn toggle(&mut self) {
        *self = match self {
            Self::AllCycles => Self::VisibleCycles,
            Self::VisibleCycles => Self::AllCycles,
        };
    }

    const fn label(self) -> &'static str {
        match self {
            Self::AllCycles => "all cycles",
            Self::VisibleCycles => "visible cycles",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DateRange {
    Last24Hours,
    Last7Days,
    Last30Days,
    All,
}

impl DateRange {
    const ALL: [Self; 4] = [
        Self::Last24Hours,
        Self::Last7Days,
        Self::Last30Days,
        Self::All,
    ];

    const fn duration_ms(self) -> Option<u64> {
        match self {
            Self::Last24Hours => Some(24 * 60 * 60 * 1_000),
            Self::Last7Days => Some(7 * 24 * 60 * 60 * 1_000),
            Self::Last30Days => Some(30 * 24 * 60 * 60 * 1_000),
            Self::All => None,
        }
    }

    fn contains(self, timestamp: u64, now: u64) -> bool {
        self.duration_ms()
            .is_none_or(|duration| timestamp >= now.saturating_sub(duration))
    }

    const fn picker_label(self) -> &'static str {
        match self {
            Self::Last24Hours => "last 24 hours",
            Self::Last7Days => "last 7 days",
            Self::Last30Days => "last 30 days",
            Self::All => "all dates",
        }
    }

    fn filter_label(self, now: u64) -> String {
        let Some(duration) = self.duration_ms() else {
            return "all dates".to_owned();
        };
        let start = now.saturating_sub(duration);
        format!("{} - {}", short_date(start), short_date(now))
    }

    const fn compact_filter_label(self) -> &'static str {
        match self {
            Self::Last24Hours => "24h",
            Self::Last7Days => "7d",
            Self::Last30Days => "30d",
            Self::All => "all",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct VerdictFilters {
    positive: bool,
    equivalent: bool,
    inconclusive: bool,
    negative: bool,
}

impl Default for VerdictFilters {
    fn default() -> Self {
        Self {
            positive: true,
            equivalent: true,
            inconclusive: true,
            negative: true,
        }
    }
}

impl VerdictFilters {
    fn allows(self, outcome: &str) -> bool {
        match outcome {
            "positive" => self.positive,
            "equivalent" => self.equivalent,
            "inconclusive" => self.inconclusive,
            "negative" => self.negative,
            "measured" => true,
            _ => false,
        }
    }

    fn toggle(&mut self, outcome: char) {
        match outcome {
            'p' => self.positive = !self.positive,
            'e' => self.equivalent = !self.equivalent,
            'i' => self.inconclusive = !self.inconclusive,
            'n' => self.negative = !self.negative,
            _ => {}
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PickerKind {
    Benchmark,
    Lineage,
    Date,
    Artifact,
}

#[derive(Clone, Debug)]
struct Picker {
    kind: PickerKind,
    selected: usize,
}

struct App {
    history_reader: Option<HistoryReader>,
    display_root: PathBuf,
    index: HistoryIndex,
    history: HistoryOverview,
    cycle_details: HashMap<String, HistoryCycle>,
    selected_cycle_id: Option<String>,
    accepted_only: bool,
    verdicts: VerdictFilters,
    graph_scope: GraphScope,
    lineage_filter: Option<String>,
    date_range: DateRange,
    picker: Option<Picker>,
    notice: Option<String>,
}

impl App {
    fn load(lineage_root: PathBuf) -> Result<Self> {
        let display_root = artifact_display_root(&lineage_root);
        let history_reader = HistoryReader::open(&lineage_root)?;
        let index = history_reader.index()?;
        let history = history_reader.overview(Some(&index.latest_benchmark_id))?;
        let mut app = Self {
            history_reader: Some(history_reader),
            display_root,
            selected_cycle_id: history.cycles.first().map(|cycle| cycle.cycle_id.clone()),
            lineage_filter: history.current_baseline_label.clone(),
            index,
            history,
            cycle_details: HashMap::new(),
            accepted_only: false,
            verdicts: VerdictFilters::default(),
            graph_scope: GraphScope::AllCycles,
            date_range: DateRange::Last30Days,
            picker: None,
            notice: None,
        };
        if app.visible_indices().is_empty() {
            app.date_range = DateRange::All;
        }
        app.ensure_selection();
        app.load_selected_cycle()?;
        Ok(app)
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<ControlFlow> {
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'C'))
        {
            return Ok(ControlFlow::Quit);
        }
        if self.picker.is_some() {
            return self.handle_picker_key(key);
        }
        match key.code {
            KeyCode::Char('q' | 'Q') => return Ok(ControlFlow::Quit),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Home => self.select_edge(false),
            KeyCode::End => self.select_edge(true),
            KeyCode::Char('a' | 'A') => {
                self.accepted_only = !self.accepted_only;
                self.ensure_selection();
            }
            KeyCode::Char(value @ ('p' | 'e' | 'i' | 'n')) => {
                self.verdicts.toggle(value);
                self.ensure_selection();
            }
            KeyCode::Char(value @ ('P' | 'E' | 'I' | 'N')) => {
                self.verdicts.toggle(value.to_ascii_lowercase());
                self.ensure_selection();
            }
            KeyCode::Char('g' | 'G') => {
                self.graph_scope.toggle();
                self.notice = None;
            }
            KeyCode::Char('b' | 'B') => self.open_picker(PickerKind::Benchmark),
            KeyCode::Char('l' | 'L') => self.open_picker(PickerKind::Lineage),
            KeyCode::Char('d' | 'D') => self.open_picker(PickerKind::Date),
            KeyCode::Char('o' | 'O') => {
                if self
                    .selected_cycle()
                    .is_some_and(|cycle| !cycle.artifacts.is_empty())
                {
                    self.open_picker(PickerKind::Artifact);
                } else {
                    self.notice = Some("selected cycle has no retained artifact".to_owned());
                }
            }
            _ => {}
        }
        self.load_selected_cycle()?;
        Ok(ControlFlow::Continue)
    }

    fn handle_picker_key(&mut self, key: KeyEvent) -> Result<ControlFlow> {
        match key.code {
            KeyCode::Char('q' | 'Q') => return Ok(ControlFlow::Quit),
            KeyCode::Esc => self.picker = None,
            KeyCode::Up | KeyCode::Char('k') => self.move_picker(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_picker(1),
            KeyCode::Home => {
                if let Some(picker) = &mut self.picker {
                    picker.selected = 0;
                }
            }
            KeyCode::End => {
                let count = self.picker_item_count();
                if let Some(picker) = &mut self.picker {
                    picker.selected = count.saturating_sub(1);
                }
            }
            KeyCode::Enter => self.apply_picker()?,
            _ => {}
        }
        Ok(ControlFlow::Continue)
    }

    fn open_picker(&mut self, kind: PickerKind) {
        let selected = match kind {
            PickerKind::Benchmark => self
                .index
                .benchmarks
                .iter()
                .position(|entry| entry.benchmark_id == self.history.benchmark_id)
                .unwrap_or_default(),
            PickerKind::Lineage => self
                .lineage_filter
                .as_ref()
                .and_then(|label| {
                    self.history
                        .baselines
                        .iter()
                        .position(|baseline| &baseline.label == label)
                        .map(|index| index + 1)
                })
                .unwrap_or_default(),
            PickerKind::Date => DateRange::ALL
                .iter()
                .position(|range| *range == self.date_range)
                .unwrap_or_default(),
            PickerKind::Artifact => 0,
        };
        self.picker = Some(Picker { kind, selected });
        self.notice = None;
    }

    fn picker_item_count(&self) -> usize {
        let Some(picker) = &self.picker else {
            return 0;
        };
        match picker.kind {
            PickerKind::Benchmark => self.index.benchmarks.len(),
            PickerKind::Lineage => self.history.baselines.len() + 1,
            PickerKind::Date => DateRange::ALL.len(),
            PickerKind::Artifact => self
                .selected_cycle()
                .map_or(0, |cycle| cycle.artifacts.len()),
        }
    }

    fn picker_items(&self) -> Vec<String> {
        let Some(picker) = &self.picker else {
            return Vec::new();
        };
        match picker.kind {
            PickerKind::Benchmark => self
                .index
                .benchmarks
                .iter()
                .map(|entry| {
                    format!(
                        "{:<30} {:>3} cycles  {}",
                        entry.benchmark_id,
                        entry.cycle_count,
                        relative_age(entry.latest_recorded_at_unix_ms)
                    )
                })
                .collect(),
            PickerKind::Lineage => std::iter::once("all lineages".to_owned())
                .chain(self.history.baselines.iter().map(|baseline| {
                    let current = if baseline.current { " current" } else { "" };
                    format!(
                        "{:<8} {}{}",
                        baseline.label,
                        short_selector(&baseline.cycle_id),
                        current
                    )
                }))
                .collect(),
            PickerKind::Date => DateRange::ALL
                .into_iter()
                .map(|range| range.picker_label().to_owned())
                .collect(),
            PickerKind::Artifact => self
                .selected_cycle()
                .into_iter()
                .flat_map(|cycle| cycle.artifacts.iter())
                .map(|artifact| artifact_picker_label(artifact, &self.display_root))
                .collect(),
        }
    }

    fn move_picker(&mut self, delta: isize) {
        let count = self.picker_item_count();
        if count == 0 {
            return;
        }
        if let Some(picker) = &mut self.picker {
            picker.selected = offset_index(picker.selected, delta, count);
        }
    }

    fn apply_picker(&mut self) -> Result<()> {
        let Some(picker) = self.picker.clone() else {
            return Ok(());
        };
        match picker.kind {
            PickerKind::Benchmark => {
                let benchmark_id = self
                    .index
                    .benchmarks
                    .get(picker.selected)
                    .map(|entry| entry.benchmark_id.clone())
                    .context("benchmark picker has no selection")?;
                self.history = self
                    .history_reader
                    .as_ref()
                    .context("history reader is unavailable")?
                    .overview(Some(&benchmark_id))?;
                self.cycle_details.clear();
                self.lineage_filter = self.history.current_baseline_label.clone();
                self.date_range = DateRange::Last30Days;
                self.selected_cycle_id = self
                    .history
                    .cycles
                    .first()
                    .map(|cycle| cycle.cycle_id.clone());
                if self.visible_indices().is_empty() {
                    self.date_range = DateRange::All;
                }
                self.ensure_selection();
            }
            PickerKind::Lineage => {
                self.lineage_filter = if picker.selected == 0 {
                    None
                } else {
                    self.history
                        .baselines
                        .get(picker.selected - 1)
                        .map(|baseline| baseline.label.clone())
                };
                self.ensure_selection();
            }
            PickerKind::Date => {
                self.date_range = *DateRange::ALL
                    .get(picker.selected)
                    .context("date picker has no selection")?;
                self.ensure_selection();
            }
            PickerKind::Artifact => {
                let artifact = self
                    .selected_cycle()
                    .and_then(|cycle| cycle.artifacts.get(picker.selected))
                    .context("artifact picker has no selection")?;
                self.notice = Some(match open_artifact(&artifact.path) {
                    Ok(()) => {
                        format!(
                            "opened {}",
                            display_path(&artifact.path, &self.display_root)
                        )
                    }
                    Err(error) => format!("could not open artifact: {error:#}"),
                });
            }
        }
        self.picker = None;
        self.load_selected_cycle()?;
        Ok(())
    }

    fn visible_indices(&self) -> Vec<usize> {
        let now = now_unix_ms();
        let lineage_ids = self.lineage_cycle_ids();
        self.history
            .cycles
            .iter()
            .enumerate()
            .filter(|(_, cycle)| {
                (!self.accepted_only || cycle.accepted)
                    && self.verdicts.allows(&cycle.outcome)
                    && self.date_range.contains(cycle.recorded_at_unix_ms, now)
                    && lineage_ids.as_ref().is_none_or(|ids| {
                        ids.contains(&cycle.cycle_id)
                            || cycle.baseline_cycle_id.as_ref().is_some_and(|parent| {
                                self.focused_baseline_cycle_id() == Some(parent.as_str())
                            })
                    })
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn lineage_cycle_ids(&self) -> Option<HashSet<String>> {
        let mut current = self.focused_baseline_cycle_id()?.to_owned();
        let mut ids = HashSet::new();
        loop {
            if !ids.insert(current.clone()) {
                break;
            }
            let Some(parent) = self
                .history
                .cycles
                .iter()
                .find(|cycle| cycle.cycle_id == current)
                .and_then(|cycle| cycle.baseline_cycle_id.clone())
            else {
                break;
            };
            current = parent;
        }
        Some(ids)
    }

    fn focused_baseline_cycle_id(&self) -> Option<&str> {
        let label = self.lineage_filter.as_deref()?;
        self.history
            .baselines
            .iter()
            .find(|baseline| baseline.label == label)
            .map(|baseline| baseline.cycle_id.as_str())
    }

    fn selected_cycle(&self) -> Option<&HistoryCycle> {
        let selected = self.selected_cycle_id.as_deref()?;
        self.cycle_details.get(selected)
    }

    fn load_selected_cycle(&mut self) -> Result<()> {
        let Some(cycle_id) = self.selected_cycle_id.clone() else {
            return Ok(());
        };
        if self.cycle_details.contains_key(&cycle_id) {
            return Ok(());
        }
        let summary = self
            .history
            .cycles
            .iter()
            .find(|cycle| cycle.cycle_id == cycle_id)
            .context("selected history cycle is missing from its overview")?;
        let cycle = self
            .history_reader
            .as_ref()
            .context("history reader is unavailable")?
            .cycle(&self.history.benchmark_id, &summary.cycle_id)?;
        self.cycle_details.insert(cycle_id, cycle);
        Ok(())
    }

    fn move_selection(&mut self, delta: isize) {
        let visible = self.visible_indices();
        if visible.is_empty() {
            self.selected_cycle_id = None;
            return;
        }
        let position = self
            .selected_cycle_id
            .as_ref()
            .and_then(|selected| {
                visible
                    .iter()
                    .position(|index| self.history.cycles[*index].cycle_id == *selected)
            })
            .unwrap_or_default();
        let next = offset_index(position, delta, visible.len());
        self.selected_cycle_id = Some(self.history.cycles[visible[next]].cycle_id.clone());
        self.notice = None;
    }

    fn select_edge(&mut self, end: bool) {
        let visible = self.visible_indices();
        let selected = if end { visible.last() } else { visible.first() };
        self.selected_cycle_id = selected.map(|index| self.history.cycles[*index].cycle_id.clone());
    }

    fn ensure_selection(&mut self) {
        let visible = self.visible_indices();
        let selection_is_visible = self.selected_cycle_id.as_ref().is_some_and(|selected| {
            visible
                .iter()
                .any(|index| self.history.cycles[*index].cycle_id == *selected)
        });
        if !selection_is_visible {
            self.selected_cycle_id = visible
                .first()
                .map(|index| self.history.cycles[*index].cycle_id.clone());
        }
        self.notice = None;
    }

    fn graph_scale(&self, visible: &[usize]) -> f64 {
        let cycles: Vec<&HistoryCycleSummary> = match self.graph_scope {
            GraphScope::AllCycles => self.history.cycles.iter().collect(),
            GraphScope::VisibleCycles => visible
                .iter()
                .map(|index| &self.history.cycles[*index])
                .collect(),
        };
        cycles
            .into_iter()
            .flat_map(cycle_wall_effects)
            .map(f64::abs)
            .fold(1.0, f64::max)
    }
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

fn render(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    frame.render_widget(Block::new().style(Style::default().bg(BG)), area);
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        render_too_small(frame, area);
        return;
    }

    let sections = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Min(8),
        Constraint::Length(3),
    ])
    .split(area);
    render_header(frame, sections[0], app);
    render_filters(frame, sections[1], app);
    render_content(frame, sections[2], app);
    render_footer(frame, sections[3], app);
    if app.picker.is_some() {
        render_picker(frame, area, app);
    }
}

fn render_too_small(frame: &mut Frame<'_>, area: Rect) {
    let block = chrome_block();
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let message = Paragraph::new(vec![
        Line::from(Span::styled(
            "bperf history",
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
    .alignment(Alignment::Center)
    .wrap(Wrap { trim: true });
    frame.render_widget(message, inner);
}

fn render_header(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let block = chrome_block().style(Style::default().bg(SURFACE));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let columns = Layout::horizontal([Constraint::Min(20), Constraint::Length(18)]).split(inner);
    let selected = app.selected_cycle();
    let environment = selected.map(|cycle| &cycle.environment);
    let baseline = app
        .history
        .current_baseline_label
        .as_deref()
        .unwrap_or("none");
    let mut spans = vec![
        Span::styled(
            " bperf ",
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
        ),
        Span::styled("history  ", Style::default().fg(MUTED)),
        Span::styled(
            app.history.benchmark_id.clone(),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  │  baseline  ", Style::default().fg(FAINT)),
        Span::styled(baseline.to_owned(), Style::default().fg(TEXT)),
    ];
    if let Some(environment) = environment {
        spans.extend([
            Span::styled("  │  env  ", Style::default().fg(FAINT)),
            Span::styled(
                short_environment(&environment.fingerprint),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                format!(
                    "  {} {}",
                    clip(&host_platform(environment), 18),
                    environment.arch
                ),
                Style::default().fg(MUTED),
            ),
        ]);
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans))
            .style(Style::default().bg(SURFACE))
            .alignment(Alignment::Left),
        columns[0],
    );
    let visible = app.visible_indices().len();
    frame.render_widget(
        Paragraph::new(format!("{visible}/{} cycles ", app.history.cycles.len()))
            .style(Style::default().fg(MUTED).bg(SURFACE))
            .alignment(Alignment::Right),
        columns[1],
    );
}

fn render_filters(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let block = chrome_block();
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let columns = Layout::horizontal([Constraint::Min(20), Constraint::Length(23)]).split(inner);
    let compact = area.width < 135;
    let condensed = area.width < FULL_FILTERS;
    let mut spans = vec![Span::styled(
        if condensed { " FILTER " } else { " FILTER  " },
        Style::default().fg(MUTED),
    )];
    spans.push(filter_chip(
        if condensed {
            "a accepted"
        } else {
            "a accepted only"
        },
        app.accepted_only,
        TEXT,
    ));
    spans.push(Span::raw(" "));
    spans.push(filter_chip("p positive", app.verdicts.positive, GREEN));
    spans.push(Span::raw(" "));
    spans.push(filter_chip(
        if condensed { "e equal" } else { "e equivalent" },
        app.verdicts.equivalent,
        BLUE,
    ));
    spans.push(Span::raw(" "));
    spans.push(filter_chip(
        if condensed {
            "i unsure"
        } else {
            "i inconclusive"
        },
        app.verdicts.inconclusive,
        AMBER,
    ));
    spans.push(Span::raw(" "));
    spans.push(filter_chip("n negative", app.verdicts.negative, RED));
    if !compact {
        let lineage = app.lineage_filter.as_deref().unwrap_or("all");
        let (lineage, benchmark, date) = if condensed {
            (
                format!("l {lineage}"),
                format!("b {}", clip(&app.history.benchmark_id, 12)),
                format!("d {}", app.date_range.compact_filter_label()),
            )
        } else {
            (
                format!("l lineage {lineage}"),
                format!("b {}", clip(&app.history.benchmark_id, 20)),
                format!("d {}", app.date_range.filter_label(now_unix_ms())),
            )
        };
        spans.extend([
            Span::raw("  "),
            filter_chip(&lineage, app.lineage_filter.is_some(), MUTED),
            Span::raw(" "),
            filter_chip(&benchmark, true, TEXT),
            Span::raw(" "),
            filter_chip(&date, app.date_range != DateRange::All, TEXT),
        ]);
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(BG)),
        columns[0],
    );
    frame.render_widget(
        Paragraph::new(format!("graphs: {} ", app.graph_scope.label()))
            .style(Style::default().fg(MUTED).bg(BG))
            .alignment(Alignment::Right),
        columns[1],
    );
}

fn filter_chip(label: &str, active: bool, color: Color) -> Span<'static> {
    let style = if active {
        Style::default()
            .fg(color)
            .bg(SELECTED)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(FAINT)
    };
    Span::styled(format!(" {label} "), style)
}

fn render_content(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if area.width >= HORIZONTAL_LAYOUT {
        let columns = Layout::horizontal([
            Constraint::Percentage(if area.width >= WIDE_LAYOUT { 39 } else { 38 }),
            Constraint::Percentage(if area.width >= WIDE_LAYOUT { 61 } else { 62 }),
        ])
        .split(area);
        render_cycles(frame, columns[0], app);
        if area.width >= WIDE_LAYOUT && area.height >= 29 {
            render_wide_detail(frame, columns[1], app);
        } else {
            render_compact_detail(frame, columns[1], app);
        }
    } else {
        let rows =
            Layout::vertical([Constraint::Percentage(44), Constraint::Percentage(56)]).split(area);
        render_cycles(frame, rows[0], app);
        render_compact_detail(frame, rows[1], app);
    }
}

fn render_cycles(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let visible = app.visible_indices();
    let hidden = app.history.cycles.len().saturating_sub(visible.len());
    let title = cycle_panel_title(area.width);
    let block = chrome_block().title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let rows = if hidden > 0 && inner.height > 2 {
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner)
    } else {
        Layout::vertical([Constraint::Min(1), Constraint::Length(0)]).split(inner)
    };
    let scale = app.graph_scale(&visible);
    let items = visible
        .iter()
        .map(|index| cycle_item(&app.history.cycles[*index], rows[0].width, scale))
        .collect::<Vec<_>>();
    let selected = app.selected_cycle_id.as_ref().and_then(|selected| {
        visible
            .iter()
            .position(|index| app.history.cycles[*index].cycle_id == *selected)
    });
    let mut state = ListState::default();
    state.select(selected);
    let list = List::new(items)
        .style(Style::default().fg(TEXT).bg(BG))
        .highlight_style(Style::default().fg(TEXT).add_modifier(Modifier::BOLD))
        .highlight_symbol("› ");
    StatefulWidget::render(list, rows[0], frame.buffer_mut(), &mut state);
    if hidden > 0 {
        frame.render_widget(
            Paragraph::new(format!(" … {hidden} cycles hidden by filters"))
                .style(Style::default().fg(FAINT).bg(BG)),
            rows[1],
        );
    }
}

fn cycle_panel_title(width: u16) -> Line<'static> {
    let left_label = " CYCLES";
    let left_suffix = "  newest first";
    let right = "P/E/I/N   chr ff wk ";
    let occupied = left_label.chars().count() + left_suffix.chars().count() + right.chars().count();
    let gap = usize::from(width.saturating_sub(2)).saturating_sub(occupied);
    let mut spans = vec![
        Span::styled(left_label, Style::default().fg(AMBER)),
        Span::styled(left_suffix, Style::default().fg(MUTED)),
    ];
    if width >= 54 {
        spans.push(Span::raw(" ".repeat(gap)));
        spans.push(Span::styled(right, Style::default().fg(MUTED)));
    } else {
        spans.push(Span::raw(" "));
    }
    Line::from(spans)
}

fn cycle_item(cycle: &HistoryCycleSummary, width: u16, graph_scale: f64) -> ListItem<'static> {
    let available = usize::from(width.saturating_sub(3));
    let status = outcome_mark(&cycle.outcome);
    let lineage = cycle
        .baseline_label
        .as_ref()
        .map(|label| format!(" ▲ {label}"))
        .or_else(|| {
            cycle
                .accepted_label
                .as_ref()
                .map(|label| format!(" ◆ {label}"))
        })
        .unwrap_or_default();
    let engines = engine_marks(cycle);
    let effect = headline_effect(cycle)
        .map(|value| format!("{value:+.1}%"))
        .unwrap_or_else(|| "baseline".to_owned());
    let age = relative_age(cycle.recorded_at_unix_ms);
    let right = format!("{engines}  {effect}  {age}");
    let left = format!("{}{}", bare_cycle_selector(&cycle.selector), lineage);
    let heading = fit_sides(&left, &right, available.saturating_sub(2));
    let graph = mini_graph(cycle, graph_scale);
    let graph_width = graph.chars().count();
    let message_width = available.saturating_sub(graph_width + 1);
    let message = clip(&format!("  {}", cycle.message), message_width);
    ListItem::new(vec![
        Line::from(vec![
            Span::styled(
                format!("{status} "),
                Style::default()
                    .fg(outcome_color(&cycle.outcome))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(heading),
        ]),
        Line::from(vec![
            Span::styled(
                format!("{message:<message_width$} "),
                Style::default().fg(MUTED),
            ),
            Span::styled(graph, Style::default().fg(outcome_color(&cycle.outcome))),
        ]),
        Line::from(""),
    ])
}

fn render_wide_detail(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(cycle) = app.selected_cycle() else {
        render_no_cycles(frame, area);
        return;
    };
    let rows = Layout::vertical([
        Constraint::Length(5),
        Constraint::Length(17),
        Constraint::Min(7),
    ])
    .split(area);
    render_cycle_summary(frame, rows[0], cycle);
    render_engines(frame, rows[1], cycle);
    let bottom =
        Layout::horizontal([Constraint::Percentage(67), Constraint::Percentage(33)]).split(rows[2]);
    render_artifacts(frame, bottom[0], cycle, &app.display_root);
    render_promotion(frame, bottom[1], cycle);
}

fn render_compact_detail(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(cycle) = app.selected_cycle() else {
        render_no_cycles(frame, area);
        return;
    };
    let block = chrome_block().title(Span::styled(
        format!(
            " {}  {} ",
            cycle.outcome.to_ascii_uppercase(),
            cycle.selector
        ),
        Style::default()
            .fg(outcome_color(&cycle.outcome))
            .add_modifier(Modifier::BOLD),
    ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut lines = vec![
        Line::from(Span::styled(
            clip(&cycle.message, usize::from(inner.width)),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!(
                "{}  {} files  +{} -{}  {}",
                format_timestamp(cycle.recorded_at_unix_ms),
                cycle.change.files_changed,
                cycle.change.additions,
                cycle.change.deletions,
                cycle
                    .comparison
                    .as_ref()
                    .map(|comparison| comparison.policy.as_str())
                    .unwrap_or("no comparison")
            ),
            Style::default().fg(MUTED),
        )),
        Line::from(""),
    ];
    for engine in Engine::ALL {
        lines.push(compact_engine_line(cycle, engine));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("ARTIFACTS  ", Style::default().fg(AMBER)),
        Span::styled(
            format!("{} retained · o to open", cycle.artifacts.len()),
            Style::default().fg(MUTED),
        ),
    ]));
    for artifact in artifact_overview(cycle).into_iter().take(3) {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{:<12}", artifact_kind_label(artifact.kind)),
                Style::default().fg(artifact_color(artifact.kind)),
            ),
            Span::raw("  "),
            Span::styled(
                clip(
                    &display_path(&artifact.path, &app.display_root),
                    usize::from(inner.width.saturating_sub(14)),
                ),
                Style::default().fg(TEXT),
            ),
        ]));
    }
    lines.push(Line::from(""));
    lines.extend(promotion_lines(cycle));
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(BG))
            .wrap(Wrap { trim: false }),
        inner,
    );
}

fn render_no_cycles(frame: &mut Frame<'_>, area: Rect) {
    let block = chrome_block().title(" HISTORY ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                "No cycles match the active filters.",
                Style::default().fg(TEXT),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "Use a, p/e/i/n, l, or d to broaden the view.",
                Style::default().fg(MUTED),
            )),
        ])
        .alignment(Alignment::Center),
        inner,
    );
}

fn render_cycle_summary(frame: &mut Frame<'_>, area: Rect, cycle: &HistoryCycle) {
    let block = chrome_block();
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let baseline = cycle
        .baseline_label
        .as_ref()
        .map(|label| format!(" vs {label}"))
        .unwrap_or_default();
    let heading = Line::from(vec![
        Span::styled(
            format!(" {} ", cycle.outcome.to_ascii_uppercase()),
            Style::default()
                .fg(outcome_color(&cycle.outcome))
                .bg(SELECTED)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {} ", cycle.selector),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(baseline, Style::default().fg(MUTED)),
        Span::styled(
            format!(
                "  │  {}  ({})",
                format_timestamp(cycle.recorded_at_unix_ms),
                relative_age(cycle.recorded_at_unix_ms)
            ),
            Style::default().fg(MUTED),
        ),
    ]);
    let policy = cycle
        .comparison
        .as_ref()
        .map(|comparison| comparison.policy.as_str())
        .unwrap_or("baseline-free");
    let detail = format!(
        "  msg  {}",
        clip(&cycle.message, usize::from(inner.width.saturating_sub(7)))
    );
    let change = format!(
        "  change  {} files modified  ·  +{} -{}{}  ·  policy {policy}",
        cycle.change.files_changed,
        cycle.change.additions,
        cycle.change.deletions,
        if cycle.change.binary_files > 0 {
            format!("  ·  {} binary", cycle.change.binary_files)
        } else {
            String::new()
        }
    );
    frame.render_widget(
        Paragraph::new(vec![
            heading,
            Line::from(Span::styled(detail, Style::default().fg(TEXT))),
            Line::from(Span::styled(change, Style::default().fg(MUTED))),
        ])
        .style(Style::default().bg(BG)),
        inner,
    );
}

fn render_engines(frame: &mut Frame<'_>, area: Rect, cycle: &HistoryCycle) {
    let columns = Layout::horizontal([
        Constraint::Percentage(33),
        Constraint::Percentage(34),
        Constraint::Percentage(33),
    ])
    .split(area);
    for (engine, column) in Engine::ALL.into_iter().zip(columns.iter().copied()) {
        render_engine(frame, column, cycle, engine);
    }
}

fn render_engine(frame: &mut Frame<'_>, area: Rect, cycle: &HistoryCycle, engine: Engine) {
    let summary = cycle
        .comparison
        .as_ref()
        .and_then(|comparison| comparison.engines.iter().find(|item| item.engine == engine));
    let version = cycle
        .environment
        .browser_versions
        .get(&engine)
        .map(String::as_str)
        .unwrap_or("unknown");
    let verdict = summary.map_or("measured", |summary| summary.verdict.as_str());
    let block = chrome_block().title(Line::from(vec![
        Span::styled(
            format!(" {} ", engine),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(clip(version, 16), Style::default().fg(MUTED)),
        Span::styled(
            format!("  {} ", verdict),
            Style::default()
                .fg(outcome_color(verdict))
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let Some(summary) = summary else {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled("correct  ", Style::default().fg(MUTED)),
                    Span::styled(
                        format!(
                            "pass {}/{} cases",
                            cycle.case_ids.len(),
                            cycle.case_ids.len()
                        ),
                        Style::default().fg(GREEN),
                    ),
                ]),
                Line::from(""),
                Line::from(Span::styled(
                    "No promoted baseline",
                    Style::default().fg(MUTED),
                )),
                Line::from(Span::styled(
                    "This measurement establishes evidence.",
                    Style::default().fg(FAINT),
                )),
            ]),
            inner,
        );
        return;
    };

    let mut lines = Vec::new();
    let correctness = if summary.correctness == "pass" {
        format!(
            "pass {}/{} cases",
            cycle.case_ids.len(),
            cycle.case_ids.len()
        )
    } else {
        format!(
            "{} · {} cases evaluated",
            summary.correctness,
            cycle.case_ids.len()
        )
    };
    lines.push(Line::from(vec![
        Span::styled("correct  ", Style::default().fg(MUTED)),
        Span::styled(
            correctness,
            Style::default().fg(if summary.correctness == "pass" {
                GREEN
            } else {
                RED
            }),
        ),
    ]));
    if let Some(anchor) = &summary.anchor {
        let drift = anchor
            .drift_pct
            .map(|value| format!(" drift {value:+.1}%"))
            .unwrap_or_default();
        let ci = anchor
            .ci_pct
            .map(|[low, high]| format!(" [{low:+.1}, {high:+.1}]"))
            .unwrap_or_default();
        lines.push(Line::from(Span::styled(
            clip(
                &format!("anchor   {}{drift}{ci}", anchor.status),
                usize::from(inner.width),
            ),
            Style::default().fg(if anchor.status == "stable" {
                GREEN
            } else {
                AMBER
            }),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "anchor   unreported",
            Style::default().fg(FAINT),
        )));
    }

    let metric_width = usize::from(inner.width.saturating_sub(2));
    let scale = engine_metric_scale(summary);
    for (key, label) in primary_metrics() {
        lines.push(Line::from(""));
        if let Some(metric) = summary.metrics.get(key) {
            lines.extend(metric_lines(label, metric, metric_width, scale));
        } else {
            lines.push(Line::from(vec![
                Span::styled(format!("{label:<6}"), Style::default().fg(TEXT)),
                Span::styled("unreported", Style::default().fg(FAINT)),
            ]));
        }
    }
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(BG))
            .wrap(Wrap { trim: false }),
        inner,
    );
}

fn compact_engine_line(cycle: &HistoryCycle, engine: Engine) -> Line<'static> {
    let summary = cycle
        .comparison
        .as_ref()
        .and_then(|comparison| comparison.engines.iter().find(|item| item.engine == engine));
    let Some(summary) = summary else {
        return Line::from(vec![
            Span::styled(format!("{engine:<8}"), Style::default().fg(TEXT)),
            Span::raw("  "),
            Span::styled(
                "measured · no promoted baseline",
                Style::default().fg(MUTED),
            ),
        ]);
    };
    let anchor = summary
        .anchor
        .as_ref()
        .map(|anchor| anchor.status.as_str())
        .unwrap_or("unreported");
    let metric = |key| {
        summary
            .metrics
            .get(key)
            .and_then(|metric| metric.improvement_pct)
            .map_or_else(|| " n/a".to_owned(), |value| format!("{value:+.1}%"))
    };
    Line::from(vec![
        Span::styled(
            format!("{engine:<8}"),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("{:<12}", summary.verdict),
            Style::default().fg(outcome_color(&summary.verdict)),
        ),
        Span::raw("  "),
        Span::styled(
            format!(
                "anchor {anchor:<11}  wall {}  cpu {}  heap {}",
                metric("workload.wall_ms"),
                metric("browser.cpu_profile.active_ms"),
                metric("browser.js_heap.live_bytes")
            ),
            Style::default().fg(MUTED),
        ),
    ])
}

fn metric_lines(
    label: &str,
    metric: &MetricSummary,
    width: usize,
    scale: f64,
) -> Vec<Line<'static>> {
    let effect = metric
        .improvement_pct
        .map_or_else(|| "n/a".to_owned(), |value| format!("{value:+.1}%"));
    let values = metric
        .baseline_value
        .zip(metric.candidate_value)
        .map(|(baseline, candidate)| format_metric_values(label, baseline, candidate))
        .unwrap_or_else(|| "values unavailable".to_owned());
    let color = classification_color(&metric.classification);
    let heading_left = format!("{label:<6}{effect}");
    let heading = fit_sides(&heading_left, &values, width);
    let interval = metric
        .ci_pct
        .map(|[low, high]| format!("ci95 [{low:+.1}, {high:+.1}]"))
        .unwrap_or_else(|| "ci95 unavailable".to_owned());
    let footer = fit_sides(&interval, &metric.classification, width);
    vec![
        Line::from(Span::styled(heading, Style::default().fg(color))),
        Line::from(Span::styled(
            metric_bar(metric, width, scale),
            Style::default().fg(color),
        )),
        Line::from(Span::styled(footer, Style::default().fg(MUTED))),
    ]
}

fn render_artifacts(frame: &mut Frame<'_>, area: Rect, cycle: &HistoryCycle, display_root: &Path) {
    let block = chrome_block().title(Line::from(vec![
        Span::styled(" ARTIFACTS ", Style::default().fg(AMBER)),
        Span::styled(
            " representative trials · o to open ",
            Style::default().fg(MUTED),
        ),
    ]));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if cycle.artifacts.is_empty() {
        frame.render_widget(
            Paragraph::new("No retained artifacts for this cycle.")
                .style(Style::default().fg(FAINT)),
            inner,
        );
        return;
    }
    let rows = artifact_overview(cycle)
        .into_iter()
        .take(usize::from(inner.height))
        .map(|artifact| {
            let detail = clip(&artifact_overview_detail(artifact), 17);
            let path_width = usize::from(inner.width).saturating_sub(31);
            Line::from(vec![
                Span::styled(
                    format!("{:<13}", artifact_kind_label(artifact.kind)),
                    Style::default().fg(artifact_color(artifact.kind)),
                ),
                Span::styled(
                    format!(
                        "{:<width$}",
                        clip(&display_path(&artifact.path, display_root), path_width),
                        width = path_width
                    ),
                    Style::default().fg(TEXT),
                ),
                Span::raw(" "),
                Span::styled(detail, Style::default().fg(MUTED)),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(rows).style(Style::default().bg(BG)), inner);
}

fn artifact_overview(cycle: &HistoryCycle) -> Vec<&HistoryArtifact> {
    const MAX_ROWS: usize = 6;

    let metadata = cycle
        .artifacts
        .iter()
        .filter(|artifact| {
            matches!(
                artifact.kind,
                HistoryArtifactKind::Comparison | HistoryArtifactKind::Sampling
            )
        })
        .collect::<Vec<_>>();
    let native_limit = MAX_ROWS.saturating_sub(metadata.len());
    let mut selected = Vec::new();

    for kind in [
        HistoryArtifactKind::CpuProfile,
        HistoryArtifactKind::Flamegraph,
        HistoryArtifactKind::HeapSnapshot,
    ] {
        if let Some(artifact) = cycle
            .artifacts
            .iter()
            .find(|artifact| artifact.kind == kind)
        {
            selected.push(artifact);
        }
    }

    for engine in Engine::ALL {
        if selected.len() >= native_limit {
            break;
        }
        if selected
            .iter()
            .any(|artifact| artifact.engine == Some(engine))
        {
            continue;
        }
        if let Some(artifact) = cycle
            .artifacts
            .iter()
            .find(|artifact| artifact.engine == Some(engine))
        {
            selected.push(artifact);
        }
    }

    for artifact in &cycle.artifacts {
        if selected.len() >= native_limit {
            break;
        }
        if artifact.engine.is_some()
            && !selected
                .iter()
                .any(|selected| selected.path == artifact.path)
        {
            selected.push(artifact);
        }
    }

    selected.truncate(native_limit);
    selected.extend(metadata.into_iter().take(MAX_ROWS - selected.len()));
    selected
}

fn artifact_overview_detail(artifact: &HistoryArtifact) -> String {
    match artifact.kind {
        HistoryArtifactKind::CpuProfile | HistoryArtifactKind::Flamegraph => artifact
            .capture_scope
            .as_deref()
            .filter(|scope| *scope != "page")
            .map_or_else(|| "cpu median".to_owned(), |scope| format!("scope {scope}")),
        HistoryArtifactKind::HeapSnapshot => artifact
            .capture_scope
            .as_deref()
            .filter(|scope| *scope != "page")
            .map_or_else(
                || "heap median".to_owned(),
                |scope| format!("scope {scope}"),
            ),
        HistoryArtifactKind::Comparison | HistoryArtifactKind::Sampling => {
            artifact_kind_detail(artifact.kind).to_owned()
        }
    }
}

fn render_promotion(frame: &mut Frame<'_>, area: Rect, cycle: &HistoryCycle) {
    let block = chrome_block().title(Span::styled(" PROMOTION ", Style::default().fg(AMBER)));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(promotion_lines(cycle))
            .style(Style::default().bg(BG))
            .wrap(Wrap { trim: true }),
        inner,
    );
}

fn promotion_lines(cycle: &HistoryCycle) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let promotable = promotable_outcome(&cycle.outcome);
    let ready = cycle.promotion.ready && promotable;
    let readiness = if ready {
        "ready"
    } else if !promotable {
        "not promotable"
    } else {
        "confirmation required"
    };
    lines.push(key_value_line(
        "readiness",
        readiness,
        if readiness == "ready" {
            GREEN
        } else if readiness == "not promotable" {
            RED
        } else {
            AMBER
        },
    ));
    lines.push(key_value_line(
        "searched",
        &format!(
            "{}/{} candidates{}",
            cycle.promotion.searched_candidates,
            cycle.promotion.search_threshold,
            cycle
                .baseline_label
                .as_deref()
                .map(|baseline| format!(" on {baseline}"))
                .unwrap_or_default()
        ),
        TEXT,
    ));
    let lineage_state = if ready {
        "confirmed"
    } else if !promotable {
        "not promotable"
    } else if cycle.promotion.confirmation_required {
        "confirmation required"
    } else {
        "not confirmed"
    };
    let acceptance_state = if cycle.accepted {
        "accepted"
    } else {
        "not accepted"
    };
    lines.push(key_value_line(
        "lineage",
        &format!("{lineage_state}, {acceptance_state}"),
        if ready { GREEN } else { AMBER },
    ));
    lines.push(Line::from(""));
    let command = if cycle.accepted {
        format!(
            "baseline {}",
            cycle.accepted_label.as_deref().unwrap_or("accepted")
        )
    } else if ready {
        cycle.accept_command()
    } else if cycle.promotion.confirmation_required && promotable {
        cycle.confirm_command()
    } else {
        "measure another candidate".to_owned()
    };
    lines.push(Line::from(Span::styled(
        command,
        Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
    )));
    lines
}

fn key_value_line(label: &str, value: &str, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<12}"), Style::default().fg(MUTED)),
        Span::styled(value.to_owned(), Style::default().fg(color)),
    ])
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let block = chrome_block().style(Style::default().bg(SURFACE));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if let Some(notice) = &app.notice {
        frame.render_widget(
            Paragraph::new(format!(" {notice}"))
                .style(Style::default().fg(GREEN).bg(SURFACE))
                .alignment(Alignment::Left),
            inner,
        );
        return;
    }
    let compact = area.width < 125;
    let shortcuts = if compact {
        [
            ("↑↓", "move"),
            ("a", "accepted"),
            ("p/e/i/n", "verdict"),
            ("g", "graphs"),
            ("b/l/d", "filters"),
            ("o", "artifact"),
            ("q", "quit"),
        ]
        .as_slice()
    } else {
        [
            ("↑↓", "move"),
            ("a", "accepted-only"),
            ("p e i n", "verdict"),
            ("g", "graph scope"),
            ("b", "benchmark"),
            ("l", "lineage"),
            ("d", "date"),
            ("o", "open artifact"),
            ("q", "quit"),
        ]
        .as_slice()
    };
    let mut spans = Vec::new();
    for (index, (key, label)) in shortcuts.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("   "));
        }
        spans.push(Span::styled(
            format!("{key} "),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(*label, Style::default().fg(MUTED)));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(SURFACE)),
        inner,
    );
}

fn render_picker(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(picker) = &app.picker else {
        return;
    };
    let items = app.picker_items();
    let width = area.width.saturating_sub(8).min(match picker.kind {
        PickerKind::Benchmark => 72,
        PickerKind::Lineage => 54,
        PickerKind::Date => 42,
        PickerKind::Artifact => 100,
    });
    let height = u16::try_from(items.len())
        .unwrap_or(u16::MAX)
        .saturating_add(4)
        .min(area.height.saturating_sub(4))
        .max(6);
    let popup = centered_rect(area, width, height);
    frame.render_widget(Clear, popup);
    let title = match picker.kind {
        PickerKind::Benchmark => " BENCHMARK ",
        PickerKind::Lineage => " LINEAGE ",
        PickerKind::Date => " DATE RANGE ",
        PickerKind::Artifact => " OPEN ARTIFACT ",
    };
    let block = chrome_block()
        .title(Span::styled(title, Style::default().fg(AMBER)))
        .style(Style::default().bg(SURFACE));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
    let list_items = items
        .into_iter()
        .map(|item| ListItem::new(Line::from(item)))
        .collect::<Vec<_>>();
    let list = List::new(list_items)
        .style(Style::default().fg(TEXT).bg(SURFACE))
        .highlight_style(
            Style::default()
                .fg(TEXT)
                .bg(SELECTED)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▸ ");
    let mut state = ListState::default();
    state.select(Some(picker.selected));
    StatefulWidget::render(list, rows[0], frame.buffer_mut(), &mut state);
    frame.render_widget(
        Paragraph::new(" ↑↓ move   enter choose   esc cancel")
            .style(Style::default().fg(MUTED).bg(SURFACE)),
        rows[1],
    );
}

fn primary_metrics() -> [(&'static str, &'static str); 3] {
    [
        ("workload.wall_ms", "wall"),
        ("browser.cpu_profile.active_ms", "cpu"),
        ("browser.js_heap.live_bytes", "heap"),
    ]
}

fn engine_metric_scale(summary: &EngineSummary) -> f64 {
    summary
        .metrics
        .values()
        .filter_map(|metric| metric.ci_pct)
        .flat_map(|interval| interval.into_iter())
        .map(f64::abs)
        .fold(5.0, f64::max)
}

fn metric_bar(metric: &MetricSummary, width: usize, scale: f64) -> String {
    let width = width.clamp(8, 34);
    let mut cells = vec!['·'; width];
    let center = width / 2;
    cells[center] = '│';
    let position = |value: f64| -> usize {
        let normalized = (value / scale).clamp(-1.0, 1.0);
        let offset = ((normalized + 1.0) * (width.saturating_sub(1) as f64) / 2.0).round();
        offset as usize
    };
    if let Some([low, high]) = metric.ci_pct {
        let start = position(low).min(width - 1);
        let end = position(high).min(width - 1);
        for cell in &mut cells[start.min(end)..=start.max(end)] {
            *cell = '█';
        }
    }
    if let Some(effect) = metric.improvement_pct {
        cells[position(effect).min(width - 1)] = '┃';
    }
    cells.into_iter().collect()
}

fn cycle_wall_effects(cycle: &HistoryCycleSummary) -> impl Iterator<Item = f64> + '_ {
    cycle
        .comparison
        .iter()
        .flat_map(|comparison| &comparison.engines)
        .filter_map(|engine| {
            engine
                .metrics
                .get("workload.wall_ms")
                .and_then(|metric| metric.improvement_pct)
        })
}

fn headline_effect(cycle: &HistoryCycleSummary) -> Option<f64> {
    cycle_wall_effects(cycle).reduce(f64::min)
}

fn mini_graph(cycle: &HistoryCycleSummary, scale: f64) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let values = Engine::ALL.map(|engine| {
        cycle
            .comparison
            .as_ref()
            .and_then(|comparison| comparison.engines.iter().find(|item| item.engine == engine))
            .and_then(|summary| summary.metrics.get("workload.wall_ms"))
            .and_then(|metric| metric.improvement_pct)
    });
    if values.iter().all(Option::is_none) {
        return "───".to_owned();
    }
    values
        .into_iter()
        .map(|value| {
            value.map_or('·', |value| {
                let level = ((value.abs() / scale).clamp(0.0, 1.0) * 7.0).round() as usize;
                BARS[level]
            })
        })
        .collect()
}

fn engine_marks(cycle: &HistoryCycleSummary) -> String {
    Engine::ALL
        .into_iter()
        .map(|engine| {
            cycle
                .comparison
                .as_ref()
                .and_then(|comparison| comparison.engines.iter().find(|item| item.engine == engine))
                .map_or('·', |summary| outcome_mark(&summary.verdict))
        })
        .flat_map(|mark| [mark, ' '])
        .collect::<String>()
        .trim_end()
        .to_owned()
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

fn outcome_color(outcome: &str) -> Color {
    match outcome {
        "positive" => GREEN,
        "equivalent" => BLUE,
        "inconclusive" => AMBER,
        "negative" => RED,
        _ => MUTED,
    }
}

fn classification_color(classification: &str) -> Color {
    match classification {
        "improved" => GREEN,
        "regressed" => RED,
        "equivalent" => BLUE,
        _ => AMBER,
    }
}

fn artifact_kind_label(kind: HistoryArtifactKind) -> &'static str {
    match kind {
        HistoryArtifactKind::CpuProfile => "cpuprofile",
        HistoryArtifactKind::Flamegraph => "speedscope",
        HistoryArtifactKind::HeapSnapshot => "heapsnapshot",
        HistoryArtifactKind::Comparison => "comparison",
        HistoryArtifactKind::Sampling => "sampling",
    }
}

fn artifact_kind_detail(kind: HistoryArtifactKind) -> &'static str {
    match kind {
        HistoryArtifactKind::CpuProfile => "native cpu",
        HistoryArtifactKind::Flamegraph => "flamegraph",
        HistoryArtifactKind::HeapSnapshot => "native heap",
        HistoryArtifactKind::Comparison => "verdict evidence",
        HistoryArtifactKind::Sampling => "pilot + final",
    }
}

fn artifact_color(kind: HistoryArtifactKind) -> Color {
    match kind {
        HistoryArtifactKind::HeapSnapshot | HistoryArtifactKind::Sampling => AMBER,
        HistoryArtifactKind::Comparison => BLUE,
        HistoryArtifactKind::CpuProfile | HistoryArtifactKind::Flamegraph => CYAN,
    }
}

fn artifact_picker_label(artifact: &HistoryArtifact, display_root: &Path) -> String {
    let engine = artifact
        .engine
        .map(|engine| engine.to_string())
        .unwrap_or_else(|| "all".to_owned());
    let scope = artifact.capture_scope.as_deref().unwrap_or_default();
    format!(
        "{:<13} {:<9} {:<16} {}",
        artifact_kind_label(artifact.kind),
        engine,
        scope,
        display_path(&artifact.path, display_root)
    )
}

fn format_metric_values(metric: &str, baseline: f64, candidate: f64) -> String {
    let magnitude = baseline.abs().max(candidate.abs());
    let (scale, unit) = match metric {
        "heap" if magnitude >= 1_048_576.0 => (1.0 / 1_048_576.0, " MB"),
        "heap" if magnitude >= 1_024.0 => (1.0 / 1_024.0, " KB"),
        "heap" => (1.0, " B"),
        _ if magnitude < 0.001 => (1_000_000.0, " ns"),
        _ if magnitude < 1.0 => (1_000.0, " us"),
        _ if magnitude < 1_000.0 => (1.0, " ms"),
        _ => (0.001, " s"),
    };
    format!(
        "{}{} → {}{}",
        format_number(baseline * scale),
        unit,
        format_number(candidate * scale),
        unit
    )
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

fn short_environment(fingerprint: &str) -> String {
    format!("env-{}…", fingerprint.chars().take(6).collect::<String>())
}

fn short_selector(cycle_id: &str) -> String {
    cycle_id.chars().take(20).collect()
}

fn bare_cycle_selector(selector: &str) -> String {
    selector
        .strip_prefix("cycle-")
        .unwrap_or(selector)
        .chars()
        .take(8)
        .collect()
}

fn relative_age(timestamp_ms: u64) -> String {
    let elapsed = now_unix_ms().saturating_sub(timestamp_ms);
    match elapsed {
        value if value < 60_000 => "now".to_owned(),
        value if value < 60 * 60_000 => format!("{}m ago", value / 60_000),
        value if value < 24 * 60 * 60_000 => format!("{}h ago", value / (60 * 60_000)),
        value if value < 30 * 24 * 60 * 60_000 => {
            format!("{}d ago", value / (24 * 60 * 60_000))
        }
        _ => short_date(timestamp_ms),
    }
}

fn format_timestamp(timestamp_ms: u64) -> String {
    local_datetime(timestamp_ms)
        .and_then(|datetime| {
            datetime
                .format(format_description!(
                    "[month repr:short] [day padding:none] [hour]:[minute]"
                ))
                .ok()
        })
        .unwrap_or_else(|| timestamp_ms.to_string())
}

fn short_date(timestamp_ms: u64) -> String {
    local_datetime(timestamp_ms)
        .and_then(|datetime| {
            datetime
                .format(format_description!("[month repr:short] [day padding:none]"))
                .ok()
        })
        .map(|date| date.to_ascii_lowercase())
        .unwrap_or_else(|| timestamp_ms.to_string())
}

fn local_datetime(timestamp_ms: u64) -> Option<OffsetDateTime> {
    let seconds = i64::try_from(timestamp_ms / 1_000).ok()?;
    let utc = OffsetDateTime::from_unix_timestamp(seconds).ok()?;
    let offset = UtcOffset::local_offset_at(utc).unwrap_or(UtcOffset::UTC);
    Some(utc.to_offset(offset))
}

fn host_platform(environment: &bperf_decision::environment::EnvironmentSummary) -> String {
    if environment
        .os_release
        .to_ascii_lowercase()
        .starts_with(&environment.platform.to_ascii_lowercase())
    {
        environment.os_release.clone()
    } else {
        format!("{}-{}", environment.platform, environment.os_release)
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn artifact_display_root(lineage_root: &Path) -> PathBuf {
    let lineage_root = fs::canonicalize(lineage_root).unwrap_or_else(|_| lineage_root.to_owned());
    let data_root = lineage_root.parent();
    if data_root
        .and_then(Path::file_name)
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case(".bperf"))
    {
        return data_root
            .and_then(Path::parent)
            .unwrap_or(&lineage_root)
            .to_owned();
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn display_path(path: &Path, root: &Path) -> String {
    display_path_from(path, root)
}

fn display_path_from(path: &Path, root: &Path) -> String {
    let path = portable_display_path(path);
    let root = portable_display_path(root);
    let root = root.trim_end_matches('/');
    let matches_root = if cfg!(windows) {
        path.get(..root.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(root))
    } else {
        path.starts_with(root)
    };
    if matches_root && path.as_bytes().get(root.len()) == Some(&b'/') {
        path[root.len() + 1..].to_owned()
    } else {
        path
    }
}

fn portable_display_path(path: &Path) -> String {
    let path = path.to_string_lossy().replace('\\', "/");
    if let Some(path) = path.strip_prefix("//?/UNC/") {
        format!("//{path}")
    } else if let Some(path) = path.strip_prefix("//?/") {
        path.to_owned()
    } else {
        path
    }
}

fn open_artifact(path: &Path) -> Result<()> {
    if !path.is_file() {
        bail!("retained artifact no longer exists: {}", path.display());
    }
    let mut command = platform_opener(path);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to open artifact {}", path.display()))?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn platform_opener(path: &Path) -> Command {
    let mut command = Command::new("rundll32.exe");
    command.arg("url.dll,FileProtocolHandler").arg(path);
    command
}

#[cfg(target_os = "macos")]
fn platform_opener(path: &Path) -> Command {
    let mut command = Command::new("open");
    command.arg(path);
    command
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_opener(path: &Path) -> Command {
    let mut command = Command::new("xdg-open");
    command.arg(path);
    command
}

#[cfg(not(any(target_os = "windows", target_os = "macos", unix)))]
fn platform_opener(_path: &Path) -> Command {
    Command::new("false")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use bperf_decision::{
        comparison::{AnchorSummary, ComparisonSummary, EngineSummary, MetricSummary},
        environment::EnvironmentSummary,
        lineage::{
            HistoryArtifact, HistoryBaseline, HistoryChangeSummary, HistoryCycle,
            HistoryCycleSummary, HistoryIndexEntry, HistoryOverview, HistoryPromotionSummary,
        },
    };
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

    #[test]
    fn wide_history_renders_the_decision_evidence_and_all_shortcuts() {
        let app = test_app();
        let screen = render_screen(&app, 200, 44);
        for expected in [
            "bperf",
            "history",
            "hls-mp4-parser",
            "POSITIVE",
            "chromium",
            "firefox",
            "webkit",
            "wall",
            "ARTIFACTS",
            "PROMOTION",
            "P 22222222 ▲ b-01",
            ".bperf/measurements/measure-candidate",
            "readiness   ready",
            "searched    4/5 candidates on b-01",
            "lineage     confirmed, not accepted",
            "bperf accept cycle-22222222222222",
            "accepted-only",
            "graph scope",
            "open artifact",
        ] {
            assert!(
                screen.contains(expected),
                "render omitted {expected:?}:\n{screen}"
            );
        }
    }

    #[test]
    fn artifact_paths_drop_windows_verbatim_prefixes_and_workspace_roots() {
        let root = Path::new(r"F:\hls.js");
        let artifact = Path::new(r"\\?\F:\hls.js\.bperf\measurements\measure-v5\chromium\cpu.json");
        assert_eq!(
            display_path_from(artifact, root),
            ".bperf/measurements/measure-v5/chromium/cpu.json"
        );
        assert_eq!(
            artifact_display_root(Path::new("workspace/.bperf/lineages")),
            PathBuf::from("workspace")
        );
    }

    #[test]
    fn promotion_confirmation_command_quotes_the_benchmark_module() {
        let mut app = test_app();
        let cycle_id = app.history.cycles[0].cycle_id.clone();
        let cycle = app.cycle_details.get_mut(&cycle_id).unwrap();
        cycle.promotion.ready = false;
        cycle.promotion.confirmation_required = true;
        cycle.benchmark_module = Some("benchmarks/parser suite's $draft.bench.ts".to_owned());

        let command = promotion_lines(cycle)
            .last()
            .unwrap()
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        #[cfg(not(windows))]
        assert_eq!(
            command,
            "bperf confirm 'benchmarks/parser suite'\"'\"'s $draft.bench.ts' cycle-22222222222222"
        );
        #[cfg(windows)]
        assert_eq!(
            command,
            "bperf confirm 'benchmarks/parser suite''s $draft.bench.ts' cycle-22222222222222"
        );
    }

    #[test]
    fn artifact_overview_is_bounded_and_keeps_decision_metadata() {
        let mut app = test_app();
        let cycle_id = app.history.cycles[0].cycle_id.clone();
        let cycle = app.cycle_details.get_mut(&cycle_id).unwrap();
        cycle.artifacts = [
            (HistoryArtifactKind::CpuProfile, Some(Engine::Chromium)),
            (HistoryArtifactKind::Flamegraph, Some(Engine::Chromium)),
            (HistoryArtifactKind::HeapSnapshot, Some(Engine::Chromium)),
            (HistoryArtifactKind::CpuProfile, Some(Engine::Firefox)),
            (HistoryArtifactKind::Flamegraph, Some(Engine::Firefox)),
            (HistoryArtifactKind::HeapSnapshot, Some(Engine::Firefox)),
            (HistoryArtifactKind::CpuProfile, Some(Engine::Webkit)),
            (HistoryArtifactKind::Comparison, None),
            (HistoryArtifactKind::Sampling, None),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, (kind, engine))| HistoryArtifact {
            kind,
            engine,
            capture_scope: engine.map(|_| "page".to_owned()),
            path: PathBuf::from(format!(".bperf/evidence/{index}.json")),
        })
        .collect();

        let overview = artifact_overview(cycle);
        assert_eq!(overview.len(), 6);
        assert!(
            overview
                .iter()
                .any(|artifact| artifact.kind == HistoryArtifactKind::Comparison)
        );
        assert!(
            overview
                .iter()
                .any(|artifact| artifact.kind == HistoryArtifactKind::Sampling)
        );
    }

    #[test]
    fn compact_and_minimum_size_layouts_keep_the_interface_actionable() {
        let app = test_app();
        let compact = render_screen(&app, 110, 34);
        assert!(compact.contains("chromium"));
        assert!(compact.contains("ARTIFACTS"));
        assert!(compact.contains("p/e/i/n"));

        let small = render_screen(&app, 80, 20);
        assert!(small.contains("resize to at least 92x25"));
        assert!(small.contains("q quit"));
    }

    #[test]
    fn fluid_horizontal_layout_keeps_explicit_column_gutters() {
        let mut app = test_app();
        let cycle_id = app.history.cycles[0].cycle_id.clone();
        app.cycle_details
            .get_mut(&cycle_id)
            .unwrap()
            .artifacts
            .push(HistoryArtifact {
                kind: HistoryArtifactKind::HeapSnapshot,
                engine: Some(Engine::Chromium),
                capture_scope: Some("page".to_owned()),
                path: PathBuf::from(
                    ".bperf/measurements/measure-candidate/chromium/heap.heapsnapshot",
                ),
            });

        for width in [135, 139, 140, 150, 159] {
            let fluid = render_screen(&app, width, 48);
            assert!(
                fluid.contains("chromium  positive"),
                "engine columns collided at {width} columns:\n{fluid}"
            );
            assert!(
                fluid.contains("heapsnapshot  .bperf"),
                "artifact columns collided at {width} columns:\n{fluid}"
            );
            assert!(
                fluid.contains("d 30d"),
                "medium-width filters did not compact at {width} columns:\n{fluid}"
            );
        }
    }

    #[test]
    fn shortcuts_update_filters_selection_graphs_and_pickers() {
        let mut app = test_app();
        assert_eq!(
            app.selected_cycle().map(|cycle| cycle.selector.as_str()),
            Some("cycle-22222222222222")
        );

        app.handle_key(key('a')).unwrap();
        assert!(app.accepted_only);
        assert_eq!(
            app.selected_cycle().map(|cycle| cycle.selector.as_str()),
            Some("cycle-11111111111111")
        );

        app.handle_key(key('a')).unwrap();
        app.handle_key(key('p')).unwrap();
        assert!(!app.verdicts.positive);
        assert_eq!(app.visible_indices().len(), 1);

        app.handle_key(key('g')).unwrap();
        assert_eq!(app.graph_scope, GraphScope::VisibleCycles);

        app.handle_key(key('b')).unwrap();
        assert_eq!(
            app.picker.as_ref().map(|picker| picker.kind),
            Some(PickerKind::Benchmark)
        );
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();

        app.verdicts.positive = true;
        app.ensure_selection();
        app.select_edge(false);
        app.handle_key(key('o')).unwrap();
        assert_eq!(
            app.picker.as_ref().map(|picker| picker.kind),
            Some(PickerKind::Artifact)
        );

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
                .unwrap(),
            ControlFlow::Quit
        );
    }

    fn key(value: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(value), KeyModifiers::NONE)
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

    fn test_app() -> App {
        let now = now_unix_ms();
        let baseline_id = format!("cycle-{}", "1".repeat(64));
        let candidate_id = format!("cycle-{}", "2".repeat(64));
        let baseline = HistoryCycle {
            cycle_id: baseline_id.clone(),
            selector: "cycle-11111111111111".to_owned(),
            recorded_at_unix_ms: now - 4 * 24 * 60 * 60 * 1_000,
            message: "Establish parser baseline".to_owned(),
            outcome: "measured".to_owned(),
            baseline_label: None,
            baseline_cycle_id: None,
            accepted_label: Some("b-01".to_owned()),
            accepted: true,
            current_baseline: true,
            candidate_measurement_set: "measure-baseline".to_owned(),
            benchmark_module: Some("benchmarks/parser.bench.ts".to_owned()),
            variant_id: "parser-main".to_owned(),
            case_ids: vec!["representative-fragment".to_owned()],
            environment: environment(now),
            comparison: None,
            change: HistoryChangeSummary {
                files_changed: 1,
                additions: 24,
                deletions: 0,
                binary_files: 0,
            },
            promotion: HistoryPromotionSummary {
                ready: true,
                confirmation_required: false,
                searched_candidates: 0,
                search_threshold: 5,
                confirmations: 0,
            },
            artifacts: Vec::new(),
        };
        let candidate = HistoryCycle {
            cycle_id: candidate_id,
            selector: "cycle-22222222222222".to_owned(),
            recorded_at_unix_ms: now - 3 * 60 * 60 * 1_000,
            message: "Reuse decoder config across fragments".to_owned(),
            outcome: "positive".to_owned(),
            baseline_label: Some("b-01".to_owned()),
            baseline_cycle_id: Some(baseline_id.clone()),
            accepted_label: None,
            accepted: false,
            current_baseline: false,
            candidate_measurement_set: "measure-candidate".to_owned(),
            benchmark_module: Some("benchmarks/parser.bench.ts".to_owned()),
            variant_id: "parser-main".to_owned(),
            case_ids: vec!["representative-fragment".to_owned()],
            environment: environment(now),
            comparison: Some(comparison()),
            change: HistoryChangeSummary {
                files_changed: 2,
                additions: 38,
                deletions: 26,
                binary_files: 0,
            },
            promotion: HistoryPromotionSummary {
                ready: true,
                confirmation_required: false,
                searched_candidates: 4,
                search_threshold: 5,
                confirmations: 0,
            },
            artifacts: vec![
                HistoryArtifact {
                    kind: HistoryArtifactKind::CpuProfile,
                    engine: Some(Engine::Chromium),
                    capture_scope: Some("page".to_owned()),
                    path: PathBuf::from(".bperf/measurements/measure-candidate/chromium/cpu.json"),
                },
                HistoryArtifact {
                    kind: HistoryArtifactKind::Comparison,
                    engine: None,
                    capture_scope: None,
                    path: PathBuf::from(".bperf/comparisons/compare-candidate/comparison.json"),
                },
            ],
        };
        let history = HistoryOverview {
            benchmark_id: "hls-mp4-parser".to_owned(),
            subject_id: "mp4-parser".to_owned(),
            cycles: vec![cycle_summary(&candidate), cycle_summary(&baseline)],
            baselines: vec![HistoryBaseline {
                label: "b-01".to_owned(),
                cycle_id: baseline_id,
                measurement_set_id: "measure-baseline".to_owned(),
                promoted_at_unix_ms: now - 4 * 24 * 60 * 60 * 1_000,
                current: true,
            }],
            current_baseline_label: Some("b-01".to_owned()),
        };
        App {
            history_reader: None,
            display_root: PathBuf::from("."),
            index: HistoryIndex {
                benchmarks: vec![HistoryIndexEntry {
                    benchmark_id: "hls-mp4-parser".to_owned(),
                    cycle_count: 2,
                    accepted_count: 1,
                    latest_recorded_at_unix_ms: now - 3 * 60 * 60 * 1_000,
                    latest_outcome: "positive".to_owned(),
                    latest_message: Some("Reuse decoder config across fragments".to_owned()),
                    current_baseline_label: Some("b-01".to_owned()),
                    latest_comparison: Some(comparison()),
                    wall_history_ms: BTreeMap::from([
                        (Engine::Chromium, vec![83.9, 78.1]),
                        (Engine::Firefox, vec![112.2, 106.4]),
                        (Engine::Webkit, vec![115.8, 106.9]),
                    ]),
                }],
                latest_benchmark_id: "hls-mp4-parser".to_owned(),
            },
            selected_cycle_id: Some(format!("cycle-{}", "2".repeat(64))),
            cycle_details: [candidate, baseline]
                .into_iter()
                .map(|cycle| (cycle.cycle_id.clone(), cycle))
                .collect(),
            history,
            accepted_only: false,
            verdicts: VerdictFilters::default(),
            graph_scope: GraphScope::AllCycles,
            lineage_filter: Some("b-01".to_owned()),
            date_range: DateRange::Last30Days,
            picker: None,
            notice: None,
        }
    }

    fn cycle_summary(cycle: &HistoryCycle) -> HistoryCycleSummary {
        HistoryCycleSummary {
            cycle_id: cycle.cycle_id.clone(),
            selector: cycle.selector.clone(),
            recorded_at_unix_ms: cycle.recorded_at_unix_ms,
            message: cycle.message.clone(),
            outcome: cycle.outcome.clone(),
            baseline_label: cycle.baseline_label.clone(),
            baseline_cycle_id: cycle.baseline_cycle_id.clone(),
            accepted_label: cycle.accepted_label.clone(),
            accepted: cycle.accepted,
            current_baseline: cycle.current_baseline,
            candidate_measurement_set: cycle.candidate_measurement_set.clone(),
            benchmark_module: cycle.benchmark_module.clone(),
            comparison: cycle.comparison.clone(),
            promotion: cycle.promotion.clone(),
        }
    }

    fn environment(now: u64) -> EnvironmentSummary {
        EnvironmentSummary {
            recorded_at_unix_ms: now,
            fingerprint: "1a7fabcdeffedcba".to_owned(),
            platform: "macos".to_owned(),
            arch: "arm64".to_owned(),
            os_release: "macos-15.3".to_owned(),
            browser_versions: BTreeMap::from([
                (Engine::Chromium, "141.0.7390.54".to_owned()),
                (Engine::Firefox, "134.0b7".to_owned()),
                (Engine::Webkit, "19.0 (pw)".to_owned()),
            ]),
        }
    }

    fn comparison() -> ComparisonSummary {
        ComparisonSummary {
            comparison_id: "compare-candidate".to_owned(),
            report_path: None,
            baseline_measurement_set: "measure-baseline".to_owned(),
            candidate_measurement_set: "measure-candidate".to_owned(),
            environment_fingerprint: Some("1a7fabcdeffedcba".to_owned()),
            policy: "strict-per-engine".to_owned(),
            verdict: "positive".to_owned(),
            engines: Engine::ALL
                .into_iter()
                .enumerate()
                .map(|(index, engine)| {
                    let effect = 5.2 + index as f64;
                    EngineSummary {
                        engine,
                        verdict: "positive".to_owned(),
                        correctness: "pass".to_owned(),
                        anchor: Some(AnchorSummary {
                            status: "stable".to_owned(),
                            drift_pct: Some(0.8),
                            ci_pct: Some([-1.9, 3.4]),
                        }),
                        metrics: BTreeMap::from([
                            ("workload.wall_ms".to_owned(), metric(effect, 112.2, 106.4)),
                            (
                                "browser.cpu_profile.active_ms".to_owned(),
                                metric(effect - 0.5, 104.4, 99.5),
                            ),
                            (
                                "browser.js_heap.live_bytes".to_owned(),
                                metric(effect - 1.4, 54.5 * 1_048_576.0, 52.4 * 1_048_576.0),
                            ),
                        ]),
                    }
                })
                .collect(),
            warnings: Vec::new(),
        }
    }

    fn metric(effect: f64, baseline: f64, candidate: f64) -> MetricSummary {
        MetricSummary {
            improvement_pct: Some(effect),
            ci_pct: Some([effect - 1.5, effect + 1.5]),
            classification: "improved".to_owned(),
            guardrail_regressed: false,
            baseline_value: Some(baseline),
            candidate_value: Some(candidate),
        }
    }
}
