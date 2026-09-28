//! Separate files presented as one export: an unzipped export folder, or
//! several paths given on the command line.
//!
//! A folder is searched (recursively) for `DATA.N.*` members, which are
//! taken in shard order, so `sap-bin convert BSIS.QUERY/` works on an export
//! someone already unzipped. Paths named explicitly are used as given, in the
//! order given, whatever they are called.

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::archive;
use crate::zip::{Entries, EntryInfo};

pub struct FileEntries {
    files: Vec<PathBuf>,
    next: usize,
    current: Option<File>,
}

impl FileEntries {
    /// Expand folders into their `DATA.N` members and keep files as given.
    pub fn new(paths: &[PathBuf]) -> io::Result<Self> {
        let mut files = Vec::new();
        for path in paths {
            if path.is_dir() {
                let mut found = Vec::new();
                collect(path, &mut found)?;
                found.sort_by_key(|p| {
                    let name = p
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    (archive::member_index(&name).unwrap_or(u32::MAX), name)
                });
                if found.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!(
                            "{} holds no DATA.N.BIN, DATA.N.TXT or DATA.N.zip files",
                            path.display()
                        ),
                    ));
                }
                files.extend(found);
            } else {
                files.push(path.clone());
            }
        }
        Ok(Self {
            files,
            next: 0,
            current: None,
        })
    }

    pub fn files(&self) -> &[PathBuf] {
        &self.files
    }
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect(&path, out)?;
        } else if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| archive::member_index(n).is_some())
        {
            out.push(path);
        }
    }
    Ok(())
}

impl Read for FileEntries {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match &mut self.current {
            Some(file) => file.read(buf),
            None => Ok(0),
        }
    }
}

impl Entries for FileEntries {
    fn next_entry(&mut self) -> io::Result<Option<EntryInfo>> {
        self.current = None;
        let Some(path) = self.files.get(self.next) else {
            return Ok(None);
        };
        self.next += 1;
        let file = File::open(path)?;
        let size = file.metadata()?.len();
        self.current = Some(file);
        Ok(Some(EntryInfo {
            name: path.file_name().map_or_else(
                || path.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            ),
            method: 0,
            size: Some(size),
            compressed_size: Some(size),
        }))
    }
}
