use serde::{Deserialize, Serialize};

/// Sentinel line count meaning "the whole file" (binary files, Write overwrites).
pub const WHOLE_FILE: i64 = 1 << 30;

/// One diff hunk with zero context lines: exact changed ranges.
/// Git semantics: `old_lines == 0` marks a pure insertion after old line
/// `old_start`; `new_lines == 0` marks a pure deletion, with `new_start` the
/// last line before the removed content in the new file.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Hunk {
    pub old_start: i64,
    pub old_lines: i64,
    pub new_start: i64,
    pub new_lines: i64,
}

/// A 1-based, inclusive line range owned by a session.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Region {
    pub start: i64,
    pub end: i64,
}

enum LineMap {
    At(i64),
    Inside(usize),
}

/// Map one pre-edit line number through the hunks of an edit.
fn map_line(hunks: &[Hunk], line: i64) -> LineMap {
    let mut delta = 0i64;
    for (i, h) in hunks.iter().enumerate() {
        if h.old_lines == 0 {
            if line <= h.old_start {
                return LineMap::At(line + delta);
            }
            delta += h.new_lines;
        } else {
            let old_end = h.old_start + h.old_lines - 1;
            if line < h.old_start {
                return LineMap::At(line + delta);
            }
            if line <= old_end {
                return LineMap::Inside(i);
            }
            delta += h.new_lines - h.old_lines;
        }
    }
    LineMap::At(line + delta)
}

/// Map a region through an edit's hunks into post-edit coordinates.
/// `None` means the region was fully consumed (overwritten) by the edit.
pub fn map_region(hunks: &[Hunk], r: Region) -> Option<Region> {
    let (start, end) = match (map_line(hunks, r.start), map_line(hunks, r.end)) {
        (LineMap::At(a), LineMap::At(b)) => (a, b),
        (LineMap::At(a), LineMap::Inside(j)) => {
            (a, hunks[j].new_start + hunks[j].new_lines - 1)
        }
        (LineMap::Inside(i), LineMap::At(b)) => (hunks[i].new_start.max(1), b),
        (LineMap::Inside(i), LineMap::Inside(j)) => {
            if i == j {
                return None;
            }
            (
                hunks[i].new_start.max(1),
                hunks[j].new_start + hunks[j].new_lines - 1,
            )
        }
    };
    if end < start.max(1) {
        None
    } else {
        Some(Region { start: start.max(1), end })
    }
}

/// The regions an edit's author now owns: the new-side ranges of its hunks.
/// A pure deletion leaves a one-line "seam" claim at the deletion point.
pub fn regions_from_hunks(hunks: &[Hunk]) -> Vec<Region> {
    let mut out = Vec::new();
    for h in hunks {
        if h.new_lines > 0 {
            out.push(Region {
                start: h.new_start,
                end: h.new_start + h.new_lines - 1,
            });
        } else {
            let s = h.new_start.max(1);
            out.push(Region { start: s, end: s });
        }
    }
    merge(out)
}

/// Coalesce overlapping or directly adjacent regions.
pub fn merge(mut v: Vec<Region>) -> Vec<Region> {
    v.sort_by_key(|r| r.start);
    let mut out: Vec<Region> = Vec::new();
    for r in v {
        if let Some(last) = out.last_mut() {
            if r.start <= last.end + 1 {
                last.end = last.end.max(r.end);
                continue;
            }
        }
        out.push(r);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(old_start: i64, old_lines: i64, new_start: i64, new_lines: i64) -> Hunk {
        Hunk { old_start, old_lines, new_start, new_lines }
    }
    fn r(start: i64, end: i64) -> Region {
        Region { start, end }
    }

    #[test]
    fn insertion_above_shifts_region_down() {
        // 5 lines inserted after line 2; region 10-14 slides to 15-19.
        let hunks = [h(2, 0, 3, 5)];
        assert_eq!(map_region(&hunks, r(10, 14)), Some(r(15, 19)));
    }

    #[test]
    fn insertion_below_leaves_region() {
        let hunks = [h(20, 0, 21, 5)];
        assert_eq!(map_region(&hunks, r(10, 14)), Some(r(10, 14)));
    }

    #[test]
    fn deletion_above_shifts_region_up() {
        // lines 1-3 deleted; region 10-12 slides to 7-9.
        let hunks = [h(1, 3, 0, 0)];
        assert_eq!(map_region(&hunks, r(10, 12)), Some(r(7, 9)));
    }

    #[test]
    fn full_overwrite_consumes_region() {
        // Lines 5-15 were replaced, so the enclosed region 8-10 is gone.
        let hunks = [h(5, 11, 5, 2)];
        assert_eq!(map_region(&hunks, r(8, 10)), None);
    }

    #[test]
    fn partial_overlap_clips_region() {
        // lines 12-14 replaced by 12-13; region 10-13 keeps its head,
        // its tail clips to the hunk's new extent.
        let hunks = [h(12, 3, 12, 2)];
        assert_eq!(map_region(&hunks, r(10, 13)), Some(r(10, 13)));
    }

    #[test]
    fn multiple_hunks_accumulate_deltas() {
        // Add two lines after line 1 and remove one at line 10. Region 20-22 moves to 21-23.
        let hunks = [h(1, 0, 2, 2), h(10, 1, 11, 0)];
        assert_eq!(map_region(&hunks, r(20, 22)), Some(r(21, 23)));
    }

    #[test]
    fn region_spanning_hunk_stretches_with_it() {
        // Inserting four lines inside region 5-10 grows it to 5-14.
        let hunks = [h(7, 0, 8, 4)];
        assert_eq!(map_region(&hunks, r(5, 10)), Some(r(5, 14)));
    }

    #[test]
    fn editor_regions_from_hunks() {
        let hunks = [h(2, 0, 3, 2), h(10, 3, 12, 1), h(20, 2, 21, 0)];
        assert_eq!(
            regions_from_hunks(&hunks),
            vec![r(3, 4), r(12, 12), r(21, 21)]
        );
    }

    #[test]
    fn merge_coalesces_adjacent() {
        assert_eq!(
            merge(vec![r(5, 7), r(8, 9), r(20, 22), r(6, 6)]),
            vec![r(5, 9), r(20, 22)]
        );
    }
}
