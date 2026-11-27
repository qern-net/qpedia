use qpedia_file_archive::{Archive, MAX_BYTES, MAX_PATH};
use std::io::{self, Read, Write};

/// Each record contains two little-endian u32 lengths, UTF-8 path and original bytes.
/// A zero path length explicitly commits the archive. Truncated input never succeeds.
fn main() -> io::Result<()> {
    let mut input = io::stdin().lock();
    let mut archive = Archive::default();
    let mut header = [0u8; 8];
    let mut remaining = MAX_BYTES;
    loop {
        input.read_exact(&mut header)?;
        let path_size = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
        let size = u32::from_le_bytes(header[4..].try_into().unwrap()) as usize;
        if path_size == 0 && size == 0 {
            break;
        }
        if path_size == 0 || path_size > MAX_PATH || size > remaining {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Archive limits exceeded",
            ));
        }
        let mut path = vec![0u8; path_size];
        input.read_exact(&mut path)?;
        let mut bytes = vec![0u8; size];
        input.read_exact(&mut bytes)?;
        let name = std::str::from_utf8(&path)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        archive.add(name, &bytes)?;
        remaining -= size;
    }
    io::stdout().lock().write_all(&archive.finish()?)
}
