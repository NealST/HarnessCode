//! [`JsonLinesSink`] — appends one JSON line per span to `.harnesscode/runs.jsonl`
//! in the project working directory.

use super::{Span, SpanSink};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use tracing::warn;

/// Appends a JSON line for every span to `<project_dir>/.harnesscode/runs.jsonl`.
///
/// The file is created on first write; the directory is created if absent.
/// Each line is a self-contained JSON object — the file is valid JSONL.
///
/// ```rust,ignore
/// let sink = JsonLinesSink::open(std::env::current_dir()?)?;
/// ```
pub struct JsonLinesSink {
    writer: Mutex<Option<BufWriter<std::fs::File>>>,
    path: PathBuf,
}

impl JsonLinesSink {
    /// Open (or create) the `runs.jsonl` file under `project_dir/.harnesscode/`.
    pub fn open(project_dir: impl Into<PathBuf>) -> std::io::Result<Self> {
        let dir = project_dir.into().join(".harnesscode");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("runs.jsonl");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        Ok(Self {
            writer: Mutex::new(Some(BufWriter::new(file))),
            path,
        })
    }
}

impl SpanSink for JsonLinesSink {
    fn record(&self, span: Span) {
        let line = match serde_json::to_string(&span) {
            Ok(l) => l,
            Err(e) => {
                warn!("JsonLinesSink: failed to serialize span: {e}");
                return;
            }
        };
        let mut guard = self.writer.lock().unwrap();
        if let Some(ref mut w) = *guard {
            if let Err(e) = writeln!(w, "{line}") {
                warn!(path = %self.path.display(), "JsonLinesSink: write error: {e}");
            }
        }
    }

    fn flush(&self) {
        let mut guard = self.writer.lock().unwrap();
        if let Some(ref mut w) = *guard {
            let _ = w.flush();
        }
    }
}

impl Drop for JsonLinesSink {
    fn drop(&mut self) {
        // Best-effort flush on drop.
        let mut guard = self.writer.lock().unwrap();
        if let Some(ref mut w) = *guard {
            let _ = w.flush();
        }
    }
}
