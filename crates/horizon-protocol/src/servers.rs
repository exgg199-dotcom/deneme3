//! The multiplayer server list, stored like vanilla's ServerList in an
//! uncompressed `servers.dat`: `{servers: [{name, ip, icon?, acceptTextures?,
//! acceptedCodeOfConduct?, hidden}]}`. Hidden entries (servers joined by
//! Direct Connection or quick play) are not shown but keep their settings.
use horizon_core::{
    nbt,
    nbt::{JavaString, NamedTag, Tag},
};
use std::path::Path;

#[derive(Clone, Debug, PartialEq)]
pub struct ServerEntry {
    pub name: String,
    pub ip: String,
    /// Base64 PNG favicon from the last status ping.
    pub icon: Option<String>,
    /// ServerPackStatus: None = prompt, Some(true) enabled, Some(false) disabled.
    pub accept_textures: Option<bool>,
    /// `String.hashCode` of the Code of Conduct accepted for good (ServerData's
    /// `acceptedCodeOfConduct`); 0 for none.
    pub accepted_code_of_conduct: i32,
}
impl ServerEntry {
    pub fn new(name: &str, ip: &str) -> Self {
        Self {
            name: name.to_owned(),
            ip: ip.to_owned(),
            icon: None,
            accept_textures: None,
            accepted_code_of_conduct: 0,
        }
    }
    fn to_tag(&self) -> Tag {
        let mut entries = vec![
            ("name", Tag::String(JavaString::from(self.name.as_str()))),
            ("ip", Tag::String(JavaString::from(self.ip.as_str()))),
        ];
        if let Some(icon) = &self.icon {
            entries.push(("icon", Tag::String(JavaString::from(icon.as_str()))));
        }
        if let Some(accept) = self.accept_textures {
            entries.push(("acceptTextures", Tag::Byte(i8::from(accept))));
        }
        if self.accepted_code_of_conduct != 0 {
            entries.push((
                "acceptedCodeOfConduct",
                Tag::Int(self.accepted_code_of_conduct),
            ));
        }
        Tag::compound(entries)
    }
    fn from_tag(tag: &Tag) -> Option<Self> {
        let string = |k: &str| match tag.get(k) {
            Some(Tag::String(s)) => Some(s.to_string_lossy()),
            _ => None,
        };
        Some(Self {
            name: string("name").unwrap_or_default(),
            ip: string("ip")?,
            icon: string("icon"),
            accept_textures: match tag.get("acceptTextures") {
                Some(Tag::Byte(b)) => Some(*b != 0),
                _ => None,
            },
            accepted_code_of_conduct: match tag.get("acceptedCodeOfConduct") {
                Some(Tag::Int(hash)) => *hash,
                _ => 0,
            },
        })
    }
}

/// Held while servers.dat is read and written back, so a ping storing an icon and a list
/// saved by the screen or at startup don't undo each other's change.
pub fn lock() -> std::sync::MutexGuard<'static, ()> {
    static SAVING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SAVING.lock().unwrap_or_else(|e| e.into_inner())
}

/// ServerList.load: the shown entries; missing or unreadable files give an empty list.
pub fn load(path: &Path) -> Vec<ServerEntry> {
    load_checked(path).unwrap_or_default()
}

/// Read visible servers, reporting a corrupt or inaccessible file. A missing
/// file means no servers have been saved yet.
pub fn load_checked(path: &Path) -> std::io::Result<Vec<ServerEntry>> {
    Ok(split_entries(read_root(path)?).0)
}
/// ServerList.load: the shown entries (`serverList`) and the hidden ones
/// (`hiddenServerList`, `hidden` set), each in the file's order.
pub fn load_all(path: &Path) -> (Vec<ServerEntry>, Vec<ServerEntry>) {
    split_entries(read_root(path).ok().flatten())
}

/// Keep missing files distinct from unreadable ones. A list edit may create
/// a missing file, but must never replace data it could not read.
fn read_root(path: &Path) -> std::io::Result<Option<NamedTag>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let (root, _) = nbt::read_named(&bytes, nbt::Limits::default()).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Cannot read servers.dat: {error}"),
        )
    })?;
    if !matches!(&root.tag, Tag::Compound(_))
        || !matches!(root.tag.get("servers"), None | Some(Tag::List(_)))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "servers.dat has an invalid server list",
        ));
    }
    Ok(Some(root))
}

fn split_entries(root: Option<NamedTag>) -> (Vec<ServerEntry>, Vec<ServerEntry>) {
    let (mut shown, mut hidden) = (Vec::new(), Vec::new());
    if let Some(Tag::List(list)) = root.as_ref().and_then(|root| root.tag.get("servers")) {
        for t in list {
            let Some(entry) = ServerEntry::from_tag(t) else {
                continue;
            };
            if is_hidden(t) {
                hidden.push(entry);
            } else {
                shown.push(entry);
            }
        }
    }
    (shown, hidden)
}
fn is_hidden(entry: &Tag) -> bool {
    // Match CompoundTag.getBooleanOr by reading numeric values as bytes.
    match entry.get("hidden") {
        Some(Tag::Byte(n)) => *n != 0,
        Some(Tag::Short(n)) => *n as i8 != 0,
        Some(Tag::Int(n)) => *n as i8 != 0,
        Some(Tag::Long(n)) => *n as i8 != 0,
        _ => false,
    }
}

fn is_visible_server(entry: &Tag) -> bool {
    !is_hidden(entry) && matches!(entry.get("ip"), Some(Tag::String(_)))
}

/// ServerList.save for the shown entries, keeping the hidden ones the file has.
pub fn save(path: &Path, servers: &[ServerEntry]) -> std::io::Result<()> {
    let (_, hidden) = split_entries(read_root(path)?);
    write_list(path, servers, &hidden)
}
/// ServerList.save: the shown entries with `hidden: false`, then the hidden
/// ones with `hidden: true`, written to a temporary file, then replaced.
pub fn save_all(
    path: &Path,
    servers: &[ServerEntry],
    hidden: &[ServerEntry],
) -> std::io::Result<()> {
    read_root(path)?;
    write_list(path, servers, hidden)
}

fn flagged(entry: &ServerEntry, flag: bool) -> Tag {
    let mut tag = entry.to_tag();
    if let Tag::Compound(fields) = &mut tag {
        fields.insert(JavaString::from("hidden"), Tag::Byte(i8::from(flag)));
    }
    tag
}

fn write_list(path: &Path, servers: &[ServerEntry], hidden: &[ServerEntry]) -> std::io::Result<()> {
    let list = servers
        .iter()
        .map(|e| flagged(e, false))
        .chain(hidden.iter().map(|e| flagged(e, true)))
        .collect();
    let root = NamedTag {
        name: JavaString::from(""),
        tag: Tag::compound([("servers", Tag::List(list))]),
    };
    write(path, &root)
}
/// ServerList.add(server, true): a hidden entry goes first, and only the 16
/// newest hidden entries are kept (`MAX_HIDDEN_SERVERS`); then saved.
pub fn add_hidden(path: &Path, entry: ServerEntry) -> std::io::Result<()> {
    edit_list(path, |list| {
        let is_recent = |tag: &Tag| is_hidden(tag) && matches!(tag.get("ip"), Some(Tag::String(_)));
        let first_hidden = list.iter().position(is_recent).unwrap_or(list.len());
        let mut hidden_count = 0;
        list.retain(|tag| {
            if !is_recent(tag) {
                return true;
            }
            hidden_count += 1;
            // Leave room for the new entry at the front of the hidden list.
            hidden_count <= 15
        });
        list.insert(first_hidden, flagged(&entry, true));
        Ok(())
    })
}
/// Change only the icon field on visible entries for this address. The caller
/// holds the server-list lock across this read and write.
pub fn update_visible_icon(path: &Path, address: &str, icon: Option<&str>) -> std::io::Result<()> {
    let icon = icon.map(|text| Tag::String(JavaString::from(text)));
    update_address_field(path, address, "icon", icon, false)
}

/// Store a resource-pack decision on the saved server, or its Direct Connect entry.
pub fn update_pack_preference(
    path: &Path,
    address: &str,
    enabled: Option<bool>,
) -> std::io::Result<()> {
    update_address_field(
        path,
        address,
        "acceptTextures",
        enabled.map(|value| Tag::Byte(i8::from(value))),
        true,
    )
}

/// Store a Code of Conduct acknowledgment on the saved server, or its Direct Connect entry.
pub fn update_code_of_conduct(path: &Path, address: &str, accepted: i32) -> std::io::Result<()> {
    let value = (accepted != 0).then_some(Tag::Int(accepted));
    update_address_field(path, address, "acceptedCodeOfConduct", value, true)
}

/// The caller holds the server-list lock. Only the addressed field is changed.
fn update_address_field(
    path: &Path,
    address: &str,
    key: &str,
    value: Option<Tag>,
    hidden_fallback: bool,
) -> std::io::Result<()> {
    let Some(mut root) = read_root(path)? else {
        return Ok(());
    };
    let Some(Tag::List(list)) = root.tag.get_mut("servers") else {
        return Ok(());
    };
    let address = JavaString::from(address);
    let matches_address =
        |entry: &Tag| matches!(entry.get("ip"), Some(Tag::String(ip)) if ip == &address);
    let use_hidden = hidden_fallback
        && !list
            .iter()
            .any(|entry| !is_hidden(entry) && matches_address(entry));
    let key = JavaString::from(key);
    let mut changed = false;
    for entry in list
        .iter_mut()
        .filter(|entry| is_hidden(entry) == use_hidden && matches_address(entry))
    {
        let Tag::Compound(fields) = entry else {
            continue;
        };
        if fields.get(&key) != value.as_ref() {
            if let Some(value) = &value {
                fields.insert(key.clone(), value.clone());
            } else {
                fields.remove(&key);
            }
            changed = true;
        }
    }
    if changed {
        write(path, &root)?;
    }
    Ok(())
}

/// Puts `entry` first, keeping every other entry and field as the file has them (the hidden
/// ones too). A file that is there but can't be read is left as it is.
pub fn prepend(path: &Path, entry: &ServerEntry) -> std::io::Result<()> {
    edit_list(path, |list| {
        list.insert(0, entry.to_tag());
        Ok(())
    })
}
/// Add an entry at the end of the visible list and return its visible row index.
pub fn append_visible(path: &Path, entry: &ServerEntry) -> std::io::Result<usize> {
    edit_list(path, |list| {
        let index = list.iter().filter(|tag| is_visible_server(tag)).count();
        list.push(flagged(entry, false));
        Ok(index)
    })
}

/// Remove the visible row only if it still has the expected name and address.
pub fn remove_visible_at(path: &Path, row: usize, expected: &ServerEntry) -> std::io::Result<()> {
    edit_list(path, |list| {
        let Some(index) = matching_visible_index(list, row, expected) else {
            return Err(std::io::Error::other(
                "The saved server list changed. Select the server again before deleting it.",
            ));
        };
        list.remove(index);
        Ok(())
    })
}

fn matching_visible_index(list: &[Tag], row: usize, expected: &ServerEntry) -> Option<usize> {
    let (index, tag) = list
        .iter()
        .enumerate()
        .filter(|(_, tag)| is_visible_server(tag))
        .nth(row)?;
    let entry = ServerEntry::from_tag(tag)?;
    (entry.name == expected.name && entry.ip == expected.ip).then_some(index)
}

/// Move a visible row within the same visible list, preserving all tags.
pub fn move_visible(
    path: &Path,
    expected: &[ServerEntry],
    mut from: usize,
    to: usize,
) -> std::io::Result<()> {
    edit_list(path, |list| {
        let indices: Vec<_> = list
            .iter()
            .enumerate()
            .filter(|(_, tag)| is_visible_server(tag))
            .map(|(index, _)| index)
            .collect();
        if from >= indices.len()
            || to >= indices.len()
            || indices.len() != expected.len()
            || !indices.iter().zip(expected).all(|(&index, saved)| {
                ServerEntry::from_tag(&list[index])
                    .is_some_and(|entry| entry.name == saved.name && entry.ip == saved.ip)
            })
        {
            return Err(std::io::Error::other(
                "The saved server list changed. Refresh it before moving servers.",
            ));
        }
        while from != to {
            let next = if from < to { from + 1 } else { from - 1 };
            list.swap(indices[from], indices[next]);
            from = next;
        }
        Ok(())
    })
}

/// Save changes from the multiplayer editor while preserving unrelated NBT fields.
pub fn save_editor_entry(
    path: &Path,
    original: Option<(usize, &ServerEntry)>,
    edited: &ServerEntry,
) -> std::io::Result<()> {
    edit_list(path, |list| {
        if let Some((row, expected)) = original {
            let Some(index) = matching_visible_index(list, row, expected) else {
                return Err(std::io::Error::other(
                    "The saved server list changed. Select the server again before editing it.",
                ));
            };
            let Tag::Compound(fields) = &mut list[index] else {
                unreachable!("matching_visible_index found a server compound");
            };
            fields.insert(
                JavaString::from("name"),
                Tag::String(JavaString::from(edited.name.as_str())),
            );
            fields.insert(
                JavaString::from("ip"),
                Tag::String(JavaString::from(edited.ip.as_str())),
            );
            let key = JavaString::from("acceptTextures");
            if let Some(accept) = edited.accept_textures {
                fields.insert(key, Tag::Byte(i8::from(accept)));
            } else {
                fields.remove(&key);
            }
        } else {
            let address = JavaString::from(edited.ip.as_str());
            if let Some(index) = list.iter().position(|tag| {
                is_hidden(tag) && matches!(tag.get("ip"), Some(Tag::String(ip)) if ip == &address)
            }) {
                // Unhide a Direct Connect entry and apply the new name and icon.
                let mut entry = list.remove(index);
                let Tag::Compound(fields) = &mut entry else {
                    unreachable!("the hidden server has an address field");
                };
                fields.insert(
                    JavaString::from("name"),
                    Tag::String(JavaString::from(edited.name.as_str())),
                );
                fields.insert(JavaString::from("hidden"), Tag::Byte(0));
                let key = JavaString::from("icon");
                if let Some(icon) = &edited.icon {
                    fields.insert(key, Tag::String(JavaString::from(icon.as_str())));
                } else {
                    fields.remove(&key);
                }
                list.push(entry);
            } else {
                list.push(flagged(edited, false));
            }
        }
        Ok(())
    })
}

/// Removes every entry `matches` accepts (given its name and address), keeping the other
/// entries and every field as the file has them (the hidden ones too). Whether any went; a
/// missing file has none, and the file is only written when something went.
pub fn remove_where(path: &Path, matches: impl Fn(&str, &str) -> bool) -> std::io::Result<bool> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let mut root = nbt::read_named(&bytes, nbt::Limits::default())
        .map_err(std::io::Error::other)?
        .0;
    let Some(Tag::List(list)) = root.tag.get_mut("servers") else {
        return Ok(false);
    };
    let text = |t: &Tag, key: &str| match t.get(key) {
        Some(Tag::String(s)) => s.to_string_lossy(),
        _ => String::new(),
    };
    let before = list.len();
    list.retain(|t| !matches(&text(t, "name"), &text(t, "ip")));
    if list.len() == before {
        return Ok(false);
    }
    write(path, &root)?;
    Ok(true)
}
/// Edit the original list so incidental changes keep fields Horizon does not read.
fn edit_list<T>(
    path: &Path,
    edit: impl FnOnce(&mut Vec<Tag>) -> std::io::Result<T>,
) -> std::io::Result<T> {
    let mut root = read_root(path)?.unwrap_or_else(|| NamedTag {
        name: JavaString::from(""),
        tag: Tag::Compound(Default::default()),
    });
    let Tag::Compound(top) = &mut root.tag else {
        return Err(std::io::Error::other("servers.dat is not a compound"));
    };
    let Tag::List(list) = top
        .entry(JavaString::from("servers"))
        .or_insert_with(|| Tag::List(Vec::new()))
    else {
        return Err(std::io::Error::other("servers.dat has no server list"));
    };
    let result = edit(list)?;
    write(path, &root)?;
    Ok(result)
}
fn write(path: &Path, root: &NamedTag) -> std::io::Result<()> {
    let bytes = nbt::write_named(root, nbt::Limits::default()).map_err(std::io::Error::other)?;
    use std::io::Write;

    let directory = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut staging = tempfile::Builder::new();
    staging.prefix(".horizon-servers-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Match an ordinary file creation, subject to the player's umask.
        staging.permissions(std::fs::Permissions::from_mode(0o666));
    }
    let mut temporary = staging.tempfile_in(directory)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

/// ServerAddress.parseString: `host`, `host:port` or `[v6]:port`; default 25565.
pub fn parse_address(text: &str) -> Option<(String, u16)> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Some(rest) = text.strip_prefix('[') {
        let (host, tail) = rest.rsplit_once(']')?;
        // HostAndPort requires a colon inside a bracketed host; it does not
        // accept a bracketed DNS name as an IPv6-style address.
        if !host.contains(':') {
            return None;
        }
        let port = match tail.strip_prefix(':') {
            Some(p) if p.bytes().all(|byte| byte.is_ascii_digit()) => parse_port(p)?,
            None if tail.is_empty() => 25565,
            _ => return None,
        };
        return Some((host.to_owned(), port));
    }
    match text.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => {
            if host.is_empty() {
                return None;
            }
            Some((host.to_owned(), parse_port(port)?))
        }
        Some(_) => Some((text.to_owned(), 25565)),
        None => Some((text.to_owned(), 25565)),
    }
}

/// ServerAddress.isValidAddress, which the Add/Edit Server screen's Done button
/// and the Direct Connection screen's Join Server button ask: the text parses as
/// a host (and port) and `java.net.IDN.toASCII` accepts the host. Only a host
/// made of ASCII is checked here (an empty label other than the last one, or a
/// label over 63 characters, is refused); a host with a non-ASCII character is
/// taken as valid, because the nameprep and punycode steps are not ported.
pub fn is_valid_address(text: &str) -> bool {
    parse_address(text).is_some_and(|(host, _)| idn_accepts(&host))
}

/// The ASCII half of `java.net.IDN.toASCII`: one trailing dot is allowed (a lone
/// dot too), every other label must be 1 to 63 characters long.
fn idn_accepts(host: &str) -> bool {
    if !host.is_ascii() {
        return true;
    }
    let labels = host.strip_suffix('.').unwrap_or(host);
    labels.is_empty() || labels.split('.').all(|l| (1..=63).contains(&l.len()))
}

/// HostAndPort uses the default for an empty port, refuses a leading plus,
/// and accepts signed zero on its non-bracketed path before checking range.
fn parse_port(text: &str) -> Option<u16> {
    if text.is_empty() {
        Some(25565)
    } else if text.starts_with('+') {
        None
    } else {
        u16::try_from(text.parse::<i32>().ok()?).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn servers_dat_round_trips() {
        let dir = std::env::temp_dir().join(format!("horizon-servers-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("servers.dat");
        let list = vec![
            ServerEntry::new("Minecraft Server", "localhost:25565"),
            ServerEntry {
                accept_textures: Some(false),
                ..ServerEntry::new("Other", "example.org")
            },
        ];
        save(&path, &list).unwrap();
        assert_eq!(load(&path), list);
        // Vanilla layout: an unnamed root compound with a "servers" list.
        let (root, _) =
            nbt::read_named(&std::fs::read(&path).unwrap(), nbt::Limits::default()).unwrap();
        assert_eq!(root.name, JavaString::from(""));
        assert!(matches!(root.tag.get("servers"), Some(Tag::List(l)) if l.len() == 2));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn prepend_keeps_hidden_entries_and_unknown_fields() {
        let dir = std::env::temp_dir().join(format!("horizon-prepend-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("servers.dat");
        prepend(&path, &ServerEntry::new("First", "a.example")).unwrap();
        assert_eq!(load(&path), [ServerEntry::new("First", "a.example")]);
        let hidden = Tag::compound([
            ("name", Tag::String(JavaString::from("Direct"))),
            ("ip", Tag::String(JavaString::from("b.example"))),
            ("hidden", Tag::Byte(1)),
            ("acceptedCodeOfConduct", Tag::String(JavaString::from("x"))),
        ]);
        let root = NamedTag {
            name: JavaString::from(""),
            tag: Tag::compound([("servers", Tag::List(vec![hidden.clone()]))]),
        };
        write(&path, &root).unwrap();
        prepend(&path, &ServerEntry::new("Top", "c.example")).unwrap();
        let (root, _) =
            nbt::read_named(&std::fs::read(&path).unwrap(), nbt::Limits::default()).unwrap();
        let Some(Tag::List(list)) = root.tag.get("servers") else {
            panic!("a server list");
        };
        assert_eq!(
            list,
            &[ServerEntry::new("Top", "c.example").to_tag(), hidden]
        );
        // A file that can't be read stays as it is.
        std::fs::write(&path, b"not nbt").unwrap();
        assert!(prepend(&path, &ServerEntry::new("Top", "c.example")).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"not nbt");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn edits_keep_an_unreadable_server_file_untouched() {
        let dir = std::env::temp_dir().join(format!("horizon-invalid-servers-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("servers.dat");
        let added = ServerEntry::new("New", "example.org");
        let wrong_shape = NamedTag {
            name: JavaString::from(""),
            tag: Tag::compound([("servers", Tag::Int(7))]),
        };
        let malformed_list = nbt::write_named(&wrong_shape, nbt::Limits::default()).unwrap();
        for original in [b"truncated NBT".as_slice(), &malformed_list] {
            std::fs::write(&path, original).unwrap();
            assert_eq!(
                load_checked(&path).unwrap_err().kind(),
                std::io::ErrorKind::InvalidData
            );
            for edit in [
                |p: &Path, e: &ServerEntry| save(p, std::slice::from_ref(e)),
                |p: &Path, e: &ServerEntry| save_all(p, std::slice::from_ref(e), &[]),
                |p: &Path, e: &ServerEntry| add_hidden(p, e.clone()),
            ] {
                std::fs::write(&path, original).unwrap();
                let error = edit(&path, &added).unwrap_err();
                assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
                assert_eq!(std::fs::read(&path).unwrap(), original);
            }
            std::fs::remove_file(&path).unwrap();
            assert!(load_checked(&path).unwrap().is_empty());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn icon_updates_preserve_direct_connect_entries_and_unknown_fields() {
        let dir = std::env::temp_dir().join(format!("horizon-icon-update-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("servers.dat");
        let address = "example.org";
        let first = Tag::compound([
            ("name", Tag::String(JavaString::from("First"))),
            ("ip", Tag::String(JavaString::from(address))),
            ("icon", Tag::String(JavaString::from("old"))),
            ("futureField", Tag::Int(42)),
        ]);
        let hidden = Tag::compound([
            ("name", Tag::String(JavaString::from("Direct"))),
            ("ip", Tag::String(JavaString::from(address))),
            ("hidden", Tag::Byte(1)),
            ("futureHiddenField", Tag::Int(13)),
        ]);
        let other = ServerEntry::new("Other", "other.example.org").to_tag();
        let duplicate = ServerEntry::new("Duplicate", address).to_tag();
        let mut root = NamedTag {
            name: JavaString::from(""),
            tag: Tag::compound([
                ("futureRootField", Tag::Int(7)),
                ("servers", Tag::List(vec![first, hidden, other, duplicate])),
            ]),
        };
        write(&path, &root).unwrap();
        for icon in [Some("replacement"), None] {
            update_visible_icon(&path, address, icon).unwrap();
            let Some(Tag::List(list)) = root.tag.get_mut("servers") else {
                panic!("fixture has a server list");
            };
            for index in [0, 3] {
                let Tag::Compound(fields) = &mut list[index] else {
                    panic!("fixture has server entries");
                };
                let key = JavaString::from("icon");
                if let Some(icon) = icon {
                    fields.insert(key, Tag::String(JavaString::from(icon)));
                } else {
                    fields.remove(&key);
                }
            }
            let bytes = std::fs::read(&path).unwrap();
            assert_eq!(nbt::read_named(&bytes, nbt::Limits::default()).unwrap().0, root);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn remembering_a_direct_join_preserves_metadata_and_only_trims_hidden_history() {
        let dir = std::env::temp_dir().join(format!("horizon-direct-join-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("servers.dat");
        let added = ServerEntry::new("New hidden", "new.example");
        add_hidden(&path, added.clone()).unwrap();
        assert_eq!(load_all(&path), (vec![], vec![added.clone()]));

        let visible = |id| Tag::compound([
            ("name", Tag::String(JavaString::from("Saved"))),
            ("ip", Tag::String(JavaString::from("saved.example"))),
            ("futureVisibleField", Tag::Int(id)),
        ]);
        let hidden = |id| Tag::compound([
            ("name", Tag::String(JavaString::from("Direct"))),
            ("ip", Tag::String(JavaString::from(format!("direct-{id}.example").as_str()))),
            ("hidden", Tag::Byte(1)),
            ("futureHiddenField", Tag::Int(id)),
        ]);
        let malformed = Tag::compound([("hidden", Tag::Byte(1)), ("futureField", Tag::Int(8))]);
        let list: Vec<_> = [visible(1), hidden(0), visible(2), malformed]
            .into_iter()
            .chain((1..17).map(hidden))
            .chain([visible(3)])
            .collect();
        let mut root = NamedTag {
            name: JavaString::from(""),
            tag: Tag::compound([
                ("futureRootField", Tag::Int(7)),
                ("servers", Tag::List(list)),
            ]),
        };
        write(&path, &root).unwrap();

        add_hidden(&path, added.clone()).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let (actual, _) = nbt::read_named(&bytes, nbt::Limits::default()).unwrap();
        let Some(Tag::List(expected)) = root.tag.get_mut("servers") else {
            panic!("fixture has a server list");
        };
        let new = Tag::compound([
            ("name", Tag::String(JavaString::from(added.name.as_str()))),
            ("ip", Tag::String(JavaString::from(added.ip.as_str()))),
            ("hidden", Tag::Byte(1)),
        ]);
        expected.retain(|entry| entry != &hidden(15) && entry != &hidden(16));
        expected.insert(1, new);
        assert_eq!(actual, root);
        let (shown, recent) = load_all(&path);
        assert_eq!(shown.len(), 3);
        assert_eq!(recent.len(), 16);
        assert_eq!(recent[0], added);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn consent_updates_preserve_other_fields_and_use_hidden_entries_only_as_fallback() {
        let dir =
            std::env::temp_dir().join(format!("horizon-server-consent-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("servers.dat");
        let entry = |address: &str, hidden: i8| {
            Tag::compound([
                ("ip", Tag::String(JavaString::from(address))),
                ("hidden", Tag::Byte(hidden)),
                ("futureField", Tag::Int(42)),
            ])
        };
        let shared = "shared.example";
        let direct = "direct.example";
        let mut root = NamedTag {
            name: JavaString::from(""),
            tag: Tag::compound([
                ("futureRootField", Tag::Int(7)),
                (
                    "servers",
                    Tag::List(vec![entry(shared, 1), entry(shared, 0), entry(direct, 1)]),
                ),
            ]),
        };
        write(&path, &root).unwrap();
        update_pack_preference(&path, shared, Some(false)).unwrap();
        update_code_of_conduct(&path, direct, 123).unwrap();
        update_pack_preference(&path, "missing.example", Some(true)).unwrap();

        let Some(Tag::List(list)) = root.tag.get_mut("servers") else {
            panic!("fixture has a list");
        };
        let Tag::Compound(visible) = &mut list[1] else {
            panic!("visible server")
        };
        visible.insert(JavaString::from("acceptTextures"), Tag::Byte(0));
        let Tag::Compound(hidden) = &mut list[2] else {
            panic!("hidden server")
        };
        hidden.insert(JavaString::from("acceptedCodeOfConduct"), Tag::Int(123));
        assert_eq!(read_root(&path).unwrap().unwrap(), root);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn visible_list_edits_preserve_hidden_entries_metadata_and_visible_indices() {
        let dir = std::env::temp_dir().join(format!("horizon-visible-edit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("servers.dat");
        let visible = ServerEntry::new("Saved", "saved.example");
        let direct = ServerEntry::new("Direct", "direct.example");
        let appended = ServerEntry::new("Added", "added.example");
        let mut hidden = flagged(&direct, true);
        let Tag::Compound(fields) = &mut hidden else {
            panic!("hidden server")
        };
        fields.insert(JavaString::from("futureField"), Tag::Int(42));
        let malformed = Tag::compound([("futureMalformedField", Tag::Int(3))]);
        let original = NamedTag {
            name: JavaString::from(""),
            tag: Tag::compound([
                ("futureRootField", Tag::Int(7)),
                (
                    "servers",
                    Tag::List(vec![hidden.clone(), malformed.clone(), visible.to_tag()]),
                ),
            ]),
        };
        write(&path, &original).unwrap();
        assert_eq!(append_visible(&path, &appended).unwrap(), 1);
        let after_append = std::fs::read(&path).unwrap();
        assert!(remove_visible_at(&path, 0, &appended).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), after_append);
        remove_visible_at(&path, 0, &visible).unwrap();
        let expected = NamedTag {
            name: JavaString::from(""),
            tag: Tag::compound([
                ("futureRootField", Tag::Int(7)),
                (
                    "servers",
                    Tag::List(vec![hidden, malformed, flagged(&appended, false)]),
                ),
            ]),
        };
        assert_eq!(read_root(&path).unwrap().unwrap(), expected);
        assert_eq!(load_all(&path), (vec![appended], vec![direct]));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn server_editor_rejects_a_stale_row_and_preserves_existing_entry_fields() {
        let dir =
            std::env::temp_dir().join(format!("horizon-server-editor-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("servers.dat");
        let original = ServerEntry::new("Saved", "saved.example");
        let direct = ServerEntry::new("Direct", "direct.example");
        let mut saved = original.to_tag();
        let Tag::Compound(fields) = &mut saved else {
            panic!("saved entry")
        };
        fields.insert(JavaString::from("futureField"), Tag::Int(42));
        let root = NamedTag {
            name: JavaString::from(""),
            tag: Tag::compound([
                ("futureRootField", Tag::Int(7)),
                ("servers", Tag::List(vec![flagged(&direct, true), saved])),
            ]),
        };
        write(&path, &root).unwrap();
        let before = std::fs::read(&path).unwrap();
        let mut edited = ServerEntry::new("Renamed", "saved.example");
        edited.accept_textures = Some(true);
        assert!(save_editor_entry(&path, Some((0, &direct)), &edited).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        save_editor_entry(&path, Some((0, &original)), &edited).unwrap();
        let actual = read_root(&path).unwrap().unwrap();
        assert_eq!(actual.tag.get("futureRootField"), Some(&Tag::Int(7)));
        let Some(Tag::List(entries)) = actual.tag.get("servers") else {
            panic!("server list")
        };
        assert_eq!(entries[1].get("futureField"), Some(&Tag::Int(42)));
        assert_eq!(
            load_all(&path),
            (vec![edited.clone()], vec![direct.clone()])
        );
        save_editor_entry(&path, None, &ServerEntry::new("Promoted", "direct.example")).unwrap();
        assert_eq!(
            load_all(&path),
            (
                vec![edited, ServerEntry::new("Promoted", "direct.example")],
                vec![]
            )
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn moving_visible_rows_keeps_saved_metadata_and_rejects_a_stale_order() {
        let dir = std::env::temp_dir().join(format!("horizon-server-move-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("servers.dat");
        let [a, b, c] = ["A", "B", "C"].map(|name| ServerEntry::new(name, "shared.example"));
        let mut first = a.to_tag();
        let Tag::Compound(fields) = &mut first else {
            panic!("fixture server")
        };
        fields.insert(JavaString::from("futureField"), Tag::Int(7));
        let hidden = flagged(&ServerEntry::new("Direct", "direct.example"), true);
        let root = |visible: [Tag; 3]| NamedTag {
            name: JavaString::from(""),
            tag: Tag::compound([(
                "servers",
                Tag::List(vec![
                    visible[0].clone(),
                    hidden.clone(),
                    visible[1].clone(),
                    visible[2].clone(),
                ]),
            )]),
        };
        let initial = root([first.clone(), b.to_tag(), c.to_tag()]);
        write(&path, &initial).unwrap();
        let expected = [a.clone(), b.clone(), c.clone()];
        move_visible(&path, &expected, 0, 2).unwrap();
        let moved = root([b.to_tag(), c.to_tag(), first]);
        assert_eq!(read_root(&path).unwrap().unwrap(), moved);

        let before = std::fs::read(&path).unwrap();
        assert!(move_visible(&path, &expected, 1, 0).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        move_visible(&path, &[b, c, a], 2, 0).unwrap();
        assert_eq!(read_root(&path).unwrap().unwrap(), initial);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stale_staging_entries_do_not_block_server_saves() {
        let dir = std::env::temp_dir().join(format!("horizon-stale-server-write-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("servers.dat");
        let before = [ServerEntry::new("Before", "before.example")];
        let after = [ServerEntry::new("After", "after.example")];
        save(&path, &before).unwrap();
        let stale = path.with_extension(format!("dat_tmp.{}", std::process::id()));
        std::fs::create_dir(&stale).unwrap();

        save(&path, &after).unwrap();
        assert_eq!(load(&path), after);
        assert!(stale.is_dir());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_failed_server_file_replacement_cleans_up_its_stage() {
        let dir = std::env::temp_dir().join(format!("horizon-failed-server-write-{}", std::process::id()));
        let destination = dir.join("servers.dat");
        std::fs::create_dir_all(&destination).unwrap();
        let root = NamedTag {
            name: JavaString::from(""),
            tag: Tag::compound([("servers", Tag::List(vec![]))]),
        };
        assert!(write(&destination, &root).is_err());
        assert!(destination.is_dir());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn parses_addresses_like_server_address() {
        assert_eq!(
            parse_address("localhost"),
            Some(("localhost".into(), 25565))
        );
        assert_eq!(
            parse_address("127.0.0.1:25570"),
            Some(("127.0.0.1".into(), 25570))
        );
        assert_eq!(parse_address("[::1]:1234"), Some(("::1".into(), 1234)));
        assert_eq!(parse_address("host:notaport"), None);
        assert_eq!(parse_address(""), None);
    }

    #[test]
    fn empty_explicit_ports_use_the_vanilla_default() {
        for (input, host) in [
            ("example.com:", "example.com"),
            ("127.0.0.1:", "127.0.0.1"),
            ("[::1]:", "::1"),
            ("[2001:db8::1]:", "2001:db8::1"),
            (" \t[::1]:\r\n", "::1"),
        ] {
            assert_eq!(parse_address(input), Some((host.into(), 25565)), "{input}");
        }
    }

    #[test]
    fn ports_and_brackets_keep_vanilla_validation() {
        for input in [
            ":",
            ":25565",
            "[]",
            "[]:",
            "[example.com]",
            "[example.com]:",
            "[::1]junk",
            "example.com:+25565",
            "[::1]:+25565",
            "[::1]:-0",
            "example.com:-1",
            "example.com:65536",
            "[::1]:65536",
            "host:2147483648",
            "host:1_000",
            "host:\u{0661}\u{0662}",
        ] {
            assert_eq!(parse_address(input), None, "{input}");
        }
        assert_eq!(parse_address("host:-0"), Some(("host".into(), 0)));
        assert_eq!(parse_address("host:0"), Some(("host".into(), 0)));
        assert_eq!(parse_address("[::1]:65535"), Some(("::1".into(), 65535)));
    }

    #[test]
    fn the_server_screens_refuse_a_host_idn_refuses() {
        let label63 = "x".repeat(63);
        let label64 = "x".repeat(64);
        // Each verdict was taken from java.net.IDN.toASCII on the same host.
        for host in [
            "localhost".to_owned(),
            "a.b.c".to_owned(),
            "a.b.".to_owned(),
            ".".to_owned(),
            "-a.a-.a_b".to_owned(),
            "::1".to_owned(),
            "127.0.0.1".to_owned(),
            label63.clone(),
            format!("{label63}.{label63}.{label63}.{label63}"),
        ] {
            assert!(is_valid_address(&host), "{host}");
            assert!(is_valid_address(&format!("{host}:25570")), "{host}");
        }
        for host in [
            "a..b".to_owned(),
            ".a".to_owned(),
            "a..".to_owned(),
            "..".to_owned(),
            label64.clone(),
            format!("{label64}."),
            format!("{label63}.{label64}"),
        ] {
            assert!(!is_valid_address(&host), "{host}");
            assert!(!is_valid_address(&format!("{host}:25570")), "{host}");
            // ServerAddress.parseString does not ask IDN, so the parser still reads it.
            assert!(parse_address(&host).is_some(), "{host}");
        }
        // What parse_address refuses stays refused.
        assert!(!is_valid_address(""));
        assert!(!is_valid_address("host:notaport"));
        // A non-ASCII host is not checked here.
        assert!(is_valid_address("m\u{fc}nchen.de"));
    }
}
