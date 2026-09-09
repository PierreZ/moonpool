//! How a file is opened: [`OpenOptions`].
//!
//! Opening is where a file's *properties* are decided — read-only or
//! read-write, created or required to exist, truncated or preserved. Database
//! engines need a few more of those properties than a log writer does, and the
//! lesson this module encodes is that they belong here rather than in a second
//! provider stack: a journal opens an ordinary file, it does not open a
//! "journal device".

/// Whether an open should use direct (uncached) I/O.
///
/// Direct I/O bypasses the operating system's page cache, which is what a
/// database wants when it manages its own buffer pool: a read returns what the
/// device holds rather than what the kernel remembers, so a failed sync cannot
/// be papered over by re-reading a clean-marked page.
///
/// It is **not** durability. A direct write is still only visible until a
/// [`sync_all`](super::StorageFile::sync_all) /
/// [`sync_data`](super::StorageFile::sync_data) makes it durable, exactly as a
/// buffered one is.
///
/// Direct I/O comes with alignment requirements; read them off the opened file
/// with [`StorageFile::constraints`](super::StorageFile::constraints) rather
/// than assuming a value. Because those requirements cannot be expressed
/// through a shared cursor, a direct-I/O file serves
/// [`read_at`](super::StorageFile::read_at) /
/// [`write_at`](super::StorageFile::write_at) and refuses the stream API —
/// see [`StorageFile`](super::StorageFile).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DirectIo {
    /// Ordinary buffered I/O.
    #[default]
    Disabled,
    /// Ask for direct I/O, and accept buffered I/O where the platform or
    /// filesystem will not provide it (tmpfs, for instance).
    ///
    /// The open succeeds either way; ask the opened file which it got with
    /// [`StorageFile::is_direct_io`](super::StorageFile::is_direct_io).
    Optional,
    /// Require direct I/O: if it cannot be honored, the open **fails** with
    /// [`io::ErrorKind::Unsupported`](std::io::ErrorKind::Unsupported).
    ///
    /// Never silently downgraded.
    Required,
}

/// Options for opening a file.
///
/// A value-consuming builder, matching moonpool's provider conventions: each
/// setter takes `self` and returns it, so options compose in one expression
/// and an `OpenOptions` can be stored and reused.
///
/// ```
/// use moonpool_core::OpenOptions;
///
/// let options = OpenOptions::new().read(true).write(true).create(true);
/// assert!(options.is_read() && options.is_write());
/// ```
///
/// The individual flags are stored in a single bitfield and exposed via
/// `is_*` accessors.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpenOptions {
    /// Bit-packed flags. See the `FLAG_*` constants on this type.
    flags: u8,
    /// Direct-I/O policy for this open.
    direct_io: DirectIo,
}

impl OpenOptions {
    const FLAG_READ: u8 = 1 << 0;
    const FLAG_WRITE: u8 = 1 << 1;
    const FLAG_CREATE: u8 = 1 << 2;
    const FLAG_CREATE_NEW: u8 = 1 << 3;
    const FLAG_TRUNCATE: u8 = 1 << 4;
    const FLAG_APPEND: u8 = 1 << 5;

    /// Create new open options with all flags set to false.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the direct-I/O policy (see [`DirectIo`]).
    #[must_use]
    pub fn direct_io(mut self, policy: DirectIo) -> Self {
        self.direct_io = policy;
        self
    }

    /// The direct-I/O policy this open requests.
    #[must_use]
    pub fn requested_direct_io(&self) -> DirectIo {
        self.direct_io
    }

    fn set_flag(mut self, flag: u8, value: bool) -> Self {
        if value {
            self.flags |= flag;
        } else {
            self.flags &= !flag;
        }
        self
    }
}

macro_rules! flag_setters {
    ($($(#[$attr:meta])* $name:ident => $flag:ident),* $(,)?) => {
        impl OpenOptions {
            $(
                $(#[$attr])*
                #[must_use]
                pub fn $name(self, value: bool) -> Self {
                    self.set_flag(Self::$flag, value)
                }
            )*
        }
    };
}

flag_setters! {
    /// Set the read flag.
    read => FLAG_READ,
    /// Set the write flag.
    write => FLAG_WRITE,
    /// Set the create flag.
    create => FLAG_CREATE,
    /// Set the `create_new` flag.
    create_new => FLAG_CREATE_NEW,
    /// Set the truncate flag.
    truncate => FLAG_TRUNCATE,
    /// Set the append flag.
    ///
    /// Append mode moves the *stream cursor* to the end of the file before
    /// every stream write. It has no effect on positioned I/O
    /// ([`StorageFile::write_at`](super::StorageFile::write_at)), which
    /// always writes exactly where it is told.
    append => FLAG_APPEND,
}

macro_rules! flag_getters {
    ($($(#[$attr:meta])* $name:ident => $flag:ident),* $(,)?) => {
        impl OpenOptions {
            $(
                $(#[$attr])*
                #[must_use]
                pub fn $name(&self) -> bool {
                    self.flags & Self::$flag != 0
                }
            )*
        }
    };
}

flag_getters! {
    /// Returns true if the file will be opened for reading.
    is_read => FLAG_READ,
    /// Returns true if the file will be opened for writing.
    is_write => FLAG_WRITE,
    /// Returns true if the file will be created if it does not exist.
    is_create => FLAG_CREATE,
    /// Returns true if the file must be created new (failing if it exists).
    is_create_new => FLAG_CREATE_NEW,
    /// Returns true if the file will be truncated to zero length on open.
    is_truncate => FLAG_TRUNCATE,
    /// Returns true if stream writes will be appended to the end of the file.
    is_append => FLAG_APPEND,
}

impl OpenOptions {
    /// Create options for read-only access.
    #[must_use]
    pub fn read_only() -> Self {
        Self::new().read(true)
    }

    /// Create options for read-write access to an existing file.
    ///
    /// The shape a storage engine opens its data file with once the file is
    /// known to exist: no create, no truncate, both directions readable.
    #[must_use]
    pub fn read_write() -> Self {
        Self::new().read(true).write(true)
    }

    /// Create options for creating and writing a new file (truncating if exists).
    #[must_use]
    pub fn create_write() -> Self {
        Self::new().write(true).create(true).truncate(true)
    }

    /// Create options for creating a new file for writing (fails if exists).
    #[must_use]
    pub fn create_new_write() -> Self {
        Self::new().write(true).create_new(true)
    }
}

#[cfg(test)]
mod tests {
    use super::{DirectIo, OpenOptions};

    #[test]
    fn flags_round_trip() {
        let options = OpenOptions::new().read(true).write(true).append(true);
        assert!(options.is_read());
        assert!(options.is_write());
        assert!(options.is_append());
        assert!(!options.is_create());

        let cleared = options.append(false);
        assert!(!cleared.is_append());
        assert!(cleared.is_read());
    }

    #[test]
    fn presets_describe_themselves() {
        assert_eq!(OpenOptions::read_only(), OpenOptions::new().read(true));
        assert_eq!(
            OpenOptions::read_write(),
            OpenOptions::new().read(true).write(true)
        );
        assert!(OpenOptions::create_write().is_truncate());
        assert!(OpenOptions::create_new_write().is_create_new());
    }

    #[test]
    fn direct_io_is_off_unless_asked_for() {
        assert_eq!(OpenOptions::new().requested_direct_io(), DirectIo::Disabled);
        assert_eq!(
            OpenOptions::read_write()
                .direct_io(DirectIo::Required)
                .requested_direct_io(),
            DirectIo::Required
        );
    }
}
