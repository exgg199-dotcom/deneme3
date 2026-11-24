//! Anvil region reading, export to a new region, and in-place chunk writes
//! as vanilla's `RegionFile` makes them (a file opened with `open_writable`).
//! Exports never overwrite a file: use a separate directory for offline
//! edits and copied .mcc data.
use horizon_core::position::ChunkPos;
use std::{
    cell::RefCell,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU32, Ordering},
};
use {
    crate::compression::{Compression, invalid},
    horizon_core::{
        nbt,
        nbt::{Limits, NamedTag},
    },
};

const SECTOR: usize = 4096;
const HEADER: usize = SECTOR * 2;
const MAX_COMPRESSED: usize = 64 * 1024 * 1024;

thread_local! {
    /// Decompression storage retained by each region-reading worker.
    static NBT_DECODE_BUFFER: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

#[derive(Clone, Debug)]
pub struct ChunkRecord {
    pub compression: Compression,
    pub compressed: Vec<u8>,
    pub timestamp: u32,
}
impl ChunkRecord {
    pub fn from_nbt(
        root: &NamedTag,
        compression: Compression,
        timestamp: u32,
        limits: Limits,
    ) -> io::Result<Self> {
        let bytes = nbt::write_named(root, limits).map_err(|e| invalid(e.to_string()))?;
        Ok(Self {
            compression,
            compressed: compression.encode(&bytes)?,
            timestamp,
        })
    }
    /// `RegionFileStorage.read`: the root read straight off the decompressing
    /// stream, so bytes after it, and a damaged stream past it, are never read.
    pub fn nbt(&self, limits: Limits) -> io::Result<NamedTag> {
        NBT_DECODE_BUFFER.with_borrow_mut(|bytes| {
            bytes.clear();
            let decoded = self
                .compression
                .decode_into(&self.compressed, limits.max_bytes, bytes);
            match nbt::read_named(bytes, limits) {
                Ok((root, _)) => Ok(root),
                Err(e) => Err(decoded.err().unwrap_or_else(|| invalid(e.to_string()))),
            }
        })
    }
}

pub struct RegionFile {
    file: File,
    writable: bool,
    path: PathBuf,
    position: ChunkPos,
    offsets: [u32; 1024],
    timestamps: [u32; 1024],
    /// The sectors in use (`RegionBitmap`): the header's two and every
    /// chunk's.
    used: Vec<bool>,
    overlapping_offsets: bool,
    /// The first free sector (`RegionBitmap`'s low watermark): allocation
    /// starts its first-fit search here instead of at sector 0.
    first_free: usize,
}

pub fn position_from_path(path: &Path) -> io::Result<ChunkPos> {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| invalid("invalid region filename"))?;
    let parts: Vec<_> = name.split('.').collect();
    if parts.len() != 4 || parts[0] != "r" || parts[3] != "mca" {
        return Err(invalid("expected r.<x>.<z>.mca"));
    }
    let x: i32 = parts[1]
        .parse()
        .map_err(|_| invalid("invalid region x coordinate"))?;
    let z: i32 = parts[2]
        .parse()
        .map_err(|_| invalid("invalid region z coordinate"))?;
    // Regions outside this range cannot be addressed with signed chunk coordinates.
    if !(-67_108_864..=67_108_863).contains(&x) || !(-67_108_864..=67_108_863).contains(&z) {
        return Err(invalid("region coordinate outside chunk coordinate range"));
    }
    Ok(ChunkPos::new(x, z))
}

fn slot(region: ChunkPos, chunk: ChunkPos) -> io::Result<usize> {
    if chunk.region() != region {
        return Err(invalid("chunk does not belong to this region"));
    }
    let (x, z) = chunk.region_local();
    Ok(usize::from(x) + usize::from(z) * 32)
}
fn position(region: ChunkPos, index: usize) -> ChunkPos {
    ChunkPos::new(
        region.x * 32 + (index % 32) as i32,
        region.z * 32 + (index / 32) as i32,
    )
}
fn external_path(directory: &Path, chunk: ChunkPos) -> PathBuf {
    directory.join(format!("c.{}.{}.mcc", chunk.x, chunk.z))
}
/// The header of a region file, from its start: each chunk's place (sector
/// and sector count) and timestamp.
fn read_header(file: &mut File) -> io::Result<([u32; 1024], [u32; 1024], bool)> {
    let len = file.metadata()?.len();
    // RegionFile's constructor: a short header (an empty or truncated file)
    // reads as far as it goes, the rest stays zero (no chunks).
    let mut header = [0; HEADER];
    let mut read = 0;
    while read < HEADER {
        match file.read(&mut header[read..])? {
            0 => break,
            n => read += n,
        }
    }
    let mut offsets = [0; 1024];
    let mut timestamps = [0; 1024];
    for i in 0..1024 {
        offsets[i] = u32::from_be_bytes(header[i * 4..i * 4 + 4].try_into().unwrap());
        timestamps[i] = u32::from_be_bytes(
            header[SECTOR + i * 4..SECTOR + i * 4 + 4]
                .try_into()
                .unwrap(),
        );
        if offsets[i] == 0 {
            continue;
        }
        let start = offsets[i] >> 8;
        let count = offsets[i] & 255;
        // Vanilla drops entries that overlap the header, are empty or start past
        // the end of the file (logged as invalid sectors).
        if start < 2 || count == 0 || u64::from(start) * SECTOR as u64 > len {
            offsets[i] = 0;
        }
    }
    Ok((offsets, timestamps, read == HEADER))
}
/// `RegionFile.getChunkDataInputStream` for the chunk the header puts at
/// `offset`: a chunk the file cannot deliver is absent (`null`), not an
/// error, so a reader skips it the way vanilla does instead of giving up on
/// the whole region.
fn read_record(
    file: &mut File,
    directory: &Path,
    chunk: ChunkPos,
    offset: u32,
    timestamp: u32,
) -> io::Result<Option<ChunkRecord>> {
    if offset == 0 {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(u64::from(offset >> 8) * SECTOR as u64))?;
    let mut header = [0; 5];
    if file.read(&mut header)? < header.len() {
        return Ok(None); // "Chunk header is truncated"
    }
    let len = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
    if len == 0 {
        return Ok(None); // "Chunk is allocated, but stream is missing"
    }
    if len as u64 + 4 > (u64::from(offset & 255) * SECTOR as u64) {
        return Ok(None); // "Chunk stream is truncated"
    }
    let compression = Compression::try_from(header[4] & 127)?;
    let compressed = if header[4] & 128 != 0 {
        // `createExternalChunkInputStream`: the stub's length says nothing
        // about the stream in the `.mcc`, so a stub that also claims inline
        // data only warns; and a stub whose file is gone is a chunk that is
        // not there (`null`), not a damaged one.
        let path = external_path(directory, chunk);
        if !path.is_file() {
            return Ok(None);
        }
        let file = File::open(path)?;
        let mut data = Vec::new();
        file.take((MAX_COMPRESSED + 1) as u64)
            .read_to_end(&mut data)?;
        if data.len() > MAX_COMPRESSED {
            return Err(invalid("external chunk compressed limit exceeded"));
        }
        data
    } else {
        let mut data = vec![0; len - 1];
        match file.read_exact(&mut data) {
            Ok(()) => data,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
    };
    Ok(Some(ChunkRecord {
        compression,
        compressed,
        timestamp,
    }))
}
fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

impl RegionFile {
    pub fn position(&self) -> ChunkPos {
        self.position
    }
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_owned();
        let file = File::open(&path)?;
        Self::from_file(path, file, false)
    }

    /// Open for reading and in-place writes (`write`, `clear`), creating an
    /// empty region with `create`.
    pub fn open_writable(path: impl AsRef<Path>, create: bool) -> io::Result<Self> {
        let path = path.as_ref().to_owned();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .truncate(false)
            .open(&path)?;
        Self::from_file(path, file, true)
    }

    /// Upgrade the shared read-only handle after access is restored.
    /// On failure readers keep the original handle and cached index.
    pub fn ensure_writable(&mut self) -> io::Result<()> {
        if !self.writable {
            *self = Self::open_writable(&self.path, false)?;
        }
        Ok(())
    }

    fn from_file(path: PathBuf, mut file: File, writable: bool) -> io::Result<Self> {
        let position = position_from_path(&path)?;
        let (offsets, timestamps, _) = read_header(&mut file)?;
        let mut region = Self {
            file,
            writable,
            path,
            position,
            offsets,
            timestamps,
            used: Vec::new(),
            overlapping_offsets: false,
            first_free: 0,
        };
        region.force(0, 2);
        for offset in offsets.into_iter().filter(|&o| o != 0) {
            let start = (offset >> 8) as usize;
            let count = (offset & 255) as usize;
            region.overlapping_offsets |= region
                .used
                .get(start..(start + count).min(region.used.len()))
                .is_some_and(|sectors| sectors.contains(&true));
            region.force(start, count);
        }
        region.first_free = region
            .used
            .iter()
            .position(|&u| !u)
            .unwrap_or(region.used.len());
        Ok(region)
    }

    /// `RegionBitmap.force`.
    fn force(&mut self, start: usize, count: usize) {
        if self.used.len() < start + count {
            self.used.resize(start + count, false);
        }
        self.used[start..start + count].fill(true);
        if self.first_free >= start && self.first_free < start + count {
            self.first_free = self.used[start + count..]
                .iter()
                .position(|&u| !u)
                .map_or(self.used.len(), |i| start + count + i);
        }
    }
    /// `RegionBitmap.free`.
    fn free(&mut self, start: usize, count: usize) {
        let end = (start + count).min(self.used.len());
        if start < end {
            self.used[start..end].fill(false);
            self.first_free = self.first_free.min(start);
        }
    }
    /// Release an old index entry after it has been replaced or cleared.
    fn free_old_chunk(&mut self, offset: u32) {
        let start = (offset >> 8) as usize;
        let count = (offset & 255) as usize;
        self.free(start, count);
        if self.overlapping_offsets {
            // A damaged index may still have another readable chunk here.
            for other in self.offsets.into_iter().filter(|&o| o != 0) {
                let other_start = (other >> 8) as usize;
                let shared_start = start.max(other_start);
                let shared_end = (start + count).min(other_start + (other & 255) as usize);
                if shared_start < shared_end {
                    self.force(shared_start, shared_end - shared_start);
                }
            }
        }
    }
    /// `RegionBitmap.allocate`: the first run of free sectors long enough
    /// (the end of the file is free).
    fn allocate(&mut self, count: usize) -> usize {
        let mut current = self.first_free;
        loop {
            let start = (current..self.used.len())
                .find(|&s| !self.used[s])
                .unwrap_or(self.used.len());
            let end = (start..self.used.len()).find(|&s| self.used[s]);
            if end.is_none_or(|end| end - start >= count) {
                self.force(start, count);
                return start;
            }
            current = end.unwrap_or(start);
        }
    }

    /// `RegionFile.write`: the chunk goes to the first free run of sectors
    /// that holds it (an oversized one, 256 sectors or more, to its external
    /// `c.X.Z.mcc` file with a one-sector stub), the header points to it,
    /// and only then are its old sectors freed. The other chunks stay where
    /// they are. A chunk's last sector is padded with zeros.
    pub fn write(&mut self, chunk: ChunkPos, record: &ChunkRecord) -> io::Result<()> {
        self.write_with_index(chunk, record, Self::update_header_entry)
    }

    fn write_with_index(
        &mut self,
        chunk: ChunkPos,
        record: &ChunkRecord,
        update_index: impl FnOnce(&mut Self, usize, u32, u32) -> io::Result<()>,
    ) -> io::Result<()> {
        let i = slot(self.position, chunk)?;
        if record.compressed.len() > MAX_COMPRESSED {
            return Err(invalid("compressed chunk limit exceeded"));
        }
        let required = (record.compressed.len() + 5).div_ceil(SECTOR);
        let external = required >= 256;
        let sectors = if external { 1 } else { required };
        let start = self.allocate(sectors);
        if start + sectors > 0x100_0000 {
            self.free(start, sectors);
            return Err(invalid("region sector address limit exceeded"));
        }
        let directory = self.path.parent().unwrap_or(Path::new(".")).to_owned();
        let external_file = external_path(&directory, chunk);
        let mut data = Vec::with_capacity(sectors * SECTOR);
        if external {
            data.extend_from_slice(&1u32.to_be_bytes());
            data.push(record.compression as u8 | 128);
        } else {
            data.extend_from_slice(&(record.compressed.len() as u32 + 1).to_be_bytes());
            data.push(record.compression as u8);
            data.extend_from_slice(&record.compressed);
        }
        data.resize(sectors * SECTOR, 0);
        let written = self
            .file
            .seek(SeekFrom::Start((start * SECTOR) as u64))
            .and_then(|_| self.file.write_all(&data));
        if let Err(e) = written {
            self.free(start, sectors);
            return Err(e);
        }
        let backup = if external {
            match replace_external(&external_file, &record.compressed) {
                Ok(backup) => backup,
                Err(error) => {
                    self.free(start, sectors);
                    return Err(error);
                }
            }
        } else {
            None
        };
        let old = self.offsets[i];
        if let Err(error) = update_index(
            self,
            i,
            (start as u32) << 8 | sectors as u32,
            record.timestamp,
        ) {
            if external {
                let restored = match &backup {
                    Some(path) => fs::rename(path, &external_file),
                    None => remove_if_exists(&external_file),
                };
                if let Err(restore) = restored {
                    let source = backup.as_deref().unwrap_or(&external_file);
                    return Err(io::Error::new(
                        error.kind(),
                        format!(
                            "{error}; external chunk rollback failed at {}: {restore}",
                            source.display()
                        ),
                    ));
                }
            }
            return Err(error);
        }
        if let Some(path) = backup {
            let _ = fs::remove_file(path);
        }
        // The index no longer refers to the old sectors, even if removing the
        // old external file later fails.
        if old != 0 {
            self.free_old_chunk(old);
        }
        if !external {
            remove_if_exists(&external_file)?;
        }
        Ok(())
    }

    /// `RegionFile.clear`: the chunk is gone; `timestamp` marks when.
    /// Returns whether the region held it.
    pub fn clear(&mut self, chunk: ChunkPos, timestamp: u32) -> io::Result<bool> {
        let i = slot(self.position, chunk)?;
        let old = self.offsets[i];
        if old == 0 {
            return Ok(false);
        }
        self.update_header_entry(i, 0, timestamp)?;
        self.free_old_chunk(old);
        let directory = self.path.parent().unwrap_or(Path::new("."));
        remove_if_exists(&external_path(directory, chunk))?;
        Ok(true)
    }

    /// Flush the writes to the disk.
    pub fn sync(&self) -> io::Result<()> {
        self.file.sync_data()
    }

    fn update_header_entry(&mut self, index: usize, offset: u32, timestamp: u32) -> io::Result<()> {
        let previous = (self.offsets[index], self.timestamps[index]);
        self.offsets[index] = offset;
        self.timestamps[index] = timestamp;
        if let Err(error) = self.write_header() {
            (self.offsets[index], self.timestamps[index]) = previous;
            // A failed write can have changed part of the header on disk.
            // Keep both allocations until reopening if rollback also fails.
            if self.write_header().is_ok() && offset != 0 {
                self.free((offset >> 8) as usize, (offset & 255) as usize);
            }
            return Err(error);
        }
        Ok(())
    }

    /// `RegionFile.writeHeader`: both header sectors from memory, so the
    /// entries dropped as invalid when the file was opened are zero on disk too.
    fn write_header(&mut self) -> io::Result<()> {
        let mut header = [0; HEADER];
        for i in 0..1024 {
            header[i * 4..i * 4 + 4].copy_from_slice(&self.offsets[i].to_be_bytes());
            header[SECTOR + i * 4..SECTOR + i * 4 + 4]
                .copy_from_slice(&self.timestamps[i].to_be_bytes());
        }
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&header)
    }

    pub fn chunks(&self) -> impl Iterator<Item = (ChunkPos, u32)> + '_ {
        self.offsets
            .iter()
            .enumerate()
            .filter(|(_, value)| **value != 0)
            .map(|(i, _)| (position(self.position, i), self.timestamps[i]))
    }

    /// The chunk's record, `None` for a chunk the file does not hold or cannot
    /// deliver (`read_record`).
    pub fn read(&mut self, chunk: ChunkPos) -> io::Result<Option<ChunkRecord>> {
        let i = slot(self.position, chunk)?;
        let directory = self.path.parent().unwrap_or(Path::new("."));
        read_record(
            &mut self.file,
            directory,
            chunk,
            self.offsets[i],
            self.timestamps[i],
        )
    }

    /// Export all chunks, preserving untouched compressed bytes and timestamps.
    /// Overrides are keyed by chunk position; None deletes a chunk in the export.
    pub fn export(
        &mut self,
        destination: impl AsRef<Path>,
        changes: &[(ChunkPos, Option<ChunkRecord>)],
    ) -> io::Result<()> {
        let destination = destination.as_ref();
        if position_from_path(destination)? != self.position {
            return Err(invalid("export region coordinates differ"));
        }
        let mut replacements = BTreeChanges::new();
        for (chunk, record) in changes {
            let i = slot(self.position, *chunk)?;
            if replacements.insert(i, record.as_ref()).is_some() {
                return Err(invalid("duplicate chunk override"));
            }
        }
        let mut writer = RegionWriter::create(destination)?;
        for i in 0..1024 {
            let chunk = position(self.position, i);
            if let Some(record) = replacements.get(&i) {
                if let Some(record) = record {
                    writer.write(chunk, record)?;
                }
            } else if let Some(record) = self.read(chunk)? {
                writer.write(chunk, &record)?;
            }
        }
        writer.finish()
    }
}
type BTreeChanges<'a> = std::collections::BTreeMap<usize, Option<&'a ChunkRecord>>;

/// A region file's header kept without the file open: which chunks the file
/// holds and where, as of when it was read. For a reader of many chunks that
/// must not keep every file open (far terrain: a region file for each 32
/// chunks of the view), so that the header is read once and not once a chunk.
///
/// It goes stale when the file is written: `RegionFile::write` moves the
/// chunk, and frees its old sectors for the next chunk written. Whoever keeps
/// a header must know when that happens.
pub struct RegionHeader {
    path: PathBuf,
    position: ChunkPos,
    offsets: [u32; 1024],
    timestamps: [u32; 1024],
    complete: bool,
}
impl RegionHeader {
    pub fn read(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_owned();
        let position = position_from_path(&path)?;
        let (offsets, timestamps, complete) = read_header(&mut File::open(&path)?)?;
        Ok(Self {
            path,
            position,
            offsets,
            timestamps,
            complete,
        })
    }

    /// A short header can be read, but may only be a snapshot of a file still being written.
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// `RegionFile::read` at the place this header gives the chunk, with the
    /// file opened for this one record.
    pub fn read_chunk(&self, chunk: ChunkPos) -> io::Result<Option<ChunkRecord>> {
        let i = slot(self.position, chunk)?;
        if self.offsets[i] == 0 {
            return Ok(None);
        }
        let directory = self.path.parent().unwrap_or(Path::new("."));
        read_record(
            &mut File::open(&self.path)?,
            directory,
            chunk,
            self.offsets[i],
            self.timestamps[i],
        )
    }
}

static NEXT_TEMP: AtomicU32 = AtomicU32::new(0);

fn temporary_path(directory: &Path) -> PathBuf {
    directory.join(format!(
        ".horizon-{}-{}.tmp",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ))
}

fn temporary_file(directory: &Path) -> io::Result<(PathBuf, File)> {
    loop {
        let path = temporary_path(directory);
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
}

/// Keep the previous .mcc reachable until its replacement's index is written.
fn backup_external(destination: &Path, directory: &Path) -> io::Result<Option<PathBuf>> {
    let metadata = match fs::metadata(destination) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() {
        return Err(invalid("external chunk is not a regular file"));
    }
    loop {
        let backup = temporary_path(directory);
        match fs::hard_link(destination, &backup) {
            Ok(()) => return Ok(Some(backup)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => {
                // Some filesystems used for worlds do not support hard links.
                let mut previous = File::open(destination)?;
                let (backup, mut file) = temporary_file(directory)?;
                let copied = io::copy(&mut previous, &mut file).and_then(|_| file.sync_all());
                if let Err(error) = copied {
                    drop(file);
                    let _ = fs::remove_file(&backup);
                    return Err(error);
                }
                return Ok(Some(backup));
            }
        }
    }
}

fn replace_external(destination: &Path, bytes: &[u8]) -> io::Result<Option<PathBuf>> {
    let directory = destination.parent().unwrap_or(Path::new("."));
    let (staged, mut file) = temporary_file(directory)?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        let backup = backup_external(destination, directory)?;
        if let Err(error) = fs::rename(&staged, destination) {
            if let Some(path) = &backup {
                let _ = fs::remove_file(path);
            }
            return Err(error);
        }
        Ok(backup)
    })();
    if result.is_err() {
        let _ = fs::remove_file(staged);
    }
    result
}

/// Writes a complete new region to a temporary sibling, then publishes it with
/// an atomic, non-overwriting hard link. Drop rolls back unpublished files.
pub struct RegionWriter {
    file: File,
    temporary: PathBuf,
    destination: PathBuf,
    position: ChunkPos,
    offsets: [u32; 1024],
    timestamps: [u32; 1024],
    next_sector: u32,
    externals: Vec<PathBuf>,
    committed: bool,
    failed: bool,
}
impl RegionWriter {
    pub fn create(destination: impl AsRef<Path>) -> io::Result<Self> {
        let destination = destination.as_ref().to_owned();
        let position = position_from_path(&destination)?;
        if destination.try_exists()? {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "destination region already exists",
            ));
        }
        let parent = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        let (temporary, file) = temporary_file(parent)?;
        let mut writer = Self {
            file,
            temporary,
            destination,
            position,
            offsets: [0; 1024],
            timestamps: [0; 1024],
            next_sector: 2,
            externals: Vec::new(),
            committed: false,
            failed: false,
        };
        writer.file.write_all(&[0; HEADER])?;
        Ok(writer)
    }

    pub fn write(&mut self, chunk: ChunkPos, record: &ChunkRecord) -> io::Result<()> {
        if self.failed {
            return Err(invalid("region writer failed previously"));
        }
        let result = self.write_record(chunk, record);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn write_record(&mut self, chunk: ChunkPos, record: &ChunkRecord) -> io::Result<()> {
        let i = slot(self.position, chunk)?;
        if self.offsets[i] != 0 {
            return Err(invalid("chunk already written"));
        }
        if record.compressed.len() > MAX_COMPRESSED {
            return Err(invalid("compressed chunk limit exceeded"));
        }
        let required = (record.compressed.len() + 5).div_ceil(SECTOR);
        let external = required >= 256;
        let sectors = if external { 1 } else { required as u32 };
        if self.next_sector + sectors > 0x100_0000 {
            return Err(invalid("region sector address limit exceeded"));
        }
        if external {
            let path = external_path(self.destination.parent().unwrap_or(Path::new(".")), chunk);
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            self.externals.push(path);
            file.write_all(&record.compressed)?;
            file.sync_all()?;
        }
        self.file
            .seek(SeekFrom::Start(u64::from(self.next_sector) * SECTOR as u64))?;
        let len = if external {
            1
        } else {
            record.compressed.len() as u32 + 1
        };
        self.file.write_all(&len.to_be_bytes())?;
        self.file
            .write_all(&[record.compression as u8 | if external { 128 } else { 0 }])?;
        if !external {
            self.file.write_all(&record.compressed)?;
        }
        self.offsets[i] = self.next_sector << 8 | sectors;
        self.timestamps[i] = record.timestamp;
        self.next_sector += sectors;
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<()> {
        if self.failed {
            return Err(invalid("cannot publish a failed region writer"));
        }
        self.file
            .set_len(u64::from(self.next_sector) * SECTOR as u64)?;
        self.file.seek(SeekFrom::Start(0))?;
        for value in self.offsets {
            self.file.write_all(&value.to_be_bytes())?;
        }
        for value in self.timestamps {
            self.file.write_all(&value.to_be_bytes())?;
        }
        self.file.sync_all()?;
        // Hard-link publication fails atomically if another writer won the name.
        fs::hard_link(&self.temporary, &self.destination)?;
        self.committed = true;
        Ok(())
    }
}
impl Drop for RegionWriter {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.temporary);
        if !self.committed {
            for path in &self.externals {
                let _ = fs::remove_file(path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pre-hint allocator: first fit from sector 0. The hint must keep
    /// producing exactly its placements.
    fn naive_first_fit(used: &[bool], count: usize) -> usize {
        let mut current = 0;
        loop {
            let start = (current..used.len())
                .find(|&s| !used[s])
                .unwrap_or(used.len());
            let end = (start..used.len()).find(|&s| used[s]);
            if end.is_none_or(|end| end - start >= count) {
                return start;
            }
            current = end.unwrap_or(start);
        }
    }

    #[test]
    fn removing_an_overlapping_entry_does_not_release_another_chunks_sectors() {
        let record = |fill| ChunkRecord {
            compression: Compression::None,
            compressed: vec![fill; 8],
            timestamp: u32::from(fill),
        };
        let first = ChunkPos::new(0, 0);
        let neighbor = ChunkPos::new(1, 0);
        let next = ChunkPos::new(2, 0);
        let later = ChunkPos::new(3, 0);
        for replace in [false, true] {
            for alias in [false, true] {
                let dir = std::env::temp_dir().join(format!(
                    "horizon-region-overlap-{replace}-{alias}-{}",
                    std::process::id()
                ));
                fs::create_dir_all(&dir).unwrap();
                let path = dir.join("r.0.0.mca");
                let mut region = RegionFile::open_writable(&path, true).unwrap();
                region.write(first, &record(1)).unwrap();
                region.write(neighbor, &record(2)).unwrap();
                let first_offset = region.offsets[0];
                let neighbor_offset = region.offsets[1];
                let overlapping = if alias {
                    neighbor_offset
                } else {
                    // Claim one sector beyond the first chunk's own record.
                    first_offset + 1
                };
                region.file.seek(SeekFrom::Start(0)).unwrap();
                region.file.write_all(&overlapping.to_be_bytes()).unwrap();
                drop(region);

                let mut region = RegionFile::open_writable(&path, false).unwrap();
                if replace {
                    region.write(first, &record(3)).unwrap();
                } else {
                    assert!(region.clear(first, 3).unwrap());
                }
                region.write(next, &record(4)).unwrap();
                region.write(later, &record(5)).unwrap();
                if !alias {
                    // The part of the range that no entry still owns is reusable.
                    assert_eq!(region.offsets[2], first_offset);
                }
                drop(region);

                let mut region = RegionFile::open(&path).unwrap();
                let restored = region.read(neighbor).unwrap().unwrap();
                assert_eq!(restored.compressed, record(2).compressed);
                assert_eq!(restored.timestamp, 2);
                drop(region);
                fs::remove_dir_all(dir).unwrap();
            }
        }
    }

    #[test]
    fn allocation_matches_first_fit_from_zero() {
        let dir =
            std::env::temp_dir().join(format!("horizon-region-first-free-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("r.0.0.mca");
        let mut region = RegionFile::open_writable(&path, true).unwrap();

        let mut seed = 0x1234_5678u64;
        let mut next = move || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as usize
        };
        for op in 0..120 {
            let slot = next() % 1024;
            let size = match next() % 4 {
                0 => 1, // external: one stub sector
                _ => 2,
            };
            let pos = position(region.position, slot);
            if next() % 5 == 0 {
                assert!(region.clear(pos, op as u32).is_ok());
            } else {
                let record = ChunkRecord {
                    compression: Compression::None,
                    compressed: vec![0; size * SECTOR - 5],
                    timestamp: op as u32,
                };
                assert!(region.write(pos, &record).is_ok());
            }
            // The naive bitmap from what the region placed, under the old rule.
            let mut naive = vec![false; region.used.len()];
            naive[0..2].fill(true);
            for i in 0..1024 {
                let o = region.offsets[i];
                if o != 0 {
                    let s = (o >> 8) as usize;
                    let c = (o & 255) as usize;
                    if naive.len() < s + c {
                        naive.resize(s + c, false);
                    }
                    naive[s..s + c].fill(true);
                }
            }
            assert_eq!(region.used, naive);
            assert_eq!(region.first_free, naive_first_fit(&naive, 1));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_header_write_preserves_the_chunk_index() {
        let dir = std::env::temp_dir().join(format!(
            "horizon-region-clear-failure-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("r.0.0.mca");
        let chunk = ChunkPos::new(0, 0);
        let mut region = RegionFile::open_writable(&path, true).unwrap();
        region
            .write(
                chunk,
                &ChunkRecord {
                    compression: Compression::None,
                    compressed: vec![42; 10],
                    timestamp: 7,
                },
            )
            .unwrap();

        region.file = File::open(&path).unwrap();
        assert!(region.clear(chunk, 9).is_err());
        assert_eq!(region.chunks().collect::<Vec<_>>(), [(chunk, 7)]);
        assert_eq!(
            RegionHeader::read(&path)
                .unwrap()
                .read_chunk(chunk)
                .unwrap()
                .unwrap()
                .compressed,
            vec![42; 10]
        );

        region.file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert!(region.clear(chunk, 9).unwrap());
        assert!(
            RegionHeader::read(&path)
                .unwrap()
                .read_chunk(chunk)
                .unwrap()
                .is_none()
        );
        drop(region);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cleanup_error_does_not_keep_unreferenced_sectors_allocated() {
        let dir = std::env::temp_dir().join(format!(
            "horizon-region-cleanup-failure-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("r.0.0.mca");
        let chunk = ChunkPos::new(0, 0);
        let mut region = RegionFile::open_writable(&path, true).unwrap();
        region
            .write(
                chunk,
                &ChunkRecord {
                    compression: Compression::None,
                    compressed: vec![0; 10],
                    timestamp: 7,
                },
            )
            .unwrap();
        let offset = region.offsets[0];
        // A directory can't be removed as a stale external chunk file.
        fs::create_dir(external_path(&dir, chunk)).unwrap();
        assert!(region.clear(chunk, 9).is_err());
        assert!(region.chunks().next().is_none());
        assert!(!region.used[(offset >> 8) as usize]);
        drop(region);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_external_stub_write_does_not_replace_the_saved_payload() {
        let dir = std::env::temp_dir().join(format!(
            "horizon-region-external-stub-failure-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("r.0.0.mca");
        let chunk = ChunkPos::new(0, 0);
        let record = |fill, compression| ChunkRecord {
            compression,
            compressed: vec![fill; 256 * SECTOR],
            timestamp: u32::from(fill),
        };
        let mut region = RegionFile::open_writable(&path, true).unwrap();
        let previous = record(41, Compression::None);
        region.write(chunk, &previous).unwrap();

        region.file = File::open(&path).unwrap();
        let replacement = record(42, Compression::Zlib);
        assert!(region.write(chunk, &replacement).is_err());
        let read = RegionHeader::read(&path)
            .unwrap()
            .read_chunk(chunk)
            .unwrap()
            .unwrap();
        assert_eq!(read.compressed, previous.compressed);
        assert_eq!(read.compression, previous.compression);
        assert_eq!(read.timestamp, previous.timestamp);

        region.file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        region.write(chunk, &replacement).unwrap();
        let read = RegionHeader::read(&path)
            .unwrap()
            .read_chunk(chunk)
            .unwrap()
            .unwrap();
        assert_eq!(read.compressed, replacement.compressed);
        assert_eq!(read.compression, replacement.compression);
        assert_eq!(read.timestamp, replacement.timestamp);
        drop(region);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_header_write_restores_the_previous_external_payload() {
        for previous_external in [false, true] {
            let dir = std::env::temp_dir().join(format!(
                "horizon-region-external-header-failure-{previous_external}-{}",
                std::process::id()
            ));
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join("r.0.0.mca");
            let chunk = ChunkPos::new(0, 0);
            let previous = ChunkRecord {
                compression: Compression::None,
                compressed: vec![41; if previous_external { 256 * SECTOR } else { 16 }],
                timestamp: 41,
            };
            let replacement = ChunkRecord {
                compression: Compression::Zlib,
                compressed: vec![42; 256 * SECTOR],
                timestamp: 42,
            };
            let mut region = RegionFile::open_writable(&path, true).unwrap();
            region.write(chunk, &previous).unwrap();

            // The new stub and payload have already been written. Fail only
            // the subsequent header write, after the payload was replaced.
            assert!(
                region
                    .write_with_index(chunk, &replacement, |region, slot, offset, timestamp| {
                        region.file = File::open(&path).unwrap();
                        region.update_header_entry(slot, offset, timestamp)
                    })
                    .is_err()
            );
            let read = RegionFile::open(&path)
                .unwrap()
                .read(chunk)
                .unwrap()
                .unwrap();
            assert_eq!(read.compressed, previous.compressed);
            assert_eq!(read.compression, previous.compression);
            assert_eq!(read.timestamp, previous.timestamp);
            assert_eq!(external_path(&dir, chunk).is_file(), previous_external);
            drop(region);

            // Retrying must publish both files as one readable chunk.
            let mut region = RegionFile::open_writable(&path, false).unwrap();
            region.write(chunk, &replacement).unwrap();
            let read = region.read(chunk).unwrap().unwrap();
            assert_eq!(read.compressed, replacement.compressed);
            assert_eq!(read.compression, replacement.compression);
            assert_eq!(read.timestamp, replacement.timestamp);
            assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);
            drop(region);
            fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn a_read_only_region_handle_can_be_upgraded_for_later_writes() {
        let dir = std::env::temp_dir().join(format!(
            "horizon-region-write-upgrade-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("r.0.0.mca");
        let chunk = ChunkPos::new(0, 0);
        let mut writer = RegionFile::open_writable(&path, true).unwrap();
        writer
            .write(
                chunk,
                &ChunkRecord {
                    compression: Compression::None,
                    compressed: vec![42],
                    timestamp: 7,
                },
            )
            .unwrap();
        drop(writer);

        let mut region = RegionFile::open(&path).unwrap();
        assert_eq!(region.read(chunk).unwrap().unwrap().compressed, vec![42]);
        region.ensure_writable().unwrap();
        assert!(region.clear(chunk, 9).unwrap());
        assert!(region.read(chunk).unwrap().is_none());
        drop(region);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_kept_header_reads_the_chunks_of_its_file() {
        let dir =
            std::env::temp_dir().join(format!("horizon-region-header-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("r.-1.2.mca");
        let record = |fill: u8, len: usize| ChunkRecord {
            compression: Compression::None,
            compressed: vec![fill; len],
            timestamp: u32::from(fill),
        };
        let (first, last) = (ChunkPos::new(-32, 64), ChunkPos::new(-1, 95));
        let missing = ChunkPos::new(-31, 64);
        let mut region = RegionFile::open_writable(&path, true).unwrap();
        region.write(first, &record(7, 100)).unwrap();
        // Two sectors, after the first chunk's one.
        region.write(last, &record(9, 5000)).unwrap();

        let header = RegionHeader::read(&path).unwrap();
        for (chunk, fill, len) in [(first, 7, 100), (last, 9, 5000)] {
            let read = header.read_chunk(chunk).unwrap().unwrap();
            assert_eq!(read.compressed, vec![fill; len]);
            assert_eq!(read.timestamp, u32::from(fill));
        }
        assert!(header.read_chunk(missing).unwrap().is_none());
        // The next region's chunk.
        assert!(header.read_chunk(ChunkPos::new(0, 64)).is_err());
        assert!(RegionHeader::read(dir.join("r.5.5.mca")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
