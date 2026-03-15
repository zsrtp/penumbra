//! Integration tests for gdb-client against a real GDB stub.
//!
//! These tests require a running GDB stub at the address specified by
//! `GDB_TEST_HOST`.  Set `GDB_TEST_TARGET` to `dolphin` or `nintendont`
//! to enable/skip target-specific tests.
//!
//! Run with:
//!   GDB_TEST_HOST=192.168.1.100:2159 \
//!   GDB_TEST_TARGET=nintendont \
//!   GDB_TEST_ELF=/path/to/game.elf \
//!   GDB_TEST_BP_SYMBOL=fapGm_Execute__Fv \
//!   cargo test --package gdb-client --test integration -- --ignored --test-threads=1
//!
//! All tests are `#[ignore]` so `cargo test` doesn't accidentally try to connect.

use gdb_client::{GDB, GDBCmd, GDBSource};
use std::net::IpAddr;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Dolphin,
    Nintendont,
    Unknown,
}

fn target() -> Target {
    match std::env::var("GDB_TEST_TARGET")
        .unwrap_or_default()
        .to_lowercase()
        .as_str()
    {
        "dolphin" => Target::Dolphin,
        "nintendont" | "nint" => Target::Nintendont,
        _ => Target::Unknown,
    }
}

/// Skip the test if the current target matches any of the listed targets.
macro_rules! skip_on {
    ($($t:expr),+ $(,)?) => {
        let cur = target();
        $(
            if cur == $t {
                eprintln!("SKIPPED: test not applicable to {:?}", cur);
                return;
            }
        )+
    };
}

fn test_target() -> Option<(IpAddr, u16)> {
    let host_port = std::env::var("GDB_TEST_HOST").ok()?;
    let (ip_str, port_str) = host_port.rsplit_once(':')?;
    let ip: IpAddr = ip_str.parse().ok()?;
    let port: u16 = port_str.parse().ok()?;
    Some((ip, port))
}

fn connect() -> GDB {
    let (ip, port) = test_target().expect("GDB_TEST_HOST not set (e.g. 192.168.1.100:2159)");
    let mut gdb = GDB::new();
    gdb.execute_cmd(GDBCmd::Connect(GDBSource::Network((ip, port))))
        .expect("failed to connect");
    gdb
}

fn connect_noack() -> GDB {
    let mut gdb = connect();
    gdb.query_stop_reason().expect("initial halt failed");
    gdb.negotiate().expect("negotiate failed");
    gdb
}

fn test_elf_path() -> PathBuf {
    let path = std::env::var("GDB_TEST_ELF").expect("GDB_TEST_ELF not set (path to game ELF)");
    PathBuf::from(path)
}

fn lookup_symbol(elf_data: &[u8], name: &str) -> u32 {
    use object::{Object, ObjectSymbol, SymbolKind};
    let file = object::File::parse(elf_data).expect("failed to parse ELF");
    for sym in file.symbols() {
        if sym.kind() == SymbolKind::Text {
            if sym.name() == Ok(name) {
                return sym.address() as u32;
            }
        }
    }
    panic!("symbol '{}' not found in ELF", name);
}

fn bp_addr() -> u32 {
    let sym_name = std::env::var("GDB_TEST_BP_SYMBOL")
        .expect("GDB_TEST_BP_SYMBOL not set (e.g. fapGm_Execute)");
    let elf_data = std::fs::read(test_elf_path()).expect("failed to read ELF file");
    lookup_symbol(&elf_data, &sym_name)
}

// ── Connection ──────────────────────────────────────────────────────────────

#[test]
#[ignore]
fn connect_and_halt() {
    let mut gdb = connect();
    let reply = gdb.query_stop_reason().expect("? command failed");
    assert!(
        reply.signal == 2 || reply.signal == 5,
        "unexpected signal: {}",
        reply.signal,
    );
    gdb.detach().expect("detach failed");
}

/// Test the consolidated connect_and_init() path (connect + ? + negotiate).
#[test]
#[ignore]
fn connect_and_init() {
    let (ip, port) = test_target().expect("GDB_TEST_HOST not set");
    let mut gdb = GDB::new();
    gdb.execute_cmd(GDBCmd::ConnectAndInit(GDBSource::Network((ip, port))))
        .expect("connect_and_init failed");
    assert!(
        gdb.is_no_ack_mode(),
        "no-ack mode should be active after init"
    );
    let regs = gdb.read_registers().expect("read regs failed");
    assert!(regs.pc >= 0x80000000, "PC out of range: 0x{:08x}", regs.pc);
    gdb.detach().expect("detach failed");
}

fn serial_source() -> Option<GDBSource> {
    let path = std::env::var("GDB_TEST_SERIAL").ok()?;
    let baud: u32 = std::env::var("GDB_TEST_SERIAL_BAUD")
        .unwrap_or_else(|_| "115200".to_string())
        .parse()
        .ok()?;
    Some(GDBSource::Serial {
        path: std::path::PathBuf::from(path),
        baud_rate: baud,
    })
}

fn connect_serial() -> GDB {
    let source = serial_source().expect("GDB_TEST_SERIAL not set (e.g. /dev/ttyUSB0)");
    let mut gdb = GDB::new();
    gdb.execute_cmd(GDBCmd::Connect(source))
        .expect("failed to connect");
    gdb
}

fn connect_serial_noack() -> GDB {
    let mut gdb = connect_serial();
    gdb.query_stop_reason().expect("initial halt failed");
    gdb.negotiate().expect("negotiate failed");
    gdb
}

// ── Serial Connection ─────────────────────────────────────────────────────────

#[test]
#[ignore]
fn serial_connect_and_halt() {
    let mut gdb = connect_serial();
    let reply = gdb.query_stop_reason().expect("? command failed");
    assert!(
        reply.signal == 2 || reply.signal == 5,
        "unexpected signal: {}",
        reply.signal,
    );
    gdb.detach().expect("detach failed");
}

#[test]
#[ignore]
fn serial_connect_and_init() {
    let source = serial_source().expect("GDB_TEST_SERIAL not set");
    let mut gdb = GDB::new();
    gdb.execute_cmd(GDBCmd::ConnectAndInit(source))
        .expect("connect_and_init failed");
    assert!(
        gdb.is_no_ack_mode(),
        "no-ack mode should be active after init"
    );
    let regs = gdb.read_registers().expect("read regs failed");
    assert!(regs.pc >= 0x80000000, "PC out of range: 0x{:08x}", regs.pc);
    gdb.detach().expect("detach failed");
}

#[test]
#[ignore]
fn serial_clone_and_monitor() {
    let mut gdb = connect_serial_noack();
    let no_ack = gdb.is_no_ack_mode();

    gdb.resume().expect("resume failed");

    // Clone the GDB for the monitor thread
    let mut gdb_clone = gdb.try_clone_stream().expect("clone failed");
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(1);
    std::thread::spawn(move || {
        if let Ok(stream) = gdb_clone.get_stream() {
            let _ = stream.set_nonblocking(false);
            if let Some(packet) = gdb_client::read_packet_from_stream(&mut *stream, no_ack) {
                let _ = tx.send(packet);
            }
        }
    });

    // Give target time to run
    std::thread::sleep(std::time::Duration::from_millis(100));
    gdb.send_interrupt().expect("interrupt failed");

    // Monitor thread should receive the stop-reply
    let packet = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("monitor didn't receive stop-reply");
    assert!(
        packet.starts_with(b"T") || packet.starts_with(b"S"),
        "expected stop-reply, got {:?}",
        String::from_utf8_lossy(&packet[..packet.len().min(20)]),
    );
}

#[test]
#[ignore]
fn serial_clone_preserves_connection() {
    let mut gdb = connect_serial();

    // Clone the GDB
    let gdb_clone = gdb.clone();

    // Both should still be connected (check state)
    assert_eq!(gdb.state, gdb_clone.state);
    assert!(matches!(gdb.state, gdb_client::GDBState::Connected));

    // The original should be able to query
    let reply = gdb.query_stop_reason().expect("query failed");
    assert!(reply.signal == 2 || reply.signal == 5);

    gdb.detach().expect("detach failed");
}

#[test]
#[ignore]
fn serial_read_registers() {
    let mut gdb = connect_serial_noack();
    let regs = gdb.read_registers().expect("read_registers failed");
    let sp = regs.gpr[1];
    assert!(
        sp >= 0x80000000 && sp <= 0x817FFFFF,
        "SP out of range: 0x{:08x}",
        sp,
    );
}

#[test]
#[ignore]
fn noack_mode() {
    let mut gdb = connect();
    gdb.query_stop_reason().expect("initial halt");
    gdb.negotiate().expect("negotiate failed");
    assert!(gdb.is_no_ack_mode());
    gdb.detach().expect("detach failed");
}

// ── Registers ───────────────────────────────────────────────────────────────

#[test]
#[ignore]
fn register_read_all() {
    let mut gdb = connect_noack();
    let regs = gdb.read_registers().expect("read_registers failed");
    let sp = regs.gpr[1];
    assert!(
        sp >= 0x80000000 && sp <= 0x817FFFFF,
        "SP out of range: 0x{:08x}",
        sp,
    );
    assert!(
        regs.pc >= 0x80000000 && regs.pc <= 0x817FFFFF,
        "PC out of range: 0x{:08x}",
        regs.pc,
    );
    assert!(
        regs.lr >= 0x80000000 && regs.lr <= 0x817FFFFF,
        "LR out of range: 0x{:08x}",
        regs.lr,
    );
    gdb.detach().expect("detach failed");
}

/// Both Dolphin and Nintendont return a full register blob (GPRs + FPRs +
/// specials) from the `g` command, though the exact size may vary slightly
/// (Dolphin: 416 bytes, Nintendont: 412 bytes).
#[test]
#[ignore]
fn register_blob_size() {
    let mut gdb = connect_noack();
    let regs = gdb.read_registers().expect("read_registers failed");
    eprintln!(
        "{:?} register blob size: {} bytes",
        target(),
        regs.reg_blob_size
    );
    // Both stubs return at least GPRs + FPRs + specials (>= 392 bytes)
    assert!(
        regs.reg_blob_size >= 392,
        "register blob too small: {} bytes (expected >= 392)",
        regs.reg_blob_size,
    );
    // All special regs should be populated from the blob
    assert!(regs.pc >= 0x80000000, "PC not populated: 0x{:08x}", regs.pc);
    assert!(regs.lr >= 0x80000000, "LR not populated: 0x{:08x}", regs.lr);
    gdb.detach().expect("detach failed");
}

#[test]
#[ignore]
fn register_read_individual() {
    let mut gdb = connect_noack();
    let sp_val = gdb.read_register(1).expect("p1 failed");
    assert!(
        sp_val >= 0x80000000 && sp_val <= 0x817FFFFF,
        "SP via p1 out of range: 0x{:08x}",
        sp_val,
    );
    // PC = register 0x40 = 64
    let pc_val = gdb.read_register(64).expect("p40 failed") as u32;
    assert!(
        pc_val >= 0x80000000 && pc_val <= 0x817FFFFF,
        "PC via p40 out of range: 0x{:08x}",
        pc_val,
    );
    gdb.detach().expect("detach failed");
}

#[test]
#[ignore]
fn register_write_readback() {
    let mut gdb = connect_noack();
    let orig = gdb.read_register(3).expect("read r3 failed");
    gdb.write_register(3, 0xDEADBEEF).expect("write r3 failed");
    let readback = gdb.read_register(3).expect("readback r3 failed");
    assert_eq!(readback, 0xDEADBEEF, "r3 write/readback mismatch");
    gdb.write_register(3, orig).expect("restore r3 failed");
    gdb.detach().expect("detach failed");
}

#[test]
#[ignore]
fn register_g_vs_p_consistency() {
    let mut gdb = connect_noack();
    let regs = gdb.read_registers().expect("g failed");
    let sp_p = gdb.read_register(1).expect("p1 failed") as u32;
    assert_eq!(
        regs.gpr[1], sp_p,
        "SP mismatch: g=0x{:08x} vs p=0x{:08x}",
        regs.gpr[1], sp_p
    );
    let pc_p = gdb.read_register(64).expect("p40 failed") as u32;
    assert_eq!(
        regs.pc, pc_p,
        "PC mismatch: g=0x{:08x} vs p=0x{:08x}",
        regs.pc, pc_p
    );
    let lr_p = gdb.read_register(67).expect("p43 failed") as u32;
    assert_eq!(
        regs.lr, lr_p,
        "LR mismatch: g=0x{:08x} vs p=0x{:08x}",
        regs.lr, lr_p
    );
    gdb.detach().expect("detach failed");
}

// ── Memory ──────────────────────────────────────────────────────────────────

#[test]
#[ignore]
fn memory_read() {
    let mut gdb = connect_noack();
    let addr = bp_addr();
    let data = gdb.read_memory(addr, 4).expect("read 4 bytes failed");
    assert_eq!(data.len(), 4);
    let data = gdb.read_memory(addr, 16).expect("read 16 bytes failed");
    assert_eq!(data.len(), 16);
    gdb.detach().expect("detach failed");
}

#[test]
#[ignore]
fn memory_read_invalid() {
    let mut gdb = connect_noack();
    let result = gdb.read_memory(0x20000000, 4);
    assert!(result.is_err(), "expected error for invalid address");
    gdb.detach().expect("detach failed");
}

#[test]
#[ignore]
fn memory_write_readback() {
    let mut gdb = connect_noack();
    let regs = gdb.read_registers().expect("read regs failed");
    let addr = regs.gpr[1] + 0x20; // safe scratch space below the stack frame
    let orig = gdb.read_memory(addr, 2).expect("read original failed");
    gdb.write_memory(addr, &[0x00, 0x42]).expect("write failed");
    let readback = gdb.read_memory(addr, 2).expect("readback failed");
    assert_eq!(readback, &[0x00, 0x42], "write/readback mismatch");
    gdb.write_memory(addr, &orig).expect("restore failed");
    gdb.detach().expect("detach failed");
}

// ── Breakpoints ─────────────────────────────────────────────────────────────

#[test]
#[ignore]
fn breakpoint_hit() {
    let mut gdb = connect_noack();
    let addr = bp_addr();
    gdb.set_breakpoint(addr).expect("set BP failed");
    let reply = gdb.continue_and_wait().expect("continue_and_wait failed");
    assert_eq!(
        reply.signal, 5,
        "expected SIGTRAP (5), got {}",
        reply.signal
    );
    let regs = gdb.read_registers().expect("read regs failed");
    assert_eq!(regs.pc, addr, "PC should be at breakpoint");
    gdb.remove_breakpoint(addr).expect("remove BP failed");
    gdb.detach().expect("detach failed");
}

#[test]
#[ignore]
fn breakpoint_continue_multiple() {
    let mut gdb = connect_noack();
    let addr = bp_addr();
    gdb.set_breakpoint(addr).expect("set BP failed");
    for i in 0..5 {
        let reply = gdb
            .continue_and_wait()
            .expect(&format!("continue {} failed", i));
        assert_eq!(reply.signal, 5, "iteration {}: expected SIGTRAP", i);
        let regs = gdb.read_registers().expect("read regs failed");
        assert_eq!(regs.pc, addr, "iteration {}: PC mismatch", i);
    }
    gdb.remove_breakpoint(addr).expect("remove BP failed");
    gdb.detach().expect("detach failed");
}

#[test]
#[ignore]
fn breakpoint_remove_restores_instruction() {
    let mut gdb = connect_noack();
    let addr = bp_addr();
    let orig = gdb.read_memory(addr, 4).expect("read original failed");
    gdb.set_breakpoint(addr).expect("set BP failed");
    let trapped = gdb.read_memory(addr, 4).expect("read trap failed");
    assert_eq!(
        trapped,
        &[0x7F, 0xE0, 0x00, 0x08],
        "expected trap instruction"
    );
    gdb.remove_breakpoint(addr).expect("remove BP failed");
    let restored = gdb.read_memory(addr, 4).expect("read restored failed");
    assert_eq!(restored, orig, "instruction not restored");
    gdb.detach().expect("detach failed");
}

// ── Interrupt ───────────────────────────────────────────────────────────────

/// Test send_interrupt(): resume the target, send 0x03, read stop-reply.
#[test]
#[ignore]
fn interrupt_stops_running_target() {
    let mut gdb = connect_noack();
    gdb.resume().expect("resume failed");

    // Give the target a moment to actually start running
    std::thread::sleep(std::time::Duration::from_millis(100));

    gdb.send_interrupt().expect("send_interrupt failed");
    let reply = gdb.wait_stop().expect("wait_stop after interrupt failed");
    assert!(
        reply.signal == 2 || reply.signal == 5,
        "expected SIGINT(2) or SIGTRAP(5) after interrupt, got {}",
        reply.signal,
    );

    // Should be able to read registers after interrupt
    let regs = gdb
        .read_registers()
        .expect("read regs after interrupt failed");
    assert!(regs.pc >= 0x80000000, "PC out of range: 0x{:08x}", regs.pc);
    gdb.detach().expect("detach failed");
}

/// Test interrupt + resume cycle multiple times.
#[test]
#[ignore]
fn interrupt_resume_cycle() {
    let mut gdb = connect_noack();

    for i in 0..5 {
        gdb.resume().expect(&format!("resume {} failed", i));
        std::thread::sleep(std::time::Duration::from_millis(100));
        gdb.send_interrupt()
            .expect(&format!("interrupt {} failed", i));
        let reply = gdb.wait_stop().expect(&format!("wait_stop {} failed", i));
        assert!(
            reply.signal == 2 || reply.signal == 5,
            "iteration {}: unexpected signal {}",
            i,
            reply.signal,
        );
        let regs = gdb
            .read_registers()
            .expect(&format!("read regs {} failed", i));
        assert!(regs.pc >= 0x80000000, "iter {}: PC out of range", i);
    }

    gdb.detach().expect("detach failed");
}

// ── Stack Walk ──────────────────────────────────────────────────────────────

/// Walk PPC EABI back chain, returning (depth, terminated_cleanly).
fn walk_stack(gdb: &mut GDB, sp: u32) -> (usize, bool) {
    let mut current = sp;
    let mut depth = 0;
    for _ in 0..64 {
        let data = match gdb.read_memory(current, 4) {
            Ok(d) if d.len() == 4 => d,
            _ => return (depth, false),
        };
        let back_chain = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        if back_chain == 0 {
            return (depth, true);
        }
        if back_chain < 0x80000000 || back_chain > 0x817FFFFF || back_chain == current {
            return (depth, false);
        }
        current = back_chain;
        depth += 1;
    }
    (depth, false) // max depth reached
}

#[test]
#[ignore]
fn stack_walk_from_halt() {
    let mut gdb = connect_noack();
    let regs = gdb.read_registers().expect("read regs failed");
    let (depth, clean) = walk_stack(&mut gdb, regs.gpr[1]);
    assert!(clean, "stack walk did not terminate cleanly");
    assert!(depth >= 1, "stack should have at least 1 frame");
    gdb.detach().expect("detach failed");
}

#[test]
#[ignore]
fn stack_walk_after_breakpoint_continue() {
    let mut gdb = connect_noack();
    let addr = bp_addr();
    gdb.set_breakpoint(addr).expect("set BP failed");

    for i in 0..10 {
        gdb.continue_and_wait()
            .expect(&format!("continue {} failed", i));
        let regs = gdb.read_registers().expect("read regs failed");
        assert_eq!(regs.pc, addr, "iter {}: PC mismatch", i);
        let (depth, clean) = walk_stack(&mut gdb, regs.gpr[1]);
        assert!(clean, "iter {}: stack walk not clean (depth {})", i, depth);
        assert!(depth >= 1, "iter {}: empty stack", i);
    }

    gdb.remove_breakpoint(addr).expect("remove BP failed");
    gdb.detach().expect("detach failed");
}

// ── Monitor Pattern (Cloned Socket) ─────────────────────────────────────────

/// Test the shared read_packet_from_stream() with the monitor pattern:
/// resume target, read stop-reply on cloned socket, then use main socket.
#[test]
#[ignore]
fn monitor_pattern_stack_walk() {
    let mut gdb = connect_noack();
    let addr = bp_addr();
    let no_ack = gdb.is_no_ack_mode();
    gdb.set_breakpoint(addr).expect("set BP failed");

    for i in 0..10 {
        gdb.resume().expect("resume failed");

        // Read stop-reply from a cloned stream using the shared function
        let mut clone = gdb.try_clone_tcp_stream().expect("clone failed");
        clone
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let packet = gdb_client::read_packet_from_stream(&mut clone, no_ack);
        drop(clone);

        let payload = packet.expect(&format!("iter {}: no packet received", i));
        assert!(
            payload.starts_with(b"T"),
            "iter {}: expected stop-reply, got {:?}",
            i,
            String::from_utf8_lossy(&payload[..payload.len().min(20)]),
        );

        // Verify stop-reply parses correctly
        let stop = gdb_client::parse_stop_reply(&payload)
            .expect(&format!("iter {}: parse_stop_reply failed", i));
        assert_eq!(stop.signal, 5, "iter {}: expected SIGTRAP", i);

        // Use main socket for reads (simulates DAP handler after monitor sets running=false)
        let regs = gdb
            .read_registers()
            .expect(&format!("iter {}: read regs failed", i));
        assert_eq!(regs.pc, addr, "iter {}: PC mismatch", i);
        let (depth, clean) = walk_stack(&mut gdb, regs.gpr[1]);
        assert!(clean, "iter {}: stack walk not clean (depth {})", i, depth);
    }

    gdb.remove_breakpoint(addr).expect("remove BP failed");
    gdb.detach().expect("detach failed");
}

/// Test the full GUI-style monitor pattern: resume, spawn monitor thread,
/// send interrupt, monitor thread receives stop-reply.
#[test]
#[ignore]
fn monitor_interrupt_pattern() {
    let mut gdb = connect_noack();
    let no_ack = gdb.is_no_ack_mode();

    for i in 0..5 {
        gdb.resume().expect(&format!("resume {} failed", i));

        // Spawn a monitor thread (like gui.rs gdb_thread does)
        let mut clone = gdb.try_clone_tcp_stream().expect("clone failed");
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(1);
        std::thread::spawn(move || {
            let _ = clone.set_read_timeout(Some(std::time::Duration::from_secs(10)));
            if let Some(packet) = gdb_client::read_packet_from_stream(&mut clone, no_ack) {
                let _ = tx.send(packet);
            }
        });

        // Give target time to run, then interrupt
        std::thread::sleep(std::time::Duration::from_millis(100));
        gdb.send_interrupt()
            .expect(&format!("interrupt {} failed", i));

        // Monitor thread should receive the stop-reply
        let packet = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect(&format!("iter {}: monitor didn't receive stop-reply", i));
        assert!(
            packet.starts_with(b"T") || packet.starts_with(b"S"),
            "iter {}: expected stop-reply, got {:?}",
            i,
            String::from_utf8_lossy(&packet[..packet.len().min(20)]),
        );

        // Main socket should work for commands now
        let regs = gdb
            .read_registers()
            .expect(&format!("iter {}: read regs failed", i));
        assert!(regs.pc >= 0x80000000, "iter {}: PC out of range", i);
    }

    gdb.detach().expect("detach failed");
}

// ── Halt-for-Breakpoints (set BP while running) ─────────────────────────────

/// Simulate the DAP "set breakpoint while target is running" pattern:
/// resume → interrupt → wait for stop → set breakpoint → resume → hit BP.
///
/// This is the core flow that `SetBreakpoints` uses when the target is running.
#[test]
#[ignore]
fn set_breakpoint_while_running() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let mut gdb = connect_noack();
    let addr = bp_addr();
    let no_ack = gdb.is_no_ack_mode();
    let target_running = Arc::new(AtomicBool::new(false));

    // Resume the target (simulates ConfigurationDone/Continue)
    gdb.resume().expect("initial resume failed");
    target_running.store(true, Ordering::SeqCst);

    // Spawn a monitor thread (same pattern as start_rsp_monitor)
    let mut clone = gdb.try_clone_tcp_stream().expect("clone failed");
    let running = target_running.clone();
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(1);
    std::thread::spawn(move || {
        let _ = clone.set_read_timeout(None);
        if let Some(packet) = gdb_client::read_packet_from_stream(&mut clone, no_ack) {
            // Clear running flag FIRST (same as start_rsp_monitor)
            running.store(false, Ordering::SeqCst);
            let _ = tx.send(packet);
        }
    });

    // Give target time to run
    std::thread::sleep(std::time::Duration::from_millis(100));

    // Now simulate SetBreakpoints while running:
    // 1. Check target is running
    assert!(
        target_running.load(Ordering::SeqCst),
        "target should be running"
    );

    // 2. Send interrupt
    gdb.send_interrupt().expect("interrupt failed");

    // 3. Spin-wait for target_running to become false (with timeout)
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while target_running.load(Ordering::SeqCst) {
        assert!(
            std::time::Instant::now() < deadline,
            "timeout waiting for target to halt"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }

    // Monitor should have received the stop-reply
    let packet = rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("monitor didn't receive stop-reply");
    assert!(
        packet.starts_with(b"T") || packet.starts_with(b"S"),
        "expected stop-reply, got {:?}",
        String::from_utf8_lossy(&packet[..packet.len().min(20)]),
    );

    // 4. Drain stale data (same pattern as SetBreakpoints handler)
    let _ = gdb.take_pending_stop_reply();
    gdb.drain_stale_data();

    // 5. Set breakpoint while halted
    gdb.set_breakpoint(addr)
        .expect("set BP failed (while halted after interrupt)");

    // 6. Resume
    gdb.resume().expect("resume after BP set failed");

    // 7. Wait for breakpoint hit
    let reply = gdb.wait_stop().expect("wait_stop for BP hit failed");
    assert_eq!(reply.signal, 5, "expected SIGTRAP, got {}", reply.signal);
    let regs = gdb.read_registers().expect("read regs failed");
    assert_eq!(regs.pc, addr, "PC should be at breakpoint: 0x{:08x}", addr);

    gdb.remove_breakpoint(addr).expect("remove BP failed");
    gdb.detach().expect("detach failed");
}

/// Verify that setting a breakpoint while the target is already stopped
/// does NOT require halting — no interrupt is sent.
/// Requires reconnection, so skip on Dolphin (single-use stub).
#[test]
#[ignore]
fn set_breakpoint_while_stopped_no_halt() {
    skip_on!(Target::Dolphin);
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let mut gdb = connect_noack();
    let addr = bp_addr();
    let target_running = Arc::new(AtomicBool::new(false));

    // Target is stopped (just connected). needs_halt should be false.
    assert!(
        !target_running.load(Ordering::SeqCst),
        "target should be stopped"
    );

    // Set breakpoint directly — no halt needed.
    gdb.set_breakpoint(addr).expect("set BP failed");

    // Verify the trap instruction is in place.
    let mem = gdb.read_memory(addr, 4).expect("read BP addr failed");
    assert_eq!(mem, &[0x7F, 0xE0, 0x00, 0x08], "expected trap instruction");

    gdb.remove_breakpoint(addr).expect("remove BP failed");
    gdb.detach().expect("detach failed");
}

/// Halt-for-breakpoints multiple times in a row to test the full cycle.
/// resume → halt → set BP → resume → hit → remove → repeat with different state.
/// Requires reconnection, so skip on Dolphin (single-use stub).
#[test]
#[ignore]
fn set_breakpoint_while_running_cycle() {
    skip_on!(Target::Dolphin);
    let mut gdb = connect_noack();
    let addr = bp_addr();

    for i in 0..3 {
        // Resume
        gdb.resume().expect(&format!("resume {} failed", i));
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Halt
        gdb.send_interrupt()
            .expect(&format!("interrupt {} failed", i));
        let reply = gdb.wait_stop().expect(&format!("wait_stop {} failed", i));
        assert!(
            reply.signal == 2 || reply.signal == 5,
            "iter {}: unexpected signal {}",
            i,
            reply.signal,
        );

        // Set breakpoint while halted
        gdb.set_breakpoint(addr)
            .expect(&format!("set BP {} failed", i));

        // Resume — should hit BP
        let reply = gdb
            .continue_and_wait()
            .expect(&format!("continue {} failed", i));
        assert_eq!(reply.signal, 5, "iter {}: expected SIGTRAP", i);
        let regs = gdb
            .read_registers()
            .expect(&format!("read regs {} failed", i));
        assert_eq!(regs.pc, addr, "iter {}: PC mismatch", i);

        // Remove and continue cycle
        gdb.remove_breakpoint(addr)
            .expect(&format!("remove BP {} failed", i));
    }

    gdb.detach().expect("detach failed");
}

// ── Single Step ─────────────────────────────────────────────────────────────

/// Dolphin's `s` (single step) command is unreliable — it intermittently
/// treats `s` as `c` (continue).  Skip on Dolphin.
#[test]
#[ignore]
fn single_step_advances_pc() {
    skip_on!(Target::Dolphin);
    let mut gdb = connect_noack();
    let regs = gdb.read_registers().expect("read regs failed");
    let orig_pc = regs.pc;
    gdb.step().expect("step failed");
    let regs = gdb.read_registers().expect("read regs after step");
    assert_ne!(regs.pc, orig_pc, "PC should change after step");
    gdb.detach().expect("detach failed");
}

// ── Reconnection ────────────────────────────────────────────────────────────

/// Dolphin's GDB stub is single-use — it stops listening after the first
/// connection detaches.  Skip on Dolphin.
#[test]
#[ignore]
fn reconnect_rapid() {
    skip_on!(Target::Dolphin);
    for _ in 0..5 {
        let mut gdb = connect_noack();
        let regs = gdb.read_registers().expect("read regs failed");
        assert!(regs.pc >= 0x80000000);
        gdb.detach().expect("detach failed");
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}
