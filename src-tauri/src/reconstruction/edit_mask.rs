use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use uuid::Uuid;

use crate::{
    error::{Result, SplatError},
    project::GaussianEditing,
};

const MAGIC: &[u8; 8] = b"OOSMASK\0";
const FORMAT_VERSION: u32 = 1;
const HEADER_BYTES: usize = 44;

pub fn packed_mask_bytes(splat_count: u64) -> Result<usize> {
    usize::try_from(splat_count.div_ceil(8))
        .map_err(|_| SplatError::Process("Gaussian 删除位图过大".into()))
}

fn checksum(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325_u64, |hash, value| {
        (hash ^ u64::from(*value)).wrapping_mul(0x100000001b3)
    })
}

pub fn mask_path(project_root: &Path, revision: u64) -> PathBuf {
    project_root
        .join("edits")
        .join(format!("deleted-{revision}.mask"))
}

pub fn count_deleted(mask: &[u8], splat_count: u64) -> Result<u64> {
    let expected = packed_mask_bytes(splat_count)?;
    if mask.len() != expected {
        return Err(SplatError::Process(format!(
            "Gaussian 删除位图长度无效：期望 {expected} 字节，实际 {} 字节",
            mask.len()
        )));
    }
    if let Some(last) = mask.last() {
        let used_bits = (splat_count % 8) as u8;
        if used_bits != 0 && (*last & !((1_u8 << used_bits) - 1)) != 0 {
            return Err(SplatError::Process("Gaussian 删除位图包含越界数据".into()));
        }
    }
    Ok(mask.iter().map(|value| u64::from(value.count_ones())).sum())
}

pub fn write_mask_atomic(
    project_root: &Path,
    revision: u64,
    splat_count: u64,
    mask: &[u8],
) -> Result<PathBuf> {
    let deleted_count = count_deleted(mask, splat_count)?;
    let directory = project_root.join("edits");
    std::fs::create_dir_all(&directory)?;
    let destination = mask_path(project_root, revision);
    let temporary = directory.join(format!(".deleted-{}.mask.tmp", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(MAGIC)?;
        file.write_all(&FORMAT_VERSION.to_le_bytes())?;
        file.write_all(&revision.to_le_bytes())?;
        file.write_all(&splat_count.to_le_bytes())?;
        file.write_all(&deleted_count.to_le_bytes())?;
        file.write_all(&(mask.len() as u64).to_le_bytes())?;
        file.write_all(&checksum(mask).to_le_bytes())?;
        file.write_all(mask)?;
        file.flush()?;
        file.sync_all()?;
        std::fs::rename(&temporary, &destination)?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(destination)
}

pub fn read_mask(project_root: &Path, editing: GaussianEditing) -> Result<Vec<u8>> {
    let expected = packed_mask_bytes(editing.source_splat_count)?;
    if editing.revision == 0 {
        if editing.deleted_count != 0 {
            return Err(SplatError::Process(
                "Gaussian 编辑状态缺少删除位图修订号".into(),
            ));
        }
        return Ok(vec![0; expected]);
    }
    let path = mask_path(project_root, editing.revision);
    let mut file = File::open(&path).map_err(|error| {
        SplatError::Process(format!(
            "无法读取 Gaussian 编辑位图 {}：{error}",
            path.display()
        ))
    })?;
    let mut header = [0_u8; HEADER_BYTES];
    file.read_exact(&mut header)
        .map_err(|error| SplatError::Process(format!("Gaussian 编辑位图 header 损坏：{error}")))?;
    if &header[..8] != MAGIC {
        return Err(SplatError::Process("Gaussian 编辑位图标识无效".into()));
    }
    let read_u32 = |start| u32::from_le_bytes(header[start..start + 4].try_into().unwrap());
    let read_u64 = |start| u64::from_le_bytes(header[start..start + 8].try_into().unwrap());
    let version = read_u32(8);
    let revision = read_u64(12);
    let splat_count = read_u64(20);
    let deleted_count = read_u64(28);
    let payload_len = read_u64(36) as usize;
    let mut checksum_bytes = [0_u8; 8];
    file.read_exact(&mut checksum_bytes)
        .map_err(|error| SplatError::Process(format!("Gaussian 编辑位图校验字段损坏：{error}")))?;
    let expected_checksum = u64::from_le_bytes(checksum_bytes);
    if version != FORMAT_VERSION
        || revision != editing.revision
        || splat_count != editing.source_splat_count
        || deleted_count != editing.deleted_count
        || payload_len != expected
    {
        return Err(SplatError::Process("Gaussian 编辑位图元数据不匹配".into()));
    }
    let mut mask = vec![0_u8; payload_len];
    file.read_exact(&mut mask)
        .map_err(|error| SplatError::Process(format!("Gaussian 编辑位图内容损坏：{error}")))?;
    let mut trailing = [0_u8; 1];
    if file.read(&mut trailing)? != 0 {
        return Err(SplatError::Process("Gaussian 编辑位图包含多余数据".into()));
    }
    if checksum(&mask) != expected_checksum || count_deleted(&mask, splat_count)? != deleted_count {
        return Err(SplatError::Process("Gaussian 编辑位图校验失败".into()));
    }
    Ok(mask)
}

pub fn remove_edit_files(project_root: &Path) -> Result<()> {
    let directory = project_root.join("edits");
    if directory.exists() {
        std::fs::remove_dir_all(directory)?;
    }
    Ok(())
}

pub fn cleanup_old_masks(project_root: &Path, keep_revision: u64) {
    let directory = project_root.join("edits");
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path != mask_path(project_root, keep_revision)
            && path.extension().and_then(|value| value.to_str()) == Some("mask")
        {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_rejects_corrupt_masks() {
        let root = tempfile::tempdir().unwrap();
        let mask = vec![0b0101_0011, 0b0000_0001];
        write_mask_atomic(root.path(), 4, 9, &mask).unwrap();
        let editing = GaussianEditing {
            crop: None,
            revision: 4,
            source_splat_count: 9,
            deleted_count: 5,
        };
        assert_eq!(read_mask(root.path(), editing).unwrap(), mask);
        let path = mask_path(root.path(), 4);
        let mut bytes = std::fs::read(&path).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        std::fs::write(path, bytes).unwrap();
        assert!(read_mask(root.path(), editing).is_err());
    }

    #[test]
    fn rejects_bits_beyond_splat_count() {
        assert!(count_deleted(&[0, 0b1000_0000], 9).is_err());
    }
}
