//! Little-endian fixed-width field helpers for the hand-written layouts.

/// Appends little-endian fields.
pub(crate) struct Writer(pub(crate) Vec<u8>);

impl Writer {
    pub(crate) fn new() -> Self {
        Self(Vec::new())
    }

    pub(crate) fn u8(&mut self, value: u8) -> &mut Self {
        self.0.push(value);
        self
    }

    pub(crate) fn u16(&mut self, value: u16) -> &mut Self {
        self.0.extend_from_slice(&value.to_le_bytes());
        self
    }

    pub(crate) fn u32(&mut self, value: u32) -> &mut Self {
        self.0.extend_from_slice(&value.to_le_bytes());
        self
    }

    pub(crate) fn u64(&mut self, value: u64) -> &mut Self {
        self.0.extend_from_slice(&value.to_le_bytes());
        self
    }

    pub(crate) fn u128(&mut self, value: u128) -> &mut Self {
        self.0.extend_from_slice(&value.to_le_bytes());
        self
    }

    /// Family byte (4 or 6), the address octets, then the port.
    pub(crate) fn socket_addr(&mut self, value: std::net::SocketAddr) -> &mut Self {
        match value.ip() {
            std::net::IpAddr::V4(ip) => self.u8(4).bytes(&ip.octets()),
            std::net::IpAddr::V6(ip) => self.u8(6).bytes(&ip.octets()),
        };
        self.u16(value.port())
    }

    pub(crate) fn bytes(&mut self, value: &[u8]) -> &mut Self {
        self.0.extend_from_slice(value);
        self
    }
}

/// Reads little-endian fields; every read is bounds-checked.
pub(crate) struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self(bytes)
    }

    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (head, rest) = self.0.split_first_chunk::<N>()?;
        self.0 = rest;
        Some(*head)
    }

    pub(crate) fn u8(&mut self) -> Option<u8> {
        self.take::<1>().map(|[byte]| byte)
    }

    pub(crate) fn u16(&mut self) -> Option<u16> {
        self.take().map(u16::from_le_bytes)
    }

    pub(crate) fn u32(&mut self) -> Option<u32> {
        self.take().map(u32::from_le_bytes)
    }

    pub(crate) fn u64(&mut self) -> Option<u64> {
        self.take().map(u64::from_le_bytes)
    }

    pub(crate) fn u128(&mut self) -> Option<u128> {
        self.take().map(u128::from_le_bytes)
    }

    pub(crate) fn socket_addr(&mut self) -> Option<std::net::SocketAddr> {
        let ip = match self.u8()? {
            4 => std::net::IpAddr::from(self.take::<4>()?),
            6 => std::net::IpAddr::from(self.take::<16>()?),
            _ => return None,
        };
        Some(std::net::SocketAddr::new(ip, self.u16()?))
    }

    pub(crate) fn slice(&mut self, len: usize) -> Option<&'a [u8]> {
        if self.0.len() < len {
            return None;
        }
        let (head, rest) = self.0.split_at(len);
        self.0 = rest;
        Some(head)
    }

    /// Everything not read yet.
    pub(crate) fn rest(self) -> &'a [u8] {
        self.0
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}
