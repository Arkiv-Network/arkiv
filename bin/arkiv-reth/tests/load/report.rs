//! The run's report file: written when the run starts, rewritten when it ends.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// `report.json` in the run's directory. A run that panics outside an
/// invariant check (an RPC failure, say) leaves `outcome: aborted`; one that is
/// killed leaves `outcome: running`.
pub struct ReportFile {
    path: PathBuf,
    finished: bool,
}

impl ReportFile {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            finished: false,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Replace the report, atomically: a reader never sees half of one.
    pub fn write(&self, report: &Value) {
        let partial = self.path.with_extension("json.partial");
        std::fs::write(&partial, serde_json::to_vec_pretty(report).unwrap())
            .unwrap_or_else(|e| panic!("write {}: {e}", partial.display()));
        std::fs::rename(&partial, &self.path)
            .unwrap_or_else(|e| panic!("rename to {}: {e}", self.path.display()));
    }

    /// Write the final report.
    pub fn finish(&mut self, report: &Value) {
        self.write(report);
        self.finished = true;
    }
}

impl Drop for ReportFile {
    fn drop(&mut self) {
        if self.finished || !std::thread::panicking() {
            return;
        }
        let Ok(bytes) = std::fs::read(&self.path) else {
            return;
        };
        let Ok(mut report) = serde_json::from_slice::<Value>(&bytes) else {
            return;
        };
        report["outcome"] = "aborted".into();
        report["abortedBecause"] = "the test panicked outside an invariant check; the panic \
             message is in the test's output, and the node's own account in node.log"
            .into();
        let _ = std::fs::write(&self.path, serde_json::to_vec_pretty(&report).unwrap());
    }
}
