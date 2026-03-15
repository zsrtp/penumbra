mod dap;
mod gui;

use clap::Parser;

#[derive(Debug, Parser)]
struct Args {
    #[arg()]
    command: Option<String>,

    /// Run as a DAP (Debug Adapter Protocol) server on stdin/stdout.
    #[arg(long)]
    dap: bool,

    /// Path to the ELF file (e.g. build/GZ2E01/framework.elf).
    #[arg(long)]
    program: Option<String>,

    /// Path to the DWARF debug info object (e.g. build/GZ2E01/debug_info.o).
    #[arg(long)]
    debug_info: Option<String>,

    /// RSP target address as ip:port (e.g. 192.168.1.100:2159).
    #[arg(long)]
    target: Option<String>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    if args.dap {
        let program = args.program.as_deref().unwrap_or("");
        let target = args.target.as_deref().unwrap_or("127.0.0.1:2159");
        let debug_info = args.debug_info.as_deref();

        dap::run_dap_server(program, debug_info, target)?;
    } else if args.command.is_none() {
        let (snd_to_gdb, rcv_from_app) = flume::unbounded();
        let (snd_to_app, rcv_from_gdb) = flume::unbounded();
        let gdb_thread_handle =
            std::thread::spawn(move || gui::gdb_thread(rcv_from_app, snd_to_app));
        gui::run_gui(snd_to_gdb, rcv_from_gdb).expect("GUI thread panicked");
        gdb_thread_handle.join().unwrap();
    }

    Ok(())
}
