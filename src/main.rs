mod app;
mod docker;
mod files;
mod pager;
mod tree;
mod ui;

use std::io::{self, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ratatui::crossterm::{
    cursor,
    event::{self, DisableMouseCapture, EnableMouseCapture, Event},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};

use app::{App, Suspend};

const USAGE: &str = "containerbee — a lightweight Docker TUI

usage: containerbee

environment:
  CONTAINERBEE_DOCKER        docker binary (default: docker; podman works too)
  CONTAINERBEE_HELPER_IMAGE  image used to browse volumes (default: busybox:latest)
  CONTAINERBEE_DOWNLOADS     where file downloads go (default: current dir)
  CONTAINERBEE_TAIL          log lines to load initially (default: 1000)

press ? inside the app for key bindings.";

fn setup() -> io::Result<()> {
    terminal::enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture, cursor::Hide)
}

fn restore() {
    let _ = terminal::disable_raw_mode();
    let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen, cursor::Show);
}

fn interactive(s: &Suspend) {
    println!("\x1b[1;33m⬢ docker {}\x1b[0m", s.args.join(" "));
    let start = Instant::now();
    let st = std::process::Command::new(docker::bin()).args(&s.args).status();
    let quick_fail = match &st {
        Ok(x) => !x.success() && start.elapsed() < Duration::from_secs(2),
        Err(_) => true,
    };
    if s.pause || quick_fail {
        match st {
            Ok(x) => print!("\n\x1b[2m[{x}] press Enter to return\x1b[0m"),
            Err(e) => print!("\nfailed to run docker: {e} — press Enter to return"),
        }
        let _ = io::stdout().flush();
        let _ = io::stdin().read_line(&mut String::new());
    }
}

fn run() -> io::Result<()> {
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        hook(info);
    }));

    let (tx, rx) = mpsc::channel();
    let refresh = docker::spawn_poller(tx.clone());
    docker::spawn_events(refresh.clone());
    docker::spawn_stats(tx.clone());
    let mut app = App::new(tx, refresh);

    setup()?;
    let mut term = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    term.clear()?;
    let tick = Duration::from_millis(250);
    let mut last = Instant::now();
    let mut last_draw = Instant::now();
    let mut dirty = true;
    let res = (|| -> io::Result<()> {
        loop {
            // Redraw on input/new data, while something is in flight, or once a second.
            if dirty || app.busy() || last_draw.elapsed() >= Duration::from_secs(1) {
                term.draw(|f| ui::draw(f, &mut app))?;
                last_draw = Instant::now();
                dirty = false;
            }
            let wait = tick.saturating_sub(last.elapsed());
            if event::poll(wait)? {
                loop {
                    match event::read()? {
                        Event::Key(k) => {
                            app.on_key(k);
                            dirty = true;
                        }
                        // Mouse motion arrives constantly; only redraw when it did something.
                        Event::Mouse(m) => dirty |= app.on_mouse(m),
                        Event::Resize(..) => dirty = true,
                        _ => {}
                    }
                    if app.quit || app.suspend.is_some() || !event::poll(Duration::ZERO)? {
                        break;
                    }
                }
            }
            while let Ok(m) = rx.try_recv() {
                dirty |= !matches!(m, app::Msg::Stat(_));
                app.on_msg(m);
            }
            if last.elapsed() >= tick {
                app.on_tick();
                last = Instant::now();
            }
            if let Some(s) = app.suspend.take() {
                restore();
                interactive(&s);
                setup()?;
                term.clear()?;
                dirty = true;
            }
            if app.quit {
                return Ok(());
            }
        }
    })();
    restore();
    app.shutdown();
    res
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return;
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("containerbee {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if let Err(e) = docker::run(["version", "--format", "{{.Server.Version}}"]) {
        eprintln!("containerbee: cannot reach the docker daemon: {e}");
        std::process::exit(1);
    }
    let res = run();
    docker::kill_all();
    if let Err(e) = res {
        eprintln!("containerbee: {e}");
        std::process::exit(1);
    }
}
