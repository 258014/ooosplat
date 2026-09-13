use std::{
    fs::File,
    io::{BufRead, BufReader, Read},
    path::Path,
};

use crate::error::{Result, SplatError};
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlyInfo {
    pub file_size: u64,
    pub splat_count: u64,
}

const MAX_PLY_HEADER_BYTES: u64 = 256 * 1024;

fn trim_line_ending(mut line: &[u8]) -> &[u8] {
    if let Some(value) = line.strip_suffix(b"\n") {
        line = value;
    }
    line.strip_suffix(b"\r").unwrap_or(line)
}

fn read_ply_header(reader: impl Read) -> Result<String> {
    let mut reader = BufReader::new(reader.take(MAX_PLY_HEADER_BYTES + 1));
    let mut header = Vec::new();
    loop {
        let line_start = header.len();
        let read = reader.read_until(b'\n', &mut header)?;
        if header.len() as u64 > MAX_PLY_HEADER_BYTES {
            return Err(SplatError::Process(format!(
                "PLY header 超过 {} KiB 安全限制",
                MAX_PLY_HEADER_BYTES / 1024
            )));
        }
        if read == 0 {
            break;
        }
        if trim_line_ending(&header[line_start..]) == b"end_header" {
            return String::from_utf8(header)
                .map_err(|_| SplatError::Process("PLY header 不是有效 UTF-8 文本".into()));
        }
    }
    Err(SplatError::Process("PLY 缺少 end_header".into()))
}

pub fn inspect_gaussian_ply(path: &Path) -> Result<PlyInfo> {
    let size = path.metadata()?.len();
    if size == 0 {
        return Err(SplatError::Process("Brush 输出的 PLY 为空".into()));
    }
    let header = read_ply_header(File::open(path)?)?;
    let mut lines = header.lines().map(str::trim_end);
    if lines.next() != Some("ply") {
        return Err(SplatError::Process("输出不是合法 PLY 文件".into()));
    }
    let lines = lines.collect::<Vec<_>>();
    let splat_count = lines
        .iter()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            (fields.next() == Some("element") && fields.next() == Some("vertex"))
                .then(|| fields.next()?.parse::<u64>().ok())
                .flatten()
        })
        .unwrap_or(0);
    if splat_count == 0 {
        return Err(SplatError::Process("PLY 不包含 Gaussian 顶点".into()));
    }
    for property in [
        " x", " y", " z", " f_dc_0", " opacity", " scale_0", " rot_0",
    ] {
        if !lines.iter().any(|line| {
            let mut fields = line.split_whitespace();
            fields.next() == Some("property")
                && fields.next().is_some()
                && fields.next() == Some(property.trim())
        }) {
            return Err(SplatError::Process(format!(
                "PLY 缺少 Gaussian 属性：{}",
                property.trim()
            )));
        }
    }
    Ok(PlyInfo {
        file_size: size,
        splat_count,
    })
}

pub fn validate_gaussian_ply(path: &Path) -> Result<u64> {
    Ok(inspect_gaussian_ply(path)?.file_size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read};

    struct ChunkedReader<R> {
        inner: R,
        chunk_size: usize,
    }

    impl<R: Read> Read for ChunkedReader<R> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let limit = buffer.len().min(self.chunk_size);
            self.inner.read(&mut buffer[..limit])
        }
    }

    const VALID_PROPERTIES: &str = "property float x\nproperty float y\nproperty float z\nproperty float f_dc_0\nproperty float opacity\nproperty float scale_0\nproperty float rot_0\n";

    #[test]
    fn parses_splat_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("valid.ply");
        std::fs::write(&path, b"ply\nformat binary_little_endian 1.0\nelement vertex 42\nproperty float x\nproperty float y\nproperty float z\nproperty float f_dc_0\nproperty float opacity\nproperty float scale_0\nproperty float rot_0\nend_header\n").unwrap();
        assert_eq!(inspect_gaussian_ply(&path).unwrap().splat_count, 42);
    }
    #[test]
    fn rejects_plain_point_cloud() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plain.ply");
        std::fs::write(&path, b"ply\nformat binary_little_endian 1.0\nelement vertex 1\nproperty float x\nproperty float y\nproperty float z\nend_header\n").unwrap();
        assert!(inspect_gaussian_ply(&path).is_err());
    }

    #[test]
    fn reads_a_header_across_legal_short_reads() {
        let document = format!(
            "ply\nformat binary_little_endian 1.0\nelement vertex 42\n{VALID_PROPERTIES}end_header\n"
        );
        let reader = ChunkedReader {
            inner: Cursor::new(document.as_bytes()),
            chunk_size: 3,
        };
        assert!(read_ply_header(reader)
            .unwrap()
            .contains("element vertex 42"));
    }

    #[test]
    fn accepts_crlf_and_flexible_header_spacing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crlf.ply");
        let header = format!(
            "ply\r\nformat binary_little_endian 1.0\r\nelement   vertex   7\r\n{}end_header\r\n",
            VALID_PROPERTIES.replace('\n', "\r\n")
        );
        std::fs::write(&path, header).unwrap();
        assert_eq!(inspect_gaussian_ply(&path).unwrap().splat_count, 7);
    }

    #[test]
    fn rejects_missing_or_oversized_headers() {
        assert!(read_ply_header(Cursor::new(b"ply\ncomment no marker\n")).is_err());
        let oversized = vec![b'x'; MAX_PLY_HEADER_BYTES as usize + 1];
        assert!(read_ply_header(Cursor::new(oversized)).is_err());
    }
}
