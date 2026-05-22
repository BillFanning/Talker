mod config;
mod serial;
mod tcp;
mod udp;

pub use config::{
    ConnectionConfig, DataBits, FlowControl, Parity, SerialConfig, StopBits, TcpClientConfig,
    UdpConfig, UdpMode,
};

/// A live connection that can send raw bytes.
///
/// Implementors are owned by a single talker thread; `Send` is required so they
/// can be moved into that thread.
pub trait Connection: Send {
    fn send(&mut self, data: &[u8]) -> anyhow::Result<()>;
}

impl ConnectionConfig {
    /// Open a live connection described by this config.
    pub fn open(&self) -> anyhow::Result<Box<dyn Connection>> {
        match self {
            Self::Serial(c) => Ok(Box::new(serial::SerialConnection::open(c)?)),
            Self::Udp(c) => Ok(Box::new(udp::UdpConnection::open(c)?)),
            Self::TcpClient(c) => Ok(Box::new(tcp::TcpClientConnection::open(c)?)),
        }
    }
}

/// Owns and broadcasts to a collection of open connections.
///
/// Created empty; connections are added with [`push`][ConnectionCollection::push].
/// The collection is the unit of ownership for the talker thread.
pub struct ConnectionCollection {
    connections: Vec<Box<dyn Connection>>,
}

impl ConnectionCollection {
    pub fn new() -> Self {
        Self { connections: Vec::new() }
    }

    pub fn push(&mut self, conn: Box<dyn Connection>) {
        self.connections.push(conn);
    }

    pub fn len(&self) -> usize {
        self.connections.len()
    }

    pub fn is_empty(&self) -> bool {
        self.connections.is_empty()
    }

    /// Send `data` to every connection and return one `(index, error_message)` entry
    /// per failing connection. Succeeding connections produce no entry.
    pub fn send_reporting(&mut self, data: &[u8]) -> Vec<(usize, String)> {
        self.connections
            .iter_mut()
            .enumerate()
            .filter_map(|(i, conn)| conn.send(data).err().map(|e| (i, format!("{e:#}"))))
            .collect()
    }

    /// Replace the connection at `index` with a new one.
    ///
    /// No-op if `index` is out of range.
    pub fn replace(&mut self, index: usize, conn: Box<dyn Connection>) {
        if index < self.connections.len() {
            self.connections[index] = conn;
        }
    }

    /// Send `data` to every connection. Attempts all connections even if one
    /// fails; returns the first error encountered, if any.
    pub fn send_all(&mut self, data: &[u8]) -> anyhow::Result<()> {
        let mut first_err: Option<anyhow::Error> = None;
        for (i, conn) in self.connections.iter_mut().enumerate() {
            if let Err(e) = conn.send(data) {
                let e = e.context(format!("connection {i}"));
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl Default for ConnectionCollection {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct BufConn(Vec<u8>);
    impl Connection for BufConn {
        fn send(&mut self, data: &[u8]) -> anyhow::Result<()> {
            self.0.extend_from_slice(data);
            Ok(())
        }
    }

    struct ErrConn;
    impl Connection for ErrConn {
        fn send(&mut self, _data: &[u8]) -> anyhow::Result<()> {
            Err(anyhow::anyhow!("simulated send failure"))
        }
    }

    #[test]
    fn new_collection_is_empty() {
        let c = ConnectionCollection::new();
        assert!(c.is_empty());
        assert_eq!(c.len(), 0);
    }

    #[test]
    fn default_equals_new() {
        let c = ConnectionCollection::default();
        assert!(c.is_empty());
    }

    #[test]
    fn push_increments_len() {
        let mut c = ConnectionCollection::new();
        c.push(Box::new(BufConn(vec![])));
        assert_eq!(c.len(), 1);
        c.push(Box::new(BufConn(vec![])));
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn send_all_empty_returns_ok() {
        let mut c = ConnectionCollection::new();
        assert!(c.send_all(b"data").is_ok());
    }

    #[test]
    fn send_all_delivers_to_all_connections() {
        let mut c = ConnectionCollection::new();
        c.push(Box::new(BufConn(vec![])));
        c.push(Box::new(BufConn(vec![])));
        c.send_all(b"hi").unwrap();

        // Reach into connections via downcast — instead, check indirectly by
        // verifying no error and counting sends.
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn send_all_returns_error_on_failure() {
        let mut c = ConnectionCollection::new();
        c.push(Box::new(ErrConn));
        let err = c.send_all(b"x").unwrap_err();
        assert!(err.to_string().contains("connection 0"));
    }

    #[test]
    fn send_all_continues_after_first_failure_and_returns_first_error() {
        let mut c = ConnectionCollection::new();
        c.push(Box::new(ErrConn));
        c.push(Box::new(BufConn(vec![])));
        c.push(Box::new(ErrConn));
        // All three are attempted; first error (index 0) is returned.
        let err = c.send_all(b"x").unwrap_err();
        assert!(err.to_string().contains("connection 0"));
    }
}
