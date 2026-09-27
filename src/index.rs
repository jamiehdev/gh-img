//! Expiry times for stored images. The index lives outside the served
//! directory, so nginx never exposes it.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Image name to expiry in Unix seconds. `None` means the image is kept.
#[derive(Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Index {
    pub images: BTreeMap<String, Option<u64>>,
}

impl Index {
    pub fn load(path: &Path) -> io::Result<Index> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Index::default()),
            Err(e) => Err(e),
        }
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let dir = path.parent().unwrap_or(Path::new("."));
        let mut tmp = tempfile::Builder::new()
            .prefix(".index")
            .suffix(".tmp")
            .tempfile_in(dir)?;
        serde_json::to_writer_pretty(&mut tmp, self).map_err(io::Error::other)?;
        tmp.write_all(b"\n")?;
        tmp.as_file().sync_all()?;
        tmp.persist(path).map_err(|e| e.error)?;
        std::fs::File::open(dir)?.sync_all()
    }

    pub fn expired(&self, now: u64) -> Vec<String> {
        self.images
            .iter()
            .filter(|(_, exp)| exp.is_some_and(|t| t <= now))
            .map(|(name, _)| name.clone())
            .collect()
    }
}

/// Delete every image whose expiry has passed, then drop it from the index.
/// Images missing from the index are never touched. Returns the deleted names.
pub fn sweep(img_dir: &Path, index_path: &Path, now: u64) -> io::Result<Vec<String>> {
    let mut index = Index::load(index_path)?;
    let expired = index.expired(now);

    for name in &expired {
        match std::fs::remove_file(img_dir.join(name)) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        index.images.remove(name);
    }

    if !expired.is_empty() {
        index.save(index_path)?;
    }
    Ok(expired)
}

/// Add every stored image missing from the index, expiring `ttl_secs` after
/// its modification time. Used once when images predate the index.
/// Returns the added names.
pub fn adopt(img_dir: &Path, index_path: &Path, ttl_secs: u64) -> io::Result<Vec<String>> {
    let mut index = Index::load(index_path)?;
    let mut added = Vec::new();

    for entry in std::fs::read_dir(img_dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !crate::valid_name(&name) || index.images.contains_key(&name) {
            continue;
        }

        let modified = entry.metadata()?.modified()?;
        let mtime = modified
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        index.images.insert(name.clone(), Some(mtime + ttl_secs));
        added.push(name);
    }

    if !added.is_empty() {
        index.save(index_path)?;
    }
    added.sort();
    Ok(added)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sweep_deletes_expired_and_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let idx = dir.path().join("index.json");
        for name in ["old.png", "new.png", "kept.png", "unindexed.png"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }

        let mut index = Index::default();
        index.images.insert("old.png".into(), Some(100));
        index.images.insert("new.png".into(), Some(300));
        index.images.insert("kept.png".into(), None);
        index.images.insert("gone.png".into(), Some(50));
        index.save(&idx).unwrap();

        let deleted = sweep(dir.path(), &idx, 200).unwrap();

        assert_eq!(deleted, vec!["gone.png".to_owned(), "old.png".to_owned()]);
        assert!(!dir.path().join("old.png").exists());
        for kept in ["new.png", "kept.png", "unindexed.png"] {
            assert!(dir.path().join(kept).exists(), "{kept}");
        }
        let after = Index::load(&idx).unwrap();
        assert_eq!(
            after.images.keys().collect::<Vec<_>>(),
            vec!["kept.png", "new.png"]
        );
    }

    #[test]
    fn adopt_indexes_only_unknown_valid_names() {
        let dir = tempfile::tempdir().unwrap();
        let idx = dir.path().join("index.json");
        let img = dir.path().join("img");
        std::fs::create_dir(&img).unwrap();
        let known = "AAAAAAAAAAAAAAAAAAAAAA.png";
        let unknown = "BBBBBBBBBBBBBBBBBBBBBB.jpg";
        for name in [
            known,
            unknown,
            ".CCCCCCCCCCCCCCCCCCCCCC.png.tmp",
            "notes.txt",
        ] {
            std::fs::write(img.join(name), b"x").unwrap();
        }
        let mtime = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        std::fs::File::options()
            .write(true)
            .open(img.join(unknown))
            .unwrap()
            .set_modified(mtime)
            .unwrap();

        let mut index = Index::default();
        index.images.insert(known.into(), None);
        index.save(&idx).unwrap();

        assert_eq!(adopt(&img, &idx, 500).unwrap(), vec![unknown.to_owned()]);

        let after = Index::load(&idx).unwrap();
        assert_eq!(after.images.len(), 2);
        assert_eq!(after.images[known], None);
        assert_eq!(after.images[unknown], Some(1_500));
    }

    #[test]
    fn load_of_missing_index_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            Index::load(&dir.path().join("none.json")).unwrap(),
            Index::default()
        );
    }
}
