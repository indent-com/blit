//! Git-worktree discovery and durable port assignment.
//!
//! Protocol I/O stays in `main.rs`; these are the deterministic pieces that
//! can be exercised on the host without a server.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const MAIN_ID: &str = "main";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Worktree {
    pub id: String,
    pub path: String,
    pub is_main: bool,
}

/// One concrete allocation. The span is stored with the base so a worktree
/// that is temporarily absent still reserves the whole block it used.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PortLease {
    pub base: i64,
    pub span: u32,
}

/// All generated worktree allocations for one muster server.
///
/// The outer key is the source file's stem and the inner key is Git's stable
/// administrative worktree id. Removed worktrees deliberately remain here:
/// their neighbours never move, and a returning worktree gets its old ports.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct PortLedger {
    #[serde(default)]
    pub sources: BTreeMap<String, BTreeMap<String, PortLease>>,
}

/// Join a worktree root and its repository-relative stack path without
/// canonicalizing either. The server owns path resolution; retaining the
/// spelling makes diagnostics match the pointer the user wrote.
pub fn stack_path(root: &str, relative: &str) -> String {
    format!(
        "{}/{}",
        root.trim_end_matches(['/', '\\']),
        relative.trim_matches(['/', '\\'])
    )
}

/// A worktree source must name a path *inside* each checkout. Absolute paths
/// and parent traversal would turn one source into an unrelated-directory
/// discovery mechanism.
pub fn validate_stack_path(path: &str) -> Result<(), String> {
    if path.is_empty()
        || path.starts_with(['/', '\\', '~'])
        || path
            .split(['/', '\\'])
            .any(|part| part.is_empty() || part == "..")
    {
        return Err("stack must be a non-empty relative path without '..'".into());
    }
    Ok(())
}

/// Turn the filtered `.git` mirror into the current worktree set.
///
/// `files` contains only `worktrees/<git-id>/gitdir`. Git does not represent
/// the main worktree there, so the explicit root is always first.
pub fn discover(main: &str, files: &BTreeMap<String, Vec<u8>>) -> Vec<Worktree> {
    let main = main.trim_end_matches(['/', '\\']).to_string();
    let mut out = vec![Worktree {
        id: MAIN_ID.into(),
        path: main.clone(),
        is_main: true,
    }];
    let mut paths = BTreeSet::from([main]);

    for (file, content) in files {
        let Some(rest) = file.strip_prefix("worktrees/") else {
            continue;
        };
        let Some(id) = rest.strip_suffix("/gitdir") else {
            continue;
        };
        if id.is_empty() || id.contains('/') {
            continue;
        }
        let Ok(text) = std::str::from_utf8(content) else {
            continue;
        };
        let gitfile = text.trim();
        let Some(path) = gitfile
            .strip_suffix("/.git")
            .or_else(|| gitfile.strip_suffix("\\.git"))
        else {
            continue;
        };
        let path = path.trim_end_matches(['/', '\\']).to_string();
        if path.is_empty() || !paths.insert(path.clone()) {
            continue;
        }
        out.push(Worktree {
            id: id.to_string(),
            path,
            is_main: false,
        });
    }
    out
}

pub fn instance_name(source: &str, worktree: &Worktree) -> String {
    if worktree.is_main {
        source.to_string()
    } else {
        format!("{source}-{}", worktree.id)
    }
}

fn overlaps(a: PortLease, b: PortLease) -> bool {
    a.base < b.base + i64::from(b.span.max(1)) && b.base < a.base + i64::from(a.span.max(1))
}

fn valid(lease: PortLease) -> bool {
    let span = i64::from(lease.span.max(1));
    lease.base >= 1 && lease.base <= 65_535 && lease.base + span - 1 <= 65_535
}

/// Assign concrete blocks to the active worktrees of one source.
///
/// The main worktree always wins `start`, even over an old conflicting lease;
/// overlap reporting will name any explicit configuration that already owns
/// that block. Linked worktrees retain a valid old lease or take the first free
/// block above `start`. Leases for absent worktrees remain reservations.
pub fn assign_ports(
    source: &str,
    worktrees: &[Worktree],
    start: i64,
    span: u32,
    ledger: &mut PortLedger,
    explicit: &[(i64, u32)],
) -> Result<BTreeMap<String, i64>, String> {
    let span = span.max(1);
    let fixed = PortLease { base: start, span };
    if !valid(fixed) {
        return Err(format!("port block {start}+{span} is outside 1..65535"));
    }

    let active: BTreeSet<&str> = worktrees.iter().map(|w| w.id.as_str()).collect();
    let mut occupied: Vec<PortLease> = explicit
        .iter()
        .map(|(base, span)| PortLease {
            base: *base,
            span: (*span).max(1),
        })
        .collect();
    for (other_source, leases) in &ledger.sources {
        for (id, lease) in leases {
            if other_source != source || !active.contains(id.as_str()) {
                occupied.push(*lease);
            }
        }
    }

    let leases = ledger.sources.entry(source.to_string()).or_default();
    let mut assigned = BTreeMap::new();
    // The main lease is an invariant, not a candidate: establish it first.
    if let Some(main) = worktrees.iter().find(|worktree| worktree.is_main) {
        leases.insert(main.id.clone(), fixed);
        occupied.push(fixed);
        assigned.insert(main.id.clone(), fixed.base);
    }

    // Reserve every still-valid active lease before allocating newcomers. If
    // allocation followed lexical worktree order, inserting an earlier name
    // could steal a later name's old block and move the very lease this ledger
    // exists to keep stable.
    for worktree in worktrees {
        if worktree.is_main {
            continue;
        }
        if let Some(old) = leases.get(&worktree.id).copied().filter(|old| {
            old.span.max(1) == span && valid(*old) && !occupied.iter().any(|o| overlaps(*old, *o))
        }) {
            occupied.push(old);
            assigned.insert(worktree.id.clone(), old.base);
        }
    }

    for worktree in worktrees {
        if assigned.contains_key(&worktree.id) {
            continue;
        }
        let mut lease = fixed;
        while occupied.iter().any(|other| overlaps(lease, *other)) {
            lease.base += i64::from(span);
            if !valid(lease) {
                return Err(format!(
                    "no free {span}-port block at or above {start} for {}",
                    worktree.id
                ));
            }
        }
        leases.insert(worktree.id.clone(), lease);
        occupied.push(lease);
        assigned.insert(worktree.id.clone(), lease.base);
    }
    Ok(assigned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wt(id: &str, is_main: bool) -> Worktree {
        Worktree {
            id: id.into(),
            path: format!("/src/{id}"),
            is_main,
        }
    }

    #[test]
    fn discovers_main_and_linked_worktrees_from_gitdir_files() {
        let files = BTreeMap::from([
            (
                "worktrees/epic/gitdir".into(),
                b"/src/blit-wt/epic/.git\n".to_vec(),
            ),
            ("worktrees/epic/HEAD".into(), b"ignored".to_vec()),
            ("objects/nope/gitdir".into(), b"ignored".to_vec()),
        ]);
        assert_eq!(
            discover("/src/blit/", &files),
            vec![
                Worktree {
                    id: "main".into(),
                    path: "/src/blit".into(),
                    is_main: true,
                },
                Worktree {
                    id: "epic".into(),
                    path: "/src/blit-wt/epic".into(),
                    is_main: false,
                },
            ]
        );
    }

    #[test]
    fn main_is_exact_and_linked_leases_stay_stable() {
        let mut ledger = PortLedger::default();
        let first = assign_ports(
            "blit",
            &[wt(MAIN_ID, true), wt("z", false)],
            10_000,
            4,
            &mut ledger,
            &[],
        )
        .unwrap();
        assert_eq!(first[MAIN_ID], 10_000);
        assert_eq!(first["z"], 10_004);

        let second = assign_ports(
            "blit",
            &[wt(MAIN_ID, true), wt("a", false), wt("z", false)],
            10_000,
            4,
            &mut ledger,
            &[],
        )
        .unwrap();
        assert_eq!(second[MAIN_ID], 10_000);
        assert_eq!(second["z"], 10_004);
        assert_eq!(second["a"], 10_008);
    }

    #[test]
    fn absent_worktrees_keep_their_reservations() {
        let mut ledger = PortLedger::default();
        assign_ports(
            "blit",
            &[wt(MAIN_ID, true), wt("old", false)],
            10_000,
            4,
            &mut ledger,
            &[],
        )
        .unwrap();
        let assigned = assign_ports(
            "blit",
            &[wt(MAIN_ID, true), wt("new", false)],
            10_000,
            4,
            &mut ledger,
            &[],
        )
        .unwrap();
        assert_eq!(assigned["new"], 10_008);
    }

    #[test]
    fn rereading_a_winning_ledger_needs_no_write() {
        let mut ledger = PortLedger::default();
        assign_ports(
            "blit",
            &[wt(MAIN_ID, true), wt("epic", false)],
            10_000,
            4,
            &mut ledger,
            &[],
        )
        .unwrap();
        let winner = ledger.clone();

        let assigned = assign_ports(
            "blit",
            &[wt(MAIN_ID, true), wt("epic", false)],
            10_000,
            4,
            &mut ledger,
            &[],
        )
        .unwrap();

        assert_eq!(assigned[MAIN_ID], 10_000);
        assert_eq!(assigned["epic"], 10_004);
        assert_eq!(ledger, winner);
    }

    #[test]
    fn validates_repository_relative_stack_paths() {
        assert!(validate_stack_path(".blit/muster").is_ok());
        assert!(validate_stack_path("../muster").is_err());
        assert!(validate_stack_path("/tmp/muster").is_err());
    }
}
