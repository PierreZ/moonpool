//! Actor lifecycle state machine and deactivation reasons.

use serde::{Deserialize, Serialize};

/// Actor lifecycle state machine.
///
/// # State Transitions
///
/// ```text
/// Creating → Activating → Valid → Deactivating → Invalid
///          ↓
///          Deactivating (activation failed)
/// ```
///
/// # Validation Rules
///
/// - Must not process messages unless state == Valid
/// - Must not transition backward (except activation failure)
/// - Transitions guarded by `can_transition_to()` validation
///
/// # Invariants
///
/// - Actor in Valid state has completed `on_activate()` successfully
/// - Actor in Invalid state cannot transition to any other state
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActivationState {
    /// Instance created, not yet activated.
    Creating,

    /// `on_activate()` in progress.
    Activating,

    /// Ready to process messages.
    Valid,

    /// `on_deactivate()` in progress.
    Deactivating,

    /// Disposed, no longer usable.
    Invalid,
}

impl ActivationState {
    /// Check if transition to next state is valid.
    ///
    /// # Valid Transitions
    ///
    /// - Creating → Activating
    /// - Activating → Valid (activation succeeded)
    /// - Activating → Deactivating (activation failed)
    /// - Valid → Deactivating
    /// - Deactivating → Invalid
    pub fn can_transition_to(&self, next: ActivationState) -> bool {
        use ActivationState::*;
        matches!(
            (self, next),
            (Creating, Activating)
                | (Activating, Valid)
                | (Activating, Deactivating)
                | (Valid, Deactivating)
                | (Deactivating, Invalid)
        )
    }

    /// Check if actor can process messages in this state.
    pub fn can_process_messages(&self) -> bool {
        matches!(self, ActivationState::Valid)
    }

    /// Check if this is a terminal state.
    pub fn is_terminal(&self) -> bool {
        matches!(self, ActivationState::Invalid)
    }
}

/// Reason why an actor is being deactivated.
///
/// Used in `Actor::on_deactivate()` to inform the actor why it's being removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeactivationReason {
    /// Actor has been idle for longer than the idle timeout (default: 10 minutes).
    IdleTimeout,

    /// Explicit deactivation request (e.g., via ActorRuntime::deactivate_actor).
    ExplicitRequest,

    /// Node is shutting down.
    NodeShutdown,

    /// Activation failed during `on_activate()`.
    ActivationFailed,

    /// Lost activation race (another node won).
    ActivationRace,

    /// Actor threw an unrecoverable error.
    UnrecoverableError,
}

impl DeactivationReason {
    /// Check if this reason indicates a failure.
    pub fn is_failure(&self) -> bool {
        matches!(
            self,
            DeactivationReason::ActivationFailed | DeactivationReason::UnrecoverableError
        )
    }

    /// Check if actor should be removed immediately without cleanup.
    pub fn is_immediate(&self) -> bool {
        matches!(
            self,
            DeactivationReason::NodeShutdown | DeactivationReason::ActivationRace
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_activation_state_transitions() {
        use ActivationState::*;

        // Valid transitions
        assert!(Creating.can_transition_to(Activating));
        assert!(Activating.can_transition_to(Valid));
        assert!(Activating.can_transition_to(Deactivating)); // Activation failure
        assert!(Valid.can_transition_to(Deactivating));
        assert!(Deactivating.can_transition_to(Invalid));

        // Invalid transitions
        assert!(!Creating.can_transition_to(Valid)); // Skip Activating
        assert!(!Valid.can_transition_to(Creating)); // Backward
        assert!(!Invalid.can_transition_to(Creating)); // From terminal state
        assert!(!Deactivating.can_transition_to(Activating)); // Backward
    }

    #[test]
    fn test_activation_state_can_process_messages() {
        use ActivationState::*;

        assert!(!Creating.can_process_messages());
        assert!(!Activating.can_process_messages());
        assert!(Valid.can_process_messages()); // Only Valid state
        assert!(!Deactivating.can_process_messages());
        assert!(!Invalid.can_process_messages());
    }

    #[test]
    fn test_activation_state_is_terminal() {
        use ActivationState::*;

        assert!(!Creating.is_terminal());
        assert!(!Activating.is_terminal());
        assert!(!Valid.is_terminal());
        assert!(!Deactivating.is_terminal());
        assert!(Invalid.is_terminal()); // Only Invalid is terminal
    }

    #[test]
    fn test_deactivation_reason_is_failure() {
        use DeactivationReason::*;

        assert!(ActivationFailed.is_failure());
        assert!(UnrecoverableError.is_failure());

        assert!(!IdleTimeout.is_failure());
        assert!(!ExplicitRequest.is_failure());
        assert!(!NodeShutdown.is_failure());
        assert!(!ActivationRace.is_failure());
    }

    #[test]
    fn test_deactivation_reason_is_immediate() {
        use DeactivationReason::*;

        assert!(NodeShutdown.is_immediate());
        assert!(ActivationRace.is_immediate());

        assert!(!IdleTimeout.is_immediate());
        assert!(!ExplicitRequest.is_immediate());
        assert!(!ActivationFailed.is_immediate());
        assert!(!UnrecoverableError.is_immediate());
    }
}
