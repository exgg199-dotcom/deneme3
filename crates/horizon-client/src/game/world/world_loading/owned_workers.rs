//! Handles retained until the engine has stopped every producer and drained its workers.
use std::{path::PathBuf, sync::Mutex, thread::JoinHandle, time::Instant};

pub(super) enum Preload {
    Idle,
    Started {
        thread: JoinHandle<()>,
        left: Option<Instant>,
    },
    // Opening a world transfers ownership of the data, not the thread handle.
    Used(Vec<JoinHandle<()>>),
}
pub(super) static PRELOAD: Mutex<Preload> = Mutex::new(Preload::Idle);
pub(super) static PREPARED_WRITERS: Mutex<Vec<(PathBuf, JoinHandle<()>)>> = Mutex::new(Vec::new());

fn take_workers(state: &mut Preload) -> Vec<JoinHandle<()>> {
    match std::mem::replace(state, Preload::Idle) {
        Preload::Started { thread, .. } => vec![thread],
        Preload::Used(workers) => workers,
        Preload::Idle => Vec::new(),
    }
}
pub(in crate::game) fn world_data_used() {
    let mut state = PRELOAD.lock().unwrap_or_else(|e| e.into_inner());
    *state = Preload::Used(take_workers(&mut state));
}
pub(super) fn retain_loot_worker(worker: JoinHandle<()>) {
    let mut state = PRELOAD.lock().unwrap_or_else(|e| e.into_inner());
    let mut workers = take_workers(&mut state);
    workers.push(worker);
    *state = Preload::Used(workers);
}
/// Poll on the owning event loop after producers stop; never block it on a live worker.
pub(in crate::game) fn shutdown_preload() -> bool {
    let mut state = PRELOAD.lock().unwrap_or_else(|e| e.into_inner());
    let running = match &*state {
        Preload::Started { thread, .. } => !thread.is_finished(),
        Preload::Used(workers) => workers.iter().any(|worker| !worker.is_finished()),
        Preload::Idle => false,
    };
    if running {
        return false;
    }
    for worker in take_workers(&mut state) {
        let _ = worker.join();
    }
    true
}
pub(in crate::game) fn shutdown_prepared_files() -> bool {
    let mut writers = PREPARED_WRITERS.lock().unwrap_or_else(|e| e.into_inner());
    if writers.iter().any(|(_, worker)| !worker.is_finished()) {
        return false;
    }
    for (_, worker) in writers.drain(..) {
        let _ = worker.join();
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    static TEST_LOCK: Mutex<()> = Mutex::new(());
    fn held_worker() -> (std::sync::mpsc::Sender<()>, JoinHandle<()>) {
        let (release, wait) = std::sync::mpsc::channel();
        (
            release,
            std::thread::spawn(move || {
                wait.recv().unwrap();
            }),
        )
    }
    fn wait_until_finished() {
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let finished = match &*PRELOAD.lock().unwrap() {
                Preload::Used(workers) => workers.iter().all(JoinHandle::is_finished),
                _ => panic!("preload handles lost before shutdown"),
            };
            if finished {
                break;
            }
            assert!(Instant::now() < deadline, "worker did not finish");
            std::thread::yield_now();
        }
    }
    // Tests use the same process-global ownership as production; run serially.
    #[test]
    fn used_preload_is_still_drained_before_shutdown() {
        let _serial = TEST_LOCK.lock().unwrap();
        assert!(shutdown_preload());
        let (release, thread) = held_worker();
        *PRELOAD.lock().unwrap() = Preload::Started { thread, left: None };
        world_data_used();
        world_data_used();
        assert!(!shutdown_preload());
        release.send(()).unwrap();
        wait_until_finished();
        assert!(shutdown_preload());
        assert!(matches!(*PRELOAD.lock().unwrap(), Preload::Idle));
        assert!(shutdown_preload());
    }
    #[test]
    fn loot_and_world_preloads_both_belong_to_shutdown() {
        let _serial = TEST_LOCK.lock().unwrap();
        assert!(shutdown_preload());
        let (world_release, thread) = held_worker();
        *PRELOAD.lock().unwrap() = Preload::Started { thread, left: None };
        let (loot_release, loot) = held_worker();
        retain_loot_worker(loot);
        world_data_used();
        assert!(matches!(&*PRELOAD.lock().unwrap(), Preload::Used(workers) if workers.len() == 2));
        world_release.send(()).unwrap();
        assert!(
            !shutdown_preload(),
            "live loot worker must prevent retirement"
        );
        loot_release.send(()).unwrap();
        wait_until_finished();
        assert!(shutdown_preload());
    }
    #[test]
    fn prepared_writer_is_retained_until_it_finishes() {
        let _serial = TEST_LOCK.lock().unwrap();
        assert!(shutdown_prepared_files());
        let (release, writer) = held_worker();
        PREPARED_WRITERS
            .lock()
            .unwrap()
            .push((PathBuf::from("fixture"), writer));
        assert!(!shutdown_prepared_files());
        assert_eq!(PREPARED_WRITERS.lock().unwrap().len(), 1);
        release.send(()).unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while !PREPARED_WRITERS.lock().unwrap()[0].1.is_finished() {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(shutdown_prepared_files());
        assert!(PREPARED_WRITERS.lock().unwrap().is_empty());
        assert!(shutdown_prepared_files());
    }
}
