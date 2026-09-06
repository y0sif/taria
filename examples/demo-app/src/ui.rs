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
        let cursor = u16::try_from(app.input.chars().count()).unwrap_or(u16::MAX);
        let x = (area.x + 1 + cursor).min(area.x + area.width - 2);
        frame.set_cursor_position(Position::new(x, area.y + 1));
    }
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
