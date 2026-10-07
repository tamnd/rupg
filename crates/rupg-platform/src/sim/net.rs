//! A network in memory.
//!
//! A connection is two channels of byte chunks. Each listener has an address of the form `host:port`, and port 0 gets a free port from 40000. The fault injection of spec/21 section 21.10 (delay, drop, duplicate, partition) comes with the cluster at M10.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use rupg_common::{Error, Result, SqlState};

use super::lock;
use crate::net::{Listener, Net, Stream};

#[derive(Debug, Default)]
struct State {
    listeners: HashMap<String, Sender<SimStream>>,
    next_port: u16,
    next_client: u64,
}

/// The network in memory.
#[derive(Clone, Debug, Default)]
pub struct SimNet {
    state: Arc<Mutex<State>>,
}

impl SimNet {
    /// A network with no listener.
    pub fn new() -> SimNet {
        SimNet::default()
    }
}

impl Net for SimNet {
    fn listen(&self, addr: &str) -> Result<Box<dyn Listener>> {
        let bad =
            || Error::new(SqlState::IO_ERROR, format!("could not bind to the address \"{addr}\""));
        let (host, port) = addr.rsplit_once(':').ok_or_else(bad)?;
        let port: u16 = port.parse().map_err(|_| bad())?;
        let mut state = lock(&self.state);
        let addr = if port == 0 {
            loop {
                let p = 40000 + state.next_port;
                state.next_port = state.next_port.wrapping_add(1) % 20000;
                let a = format!("{host}:{p}");
                if !state.listeners.contains_key(&a) {
                    break a;
                }
            }
        } else {
            addr.to_string()
        };
        if state.listeners.contains_key(&addr) {
            return Err(Error::new(
                SqlState::IO_ERROR,
                format!("could not bind to the address \"{addr}\": the address is in use"),
            ));
        }
        let (tx, rx) = channel();
        state.listeners.insert(addr.clone(), tx);
        Ok(Box::new(SimListener { net: self.clone(), addr, rx: Mutex::new(rx) }))
    }

    fn connect(&self, addr: &str) -> Result<Box<dyn Stream>> {
        let mut state = lock(&self.state);
        let refused = || {
            Error::new(
                SqlState::UNABLE_TO_CONNECT,
                format!("could not connect to the address \"{addr}\": connection refused"),
            )
        };
        let listener = state.listeners.get(addr).ok_or_else(refused)?.clone();
        state.next_client += 1;
        let client = format!("sim-client:{}", state.next_client);
        let (to_server, from_client) = channel();
        let (to_client, from_server) = channel();
        let server_end = SimStream::new(to_client, from_client, client);
        listener.send(server_end).map_err(|_| refused())?;
        Ok(Box::new(SimStream::new(to_server, from_server, addr.to_string())))
    }
}

#[derive(Debug)]
struct SimListener {
    net: SimNet,
    addr: String,
    rx: Mutex<Receiver<SimStream>>,
}

impl Listener for SimListener {
    fn accept(&self) -> Result<Box<dyn Stream>> {
        match lock(&self.rx).recv() {
            Ok(stream) => Ok(Box::new(stream)),
            Err(_) => Err(Error::new(SqlState::CONNECTION_FAILURE, "the listener is closed")),
        }
    }

    fn local_addr(&self) -> String {
        self.addr.clone()
    }
}

impl Drop for SimListener {
    fn drop(&mut self) {
        lock(&self.net.state).listeners.remove(&self.addr);
    }
}

#[derive(Debug)]
struct SimStream {
    tx: Mutex<Option<Sender<Vec<u8>>>>,
    rx: Receiver<Vec<u8>>,
    buf: Vec<u8>,
    pos: usize,
    closed: AtomicBool,
    peer: String,
}

impl SimStream {
    fn new(tx: Sender<Vec<u8>>, rx: Receiver<Vec<u8>>, peer: String) -> SimStream {
        SimStream {
            tx: Mutex::new(Some(tx)),
            rx,
            buf: Vec::new(),
            pos: 0,
            closed: AtomicBool::new(false),
            peer,
        }
    }
}

impl Read for SimStream {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.closed.load(Ordering::SeqCst) || out.is_empty() {
            return Ok(0);
        }
        if self.pos == self.buf.len() {
            match self.rx.recv() {
                Ok(chunk) => {
                    self.buf = chunk;
                    self.pos = 0;
                }
                // The other side closed.
                Err(_) => return Ok(0),
            }
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl Write for SimStream {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        match &*lock(&self.tx) {
            Some(tx) if tx.send(data.to_vec()).is_ok() => Ok(data.len()),
            _ => Err(io::ErrorKind::BrokenPipe.into()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Stream for SimStream {
    fn shutdown(&self) -> Result<()> {
        self.closed.store(true, Ordering::SeqCst);
        lock(&self.tx).take();
        Ok(())
    }

    fn peer_addr(&self) -> String {
        self.peer.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_connection_carries_bytes_both_ways() {
        let net = SimNet::new();
        let listener = net.listen("db:0").unwrap();
        let addr = listener.local_addr();
        assert_eq!(addr, "db:40000");
        assert!(net.listen(&addr).is_err());
        let mut client = net.connect(&addr).unwrap();
        let mut server = listener.accept().unwrap();
        assert_eq!(server.peer_addr(), "sim-client:1");
        assert_eq!(client.peer_addr(), "db:40000");

        client.write_all(b"hello").unwrap();
        client.write_all(b" world").unwrap();
        let mut buf = [0; 11];
        server.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"hello world");
        server.write_all(b"ok").unwrap();
        let mut buf = [0; 2];
        client.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"ok");

        client.shutdown().unwrap();
        assert_eq!(server.read(&mut buf).unwrap(), 0);
        assert!(client.write_all(b"x").is_err());
    }

    #[test]
    fn no_listener_refuses() {
        let net = SimNet::new();
        assert_eq!(net.connect("db:5432").unwrap_err().state(), SqlState::UNABLE_TO_CONNECT);
        let listener = net.listen("db:5432").unwrap();
        drop(listener);
        assert!(net.connect("db:5432").is_err());
        assert!(net.listen("no-port").is_err());
    }
}
