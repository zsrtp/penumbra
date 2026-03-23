//! DAP <-> GDB translation layer.
//!
//! Translates Debug Adapter Protocol requests into GDB RSP commands.

use std::collections::HashMap;
use std::path::Path;

use dap::types::*;
use gdb_client::{GDB, GDBError, GDBSource, PpcRegisters};

use super::symbols::{SymbolResolver, VarLocation};

/// Variable reference types for DAP's evaluate request.
const VAR_REF_GPR: i64 = 1;
const VAR_REF_FPR: i64 = 2;
const VAR_REF_SPECIAL: i64 = 3;
const VAR_REF_LOCALS: i64 = 4;

/// Maximum stack depth to fetch.
const MAX_STACK_DEPTH: usize = 64;

/// Main adapter state machine.
///
/// This struct bridges DAP (Debug Adapter Protocol) with GDB RSP.
/// It maintains:
/// - GDB client connection
/// - Breakpoint state
/// - Cached register values
/// - Symbol resolver for source-level debugging
pub struct DebugAdapter {
    /// The GDB client instance.
    pub gdb: GDB,
    /// Symbol resolver for source file/line to address mapping.
    symbols: Option<SymbolResolver>,
    /// Active breakpoints: address → source info.
    breakpoints: HashMap<u32, BreakpointInfo>,
    /// Cached registers, refreshed on each stop.
    cached_regs: Option<PpcRegisters>,
    /// Project root directory for resolving relative source paths.
    project_root: Option<String>,
    /// Diagnostic log accumulated during stepping operations.
    step_log: Vec<String>,
    /// Temp breakpoint from step-out that needs cleanup on next stop.
    pending_temp_bp: Option<u32>,
    /// When true, emit detailed diagnostic output to the debug console.
    pub verbose: bool,
}

struct BreakpointInfo {
    source_file: Option<String>,
}

impl DebugAdapter {
    pub fn new() -> Self {
        Self {
            gdb: GDB::default(),
            symbols: None,
            breakpoints: HashMap::new(),
            cached_regs: None,
            project_root: None,
            step_log: Vec::new(),
            pending_temp_bp: None,
            verbose: false,
        }
    }

    /// Return addresses of all active breakpoints.
    pub fn breakpoint_addrs(&self) -> Vec<u32> {
        self.breakpoints.keys().copied().collect()
    }

    pub fn handle_initialize(&self) -> Capabilities {
        Capabilities {
            supports_configuration_done_request: Some(true),
            supports_function_breakpoints: Some(true),
            supports_set_variable: Some(false),
            supports_evaluate_for_hovers: Some(false),
            supports_step_back: Some(false),
            supports_restart_request: Some(false),
            ..Default::default()
        }
    }

    /// Connect to RSP target + load symbols.
    /// `args` is the raw JSON value from the attach request.
    pub fn handle_attach(&mut self, args: &serde_json::Value) -> Result<(), String> {
        let target = args
            .get("target")
            .and_then(|v| v.as_str())
            .ok_or("missing 'target' in attach args")?;
        let program = args
            .get("program")
            .and_then(|v| v.as_str())
            .ok_or("missing 'program' in attach args")?;
        let debug_info = args.get("debugInfo").and_then(|v| v.as_str());
        self.verbose = args
            .get("verbose")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Parse target as ip:port
        let (ip_str, port_str) = target.rsplit_once(':').ok_or("target must be ip:port")?;
        let ip: std::net::IpAddr = ip_str.parse().map_err(|e| format!("bad IP: {}", e))?;
        let port: u16 = port_str.parse().map_err(|e| format!("bad port: {}", e))?;

        // Connect + query_stop_reason + negotiate in one step.
        // Nintendont defers installing the PPC exception handler (MAGIC/HALT_REQ)
        // until '?' is received, so connect_and_init() must happen before
        // setting breakpoints.
        let source = GDBSource::Network((ip, port));
        self.gdb
            .execute_cmd(gdb_client::GDBCmd::ConnectAndInit(source))
            .map_err(|e| e.to_string())?;

        // Derive project root from program path (e.g. /path/to/project/build/GZ2E01/framework.elf → /path/to/project)
        let program_path = Path::new(program);
        if let Some(build_dir) = program_path.parent().and_then(|p| p.parent()) {
            if let Some(root) = build_dir.parent() {
                self.project_root = Some(root.to_string_lossy().into_owned());
            }
        }

        // Load symbols
        let debug_info_path = debug_info.map(Path::new);
        match SymbolResolver::load(Path::new(program), debug_info_path) {
            Ok(resolver) => self.symbols = Some(resolver),
            Err(e) => eprintln!("Warning: failed to load symbols: {}", e),
        }

        Ok(())
    }

    pub fn handle_configuration_done(&mut self) -> Result<(), GDBError> {
        // Resume the target after configuration is complete
        self.gdb.resume()
    }

    /// Set source breakpoints for a single file. Replaces all previous breakpoints
    /// for that file (DAP semantics).
    pub fn handle_set_breakpoints(
        &mut self,
        source: &Source,
        source_breakpoints: &[SourceBreakpoint],
    ) -> Vec<Breakpoint> {
        let file = source
            .path
            .as_deref()
            .or(source.name.as_deref())
            .unwrap_or("");

        // Remove old breakpoints for this file
        let old_addrs: Vec<u32> = self
            .breakpoints
            .iter()
            .filter(|(_, info)| info.source_file.as_deref() == Some(file))
            .map(|(addr, _)| *addr)
            .collect();
        for addr in &old_addrs {
            let _ = self.gdb.remove_breakpoint(*addr);
            self.breakpoints.remove(addr);
        }

        // Set new breakpoints
        let mut results = Vec::new();
        for sbp in source_breakpoints {
            let line = sbp.line as u32;
            let resolved = self
                .symbols
                .as_ref()
                .and_then(|s| s.location_to_addr(file, line));

            if let Some(addr) = resolved {
                let ok = self.gdb.set_breakpoint(addr).is_ok();
                if ok {
                    self.breakpoints.insert(
                        addr,
                        BreakpointInfo {
                            source_file: Some(file.to_string()),
                        },
                    );
                }
                results.push(Breakpoint {
                    verified: ok,
                    line: Some(sbp.line),
                    source: Some(source.clone()),
                    message: if ok {
                        None
                    } else {
                        Some("failed to set breakpoint on target".to_string())
                    },
                    ..Default::default()
                });
            } else {
                results.push(Breakpoint {
                    verified: false,
                    line: Some(sbp.line),
                    source: Some(source.clone()),
                    message: Some("could not resolve line to address".to_string()),
                    ..Default::default()
                });
            }
        }
        results
    }

    pub fn handle_set_function_breakpoints(
        &mut self,
        fn_breakpoints: &[FunctionBreakpoint],
    ) -> Vec<Breakpoint> {
        // Remove old function breakpoints (those without source_file)
        let old_addrs: Vec<u32> = self
            .breakpoints
            .iter()
            .filter(|(_, info)| info.source_file.is_none())
            .map(|(addr, _)| *addr)
            .collect();
        for addr in &old_addrs {
            let _ = self.gdb.remove_breakpoint(*addr);
            self.breakpoints.remove(addr);
        }

        let mut results = Vec::new();
        for fbp in fn_breakpoints {
            let resolved = self
                .symbols
                .as_ref()
                .and_then(|s| s.function_to_addr(&fbp.name));

            if let Some(addr) = resolved {
                let ok = self.gdb.set_breakpoint(addr).is_ok();
                if ok {
                    self.breakpoints
                        .insert(addr, BreakpointInfo { source_file: None });
                }
                results.push(Breakpoint {
                    verified: ok,
                    message: if ok {
                        Some(format!("{} @ 0x{:08x}", fbp.name, addr))
                    } else {
                        Some("failed to set breakpoint on target".to_string())
                    },
                    ..Default::default()
                });
            } else {
                results.push(Breakpoint {
                    verified: false,
                    message: Some(format!("unknown function: {}", fbp.name)),
                    ..Default::default()
                });
            }
        }
        results
    }

    pub fn handle_continue(&mut self) -> Result<(), GDBError> {
        self.cached_regs = None;
        self.gdb.resume()
    }

    /// Step a single machine instruction using breakpoints instead of the GDB `s`
    /// command, which is unreliable on some stubs (Dolphin treats `s` as `c`
    /// intermittently). We decode the current instruction to find all possible
    /// next-PC values, set temp breakpoints, continue, and remove them.
    fn step_one_instruction(&mut self) -> Result<(), GDBError> {
        let regs = self.ensure_registers()?;
        let pc = regs.pc;
        let lr = regs.lr;
        let ctr = regs.ctr;

        // Read current instruction
        let insn_data = self.gdb.read_memory(pc, 4)?;
        if insn_data.len() < 4 {
            return Err(GDBError::InvalidResponse(
                "short memory read for insn".into(),
            ));
        }
        let insn = u32::from_be_bytes([insn_data[0], insn_data[1], insn_data[2], insn_data[3]]);

        // Compute all possible next-PC addresses
        let targets = ppc_step_targets(pc, insn, lr, ctr);

        // Remove any breakpoint at the current PC before continuing.
        let had_user_bp = self.breakpoints.contains_key(&pc);
        let had_temp_bp = self.pending_temp_bp == Some(pc);
        if had_user_bp || had_temp_bp {
            let _ = self.gdb.remove_breakpoint(pc);
        }
        if had_temp_bp {
            self.pending_temp_bp = None;
        }

        // Set temp breakpoints at all possible targets (skip if user BP already there)
        let mut temp_bps: Vec<u32> = Vec::new();
        for &t in &targets {
            if !self.breakpoints.contains_key(&t) {
                if self.gdb.set_breakpoint(t).is_ok() {
                    temp_bps.push(t);
                }
            }
        }

        // Drain any stale pending_stop_reply
        let _ = self.gdb.take_pending_stop_reply();

        self.cached_regs = None;
        let reply = self.gdb.continue_and_wait()?;

        // Remove temp breakpoints
        for &t in &temp_bps {
            let _ = self.gdb.remove_breakpoint(t);
        }

        // Re-add user BP at original PC
        if had_user_bp {
            let _ = self.gdb.set_breakpoint(pc);
        }

        self.refresh_registers()?;
        let final_pc = self.cached_regs.as_ref().map(|r| r.pc).unwrap_or(0);

        self.step_log.push(format!(
            "  [step1] from=0x{:08x} insn=0x{:08x} targets={:x?} final_pc=0x{:08x} sig={}",
            pc, insn, targets, final_pc, reply.signal
        ));

        Ok(())
    }

    pub fn handle_step_in(&mut self) -> Result<(), GDBError> {
        self.step_log.clear();
        let start_loc = self.current_source_line();
        let start_pc = self.cached_regs.as_ref().map(|r| r.pc);
        self.step_log.push(format!(
            "[step-in] start: pc=0x{:08x} loc={:?}",
            start_pc.unwrap_or(0),
            start_loc
        ));

        self.step_one_instruction()?;

        // If no source info, single instruction step is enough
        let start_loc = match start_loc {
            Some(loc) => loc,
            None => return Ok(()),
        };

        // Keep stepping until the source line changes or we lose source info
        for i in 0..10000 {
            let cur_pc = self.cached_regs.as_ref().map(|r| r.pc).unwrap_or(0);
            let cur_loc = self.current_source_line();
            self.step_log.push(format!(
                "[step-in] iter {}: pc=0x{:08x} loc={:?}",
                i, cur_pc, cur_loc
            ));
            match cur_loc {
                Some(ref loc) if *loc == start_loc => {}
                _ => {
                    self.step_log.push(format!("[step-in] break: line changed"));
                    break;
                }
            }
            self.step_one_instruction()?;
        }

        Ok(())
    }

    pub fn handle_next(&mut self) -> Result<(), GDBError> {
        self.step_log.clear();
        let start_loc = self.current_source_line();
        let start_pc = self.cached_regs.as_ref().map(|r| r.pc);
        self.step_log.push(format!(
            "[next] start: pc=0x{:08x} loc={:?}",
            start_pc.unwrap_or(0),
            start_loc
        ));

        let start_loc = match start_loc {
            Some(loc) => loc,
            None => {
                self.step_one_instruction()?;
                return Ok(());
            }
        };

        for i in 0..10000 {
            let regs = self.ensure_registers()?;
            let pc = regs.pc;

            let is_call = self.is_call_at(pc)?;
            self.step_log.push(format!(
                "[next] iter {}: pc=0x{:08x} is_call={}",
                i, pc, is_call
            ));

            if is_call {
                self.step_over_call(pc)?;
            } else {
                self.step_one_instruction()?;
            }

            let new_pc = self.cached_regs.as_ref().map(|r| r.pc).unwrap_or(0);
            let cur_loc = self.current_source_line();
            self.step_log
                .push(format!("[next]   -> pc=0x{:08x} loc={:?}", new_pc, cur_loc));

            match cur_loc {
                Some(ref loc) if *loc == start_loc => {}
                _ => {
                    self.step_log.push(format!("[next] break: line changed"));
                    break;
                }
            }
            if new_pc == pc {
                self.step_log.push(format!("[next] break: pc stuck"));
                break;
            }
        }

        Ok(())
    }

    /// Check if the instruction at `pc` is a call (branch-and-link).
    fn is_call_at(&mut self, pc: u32) -> Result<bool, GDBError> {
        let data = self.gdb.read_memory(pc, 4)?;
        if data.len() < 4 {
            return Ok(false);
        }
        let insn = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        Ok(is_call_instruction(insn))
    }

    /// Step over a call instruction: set temp breakpoint at PC+4, continue,
    /// then remove the temp breakpoint.
    fn step_over_call(&mut self, pc: u32) -> Result<(), GDBError> {
        let return_addr = pc + 4;

        let had_bp = self.breakpoints.contains_key(&pc);
        if had_bp {
            let _ = self.gdb.remove_breakpoint(pc);
        }

        let is_user_bp = self.breakpoints.contains_key(&return_addr);
        if !is_user_bp {
            self.gdb.set_breakpoint(return_addr)?;
        }

        // Drain stale pending_stop_reply before continuing
        let had_pending = self.gdb.take_pending_stop_reply().is_some();

        self.cached_regs = None;
        let reply = self.gdb.continue_and_wait()?;

        if !is_user_bp {
            let _ = self.gdb.remove_breakpoint(return_addr);
        }
        if had_bp {
            let _ = self.gdb.set_breakpoint(pc);
        }

        self.refresh_registers()?;
        let final_pc = self.cached_regs.as_ref().map(|r| r.pc).unwrap_or(0);
        self.step_log.push(format!(
            "  [step_over] from=0x{:08x} ret=0x{:08x} final_pc=0x{:08x} sig={} had_pending={}",
            pc, return_addr, final_pc, reply.signal, had_pending
        ));

        Ok(())
    }

    fn current_source_line(&self) -> Option<(String, u32)> {
        let regs = self.cached_regs.as_ref()?;
        self.symbols.as_ref()?.addr_to_location(regs.pc)
    }

    /// Drain accumulated step diagnostic messages.
    pub fn take_step_log(&mut self) -> Vec<String> {
        std::mem::take(&mut self.step_log)
    }

    pub fn handle_step_out(&mut self) -> Result<(), GDBError> {
        let regs = self.ensure_registers()?;
        let lr = regs.lr;
        // Only set temp BP if there isn't already a user BP at LR
        if !self.breakpoints.contains_key(&lr) {
            self.gdb.set_breakpoint(lr)?;
            self.pending_temp_bp = Some(lr);
        }
        self.cached_regs = None;
        self.gdb.resume()?;
        Ok(())
    }

    pub fn handle_threads(&self) -> Vec<Thread> {
        vec![Thread {
            id: 1,
            name: "PPC".to_string(),
        }]
    }

    pub fn handle_stack_trace(&mut self) -> Result<Vec<StackFrame>, GDBError> {
        let regs = self.ensure_registers()?;
        let pc = regs.pc;
        let lr = regs.lr;
        let sp = regs.gpr[1];

        let mut frames = Vec::new();

        // Frame 0: current PC
        let (source, line, name) = self.resolve_frame(pc);
        frames.push(StackFrame {
            id: 0,
            name: name.unwrap_or_else(|| format!("0x{:08x}", pc)),
            source,
            line: line.map(|l| l as i64).unwrap_or(0),
            column: 0,
            ..Default::default()
        });

        // Frame 1: LR (return address back to caller)
        let call_addr = lr.wrapping_sub(4);
        let (source, line, name) = self.resolve_frame(call_addr);
        frames.push(StackFrame {
            id: 1,
            name: name.unwrap_or_else(|| format!("0x{:08x}", call_addr)),
            source,
            line: line.map(|l| l as i64).unwrap_or(0),
            column: 0,
            ..Default::default()
        });

        // Walk the stack using back chain pointers.
        // PPC EABI: [SP+0] = back chain (caller's SP).
        // The saved LR at [back_chain+4] is the SAME as the LR register
        // (the current function saved it there in its prologue), so we
        // must follow TWO back chain links to reach the next NEW frame.
        //
        // Starting point: follow SP → caller_sp → grandparent_sp.
        // Then [grandparent_sp+4] = caller's saved LR = frame 2.
        let mut current_sp = sp;

        // First back chain: SP → caller's SP (skip saved LR here, it's the LR register)
        match self.gdb.read_memory(current_sp, 4) {
            Ok(data) if data.len() >= 4 => {
                current_sp = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
                self.step_log.push(format!(
                    "[bt] back chain: [0x{:08x}] → 0x{:08x}",
                    sp, current_sp
                ));
            }
            Err(e) => {
                self.step_log.push(format!(
                    "[bt] FAILED read back chain at 0x{:08x}: {}",
                    current_sp, e
                ));
                return Ok(frames);
            }
            _ => {
                self.step_log
                    .push("[bt] short read for back chain".to_string());
                return Ok(frames);
            }
        }
        if current_sp == 0 {
            self.step_log.push("[bt] back chain is NULL".to_string());
            return Ok(frames);
        }

        for i in 2..MAX_STACK_DEPTH {
            // Follow back chain to next frame
            let prev_sp = match self.gdb.read_memory(current_sp, 4) {
                Ok(data) if data.len() >= 4 => {
                    u32::from_be_bytes([data[0], data[1], data[2], data[3]])
                }
                Err(e) => {
                    self.step_log.push(format!(
                        "[bt] FAILED read back chain at 0x{:08x}: {}",
                        current_sp, e
                    ));
                    break;
                }
                _ => {
                    self.step_log
                        .push(format!("[bt] short read at 0x{:08x}", current_sp));
                    break;
                }
            };
            if prev_sp == 0 {
                self.step_log
                    .push(format!("[bt] end: [0x{:08x}] = 0", current_sp));
                break;
            }
            if prev_sp == current_sp {
                self.step_log
                    .push(format!("[bt] stuck: [0x{:08x}] = same", current_sp));
                break;
            }

            // Read saved LR from prev_sp's LR save area.
            // [prev_sp+4] = LR saved by the function whose SP is current_sp.
            let saved_lr = match self.gdb.read_memory(prev_sp + 4, 4) {
                Ok(data) if data.len() >= 4 => {
                    u32::from_be_bytes([data[0], data[1], data[2], data[3]])
                }
                Err(e) => {
                    self.step_log.push(format!(
                        "[bt] FAILED read LR at 0x{:08x}: {}",
                        prev_sp + 4,
                        e
                    ));
                    break;
                }
                _ => break,
            };

            self.step_log.push(format!(
                "[bt] frame {}: sp=0x{:08x} → prev_sp=0x{:08x} saved_lr=0x{:08x}",
                i, current_sp, prev_sp, saved_lr
            ));

            let call_addr = saved_lr.wrapping_sub(4);
            let (source, line, name) = self.resolve_frame(call_addr);
            frames.push(StackFrame {
                id: i as i64,
                name: name.unwrap_or_else(|| format!("0x{:08x}", call_addr)),
                source,
                line: line.map(|l| l as i64).unwrap_or(0),
                column: 0,
                ..Default::default()
            });

            current_sp = prev_sp;
        }

        Ok(frames)
    }

    pub fn handle_scopes(&self, _frame_id: i64) -> Vec<Scope> {
        vec![
            Scope {
                name: "Locals".to_string(),
                variables_reference: VAR_REF_LOCALS,
                expensive: false,
                ..Default::default()
            },
            Scope {
                name: "General Purpose Registers".to_string(),
                variables_reference: VAR_REF_GPR,
                expensive: false,
                ..Default::default()
            },
            Scope {
                name: "Floating Point Registers".to_string(),
                variables_reference: VAR_REF_FPR,
                expensive: false,
                ..Default::default()
            },
            Scope {
                name: "Special Registers".to_string(),
                variables_reference: VAR_REF_SPECIAL,
                expensive: false,
                ..Default::default()
            },
        ]
    }

    pub fn handle_variables(&mut self, var_ref: i64) -> Result<Vec<Variable>, GDBError> {
        let regs = self.ensure_registers()?;

        match var_ref {
            VAR_REF_GPR => {
                let vars: Vec<Variable> = (0..32)
                    .map(|i| Variable {
                        name: format!("r{}", i),
                        value: format!("0x{:08x}", regs.gpr[i]),
                        variables_reference: 0,
                        ..Default::default()
                    })
                    .collect();
                Ok(vars)
            }
            VAR_REF_FPR => {
                let vars: Vec<Variable> = (0..32)
                    .map(|i| Variable {
                        name: format!("f{}", i),
                        value: format!("{}", f64::from_bits(regs.fpr[i])),
                        variables_reference: 0,
                        ..Default::default()
                    })
                    .collect();
                Ok(vars)
            }
            VAR_REF_SPECIAL => {
                let vars = vec![
                    Variable {
                        name: "PC".to_string(),
                        value: format!("0x{:08x}", regs.pc),
                        variables_reference: 0,
                        ..Default::default()
                    },
                    Variable {
                        name: "LR".to_string(),
                        value: format!("0x{:08x}", regs.lr),
                        variables_reference: 0,
                        ..Default::default()
                    },
                    Variable {
                        name: "CR".to_string(),
                        value: format!("0x{:08x}", regs.cr),
                        variables_reference: 0,
                        ..Default::default()
                    },
                    Variable {
                        name: "CTR".to_string(),
                        value: format!("0x{:08x}", regs.ctr),
                        variables_reference: 0,
                        ..Default::default()
                    },
                    Variable {
                        name: "XER".to_string(),
                        value: format!("0x{:08x}", regs.xer),
                        variables_reference: 0,
                        ..Default::default()
                    },
                    Variable {
                        name: "MSR".to_string(),
                        value: format!("0x{:08x}", regs.msr),
                        variables_reference: 0,
                        ..Default::default()
                    },
                    Variable {
                        name: "FPSCR".to_string(),
                        value: format!("0x{:016x}", regs.fpscr),
                        variables_reference: 0,
                        ..Default::default()
                    },
                ];
                Ok(vars)
            }
            VAR_REF_LOCALS => {
                let pc = regs.pc;
                let sp = regs.gpr[1];

                let var_infos = self
                    .symbols
                    .as_ref()
                    .map(|s| s.find_variables(pc))
                    .unwrap_or_default();

                let mut vars = Vec::new();
                for info in &var_infos {
                    let value = match &info.location {
                        VarLocation::Register(n) => {
                            let n = *n as usize;
                            if n >= 32 && n < 64 {
                                // FPR
                                let fpr_raw = regs.fpr[n - 32];
                                format_float_value(fpr_raw, info.var_type.byte_size)
                            } else if n < 32 {
                                // GPR
                                format_var_value(regs.gpr[n] as u64, &info.var_type)
                            } else {
                                "<unsupported register>".to_string()
                            }
                        }
                        VarLocation::StackOffset(off) => {
                            let addr = (sp as i64 + off) as u32;
                            let size = info.var_type.byte_size as usize;
                            match self.gdb.read_memory(addr, size) {
                                Ok(data) if data.len() >= size => {
                                    let raw = read_be_value(&data, info.var_type.byte_size);
                                    if info.var_type.encoding == gimli::DW_ATE_float {
                                        format_float_value(raw, info.var_type.byte_size)
                                    } else {
                                        format_var_value(raw, &info.var_type)
                                    }
                                }
                                _ => "<read error>".to_string(),
                            }
                        }
                        VarLocation::Unknown => "<optimized out>".to_string(),
                    };
                    vars.push(Variable {
                        name: info.name.clone(),
                        value,
                        type_field: Some(info.var_type.name.clone()),
                        variables_reference: 0,
                        ..Default::default()
                    });
                }
                Ok(vars)
            }
            _ => Ok(Vec::new()),
        }
    }

    pub fn handle_disconnect(&mut self) -> Result<(), GDBError> {
        self.cached_regs = None;
        self.breakpoints.clear();
        self.gdb.detach()
    }

    /// Called by the server loop when a stop event is received from the RSP monitor.
    pub fn on_stopped(&mut self) {
        // Clean up temp breakpoint from step-out
        if let Some(addr) = self.pending_temp_bp.take() {
            let _ = self.gdb.remove_breakpoint(addr);
        }
        let _ = self.refresh_registers();
    }

    /// Refresh registers and return a log message about the result.
    pub fn refresh_registers_logged(&mut self) -> Result<String, GDBError> {
        match self.refresh_registers() {
            Ok(()) => {
                if let Some(regs) = &self.cached_regs {
                    Ok(format!(
                        "Registers (g={} bytes): PC=0x{:08x} LR=0x{:08x} SP=0x{:08x}",
                        regs.reg_blob_size, regs.pc, regs.lr, regs.gpr[1]
                    ))
                } else {
                    Ok("Registers refreshed (empty)".to_string())
                }
            }
            Err(e) => Err(e),
        }
    }

    fn refresh_registers(&mut self) -> Result<(), GDBError> {
        match self.gdb.read_registers() {
            Ok(regs) => {
                self.cached_regs = Some(regs);
                Ok(())
            }
            Err(e) => {
                self.cached_regs = None;
                Err(e)
            }
        }
    }

    fn ensure_registers(&mut self) -> Result<PpcRegisters, GDBError> {
        if self.cached_regs.is_none() {
            self.refresh_registers()?;
        }
        self.cached_regs
            .clone()
            .ok_or(GDBError::InvalidResponse("failed to read registers".into()))
    }

    fn resolve_frame(&self, addr: u32) -> (Option<Source>, Option<u32>, Option<String>) {
        let symbols = match &self.symbols {
            Some(s) => s,
            None => {
                if self.verbose {
                    eprintln!("[resolve_frame] 0x{:08x}: no symbols loaded", addr);
                }
                return (None, None, None);
            }
        };

        let name = symbols.addr_to_function(addr).map(|s| demangle(s));
        let loc = symbols.addr_to_location(addr);
        if self.verbose {
            eprintln!(
                "[resolve_frame] 0x{:08x}: func={:?} loc={:?}",
                addr, name, loc
            );
        }
        let (source, line) = match loc {
            Some((file, line)) => {
                let abs_path = self.resolve_source_path(&file);
                let source = Source {
                    name: Some(
                        file.rsplit_once('/')
                            .map(|(_, f)| f)
                            .unwrap_or(&file)
                            .to_string(),
                    ),
                    path: Some(abs_path),
                    ..Default::default()
                };
                (Some(source), Some(line))
            }
            None => (None, None),
        };

        (source, line, name)
    }

    /// Resolve a DWARF source path to an absolute filesystem path.
    fn resolve_source_path(&self, dwarf_path: &str) -> String {
        if dwarf_path.starts_with('/') {
            return dwarf_path.to_string();
        }
        let root = match &self.project_root {
            Some(r) => r,
            None => return dwarf_path.to_string(),
        };

        let joined = format!("{}/{}", root, dwarf_path);
        // Normalize to OS path separators so VS Code can match to filesystem files
        if cfg!(windows) {
            joined.replace('/', "\\")
        } else {
            joined
        }
    }
}

fn format_var_value(raw: u64, var_type: &super::symbols::VarType) -> String {
    match var_type.encoding {
        e if e == gimli::DW_ATE_signed || e == gimli::DW_ATE_signed_char => {
            match var_type.byte_size {
                1 => format!("{}", raw as i8),
                2 => format!("{}", raw as i16),
                4 => format!("{}", raw as i32),
                _ => format!("0x{:x}", raw),
            }
        }
        e if e == gimli::DW_ATE_float => format_float_value(raw, var_type.byte_size),
        e if e == gimli::DW_ATE_unsigned_char => {
            let ch = raw as u8;
            if ch.is_ascii_graphic() || ch == b' ' {
                format!("'{}' (0x{:02x})", ch as char, ch)
            } else {
                format!("0x{:02x}", ch)
            }
        }
        _ => match var_type.byte_size {
            1 => format!("0x{:02x}", raw as u8),
            2 => format!("0x{:04x}", raw as u16),
            _ => format!("0x{:08x}", raw as u32),
        },
    }
}

fn format_float_value(raw: u64, byte_size: u8) -> String {
    match byte_size {
        4 => format!("{}", f32::from_bits(raw as u32)),
        8 => format!("{}", f64::from_bits(raw)),
        _ => format!("0x{:x}", raw),
    }
}

fn read_be_value(data: &[u8], byte_size: u8) -> u64 {
    match byte_size {
        1 => data[0] as u64,
        2 => u16::from_be_bytes([data[0], data[1]]) as u64,
        4 => u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as u64,
        8 if data.len() >= 8 => u64::from_be_bytes([
            data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
        ]),
        _ => {
            let mut val = 0u64;
            for &b in data.iter().take(byte_size as usize) {
                val = (val << 8) | b as u64;
            }
            val
        }
    }
}

/// Compute all possible next-PC addresses for a PPC instruction.
/// Returns 1 address for non-branch/unconditional, 2 for conditional branches.
pub(crate) fn ppc_step_targets(pc: u32, insn: u32, lr: u32, ctr: u32) -> Vec<u32> {
    let opcode = insn >> 26;
    let aa = (insn >> 1) & 1; // Absolute address bit

    match opcode {
        // Opcode 18: b/bl (unconditional branch)
        18 => {
            let mut li = insn & 0x03FF_FFFC;
            // Sign-extend 26-bit value
            if li & 0x0200_0000 != 0 {
                li |= 0xFC00_0000;
            }
            let target = if aa != 0 { li } else { pc.wrapping_add(li) };
            vec![target]
        }

        // Opcode 16: bc/bcl (conditional branch)
        16 => {
            let bo = (insn >> 21) & 0x1F;
            let mut bd = insn & 0x0000_FFFC;
            // Sign-extend 16-bit value
            if bd & 0x0000_8000 != 0 {
                bd |= 0xFFFF_0000;
            }
            let target = if aa != 0 { bd } else { pc.wrapping_add(bd) };
            // BO=20 (0b10100) means "always" — unconditional
            if bo & 0x14 == 0x14 {
                vec![target]
            } else {
                vec![target, pc + 4]
            }
        }

        // Opcode 19: bclr/bcctr and variants
        19 => {
            let xo = (insn >> 1) & 0x3FF;
            let bo = (insn >> 21) & 0x1F;
            match xo {
                // bclr/bclrl (branch to LR)
                16 => {
                    if bo & 0x14 == 0x14 {
                        vec![lr & !3] // unconditional
                    } else {
                        vec![lr & !3, pc + 4]
                    }
                }
                // bcctr/bcctrl (branch to CTR)
                528 => {
                    if bo & 0x14 == 0x14 {
                        vec![ctr & !3]
                    } else {
                        vec![ctr & !3, pc + 4]
                    }
                }
                _ => vec![pc + 4],
            }
        }

        // All other instructions: sequential
        _ => vec![pc + 4],
    }
}

/// Check if a PPC instruction is a call (any branch-and-link variant).
pub(crate) fn is_call_instruction(insn: u32) -> bool {
    // bl (branch and link): opcode 18, LK=1
    if insn & 0xFC000001 == 0x48000001 {
        return true;
    }
    // bcl (branch conditional and link): opcode 16, LK=1
    if insn & 0xFC000001 == 0x40000001 {
        return true;
    }
    // bcctrl (branch to CTR and link): opcode 19, XO=528, LK=1
    if insn & 0xFC0007FF == 0x4C000421 {
        return true;
    }
    // bclrl (branch to LR and link): opcode 19, XO=16, LK=1
    if insn & 0xFC0007FF == 0x4C000021 {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── ppc_step_targets ────────────────────────────────────────────────

    #[test]
    fn step_non_branch_is_sequential() {
        // addi r3, r3, 1
        let insn: u32 = 0x38630001;
        assert_eq!(ppc_step_targets(0x80001000, insn, 0, 0), vec![0x80001004]);
    }

    #[test]
    fn step_b_forward() {
        // b +0x100 (opcode 18, AA=0, LK=0)
        let insn: u32 = 0x48000100;
        assert_eq!(ppc_step_targets(0x80001000, insn, 0, 0), vec![0x80001100]);
    }

    #[test]
    fn step_b_backward() {
        // b -0x100 (sign-extended negative offset)
        let li: u32 = (-0x100i32 as u32) & 0x03FF_FFFC;
        let insn: u32 = (18 << 26) | li;
        assert_eq!(ppc_step_targets(0x80001000, insn, 0, 0), vec![0x80000F00]);
    }

    #[test]
    fn step_bl_is_still_single_target() {
        // bl +0x200 (opcode 18, LK=1 — call, but still unconditional)
        let insn: u32 = 0x48000201;
        assert_eq!(ppc_step_targets(0x80001000, insn, 0, 0), vec![0x80001200]);
    }

    #[test]
    fn step_b_absolute() {
        // ba 0x3000 (opcode 18, AA=1, LI=0x3000)
        let insn: u32 = (18 << 26) | 0x3000 | 0b10;
        assert_eq!(ppc_step_targets(0x80001000, insn, 0, 0), vec![0x00003000]);
    }

    #[test]
    fn step_bc_conditional_two_targets() {
        // bc 4,0, +0x20 (BO=4, not "always" — conditional)
        let insn: u32 = (16 << 26) | (4 << 21) | 0x0020;
        assert_eq!(
            ppc_step_targets(0x80001000, insn, 0, 0),
            vec![0x80001020, 0x80001004]
        );
    }

    #[test]
    fn step_bc_unconditional_one_target() {
        // bc 20,0, +0x20 (BO=20=0b10100, "always" — unconditional)
        let insn: u32 = (16 << 26) | (20 << 21) | 0x0020;
        assert_eq!(ppc_step_targets(0x80001000, insn, 0, 0), vec![0x80001020]);
    }

    #[test]
    fn step_bc_backward() {
        // bc 4,0, -0x40 (conditional, negative offset)
        let bd: u32 = (-0x40i32 as u32) & 0x0000_FFFC;
        let insn: u32 = (16 << 26) | (4 << 21) | bd;
        assert_eq!(
            ppc_step_targets(0x80001000, insn, 0, 0),
            vec![0x80000FC0, 0x80001004]
        );
    }

    #[test]
    fn step_bclr_unconditional() {
        // blr (bclr 20,0 — BO=20, XO=16)
        let insn: u32 = (19 << 26) | (20 << 21) | (16 << 1);
        let lr = 0x80002000u32;
        assert_eq!(ppc_step_targets(0x80001000, insn, lr, 0), vec![0x80002000]);
    }

    #[test]
    fn step_bclr_conditional() {
        // bclr 4,0 (conditional return)
        let insn: u32 = (19 << 26) | (4 << 21) | (16 << 1);
        let lr = 0x80002000u32;
        assert_eq!(
            ppc_step_targets(0x80001000, insn, lr, 0),
            vec![0x80002000, 0x80001004]
        );
    }

    #[test]
    fn step_bclr_aligns_lr() {
        // blr with misaligned LR — should mask low 2 bits
        let insn: u32 = (19 << 26) | (20 << 21) | (16 << 1);
        let lr = 0x80002003u32;
        assert_eq!(ppc_step_targets(0x80001000, insn, lr, 0), vec![0x80002000]);
    }

    #[test]
    fn step_bcctr_unconditional() {
        // bctr (bcctr 20,0 — BO=20, XO=528)
        let insn: u32 = (19 << 26) | (20 << 21) | (528 << 1);
        let ctr = 0x80003000u32;
        assert_eq!(ppc_step_targets(0x80001000, insn, 0, ctr), vec![0x80003000]);
    }

    #[test]
    fn step_bcctr_conditional() {
        // bcctr 4,0 (conditional branch to CTR)
        let insn: u32 = (19 << 26) | (4 << 21) | (528 << 1);
        let ctr = 0x80003000u32;
        assert_eq!(
            ppc_step_targets(0x80001000, insn, 0, ctr),
            vec![0x80003000, 0x80001004]
        );
    }

    #[test]
    fn step_opcode19_unknown_xo() {
        // opcode 19 with unrecognized XO — should fall through to pc+4
        let insn: u32 = (19 << 26) | (20 << 21) | (999 << 1);
        assert_eq!(ppc_step_targets(0x80001000, insn, 0, 0), vec![0x80001004]);
    }

    // ── is_call_instruction ─────────────────────────────────────────────

    #[test]
    fn bl_is_call() {
        // bl +0x100 (opcode 18, LK=1)
        assert!(is_call_instruction(0x48000101));
    }

    #[test]
    fn b_is_not_call() {
        // b +0x100 (opcode 18, LK=0)
        assert!(!is_call_instruction(0x48000100));
    }

    #[test]
    fn bcl_is_call() {
        // bcl 20,0, +0x20 (opcode 16, LK=1)
        let insn: u32 = (16 << 26) | (20 << 21) | 0x0021;
        assert!(is_call_instruction(insn));
    }

    #[test]
    fn bc_is_not_call() {
        // bc 20,0, +0x20 (opcode 16, LK=0)
        let insn: u32 = (16 << 26) | (20 << 21) | 0x0020;
        assert!(!is_call_instruction(insn));
    }

    #[test]
    fn bcctrl_is_call() {
        // bcctrl 20,0 (opcode 19, XO=528, LK=1)
        assert!(is_call_instruction(0x4E800421));
    }

    #[test]
    fn bcctr_is_not_call() {
        // bcctr 20,0 (opcode 19, XO=528, LK=0)
        assert!(!is_call_instruction(0x4E800420));
    }

    #[test]
    fn bclrl_is_call() {
        // bclrl 20,0 (opcode 19, XO=16, LK=1)
        assert!(is_call_instruction(0x4E800021));
    }

    #[test]
    fn blr_is_not_call() {
        // blr (bclr 20,0 — LK=0)
        assert!(!is_call_instruction(0x4E800020));
    }

    #[test]
    fn non_branch_is_not_call() {
        // addi r3, r3, 1
        assert!(!is_call_instruction(0x38630001));
        // nop (ori r0,r0,0)
        assert!(!is_call_instruction(0x60000000));
    }

    // ── resolve_source_path ─────────────────────────────────────────────

    fn adapter_with_root(root: &str) -> DebugAdapter {
        let mut a = DebugAdapter::new();
        a.project_root = Some(root.to_string());
        a
    }

    #[test]
    fn resolve_absolute_path_unchanged() {
        let a = adapter_with_root("/project");
        assert_eq!(
            a.resolve_source_path("/usr/include/stdio.h"),
            "/usr/include/stdio.h"
        );
    }

    #[test]
    fn resolve_no_root_returns_as_is() {
        let a = DebugAdapter::new();
        assert_eq!(
            a.resolve_source_path("src/JSystem/JFWDisplay.cpp"),
            "src/JSystem/JFWDisplay.cpp"
        );
    }

    #[test]
    fn resolve_relative_prepends_root() {
        let a = adapter_with_root("/home/user/project");
        assert_eq!(
            a.resolve_source_path("src/JSystem/JFramework/JFWDisplay.cpp"),
            "/home/user/project/src/JSystem/JFramework/JFWDisplay.cpp"
        );
    }

    #[test]
    fn resolve_include_prepends_root() {
        let a = adapter_with_root("/proj");
        assert_eq!(
            a.resolve_source_path("include/header.h"),
            "/proj/include/header.h"
        );
    }
}

fn demangle(name: &str) -> String {
    // Try MW/CodeWarrior demangling first (GameCube/Wii), then Itanium
    cwdemangle::demangle(name, &cwdemangle::DemangleOptions::default())
        .or_else(|| cpp_demangle::Symbol::new(name).map(|s| s.to_string()).ok())
        .unwrap_or_else(|| name.to_string())
}
