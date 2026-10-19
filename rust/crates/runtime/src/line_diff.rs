//! Line diffs (Myers' algorithm) and unified-diff hunks, for the patches the
//! write and edit tools report and the diffs claw shows.

use crate::file_ops::StructuredPatchHunk;

/// Context lines kept around each change, as `git diff` does.
pub const DEFAULT_CONTEXT: usize = 3;

/// Above this many differing lines the middle of the files is reported as
/// replaced whole: the search would cost too much time and memory.
const MAX_EDIT_DISTANCE: usize = 4_000;

/// One step of turning the old lines into the new ones (0-based indexes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffOp {
    Equal { old: usize, new: usize },
    Delete { old: usize },
    Insert { new: usize },
}

/// The shortest edit script from `old` to `new`.
#[must_use]
pub fn diff_lines(old: &[&str], new: &[&str]) -> Vec<DiffOp> {
    let prefix = old
        .iter()
        .zip(new)
        .take_while(|(left, right)| left == right)
        .count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(left, right)| left == right)
        .count();
    let old_middle = &old[prefix..old.len() - suffix];
    let new_middle = &new[prefix..new.len() - suffix];

    let mut ops: Vec<DiffOp> = (0..prefix)
        .map(|index| DiffOp::Equal {
            old: index,
            new: index,
        })
        .collect();
    let middle = myers(old_middle, new_middle).unwrap_or_else(|| {
        (0..old_middle.len())
            .map(|old| DiffOp::Delete { old })
            .chain((0..new_middle.len()).map(|new| DiffOp::Insert { new }))
            .collect()
    });
    ops.extend(middle.into_iter().map(|op| match op {
        DiffOp::Equal { old, new } => DiffOp::Equal {
            old: old + prefix,
            new: new + prefix,
        },
        DiffOp::Delete { old } => DiffOp::Delete { old: old + prefix },
        DiffOp::Insert { new } => DiffOp::Insert { new: new + prefix },
    }));
    let (old_tail, new_tail) = (old.len() - suffix, new.len() - suffix);
    ops.extend((0..suffix).map(|index| DiffOp::Equal {
        old: old_tail + index,
        new: new_tail + index,
    }));
    ops
}

/// Myers' O((N+M)D) search; `None` past [`MAX_EDIT_DISTANCE`].
#[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
fn myers(old: &[&str], new: &[&str]) -> Option<Vec<DiffOp>> {
    let (n, m) = (old.len() as isize, new.len() as isize);
    let max = (n + m) as usize;
    if max == 0 {
        return Some(Vec::new());
    }
    let offset = max as isize + 1;
    let mut v = vec![0_isize; 2 * max + 3];
    // `trace[d]` holds the furthest x of diagonals -d..=d before step d.
    let mut trace: Vec<Vec<isize>> = Vec::new();
    for d in 0..=max.min(MAX_EDIT_DISTANCE) as isize {
        trace.push(v[(offset - d) as usize..=(offset + d) as usize].to_vec());
        let mut k = -d;
        while k <= d {
            let at = |k: isize| v[(offset + k) as usize];
            let mut x = if k == -d || (k != d && at(k - 1) < at(k + 1)) {
                at(k + 1)
            } else {
                at(k - 1) + 1
            };
            let mut y = x - k;
            while x < n && y < m && old[x as usize] == new[y as usize] {
                x += 1;
                y += 1;
            }
            v[(offset + k) as usize] = x;
            if x >= n && y >= m {
                return Some(backtrack(&trace, n, m));
            }
            k += 2;
        }
    }
    None
}

#[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
fn backtrack(trace: &[Vec<isize>], n: isize, m: isize) -> Vec<DiffOp> {
    let mut ops = Vec::new();
    let (mut x, mut y) = (n, m);
    for d in (1..trace.len() as isize).rev() {
        let before = &trace[d as usize];
        let at = |k: isize| before[(k + d) as usize];
        let k = x - y;
        let previous_k = if k == -d || (k != d && at(k - 1) < at(k + 1)) {
            k + 1
        } else {
            k - 1
        };
        let previous_x = at(previous_k);
        let previous_y = previous_x - previous_k;
        while x > previous_x && y > previous_y {
            x -= 1;
            y -= 1;
            ops.push(DiffOp::Equal {
                old: x as usize,
                new: y as usize,
            });
        }
        if previous_k == k + 1 {
            y -= 1;
            ops.push(DiffOp::Insert { new: y as usize });
        } else {
            x -= 1;
            ops.push(DiffOp::Delete { old: x as usize });
        }
    }
    while x > 0 && y > 0 {
        x -= 1;
        y -= 1;
        ops.push(DiffOp::Equal {
            old: x as usize,
            new: y as usize,
        });
    }
    ops.reverse();
    ops
}

/// Unified-diff hunks (`" "`, `"-"`, `"+"` lines) from `original` to
/// `updated`, with `context` unchanged lines around each change.
#[must_use]
pub fn unified_hunks(original: &str, updated: &str, context: usize) -> Vec<StructuredPatchHunk> {
    let old: Vec<&str> = original.lines().collect();
    let new: Vec<&str> = updated.lines().collect();
    let ops = diff_lines(&old, &new);
    let changes: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, op)| !matches!(op, DiffOp::Equal { .. }))
        .map(|(index, _)| index)
        .collect();
    let mut hunks = Vec::new();
    let mut index = 0;
    while index < changes.len() {
        // Changes closer than twice the context share one hunk.
        let first = changes[index];
        let mut last = first;
        while index + 1 < changes.len() && changes[index + 1] - last <= 2 * context + 1 {
            index += 1;
            last = changes[index];
        }
        index += 1;
        let start = first.saturating_sub(context);
        let end = (last + context + 1).min(ops.len());
        hunks.push(hunk(&ops[start..end], &old, &new));
    }
    hunks
}

fn hunk(ops: &[DiffOp], old: &[&str], new: &[&str]) -> StructuredPatchHunk {
    // 1-based starts; 0 for a side with no lines (a created or emptied file).
    let old_start = ops
        .iter()
        .find_map(|op| match op {
            DiffOp::Equal { old, .. } | DiffOp::Delete { old } => Some(old + 1),
            DiffOp::Insert { .. } => None,
        })
        .unwrap_or(0);
    let new_start = ops
        .iter()
        .find_map(|op| match op {
            DiffOp::Equal { new, .. } | DiffOp::Insert { new } => Some(new + 1),
            DiffOp::Delete { .. } => None,
        })
        .unwrap_or(0);
    let mut lines = Vec::with_capacity(ops.len());
    let (mut old_lines, mut new_lines) = (0, 0);
    for op in ops {
        match *op {
            DiffOp::Equal { old: index, .. } => {
                lines.push(format!(" {}", old[index]));
                old_lines += 1;
                new_lines += 1;
            }
            DiffOp::Delete { old: index } => {
                lines.push(format!("-{}", old[index]));
                old_lines += 1;
            }
            DiffOp::Insert { new: index } => {
                lines.push(format!("+{}", new[index]));
                new_lines += 1;
            }
        }
    }
    StructuredPatchHunk {
        old_start,
        old_lines,
        new_start,
        new_lines,
        lines,
    }
}

#[cfg(test)]
mod tests {
    use super::{diff_lines, unified_hunks, DiffOp};

    fn apply(old: &[&str], new: &[&str], ops: &[DiffOp]) -> Vec<String> {
        ops.iter()
            .filter_map(|op| match *op {
                DiffOp::Equal { old: index, .. } => Some(old[index].to_string()),
                DiffOp::Insert { new: index } => Some(new[index].to_string()),
                DiffOp::Delete { .. } => None,
            })
            .collect()
    }

    #[test]
    fn edit_script_rebuilds_the_new_lines_and_is_minimal() {
        let old = ["a", "b", "c", "a", "b", "b", "a"];
        let new = ["c", "b", "a", "b", "a", "c"];
        let ops = diff_lines(&old, &new);
        assert_eq!(apply(&old, &new, &ops), new);
        let changes = ops
            .iter()
            .filter(|op| !matches!(op, DiffOp::Equal { .. }))
            .count();
        assert_eq!(changes, 5, "Myers' classic example has distance 5");
    }

    #[test]
    fn identical_and_empty_inputs() {
        assert!(unified_hunks("same\n", "same\n", 3).is_empty());
        let created = unified_hunks("", "one\ntwo\n", 3);
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].lines, vec!["+one", "+two"]);
        assert_eq!((created[0].old_start, created[0].old_lines), (0, 0));
        assert_eq!((created[0].new_start, created[0].new_lines), (1, 2));
    }

    #[test]
    fn hunks_have_context_and_line_numbers() {
        let old: String = (1..=20).map(|n| format!("line {n}\n")).collect();
        let new = old
            .replace("line 5\n", "line five\n")
            .replace("line 16\n", "");
        let hunks = unified_hunks(&old, &new, 3);
        assert_eq!(hunks.len(), 2, "changes 11 lines apart stay separate");
        assert_eq!((hunks[0].old_start, hunks[0].new_start), (2, 2));
        assert_eq!(
            hunks[0].lines,
            vec![
                " line 2",
                " line 3",
                " line 4",
                "-line 5",
                "+line five",
                " line 6",
                " line 7",
                " line 8"
            ]
        );
        assert_eq!((hunks[1].old_start, hunks[1].old_lines), (13, 7));
        assert_eq!((hunks[1].new_start, hunks[1].new_lines), (13, 6));
        assert!(hunks[1].lines.contains(&"-line 16".to_string()));
    }

    #[test]
    fn nearby_changes_share_a_hunk() {
        let old = "a\nb\nc\nd\ne\nf\ng\n";
        let new = "A\nb\nc\nd\ne\nf\nG\n";
        assert_eq!(unified_hunks(old, new, 3).len(), 1);
    }
}
