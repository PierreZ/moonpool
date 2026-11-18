//! The seeded generator every walk draws from.

/// A seeded `splitmix64` generator: deterministic and dependency-free.
///
/// A walk makes one per seed and draws every choice from it, so a seed *is*
/// its schedule. A model draws its own choices from the same generator
/// (through [`Cx::rng`](crate::Cx::rng)), which keeps one stream for each
/// run.
///
/// ```rust
/// use moonpool_walk::Rng;
///
/// let mut a = Rng::new(7);
/// let mut b = Rng::new(7);
/// assert_eq!(a.next_u64(), b.next_u64());
/// assert!(a.below(10) < 10);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rng(u64);

impl Rng {
    /// The generator for `seed`.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// The next 64 bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A draw in `0..n`.
    ///
    /// # Panics
    ///
    /// If `n` is zero: a draw needs a range.
    pub fn below(&mut self, n: u64) -> u64 {
        assert!(n > 0, "a draw needs a range");
        self.next_u64() % n
    }

    /// An index into a collection of `len` items, or `None` when it is empty.
    pub fn index(&mut self, len: usize) -> Option<usize> {
        if len == 0 {
            return None;
        }
        let n = u64::try_from(len).ok()?;
        usize::try_from(self.below(n)).ok()
    }

    /// One element of `items`, or `None` when there are none.
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        self.index(items.len()).map(|index| &items[index])
    }

    /// `true` `num` times out of `den`.
    ///
    /// # Panics
    ///
    /// If `den` is zero.
    pub fn chance(&mut self, num: u64, den: u64) -> bool {
        self.below(den) < num
    }

    /// A draw in `[0.0, 1.0)`, with 32 bits of resolution.
    pub fn unit(&mut self) -> f64 {
        let high = u32::try_from(self.next_u64() >> 32).unwrap_or(u32::MAX);
        f64::from(high) / 4_294_967_296.0
    }

    /// `true` with probability `probability`. A probability of zero or less
    /// never happens and makes no draw.
    pub fn happens(&mut self, probability: f64) -> bool {
        probability > 0.0 && self.unit() < probability
    }

    /// The index of one weight, drawn in proportion to the weights, or
    /// `None` when every weight is zero.
    ///
    /// ```rust
    /// use moonpool_walk::Rng;
    ///
    /// let mut rng = Rng::new(1);
    /// assert_eq!(rng.weighted(&[0, 5, 0]), Some(1));
    /// assert_eq!(rng.weighted(&[0, 0]), None);
    /// ```
    pub fn weighted(&mut self, weights: &[u32]) -> Option<usize> {
        let total: u64 = weights.iter().map(|w| u64::from(*w)).sum();
        if total == 0 {
            return None;
        }
        let mut draw = self.below(total);
        for (index, weight) in weights.iter().enumerate() {
            let weight = u64::from(*weight);
            if draw < weight {
                return Some(index);
            }
            draw -= weight;
        }
        None
    }

    /// A new generator seeded from this one: an independent stream for a
    /// part of the model that must not move the main schedule.
    #[must_use]
    pub fn fork(&mut self) -> Self {
        Self(self.next_u64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seed_is_a_stream() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        let mut c = Rng::new(43);
        let first: Vec<u64> = (0..8).map(|_| a.next_u64()).collect();
        assert_eq!(first, (0..8).map(|_| b.next_u64()).collect::<Vec<_>>());
        assert_ne!(first, (0..8).map(|_| c.next_u64()).collect::<Vec<_>>());
    }

    #[test]
    fn draws_stay_in_range() {
        let mut rng = Rng::new(1);
        for _ in 0..1_000 {
            assert!(rng.below(7) < 7);
            let unit = rng.unit();
            assert!((0.0..1.0).contains(&unit));
        }
        assert_eq!(rng.index(0), None);
        assert_eq!(rng.pick::<u8>(&[]), None);
        assert_eq!(rng.pick(&[9]), Some(&9));
    }

    #[test]
    fn chances_follow_their_odds() {
        let mut rng = Rng::new(3);
        assert!((0..100).all(|_| rng.chance(1, 1)));
        assert!((0..100).all(|_| !rng.chance(0, 5)));
        assert!((0..100).all(|_| !rng.happens(0.0)));
        assert!((0..100).all(|_| rng.happens(1.0)));
        let hits = (0..10_000).filter(|_| rng.happens(0.25)).count();
        assert!((2_000..3_000).contains(&hits), "about a quarter: {hits}");
    }

    #[test]
    fn a_weighted_draw_follows_the_weights() {
        let mut rng = Rng::new(5);
        let mut seen = [0_u32; 3];
        for _ in 0..3_000 {
            let index = rng.weighted(&[1, 0, 3]).expect("a weight is set");
            seen[index] += 1;
        }
        assert_eq!(seen[1], 0);
        assert!(seen[2] > seen[0] * 2, "three to one: {seen:?}");
    }

    #[test]
    fn a_fork_does_not_repeat_its_parent() {
        let mut parent = Rng::new(9);
        let mut child = parent.fork();
        assert_ne!(parent.next_u64(), child.next_u64());
    }
}
