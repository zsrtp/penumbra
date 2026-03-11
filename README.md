# penumbra

A Rust-based debugger for GameCube/Wii game mods running on [umbra-dolphin](https://github.com/zsrtp/umbra-dolphin) or [umbra-nintendont](https://github.com/zsrtp/umbra-nintendont). Connects to the GDB Remote Serial Protocol (RSP) stub exposed by these emulators to provide source-level debugging of PowerPC code.

## Features

- **GDB RSP client** — Connect over TCP to Dolphin/Nintendont's GDB stub (default port 2159)
- **Register access** — Read/write all 32 GPRs, 32 FPRs, PC, MSR, CR, LR, CTR, XER, and FPSCR
- **Memory access** — Read and write arbitrary target memory
- **Breakpoints** — Set and remove software breakpoints
- **Execution control** — Halt, continue, single-step, step-over, step-in, step-out
- **ELF/DWARF symbol resolution** — Source-level breakpoints, address-to-line mapping, local variable inspection with type-aware formatting
- **C++ name demangling** — Supports both CodeWarrior (GameCube-era) and Itanium (GNU) mangling schemes
- **Stack trace walking** — Reconstructs call stacks via PPC EABI back-chain pointers

## Modes

### GUI mode (default)

A native desktop GUI built with [egui](https://github.com/emilk/egui). Enter the target IP address, connect, and use halt/continue controls.

```sh
cargo run
```

### DAP server mode

A [Debug Adapter Protocol](https://microsoft.github.io/debug-adapter-protocol/) server over stdin/stdout for integration with VS Code and other DAP-compatible editors.

```sh
cargo run -- --dap --program <path/to/game.elf> --debug-info <path/to/debug_info.o> --target <ip:port>
```

Supports attach, source/function breakpoints, stack traces, register and local variable views, and stepping.

## Building

```sh
cargo build --release
```

On NixOS, use `nix-shell` to set up the Wayland/OpenGL libraries needed by the GUI.

## Testing

### Unit tests

Run all unit tests across both crates (no hardware needed):

```sh
cargo test --workspace
```

This runs:
- **gdb-client** — RSP packet parsing, hex encoding, stop-reply parsing
- **penumbra** — PPC instruction decoding (`ppc_step_targets`, `is_call_instruction`), DWARF source path resolution, symbol lookup, path matching

### Integration tests

Integration tests require a running Wii/Dolphin with a GDB stub. Set the following env vars and run with `--ignored`:

```sh
GDB_TEST_HOST=<ip>:<port> \
GDB_TEST_TARGET=<dolphin|nintendont> \
GDB_TEST_ELF=/path/to/game.elf \
GDB_TEST_BP_SYMBOL=fapGm_Execute__Fv \
cargo test --package gdb-client --test integration -- --ignored --test-threads=1
```

- `GDB_TEST_HOST` — target IP and port (e.g. `192.168.1.100:2159`)
- `GDB_TEST_TARGET` — `dolphin` or `nintendont`, used to skip tests that don't apply to a given stub
- `GDB_TEST_ELF` — path to the game's ELF file (used for symbol lookup)
- `GDB_TEST_BP_SYMBOL` — exact (mangled) name of a frequently-called function for breakpoint tests

`--test-threads=1` is required since all tests share a single GDB connection.

**Target-specific notes:**
- Dolphin's GDB stub is single-use — it stops listening after the first connection detaches, so `reconnect_rapid` is skipped
- Both stubs return a full register blob from `g` (Dolphin: 416 bytes, Nintendont: 412 bytes)

## Project structure

```
penumbra/               Binary crate — main application
  src/
    main.rs             Entry point and CLI arg parsing (clap)
    gui.rs              egui GUI + GDB worker thread
    dap/
      mod.rs            DAP server loop and RSP monitor thread
      adapter.rs        Bridges DAP commands to GDB operations + symbol lookups
      symbols.rs        ELF/DWARF parsing and address ↔ source mapping
gdb-client/             Library crate — GDB RSP protocol implementation
  src/lib.rs            Packet framing, register/memory ops, breakpoints, execution control
```
