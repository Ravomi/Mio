//! Bounded byte paging for UTF-8 text, including single-line tool artifacts.
use serde::Serialize;
use std::{
    fs::OpenOptions,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

#[derive(Debug, Serialize)]
pub struct BytePage {
    pub text: String,
    pub byte_offset: u64,
    pub next_byte_offset: Option<u64>,
    pub total_bytes: u64,
}

/// Read at most `limit` bytes without splitting a UTF-8 character. The caller
/// owns workspace admission; this function performs path and file checks.
pub fn read_byte_page(
    cwd: &Path,
    path: &str,
    offset: u64,
    limit: usize,
) -> Result<BytePage, String> {
    if !(4..=crate::workspace::MAX_READ_OUTPUT_BYTES).contains(&limit) {
        return Err("byte_limit must be between 4 and 65536 bytes".into());
    }
    let path = crate::workspace::checked_path(cwd, path, false)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path).map_err(|error| error.to_string())?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file() {
        return Err("byte paging requires a regular file".into());
    }
    if metadata.len() > crate::workspace::MAX_READ_INPUT_BYTES as u64 {
        return Err("file exceeds the 8 MiB byte paging limit".into());
    }
    if offset > metadata.len() {
        return Err("byte_offset is beyond the end of the file".into());
    }
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(limit as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    let usable = match std::str::from_utf8(&bytes) {
        Ok(_) => bytes.len(),
        Err(error)
            if error.error_len().is_none()
                && error.valid_up_to() > 0
                && offset + (bytes.len() as u64) < metadata.len() =>
        {
            error.valid_up_to()
        }
        Err(_) => {
            return Err("file is not UTF-8 or byte_offset is not at a character boundary".into());
        }
    };
    if usable == 0 && offset < metadata.len() {
        return Err("file changed while reading the byte page".into());
    }
    bytes.truncate(usable);
    let text = String::from_utf8(bytes).map_err(|error| error.to_string())?;
    let next = offset + usable as u64;
    Ok(BytePage {
        text,
        byte_offset: offset,
        next_byte_offset: (next < metadata.len()).then_some(next),
        total_bytes: metadata.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_reconstruct_a_long_single_line_without_splitting_utf8() {
        let root = tempfile::tempdir().unwrap();
        let original = "界面保持简洁🌊".repeat(10_000);
        std::fs::write(root.path().join("output.txt"), &original).unwrap();
        let mut reconstructed = String::new();
        let mut offset = 0;
        loop {
            let page = read_byte_page(root.path(), "output.txt", offset, 257).unwrap();
            assert!(page.text.len() <= 257);
            reconstructed.push_str(&page.text);
            match page.next_byte_offset {
                Some(next) => {
                    assert!(next > offset);
                    offset = next;
                }
                None => break,
            }
        }
        assert_eq!(reconstructed, original);
    }

    #[test]
    fn invalid_offsets_binary_data_and_escapes_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("text"), "海浪").unwrap();
        assert!(read_byte_page(root.path(), "text", 1, 4).is_err());
        assert!(read_byte_page(root.path(), "text", 100, 4).is_err());
        std::fs::write(root.path().join("binary"), [0xff]).unwrap();
        assert!(read_byte_page(root.path(), "binary", 0, 4).is_err());
        assert!(read_byte_page(root.path(), "../outside", 0, 4).is_err());
        assert!(read_byte_page(root.path(), "text", 0, 0).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn linked_files_cannot_be_paged() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("linked")).unwrap();
        assert!(read_byte_page(root.path(), "linked", 0, 4).is_err());
    }
}
