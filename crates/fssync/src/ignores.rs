//! Configurable exclusion for a synced root (docs/design/fs-watch.md
//! "Ignoring").
//!
//! Three sources, composed into one matcher and evaluated per path:
//!
//! - **`.git`** — a pure name filter, no git data read.
//! - **ignore files** — the enclosing worktree's exclude stack: every
//!   `.gitignore` / `.ignore` from the worktree top down to the deepest
//!   directory on the path, plus `$GIT_DIR/info/exclude` and the user's
//!   `core.excludesFile`.
//! - **client patterns** — gitignore syntax, anchored at the sync root,
//!   highest precedence so `!keep-this` re-includes what the rest hide.
//!
//! Precedence is git's: the deepest ignore file wins over shallower ones,
//! a match on an ancestor *directory* excludes everything below it (which
//! is why a negation cannot resurrect a file under an excluded directory),
//! and client patterns sit above the whole stack.
//!
//! Filtering is not a view over a full index — an excluded path is never
//! stated, indexed, hashed, or counted against the entry budget, and its
//! hints are dropped before the settle tick. That is the whole point: a
//! sync of a checkout should cost the checkout, not `node_modules`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// The non-git per-directory ignore file — ripgrep's convention, honored
/// by the `FS_INDEX` / `FS_GREP` walkers and selectable here on its own.
pub const DOT_IGNORE_NAME: &str = ".ignore";
/// The git per-directory ignore file. Lower in the same directory than
/// [`DOT_IGNORE_NAME`] is: within one directory `.gitignore` wins, matching
/// the walkers.
pub const GITIGNORE_NAME: &str = ".gitignore";

/// Directory name excluded by `exclude_git`, and the gitdir marker the
/// worktree search looks for.
const GIT_DIR_NAME: &str = ".git";

/// Cap on client patterns, so a hostile `FS_SYNC` cannot compile an
/// unbounded glob set. Refused at request validation, not silently cut.
pub const MAX_PATTERNS: usize = 4096;

/// What a sync excludes. Part of the shared root's identity ([`crate::RootKey`]):
/// two syncs indexing different trees cannot share a reconciler, exactly as
/// for `recursive` — so this derives `Hash`/`Eq` over the normalized
/// pattern list rather than over a compiled matcher.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct IgnoreSpec {
    /// Honor `.gitignore` in and above the root, plus the governing
    /// repository's `$GIT_DIR/info/exclude` and the user's
    /// `core.excludesFile`.
    pub gitignore: bool,
    /// Honor `.ignore` in and above the root — ripgrep's convention,
    /// which a project can use to hide things from tooling without
    /// telling git to stop tracking them.
    pub dot_ignore: bool,
    /// Omit every entry whose final component is exactly `.git`.
    pub exclude_git: bool,
    /// Extra gitignore-syntax patterns, root-anchored, highest precedence.
    pub patterns: Vec<String>,
}

impl IgnoreSpec {
    /// Nothing to exclude: the reconciler skips building a matcher at all,
    /// so an unfiltered sync pays exactly what it paid before.
    pub fn is_empty(&self) -> bool {
        !self.reads_ignore_files() && !self.exclude_git && self.patterns.is_empty()
    }

    /// Whether any per-directory ignore file is consulted.
    pub fn reads_ignore_files(&self) -> bool {
        self.gitignore || self.dot_ignore
    }

    /// Per-directory ignore file names this spec reads, ascending
    /// precedence within one directory.
    pub fn file_names(&self) -> Vec<&'static str> {
        let mut names = Vec::with_capacity(2);
        if self.dot_ignore {
            names.push(DOT_IGNORE_NAME);
        }
        if self.gitignore {
            names.push(GITIGNORE_NAME);
        }
        names
    }

    /// Normalize the wire form — one gitignore line per `\n` — into the
    /// pattern list. Blank lines and `#` comments are dropped here rather
    /// than at compile time so that two specs differing only in whitespace
    /// share one shared root.
    ///
    /// Order carries meaning only when a negation is present, since
    /// gitignore's rule is last-match-wins and a list of pure exclusions
    /// commutes. Such a list is therefore sorted and deduplicated, so two
    /// clients that asked for the same thing in a different order share
    /// one root instead of building two identical indexes. A list with a
    /// `!` in it is left exactly as written — reordering it would change
    /// what it means.
    pub fn parse_patterns(text: &str) -> Vec<String> {
        let mut patterns: Vec<String> = text
            .split('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line).trim())
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(str::to_string)
            .collect();
        if !patterns.iter().any(|p| p.starts_with('!')) {
            patterns.sort();
            patterns.dedup();
        }
        patterns
    }
}

/// Compiled exclusion for one root. Per-directory matchers are built on
/// first use and memoized, so the initial scan reads each `.gitignore`
/// once and incremental reconciliation reads none.
pub struct Ignores {
    root: PathBuf,
    /// Kept so [`Ignores::invalidate`] can rebuild from scratch. The
    /// memo tables are not the only thing a rules change invalidates —
    /// `base`, `global` and `fold_case` are all read once at construction
    /// — so dropping the caches alone would keep serving an edited
    /// ancestor `.gitignore` from the copy compiled at open.
    spec: IgnoreSpec,
    exclude_git: bool,
    /// Per-directory file names to read, ascending precedence. Empty when
    /// only `.git` and client patterns are configured.
    file_names: Vec<&'static str>,
    /// `core.ignorecase` from the governing repository: on a
    /// case-insensitive filesystem git folds case when matching, and a
    /// mirror that did not would exclude a different set of paths than
    /// the repository it is mirroring.
    fold_case: bool,
    /// Client patterns: consulted before everything else, at every level.
    /// They are the sync's own filter, not a repository's, so unlike
    /// everything below they apply across repository boundaries too.
    overrides: Gitignore,
    /// The user's `core.excludesFile`, which every repository inherits —
    /// kept apart from `base` so a nested repository can start a fresh
    /// stack with it still at the bottom.
    global: Option<Arc<Gitignore>>,
    /// Sources for the root's own repository, ascending: `global`,
    /// `$GIT_DIR/info/exclude`, then each `.gitignore` between the
    /// enclosing worktree top and the root. Below every per-directory
    /// matcher inside the root, and discarded at a nested repository.
    base: Vec<Arc<Gitignore>>,
    /// Every `$GIT_DIR/info/exclude` loaded so far — the root's, plus one
    /// per nested repository discovered while scanning. Tracked because
    /// they are the ignore sources whose names are neither of the
    /// per-directory ones, so a change to one has to be recognized.
    info_excludes: std::collections::HashSet<PathBuf>,
    /// Ignore files outside the root that the stack consulted: the
    /// ancestors' own, and the governing `info/exclude` when its gitdir
    /// sits above the root. Watched separately, since no hint from inside
    /// the tree could ever report them.
    external_sources: std::collections::HashSet<PathBuf>,
    /// Ignore files at one directory, keyed by its wire path (`""` = root).
    /// `None` = that directory has no ignore file.
    per_dir: HashMap<String, Option<Arc<Gitignore>>>,
    /// `base` plus every per-directory matcher from the root down to this
    /// directory, in ascending precedence. Memoized per directory so a
    /// scan pays one lookup per level instead of re-walking the chain.
    stacks: HashMap<String, Arc<Vec<Arc<Gitignore>>>>,
    /// Whether each directory is excluded, itself or through an ancestor.
    /// This is what keeps matching linear: without it every path re-tests
    /// each of its ancestors against that ancestor's own stack, so a path
    /// `d` deep costs O(d²) glob probes and a tree pays it per entry.
    dir_verdicts: HashMap<String, bool>,
}

/// Cache ceiling, in directories, across the three memo tables. They are
/// pure memoization of what the filesystem says, so the cheap bound is to
/// drop them wholesale and pay the re-reads; the normal case never gets
/// near it, since the tables hold one entry per *indexed* directory and
/// the entry budget already bounds those.
const MAX_CACHED_DIRS: usize = 1 << 17;

impl Ignores {
    /// Compile `spec` for `root`. Unparseable client patterns are dropped
    /// individually: a sync is never refused for one bad glob, and the
    /// server validates the list before it gets here.
    pub fn new(root: &Path, spec: &IgnoreSpec) -> Ignores {
        let file_names = spec.file_names();
        let governing =
            gitdir_at(root).or_else(|| enclosing_worktree_top(root).as_deref().and_then(gitdir_at));
        let fold_case = governing.as_deref().is_some_and(config_ignorecase);
        let mut overrides = GitignoreBuilder::new(root);
        overrides.case_insensitive(fold_case).ok();
        for pattern in &spec.patterns {
            let _ = overrides.add_line(None, pattern);
        }
        let overrides = overrides.build().unwrap_or_else(|_| Gitignore::empty());
        let mut global = None;
        let mut base = Vec::new();
        let mut info_excludes = std::collections::HashSet::new();
        let mut external_sources = std::collections::HashSet::new();
        if spec.reads_ignore_files() {
            // `.gitignore`-only sources: the user's `core.excludesFile`
            // and the repository's `info/exclude` are git's, so `.ignore`
            // alone reads neither.
            if spec.gitignore {
                let (found, _) = Gitignore::global();
                if !found.is_empty() {
                    let found = Arc::new(found);
                    base.push(found.clone());
                    global = Some(found);
                }
            }
            match gitdir_at(root) {
                // The root is itself a repository top: its own stack is
                // the whole stack. An enclosing repository's rules do not
                // reach inside it, exactly as they do not for a nested one.
                Some(gitdir) => {
                    if spec.gitignore {
                        push_info_exclude(&mut base, &mut info_excludes, root, &gitdir, fold_case);
                    }
                }
                // Inside a worktree: the ignore files above the root still
                // apply — a sync of `repo/crates` inherits `repo/.gitignore`
                // — shallowest first, so a deeper file overrides.
                None => {
                    if let Some(top) = enclosing_worktree_top(root) {
                        if spec.gitignore
                            && let Some(gitdir) = gitdir_at(&top)
                        {
                            let before = info_excludes.len();
                            // Anchored at the *worktree top*, not at the sync
                            // root: git reads `info/exclude` relative to the
                            // top, so `/build` there means `<top>/build` and
                            // nothing else. Anchoring it at `repo/crates`
                            // instead made it hide `repo/crates/build` — a
                            // path git would have synced — and miss the one it
                            // names. Non-anchored patterns (`target/`) match
                            // at every level either way, which is why this
                            // only shows up with a leading or embedded slash.
                            push_info_exclude(
                                &mut base,
                                &mut info_excludes,
                                &top,
                                &gitdir,
                                fold_case,
                            );
                            if info_excludes.len() != before {
                                external_sources.extend(info_excludes.iter().cloned());
                            }
                        }
                        for dir in ancestors_between(&top, root) {
                            if let Some(matcher) = build_dir_matcher(&dir, &file_names, fold_case) {
                                base.push(matcher);
                            }
                            // Recorded whether or not they exist today:
                            // creating one is exactly the change that has
                            // to invalidate the stack.
                            external_sources.extend(file_names.iter().map(|n| dir.join(n)));
                        }
                    }
                }
            }
        }
        Ignores {
            root: root.to_path_buf(),
            spec: spec.clone(),
            exclude_git: spec.exclude_git,
            file_names,
            fold_case,
            overrides,
            global,
            base,
            info_excludes,
            external_sources,
            per_dir: HashMap::new(),
            stacks: HashMap::new(),
            dir_verdicts: HashMap::new(),
        }
    }

    /// Directories outside the root that hold ignore sources this matcher
    /// consulted, so the reconciler can watch them. Their *parents* are
    /// what gets armed — a watch on a file follows its inode and misses
    /// the rename-over an editor or `git config` performs, the same reason
    /// a single-file sync watches the parent directory.
    pub fn external_watch_dirs(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = self
            .external_sources
            .iter()
            .filter_map(|p| p.parent().map(Path::to_path_buf))
            .collect();
        dirs.sort();
        dirs.dedup();
        dirs
    }

    /// True when `abs` is one of the ignore sources above the root. Their
    /// hints arrive from outside the tree, so nothing else recognizes them.
    pub fn is_external_source(&self, abs: &Path) -> bool {
        self.external_sources.contains(abs)
    }

    /// Whether a write to this ignore source can change what the matcher
    /// excludes — the question the reconciler actually has, since the
    /// answer costs it a full re-enumeration.
    ///
    /// An ignore file inside an already-excluded directory cannot: it is
    /// never read, because the directory is never descended. That
    /// distinction is not academic — `npm install` writes thousands of
    /// `.gitignore` files under `node_modules`, and rescanning the root
    /// for each one would make the exclusion cost more than it saves.
    ///
    /// `abs` is the absolute path and `rel` its wire path under the root.
    pub fn source_affects_rules(&mut self, abs: &Path, rel: &str) -> bool {
        if !self.is_source_abs(abs) {
            return false;
        }
        if self.is_info_exclude(abs) {
            // It governs the worktree three components above it, and is
            // relevant exactly while that worktree is. Testing its own
            // directory instead would always say no: `.git/info` is
            // excluded by `EXCLUDE_GIT`, including for the root's own
            // repository, which is the one case that always matters.
            return match repo_top_of_info_exclude(rel) {
                Some(top) => !self.matched(top, true),
                // A gitfile-linked gitdir (a submodule's lives under the
                // superproject's `.git/modules/`): no worktree to derive,
                // and it is only in the recorded set because we loaded it.
                None => true,
            };
        }
        match crate::parent_wire(rel) {
            Some(dir) => !self.matched(dir, true),
            None => true,
        }
    }

    /// True when an absolute path is an ignore source at all — before
    /// asking whether it is a *relevant* one
    /// ([`Ignores::source_affects_rules`]).
    pub fn is_source_abs(&self, abs: &Path) -> bool {
        if self.file_names.is_empty() {
            return false;
        }
        if abs
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| self.file_names.contains(&name))
        {
            return true;
        }
        self.is_info_exclude(abs)
    }

    /// `$GIT_DIR/info/exclude`, the one ignore source not named by a
    /// per-directory file. Matched structurally rather than only
    /// against what has been loaded, so a nested repository whose subtree
    /// no scan has reached yet still gets its rules noticed; the recorded
    /// set then covers gitfile-linked gitdirs (a submodule's lives under
    /// the superproject's `.git/modules/`, not at `…/.git/info/exclude`).
    fn is_info_exclude(&self, abs: &Path) -> bool {
        abs.ends_with(Path::new(GIT_DIR_NAME).join("info").join("exclude"))
            || self.info_excludes.contains(abs)
    }

    /// Recompile from disk. Called when an ignore source changed; the
    /// reconciler pairs it with a full rescan, since entries the old rules
    /// admitted may now be excluded and vice versa.
    ///
    /// A full rebuild rather than a cache drop: the sources read once at
    /// construction — the ancestors' files, `info/exclude`,
    /// `core.excludesFile`, `core.ignorecase` — are exactly the ones no
    /// per-directory cache holds, so clearing only the caches would keep
    /// serving an edited parent `.gitignore` from the copy compiled at
    /// open.
    pub fn invalidate(&mut self) {
        *self = Ignores::new(&self.root.clone(), &self.spec.clone());
    }

    /// Whether the entry at wire path `rel` is excluded. `is_dir` means
    /// the entry is *enumerated as* a directory — a symlink to one counts,
    /// unlike in git, because this sync descends it: a `build/` pattern
    /// that could not exclude a symlinked `build` would leave the one hole
    /// through which a whole subtree still gets mirrored.
    ///
    /// The root itself is never excluded — a sync of an ignored directory
    /// mirrors it, which is what the client asked for.
    pub fn matched(&mut self, rel: &str, is_dir: bool) -> bool {
        if rel.is_empty() {
            return false;
        }
        if self.exclude_git && rel.split('/').any(|c| c == GIT_DIR_NAME) {
            return true;
        }
        if self.overrides.is_empty() && self.base.is_empty() && self.file_names.is_empty() {
            return false;
        }
        // An excluded directory excludes everything below it (git's rule,
        // which is also why no deeper negation can undo it), and that
        // verdict is memoized — so this is one map lookup per ancestor and
        // one glob probe for the entry itself.
        let parent = crate::parent_wire(rel).unwrap_or("");
        if self.dir_excluded(parent) {
            return true;
        }
        let Some(abs) = crate::resolve_wire_path(&self.root, rel) else {
            return false;
        };
        self.decide(parent, &abs, is_dir)
    }

    /// Whether a directory is excluded, itself or through an ancestor,
    /// memoized. Recursion terminates at the root, which never is.
    fn dir_excluded(&mut self, dir: &str) -> bool {
        if dir.is_empty() {
            return false;
        }
        if let Some(known) = self.dir_verdicts.get(dir) {
            return *known;
        }
        let parent = crate::parent_wire(dir).unwrap_or("");
        let excluded = self.dir_excluded(parent)
            || crate::resolve_wire_path(&self.root, dir)
                .is_some_and(|abs| self.decide(parent, &abs, true));
        self.trim_caches();
        self.dir_verdicts.insert(dir.to_string(), excluded);
        excluded
    }

    /// Keep the memo tables bounded. They only ever cache what the
    /// filesystem says, so dropping them is always safe and costs re-reads
    /// — and dropping all three together keeps them consistent with each
    /// other.
    fn trim_caches(&mut self) {
        if self.dir_verdicts.len() < MAX_CACHED_DIRS
            && self.stacks.len() < MAX_CACHED_DIRS
            && self.per_dir.len() < MAX_CACHED_DIRS
        {
            return;
        }
        self.per_dir.clear();
        self.stacks.clear();
        self.dir_verdicts.clear();
    }

    /// One level's verdict: client patterns first, then the directory
    /// stack from deepest to shallowest. A whitelist stops the search for
    /// *this* component without whitelisting its children.
    fn decide(&mut self, parent_dir: &str, abs: &Path, is_dir: bool) -> bool {
        match self.overrides.matched(abs, is_dir) {
            Match::Ignore(_) => return true,
            Match::Whitelist(_) => return false,
            Match::None => {}
        }
        let stack = self.stack_for(parent_dir);
        for matcher in stack.iter().rev() {
            match matcher.matched(abs, is_dir) {
                Match::Ignore(_) => return true,
                Match::Whitelist(_) => return false,
                Match::None => {}
            }
        }
        false
    }

    /// Matchers applying inside `dir` (wire path, `""` = root), ascending
    /// precedence: the enclosing repository's `base`, then every
    /// per-directory ignore file from that repository's top down to `dir`
    /// inclusive.
    fn stack_for(&mut self, dir: &str) -> Arc<Vec<Arc<Gitignore>>> {
        if let Some(cached) = self.stacks.get(dir) {
            return cached.clone();
        }
        let mut stack = match self.nested_repo_base(dir) {
            // A repository nested inside the root is its own scope: git
            // does not apply an outer repository's rules inside an inner
            // one, so the stack restarts here rather than inheriting.
            Some(fresh) => fresh,
            // Recurse on the parent so a deep first touch memoizes the
            // whole chain rather than rebuilding it per level.
            None => match crate::parent_wire(dir) {
                Some(parent) => (*self.stack_for(parent)).clone(),
                None => self.base.clone(),
            },
        };
        if let Some(matcher) = self.dir_matcher(dir) {
            stack.push(matcher);
        }
        let stack = Arc::new(stack);
        self.stacks.insert(dir.to_string(), stack.clone());
        stack
    }

    /// A fresh stack when `dir` is the top of a repository nested inside
    /// the sync root: `core.excludesFile` (which every repository
    /// inherits) plus that repository's own `info/exclude`, and nothing
    /// from the outer one. `None` for an ordinary directory.
    fn nested_repo_base(&mut self, dir: &str) -> Option<Vec<Arc<Gitignore>>> {
        if self.file_names.is_empty() || dir.is_empty() {
            return None;
        }
        let abs = crate::resolve_wire_path(&self.root, dir)?;
        let gitdir = gitdir_at(&abs)?;
        let mut base = Vec::new();
        base.extend(self.global.clone());
        push_info_exclude(
            &mut base,
            &mut self.info_excludes,
            &abs,
            &gitdir,
            self.fold_case,
        );
        Some(base)
    }

    /// The ignore file(s) at one directory, compiled once.
    fn dir_matcher(&mut self, dir: &str) -> Option<Arc<Gitignore>> {
        if self.file_names.is_empty() {
            return None;
        }
        if let Some(cached) = self.per_dir.get(dir) {
            return cached.clone();
        }
        let built = crate::resolve_wire_path(&self.root, dir)
            .as_deref()
            .and_then(|abs| build_dir_matcher(abs, &self.file_names, self.fold_case));
        self.trim_caches();
        self.per_dir.insert(dir.to_string(), built.clone());
        built
    }
}

/// Compile the ignore files present in `dir`, `None` when it has none.
fn build_dir_matcher(dir: &Path, names: &[&str], fold_case: bool) -> Option<Arc<Gitignore>> {
    let paths: Vec<PathBuf> = names.iter().map(|n| dir.join(n)).collect();
    build_dir_matcher_from(dir, &paths, fold_case)
}

/// Compile `files` (any that exist) as ignore sources anchored at `dir`.
fn build_dir_matcher_from(
    dir: &Path,
    files: &[PathBuf],
    fold_case: bool,
) -> Option<Arc<Gitignore>> {
    let mut builder = GitignoreBuilder::new(dir);
    builder.case_insensitive(fold_case).ok();
    let mut any = false;
    for file in files {
        if builder.add(file).is_none() {
            any = true;
        }
    }
    if !any {
        return None;
    }
    let matcher = builder.build().ok()?;
    (!matcher.is_empty()).then(|| Arc::new(matcher))
}

/// The wire path of the worktree an `info/exclude` governs: strip the
/// `.git/info/exclude` tail. `None` when `rel` is not of that shape — a
/// gitfile-linked gitdir, whose worktree is elsewhere entirely.
fn repo_top_of_info_exclude(rel: &str) -> Option<&str> {
    let tail = format!("{GIT_DIR_NAME}/info/exclude");
    if rel == tail {
        return Some("");
    }
    rel.strip_suffix(&tail)?.strip_suffix('/')
}

/// Add `gitdir`'s `info/exclude` to `base` (anchored at the worktree `dir`
/// it governs) and record it as an ignore source.
fn push_info_exclude(
    base: &mut Vec<Arc<Gitignore>>,
    seen: &mut std::collections::HashSet<PathBuf>,
    dir: &Path,
    gitdir: &Path,
    fold_case: bool,
) {
    let exclude = gitdir.join("info").join("exclude");
    if let Some(matcher) = build_dir_matcher_from(dir, std::slice::from_ref(&exclude), fold_case) {
        base.push(matcher);
    }
    // Recorded whether or not it exists today: creating one later is the
    // change that has to invalidate the stack.
    seen.insert(exclude);
}

/// `core.ignorecase` from a repository's config.
///
/// git sets it at `init`/`clone` on a case-insensitive filesystem and then
/// folds case when matching ignore rules, so a mirror that did not would
/// exclude a different set of paths than the repository it mirrors. Read
/// with a minimal INI scan rather than a git library: this crate needs one
/// boolean, and pulling in a repository object to get it would put git on
/// the path of every filtered sync — including the ones nowhere near a
/// repository.
fn config_ignorecase(gitdir: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(gitdir.join("config")) else {
        return false;
    };
    let mut in_core = false;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if let Some(section) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            // `[core]` only — a subsection (`[core "x"]`) is a different
            // section and carries no `ignorecase`.
            in_core = section.trim().eq_ignore_ascii_case("core");
            continue;
        }
        if !in_core {
            continue;
        }
        // git's boolean spelling: a valueless key is true, as is anything
        // but the explicit falses.
        let (key, value) = match line.split_once('=') {
            Some((key, value)) => (key.trim(), value.trim()),
            None => (line, "true"),
        };
        if key.eq_ignore_ascii_case("ignorecase") {
            return !matches!(
                value.to_ascii_lowercase().as_str(),
                "false" | "no" | "off" | "0" | ""
            );
        }
    }
    false
}

/// `dir`'s own gitdir if `dir` is a repository top, following a `.git`
/// *file* (submodule / linked worktree) to the directory it names.
fn gitdir_at(dir: &Path) -> Option<PathBuf> {
    let candidate = dir.join(GIT_DIR_NAME);
    match std::fs::metadata(&candidate) {
        Ok(md) if md.is_dir() => Some(candidate),
        Ok(md) if md.is_file() => {
            let text = std::fs::read_to_string(&candidate).ok()?;
            let target = Path::new(text.strip_prefix("gitdir:")?.trim());
            Some(if target.is_absolute() {
                target.to_path_buf()
            } else {
                dir.join(target)
            })
        }
        _ => None,
    }
}

/// The top of the worktree enclosing `root`, searched strictly above it.
/// `None` when `root` is not inside one — a sync outside a repository
/// reads no ignore files above itself.
fn enclosing_worktree_top(root: &Path) -> Option<PathBuf> {
    std::iter::successors(root.parent(), |d| d.parent())
        .find(|dir| gitdir_at(dir).is_some())
        .map(Path::to_path_buf)
}

/// Directories from `top` down to `root`'s parent, shallowest first —
/// `root`'s own ignore files are the per-directory stack's first level, so
/// they are not included here.
fn ancestors_between(top: &Path, root: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::iter::successors(root.parent(), |d| d.parent())
        .take_while(|dir| dir.starts_with(top))
        .map(Path::to_path_buf)
        .collect();
    dirs.reverse();
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(patterns: &[&str], ignore_files: bool, exclude_git: bool) -> IgnoreSpec {
        IgnoreSpec {
            gitignore: ignore_files,
            dot_ignore: ignore_files,
            exclude_git,
            patterns: patterns.iter().map(|p| p.to_string()).collect(),
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "blit-fssync-ign-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn patterns_normalize_and_drop_noise() {
        let parsed = IgnoreSpec::parse_patterns("target\n\n # comment\n  node_modules  \r\n!keep");
        assert_eq!(parsed, vec!["target", "node_modules", "!keep"]);
        // Two specs differing only in blank lines share a root.
        assert_eq!(
            IgnoreSpec::parse_patterns("a\n\nb"),
            IgnoreSpec::parse_patterns("a\nb")
        );
    }

    #[test]
    fn exclude_git_is_a_pure_name_filter() {
        let dir = temp_dir("git");
        let mut ign = Ignores::new(&dir, &spec(&[], false, true));
        assert!(ign.matched(".git", true));
        assert!(ign.matched(".git/config", false));
        assert!(ign.matched("sub/.git", false), "a gitfile too");
        assert!(!ign.matched(".gitignore", false));
        assert!(!ign.matched("git", true));
        assert!(!ign.matched("", true), "the root is never excluded");
    }

    #[test]
    fn client_patterns_outrank_the_ignore_files() {
        let dir = temp_dir("over");
        std::fs::write(dir.join(".gitignore"), "build/\n").unwrap();
        std::fs::create_dir_all(dir.join("build")).unwrap();
        let mut ign = Ignores::new(&dir, &spec(&["!build/", "*.log"], true, false));
        assert!(!ign.matched("build", true), "re-included by the client");
        assert!(ign.matched("a.log", false));
    }

    #[test]
    fn nested_ignore_files_stack_deepest_first() {
        let dir = temp_dir("stack");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join(".gitignore"), "*.tmp\n").unwrap();
        std::fs::write(dir.join("sub/.gitignore"), "!keep.tmp\n").unwrap();
        let mut ign = Ignores::new(&dir, &spec(&[], true, false));
        assert!(ign.matched("a.tmp", false));
        assert!(ign.matched("sub/other.tmp", false));
        assert!(!ign.matched("sub/keep.tmp", false), "deeper file wins");
    }

    #[test]
    fn an_excluded_directory_excludes_its_subtree() {
        let dir = temp_dir("subtree");
        std::fs::create_dir_all(dir.join("target/debug")).unwrap();
        std::fs::write(dir.join(".gitignore"), "target/\n!target/debug/keep\n").unwrap();
        let mut ign = Ignores::new(&dir, &spec(&[], true, false));
        assert!(ign.matched("target", true));
        assert!(ign.matched("target/debug", true));
        // git's rule: no negation resurrects a file under an excluded dir.
        assert!(ign.matched("target/debug/keep", false));
    }

    /// A directory-only pattern matches what this sync *enumerates* as a
    /// directory, which includes a symlink to one — git calls that a file,
    /// but git also does not descend it. Matching git here would leave one
    /// hole: `build/` could not exclude a symlinked `build`, and the whole
    /// subtree behind it would still be mirrored.
    #[test]
    fn a_directory_only_pattern_matches_directories_including_symlinked_ones() {
        let dir = temp_dir("dironly");
        let mut ign = Ignores::new(&dir, &spec(&["build/"], false, false));
        assert!(ign.matched("build", true));
        assert!(!ign.matched("build", false));
    }

    #[test]
    fn invalidate_picks_up_an_edited_ignore_file() {
        let dir = temp_dir("edit");
        std::fs::write(dir.join(".gitignore"), "a.txt\n").unwrap();
        let mut ign = Ignores::new(&dir, &spec(&[], true, false));
        assert!(ign.matched("a.txt", false));
        assert!(!ign.matched("b.txt", false));
        std::fs::write(dir.join(".gitignore"), "b.txt\n").unwrap();
        assert!(ign.matched("a.txt", false), "still the memoized stack");
        ign.invalidate();
        assert!(!ign.matched("a.txt", false));
        assert!(ign.matched("b.txt", false));
        let mut src = |rel: &str| ign.source_affects_rules(&dir.join(rel), rel);
        assert!(src(".gitignore"));
        assert!(src("sub/.ignore"));
        assert!(!src("sub/notes.txt"));
    }

    #[test]
    fn ignore_files_above_the_root_still_apply() {
        let top = temp_dir("parent");
        std::fs::create_dir_all(top.join(".git")).unwrap();
        std::fs::create_dir_all(top.join("crates")).unwrap();
        std::fs::write(top.join(".gitignore"), "*.bak\n").unwrap();
        std::fs::write(top.join(".git/info-placeholder"), "").unwrap();
        let root = top.join("crates");
        let mut ign = Ignores::new(&root, &spec(&[], true, false));
        assert!(
            ign.matched("a.bak", false),
            "inherited from the worktree top"
        );
    }

    /// An enclosing worktree's `info/exclude` is anchored at the worktree
    /// top, which is what git anchors it at — not at the subdirectory being
    /// synced. Anchoring it at the root instead excluded `<root>/build` for a
    /// `/build` rule that only ever meant `<top>/build`, and let the path git
    /// really excludes through. Only anchored patterns show it, which is why
    /// the `*.bak` case above passes either way.
    #[test]
    fn an_enclosing_info_exclude_is_anchored_at_the_worktree_top() {
        let top = temp_dir("infoexcl-anchor");
        std::fs::create_dir_all(top.join(".git/info")).unwrap();
        std::fs::create_dir_all(top.join("crates/build")).unwrap();
        std::fs::create_dir_all(top.join("build")).unwrap();
        // Anchored (leading slash) and embedded-slash rules, both of which
        // git reads relative to the worktree top.
        std::fs::write(top.join(".git/info/exclude"), "/build\ncrates/gen\n").unwrap();

        // Syncing the whole worktree: `/build` is the top's own.
        let mut whole = Ignores::new(&top, &spec(&[], true, false));
        assert!(whole.matched("build", true));
        assert!(!whole.matched("crates/build", true));
        assert!(whole.matched("crates/gen", true));

        // Syncing `crates`: both rules still resolve against the top, so
        // `/build` names a path outside this sync and `crates/gen` names one
        // inside it — the sync sees exactly what git would ignore, not what
        // the same text would mean if it were re-anchored at the root.
        let root = top.join("crates");
        let mut sub = Ignores::new(&root, &spec(&[], true, false));
        assert!(
            !sub.matched("build", true),
            "/build in the top's info/exclude is <top>/build, not <root>/build"
        );
        assert!(
            sub.matched("gen", true),
            "crates/gen resolves to this root's gen"
        );
    }

    #[test]
    fn info_exclude_is_honored_and_is_a_source() {
        let dir = temp_dir("infoexcl");
        std::fs::create_dir_all(dir.join(".git/info")).unwrap();
        std::fs::write(dir.join(".git/info/exclude"), "secret*\n").unwrap();
        let mut ign = Ignores::new(&dir, &spec(&[], true, true));
        assert!(ign.matched("secret.txt", false));
        assert!(ign.is_source_abs(&dir.join(".git/info/exclude")));
        assert!(
            ign.source_affects_rules(&dir.join(".git/info/exclude"), ".git/info/exclude"),
            "the root's own info/exclude matters even though .git is excluded"
        );
    }

    /// A repository nested inside the root is its own scope: git does not
    /// apply an outer repository's rules inside an inner one, and neither
    /// does this. The outer repo can still exclude the nested directory
    /// itself — that entry is decided by its parent's stack.
    #[test]
    fn a_nested_repository_starts_a_fresh_stack() {
        let dir = temp_dir("nested");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".gitignore"), "*.rs\n").unwrap();
        std::fs::create_dir_all(dir.join("vendor/lib/.git/info")).unwrap();
        std::fs::write(dir.join("vendor/lib/.gitignore"), "*.txt\n").unwrap();
        std::fs::write(dir.join("vendor/lib/.git/info/exclude"), "local-*\n").unwrap();
        let mut ign = Ignores::new(&dir, &spec(&[], true, false));

        assert!(ign.matched("a.rs", false), "the outer rule, outside");
        assert!(
            !ign.matched("vendor/lib/a.rs", false),
            "an outer repository's rules do not reach into a nested one"
        );
        assert!(ign.matched("vendor/lib/b.txt", false), "the inner rule");
        assert!(
            ign.matched("vendor/lib/local-notes.md", false),
            "the nested repository's own info/exclude"
        );
        assert!(
            ign.is_source_abs(&dir.join("vendor/lib/.git/info/exclude")),
            "editing it has to invalidate the stack"
        );
        // Directories between the two tops still belong to the outer repo.
        assert!(ign.matched("vendor/c.rs", false));
    }

    /// A root that is itself a repository top inherits nothing from a
    /// repository it happens to sit inside.
    #[test]
    fn a_root_that_is_a_repo_top_ignores_the_outer_repo() {
        let outer = temp_dir("outertop");
        std::fs::create_dir_all(outer.join(".git")).unwrap();
        std::fs::write(outer.join(".gitignore"), "*.bak\n").unwrap();
        let inner = outer.join("inner");
        std::fs::create_dir_all(inner.join(".git")).unwrap();
        let mut ign = Ignores::new(&inner, &spec(&[], true, false));
        assert!(!ign.matched("a.bak", false));

        // …while a plain subdirectory of the outer repo does inherit it.
        let plain = outer.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let mut ign = Ignores::new(&plain, &spec(&[], true, false));
        assert!(ign.matched("a.bak", false));
    }

    /// A pure exclusion list commutes, so equivalent specs normalize to
    /// one key and share a root; a list with a negation in it does not,
    /// since gitignore is last-match-wins.
    #[test]
    fn pattern_order_normalizes_only_when_it_carries_no_meaning() {
        assert_eq!(
            IgnoreSpec::parse_patterns("b\na\nb"),
            IgnoreSpec::parse_patterns("a\nb"),
            "reordered and duplicated exclusions are the same request"
        );
        assert_eq!(
            IgnoreSpec::parse_patterns("*.log\n!keep.log"),
            vec!["*.log", "!keep.log"],
            "a negation pins the order — sorting it would invert the rule"
        );
        assert_ne!(
            IgnoreSpec::parse_patterns("!keep.log\n*.log"),
            IgnoreSpec::parse_patterns("*.log\n!keep.log"),
        );
    }

    /// The two ignore-file kinds are selectable independently, and only
    /// `.gitignore` brings git's repository-wide sources with it.
    #[test]
    fn the_two_ignore_file_kinds_are_independent() {
        let dir = temp_dir("kinds");
        std::fs::create_dir_all(dir.join(".git/info")).unwrap();
        std::fs::write(dir.join(".git/info/exclude"), "excluded-*\n").unwrap();
        std::fs::write(dir.join(".gitignore"), "from-git\n").unwrap();
        std::fs::write(dir.join(".ignore"), "from-dot\n").unwrap();

        let only_git = IgnoreSpec {
            gitignore: true,
            ..Default::default()
        };
        let mut ign = Ignores::new(&dir, &only_git);
        assert!(ign.matched("from-git", false));
        assert!(!ign.matched("from-dot", false));
        assert!(ign.matched("excluded-x", false), "info/exclude is git's");

        let only_dot = IgnoreSpec {
            dot_ignore: true,
            ..Default::default()
        };
        let mut ign = Ignores::new(&dir, &only_dot);
        assert!(!ign.matched("from-git", false));
        assert!(ign.matched("from-dot", false));
        assert!(
            !ign.matched("excluded-x", false),
            "`.ignore` alone reads no git sources"
        );
        assert!(
            !ign.source_affects_rules(&dir.join(".gitignore"), ".gitignore"),
            "a file this spec never reads is not one of its sources"
        );
    }

    /// `core.ignorecase` is what git matches by on a case-insensitive
    /// filesystem, so a mirror that ignored it would exclude a different
    /// set of paths than the repository it mirrors.
    #[test]
    fn core_ignorecase_folds_the_matchers() {
        let dir = temp_dir("icase");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".gitignore"), "Build/\n*.LOG\n").unwrap();

        std::fs::write(
            dir.join(".git/config"),
            "[core]\n\trepositoryformatversion = 0\n",
        )
        .unwrap();
        let mut ign = Ignores::new(&dir, &spec(&["Vendor"], true, false));
        assert!(!ign.matched("build", true), "case-sensitive by default");
        assert!(!ign.matched("a.log", false));
        assert!(!ign.matched("vendor", true));

        std::fs::write(dir.join(".git/config"), "[core]\n\tignorecase = true\n").unwrap();
        let mut ign = Ignores::new(&dir, &spec(&["Vendor"], true, false));
        assert!(ign.matched("build", true));
        assert!(ign.matched("a.log", false));
        assert!(ign.matched("vendor", true), "client patterns fold too");

        // git's boolean spellings, and a section that is not `[core]`.
        std::fs::write(dir.join(".git/config"), "[core]\nignorecase = false\n").unwrap();
        assert!(!Ignores::new(&dir, &spec(&[], true, false)).matched("build", true));
        std::fs::write(dir.join(".git/config"), "[core]\nignorecase\n").unwrap();
        assert!(Ignores::new(&dir, &spec(&[], true, false)).matched("build", true));
        std::fs::write(dir.join(".git/config"), "[other]\nignorecase = true\n").unwrap();
        assert!(!Ignores::new(&dir, &spec(&[], true, false)).matched("build", true));
    }

    #[test]
    fn nothing_configured_matches_nothing() {
        let dir = temp_dir("empty");
        assert!(IgnoreSpec::default().is_empty());
        let mut ign = Ignores::new(&dir, &IgnoreSpec::default());
        assert!(!ign.matched("anything/at/all", false));
        assert!(!ign.source_affects_rules(&dir.join(".gitignore"), ".gitignore"));
    }

    /// An ignore file inside an already-excluded directory is never read,
    /// so writing one changes nothing and must not cost a rebuild plus a
    /// full re-enumeration. `npm install` writes thousands of them.
    #[test]
    fn an_ignore_file_under_an_excluded_directory_changes_no_rules() {
        let dir = temp_dir("deadsrc");
        std::fs::create_dir_all(dir.join(".git/info")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join(".gitignore"), "node_modules/\n").unwrap();
        let mut ign = Ignores::new(&dir, &spec(&[], true, true));
        let affects = |ign: &mut Ignores, rel: &str| ign.source_affects_rules(&dir.join(rel), rel);

        assert!(affects(&mut ign, ".gitignore"), "the root's own");
        assert!(affects(&mut ign, "src/.gitignore"), "a live subdirectory");
        assert!(
            !affects(&mut ign, "node_modules/.gitignore"),
            "inside an excluded directory: never read, so never a rebuild"
        );
        assert!(!affects(&mut ign, "node_modules/pkg/.gitignore"));
        assert!(!affects(&mut ign, "src/notes.txt"), "not a source at all");

        // A nested repository's info/exclude follows its *worktree*, not
        // its own directory — which `EXCLUDE_GIT` always excludes.
        std::fs::create_dir_all(dir.join("vendor/lib/.git/info")).unwrap();
        assert!(affects(&mut ign, "vendor/lib/.git/info/exclude"));
        assert!(
            !affects(&mut ign, "node_modules/pkg/.git/info/exclude"),
            "a repository inside an excluded directory is not indexed either"
        );
    }
}
