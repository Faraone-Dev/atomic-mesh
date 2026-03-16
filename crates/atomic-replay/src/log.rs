use atomic_core::event::Event;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};

/// Append-only event log backed by a file.
/// Format: [4-byte length][bincode-encoded Event]...
/// Used for deterministic replay: replay(events.log) must produce identical state.
pub struct EventLog {
    writer: Option<BufWriter<File>>,
    path: String,
    event_count: u64,
}

impl EventLog {
    /// Open or create an event log file for appending.
    pub fn open(path: &str) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let writer = BufWriter::new(file);
        Ok(Self {
            writer: Some(writer),
            path: path.to_string(),
            event_count: 0,
        })
    }

    /// Create a read-only event log (for replay).
    pub fn read_only(path: &str) -> Self {
        Self {
            writer: None,
            path: path.to_string(),
            event_count: 0,
        }
    }

    /// Append an event to the log.
    pub fn append(&mut self, event: &Event) -> std::io::Result<()> {
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "log is read-only"))?;

        let encoded = bincode::serialize(event).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
        })?;

        let len = encoded.len() as u32;
        writer.write_all(&len.to_le_bytes())?;
        writer.write_all(&encoded)?;
        self.event_count += 1;

        Ok(())
    }

    /// Flush the log to disk.
    pub fn flush(&mut self) -> std::io::Result<()> {
        if let Some(writer) = &mut self.writer {
            writer.flush()?;
        }
        Ok(())
    }

    /// Read all events from the log file.
    pub fn read_all(&self) -> std::io::Result<Vec<Event>> {
        let file = File::open(&self.path)?;
        let mut reader = BufReader::new(file);
        let mut events = Vec::new();

        loop {
            let mut len_buf = [0u8; 4];
            match reader.read_exact(&mut len_buf) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            }

            let len = u32::from_le_bytes(len_buf) as usize;
            let mut data = vec![0u8; len];
            reader.read_exact(&mut data)?;

            let event: Event = bincode::deserialize(&data).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
            })?;
            events.push(event);
        }

        Ok(events)
    }

    /// Read events in a range [from_seq, to_seq].
    pub fn read_range(&self, from_seq: u64, to_seq: u64) -> std::io::Result<Vec<Event>> {
        let all = self.read_all()?;
        Ok(all
            .into_iter()
            .filter(|e| e.seq >= from_seq && e.seq <= to_seq)
            .collect())
    }

    pub fn event_count(&self) -> u64 {
        self.event_count
    }

    pub fn path(&self) -> &str {
        &self.path
    }
}
