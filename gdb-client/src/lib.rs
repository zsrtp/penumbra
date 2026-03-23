//! GDB Remote Serial Protocol (RSP) client library.
//!
//! This library provides a Rust interface for connecting to GDB stubs
//! over TCP or serial connections. It handles the RSP protocol including
//! packet framing, checksums, and ACK handling.
//!
//! # Architecture
//!
//! - [`Stream`]: Low-level TCP/serial connection handling
//! - [`GDB`]: High-level RSP protocol implementation
//! - [`GDBSource`]: Connection configuration (address/serial params)
//!
//! # Example
//!
//! ```ignore
//! use gdb_client::{GDB, GDBSource};
//!
//! // Connect to a GDB stub on TCP
//! let source = GDBSource::Network(([192, 168, 1, 100].into(), 2159));
//! let mut gdb = GDB::connect(&source)?;
//!
//! // Query stop reason
//! let stop = gdb.query_stop_reason()?;
//!
//! // Read registers
//! let regs = gdb.read_registers()?;
//! ```

use serialport::SerialPort;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use thiserror::Error;

/// Result of an asynchronous connection attempt.
///
/// Returned by [`GDB::connect_async`] to allow callers to poll for
/// connection completion without blocking.
pub enum GDBConnectResult {
    /// Connection completed successfully. Contains the connected GDB instance.
    Connected(GDB),
    /// Connection failed. Contains the error.
    Error(GDBError),
    /// Connection in progress. Contains a receiver to poll for completion.
    InProgress(std::sync::mpsc::Receiver<Result<GDB, GDBError>>),
}

impl GDB {
    /// Connect to a GDB stub asynchronously (for GUI event loops).
    ///
    /// For TCP connections, spawns a background thread to handle the
    /// blocking connect operation. Returns immediately with:
    /// - [`GDBConnectResult::InProgress`] - poll the receiver for completion
    /// - [`GDBConnectResult::Connected`] - serial connections complete immediately
    /// - [`GDBConnectResult::Error`] - connection failed
    ///
    /// Use this in event-driven code where blocking is not acceptable.
    pub fn connect_async(source: &GDBSource) -> GDBConnectResult {
        match source {
            GDBSource::Network((ip, port)) => {
                let addr = std::net::SocketAddr::new(*ip, *port);
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let result = std::net::TcpStream::connect_timeout(
                        &addr,
                        std::time::Duration::from_secs(5),
                    )
                    .map_err(GDBError::Disconnected)
                    .and_then(|stream| {
                        let mut gdb = GDB::new();
                        gdb.attach_stream(stream);
                        Ok(gdb)
                    });
                    let _ = tx.send(result);
                });
                GDBConnectResult::InProgress(rx)
            }
            GDBSource::Serial { .. } => match Self::connect(source) {
                Ok(gdb) => GDBConnectResult::Connected(gdb),
                Err(e) => GDBConnectResult::Error(e),
            },
        }
    }

    /// Connect to a GDB stub synchronously.
    ///
    /// This is a blocking call that will wait for the connection to
    /// complete. For TCP, this waits up to 5 seconds. For serial,
    /// this returns immediately.
    ///
    /// Note: This only establishes the connection. Use
    /// [`connect_and_init`][GDB::connect_and_init] to complete the
    /// GDB handshake (query stop reason, negotiate features).
    pub fn connect(source: &GDBSource) -> Result<Self, GDBError> {
        let mut gdb = GDB::new();
        gdb.stream = Some(Stream::connect(source)?);
        gdb.state = GDBState::Connected;
        gdb.no_ack_mode = false;
        Ok(gdb)
    }
}

/// Low-level stream connection handling (TCP or Serial).
///
/// This enum abstracts over the underlying transport layer, allowing
/// the RSP protocol code to work identically with either connection type.
///
/// - TCP: Direct socket connection with address tracking
/// - Serial: Thread-safe wrapper around serial port with mutex for concurrent access
#[derive(Debug)]
pub enum Stream {
    Tcp(TcpStream, std::net::SocketAddr),
    Serial(Arc<Mutex<Box<dyn SerialPort>>>),
}

impl Stream {
    pub(crate) fn connect(source: &GDBSource) -> Result<Self, GDBError> {
        match source {
            GDBSource::Network((ip, port)) => {
                let addr = std::net::SocketAddr::new(*ip, *port);
                let stream = TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(5))?;
                stream.set_nonblocking(false)?;
                stream.set_nodelay(true)?;
                Ok(Stream::Tcp(stream, addr))
            }
            GDBSource::Serial { path, baud_rate } => {
                let port: Box<dyn SerialPort> =
                    serialport::new(path.to_string_lossy().into_owned(), *baud_rate)
                        .open()
                        .map_err(|e| {
                            GDBError::InvalidResponse(format!("failed to open serial port: {}", e))
                        })?;
                Ok(Stream::Serial(Arc::new(Mutex::new(port))))
            }
        }
    }

    pub(crate) fn is_tcp(&self) -> bool {
        matches!(self, Stream::Tcp(_, _))
    }

    pub(crate) fn tcp_addr(&self) -> Option<std::net::SocketAddr> {
        match self {
            Stream::Tcp(_, addr) => Some(*addr),
            Stream::Serial(_) => None,
        }
    }

    pub(crate) fn tcp_shutdown(&mut self) {
        if let Stream::Tcp(tcp, _) = self {
            let _ = tcp.shutdown(std::net::Shutdown::Both);
        }
    }

    pub fn set_nonblocking(&mut self, nonblocking: bool) -> std::io::Result<()> {
        match self {
            Stream::Tcp(tcp, _) => tcp.set_nonblocking(nonblocking),
            Stream::Serial(_) => Ok(()),
        }
    }

    pub(crate) fn try_clone(&self) -> Self {
        match self {
            Stream::Tcp(tcp, addr) => Stream::Tcp(tcp.try_clone().unwrap(), *addr),
            Stream::Serial(port) => Stream::Serial(Arc::clone(port)),
        }
    }

    pub(crate) fn drain_stale_data(&mut self) -> usize {
        if let Stream::Tcp(stream, _) = self {
            let orig_timeout = stream.read_timeout().ok().flatten();
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(1)));
            let mut buf = [0u8; 256];
            let mut total = 0;
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => total += n,
                    Err(_) => break,
                }
            }
            let _ = stream.set_read_timeout(orig_timeout);
            return total;
        }
        0
    }

    pub(crate) fn read_packet(&mut self, no_ack: bool) -> Option<Vec<u8>> {
        read_packet_from_stream(self, no_ack)
    }
}

impl Clone for Stream {
    fn clone(&self) -> Self {
        match self {
            Stream::Tcp(tcp, addr) => Stream::Tcp(tcp.try_clone().unwrap(), *addr),
            Stream::Serial(port) => Stream::Serial(Arc::clone(port)),
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Stream::Tcp(s, _) => s.read(buf),
            Stream::Serial(s) => s.lock().unwrap().read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Stream::Tcp(s, _) => s.write(buf),
            Stream::Serial(s) => s.lock().unwrap().write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Stream::Tcp(s, _) => s.flush(),
            Stream::Serial(s) => s.lock().unwrap().flush(),
        }
    }
}

/// Connection destination configuration.
///
/// Specifies how to connect to a GDB stub - either over TCP/IP
/// or via a serial port.
#[derive(Debug, Clone)]
pub enum GDBSource {
    /// TCP/IP connection: (IP address, port)
    ///
    /// Standard GDB port is 2159.
    Network((std::net::IpAddr, u16)),
    /// Serial connection: (device path, baud rate)
    ///
    /// Common baud rates: 115200, 57600, 9600
    Serial {
        path: std::path::PathBuf,
        baud_rate: u32,
    },
}

impl Default for GDBSource {
    fn default() -> Self {
        GDBSource::Network(([127, 0, 0, 1].into(), 2159))
    }
}

/// GDB Remote Serial Protocol (RSP) client.
///
/// This is the main struct for interacting with a GDB stub. It provides
/// methods for all common GDB operations:
///
/// - **Connection**: [`connect`][GDB::connect], [`connect_async`][GDB::connect_async]
/// - **Execution control**: [`halt`][GDB::halt], [`resume`][GDB::resume], [`step`][GDB::step]
/// - **Register access**: [`read_registers`][GDB::read_registers], [`read_register`][GDB::read_register]
/// - **Memory access**: [`read_memory`][GDB::read_memory], [`write_memory`][GDB::write_memory]
/// - **Breakpoints**: [`set_breakpoint`][GDB::set_breakpoint], [`remove_breakpoint`][GDB::remove_breakpoint]
///
/// # Thread Safety
///
/// The GDB struct is not thread-safe by itself. To read packets while
/// the target is running (for async stop detection), use
/// [`try_clone_stream`][GDB::try_clone_stream] to create a separate
/// instance that shares the underlying connection.
///
/// # Protocol State
///
/// - `no_ack_mode`: When true, the stub supports qSupported and we skip ACK handling
/// - `pending_stop_reply`: Catches stop replies that arrive before our `c` command ACK
/// - `target_running`: Atomic flag to prevent concurrent reads from stream
#[derive(Debug, Default, Clone)]
pub struct GDB {
    /// Current connection state.
    pub state: GDBState,
    /// Underlying stream connection (None when disconnected).
    stream: Option<Stream>,
    /// Whether no-ack mode has been negotiated with the stub.
    no_ack_mode: bool,
    /// Stop-reply consumed by send_packet's ACK loop (happens when target
    /// hits a breakpoint before the stub ACKs our `c` command).
    pending_stop_reply: Option<Vec<u8>>,
    /// Set to true when the target is running (after `c`/`s`) and a monitor
    /// thread is reading from the cloned socket.  Prevents concurrent reads.
    target_running: Arc<AtomicBool>,
}

/// Errors that can occur during GDB RSP operations.
#[derive(Debug, Error)]
pub enum GDBError {
    /// Connection was lost (IO error).
    #[error("GDB server disconnected")]
    Disconnected(#[from] std::io::Error),
    /// Response from stub was malformed or unexpected.
    #[error("Invalid response: {0}")]
    InvalidResponse(String),
    /// Operation requires an active connection.
    #[error("Not connected")]
    NotConnected,
    /// No connection destination specified.
    #[error("No source configured")]
    NoSource,
    /// Stub sent NACK instead of ACK (protocol error).
    #[error("Negative ACK from server")]
    NegativeAck,
}

/// Current connection state.
#[derive(Debug, Default, PartialEq, Eq, Clone)]
pub enum GDBState {
    /// No active connection.
    #[default]
    Disconnected,
    Connected,
}

/// Commands that can be sent to the GDB client (used for threaded GUI).
#[derive(Debug)]
pub enum GDBCmd {
    /// Connect to a GDB stub.
    Connect(GDBSource),
    /// Connect + query_stop_reason + negotiate (full initialization).
    ConnectAndInit(GDBSource),
    /// Send interrupt (Ctrl+C) to halt the target.
    Halt,
    /// Resume target execution.
    Continue,
    /// Disconnect from the stub.
    Disconnect,
}

/// Responses from the GDB client (used for threaded GUI).
#[derive(Debug)]
pub enum GDBResponse {
    /// Connection successful.
    Connected,
    /// Disconnected from stub.
    Disconnected,
    /// Target has halted (due to breakpoint, interrupt, etc.).
    Halted,
    /// Target has resumed execution.
    Continued,
    /// An error occurred.
    Error(String),
}

/// Stop reply from GDB stub (T or S packet).
///
/// Sent when the target stops execution - due to breakpoint,
/// interrupt, exception, or reaching a watchpoint.
#[derive(Debug, Clone)]
pub struct StopReply {
    /// The signal number that caused the stop (e.g., 5 = SIGTRAP, 2 = SIGINT).
    pub signal: u8,
}

/// PowerPC register state.
///
/// Contains the full register set as returned by the GDB `g` packet.
/// For PowerPC, this includes:
/// - GPRs: General purpose registers (r0-r31)
/// - FPRs: Floating point registers (fp0-fp31)
/// - Special: PC, MSR, CR, LR, CTR, XER, FPSCR
#[derive(Debug, Clone, Default)]
pub struct PpcRegisters {
    /// General purpose registers (r0 through r31).
    pub gpr: [u32; 32],
    /// Floating point registers (fp0 through fp31).
    pub fpr: [u64; 32],
    /// Program Counter (Instruction Address).
    pub pc: u32,
    /// Machine State Register.
    pub msr: u32,
    /// Condition Register.
    pub cr: u32,
    /// Link Register.
    pub lr: u32,
    /// Count Register (used for loop optimization).
    pub ctr: u32,
    /// Integer Exception Register.
    pub xer: u32,
    /// Floating Point Status and Control Register.
    pub fpscr: u64,
    /// Size of the `g` response blob in bytes (for diagnostics).
    pub reg_blob_size: usize,
}

impl GDB {
    pub fn new() -> Self {
        Self {
            state: GDBState::default(),
            stream: None,
            no_ack_mode: false,
            pending_stop_reply: None,
            target_running: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Execute a command from the GDBCmd enum.
    ///
    /// This is the main entry point for threaded GUI applications.
    /// Each command corresponds to a GDB operation.
    pub fn execute_cmd(&mut self, cmd: GDBCmd) -> Result<GDBResponse, GDBError> {
        match cmd {
            GDBCmd::Connect(source) => {
                self.stream = Some(Stream::connect(&source)?);
                self.state = GDBState::Connected;
                self.no_ack_mode = false;
                Ok(GDBResponse::Connected)
            }
            GDBCmd::ConnectAndInit(source) => {
                self.execute_cmd(GDBCmd::Connect(source))?;
                self.connect_and_init()?;
                Ok(GDBResponse::Connected)
            }
            GDBCmd::Halt => {
                self.halt()?;
                Ok(GDBResponse::Halted)
            }
            GDBCmd::Continue => {
                self.resume()?;
                Ok(GDBResponse::Continued)
            }
            GDBCmd::Disconnect => {
                self.target_running.store(false, Ordering::SeqCst);
                if let Some(mut stream) = self.stream.take() {
                    stream.tcp_shutdown();
                }
                self.state = GDBState::Disconnected;
                self.no_ack_mode = false;
                Ok(GDBResponse::Disconnected)
            }
        }
    }

    pub fn get_stream(&mut self) -> Result<&mut Stream, GDBError> {
        if let Some(ref mut stream) = self.stream {
            return Ok(stream);
        }
        Err(GDBError::NotConnected)
    }

    /// Send Ctrl-C (0x03) interrupt byte without reading a response.
    pub fn send_interrupt(&mut self) -> Result<(), GDBError> {
        let stream = self.get_stream()?;
        stream.write_all(&[0x03])?;
        stream.flush()?;
        Ok(())
    }

    /// Send Ctrl-C (0x03) interrupt and read the stop-reply.
    pub fn halt(&mut self) -> Result<StopReply, GDBError> {
        self.send_interrupt()?;
        // Read the stop-reply. Some stubs send an extra packet (empty or
        // duplicate); if the first packet isn't a valid stop-reply, try
        // reading one more. Any leftover data is consumed by the next
        // send_packet() call.
        let first = self.read_packet_from_stream()?;
        match parse_stop_reply(&first) {
            Ok(sr) => Ok(sr),
            Err(_) => {
                let second = self.read_packet_from_stream()?;
                parse_stop_reply(&second)
            }
        }
    }

    /// Send `c` (continue) without waiting for a stop reply.
    pub fn resume(&mut self) -> Result<(), GDBError> {
        self.send_packet(b"c")?;
        Ok(())
    }

    /// Send `c` (continue) and block until the target stops.
    pub fn continue_and_wait(&mut self) -> Result<StopReply, GDBError> {
        self.send_packet(b"c")?;
        // The stop reply may have been consumed during ACK handling
        if let Some(reply) = self.pending_stop_reply.take() {
            return parse_stop_reply(&reply);
        }
        let reply = self.read_packet_from_stream()?;
        parse_stop_reply(&reply)
    }

    /// Send `?` to query the current stop reason.
    pub fn query_stop_reason(&mut self) -> Result<StopReply, GDBError> {
        let reply = self.send_and_recv(b"?")?;
        parse_stop_reply(&reply)
    }

    /// Read all registers. Uses `g` for GPRs, then `p` for special registers
    /// since some stubs only return GPRs from `g`.
    pub fn read_registers(&mut self) -> Result<PpcRegisters, GDBError> {
        let reply = self.send_and_recv(b"g")?;
        let hex = std::str::from_utf8(&reply)
            .map_err(|_| GDBError::InvalidResponse("non-utf8 register data".into()))?;
        let bytes = decode_hex(hex)?;

        let mut regs = PpcRegisters::default();

        regs.reg_blob_size = bytes.len();

        // Parse GPRs (always present: 32 × 4 bytes = 128 bytes minimum)
        if bytes.len() < 128 {
            return Err(GDBError::InvalidResponse(format!(
                "register blob too short for GPRs: {} bytes (need 128)",
                bytes.len()
            )));
        }
        for i in 0..32 {
            let o = i * 4;
            regs.gpr[i] = u32::from_be_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);
        }

        if bytes.len() >= 416 {
            // Full register set: GPRs + FPRs + specials
            let mut offset = 128;
            for i in 0..32 {
                regs.fpr[i] = u64::from_be_bytes([
                    bytes[offset],
                    bytes[offset + 1],
                    bytes[offset + 2],
                    bytes[offset + 3],
                    bytes[offset + 4],
                    bytes[offset + 5],
                    bytes[offset + 6],
                    bytes[offset + 7],
                ]);
                offset += 8;
            }
            let read_u32 = |o: usize| -> u32 {
                u32::from_be_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]])
            };
            regs.pc = read_u32(offset);
            offset += 4;
            regs.msr = read_u32(offset);
            offset += 4;
            regs.cr = read_u32(offset);
            offset += 4;
            regs.lr = read_u32(offset);
            offset += 4;
            regs.ctr = read_u32(offset);
            offset += 4;
            regs.xer = read_u32(offset);
            offset += 4;
            regs.fpscr = u64::from_be_bytes([
                bytes[offset],
                bytes[offset + 1],
                bytes[offset + 2],
                bytes[offset + 3],
                bytes[offset + 4],
                bytes[offset + 5],
                bytes[offset + 6],
                bytes[offset + 7],
            ]);
        } else {
            // Stub only returned GPRs — read special registers individually.
            // PPC register numbers: PC=64, MSR=65, CR=66, LR=67, CTR=68, XER=69
            regs.pc = self.read_register(64)? as u32;
            regs.msr = self.read_register(65)? as u32;
            regs.cr = self.read_register(66)? as u32;
            regs.lr = self.read_register(67)? as u32;
            regs.ctr = self.read_register(68)? as u32;
            regs.xer = self.read_register(69)? as u32;
        }

        Ok(regs)
    }

    /// Send `p<num>` to read a single register.
    /// Returns the value (u64 to accommodate 64-bit FPRs).
    pub fn read_register(&mut self, num: u8) -> Result<u64, GDBError> {
        let cmd = format!("p{:x}", num);
        let reply = self.send_and_recv(cmd.as_bytes())?;
        let hex = std::str::from_utf8(&reply)
            .map_err(|_| GDBError::InvalidResponse("non-utf8 register data".into()))?;
        u64::from_str_radix(hex, 16)
            .map_err(|_| GDBError::InvalidResponse(format!("bad register hex: {}", hex)))
    }

    /// Send `P<num>=<value>` to write a single register.
    pub fn write_register(&mut self, num: u8, value: u64) -> Result<(), GDBError> {
        // GPRs (0-31) and special regs (64-69) are 32-bit, FPRs (32-63) and FPSCR (70) are 64-bit
        let cmd = if (32..=63).contains(&num) || num == 70 {
            format!("P{:x}={:016x}", num, value)
        } else {
            format!("P{:x}={:08x}", num, value as u32)
        };
        let reply = self.send_and_recv(cmd.as_bytes())?;
        if reply == b"OK" {
            Ok(())
        } else {
            Err(GDBError::InvalidResponse(
                String::from_utf8_lossy(&reply).into_owned(),
            ))
        }
    }

    /// Send `m<addr>,<len>` to read target memory. Returns raw bytes.
    pub fn read_memory(&mut self, addr: u32, len: usize) -> Result<Vec<u8>, GDBError> {
        let cmd = format!("m{:x},{:x}", addr, len);
        let reply = self.send_and_recv(cmd.as_bytes())?;
        let hex = std::str::from_utf8(&reply)
            .map_err(|_| GDBError::InvalidResponse("non-utf8 memory data".into()))?;
        if hex.starts_with('E') {
            return Err(GDBError::InvalidResponse(format!(
                "memory read error: {}",
                hex
            )));
        }
        decode_hex(hex)
    }

    /// Send `M<addr>,<len>:<hex>` to write target memory.
    pub fn write_memory(&mut self, addr: u32, data: &[u8]) -> Result<(), GDBError> {
        let hex: String = data.iter().map(|b| format!("{:02x}", b)).collect();
        let cmd = format!("M{:x},{:x}:{}", addr, data.len(), hex);
        let reply = self.send_and_recv(cmd.as_bytes())?;
        if reply == b"OK" {
            Ok(())
        } else {
            Err(GDBError::InvalidResponse(
                String::from_utf8_lossy(&reply).into_owned(),
            ))
        }
    }

    /// Send `s` (single step) and wait for the stop reply.
    pub fn step(&mut self) -> Result<StopReply, GDBError> {
        let reply = self.send_and_recv(b"s")?;
        parse_stop_reply(&reply)
    }

    /// Set a software breakpoint via Z0.
    pub fn set_breakpoint(&mut self, addr: u32) -> Result<(), GDBError> {
        let cmd = format!("Z0,{:x},4", addr);
        let reply = self.send_and_recv(cmd.as_bytes())?;
        if reply == b"OK" {
            Ok(())
        } else {
            Err(GDBError::InvalidResponse(
                String::from_utf8_lossy(&reply).into_owned(),
            ))
        }
    }

    /// Remove a software breakpoint via z0.
    pub fn remove_breakpoint(&mut self, addr: u32) -> Result<(), GDBError> {
        let cmd = format!("z0,{:x},4", addr);
        let reply = self.send_and_recv(cmd.as_bytes())?;
        if reply == b"OK" {
            Ok(())
        } else {
            Err(GDBError::InvalidResponse(
                String::from_utf8_lossy(&reply).into_owned(),
            ))
        }
    }

    /// Send `D` to detach from the target.
    pub fn detach(&mut self) -> Result<(), GDBError> {
        let reply = self.send_and_recv(b"D")?;
        if reply == b"OK" {
            self.stream = None;
            self.state = GDBState::Disconnected;
            self.no_ack_mode = false;
            Ok(())
        } else {
            Err(GDBError::InvalidResponse(
                String::from_utf8_lossy(&reply).into_owned(),
            ))
        }
    }

    /// Negotiate features with the stub: qSupported + QStartNoAckMode.
    pub fn negotiate(&mut self) -> Result<(), GDBError> {
        let reply = self.send_and_recv(b"qSupported:QStartNoAckMode+")?;
        let features = std::str::from_utf8(&reply)
            .map_err(|_| GDBError::InvalidResponse("non-utf8 qSupported".into()))?;

        if features.contains("QStartNoAckMode+") {
            let ack_reply = self.send_and_recv(b"QStartNoAckMode")?;
            if ack_reply == b"OK" {
                self.no_ack_mode = true;
            }
        }
        Ok(())
    }

    /// Attach an already-connected TCP stream (used when the connect was
    /// performed in a background thread).
    fn attach_stream(&mut self, stream: std::net::TcpStream) {
        let _ = stream.set_nonblocking(false);
        let addr = stream
            .peer_addr()
            .unwrap_or_else(|_| "0.0.0.0:0".parse().unwrap());
        self.stream = Some(Stream::Tcp(stream, addr));
        self.state = GDBState::Connected;
        self.no_ack_mode = false;
    }

    /// Return the target address if connected via TCP.
    pub fn target_addr(&self) -> Option<std::net::SocketAddr> {
        self.stream.as_ref().and_then(|s| s.tcp_addr())
    }

    /// Check if the current connection is serial.
    pub fn is_serial(&self) -> bool {
        self.stream.as_ref().map_or(false, |s| !s.is_tcp())
    }

    /// Full initialization: query_stop_reason + negotiate.
    /// Call after connecting to perform the handshake that Nintendont requires
    /// (the `?` packet installs the PPC exception handler).
    pub fn connect_and_init(&mut self) -> Result<StopReply, GDBError> {
        let stop = self.query_stop_reason()?;
        self.negotiate()?;
        Ok(stop)
    }

    /// Block reading until a stop-reply packet arrives. Used when the target
    /// is running after `c` to detect breakpoint hits, exceptions, etc.
    pub fn wait_stop(&mut self) -> Result<StopReply, GDBError> {
        let reply = self.read_packet_from_stream()?;
        parse_stop_reply(&reply)
    }

    /// Try to clone the underlying stream for the RSP monitor thread.
    /// Returns a new GDB with a cloned stream.
    pub fn try_clone_stream(&self) -> Result<GDB, GDBError> {
        let stream = self.stream.as_ref().ok_or(GDBError::NotConnected)?;
        Ok(GDB {
            state: self.state.clone(),
            stream: Some(stream.try_clone()),
            no_ack_mode: self.no_ack_mode,
            pending_stop_reply: None,
            target_running: self.target_running.clone(),
        })
    }

    /// Read a packet from this GDB's stream (for monitor thread).
    /// Returns the packet payload if successful.
    pub fn read_packet(&mut self) -> Option<Vec<u8>> {
        let stream = self.stream.as_mut()?;
        stream.read_packet(self.no_ack_mode)
    }

    /// Read a packet from this GDB's stream.
    fn read_packet_from_stream(&mut self) -> Result<Vec<u8>, GDBError> {
        let no_ack = self.no_ack_mode;
        let stream = self.get_stream()?;
        read_packet_from_stream(&mut *stream, no_ack)
            .ok_or_else(|| GDBError::InvalidResponse("failed to read RSP packet".into()))
    }

    /// Whether no-ack mode was successfully negotiated.
    pub fn is_no_ack_mode(&self) -> bool {
        self.no_ack_mode
    }

    /// Take a pending stop-reply that was consumed by send_packet's ACK loop.
    pub fn take_pending_stop_reply(&mut self) -> Option<Vec<u8>> {
        self.pending_stop_reply.take()
    }

    /// Get a reference to the target_running flag (for sharing with the monitor).
    pub fn target_running_flag(&self) -> Arc<AtomicBool> {
        self.target_running.clone()
    }

    /// Set the target_running flag.
    pub fn set_target_running(&self, running: bool) {
        self.target_running.store(running, Ordering::SeqCst);
    }

    /// Drain any stale data from the TCP socket.
    pub fn drain_stale_data(&mut self) -> usize {
        if let Some(ref mut stream) = self.stream {
            return stream.drain_stale_data();
        }
        0
    }

    /// Send a packet and read the response in one call.
    /// Refuses to operate if the target is running (monitor thread is reading).
    pub fn send_and_recv(&mut self, data: &[u8]) -> Result<Vec<u8>, GDBError> {
        if self.target_running.load(Ordering::SeqCst) {
            return Err(GDBError::InvalidResponse(
                "cannot send command while target is running".into(),
            ));
        }
        self.send_packet(data)?;
        self.read_packet_from_stream()
    }

    fn send_packet(&mut self, data: &[u8]) -> Result<(), GDBError> {
        let no_ack = self.no_ack_mode;
        let pending_payload;
        {
            let stream = self.get_stream()?;

            let checksum: u8 = data.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
            let mut packet = Vec::with_capacity(data.len() + 4);
            packet.push(b'$');
            packet.extend_from_slice(data);
            packet.push(b'#');
            packet.push(hex_digit(checksum >> 4));
            packet.push(hex_digit(checksum & 0x0f));

            stream.write_all(&packet)?;
            stream.flush()?;

            // In no-ack mode, don't wait for ACK
            if no_ack {
                return Ok(());
            }

            // Read ACK, consuming any unsolicited server packets (stop-replies)
            // that may be buffered ahead of it.
            let mut ack = [0u8; 1];
            pending_payload = loop {
                stream.read_exact(&mut ack)?;
                match ack[0] {
                    b'+' => break None,
                    b'-' => return Err(GDBError::NegativeAck),
                    b'$' => {
                        // Unsolicited packet — consume it ($...#xx) and ACK
                        let mut payload = Vec::new();
                        loop {
                            stream.read_exact(&mut ack)?;
                            if ack[0] == b'#' {
                                break;
                            }
                            payload.push(ack[0]);
                        }
                        let mut _chk = [0u8; 2];
                        stream.read_exact(&mut _chk)?;
                        stream.write_all(b"+")?;
                        stream.flush()?;
                        // Save stop-replies so the monitor thread can find them
                        if payload.first() == Some(&b'T') || payload.first() == Some(&b'S') {
                            break Some(payload);
                        }
                    }
                    _ => {} // skip other junk bytes
                }
            };
        }
        if let Some(payload) = pending_payload {
            self.pending_stop_reply = Some(payload);
        }
        Ok(())
    }
}

/// Read a single RSP packet from a stream.
///
/// The GDB Remote Serial Protocol uses a simple framing scheme:
/// 1. Find the start marker: `$`
/// 2. Read until end marker: `#`
/// 3. Read 2-digit hexadecimal checksum
/// 4. Send `+` ACK (unless `no_ack` is true)
///
/// This function handles steps 1-4 and returns the payload (without
/// the `$` and `#` markers and checksum).
///
/// Returns `None` on connection error or EOF.
pub fn read_packet_from_stream<R: Read + Write + ?Sized>(
    reader: &mut R,
    no_ack: bool,
) -> Option<Vec<u8>> {
    let mut byte = [0u8; 1];

    // Skip until '$', counting skipped bytes for diagnostics
    let mut skipped = 0u32;
    loop {
        if reader.read_exact(&mut byte).is_err() {
            return None;
        }
        if byte[0] == b'$' {
            break;
        }
        skipped += 1;
    }
    if skipped > 0 {
        eprintln!("RSP: skipped {} bytes before '$'", skipped);
    }

    // Read payload until '#', computing checksum
    let mut payload = Vec::new();
    let mut computed_csum: u8 = 0;
    loop {
        if reader.read_exact(&mut byte).is_err() {
            return None;
        }
        if byte[0] == b'#' {
            break;
        }
        payload.push(byte[0]);
        computed_csum = computed_csum.wrapping_add(byte[0]);
    }

    // Read and validate 2-byte checksum
    let mut csum_bytes = [0u8; 2];
    if reader.read_exact(&mut csum_bytes).is_err() {
        return None;
    }
    let received_csum = from_hex_digit(csum_bytes[0]) << 4 | from_hex_digit(csum_bytes[1]);
    if received_csum != computed_csum {
        eprintln!(
            "RSP checksum mismatch: received {:02x}, computed {:02x}, payload len={}",
            received_csum,
            computed_csum,
            payload.len()
        );
    }

    // Send ACK only if not in no-ack mode
    if !no_ack {
        let _ = reader.write_all(b"+");
        let _ = reader.flush();
    }

    Some(payload)
}

fn hex_digit(nibble: u8) -> u8 {
    match nibble {
        0..=9 => b'0' + nibble,
        10..=15 => b'a' + nibble - 10,
        _ => b'?',
    }
}

pub fn from_hex_digit(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => 0,
    }
}

fn decode_hex(hex: &str) -> Result<Vec<u8>, GDBError> {
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    let mut chars = hex.chars();
    while let (Some(hi), Some(lo)) = (chars.next(), chars.next()) {
        let byte = u8::from_str_radix(&format!("{}{}", hi, lo), 16)
            .map_err(|_| GDBError::InvalidResponse(format!("bad hex: {}{}", hi, lo)))?;
        bytes.push(byte);
    }
    Ok(bytes)
}

pub fn parse_stop_reply(data: &[u8]) -> Result<StopReply, GDBError> {
    let s = std::str::from_utf8(data)
        .map_err(|_| GDBError::InvalidResponse("non-utf8 stop reply".into()))?;

    // Stop replies: T<signal><...> or S<signal>
    if let Some(rest) = s.strip_prefix('T') {
        let sig_hex = &rest[..2.min(rest.len())];
        let signal = u8::from_str_radix(sig_hex, 16)
            .map_err(|_| GDBError::InvalidResponse(format!("bad signal: {}", sig_hex)))?;
        Ok(StopReply { signal })
    } else if let Some(rest) = s.strip_prefix('S') {
        let sig_hex = &rest[..2.min(rest.len())];
        let signal = u8::from_str_radix(sig_hex, 16)
            .map_err(|_| GDBError::InvalidResponse(format!("bad signal: {}", sig_hex)))?;
        Ok(StopReply { signal })
    } else {
        Err(GDBError::InvalidResponse(format!(
            "unexpected stop reply: {}",
            s
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_digit_roundtrip() {
        for i in 0..16u8 {
            let ch = hex_digit(i);
            assert_eq!(from_hex_digit(ch), i, "roundtrip failed for {}", i);
        }
    }

    #[test]
    fn from_hex_digit_uppercase() {
        assert_eq!(from_hex_digit(b'A'), 10);
        assert_eq!(from_hex_digit(b'F'), 15);
    }

    #[test]
    fn decode_hex_basic() {
        assert_eq!(
            decode_hex("deadbeef").unwrap(),
            vec![0xde, 0xad, 0xbe, 0xef]
        );
        assert_eq!(decode_hex("00ff").unwrap(), vec![0x00, 0xff]);
        assert_eq!(decode_hex("").unwrap(), vec![]);
    }

    #[test]
    fn decode_hex_invalid() {
        assert!(decode_hex("zz").is_err());
        assert!(decode_hex("0g").is_err());
    }

    #[test]
    fn parse_stop_reply_t_packet() {
        let reply = parse_stop_reply(b"T05thread:1;").unwrap();
        assert_eq!(reply.signal, 5);

        let reply = parse_stop_reply(b"T02thread:1;").unwrap();
        assert_eq!(reply.signal, 2);
    }

    #[test]
    fn parse_stop_reply_s_packet() {
        let reply = parse_stop_reply(b"S05").unwrap();
        assert_eq!(reply.signal, 5);
    }

    #[test]
    fn parse_stop_reply_invalid() {
        assert!(parse_stop_reply(b"OK").is_err());
        assert!(parse_stop_reply(b"E14").is_err());
    }
}
