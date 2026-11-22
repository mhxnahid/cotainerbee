//! Application state, message handling and key bindings.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::Write;
use std::process::Stdio;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

use crate::docker::{self, Container, Image, Network, Stat, Volume};
use crate::files::{CRef, FEntry, Files, Source};
use crate::pager::Pager;
use crate::tree::{self, Tree};

pub enum Msg {
    Containers(Result<Vec<Container>, String>),
    Images(Result<Vec<Image>, String>),
    Volumes(Result<Vec<Volume>, String>),
    Networks(Result<Vec<Network>, String>),
    Stat(Stat),
    Details { key: String, res: Result<Value, String> },
    Job { label: String, res: Result<String, String> },
    Status { text: String, err: bool },
    Pager { gen: u64, title: String, res: Result<String, String> },
    Dir { gen: u64, path: String, fallback: bool, res: Result<Vec<FEntry>, String> },
    File { gen: u64, path: String, res: Result<(Vec<u8>, u64), String> },
    Tree { gen: u64, res: Result<Tree, String>, cref: CRef },
    Helper { gen: u64, res: Result<String, String> },
    Tmp { gen: u64, id: String },
    Reload { gen: u64 },
    LogLine { gen: u64, src: usize, line: String },
    LogEnd { gen: u64 },
    Top { gen: u64, res: Result<String, String> },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Containers,
    Images,
    Volumes,
    Networks,
}

impl Tab {
    pub const ALL: [Tab; 4] = [Tab::Containers, Tab::Images, Tab::Volumes, Tab::Networks];
    pub fn idx(self) -> usize {
        self as usize
    }
    pub fn title(self) -> &'static str {
        ["Containers", "Images", "Volumes", "Networks"][self.idx()]
    }
}

#[derive(Clone)]
pub enum CRow {
    Project { name: String, total: usize, running: usize },
    Ctr(usize),
}

pub const HIST_LEN: usize = 120;

#[derive(Default)]
pub struct Hist {
    pub cpu: VecDeque<f64>,
    pub mem: VecDeque<f64>,
    pub rx: VecDeque<f64>,
    pub tx: VecDeque<f64>,
    last_net: Option<(f64, f64, Instant)>,
}

impl Hist {
    fn push(&mut self, s: &Stat) {
        fn put(q: &mut VecDeque<f64>, v: f64) {
            q.push_back(v);
            while q.len() > HIST_LEN {
                q.pop_front();
            }
        }
        put(&mut self.cpu, s.cpu);
        put(&mut self.mem, s.mem);
        let now = Instant::now();
        match self.last_net {
            Some((rx, tx, t)) => {
                let dt = now.duration_since(t).as_secs_f64();
                if dt >= 0.5 {
                    put(&mut self.rx, ((s.net.0 - rx) / dt).max(0.0));
                    put(&mut self.tx, ((s.net.1 - tx) / dt).max(0.0));
                    self.last_net = Some((s.net.0, s.net.1, now));
                }
            }
            None => self.last_net = Some((s.net.0, s.net.1, now)),
        }
    }
}

pub struct Monitor {
    pub id: String,
    pub name: String,
    pub gen: u64,
    pub top: Option<Result<String, String>>,
    pub last_top: Option<Instant>,
}

pub enum InputKind {
    Filter,
    PagerSearch,
    PagerFilter,
    FilesFilter,
    Upload,
    Exec(String),
}

pub struct Input {
    pub kind: InputKind,
    pub prompt: String,
    pub buf: String,
}

pub struct Confirm {
    pub text: String,
    pub action: Box<dyn FnOnce(&mut App)>,
}

pub struct Suspend {
    pub args: Vec<String>,
    pub pause: bool,
}

pub enum View {
    Main,
    Pager(Pager),
    Files(Files),
    Monitor(Monitor),
}

pub struct App {
    pub tx: Sender<Msg>,
    refresh: Sender<()>,
    pub tab: Tab,
    pub containers: Vec<Container>,
    pub images: Vec<Image>,
    pub volumes: Vec<Volume>,
    pub networks: Vec<Network>,
    pub loaded: [bool; 4],
    pub errors: [Option<String>; 4],
    pub stats: HashMap<String, Stat>,
    pub hist: HashMap<String, Hist>,
    pub sel: [usize; 4],
    keys: [String; 4],
    pub offset: [usize; 4],
    pub marks: [HashSet<String>; 4],
    pub filter: [String; 4],
    pub group: bool,
    pub collapsed: HashSet<String>,
    pub details: HashMap<String, (Instant, Result<Value, String>)>,
    pending: HashSet<String>,
    pub view: View,
    pub help: bool,
    pub confirm: Option<Confirm>,
    pub input: Option<Input>,
    pub msg: Option<(String, bool, Instant)>,
    pub jobs: usize,
    pub gen: u64,
    pub quit: bool,
    pub suspend: Option<Suspend>,
    helpers: HashSet<String>,
    pub tick: u64,
    /// Screen areas of the header tabs, filled in by the renderer: (row, x, x_end).
    pub tab_hit: [(u16, u16, u16); 4],
}

fn label_of(cs: &[Container]) -> String {
    match cs {
        [c] => c.name.clone(),
        _ => format!("{} containers", cs.len()),
    }
}

impl App {
    pub fn new(tx: Sender<Msg>, refresh: Sender<()>) -> Self {
        App {
            tx,
            refresh,
            tab: Tab::Containers,
            containers: Vec::new(),
            images: Vec::new(),
            volumes: Vec::new(),
            networks: Vec::new(),
            loaded: [false; 4],
            errors: Default::default(),
            stats: HashMap::new(),
            hist: HashMap::new(),
            sel: [0; 4],
            keys: Default::default(),
            offset: [0; 4],
            marks: Default::default(),
            filter: Default::default(),
            group: true,
            collapsed: HashSet::new(),
            details: HashMap::new(),
            pending: HashSet::new(),
            view: View::Main,
            help: false,
            confirm: None,
            input: None,
            msg: None,
            jobs: 0,
            gen: 0,
            quit: false,
            suspend: None,
            helpers: HashSet::new(),
            tick: 0,
            tab_hit: [(0, 0, 0); 4],
        }
    }

    /// Something is animating (spinner) and wants frequent redraws.
    pub fn busy(&self) -> bool {
        self.jobs > 0
            || self.loaded.iter().any(|l| !l)
            || matches!(&self.view, View::Files(f) if f.loading)
            || matches!(&self.view, View::Pager(p) if p.is_log && p.follow)
    }

    pub fn info(&mut self, s: impl Into<String>) {
        self.msg = Some((s.into(), false, Instant::now()));
    }
    pub fn error(&mut self, s: impl Into<String>) {
        self.msg = Some((s.into(), true, Instant::now()));
    }

    fn bg(&self, f: impl FnOnce() -> Msg + Send + 'static) {
        let tx = self.tx.clone();
        thread::spawn(move || {
            let _ = tx.send(f());
        });
    }

    fn next_gen(&mut self) -> u64 {
        self.gen += 1;
        self.gen
    }

    pub fn job(&mut self, label: impl Into<String>, args: Vec<String>) {
        let label = label.into();
        self.jobs += 1;
        self.info(format!("… {label}"));
        self.bg(move || Msg::Job { res: docker::run_job(&args), label });
    }

    fn ask(&mut self, text: String, f: impl FnOnce(&mut App) + 'static) {
        self.confirm = Some(Confirm { text, action: Box::new(f) });
    }

    // ------------------------------------------------------------ rows

    fn ctr_matches(c: &Container, f: &str) -> bool {
        f.is_empty()
            || c.name.to_lowercase().contains(f)
            || c.image.to_lowercase().contains(f)
            || c.project.to_lowercase().contains(f)
            || c.state.contains(f)
    }

    pub fn container_rows(&self) -> Vec<CRow> {
        let f = self.filter[0].to_lowercase();
        let idx = (0..self.containers.len()).filter(|&i| Self::ctr_matches(&self.containers[i], &f));
        if !self.group {
            return idx.map(CRow::Ctr).collect();
        }
        let live: HashSet<&str> = self.containers.iter().filter(|c| c.running()).map(|c| c.project.as_str()).collect();
        let mut groups: BTreeMap<(bool, bool, String), Vec<usize>> = BTreeMap::new();
        for i in idx {
            let p = &self.containers[i].project;
            groups.entry((p.is_empty(), !live.contains(p.as_str()), p.clone())).or_default().push(i);
        }
        let mut rows = Vec::new();
        for ((_, _, name), v) in groups {
            let all: Vec<&Container> = self.containers.iter().filter(|c| c.project == name).collect();
            let running = all.iter().filter(|c| c.running()).count();
            let collapsed = self.collapsed.contains(&name);
            rows.push(CRow::Project { name, total: all.len(), running });
            if !collapsed {
                rows.extend(v.into_iter().map(CRow::Ctr));
            }
        }
        rows
    }

    fn simple_rows<T>(items: &[T], f: &str, text: impl Fn(&T) -> String) -> Vec<usize> {
        let f = f.to_lowercase();
        (0..items.len()).filter(|&i| f.is_empty() || text(&items[i]).to_lowercase().contains(&f)).collect()
    }

    pub fn image_rows(&self) -> Vec<usize> {
        Self::simple_rows(&self.images, &self.filter[1], |i| format!("{} {} {}", i.repo, i.tag, i.id))
    }
    pub fn volume_rows(&self) -> Vec<usize> {
        Self::simple_rows(&self.volumes, &self.filter[2], |v| format!("{} {}", v.name, v.project))
    }
    pub fn network_rows(&self) -> Vec<usize> {
        Self::simple_rows(&self.networks, &self.filter[3], |n| format!("{} {}", n.name, n.driver))
    }

    fn row_keys(&self) -> Vec<String> {
        match self.tab {
            Tab::Containers => self
                .container_rows()
                .iter()
                .map(|r| match r {
                    CRow::Project { name, .. } => format!("p:{name}"),
                    CRow::Ctr(i) => self.containers[*i].id.clone(),
                })
                .collect(),
            Tab::Images => self.image_rows().iter().map(|&i| self.images[i].reference()).collect(),
            Tab::Volumes => self.volume_rows().iter().map(|&i| self.volumes[i].name.clone()).collect(),
            Tab::Networks => self.network_rows().iter().map(|&i| self.networks[i].id.clone()).collect(),
        }
    }

    pub fn cursor(&self) -> usize {
        let keys = self.row_keys();
        let t = self.tab.idx();
        keys.iter().position(|k| *k == self.keys[t]).unwrap_or(self.sel[t].min(keys.len().saturating_sub(1)))
    }

    fn move_sel(&mut self, d: isize) {
        let keys = self.row_keys();
        if keys.is_empty() {
            return;
        }
        let n = (self.cursor() as isize + d).clamp(0, keys.len() as isize - 1) as usize;
        let t = self.tab.idx();
        self.sel[t] = n;
        self.keys[t] = keys[n].clone();
    }

    fn cur_key(&self) -> Option<String> {
        self.row_keys().get(self.cursor()).cloned()
    }

    pub fn cur_crow(&self) -> Option<CRow> {
        self.container_rows().get(self.cursor()).cloned()
    }
    pub fn cur_container(&self) -> Option<&Container> {
        match self.cur_crow()? {
            CRow::Ctr(i) => self.containers.get(i),
            _ => None,
        }
    }
    pub fn cur_image(&self) -> Option<&Image> {
        self.image_rows().get(self.cursor()).map(|&i| &self.images[i])
    }
    pub fn cur_volume(&self) -> Option<&Volume> {
        self.volume_rows().get(self.cursor()).map(|&i| &self.volumes[i])
    }
    pub fn cur_network(&self) -> Option<&Network> {
        self.network_rows().get(self.cursor()).map(|&i| &self.networks[i])
    }

    /// Marked containers, else the project under the cursor, else the cursor container.
    fn target_containers(&self) -> Vec<Container> {
        if !self.marks[0].is_empty() {
            return self.containers.iter().filter(|c| self.marks[0].contains(&c.id)).cloned().collect();
        }
        match self.cur_crow() {
            Some(CRow::Project { name, .. }) => self.containers.iter().filter(|c| c.project == name).cloned().collect(),
            Some(CRow::Ctr(i)) => vec![self.containers[i].clone()],
            None => vec![],
        }
    }

    fn cur_project_row(&self) -> Option<String> {
        if !self.marks[0].is_empty() {
            return None;
        }
        match self.cur_crow() {
            Some(CRow::Project { name, .. }) if !name.is_empty() => Some(name),
            _ => None,
        }
    }

    fn compose(&self, project: &str, sub: &[&str]) -> Vec<String> {
        let c = self.containers.iter().find(|c| c.project == project);
        let (dir, files) = c.map(|c| (c.compose_dir.as_str(), c.compose_files.as_str())).unwrap_or(("", ""));
        docker::compose_args(project, dir, files, sub)
    }

    fn marked_or_cur(&self) -> Vec<String> {
        let t = self.tab.idx();
        if !self.marks[t].is_empty() {
            let mut v: Vec<String> = self.marks[t].iter().cloned().collect();
            v.sort();
            v
        } else {
            self.cur_key().into_iter().collect()
        }
    }

    // ------------------------------------------------------------ messages

    pub fn on_msg(&mut self, m: Msg) {
        match m {
            Msg::Containers(r) => match r {
                Ok(v) => {
                    let running: HashSet<String> = v.iter().filter(|c| c.running()).map(|c| c.short().to_string()).collect();
                    self.stats.retain(|k, _| running.contains(k));
                    self.hist.retain(|k, _| running.contains(k));
                    let ids: HashSet<&String> = v.iter().map(|c| &c.id).collect();
                    self.marks[0].retain(|k| ids.contains(k));
                    self.containers = v;
                    self.loaded[0] = true;
                    self.errors[0] = None;
                }
                Err(e) => self.errors[0] = Some(e),
            },
            Msg::Images(r) => match r {
                Ok(v) => {
                    self.images = v;
                    self.loaded[1] = true;
                    self.errors[1] = None;
                }
                Err(e) => self.errors[1] = Some(e),
            },
            Msg::Volumes(r) => match r {
                Ok(v) => {
                    self.volumes = v;
                    self.loaded[2] = true;
                    self.errors[2] = None;
                }
                Err(e) => self.errors[2] = Some(e),
            },
            Msg::Networks(r) => match r {
                Ok(v) => {
                    self.networks = v;
                    self.loaded[3] = true;
                    self.errors[3] = None;
                }
                Err(e) => self.errors[3] = Some(e),
            },
            Msg::Stat(s) => {
                self.hist.entry(s.id.clone()).or_default().push(&s);
                self.stats.insert(s.id.clone(), s);
            }
            Msg::Details { key, res } => {
                self.pending.remove(&key);
                self.details.insert(key, (Instant::now(), res));
            }
            Msg::Job { label, res } => {
                self.jobs = self.jobs.saturating_sub(1);
                match res {
                    Ok(o) if o.is_empty() => self.info(format!("✓ {label}")),
                    Ok(o) => self.info(format!("✓ {label}: {o}")),
                    Err(e) => self.error(format!("✗ {label}: {e}")),
                }
                let _ = self.refresh.send(());
            }
            Msg::Status { text, err } => {
                if let View::Files(f) = &mut self.view {
                    f.loading = false;
                }
                if err {
                    self.error(text)
                } else {
                    self.info(text)
                }
            }
            Msg::Pager { gen, title, res } => {
                self.jobs = self.jobs.saturating_sub(1);
                if gen == self.gen && matches!(self.view, View::Main | View::Monitor(_)) {
                    match res {
                        Ok(t) => self.view = View::Pager(Pager::text(title, &t)),
                        Err(e) => self.error(e),
                    }
                }
            }
            Msg::Dir { gen, path, fallback, res } => {
                let tx = self.tx.clone();
                let View::Files(f) = &mut self.view else { return };
                if f.gen != gen {
                    return;
                }
                match res {
                    Ok(es) => f.set_entries(path, es),
                    Err(_) if fallback => f.open_file(&tx, path),
                    Err(e) => {
                        f.loading = false;
                        let no_ls = e.contains("executable file not found") || e.contains("\"ls\"");
                        if let (true, true, Some(id)) = (no_ls, f.entries.is_empty(), f.export_fallback.clone()) {
                            f.loading = true;
                            f.err = None;
                            let gen = f.gen;
                            self.msg = Some(("no `ls` in container — exporting its filesystem…".into(), false, Instant::now()));
                            self.bg(move || Msg::Tree { gen, res: tree::from_export(&id), cref: CRef::Container(id) });
                        } else {
                            f.err = Some(e);
                        }
                    }
                }
            }
            Msg::File { gen, path, res } => {
                let View::Files(f) = &mut self.view else { return };
                if f.gen != gen {
                    return;
                }
                f.loading = false;
                match res {
                    Ok((data, total)) => f.preview = Some(Pager::file(&path, &data, total)),
                    Err(e) => self.msg = Some((format!("{path}: {e}"), true, Instant::now())),
                }
            }
            Msg::Tree { gen, res, cref } => {
                let tx = self.tx.clone();
                let View::Files(f) = &mut self.view else { return };
                if f.gen != gen {
                    return;
                }
                match res {
                    Ok(t) => f.set_tree(&tx, t, cref),
                    Err(e) => {
                        f.loading = false;
                        f.err = Some(e);
                    }
                }
            }
            Msg::Helper { gen, res } => {
                let tx = self.tx.clone();
                match res {
                    Ok(id) => {
                        self.helpers.insert(id.clone());
                        match &mut self.view {
                            View::Files(f) if f.gen == gen => {
                                f.helper = Some(id.clone());
                                f.src = Source::Exec { id };
                                let root = f.root.clone();
                                f.load(&tx, root, false);
                            }
                            _ => self.bg(move || Msg::Status { text: docker::run(["rm", "-fv", &id]).err().unwrap_or_default(), err: false }),
                        }
                    }
                    Err(e) => {
                        if let View::Files(f) = &mut self.view {
                            f.loading = false;
                            f.err = Some(format!("cannot start helper container ({}): {e}", docker::helper_image()));
                        }
                    }
                }
            }
            Msg::Tmp { gen, id } => {
                self.helpers.insert(id.clone());
                if let View::Files(f) = &mut self.view {
                    if f.gen == gen {
                        if let Source::Tree { cref: CRef::Image { tmp, .. }, .. } = &mut f.src {
                            if tmp.is_none() {
                                *tmp = Some(id);
                                return;
                            }
                        }
                    }
                }
                self.bg(move || {
                    let _ = docker::run(["rm", "-fv", &id]);
                    Msg::Status { text: String::new(), err: false }
                });
            }
            Msg::Reload { gen } => {
                let tx = self.tx.clone();
                if let View::Files(f) = &mut self.view {
                    if f.gen == gen {
                        f.reload(&tx);
                    }
                }
            }
            Msg::LogLine { gen, src, line } => {
                if let View::Pager(p) = &mut self.view {
                    if p.gen == gen {
                        p.push_log(src, &line);
                    }
                }
            }
            Msg::LogEnd { gen } => {
                if let View::Pager(p) = &mut self.view {
                    if p.gen == gen {
                        p.ended += 1;
                    }
                }
            }
            Msg::Top { gen, res } => {
                if let View::Monitor(m) = &mut self.view {
                    if m.gen == gen {
                        m.top = Some(res);
                    }
                }
            }
        }
    }

    pub fn on_tick(&mut self) {
        self.tick += 1;
        if matches!(self.view, View::Main) {
            self.ensure_details();
        }
        let tx = self.tx.clone();
        if let View::Monitor(m) = &mut self.view {
            if m.last_top.is_none_or(|t| t.elapsed() > Duration::from_secs(3)) {
                m.last_top = Some(Instant::now());
                let (id, gen) = (m.id.clone(), m.gen);
                thread::spawn(move || {
                    let res = docker::run(["top", &id, "-eo", "pid,user,%cpu,%mem,rss,etime,args"])
                        .or_else(|_| docker::run(["top", &id]));
                    let _ = tx.send(Msg::Top { gen, res });
                });
            }
        }
        if self.msg.as_ref().is_some_and(|(_, err, t)| t.elapsed() > Duration::from_secs(if *err { 12 } else { 6 })) {
            self.msg = None;
        }
    }

    pub fn detail_key(&self) -> Option<(String, String)> {
        match self.tab {
            Tab::Containers => self.cur_container().map(|c| ("container".into(), c.id.clone())),
            Tab::Images => self.cur_image().map(|i| ("image".into(), i.id.clone())),
            Tab::Volumes => self.cur_volume().map(|v| ("volume".into(), v.name.clone())),
            Tab::Networks => self.cur_network().map(|n| ("network".into(), n.id.clone())),
        }
    }

    fn ensure_details(&mut self) {
        let Some((ty, id)) = self.detail_key() else { return };
        let key = format!("{ty}:{id}");
        let ttl = Duration::from_secs(if ty == "container" { 5 } else { 30 });
        let stale = self.details.get(&key).is_none_or(|(t, _)| t.elapsed() > ttl);
        if stale && !self.pending.contains(&key) {
            self.pending.insert(key.clone());
            self.bg(move || Msg::Details {
                res: docker::run(["inspect", "--type", &ty, &id])
                    .and_then(|s| serde_json::from_str::<Value>(&s).map_err(|e| e.to_string()))
                    .map(|v| v.get(0).cloned().unwrap_or(Value::Null)),
                key,
            });
        }
    }

    pub fn shutdown(&mut self) {
        let mut ids: Vec<String> = self.helpers.drain().collect();
        if let View::Files(f) = &mut self.view {
            ids.extend(f.take_owned());
        }
        ids.sort();
        ids.dedup();
        if !ids.is_empty() {
            let mut a = vec!["rm".to_string(), "-fv".into()];
            a.extend(ids);
            let _ = docker::cmd(&a).stdout(Stdio::null()).stderr(Stdio::null()).status();
        }
        self.view = View::Main;
    }

    // ------------------------------------------------------------ openers

    fn open_text(&mut self, title: String, args: Vec<String>) {
        let gen = self.next_gen();
        self.jobs += 1;
        self.bg(move || Msg::Pager { gen, title, res: docker::run(&args) });
    }

    fn open_inspect(&mut self, ty: &str, id: &str, title: String) {
        self.open_text(title, vec!["inspect".into(), "--type".into(), ty.into(), id.into()]);
    }

    fn open_logs(&mut self, targets: Vec<(String, String)>, title: String) {
        if targets.is_empty() {
            return;
        }
        let gen = self.next_gen();
        let tail: usize = std::env::var("CONTAINERBEE_TAIL")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(if targets.len() > 1 { 300 } else { 1000 });
        let mut p = Pager::logs(title, targets.iter().map(|t| t.1.clone()).collect(), gen);
        for (i, (id, _)) in targets.iter().enumerate() {
            if let Some(pid) = docker::spawn_logs(self.tx.clone(), gen, i, id, tail) {
                p.pids.push(pid);
            }
        }
        self.view = View::Pager(p);
    }

    fn open_container_files(&mut self, c: &Container) {
        let gen = self.next_gen();
        let mut f = Files::new(format!("files · {}", c.name), "/", gen);
        if c.running() {
            f.src = Source::Exec { id: c.id.clone() };
            f.export_fallback = Some(c.id.clone());
            f.load(&self.tx, "/".into(), false);
        } else {
            let id = c.id.clone();
            self.info("container is not running — exporting its filesystem…");
            self.bg(move || Msg::Tree { gen, res: tree::from_export(&id), cref: CRef::Container(id) });
        }
        self.view = View::Files(f);
    }

    fn open_image_explorer(&mut self, img: &Image) {
        let gen = self.next_gen();
        let r = img.reference();
        let f = Files::new(format!("image · {r}"), "/", gen);
        self.info(format!("reading layers of {r}…"));
        self.bg(move || Msg::Tree { gen, res: tree::from_image(&r), cref: CRef::Image { image: r, tmp: None } });
        self.view = View::Files(f);
    }

    fn open_volume_files(&mut self, name: String) {
        let gen = self.next_gen();
        let f = Files::new(format!("volume · {name}"), "/vol", gen);
        self.bg(move || Msg::Helper { gen, res: docker::start_volume_helper(&name) });
        self.view = View::Files(f);
    }

    fn open_monitor(&mut self, c: &Container) {
        if !c.running() {
            self.error(format!("{} is not running", c.name));
            return;
        }
        let gen = self.next_gen();
        self.view = View::Monitor(Monitor { id: c.id.clone(), name: c.name.clone(), gen, top: None, last_top: None });
    }

    fn shell(&mut self, c: &Container, workdir: Option<&str>) {
        if !c.running() {
            self.error(format!("{} is not running (S to start it)", c.name));
            return;
        }
        let mut a: Vec<String> = ["exec", "-it", "-e", "TERM=xterm-256color"].map(String::from).to_vec();
        if let Some(w) = workdir {
            a.extend(["-w".to_string(), w.to_string()]);
        }
        a.extend([c.id.clone(), "sh".into(), "-c".into(), docker::shell_script().into()]);
        self.suspend = Some(Suspend { args: a, pause: false });
    }

    fn yank(&mut self, text: &str) {
        let mut out = std::io::stdout();
        let _ = write!(out, "\x1b]52;c;{}\x07", docker::b64(text.as_bytes()));
        let _ = out.flush();
        self.info(format!("copied {text}"));
    }

    // ------------------------------------------------------------ keys

    pub fn on_key(&mut self, k: KeyEvent) {
        if k.kind == KeyEventKind::Release {
            return;
        }
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if self.input.is_some() {
            self.input_key(k);
            return;
        }
        if let Some(c) = self.confirm.take() {
            if matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter) {
                (c.action)(self);
            } else {
                self.info("cancelled");
            }
            return;
        }
        if self.help {
            self.help = false;
            return;
        }
        if k.code == KeyCode::Char('?') {
            self.help = true;
            return;
        }
        let mut v = std::mem::replace(&mut self.view, View::Main);
        let keep = match &mut v {
            View::Main => {
                self.main_key(k);
                return;
            }
            View::Pager(p) => self.pager_key(p, k),
            View::Files(f) => self.files_key(f, k),
            View::Monitor(m) => self.monitor_key(m, k),
        };
        if keep && matches!(self.view, View::Main) {
            self.view = v;
        }
    }

    fn start_input(&mut self, kind: InputKind, prompt: &str, buf: String) {
        self.input = Some(Input { kind, prompt: prompt.into(), buf });
    }

    fn input_key(&mut self, k: KeyEvent) {
        let Some(inp) = self.input.as_mut() else { return };
        match k.code {
            KeyCode::Esc => {
                inp.buf.clear();
                let live = matches!(inp.kind, InputKind::Filter | InputKind::FilesFilter);
                if live {
                    self.live_input();
                }
                self.input = None;
            }
            KeyCode::Enter => {
                let inp = self.input.take().unwrap();
                self.submit_input(inp);
            }
            KeyCode::Backspace => {
                inp.buf.pop();
                self.live_input();
            }
            KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                inp.buf.clear();
                self.live_input();
            }
            KeyCode::Char(c) => {
                inp.buf.push(c);
                self.live_input();
            }
            _ => {}
        }
    }

    fn live_input(&mut self) {
        let Some(inp) = &self.input else { return };
        match inp.kind {
            InputKind::Filter => {
                self.filter[self.tab.idx()] = inp.buf.clone();
            }
            InputKind::FilesFilter => {
                if let View::Files(f) = &mut self.view {
                    f.filter = inp.buf.clone();
                    f.sel = 0;
                    f.offset = 0;
                }
            }
            _ => {}
        }
    }

    fn active_pager(&mut self) -> Option<&mut Pager> {
        match &mut self.view {
            View::Pager(p) => Some(p),
            View::Files(f) => f.preview.as_mut(),
            _ => None,
        }
    }

    fn submit_input(&mut self, inp: Input) {
        match inp.kind {
            InputKind::Filter | InputKind::FilesFilter => {}
            InputKind::PagerSearch => {
                let found = self.active_pager().map(|p| p.set_search(&inp.buf));
                if found == Some(false) {
                    self.error(format!("no match for /{}", inp.buf));
                }
            }
            InputKind::PagerFilter => {
                if let Some(p) = self.active_pager() {
                    p.set_filter(&inp.buf);
                }
            }
            InputKind::Upload => {
                let local = match inp.buf.strip_prefix("~/") {
                    Some(r) => format!("{}/{r}", std::env::var("HOME").unwrap_or_default()),
                    None => inp.buf.clone(),
                };
                if local.is_empty() {
                    return;
                }
                let tx = self.tx.clone();
                if let View::Files(f) = &self.view {
                    f.upload(&tx, local);
                    self.info("uploading…");
                }
            }
            InputKind::Exec(id) => {
                if inp.buf.trim().is_empty() {
                    return;
                }
                let a = ["exec", "-it", &id, "sh", "-c", &inp.buf].map(String::from).to_vec();
                self.suspend = Some(Suspend { args: a, pause: true });
            }
        }
    }

    fn set_tab(&mut self, t: Tab) {
        self.tab = t;
    }

    // ------------------------------------------------------------ mouse

    /// The tab whose header label covers this cell, if any.
    fn tab_at(&self, col: u16, row: u16) -> Option<Tab> {
        Tab::ALL.into_iter().find(|t| {
            let (y, x0, x1) = self.tab_hit[t.idx()];
            row == y && col >= x0 && col < x1
        })
    }

    /// Returns true when the event changed something and the screen needs a redraw.
    pub fn on_mouse(&mut self, m: MouseEvent) -> bool {
        // Modals own the screen; let the keyboard dismiss them.
        if self.input.is_some() || self.confirm.is_some() || self.help {
            return false;
        }
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => match self.tab_at(m.column, m.row) {
                Some(t) => {
                    self.view = View::Main;
                    self.set_tab(t);
                    true
                }
                None => false,
            },
            // Wheel: reuse the arrow-key paths so every view scrolls as usual.
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let code = if m.kind == MouseEventKind::ScrollUp { KeyCode::Up } else { KeyCode::Down };
                for _ in 0..3 {
                    self.on_key(KeyEvent::new(code, KeyModifiers::NONE));
                }
                true
            }
            _ => false,
        }
    }

    fn main_key(&mut self, k: KeyEvent) {
        let t = self.tab.idx();
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Tab => self.set_tab(Tab::ALL[(t + 1) % 4]),
            KeyCode::BackTab => self.set_tab(Tab::ALL[(t + 3) % 4]),
            KeyCode::Char(c @ '1'..='4') => self.set_tab(Tab::ALL[c as usize - '1' as usize]),
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            KeyCode::PageDown => self.move_sel(15),
            KeyCode::PageUp => self.move_sel(-15),
            KeyCode::Char('d') if ctrl => self.move_sel(15),
            KeyCode::Char('u') if ctrl => self.move_sel(-15),
            KeyCode::Home | KeyCode::Char('g') => self.move_sel(-100_000),
            KeyCode::End | KeyCode::Char('G') => self.move_sel(100_000),
            KeyCode::Char('/') => self.start_input(InputKind::Filter, "filter: ", self.filter[t].clone()),
            KeyCode::Esc => {
                if !self.marks[t].is_empty() {
                    self.marks[t].clear();
                } else {
                    self.filter[t].clear();
                }
            }
            KeyCode::F(5) => {
                let _ = self.refresh.send(());
                self.info("refreshing…");
            }
            KeyCode::Char('r') if ctrl => {
                let _ = self.refresh.send(());
                self.info("refreshing…");
            }
            KeyCode::Char(' ') => {
                self.toggle_mark();
                self.move_sel(1);
            }
            KeyCode::Char('a') => self.mark_all(),
            _ => match self.tab {
                Tab::Containers => self.ctr_key(k),
                Tab::Images => self.img_key(k),
                Tab::Volumes => self.vol_key(k),
                Tab::Networks => self.net_key(k),
            },
        }
    }

    fn toggle_mark(&mut self) {
        let t = self.tab.idx();
        if self.tab == Tab::Containers {
            if let Some(CRow::Project { name, .. }) = self.cur_crow() {
                let ids: Vec<String> = self.containers.iter().filter(|c| c.project == name).map(|c| c.id.clone()).collect();
                let all = ids.iter().all(|i| self.marks[0].contains(i));
                for i in ids {
                    if all {
                        self.marks[0].remove(&i);
                    } else {
                        self.marks[0].insert(i);
                    }
                }
                return;
            }
        }
        if let Some(key) = self.cur_key() {
            if !self.marks[t].remove(&key) {
                self.marks[t].insert(key);
            }
        }
    }

    fn mark_all(&mut self) {
        let t = self.tab.idx();
        let keys: Vec<String> = match self.tab {
            Tab::Containers => {
                let f = self.filter[0].to_lowercase();
                self.containers.iter().filter(|c| Self::ctr_matches(c, &f)).map(|c| c.id.clone()).collect()
            }
            _ => self.row_keys(),
        };
        if keys.iter().all(|k| self.marks[t].contains(k)) {
            self.marks[t].clear();
        } else {
            self.marks[t].extend(keys);
        }
    }

    fn ctr_verb(&mut self, verb: &str) {
        if let Some(p) = self.cur_project_row() {
            let args = self.compose(&p, &[verb]);
            self.job(format!("compose {verb} {p}"), args);
            return;
        }
        let t = self.target_containers();
        if t.is_empty() {
            return;
        }
        let mut args = vec![verb.to_string()];
        args.extend(t.iter().map(|c| c.id.clone()));
        self.job(format!("{verb} {}", label_of(&t)), args);
    }

    fn ctr_key(&mut self, k: KeyEvent) {
        let cur = self.cur_container().cloned();
        match k.code {
            KeyCode::Enter => match (self.cur_crow(), &cur) {
                (Some(CRow::Project { name, .. }), _) => {
                    if !self.collapsed.remove(&name) {
                        self.collapsed.insert(name);
                    }
                }
                (_, Some(c)) => self.open_inspect("container", &c.id, format!("inspect · {}", c.name)),
                _ => {}
            },
            KeyCode::Left | KeyCode::Char('h') => {
                let p = match self.cur_crow() {
                    Some(CRow::Project { name, .. }) => Some(name),
                    _ => cur.as_ref().map(|c| c.project.clone()),
                };
                if let (true, Some(p)) = (self.group, p) {
                    self.keys[0] = format!("p:{p}");
                    self.collapsed.insert(p);
                }
            }
            KeyCode::Right => {
                if let Some(CRow::Project { name, .. }) = self.cur_crow() {
                    self.collapsed.remove(&name);
                }
            }
            KeyCode::Char('c') => {
                self.group = !self.group;
                self.info(if self.group { "grouped by compose project" } else { "flat list" });
            }
            KeyCode::Char('i') => {
                if let Some(c) = &cur {
                    self.open_inspect("container", &c.id, format!("inspect · {}", c.name));
                } else if let Some(p) = self.cur_project_row() {
                    let args = self.compose(&p, &["ps", "-a"]);
                    self.open_text(format!("compose ps · {p}"), args);
                }
            }
            KeyCode::Char('l') => {
                let t = self.target_containers();
                let title = match (self.cur_project_row(), t.as_slice()) {
                    (Some(p), _) => format!("logs · {p}"),
                    (_, [c]) => format!("logs · {}", c.name),
                    _ => format!("logs · {} containers", t.len()),
                };
                let short = t.len() > 1;
                let targets = t
                    .iter()
                    .map(|c| (c.id.clone(), if short && !c.service.is_empty() { c.service.clone() } else { c.name.clone() }))
                    .collect();
                self.open_logs(targets, title);
            }
            KeyCode::Char('e') => {
                if let Some(c) = &cur {
                    self.shell(c, None);
                }
            }
            KeyCode::Char('E') | KeyCode::Char(':') => {
                if let Some(c) = &cur {
                    if c.running() {
                        self.start_input(InputKind::Exec(c.id.clone()), &format!("exec in {} $ ", c.name), String::new());
                    } else {
                        self.error(format!("{} is not running", c.name));
                    }
                }
            }
            KeyCode::Char('f') => {
                if let Some(c) = &cur {
                    self.open_container_files(c);
                }
            }
            KeyCode::Char('m') => {
                if let Some(c) = &cur {
                    self.open_monitor(c);
                }
            }
            KeyCode::Char('S') => self.ctr_verb("start"),
            KeyCode::Char('s') => self.ctr_verb("stop"),
            KeyCode::Char('r') => self.ctr_verb("restart"),
            KeyCode::Char('K') => self.ctr_verb("kill"),
            KeyCode::Char('p') => {
                if let Some(p) = self.cur_project_row() {
                    let any_running = self.containers.iter().any(|c| c.project == p && c.running());
                    self.ctr_verb(if any_running { "pause" } else { "unpause" });
                    return;
                }
                let t = self.target_containers();
                let (paused, other): (Vec<Container>, Vec<Container>) = t.into_iter().partition(|c| c.state == "paused");
                if !paused.is_empty() {
                    let mut a = vec!["unpause".to_string()];
                    a.extend(paused.iter().map(|c| c.id.clone()));
                    self.job(format!("unpause {}", label_of(&paused)), a);
                }
                let other: Vec<Container> = other.into_iter().filter(|c| c.running()).collect();
                if !other.is_empty() {
                    let mut a = vec!["pause".to_string()];
                    a.extend(other.iter().map(|c| c.id.clone()));
                    self.job(format!("pause {}", label_of(&other)), a);
                }
            }
            KeyCode::Char('D') => {
                if let Some(p) = self.cur_project_row() {
                    let args = self.compose(&p, &["down"]);
                    self.ask(format!("compose down '{p}'?  Stops and removes its containers and networks."), move |a| {
                        a.job(format!("compose down {p}"), args)
                    });
                    return;
                }
                let t = self.target_containers();
                if t.is_empty() {
                    return;
                }
                let label = label_of(&t);
                let mut args = vec!["rm".to_string(), "-f".into()];
                args.extend(t.iter().map(|c| c.id.clone()));
                self.ask(format!("Force-remove {label}?"), move |a| {
                    a.marks[0].clear();
                    a.job(format!("rm {label}"), args)
                });
            }
            KeyCode::Char('U') => {
                let p = self.cur_project_row().or_else(|| cur.as_ref().map(|c| c.project.clone()).filter(|p| !p.is_empty()));
                match p {
                    Some(p) => {
                        let args = self.compose(&p, &["up", "-d"]);
                        self.job(format!("compose up -d {p}"), args);
                    }
                    None => self.error("not a compose project"),
                }
            }
            KeyCode::Char('P') => self.ask("Remove all stopped containers?".into(), |a| {
                a.job("container prune", ["container", "prune", "-f"].map(String::from).to_vec())
            }),
            KeyCode::Char('o') => {
                if let Some(c) = &cur {
                    match docker::first_host_port(&c.ports) {
                        Some(port) => {
                            let url = format!("http://localhost:{port}");
                            let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
                            let _ = std::process::Command::new(opener)
                                .arg(&url)
                                .stdin(Stdio::null())
                                .stdout(Stdio::null())
                                .stderr(Stdio::null())
                                .spawn();
                            self.info(format!("opening {url}"));
                        }
                        None => self.error("no published ports"),
                    }
                }
            }
            KeyCode::Char('y') => {
                if let Some(c) = &cur {
                    let id = c.id.clone();
                    self.yank(&id);
                }
            }
            _ => {}
        }
    }

    fn img_key(&mut self, k: KeyEvent) {
        let Some(img) = self.cur_image().cloned() else { return };
        let r = img.reference();
        match k.code {
            KeyCode::Enter | KeyCode::Char('i') => self.open_inspect("image", &img.id, format!("inspect · {r}")),
            KeyCode::Char('x') | KeyCode::Char('f') => self.open_image_explorer(&img),
            KeyCode::Char('h') => self.open_text(
                format!("history · {r}"),
                ["history", "--no-trunc", "--format", "{{.Size}}\t{{.CreatedSince}}\t{{.CreatedBy}}", &r].map(String::from).to_vec(),
            ),
            KeyCode::Char('e') => {
                let a = ["run", "--rm", "-it", "-e", "TERM=xterm-256color", "--entrypoint", "sh", &r, "-c", docker::shell_script()]
                    .map(String::from)
                    .to_vec();
                self.suspend = Some(Suspend { args: a, pause: false });
            }
            KeyCode::Char('u') => {
                if img.dangling() {
                    self.error("dangling image has no tag to pull");
                } else {
                    self.job(format!("pull {r}"), vec!["pull".into(), r]);
                }
            }
            KeyCode::Char('D') => {
                let refs = self.marked_or_cur();
                let label = if refs.len() == 1 { refs[0].clone() } else { format!("{} images", refs.len()) };
                self.ask(format!("Remove {label}?"), move |a| {
                    a.marks[1].clear();
                    let mut args = vec!["rmi".to_string()];
                    args.extend(refs);
                    a.job(format!("rmi {label}"), args)
                });
            }
            KeyCode::Char('P') => self.ask("Remove dangling images?".into(), |a| {
                a.job("image prune", ["image", "prune", "-f"].map(String::from).to_vec())
            }),
            KeyCode::Char('y') => self.yank(&img.id),
            _ => {}
        }
    }

    fn vol_key(&mut self, k: KeyEvent) {
        let Some(v) = self.cur_volume().cloned() else { return };
        match k.code {
            KeyCode::Enter | KeyCode::Char('i') => self.open_inspect("volume", &v.name, format!("inspect · {}", v.name)),
            KeyCode::Char('f') | KeyCode::Char('x') => self.open_volume_files(v.name.clone()),
            KeyCode::Char('D') => {
                let names = self.marked_or_cur();
                let label = if names.len() == 1 { names[0].clone() } else { format!("{} volumes", names.len()) };
                self.ask(format!("Remove volume {label}?  Its data will be lost."), move |a| {
                    a.marks[2].clear();
                    let mut args = vec!["volume".to_string(), "rm".into()];
                    args.extend(names);
                    a.job(format!("volume rm {label}"), args)
                });
            }
            KeyCode::Char('P') => self.ask("Remove all unused anonymous volumes?".into(), |a| {
                a.job("volume prune", ["volume", "prune", "-f"].map(String::from).to_vec())
            }),
            KeyCode::Char('y') => self.yank(&v.name),
            _ => {}
        }
    }

    fn net_key(&mut self, k: KeyEvent) {
        let Some(n) = self.cur_network().cloned() else { return };
        match k.code {
            KeyCode::Enter | KeyCode::Char('i') => self.open_inspect("network", &n.id, format!("inspect · {}", n.name)),
            KeyCode::Char('D') => {
                let ids = self.marked_or_cur();
                let label = if ids.len() == 1 { n.name.clone() } else { format!("{} networks", ids.len()) };
                self.ask(format!("Remove network {label}?"), move |a| {
                    a.marks[3].clear();
                    let mut args = vec!["network".to_string(), "rm".into()];
                    args.extend(ids);
                    a.job(format!("network rm {label}"), args)
                });
            }
            KeyCode::Char('P') => self.ask("Remove all unused networks?".into(), |a| {
                a.job("network prune", ["network", "prune", "-f"].map(String::from).to_vec())
            }),
            KeyCode::Char('y') => self.yank(&n.id),
            _ => {}
        }
    }

    fn pager_key(&mut self, p: &mut Pager, k: KeyEvent) -> bool {
        let h = p.height.max(1) as isize;
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Esc => {
                if p.search.is_some() {
                    p.set_search("");
                } else if p.filter.is_some() {
                    p.set_filter("");
                } else {
                    return false;
                }
            }
            KeyCode::Char('q') => return false,
            KeyCode::Char('d') if ctrl => p.scroll(h / 2),
            KeyCode::Char('u') if ctrl => p.scroll(-h / 2),
            KeyCode::Char('f') if ctrl => p.scroll(h),
            KeyCode::Char('b') if ctrl => p.scroll(-h),
            KeyCode::Down | KeyCode::Char('j') => p.scroll(1),
            KeyCode::Up | KeyCode::Char('k') => p.scroll(-1),
            KeyCode::PageDown | KeyCode::Char(' ') => p.scroll(h),
            KeyCode::PageUp => p.scroll(-h),
            KeyCode::Home | KeyCode::Char('g') => p.to_top(),
            KeyCode::End | KeyCode::Char('G') => p.to_bottom(),
            KeyCode::Left => p.hscroll = p.hscroll.saturating_sub(8),
            KeyCode::Right => p.hscroll += 8,
            KeyCode::Char('/') => self.start_input(InputKind::PagerSearch, "search: ", p.search_src.clone()),
            KeyCode::Char('f') | KeyCode::Char('&') => self.start_input(InputKind::PagerFilter, "filter lines: ", p.filter_src.clone()),
            KeyCode::Char('n') => {
                if !p.next_match(1) {
                    self.info("no more matches");
                }
            }
            KeyCode::Char('N') => {
                if !p.next_match(-1) {
                    self.info("no earlier matches");
                }
            }
            KeyCode::Char('F') if p.is_log => {
                p.follow = !p.follow;
                if p.follow {
                    p.to_bottom();
                }
            }
            KeyCode::Char('t') if p.is_log => p.show_ts = !p.show_ts,
            KeyCode::Char('w') => p.wrap = !p.wrap,
            KeyCode::Char('c') if p.is_log => p.clear(),
            _ => {}
        }
        true
    }

    fn files_key(&mut self, f: &mut Files, k: KeyEvent) -> bool {
        let tx = self.tx.clone();
        if let Some(p) = &mut f.preview {
            if k.code == KeyCode::Char('d') && !k.modifiers.contains(KeyModifiers::CONTROL) {
                if f.download(&tx) {
                    self.info("downloading…");
                }
                return true;
            }
            if !self.pager_key(p, k) {
                f.preview = None;
            }
            return true;
        }
        let n = f.visible().len();
        if f.layer_focus {
            let layers = f.tree().map_or(0, |t| t.layers.len());
            let old = f.layer_sel;
            match k.code {
                KeyCode::Esc | KeyCode::Char('q') => return false,
                KeyCode::Down | KeyCode::Char('j') => f.layer_sel = (f.layer_sel + 1).min(layers),
                KeyCode::Up | KeyCode::Char('k') => f.layer_sel = f.layer_sel.saturating_sub(1),
                KeyCode::Home | KeyCode::Char('g') => f.layer_sel = 0,
                KeyCode::End | KeyCode::Char('G') => f.layer_sel = layers,
                KeyCode::Tab | KeyCode::Right | KeyCode::Enter | KeyCode::Char('l') => f.layer_focus = false,
                _ => {}
            }
            if f.layer_sel != old {
                f.reload(&tx);
            }
            return true;
        }
        match k.code {
            KeyCode::Esc if !f.filter.is_empty() => {
                f.filter.clear();
                f.sel = 0;
            }
            KeyCode::Esc | KeyCode::Char('q') => return false,
            KeyCode::Down | KeyCode::Char('j') => f.sel = (f.sel + 1).min(n.saturating_sub(1)),
            KeyCode::Up | KeyCode::Char('k') => f.sel = f.sel.saturating_sub(1),
            KeyCode::PageDown => f.sel = (f.sel + 15).min(n.saturating_sub(1)),
            KeyCode::PageUp => f.sel = f.sel.saturating_sub(15),
            KeyCode::Home | KeyCode::Char('g') => f.sel = 0,
            KeyCode::End | KeyCode::Char('G') => f.sel = n.saturating_sub(1),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                if !f.loading {
                    f.filter.clear();
                    f.enter(&tx);
                }
            }
            KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => {
                if !f.loading {
                    f.filter.clear();
                    f.up(&tx);
                }
            }
            KeyCode::Char('~') => {
                let root = f.root.clone();
                f.load(&tx, root, false);
            }
            KeyCode::Char('/') => self.start_input(InputKind::FilesFilter, "find: ", f.filter.clone()),
            KeyCode::Char('d') => {
                if f.download(&tx) {
                    self.info(format!("downloading to {}…", crate::files::download_dir()));
                }
            }
            KeyCode::Char('u') => {
                if f.is_image() {
                    self.error("images are read-only");
                } else {
                    self.start_input(InputKind::Upload, &format!("upload local path → {}: ", f.cwd), String::new());
                }
            }
            KeyCode::Char('r') => f.reload(&tx),
            KeyCode::Char('e') => match f.is_exec() {
                Some(id) => {
                    let c = Container { id: id.to_string(), name: f.title.clone(), state: "running".into(), ..Default::default() };
                    let cwd = f.cwd.clone();
                    self.shell(&c, Some(&cwd));
                }
                None => self.error("shell needs a running container"),
            },
            KeyCode::Tab => {
                if f.is_image() {
                    f.layer_focus = true;
                }
            }
            _ => {}
        }
        true
    }

    fn monitor_key(&mut self, m: &mut Monitor, k: KeyEvent) -> bool {
        let c = self.containers.iter().find(|c| c.id == m.id).cloned();
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return false,
            KeyCode::Char('l') => self.open_logs(vec![(m.id.clone(), m.name.clone())], format!("logs · {}", m.name)),
            KeyCode::Char('e') => {
                if let Some(c) = c {
                    self.shell(&c, None);
                }
            }
            KeyCode::Char('f') => {
                if let Some(c) = c {
                    self.open_container_files(&c);
                }
            }
            KeyCode::Char('i') => self.open_inspect("container", &m.id.clone(), format!("inspect · {}", m.name)),
            _ => {}
        }
        true
    }
}
