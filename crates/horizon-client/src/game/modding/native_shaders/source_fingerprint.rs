use std::{collections::HashSet, fs, hash::Hash, path::Path};

pub(super) fn walk(path: &Path, hasher: &mut std::hash::DefaultHasher) -> Option<()> {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;

    let mut pending = vec![path.to_owned()];
    let mut visited = HashSet::new();
    while let Some(path) = pending.pop() {
        let meta = fs::metadata(&path).ok()?;
        path.hash(hasher);
        if meta.is_dir() {
            // Follow linked shader directories, but do not walk the same
            // filesystem directory twice through links or junctions.
            #[cfg(unix)]
            let identity = (meta.dev(), meta.ino());
            #[cfg(not(unix))]
            let identity = fs::canonicalize(&path).ok()?;
            identity.hash(hasher);
            if !visited.insert(identity) {
                continue;
            }
            let mut entries = fs::read_dir(&path)
                .ok()?
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<Result<Vec<_>, _>>()
                .ok()?;
            entries.sort_unstable();
            pending.extend(entries.into_iter().rev());
        } else {
            meta.len().hash(hasher);
            meta.modified().ok()?.hash(hasher);
            // Replacing a ZIP can preserve both size and mtime.
            #[cfg(unix)]
            {
                meta.dev().hash(hasher);
                meta.ino().hash(hasher);
                meta.ctime().hash(hasher);
                meta.ctime_nsec().hash(hasher);
            }
        }
    }
    Some(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{
        fs,
        hash::{DefaultHasher, Hasher},
    };

    #[test]
    fn replacing_a_pack_changes_its_stamp_even_with_the_same_size_and_mtime() {
        let dir = std::env::temp_dir().join(format!("horizon-source-stamp-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let zip = dir.join("shader.zip");
        let replacement = dir.join("replacement.zip");
        let stamp = |path: &Path| {
            let mut hash = DefaultHasher::new();
            walk(path, &mut hash).unwrap();
            hash.finish()
        };
        fs::write(&zip, b"old").unwrap();
        let old = stamp(&zip);
        let mtime = fs::metadata(&zip).unwrap().modified().unwrap();
        fs::write(&replacement, b"new").unwrap();
        fs::File::open(&replacement)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        fs::rename(replacement, &zip).unwrap();
        assert_eq!(fs::metadata(&zip).unwrap().modified().unwrap(), mtime);
        assert_ne!(stamp(&zip), old);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn linked_shader_directories_are_fingerprinted_without_following_cycles() {
        use std::os::unix::fs::symlink;
        let dir = std::env::temp_dir().join(format!("horizon-source-link-{}", std::process::id()));
        let pack = dir.join("pack");
        let assets = dir.join("assets");
        fs::create_dir_all(&pack).unwrap();
        fs::create_dir(&assets).unwrap();
        let shader = assets.join("final.fsh");
        fs::write(&shader, "first source").unwrap();
        symlink(&assets, pack.join("linked")).unwrap();
        symlink(&pack, assets.join("cycle")).unwrap();
        let stamp = || {
            let mut hash = DefaultHasher::new();
            walk(&pack, &mut hash).unwrap();
            hash.finish()
        };
        let old = stamp();
        assert_eq!(stamp(), old);
        fs::write(&shader, "replacement source").unwrap();
        assert_ne!(stamp(), old);
        fs::remove_dir_all(dir).unwrap();
    }
}
