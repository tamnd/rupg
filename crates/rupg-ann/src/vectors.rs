//! The full vectors of an index, each with the row id of its row.

/// The vectors of an index, each of [`Vectors::dim`] values, by their node numbers.
#[derive(Clone, Debug, Default)]
pub struct Vectors {
    dim: usize,
    data: Vec<f32>,
    ids: Vec<u64>,
}

impl Vectors {
    /// No vectors, each of `dim` values.
    #[must_use]
    pub fn new(dim: usize) -> Vectors {
        Vectors { dim, data: Vec::new(), ids: Vec::new() }
    }

    /// The number of values of each vector.
    #[must_use]
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// The number of vectors.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// True when there are no vectors.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// Adds the vector `vector` of the row `id`, and gives its node number.
    ///
    /// # Panics
    ///
    /// When `vector` does not have [`Vectors::dim`] values, or there are more than `u32::MAX` vectors.
    pub fn push(&mut self, id: u64, vector: &[f32]) -> u32 {
        assert_eq!(
            vector.len(),
            self.dim,
            "a vector of {} values in an index of {}",
            vector.len(),
            self.dim
        );
        let node = u32::try_from(self.ids.len()).expect("too many vectors");
        self.data.extend_from_slice(vector);
        self.ids.push(id);
        node
    }

    /// The vector of the node `node`.
    #[must_use]
    pub fn get(&self, node: u32) -> &[f32] {
        let at = node as usize * self.dim;
        &self.data[at..at + self.dim]
    }

    /// The row id of the node `node`.
    #[must_use]
    pub fn id(&self, node: u32) -> u64 {
        self.ids[node as usize]
    }

    /// The node numbers.
    #[must_use]
    pub fn nodes(&self) -> std::ops::Range<u32> {
        0..self.ids.len() as u32
    }
}
