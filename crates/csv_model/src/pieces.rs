//! The order of a sheet's rows, as a short list of runs of consecutive row ids
//! instead of one entry per row (docs/design/large-file-engine.md, 4.2).
//!
//! A freshly opened file is a single run `0..n`. Deleting a row splits a run;
//! inserting one adds a run (or extends a neighbour); putting a deleted row
//! back where it was merges everything into one run again. Memory and time
//! therefore follow the number of structural edits, never the number of rows:
//! deleting a row from a 100-million-row file allocates a few dozen bytes.

/// Row ids `first .. first + len`, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    pub first: usize,
    pub len: usize,
}

impl Run {
    fn end(&self) -> usize {
        self.first + self.len
    }
}

#[derive(Clone, Debug, Default)]
pub struct Pieces {
    runs: Vec<Run>,
    /// Position of the first row of each run (so `starts[i] + run.len` is the
    /// next run's start). Rebuilt on every edit: edits are rare and runs few.
    starts: Vec<usize>,
    total: usize,
}

impl Pieces {
    /// Rows `0..n` in order.
    pub fn identity(n: usize) -> Self {
        let runs = if n == 0 {
            vec![]
        } else {
            vec![Run { first: 0, len: n }]
        };
        Pieces {
            starts: vec![0; runs.len()],
            runs,
            total: n,
        }
    }

    pub fn len(&self) -> usize {
        self.total
    }

    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// Still exactly rows `0..n` in order.
    pub fn is_identity(&self, n: usize) -> bool {
        self.total == n
            && self.runs.len() == usize::from(n > 0)
            && self.runs.iter().all(|r| r.first == 0)
    }

    /// The run holding `position`, and the offset into it.
    fn locate(&self, position: usize) -> Option<(usize, usize)> {
        if position >= self.total {
            return None;
        }
        let run = self.starts.partition_point(|&start| start <= position) - 1;
        Some((run, position - self.starts[run]))
    }

    /// Id of the row at `position`.
    pub fn id_at(&self, position: usize) -> Option<usize> {
        let (run, offset) = self.locate(position)?;
        Some(self.runs[run].first + offset)
    }

    /// Position of the row with `id`, scanning the runs (few).
    pub fn position_of(&self, id: usize) -> Option<usize> {
        self.runs()
            .find(|(_, run)| id >= run.first && id < run.end())
            .map(|(start, run)| start + (id - run.first))
    }

    /// One past the largest id present (0 when empty).
    pub fn next_id(&self) -> usize {
        self.runs.iter().map(Run::end).max().unwrap_or(0)
    }

    /// Remove the row at `position`; its id.
    pub fn remove_at(&mut self, position: usize) -> Option<usize> {
        let (index, offset) = self.locate(position)?;
        let run = self.runs[index];
        let id = run.first + offset;
        let before = Run {
            first: run.first,
            len: offset,
        };
        let after = Run {
            first: id + 1,
            len: run.len - offset - 1,
        };
        let kept: Vec<Run> = [before, after].into_iter().filter(|r| r.len > 0).collect();
        self.runs.splice(index..=index, kept);
        self.total -= 1;
        self.tidy();
        Some(id)
    }

    /// Insert a row with `id` at `position` (clamped to the end).
    pub fn insert_at(&mut self, position: usize, id: usize) {
        let new = Run { first: id, len: 1 };
        match self.locate(position.min(self.total)) {
            None => self.runs.push(new),
            Some((index, 0)) => self.runs.insert(index, new),
            Some((index, offset)) => {
                let run = self.runs[index];
                let before = Run {
                    first: run.first,
                    len: offset,
                };
                let after = Run {
                    first: run.first + offset,
                    len: run.len - offset,
                };
                self.runs.splice(index..=index, [before, new, after]);
            }
        }
        self.total += 1;
        self.tidy();
    }

    /// Merge runs that continue each other and recompute the starts.
    fn tidy(&mut self) {
        let mut merged: Vec<Run> = Vec::with_capacity(self.runs.len());
        for run in self.runs.drain(..) {
            match merged.last_mut() {
                Some(last) if last.end() == run.first => last.len += run.len,
                _ => merged.push(run),
            }
        }
        self.runs = merged;
        self.starts.clear();
        let mut at = 0;
        for run in &self.runs {
            self.starts.push(at);
            at += run.len;
        }
    }

    /// The runs in order, each with the position of its first row.
    pub fn runs(&self) -> impl Iterator<Item = (usize, Run)> + '_ {
        self.starts.iter().copied().zip(self.runs.iter().copied())
    }

    pub fn run_count(&self) -> usize {
        self.runs.len()
    }

    /// Memory this keeps alive.
    pub fn size_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.runs.capacity() * std::mem::size_of::<Run>()
            + self.starts.capacity() * std::mem::size_of::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn ids(p: &Pieces) -> Vec<usize> {
        (0..p.len()).map(|i| p.id_at(i).unwrap()).collect()
    }

    /// Runs are non-empty and never touch: two runs that could be one are one.
    fn assert_canonical(p: &Pieces) {
        let runs: Vec<Run> = p.runs().map(|(_, r)| r).collect();
        assert!(runs.iter().all(|r| r.len > 0), "{runs:?}");
        assert!(
            runs.windows(2).all(|w| w[0].end() != w[1].first),
            "mergeable neighbours in {runs:?}"
        );
        let mut at = 0;
        for (start, run) in p.runs() {
            assert_eq!(start, at);
            at += run.len;
        }
        assert_eq!(at, p.len());
    }

    #[test]
    fn identity_is_one_run() {
        let p = Pieces::identity(1_000_000);
        assert_eq!((p.len(), p.run_count()), (1_000_000, 1));
        assert!(p.is_identity(1_000_000));
        assert_eq!(
            (p.id_at(0), p.id_at(999_999), p.id_at(1_000_000)),
            (Some(0), Some(999_999), None)
        );
        assert_eq!(
            (p.position_of(42), p.position_of(1_000_000)),
            (Some(42), None)
        );
        assert_eq!(p.next_id(), 1_000_000);
        let empty = Pieces::identity(0);
        assert!(empty.is_empty() && empty.is_identity(0));
        assert_eq!(
            (empty.id_at(0), empty.next_id(), empty.run_count()),
            (None, 0, 0)
        );
    }

    #[test]
    fn deleting_a_row_splits_a_run_and_putting_it_back_merges_it_again() {
        let mut p = Pieces::identity(100);
        assert_eq!(p.remove_at(40), Some(40));
        assert_eq!((p.len(), p.run_count()), (99, 2));
        assert_eq!((p.id_at(39), p.id_at(40)), (Some(39), Some(41)));
        assert_eq!((p.position_of(40), p.position_of(41)), (None, Some(40)));
        p.insert_at(40, 40);
        assert!(p.is_identity(100), "back to a single run");
        assert_canonical(&p);
    }

    #[test]
    fn edges_and_out_of_range() {
        let mut p = Pieces::identity(5);
        assert_eq!(p.remove_at(5), None);
        assert_eq!(p.remove_at(0), Some(0));
        assert_eq!(p.remove_at(3), Some(4));
        assert_eq!(ids(&p), [1, 2, 3]);
        p.insert_at(99, 7); // past the end clamps
        p.insert_at(0, 9);
        assert_eq!(ids(&p), [9, 1, 2, 3, 7]);
        assert_eq!(p.next_id(), 10);
        assert_canonical(&p);
    }

    #[test]
    fn a_thousand_scattered_deletes_cost_kilobytes_not_megabytes() {
        let mut p = Pieces::identity(100_000_000);
        for k in 0..1000 {
            p.remove_at(50_000 + k * 7);
        }
        assert_eq!(p.len(), 100_000_000 - 1000);
        assert!(p.run_count() <= 1001);
        assert!(p.size_bytes() < 64 * 1024, "{} bytes", p.size_bytes());
        assert_canonical(&p);
    }

    #[test]
    fn inserting_an_adjacent_id_extends_a_run() {
        let mut p = Pieces::identity(3); // 0 1 2
        p.insert_at(3, 3);
        p.insert_at(4, 4);
        assert_eq!(p.run_count(), 1);
        assert!(p.is_identity(5));
        p.insert_at(0, 100);
        p.insert_at(0, 99);
        assert_eq!(
            p.run_count(),
            2,
            "99 and 100 are not consecutive in this order"
        );
        assert_canonical(&p);
    }

    #[derive(Clone, Debug)]
    enum Op {
        Remove(usize),
        /// Insert a fresh id, or re-insert the most recently removed one.
        Insert(usize, bool),
    }

    fn ops() -> impl Strategy<Value = Vec<Op>> {
        let op = prop_oneof![
            (0usize..14).prop_map(Op::Remove),
            (0usize..14, any::<bool>()).prop_map(|(p, back)| Op::Insert(p, back)),
        ];
        prop::collection::vec(op, 0..60)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(3000))]

        #[test]
        fn pieces_behave_like_a_vec_of_ids(n in 0usize..12, ops in ops()) {
            let mut model: Vec<usize> = (0..n).collect();
            let mut pieces = Pieces::identity(n);
            let mut removed: Vec<usize> = vec![];
            let mut fresh = n + 100;
            for op in ops {
                match op {
                    Op::Remove(pos) => {
                        let want = (pos < model.len()).then(|| model.remove(pos));
                        prop_assert_eq!(pieces.remove_at(pos), want);
                        removed.extend(want);
                    }
                    Op::Insert(pos, back) => {
                        let id = match (back, removed.pop()) {
                            (true, Some(id)) if !model.contains(&id) => id,
                            _ => { fresh += 1; fresh }
                        };
                        model.insert(pos.min(model.len()), id);
                        pieces.insert_at(pos, id);
                    }
                }
                prop_assert_eq!(ids(&pieces), model.clone());
                prop_assert_eq!(pieces.len(), model.len());
                prop_assert_eq!(pieces.next_id(), model.iter().max().map_or(0, |m| m + 1));
                for (pos, id) in model.iter().enumerate() {
                    prop_assert_eq!(pieces.position_of(*id), Some(pos));
                }
                prop_assert_eq!(pieces.position_of(100_000), None);
                prop_assert_eq!(pieces.id_at(model.len()), None);
                assert_canonical(&pieces);
                prop_assert_eq!(pieces.is_identity(model.len()), model.iter().copied().eq(0..model.len()));
            }
        }
    }
}
