//! IPC server for CLI/TUI/UI communication

use crate::error::Result;

pub struct IpcServer {
    // TODO: Unix domain socket JSON-RPC
}

impl IpcServer {
    pub fn new() -> Self {
        Self {}
    }

    pub async fn start(&self) -> Result<()> {
        Ok(())
    }
}

impl Default for IpcServer {
    fn default() -> Self {
        Self::new()
    }
}
