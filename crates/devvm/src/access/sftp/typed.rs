//! Naming paths in messages as the user typed them.
//!
//! The copy operates on a normalised path (`~` expanded, a trailing `/` dropped, `.` and `..` read by name) but a
//! message must name what the user wrote. [`Typed`] holds, for the one end of a copy the user typed a path for, the
//! normalised root and the text; [`Typed::name`] turns the spelling of any path into the user's.

#[cfg(test)]
#[path = "typed_tests.rs"]
mod typed_tests;

/// The user's text for one path of a copy, and the normalised path it stands for.
pub(super) struct Typed {
    /// The normalised root as a message would name it, or `None` to name every path as it is.
    root: Option<String>,
    /// What the user typed for the root.
    typed: String,
    is_separator: fn(char) -> bool,
}

impl Typed {
    /// `typed` is what the user wrote for the path that a message names as `root`; `is_separator` tells the
    /// separators of the paths.
    pub(super) fn new(root: String, typed: String, is_separator: fn(char) -> bool) -> Typed {
        Typed {
            root: Some(root),
            typed,
            is_separator,
        }
    }

    /// A path the user did not type: nothing is renamed.
    pub(super) fn none() -> Typed {
        Typed {
            root: None,
            typed: String::new(),
            is_separator: |_| false,
        }
    }

    /// `path` as the user would write it: the root is the typed text, a path below it is the typed text and the rest.
    /// A part above the root is its own name and the typed text it belongs to: the user never wrote the path to it. A
    /// path elsewhere (a temporary file beside the root) is not the user's and stays as it is.
    pub(super) fn name(&self, path: String) -> String {
        let Some(root) = &self.root else { return path };
        let Some(rest) = path.strip_prefix(root.as_str()) else {
            return self.part_above(root, path);
        };
        if rest.is_empty() {
            return self.typed.clone();
        }
        let name = rest.trim_start_matches(self.is_separator);
        let at_a_name = name.len() != rest.len() || root.ends_with(self.is_separator);
        if !at_a_name {
            return path;
        }
        let base = self.typed.trim_end_matches(self.is_separator);
        if base.is_empty() {
            return format!("{}{name}", self.typed);
        }
        let separator = rest
            .chars()
            .next()
            .filter(|c| (self.is_separator)(*c))
            .or_else(|| root.chars().next_back())
            .expect("a root that ends the prefix has a last character");
        format!("{base}{separator}{name}")
    }

    /// `path` when it is a directory above the root: its last name and the typed text it is a part of.
    fn part_above(&self, root: &str, path: String) -> String {
        let Some(rest) = root
            .strip_prefix(path.as_str())
            .filter(|rest| !path.is_empty() && !rest.is_empty())
        else {
            return path;
        };
        let above = rest.starts_with(self.is_separator) || path.ends_with(self.is_separator);
        let name = path
            .trim_end_matches(self.is_separator)
            .rsplit(self.is_separator)
            .next();
        match name.filter(|name| above && !name.is_empty()) {
            Some(name) => format!("{name} (a part of {})", self.typed),
            None if above => format!("{path} (a part of {})", self.typed),
            None => path,
        }
    }
}
