use std::io::{Read, Write};
use std::net::TcpStream;
use thiserror::Error;

#[derive(Debug)]
pub enum GDBSource {
    Network((std::net::IpAddr, u16)),
    Serial(std::path::PathBuf),
}

#[derive(Debug, Default)]
pub struct GDB {
    pub source: Option<GDBSource>,
    pub state: GDBState,
    stream: Option<TcpStream>,
}

#[derive(Debug, Error)]
pub enum GDBError {
    #[error("GDB server disconnected")]
    Disconnected(#[from] std::io::Error),
    #[error("Invalid response: {0}")]
    InvalidResponse(String),
    #[error("Not connected")]
    NotConnected,
    #[error("No source configured")]
    NoSource,
    #[error("Negative ACK from server")]
    NegativeAck,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub enum GDBState {
    #[default]
    Disconnected,
    Connected,
}

#[derive(Debug)]
pub enum GDBCmd {
    Connect,
    Halt,
    Continue,
    Disconnect,
    SetSource(GDBSource),
}

#[derive(Debug)]
pub enum GDBResponse {
    Connected,
    Disconnected,
    Halted,
    Continued,
    Error(String),
}

impl GDB {
    pub fn new(source: GDBSource) -> Self {
        Self {
            source: Some(source),
            state: GDBState::default(),
            stream: None,
        }
    }

    pub fn execute_cmd(&mut self, cmd: GDBCmd) -> Result<GDBResponse, GDBError> {
        match cmd {
            GDBCmd::Connect => {
                let source = self.source.as_ref().ok_or(GDBError::NoSource)?;
                match source {
                    GDBSource::Network((ip, port)) => {
                        let addr = format!("{}:{}", ip, port);
                        let stream = TcpStream::connect(&addr)?;
                        stream.set_nonblocking(false)?;
                        self.stream = Some(stream);
                        self.state = GDBState::Connected;
                        Ok(GDBResponse::Connected)
                    }
                    GDBSource::Serial(_) => {
                        Err(GDBError::InvalidResponse("Serial not yet supported".into()))
                    }
                }
            }
            GDBCmd::Halt => {
                let stream = self.stream.as_mut().ok_or(GDBError::NotConnected)?;
                stream.write_all(&[0x03])?;
                stream.flush()?;
                // The server sends two SIGTRAP stop-reply packets after 0x03:
                // one from the UpdateCallback handler, and one from the CPU
                // stepping handler in CPU.cpp. Drain both.
                self.read_packet()?;
                self.read_packet()?;
                Ok(GDBResponse::Halted)
            }
            GDBCmd::Continue => {
                self.send_packet(b"c")?;
                Ok(GDBResponse::Continued)
            }
            GDBCmd::Disconnect => {
                self.stream = None;
                self.state = GDBState::Disconnected;
                Ok(GDBResponse::Disconnected)
            }
            GDBCmd::SetSource(gdbsource) => {
                self.source = Some(gdbsource);
                Ok(GDBResponse::Disconnected)
            }
        }
    }

    /// Read a single RSP packet (`$...#xx`) from the stream, send `+` ACK, return the payload.
    fn read_packet(&mut self) -> Result<Vec<u8>, GDBError> {
        let stream = self.stream.as_mut().ok_or(GDBError::NotConnected)?;
        let mut byte = [0u8; 1];

        // Skip any leading ACK characters or junk until we see '$'
        loop {
            stream.read_exact(&mut byte)?;
            if byte[0] == b'$' {
                break;
            }
        }

        // Read payload until '#'
        let mut payload = Vec::new();
        loop {
            stream.read_exact(&mut byte)?;
            if byte[0] == b'#' {
                break;
            }
            payload.push(byte[0]);
        }

        // Read 2-byte checksum (we don't validate it for now)
        let mut _checksum = [0u8; 2];
        stream.read_exact(&mut _checksum)?;

        // Send ACK
        stream.write_all(b"+")?;
        stream.flush()?;

        Ok(payload)
    }

    fn send_packet(&mut self, data: &[u8]) -> Result<(), GDBError> {
        let stream = self.stream.as_mut().ok_or(GDBError::NotConnected)?;

        let checksum: u8 = data.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        let mut packet = Vec::with_capacity(data.len() + 4);
        packet.push(b'$');
        packet.extend_from_slice(data);
        packet.push(b'#');
        packet.push(hex_digit(checksum >> 4));
        packet.push(hex_digit(checksum & 0x0f));

        stream.write_all(&packet)?;
        stream.flush()?;

        // Read ACK, consuming any unsolicited server packets (stop-replies)
        // that may be buffered ahead of it.
        let mut ack = [0u8; 1];
        loop {
            stream.read_exact(&mut ack)?;
            match ack[0] {
                b'+' => return Ok(()),
                b'-' => return Err(GDBError::NegativeAck),
                b'$' => {
                    // Unsolicited packet — consume it ($...#xx) and ACK
                    loop {
                        stream.read_exact(&mut ack)?;
                        if ack[0] == b'#' {
                            break;
                        }
                    }
                    let mut _chk = [0u8; 2];
                    stream.read_exact(&mut _chk)?;
                    stream.write_all(b"+")?;
                    stream.flush()?;
                }
                _ => {} // skip other junk bytes
            }
        }
    }
}

fn hex_digit(nibble: u8) -> u8 {
    match nibble {
        0..=9 => b'0' + nibble,
        10..=15 => b'a' + nibble - 10,
        _ => b'?',
    }
}
