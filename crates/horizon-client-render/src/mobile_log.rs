//! Bounded, privacy-filtered mobile diagnostics. Desktop logging is unchanged.
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
};
pub const LIMIT: usize = 1024 * 1024;
pub const ENTRY_LIMIT: usize = 32 * 1024;

fn begins_entry(line: &str) -> bool {
    line.as_bytes().get(..10).is_some_and(|p| {
        p[0] == b'['
            && p[3] == b':'
            && p[6] == b':'
            && p[9] == b']'
            && [1, 2, 4, 5, 7, 8]
                .into_iter()
                .all(|i| p[i].is_ascii_digit())
    }) && line
        .get(10..)
        .and_then(|rest| rest.strip_prefix(" ["))
        .and_then(|rest| rest.split_once("]: "))
        .and_then(|(header, _)| header.rsplit_once('/'))
        .is_some_and(|(thread, level)| {
            !thread.is_empty() && matches!(level, "TRACE" | "DEBUG" | "INFO" | "WARN" | "ERROR")
        })
}

fn has_credential_field(text: &str) -> bool {
    ["token", "uhs", "apikey", "api-key", "api key"]
        .into_iter()
        .any(|field| {
            text.match_indices(field).any(|(index, name)| {
                // Token can be a suffix of a compound field name. The other
                // fields must start at a word boundary.
                if field != "token"
                    && text
                        .as_bytes()
                        .get(index.wrapping_sub(1))
                        .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
                {
                    return false;
                }
                text[index + name.len()..]
                    .trim_start_matches(['"', '\''])
                    .trim_start()
                    .starts_with([':', '='])
            })
        })
}

/// A bounded read may start anywhere inside a multiline message. Its label could
/// have been cut off, so retain nothing until the next timestamped entry.
fn complete_tail(text: &str, starts_at_line: bool) -> &str {
    let tail = if starts_at_line {
        text
    } else {
        let Some((_, tail)) = text.split_once('\n') else {
            return "";
        };
        tail
    };
    let mut offset = 0;
    for line in tail.split_inclusive('\n') {
        if begins_entry(line) {
            return &tail[offset..];
        }
        offset += line.len();
    }
    ""
}
/// Fail closed for authentication messages rather than guessing where a secret ends.
pub fn redact(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    if [
        "accesstoken",
        "refreshtoken",
        "idtoken",
        "user code",
        "access_token",
        "refresh_token",
        "id_token",
        "token\"",
        "authorization",
        "bearer",
        "password",
        "secret",
        "cookie",
        "xbl3.0",
        "device_code",
        "user_code",
        "devicecode",
        "usercode",
        "rpsticket",
        "usertokens",
        "sessionid",
        "session_id",
        "accesskey",
        "api_key",
    ]
    .iter()
    .any(|s| lower.contains(s))
        || has_credential_field(&lower)
    {
        return "[redacted sensitive log message]".to_owned();
    }
    text.lines()
        .map(|line| {
            line.split_whitespace()
                .map(|word| {
                    if word.len() > 128
                        || word.contains('@')
                        || word.contains("http://")
                        || word.contains("https://")
                        || word.contains("/Users/")
                        || word.contains("/var/mobile/")
                        || word.starts_with("eyJ")
                    {
                        "[redacted]".to_owned()
                    } else {
                        word.to_owned()
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn redact_entry(text: &str) -> String {
    let safe = redact(text);
    if safe == "[redacted sensitive log message]" && begins_entry(text) {
        // Keep the boundary without retaining a potentially sensitive thread name.
        return format!("{} [Diagnostics/INFO]: {safe}", &text[..10]);
    }
    let mut lines = safe.split('\n');
    let mut entry = lines.next().unwrap_or_default().to_owned();
    for line in lines {
        // Continuations must never look like a fresh logger-generated header.
        entry.push_str("\n  ");
        entry.push_str(line);
    }
    entry
}

/// Redact each dated message together with its continuation lines. A single
/// credential in an older log must not erase the rest of that session.
fn redact_entries(text: &str) -> String {
    let mut safe = String::with_capacity(text.len());
    let mut entry_start = 0;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if offset > entry_start && begins_entry(line) {
            safe.push_str(&redact_entry(&text[entry_start..offset]));
            safe.push('\n');
            entry_start = offset;
        }
        offset += line.len();
    }
    if entry_start < text.len() {
        safe.push_str(&redact_entry(&text[entry_start..]));
        safe.push('\n');
    }
    safe
}

/// Publish the archive only after all of its bytes have been written.
/// The caller holds the log directory's writer lock.
fn write_previous(dir: &Path, text: &str) -> io::Result<()> {
    let mut staging = tempfile::Builder::new()
        .prefix(".horizon-previous-log-")
        .tempfile_in(dir)?;
    staging.write_all(text.as_bytes())?;
    staging.as_file().sync_all()?;
    staging
        .persist(dir.join("previous.log"))
        .map_err(|error| error.error)?;
    Ok(())
}

fn with_log_lock(
    file: &mut File,
    operation: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    // The launcher takes a shared flock on latest.log while reading both files.
    file.lock()?;
    let result = operation(file);
    let unlocked = file.unlock();
    result.and(unlocked)
}

pub fn rotate(file: &mut File, dir: &Path) -> io::Result<()> {
    with_log_lock(file, |file| rotate_locked(file, dir, false))
}

fn bounded_entries(safe: &str, maximum: usize) -> &str {
    if safe.len() <= maximum {
        return safe;
    }
    let offset = safe.ceil_char_boundary(safe.len() - maximum);
    complete_tail(&safe[offset..], safe.as_bytes()[offset - 1] == b'\n')
}

fn rotate_locked(file: &mut File, dir: &Path, allow_in_place: bool) -> io::Result<()> {
    let len = file.metadata()?.len();
    if len > 0 {
        // A pre-upgrade file can be huge. Only read a bounded tail, then scrub it.
        let offset = len.saturating_sub(LIMIT as u64);
        let starts_at_line = if offset == 0 {
            file.seek(SeekFrom::Start(0))?;
            true
        } else {
            file.seek(SeekFrom::Start(offset - 1))?;
            let mut previous = [0];
            file.read_exact(&mut previous)?;
            previous[0] == b'\n'
        };
        let mut bytes = Vec::new();
        file.take(LIMIT as u64).read_to_end(&mut bytes)?;
        let text = String::from_utf8_lossy(&bytes);
        // A whole leading partial message can contain unlabeled secrets.
        let text = if len > LIMIT as u64 {
            complete_tail(&text, starts_at_line)
        } else {
            &text
        };
        let safe = redact_entries(text);
        let retained = bounded_entries(&safe, LIMIT);
        // A bounded read can discard the entire tail if it contains no
        // complete entries. Keep the previous archive in that case.
        if !retained.trim().is_empty()
            && let Err(error) = write_previous(dir, retained)
        {
            if !allow_in_place {
                return Err(error);
            }
            // At the size limit, a failed archive would otherwise block every
            // new message. Keep recent complete entries on the locked inode.
            let recent = bounded_entries(retained, LIMIT / 2);
            file.seek(SeekFrom::Start(0))?;
            file.write_all(b"[older game log entries omitted: archive could not be written]\n")?;
            file.write_all(recent.as_bytes())?;
            let length = file.stream_position()?;
            return file.set_len(length);
        }
    }
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    Ok(())
}
pub fn append(file: &mut File, dir: &Path, text: &str) -> io::Result<()> {
    let mut safe = redact_entry(text);
    if safe.len() > ENTRY_LIMIT {
        let mut end = ENTRY_LIMIT;
        while !safe.is_char_boundary(end) {
            end -= 1;
        }
        safe.truncate(end);
        safe.push_str("\n  [message truncated]");
    }
    with_log_lock(file, |file| {
        // Allow a newline both before an interrupted tail and after this entry.
        if file.metadata()?.len() + safe.len() as u64 + 2 > LIMIT as u64 {
            rotate_locked(file, dir, true)?;
        }
        append_record(file, |file| writeln!(file, "{safe}"))
    })
}

// A partial write must not merge the next timestamp into a broken entry.
fn append_record(
    file: &mut File,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    let start = file.seek(SeekFrom::End(0))?;
    let result = (|| {
        if start != 0 {
            file.seek(SeekFrom::End(-1))?;
            let mut byte = [0];
            file.read_exact(&mut byte)?;
            if byte[0] != b'\n' {
                file.write_all(b"\n")?;
            }
        }
        write(file)
    })();
    if let Err(error) = result {
        file.set_len(start)?;
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn a_failed_append_preserves_previous_and_next_entries() {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"previous entry\n").unwrap();
        let error = append_record(&mut file, |file| {
            file.write_all(b"partial")?;
            Err(io::ErrorKind::StorageFull.into())
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::StorageFull);
        append_record(&mut file, |file| writeln!(file, "next entry")).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut log = String::new();
        file.read_to_string(&mut log).unwrap();
        assert_eq!(log, "previous entry\nnext entry\n");
    }

    #[cfg(unix)]
    #[test]
    fn an_append_waits_for_the_export_snapshot_to_finish() {
        use std::{sync::mpsc, thread, time::Duration};

        let dir = std::env::temp_dir().join(format!("horizon-log-append-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("latest.log");
        fs::write(&path, "previous entry\n").unwrap();
        let snapshot = File::open(&path).unwrap();
        snapshot.lock_shared().unwrap();
        let mut writer = File::options().read(true).write(true).open(&path).unwrap();
        let home = dir.clone();
        let (started, running) = mpsc::channel();
        let (finished, done) = mpsc::channel();
        let thread = thread::spawn(move || {
            started.send(()).unwrap();
            finished
                .send(append(&mut writer, &home, "new entry"))
                .unwrap();
        });

        running.recv().unwrap();
        let early = done.recv_timeout(Duration::from_millis(50));
        let captured = fs::read_to_string(&path).unwrap();
        snapshot.unlock().unwrap();
        thread.join().unwrap();
        assert!(matches!(early, Err(mpsc::RecvTimeoutError::Timeout)));
        done.recv().unwrap().unwrap();
        assert_eq!(captured, "previous entry\n");
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "previous entry\nnew entry\n"
        );
        drop(snapshot);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rotation_keeps_a_complete_entry_at_the_exact_tail_boundary() {
        let directory =
            std::env::temp_dir().join(format!("horizon-log-boundary-{}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("latest.log");
        let preceding = "[12:00:00] [Render/INFO]: previous entry\n";
        let first = "[12:00:01] [Render/INFO]: first complete entry\n";
        let data = format!("{preceding}{first}{}", "a".repeat(LIMIT - first.len()));
        fs::write(&path, data).unwrap();
        let mut file = File::options().read(true).write(true).open(&path).unwrap();

        rotate(&mut file, &directory).unwrap();

        let archive = fs::read_to_string(directory.join("previous.log")).unwrap();
        assert!(archive.starts_with(first));
        assert!(!archive.contains("previous entry"));
        assert!(archive.len() <= LIMIT);
        drop(file);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_timestamp_in_a_continuation_does_not_start_a_new_entry() {
        let first = "[12:00:00] [Render/INFO]: first line";
        let continuation = "[12:00:01] [Render/INFO]: a literal timestamp";
        let next = "[12:00:02] [Render/INFO]: next message";
        let safe = redact_entry(&format!("{first}\n{continuation}"));
        assert_eq!(safe, format!("{first}\n  {continuation}"));
        let rotated = redact_entries(&format!("{safe}\n{next}\n"));
        assert_eq!(rotated, format!("{first}\n  {continuation}\n{next}\n"));
    }

    #[test]
    fn a_redacted_mobile_record_keeps_its_timestamp_boundary() {
        let safe = redact_entry("[12:00:00] [Bearer opaque-fixture/INFO]: shader status");
        assert_eq!(
            safe,
            "[12:00:00] [Diagnostics/INFO]: [redacted sensitive log message]"
        );
        assert!(begins_entry(&safe));
    }

    #[test]
    fn whitespace_in_token_assignments_does_not_expose_the_value() {
        for message in ["Token\t=opaque-fixture", "'token'\t : opaque-fixture"] {
            assert_eq!(redact(message), "[redacted sensitive log message]");
        }
        assert_eq!(
            redact("unexpected token near main()"),
            "unexpected token near main()"
        );
    }

    #[test]
    fn xbox_and_camel_case_oauth_fields_redact_short_values() {
        for message in [
            r#"{"RpsTicket":"d=opaque-fixture"}"#,
            r#"{"UserTokens":["opaque-fixture"]}"#,
            r#"{"uhs":"opaque-fixture"}"#,
            r#"{"userCode":"opaque-fixture"}"#,
            "deviceCode = opaque-fixture",
            "identity_token: opaque-fixture",
            "X-Api-Key: opaque-fixture",
            r#"{"apiKey":"opaque-fixture"}"#,
            "API key\n = opaque-fixture",
        ] {
            assert_eq!(
                redact(message),
                "[redacted sensitive log message]",
                "{message}"
            );
        }
        assert_eq!(redact("shader tokenCount: 8"), "shader tokenCount: 8");
    }

    #[test]
    fn rotation_waits_until_a_diagnostic_reader_finishes() {
        use std::{sync::mpsc, thread, time::Duration};
        let dir = std::env::temp_dir().join(format!("horizon-log-reader-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("latest.log");
        fs::write(&path, "[12:00:00] [Render/INFO]: initial\n").unwrap();
        let reader = File::open(&path).unwrap();
        reader.lock_shared().unwrap();
        let mut writer = File::options().read(true).write(true).open(&path).unwrap();
        let worker_dir = dir.clone();
        let (ready, waiting) = mpsc::channel();
        let (done, completed) = mpsc::channel();
        let worker = thread::spawn(move || {
            ready.send(()).unwrap();
            done.send(rotate(&mut writer, &worker_dir)).unwrap();
        });
        waiting.recv().unwrap();
        let early = completed.recv_timeout(Duration::from_millis(50));
        reader.unlock().unwrap();
        worker.join().unwrap();
        assert!(matches!(early, Err(mpsc::RecvTimeoutError::Timeout)));
        completed.recv().unwrap().unwrap();
        assert_eq!(fs::metadata(path).unwrap().len(), 0);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_failed_rotation_leaves_both_logs_intact_and_can_still_append_below_the_limit() {
        let dir = std::env::temp_dir().join(format!("horizon-log-blocked-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let previous = dir.join("previous.log");
        fs::create_dir(&previous).unwrap();
        fs::write(previous.join("keep"), "older data").unwrap();
        let path = dir.join("latest.log");
        let initial = "[12:00:00] [Render/INFO]: interrupted entry";
        fs::write(&path, initial).unwrap();
        let mut file = File::options().read(true).write(true).open(&path).unwrap();

        assert!(rotate(&mut file, &dir).is_err());
        assert_eq!(
            fs::read_to_string(previous.join("keep")).unwrap(),
            "older data"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), initial);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);

        file.seek(SeekFrom::Start(0)).unwrap();
        append(&mut file, &dir, "[12:00:01] [Render/INFO]: new entry").unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("{initial}\n[12:00:01] [Render/INFO]: new entry\n")
        );
        drop(file);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_blocked_archive_does_not_stop_logging_at_the_size_limit() {
        let dir = tempfile::tempdir().unwrap();
        let previous = dir.path().join("previous.log");
        fs::create_dir(&previous).unwrap();
        fs::write(previous.join("keep"), "older data").unwrap();
        let path = dir.path().join("latest.log");
        let oldest = "[12:00:00] [Render/INFO]: oldest entry\n";
        let recent = "[12:00:02] [Render/WARN]: keep this recent entry\n";
        let line = format!("[12:00:01] [Render/INFO]: {}\n", "fixture ".repeat(45));
        let middle = line.repeat((LIMIT - oldest.len() - recent.len()) / line.len());
        fs::write(&path, format!("{oldest}{middle}{recent}")).unwrap();
        let mut file = File::options().read(true).write(true).open(&path).unwrap();
        let newest = format!(
            "[12:00:03] [Render/ERROR]: newest failure {}",
            "detail ".repeat(100)
        );

        append(&mut file, dir.path(), &newest).unwrap();
        append(
            &mut file,
            dir.path(),
            "[12:00:04] [Render/INFO]: still recording",
        )
        .unwrap();

        let current = fs::read_to_string(&path).unwrap();
        assert!(current.contains(recent));
        assert!(current.contains("newest failure"));
        assert!(current.contains("still recording"));
        assert!(!current.contains(oldest));
        assert!(current.len() <= LIMIT);
        assert_eq!(
            fs::read_to_string(previous.join("keep")).unwrap(),
            "older data"
        );
    }

    #[test]
    fn a_rotation_with_no_retained_entries_preserves_the_previous_archive() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let current = dir.join("latest.log");
        let previous = dir.join("previous.log");
        let archived = "[12:00:00] [Render/INFO]: previous session\n";
        fs::write(&previous, archived).unwrap();
        fs::write(&current, vec![b'x'; LIMIT + 1]).unwrap();
        let mut file = File::options().read(true).write(true).open(&current).unwrap();

        rotate(&mut file, dir).unwrap();
        assert_eq!(fs::read_to_string(previous).unwrap(), archived);
        assert_eq!(fs::metadata(&current).unwrap().len(), 0);

        append(&mut file, dir, "[12:00:01] [Render/INFO]: current session").unwrap();
        assert!(fs::read_to_string(current).unwrap().contains("current session"));
    }

    #[test]
    fn stale_staging_paths_do_not_block_mobile_log_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let legacy_stage = dir.join(".previous.log.tmp");
        fs::create_dir(&legacy_stage).unwrap();
        let current = dir.join("latest.log");
        let entry = "[12:00:00] [Render/INFO]: previous session\n";
        fs::write(&current, entry).unwrap();
        let mut file = File::options().read(true).write(true).open(&current).unwrap();

        rotate(&mut file, dir).unwrap();
        assert_eq!(fs::read_to_string(dir.join("previous.log")).unwrap(), entry);
        assert!(legacy_stage.is_dir());
        append(&mut file, dir, "[12:00:01] [Render/INFO]: current session").unwrap();
        assert!(fs::read_to_string(current).unwrap().contains("current session"));
        assert_eq!(fs::read_dir(dir).unwrap().count(), 3);
    }

    #[test]
    fn rotation_retains_entries_surrounding_a_sensitive_multiline_message() {
        let dir = std::env::temp_dir().join(format!("horizon-log-entries-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("latest.log");
        let text = concat!(
            "[12:00:00] [Render thread/INFO]: shader setup started\n",
            "[12:00:01] [Render thread/INFO]: access_token:\n",
            "short-fixture-value\n",
            "[12:00:02] [Render thread/WARN]: shader compile failed\n",
            "vertex stage needs a terrain input\n",
        );
        fs::write(&path, text).unwrap();
        let mut file = File::options().read(true).write(true).open(&path).unwrap();
        rotate(&mut file, &dir).unwrap();

        let previous = fs::read_to_string(dir.join("previous.log")).unwrap();
        assert!(previous.contains("shader setup started"));
        assert!(previous.contains("shader compile failed"));
        assert!(previous.contains("vertex stage needs a terrain input"));
        assert!(!previous.contains("short-fixture-value"));
        assert!(!previous.contains("access_token"));
        assert_eq!(file.metadata().unwrap().len(), 0);
        drop(file);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rotation_keeps_the_latest_entries_if_redaction_expands_the_log() {
        let dir =
            std::env::temp_dir().join(format!("horizon-log-expansion-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("latest.log");
        let mut text = ("[12:00:00] [Render thread/INFO]: ".to_owned() + &"x@x ".repeat(10) + "\n")
            .repeat(10_000);
        text.push_str("[12:00:01] [Render thread/WARN]: latest shader diagnostic\n");
        assert!(text.len() < LIMIT);
        assert!(redact_entries(&text).len() > LIMIT);
        fs::write(&path, text).unwrap();
        let mut file = File::options().read(true).write(true).open(&path).unwrap();
        rotate(&mut file, &dir).unwrap();

        let previous = fs::read_to_string(dir.join("previous.log")).unwrap();
        assert!(previous.len() <= LIMIT);
        assert!(previous.lines().next().is_some_and(begins_entry));
        assert!(previous.ends_with("latest shader diagnostic\n"));
        assert!(!previous.contains("x@x"));
        drop(file);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rotating_a_bounded_tail_drops_the_whole_partial_message() {
        let dir = std::env::temp_dir().join(format!(
            "horizon-log-tail-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("latest.log");
        let text = "[12:00:00] [Render thread/INFO]: access_token:\n".to_owned()
            + &"A".repeat(LIMIT)
            + "\n[12:00:00] opaque-fixture-value\n[12:00:01] [Render thread/INFO]: shader ready\n";
        fs::write(&path, text).unwrap();
        let mut file = File::options().read(true).write(true).open(&path).unwrap();
        rotate(&mut file, &dir).unwrap();
        let previous = fs::read_to_string(dir.join("previous.log")).unwrap();
        assert!(!previous.contains("opaque-fixture-value"));
        assert!(previous.contains("shader ready"));
        drop(file);
        fs::remove_dir_all(dir).unwrap();
    }
}
