// API Contract: Message Structure and Protocol
// This is a specification file, not compilable code

/// Message sent between actors across nodes.
///
/// Contains addressing, correlation, payload, and control metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// Unique correlation ID for request-response matching.
    pub correlation_id: CorrelationId,

    /// Message direction (Request, Response, OneWay).
    pub direction: Direction,

    /// Target actor identifier.
    pub target_actor: ActorId,

    /// Sender actor identifier.
    pub sender_actor: ActorId,

    /// Physical node hosting target actor.
    pub target_node: NodeId,

    /// Physical node hosting sender actor.
    pub sender_node: NodeId,

    /// Serialized message payload (serde_json).
    pub payload: Vec<u8>,

    /// Control flags for message processing.
    pub flags: MessageFlags,

    /// Optional expiration time for message delivery.
    pub time_to_expiry: Option<Instant>,
}

impl Message {
    /// Create request message.
    ///
    /// ## Parameters
    /// - `correlation_id` - Unique ID for response matching
    /// - `target_actor` - Destination actor
    /// - `sender_actor` - Source actor
    /// - `target_node` - Physical node hosting target
    /// - `sender_node` - Physical node hosting sender
    /// - `payload` - Serialized request data
    /// - `timeout` - Maximum wait duration for response
    ///
    /// ## Example
    /// ```ignore
    /// let msg = Message::request(
    ///     correlation_id,
    ///     "alice".into(),
    ///     "system".into(),
    ///     NodeId(1),
    ///     NodeId(0),
    ///     serde_json::to_vec(&request)?,
    ///     Duration::from_secs(30),
    /// );
    /// ```
    pub fn request(
        correlation_id: CorrelationId,
        target_actor: ActorId,
        sender_actor: ActorId,
        target_node: NodeId,
        sender_node: NodeId,
        payload: Vec<u8>,
        timeout: Duration,
    ) -> Self {
        Self {
            correlation_id,
            direction: Direction::Request,
            target_actor,
            sender_actor,
            target_node,
            sender_node,
            payload,
            flags: MessageFlags::empty(),
            time_to_expiry: Some(Instant::now() + timeout),
        }
    }

    /// Create response message from request.
    ///
    /// Automatically swaps target/sender and copies correlation ID.
    ///
    /// ## Example
    /// ```ignore
    /// let response = Message::response(&request, response_payload);
    /// ```
    pub fn response(request: &Message, payload: Vec<u8>) -> Self {
        Self {
            correlation_id: request.correlation_id,
            direction: Direction::Response,
            target_actor: request.sender_actor.clone(),  // Swap
            sender_actor: request.target_actor.clone(),  // Swap
            target_node: request.sender_node,            // Swap
            sender_node: request.target_node,            // Swap
            payload,
            flags: MessageFlags::empty(),
            time_to_expiry: None,  // Responses don't expire
        }
    }

    /// Create one-way message (no response expected).
    ///
    /// ## Example
    /// ```ignore
    /// let msg = Message::oneway(
    ///     "alice".into(),
    ///     "system".into(),
    ///     NodeId(1),
    ///     NodeId(0),
    ///     notification_payload,
    /// );
    /// ```
    pub fn oneway(
        target_actor: ActorId,
        sender_actor: ActorId,
        target_node: NodeId,
        sender_node: NodeId,
        payload: Vec<u8>,
    ) -> Self {
        Self {
            correlation_id: CorrelationId(0),  // OneWay doesn't need correlation
            direction: Direction::OneWay,
            target_actor,
            sender_actor,
            target_node,
            sender_node,
            payload,
            flags: MessageFlags::empty(),
            time_to_expiry: None,
        }
    }

    /// Check if message has expired.
    ///
    /// ## Returns
    /// `true` if `time_to_expiry` has passed, `false` otherwise.
    pub fn is_expired(&self) -> bool {
        self.time_to_expiry
            .map_or(false, |expiry| Instant::now() >= expiry)
    }
}

/// Message direction enum.
///
/// Determines message flow semantics and response expectations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    /// Request message expecting a response.
    ///
    /// - Creates `CallbackData` for response tracking
    /// - Has timeout enforcement
    /// - Response must copy correlation ID
    Request,

    /// Response message to a previous request.
    ///
    /// - Matches request via correlation ID
    /// - Completes pending `CallbackData`
    /// - Does not have timeout
    Response,

    /// One-way message with no response expected.
    ///
    /// - No `CallbackData` created
    /// - No timeout
    /// - Fire-and-forget semantics
    OneWay,
}

/// Message control flags.
///
/// Bitflags for controlling message processing behavior.
bitflags! {
    #[derive(Serialize, Deserialize)]
    pub struct MessageFlags: u16 {
        /// Message does not mutate actor state (read-only operation).
        ///
        /// Future use: Enable concurrent read-only message processing.
        const READ_ONLY = 1 << 0;

        /// Message can always interleave with other messages.
        ///
        /// Used for system messages, timers, diagnostics.
        /// Bypasses reentrancy checks.
        const ALWAYS_INTERLEAVE = 1 << 1;

        /// Message bound to specific activation instance.
        ///
        /// Cannot be forwarded to another node during migration or deactivation.
        /// Used for internal actor operations.
        const IS_LOCAL_ONLY = 1 << 2;

        /// Message does not extend actor lifetime.
        ///
        /// By default, receiving a message resets idle timeout.
        /// This flag prevents lifetime extension (for probes, diagnostics).
        const SUPPRESS_KEEP_ALIVE = 1 << 3;
    }
}

/// Wire format for messages over network.
///
/// Custom binary protocol on top of `PeerTransport::send(Vec<u8>)`.
///
/// ## Format (little-endian)
/// ```text
/// [message_type: 1 byte]                    // 1=Request, 2=Response, 3=OneWay
/// [correlation_id: 8 bytes (u64)]
///
/// // Target ActorId (namespace/actor_type/key)
/// [target_namespace_len: 4 bytes (u32)]
/// [target_namespace: N1 bytes (UTF-8)]
/// [target_actor_type_len: 4 bytes (u32)]
/// [target_actor_type: N2 bytes (UTF-8)]
/// [target_key_len: 4 bytes (u32)]
/// [target_key: N3 bytes (UTF-8)]
///
/// // Sender ActorId (namespace/actor_type/key)
/// [sender_namespace_len: 4 bytes (u32)]
/// [sender_namespace: M1 bytes (UTF-8)]
/// [sender_actor_type_len: 4 bytes (u32)]
/// [sender_actor_type: M2 bytes (UTF-8)]
/// [sender_key_len: 4 bytes (u32)]
/// [sender_key: M3 bytes (UTF-8)]
///
/// [target_node: 8 bytes (u64)]
/// [sender_node: 8 bytes (u64)]
/// [flags: 2 bytes (u16)]
/// [has_expiry: 1 byte (bool)]
/// [expiry_micros: 8 bytes (u64) if has_expiry]
/// [payload_len: 4 bytes (u32)]
/// [payload: P bytes]
/// ```
///
/// ## Total Header Size
/// - Fixed: 1 + 8 + 8 + 8 + 2 + 1 = 28 bytes
/// - Target ActorId: 12 + N1 + N2 + N3 bytes (3 length prefixes + 3 strings)
/// - Sender ActorId: 12 + M1 + M2 + M3 bytes (3 length prefixes + 3 strings)
/// - Optional expiry: 8 bytes
/// - Payload length: 4 bytes
///
/// Example:
/// - Target: "default"(7)/"BankAccount"(11)/"alice"(5) = 12 + 23 = 35 bytes
/// - Sender: "default"(7)/"System"(6)/"client"(6) = 12 + 19 = 31 bytes
/// - Total: 28 + 35 + 31 + 8 + 4 + payload = 106 + payload bytes
pub struct ActorEnvelope;

impl ActorEnvelope {
    /// Serialize message to wire format.
    ///
    /// ## Returns
    /// `Vec<u8>` ready for `peer.send()`.
    ///
    /// ## Example
    /// ```ignore
    /// let wire_bytes = ActorEnvelope::serialize(&message)?;
    /// peer.send(wire_bytes)?;
    /// ```
    pub fn serialize(message: &Message) -> Result<Vec<u8>, EnvelopeError> {
        unimplemented!("see messaging/protocol.rs")
    }

    /// Deserialize message from wire format.
    ///
    /// ## Returns
    /// - `Ok(Some(message))` - Complete message parsed
    /// - `Ok(None)` - Buffer empty (no data to parse)
    /// - `Err(InsufficientData)` - Need more bytes (partial message)
    /// - `Err(DeserializationFailed)` - Malformed data
    ///
    /// ## Example
    /// ```ignore
    /// let data = peer.receive().await?;
    /// let message = ActorEnvelope::deserialize(&data)?;
    /// ```
    pub fn deserialize(data: &[u8]) -> Result<Message, EnvelopeError> {
        unimplemented!("see messaging/protocol.rs")
    }

    /// Try to deserialize from mutable buffer (streaming reception).
    ///
    /// Consumes bytes from buffer if complete message found.
    ///
    /// ## Returns
    /// - `Ok(Some(message))` - Message parsed, bytes removed from buffer
    /// - `Ok(None)` - Insufficient data, buffer unchanged
    /// - `Err(...)` - Parse error
    ///
    /// ## Example
    /// ```ignore
    /// let mut buffer = Vec::new();
    /// loop {
    ///     let chunk = peer.try_receive();
    ///     buffer.extend_from_slice(&chunk);
    ///
    ///     while let Some(msg) = ActorEnvelope::try_deserialize(&mut buffer)? {
    ///         process_message(msg).await?;
    ///     }
    /// }
    /// ```
    pub fn try_deserialize(buffer: &mut Vec<u8>) -> Result<Option<Message>, EnvelopeError> {
        unimplemented!("see messaging/protocol.rs")
    }
}

/// Envelope serialization errors.
#[derive(Debug, Clone)]
pub enum EnvelopeError {
    /// Not enough bytes available to parse complete message.
    ///
    /// Contains (needed, available) byte counts.
    InsufficientData { needed: usize, available: usize },

    /// Message data is malformed or corrupt.
    DeserializationFailed(String),

    /// Message exceeds maximum size limit.
    MessageTooLarge { size: usize, max: usize },

    /// Invalid UTF-8 in actor ID.
    InvalidActorId(String),
}

/// Configuration for message size limits.
pub struct MessageConfig {
    /// Maximum message payload size (default: 1MB).
    pub max_payload_size: usize,

    /// Maximum actor ID length (default: 256 bytes).
    pub max_actor_id_length: usize,
}

impl Default for MessageConfig {
    fn default() -> Self {
        Self {
            max_payload_size: 1024 * 1024,  // 1MB
            max_actor_id_length: 256,
        }
    }
}

/// Unique identifier for request-response correlation.
///
/// Monotonically increasing counter per node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CorrelationId(pub u64);

impl CorrelationId {
    /// Create next correlation ID (thread-safe).
    ///
    /// Uses `AtomicU64` to ensure uniqueness.
    pub fn next(counter: &AtomicU64) -> Self {
        Self(counter.fetch_add(1, Ordering::SeqCst))
    }
}

/// Example message payloads (application-defined).
///
/// These are serialized into `Message.payload` via serde_json.

#[derive(Debug, Serialize, Deserialize)]
pub struct DepositRequest {
    pub amount: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DepositResponse {
    pub new_balance: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GetBalanceRequest;

#[derive(Debug, Serialize, Deserialize)]
pub struct GetBalanceResponse {
    pub balance: u64,
}

// Note: Actual message routing from typed requests (DepositRequest) to
// actor methods (deposit()) happens via:
// 1. serde_json serialization
// 2. Message envelope transport
// 3. serde_json deserialization
// 4. Dynamic dispatch based on message type or method name
//
// Implementation details in messaging/protocol.rs and actor/catalog.rs.
