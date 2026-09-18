//! Brute-force cosine vector index (D2-B1).
//!
//! Deliberately simple: a `Vec<(MemoryId, Vec<f32>)>` walked in a
//! single pass per query. At the scale this index is designed for
//! (≤ 10 000 entries — a few months of session facts), the pass is
//! under 5 ms on any machine that runs a local model. An ANN index
//! (HNSW, IVF) would be premature complexity for a workload that has
//! not been measured yet; the roadmap explicitly says "ne pas payer
//! une complexité sans donnée".
//!
//! # Normalization
//!
//! Vectors are L2-normalized on insert. That turns the cosine
//! similarity into a plain dot product, which is ~10× faster on the
//! inner loop and simpler to reason about. A zero vector is rejected:
//! cosine against a zero vector is undefined, and the memory writer
//! should not be storing one anyway.
//!
//! # Dimensions
//!
//! The index has a fixed `dim`. An insert with a different length is
//! rejected with an error naming both the expected and the received
//! length — a model swap that changes dims must rebuild the index,
//! not silently corrupt it.

use kod_error::{KodError, Result};
use kod_types::MemoryId;

/// In-memory index of embedding vectors, keyed by `MemoryId`.
pub struct VectorIndex {
    entries: Vec<(MemoryId, Vec<f32>)>,
    dim: usize,
}

impl VectorIndex {
    /// A new empty index sized for `dim`-dimensional vectors.
    pub fn new(dim: usize) -> Self {
        Self {
            entries: Vec::new(),
            dim,
        }
    }

    /// Vector length this index accepts. 0 means "unusable"; an index
    /// built with 0 dims is a placeholder and refuses every insert.
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Number of stored vectors.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Insert (or replace) the vector for `id`. The vector is
    /// L2-normalized in place. A zero vector, a wrong-dim vector, or a
    /// NaN component is rejected.
    pub fn insert(&mut self, id: MemoryId, v: Vec<f32>) -> Result<()> {
        if self.dim == 0 {
            return Err(KodError::MemoryStorage(
                "vector index has dim = 0; cannot insert".to_string(),
            ));
        }
        if v.len() != self.dim {
            return Err(KodError::MemoryStorage(format!(
                "vector index expects {} dims, got {}",
                self.dim,
                v.len()
            )));
        }
        let norm_sq: f32 = v.iter().map(|x| x * x).sum();
        if !norm_sq.is_finite() || norm_sq == 0.0 {
            return Err(KodError::MemoryStorage(
                "vector index: zero or non-finite vector".to_string(),
            ));
        }
        let norm = norm_sq.sqrt();
        let normalized: Vec<f32> = v.iter().map(|x| x / norm).collect();

        // Replace if present, else append.
        if let Some(slot) = self
            .entries
            .iter_mut()
            .find(|(existing, _)| existing == &id)
        {
            slot.1 = normalized;
        } else {
            self.entries.push((id, normalized));
        }
        Ok(())
    }

    /// Remove the entry for `id`, if present. Returns `true` when a
    /// vector was removed.
    pub fn remove(&mut self, id: &MemoryId) -> bool {
        if let Some(pos) = self.entries.iter().position(|(i, _)| i == id) {
            self.entries.swap_remove(pos);
            true
        } else {
            false
        }
    }

    /// Top-`k` vectors by dot product (== cosine, since inputs are
    /// normalized). The query is normalized on the fly. Ties are
    /// broken by insertion order (stable via `sort_by` semantics).
    ///
    /// `keep` is an optional predicate evaluated against each id; a
    /// `false` result skips that entry. Used to filter by project_key
    /// without materialising a second index.
    pub fn search<F>(&self, query: &[f32], k: usize, keep: F) -> Result<Vec<(MemoryId, f32)>>
    where
        F: Fn(&MemoryId) -> bool,
    {
        if query.len() != self.dim {
            return Err(KodError::MemoryStorage(format!(
                "vector query expects {} dims, got {}",
                self.dim,
                query.len()
            )));
        }
        let norm_sq: f32 = query.iter().map(|x| x * x).sum();
        if !norm_sq.is_finite() || norm_sq == 0.0 {
            return Ok(Vec::new());
        }
        let norm = norm_sq.sqrt();
        let q: Vec<f32> = query.iter().map(|x| x / norm).collect();

        let mut scored: Vec<(MemoryId, f32)> = self
            .entries
            .iter()
            .filter(|(id, _)| keep(id))
            .map(|(id, v)| {
                let dot: f32 = v.iter().zip(q.iter()).map(|(a, b)| a * b).sum();
                (id.clone(), dot)
            })
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(k);
        Ok(scored)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> MemoryId {
        MemoryId::new()
    }

    #[test]
    fn insert_rejects_wrong_dims() {
        let mut idx = VectorIndex::new(3);
        assert!(idx.insert(id(), vec![1.0, 2.0]).is_err());
    }

    #[test]
    fn insert_rejects_zero_vector() {
        let mut idx = VectorIndex::new(3);
        assert!(idx.insert(id(), vec![0.0, 0.0, 0.0]).is_err());
    }

    #[test]
    fn insert_replaces_by_id() {
        let mut idx = VectorIndex::new(2);
        let i = id();
        idx.insert(i.clone(), vec![1.0, 0.0]).unwrap();
        idx.insert(i.clone(), vec![0.0, 1.0]).unwrap();
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn search_returns_most_similar_first() {
        let mut idx = VectorIndex::new(2);
        let a = id();
        let b = id();
        let c = id();
        // a is along +x, b along +y, c along -x.
        idx.insert(a.clone(), vec![1.0, 0.0]).unwrap();
        idx.insert(b.clone(), vec![0.0, 1.0]).unwrap();
        idx.insert(c.clone(), vec![-1.0, 0.0]).unwrap();

        // Query along +x: a is closest (cosine 1), b is orthogonal
        // (cosine 0), c is opposite (cosine -1). Sorted descending.
        let hits = idx.search(&[1.0, 0.0], 3, |_| true).unwrap();
        assert_eq!(hits[0].0, a);
        assert!((hits[0].1 - 1.0).abs() < 1e-5);
        assert_eq!(hits[1].0, b);
        assert!((hits[1].1 - 0.0).abs() < 1e-5);
        assert_eq!(hits[2].0, c);
        assert!((hits[2].1 - (-1.0)).abs() < 1e-5);
    }

    #[test]
    fn search_honours_keep_filter() {
        let mut idx = VectorIndex::new(2);
        let a = id();
        let b = id();
        idx.insert(a.clone(), vec![1.0, 0.0]).unwrap();
        idx.insert(b.clone(), vec![0.9, 0.1]).unwrap();

        // Exclude `a`; only `b` must come back.
        let hits = idx.search(&[1.0, 0.0], 5, |i| i != &a).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, b);
    }

    #[test]
    fn search_on_empty_index_is_empty() {
        let idx = VectorIndex::new(4);
        let hits = idx.search(&[1.0, 0.0, 0.0, 0.0], 5, |_| true).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn search_rejects_wrong_query_dims() {
        let idx = VectorIndex::new(3);
        assert!(idx.search(&[1.0, 0.0], 5, |_| true).is_err());
    }

    #[test]
    fn remove_removes() {
        let mut idx = VectorIndex::new(2);
        let a = id();
        idx.insert(a.clone(), vec![1.0, 0.0]).unwrap();
        assert_eq!(idx.len(), 1);
        assert!(idx.remove(&a));
        assert!(idx.is_empty());
        assert!(!idx.remove(&a));
    }

    #[test]
    fn zero_dim_index_rejects_inserts() {
        let mut idx = VectorIndex::new(0);
        assert!(idx.insert(id(), vec![]).is_err());
    }
}

#[cfg(test)]
mod coverage_vector_search {
    //! The vector index is a brute-force walk that the retrieval
    //! path consults on every query. Its correctness condition is
    //! "the top-k list is right" and "no vector that fails the
    //! keep predicate leaks". A regression here silently reranks
    //! memory on every retrieval.
    use super::*;

    fn id() -> MemoryId {
        MemoryId::new()
    }

    #[test]
    fn nan_vector_is_rejected() {
        // A NaN in an embedding is a server bug, not valid data.
        // The `norm_sq.is_finite()` guard is what stops it from
        // poisoning every subsequent dot product.
        let mut idx = VectorIndex::new(2);
        assert!(idx.insert(id(), vec![f32::NAN, 0.0]).is_err());
        assert!(idx.insert(id(), vec![f32::INFINITY, 0.0]).is_err());
    }

    #[test]
    fn search_with_k_zero_returns_empty() {
        let mut idx = VectorIndex::new(2);
        idx.insert(id(), vec![1.0, 0.0]).unwrap();
        let hits = idx.search(&[1.0, 0.0], 0, |_| true).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn search_with_k_larger_than_len_returns_everything() {
        let mut idx = VectorIndex::new(2);
        for _ in 0..3 {
            idx.insert(id(), vec![1.0, 0.0]).unwrap();
        }
        let hits = idx.search(&[1.0, 0.0], 100, |_| true).unwrap();
        assert_eq!(hits.len(), 3);
    }

    #[test]
    fn search_on_zero_query_vector_returns_empty() {
        // A zero query vector has no direction; cosine is
        // undefined. The safe answer is "no results", not a
        // division by zero or a panic.
        let mut idx = VectorIndex::new(2);
        idx.insert(id(), vec![1.0, 0.0]).unwrap();
        let hits = idx.search(&[0.0, 0.0], 5, |_| true).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn replace_then_search_returns_the_new_vector() {
        let mut idx = VectorIndex::new(2);
        let i = id();
        idx.insert(i.clone(), vec![1.0, 0.0]).unwrap();
        idx.insert(i.clone(), vec![0.0, 1.0]).unwrap();
        // The index is now a single vector along +y; a query
        // along +x must not match it at the top of the ranking.
        let hits_x = idx.search(&[1.0, 0.0], 5, |_| true).unwrap();
        assert_eq!(hits_x.len(), 1);
        assert!((hits_x[0].1 - 0.0).abs() < 1e-6);
        let hits_y = idx.search(&[0.0, 1.0], 5, |_| true).unwrap();
        assert!((hits_y[0].1 - 1.0).abs() < 1e-6);
    }

    #[test]
    fn remove_then_search_excludes_the_removed_id() {
        let mut idx = VectorIndex::new(2);
        let a = id();
        let b = id();
        idx.insert(a.clone(), vec![1.0, 0.0]).unwrap();
        idx.insert(b.clone(), vec![0.9, 0.1]).unwrap();
        idx.remove(&a);
        let hits = idx.search(&[1.0, 0.0], 5, |_| true).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, b);
    }

    #[test]
    fn dim_reports_the_constructor_argument() {
        assert_eq!(VectorIndex::new(4).dim(), 4);
        assert_eq!(VectorIndex::new(0).dim(), 0);
    }

    #[test]
    fn is_empty_is_true_initially_and_false_after_insert() {
        let mut idx = VectorIndex::new(2);
        assert!(idx.is_empty());
        idx.insert(id(), vec![1.0, 0.0]).unwrap();
        assert!(!idx.is_empty());
    }

    #[test]
    fn remove_returns_false_for_unknown_id() {
        let mut idx = VectorIndex::new(2);
        assert!(!idx.remove(&id()));
    }

    #[test]
    fn search_is_stable_across_calls_for_identical_inputs() {
        // Two searches against the same index and the same query
        // must produce the same ranking. A regression that used a
        // HashMap internally would make the order
        // non-deterministic and every golden snapshot of the
        // retrieval path flaky.
        let mut idx = VectorIndex::new(2);
        for _ in 0..5 {
            idx.insert(id(), vec![1.0, 0.0]).unwrap();
        }
        let a = idx.search(&[1.0, 0.0], 5, |_| true).unwrap();
        let b = idx.search(&[1.0, 0.0], 5, |_| true).unwrap();
        let ids_a: Vec<_> = a.iter().map(|(id, _)| id.clone()).collect();
        let ids_b: Vec<_> = b.iter().map(|(id, _)| id.clone()).collect();
        assert_eq!(ids_a, ids_b);
    }
}
