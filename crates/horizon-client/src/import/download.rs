//! The phone's first door: game files downloaded from a server, with no computer involved. A phone
//! that opens with nothing to load fetches `<base><version>.zip` (the archive `--pack-resources`
//! writes) into its home, then installs it as an archive that came over Wi-Fi or a cable would be
//! (`Pack`), so what arrives is checked the same way. The download runs on its own thread and the
//! screen reads its progress; if it fails, the other doors are still there.

use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use super::{Installed, MAX_PACKED, Pack};

/// The archive's address for this version, from the player's override or the
/// app's registered provider. A plain engine without either uses manual import.
pub fn url(version: &str) -> Option<String> {
    let base = std::env::var("HORIZON_GAME_FILES_URL")
        .ok()
        .filter(|b| !b.is_empty())
        .or_else(|| horizon_hooks::engine::game_files_base().map(str::to_owned))?;
    Some(archive_url(version, &base))
}

fn archive_url(version: &str, base: &str) -> String {
    let base = base.split('#').next().unwrap_or_default();
    let (path, query) = base.split_at(base.find('?').unwrap_or(base.len()));
    format!("{}/{version}.zip{query}", path.trim_end_matches('/'))
}

/// What the download is doing now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Bytes so far, and the whole size when the server said it (0 when it didn't).
    Downloading { received: u64, total: u64 },
    /// The archive is here and being unpacked.
    Installing,
}

impl Default for Stage {
    fn default() -> Self {
        Self::Downloading {
            received: 0,
            total: 0,
        }
    }
}

/// The cache files are shared by all sources in one home. Keep the lease on
/// the worker, including while a cancelled request is unwinding.
static ACTIVE_DOWNLOADS: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());

struct DownloadLease(PathBuf);

impl DownloadLease {
    fn acquire(home: &Path) -> io::Result<Self> {
        let home = fs::canonicalize(home)?;
        let mut active = ACTIVE_DOWNLOADS
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !active.insert(home.clone()) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "a game-file download is still running",
            ));
        }
        Ok(Self(home))
    }
}

impl Drop for DownloadLease {
    fn drop(&mut self) {
        ACTIVE_DOWNLOADS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.0);
    }
}

/// A download under way. Dropping it requests cancellation without waiting on network I/O.
pub struct Download {
    // A restart changes the received bytes and the total at the same time.
    progress: Arc<Mutex<Stage>>,
    done: mpsc::Receiver<Result<Option<Installed>, String>>,
    cancelled: Arc<AtomicBool>,
    worker: thread::Thread,
}

impl Drop for Download {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        self.worker.unpark();
    }
}

impl Download {
    /// Start downloading prepared game files at `url` into `home` and installing them there.
    /// Worlds and resource packs are rejected before installation. A finished archive already in
    /// `home` (an earlier download, or `<version>.zip` copied in by hand) is installed without one.
    pub fn start(url: String, home: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(&home)?;
        let lease = DownloadLease::acquire(&home)?;
        let home = lease.0.clone();
        let expected_marker = super::resource_marker(&home)?;
        let progress = Arc::new(Mutex::new(Stage::default()));
        let cancelled = Arc::new(AtomicBool::new(false));
        let (sender, done) = mpsc::channel();
        let worker_progress = progress.clone();
        let stop = cancelled.clone();
        let worker = thread::Builder::new()
            .name("horizon-download".into())
            .spawn(move || {
                let archive = match local_archive(&url, &home) {
                    Some(archive) => {
                        log::info!(
                            "Horizon: installing {} already on this device",
                            archive.display()
                        );
                        Ok(archive)
                    }
                    None => fetch_cancellable(&url, &home, &worker_progress, &stop),
                };
                let result = (|| {
                    if stop.load(Ordering::Acquire) {
                        return Err("the download was cancelled".into());
                    }
                    let access = super::acquire_home_access(&home).map_err(|e| e.to_string())?;
                    if stop.load(Ordering::Acquire) {
                        return Err("the download was cancelled".into());
                    }
                    if !super::game_files_are_current(&home, &expected_marker)
                        .map_err(|e| e.to_string())?
                    {
                        if let Ok(archive) = archive
                            && archive.file_name() == Some(DOWNLOADED.as_ref())
                        {
                            let _ = fs::remove_file(archive);
                            let _ = fs::remove_file(home.join(DOWNLOADED_SOURCE));
                        }
                        return Ok(None);
                    }
                    let archive = archive?;
                    *worker_progress
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner) = Stage::Installing;
                    let downloaded = archive.file_name() == Some(DOWNLOADED.as_ref());
                    let opened = Pack::open(&archive).and_then(Pack::require_game_files);
                    let invalid_archive = opened.is_err();
                    let result = opened.and_then(|pack| {
                        pack.install_with_access(&home, &access, || !stop.load(Ordering::Acquire))
                    });
                    let invalid_contents = result.as_ref().err().is_some_and(|error| {
                        error
                            .downcast_ref::<io::Error>()
                            .is_some_and(|error| error.kind() == io::ErrorKind::InvalidData)
                    });
                    if downloaded && (invalid_archive || invalid_contents) {
                        // A known-invalid archive cannot be fixed by retrying its installation.
                        let _ = fs::remove_file(&archive);
                    }
                    if downloaded && !archive.exists() {
                        let _ = fs::remove_file(home.join(DOWNLOADED_SOURCE));
                    }
                    result.map(Some).map_err(|e| e.to_string())
                })();
                drop(lease);
                let _ = sender.send(result);
            })?;
        Ok(Self {
            progress,
            done,
            cancelled,
            worker: worker.thread().clone(),
        })
    }

    pub fn stage(&self) -> Stage {
        *self.progress.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The result, once there is one. `Ok(None)` means another setup finished first.
    pub fn poll(&self) -> Option<Result<Option<Installed>, String>> {
        match self.done.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Err("the download stopped".into())),
        }
    }
}

/// The finished download's name in `home`, and the partial one's.
const DOWNLOADED: &str = "horizon-download.zip";

/// Internal staging files must not enter the manual drop-folder importer:
/// their owning download has yet to enforce source identity and archive type.
/// The UUID form is the native launcher's historical Documents staging name.
pub(super) fn reserved_archive(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    if name == DOWNLOADED {
        return true;
    }
    let Some(id) = name
        .strip_prefix("horizon-download-")
        .and_then(|name| name.strip_suffix(".zip"))
    else {
        return false;
    };
    id.len() == 36
        && id.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}
/// A completed cache is reusable only for this requested URL's SHA-256. Never
/// persist URLs themselves: a custom source may contain an access token.
const DOWNLOADED_SOURCE: &str = "horizon-download.zip.source";
const PARTIAL: &str = "horizon-download.zip.part";
/// The requested/final URL hashes and strong `ETag` of the partial download. ETags identify
/// a representation only within its resource, so another source or redirect target must restart.
const PARTIAL_TAG: &str = "horizon-download.zip.etag";

#[derive(serde::Serialize, serde::Deserialize)]
struct ResumeTag {
    source: String,
    target: String,
    etag: String,
    #[serde(default)]
    length: Option<u64>,
}

fn strong_etag(tag: &str) -> bool {
    tag.strip_prefix('"')
        .and_then(|tag| tag.strip_suffix('"'))
        .is_some_and(|tag| {
            tag.bytes()
                .all(|byte| byte == 0x21 || (0x23..=0x7e).contains(&byte) || byte >= 0x80)
        })
}

fn source_hash(url: &str) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(url.as_bytes()))
}

/// Find a completed download for this URL or a manually copied version archive.
fn local_archive(url: &str, home: &Path) -> Option<PathBuf> {
    let path = url.split(['?', '#']).next()?;
    let name = path.rsplit('/').next().filter(|n| n.ends_with(".zip"));
    let cached = fs::read_to_string(home.join(DOWNLOADED_SOURCE))
        .is_ok_and(|source| source == source_hash(url))
        .then_some(DOWNLOADED);
    [cached, name.filter(|name| *name != DOWNLOADED)]
        .into_iter()
        .flatten()
        .map(|name| home.join(name))
        .find(|path| {
            path.is_file() && Pack::open(path).is_ok_and(|pack| pack.kind == super::Kind::Resources)
        })
}

/// How many times a download that broke off is picked up again, and the pause before each try.
const RETRIES: u32 = 6;
// Long waits should return control to the launcher, keeping resumable bytes.
const MAX_RETRY_WAIT: Duration = Duration::from_secs(60);
#[cfg(not(test))]
const PAUSE: Duration = Duration::from_secs(2);
#[cfg(test)]
const PAUSE: Duration = Duration::from_millis(10);

fn wait_for_retry(delay: Duration, cancelled: &AtomicBool) {
    let deadline = Instant::now() + delay;
    while !cancelled.load(Ordering::Acquire) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        // An unrelated wake must not skip the remaining backoff.
        thread::park_timeout(remaining);
    }
}

fn retry_after_delay(value: &str, now: chrono::DateTime<chrono::Utc>) -> Option<Duration> {
    let value = value.trim();
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Some(Duration::from_secs(value.parse().unwrap_or(u64::MAX)));
    }
    let date = [
        "%a, %d %b %Y %H:%M:%S GMT",
        "%A, %d-%b-%y %H:%M:%S GMT",
        "%a %b %e %H:%M:%S %Y",
    ]
    .into_iter()
    .find_map(|format| chrono::NaiveDateTime::parse_from_str(value, format).ok())?;
    Some(
        date.and_utc()
            .signed_duration_since(now)
            .to_std()
            .unwrap_or_default(),
    )
}

/// What stopped one request.
enum Stop {
    Cancelled,
    /// The connection broke: try again from where it stopped.
    Broke(String),
    /// A temporary HTTP failure, with any delay requested by the server.
    RetryHttp {
        status: u16,
        delay: Duration,
    },
    /// The server refused, or the file can't be kept: trying again won't help.
    Failed(String),
}

#[derive(Debug, PartialEq, Eq)]
enum FetchProgress {
    Complete,
    More,
}

/// Download `url` to `home/horizon-download.zip`, through a `.part` file so a cut download is
/// never taken for an archive. A connection that breaks (on a phone: the screen locked, the network
/// changed) is picked up where it stopped with a `Range` request; the `.part` file stays between runs
/// for the same reason.
fn fetch_cancellable(
    url: &str,
    home: &Path,
    progress: &Mutex<Stage>,
    cancelled: &AtomicBool,
) -> Result<PathBuf, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(20))
        .timeout_read(Duration::from_secs(60))
        .build();
    fs::create_dir_all(home).map_err(|e| e.to_string())?;
    let part = home.join(PARTIAL);
    let archive = home.join(DOWNLOADED);
    let mut tries = 0;
    loop {
        match fetch_once_cancellable(&agent, url, &part, progress, cancelled) {
            Ok(FetchProgress::Complete) => break,
            // A server can cap the size of each range response. A complete
            // range advances the file and does not spend a failure retry.
            Ok(FetchProgress::More) => continue,
            Err(Stop::Cancelled) => return Err("the download was cancelled".into()),
            Err(Stop::Failed(e)) => {
                let _ = fs::remove_file(&part);
                let _ = fs::remove_file(home.join(PARTIAL_TAG));
                return Err(e);
            }
            Err(stop) => {
                let (e, delay) = match stop {
                    Stop::Broke(error) => (error, Duration::ZERO),
                    Stop::RetryHttp { status, delay } => {
                        if delay > MAX_RETRY_WAIT {
                            return Err(format!(
                                "the download server answered HTTP {status}; retry in {} seconds",
                                delay
                                    .as_secs()
                                    .saturating_add(u64::from(delay.subsec_nanos() != 0))
                            ));
                        }
                        (format!("the download server answered HTTP {status}"), delay)
                    }
                    Stop::Cancelled | Stop::Failed(_) => unreachable!("handled above"),
                };
                tries += 1;
                if tries > RETRIES {
                    return Err(format!("the download was cut: {e}"));
                }
                log::warn!(
                    "Horizon: the download broke off ({e}); picking it up again ({tries}/{RETRIES})"
                );
                wait_for_retry((PAUSE * tries).max(delay), cancelled);
            }
        }
    }
    if cancelled.load(Ordering::Acquire) {
        return Err("the download was cancelled".into());
    }
    // Invalidate the old cache's identity before replacing its bytes. A crash
    // between the rename and recording the new source leaves an unidentified
    // cache, never new bytes falsely labelled as an older source.
    match fs::remove_file(home.join(DOWNLOADED_SOURCE)) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    fs::rename(&part, &archive).map_err(|e| e.to_string())?;
    fs::write(home.join(DOWNLOADED_SOURCE), source_hash(url)).map_err(|e| e.to_string())?;
    let _ = fs::remove_file(home.join(PARTIAL_TAG));
    Ok(archive)
}

/// One request: the rest of `part`, or all of it when there is none or the server can't send a part.
fn fetch_once_cancellable(
    agent: &ureq::Agent,
    url: &str,
    part: &Path,
    progress: &Mutex<Stage>,
    cancelled: &AtomicBool,
) -> Result<FetchProgress, Stop> {
    if cancelled.load(Ordering::Acquire) {
        return Err(Stop::Cancelled);
    }
    let tag_file = part.with_file_name(PARTIAL_TAG);
    let source = source_hash(url);
    let saved = fs::read(&tag_file)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<ResumeTag>(&bytes).ok())
        .filter(|saved| saved.source == source && strong_etag(&saved.etag));
    let tag = saved.as_ref().map(|saved| saved.etag.as_str());
    // A part whose file can't be named is not resumed: it could be the start of another file.
    let have = match &tag {
        Some(_) => fs::metadata(part).map(|m| m.len()).unwrap_or(0),
        None => 0,
    };
    let mut request = agent.get(url);
    if let (true, Some(tag)) = (have > 0, &tag) {
        request = request
            .set("Range", &format!("bytes={have}-"))
            .set("If-Range", tag);
    }
    // Rejected ranges must restart from byte zero.
    let restart = || {
        let _ = fs::remove_file(&tag_file);
        let _ = fs::remove_file(part);
        Stop::Broke("the server could not resume this archive; restarting".into())
    };
    let response = request.call();
    if cancelled.load(Ordering::Acquire) {
        return Err(Stop::Cancelled);
    }
    let response = match response {
        Ok(response) if matches!(response.status(), 204 | 205) => {
            return Err(Stop::Failed(
                "the download server did not send an archive".into(),
            ));
        }
        Ok(response) if (200..300).contains(&response.status()) => response,
        // A 416 may mean the remote file shrank.
        Err(ureq::Error::Status(416, _)) if have > 0 => return Err(restart()),
        Err(ureq::Error::Status(status @ (408 | 429 | 500 | 502 | 503 | 504), response)) => {
            // Keep saved bytes after a transient server failure.
            let delay = response
                .header("Retry-After")
                .and_then(|value| retry_after_delay(value, chrono::Utc::now()))
                .unwrap_or_default();
            return Err(Stop::RetryHttp { status, delay });
        }
        Ok(response) | Err(ureq::Error::Status(_, response)) => {
            return Err(Stop::Failed(format!(
                "the download server answered HTTP {}",
                response.status()
            )));
        }
        // Transport's Display can include the full request or redirect URL.
        Err(ureq::Error::Transport(error)) => {
            return Err(Stop::Broke(format!(
                "could not reach the download server ({:?})",
                error.kind()
            )));
        }
    };
    let target = source_hash(response.get_url());
    // A range must be explicit and independently verified.
    let resumed = have > 0 && response.status() == 206;
    let range = if response.status() == 206 {
        let parsed = response.header("Content-Range").and_then(|header| {
            let (span, total) = header.strip_prefix("bytes ")?.split_once('/')?;
            let (first, last) = span.split_once('-')?;
            Some((
                first.parse::<u64>().ok()?,
                last.parse::<u64>().ok()?,
                total.parse::<u64>().ok()?,
            ))
        });
        let Some((first, last, length)) = parsed.filter(|&(first, last, length)| {
            resumed && first == have && first <= last && last < length
        }) else {
            return Err(restart());
        };
        if response
            .header("ETag")
            .is_some_and(|value| Some(value) != tag)
            || saved.as_ref().is_none_or(|saved| {
                saved.target != target || saved.length.is_some_and(|expected| expected != length)
            })
        {
            return Err(restart());
        }
        Some((last + 1, length, last - first + 1))
    } else if response.header("Content-Range").is_some() {
        // A response marked as complete can still describe a slice of the file.
        return Err(restart());
    } else {
        None
    };
    let start = if resumed { have } else { 0 };
    let rest = response
        .header("Content-Length")
        .and_then(|l| l.parse::<u64>().ok())
        .unwrap_or(0);
    if range.is_some_and(|(_, _, expected)| rest != 0 && rest != expected) {
        return Err(restart());
    }
    let size = match range {
        Some((end, _, _)) => end,
        None => rest,
    };
    let whole = range.map_or(size, |(_, length, _)| length);
    if whole > MAX_PACKED {
        return Err(Stop::Failed(
            "the archive is larger than this will unpack".into(),
        ));
    }
    let file = if resumed {
        fs::OpenOptions::new().append(true).open(part)
    } else {
        // Unlink the old validator before changing the bytes it describes.
        match fs::remove_file(&tag_file) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(Stop::Failed(format!(
                    "could not clear saved download state: {error}"
                )));
            }
        }
        File::create(part)
    };
    let mut file = file.map_err(|e| Stop::Failed(format!("could not save the download: {e}")))?;
    if resumed {
        if let Some(mut saved) = saved
            && saved.length.is_none()
        {
            // The initial response did not say how long the archive was.
            // Pin its first validated range total before appending any bytes.
            saved.length = Some(whole);
            let bytes = serde_json::to_vec(&saved)
                .map_err(|e| Stop::Failed(format!("could not encode saved download state: {e}")))?;
            File::create(&tag_file)
                .and_then(|mut tag| {
                    tag.write_all(&bytes)?;
                    tag.sync_all()
                })
                .map_err(|e| Stop::Failed(format!("could not save download state: {e}")))?;
        }
    } else {
        // Never persist URL secrets, or a validator that cannot be used by If-Range.
        if let Some(etag) = response.header("ETag").filter(|tag| strong_etag(tag))
            && let Ok(saved) = serde_json::to_vec(&ResumeTag {
                source,
                target,
                etag: etag.to_owned(),
                length: (whole != 0).then_some(whole),
            })
        {
            let _ = fs::write(&tag_file, saved);
        }
    }
    let mut reader = response.into_reader();
    let mut buffer = vec![0; 1 << 16];
    let mut so_far = start;
    *progress.lock().unwrap_or_else(PoisonError::into_inner) = Stage::Downloading {
        received: so_far,
        total: whole,
    };
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(Stop::Cancelled);
        }
        // Stop at the size the server announced: on iOS a read past the end of a connection the server
        // closed fails ("Socket is not connected", os error 57) instead of saying the body ended.
        if size != 0 && so_far >= size {
            break;
        }
        let capacity = if size == 0 {
            buffer.len()
        } else {
            (size - so_far).min(buffer.len() as u64) as usize
        };
        let next = reader.read(&mut buffer[..capacity]);
        if cancelled.load(Ordering::Acquire) {
            return Err(Stop::Cancelled);
        }
        let n = match next {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                // Keep what arrived for the next request.
                let _ = file.sync_all();
                return Err(Stop::Broke(format!(
                    "could not read the download response ({:?})",
                    e.kind()
                )));
            }
        };
        so_far += n as u64;
        if so_far > MAX_PACKED {
            return Err(Stop::Failed(
                "the archive is larger than this will unpack".into(),
            ));
        }
        file.write_all(&buffer[..n])
            .map_err(|e| Stop::Failed(format!("could not save the download: {e}")))?;
        *progress.lock().unwrap_or_else(PoisonError::into_inner) = Stage::Downloading {
            received: so_far,
            total: whole,
        };
    }
    file.sync_all()
        .map_err(|e| Stop::Failed(format!("could not save the download: {e}")))?;
    if size != 0 && so_far < size {
        return Err(Stop::Broke(format!(
            "the connection closed at {so_far} of {size} bytes"
        )));
    }
    if so_far == 0 {
        return Err(Stop::Failed(
            "the download server did not send an archive".into(),
        ));
    }
    if so_far < whole {
        Ok(FetchProgress::More)
    } else {
        Ok(FetchProgress::Complete)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fetch(url: &str, home: &Path, progress: &Mutex<Stage>) -> Result<PathBuf, String> {
        fetch_cancellable(url, home, progress, &AtomicBool::new(false))
    }

    fn fetch_once(
        agent: &ureq::Agent,
        url: &str,
        part: &Path,
        progress: &Mutex<Stage>,
    ) -> Result<FetchProgress, Stop> {
        fetch_once_cancellable(agent, url, part, progress, &AtomicBool::new(false))
    }

    #[test]
    fn the_archive_is_named_for_the_version() {
        for base in ["https://example.org/game", "https://example.org/game/"] {
            assert_eq!(
                archive_url("26.3", base),
                "https://example.org/game/26.3.zip"
            );
        }
        assert_eq!(
            archive_url("26.3", "https://example.org/game/?key=A%2FB#note"),
            "https://example.org/game/26.3.zip?key=A%2FB"
        );
    }

    #[test]
    fn the_plain_engine_has_no_implicit_download_service() {
        assert_eq!(horizon_hooks::engine::game_files_base(), None);
        let explicit = std::env::var("HORIZON_GAME_FILES_URL")
            .ok()
            .filter(|base| !base.is_empty());
        assert_eq!(url("26.3"), explicit.map(|base| archive_url("26.3", &base)));
    }

    #[test]
    fn download_failures_preserve_diagnostics_without_the_source_url() {
        let agent = ureq::AgentBuilder::new().build();
        let part = home("download-private-errors").join(PARTIAL);
        let progress = Mutex::default();
        for status in ["403 Forbidden", "302 Found"] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let server = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 1024];
                stream.read(&mut request).unwrap();
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: 4\r\nConnection: close\r\n\r\nBODY"
                )
                .unwrap();
            });
            let url = format!("http://{address}/private-game.zip?key=private-download-token");
            let result = fetch_once(&agent, &url, &part, &progress);
            server.join().unwrap();
            assert!(matches!(result, Err(Stop::Failed(message))
                if message.contains(&status[..3]) && !message.contains("private") && !message.contains(&address.to_string())));
            assert!(!part.exists());
        }

        let malformed = "http://[invalid]/private-game.zip?key=private-download-token";
        let connection = fetch_once(&agent, malformed, &part, &progress);
        assert!(matches!(connection, Err(Stop::Broke(message))
            if message.contains("download server") && !message.contains("private") && !message.contains("invalid")));
    }

    #[test]
    fn retry_after_accepts_seconds_and_http_dates() {
        let now = chrono::DateTime::parse_from_rfc3339("2015-10-21T07:28:00Z")
            .unwrap()
            .to_utc();
        for value in [
            "3",
            "Wed, 21 Oct 2015 07:28:03 GMT",
            "Wed Oct 21 07:28:03 2015",
        ] {
            assert_eq!(retry_after_delay(value, now), Some(Duration::from_secs(3)));
        }
        assert_eq!(
            retry_after_delay("Wed, 21 Oct 2015 07:00:00 GMT", now),
            Some(Duration::ZERO)
        );
        assert_eq!(retry_after_delay("soon", now), None);
    }

    #[test]
    fn temporary_failures_retry_and_complete_ranges_do_not_spend_the_retry_budget() {
        use std::{
            io::{BufRead, BufReader},
            net::{TcpListener, TcpStream},
            time::Instant,
        };

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/archive.zip", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let server = thread::spawn(move || {
            fn accept(listener: &TcpListener) -> TcpStream {
                let deadline = Instant::now() + Duration::from_secs(3);
                loop {
                    match listener.accept() {
                        Ok((stream, _)) => return stream,
                        Err(error)
                            if error.kind() == io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("expected a download request: {error}"),
                    }
                }
            }
            for (start, response) in ["503 Service Unavailable", "429 Too Many Requests"]
            .into_iter()
            .map(|status| (4, format!(
                "HTTP/1.1 {status}\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )))
            .chain("efghijklm".chars().enumerate().map(|(offset, byte)| {
                let start = 4 + offset;
                (start, format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: 1\r\nContent-Range: bytes {start}-{start}/13\r\nETag: \"fixture\"\r\nConnection: close\r\n\r\n{byte}"
                ))
            }))
            {
                let mut socket = accept(&listener);
                socket.set_nonblocking(false).unwrap();
                socket.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                let mut request = String::new();
                let mut reader = BufReader::new(&socket);
                loop {
                    let mut line = String::new();
                    let bytes = reader.read_line(&mut line).unwrap();
                    if bytes == 0 || line == "\r\n" {
                        break;
                    }
                    request.push_str(&line.to_ascii_lowercase());
                }
                assert!(request.contains(&format!("range: bytes={start}-\r\n")), "{request}");
                assert!(request.contains("if-range: \"fixture\"\r\n"), "{request}");
                socket.write_all(response.as_bytes()).unwrap();
            }
        });

        let home = home("temporary-server-failure");
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join(PARTIAL), b"abcd").unwrap();
        fs::write(
            home.join(PARTIAL_TAG),
            serde_json::to_vec(&ResumeTag {
                source: source_hash(&url),
                target: source_hash(&url),
                etag: "\"fixture\"".into(),
                length: Some(13),
            })
            .unwrap(),
        )
        .unwrap();
        let result = fetch(&url, &home, &Mutex::default());
        let contents = result.and_then(|path| fs::read(path).map_err(|e| e.to_string()));
        fs::remove_dir_all(home).unwrap();
        assert_eq!(contents.unwrap(), b"abcdefghijklm");
        server.join().unwrap();
    }

    #[test]
    fn resume_rejects_wrong_ranges_without_appending_or_completing() {
        for (name, status, headers, complete, retained) in [
            (
                "wrong-start",
                "206 Partial Content",
                "Content-Range: bytes 0-1/6\r\n",
                false,
                false,
            ),
            ("missing-range", "206 Partial Content", "", false, false),
            (
                "false-full",
                "200 OK",
                "Content-Range: bytes 4-5/6\r\n",
                false,
                false,
            ),
            (
                "false-complete",
                "416 Range Not Satisfiable",
                "Content-Range: bytes */8\r\n",
                false,
                false,
            ),
            (
                "valid",
                "206 Partial Content",
                "Content-Range: bytes 4-5/6\r\n",
                true,
                true,
            ),
            (
                "more-remains",
                "206 Partial Content",
                "Content-Range: bytes 4-5/8\r\n",
                false,
                true,
            ),
            (
                "changed-validator",
                "206 Partial Content",
                "Content-Range: bytes 4-5/6\r\nETag: \"two\"\r\n",
                false,
                false,
            ),
            (
                "changed-total",
                "206 Partial Content",
                "Content-Range: bytes 4-5/6\r\nETag: \"one\"\r\n",
                false,
                false,
            ),
            (
                "wrong-length",
                "206 Partial Content",
                "Content-Range: bytes 4-6/7\r\n",
                false,
                false,
            ),
        ] {
            let home = home(name);
            fs::create_dir_all(&home).unwrap();
            let part = home.join(PARTIAL);
            fs::write(&part, b"abcd").unwrap();
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            fs::write(
                home.join(PARTIAL_TAG),
                serde_json::to_vec(&ResumeTag {
                    source: source_hash(&format!("http://{address}/game.zip")),
                    target: source_hash(&format!("http://{address}/game.zip")),
                    etag: "\"one\"".into(),
                    length: (name == "changed-total").then_some(8),
                })
                .unwrap(),
            )
            .unwrap();
            let server = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 1024];
                let n = stream.read(&mut request).unwrap();
                assert!(
                    String::from_utf8_lossy(&request[..n])
                        .to_lowercase()
                        .contains("range: bytes=4-")
                );
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\n{headers}Content-Length: 2\r\nConnection: close\r\n\r\nef"
                )
                .unwrap();
            });
            let result = fetch_once(
                &ureq::AgentBuilder::new().build(),
                &format!("http://{address}/game.zip"),
                &part,
                &Mutex::default(),
            );
            server.join().unwrap();
            assert_eq!(
                result.ok(),
                match (complete, retained) {
                    (true, _) => Some(FetchProgress::Complete),
                    (false, true) => Some(FetchProgress::More),
                    (false, false) => None,
                },
                "{name}: incorrect completion"
            );
            if retained {
                assert_eq!(fs::read(&part).unwrap(), b"abcdef", "{name}");
                assert!(home.join(PARTIAL_TAG).exists());
            } else {
                assert!(!part.exists(), "{name}: incompatible bytes retained");
                assert!(!home.join(PARTIAL_TAG).exists());
            }
            fs::remove_dir_all(home).unwrap();
        }
    }

    #[test]
    fn a_first_resumed_range_pins_the_archive_length_for_later_ranges() {
        let home = home("pin-range-total");
        fs::create_dir_all(&home).unwrap();
        let part = home.join(PARTIAL);
        let validator = home.join(PARTIAL_TAG);
        fs::write(&part, b"ab").unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/game.zip", listener.local_addr().unwrap());
        fs::write(
            &validator,
            serde_json::to_vec(&ResumeTag {
                source: source_hash(&url),
                target: source_hash(&url),
                etag: "\"unchanged\"".into(),
                length: None,
            })
            .unwrap(),
        )
        .unwrap();
        let server = thread::spawn(move || {
            for (range, body) in [("2-3/8", "cd"), ("4-5/6", "ef")] {
                let (mut socket, _) = listener.accept().unwrap();
                let mut request = [0; 1024];
                assert!(socket.read(&mut request).unwrap() > 0);
                write!(
                    socket,
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: 2\r\nContent-Range: bytes {range}\r\nETag: \"unchanged\"\r\nConnection: close\r\n\r\n{body}"
                )
                .unwrap();
            }
        });
        let agent = ureq::AgentBuilder::new().build();
        let progress = Mutex::default();
        let first = fetch_once(&agent, &url, &part, &progress);
        let pinned: ResumeTag = serde_json::from_slice(&fs::read(&validator).unwrap()).unwrap();
        let second = fetch_once(&agent, &url, &part, &progress);
        server.join().unwrap();
        assert!(matches!(first, Ok(FetchProgress::More)));
        assert_eq!(pinned.length, Some(8));
        assert!(matches!(second, Err(Stop::Broke(_))));
        assert!(!part.exists());
        assert!(!validator.exists());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn only_well_formed_strong_entity_tags_can_resume() {
        for (tag, expected) in [
            ("\"one\"", true),
            ("\"\"", true),
            ("W/\"one\"", false),
            ("one", false),
            ("\"one\", \"two\"", false),
            ("\"has a space\"", false),
            ("\"unterminated", false),
        ] {
            assert_eq!(strong_etag(tag), expected, "{tag:?}");
        }
    }

    #[test]
    fn full_download_does_not_replace_partial_bytes_if_its_validator_cannot_be_cleared() {
        let home = home("blocked-validator");
        fs::create_dir_all(&home).unwrap();
        let part = home.join(PARTIAL);
        fs::write(&part, b"old").unwrap();
        fs::create_dir(home.join(PARTIAL_TAG)).unwrap();
        let url = serve(b"new".to_vec(), 3);
        let prior = Stage::Downloading {
            received: 3,
            total: 5,
        };
        let progress = Mutex::new(prior);
        let result = fetch_once(&ureq::AgentBuilder::new().build(), &url, &part, &progress);
        assert!(
            matches!(result, Err(Stop::Failed(message)) if message.contains("clear saved download state"))
        );
        assert_eq!(fs::read(part).unwrap(), b"old");
        assert_eq!(*progress.lock().unwrap(), prior);
        fs::remove_dir_all(home).unwrap();
    }

    /// One HTTP answer from a local socket: `length` in the header, `body` sent.
    fn serve(body: Vec<u8>, length: usize) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request);
            let head =
                format!("HTTP/1.1 200 OK\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n");
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
        });
        format!("http://{address}/26.3.zip")
    }

    #[test]
    fn an_empty_success_response_is_not_cached_as_an_archive() {
        let home = home("empty-success");
        let error = fetch_cancellable(
            &serve(Vec::new(), 0),
            &home,
            &Mutex::default(),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert_eq!(error, "the download server did not send an archive");
        for name in [DOWNLOADED, DOWNLOADED_SOURCE, PARTIAL, PARTIAL_TAG] {
            assert!(!home.join(name).exists(), "{name}");
        }
        fs::remove_dir_all(home).unwrap();
    }

    fn prepared() -> Vec<u8> {
        prepared_with_facts("{}")
    }

    fn prepared_with_facts(facts: &str) -> Vec<u8> {
        use zip::{CompressionMethod, write::SimpleFileOptions, write::ZipWriter};
        let mut zip = ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        for (name, mut body) in super::super::tests::prepared_entries() {
            if name == "facts.json" {
                body = facts.into();
            }
            zip.start_file(name, options).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn automatic_setup_never_installs_a_world_or_resource_pack() {
        use zip::{write::SimpleFileOptions, write::ZipWriter};
        for (kind, marker) in [("world", "level.dat"), ("pack", "pack.mcmeta")] {
            let home = home(&format!("wrong-kind-{kind}"));
            let mut zip = ZipWriter::new(std::io::Cursor::new(Vec::new()));
            zip.start_file(marker, SimpleFileOptions::default())
                .unwrap();
            zip.write_all(b"{}").unwrap();
            let body = zip.finish().unwrap().into_inner();
            let size = body.len();
            let result = wait(&Download::start(serve(body, size), home.clone()).unwrap());
            assert!(
                result.is_err(),
                "automatic setup installed {kind}: {result:?}"
            );
            for folder in ["saves", "resourcepacks", "resources"] {
                assert!(
                    !home.join(folder).exists(),
                    "automatic setup created {folder}"
                );
            }
            assert!(!home.join(DOWNLOADED).exists());
            assert!(!home.join(DOWNLOADED_SOURCE).exists());
            fs::remove_dir_all(home).unwrap();
        }
    }

    #[test]
    fn a_completed_download_is_only_reused_for_its_source() {
        let body = prepared();
        let size = body.len();
        let url = format!("{}?token=fixture-secret", serve(body, size));
        let home = home("completed-source");
        let archive = fetch(&url, &home, &Mutex::default()).unwrap();
        assert_eq!(local_archive(&url, &home), Some(archive));
        assert_eq!(
            fs::read_to_string(home.join(DOWNLOADED_SOURCE)).unwrap(),
            source_hash(&url)
        );
        assert_eq!(
            local_archive("https://example.org/new-version.zip", &home),
            None
        );
        // The reserved cache filename must not bypass the source check through
        // the separate path for a versioned archive supplied by the player.
        assert_eq!(
            local_archive("https://example.org/horizon-download.zip", &home),
            None
        );
        // The original server has already closed: retrying installation should
        // consume its matching completed cache without another HTTP request.
        wait(&Download::start(url, home.clone()).unwrap()).unwrap();
        assert!(!home.join(DOWNLOADED).exists());
        assert!(!home.join(DOWNLOADED_SOURCE).exists());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_download_keeps_the_game_files_another_import_published_first() {
        check_concurrent_install(true);
        check_concurrent_install(false);
    }

    fn check_concurrent_install(transfer_succeeds: bool) {
        let home = home(&format!("concurrent-install-{transfer_succeeds}"));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (requested, request) = mpsc::channel();
        let (send_archive, release) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut headers = [0; 2048];
            assert!(stream.read(&mut headers).unwrap() > 0);
            requested.send(()).unwrap();
            release.recv_timeout(Duration::from_secs(5)).unwrap();
            if !transfer_succeeds {
                stream
                    .write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .unwrap();
                return;
            }
            let body = prepared_with_facts(r#"{"source":"download"}"#);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(&body).unwrap();
        });
        let download = Download::start(format!("http://{address}/game.zip"), home.clone()).unwrap();
        request.recv_timeout(Duration::from_secs(5)).unwrap();
        let archive = home.join("manual.zip");
        let facts = r#"{"source":"manual"}"#;
        fs::write(&archive, prepared_with_facts(facts)).unwrap();
        Pack::open(&archive).unwrap().install(&home).unwrap();
        send_archive.send(()).unwrap();
        assert!(wait_result(&download).unwrap().is_none());
        server.join().unwrap();
        assert_eq!(
            fs::read_to_string(home.join("resources/facts.json")).unwrap(),
            facts
        );
        assert!(!home.join(DOWNLOADED).exists());
        assert!(!home.join(DOWNLOADED_SOURCE).exists());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn switching_sources_after_a_completed_download_installs_the_new_archive() {
        let home = home("completed-switch");
        let old = prepared_with_facts(r#"{"source":"old"}"#);
        fetch(&serve(old.clone(), old.len()), &home, &Mutex::default()).unwrap();
        let new = prepared_with_facts(r#"{"source":"new"}"#);
        wait(&Download::start(serve(new.clone(), new.len()), home.clone()).unwrap()).unwrap();
        assert_eq!(
            fs::read_to_string(home.join("resources/facts.json")).unwrap(),
            r#"{"source":"new"}"#
        );
        assert!(!home.join(DOWNLOADED_SOURCE).exists());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn an_unidentified_cache_is_skipped_but_a_manual_version_archive_is_used() {
        let home = home("unidentified-cache");
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join(DOWNLOADED), prepared()).unwrap();
        let url = "https://example.org/26.3.zip?key=A%2FB";
        assert_eq!(local_archive(url, &home), None);
        let manual = home.join("26.3.zip");
        fs::write(&manual, prepared()).unwrap();
        assert_eq!(local_archive(url, &home), Some(manual));
        fs::remove_dir_all(home).unwrap();
    }

    fn home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "horizon-download-test-{}-{tag}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn wait(download: &Download) -> Result<Installed, String> {
        wait_result(download)?.ok_or_else(|| "another setup installed the game files".into())
    }

    fn wait_result(download: &Download) -> Result<Option<Installed>, String> {
        for _ in 0..500 {
            if let Some(result) = download.poll() {
                return result;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("the download never finished");
    }

    #[test]
    fn downloaded_game_files_are_installed_where_the_game_reads_them() {
        let body = prepared();
        let size = body.len();
        let home = home("ok");
        let download = Download::start(serve(body, size), home.clone()).unwrap();
        let installed = wait(&download).unwrap();
        assert_eq!(installed.kind, super::super::Kind::Resources);
        assert!(home.join("resources/horizon-resources.json").is_file());
        assert_eq!(download.stage(), Stage::Installing);
        // The archive is consumed, and no partial file is left.
        assert!(!home.join("horizon-download.zip").exists());
        assert!(!home.join("horizon-download.zip.part").exists());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn retry_refetches_a_download_with_damaged_prepared_files() {
        let good = prepared();
        let size = good.len();
        let url = serve(good, size);
        let home = home("damaged-cache");
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join(DOWNLOADED), prepared_with_facts("")).unwrap();
        fs::write(home.join(DOWNLOADED_SOURCE), source_hash(&url)).unwrap();

        let download = Download::start(url.clone(), home.clone()).unwrap();
        let error = wait(&download).unwrap_err();
        assert!(error.contains("facts.json is empty"), "{error}");
        assert!(!home.join(DOWNLOADED).exists());
        assert!(!home.join(DOWNLOADED_SOURCE).exists());
        assert!(!home.join("resources").exists());

        let retry = Download::start(url, home.clone()).unwrap();
        assert_eq!(wait(&retry).unwrap().kind, super::super::Kind::Resources);
        assert_eq!(
            fs::read_to_string(home.join("resources/facts.json")).unwrap(),
            "{}"
        );
        fs::remove_dir_all(home).unwrap();
    }

    /// The body, then the connection held open: the download must end at the announced size, not wait
    /// for the server to close (or, on iOS, fail when it does).
    #[test]
    fn the_download_ends_at_the_announced_size() {
        let body = prepared();
        let size = body.len();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (done, held) = mpsc::channel::<()>();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request);
            let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {size}\r\n\r\n");
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
            // Keep the socket open until the test is over.
            let _ = held.recv_timeout(Duration::from_secs(10));
        });
        let home = home("held");
        let download = Download::start(format!("http://{address}/26.3.zip"), home.clone()).unwrap();
        let installed = wait(&download).unwrap();
        assert_eq!(installed.kind, super::super::Kind::Resources);
        let _ = done.send(());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_download_that_stays_cut_keeps_its_part_for_the_next_run() {
        let body = prepared();
        let size = body.len();
        let home = home("cut");
        let download =
            Download::start(serve(body[..size / 2].to_vec(), size), home.clone()).unwrap();
        let error = wait(&download).unwrap_err();
        assert!(error.contains("cut"), "{error}");
        assert!(!home.join("resources").exists());
        assert_eq!(
            fs::metadata(home.join(PARTIAL)).unwrap().len(),
            (size / 2) as u64
        );
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn dropping_a_download_wakes_a_worker_waiting_to_retry() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let stop = cancelled.clone();
        let (ready, waiting) = mpsc::channel();
        let (finished, exited) = mpsc::channel();
        let worker = thread::spawn(move || {
            ready.send(()).unwrap();
            wait_for_retry(Duration::from_secs(10), &stop);
            finished.send(stop.load(Ordering::Acquire)).unwrap();
        });
        waiting.recv().unwrap();
        assert_eq!(
            exited.recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        let (_sender, done) = mpsc::channel();
        let download = Download {
            progress: Default::default(),
            done,
            cancelled,
            worker: worker.thread().clone(),
        };

        drop(download);
        assert_eq!(exited.recv_timeout(Duration::from_secs(2)), Ok(true));
        worker.join().unwrap();
    }

    #[test]
    fn dropping_a_download_keeps_its_partial_and_blocks_retry_until_the_worker_stops() {
        let body = prepared();
        let half = body.len() / 2;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (send_tail, resume) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 2048];
            assert!(stream.read(&mut request).unwrap() > 0);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"one\"\r\nConnection: close\r\n\r\n",
                body.len()
            ).unwrap();
            stream.write_all(&body[..half]).unwrap();
            resume.recv_timeout(Duration::from_secs(5)).unwrap();
            let _ = stream.write_all(&body[half..]);
        });
        let home = home("cancel-active");
        let url = format!("http://{address}/game.zip");
        let download = Download::start(url.clone(), home.clone()).unwrap();
        for _ in 0..500 {
            if matches!(download.stage(), Stage::Downloading { received, .. } if received == half as u64)
            {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(fs::metadata(home.join(PARTIAL)).unwrap().len(), half as u64);
        drop(download);
        assert!(
            matches!(Download::start(url, home.clone()), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
        send_tail.send(()).unwrap();
        server.join().unwrap();
        let lease = (0..500)
            .find_map(|_| match DownloadLease::acquire(&home) {
                Ok(lease) => Some(lease),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                    None
                }
                Err(error) => panic!("{error}"),
            })
            .expect("the cancelled worker should release its cache");
        assert_eq!(fs::metadata(home.join(PARTIAL)).unwrap().len(), half as u64);
        assert!(home.join(PARTIAL_TAG).is_file());
        assert!(!home.join(DOWNLOADED).exists());
        assert!(!home.join("resources").exists());
        drop(lease);
        fs::remove_dir_all(home).unwrap();
    }

    /// ETags can collide across resources: switching URL starts a whole new download.
    #[test]
    fn changing_source_never_resumes_another_urls_bytes() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 2048];
            stream.read(&mut request).unwrap();
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nETag: \"one\"\r\nConnection: close\r\n\r\nAAAA").unwrap();
            drop(stream);
            let (mut stream, _) = listener.accept().unwrap();
            let n = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..n]).to_lowercase();
            if request.contains("range: bytes=4-") {
                stream.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nContent-Range: bytes 4-7/8\r\nETag: \"one\"\r\nConnection: close\r\n\r\nBBBB").unwrap();
            } else {
                stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nETag: \"one\"\r\nConnection: close\r\n\r\nBBBBBBBB").unwrap();
            }
            request
        });
        let home = home("source-boundary");
        fs::create_dir_all(&home).unwrap();
        let part = home.join(PARTIAL);
        let agent = ureq::AgentBuilder::new().build();
        let progress = Mutex::default();
        assert!(matches!(
            fetch_once(&agent, &format!("http://{address}/a.zip"), &part, &progress),
            Err(Stop::Broke(_))
        ));
        assert!(
            fetch_once(
                &agent,
                &format!("http://{address}/b.zip?access=fixture-secret"),
                &part,
                &progress
            )
            .is_ok()
        );
        let request = server.join().unwrap();
        assert_eq!(fs::read(&part).unwrap(), b"BBBBBBBB");
        assert!(!request.contains("range:"), "{request}");
        assert!(
            !fs::read_to_string(home.join(PARTIAL_TAG))
                .unwrap()
                .contains("fixture-secret")
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_changed_redirect_target_cannot_reuse_the_previous_targets_etag() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for step in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 2048];
                stream.read(&mut request).unwrap();
                let response = match step {
                    0 => {
                        "HTTP/1.1 302 Found\r\nLocation: /a.zip\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    }
                    1 => {
                        "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nETag: \"one\"\r\nConnection: close\r\n\r\nAAAA"
                    }
                    2 => {
                        "HTTP/1.1 302 Found\r\nLocation: /b.zip\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    }
                    _ => {
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nContent-Range: bytes 4-7/8\r\nETag: \"one\"\r\nConnection: close\r\n\r\nBBBB"
                    }
                };
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        let home = home("redirect-boundary");
        fs::create_dir_all(&home).unwrap();
        let part = home.join(PARTIAL);
        let agent = ureq::AgentBuilder::new().build();
        let url = format!("http://{address}/start");
        let progress = Mutex::default();
        assert!(matches!(
            fetch_once(&agent, &url, &part, &progress),
            Err(Stop::Broke(_))
        ));
        let result = fetch_once(&agent, &url, &part, &progress);
        server.join().unwrap();
        assert!(
            matches!(result, Err(Stop::Broke(_))),
            "changed target must restart"
        );
        assert!(!part.exists());
        assert!(!home.join(PARTIAL_TAG).exists());
        fs::remove_dir_all(home).unwrap();
    }

    /// The connection breaks halfway; the next request asks for the rest and gets it.
    #[test]
    fn a_broken_download_is_picked_up_where_it_stopped() {
        let body = prepared();
        let size = body.len();
        let half = size / 2;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (ranges, asked) = mpsc::channel::<String>();
        thread::spawn(move || {
            // The first answer announces everything and sends half.
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {size}\r\nETag: \"one\"\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body[..half]);
            drop(stream);
            // The second sends the rest of the range it is asked for.
            let (mut stream, _) = listener.accept().unwrap();
            let n = stream.read(&mut request).unwrap();
            let text = String::from_utf8_lossy(&request[..n]).to_lowercase();
            let range = text
                .lines()
                .find_map(|l| l.strip_prefix("range: bytes="))
                .unwrap_or("")
                .to_owned();
            assert!(text.contains("if-range: \"one\""), "{text}");
            let _ = ranges.send(range.clone());
            let from: usize = range.trim_end_matches('-').parse().unwrap();
            let rest = &body[from..];
            let head = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {from}-{}/{size}\r\nConnection: close\r\n\r\n",
                rest.len(),
                size - 1
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(rest);
        });
        let home = home("resumed");
        let download = Download::start(format!("http://{address}/26.3.zip"), home.clone()).unwrap();
        let installed = wait(&download).unwrap();
        assert_eq!(installed.kind, super::super::Kind::Resources);
        assert_eq!(asked.recv().unwrap(), format!("{half}-"));
        assert!(home.join("resources/horizon-resources.json").is_file());
        assert!(!home.join(PARTIAL).exists());
        fs::remove_dir_all(home).unwrap();
    }

    /// The archive copied into the home by hand installs with no server at all.
    #[test]
    fn an_archive_already_in_the_home_is_installed_without_downloading() {
        let home = home("local");
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join("26.3.zip"), prepared()).unwrap();
        // Nothing listens on port 9: the download would fail.
        let download = Download::start("http://127.0.0.1:9/26.3.zip".into(), home.clone()).unwrap();
        let installed = wait(&download).unwrap();
        assert_eq!(installed.kind, super::super::Kind::Resources);
        assert!(home.join("resources/horizon-resources.json").is_file());
        assert!(!home.join("26.3.zip").exists());
        fs::remove_dir_all(home).unwrap();
    }

    /// The file changed on the server between two tries: the second request gets it whole, not a part of it.
    #[test]
    fn a_file_that_changed_on_the_server_starts_over() {
        use zip::{CompressionMethod, write::SimpleFileOptions, write::ZipWriter};
        let old = prepared();
        let mut zip = ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        for (name, mut body) in super::super::tests::prepared_entries() {
            if name == "facts.json" {
                body = r#"{"new":true}"#.into();
            }
            zip.start_file(name, options).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        let new = zip.finish().unwrap().into_inner();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"old\"\r\nConnection: close\r\n\r\n",
                old.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&old[..old.len() / 2]);
            drop(stream);
            // If-Range names the old file: the new one comes whole, with 200.
            let (mut stream, _) = listener.accept().unwrap();
            let _ = stream.read(&mut request);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"new\"\r\nConnection: close\r\n\r\n",
                new.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&new);
        });
        let home = home("changed");
        let download = Download::start(format!("http://{address}/26.3.zip"), home.clone()).unwrap();
        wait(&download).unwrap();
        assert_eq!(
            fs::read_to_string(home.join("resources/facts.json")).unwrap(),
            r#"{"new":true}"#
        );
        assert!(!home.join(PARTIAL_TAG).exists());
        fs::remove_dir_all(home).unwrap();
    }
}
