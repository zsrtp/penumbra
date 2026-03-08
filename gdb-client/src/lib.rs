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
}

#[derive(Debug, Error)]
pub enum GDBError {
    #[error("GDB server disconnected")]
    Disconnected(#[from] std::io::Error),
    #[error("Response invalide {0}")]
    InvalidResponse(String),
}

#[derive(Debug, Default)]
pub enum GDBState {
    #[default]
    Disconnected,
}

#[derive(Debug)]
pub enum GDBCmd {
    Connect,
    SetSource(GDBSource),
}

impl GDB {
    pub fn new(source: GDBSource) -> Self {
        Self {
            source: Some(source),
            state: GDBState::default(),
        }
    }

    pub fn execute_cmd(&mut self, cmd: GDBCmd) -> Result<(), GDBError> {
        match cmd {
            GDBCmd::Connect => {},
            GDBCmd::SetSource(gdbsource) => {
                // TODO Check if we are disconnected first.
                self.source = Some(gdbsource);
            },
        }
        Ok(())
    }
}