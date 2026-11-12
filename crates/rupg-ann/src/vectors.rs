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

/// The values of the data of a `vector` of pgvector, `[i16 dim][i16 unused][f32 × dim]` in the order of the bytes of the machine, or of a `halfvec` with `half` values when `half` is true, or `None` for data of another shape.
#[must_use]
pub fn decode(data: &[u8], half: bool) -> Option<Vec<f32>> {
    let dim = usize::try_from(i16::from_ne_bytes(data.get(0..2)?.try_into().ok()?)).ok()?;
    let body = data.get(4..)?;
    let width = if half { 2 } else { 4 };
    if body.len() != dim * width {
        return None;
    }
    let values = if half {
        body.as_chunks::<2>().0.iter().map(|&b| half_to_f32(u16::from_ne_bytes(b))).collect()
    } else {
        body.as_chunks::<4>().0.iter().map(|&b| f32::from_ne_bytes(b)).collect()
    };
    Some(values)
}

/// The `f32` of the bits `bits` of a `half`.
#[must_use]
pub fn half_to_f32(bits: u16) -> f32 {
    let sign = if bits & 0x8000 == 0 { 1.0 } else { -1.0 };
    let exponent = i32::from((bits >> 10) & 0x1F);
    let mantissa = f32::from(bits & 0x03FF);
    match exponent {
        0 => sign * mantissa * 2f32.powi(-24),
        31 if mantissa == 0.0 => sign * f32::INFINITY,
        31 => f32::NAN,
        _ => sign * (1.0 + mantissa / 1024.0) * 2f32.powi(exponent - 15),
    }
}

#[cfg(test)]
mod tests {
    use super::{decode, half_to_f32};

    #[test]
    fn halves() {
        assert_eq!(half_to_f32(0x3C00), 1.0);
        assert_eq!(half_to_f32(0xC000), -2.0);
        assert_eq!(half_to_f32(0x3555), 0.333_251_95);
        assert_eq!(half_to_f32(0x0001), 2f32.powi(-24));
        assert!(half_to_f32(0x7C01).is_nan());
        assert_eq!(half_to_f32(0xFC00), f32::NEG_INFINITY);
    }

    #[test]
    fn data() {
        let mut bytes = 2i16.to_ne_bytes().to_vec();
        bytes.extend([0, 0]);
        bytes.extend(1.5f32.to_ne_bytes());
        bytes.extend((-3f32).to_ne_bytes());
        assert_eq!(decode(&bytes, false), Some(vec![1.5, -3.0]));
        assert_eq!(decode(&bytes, true), None);
        let mut half = 2i16.to_ne_bytes().to_vec();
        half.extend([0, 0]);
        half.extend(0x3C00u16.to_ne_bytes());
        half.extend(0xC000u16.to_ne_bytes());
        assert_eq!(decode(&half, true), Some(vec![1.0, -2.0]));
    }
}
