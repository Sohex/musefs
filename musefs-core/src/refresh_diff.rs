use std::collections::HashMap;

use musefs_db::Format;

/// Per-track state persisted between refreshes so unchanged tracks need no
/// re-render. `(content_version, format)` is the render key (the only track-level
/// inputs to `render_path`); `path` is the last rendered path, reused verbatim for
/// unchanged tracks. See SP2 Component 1/2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TrackRenderState {
    pub content_version: i64,
    pub format: Format,
    pub path: String,
}

/// The result of diffing the previous snapshot against a fresh render-key scan.
#[allow(dead_code)]
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ChangeSet {
    /// In both, render key differs (must re-render).
    pub changed: Vec<i64>,
    /// New ids (must render).
    pub added: Vec<i64>,
    /// Ids gone from the scan (must drop).
    pub removed: Vec<i64>,
}

impl ChangeSet {
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.added.is_empty() && self.removed.is_empty()
    }
}

/// Partition a fresh `(id, content_version, format)` scan against the previous
/// snapshot. `scan` is ordered by id (as `list_render_keys` returns it); outputs
/// are id-ascending so downstream rendering and tree assembly are deterministic.
#[allow(dead_code)]
pub(crate) fn partition_changes(
    prev: &HashMap<i64, TrackRenderState>,
    scan: &[(i64, i64, Format)],
) -> ChangeSet {
    let mut cs = ChangeSet::default();
    let mut seen = std::collections::HashSet::with_capacity(scan.len());
    for &(id, cv, fmt) in scan {
        seen.insert(id);
        match prev.get(&id) {
            None => cs.added.push(id),
            Some(s) if s.content_version != cv || s.format != fmt => cs.changed.push(id),
            Some(_) => {}
        }
    }
    for &id in prev.keys() {
        if !seen.contains(&id) {
            cs.removed.push(id);
        }
    }
    cs.removed.sort_unstable();
    cs
}

/// Partition a changelog read against the previous snapshot. `prev_states` holds
/// the prior states of just the changelog ids (the caller extracts them under a
/// short snapshot lock); `keys` are the live render keys for those ids (absent =
/// no longer in `tracks`). An id that is neither live nor previously known —
/// added and deleted between polls — is pure churn and lands nowhere.
#[allow(dead_code)] // wired into facade.rs in a later task (mirrors partition_changes)
pub(crate) fn partition_changelog(
    prev_states: &HashMap<i64, TrackRenderState>,
    changelog_ids: &[i64],
    keys: &[(i64, i64, Format)],
) -> ChangeSet {
    let live: HashMap<i64, (i64, Format)> = keys.iter().map(|&(id, cv, f)| (id, (cv, f))).collect();
    let mut cs = ChangeSet::default();
    for &id in changelog_ids {
        match (live.get(&id), prev_states.get(&id)) {
            (Some(&(cv, fmt)), Some(s)) if s.content_version != cv || s.format != fmt => {
                cs.changed.push(id);
            }
            (Some(_), None) => cs.added.push(id),
            (None, Some(_)) => cs.removed.push(id),
            _ => {} // no-op (unchanged) or churn (added+removed between polls)
        }
    }
    cs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(cv: i64, fmt: Format, path: &str) -> TrackRenderState {
        TrackRenderState {
            content_version: cv,
            format: fmt,
            path: path.into(),
        }
    }

    #[test]
    fn partitions_changed_added_removed() {
        let mut prev = HashMap::new();
        prev.insert(1, st(0, Format::Flac, "A/1.flac"));
        prev.insert(2, st(0, Format::Flac, "A/2.flac"));
        prev.insert(3, st(0, Format::Flac, "A/3.flac"));
        let scan = vec![
            (1, 0, Format::Flac),
            (2, 1, Format::Flac),
            (4, 0, Format::Flac),
        ];
        let cs = partition_changes(&prev, &scan);
        assert_eq!(cs.changed, vec![2]);
        assert_eq!(cs.added, vec![4]);
        assert_eq!(cs.removed, vec![3]);
    }

    #[test]
    fn format_only_change_is_changed() {
        let mut prev = HashMap::new();
        prev.insert(1, st(5, Format::Flac, "A/1.flac"));
        let scan = vec![(1, 5, Format::Mp3)];
        let cs = partition_changes(&prev, &scan);
        assert_eq!(cs.changed, vec![1]);
        assert!(cs.added.is_empty() && cs.removed.is_empty());
    }

    #[test]
    fn no_changes_is_empty() {
        let mut prev = HashMap::new();
        prev.insert(1, st(0, Format::Flac, "A/1.flac"));
        let scan = vec![(1, 0, Format::Flac)];
        assert!(partition_changes(&prev, &scan).is_empty());
    }

    #[test]
    fn is_empty_false_when_any_single_category_is_nonempty() {
        // Each category alone must make `is_empty` false. A single non-empty
        // category isolates the three `&&` conjuncts: it would read as empty
        // under any `&&`->`||` swap, and (with the default empty case below) it
        // also pins the function against a constant-`true` body.
        let only_changed = ChangeSet {
            changed: vec![1],
            ..Default::default()
        };
        let only_added = ChangeSet {
            added: vec![1],
            ..Default::default()
        };
        let only_removed = ChangeSet {
            removed: vec![1],
            ..Default::default()
        };
        assert!(!only_changed.is_empty());
        assert!(!only_added.is_empty());
        assert!(!only_removed.is_empty());
        assert!(ChangeSet::default().is_empty());
    }

    #[test]
    fn changelog_partition_classifies_changed_added_removed_and_churn() {
        use musefs_db::Format;
        let state = |cv: i64| TrackRenderState {
            content_version: cv,
            format: Format::Flac,
            path: "p".into(),
        };
        // prev knows 1 (cv 5), 2 (cv 1), 3 (cv 9).
        let prev_states: HashMap<i64, TrackRenderState> =
            [(1, state(5)), (2, state(1)), (3, state(9))].into();
        // Changelog mentioned 1,2,3,4,5. Live keys: 1 unchanged, 2 bumped, 4 new.
        // 3 is gone (removed); 5 was added+deleted between polls (pure churn).
        let changelog_ids = vec![1, 2, 3, 4, 5];
        let keys = vec![
            (1, 5, Format::Flac),
            (2, 2, Format::Flac),
            (4, 0, Format::Flac),
        ];
        let cs = partition_changelog(&prev_states, &changelog_ids, &keys);
        assert_eq!(cs.changed, vec![2]);
        assert_eq!(cs.added, vec![4]);
        assert_eq!(cs.removed, vec![3]); // churn id 5 is in neither output
    }
}
