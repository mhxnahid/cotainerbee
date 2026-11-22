//! Rendering.

use ratatui::prelude::*;
use ratatui::symbols::Marker;
use ratatui::widgets::*;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

use crate::app::{App, CRow, Confirm, InputKind, Monitor, Tab, View, HIST_LEN};
use crate::docker::{self, human, short_ports};
use crate::files::Files;
use crate::pager::Pager;
use crate::tree::{Change, Kind};

const ACC: Color = Color::Yellow;
const SEL_BG: Color = Color::Indexed(237);
const SPIN: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
const PALETTE: [Color; 6] = [Color::Cyan, Color::Magenta, Color::Green, Color::Blue, Color::LightRed, Color::LightYellow];

fn dim() -> Style {
    Style::new().fg(Color::DarkGray)
}
fn sel() -> Style {
    Style::new().bg(SEL_BG).add_modifier(Modifier::BOLD)
}
fn block(title: Line<'static>, focused: bool) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(if focused { Style::new().fg(ACC) } else { dim() })
        .title(title)
}
fn title(s: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(format!(" {} ", s.into()), Style::new().fg(ACC).bold()))
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let [head, body, foot] = Layout::vertical([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)]).areas(f.area());
    header(f, head, app);
    let mut v = std::mem::replace(&mut app.view, View::Main);
    match &mut v {
        View::Main => main_view(f, body, app),
        View::Pager(p) => pager(f, body, p),
        View::Files(fl) => files(f, body, fl, app),
        View::Monitor(m) => monitor(f, body, m, app),
    }
    app.view = v;
    footer(f, foot, app);
    if let Some(c) = &app.confirm {
        confirm(f, c);
    }
    if app.help {
        help(f, app);
    }
}

// ------------------------------------------------------------------ chrome

fn header(f: &mut Frame, area: Rect, app: &mut App) {
    let mut spans = vec![Span::styled(" ⬢ containerbee ", Style::new().fg(Color::Black).bg(ACC).bold()), Span::raw(" ")];
    let main = matches!(app.view, View::Main);
    let mut x = area.x + spans.iter().map(|s| s.width() as u16).sum::<u16>();
    let mut hit = [(0u16, 0u16, 0u16); 4];
    for t in Tab::ALL {
        let count = match t {
            Tab::Containers => {
                let r = app.containers.iter().filter(|c| c.running()).count();
                format!("{r}/{}", app.containers.len())
            }
            Tab::Images => app.images.len().to_string(),
            Tab::Volumes => app.volumes.len().to_string(),
            Tab::Networks => app.networks.len().to_string(),
        };
        let label = format!(" {} {} {} ", t.idx() + 1, t.title(), count);
        let w = label.chars().count() as u16;
        hit[t.idx()] = (area.y, x, (x + w).min(area.right()));
        x += w;
        spans.push(if t == app.tab {
            Span::styled(label, if main { Style::new().fg(Color::Black).bg(Color::Gray).bold() } else { Style::new().fg(ACC) })
        } else {
            Span::styled(label, dim())
        });
    }
    app.tab_hit = hit;
    f.render_widget(Line::from(spans), area);
    let mut right = Vec::new();
    if app.jobs > 0 {
        right.push(Span::styled(format!("{} {} running ", SPIN[app.tick as usize % 8], app.jobs), Style::new().fg(ACC)));
    }
    let marks = app.marks[app.tab.idx()].len();
    if marks > 0 && main {
        right.push(Span::styled(format!(" {marks} marked "), Style::new().fg(Color::Black).bg(Color::Cyan)));
    }
    f.render_widget(Line::from(right).right_aligned(), area);
}

fn hints(app: &App) -> &'static str {
    match &app.view {
        View::Pager(p) if p.is_log => "/ search  n/N next  f filter  F follow  t time  w wrap  c clear  ←→ scroll  q back",
        View::Pager(_) => "/ search  n/N next  f filter  w wrap  ←→ scroll  g/G top/end  q back",
        View::Files(f) if f.preview.is_some() => "/ search  n/N next  f filter  w wrap  d download  q back",
        View::Files(f) if f.is_image() => "⏎ open  ⌫ up  Tab layers  / find  d download  q back",
        View::Files(_) => "⏎ open  ⌫ up  / find  d download  u upload  e shell here  r reload  q back",
        View::Monitor(_) => "l logs  e shell  f files  i inspect  q back",
        View::Main => match app.tab {
            Tab::Containers => "⏎ inspect  l logs  e shell  f files  m monitor  s/S stop/start  r restart  D rm  U up  ␣ mark  c group  / filter  ? help",
            Tab::Images => "⏎ inspect  x explore  h history  e shell  u pull  D rm  P prune  ␣ mark  / filter  ? help",
            Tab::Volumes => "⏎ inspect  f browse files  D rm  P prune  ␣ mark  / filter  ? help",
            Tab::Networks => "⏎ inspect  D rm  P prune  ␣ mark  / filter  ? help",
        },
    }
}

fn footer(f: &mut Frame, area: Rect, app: &App) {
    let line = if let Some(inp) = &app.input {
        let hint = match inp.kind {
            InputKind::PagerSearch | InputKind::PagerFilter => "   (regex, smart-case · ⏎ apply · esc cancel)",
            InputKind::Upload => "   (⏎ upload · esc cancel)",
            _ => "",
        };
        Line::from(vec![
            Span::styled(format!(" {}", inp.prompt), Style::new().fg(ACC).bold()),
            Span::raw(inp.buf.clone()),
            Span::styled("█", Style::new().fg(ACC)),
            Span::styled(hint, dim()),
        ])
    } else if let Some((m, err, _)) = &app.msg {
        Line::from(Span::styled(format!(" {m}"), if *err { Style::new().fg(Color::LightRed) } else { Style::new().fg(Color::Green) }))
    } else {
        Line::from(Span::styled(format!(" {}", hints(app)), dim()))
    };
    f.render_widget(line, area);
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height.saturating_sub(2));
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

fn confirm(f: &mut Frame, c: &Confirm) {
    let r = centered(f.area(), 64, 6);
    f.render_widget(Clear, r);
    let b = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(Color::LightRed)).title(title("confirm"));
    let p = Paragraph::new(vec![
        Line::from(c.text.clone()),
        Line::raw(""),
        Line::from(vec![
            Span::styled(" y ", Style::new().fg(Color::Black).bg(Color::LightRed).bold()),
            Span::raw(" yes    "),
            Span::styled(" any ", Style::new().fg(Color::Black).bg(Color::Gray)),
            Span::raw(" cancel"),
        ]),
    ])
    .wrap(Wrap { trim: true })
    .block(b);
    f.render_widget(p, r);
}

fn help(f: &mut Frame, app: &App) {
    let sections: Vec<(&str, Vec<(&str, &str)>)> = vec![
        ("Global", vec![
            ("1-4 / Tab", "switch tab"),
            ("j k ↑ ↓ g G", "move"),
            ("/", "filter list"),
            ("␣ / a / esc", "mark / mark all / clear"),
            ("F5", "refresh"),
            ("mouse", "click a tab · wheel scrolls"),
            ("q", "quit / back"),
        ]),
        ("Containers", vec![
            ("⏎ / i", "inspect (on project: collapse)"),
            ("l", "logs (marked or project = merged)"),
            ("e / E", "shell / run a command"),
            ("f", "browse files"),
            ("m", "monitor (cpu/mem/net/processes)"),
            ("S s r p K", "start stop restart pause kill"),
            ("D", "remove (on project: compose down)"),
            ("U", "compose up -d"),
            ("c", "group by compose project"),
            ("← →", "collapse / expand project"),
            ("o / y", "open port in browser / copy id"),
            ("P", "prune stopped"),
        ]),
        ("Images", vec![
            ("x", "explore layers + files"),
            ("h", "history"),
            ("e", "shell in a throwaway container"),
            ("u", "pull / update"),
            ("D / P", "remove / prune dangling"),
        ]),
        ("Volumes · Networks", vec![("f", "browse volume files (helper container)"), ("D / P", "remove / prune")]),
        ("Logs · text", vec![
            ("/ n N", "search, next, previous"),
            ("f", "filter lines (regex)"),
            ("F t w", "follow, timestamps, wrap"),
            ("← →", "horizontal scroll"),
        ]),
        ("Files", vec![
            ("⏎ ⌫", "open / up"),
            ("d / u", "download / upload (docker cp)"),
            ("e", "shell in current dir"),
            ("Tab", "image layers ↔ files"),
        ]),
    ];
    let wanted: &[&str] = match &app.view {
        View::Main => match app.tab {
            Tab::Containers => &["Global", "Containers"],
            Tab::Images => &["Global", "Images"],
            _ => &["Global", "Volumes · Networks"],
        },
        View::Pager(_) => &["Logs · text"],
        View::Files(f) if f.preview.is_some() => &["Logs · text"],
        View::Files(_) => &["Files", "Global"],
        View::Monitor(_) => &["Global"],
    };
    let mut lines = Vec::new();
    for (name, keys) in sections.into_iter().filter(|(n, _)| wanted.contains(n)) {
        lines.push(Line::from(Span::styled(name, Style::new().fg(ACC).bold())));
        for (k, d) in keys {
            lines.push(Line::from(vec![Span::styled(format!("  {k:<14}"), Style::new().fg(Color::Cyan)), Span::raw(d)]));
        }
    }
    let r = centered(f.area(), 60, lines.len() as u16 + 2);
    f.render_widget(Clear, r);
    f.render_widget(Paragraph::new(lines).block(block(title("keys · any key to close"), true)), r);
}

// ------------------------------------------------------------------ main lists

fn main_view(f: &mut Frame, area: Rect, app: &mut App) {
    let (list, det) = if area.width >= 120 {
        let [a, b] = Layout::horizontal([Constraint::Percentage(64), Constraint::Percentage(36)]).areas(area);
        (a, Some(b))
    } else if area.height >= 28 {
        let [a, b] = Layout::vertical([Constraint::Min(8), Constraint::Length(13)]).areas(area);
        (a, Some(b))
    } else {
        (area, None)
    };
    let t = app.tab.idx();
    if let Some(e) = &app.errors[t] {
        let p = Paragraph::new(e.clone()).style(Style::new().fg(Color::LightRed)).wrap(Wrap { trim: true });
        f.render_widget(p.block(block(title(app.tab.title()), true)), list);
        return;
    }
    match app.tab {
        Tab::Containers => containers(f, list, app),
        Tab::Images => images(f, list, app),
        Tab::Volumes => volumes(f, list, app),
        Tab::Networks => networks(f, list, app),
    }
    if let Some(d) = det {
        details(f, d, app);
    }
}

fn list_block(app: &App, name: &str, shown: usize) -> Block<'static> {
    let t = app.tab.idx();
    let mut tl = vec![Span::styled(format!(" {name} "), Style::new().fg(ACC).bold())];
    if !app.filter[t].is_empty() {
        tl.push(Span::styled(format!("filter: {} ({shown}) ", app.filter[t]), Style::new().fg(Color::Cyan)));
    }
    if !app.loaded[t] {
        tl.push(Span::styled(format!("{} loading ", SPIN[app.tick as usize % 8]), dim()));
    }
    block(Line::from(tl), true)
}

fn render_table(f: &mut Frame, area: Rect, app: &mut App, rows: Vec<Row<'static>>, header: Vec<&'static str>, widths: Vec<Constraint>, blk: Block<'static>) {
    let t = app.tab.idx();
    let n = rows.len();
    let cur = app.cursor();
    let table = Table::new(rows, widths)
        .header(Row::new(header).style(Style::new().fg(Color::Gray).add_modifier(Modifier::UNDERLINED)))
        .block(blk)
        .row_highlight_style(sel())
        .column_spacing(1);
    let mut st = TableState::new().with_offset(app.offset[t]).with_selected(if n > 0 { Some(cur) } else { None });
    f.render_stateful_widget(table, area, &mut st);
    app.offset[t] = st.offset();
    if n == 0 && app.loaded[t] {
        let inner = Rect { x: area.x + 2, y: area.y + 2, width: area.width.saturating_sub(4), height: 1 };
        f.render_widget(Span::styled("nothing here", dim()), inner);
    }
}

fn state_span(state: &str, status: &str) -> Span<'static> {
    let (g, c) = match state {
        "running" if status.contains("unhealthy") => ("●", Color::LightRed),
        "running" if status.contains("starting") => ("●", Color::Yellow),
        "running" => ("●", Color::Green),
        "paused" => ("‖", Color::Yellow),
        "restarting" => ("↻", Color::Magenta),
        "created" => ("○", Color::Blue),
        "exited" if status.starts_with("Exited (0)") => ("○", Color::DarkGray),
        "exited" | "dead" => ("○", Color::LightRed),
        _ => ("?", Color::DarkGray),
    };
    Span::styled(g, Style::new().fg(c))
}

fn spark(vals: Option<&std::collections::VecDeque<f64>>, n: usize) -> String {
    const B: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    const T: [f64; 7] = [0.5, 2.0, 5.0, 10.0, 25.0, 50.0, 90.0];
    let Some(v) = vals else { return String::new() };
    v.iter().rev().take(n).collect::<Vec<_>>().into_iter().rev().map(|&x| B[T.iter().filter(|&&t| x >= t).count()]).collect()
}

fn mark_cell(marked: bool) -> Cell<'static> {
    Cell::from(if marked { Span::styled("▌", Style::new().fg(Color::Cyan)) } else { Span::raw(" ") })
}

fn containers(f: &mut Frame, area: Rect, app: &mut App) {
    let rows = app.container_rows();
    let shown = rows.iter().filter(|r| matches!(r, CRow::Ctr(_))).count();
    let indent = if app.group { "  " } else { "" };
    let trs: Vec<Row> = rows
        .iter()
        .map(|r| match r {
            CRow::Project { name, total, running } => {
                let collapsed = app.collapsed.contains(name);
                let label = if name.is_empty() { "standalone".to_string() } else { name.clone() };
                let all_marked = app.containers.iter().filter(|c| &c.project == name).all(|c| app.marks[0].contains(&c.id));
                let color = if running == total { Color::Green } else if *running == 0 { Color::DarkGray } else { Color::Yellow };
                Row::new(vec![
                    mark_cell(all_marked && *total > 0),
                    Cell::from(Span::styled(if collapsed { "▸" } else { "▾" }, Style::new().fg(ACC))),
                    Cell::from(Span::styled(label, Style::new().fg(ACC).bold())),
                    Cell::from(Span::styled(if name.is_empty() { String::new() } else { "compose".into() }, dim())),
                    Cell::from(Span::styled(format!("{running}/{total} running"), Style::new().fg(color))),
                ])
            }
            CRow::Ctr(i) => {
                let c = &app.containers[*i];
                let st = app.stats.get(c.short());
                let h = app.hist.get(c.short());
                let cpu = st.map(|s| format!("{:.1}%", s.cpu)).unwrap_or_default();
                let mem = st.map(|s| human(s.mem_used)).unwrap_or_default();
                let name_style = if c.running() { Style::new() } else { dim() };
                Row::new(vec![
                    mark_cell(app.marks[0].contains(&c.id)),
                    Cell::from(state_span(&c.state, &c.status)),
                    Cell::from(Span::styled(format!("{indent}{}", c.name), name_style)),
                    Cell::from(Span::styled(c.image.clone(), dim())),
                    Cell::from(c.status.clone()),
                    Cell::from(Line::from(cpu).right_aligned()),
                    Cell::from(Span::styled(spark(h.map(|h| &h.cpu), 8), Style::new().fg(Color::Cyan))),
                    Cell::from(Line::from(mem).right_aligned()),
                    Cell::from(Span::styled(short_ports(&c.ports), Style::new().fg(Color::Blue))),
                ])
            }
        })
        .collect();
    let name_w = app.containers.iter().map(|c| c.name.chars().count()).max().unwrap_or(10).clamp(10, 34) as u16 + if app.group { 2 } else { 0 };
    let blk = list_block(app, "Containers", shown);
    render_table(
        f,
        area,
        app,
        trs,
        vec!["", "", "NAME", "IMAGE", "STATUS", "CPU", "", "MEM", "PORTS"],
        vec![
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(name_w),
            Constraint::Fill(1),
            Constraint::Length(22),
            Constraint::Length(6),
            Constraint::Length(8),
            Constraint::Length(6),
            Constraint::Fill(1),
        ],
        blk,
    );
}

fn image_users(app: &App) -> HashMap<String, Vec<String>> {
    let mut m: HashMap<String, Vec<String>> = HashMap::new();
    for c in &app.containers {
        m.entry(c.image.clone()).or_default().push(c.name.clone());
    }
    m
}

fn users_of<'a>(users: &'a HashMap<String, Vec<String>>, img: &docker::Image) -> Vec<&'a String> {
    let mut v = Vec::new();
    for k in [img.reference(), format!("{}:{}", img.repo, img.tag), img.id.clone()] {
        if let Some(u) = users.get(&k) {
            v.extend(u.iter());
        }
    }
    if img.tag == "latest" {
        if let Some(u) = users.get(&img.repo) {
            v.extend(u.iter());
        }
    }
    v.sort();
    v.dedup();
    v
}

fn images(f: &mut Frame, area: Rect, app: &mut App) {
    let rows = app.image_rows();
    let users = image_users(app);
    let trs: Vec<Row> = rows
        .iter()
        .map(|&i| {
            let im = &app.images[i];
            let used = !users_of(&users, im).is_empty();
            let st = if im.dangling() { dim() } else { Style::new() };
            Row::new(vec![
                mark_cell(app.marks[1].contains(&im.reference())),
                Cell::from(if used { Span::styled("●", Style::new().fg(Color::Green)) } else { Span::raw(" ") }),
                Cell::from(Span::styled(im.repo.clone(), st)),
                Cell::from(Span::styled(im.tag.clone(), Style::new().fg(Color::Cyan))),
                Cell::from(Span::styled(im.id.clone(), dim())),
                Cell::from(Line::from(im.size.clone()).right_aligned()),
                Cell::from(Span::styled(im.created.clone(), dim())),
            ])
        })
        .collect();
    let blk = list_block(app, "Images", rows.len());
    render_table(
        f,
        area,
        app,
        trs,
        vec!["", "", "REPOSITORY", "TAG", "ID", "SIZE", "CREATED"],
        vec![
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Fill(3),
            Constraint::Fill(1),
            Constraint::Length(12),
            Constraint::Length(8),
            Constraint::Length(16),
        ],
        blk,
    );
}

fn volume_users(app: &App) -> HashMap<String, Vec<String>> {
    let mut m: HashMap<String, Vec<String>> = HashMap::new();
    for c in &app.containers {
        for v in c.mounts.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            m.entry(v.to_string()).or_default().push(c.name.clone());
        }
    }
    m
}

fn volumes(f: &mut Frame, area: Rect, app: &mut App) {
    let rows = app.volume_rows();
    let users = volume_users(app);
    let trs: Vec<Row> = rows
        .iter()
        .map(|&i| {
            let v = &app.volumes[i];
            let u = users.get(&v.name).map(|u| u.join(", ")).unwrap_or_default();
            Row::new(vec![
                mark_cell(app.marks[2].contains(&v.name)),
                Cell::from(if u.is_empty() { Span::raw(" ") } else { Span::styled("●", Style::new().fg(Color::Green)) }),
                Cell::from(v.name.clone()),
                Cell::from(Span::styled(v.project.clone(), Style::new().fg(ACC))),
                Cell::from(Span::styled(v.driver.clone(), dim())),
                Cell::from(Span::styled(u, dim())),
            ])
        })
        .collect();
    let blk = list_block(app, "Volumes", rows.len());
    render_table(
        f,
        area,
        app,
        trs,
        vec!["", "", "NAME", "PROJECT", "DRIVER", "USED BY"],
        vec![
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Fill(3),
            Constraint::Fill(1),
            Constraint::Length(7),
            Constraint::Fill(2),
        ],
        blk,
    );
}

fn networks(f: &mut Frame, area: Rect, app: &mut App) {
    let rows = app.network_rows();
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for c in &app.containers {
        for n in c.networks.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            *counts.entry(n).or_default() += 1;
        }
    }
    let trs: Vec<Row> = rows
        .iter()
        .map(|&i| {
            let n = &app.networks[i];
            let c = counts.get(n.name.as_str()).copied().unwrap_or(0);
            Row::new(vec![
                mark_cell(app.marks[3].contains(&n.id)),
                Cell::from(n.name.clone()),
                Cell::from(Span::styled(n.project.clone(), Style::new().fg(ACC))),
                Cell::from(Span::styled(n.driver.clone(), Style::new().fg(Color::Cyan))),
                Cell::from(Span::styled(n.scope.clone(), dim())),
                Cell::from(Span::styled(n.id.chars().take(12).collect::<String>(), dim())),
                Cell::from(Line::from(if c > 0 { c.to_string() } else { String::new() }).right_aligned()),
            ])
        })
        .collect();
    let blk = list_block(app, "Networks", rows.len());
    render_table(
        f,
        area,
        app,
        trs,
        vec!["", "NAME", "PROJECT", "DRIVER", "SCOPE", "ID", "CTRS"],
        vec![
            Constraint::Length(1),
            Constraint::Fill(3),
            Constraint::Fill(1),
            Constraint::Length(8),
            Constraint::Length(6),
            Constraint::Length(12),
            Constraint::Length(4),
        ],
        blk,
    );
}

// ------------------------------------------------------------------ details

fn kv(k: &str, v: impl Into<String>) -> Line<'static> {
    Line::from(vec![Span::styled(format!("{k:<9}"), Style::new().fg(ACC)), Span::raw(v.into())])
}
fn kvs(k: &str, v: impl Into<String>, st: Style) -> Line<'static> {
    Line::from(vec![Span::styled(format!("{k:<9}"), Style::new().fg(ACC)), Span::styled(v.into(), st)])
}
fn js(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Array(a) => a.iter().map(js).collect::<Vec<_>>().join(" "),
        other => other.to_string(),
    }
}
fn when(v: &Value) -> String {
    let s = js(v);
    if s.len() >= 19 && !s.starts_with("0001") {
        s[..19].replace('T', " ")
    } else {
        String::new()
    }
}
fn section(name: &str) -> Line<'static> {
    Line::from(Span::styled(format!("─ {name} "), dim()))
}

fn details(f: &mut Frame, area: Rect, app: &App) {
    let Some((ty, id)) = app.detail_key() else {
        let msg = if app.tab == Tab::Containers && matches!(app.cur_crow(), Some(CRow::Project { .. })) {
            project_details(app)
        } else {
            vec![]
        };
        f.render_widget(Paragraph::new(msg).wrap(Wrap { trim: false }).block(block(title("details"), false)), area);
        return;
    };
    let key = format!("{ty}:{id}");
    let lines = match app.details.get(&key) {
        None => vec![Line::from(Span::styled("loading…", dim()))],
        Some((_, Err(e))) => vec![Line::from(Span::styled(e.clone(), Style::new().fg(Color::LightRed)))],
        Some((_, Ok(v))) => match app.tab {
            Tab::Containers => container_details(v, app),
            Tab::Images => image_details(v, app),
            Tab::Volumes => volume_details(v, app),
            Tab::Networks => network_details(v),
        },
    };
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block(title("details"), false)), area);
}

fn project_details(app: &App) -> Vec<Line<'static>> {
    let Some(CRow::Project { name, .. }) = app.cur_crow() else { return vec![] };
    let cs: Vec<_> = app.containers.iter().filter(|c| c.project == name).collect();
    let mut l = vec![kv("Project", if name.is_empty() { "standalone containers".into() } else { name.clone() })];
    if let Some(c) = cs.first().filter(|_| !name.is_empty()) {
        l.push(kv("Dir", c.compose_dir.clone()));
        l.push(kv("Files", c.compose_files.replace(',', "\n         ")));
    }
    let (mut cpu, mut mem) = (0.0, 0.0);
    for c in &cs {
        if let Some(s) = app.stats.get(c.short()) {
            cpu += s.cpu;
            mem += s.mem_used;
        }
    }
    l.push(kv("Usage", format!("{cpu:.1}% cpu · {} mem", human(mem))));
    l.push(section("services"));
    for c in cs {
        l.push(Line::from(vec![
            state_span(&c.state, &c.status),
            Span::raw(format!(" {:<16} ", if c.service.is_empty() { &c.name } else { &c.service })),
            Span::styled(c.status.clone(), dim()),
        ]));
    }
    if !name.is_empty() {
        l.push(Line::raw(""));
        l.push(Line::from(Span::styled("S start · s stop · r restart · U up -d · D down · l logs", dim())));
    }
    l
}

fn container_details(v: &Value, app: &App) -> Vec<Line<'static>> {
    let cfg = &v["Config"];
    let st = &v["State"];
    let mut l = vec![
        kv("Name", js(&v["Name"]).trim_start_matches('/').to_string()),
        kvs("ID", js(&v["Id"]).chars().take(12).collect::<String>(), dim()),
        kv("Image", js(&cfg["Image"])),
    ];
    let mut state = js(&st["Status"]);
    if let Some(h) = st["Health"]["Status"].as_str() {
        state.push_str(&format!(" ({h})"));
    }
    let since = if st["Running"].as_bool() == Some(true) { when(&st["StartedAt"]) } else { when(&st["FinishedAt"]) };
    if !since.is_empty() {
        state.push_str(&format!(" since {since}"));
    }
    let color = if st["Running"].as_bool() == Some(true) { Color::Green } else { Color::LightRed };
    l.push(kvs("State", state, Style::new().fg(color)));
    if st["Running"].as_bool() != Some(true) {
        l.push(kv("Exit", js(&st["ExitCode"])));
    }
    let err = js(&st["Error"]);
    if !err.is_empty() {
        l.push(kvs("Error", err, Style::new().fg(Color::LightRed)));
    }
    l.push(kv("Restart", format!("{} · {} restarts", js(&v["HostConfig"]["RestartPolicy"]["Name"]), js(&v["RestartCount"]))));
    l.push(kv("Cmd", format!("{} {}", js(&v["Path"]), js(&v["Args"]))));
    let wd = js(&cfg["WorkingDir"]);
    if !wd.is_empty() {
        l.push(kv("Workdir", wd));
    }
    if let Some(s) = app.stats.get(&js(&v["Id"]).chars().take(12).collect::<String>()) {
        l.push(kvs("Usage", format!("{:.1}% cpu · {} / {} · {} pids", s.cpu, human(s.mem_used), human(s.mem_limit), s.pids), Style::new().fg(Color::Cyan)));
        l.push(kvs("I/O", format!("net ↓{} ↑{} · disk r{} w{}", human(s.net.0), human(s.net.1), human(s.blk.0), human(s.blk.1)), Style::new().fg(Color::Cyan)));
    }
    if let Some(ports) = v["NetworkSettings"]["Ports"].as_object() {
        let mut ps: Vec<String> = Vec::new();
        for (cp, binds) in ports {
            match binds.as_array() {
                Some(b) if !b.is_empty() => {
                    let hp: Vec<String> = b.iter().map(|x| js(&x["HostPort"])).collect::<HashSet<_>>().into_iter().collect();
                    ps.push(format!("{}→{cp}", hp.join(",")));
                }
                _ => ps.push(cp.clone()),
            }
        }
        if !ps.is_empty() {
            l.push(kv("Ports", ps.join("  ")));
        }
    }
    if let Some(nets) = v["NetworkSettings"]["Networks"].as_object() {
        for (i, (n, nv)) in nets.iter().enumerate() {
            l.push(kv(if i == 0 { "Networks" } else { "" }, format!("{n}  {}", js(&nv["IPAddress"]))));
        }
    }
    if let Some(m) = v["Mounts"].as_array() {
        for (i, mv) in m.iter().enumerate() {
            let src = if mv["Type"] == "volume" { js(&mv["Name"]) } else { js(&mv["Source"]) };
            let ro = if mv["RW"].as_bool() == Some(false) { " (ro)" } else { "" };
            l.push(kv(if i == 0 { "Mounts" } else { "" }, format!("{src} → {}{ro}", js(&mv["Destination"]))));
        }
    }
    let labels = &cfg["Labels"];
    if let Some(p) = labels["com.docker.compose.project"].as_str() {
        l.push(kv("Compose", format!("{p} / {}", js(&labels["com.docker.compose.service"]))));
    }
    if let Some(env) = cfg["Env"].as_array() {
        l.push(section(&format!("env ({})", env.len())));
        for e in env {
            let s = js(e);
            match s.split_once('=') {
                Some((k, val)) => l.push(Line::from(vec![Span::styled(k.to_string(), Style::new().fg(Color::Cyan)), Span::styled("=", dim()), Span::raw(val.to_string())])),
                None => l.push(Line::raw(s)),
            }
        }
    }
    l
}

fn image_details(v: &Value, app: &App) -> Vec<Line<'static>> {
    let cfg = &v["Config"];
    let mut l = vec![
        kv("Tags", js(&v["RepoTags"])),
        kvs("ID", js(&v["Id"]).trim_start_matches("sha256:").chars().take(12).collect::<String>(), dim()),
        kv("Created", when(&v["Created"])),
        kv("Size", human(v["Size"].as_f64().unwrap_or(0.0))),
        kv("Platform", format!("{}/{}", js(&v["Os"]), js(&v["Architecture"]))),
        kv("Layers", v["RootFS"]["Layers"].as_array().map_or(0, |a| a.len()).to_string()),
    ];
    for (k, f) in [("Entry", "Entrypoint"), ("Cmd", "Cmd"), ("Workdir", "WorkingDir"), ("User", "User")] {
        let s = js(&cfg[f]);
        if !s.is_empty() {
            l.push(kv(k, s));
        }
    }
    if let Some(p) = cfg["ExposedPorts"].as_object() {
        l.push(kv("Expose", p.keys().cloned().collect::<Vec<_>>().join(" ")));
    }
    if let Some(img) = app.cur_image() {
        let users = image_users(app);
        let u = users_of(&users, img);
        if !u.is_empty() {
            l.push(kvs("Used by", u.into_iter().cloned().collect::<Vec<_>>().join(", "), Style::new().fg(Color::Green)));
        }
    }
    if let Some(env) = cfg["Env"].as_array() {
        l.push(section("env"));
        l.extend(env.iter().map(|e| Line::raw(js(e))));
    }
    if let Some(lb) = cfg["Labels"].as_object() {
        l.push(section("labels"));
        l.extend(lb.iter().map(|(k, v)| Line::from(vec![Span::styled(k.clone(), Style::new().fg(Color::Cyan)), Span::raw(format!(" {}", js(v)))])));
    }
    l
}

fn volume_details(v: &Value, app: &App) -> Vec<Line<'static>> {
    let mut l = vec![
        kv("Name", js(&v["Name"])),
        kv("Driver", js(&v["Driver"])),
        kv("Created", when(&v["CreatedAt"])),
        kvs("Path", js(&v["Mountpoint"]), dim()),
        kv("Scope", js(&v["Scope"])),
    ];
    let users = volume_users(app);
    if let Some(u) = users.get(&js(&v["Name"])) {
        l.push(kvs("Used by", u.join(", "), Style::new().fg(Color::Green)));
    }
    if let Some(lb) = v["Labels"].as_object() {
        l.push(section("labels"));
        l.extend(lb.iter().map(|(k, v)| Line::from(vec![Span::styled(k.clone(), Style::new().fg(Color::Cyan)), Span::raw(format!(" {}", js(v)))])));
    }
    l.push(Line::raw(""));
    l.push(Line::from(Span::styled("f: browse files (mounts it into a tiny helper container)", dim())));
    l
}

fn network_details(v: &Value) -> Vec<Line<'static>> {
    let mut l = vec![
        kv("Name", js(&v["Name"])),
        kvs("ID", js(&v["Id"]).chars().take(12).collect::<String>(), dim()),
        kv("Driver", js(&v["Driver"])),
        kv("Scope", js(&v["Scope"])),
        kv("Internal", js(&v["Internal"])),
        kv("Created", when(&v["Created"])),
    ];
    if let Some(cfgs) = v["IPAM"]["Config"].as_array() {
        for c in cfgs {
            l.push(kv("Subnet", format!("{}  gw {}", js(&c["Subnet"]), js(&c["Gateway"]))));
        }
    }
    if let Some(cs) = v["Containers"].as_object() {
        l.push(section(&format!("containers ({})", cs.len())));
        let mut v: Vec<(String, String)> = cs.values().map(|c| (js(&c["Name"]), js(&c["IPv4Address"]))).collect();
        v.sort();
        l.extend(v.into_iter().map(|(n, ip)| Line::from(vec![Span::raw(format!("{n:<24} ")), Span::styled(ip, Style::new().fg(Color::Cyan))])));
    }
    l
}

// ------------------------------------------------------------------ pager

fn skip_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[i..],
        None if n == 0 => s,
        None => "",
    }
}

fn level_style(text: &str) -> Style {
    let t = &text[..text.len().min(200)];
    if t.contains("ERROR") || t.contains("error") || t.contains("Error") || t.contains("FATAL") || t.contains("fatal") || t.contains("panic") || t.contains("CRIT") {
        Style::new().fg(Color::LightRed)
    } else if t.contains("WARN") || t.contains("warn") || t.contains("Warn") {
        Style::new().fg(Color::Yellow)
    } else {
        Style::new()
    }
}

fn highlight(text: &str, re: Option<&regex_lite::Regex>, base: Style, current: bool) -> Vec<Span<'static>> {
    let Some(re) = re else { return vec![Span::styled(text.to_string(), base)] };
    let mut out = Vec::new();
    let mut last = 0;
    let hl = if current { Style::new().fg(Color::Black).bg(Color::LightMagenta) } else { Style::new().fg(Color::Black).bg(ACC) };
    for m in re.find_iter(text) {
        if m.start() == m.end() {
            continue;
        }
        if m.start() > last {
            out.push(Span::styled(text[last..m.start()].to_string(), base));
        }
        out.push(Span::styled(text[m.start()..m.end()].to_string(), hl));
        last = m.end();
    }
    if last < text.len() {
        out.push(Span::styled(text[last..].to_string(), base));
    }
    out
}

fn short_ts(ts: &str) -> String {
    if ts.len() >= 19 {
        format!("{} {}", &ts[5..10], &ts[11..19])
    } else {
        ts.to_string()
    }
}

fn pager(f: &mut Frame, area: Rect, p: &mut Pager) {
    let mut flags = Vec::new();
    if p.is_log {
        flags.push(if p.follow { Span::styled(" ● follow ", Style::new().fg(Color::Green)) } else { Span::styled(" ○ paused ", dim()) });
        if p.ended > 0 && p.ended >= p.sources.len() {
            flags.push(Span::styled(" stream ended ", Style::new().fg(Color::LightRed)));
        }
    }
    if p.filter.is_some() {
        flags.push(Span::styled(format!(" filter /{}/ ", p.filter_src), Style::new().fg(Color::Cyan)));
    }
    if p.search.is_some() {
        flags.push(Span::styled(format!(" search /{}/ ", p.search_src), Style::new().fg(ACC)));
    }
    if p.wrap {
        flags.push(Span::styled(" wrap ", dim()));
    }
    let end = (p.top + p.height).min(p.vis.len());
    flags.push(Span::styled(format!(" {}-{}/{} ", if p.vis.is_empty() { 0 } else { p.top + 1 }, end, p.vis.len()), dim()));
    let blk = block(title(p.title.clone()), true).title_bottom(Line::from(flags).right_aligned());
    let inner = blk.inner(area);
    f.render_widget(blk, area);

    let gutter = if p.numbers { p.lines.len().to_string().len() + 1 } else { 0 };
    p.prepare(inner.height as usize, (inner.width as usize).saturating_sub(gutter));
    let multi = p.sources.len() > 1;
    let src_w = p.sources.iter().map(|s| s.chars().count()).max().unwrap_or(0).min(16);
    let end = (p.top + inner.height as usize).min(p.vis.len());
    let mut lines = Vec::with_capacity(end - p.top);
    for vi in p.top..end {
        let li = p.vis[vi];
        let l = &p.lines[li];
        let mut spans = Vec::new();
        if p.numbers {
            spans.push(Span::styled(format!("{:>w$} ", li + 1, w = gutter - 1), dim()));
        }
        if p.show_ts && !l.ts.is_empty() {
            spans.push(Span::styled(format!("{} ", short_ts(&l.ts)), dim()));
        }
        if multi {
            let name: String = p.sources[l.src].chars().take(src_w).collect();
            spans.push(Span::styled(format!("{name:<src_w$} │ "), Style::new().fg(PALETTE[l.src % PALETTE.len()])));
        }
        let text = if p.wrap { l.text.as_str() } else { skip_chars(&l.text, p.hscroll) };
        let base = if p.is_log { level_style(&l.text) } else { Style::new() };
        spans.extend(highlight(text, p.search.as_ref(), base, p.cur == Some(vi)));
        let mut line = Line::from(spans);
        if p.cur == Some(vi) {
            line = line.style(Style::new().bg(SEL_BG));
        }
        lines.push(line);
    }
    let mut para = Paragraph::new(lines);
    if p.wrap {
        para = para.wrap(Wrap { trim: false });
    }
    f.render_widget(para, inner);
    if p.vis.is_empty() {
        let msg = if p.is_log && p.lines.is_empty() { "waiting for log output…" } else { "no lines" };
        f.render_widget(Span::styled(msg, dim()), inner);
    }
}

// ------------------------------------------------------------------ files

fn files(f: &mut Frame, area: Rect, fl: &mut Files, app: &App) {
    if let Some(p) = &mut fl.preview {
        pager(f, area, p);
        return;
    }
    let show_layers = fl.is_image() && fl.tree().is_some();
    let file_area = if show_layers {
        let [a, b] = Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)]).areas(area);
        layers(f, a, fl);
        b
    } else {
        area
    };
    let mut tl = vec![
        Span::styled(format!(" {} ", fl.title), Style::new().fg(ACC).bold()),
        Span::styled(format!("{} ", fl.cwd), Style::new().fg(Color::Cyan).bold()),
    ];
    if !fl.filter.is_empty() {
        tl.push(Span::styled(format!("find: {} ", fl.filter), Style::new().fg(Color::Magenta)));
    }
    if fl.loading {
        tl.push(Span::styled(format!("{} loading ", SPIN[app.tick as usize % 8]), dim()));
    }
    let blk = block(Line::from(tl), !fl.layer_focus);
    if let Some(e) = &fl.err {
        let p = Paragraph::new(e.clone()).style(Style::new().fg(Color::LightRed)).wrap(Wrap { trim: true }).block(blk);
        f.render_widget(p, file_area);
        return;
    }
    let vis = fl.visible();
    let layer_view = fl.layer_sel > 0;
    let rows: Vec<Row> = vis
        .iter()
        .map(|&i| {
            let e = &fl.entries[i];
            let (mark, mstyle) = match e.change {
                Some(Change::Added) => ("+", Style::new().fg(Color::Green)),
                Some(Change::Modified) => ("~", Style::new().fg(Color::Yellow)),
                Some(Change::Removed) => ("-", Style::new().fg(Color::LightRed)),
                _ => (" ", Style::new()),
            };
            let mut name = vec![Span::styled(
                format!("{}{}", e.name, if e.kind == Kind::Dir { "/" } else { "" }),
                match (e.change, e.kind) {
                    (Some(Change::Removed), _) => Style::new().fg(Color::LightRed).add_modifier(Modifier::CROSSED_OUT),
                    (_, Kind::Dir) => Style::new().fg(Color::Blue).bold(),
                    (_, Kind::Link) => Style::new().fg(Color::Cyan),
                    (_, Kind::Other) => Style::new().fg(Color::Magenta),
                    _ if e.mode.contains('x') => Style::new().fg(Color::Green),
                    _ => Style::new(),
                },
            )];
            if let Some(t) = &e.link {
                name.push(Span::styled(format!(" → {t}"), dim()));
            }
            let size = if e.kind == Kind::Dir { String::new() } else { human(e.size as f64) };
            let extra = match (show_layers, e.layer) {
                (true, Some(l)) => format!("L{l}"),
                _ => e.owner.clone(),
            };
            Row::new(vec![
                Cell::from(Span::styled(mark, mstyle)),
                Cell::from(Span::styled(e.mode.clone(), dim())),
                Cell::from(Span::styled(extra, dim())),
                Cell::from(Line::from(size).right_aligned()),
                Cell::from(Span::styled(e.mtime.clone(), dim())),
                Cell::from(Line::from(name)),
            ])
        })
        .collect();
    let n = rows.len();
    let table = Table::new(
        rows,
        [
            Constraint::Length(1),
            Constraint::Length(10),
            Constraint::Length(if show_layers { 4 } else { 11 }),
            Constraint::Length(6),
            Constraint::Length(16),
            Constraint::Fill(1),
        ],
    )
    .header(
        Row::new(["", "MODE", if show_layers { "LYR" } else { "OWNER" }, "SIZE", "MODIFIED", if layer_view { "CHANGES IN LAYER" } else { "NAME" }])
            .style(Style::new().fg(Color::Gray).add_modifier(Modifier::UNDERLINED)),
    )
    .block(blk)
    .row_highlight_style(if fl.layer_focus { Style::new() } else { sel() })
    .column_spacing(1);
    let mut st = TableState::new().with_offset(fl.offset).with_selected(if n > 0 { Some(fl.sel.min(n - 1)) } else { None });
    f.render_stateful_widget(table, file_area, &mut st);
    fl.offset = st.offset();
    if n == 0 && !fl.loading {
        let r = Rect { x: file_area.x + 2, y: file_area.y + 2, width: file_area.width.saturating_sub(4), height: 1 };
        f.render_widget(Span::styled(if fl.filter.is_empty() { "empty directory" } else { "no matches" }, dim()), r);
    }
}

fn layers(f: &mut Frame, area: Rect, fl: &Files) {
    let Some(t) = fl.tree() else { return };
    let [top, bottom] = Layout::vertical([Constraint::Min(5), Constraint::Length(8)]).areas(area);
    let mut items = vec![ListItem::new(Line::from(vec![
        Span::styled("◆ all layers  ", Style::new().fg(ACC).bold()),
        Span::styled(format!("{:>7}  {} files", human(t.total as f64), t.files), dim()),
    ]))];
    for (i, l) in t.layers.iter().enumerate() {
        items.push(ListItem::new(Line::from(vec![
            Span::styled(format!("L{i:<3}"), Style::new().fg(Color::Cyan)),
            Span::styled(format!("{:>7} {:>5}  ", human(l.size as f64), l.count), if l.size > 50 << 20 { Style::new().fg(Color::LightRed) } else { Style::new() }),
            Span::styled(l.created_by.clone(), dim()),
        ])));
    }
    let list = List::new(items)
        .block(block(title(format!("layers ({})", t.layers.len())), fl.layer_focus))
        .highlight_style(if fl.layer_focus { sel() } else { Style::new().bg(SEL_BG) });
    let mut st = ListState::default().with_selected(Some(fl.layer_sel));
    f.render_stateful_widget(list, top, &mut st);
    let info = if fl.layer_sel == 0 {
        vec![
            Line::from(format!("{} files · {}", t.files, human(t.total as f64))),
            Line::from(Span::styled("merged filesystem as a container would see it", dim())),
            Line::raw(""),
            Line::from(Span::styled("Tab: pick a layer to see only what it changed", dim())),
        ]
    } else {
        let l = &t.layers[fl.layer_sel - 1];
        let (mut a, mut m, mut r) = (0, 0, 0);
        for n in l.files.values().filter(|n| n.kind != Kind::Dir || n.change == Change::Removed) {
            match n.change {
                Change::Added => a += 1,
                Change::Modified => m += 1,
                Change::Removed => r += 1,
                Change::None => {}
            }
        }
        vec![
            Line::from(vec![
                Span::styled(format!("+{a} "), Style::new().fg(Color::Green)),
                Span::styled(format!("~{m} "), Style::new().fg(Color::Yellow)),
                Span::styled(format!("-{r} "), Style::new().fg(Color::LightRed)),
                Span::styled(format!(" {} · {}", human(l.size as f64), l.digest), dim()),
            ]),
            Line::from(l.created_by.clone()),
        ]
    };
    f.render_widget(Paragraph::new(info).wrap(Wrap { trim: false }).block(block(title("layer"), false)), bottom);
}

// ------------------------------------------------------------------ monitor

fn series(q: Option<&std::collections::VecDeque<f64>>, scale: f64) -> Vec<(f64, f64)> {
    let Some(q) = q else { return vec![] };
    let off = HIST_LEN - q.len();
    q.iter().enumerate().map(|(i, &v)| ((off + i) as f64, v * scale)).collect()
}

fn chart<'a>(name: Line<'static>, sets: Vec<(&'a [(f64, f64)], Color)>, max: f64, fmt: fn(f64) -> String) -> Chart<'a> {
    let ds = sets
        .into_iter()
        .map(|(d, c)| Dataset::default().marker(Marker::Braille).graph_type(GraphType::Line).style(Style::new().fg(c)).data(d))
        .collect();
    Chart::new(ds)
        .block(block(name, false))
        .x_axis(Axis::default().bounds([0.0, HIST_LEN as f64]))
        .y_axis(Axis::default().bounds([0.0, max]).labels(vec![Span::styled(fmt(0.0), dim()), Span::styled(fmt(max), dim())]).style(dim()))
        .legend_position(None)
}

fn nice_max(v: f64, min: f64) -> f64 {
    let v = (v * 1.2).max(min);
    let mag = 10f64.powf(v.log10().floor());
    (v / mag).ceil() * mag
}

fn monitor(f: &mut Frame, area: Rect, m: &Monitor, app: &App) {
    let short: String = m.id.chars().take(12).collect();
    let h = app.hist.get(&short);
    let s = app.stats.get(&short);
    let blk = block(title(format!("monitor · {}", m.name)), true);
    let inner = blk.inner(area);
    f.render_widget(blk, area);
    let [info, charts, procs] = Layout::vertical([Constraint::Length(2), Constraint::Percentage(55), Constraint::Min(4)]).areas(inner);

    let info_lines = match s {
        Some(s) => vec![
            Line::from(vec![
                Span::styled(" CPU ", Style::new().fg(ACC)),
                Span::raw(format!("{:>6.1}%   ", s.cpu)),
                Span::styled("MEM ", Style::new().fg(ACC)),
                Span::raw(format!("{} / {} ({:.1}%)   ", human(s.mem_used), human(s.mem_limit), s.mem)),
                Span::styled("PIDs ", Style::new().fg(ACC)),
                Span::raw(s.pids.clone()),
            ]),
            Line::from(vec![
                Span::styled(" NET ", Style::new().fg(ACC)),
                Span::raw(format!("↓{} ↑{} total   ", human(s.net.0), human(s.net.1))),
                Span::styled("DISK ", Style::new().fg(ACC)),
                Span::raw(format!("read {} write {}", human(s.blk.0), human(s.blk.1))),
            ]),
        ],
        None => vec![Line::from(Span::styled(" waiting for stats…", dim()))],
    };
    f.render_widget(Paragraph::new(info_lines), info);

    let cpu = series(h.map(|h| &h.cpu), 1.0);
    let mem = series(h.map(|h| &h.mem), 1.0);
    let rx = series(h.map(|h| &h.rx), 1.0);
    let tx = series(h.map(|h| &h.tx), 1.0);
    let maxof = |d: &[(f64, f64)]| d.iter().map(|p| p.1).fold(0.0, f64::max);
    let [c1, c2, c3] = Layout::horizontal([Constraint::Ratio(1, 3); 3]).areas(charts);
    let last = |d: &[(f64, f64)]| d.last().map_or(0.0, |p| p.1);
    let tl = |s: String, c: Color| Line::from(Span::styled(format!(" {s} "), Style::new().fg(c).bold()));
    f.render_widget(chart(tl(format!("cpu {:.1}%", last(&cpu)), Color::Cyan), vec![(&cpu, Color::Cyan)], nice_max(maxof(&cpu), 1.0), |v| format!("{v:.0}%")), c1);
    f.render_widget(chart(tl(format!("mem {:.1}%", last(&mem)), Color::Magenta), vec![(&mem, Color::Magenta)], nice_max(maxof(&mem), 1.0), |v| format!("{v:.0}%")), c2);
    let net_title = Line::from(vec![
        Span::styled(" net ", Style::new().fg(ACC).bold()),
        Span::styled(format!("↓{}/s ", human(last(&rx))), Style::new().fg(Color::Green).bold()),
        Span::styled(format!("↑{}/s ", human(last(&tx))), Style::new().fg(Color::Yellow).bold()),
    ]);
    f.render_widget(
        chart(
            net_title,
            vec![(&rx, Color::Green), (&tx, Color::Yellow)],
            nice_max(maxof(&rx).max(maxof(&tx)), 1024.0),
            |v| human(v),
        ),
        c3,
    );

    let pblk = block(title("processes"), false);
    match &m.top {
        None => f.render_widget(Paragraph::new(Span::styled("loading processes…", dim())).block(pblk), procs),
        Some(Err(e)) => f.render_widget(Paragraph::new(Span::styled(e.clone(), Style::new().fg(Color::LightRed))).block(pblk), procs),
        Some(Ok(t)) => {
            let mut it = t.lines();
            let hdr: Vec<String> = it.next().unwrap_or("").split_whitespace().map(String::from).collect();
            let n = hdr.len().max(1);
            let rows: Vec<Vec<String>> = it
                .filter(|l| !l.trim().is_empty())
                .map(|l| {
                    let (mut parts, mut rest) = (Vec::with_capacity(n), l.trim());
                    for _ in 0..n - 1 {
                        rest = rest.trim_start();
                        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
                        parts.push(rest[..end].to_string());
                        rest = &rest[end..];
                    }
                    parts.push(rest.trim().to_string());
                    parts
                })
                .collect();
            let mut widths: Vec<Constraint> = (0..n - 1)
                .map(|c| Constraint::Length(rows.iter().map(|r| r[c].len()).chain([hdr.get(c).map_or(0, |h| h.len())]).max().unwrap_or(4) as u16))
                .collect();
            widths.push(Constraint::Fill(1));
            let table = Table::new(rows.into_iter().map(Row::new), widths)
                .header(Row::new(hdr).style(Style::new().fg(Color::Gray).add_modifier(Modifier::UNDERLINED)))
                .column_spacing(2)
                .block(pblk);
            f.render_widget(table, procs);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    #[test]
    fn tab_hitboxes_match_rendered_labels() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let (rtx, _rrx) = std::sync::mpsc::channel();
        let mut app = App::new(tx, rtx);
        let mut term = Terminal::new(TestBackend::new(100, 6)).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        let buf = term.backend().buffer().clone();
        for t in Tab::ALL {
            let (y, x0, x1) = app.tab_hit[t.idx()];
            let s: String = (x0..x1).map(|x| buf[(x, y)].symbol().to_string()).collect();
            assert_eq!(s, format!(" {} {} {} ", t.idx() + 1, t.title(), if t == Tab::Containers { "0/0".into() } else { "0".to_string() }), "tab {}", t.title());
        }
    }
}
