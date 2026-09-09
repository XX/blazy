//! The workspace file: a split tree plus what lives in each of its areas.
//!
//! `rnd/architecture.md` §8 asks the split tree to be the piece that can be written to a
//! file, and §41.5 answers the question that leaves open: a file has to say more than
//! "two splits and three leaves" — it has to say that *this* leaf is a node editor and
//! *that* one is an outliner, and the tree is pure geometry on purpose.
//!
//! So the pairing lives here rather than in the tree. A [`Workspace`] is a tree and a
//! map from [`AreaId`] to a **name the application chose**, which is the same shape the
//! keymap already uses for operators (§11): a string, because a file has to survive being
//! written, edited by hand and read back, and because this crate has no business knowing
//! what a node editor is.
//!
//! # The format
//!
//! Line-based, and deliberately small:
//!
//! ```text
//! blazy-workspace 1
//! tree split h 0.5 area 0 split v 0.5 area 1 area 2
//! content 0 node-editor
//! content 1 outliner
//! maximized 1
//! ```
//!
//! `tree` is a prefix expression — `area <id>` or `split <h|v> <ratio> <expr> <expr>`.
//! The rest of a `content` line is the application's own name, spaces included.
//!
//! **No `serde`, and that is a decision rather than an omission (§41.6).** A dependency
//! in `blazy-areas` is a dependency for everyone who sits on this library; behind a
//! feature flag it is also a flag the facade has to forward (§40.4). What it would buy is
//! a hundred lines of reader and writer for a structure with three shapes in it. The
//! trade would look different for a format with a hundred fields, and that is the point
//! at which to revisit it.
//!
//! **The format is not promised stable.** The version on the first line exists so that a
//! file this build cannot read is refused rather than misread.

use std::fmt;

use crate::tree::{AreaId, SplitTree};

/// The magic word and the version this build reads and writes.
const MAGIC: &str = "blazy-workspace";
const VERSION: u32 = 1;

/// A screen layout as it goes to a file: the tree, and what fills each area.
///
/// The owner the pairing needed. `SplitTree` stays pure geometry — it knows an area only
/// by its id — and this is where the id is joined to the application's idea of what the
/// area is for.
#[derive(Clone, Debug)]
pub struct Workspace {
    tree: SplitTree,
    /// What fills each area, ascending by id and without duplicates.
    contents: Vec<(AreaId, String)>,
}

/// Why a workspace file could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum WorkspaceError {
    /// The first line is not this format's, or names a version this build cannot read.
    NotAWorkspace,
    /// A line could not be read. Lines are numbered from one.
    Malformed {
        /// The line the reader gave up on.
        line: usize,
    },
    /// The file describes no tree, or more than one.
    NoTree,
    /// A `content` or `maximized` line names an area the tree does not hold.
    UnknownArea {
        /// The id that is not in the tree.
        area: AreaId,
    },
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAWorkspace => write!(f, "not a {MAGIC} file this build can read"),
            Self::Malformed { line } => write!(f, "line {line} could not be read"),
            Self::NoTree => write!(f, "the file describes no tree, or more than one"),
            Self::UnknownArea { area } => write!(f, "area {area} is not in the tree"),
        }
    }
}

impl std::error::Error for WorkspaceError {}

impl Workspace {
    /// A workspace over `tree`, with nothing said about what fills its areas.
    pub fn new(tree: SplitTree) -> Self {
        Self {
            tree,
            contents: Vec::new(),
        }
    }

    /// Says what fills one area. Replaces whatever was said before.
    #[must_use]
    pub fn with_content(mut self, area: AreaId, kind: impl Into<String>) -> Self {
        self.set_content(area, kind);
        self
    }

    /// Says what fills one area, in place.
    pub fn set_content(&mut self, area: AreaId, kind: impl Into<String>) {
        let kind = kind.into();
        match self.contents.binary_search_by_key(&area, |(id, _)| *id) {
            Ok(at) => self.contents[at].1 = kind,
            Err(at) => self.contents.insert(at, (area, kind)),
        }
    }

    /// The tree.
    pub fn tree(&self) -> &SplitTree {
        &self.tree
    }

    /// The tree, to go on editing.
    pub fn tree_mut(&mut self) -> &mut SplitTree {
        &mut self.tree
    }

    /// What fills `area`, if the file said.
    pub fn content(&self, area: AreaId) -> Option<&str> {
        self.contents
            .binary_search_by_key(&area, |(id, _)| *id)
            .ok()
            .map(|at| self.contents[at].1.as_str())
    }

    /// Every area the file said something about, ascending by id.
    pub fn contents(&self) -> impl Iterator<Item = (AreaId, &str)> {
        self.contents.iter().map(|(area, kind)| (*area, kind.as_str()))
    }

    /// Writes the workspace out.
    ///
    /// What is written is the tree's *shape* and its area ids, never its node ids: the
    /// round trip has to give back the same rectangles and the same [`AreaId`]s, because
    /// the caller's widgets are keyed by those, and nothing else about the tree is
    /// anybody's business.
    pub fn write(&self) -> String {
        let mut out = String::new();
        out.push_str(MAGIC);
        out.push(' ');
        out.push_str(&VERSION.to_string());
        out.push('\n');

        out.push_str("tree ");
        self.tree.write_expr(&mut out);
        out.push('\n');

        for (area, kind) in &self.contents {
            // A kind with a newline in it would produce a file that reads back as
            // something else, so it is written as one line and no more.
            let kind = kind.replace(['\n', '\r'], " ");
            out.push_str(&format!("content {area} {kind}\n"));
        }

        if let Some(area) = self.tree.maximized() {
            out.push_str(&format!("maximized {area}\n"));
        }
        out
    }

    /// Reads a workspace back.
    pub fn parse(text: &str) -> Result<Self, WorkspaceError> {
        let mut lines = text.lines().enumerate().map(|(at, line)| (at + 1, line.trim()));

        let (_, header) = lines.next().ok_or(WorkspaceError::NotAWorkspace)?;
        let mut header = header.split_whitespace();
        if header.next() != Some(MAGIC) || header.next().and_then(|v| v.parse::<u32>().ok()) != Some(VERSION) {
            return Err(WorkspaceError::NotAWorkspace);
        }

        let mut tree: Option<SplitTree> = None;
        let mut contents: Vec<(AreaId, String)> = Vec::new();
        let mut maximized: Option<(usize, AreaId)> = None;

        for (number, line) in lines {
            if line.is_empty() {
                continue;
            }
            let malformed = WorkspaceError::Malformed { line: number };
            let (key, rest) = line.split_once(' ').ok_or(malformed.clone())?;
            match key {
                "tree" => {
                    if tree.is_some() {
                        return Err(WorkspaceError::NoTree);
                    }
                    let mut tokens = rest.split_whitespace();
                    tree = Some(SplitTree::parse_expr(&mut tokens).ok_or(malformed)?);
                },
                "content" => {
                    let (area, kind) = rest.split_once(' ').ok_or(malformed.clone())?;
                    let area: AreaId = area.parse().map_err(|_| malformed)?;
                    contents.push((area, kind.to_string()));
                },
                "maximized" => {
                    let area: AreaId = rest.trim().parse().map_err(|_| malformed)?;
                    maximized = Some((number, area));
                },
                _ => return Err(malformed),
            }
        }

        let tree = tree.ok_or(WorkspaceError::NoTree)?;
        let mut workspace = Self::new(tree);
        for (area, kind) in contents {
            if !workspace.tree.holds(area) {
                return Err(WorkspaceError::UnknownArea { area });
            }
            workspace.set_content(area, kind);
        }
        if let Some((_, area)) = maximized
            && !workspace.tree.set_maximized(area)
        {
            return Err(WorkspaceError::UnknownArea { area });
        }
        Ok(workspace)
    }
}

#[cfg(test)]
mod tests {
    use masonry::kurbo::{Axis, Rect};

    use super::*;
    use crate::tree::Bar;

    const SCREEN: Rect = Rect::new(0.0, 0.0, 1400.0, 900.0);
    const BAR: f64 = 4.0;

    /// Everything a round trip has to preserve: where each area is, by id, and where
    /// each splitter is.
    ///
    /// A [`Bar`]'s `split` is deliberately left out. It is a node index, the file does
    /// not carry one, and nothing may keep one across a layout — the type says so. What
    /// has to come back is the *geometry* and the area ids, because the caller's widgets
    /// are keyed by those.
    /// A splitter, as far as a round trip is concerned: where it is and what it divides.
    type BarGeometry = (Axis, Rect, Rect);

    fn laid_out(tree: &SplitTree) -> (Vec<(AreaId, Rect)>, Vec<BarGeometry>) {
        let (mut areas, mut bars) = (Vec::new(), Vec::new());
        tree.layout(SCREEN, BAR, &mut areas, &mut bars);
        let bars: Vec<BarGeometry> = bars.into_iter().map(|b: Bar| (b.axis, b.rect, b.span)).collect();
        (areas, bars)
    }

    /// A workspace that has been through a file has to put every widget back in its own
    /// area — which means the same rectangles **and** the same ids, not one or the other.
    #[test]
    fn a_round_trip_gives_the_same_rectangles_and_the_same_ids() {
        for count in [1, 2, 3, 8, 16] {
            let mut tree = SplitTree::balanced(count);
            // A tree that has been operated on, not just built: joins leave holes in the
            // id space, and the file has to carry those through.
            if count >= 8 {
                assert!(tree.join(0, 1));
                assert!(tree.swap(2, 3));
            }
            let before = laid_out(&tree);

            let text = Workspace::new(tree).write();
            let read = Workspace::parse(&text).expect("what we wrote reads back");

            assert_eq!(laid_out(read.tree()), before, "{count} areas");
        }
    }

    /// A ratio is a float, and a round trip that rounds it moves every splitter under it.
    #[test]
    fn an_awkward_ratio_survives_the_trip() {
        let mut tree = SplitTree::balanced(2);
        let (mut areas, mut bars) = (Vec::new(), Vec::new());
        tree.layout(SCREEN, BAR, &mut areas, &mut bars);
        assert!(tree.set_ratio(bars[0].split, 1.0 / 3.0));
        let before = laid_out(&tree);

        let read = Workspace::parse(&Workspace::new(tree).write()).expect("reads back");
        assert_eq!(laid_out(read.tree()), before);
    }

    #[test]
    fn contents_survive_the_trip() {
        let workspace = Workspace::new(SplitTree::balanced(3))
            .with_content(0, "node-editor")
            .with_content(2, "outliner with spaces");

        let read = Workspace::parse(&workspace.write()).expect("reads back");
        assert_eq!(read.content(0), Some("node-editor"));
        assert_eq!(read.content(1), None, "an area nobody named stays unnamed");
        assert_eq!(read.content(2), Some("outliner with spaces"));
        assert_eq!(read.contents().count(), 2);
    }

    #[test]
    fn the_maximized_area_survives_the_trip() {
        let mut tree = SplitTree::balanced(4);
        assert!(tree.maximize(2));
        let read = Workspace::parse(&Workspace::new(tree).write()).expect("reads back");
        assert_eq!(read.tree().maximized(), Some(2));
        assert_eq!(laid_out(read.tree()).0, vec![(2, SCREEN)]);
    }

    #[test]
    fn a_file_this_build_cannot_read_is_refused() {
        assert_eq!(Workspace::parse("").err(), Some(WorkspaceError::NotAWorkspace));
        assert_eq!(
            Workspace::parse("something else\n").err(),
            Some(WorkspaceError::NotAWorkspace)
        );
        assert_eq!(
            Workspace::parse("blazy-workspace 2\ntree area 0\n").err(),
            Some(WorkspaceError::NotAWorkspace),
            "a later version is refused rather than misread"
        );
    }

    #[test]
    fn a_broken_file_says_which_line() {
        assert_eq!(
            Workspace::parse("blazy-workspace 1\ntree split h nonsense area 0 area 1\n").err(),
            Some(WorkspaceError::Malformed { line: 2 })
        );
        assert_eq!(
            Workspace::parse("blazy-workspace 1\ntree area 0\nnonsense 1\n").err(),
            Some(WorkspaceError::Malformed { line: 3 })
        );
        assert_eq!(
            Workspace::parse("blazy-workspace 1\n").err(),
            Some(WorkspaceError::NoTree)
        );
    }

    /// A file whose content lines name areas that are not there is inconsistent, and
    /// loading it would put a widget nowhere.
    #[test]
    fn content_for_an_area_that_is_not_there_is_refused() {
        assert_eq!(
            Workspace::parse("blazy-workspace 1\ntree area 0\ncontent 4 outliner\n").err(),
            Some(WorkspaceError::UnknownArea { area: 4 })
        );
        assert_eq!(
            Workspace::parse("blazy-workspace 1\ntree area 0\nmaximized 4\n").err(),
            Some(WorkspaceError::UnknownArea { area: 4 })
        );
    }

    /// Two leaves with one id would give two widgets one identity.
    #[test]
    fn a_tree_naming_an_area_twice_is_refused() {
        assert_eq!(
            Workspace::parse("blazy-workspace 1\ntree split h 0.5 area 0 area 0\n").err(),
            Some(WorkspaceError::Malformed { line: 2 })
        );
    }
}
