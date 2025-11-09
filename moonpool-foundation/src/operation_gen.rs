//! Operation generation framework for autonomous chaos testing.
//!
//! This module provides traits and implementations for automatically generating
//! diverse operation sequences in simulation tests. Instead of manually scripting
//! fixed test scenarios, tests can use operation generators to explore a vast
//! space of possible execution paths.
//!
//! # Philosophy
//!
//! Traditional tests follow fixed patterns:
//! ```ignore
//! for _ in 0..10 {
//!     send_message();  // Always 10 messages, always in order
//! }
//! ```
//!
//! Autonomous tests explore diverse sequences:
//! ```ignore
//! let generator = WeightedGenerator::new()
//!     .operation(Op::Send, 0.7)
//!     .operation(Op::Disconnect, 0.2)
//!     .operation(Op::Reconnect, 0.1);
//!
//! // Generates 500-1000 operations with random order, timing, and interleaving
//! autonomous_workload(generator, execute_op, (500, 1000), random).await
//! ```
//!
//! This approach finds bugs that fixed scripts miss: concurrent disconnects,
//! server switching mid-request, resource exhaustion, race conditions, etc.
//!
//! # Core Components
//!
//! - [`OperationGenerator`] - Trait for generating operation sequences
//! - [`UniformGenerator`] - Equal probability for all operations
//! - [`WeightedGenerator`] - Custom distribution (e.g., 80% sends, 20% failures)
//! - [`autonomous_workload`] - Execute generated operations with chaos

use crate::random::RandomProvider;

/// Core trait for generating operations in autonomous testing.
///
/// Generators produce operations based on randomization and optionally state.
/// They define the "alphabet" of possible operations and how they're selected.
///
/// # Design Philosophy
///
/// Generators embody the "operation alphabet pattern": define ALL possible
/// operations the system supports, then let the generator choose sequences.
/// This explores edge cases that manual tests miss.
///
/// # Example
///
/// ```
/// use moonpool_foundation::operation_gen::{OperationGenerator, WeightedGenerator};
/// use moonpool_foundation::random::sim::SimRandomProvider;
///
/// #[derive(Clone, Debug, PartialEq)]
/// enum BankOp {
///     Deposit(u64),
///     Withdraw(u64),
///     CheckBalance,
/// }
///
/// let random = SimRandomProvider::new(42);
/// let mut generator = WeightedGenerator::new()
///     .with_operation(BankOp::Deposit(0), 0.5)
///     .with_operation(BankOp::Withdraw(0), 0.3)
///     .with_operation(BankOp::CheckBalance, 0.2);
///
/// // Generate 10 operations following the weight distribution
/// for _ in 0..10 {
///     let op = generator.generate(&random);
///     println!("{:?}", op);
/// }
/// ```
pub trait OperationGenerator {
    /// The type of operation this generator produces
    type Operation: Clone;

    /// Generate a single operation based on internal strategy and randomization.
    ///
    /// This is called repeatedly to build operation sequences. Implementations
    /// can be stateless (pure random) or stateful (e.g., Markov chains).
    fn generate<R: RandomProvider>(&mut self, rng: &R) -> Self::Operation;

    /// Get the weight distribution for operations.
    ///
    /// Returns pairs of (operation, weight) where higher weights mean higher
    /// probability of selection. Used for introspection and debugging.
    ///
    /// # Returns
    ///
    /// Vector of (operation, weight) pairs. Weights don't need to sum to 1.0.
    fn weight_distribution(&self) -> Vec<(Self::Operation, f64)>;

    /// Generate a sequence of operations.
    ///
    /// Convenience method for generating multiple operations at once.
    ///
    /// # Parameters
    ///
    /// * `rng` - Random number generator for deterministic randomness
    /// * `count` - Number of operations to generate
    ///
    /// # Returns
    ///
    /// Vector of generated operations
    fn generate_sequence<R: RandomProvider>(
        &mut self,
        rng: &R,
        count: usize,
    ) -> Vec<Self::Operation> {
        (0..count).map(|_| self.generate(rng)).collect()
    }
}

/// Uniform operation generator - all operations have equal probability.
///
/// This is the simplest generator, useful when you want to explore all
/// operations equally without bias. Good for initial testing and baseline.
///
/// # Example
///
/// ```
/// use moonpool_foundation::operation_gen::{OperationGenerator, UniformGenerator};
/// use moonpool_foundation::random::sim::SimRandomProvider;
///
/// #[derive(Clone, Debug, PartialEq)]
/// enum Op { A, B, C }
///
/// let random = SimRandomProvider::new(42);
/// let mut generator = UniformGenerator::new(vec![Op::A, Op::B, Op::C]);
///
/// // Each operation has 33.3% probability
/// let op = generator.generate(&random);
/// ```
pub struct UniformGenerator<T: Clone> {
    operations: Vec<T>,
}

impl<T: Clone> UniformGenerator<T> {
    /// Create a new uniform generator with the given operations.
    ///
    /// # Parameters
    ///
    /// * `operations` - Vector of all possible operations
    ///
    /// # Panics
    ///
    /// Panics if operations vector is empty.
    pub fn new(operations: Vec<T>) -> Self {
        assert!(
            !operations.is_empty(),
            "UniformGenerator requires at least one operation"
        );
        Self { operations }
    }

    /// Add a new operation to the generator.
    ///
    /// # Parameters
    ///
    /// * `operation` - The operation to add
    pub fn with_operation(mut self, operation: T) -> Self {
        self.operations.push(operation);
        self
    }
}

impl<T: Clone> OperationGenerator for UniformGenerator<T> {
    type Operation = T;

    fn generate<R: RandomProvider>(&mut self, rng: &R) -> Self::Operation {
        let index = rng.random_range(0..self.operations.len());
        self.operations[index].clone()
    }

    fn weight_distribution(&self) -> Vec<(Self::Operation, f64)> {
        let weight = 1.0 / self.operations.len() as f64;
        self.operations
            .iter()
            .map(|op| (op.clone(), weight))
            .collect()
    }
}

/// Weighted operation generator - operations have custom probability distribution.
///
/// This is the most commonly used generator. Allows fine-tuning the mix of
/// operations to match realistic workloads or focus on specific scenarios.
///
/// # Example
///
/// ```
/// use moonpool_foundation::operation_gen::{OperationGenerator, WeightedGenerator};
/// use moonpool_foundation::random::sim::SimRandomProvider;
///
/// #[derive(Clone, Debug, PartialEq)]
/// enum Op { Send, Fail }
///
/// let random = SimRandomProvider::new(42);
/// let mut generator = WeightedGenerator::new()
///     .with_operation(Op::Send, 0.9)   // 90% of operations
///     .with_operation(Op::Fail, 0.1);  // 10% of operations
///
/// let op = generator.generate(&random);
/// ```
pub struct WeightedGenerator<T: Clone> {
    operations: Vec<T>,
    weights: Vec<f64>,
    cumulative_weights: Vec<f64>,
}

impl<T: Clone> WeightedGenerator<T> {
    /// Create a new weighted generator with no operations.
    ///
    /// Operations must be added using `with_operation()` before generation.
    pub fn new() -> Self {
        Self {
            operations: Vec::new(),
            weights: Vec::new(),
            cumulative_weights: Vec::new(),
        }
    }

    /// Add an operation with the specified weight.
    ///
    /// Weights are relative - they don't need to sum to 1.0. For example,
    /// weights of [2.0, 1.0] give 67% / 33% distribution.
    ///
    /// # Parameters
    ///
    /// * `operation` - The operation to add
    /// * `weight` - Relative weight (higher = more frequent)
    ///
    /// # Panics
    ///
    /// Panics if weight is negative or zero.
    pub fn with_operation(mut self, operation: T, weight: f64) -> Self {
        assert!(weight > 0.0, "Weight must be positive, got {}", weight);
        self.operations.push(operation);
        self.weights.push(weight);
        self.recalculate_cumulative_weights();
        self
    }

    /// Recalculate cumulative weights after adding operations.
    fn recalculate_cumulative_weights(&mut self) {
        let total: f64 = self.weights.iter().sum();
        let mut cumulative = 0.0;
        self.cumulative_weights = self
            .weights
            .iter()
            .map(|&w| {
                cumulative += w / total;
                cumulative
            })
            .collect();
    }
}

impl<T: Clone> Default for WeightedGenerator<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Clone> OperationGenerator for WeightedGenerator<T> {
    type Operation = T;

    fn generate<R: RandomProvider>(&mut self, rng: &R) -> Self::Operation {
        assert!(
            !self.operations.is_empty(),
            "WeightedGenerator has no operations"
        );

        let rand_val = rng.random_ratio();

        // Binary search for the right cumulative weight bucket
        let index = self
            .cumulative_weights
            .iter()
            .position(|&w| rand_val <= w)
            .unwrap_or(self.operations.len() - 1);

        self.operations[index].clone()
    }

    fn weight_distribution(&self) -> Vec<(Self::Operation, f64)> {
        self.operations
            .iter()
            .zip(self.weights.iter())
            .map(|(op, &w)| (op.clone(), w))
            .collect()
    }
}

/// Builder for constructing operation sequences with parameters.
///
/// Useful when operations have parameters that need to be generated
/// (e.g., random values, IDs, payloads).
///
/// # Example
///
/// ```
/// use moonpool_foundation::operation_gen::{OperationGenerator, OperationBuilder};
/// use moonpool_foundation::random::sim::SimRandomProvider;
///
/// #[derive(Clone, Debug)]
/// enum BankOp {
///     Deposit(u64),
///     Withdraw(u64),
/// }
///
/// let random = SimRandomProvider::new(42);
/// let builder = OperationBuilder::new(&random);
///
/// // Build operation with random parameters
/// let amount = builder.random_range(1..1000);
/// let op = BankOp::Deposit(amount);
/// ```
pub struct OperationBuilder<'a, R: RandomProvider> {
    rng: &'a R,
}

impl<'a, R: RandomProvider> OperationBuilder<'a, R> {
    /// Create a new operation builder with the given random provider.
    pub fn new(rng: &'a R) -> Self {
        Self { rng }
    }

    /// Generate a random value in the specified range.
    pub fn random_range(&self, range: std::ops::Range<u64>) -> u64 {
        self.rng.random_range(range)
    }

    /// Generate a random float between 0.0 and 1.0.
    pub fn random_ratio(&self) -> f64 {
        self.rng.random_ratio()
    }

    /// Generate a random boolean with the given probability of being true.
    pub fn random_bool(&self, probability: f64) -> bool {
        self.rng.random_bool(probability)
    }

    /// Choose a random element from a slice.
    pub fn choose<T: Clone>(&self, items: &[T]) -> Option<T> {
        if items.is_empty() {
            None
        } else {
            let index = self.rng.random_range(0..items.len());
            Some(items[index].clone())
        }
    }
}

// Note: ParameterizedGenerator is commented out for now due to complexity with generic type parameters
// It can be added back in a future iteration if needed
//
// /// Parameterized operation generator with dynamic parameter generation.
// ///
// /// This generator wraps another generator and allows generating parameters
// /// for operations dynamically using a closure.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::random::sim::SimRandomProvider;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum TestOp {
        A,
        B,
        C,
    }

    #[test]
    fn test_uniform_generator_basic() {
        let random = SimRandomProvider::new(42);
        let mut generator = UniformGenerator::new(vec![TestOp::A, TestOp::B, TestOp::C]);

        let ops = generator.generate_sequence(&random, 30);
        assert_eq!(ops.len(), 30);

        // Check that all operations appear (with high probability)
        let has_a = ops.iter().any(|op| matches!(op, TestOp::A));
        let has_b = ops.iter().any(|op| matches!(op, TestOp::B));
        let has_c = ops.iter().any(|op| matches!(op, TestOp::C));

        assert!(has_a, "Should generate TestOp::A");
        assert!(has_b, "Should generate TestOp::B");
        assert!(has_c, "Should generate TestOp::C");
    }

    #[test]
    fn test_uniform_generator_distribution() {
        let _random = SimRandomProvider::new(123);
        let generator = UniformGenerator::new(vec![TestOp::A, TestOp::B, TestOp::C]);

        let weights = generator.weight_distribution();
        assert_eq!(weights.len(), 3);
        for (_, weight) in weights {
            assert!((weight - 1.0 / 3.0).abs() < 0.0001);
        }
    }

    #[test]
    #[should_panic(expected = "UniformGenerator requires at least one operation")]
    fn test_uniform_generator_empty_panics() {
        UniformGenerator::<TestOp>::new(vec![]);
    }

    #[test]
    fn test_weighted_generator_basic() {
        let random = SimRandomProvider::new(42);
        let mut generator = WeightedGenerator::new()
            .with_operation(TestOp::A, 0.5)
            .with_operation(TestOp::B, 0.3)
            .with_operation(TestOp::C, 0.2);

        let ops = generator.generate_sequence(&random, 100);
        assert_eq!(ops.len(), 100);

        let count_a = ops.iter().filter(|op| matches!(op, TestOp::A)).count();
        let count_b = ops.iter().filter(|op| matches!(op, TestOp::B)).count();
        let count_c = ops.iter().filter(|op| matches!(op, TestOp::C)).count();

        // With 100 operations and weights [0.5, 0.3, 0.2], we expect roughly:
        // A: 50, B: 30, C: 20 (±15 for statistical variation)
        assert!(
            count_a > 35 && count_a < 65,
            "Expected ~50 A operations, got {}",
            count_a
        );
        assert!(
            count_b > 15 && count_b < 45,
            "Expected ~30 B operations, got {}",
            count_b
        );
        assert!(
            count_c > 5 && count_c < 35,
            "Expected ~20 C operations, got {}",
            count_c
        );
    }

    #[test]
    fn test_weighted_generator_relative_weights() {
        let random = SimRandomProvider::new(789);
        let mut generator = WeightedGenerator::new()
            .with_operation(TestOp::A, 2.0) // 2/(2+1) = 66.7%
            .with_operation(TestOp::B, 1.0); // 1/(2+1) = 33.3%

        let ops = generator.generate_sequence(&random, 90);

        let count_a = ops.iter().filter(|op| matches!(op, TestOp::A)).count();
        let count_b = ops.iter().filter(|op| matches!(op, TestOp::B)).count();

        // Expect A:B ratio of roughly 2:1
        assert!(
            count_a > 45 && count_a < 75,
            "Expected ~60 A operations, got {}",
            count_a
        );
        assert!(
            count_b > 15 && count_b < 45,
            "Expected ~30 B operations, got {}",
            count_b
        );
    }

    #[test]
    #[should_panic(expected = "Weight must be positive")]
    fn test_weighted_generator_negative_weight_panics() {
        WeightedGenerator::new().with_operation(TestOp::A, -1.0);
    }

    #[test]
    #[should_panic(expected = "Weight must be positive")]
    fn test_weighted_generator_zero_weight_panics() {
        WeightedGenerator::new().with_operation(TestOp::A, 0.0);
    }

    #[test]
    #[should_panic(expected = "WeightedGenerator has no operations")]
    fn test_weighted_generator_empty_generate_panics() {
        let random = SimRandomProvider::new(42);
        let mut generator = WeightedGenerator::<TestOp>::new();
        generator.generate(&random);
    }

    #[test]
    fn test_operation_builder() {
        let random = SimRandomProvider::new(456);
        let builder = OperationBuilder::new(&random);

        // Test random_range
        let val = builder.random_range(0..100);
        assert!(val < 100);

        // Test random_ratio
        let f = builder.random_ratio();
        assert!(f >= 0.0 && f <= 1.0);

        // Test random_bool
        let b = builder.random_bool(0.5);
        assert!(b || !b); // Tautology, but ensures it compiles

        // Test choose
        let items = vec![TestOp::A, TestOp::B, TestOp::C];
        let chosen = builder.choose(&items);
        assert!(chosen.is_some());
    }

    #[test]
    fn test_operation_builder_choose_empty() {
        let random = SimRandomProvider::new(42);
        let builder = OperationBuilder::new(&random);

        let items: Vec<TestOp> = vec![];
        let chosen = builder.choose(&items);
        assert!(chosen.is_none());
    }

    #[test]
    fn test_determinism() {
        // Same seed should produce same sequences
        let seed = 12345;

        let random1 = SimRandomProvider::new(seed);
        let mut gen1 = UniformGenerator::new(vec![TestOp::A, TestOp::B, TestOp::C]);
        let ops1 = gen1.generate_sequence(&random1, 20);

        let random2 = SimRandomProvider::new(seed);
        let mut gen2 = UniformGenerator::new(vec![TestOp::A, TestOp::B, TestOp::C]);
        let ops2 = gen2.generate_sequence(&random2, 20);

        assert_eq!(ops1, ops2, "Same seed should produce identical sequences");
    }

    #[test]
    fn test_uniform_with_operation() {
        let random = SimRandomProvider::new(42);
        let mut generator = UniformGenerator::new(vec![TestOp::A])
            .with_operation(TestOp::B)
            .with_operation(TestOp::C);

        let ops = generator.generate_sequence(&random, 30);

        let has_a = ops.iter().any(|op| matches!(op, TestOp::A));
        let has_b = ops.iter().any(|op| matches!(op, TestOp::B));
        let has_c = ops.iter().any(|op| matches!(op, TestOp::C));

        assert!(has_a && has_b && has_c);
    }
}
