//! Thin wrapper around the `docker` CLI plus the background workers
//! (list poller, event watcher, stats stream, log followers).

use serde_json::Value;
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use crate::app::Msg;
use crate::files::FEntry;
use crate::tree::Kind;

pub const HELPER_LABEL: &str = "containerbee.helper=1";

pub fn bin() -> String {
    std::env::var("CONTAINERBEE_DOCKER").unwrap_or_else(|_| "docker".to_string())
}

pub fn helper_image() -> String {
    std::env::var("CONTAINERBEE_HELPER_IMAGE").unwrap_or_else(|_| "busybox:latest".to_string())
}

pub fn cmd<I, S>(args: I) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut c = Command::new(bin());
    c.args(args).stdin(Stdio::null());
    c
}

fn err_text(stderr: &[u8], st: ExitStatus) -> String {
    let e = String::from_utf8_lossy(stderr);
    let e = e.trim();
    if e.is_empty() {
        format!("docker exited with {st}")
    } else {
        e.lines().last().unwrap_or(e).trim().to_string()
    }
}

pub fn run_bytes<I, S>(args: I) -> Result<Vec<u8>, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let out = cmd(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("cannot run docker: {e}"))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(err_text(&out.stderr, out.status))
    }
}

pub fn run<I, S>(args: I) -> Result<String, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_bytes(args).map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// Run a command whose useful output may be on stderr (compose); returns the last line.
pub fn run_job(args: &[String]) -> Result<String, String> {
    let out = cmd(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("cannot run docker: {e}"))?;
    if out.status.success() {
        let mut t = String::from_utf8_lossy(&out.stdout).into_owned();
        t.push_str(&String::from_utf8_lossy(&out.stderr));
        let last = t.lines().map(str::trim).filter(|l| !l.is_empty()).last().unwrap_or("");
        let is_id = last.len() >= 12 && last.chars().all(|c| c.is_ascii_hexdigit());
        Ok(if last.len() > 120 || is_id { String::new() } else { last.to_string() })
    } else {
        Err(err_text(&out.stderr, out.status))
    }
}

// ---------------------------------------------------------------- processes

static PROCS: Mutex<Vec<Child>> = Mutex::new(Vec::new());

pub fn register(c: Child) -> u32 {
    let id = c.id();
    PROCS.lock().unwrap().push(c);
    id
}

pub fn reap(id: u32) {
    let mut p = PROCS.lock().unwrap();
    if let Some(i) = p.iter().position(|c| c.id() == id) {
        let mut c = p.remove(i);
        let _ = c.kill();
        let _ = c.wait();
    }
}

pub fn kill_all() {
    for mut c in PROCS.lock().unwrap().drain(..) {
        let _ = c.kill();
        let _ = c.wait();
    }
}

// ------------------------------------------------------------------- models

#[derive(Clone, Debug, Default)]
pub struct Container {
    pub id: String,
    pub name: String,
    pub image: String,
    pub state: String,
    pub status: String,
    pub ports: String,
    pub networks: String,
    pub mounts: String,
    pub project: String,
    pub service: String,
    pub compose_files: String,
    pub compose_dir: String,
}

impl Container {
    pub fn short(&self) -> &str {
        &self.id[..12.min(self.id.len())]
    }
    pub fn running(&self) -> bool {
        self.state == "running"
    }
}

#[derive(Clone, Debug, Default)]
pub struct Image {
    pub id: String,
    pub repo: String,
    pub tag: String,
    pub size: String,
    pub created: String,
}

impl Image {
    pub fn reference(&self) -> String {
        if self.repo != "<none>" && self.tag != "<none>" {
            format!("{}:{}", self.repo, self.tag)
        } else if self.repo != "<none>" {
            self.repo.clone()
        } else {
            self.id.clone()
        }
    }
    pub fn dangling(&self) -> bool {
        self.repo == "<none>"
    }
}

#[derive(Clone, Debug, Default)]
pub struct Volume {
    pub name: String,
    pub driver: String,
    pub project: String,
}

#[derive(Clone, Debug, Default)]
pub struct Network {
    pub id: String,
    pub name: String,
    pub driver: String,
    pub scope: String,
    pub project: String,
}

#[derive(Clone, Debug, Default)]
pub struct Stat {
    pub id: String,
    pub cpu: f64,
    pub mem: f64,
    pub mem_used: f64,
    pub mem_limit: f64,
    pub net: (f64, f64),
    pub blk: (f64, f64),
    pub pids: String,
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

const PS_FMT: &str = r#"{"c":{{json .}},"p":{{json (.Label "com.docker.compose.project")}},"s":{{json (.Label "com.docker.compose.service")}},"f":{{json (.Label "com.docker.compose.project.config_files")}},"w":{{json (.Label "com.docker.compose.project.working_dir")}},"h":{{json (.Label "containerbee.helper")}}}"#;

pub fn containers() -> Result<Vec<Container>, String> {
    let out = run(["ps", "-a", "--no-trunc", "--format", PS_FMT])?;
    let mut v: Vec<Container> = out
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|j| s(j, "h").is_empty())
        .map(|j| {
            let c = &j["c"];
            Container {
                id: s(c, "ID"),
                name: s(c, "Names"),
                image: s(c, "Image"),
                state: s(c, "State"),
                status: s(c, "Status"),
                ports: s(c, "Ports"),
                networks: s(c, "Networks"),
                mounts: s(c, "Mounts"),
                project: s(&j, "p"),
                service: s(&j, "s"),
                compose_files: s(&j, "f"),
                compose_dir: s(&j, "w"),
            }
        })
        .collect();
    v.sort_by(|a, b| (!a.running(), &a.name).cmp(&(!b.running(), &b.name)));
    Ok(v)
}

pub fn images() -> Result<Vec<Image>, String> {
    let out = run(["images", "--format", "{{json .}}"])?;
    let mut v: Vec<Image> = out
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .map(|j| Image {
            id: s(&j, "ID"),
            repo: s(&j, "Repository"),
            tag: s(&j, "Tag"),
            size: s(&j, "Size"),
            created: s(&j, "CreatedSince"),
        })
        .collect();
    v.sort_by(|a, b| (a.dangling(), &a.repo, &a.tag).cmp(&(b.dangling(), &b.repo, &b.tag)));
    Ok(v)
}

pub fn volumes() -> Result<Vec<Volume>, String> {
    let out = run([
        "volume",
        "ls",
        "--format",
        r#"{"v":{{json .}},"p":{{json (.Label "com.docker.compose.project")}}}"#,
    ])?;
    let mut v: Vec<Volume> = out
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .map(|j| Volume { name: s(&j["v"], "Name"), driver: s(&j["v"], "Driver"), project: s(&j, "p") })
        .collect();
    v.sort_by(|a, b| (a.project.is_empty(), &a.project, &a.name).cmp(&(b.project.is_empty(), &b.project, &b.name)));
    Ok(v)
}

pub fn networks() -> Result<Vec<Network>, String> {
    let out = run([
        "network",
        "ls",
        "--no-trunc",
        "--format",
        r#"{"n":{{json .}},"p":{{json (.Label "com.docker.compose.project")}}}"#,
    ])?;
    let mut v: Vec<Network> = out
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .map(|j| Network {
            id: s(&j["n"], "ID"),
            name: s(&j["n"], "Name"),
            driver: s(&j["n"], "Driver"),
            scope: s(&j["n"], "Scope"),
            project: s(&j, "p"),
        })
        .collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(v)
}

pub fn parse_bytes(s: &str) -> f64 {
    let s = s.trim();
    let i = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let n: f64 = s[..i].parse().unwrap_or(0.0);
    let m = match s[i..].trim() {
        "kB" | "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        "KiB" => 1024.0,
        "MiB" => 1048576.0,
        "GiB" => 1073741824.0,
        "TiB" => 1099511627776.0,
        _ => 1.0,
    };
    n * m
}

fn pair(s: &str) -> (f64, f64) {
    match s.split_once('/') {
        Some((a, b)) => (parse_bytes(a), parse_bytes(b)),
        None => (parse_bytes(s), 0.0),
    }
}

fn pct(s: &str) -> f64 {
    s.trim().trim_end_matches('%').parse().unwrap_or(0.0)
}

pub fn human(b: f64) -> String {
    const U: [&str; 5] = ["B", "K", "M", "G", "T"];
    let (mut v, mut i) = (b.max(0.0), 0);
    while v >= 1024.0 && i < 4 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{}B", v as u64)
    } else if v < 10.0 {
        format!("{v:.1}{}", U[i])
    } else {
        format!("{v:.0}{}", U[i])
    }
}

/// "0.0.0.0:8081->80/tcp, [::]:8081->80/tcp" -> "8081→80"
pub fn short_ports(p: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for part in p.split(',').map(str::trim).filter(|x| !x.is_empty()) {
        let s = match part.split_once("->") {
            Some((host, ctr)) => {
                let hp = host.rsplit(':').next().unwrap_or(host);
                format!("{hp}→{}", ctr.trim_end_matches("/tcp"))
            }
            None => part.trim_end_matches("/tcp").to_string(),
        };
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out.join(" ")
}

pub fn first_host_port(p: &str) -> Option<String> {
    p.split(',')
        .filter_map(|x| x.split_once("->"))
        .map(|(h, _)| h.rsplit(':').next().unwrap_or(h).to_string())
        .next()
}

// ------------------------------------------------------------------ workers

/// Polls lists. Returns a sender: send `()` to force a full refresh.
pub fn spawn_poller(tx: Sender<Msg>) -> Sender<()> {
    let (rtx, rrx) = mpsc::channel::<()>();
    thread::spawn(move || {
        let mut full = true;
        let mut last_full = Instant::now();
        loop {
            if tx.send(Msg::Containers(containers())).is_err() {
                return;
            }
            if full || last_full.elapsed() > Duration::from_secs(20) {
                let _ = tx.send(Msg::Images(images()));
                let _ = tx.send(Msg::Volumes(volumes()));
                let _ = tx.send(Msg::Networks(networks()));
                last_full = Instant::now();
                full = false;
            }
            match rrx.recv_timeout(Duration::from_secs(4)) {
                Ok(()) => {
                    thread::sleep(Duration::from_millis(150));
                    while rrx.try_recv().is_ok() {}
                    full = true;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    });
    rtx
}

/// Watches `docker events` and pokes the poller so lists update instantly.
pub fn spawn_events(refresh: Sender<()>) {
    thread::spawn(move || loop {
        if let Ok(mut ch) = cmd(["events", "--format", "{{.Type}}"]).stdout(Stdio::piped()).stderr(Stdio::null()).spawn() {
            let out = ch.stdout.take().unwrap();
            let pid = register(ch);
            for line in BufReader::new(out).lines() {
                if line.is_err() || refresh.send(()).is_err() {
                    break;
                }
            }
            reap(pid);
        }
        thread::sleep(Duration::from_secs(3));
    });
}

/// One long-lived `docker stats` stream for all running containers.
pub fn spawn_stats(tx: Sender<Msg>) {
    thread::spawn(move || loop {
        if let Ok(mut ch) = cmd(["stats", "--format", "{{json .}}"]).stdout(Stdio::piped()).stderr(Stdio::null()).spawn() {
            let out = ch.stdout.take().unwrap();
            let pid = register(ch);
            for line in BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                let (Some(a), Some(b)) = (line.find('{'), line.rfind('}')) else { continue };
                let Ok(v) = serde_json::from_str::<Value>(&line[a..=b]) else { continue };
                let (used, limit) = pair(&s(&v, "MemUsage"));
                let st = Stat {
                    id: s(&v, "ID"),
                    cpu: pct(&s(&v, "CPUPerc")),
                    mem: pct(&s(&v, "MemPerc")),
                    mem_used: used,
                    mem_limit: limit,
                    net: pair(&s(&v, "NetIO")),
                    blk: pair(&s(&v, "BlockIO")),
                    pids: s(&v, "PIDs"),
                };
                if st.id.is_empty() || st.id.starts_with('-') {
                    continue;
                }
                if tx.send(Msg::Stat(st)).is_err() {
                    reap(pid);
                    return;
                }
            }
            reap(pid);
        }
        thread::sleep(Duration::from_secs(3));
    });
}

/// Follow logs of one container; returns pid to reap when the view closes.
pub fn spawn_logs(tx: Sender<Msg>, gen: u64, src: usize, id: &str, tail: usize) -> Option<u32> {
    let (r, w) = std::io::pipe().ok()?;
    let w2 = w.try_clone().ok()?;
    let child = cmd(["logs", "-f", "--timestamps", "--tail", &tail.to_string(), id])
        .stdout(w)
        .stderr(w2)
        .spawn()
        .ok()?;
    let pid = register(child);
    thread::spawn(move || {
        let mut rd = BufReader::new(r);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match rd.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let line = String::from_utf8_lossy(&buf).into_owned();
                    if tx.send(Msg::LogLine { gen, src, line }).is_err() {
                        return;
                    }
                }
            }
        }
        let _ = tx.send(Msg::LogEnd { gen });
    });
    Some(pid)
}

// -------------------------------------------------------------- filesystem

/// List a directory inside a running container via `ls -lAn`.
pub fn ls(id: &str, path: &str) -> Result<Vec<FEntry>, String> {
    let p = if path.ends_with('/') { path.to_string() } else { format!("{path}/") };
    let out = run(["exec", "-e", "LC_ALL=C", id, "ls", "-lAn", &p])?;
    Ok(out.lines().filter_map(parse_ls_line).collect())
}

fn parse_ls_line(l: &str) -> Option<FEntry> {
    if l.starts_with("total ") || l.len() < 10 {
        return None;
    }
    let first = l.chars().next()?;
    let nf = if first == 'c' || first == 'b' { 9 } else { 8 };
    let mut rest = l;
    let mut f: Vec<&str> = Vec::with_capacity(nf);
    for _ in 0..nf {
        rest = rest.trim_start();
        let end = rest.find(' ').unwrap_or(rest.len());
        f.push(&rest[..end]);
        rest = &rest[end..];
    }
    let name_part = rest.strip_prefix(' ')?;
    let kind = match first {
        'd' => Kind::Dir,
        'l' => Kind::Link,
        '-' => Kind::File,
        _ => Kind::Other,
    };
    let (name, link) = if kind == Kind::Link {
        match name_part.split_once(" -> ") {
            Some((n, t)) => (n.to_string(), Some(t.to_string())),
            None => (name_part.to_string(), None),
        }
    } else {
        (name_part.to_string(), None)
    };
    if name == "." || name == ".." || name.is_empty() {
        return None;
    }
    Some(FEntry {
        name,
        kind,
        size: if nf == 8 { f[4].parse().unwrap_or(0) } else { 0 },
        mode: f[0].chars().take(10).collect(),
        owner: format!("{}:{}", f[2], f[3]),
        mtime: f[nf - 3..].join(" "),
        link,
        change: None,
        layer: None,
    })
}

/// Read (up to `limit` bytes of) a file via `docker cp` — works on stopped containers too.
pub fn cp_read(id: &str, path: &str, limit: u64) -> Result<(Vec<u8>, u64), String> {
    let mut child = cmd(["cp", "-L", &format!("{id}:{path}"), "-"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let out = child.stdout.take().unwrap();
    let res = (move || -> Result<(Vec<u8>, u64), String> {
        let mut ar = tar::Archive::new(out);
        let mut entries = ar.entries().map_err(|e| e.to_string())?;
        let mut e = match entries.next() {
            Some(e) => e.map_err(|e| e.to_string())?,
            None => return Err(String::new()),
        };
        if e.header().entry_type().is_dir() {
            return Err("is a directory".into());
        }
        let total = e.size();
        let mut buf = Vec::new();
        (&mut e).take(limit).read_to_end(&mut buf).map_err(|e| e.to_string())?;
        Ok((buf, total))
    })();
    let _ = child.kill();
    let mut err = String::new();
    if let Some(mut s) = child.stderr.take() {
        let _ = s.read_to_string(&mut err);
    }
    let _ = child.wait();
    match res {
        Err(e) => Err(if err.trim().is_empty() { if e.is_empty() { "no data".into() } else { e } } else { err.trim().to_string() }),
        ok => ok,
    }
}

pub fn create_tmp(image: &str) -> Result<String, String> {
    run(["create", "--label", HELPER_LABEL, "--entrypoint", "/containerbee-none", image]).map(|s| s.trim().to_string())
}

pub fn start_volume_helper(vol: &str) -> Result<String, String> {
    let v = format!("{vol}:/vol");
    let img = helper_image();
    run(["run", "-d", "--rm", "--label", HELPER_LABEL, "-v", &v, "-w", "/vol", &img, "sleep", "86400"]).map(|s| s.trim().to_string())
}

pub fn compose_args(project: &str, dir: &str, files: &str, sub: &[&str]) -> Vec<String> {
    let mut a: Vec<String> = vec!["compose".into(), "-p".into(), project.into()];
    if !dir.is_empty() && Path::new(dir).is_dir() {
        a.push("--project-directory".into());
        a.push(dir.into());
    }
    for f in files.split(',').map(str::trim).filter(|f| !f.is_empty() && Path::new(f).exists()) {
        a.push("-f".into());
        a.push(f.into());
    }
    a.extend(sub.iter().map(|s| s.to_string()));
    a
}

pub fn shell_script() -> &'static str {
    "if command -v bash >/dev/null 2>&1; then exec bash; elif command -v ash >/dev/null 2>&1; then exec ash; else exec sh; fi"
}

pub fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

pub fn fmt_unix(t: u64) -> String {
    if t == 0 {
        return "-".into();
    }
    let z = (t / 86400) as i64 + 719468;
    let secs = t % 86400;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe as i64 + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", secs / 3600, secs % 3600 / 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ls_lines() {
        let e = parse_ls_line("drwxr-xr-x    2 0        0             4096 Nov 22 11:57 my dir").unwrap();
        assert_eq!(e.name, "my dir");
        assert_eq!(e.kind, Kind::Dir);
        let e = parse_ls_line("lrwxrwxrwx 1 0 0 7 Jan  1  2024 lib -> usr/lib").unwrap();
        assert_eq!(e.name, "lib");
        assert_eq!(e.link.as_deref(), Some("usr/lib"));
        let e = parse_ls_line("crw-rw-rw- 1 0 0 1, 3 Nov 22 05:57 null").unwrap();
        assert_eq!(e.name, "null");
        assert_eq!(e.kind, Kind::Other);
        assert!(parse_ls_line("total 12").is_none());
    }

    #[test]
    fn misc() {
        assert_eq!(short_ports("0.0.0.0:8081->80/tcp, [::]:8081->80/tcp"), "8081→80");
        assert_eq!(parse_bytes("9.5MiB"), 9.5 * 1048576.0);
        assert_eq!(b64(b"hello"), "aGVsbG8=");
        assert_eq!(fmt_unix(1700000000), "2023-11-14 22:13");
    }
}
