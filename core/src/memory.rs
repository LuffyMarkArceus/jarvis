//! Memory system - episodic, semantic, procedural

use crate::error::Result;

pub struct MemoryManager {
    // TODO: SQLite + sqlite-vec
}

impl MemoryManager {
    pub fn new() -> Self {
        Self {}
    }

    pub async fn init(&self) -> Result<()> {
        Ok(())
    }
}

impl Default for MemoryManager {
    fn default() -> Self {
        Self::new()
    }
}
