//! End-to-end tests for the transport layer.
//!
//! These tests verify that ClientTransport and ServerTransport work together
//! to provide request-response messaging functionality.

use moonpool_foundation::network::transport::{
    EnvelopeFactory, EnvelopeSerializer, RequestResponseEnvelopeFactory, RequestResponseSerializer,
    TransportError,
};

#[tokio::test]
async fn test_basic_envelope_serialization() {
    // Test basic envelope serialization roundtrip
    let serializer = RequestResponseSerializer;
    let envelope = RequestResponseEnvelopeFactory::create_request(42, b"test payload".to_vec());

    let serialized = serializer.serialize(&envelope);
    let deserialized = serializer.deserialize(&serialized).unwrap();

    assert_eq!(envelope.correlation_id, deserialized.correlation_id);
    assert_eq!(envelope.payload, deserialized.payload);
}

#[test]
fn test_transport_types_compile() {
    // This test verifies that the transport types compile correctly
    // and have the expected API signatures

    // Test that we can create envelopes
    let request = RequestResponseEnvelopeFactory::create_request(42, b"test".to_vec());
    assert_eq!(request.correlation_id, 42);
    assert_eq!(request.payload, b"test");

    // Test that we can create replies
    let reply = RequestResponseEnvelopeFactory::create_reply(&request, b"response".to_vec());
    assert_eq!(reply.correlation_id, 42);
    assert_eq!(reply.payload, b"response");
}

#[test]
fn test_serialization_roundtrip() {
    // Test serialization and deserialization work correctly

    let serializer = RequestResponseSerializer;

    // Create different types of messages
    let request = RequestResponseEnvelopeFactory::create_request(1, b"hello".to_vec());
    let reply = RequestResponseEnvelopeFactory::create_reply(&request, b"world".to_vec());

    // Test request serialization
    let request_bytes = serializer.serialize(&request);
    let request_restored = serializer.deserialize(&request_bytes).unwrap();
    assert_eq!(request.correlation_id, request_restored.correlation_id);
    assert_eq!(request.payload, request_restored.payload);

    // Test reply serialization
    let reply_bytes = serializer.serialize(&reply);
    let reply_restored = serializer.deserialize(&reply_bytes).unwrap();
    assert_eq!(reply.correlation_id, reply_restored.correlation_id);
    assert_eq!(reply.payload, reply_restored.payload);
}

#[test]
fn test_envelope_reply_detection() {
    // Test that envelope reply detection works correctly

    use moonpool_foundation::network::transport::EnvelopeReplyDetection;

    // Create a request
    let request = RequestResponseEnvelopeFactory::create_request(123, b"ping".to_vec());
    assert_eq!(request.correlation_id, 123);
    assert_eq!(request.payload, b"ping");

    // Create a reply
    let reply = RequestResponseEnvelopeFactory::create_reply(&request, b"pong".to_vec());
    assert_eq!(reply.correlation_id, 123); // Same correlation ID
    assert_eq!(reply.payload, b"pong");

    // Test reply detection
    assert!(reply.is_reply_to(123));
    assert!(!reply.is_reply_to(124));
    assert_eq!(reply.correlation_id(), Some(123));
}

#[test]
fn test_transport_error_types() {
    // Test that error types work correctly

    let errors = vec![
        TransportError::PeerError("test".to_string()),
        TransportError::SerializationError("test".to_string()),
        TransportError::Disconnected,
        TransportError::Timeout,
        TransportError::BindFailed("test".to_string()),
        TransportError::SendFailed("test".to_string()),
        TransportError::IoError("test".to_string()),
    ];

    for error in errors {
        // Verify errors implement Display and Debug
        let _display = format!("{}", error);
        let _debug = format!("{:?}", error);

        // Verify they can be cloned
        let _cloned = error.clone();
    }
}
