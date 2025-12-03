//! EndpointMap: Token → receiver routing (FDB pattern).
//!
//! Routes incoming packets by token to registered receivers.
//! Uses hybrid lookup: O(1) array for well-known tokens, HashMap for dynamic.
//!
//! # FDB Reference
//! From FlowTransport.actor.cpp:80-220

use std::collections::HashMap;
use std::rc::Rc;

use crate::{UID, WELL_KNOWN_RESERVED_COUNT, WellKnownToken};
use moonpool_sim::sometimes_assert;

use crate::error::MessagingError;

/// Trait for receiving deserialized messages from the transport layer.
///
/// Implementors handle incoming packets dispatched by EndpointMap.
/// The `receive` method is called synchronously during packet processing.
pub trait MessageReceiver {
    /// Process an incoming message payload.
    ///
    /// # Arguments
    /// * `payload` - Raw bytes to be deserialized by the receiver
    fn receive(&self, payload: &[u8]);

    /// Whether this receiver handles a stream of messages (default: true).
    ///
    /// Stream receivers can receive multiple messages over their lifetime.
    /// Non-stream receivers (promises) expect exactly one message.
    fn is_stream(&self) -> bool {
        true
    }
}

/// Maps endpoint tokens to message receivers (FDB-compatible pattern).
///
/// # Design
///
/// - **Well-known endpoints**: O(1) array access via token index (0-63)
/// - **Dynamic endpoints**: HashMap lookup by full UID
///
/// Well-known endpoints are checked first for hot-path performance.
///
/// # FDB Reference
/// From FlowTransport.actor.cpp:80-220 (EndpointMap class)
pub struct EndpointMap {
    /// Well-known receivers indexed by token.second (0-63).
    /// These use O(1) array access for hot-path performance.
    well_known: [Option<Rc<dyn MessageReceiver>>; WELL_KNOWN_RESERVED_COUNT],

    /// Dynamic receivers keyed by full UID.
    /// Used for endpoints allocated at runtime.
    dynamic: HashMap<UID, Rc<dyn MessageReceiver>>,

    /// Counter for metrics and debugging.
    registration_count: u64,
    deregistration_count: u64,
}

impl Default for EndpointMap {
    fn default() -> Self {
        Self::new()
    }
}

impl EndpointMap {
    /// Create a new empty endpoint map.
    pub fn new() -> Self {
        Self {
            well_known: std::array::from_fn(|_| None),
            dynamic: HashMap::new(),
            registration_count: 0,
            deregistration_count: 0,
        }
    }

    /// Register a well-known endpoint.
    ///
    /// Well-known endpoints have deterministic tokens and use O(1) array lookup.
    ///
    /// # Errors
    ///
    /// Returns error if the token index is out of range.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut map = EndpointMap::new();
    /// let receiver = Rc::new(MyReceiver::new());
    /// map.insert_well_known(WellKnownToken::Ping, receiver)?;
    /// ```
    pub fn insert_well_known(
        &mut self,
        token: WellKnownToken,
        receiver: Rc<dyn MessageReceiver>,
    ) -> Result<(), MessagingError> {
        let index = token.as_u32() as usize;
        if index >= WELL_KNOWN_RESERVED_COUNT {
            return Err(MessagingError::InvalidWellKnownToken {
                index: token.as_u32(),
                max: WELL_KNOWN_RESERVED_COUNT,
            });
        }
        self.well_known[index] = Some(receiver);
        self.registration_count += 1;
        sometimes_assert!(
            well_known_registered,
            true,
            "Well-known endpoint registered successfully"
        );
        Ok(())
    }

    /// Register a dynamic endpoint with the given UID.
    ///
    /// Dynamic endpoints use HashMap lookup. Call this when you need
    /// a specific UID (e.g., for request-response correlation).
    ///
    /// # Arguments
    ///
    /// * `token` - The UID for this endpoint
    /// * `receiver` - The receiver to handle incoming messages
    pub fn insert(&mut self, token: UID, receiver: Rc<dyn MessageReceiver>) {
        self.dynamic.insert(token, receiver);
        self.registration_count += 1;
    }

    /// Look up a receiver by token.
    ///
    /// Checks well-known endpoints first (O(1)), then dynamic (O(1) amortized).
    ///
    /// # Returns
    ///
    /// The receiver if found, or None if no endpoint is registered for this token.
    pub fn get(&self, token: &UID) -> Option<Rc<dyn MessageReceiver>> {
        // Check well-known first (hot path)
        if token.is_well_known() {
            let index = token.second as usize;
            if index < WELL_KNOWN_RESERVED_COUNT
                && let Some(receiver) = &self.well_known[index]
            {
                return Some(Rc::clone(receiver));
            }
            return None;
        }

        // Fall back to dynamic lookup
        let result = self.dynamic.get(token).cloned();
        sometimes_assert!(
            dynamic_lookup_found,
            result.is_some(),
            "Dynamic endpoint lookup succeeds"
        );
        result
    }

    /// Remove a dynamic endpoint.
    ///
    /// Note: Well-known endpoints cannot be removed.
    ///
    /// # Returns
    ///
    /// The removed receiver if it existed.
    pub fn remove(&mut self, token: &UID) -> Option<Rc<dyn MessageReceiver>> {
        if token.is_well_known() {
            // Well-known endpoints cannot be removed
            sometimes_assert!(
                well_known_removal_rejected,
                true,
                "Well-known endpoint removal correctly rejected"
            );
            return None;
        }

        let result = self.dynamic.remove(token);
        if result.is_some() {
            self.deregistration_count += 1;
            sometimes_assert!(
                endpoint_deregistered,
                true,
                "Dynamic endpoint deregistered successfully"
            );
        }
        result
    }

    /// Get the number of registered well-known endpoints.
    pub fn well_known_count(&self) -> usize {
        self.well_known.iter().filter(|e| e.is_some()).count()
    }

    /// Get the number of registered dynamic endpoints.
    pub fn dynamic_count(&self) -> usize {
        self.dynamic.len()
    }

    /// Get total registration count (for metrics).
    pub fn registration_count(&self) -> u64 {
        self.registration_count
    }

    /// Get total deregistration count (for metrics).
    pub fn deregistration_count(&self) -> u64 {
        self.deregistration_count
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    /// Mock receiver for testing.
    struct MockReceiver {
        received: RefCell<Vec<Vec<u8>>>,
    }

    impl MockReceiver {
        fn new() -> Self {
            Self {
                received: RefCell::new(Vec::new()),
            }
        }

        fn received_count(&self) -> usize {
            self.received.borrow().len()
        }

        fn last_received(&self) -> Option<Vec<u8>> {
            self.received.borrow().last().cloned()
        }
    }

    impl MessageReceiver for MockReceiver {
        fn receive(&self, payload: &[u8]) {
            self.received.borrow_mut().push(payload.to_vec());
        }
    }

    #[test]
    fn test_new_endpoint_map_is_empty() {
        let map = EndpointMap::new();
        assert_eq!(map.well_known_count(), 0);
        assert_eq!(map.dynamic_count(), 0);
        assert_eq!(map.registration_count(), 0);
        assert_eq!(map.deregistration_count(), 0);
    }

    #[test]
    fn test_insert_well_known() {
        let mut map = EndpointMap::new();
        let receiver = Rc::new(MockReceiver::new());

        map.insert_well_known(WellKnownToken::Ping, receiver.clone())
            .expect("insert should succeed");

        assert_eq!(map.well_known_count(), 1);
        assert_eq!(map.registration_count(), 1);
    }

    #[test]
    fn test_get_well_known() {
        let mut map = EndpointMap::new();
        let receiver = Rc::new(MockReceiver::new());

        map.insert_well_known(WellKnownToken::Ping, receiver.clone())
            .expect("insert should succeed");

        // Look up by well-known UID
        let token = WellKnownToken::Ping.uid();
        let found = map.get(&token);
        assert!(found.is_some());
    }

    #[test]
    fn test_get_well_known_not_registered() {
        let map = EndpointMap::new();

        // Look up unregistered well-known token
        let token = WellKnownToken::Ping.uid();
        let found = map.get(&token);
        assert!(found.is_none());
    }

    #[test]
    fn test_insert_dynamic() {
        let mut map = EndpointMap::new();
        let receiver = Rc::new(MockReceiver::new());
        let token = UID::new(0x1234, 0x5678);

        map.insert(token, receiver);

        assert_eq!(map.dynamic_count(), 1);
        assert_eq!(map.registration_count(), 1);
    }

    #[test]
    fn test_get_dynamic() {
        let mut map = EndpointMap::new();
        let receiver = Rc::new(MockReceiver::new());
        let token = UID::new(0x1234, 0x5678);

        map.insert(token, receiver.clone());

        let found = map.get(&token);
        assert!(found.is_some());
    }

    #[test]
    fn test_get_dynamic_not_registered() {
        let map = EndpointMap::new();
        let token = UID::new(0x1234, 0x5678);

        let found = map.get(&token);
        assert!(found.is_none());
    }

    #[test]
    fn test_remove_dynamic() {
        let mut map = EndpointMap::new();
        let receiver = Rc::new(MockReceiver::new());
        let token = UID::new(0x1234, 0x5678);

        map.insert(token, receiver);
        assert_eq!(map.dynamic_count(), 1);

        let removed = map.remove(&token);
        assert!(removed.is_some());
        assert_eq!(map.dynamic_count(), 0);
        assert_eq!(map.deregistration_count(), 1);

        // Should not find it anymore
        assert!(map.get(&token).is_none());
    }

    #[test]
    fn test_remove_well_known_not_allowed() {
        let mut map = EndpointMap::new();
        let receiver = Rc::new(MockReceiver::new());

        map.insert_well_known(WellKnownToken::Ping, receiver)
            .expect("insert should succeed");

        // Attempt to remove well-known endpoint should fail
        let token = WellKnownToken::Ping.uid();
        let removed = map.remove(&token);
        assert!(removed.is_none());

        // Should still be registered
        assert_eq!(map.well_known_count(), 1);
    }

    #[test]
    fn test_receiver_receives_payload() {
        let mut map = EndpointMap::new();
        let receiver = Rc::new(MockReceiver::new());
        let token = UID::new(0x1234, 0x5678);

        map.insert(token, receiver.clone());

        // Dispatch a message
        if let Some(r) = map.get(&token) {
            r.receive(b"hello world");
        }

        assert_eq!(receiver.received_count(), 1);
        assert_eq!(receiver.last_received(), Some(b"hello world".to_vec()));
    }

    #[test]
    fn test_o1_well_known_lookup() {
        // This test verifies the design intention (O(1) array access)
        // by checking that well-known tokens use array indexing
        let mut map = EndpointMap::new();
        let receiver = Rc::new(MockReceiver::new());

        // Register all well-known tokens
        for i in 0..4 {
            // EndpointNotFound, Ping, UnauthorizedEndpoint, FirstAvailable
            let token = match i {
                0 => WellKnownToken::EndpointNotFound,
                1 => WellKnownToken::Ping,
                2 => WellKnownToken::UnauthorizedEndpoint,
                3 => WellKnownToken::FirstAvailable,
                _ => unreachable!(),
            };
            map.insert_well_known(token, receiver.clone())
                .expect("insert should succeed");
        }

        // All should be found via O(1) array access
        for i in 0..4u32 {
            let uid = UID::well_known(i);
            assert!(
                map.get(&uid).is_some(),
                "well-known token {i} should be found"
            );
        }
    }
}
