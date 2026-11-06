//! A tree of real directories whose paths have exact lengths.
//!
//! A chain of nodes, each `NODE_NAME` characters below the previous; a leaf at any length hangs off the deepest node
//! that leaves room for its name. Unix builds by relative `mkdir`/`chdir` (an absolute path past PATH_MAX cannot be
//! created); Windows builds by full path and lets std add `\\?\`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const NODE_NAME: usize = 50;
pub const STRIDE: usize = NODE_NAME + 1;

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    /// An empty directory.
    Dir,
    /// A directory holding a file `f`.
    File,
}

pub struct Ladder {
    root_len: usize,
    /// The deepest chain node to build; a leaf deeper than this hangs off it with a longer name.
    max_node: usize,
    node: PathBuf,
    relative: bool,
}

pub struct Leaf {
    pub target: usize,
    pub kind: Kind,
    /// The leaf directory, or why it could not be built.
    pub path: Result<PathBuf, String>,
}

/// Removes a tree built by `Ladder`. A path near the Windows limit cannot be enumerated, so the lower half of the chain
/// is first renamed next to the root, which shortens every path in it.
pub fn remove_all(root: &Path) -> io::Result<()> {
    let node = |k: usize| {
        let mut p = root.to_path_buf();
        for _ in 0..k {
            p.push("n".repeat(NODE_NAME));
        }
        p
    };
    let mut depth = 0;
    while fs::symlink_metadata(node(depth + 1)).is_ok() {
        depth += 1;
    }
    if depth >= 2 {
        let moved = root.with_file_name("M");
        fs::rename(node(depth / 2), &moved)?;
        fs::remove_dir_all(&moved)?;
    }
    fs::remove_dir_all(root)
}

impl Ladder {
    pub fn new(root: PathBuf, relative: bool, max_node: usize) -> io::Result<Ladder> {
        fs::create_dir(&root)?;
        if relative {
            std::env::set_current_dir(&root)?;
        }
        Ok(Ladder { root_len: root.as_os_str().len(), max_node, node: root, relative })
    }

    fn at(&self, name: &str) -> PathBuf {
        if self.relative { PathBuf::from(name) } else { self.node.join(name) }
    }

    fn node_index(&self, target: usize) -> usize {
        assert!(target >= self.root_len + 2, "target {target} is shorter than the root ({})", self.root_len);
        ((target - 2 - self.root_len) / STRIDE).min(self.max_node)
    }

    fn leaf_name(&self, kind: Kind, target: usize) -> Result<String, String> {
        let len = target - (self.root_len + STRIDE * self.node_index(target)) - 1;
        if len > 255 {
            return Err(format!("leaf name of {len} characters"));
        }
        let c = if kind == Kind::Dir { "D" } else { "F" };
        Ok(c.repeat(len))
    }

    /// Builds one leaf directory of path length `target` (and its file `f`) below the current node.
    fn make_leaf(&self, kind: Kind, target: usize) -> Result<PathBuf, String> {
        let name = self.leaf_name(kind, target)?;
        fs::create_dir(self.at(&name)).map_err(|e| format!("mkdir leaf: {}", crate::calls::err(&e)))?;
        if kind == Kind::File {
            let rel = Path::new(&name).join("f");
            let file = if self.relative { rel } else { self.node.join(rel) };
            fs::File::create(file).map_err(|e| format!("create f: {}", crate::calls::err(&e)))?;
        }
        Ok(self.node.join(name))
    }

    fn descend(&mut self) -> io::Result<()> {
        let name = "n".repeat(NODE_NAME);
        fs::create_dir(self.at(&name))?;
        if self.relative {
            std::env::set_current_dir(&name)?;
        }
        self.node.push(name);
        Ok(())
    }

    /// Builds a leaf for every `(target, kind)`; every target is at least `root_len + 2`.
    pub fn build(&mut self, jobs: &[(usize, Kind)]) -> Vec<Leaf> {
        let mut jobs: Vec<(usize, Kind)> = jobs.to_vec();
        jobs.sort_by_key(|&(t, _)| t);
        let max_node = jobs.iter().map(|&(t, _)| self.node_index(t)).max().unwrap_or(0);
        let mut out = Vec::new();
        let mut stuck: Option<String> = None;
        for k in 0..=max_node {
            for &(target, kind) in jobs.iter().filter(|&&(t, _)| self.node_index(t) == k) {
                let path = match &stuck {
                    Some(why) => Err(why.clone()),
                    None => self.make_leaf(kind, target),
                };
                out.push(Leaf { target, kind, path });
            }
            if stuck.is_none() && k < max_node {
                if let Err(e) = self.descend() {
                    let at = self.node.as_os_str().len() + STRIDE;
                    stuck = Some(format!("mkdir chain node {} (len {at}): {}", k + 1, crate::calls::err(&e)));
                }
            }
        }
        out
    }
}

/// Component lengths of a path of exactly `target` characters below a root of `root_len`, for a path that is never created.
pub fn absent_tail(root_len: usize, target: usize) -> Vec<usize> {
    let mut rem = target - root_len;
    let mut names = Vec::new();
    while rem >= 2 {
        let n = (rem - 1).min(NODE_NAME);
        names.push(n);
        rem -= n + 1;
    }
    if rem == 1 {
        *names.last_mut().expect("target is at least root_len + 2") += 1;
    }
    names
}

pub fn absent_path(root: &Path, target: usize) -> PathBuf {
    let mut p = root.to_path_buf();
    for n in absent_tail(root.as_os_str().len(), target) {
        p.push("x".repeat(n));
    }
    p
}
