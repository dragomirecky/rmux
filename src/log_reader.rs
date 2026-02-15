use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const CHUNK_SIZE: u64 = 8192;

/// Read the last `n` lines from a file, reading from the end in chunks.
///
/// This avoids reading the entire file for large log files.
/// Lines are returned in chronological order (oldest first).
pub fn read_last_lines(path: &Path, n: usize) -> std::io::Result<Vec<String>> {
    if n == 0 {
        return Ok(Vec::new());
    }

    let mut file = std::fs::File::open(path)?;
    let file_len = file.metadata()?.len();
    if file_len == 0 {
        return Ok(Vec::new());
    }

    let mut lines: Vec<String> = Vec::new();
    let mut remaining = Vec::new();
    let mut pos = file_len;

    loop {
        let read_size = CHUNK_SIZE.min(pos);
        pos -= read_size;
        file.seek(SeekFrom::Start(pos))?;

        let buf_len = usize::try_from(read_size)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let mut buf = vec![0u8; buf_len];
        file.read_exact(&mut buf)?;

        // Prepend to remaining data
        buf.extend_from_slice(&remaining);
        remaining = buf;

        // Split by newlines — convert to owned strings to avoid borrow conflicts
        let s = String::from_utf8_lossy(&remaining).into_owned();
        let parts: Vec<&str> = s.split('\n').collect();
        let mut temp_lines: Vec<String> = Vec::new();

        if pos == 0 {
            // We've read the entire file; include all parts
            for part in &parts {
                if !part.is_empty() {
                    temp_lines.push((*part).to_string());
                }
            }
            temp_lines.extend(lines);
            lines = temp_lines;
            break;
        }

        // First part may be incomplete — save it for next iteration
        let first = parts[0].to_string();
        for part in &parts[1..] {
            if !part.is_empty() {
                temp_lines.push((*part).to_string());
            }
        }
        temp_lines.extend(lines);
        lines = temp_lines;
        remaining = first.into_bytes();

        if lines.len() >= n {
            break;
        }
    }

    // Return only the last n lines
    let start = lines.len().saturating_sub(n);
    Ok(lines[start..].to_vec())
}

/// Strip a timestamp prefix from a line (format: `[YYYY-MM-DD HH:MM:SS.mmm] ...`).
pub fn strip_timestamp(line: &str) -> &str {
    if line.starts_with('[') {
        if let Some(end) = line.find("] ") {
            return &line[end + 2..];
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_test_file(path: &Path, content: &str) {
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    fn test_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rmux_log_test_{}_{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn read_last_lines_basic() {
        let dir = test_dir("basic");
        let path = dir.join("test.log");
        write_test_file(&path, "line1\nline2\nline3\nline4\nline5\n");

        let lines = read_last_lines(&path, 3).unwrap();
        assert_eq!(lines, vec!["line3", "line4", "line5"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_all_lines() {
        let dir = test_dir("all");
        let path = dir.join("test.log");
        write_test_file(&path, "a\nb\nc\n");

        let lines = read_last_lines(&path, 100).unwrap();
        assert_eq!(lines, vec!["a", "b", "c"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_empty_file() {
        let dir = test_dir("empty");
        let path = dir.join("test.log");
        write_test_file(&path, "");

        let lines = read_last_lines(&path, 10).unwrap();
        assert!(lines.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_zero_lines() {
        let dir = test_dir("zero");
        let path = dir.join("test.log");
        write_test_file(&path, "line1\nline2\n");

        let lines = read_last_lines(&path, 0).unwrap();
        assert!(lines.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_single_line_no_trailing_newline() {
        let dir = test_dir("single_no_nl");
        let path = dir.join("test.log");
        write_test_file(&path, "only line");

        let lines = read_last_lines(&path, 5).unwrap();
        assert_eq!(lines, vec!["only line"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn skip_empty_lines() {
        let dir = test_dir("skip_empty");
        let path = dir.join("test.log");
        write_test_file(&path, "line1\n\nline2\n\nline3\n");

        let lines = read_last_lines(&path, 10).unwrap();
        assert_eq!(lines, vec!["line1", "line2", "line3"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn large_file() {
        let dir = test_dir("large");
        let path = dir.join("test.log");
        let mut content = String::new();
        for i in 0..10000 {
            content.push_str(&format!("line {i}\n"));
        }
        write_test_file(&path, &content);

        let lines = read_last_lines(&path, 5).unwrap();
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[4], "line 9999");
        assert_eq!(lines[0], "line 9995");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn strip_timestamp_with_prefix() {
        let line = "[2024-01-01 12:00:00.000] hello world";
        assert_eq!(strip_timestamp(line), "hello world");
    }

    #[test]
    fn strip_timestamp_no_prefix() {
        let line = "hello world";
        assert_eq!(strip_timestamp(line), "hello world");
    }

    #[test]
    fn strip_timestamp_bracket_no_close() {
        let line = "[incomplete timestamp";
        assert_eq!(strip_timestamp(line), "[incomplete timestamp");
    }

    #[test]
    fn utf8_content() {
        let dir = test_dir("utf8");
        let path = dir.join("test.log");
        write_test_file(&path, "hello \u{1F600}\nworld \u{2764}\n");

        let lines = read_last_lines(&path, 10).unwrap();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains('\u{1F600}'));
        assert!(lines[1].contains('\u{2764}'));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
