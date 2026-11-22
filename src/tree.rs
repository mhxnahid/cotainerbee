//! In-memory filesystem trees built from tar streams:
//! `docker save` (per-layer, dive-style) and `docker export` (container fs).

use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::io::{Cursor, Read};
use std::ops::Bound;
use std::process::{Child, Stdio};

use crate::docker;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    File,
    Dir,
    Link,
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Change {
    Added,
    Modified,
    Removed,
    None,
}

#[derive(Clone, Debug)]
pub struct Node {
    pub kind: Kind,
    pub size: u64,
    pub mode: u32,
    pub uid: u64,
    pub gid: u64,
    pub mtime: u64,
    pub link: Option<String>,
    pub layer: usize,
    pub change: Change,
}

impl Node {
    fn dir(layer: usize) -> Node {
        Node { kind: Kind::Dir, size: 0, mode: 0o755, uid: 0, gid: 0, mtime: 0, link: None, layer, change: Change::None }
    }
}

pub struct Layer {
    pub created_by: String,
    pub digest: String,
    pub size: u64,
    pub count: usize,
    pub files: BTreeMap<String, Node>,
}

pub struct Tree {
    pub merged: BTreeMap<String, Node>,
    pub layers: Vec<Layer>,
    pub total: u64,
    pub files: usize,
}

pub struct Raw {
    path: String,
    node: Node,
}

fn es<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

/// Normalise to a relative key: no leading "./" or "/", no trailing "/". Root is "".
pub fn norm(p: &str) -> String {
    let mut s = p;
    loop {
        if let Some(r) = s.strip_prefix("./") {
            s = r
        } else if let Some(r) = s.strip_prefix('/') {
            s = r
        } else {
            break;
        }
    }
    let s = s.trim_end_matches('/');
    if s == "." {
        String::new()
    } else {
        s.to_string()
    }
}

pub fn split(p: &str) -> (&str, &str) {
    match p.rfind('/') {
        Some(i) => (&p[..i], &p[i + 1..]),
        None => ("", p),
    }
}

fn join(d: &str, n: &str) -> String {
    if d.is_empty() {
        n.to_string()
    } else {
        format!("{d}/{n}")
    }
}

/// Resolve a (possibly relative) link target against `dir` (both keys).
pub fn resolve(dir: &str, target: &str) -> String {
    if target.starts_with('/') {
        return norm(target);
    }
    let mut parts: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    for c in target.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            x => parts.push(x),
        }
    }
    parts.join("/")
}

pub fn parse_layer<R: Read>(r: R) -> Result<Vec<Raw>, String> {
    let mut ar = tar::Archive::new(r);
    let mut v = Vec::new();
    for e in ar.entries().map_err(es)? {
        let e = e.map_err(es)?;
        let path = norm(&e.path().map_err(es)?.to_string_lossy());
        if path.is_empty() {
            continue;
        }
        let h = e.header();
        let et = h.entry_type();
        let kind = if et.is_dir() {
            Kind::Dir
        } else if et.is_symlink() {
            Kind::Link
        } else if et.is_file() || et.is_hard_link() || et == tar::EntryType::Continuous {
            Kind::File
        } else {
            Kind::Other
        };
        let link = if kind == Kind::Link || et.is_hard_link() {
            e.link_name().ok().flatten().map(|l| l.to_string_lossy().into_owned())
        } else {
            None
        };
        v.push(Raw {
            path,
            node: Node {
                kind,
                size: if kind == Kind::File { e.size() } else { 0 },
                mode: h.mode().unwrap_or(0),
                uid: h.uid().unwrap_or(0),
                gid: h.gid().unwrap_or(0),
                mtime: h.mtime().unwrap_or(0),
                link,
                layer: 0,
                change: Change::Added,
            },
        });
    }
    Ok(v)
}

fn ensure_parents(m: &mut BTreeMap<String, Node>, path: &str, layer: usize) {
    let mut p = path;
    while let Some(i) = p.rfind('/') {
        p = &p[..i];
        if m.contains_key(p) {
            break;
        }
        m.insert(p.to_string(), Node::dir(layer));
    }
}

/// Remove everything below `dir` that came from a layer older than `before`.
fn remove_under(m: &mut BTreeMap<String, Node>, dir: &str, before: usize) {
    let prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };
    let keys: Vec<String> = m
        .range::<str, _>((Bound::Included(prefix.as_str()), Bound::Unbounded))
        .take_while(|(k, _)| k.starts_with(&prefix))
        .filter(|(_, n)| n.layer < before)
        .map(|(k, _)| k.clone())
        .collect();
    for k in keys {
        m.remove(&k);
    }
}

/// Direct children of `dir` (a key) in `m`.
pub fn list<'a>(m: &'a BTreeMap<String, Node>, dir: &str) -> Vec<(&'a str, &'a Node)> {
    let prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };
    m.range::<str, _>((Bound::Included(prefix.as_str()), Bound::Unbounded))
        .take_while(|(k, _)| k.starts_with(prefix.as_str()))
        .filter_map(|(k, n)| {
            let rest = &k[prefix.len()..];
            (!rest.is_empty() && !rest.contains('/')).then_some((rest, n))
        })
        .collect()
}

impl Tree {
    pub fn build(raw: Vec<(String, String, Vec<Raw>)>) -> Tree {
        let mut merged: BTreeMap<String, Node> = BTreeMap::new();
        let mut layers = Vec::new();
        for (li, (created_by, digest, raws)) in raw.into_iter().enumerate() {
            let mut files: BTreeMap<String, Node> = BTreeMap::new();
            let (mut size, mut count) = (0u64, 0usize);
            for r in raws {
                let (dir, base) = split(&r.path);
                if base == ".wh..wh..opq" {
                    remove_under(&mut merged, dir, li);
                    if !dir.is_empty() {
                        ensure_parents(&mut files, &r.path, li);
                        if let Some(n) = files.get_mut(dir) {
                            if n.change == Change::None {
                                n.change = Change::Modified;
                            }
                        }
                    }
                    continue;
                }
                if let Some(name) = base.strip_prefix(".wh.") {
                    let target = join(dir, name);
                    let mut n = merged.remove(&target).unwrap_or_else(|| Node { kind: Kind::Other, ..Node::dir(li) });
                    remove_under(&mut merged, &target, li);
                    n.change = Change::Removed;
                    n.layer = li;
                    ensure_parents(&mut files, &target, li);
                    files.insert(target, n);
                    count += 1;
                    continue;
                }
                let mut n = r.node;
                n.layer = li;
                let prev = merged.get(&r.path).map(|o| o.kind);
                n.change = match prev {
                    Some(Kind::Dir) if n.kind == Kind::Dir => Change::None,
                    Some(_) => Change::Modified,
                    None => Change::Added,
                };
                if prev == Some(Kind::Dir) && n.kind != Kind::Dir {
                    remove_under(&mut merged, &r.path, usize::MAX);
                }
                if n.kind != Kind::Dir {
                    size += n.size;
                    count += 1;
                }
                ensure_parents(&mut merged, &r.path, li);
                ensure_parents(&mut files, &r.path, li);
                files.insert(r.path.clone(), n.clone());
                merged.insert(r.path, n);
            }
            layers.push(Layer { created_by, digest, size, count, files });
        }
        let total = merged.values().map(|n| n.size).sum();
        let files = merged.values().filter(|n| n.kind != Kind::Dir).count();
        Tree { merged, layers, total, files }
    }

    /// Follow symlinks in the merged view; returns the final key and kind.
    pub fn follow(&self, key: &str) -> Option<(String, Kind)> {
        let mut k = norm(key);
        for _ in 0..16 {
            let n = self.merged.get(&k)?;
            match (&n.kind, &n.link) {
                (Kind::Link, Some(t)) => k = resolve(split(&k).0, t),
                _ => return Some((k, n.kind)),
            }
        }
        None
    }
}

/// Kill the producer, collect its stderr, and prefer that as the error message.
fn finish<T>(mut child: Child, res: Result<T, String>) -> Result<T, String> {
    let _ = child.kill();
    let mut err = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut err);
    }
    let _ = child.wait();
    match res {
        Err(e) if !err.trim().is_empty() => Err(err.trim().lines().last().unwrap_or(&e).to_string()),
        r => r,
    }
}

pub fn from_export(id: &str) -> Result<Tree, String> {
    let mut child = docker::cmd(["export", id]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(es)?;
    let out = child.stdout.take().unwrap();
    let res = parse_layer(out);
    let raws = finish(child, res)?;
    Ok(Tree::build(vec![("container filesystem".into(), String::new(), raws)]))
}

type Save = (HashMap<String, String>, HashMap<String, Vec<Raw>>, HashMap<String, String>);

fn read_full<R: Read>(r: &mut R, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

fn is_tar(h: &[u8]) -> bool {
    h.len() == 512 && (&h[257..262] == b"ustar" || h.iter().all(|&b| b == 0))
}

fn read_save<R: Read>(r: R) -> Result<Save, String> {
    let mut ar = tar::Archive::new(r);
    let (mut jsons, mut tars, mut links) = (HashMap::new(), HashMap::new(), HashMap::new());
    for e in ar.entries().map_err(es)? {
        let mut e = e.map_err(es)?;
        let path = norm(&e.path().map_err(es)?.to_string_lossy());
        let et = e.header().entry_type();
        if et.is_symlink() {
            if let Ok(Some(l)) = e.link_name() {
                let t = resolve(split(&path).0, &l.to_string_lossy());
                links.insert(path, t);
            }
            continue;
        }
        if !(et.is_file() || et == tar::EntryType::Continuous) {
            continue;
        }
        let size = e.size();
        let mut head = vec![0u8; 512];
        let n = read_full(&mut e, &mut head).map_err(es)?;
        head.truncate(n);
        if matches!(head.first(), Some(b'{') | Some(b'[')) && size < (16 << 20) {
            let mut b = head;
            e.read_to_end(&mut b).map_err(es)?;
            jsons.insert(path, String::from_utf8_lossy(&b).into_owned());
        } else if head.starts_with(&[0x1f, 0x8b]) {
            if let Ok(v) = parse_layer(flate2::read::GzDecoder::new(Cursor::new(head).chain(&mut e))) {
                tars.insert(path, v);
            }
        } else if is_tar(&head) {
            if let Ok(v) = parse_layer(Cursor::new(head).chain(&mut e)) {
                tars.insert(path, v);
            }
        }
    }
    Ok((jsons, tars, links))
}

fn clean_cmd(s: &str) -> String {
    let mut s = s.trim();
    if let Some(r) = s.strip_prefix("/bin/sh -c #(nop) ") {
        s = r;
    }
    let s = s.strip_suffix("# buildkit").unwrap_or(s).replace("/bin/sh -c ", "");
    let s = s.strip_prefix("|").map(|r| r.trim_start_matches(|c: char| c.is_ascii_digit())).unwrap_or(&s);
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn from_image(image: &str) -> Result<Tree, String> {
    let mut child = docker::cmd(["save", image]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(es)?;
    let out = child.stdout.take().unwrap();
    let res = read_save(out);
    let (jsons, mut tars, links) = finish(child, res)?;
    let manifest: Value = jsons
        .get("manifest.json")
        .and_then(|s| serde_json::from_str(s).ok())
        .ok_or("image archive has no manifest.json")?;
    let m = &manifest[0];
    let layer_paths: Vec<String> =
        m["Layers"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(norm)).collect()).unwrap_or_default();
    let cfg: Value = m["Config"]
        .as_str()
        .and_then(|c| jsons.get(&norm(c)))
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(Value::Null);
    let names: Vec<String> = cfg["history"]
        .as_array()
        .map(|h| {
            h.iter()
                .filter(|x| !x["empty_layer"].as_bool().unwrap_or(false))
                .map(|x| clean_cmd(x["created_by"].as_str().unwrap_or("")))
                .collect()
        })
        .unwrap_or_default();
    let raw = layer_paths
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let p = links.get(p).cloned().unwrap_or_else(|| p.clone());
            let raws = tars.remove(&p).unwrap_or_default();
            let digest: String = p.rsplit('/').next().unwrap_or("").chars().take(12).collect();
            let name = names.get(i).cloned().filter(|s| !s.is_empty()).unwrap_or_else(|| format!("layer {i}"));
            (name, digest, raws)
        })
        .collect();
    Ok(Tree::build(raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(path: &str, kind: Kind) -> Raw {
        Raw { path: path.into(), node: Node { kind, size: 10, ..Node::dir(0) } }
    }

    #[test]
    fn layers_and_whiteouts() {
        let t = Tree::build(vec![
            ("base".into(), "".into(), vec![raw("etc", Kind::Dir), raw("etc/a", Kind::File), raw("etc/b", Kind::File), raw("var/x/y", Kind::File)]),
            ("mod".into(), "".into(), vec![raw("etc/.wh.a", Kind::File), raw("etc/b", Kind::File), raw("var/x/.wh..wh..opq", Kind::File), raw("var/x/z", Kind::File)]),
        ]);
        assert!(!t.merged.contains_key("etc/a"));
        assert!(!t.merged.contains_key("var/x/y"));
        assert!(t.merged.contains_key("var/x/z"));
        assert_eq!(t.layers[1].files["etc/a"].change, Change::Removed);
        assert_eq!(t.layers[1].files["etc/b"].change, Change::Modified);
        let root: Vec<_> = list(&t.merged, "").into_iter().map(|(n, _)| n).collect();
        assert_eq!(root, vec!["etc", "var"]);
        assert_eq!(resolve("usr/bin", "../lib/x"), "usr/lib/x");
    }
}
