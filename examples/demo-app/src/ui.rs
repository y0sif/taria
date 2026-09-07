//! Rendering (the View). Reads state, never writes it.

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};

use crate::app::{App, Focus, Tab};

fn bold() -> Style {
    Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD)
}

fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

pub fn view(app: &App, frame: &mut Frame) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title bar
            Constraint::Length(1), // tab header
            Constraint::Min(0),    // task list
            Constraint::Length(3), // input box
            Constraint::Length(1), // footer
        ])
        .split(frame.area());

    render_title(app, frame, chunks[0]);
    render_tabs(app, frame, chunks[1]);
    render_tasks(app, frame, chunks[2]);
    render_input(app, frame, chunks[3]);
    render_footer(app, frame, chunks[4]);

    if app.dialog.is_some() {
        render_dialog(app, frame, frame.area());
    }
}

fn render_title(app: &App, frame: &mut Frame, area: Rect) {
    let spans = vec![
        Span::styled(" taria-demo", bold()),
        Span::styled(format!("  ·  {}", app.socket_hint), dim()),
    ];
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_tabs(app: &App, frame: &mut Frame, area: Rect) {
    let tab_span = |tab: Tab| {
        let style = if app.tab == tab { bold() } else { dim() };
        Span::styled(format!(" {} ", tab.label()), style)
    };
    let spans = vec![
        Span::raw(" "),
        tab_span(Tab::Active),
        Span::styled("│", dim()),
        tab_span(Tab::Done),
    ];
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_tasks(app: &App, frame: &mut Frame, area: Rect) {
    let list_focused = app.focus == Focus::List && app.dialog.is_none();

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(""));
    if app.visible_len() == 0 {
        lines.push(Line::from(Span::styled("    (no tasks here)", dim())));
    }
    for (pos, task) in app.visible_tasks().enumerate() {
        // The cursor row keeps its marker whichever region owns the keyboard,
        // matching the selection the tree publishes: an agent that selects a
        // row while the input has focus moves a cursor a person can see too.
        // Only the highlight tracks focus, so the screen still says where a
        // keypress would land.
        let is_cursor = pos == app.selection;
        let is_selected = list_focused && is_cursor;
        let marker = if is_cursor { "> " } else { "  " };
        let check = if task.done { "[x] " } else { "[ ] " };
        let style = if is_selected {
            bold()
        } else if task.done {
            dim()
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(Span::styled(
            format!("  {marker}{check}{}", task.title),
            style,
        )));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

fn render_input(app: &App, frame: &mut Frame, area: Rect) {
    let input_focused = app.focus == Focus::Input && app.dialog.is_none();
    let border_style = if input_focused { bold() } else { dim() };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(" New task [i] ");
    frame.render_widget(Paragraph::new(app.input.as_str()).block(block), area);

    if input_focused && area.width > 2 {
        let x = cursor_x(area, app.input.chars().count());
        frame.set_cursor_position(Position::new(x, area.y.saturating_add(1)));
    }
}

/// Column the text cursor sits at inside `area` for a draft of `len`
/// characters, clamped to the last cell inside the border.
///
/// Saturating at every step, because `len` is agent-controlled and unbounded
/// by the protocol: a draft longer than `u16::MAX` saturates the count, and
/// then `area.x + 1 + count` overflows *before* the clamp can bite. That
/// arithmetic panicked the demo on a single `set_value` carrying 65535
/// characters, which is one semantic act killing the app an agent is driving.
fn cursor_x(area: Rect, len: usize) -> u16 {
    let cursor = u16::try_from(len).unwrap_or(u16::MAX);
    // `width > 2` at the call site, so this last column is inside the border.
    let last = area.x.saturating_add(area.width).saturating_sub(2);
    area.x.saturating_add(1).saturating_add(cursor).min(last)
}

fn render_footer(app: &App, frame: &mut Frame, area: Rect) {
    let hints = if app.dialog.is_some() {
        "[y/Enter] delete  ·  [n/Esc] cancel"
    } else if app.focus == Focus::Input {
        "[Enter] add task  ·  [Esc] back to list"
    } else {
        "[Tab] switch  ·  [↑/↓] move  ·  [Space] toggle  ·  [i] new  ·  [d] delete  ·  [q] quit"
    };
    let para = Paragraph::new(Line::from(Span::styled(hints, dim()))).alignment(Alignment::Center);
    frame.render_widget(para, area);
}

fn render_dialog(app: &App, frame: &mut Frame, area: Rect) {
    let Some(id) = app.dialog else { return };
    let title = app.task(id).map(|task| task.title.as_str()).unwrap_or("?");

    let popup = centered_rect(60, 30, area);
    frame.render_widget(Clear, popup);

    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(format!("Delete task '{title}'?"), bold())),
        Line::from(""),
        Line::from(vec![
            Span::styled("[y] Delete", bold()),
            Span::raw("    "),
            Span::styled("[n] Cancel", dim()),
        ]),
    ];
    let para = Paragraph::new(lines).alignment(Alignment::Center).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::White))
            .title(Span::styled(" Confirm delete ", bold())),
    );
    frame.render_widget(para, popup);
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);
    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1]);
    horizontal[1]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The input box the layout hands `render_input`: three rows, full width.
    fn input_area(width: u16) -> Rect {
        Rect::new(0, 2, width, 3)
    }

    #[test]
    fn the_cursor_follows_the_draft_and_stops_inside_the_border() {
        let area = input_area(20);
        assert_eq!(cursor_x(area, 0), 1, "an empty draft sits after the border");
        assert_eq!(cursor_x(area, 5), 6);
        assert_eq!(
            cursor_x(area, 40),
            18,
            "a draft wider than the box parks on the last cell inside it"
        );
    }

    /// The regression: a `set_value` long enough to saturate the `u16` count
    /// used to overflow the sum before the clamp, panicking the demo. Any
    /// length the protocol permits has to render.
    #[test]
    fn an_enormous_draft_clamps_instead_of_overflowing() {
        let area = input_area(20);
        for len in [
            u16::MAX as usize - 1,
            u16::MAX as usize,
            u16::MAX as usize + 1,
            usize::MAX,
        ] {
            assert_eq!(cursor_x(area, len), 18, "len {len}");
        }
    }

    /// The other end of the coordinate space: a box against the right edge of
    /// a full-width terminal, where `area.x + 1` is itself at the limit.
    #[test]
    fn a_box_at_the_edge_of_the_coordinate_space_clamps_too() {
        let area = Rect::new(u16::MAX - 3, 0, 3, 3);
        assert_eq!(cursor_x(area, 0), u16::MAX - 2);
        assert_eq!(cursor_x(area, usize::MAX), u16::MAX - 2);
    }
}
