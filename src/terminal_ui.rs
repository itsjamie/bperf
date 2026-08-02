use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyEvent, KeyEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    widgets::{Block, Borders},
};

pub(crate) const BG: Color = Color::Rgb(13, 13, 11);
pub(crate) const SURFACE: Color = Color::Rgb(19, 18, 15);
pub(crate) const SELECTED: Color = Color::Rgb(31, 30, 25);
pub(crate) const FOCUS: Color = Color::Rgb(18, 43, 45);
pub(crate) const BORDER: Color = Color::Rgb(55, 53, 47);
pub(crate) const TEXT: Color = Color::Rgb(210, 201, 181);
pub(crate) const MUTED: Color = Color::Rgb(128, 121, 107);
pub(crate) const FAINT: Color = Color::Rgb(78, 74, 66);
pub(crate) const AMBER: Color = Color::Rgb(222, 171, 67);
pub(crate) const GREEN: Color = Color::Rgb(72, 197, 116);
pub(crate) const RED: Color = Color::Rgb(220, 82, 75);
pub(crate) const BLUE: Color = Color::Rgb(105, 145, 197);
pub(crate) const CYAN: Color = Color::Rgb(66, 177, 181);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ControlFlow {
    Continue,
    Quit,
}

pub(crate) fn run<State>(
    label: &str,
    state: &mut State,
    mut draw: impl FnMut(&mut Frame<'_>, &State),
    mut handle_key: impl FnMut(&mut State, KeyEvent) -> Result<ControlFlow>,
) -> Result<()> {
    let mut terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(error) => {
            ratatui::restore();
            return Err(error)
                .with_context(|| format!("failed to initialize the {label} terminal"));
        }
    };
    let result = run_loop(&mut terminal, state, &mut draw, &mut handle_key);
    let restore = ratatui::try_restore().context("failed to restore the terminal");
    match (result, restore) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

fn run_loop<State>(
    terminal: &mut ratatui::DefaultTerminal,
    state: &mut State,
    draw: &mut impl FnMut(&mut Frame<'_>, &State),
    handle_key: &mut impl FnMut(&mut State, KeyEvent) -> Result<ControlFlow>,
) -> Result<()> {
    loop {
        terminal.draw(|frame| draw(frame, state))?;
        let Event::Key(key) = event::read().context("failed to read terminal input")? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if handle_key(state, key)? == ControlFlow::Quit {
            return Ok(());
        }
    }
}

pub(crate) fn chrome_block<'a>() -> Block<'a> {
    Block::new()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER))
        .style(Style::default().fg(TEXT).bg(BG))
}

pub(crate) fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let vertical = Layout::new(
        Direction::Vertical,
        [
            Constraint::Length(area.height.saturating_sub(height) / 2),
            Constraint::Length(height),
            Constraint::Min(0),
        ],
    )
    .split(area);
    Layout::new(
        Direction::Horizontal,
        [
            Constraint::Length(area.width.saturating_sub(width) / 2),
            Constraint::Length(width),
            Constraint::Min(0),
        ],
    )
    .split(vertical[1])[1]
}

pub(crate) fn fit_sides(left: &str, right: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let right_width = right.chars().count().min(width);
    if right_width + 1 >= width {
        return clip(right, width);
    }
    let left_width = width - right_width - 1;
    format!(
        "{:<left_width$} {}",
        clip(left, left_width),
        clip(right, right_width)
    )
}

pub(crate) fn clip(value: &str, width: usize) -> String {
    let count = value.chars().count();
    if count <= width {
        return value.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_owned();
    }
    let mut clipped = value.chars().take(width - 1).collect::<String>();
    clipped.push('…');
    clipped
}

pub(crate) fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

pub(crate) fn relative_age(timestamp_ms: u64) -> String {
    let elapsed = now_unix_ms().saturating_sub(timestamp_ms);
    match elapsed {
        value if value < 60_000 => "now".to_owned(),
        value if value < 60 * 60_000 => format!("{}m ago", value / 60_000),
        value if value < 24 * 60 * 60_000 => format!("{}h ago", value / (60 * 60_000)),
        value if value < 30 * 24 * 60 * 60_000 => {
            format!("{}d ago", value / (24 * 60 * 60_000))
        }
        _ => format!("{}d ago", elapsed / (24 * 60 * 60_000)),
    }
}
