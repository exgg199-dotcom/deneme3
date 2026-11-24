//! The folder a new world gets: `WorldCreationUiState.findResultFolder` through
//! `FileUtil.findAvailableName`. Start with Java's UTF-16 limit, then honor the filesystem.

/// `SharedConstants.ILLEGAL_FILE_CHARACTERS`, in its order.
const ILLEGAL_FILE_CHARACTERS: [char; 15] = [
    '/', '\n', '\r', '\t', '\0', '\u{c}', '`', '?', '*', '\\', '<', '>', '|', '"', ':',
];

/// `FileUtil.MAX_FILE_NAME`.
pub(super) const MAX_FILE_NAME: usize = 255;

/// The names `FileUtil.RESERVED_WINDOWS_FILENAMES` refuses once `sanitizeName` has turned every
/// dot into `_` (its `.*\.` and `(?:\..*)?` parts can no longer match).
const RESERVED: [&str; 24] = [
    "COM", "CLOCK$", "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6",
    "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

enum Candidate {
    Available,
    Occupied,
    TooLong,
}

pub(super) fn result_folder(saves: &std::path::Path, name: &str, new_world: &str) -> String {
    result_folder_with(name, new_world, |name| {
        match std::fs::symlink_metadata(saves.join(name)) {
            Ok(_) => Candidate::Occupied,
            Err(error) if name_too_long(&error) => Candidate::TooLong,
            // The actual creation reports other filesystem errors.
            Err(_) => Candidate::Available,
        }
    })
}

fn name_too_long(error: &std::io::Error) -> bool {
    #[cfg(unix)]
    {
        rustix::io::Errno::from_io_error(error) == Some(rustix::io::Errno::NAMETOOLONG)
    }
    #[cfg(windows)]
    {
        const ERROR_FILENAME_EXCED_RANGE: i32 = 206;
        error.raw_os_error() == Some(ERROR_FILENAME_EXCED_RANGE)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = error;
        false
    }
}

/// `findResultFolder(name)`: trim as Java does, use `new_world` if blank, and avoid occupied paths.
/// An invalid copy counter (`Mine ()`) falls back to `World`.
fn result_folder_with(name: &str, new_world: &str, probe: impl Fn(&str) -> Candidate) -> String {
    let name = name.trim_matches(|c: char| c <= ' ');
    let name = if name.is_empty() { new_world } else { name };
    available_name(name, &probe)
        .or_else(|| available_name("World", &probe))
        .unwrap_or_else(|| "World".to_owned())
}

/// `FileUtil.sanitizeName`.
fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if ILLEGAL_FILE_CHARACTERS.contains(&c) || matches!(c, '.' | '/' | '"') {
                '_'
            } else {
                c
            }
        })
        .collect()
}

/// `FileUtil.COPY_COUNTER_PATTERN`, `(?<name>.*) \((?<count>\d*)\)`, matched whole: the name and
/// the digits. `.` matches no line terminator, `\d` only ASCII digits.
fn copy_counter(name: &str) -> Option<(&str, &str)> {
    let inner = name.strip_suffix(')')?;
    let digits = inner.len() - inner.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    let (rest, digits) = inner.split_at(inner.len() - digits);
    let base = rest.strip_suffix(" (")?;
    let line_end = |c: char| matches!(c, '\n' | '\r' | '\u{85}' | '\u{2028}' | '\u{2029}');
    (!base.contains(line_end)).then_some((base, digits))
}

fn java_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `String.substring(0, n)` in UTF-16 units (a pair cut in two becomes U+FFFD).
pub(super) fn java_prefix(s: &str, n: usize) -> String {
    String::from_utf16_lossy(&s.encode_utf16().take(n).collect::<Vec<_>>())
}

/// `FileUtil.findAvailableName(folder, name, "")`: None where Java's `Integer.parseInt` of the
/// count throws.
fn available_name(name: &str, probe: &impl Fn(&str) -> Candidate) -> Option<String> {
    let mut name = sanitize_name(name);
    if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(&name)) {
        name = format!("_{name}_");
    }
    let mut count = 0i32;
    if let Some((base, digits)) = copy_counter(&name) {
        count = digits.parse().ok()?;
        name = base.to_owned();
    }
    let mut byte_limit = false;
    loop {
        let counter = if count == 0 {
            String::new()
        } else {
            format!(" ({count})")
        };
        let room = MAX_FILE_NAME - counter.len();
        let mut candidate = if java_len(&name) > room {
            java_prefix(&name, room)
        } else {
            name.clone()
        };
        if byte_limit {
            candidate.truncate(candidate.floor_char_boundary(room));
        }
        candidate.push_str(&counter);
        match probe(&candidate) {
            Candidate::Available => return Some(candidate),
            Candidate::Occupied => count = count.wrapping_add(1),
            Candidate::TooLong if !byte_limit && candidate.len() > MAX_FILE_NAME => {
                byte_limit = true;
            }
            Candidate::TooLong => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Candidate, MAX_FILE_NAME, result_folder_with};

    fn folder(name: &str, taken: &[&str]) -> String {
        result_folder_with(name, "New World", |f| {
            if taken.contains(&f) {
                Candidate::Occupied
            } else {
                Candidate::Available
            }
        })
    }

    #[test]
    fn multibyte_world_names_retry_within_the_filesystem_limit() {
        let name = "界".repeat(140);
        let first = "界".repeat(85);
        let second = result_folder_with(&name, "New World", |candidate| {
            if candidate.len() > MAX_FILE_NAME {
                Candidate::TooLong
            } else if candidate == first {
                Candidate::Occupied
            } else {
                Candidate::Available
            }
        });
        assert_eq!(second, format!("{} (1)", "界".repeat(83)));
        assert_eq!(folder(&name, &[]), name);
    }

    #[cfg(unix)]
    #[test]
    fn world_folder_selection_skips_dangling_links() {
        let saves = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(saves.path().join("missing"), saves.path().join("World"))
            .unwrap();

        let folder = super::result_folder(saves.path(), "World", "New World");
        assert_eq!(folder, "World (1)");
        std::fs::create_dir(saves.path().join(folder)).unwrap();
        assert!(!saves.path().join("missing").exists());
    }

    #[test]
    fn a_new_worlds_folder_is_named_as_vanilla_names_it() {
        // A blank name takes the translated "New World", trimmed as String.trim does.
        assert_eq!(folder("", &[]), "New World");
        assert_eq!(folder(" \t ", &[]), "New World");
        assert_eq!(folder("  Base  ", &[]), "Base");
        // sanitizeName: only ILLEGAL_FILE_CHARACTERS and . / " become _.
        assert_eq!(folder("My.World/Test", &[]), "My_World_Test");
        assert_eq!(folder("Bob's world! #1", &[]), "Bob's world! #1");
        assert_eq!(folder("a:b*c?d", &[]), "a_b_c_d");
        // A name Windows reserves is wrapped in underscores (isPathPartPortable).
        assert_eq!(folder("con", &[]), "_con_");
        assert_eq!(folder("LPT1", &[]), "_LPT1_");
        // A name already taken counts up from 1; a name that ends in a count goes on from it.
        assert_eq!(folder("Hello", &["Hello"]), "Hello (1)");
        assert_eq!(folder("Hello", &["Hello", "Hello (1)"]), "Hello (2)");
        assert_eq!(folder("World (3)", &[]), "World (3)");
        assert_eq!(folder("World (3)", &["World (3)"]), "World (4)");
        // An empty count can't be parsed: findResultFolder falls back to "World".
        assert_eq!(folder("Mine ()", &[]), "World");
        // At most 255 characters, the count included.
        let long = "a".repeat(300);
        assert_eq!(folder(&long, &[]), "a".repeat(255));
        let first = "a".repeat(255);
        assert_eq!(
            folder(&long, &[first.as_str()]),
            format!("{} (1)", "a".repeat(251))
        );
    }
}
