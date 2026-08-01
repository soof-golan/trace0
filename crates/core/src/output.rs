use std::fs::OpenOptions;
use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

/// One trace file, written by every process being traced at once.
///
/// A commit has to land whole. Two processes appending at the same time would
/// otherwise splice one's packets into the middle of the other's, and neither
/// stream would decode. Callers buffer whole packets and commit at packet
/// boundaries; the lock makes the commit atomic against the other writers.
pub struct SharedFile {
    file: File,
    buf: Vec<u8>,
}

impl SharedFile {
    /// The process the user launched, which starts the file over.
    pub fn create(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::open(path, true)
    }

    /// A child, which adds to whatever its parent has already written.
    pub fn append(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::open(path, false)
    }

    /// Every writer holds the file open in append mode, so that one process
    /// writing cannot land on top of what another has already committed.
    /// Starting over is therefore a separate step from opening.
    fn open(path: impl AsRef<Path>, truncate: bool) -> io::Result<Self> {
        if truncate {
            File::create(path.as_ref())?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            file,
            buf: Vec::with_capacity(1 << 17),
        })
    }

    fn commit(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let _guard = FileLock::acquire(&self.file)?;
        let mut handle: &File = &self.file;
        let result = handle.write_all(&self.buf);
        self.buf.clear();
        result
    }
}

impl Write for SharedFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.commit()?;
        self.file.flush()
    }
}

impl Drop for SharedFile {
    fn drop(&mut self) {
        let _whatever_is_left = self.commit();
    }
}

#[cfg(unix)]
struct FileLock<'a>(&'a File);

#[cfg(unix)]
impl<'a> FileLock<'a> {
    fn acquire(file: &'a File) -> io::Result<Self> {
        use std::os::unix::io::AsRawFd;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(file))
    }
}

#[cfg(unix)]
impl Drop for FileLock<'_> {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(not(unix))]
struct FileLock<'a>(&'a File);

#[cfg(not(unix))]
impl<'a> FileLock<'a> {
    fn acquire(file: &'a File) -> io::Result<Self> {
        Ok(Self(file))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn temp(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("trace0-{name}-{}", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn contents(path: &Path) -> Vec<u8> {
        let mut out = Vec::new();
        File::open(path).unwrap().read_to_end(&mut out).unwrap();
        out
    }

    #[test]
    fn nothing_reaches_the_file_until_it_is_committed() {
        let path = temp("uncommitted");
        let mut out = SharedFile::create(&path).unwrap();
        out.write_all(b"abc").unwrap();
        assert!(contents(&path).is_empty());
        out.flush().unwrap();
        assert_eq!(contents(&path), b"abc");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_second_writer_adds_to_the_first_rather_than_over_it() {
        let path = temp("append");
        let mut root = SharedFile::create(&path).unwrap();
        root.write_all(b"parent").unwrap();
        root.flush().unwrap();

        let mut child = SharedFile::append(&path).unwrap();
        child.write_all(b"child").unwrap();
        child.flush().unwrap();

        root.write_all(b"more").unwrap();
        root.flush().unwrap();

        assert_eq!(contents(&path), b"parentchildmore");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn creating_starts_the_file_over() {
        let path = temp("truncate");
        std::fs::write(&path, b"stale").unwrap();
        let mut out = SharedFile::create(&path).unwrap();
        out.write_all(b"fresh").unwrap();
        out.flush().unwrap();
        assert_eq!(contents(&path), b"fresh");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_dropped_writer_still_commits_what_it_buffered() {
        let path = temp("drop");
        {
            let mut out = SharedFile::create(&path).unwrap();
            out.write_all(b"buffered").unwrap();
        }
        assert_eq!(contents(&path), b"buffered");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn concurrent_writers_never_tear_a_commit() {
        let path = temp("concurrent");
        SharedFile::create(&path).unwrap().flush().unwrap();

        const WRITERS: usize = 8;
        const ROUNDS: usize = 40;
        const LEN: usize = 4096;

        std::thread::scope(|s| {
            for w in 0..WRITERS {
                let path = path.clone();
                s.spawn(move || {
                    let mut out = SharedFile::append(&path).unwrap();
                    for _ in 0..ROUNDS {
                        out.write_all(&vec![b'a' + w as u8; LEN]).unwrap();
                        out.flush().unwrap();
                    }
                });
            }
        });

        let data = contents(&path);
        assert_eq!(data.len(), WRITERS * ROUNDS * LEN);
        for chunk in data.chunks(LEN) {
            assert!(
                chunk.iter().all(|b| *b == chunk[0]),
                "a commit was torn by another writer"
            );
        }
        std::fs::remove_file(&path).ok();
    }
}
