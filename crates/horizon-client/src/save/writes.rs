//! The writer thread: what a folder queues to be written, and waiting for it to be on disk.
use super::*;

mod saved_portals;

/// The background thread that encodes chunks and writes them into the region files, in
/// submission order.
pub(super) struct Writer {
    send: mpsc::Sender<WriteBatch>,
    thread: std::thread::JoinHandle<()>,
}

/// Writes still queued or running, with a condition to wait for none.
#[derive(Default)]
pub(super) struct Outstanding {
    state: Mutex<WriteState>,
    chunks: AtomicUsize,
    pub(super) error: Mutex<Option<String>>,
    done: Condvar,
    /// What the folder's writes make stale: the portal blocks a search read from saved chunks.
    portals: saved_portals::SavedPortals,
}

#[derive(Default)]
struct WriteState {
    queued: usize,
    panicked: bool,
    snapshot_error: Option<String>,
}

struct WriterExit(Arc<Outstanding>);

impl Drop for WriterExit {
    fn drop(&mut self) {
        if std::thread::panicking() {
            // Wake waiters for batches this writer will never finish.
            let mut state = self
                .0
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.panicked = true;
            self.0.done.notify_all();
        }
    }
}

impl SaveDir {
    /// Queue `records` for the writer thread; with `wait`, return once
    /// they and every earlier write are on disk.
    pub(super) fn write_records(
        &self,
        folder: &str,
        records: Vec<((i32, i32), Option<NamedTag>)>,
        wait: bool,
    ) -> Result<()> {
        let records = records
            .into_iter()
            .map(|(k, t)| (k, t.map_or(Record::Delete, |t| Record::Tag(Arc::new(t)))))
            .collect();
        self.queue(Folder::of(folder), records, wait)
    }
    pub(super) fn queue(
        &self,
        folder: Folder,
        records: Vec<((i32, i32), Record)>,
        wait: bool,
    ) -> Result<()> {
        if !records.is_empty() {
            // Assign sequences and send under the same lock. Otherwise an older
            // snapshot can arrive after a newer one has already been written.
            let mut writer = self.writer.lock().map_err(|_| "writer poisoned")?;
            let writer = match &mut *writer {
                Some(writer) => writer,
                empty => empty.insert(self.start_writer()?),
            };
            let mut batch = Vec::with_capacity(records.len());
            {
                let mut pending = self.pending.lock().map_err(|_| "pending writes poisoned")?;
                for (key, record) in records {
                    let seq = self.sequence.fetch_add(1, Ordering::Relaxed);
                    pending.insert((folder, key.0, key.1), (seq, record.clone()));
                    batch.push((key, record, seq));
                }
            }
            // After the pending writes: a search reads the new chunks from here on.
            self.outstanding
                .portals
                .written(folder, batch.iter().map(|(key, _, _)| *key));
            self.submit_batch(writer, folder, batch)?;
        }
        if wait {
            self.flush()?;
        }
        Ok(())
    }
    fn submit_batch(
        &self,
        writer: &Writer,
        folder: Folder,
        batch: Vec<((i32, i32), Record, u64)>,
    ) -> Result<()> {
        {
            let mut state = self
                .outstanding
                .state
                .lock()
                .map_err(|_| "writer poisoned")?;
            if state.panicked {
                return Err("the region writer panicked".into());
            }
            state.queued += 1;
        }
        let chunks = if folder == Folder::Region {
            batch.len()
        } else {
            0
        };
        self.outstanding.chunks.fetch_add(chunks, Ordering::Relaxed);
        if writer.send.send((folder, batch)).is_err() {
            self.outstanding.chunks.fetch_sub(chunks, Ordering::Relaxed);
            *self
                .outstanding
                .error
                .lock()
                .map_err(|_| "writer poisoned")? = Some("the region writer stopped".into());
            self.outstanding
                .state
                .lock()
                .map_err(|_| "writer poisoned")?
                .queued -= 1;
            self.outstanding.done.notify_all();
            return Err("the region writer stopped".into());
        }
        Ok(())
    }

    /// Resubmit retained snapshots after the writer has reported a failure.
    /// The writer lock keeps these sequences ordered before any new snapshots.
    pub fn retry_failed_writes(&self) -> Result<bool> {
        let guard = self.writer.lock().map_err(|_| "writer poisoned")?;
        let state = self
            .outstanding
            .state
            .lock()
            .map_err(|_| "writer poisoned")?;
        if state.panicked {
            return Err("the region writer panicked".into());
        }
        if state.queued != 0 {
            return Ok(false);
        }
        drop(state);

        let Some(writer) = guard.as_ref() else {
            return Ok(false);
        };
        let pending = self.pending.lock().map_err(|_| "pending writes poisoned")?;
        let mut batches: HashMap<Folder, Vec<_>> = HashMap::new();
        for (&(folder, x, z), (sequence, record)) in pending.iter() {
            batches
                .entry(folder)
                .or_default()
                .push(((x, z), record.clone(), *sequence));
        }
        drop(pending);
        let submitted = !batches.is_empty();
        for folder in [Folder::Region, Folder::Entities, Folder::Poi] {
            if let Some(batch) = batches.remove(&folder) {
                self.submit_batch(writer, folder, batch)?;
            }
        }
        Ok(submitted)
    }

    fn start_writer(&self) -> std::io::Result<Writer> {
        let (send, receive) = mpsc::channel::<WriteBatch>();
        let root = self.root.clone();
        let (regions, entity_regions, poi_regions) = (
            self.regions.clone(),
            self.entity_regions.clone(),
            self.poi_regions.clone(),
        );
        let pending = self.pending.clone();
        let outstanding = self.outstanding.clone();
        let exit = WriterExit(outstanding.clone());
        // Encoding a record can enter a wasm mod, which needs the guest stack.
        let thread = horizon_guest_stack::spawn("horizon-region-writer".into(), move || {
            let _exit = exit;
            let mut buffer: std::collections::VecDeque<WriteBatch> =
                std::collections::VecDeque::new();
            const MAX_BATCH_CHUNKS: usize = 64;
            while let Some((kind, mut batch)) = buffer.pop_front().or_else(|| receive.recv().ok()) {
                let mut batch_count = 1;
                while batch.len() < MAX_BATCH_CHUNKS
                    && let Ok(next) = receive.try_recv()
                {
                    if next.0 == kind {
                        batch.extend(next.1);
                        batch_count += 1;
                    } else {
                        buffer.push_back(next);
                        break;
                    }
                }
                let cache = match kind {
                    Folder::Region => &regions,
                    Folder::Entities => &entity_regions,
                    Folder::Poi => &poi_regions,
                };
                let folder = kind.name();
                // IOWorker.pendingWrites: a chunk queued again since is
                // written once, by its newest batch.
                let newest = |k: &(i32, i32), seq: u64| {
                    pending.lock().map_or(true, |p| {
                        p.get(&(kind, k.0, k.1)).is_none_or(|p| p.0 <= seq)
                    })
                };
                let result = (|| {
                    let mut resolved = Vec::with_capacity(batch.len());
                    for (key, record, sequence) in &batch {
                        if newest(key, *sequence)
                            && let Some(record) = resolve_poi(&root, cache, *key, record)?
                        {
                            resolved.push((*key, record));
                        }
                    }
                    let records: Vec<_> = resolved.iter().map(|(k, r)| (*k, r)).collect();
                    write_region_records(&root, cache, folder, &records)
                })();
                if let Err(e) = &result {
                    let message = format!("failed to write {folder} files: {e}");
                    log::warn!("Horizon: {message}");
                    *outstanding.error.lock().unwrap() = Some(message);
                    // `reportChunkSaveFailure` for each chunk of the batch.
                    for (k, _, _) in &batch {
                        crate::chunk_io::report_save_failure(&root, k.0, k.1, e);
                    }
                }
                // Keep failed snapshots readable; never report them as durable.
                if result.is_ok()
                    && let Ok(mut pending) = pending.lock()
                {
                    for (k, _, seq) in &batch {
                        if pending.get(&(kind, k.0, k.1)).is_some_and(|p| p.0 == *seq) {
                            pending.remove(&(kind, k.0, k.1));
                        }
                    }
                    // Failed records stay pending until the same key (or a
                    // newer snapshot of it) is written. A different successful
                    // write must not hide earlier failures.
                    if pending.is_empty() {
                        *outstanding.error.lock().unwrap() = None;
                    }
                }
                if kind == Folder::Region {
                    outstanding.chunks.fetch_sub(batch.len(), Ordering::Relaxed);
                }
                if let Ok(mut state) = outstanding.state.lock() {
                    state.queued -= batch_count;
                    outstanding.done.notify_all();
                }
            }
        })?;
        Ok(Writer { send, thread })
    }
    /// Wait until every queued write is on disk.
    pub fn flush(&self) -> Result<()> {
        let submission = self.writer.lock().map_err(|_| "writer poisoned")?;
        let mut state = self
            .outstanding
            .state
            .lock()
            .map_err(|_| "writer poisoned")?;
        drop(submission);
        while state.queued > 0 && !state.panicked {
            state = self
                .outstanding
                .done
                .wait(state)
                .map_err(|_| "writer poisoned")?;
        }
        self.write_result(&state)
    }
    /// `ChunkMap.activeChunkWrites`: chunk records, excluding entity/POI IO.
    pub fn active_chunk_writes(&self) -> usize {
        self.outstanding.chunks.load(Ordering::Relaxed)
    }
    /// Poll durability without holding up the event loop. Errors remain visible
    /// to every waiter, including a later synchronous shutdown.
    pub fn writes_finished(&self) -> Result<bool> {
        // Check for batches still being submitted.
        let _guard = match self.writer.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::WouldBlock) => return Ok(false),
            Err(_) => return Err("writer poisoned".into()),
        };
        let state = self
            .outstanding
            .state
            .lock()
            .map_err(|_| "writer poisoned")?;
        // A retry can still settle an earlier failed batch. Only report
        // its unresolved error once the queue drains, or on a writer panic.
        if state.queued == 0 || state.panicked {
            self.write_result(&state)?;
        }
        Ok(state.queued == 0)
    }
    /// Preserve errors for snapshots that may never have entered the write queue.
    pub fn note_snapshot_failure(&self, error: String) {
        log::warn!("Horizon: {error}");
        let mut state = self
            .outstanding
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.snapshot_error.get_or_insert(error);
    }

    fn write_result(&self, state: &WriteState) -> Result<()> {
        if state.panicked {
            return Err("the region writer panicked".into());
        }
        if let Some(error) = &state.snapshot_error {
            return Err(error.clone().into());
        }
        match &*self
            .outstanding
            .error
            .lock()
            .map_err(|_| "writer poisoned")?
        {
            Some(error) => Err(error.clone().into()),
            None => Ok(()),
        }
    }
}

impl Drop for SaveDir {
    /// Finish queued writes before the world is let go.
    fn drop(&mut self) {
        if let Some(writer) = self.writer.get_mut().ok().and_then(Option::take) {
            drop(writer.send);
            let _ = writer.thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durability_recovers_only_after_every_failed_snapshot_is_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("entities");
        fs::write(&blocked, b"blocked").unwrap();
        let save = SaveDir::new(dir.path().to_owned(), LevelShape::OVERWORLD);
        let entity = Tag::compound([("version", Tag::Int(3))]);
        let keys = [(0, 0), (1, 0)];
        save.save_entities_later(keys.map(|key| (key, vec![entity.clone()])).to_vec())
            .unwrap();
        assert!(save.flush().is_err());

        fs::remove_file(blocked).unwrap();
        save.save_entities_later(vec![(keys[0], vec![entity.clone()])])
            .unwrap();
        assert!(save.flush().is_err());
        assert!(save.retry_failed_writes().unwrap());
        save.flush().unwrap();
        assert!(!save.retry_failed_writes().unwrap());
        assert!(save.writes_finished().unwrap());

        let reopened = SaveDir::new(dir.path().to_owned(), LevelShape::OVERWORLD);
        for (x, z) in keys {
            assert_eq!(reopened.load_entities(x, z), vec![entity.clone()]);
        }
    }

    #[test]
    fn a_snapshot_rejected_before_queueing_is_reported_after_other_writes_succeed() {
        let dir = tempfile::tempdir().unwrap();
        let save = SaveDir::new(dir.path().to_owned(), LevelShape::OVERWORLD);
        save.note_snapshot_failure("proto snapshot rejected".into());
        let entity = Tag::compound([("version", Tag::Int(3))]);
        save.save_entities_later(vec![((0, 0), vec![entity])])
            .unwrap();

        assert_eq!(
            save.flush().unwrap_err().to_string(),
            "proto snapshot rejected"
        );
        assert!(save.writes_finished().is_err());
        assert!(!save.retry_failed_writes().unwrap());
    }

    #[test]
    fn polling_sees_an_open_submission() {
        let dir = tempfile::tempdir().unwrap();
        let save = SaveDir::new(dir.path().into(), LevelShape::OVERWORLD);
        let guard = save.writer.lock().unwrap();
        assert!(!save.writes_finished().unwrap());
        drop(guard);
        assert!(save.writes_finished().unwrap());
    }

    #[test]
    fn writer_panic_wakes_save_barriers_with_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let save = Arc::new(SaveDir::new(dir.path().to_owned(), LevelShape::OVERWORLD));
        save.outstanding.state.lock().unwrap().queued = 1;
        assert!(!save.writes_finished().unwrap());

        let (sent, received) = mpsc::channel();
        let observer = save.clone();
        let waiter = std::thread::spawn(move || {
            sent.send(observer.flush().unwrap_err().to_string())
                .unwrap();
        });
        let exit = WriterExit(save.outstanding.clone());
        let writer = std::thread::spawn(move || {
            let _exit = exit;
            panic!("injected region-writer panic");
        });
        assert!(writer.join().is_err());
        assert_eq!(
            received
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap(),
            "the region writer panicked"
        );
        waiter.join().unwrap();
        assert!(save.writes_finished().is_err());
    }
}
