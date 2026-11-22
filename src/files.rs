//! File browser over three backends:
//! live `docker exec ls` (running containers, volume helpers) and in-memory
//! tar trees (images with per-layer diff, stopped/distroless containers).

use std::sync::{mpsc::Sender, Arc};
use std::thread;

use crate::app::Msg;
use crate::docker;
use crate::pager::Pager;
use crate::tree::{self, Change, Kind, Tree};

#[derive(Clone, Debug)]
pub struct FEntry {
    pub name: String,
    pub kind: Kind,
    pub size: u64,
    pub mode: String,
    pub owner: String,
    pub mtime: String,
    pub link: Option<String>,
    pub change: Option<Change>,
    pub layer: Option<usize>,
}

pub enum CRef {
    Container(String),
    Image { image: String, tmp: Option<String> },
}

pub enum Source {
    Pending,
    Exec { id: String },
    Tree { tree: Arc<Tree>, cref: CRef },
}

pub struct Files {
    pub title: String,
    pub src: Source,
    pub cwd: String,
    pub root: String,
    pub entries: Vec<FEntry>,
    pub sel: usize,
    pub offset: usize,
    pub loading: bool,
    pub err: Option<String>,
    pub gen: u64,
    pub helper: Option<String>,
    pub filter: String,
    pub layer_focus: bool,
    pub layer_sel: usize,
    pub preview: Option<Pager>,
    pub reselect: Option<String>,
    /// container id to fall back to `docker export` if exec ls is unavailable
    pub export_fallback: Option<String>,
}

pub fn mode_string(kind: Kind, mode: u32) -> String {
    let t = match kind {
        Kind::Dir => 'd',
        Kind::Link => 'l',
        Kind::File => '-',
        Kind::Other => '?',
    };
    let mut s = String::from(t);
    for shift in [6, 3, 0] {
        let m = (mode >> shift) & 7;
        s.push(if m & 4 != 0 { 'r' } else { '-' });
        s.push(if m & 2 != 0 { 'w' } else { '-' });
        s.push(if m & 1 != 0 { 'x' } else { '-' });
    }
    s
}

fn tree_entries(t: &Tree, layer_sel: usize, path: &str) -> Option<Vec<FEntry>> {
    let map = if layer_sel == 0 { &t.merged } else { &t.layers.get(layer_sel - 1)?.files };
    let key = tree::norm(path);
    if !key.is_empty() && !matches!(map.get(&key), Some(n) if n.kind == Kind::Dir) {
        return None;
    }
    Some(
        tree::list(map, &key)
            .into_iter()
            .map(|(name, n)| FEntry {
                name: name.to_string(),
                kind: n.kind,
                size: n.size,
                mode: mode_string(n.kind, n.mode),
                owner: format!("{}:{}", n.uid, n.gid),
                mtime: docker::fmt_unix(n.mtime),
                link: n.link.clone(),
                change: (layer_sel > 0).then_some(n.change),
                layer: Some(n.layer),
            })
            .collect(),
    )
}

pub fn download_dir() -> String {
    std::env::var("CONTAINERBEE_DOWNLOADS")
        .ok()
        .or_else(|| std::env::current_dir().ok().map(|p| p.to_string_lossy().into_owned()))
        .unwrap_or_else(|| ".".into())
}

impl Files {
    pub fn new(title: String, root: &str, gen: u64) -> Self {
        Files {
            title,
            src: Source::Pending,
            cwd: root.to_string(),
            root: root.to_string(),
            entries: Vec::new(),
            sel: 0,
            offset: 0,
            loading: true,
            err: None,
            gen,
            helper: None,
            filter: String::new(),
            layer_focus: false,
            layer_sel: 0,
            preview: None,
            reselect: None,
            export_fallback: None,
        }
    }

    pub fn tree(&self) -> Option<&Arc<Tree>> {
        match &self.src {
            Source::Tree { tree, .. } => Some(tree),
            _ => None,
        }
    }

    pub fn is_image(&self) -> bool {
        matches!(&self.src, Source::Tree { cref: CRef::Image { .. }, .. })
    }

    pub fn is_exec(&self) -> Option<&str> {
        match &self.src {
            Source::Exec { id } => Some(id),
            _ => None,
        }
    }

    pub fn visible(&self) -> Vec<usize> {
        let f = self.filter.to_lowercase();
        (0..self.entries.len()).filter(|&i| f.is_empty() || self.entries[i].name.to_lowercase().contains(&f)).collect()
    }

    pub fn current(&self) -> Option<&FEntry> {
        self.visible().get(self.sel).map(|&i| &self.entries[i])
    }

    pub fn path_of(&self, name: &str) -> String {
        if self.cwd == "/" {
            format!("/{name}")
        } else {
            format!("{}/{name}", self.cwd)
        }
    }

    pub fn set_entries(&mut self, path: String, mut es: Vec<FEntry>) {
        es.sort_by(|a, b| (a.kind != Kind::Dir, a.name.to_lowercase()).cmp(&(b.kind != Kind::Dir, b.name.to_lowercase())));
        self.cwd = path;
        self.entries = es;
        self.loading = false;
        self.err = None;
        self.offset = 0;
        self.sel = 0;
        if let Some(name) = self.reselect.take() {
            if let Some(p) = self.visible().iter().position(|&i| self.entries[i].name == name) {
                self.sel = p;
            }
        }
    }

    pub fn set_tree(&mut self, tx: &Sender<Msg>, tree: Tree, cref: CRef) {
        self.src = Source::Tree { tree: Arc::new(tree), cref };
        let cwd = self.cwd.clone();
        self.load(tx, cwd, false);
    }

    pub fn load(&mut self, tx: &Sender<Msg>, path: String, fallback: bool) {
        let path = if path.is_empty() { "/".to_string() } else { path };
        if let Some(id) = self.is_exec().map(str::to_string) {
            self.loading = true;
            let (tx, gen) = (tx.clone(), self.gen);
            thread::spawn(move || {
                let res = docker::ls(&id, &path);
                let _ = tx.send(Msg::Dir { gen, path, fallback, res });
            });
            return;
        }
        let Some(t) = self.tree().cloned() else { return };
        match tree_entries(&t, self.layer_sel, &path) {
            Some(es) => self.set_entries(path, es),
            None if path != "/" => self.load(tx, "/".into(), false),
            None => self.set_entries(path, Vec::new()),
        }
    }

    pub fn reload(&mut self, tx: &Sender<Msg>) {
        self.reselect = self.current().map(|e| e.name.clone());
        let cwd = self.cwd.clone();
        self.load(tx, cwd, false);
    }

    pub fn enter(&mut self, tx: &Sender<Msg>) {
        let Some(e) = self.current().cloned() else { return };
        let path = self.path_of(&e.name);
        match e.kind {
            Kind::Dir => self.load(tx, path, false),
            Kind::Link => match self.tree().cloned() {
                Some(t) => match t.follow(&path) {
                    Some((k, Kind::Dir)) => self.load(tx, format!("/{k}"), false),
                    _ => self.open_file(tx, path),
                },
                None => self.load(tx, path, true),
            },
            Kind::File | Kind::Other => self.open_file(tx, path),
        }
    }

    pub fn up(&mut self, tx: &Sender<Msg>) {
        if self.cwd == self.root || self.cwd == "/" {
            return;
        }
        let (parent, name) = tree::split(&self.cwd);
        self.reselect = Some(name.to_string());
        let parent = if parent.is_empty() { "/".to_string() } else { parent.to_string() };
        self.load(tx, parent, false);
    }

    /// Run `op` against a container id that can serve `docker cp`
    /// (creating a throwaway container for images on first use).
    fn with_ref(&self, tx: &Sender<Msg>, op: impl FnOnce(&str, &Sender<Msg>) + Send + 'static) {
        let (id, image) = match &self.src {
            Source::Exec { id } => (Some(id.clone()), None),
            Source::Tree { cref: CRef::Container(id), .. } => (Some(id.clone()), None),
            Source::Tree { cref: CRef::Image { image, tmp }, .. } => (tmp.clone(), Some(image.clone())),
            Source::Pending => return,
        };
        let (tx, gen) = (tx.clone(), self.gen);
        thread::spawn(move || {
            let id = match (id, image) {
                (Some(id), _) => id,
                (None, Some(img)) => match docker::create_tmp(&img) {
                    Ok(id) => {
                        let _ = tx.send(Msg::Tmp { gen, id: id.clone() });
                        id
                    }
                    Err(e) => {
                        let _ = tx.send(Msg::Status { text: format!("cannot create temp container: {e}"), err: true });
                        return;
                    }
                },
                _ => return,
            };
            op(&id, &tx);
        });
    }

    pub fn open_file(&mut self, tx: &Sender<Msg>, path: String) {
        self.loading = true;
        let gen = self.gen;
        self.with_ref(tx, move |id, tx| {
            let res = docker::cp_read(id, &path, 512 * 1024);
            let _ = tx.send(Msg::File { gen, path, res });
        });
    }

    pub fn download(&self, tx: &Sender<Msg>) -> bool {
        let Some(e) = self.current() else { return false };
        let path = self.path_of(&e.name);
        let dest = download_dir();
        self.with_ref(tx, move |id, tx| {
            let r = docker::run(["cp", "-L", &format!("{id}:{path}"), &dest]);
            let text = match &r {
                Ok(_) => format!("downloaded {path} → {dest}"),
                Err(e) => format!("download failed: {e}"),
            };
            let _ = tx.send(Msg::Status { text, err: r.is_err() });
        });
        true
    }

    pub fn upload(&self, tx: &Sender<Msg>, local: String) {
        let dest = self.cwd.clone();
        let gen = self.gen;
        self.with_ref(tx, move |id, tx| {
            let r = docker::run(["cp", &local, &format!("{id}:{dest}/")]);
            let text = match &r {
                Ok(_) => format!("uploaded {local} → {dest}"),
                Err(e) => format!("upload failed: {e}"),
            };
            let _ = tx.send(Msg::Status { text, err: r.is_err() });
            if r.is_ok() {
                let _ = tx.send(Msg::Reload { gen });
            }
        });
    }

    /// Helper/temporary containers owned by this view.
    pub fn take_owned(&mut self) -> Vec<String> {
        let mut v: Vec<String> = self.helper.take().into_iter().collect();
        if let Source::Tree { cref: CRef::Image { tmp, .. }, .. } = &mut self.src {
            v.extend(tmp.take());
        }
        v
    }
}

impl Drop for Files {
    fn drop(&mut self) {
        let ids = self.take_owned();
        if !ids.is_empty() {
            thread::spawn(move || {
                let mut a = vec!["rm".to_string(), "-fv".into()];
                a.extend(ids);
                let _ = docker::run(&a);
            });
        }
    }
}
