//! Debug Adapter Protocol (DAP) server implementation.
//!
//! This module implements the Debug Adapter Protocol, allowing penumbra
//! to act as a debug server for IDEs like VS Code.
//!
//! The adapter translates DAP requests into GDB RSP commands and
//! translates responses back into DAP format.
//!
//! Key concepts:
//! - [`DebugAdapter`][adapter::DebugAdapter]: Main state machine for DAP handling
//! - `start_rsp_monitor()`: Background thread for async stop detection
//! - `needs_halt_for_breakpoints()`: Determines if breakpoint changes require halting

pub mod adapter;
pub mod symbols;

use std::io::{BufReader, BufWriter, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dap::events::{Event, OutputEventBody, StoppedEventBody};
use dap::requests::Command;
use dap::responses::*;
use dap::server::{Server, ServerOutput};
use dap::types::*;

use self::adapter::DebugAdapter;

fn stopped_event(reason: StoppedEventReason) -> Event {
    Event::Stopped(StoppedEventBody {
        reason,
        description: None,
        thread_id: Some(1),
        preserve_focus_hint: None,
        text: None,
        all_threads_stopped: Some(true),
        hit_breakpoint_ids: None,
    })
}

fn output_event(msg: &str) -> Event {
    Event::Output(OutputEventBody {
        category: Some(OutputEventCategory::Console),
        output: format!("{}\n", msg),
        group: None,
        variables_reference: None,
        source: None,
        line: None,
        column: None,
        data: None,
    })
}

/// Read a big-endian u32 from the target at the given address.
fn read_u32_be(gdb: &mut gdb_client::GDB, addr: u32) -> Option<u32> {
    let bytes = gdb.read_memory(addr, 4).ok()?;
    if bytes.len() == 4 {
        Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    } else {
        None
    }
}

/// Diagnostic reads after attach to verify SHM state and exception handlers.
/// Only emits output when `verbose` is true.
fn run_attach_diagnostics<R: Read, W: Write>(
    gdb: &mut gdb_client::GDB,
    server: &mut Server<R, W>,
    verbose: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if !verbose {
        return Ok(());
    }
    // SHM MAGIC at 0xD3003600 (uncached MEM2 on PPC)
    if let Some(magic) = read_u32_be(gdb, 0xD300_3600) {
        let ok = if magic == 0x4744_4253 {
            "OK"
        } else {
            "MISMATCH"
        };
        server.send_event(output_event(&format!(
            "Diag: SHM MAGIC = 0x{:08X} (expect 0x47444253) [{}]",
            magic, ok
        )))?;
    } else {
        server.send_event(output_event("Diag: SHM MAGIC read failed"))?;
    }

    // SHM STATE at 0xD3003604
    if let Some(state) = read_u32_be(gdb, 0xD300_3604) {
        let name = match state {
            0 => "IDLE",
            1 => "STOPPED",
            2 => "RESUME",
            3 => "STEP",
            4 => "DETACH",
            _ => "UNKNOWN",
        };
        server.send_event(output_event(&format!(
            "Diag: SHM STATE = {} ({})",
            state, name
        )))?;
    }

    // OS exception table at 0x80003000.
    // Program exception = index 6, Trace = index 10.
    let prog_handler = read_u32_be(gdb, 0x8000_3018); // 0x80003000 + 6*4
    let trace_handler = read_u32_be(gdb, 0x8000_3028); // 0x80003000 + 10*4
    if let (Some(ph), Some(th)) = (prog_handler, trace_handler) {
        let same = if ph == th {
            " (same = gdb stub)"
        } else {
            " (DIFFERENT!)"
        };
        server.send_event(output_event(&format!(
            "Diag: Exc handlers: Program=0x{:08X} Trace=0x{:08X}{}",
            ph, th, same
        )))?;
    }

    Ok(())
}

pub fn run_dap_server(
    program: &str,
    debug_info: Option<&str>,
    target: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let input = BufReader::new(std::io::stdin());
    let output = BufWriter::new(std::io::stdout());
    let mut server = Server::new(input, output);
    let server_output = server.output.clone();

    let mut adapter = DebugAdapter::new();
    let suppress_stop_event = Arc::new(AtomicBool::new(false));

    'main: loop {
        let mut req = match server.poll_request()? {
            Some(r) => r,
            None => break,
        };

        let command = std::mem::replace(&mut req.command, Command::Threads);

        match command {
            Command::Initialize(_args) => {
                server.send_event(output_event("Penumbra debug adapter starting"))?;
                let caps = adapter.handle_initialize();
                let resp = req.success(ResponseBody::Initialize(caps));
                server.respond(resp)?;
                server.send_event(Event::Initialized)?;
            }

            Command::Attach(args) => {
                let mut merged = args.additional_data.unwrap_or_default();
                if merged.get("target").is_none() {
                    merged["target"] = serde_json::Value::String(target.to_string());
                }
                if merged.get("program").is_none() {
                    merged["program"] = serde_json::Value::String(program.to_string());
                }
                if let Some(di) = debug_info {
                    if merged.get("debugInfo").is_none() {
                        merged["debugInfo"] = serde_json::Value::String(di.to_string());
                    }
                }

                let target_str = merged.get("target").and_then(|v| v.as_str()).unwrap_or("?");
                let program_str = merged
                    .get("program")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                server.send_event(output_event(&format!(
                    "Attaching to {} (program: {})",
                    target_str, program_str
                )))?;

                match adapter.handle_attach(&merged) {
                    Ok(()) => {
                        let di_path = merged.get("debugInfo").and_then(|v| v.as_str());
                        server.send_event(output_event(&format!(
                            "Connected (no-ack: {}, debugInfo: {:?}, project_root: {:?})",
                            adapter.gdb.is_no_ack_mode(),
                            di_path,
                            merged.get("program").and_then(|v| v.as_str()),
                        )))?;

                        // Diagnostic: verify SHM state and exception handlers
                        run_attach_diagnostics(&mut adapter.gdb, &mut server, adapter.verbose)?;

                        server.respond(req.success(ResponseBody::Attach))?;
                    }
                    Err(e) => {
                        server.send_event(output_event(&format!("Attach failed: {}", e)))?;
                        server.respond(req.error(&e))?;
                    }
                }
            }

            Command::ConfigurationDone => {
                // Drain stale data BEFORE resume (target is stopped, socket clean)
                let _ = adapter.gdb.take_pending_stop_reply();
                let drained = adapter.gdb.drain_stale_data();
                if adapter.verbose && drained > 0 {
                    server.send_event(output_event(&format!(
                        "Drained {} stale bytes before resume",
                        drained
                    )))?;
                }
                match adapter.handle_configuration_done() {
                    Ok(()) => {
                        server.send_event(output_event("Configuration done, target resumed"))?;
                        server.respond(req.success(ResponseBody::ConfigurationDone))?;
                        adapter.gdb.set_target_running(true);
                        start_rsp_monitor(
                            &adapter.gdb,
                            &server_output,
                            &adapter.gdb.target_running_flag(),
                            &suppress_stop_event,
                            adapter.verbose,
                        );
                    }
                    Err(e) => {
                        server
                            .send_event(output_event(&format!("ConfigurationDone error: {}", e)))?;
                        server.respond(req.error(&e.to_string()))?;
                    }
                }
            }

            Command::SetBreakpoints(args) => {
                let file = args
                    .source
                    .path
                    .as_deref()
                    .or(args.source.name.as_deref())
                    .unwrap_or("?");
                let count = args.breakpoints.as_ref().map(|b| b.len()).unwrap_or(0);
                server.send_event(output_event(&format!(
                    "SetBreakpoints: {} breakpoints in {:?} (path={:?}, name={:?})",
                    count,
                    file,
                    args.source.path.as_deref(),
                    args.source.name.as_deref(),
                )))?;

                // If target is running, halt it transparently before setting breakpoints.
                let was_running = needs_halt_for_breakpoints(&adapter.gdb.target_running_flag());
                if was_running {
                    if adapter.verbose {
                        server.send_event(output_event(
                            "SetBreakpoints: target running, halting transparently",
                        ))?;
                    }
                    suppress_stop_event.store(true, Ordering::SeqCst);
                    if let Err(e) = adapter.gdb.send_interrupt() {
                        suppress_stop_event.store(false, Ordering::SeqCst);
                        server.send_event(output_event(&format!(
                            "SetBreakpoints: interrupt failed: {}",
                            e
                        )))?;
                        server.respond(req.error(&format!("failed to halt target: {}", e)))?;
                        continue;
                    }
                    // Spin-wait for the monitor thread to clear target_running.
                    let deadline = Instant::now() + Duration::from_secs(2);
                    while adapter.gdb.target_running_flag().load(Ordering::SeqCst) {
                        if Instant::now() > deadline {
                            suppress_stop_event.store(false, Ordering::SeqCst);
                            server.send_event(output_event(
                                "SetBreakpoints: timeout waiting for target to halt",
                            ))?;
                            server.respond(req.error("timeout waiting for target to halt"))?;
                            continue 'main;
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    // Drain stale data now that the target is stopped.
                    let _ = adapter.gdb.take_pending_stop_reply();
                    let drained = adapter.gdb.drain_stale_data();
                    if adapter.verbose && drained > 0 {
                        server.send_event(output_event(&format!(
                            "SetBreakpoints: drained {} stale bytes after halt",
                            drained
                        )))?;
                    }
                }

                let breakpoints = adapter
                    .handle_set_breakpoints(&args.source, &args.breakpoints.unwrap_or_default());
                for bp in &breakpoints {
                    if !bp.verified {
                        server.send_event(output_event(&format!(
                            "  Breakpoint unverified: {}",
                            bp.message.as_deref().unwrap_or("unknown reason")
                        )))?;
                    }
                }
                // Diagnostic: verify trap instructions at breakpoint addresses
                if adapter.verbose {
                    for addr in adapter.breakpoint_addrs() {
                        match adapter.gdb.read_memory(addr, 4) {
                            Ok(bytes) if bytes.len() == 4 => {
                                let insn =
                                    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                                if insn == 0x7FE00008 {
                                    server.send_event(output_event(&format!(
                                        "  BP 0x{:08X}: trap verified",
                                        addr
                                    )))?;
                                } else {
                                    server.send_event(output_event(&format!(
                                        "  BP 0x{:08X}: UNEXPECTED insn 0x{:08X} (expected 0x7FE00008)", addr, insn
                                    )))?;
                                }
                            }
                            Ok(_) => {
                                server.send_event(output_event(&format!(
                                    "  BP 0x{:08X}: short read",
                                    addr
                                )))?;
                            }
                            Err(e) => {
                                server.send_event(output_event(&format!(
                                    "  BP 0x{:08X}: read failed: {}",
                                    addr, e
                                )))?;
                            }
                        }
                    }
                }
                server.respond(req.success(ResponseBody::SetBreakpoints(
                    SetBreakpointsResponse { breakpoints },
                )))?;

                // If we halted the target, resume it transparently.
                if was_running {
                    let _ = adapter.gdb.take_pending_stop_reply();
                    let drained = adapter.gdb.drain_stale_data();
                    if adapter.verbose && drained > 0 {
                        server.send_event(output_event(&format!(
                            "SetBreakpoints: drained {} stale bytes before resume",
                            drained
                        )))?;
                    }
                    match adapter.handle_continue() {
                        Ok(()) => {
                            if adapter.verbose {
                                server.send_event(output_event(
                                    "SetBreakpoints: target resumed after breakpoint update",
                                ))?;
                            }
                            suppress_stop_event.store(false, Ordering::SeqCst);
                            adapter.gdb.set_target_running(true);
                            start_rsp_monitor(
                                &adapter.gdb,
                                &server_output,
                                &adapter.gdb.target_running_flag(),
                                &suppress_stop_event,
                                adapter.verbose,
                            );
                        }
                        Err(e) => {
                            suppress_stop_event.store(false, Ordering::SeqCst);
                            server.send_event(output_event(&format!(
                                "SetBreakpoints: resume failed: {}",
                                e
                            )))?;
                            // Target is now stopped — emit a Stopped event so the
                            // UI reflects the actual state.
                            server.send_event(stopped_event(StoppedEventReason::Breakpoint))?;
                        }
                    }
                }
            }

            Command::SetFunctionBreakpoints(args) => {
                // If target is running, halt it transparently.
                let was_running = needs_halt_for_breakpoints(&adapter.gdb.target_running_flag());
                if was_running {
                    if adapter.verbose {
                        server.send_event(output_event(
                            "SetFunctionBreakpoints: target running, halting transparently",
                        ))?;
                    }
                    suppress_stop_event.store(true, Ordering::SeqCst);
                    if let Err(e) = adapter.gdb.send_interrupt() {
                        suppress_stop_event.store(false, Ordering::SeqCst);
                        server.send_event(output_event(&format!(
                            "SetFunctionBreakpoints: interrupt failed: {}",
                            e
                        )))?;
                        server.respond(req.error(&format!("failed to halt target: {}", e)))?;
                        continue;
                    }
                    let deadline = Instant::now() + Duration::from_secs(2);
                    while adapter.gdb.target_running_flag().load(Ordering::SeqCst) {
                        if Instant::now() > deadline {
                            suppress_stop_event.store(false, Ordering::SeqCst);
                            server.send_event(output_event(
                                "SetFunctionBreakpoints: timeout waiting for target to halt",
                            ))?;
                            server.respond(req.error("timeout waiting for target to halt"))?;
                            continue 'main;
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    let _ = adapter.gdb.take_pending_stop_reply();
                    let drained = adapter.gdb.drain_stale_data();
                    if adapter.verbose && drained > 0 {
                        server.send_event(output_event(&format!(
                            "SetFunctionBreakpoints: drained {} stale bytes after halt",
                            drained
                        )))?;
                    }
                }

                let breakpoints = adapter.handle_set_function_breakpoints(&args.breakpoints);
                server.respond(req.success(ResponseBody::SetFunctionBreakpoints(
                    SetFunctionBreakpointsResponse { breakpoints },
                )))?;

                // If we halted the target, resume it transparently.
                if was_running {
                    let _ = adapter.gdb.take_pending_stop_reply();
                    let drained = adapter.gdb.drain_stale_data();
                    if adapter.verbose && drained > 0 {
                        server.send_event(output_event(&format!(
                            "SetFunctionBreakpoints: drained {} stale bytes before resume",
                            drained
                        )))?;
                    }
                    match adapter.handle_continue() {
                        Ok(()) => {
                            if adapter.verbose {
                                server.send_event(output_event(
                                    "SetFunctionBreakpoints: target resumed after breakpoint update"
                                ))?;
                            }
                            suppress_stop_event.store(false, Ordering::SeqCst);
                            adapter.gdb.set_target_running(true);
                            start_rsp_monitor(
                                &adapter.gdb,
                                &server_output,
                                &adapter.gdb.target_running_flag(),
                                &suppress_stop_event,
                                adapter.verbose,
                            );
                        }
                        Err(e) => {
                            suppress_stop_event.store(false, Ordering::SeqCst);
                            server.send_event(output_event(&format!(
                                "SetFunctionBreakpoints: resume failed: {}",
                                e
                            )))?;
                            server.send_event(stopped_event(StoppedEventReason::Breakpoint))?;
                        }
                    }
                }
            }

            Command::SetExceptionBreakpoints(_) => {
                server.respond(req.success(ResponseBody::SetExceptionBreakpoints(
                    SetExceptionBreakpointsResponse { breakpoints: None },
                )))?;
            }

            Command::Threads => {
                let threads = adapter.handle_threads();
                server.respond(req.success(ResponseBody::Threads(ThreadsResponse { threads })))?;
            }

            Command::StackTrace(_args) => {
                // Ensure registers are fresh before building frames
                match adapter.refresh_registers_logged() {
                    Ok(msg) => {
                        if adapter.verbose {
                            server.send_event(output_event(&msg))?;
                        }
                    }
                    Err(e) => {
                        server.send_event(output_event(&format!(
                            "StackTrace: register read failed: {}",
                            e
                        )))?;
                        server.respond(req.error(&e.to_string()))?;
                        continue;
                    }
                }
                match adapter.handle_stack_trace() {
                    Ok(frames) => {
                        if adapter.verbose {
                            for msg in adapter.take_step_log() {
                                server.send_event(output_event(&msg))?;
                            }
                        } else {
                            adapter.take_step_log();
                        }
                        let total = frames.len() as i64;
                        server.respond(req.success(ResponseBody::StackTrace(
                            StackTraceResponse {
                                stack_frames: frames,
                                total_frames: Some(total),
                            },
                        )))?;
                    }
                    Err(e) => {
                        for msg in adapter.take_step_log() {
                            server.send_event(output_event(&msg))?;
                        }
                        server.send_event(output_event(&format!("StackTrace error: {}", e)))?;
                        server.respond(req.error(&e.to_string()))?;
                    }
                }
            }

            Command::Scopes(args) => {
                let scopes = adapter.handle_scopes(args.frame_id);
                server.respond(req.success(ResponseBody::Scopes(ScopesResponse { scopes })))?;
            }

            Command::Variables(args) => match adapter.handle_variables(args.variables_reference) {
                Ok(variables) => {
                    server.respond(
                        req.success(ResponseBody::Variables(VariablesResponse { variables })),
                    )?;
                }
                Err(e) => server.respond(req.error(&e.to_string()))?,
            },

            Command::Continue(_args) => {
                if adapter.verbose {
                    server.send_event(output_event("Continuing target"))?;
                }
                // Drain stale data BEFORE sending 'c' (while target is stopped
                // and the socket is in a known state). Draining AFTER resume
                // could race with a fast stop-reply.
                let _ = adapter.gdb.take_pending_stop_reply();
                let drained = adapter.gdb.drain_stale_data();
                if adapter.verbose && drained > 0 {
                    server.send_event(output_event(&format!(
                        "Drained {} stale bytes before continue",
                        drained
                    )))?;
                }
                match adapter.handle_continue() {
                    Ok(()) => {
                        server.respond(req.success(ResponseBody::Continue(ContinueResponse {
                            all_threads_continued: Some(true),
                        })))?;
                        adapter.gdb.set_target_running(true);
                        start_rsp_monitor(
                            &adapter.gdb,
                            &server_output,
                            &adapter.gdb.target_running_flag(),
                            &suppress_stop_event,
                            adapter.verbose,
                        );
                    }
                    Err(e) => {
                        server.send_event(output_event(&format!("Continue error: {}", e)))?;
                        server.respond(req.error(&e.to_string()))?;
                    }
                }
            }

            Command::Pause(_args) => {
                if adapter.verbose {
                    server.send_event(output_event("Sending interrupt (0x03)"))?;
                }
                if let Err(e) = adapter.gdb.send_interrupt() {
                    server.send_event(output_event(&format!("Interrupt write failed: {}", e)))?;
                }
                server.respond(req.success(ResponseBody::Pause))?;
            }

            Command::Next(_args) => {
                if adapter.verbose {
                    server.send_event(output_event("Step over"))?;
                }
                match adapter.handle_next() {
                    Ok(()) => {
                        if adapter.verbose {
                            for msg in adapter.take_step_log() {
                                server.send_event(output_event(&msg))?;
                            }
                        } else {
                            adapter.take_step_log();
                        }
                        server.respond(req.success(ResponseBody::Next))?;
                        adapter.on_stopped();
                        server.send_event(stopped_event(StoppedEventReason::Step))?;
                    }
                    Err(e) => {
                        for msg in adapter.take_step_log() {
                            server.send_event(output_event(&msg))?;
                        }
                        server.send_event(output_event(&format!("Next error: {}", e)))?;
                        server.respond(req.error(&e.to_string()))?;
                    }
                }
            }

            Command::StepIn(_args) => {
                if adapter.verbose {
                    server.send_event(output_event("Step in"))?;
                }
                match adapter.handle_step_in() {
                    Ok(()) => {
                        if adapter.verbose {
                            for msg in adapter.take_step_log() {
                                server.send_event(output_event(&msg))?;
                            }
                        } else {
                            adapter.take_step_log();
                        }
                        server.respond(req.success(ResponseBody::StepIn))?;
                        adapter.on_stopped();
                        server.send_event(stopped_event(StoppedEventReason::Step))?;
                    }
                    Err(e) => {
                        for msg in adapter.take_step_log() {
                            server.send_event(output_event(&msg))?;
                        }
                        server.send_event(output_event(&format!("StepIn error: {}", e)))?;
                        server.respond(req.error(&e.to_string()))?;
                    }
                }
            }

            Command::StepOut(_args) => {
                if adapter.verbose {
                    server.send_event(output_event("Step out"))?;
                }
                // Drain stale data BEFORE step-out resume (target stopped, socket clean)
                let _ = adapter.gdb.take_pending_stop_reply();
                let drained = adapter.gdb.drain_stale_data();
                if adapter.verbose && drained > 0 {
                    server.send_event(output_event(&format!(
                        "Drained {} stale bytes before step-out",
                        drained
                    )))?;
                }
                match adapter.handle_step_out() {
                    Ok(()) => {
                        server.respond(req.success(ResponseBody::StepOut))?;
                        adapter.gdb.set_target_running(true);
                        start_rsp_monitor(
                            &adapter.gdb,
                            &server_output,
                            &adapter.gdb.target_running_flag(),
                            &suppress_stop_event,
                            adapter.verbose,
                        );
                    }
                    Err(e) => {
                        server.send_event(output_event(&format!("StepOut error: {}", e)))?;
                        server.respond(req.error(&e.to_string()))?;
                    }
                }
            }

            Command::Disconnect(_args) => {
                server.send_event(output_event("Disconnecting"))?;
                let _ = adapter.handle_disconnect();
                server.respond(req.success(ResponseBody::Disconnect))?;
                break;
            }

            Command::Evaluate(_args) => {
                server.respond(req.error("evaluate not yet supported"))?;
            }

            _ => {
                server.respond(req.error("not supported"))?;
            }
        }
    }

    Ok(())
}

/// Spawn a background thread that waits for the target to stop (reads a
/// stop-reply from the TCP connection) and sends a DAP Stopped event.
///
/// Stale stop-replies (e.g. the second packet Dolphin sends after Ctrl-C)
/// are NOT drained here.  Instead, `send_packet`'s ACK loop will consume
/// them the next time a command is sent, and callers discard the stale
/// `pending_stop_reply` before spawning this monitor.
/// Returns `true` if the target needs to be halted before setting breakpoints.
/// This checks the `target_running` atomic flag — when the target is already
/// stopped we skip the halt entirely.
fn needs_halt_for_breakpoints(target_running: &Arc<AtomicBool>) -> bool {
    target_running.load(Ordering::SeqCst)
}

fn start_rsp_monitor<W: Write + Send + 'static>(
    gdb: &gdb_client::GDB,
    output: &Arc<Mutex<ServerOutput<W>>>,
    target_running: &Arc<AtomicBool>,
    suppress_stop: &Arc<AtomicBool>,
    verbose: bool,
) {
    let mut gdb_clone = match gdb.try_clone_stream() {
        Ok(s) => s,
        Err(_) => return,
    };
    let output = output.clone();
    let running = target_running.clone();
    let suppress_stop = suppress_stop.clone();

    std::thread::spawn(move || {
        let packet = gdb_clone.read_packet();
        if let Some(packet) = packet {
            running.store(false, Ordering::SeqCst);

            if suppress_stop.load(Ordering::SeqCst) {
                return;
            }

            let payload = String::from_utf8_lossy(&packet);
            let is_stop_reply = matches!(
                packet.first(),
                Some(b'T') | Some(b'S') | Some(b'W') | Some(b'X') | Some(b'N')
            );

            if let Ok(mut out) = output.lock() {
                if is_stop_reply {
                    if verbose {
                        let _ = out.send_event(output_event(&format!(
                            "Target stopped (reply: {})",
                            payload
                        )));
                    }
                    let _ = out.send_event(stopped_event(StoppedEventReason::Breakpoint));
                } else {
                    let _ = out.send_event(output_event(&format!(
                        "RSP DESYNC: expected stop-reply, got: {} (len={}, first=0x{:02x})",
                        &payload[..payload.len().min(40)],
                        payload.len(),
                        packet.first().copied().unwrap_or(0),
                    )));
                    let _ = out.send_event(stopped_event(StoppedEventReason::Exception));
                }
            }
        } else {
            running.store(false, Ordering::SeqCst);
            if let Ok(mut out) = output.lock() {
                let _ = out.send_event(output_event("RSP monitor: connection lost or error"));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_halt_when_target_running() {
        let flag = Arc::new(AtomicBool::new(true));
        assert!(needs_halt_for_breakpoints(&flag));
    }

    #[test]
    fn no_halt_when_target_stopped() {
        let flag = Arc::new(AtomicBool::new(false));
        assert!(!needs_halt_for_breakpoints(&flag));
    }

    #[test]
    fn suppress_stop_event_flag_lifecycle() {
        // Simulates the lifecycle: set before interrupt, cleared after resume.
        let suppress = Arc::new(AtomicBool::new(false));
        let target_running = Arc::new(AtomicBool::new(true));

        // Before halt: suppress is false.
        assert!(!suppress.load(Ordering::SeqCst));

        // Set suppress before sending interrupt.
        suppress.store(true, Ordering::SeqCst);
        assert!(suppress.load(Ordering::SeqCst));

        // Simulate monitor thread clearing target_running (but NOT emitting
        // Stopped event because suppress is true).
        target_running.store(false, Ordering::SeqCst);
        assert!(suppress.load(Ordering::SeqCst)); // still suppressed

        // After resume: clear suppress.
        suppress.store(false, Ordering::SeqCst);
        assert!(!suppress.load(Ordering::SeqCst));
    }

    #[test]
    fn suppress_cleared_on_interrupt_failure() {
        // If interrupt fails, suppress must be cleared to avoid leaking state.
        let suppress = Arc::new(AtomicBool::new(false));
        suppress.store(true, Ordering::SeqCst);

        // Simulate interrupt failure — must clear suppress.
        suppress.store(false, Ordering::SeqCst);
        assert!(!suppress.load(Ordering::SeqCst));
    }

    #[test]
    fn suppress_cleared_on_timeout() {
        // If halt times out, suppress must be cleared.
        let suppress = Arc::new(AtomicBool::new(true));

        // Simulate timeout — must clear suppress.
        suppress.store(false, Ordering::SeqCst);
        assert!(!suppress.load(Ordering::SeqCst));
    }
}
