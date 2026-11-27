use std::collections::HashSet;
use std::io::{self, Cursor, Write};
use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

pub const MAX_BYTES: usize = 48 * 1024 * 1024;
pub const MAX_ENTRIES: usize = 2048;
pub const MAX_PATH: usize = 768;

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

pub fn check_component(name: &str) -> io::Result<()> {
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    if name.is_empty()
        || name.ends_with(['.', ' '])
        || name.len() > 255
        || name
            .chars()
            .any(|c| c.is_control() || "/\\:<>|\"?*".contains(c))
        || matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit())
    {
        return Err(invalid(
            "A filename cannot be exported safely. Rename it and retry.",
        ));
    }
    Ok(())
}

/// One bounded archive per request. Only finished archives may be downloaded.
pub struct Archive {
    zip: ZipWriter<Cursor<Vec<u8>>>,
    files: HashSet<String>,
    folders: HashSet<String>,
    size: usize,
}
impl Default for Archive {
    fn default() -> Self {
        Self {
            zip: ZipWriter::new(Cursor::new(Vec::new())),
            files: HashSet::new(),
            folders: HashSet::new(),
            size: 0,
        }
    }
}
impl Archive {
    pub fn add(&mut self, path: &str, bytes: &[u8]) -> io::Result<()> {
        if self.files.len() >= MAX_ENTRIES || bytes.len() > MAX_BYTES - self.size {
            return Err(invalid(
                "Folder exceeds the ZIP limit. Download a smaller folder.",
            ));
        }
        if path.len() > MAX_PATH {
            return Err(invalid("Archive path is too long."));
        }
        let mut prefix = String::new();
        let mut parts = path.split('/').peekable();
        while let Some(part) = parts.next() {
            check_component(part)?;
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(&part.to_lowercase());
            if self.files.contains(&prefix)
                || (parts.peek().is_none() && self.folders.contains(&prefix))
            {
                return Err(invalid("Two archive paths conflict. Rename one and retry."));
            }
            if parts.peek().is_some() {
                self.folders.insert(prefix.clone());
            }
        }
        self.files.insert(prefix);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        self.zip.start_file(path, options)?;
        self.zip.write_all(bytes)?;
        self.size += bytes.len();
        Ok(())
    }
    pub fn finish(self) -> io::Result<Vec<u8>> {
        Ok(self.zip.finish()?.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nested_originals_round_trip() {
        let mut archive = Archive::default();
        archive.add("evidence/é.txt", b"first").unwrap();
        archive.add("notes/é.txt", b"second").unwrap();
        let bytes = archive.finish().unwrap();
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        assert_eq!(zip.len(), 2);
        assert_eq!(
            std::io::read_to_string(zip.by_name("notes/é.txt").unwrap()).unwrap(),
            "second"
        );
    }
    #[test]
    fn rejects_ambiguous_paths() {
        for name in [
            "/abs",
            "../escape",
            "a\\b",
            "AUX.txt",
            "x./a",
            "a//b",
            "x:ads",
        ] {
            assert!(Archive::default().add(name, &[]).is_err(), "{name}");
        }
        let mut archive = Archive::default();
        archive.add("nested/file", &[]).unwrap();
        assert!(archive.add("NESTED/FILE", &[]).is_err());
        assert!(archive.add("nested", &[]).is_err());
    }
}
