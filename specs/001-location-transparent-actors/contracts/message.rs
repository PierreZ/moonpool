// API Contract: Message Structure and Protocol
// This is a specification file, not compilable code

/// Maximum number of times a message can be forwarded before being rejected.
///
/// Prevents infinite forwarding loops when actor locations are stale.
/// Default: 2 (original send + 2 forwards = 3 total hops max)
pub const MAX_FORWARD_COUNT: u8 = 2;

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

    /// Method name for routing to correct handler (e.g., "deposit", "withdraw").
    ///
    /// Used by receiving node to dispatch message to appropriate MessageHandler implementation.
    /// Empty for responses (routing via correlation_id only).
    pub method_name: String,

    /// Serialized message payload (serde_json).
    pub payload: Vec<u8>,

    /// Control flags for message processing.
    pub flags: MessageFlags,

    /// Optional expiration time for message delivery.
    pub time_to_expiry: Option<Instant>,

    /// Number of times message has been forwarded (prevents loops).
    ///
    /// Incremented each time message is forwarded to different node.
    /// MUST NOT exceed MAX_FORWARD_COUNT (default: 2).
    pub forward_count: u8,

    /// Optional cache invalidation updates (piggybacked on responses).
    ///
    /// When a message is forwarded due to stale directory cache, the response
    /// includes cache updates so sender can refresh its local cache.
    pub cache_invalidation: Option<Vec<CacheUpdate>>,
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
    /// - `method_name` - Method to invoke on actor (e.g., "deposit")
    /// - `payload` - Serialized request data
    /// - `timeout` - Maximum wait duration for response
    ///
    /// ## Example
    /// ```ignore
    /// let msg = Message::request(
    ///     correlation_id,
    ///     "alice".into(),
    ///     "system".into(),
    ///     NodeId::from("127.0.0.1:5001")?,
    ///     NodeId::from("127.0.0.1:5000")?,
    ///     "deposit",
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
        method_name: impl Into<String>,
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
            method_name: method_name.into(),
            payload,
            flags: MessageFlags::empty(),
            time_to_expiry: Some(Instant::now() + timeout),
            forward_count: 0,
            cache_invalidation: None,
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
            method_name: String::new(),  // Responses don't need method routing
            payload,
            flags: MessageFlags::empty(),
            time_to_expiry: None,  // Responses don't expire
            forward_count: 0,  // Responses start fresh
            cache_invalidation: None,  // Can be set later when forwarding
        }
    }

    /// Create one-way message (no response expected).
    ///
    /// ## Example
    /// ```ignore
    /// let msg = Message::oneway(
    ///     "alice".into(),
    ///     "system".into(),
    ///     NodeId::from("127.0.0.1:5001")?,
    ///     NodeId::from("127.0.0.1:5000")?,
    ///     "notify",
    ///     notification_payload,
    /// );
    /// ```
    pub fn oneway(
        target_actor: ActorId,
        sender_actor: ActorId,
        target_node: NodeId,
        sender_node: NodeId,
        method_name: impl Into<String>,
        payload: Vec<u8>,
    ) -> Self {
        Self {
            correlation_id: CorrelationId(0),  // OneWay doesn't need correlation
            direction: Direction::OneWay,
            target_actor,
            sender_actor,
            target_node,
            sender_node,
            method_name: method_name.into(),
            payload,
            flags: MessageFlags::empty(),
            time_to_expiry: None,
            forward_count: 0,
            cache_invalidation: None,
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
/// // Target NodeId (address:port)
/// [target_node_len: 4 bytes (u32)]
/// [target_node: T bytes (UTF-8 "host:port")]
///
/// // Sender NodeId (address:port)
/// [sender_node_len: 4 bytes (u32)]
/// [sender_node: S bytes (UTF-8 "host:port")]
///
/// [flags: 2 bytes (u16)]
/// [forward_count: 1 byte (u8)]
/// [has_expiry: 1 byte (bool)]
/// [expiry_micros: 8 bytes (u64) if has_expiry]
///
/// // Method routing information
/// [method_name_len: 4 bytes (u32)]
/// [method_name: L bytes (UTF-8)]           // "deposit", "withdraw", "get_balance"
///
/// [payload_len: 4 bytes (u32)]
/// [payload: P bytes]
/// ```
///
/// ## Total Header Size
/// - Fixed: 1 + 8 + 2 + 1 + 1 = 13 bytes
/// - Target ActorId: 12 + N1 + N2 + N3 bytes (3 length prefixes + 3 strings)
/// - Sender ActorId: 12 + M1 + M2 + M3 bytes (3 length prefixes + 3 strings)
/// - Target NodeId: 4 + T bytes (length prefix + "host:port" string)
/// - Sender NodeId: 4 + S bytes (length prefix + "host:port" string)
/// - Optional expiry: 8 bytes
/// - Payload length: 4 bytes
///
/// Example:
/// - Target ActorId: "default"(7)/"BankAccount"(11)/"alice"(5) = 12 + 23 = 35 bytes
/// - Sender ActorId: "default"(7)/"System"(6)/"client"(6) = 12 + 19 = 31 bytes
/// - Target NodeId: "127.0.0.1:5001"(14) = 4 + 14 = 18 bytes
/// - Sender NodeId: "127.0.0.1:5000"(14) = 4 + 14 = 18 bytes
/// - Total: 13 + 35 + 31 + 18 + 18 + 8 + 4 + payload = 127 + payload bytes
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

/// Complete location information for an actor.
///
/// Combines virtual identity (ActorId) with physical location (NodeId).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorAddress {
    /// Virtual actor identifier
    pub actor_id: ActorId,

    /// Physical node hosting actor
    pub node_id: NodeId,

    /// When actor was activated (for cache staleness detection)
    pub activation_time: Instant,
}

impl ActorAddress {
    /// Create new actor address
    pub fn new(actor_id: ActorId, node_id: NodeId, activation_time: Instant) -> Self {
        Self {
            actor_id,
            node_id,
            activation_time,
        }
    }
}

/// Cache invalidation update.
///
/// Piggybacked on message responses to proactively update directory caches.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheUpdate {
    /// Stale address to invalidate.
    pub invalid_address: ActorAddress,

    /// New valid address (if known).
    pub valid_address: Option<ActorAddress>,
}

impl CacheUpdate {
    /// Create cache invalidation update.
    ///
    /// ## Example
    /// ```ignore
    /// // Actor moved from node_1 to node_2
    /// let update = CacheUpdate::new(
    ///     ActorAddress::new("alice".into(), NodeId::from("127.0.0.1:5001")?, Instant::now()),
    ///     Some(ActorAddress::new("alice".into(), NodeId::from("127.0.0.1:5002")?, Instant::now())),
    /// );
    /// ```
    pub fn new(invalid_address: ActorAddress, valid_address: Option<ActorAddress>) -> Self {
        Self {
            invalid_address,
            valid_address,
        }
    }
}

/// Unique identifier for request-response correlation.
///
/// Monotonically increasing counter per node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CorrelationId(pub u64);

impl CorrelationId {
    /// Create next correlation ID (single-threaded).
    ///
    /// Uses `Cell<u64>` for interior mutability (no atomics needed in current_thread runtime).
    pub fn next(counter: &Cell<u64>) -> Self {
        let id = counter.get();
        counter.set(id + 1);
        Self(id)
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

/// ## Method Routing: End-to-End Flow
///
/// This section explains how typed requests (e.g., `DepositRequest`) are routed to actor methods
/// (e.g., `actor.deposit()`) across the network.
///
/// ### Sending Side (Node A)
///
/// 1. **User calls ActorRef method**:
///    ```ignore
///    let alice: ActorRef<BankAccountActor> = runtime.get_actor("BankAccount", "alice");
///    let balance = alice.call(DepositRequest { amount: 100 }).await?;
///    ```
///
/// 2. **ActorRef::call() serializes request**:
///    ```ignore
///    impl<A: Actor> ActorRef<A> {
///        pub async fn call<Req, Res>(&self, request: Req) -> Result<Res, ActorError> {
///            // Serialize request to JSON
///            let payload = serde_json::to_vec(&request)?;
///
///            // Extract method name from request type
///            let method_name = std::any::type_name::<Req>()
///                .rsplit("::")
///                .next()
///                .unwrap_or("unknown");
///
///            // Create message with method routing
///            let message = Message::request(
///                correlation_id,
///                self.actor_id.clone(),
///                sender_actor_id,
///                target_node,
///                sender_node,
///                method_name,  // "DepositRequest"
///                payload,
///                timeout,
///            );
///
///            // Send via MessageBus
///            let response = self.message_bus.send_request(message).await?;
///
///            // Deserialize response
///            let result: Res = serde_json::from_slice(&response.payload)?;
///            Ok(result)
///        }
///    }
///    ```
///
/// 3. **MessageBus looks up target node via Directory**:
///    ```ignore
///    let target_node = directory.lookup(&actor_id).await
///        .unwrap_or_else(|| directory.choose_node_for_placement());
///    ```
///
/// 4. **Message serialized to wire format and sent via Peer**:
///    ```ignore
///    let wire_bytes = ActorEnvelope::serialize(&message)?;
///    peer.send(wire_bytes)?;
///    ```
///
/// ### Receiving Side (Node B)
///
/// 5. **Peer receives bytes and deserializes**:
///    ```ignore
///    let wire_bytes = peer.receive().await?;
///    let message = ActorEnvelope::deserialize(&wire_bytes)?;
///    // message.method_name = "DepositRequest"
///    ```
///
/// 6. **ActorCatalog gets or creates actor activation**:
///    ```ignore
///    let context = catalog.get_or_create_activation(message.target_actor).await?;
///    ```
///
/// 7. **MessageBus dispatches to handler using method_name**:
///    ```ignore
///    impl MessageBus {
///        async fn dispatch_message(&self, context: &ActorContext, message: Message) -> Result<Vec<u8>> {
///            match message.method_name.as_str() {
///                "DepositRequest" => {
///                    // Deserialize request
///                    let req: DepositRequest = serde_json::from_slice(&message.payload)?;
///
///                    // Call handler trait
///                    let mut actor = context.actor_instance.borrow_mut();
///                    let response = <BankAccountActor as MessageHandler<DepositRequest, u64>>::handle(
///                        &mut *actor,
///                        req
///                    ).await?;
///
///                    // Serialize response
///                    Ok(serde_json::to_vec(&response)?)
///                }
///                "WithdrawRequest" => { /* similar */ }
///                "GetBalanceRequest" => { /* similar */ }
///                _ => Err(ActorError::UnknownMethod(message.method_name))
///            }
///        }
///    }
///    ```
///
/// 8. **Response sent back via MessageBus**:
///    ```ignore
///    let response = Message::response(&message, response_payload);
///    self.message_bus.send_response(response).await?;
///    ```
///
/// 9. **Original caller receives response via correlation ID**:
///    ```ignore
///    // MessageBus matches response.correlation_id to pending request
///    let callback_data = self.pending_requests.remove(&response.correlation_id)?;
///    callback_data.response_sender.send(Ok(response))?;
///    ```
///
/// ### Method Name Strategies
///
/// **Option 1: Type Name (shown above)**
/// ```ignore
/// let method_name = std::any::type_name::<Req>(); // "DepositRequest"
/// ```
/// - ✅ No boilerplate
/// - ✅ Compile-time type safety
/// - ❌ Tight coupling between request type name and handler
///
/// **Option 2: Trait Method**
/// ```ignore
/// trait MethodName {
///     fn method_name() -> &'static str;
/// }
///
/// impl MethodName for DepositRequest {
///     fn method_name() -> &'static str { "deposit" }
/// }
/// ```
/// - ✅ Decouples request type from method name
/// - ✅ More flexible
/// - ❌ Requires trait implementation per request
///
/// **Option 3: Procedural Macro**
/// ```ignore
/// #[derive(Serialize, Deserialize, MethodName)]
/// #[method_name = "deposit"]
/// pub struct DepositRequest { amount: u64 }
/// ```
/// - ✅ Zero runtime overhead
/// - ✅ Clean syntax
/// - ❌ Requires proc macro dependency
///
/// **Decision**: Start with **Option 1** (type name) for simplicity, add Option 2/3 later if needed.
///
/// ### Handler Registration: Compile-Time vs Runtime
///
/// **Compile-Time (Trait-Based)**: Handler resolution via trait bounds (shown above)
/// - Pro: Type-safe, no registration boilerplate
/// - Con: Requires knowing concrete actor type at dispatch site
///
/// **Runtime (Registry-Based)**: Map method_name to handler function
/// ```ignore
/// pub struct HandlerRegistry {
///     handlers: HashMap<String, Box<dyn HandlerFn>>,
/// }
///
/// impl BankAccountActor {
///     fn register_handlers(registry: &mut HandlerRegistry) {
///         registry.register("DepositRequest", |actor, payload| {
///             let req: DepositRequest = serde_json::from_slice(payload)?;
///             actor.deposit(req.amount).await
///         });
///     }
/// }
/// ```
/// - Pro: Dynamic actor types, no monomorphization
/// - Con: Runtime overhead, less type-safe
///
/// **Decision**: Use **trait-based** for type safety, defer registry pattern to future optimization.
///
/// See also:
/// - `contracts/actor.rs` - MessageHandler trait definition
/// - `data-model.md` - Complete message flow with state machines
/// - `messaging/protocol.rs` - ActorEnvelope wire format implementation
