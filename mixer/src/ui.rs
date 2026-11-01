//! Drawing. Everything is computed from the model on every frame; the only thing
//! kept is where the strips landed, for the mouse.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap};
use ratatui::Frame;

use crate::model::{to_db, App, Meter, Model, Strip, Stream, FLOOR_DB};
use crate::Mixer;

const STRIP_WIDTH: u16 = 11;
const SCALE_WIDTH: u16 = 5;
const DETAIL_HEIGHT: u16 = 6;
/// name, subtitle, above the faders; gain, state, below.
const STRIP_HEAD: u16 = 2;
const STRIP_FOOT: u16 = 2;
const EIGHTHS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
const SCALE_MARKS: [i32; 6] = [0, -6, -12, -24, -36, -48];

const DIM: Style = Style::new().fg(Color::DarkGray);

/// Where a strip is on screen, for the mouse.
#[derive(Clone)]
pub struct Hit {
    pub strip: Strip,
    pub area: Rect,
    /// The fader's rows.
    pub fader: Rect,
    /// The state row, which toggles mute.
    pub mute_row: u16,
}

/// Rows from the bottom of a column of `height` rows, as a fraction of its height.
fn fraction(db: f32) -> f32 {
    ((db - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0)
}

/// The gain a click on `row` of a fader means: the handle lands on that row.
pub fn db_at_row(fader: Rect, row: u16) -> f32 {
    let from_bottom = fader.bottom().saturating_sub(row + 1) as f32;
    FLOOR_DB + from_bottom / fader.height.saturating_sub(1).max(1) as f32 * -FLOOR_DB
}

fn zone(db: f32) -> Color {
    if db >= -6.0 {
        Color::Red
    } else if db >= -18.0 {
        Color::Yellow
    } else {
        Color::Green
    }
}

pub fn format_db(gain: f32) -> String {
    let db = to_db(gain);
    if db == f32::NEG_INFINITY || db < FLOOR_DB {
        "-inf dB".into()
    } else {
        format!("{db:+.1} dB").replace("+0.0", "0.0").replace("-0.0", "0.0")
    }
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn put(buf: &mut Buffer, area: Rect, x: u16, y: u16, text: &str, style: Style) {
    if y >= area.bottom() || x >= area.right() {
        return;
    }
    buf.set_stringn(x, y, text, (area.right() - x) as usize, style);
}

fn centered(buf: &mut Buffer, x: u16, y: u16, width: u16, text: &str, style: Style) {
    let text = truncate(text, width as usize);
    let pad = width.saturating_sub(text.chars().count() as u16) / 2;
    buf.set_stringn(x + pad, y, &text, width as usize, style);
}

/// One channel of a meter, one or two cells wide.
fn draw_meter(buf: &mut Buffer, x: u16, width: u16, top: u16, height: u16, meter: Option<&Meter>, muted: bool) {
    let Some(meter) = meter else {
        for row in 0..height {
            for dx in 0..width {
                buf[(x + dx, top + row)].set_char('░').set_style(DIM);
            }
        }
        return;
    };
    let level = fraction(meter.peak_db) * height as f32;
    let rms = fraction(meter.rms_db) * height as f32;
    let hold = (fraction(meter.hold_db) * height as f32) as u16;
    for row in 0..height {
        let from_bottom = height - 1 - row;
        let db = FLOOR_DB + (from_bottom as f32 + 0.5) / height as f32 * -FLOOR_DB;
        let fill = (level - from_bottom as f32).clamp(0.0, 1.0);
        let glyph = EIGHTHS[(fill * 8.0).round() as usize];
        // The body is the average level; the lighter top is the peak over it.
        let colour = zone(db);
        let mut style = Style::new().fg(colour);
        if muted {
            style = DIM;
        } else if (from_bottom as f32) >= rms {
            style = style.add_modifier(Modifier::DIM);
        }
        for dx in 0..width {
            let cell = &mut buf[(x + dx, top + row)];
            if glyph == ' ' && from_bottom == hold && hold > 0 && !muted {
                cell.set_char('▔').set_style(Style::new().fg(colour));
            } else {
                cell.set_char(glyph).set_style(style);
            }
        }
    }
}

fn draw_fader(buf: &mut Buffer, x: u16, top: u16, height: u16, gain: f32, muted: bool, selected: bool) {
    let db = to_db(gain);
    let handle = if db == f32::NEG_INFINITY {
        height - 1
    } else {
        let position = (fraction(db) * (height - 1) as f32).round() as u16;
        height - 1 - position
    };
    let colour = if muted {
        Color::Red
    } else if selected {
        Color::Cyan
    } else {
        Color::White
    };
    for row in 0..height {
        let (glyph, style) = if row == handle {
            ('┿', Style::new().fg(colour).add_modifier(Modifier::BOLD))
        } else if row > handle {
            ('┃', Style::new().fg(colour))
        } else {
            ('│', DIM)
        };
        buf[(x, top + row)].set_char(glyph).set_style(style);
    }
    buf[(x - 1, top + handle)].set_char('╺').set_style(Style::new().fg(colour));
    buf[(x + 1, top + handle)].set_char('╸').set_style(Style::new().fg(colour));
}

fn draw_scale(buf: &mut Buffer, x: u16, top: u16, height: u16) {
    if height < 2 {
        return;
    }
    for mark in SCALE_MARKS {
        let position = (fraction(mark as f32) * (height - 1) as f32).round() as u16;
        let label = format!("{mark:>4}");
        buf.set_stringn(x, top + height - 1 - position, &label, SCALE_WIDTH as usize, DIM);
    }
}

fn state_of_stream(stream: &Stream) -> (String, Style) {
    if stream.muted() {
        return ("MUTE".into(), Style::new().fg(Color::White).bg(Color::Red).add_modifier(Modifier::BOLD));
    }
    if !stream.status["error"].is_null() {
        return ("error".into(), Style::new().fg(Color::Red));
    }
    if !stream.flowing() {
        return ("no packets".into(), Style::new().fg(Color::Yellow));
    }
    if stream.status["on"].as_bool() == Some(false) {
        return ("idle".into(), DIM);
    }
    ("on".into(), Style::new().fg(Color::Green))
}

fn state_of_app(app: &App) -> (String, Style) {
    if app.muted() {
        return ("MUTE".into(), Style::new().fg(Color::White).bg(Color::Red).add_modifier(Modifier::BOLD));
    }
    if app.active() {
        ("playing".into(), Style::new().fg(Color::Green))
    } else {
        ("idle".into(), DIM)
    }
}

struct StripView<'a> {
    name: String,
    sub: String,
    meters: Option<&'a [Meter; 2]>,
    gain: f32,
    muted: bool,
    state: (String, Style),
}

fn strip_view<'a>(model: &'a Model, strip: &Strip) -> Option<StripView<'a>> {
    match strip {
        Strip::Stream(path) => {
            let stream = model.stream(path)?;
            Some(StripView {
                name: "stream".into(),
                sub: format!(":{}", stream.port()),
                meters: Some(&stream.meters),
                gain: stream.gain(),
                muted: stream.muted(),
                state: state_of_stream(stream),
            })
        }
        Strip::App(ip, id) => {
            let app = model.app(*ip, id)?;
            Some(StripView {
                name: app.name().to_string(),
                sub: app.info["exe"].as_str().unwrap_or("").to_string(),
                meters: app.metered.then_some(&app.meters),
                gain: app.gain(),
                muted: app.muted(),
                state: state_of_app(app),
            })
        }
    }
}

fn draw_strip(buf: &mut Buffer, x: u16, top: u16, height: u16, view: &StripView, selected: bool, solo: bool) -> (Rect, u16) {
    let width = STRIP_WIDTH - 1;
    let name_style = if selected {
        Style::new().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::new().add_modifier(Modifier::BOLD)
    };
    if selected {
        buf.set_stringn(x, top, " ".repeat(width as usize), width as usize, name_style);
    }
    centered(buf, x, top, width, &view.name, name_style);
    centered(buf, x, top + 1, width, &view.sub, DIM);

    let rows = height.saturating_sub(STRIP_HEAD + STRIP_FOOT);
    let body = top + STRIP_HEAD;
    if rows >= 2 {
        let (left, right) = match view.meters {
            Some(m) => (Some(&m[0]), Some(&m[1])),
            None => (None, None),
        };
        draw_meter(buf, x + 1, 2, body, rows, left, view.muted);
        draw_meter(buf, x + 4, 2, body, rows, right, view.muted);
        draw_fader(buf, x + 8, body, rows, view.gain, view.muted, selected);
    }
    let foot = body + rows;
    centered(buf, x, foot, width, &format_db(view.gain), if view.muted { DIM } else { Style::new() });
    let state = if solo { ("SOLO".to_string(), Style::new().fg(Color::Black).bg(Color::Yellow)) } else { view.state.clone() };
    centered(buf, x, foot + 1, width, &state.0, state.1);
    (Rect::new(x + 7, body, 3, rows), foot + 1)
}

fn group_title(model: &Model, ip: Option<std::net::IpAddr>) -> Line<'static> {
    let Some(ip) = ip else {
        return Line::from(Span::styled(" waiting for a sender ", Style::new().fg(Color::Yellow)));
    };
    // The address is in the details below; up here, the name you know it by.
    let name = model.client_name(ip);
    let mut spans = vec![Span::styled(format!(" {name} "), Style::new().add_modifier(Modifier::BOLD))];
    if let Some(os) = model.clients.get(&ip).and_then(|c| c.os.clone()) {
        spans.push(Span::styled(format!("{os} "), DIM));
    }
    Line::from(spans)
}

/// The whole screen.
pub fn draw(frame: &mut Frame, mixer: &mut Mixer) {
    let area = frame.area();
    mixer.hits.clear();
    if area.height < 16 || area.width < 40 {
        let text = "pcmlink-mixer needs at least 40x16. q quits.";
        frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), area);
        return;
    }
    let title = Rect::new(area.x, area.y, area.width, 1);
    let footer = Rect::new(area.x, area.bottom() - 1, area.width, 1);
    let detail = Rect::new(area.x, area.bottom() - 1 - DETAIL_HEIGHT, area.width, DETAIL_HEIGHT);
    let body = Rect::new(area.x, area.y + 1, area.width, detail.y - area.y - 1);

    draw_title(frame, mixer, title);
    draw_body(frame, mixer, body);
    draw_detail(frame, mixer, detail);
    draw_footer(frame, mixer, footer);
    if mixer.help {
        draw_help(frame, area);
    }
}

fn draw_title(frame: &mut Frame, mixer: &Mixer, area: Rect) {
    let mut device = mixer.model.streams.iter().find_map(|s| {
        let name = s.status["device"].as_str()?;
        Some(format!("{name} @ {} Hz", s.status["rate"].as_f64().unwrap_or(0.0)))
    });
    if mixer.model.streams.iter().filter_map(|s| s.status["device"].as_str()).collect::<std::collections::HashSet<_>>().len() > 1 {
        device = Some("several devices".into());
    }
    let buf = frame.buffer_mut();
    buf.set_style(area, Style::new().bg(Color::Indexed(236)));
    put(buf, area, area.x + 1, area.y, "pcmlink mixer", Style::new().fg(Color::Cyan).bg(Color::Indexed(236)).add_modifier(Modifier::BOLD));
    if let Some(device) = device {
        let width = device.chars().count() as u16 + 1;
        if area.width > width + 16 {
            put(buf, area, area.right() - width, area.y, &device, Style::new().bg(Color::Indexed(236)));
        }
    }
}

fn draw_body(frame: &mut Frame, mixer: &mut Mixer, area: Rect) {
    let groups = mixer.model.groups();
    if groups.is_empty() {
        let text = format!("No receivers. Looking for sockets in {}", mixer.dir.display());
        frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }).style(DIM), area);
        return;
    }
    // Scroll sideways so the selected strip is on screen.
    let mut positions: Vec<(usize, u16)> = Vec::new();
    let mut widths: Vec<u16> = Vec::new();
    let mut total = 0u16;
    for (index, group) in groups.iter().enumerate() {
        positions.push((index, total));
        let width = group_width(group.strips.len(), group_title(&mixer.model, group.ip).width() as u16);
        widths.push(width);
        total += width;
    }
    let selected_x = mixer.selected.as_ref().and_then(|selected| {
        groups.iter().enumerate().find_map(|(i, group)| {
            let at = group.strips.iter().position(|s| s == selected)?;
            Some(positions[i].1 + 1 + SCALE_WIDTH + at as u16 * STRIP_WIDTH)
        })
    });
    if let Some(x) = selected_x {
        if x < mixer.scroll {
            mixer.scroll = x.saturating_sub(SCALE_WIDTH + 1);
        } else if x + STRIP_WIDTH + 1 > mixer.scroll + area.width {
            mixer.scroll = x + STRIP_WIDTH + 1 - area.width;
        }
    }
    mixer.scroll = mixer.scroll.min(total.saturating_sub(area.width));

    // Draw everything on a canvas as wide as it needs, and show a window of it,
    // so a group can be partly on screen.
    let scroll = mixer.scroll;
    let mut canvas = Buffer::empty(Rect::new(0, 0, total.max(area.width), area.height));
    let buf = &mut canvas;
    for (i, group) in groups.iter().enumerate() {
        let rect = Rect::new(positions[i].1, 0, widths[i], area.height);
        let block = Block::new()
            .borders(Borders::ALL)
            .border_style(DIM)
            .title(group_title(&mixer.model, group.ip));
        let inner = block.inner(rect);
        block.render(rect, buf);
        draw_scale(buf, inner.x, inner.y + STRIP_HEAD, inner.height.saturating_sub(STRIP_HEAD + STRIP_FOOT));
        let mut x = inner.x + SCALE_WIDTH;
        for strip in &group.strips {
            if x + STRIP_WIDTH > inner.right() + 1 {
                break;
            }
            let Some(view) = strip_view(&mixer.model, strip) else { continue };
            let selected = mixer.selected.as_ref() == Some(strip);
            let solo = mixer.solo.as_ref().is_some_and(|s| &s.strip == strip);
            let (fader, mute_row) = draw_strip(buf, x, inner.y, inner.height, &view, selected, solo);
            // From canvas to screen; a strip scrolled off to the left takes no clicks.
            if x >= scroll && x - scroll < area.width {
                let left = area.x + x - scroll;
                mixer.hits.push(Hit {
                    strip: strip.clone(),
                    area: Rect::new(left, area.y, STRIP_WIDTH.min(area.right() - left), area.height),
                    fader: Rect::new(left + 7, area.y + fader.y, fader.width, fader.height),
                    mute_row: area.y + mute_row,
                });
            }
            x += STRIP_WIDTH;
        }
    }
    let screen = frame.buffer_mut();
    for y in 0..area.height {
        for x in 0..area.width {
            screen[(area.x + x, area.y + y)] = canvas[(scroll + x, y)].clone();
        }
    }
}

/// Wide enough for its strips and its title: a client's name is how it is known.
fn group_width(strips: usize, title: u16) -> u16 {
    (SCALE_WIDTH + strips as u16 * STRIP_WIDTH).max(title) + 2
}

fn sparkline(values: impl Iterator<Item = f32>, top: f32) -> String {
    values
        .map(|v| EIGHTHS[((v / top.max(0.001)).clamp(0.0, 1.0) * 8.0).round() as usize])
        .collect()
}

fn ago(at: Option<std::time::Instant>) -> String {
    match at {
        None => String::new(),
        Some(at) => {
            let s = at.elapsed().as_secs();
            if s < 90 {
                format!(" (last {s}s ago)")
            } else if s < 5400 {
                format!(" (last {}m ago)", s / 60)
            } else {
                format!(" (last {}h ago)", s / 3600)
            }
        }
    }
}

fn counter(label: &str, value: f64) -> Span<'static> {
    let style = if value > 0.0 { Style::new().fg(Color::Yellow) } else { DIM };
    Span::styled(format!("{label} {value:.0}  "), style)
}

fn duration(seconds: u64) -> String {
    match seconds {
        s if s < 90 => format!("{s}s"),
        s if s < 5400 => format!("{}m", s / 60),
        s => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
    }
}

fn stream_lines(model: &Model, stream: &Stream, width: usize) -> Vec<Line<'static>> {
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let source = match stream.peer {
        Some(ip) => model.client_full_name(ip),
        None => "no sender yet".to_string(),
    };
    let device = stream.status["device"].as_str().unwrap_or("no device").to_string();
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("stream :{}", stream.port()), bold),
        Span::raw(format!("  from {source}  to {device} @ {} Hz", stream.num("rate"))),
        Span::styled(format!("  receiver pid {}, up {}", stream.num("pid"), duration(stream.num("up_s") as u64)), DIM),
    ])];
    if let Some(error) = stream.status["error"].as_str() {
        lines.push(Line::from(Span::styled(error.to_string(), Style::new().fg(Color::Red))));
    }
    let top = stream.buffer_history.iter().copied().fold(stream.num("margin_ms") as f32 * 2.0, f32::max);
    let room = width.saturating_sub(62);
    let graph = sparkline(stream.buffer_history.iter().rev().take(room).rev().copied(), top);
    lines.push(Line::from(vec![
        Span::raw(format!(
            "buffer {:.1} ms, low {:.1}, floor {:.0}   {:.0} pkt/s   drift {:+.0} ppm   ",
            stream.num("buf_ms"),
            stream.num("low_ms"),
            stream.num("margin_ms"),
            stream.packet_rate,
            stream.num("drift_ppm"),
        )),
        Span::styled(graph, Style::new().fg(Color::Blue)),
    ]));
    let mut counters = vec![
        counter("lost", stream.num("lost")),
        counter("late", stream.num("late")),
        counter("dup", stream.num("dup")),
        counter("bad", stream.num("bad")),
        counter("restarts", stream.num("resets")),
        counter("gaps ms", stream.num("gap_ms")),
        counter("overflows", stream.num("over")),
    ];
    let dropouts = stream.num("under");
    let style = if dropouts > 0.0 { Style::new().fg(Color::Red) } else { DIM };
    counters.push(Span::styled(format!("dropouts {dropouts:.0}{}", ago(stream.last_dropout)), style));
    lines.push(Line::from(counters));
    lines
}

fn app_lines(app: &App) -> Vec<Line<'static>> {
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let mut head = vec![Span::styled(app.name().to_string(), bold)];
    if let Some(exe) = app.info["exe"].as_str() {
        head.push(Span::raw(format!("  {exe}")));
    }
    if let Some(pid) = app.info["pid"].as_u64() {
        head.push(Span::styled(format!("  pid {pid}"), DIM));
    }
    let mut lines = vec![Line::from(head)];
    if let Some(media) = app.info["media"].as_str() {
        lines.push(Line::from(vec![Span::styled("playing ", DIM), Span::raw(media.to_string())]));
    }
    if let Some(title) = app.info["title"].as_str() {
        lines.push(Line::from(vec![Span::styled("window  ", DIM), Span::raw(title.to_string())]));
    }
    if !app.metered {
        lines.push(Line::from(Span::styled("this platform reports no level for it", DIM)));
    } else {
        lines.push(Line::from(Span::styled("levels are before its volume and mute", DIM)));
    }
    lines
}

fn draw_detail(frame: &mut Frame, mixer: &Mixer, area: Rect) {
    let block = Block::new().borders(Borders::TOP).border_style(DIM);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let lines = match &mixer.selected {
        Some(Strip::Stream(path)) => mixer
            .model
            .stream(path)
            .map(|s| stream_lines(&mixer.model, s, inner.width as usize))
            .unwrap_or_default(),
        Some(Strip::App(ip, id)) => mixer.model.app(*ip, id).map(app_lines).unwrap_or_default(),
        None => Vec::new(),
    };
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn draw_footer(frame: &mut Frame, mixer: &Mixer, area: Rect) {
    let key = Style::new().fg(Color::Cyan);
    let spans = if let Some(question) = &mixer.confirm {
        vec![
            Span::styled(format!(" {} ", question.text), Style::new().fg(Color::Black).bg(Color::Yellow)),
            Span::styled("  y", key),
            Span::raw(" yes  "),
            Span::styled("any other key", key),
            Span::raw(" no"),
        ]
    } else if let Some((text, _)) = &mixer.notice {
        vec![Span::styled(format!(" {text} "), Style::new().fg(Color::Black).bg(Color::Yellow))]
    } else {
        let mut spans = Vec::new();
        for (k, what) in [
            ("←→", "select"),
            ("↑↓", "gain"),
            ("m", "mute"),
            ("s", "solo"),
            ("o", "reopen"),
            ("R", "sender"),
            ("a", if mixer.model.show_idle { "hide idle" } else { "show idle" }),
            ("?", "help"),
        ] {
            spans.push(Span::styled(format!(" {k}"), key));
            spans.push(Span::raw(format!(" {what} ")));
        }
        spans
    };
    let quit = vec![Span::styled("q", key), Span::raw(" or "), Span::styled("Ctrl+C", key), Span::raw(" quit ")];
    let quit_width: usize = quit.iter().map(|s| s.content.chars().count()).sum();
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    // The way out is always on screen, whatever else the line says.
    if area.width as usize > quit_width {
        let x = area.right() - quit_width as u16;
        let rect = Rect::new(x, area.y, quit_width as u16, 1);
        frame.render_widget(Clear, rect);
        frame.render_widget(Paragraph::new(Line::from(quit)), rect);
    }
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let text = [
        ("←  →   h l", "select a strip"),
        ("Tab", "next client"),
        ("↑  ↓   k j", "gain, 1 dB"),
        ("PgUp PgDn", "gain, 6 dB"),
        ("m", "mute"),
        ("s", "solo: mutes the other streams, or the client's other apps"),
        ("o", "reopen the selected stream's audio output"),
        ("R", "restart the selected client's sender (needs its agent)"),
        ("a", "show or hide apps that aren't playing"),
        ("mouse", "click selects, wheel is gain; click a fader, or the state to mute"),
        ("q Ctrl+C", "quit; a solo is undone on the way out"),
    ];
    let lines: Vec<Line> = text
        .iter()
        .map(|(k, what)| {
            Line::from(vec![
                Span::styled(format!(" {k:<12}"), Style::new().fg(Color::Cyan)),
                Span::raw(what.to_string()),
            ])
        })
        .collect();
    let width = 78.min(area.width);
    let height = (lines.len() as u16 + 4).min(area.height);
    let rect = Rect::new(area.x + (area.width - width) / 2, area.y + (area.height - height) / 2, width, height);
    frame.render_widget(Clear, rect);
    let mut lines = lines;
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(" Gain stops at 0 dB. Any key closes this.", DIM)));
    frame.render_widget(
        Paragraph::new(lines).block(Block::new().borders(Borders::ALL).title(" keys ")),
        rect,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fader_and_a_click_agree() {
        let fader = Rect::new(0, 10, 3, 20);
        assert_eq!(db_at_row(fader, 10), 0.0); // top row
        assert_eq!(db_at_row(fader, 29), FLOOR_DB); // bottom row
    }

    #[test]
    fn gain_reads_in_db() {
        assert_eq!(format_db(1.0), "0.0 dB");
        assert_eq!(format_db(0.0), "-inf dB");
        assert_eq!(format_db(0.5), "-6.0 dB");
        assert_eq!(format_db(2.0), "+6.0 dB");
    }

    #[test]
    fn long_names_are_cut() {
        assert_eq!(truncate("Steam Client WebHelper", 10), "Steam Cli…");
        assert_eq!(truncate("short", 10), "short");
    }

    #[test]
    fn sparkline_scales_to_its_top() {
        assert_eq!(sparkline([0.0, 5.0, 10.0].into_iter(), 10.0), " ▄█");
    }
}
