//! Line diffs for approval cards.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffKind {
    Context,
    Added,
    Removed,
    /// "... N unchanged lines ..."
    Skip,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    pub kind: DiffKind,
    /// 1-based line number in the old text (context/removed) or new text
    /// (added), if known.
    pub line: Option<usize>,
    pub text: String,
}

impl DiffLine {
    fn new(kind: DiffKind, line: Option<usize>, text: &str) -> Self {
        Self {
            kind,
            line,
            text: text.trim_end_matches(['\r', '\n']).to_owned(),
        }
    }
}

/// Most lines compared with LCS; larger inputs get a summary.
const MAX_LINES: usize = 4000;

/// Unified line diff of `old` -> `new` with `context` unchanged lines around
/// each change. Lines are numbered from `first_line` (1-based).
pub fn line_diff(old: &str, new: &str, context: usize, first_line: usize) -> Vec<DiffLine> {
    let a: Vec<&str> = old.split_inclusive('\n').collect();
    let b: Vec<&str> = new.split_inclusive('\n').collect();
    if a.len() > MAX_LINES || b.len() > MAX_LINES {
        return vec![DiffLine::new(
            DiffKind::Skip,
            None,
            &format!("{} lines -> {} lines (too large to show)", a.len(), b.len()),
        )];
    }
    // Trim common prefix/suffix, then LCS on the middle.
    let prefix = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (am, bm) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);
    let mut table = vec![vec![0u32; bm.len() + 1]; am.len() + 1];
    for i in (0..am.len()).rev() {
        for j in (0..bm.len()).rev() {
            table[i][j] = if am[i] == bm[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    // (kind, old index, new index)
    let mut ops: Vec<(DiffKind, usize, usize)> =
        (0..prefix).map(|i| (DiffKind::Context, i, i)).collect();
    let (mut i, mut j) = (0, 0);
    while i < am.len() || j < bm.len() {
        if i < am.len() && j < bm.len() && am[i] == bm[j] {
            ops.push((DiffKind::Context, prefix + i, prefix + j));
            i += 1;
            j += 1;
        } else if j < bm.len() && (i == am.len() || table[i][j + 1] > table[i + 1][j]) {
            // Removals before additions on ties (conventional unified diff).
            ops.push((DiffKind::Added, prefix + i, prefix + j));
            j += 1;
        } else {
            ops.push((DiffKind::Removed, prefix + i, prefix + j));
            i += 1;
        }
    }
    for k in 0..suffix {
        ops.push((
            DiffKind::Context,
            a.len() - suffix + k,
            b.len() - suffix + k,
        ));
    }
    // Keep context near changes.
    let changed: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, op)| op.0 != DiffKind::Context)
        .map(|(n, _)| n)
        .collect();
    let mut out = Vec::new();
    let mut skipped = 0;
    for (n, &(kind, oi, ni)) in ops.iter().enumerate() {
        let near = changed.iter().any(|&c| n.abs_diff(c) <= context);
        if kind == DiffKind::Context && !near {
            skipped += 1;
            continue;
        }
        if skipped > 0 {
            out.push(DiffLine::new(
                DiffKind::Skip,
                None,
                &format!("... {skipped} unchanged lines ..."),
            ));
            skipped = 0;
        }
        let (text, line) = match kind {
            DiffKind::Added => (b[ni], first_line + ni),
            _ => (a[oi], first_line + oi),
        };
        out.push(DiffLine::new(kind, Some(line), text));
    }
    if skipped > 0 && !out.is_empty() {
        out.push(DiffLine::new(
            DiffKind::Skip,
            None,
            &format!("... {skipped} unchanged lines ..."),
        ));
    }
    out
}

/// Unified text (`+`/`-`/` ` prefixes) for logs and tool results.
pub fn render(diff: &[DiffLine]) -> String {
    diff.iter()
        .map(|line| {
            let mark = match line.kind {
                DiffKind::Context => ' ',
                DiffKind::Added => '+',
                DiffKind::Removed => '-',
                DiffKind::Skip => '~',
            };
            format!("{mark} {}\n", line.text)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diffs_show_changes_with_context_and_line_numbers() {
        let old = "a\nb\nc\nd\ne\nf\ng\n";
        let new = "a\nb\nC\nd\ne\nf\ng\nh\n";
        let diff = line_diff(old, new, 1, 10);
        assert_eq!(
            render(&diff),
            "~ ... 1 unchanged lines ...\n  b\n- c\n+ C\n  d\n~ ... 2 unchanged lines ...\n  g\n+ h\n"
        );
        let removed = diff.iter().find(|l| l.kind == DiffKind::Removed).unwrap();
        assert_eq!(removed.line, Some(12));
        assert!(line_diff("same\n", "same\n", 3, 1).is_empty());
        let big = "x\n".repeat(MAX_LINES + 1);
        assert_eq!(line_diff(&big, "", 3, 1)[0].kind, DiffKind::Skip);
    }

    #[test]
    fn whole_file_creation_is_all_additions() {
        let diff = line_diff("", "one\ntwo", 2, 1);
        assert_eq!(diff.len(), 2);
        assert!(diff.iter().all(|l| l.kind == DiffKind::Added));
    }
}
