/// Represents a message to be transmitted over the network.
///
/// This is the output type from the Sans I/O protocol state machine
/// that the I/O driver will process to perform actual network transmission.
#[derive(Debug, Clone)]
pub struct Transmit {
    /// Destination address where the message should be sent
    pub destination: String,
    /// Serialized envelope data ready for network transmission
    pub data: Vec<u8>,
}

impl Transmit {
    /// Create a new transmission request
    pub fn new(destination: String, data: Vec<u8>) -> Self {
        Self { destination, data }
    }

    /// Get the size of the data payload
    pub fn data_len(&self) -> usize {
        self.data.len()
    }
}
