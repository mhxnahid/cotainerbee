//! Scrollable text/log viewer with regex search, filtering and follow mode.

use regex_lite::{Regex, RegexBuilder};

use crate::docker;

pub struct PLine {
    pub src: usize,
    pub ts: String,
    key: String,
    pub text: String,
}

pub struct Pager {
    pub title: String,
    pub lines: Vec<PLine>,
    pub vis: Vec<usize>,
    pub top: usize,
    pub follow: bool,
    pub hscroll: usize,
    pub wrap: bool,
    pub numbers: bool,
    pub filter: Option<Regex>,
    pub filter_src: String,
    pub search: Option<Regex>,
    pub search_src: String,
    pub cur: Option<usize>,
    pub sources: Vec<String>,
    pub show_ts: bool,
    pub is_log: bool,
    pub pids: Vec<u32>,
    pub ended: usize,
    pub gen: u64,
    pub height: usize,
    dirty: bool,
}

const MAX_LINES: usize = 100_000;

/// Strip ANSI escapes / control chars, expand tabs.
pub fn clean(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\x1b' => match it.peek() {
                Some('[') => {
                    it.next();
                    for d in it.by_ref() {
                        if ('@'..='~').contains(&d) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    it.next();
                    while let Some(d) = it.next() {
                        if d == '\x07' {
                            break;
                        }
                        if d == '\x1b' {
                            it.next();
                            break;
                        }
                    }
                }
                _ => {
                    it.next();
                }
            },
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

fn ts_key(ts: &str) -> String {
    if ts.is_empty() {
        return String::new();
    }
    match ts.find('.') {
        Some(dot) => {
            let digits: String = ts[dot + 1..].chars().take_while(|c| c.is_ascii_digit()).collect();
            format!("{}.{:0<9}", &ts[..dot], digits)
        }
        None => format!("{}.000000000", ts.trim_end_matches('Z')),
    }
}

/// Smart-case regex; falls back to a literal match if the pattern is invalid.
pub fn compile(s: &str) -> Option<Regex> {
    if s.is_empty() {
        return None;
    }
    let ci = !s.chars().any(|c| c.is_uppercase());
    RegexBuilder::new(s)
        .case_insensitive(ci)
        .build()
        .or_else(|_| RegexBuilder::new(&regex_lite::escape(s)).case_insensitive(ci).build())
        .ok()
}

fn hexdump(data: &[u8]) -> String {
    let mut out = String::new();
    for (i, chunk) in data.chunks(16).enumerate() {
        let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
        let ascii: String = chunk.iter().map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' }).collect();
        out.push_str(&format!("{:08x}  {:<47}  |{}|\n", i * 16, hex.join(" "), ascii));
    }
    out
}

impl Pager {
    fn new(title: String) -> Self {
        Pager {
            title,
            lines: Vec::new(),
            vis: Vec::new(),
            top: 0,
            follow: false,
            hscroll: 0,
            wrap: false,
            numbers: false,
            filter: None,
            filter_src: String::new(),
            search: None,
            search_src: String::new(),
            cur: None,
            sources: Vec::new(),
            show_ts: false,
            is_log: false,
            pids: Vec::new(),
            ended: 0,
            gen: 0,
            height: 20,
            dirty: false,
        }
    }

    pub fn text(title: impl Into<String>, body: &str) -> Self {
        let mut p = Self::new(title.into());
        p.numbers = true;
        p.lines = body.lines().map(|l| PLine { src: 0, ts: String::new(), key: String::new(), text: clean(l) }).collect();
        p.rebuild();
        p
    }

    pub fn file(path: &str, data: &[u8], total: u64) -> Self {
        let bin = data.iter().take(8192).any(|&b| b == 0);
        let body = if bin { hexdump(&data[..data.len().min(64 * 1024)]) } else { String::from_utf8_lossy(data).into_owned() };
        let mut title = format!("{path}  {}", docker::human(total as f64));
        if bin {
            title.push_str("  binary · hex view");
        }
        if (data.len() as u64) < total {
            title.push_str(&format!("  · first {} shown, d to download", docker::human(data.len() as f64)));
        }
        let mut p = Self::text(title, &body);
        p.numbers = !bin;
        p
    }

    pub fn logs(title: String, sources: Vec<String>, gen: u64) -> Self {
        let mut p = Self::new(title);
        p.sources = sources;
        p.is_log = true;
        p.follow = true;
        p.gen = gen;
        p
    }

    pub fn push_log(&mut self, src: usize, raw: &str) {
        let raw = raw.trim_end_matches(['\n', '\r']);
        let (ts, rest) = match raw.split_once(' ') {
            Some((a, b)) if a.len() >= 20 && a.as_bytes().get(4) == Some(&b'-') && a.contains('T') => (a, b),
            _ => ("", raw),
        };
        let line = PLine { src, ts: ts.to_string(), key: ts_key(ts), text: clean(rest) };
        let in_order = self.sources.len() <= 1 || self.lines.last().is_none_or(|l| l.key <= line.key);
        if in_order {
            let idx = self.lines.len();
            let pass = self.passes(&line);
            self.lines.push(line);
            if pass && !self.dirty {
                self.vis.push(idx);
            }
        } else {
            let pos = self.lines.partition_point(|l| l.key <= line.key);
            self.lines.insert(pos, line);
            self.dirty = true;
        }
        if self.lines.len() > MAX_LINES {
            self.lines.drain(..MAX_LINES / 5);
            self.dirty = true;
        }
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.rebuild();
    }

    fn passes(&self, l: &PLine) -> bool {
        match &self.filter {
            None => true,
            Some(re) => re.is_match(&l.text) || (self.sources.len() > 1 && re.is_match(&self.sources[l.src])),
        }
    }

    pub fn rebuild(&mut self) {
        self.vis = (0..self.lines.len()).filter(|&i| self.passes(&self.lines[i])).collect();
        self.dirty = false;
        self.cur = None;
        self.top = self.top.min(self.max_top());
    }

    fn max_top(&self) -> usize {
        self.vis.len().saturating_sub(self.height.max(1))
    }

    fn rows_of(&self, i: usize, width: usize) -> usize {
        let l = &self.lines[self.vis[i]];
        let extra = if self.show_ts { 15 } else { 0 } + if self.sources.len() > 1 { 18 } else { 0 };
        (l.text.chars().count() + extra).div_ceil(width.max(1)).max(1)
    }

    /// Called by the renderer with the inner size.
    pub fn prepare(&mut self, height: usize, width: usize) {
        if self.dirty {
            self.rebuild();
        }
        self.height = height.max(1);
        if self.follow {
            self.top = self.max_top();
            if self.wrap {
                let (mut rows, mut i) = (0, self.vis.len());
                while i > 0 {
                    let r = self.rows_of(i - 1, width);
                    if rows + r > self.height {
                        break;
                    }
                    rows += r;
                    i -= 1;
                }
                self.top = i;
            }
        }
        self.top = self.top.min(self.max_top());
    }

    pub fn scroll(&mut self, d: isize) {
        self.follow = false;
        let t = (self.top as isize + d).clamp(0, self.max_top() as isize) as usize;
        self.top = t;
        if self.is_log && d > 0 && t >= self.max_top() {
            self.follow = true;
        }
    }

    pub fn to_top(&mut self) {
        self.follow = false;
        self.top = 0;
    }

    pub fn to_bottom(&mut self) {
        self.top = self.max_top();
        self.follow = self.is_log;
    }

    pub fn set_filter(&mut self, s: &str) {
        self.filter_src = s.to_string();
        self.filter = compile(s);
        self.rebuild();
        if self.is_log {
            self.follow = true;
        }
    }

    /// Returns true if a match was found.
    pub fn set_search(&mut self, s: &str) -> bool {
        self.search_src = s.to_string();
        self.search = compile(s);
        self.cur = None;
        self.search.is_none() || self.next_match(1)
    }

    pub fn next_match(&mut self, dir: isize) -> bool {
        let Some(re) = &self.search else { return false };
        let n = self.vis.len() as isize;
        let mut i = match self.cur {
            Some(c) => c as isize + dir,
            None if dir > 0 => self.top as isize,
            None => (self.top + self.height) as isize - 1,
        };
        while i >= 0 && i < n {
            if re.is_match(&self.lines[self.vis[i as usize]].text) {
                let i = i as usize;
                self.cur = Some(i);
                self.follow = false;
                if i < self.top || i >= self.top + self.height {
                    self.top = i.saturating_sub(self.height / 3).min(self.max_top());
                }
                return true;
            }
            i += dir;
        }
        false
    }
}

impl Drop for Pager {
    fn drop(&mut self) {
        for p in self.pids.drain(..) {
            docker::reap(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_and_sort() {
        assert_eq!(clean("\x1b[31mred\x1b[0m\tx\r"), "red    x");
        let mut p = Pager::logs("t".into(), vec!["a".into(), "b".into()], 1);
        p.push_log(0, "2026-01-01T00:00:02.5Z second\n");
        p.push_log(1, "2026-01-01T00:00:01.25Z first\n");
        p.prepare(10, 80);
        assert_eq!(p.lines[p.vis[0]].text, "first");
        p.set_filter("sec");
        assert_eq!(p.vis.len(), 1);
        assert!(!p.set_search("SEC"));
        assert!(p.set_search("sec"));
    }
}
