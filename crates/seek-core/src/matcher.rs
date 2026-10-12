//! Compiles a parsed [`Expr`] into a matcher evaluated per row.

use memchr::memmem;
use regex::{Regex, RegexBuilder};

use crate::query::{Expr, Term};
use crate::search::SearchDevice;

/// Anchored, case-folded glob made only of `*` wildcards (no `?`).
struct Glob {
    /// Pieces between the `*`s; empty pieces are kept so their position encodes the anchoring.
    pieces: Vec<String>,
}

impl Glob {
    fn is_match(&self, s: &str) -> bool {
        let last = self.pieces.len() - 1;
        let mut pos = 0usize;
        for (i, piece) in self.pieces.iter().enumerate() {
            if piece.is_empty() {
                continue;
            }
            if i == 0 {
                if !s.starts_with(piece.as_str()) {
                    return false;
                }
                pos = piece.len();
            } else if i == last {
                return s.len() >= pos + piece.len() && s.ends_with(piece.as_str());
            } else {
                match s[pos..].find(piece.as_str()) {
                    Some(at) => pos += at + piece.len(),
                    None => return false,
                }
            }
        }
        true
    }
}

enum Pat {
    Sub(Box<memmem::Finder<'static>>),
    Glob(Glob),
    Re(Regex),
}

impl Pat {
    fn substring(needle: &str) -> Pat {
        Pat::Sub(Box::new(
            memmem::Finder::new(needle.as_bytes()).into_owned(),
        ))
    }

    fn wildcard(pattern: &str) -> Option<Pat> {
        if !pattern.contains('?') {
            return Some(Pat::Glob(Glob {
                pieces: pattern.split('*').map(str::to_string).collect(),
            }));
        }
        let mut re = String::with_capacity(pattern.len() * 2 + 2);
        re.push('^');
        let mut buf = [0u8; 4];
        for ch in pattern.chars() {
            match ch {
                '*' => re.push_str(".*"),
                '?' => re.push('.'),
                c => re.push_str(&regex::escape(c.encode_utf8(&mut buf))),
            }
        }
        re.push('$');
        RegexBuilder::new(&re)
            .case_insensitive(true)
            .build()
            .ok()
            .map(Pat::Re)
    }

    fn is_match(&self, s: &str) -> bool {
        match self {
            Pat::Sub(f) => f.find(s.as_bytes()).is_some(),
            Pat::Glob(g) => g.is_match(s),
            Pat::Re(r) => r.is_match(s),
        }
    }

    fn cost(&self) -> u8 {
        match self {
            Pat::Sub(_) => 1,
            Pat::Glob(_) => 2,
            Pat::Re(_) => 3,
        }
    }
}

enum Leaf {
    Name(Pat),
    Path(Pat),
    Ext(Vec<String>),
    Size { min: u64, max: u64 },
    Modified { min: i64, max: i64 },
    Device(String),
    Drive(String),
    Pc(String),
    Folders,
    Files,
    Online,
    Offline,
}

enum Node {
    And(Vec<Node>),
    Or(Vec<Node>),
    Not(Box<Node>),
    Leaf(Leaf),
    Never,
}

impl Node {
    fn cost(&self) -> u8 {
        match self {
            Node::And(v) | Node::Or(v) => v.iter().map(Node::cost).max().unwrap_or(0),
            Node::Not(n) => n.cost(),
            Node::Never => 0,
            Node::Leaf(l) => match l {
                Leaf::Name(p) => p.cost(),
                Leaf::Path(p) => 4 + p.cost(),
                _ => 0,
            },
        }
    }
}

/// Per-thread scratch space so path/device strings are built at most once per row/chunk.
#[derive(Default)]
pub struct Scratch {
    path_lower: String,
    path_ready: bool,
    pub dev_lower: String,
    pub pc_lower: String,
}

pub struct Matcher {
    root: Node,
}

fn compile_term(t: &Term) -> Node {
    let leaf = match t {
        Term::Text { needle, on_path } => {
            let p = Pat::substring(needle);
            if *on_path {
                Leaf::Path(p)
            } else {
                Leaf::Name(p)
            }
        }
        Term::Wildcard { pattern, on_path } => match Pat::wildcard(pattern) {
            Some(p) if *on_path => Leaf::Path(p),
            Some(p) => Leaf::Name(p),
            None => return Node::Never,
        },
        Term::Regex { pattern } => {
            match RegexBuilder::new(pattern).case_insensitive(true).build() {
                Ok(r) => Leaf::Name(Pat::Re(r)),
                Err(_) => return Node::Never,
            }
        }
        Term::Ext(exts) => Leaf::Ext(exts.clone()),
        Term::Size { min, max } => Leaf::Size {
            min: min.unwrap_or(0),
            max: max.unwrap_or(u64::MAX),
        },
        Term::Modified { min, max } => Leaf::Modified {
            min: min.unwrap_or(i64::MIN),
            max: max.unwrap_or(i64::MAX),
        },
        Term::Device(n) => Leaf::Device(n.clone()),
        Term::Drive(n) => Leaf::Drive(n.clone()),
        Term::Pc(n) => Leaf::Pc(n.clone()),
        Term::FoldersOnly => Leaf::Folders,
        Term::FilesOnly => Leaf::Files,
        Term::OnlineOnly => Leaf::Online,
        Term::OfflineOnly => Leaf::Offline,
    };
    Node::Leaf(leaf)
}

fn compile(e: &Expr) -> Node {
    match e {
        Expr::And(v) => {
            let mut nodes: Vec<Node> = v.iter().map(compile).collect();
            // cheap predicates first so expensive ones only see surviving rows
            nodes.sort_by_key(Node::cost);
            Node::And(nodes)
        }
        Expr::Or(v) => {
            let mut nodes: Vec<Node> = v.iter().map(compile).collect();
            nodes.sort_by_key(Node::cost);
            Node::Or(nodes)
        }
        Expr::Not(inner) => Node::Not(Box::new(compile(inner))),
        Expr::Term(t) => compile_term(t),
    }
}

impl Matcher {
    pub fn compile(expr: &Expr) -> Matcher {
        Matcher {
            root: compile(expr),
        }
    }

    /// A required, non-empty lowercase-name substring (longest one) that lets callers pre-scan the name
    /// arena instead of testing every row. Rows without it can never match.
    pub fn prefilter(&self) -> Option<&memmem::Finder<'static>> {
        fn pick(n: &Node) -> Option<&memmem::Finder<'static>> {
            match n {
                Node::Leaf(Leaf::Name(Pat::Sub(f))) if !f.needle().is_empty() => Some(&**f),
                _ => None,
            }
        }
        match &self.root {
            Node::And(v) => v.iter().filter_map(pick).max_by_key(|f| f.needle().len()),
            n => pick(n),
        }
    }

    /// Evaluate against one live row. Caller has already checked `is_alive`.
    pub fn is_match(&self, sd: &SearchDevice, row: u32, scratch: &mut Scratch) -> bool {
        scratch.path_ready = false;
        eval(&self.root, sd, row, scratch)
    }
}

fn eval(n: &Node, sd: &SearchDevice, row: u32, sc: &mut Scratch) -> bool {
    match n {
        Node::And(v) => v.iter().all(|c| eval(c, sd, row, sc)),
        Node::Or(v) => v.iter().any(|c| eval(c, sd, row, sc)),
        Node::Not(inner) => !eval(inner, sd, row, sc),
        Node::Never => false,
        Node::Leaf(l) => {
            let ix = sd.index;
            match l {
                Leaf::Name(p) => p.is_match(ix.name_lower(row)),
                Leaf::Path(p) => {
                    if !sc.path_ready {
                        sc.path_lower = ix.full_path(row, sd.mount).to_lowercase();
                        sc.path_ready = true;
                    }
                    p.is_match(&sc.path_lower)
                }
                Leaf::Ext(exts) => {
                    if ix.is_dir(row) {
                        return false;
                    }
                    let name = ix.name_lower(row);
                    match name.rfind('.') {
                        Some(dot) => exts.iter().any(|e| e == &name[dot + 1..]),
                        None => false,
                    }
                }
                Leaf::Size { min, max } => (*min..=*max).contains(&ix.size(row)),
                Leaf::Modified { min, max } => (*min..=*max).contains(&ix.modified(row)),
                Leaf::Device(n) => sc.dev_lower.contains(n.as_str()) || sc.pc_lower.contains(n.as_str()),
                Leaf::Drive(n) => sc.dev_lower.contains(n.as_str()),
                Leaf::Pc(n) => sc.pc_lower.contains(n.as_str()),
                Leaf::Folders => ix.is_dir(row),
                Leaf::Files => !ix.is_dir(row),
                Leaf::Online => sd.online,
                Leaf::Offline => !sd.online,
            }
        }
    }
}
