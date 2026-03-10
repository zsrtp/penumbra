use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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
    no_ack_mode: bool,
    /// Stop-reply consumed by send_packet's ACK loop (happens when target
    /// hits a breakpoint before the stub ACKs our `c` command).
    pending_stop_reply: Option<Vec<u8>>,
    /// Set to true when the target is running (after `c`/`s`) and a monitor
    /// thread is reading from the cloned socket.  Prevents concurrent reads.
    target_running: Arc<AtomicBool>,
    /// Original instructions saved for manual software breakpoints (Windows).
    #[cfg(windows)]
    saved_insns: std::collections::HashMap<u32, [u8; 4]>,
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

#[derive(Debug, Clone)]
pub struct StopReply {
    pub signal: u8,
}

#[derive(Debug, Clone, Default)]
pub struct PpcRegisters {
    pub gpr: [u32; 32],
    pub fpr: [u64; 32],
    pub pc: u32,
    pub msr: u32,
    pub cr: u32,
    pub lr: u32,
    pub ctr: u32,
    pub xer: u32,
    pub fpscr: u64,
    /// Size of the `g` response blob in bytes (for diagnostics).
    pub reg_blob_size: usize,
}

impl GDB {
    pub fn new(source: GDBSource) -> Self {
        Self {
            source: Some(source),
            state: GDBState::default(),
            stream: None,
            no_ack_mode: false,
            pending_stop_reply: None,
            target_running: Arc::new(AtomicBool::new(false)),
            #[cfg(windows)]
            saved_insns: std::collections::HashMap::new(),
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
                        self.no_ack_mode = false;
                        Ok(GDBResponse::Connected)
                    }
                    GDBSource::Serial(_) => {
                        Err(GDBError::InvalidResponse("Serial not yet supported".into()))
                    }
                }
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
                self.stream = None;
                self.state = GDBState::Disconnected;
                self.no_ack_mode = false;
                Ok(GDBResponse::Disconnected)
            }
            GDBCmd::SetSource(gdbsource) => {
                self.source = Some(gdbsource);
                Ok(GDBResponse::Disconnected)
            }
        }
    }

    /// Send Ctrl-C (0x03) interrupt and drain the two stop-reply packets
    /// that the Dolphin/Nintendont stub sends.
    pub fn halt(&mut self) -> Result<StopReply, GDBError> {
        let stream = self.stream.as_mut().ok_or(GDBError::NotConnected)?;
        stream.write_all(&[0x03])?;
        stream.flush()?;
        // The server sends two SIGTRAP stop-reply packets after 0x03:
        // one from the UpdateCallback handler, and one from the CPU
        // stepping handler in CPU.cpp. Drain both.
        self.read_packet()?;
        let reply = self.read_packet()?;
        parse_stop_reply(&reply)
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
        let reply = self.read_packet()?;
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
            regs.gpr[i] = u32::from_be_bytes([bytes[o], bytes[o+1], bytes[o+2], bytes[o+3]]);
        }

        if bytes.len() >= 416 {
            // Full register set: GPRs + FPRs + specials
            let mut offset = 128;
            for i in 0..32 {
                regs.fpr[i] = u64::from_be_bytes([
                    bytes[offset], bytes[offset+1], bytes[offset+2], bytes[offset+3],
                    bytes[offset+4], bytes[offset+5], bytes[offset+6], bytes[offset+7],
                ]);
                offset += 8;
            }
            let read_u32 = |o: usize| -> u32 {
                u32::from_be_bytes([bytes[o], bytes[o+1], bytes[o+2], bytes[o+3]])
            };
            regs.pc = read_u32(offset); offset += 4;
            regs.msr = read_u32(offset); offset += 4;
            regs.cr = read_u32(offset); offset += 4;
            regs.lr = read_u32(offset); offset += 4;
            regs.ctr = read_u32(offset); offset += 4;
            regs.xer = read_u32(offset); offset += 4;
            regs.fpscr = u64::from_be_bytes([
                bytes[offset], bytes[offset+1], bytes[offset+2], bytes[offset+3],
                bytes[offset+4], bytes[offset+5], bytes[offset+6], bytes[offset+7],
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
            return Err(GDBError::InvalidResponse(format!("memory read error: {}", hex)));
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

    /// Set a software breakpoint.
    ///
    /// On non-Windows: uses `Z0` (stub-managed breakpoints).
    /// On Windows: writes a trap instruction directly into memory because
    /// Dolphin's Windows GDB stub accepts `Z0` but doesn't act on it.
    pub fn set_breakpoint(&mut self, addr: u32) -> Result<(), GDBError> {
        #[cfg(not(windows))]
        {
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
        #[cfg(windows)]
        {
            const TRAP: [u8; 4] = [0x7F, 0xE0, 0x00, 0x08];
            if self.saved_insns.contains_key(&addr) {
                return Ok(());
            }
            let orig = self.read_memory(addr, 4)?;
            if orig.len() < 4 {
                return Err(GDBError::InvalidResponse("short read for breakpoint".into()));
            }
            let mut saved = [0u8; 4];
            saved.copy_from_slice(&orig[..4]);
            self.write_memory(addr, &TRAP)?;
            self.saved_insns.insert(addr, saved);
            Ok(())
        }
    }

    /// Remove a software breakpoint.
    pub fn remove_breakpoint(&mut self, addr: u32) -> Result<(), GDBError> {
        #[cfg(not(windows))]
        {
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
        #[cfg(windows)]
        {
            if let Some(saved) = self.saved_insns.remove(&addr) {
                self.write_memory(addr, &saved)?;
            }
            Ok(())
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

    /// Block reading until a stop-reply packet arrives. Used when the target
    /// is running after `c` to detect breakpoint hits, exceptions, etc.
    pub fn wait_stop(&mut self) -> Result<StopReply, GDBError> {
        let reply = self.read_packet()?;
        parse_stop_reply(&reply)
    }

    /// Try to clone the underlying TCP stream (for the RSP monitor thread).
    pub fn try_clone_stream(&self) -> Result<TcpStream, GDBError> {
        let stream = self.stream.as_ref().ok_or(GDBError::NotConnected)?;
        stream.try_clone().map_err(GDBError::Disconnected)
    }

    /// Get a mutable reference to the underlying stream (for raw writes like Ctrl-C).
    pub fn stream_mut(&mut self) -> Option<&mut TcpStream> {
        self.stream.as_mut()
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

    /// Drain any stale data from the TCP socket using a non-blocking read.
    /// Returns the number of bytes drained.
    pub fn drain_stale_data(&mut self) -> usize {
        let stream = match self.stream.as_mut() {
            Some(s) => s,
            None => return 0,
        };
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
        total
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
        self.read_packet()
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

        // Read payload until '#', computing checksum
        let mut payload = Vec::new();
        let mut computed_csum: u8 = 0;
        loop {
            stream.read_exact(&mut byte)?;
            if byte[0] == b'#' {
                break;
            }
            payload.push(byte[0]);
            computed_csum = computed_csum.wrapping_add(byte[0]);
        }

        // Read and validate 2-byte checksum
        let mut csum_bytes = [0u8; 2];
        stream.read_exact(&mut csum_bytes)?;
        let received_csum = from_hex_digit(csum_bytes[0]) << 4 | from_hex_digit(csum_bytes[1]);
        if received_csum != computed_csum {
            eprintln!(
                "RSP checksum mismatch: received {:02x}, computed {:02x}, payload len={}",
                received_csum, computed_csum, payload.len()
            );
        }

        // Send ACK (unless in no-ack mode)
        if !self.no_ack_mode {
            stream.write_all(b"+")?;
            stream.flush()?;
        }

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

        // In no-ack mode, don't wait for ACK
        if self.no_ack_mode {
            return Ok(());
        }

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
                        self.pending_stop_reply = Some(payload);
                    }
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

fn from_hex_digit(c: u8) -> u8 {
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

fn parse_stop_reply(data: &[u8]) -> Result<StopReply, GDBError> {
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
        assert_eq!(decode_hex("deadbeef").unwrap(), vec![0xde, 0xad, 0xbe, 0xef]);
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
