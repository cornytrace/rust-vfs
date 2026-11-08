//! An overlay file system combining two filesystems, an upper layer with read/write access and a lower layer with only read access

use crate::error::VfsErrorKind;
use crate::{FileSystem, SeekAndRead, SeekAndWrite, VfsMetadata, VfsPath, VfsResult};
use std::collections::HashSet;

use std::time::SystemTime;

/// An overlay file system combining several filesystems into one, an upper layer with read/write access and lower layers with only read access
///
/// Files in upper layers shadow those in lower layers. Directories are the merged view of all layers.
///
/// NOTE: To allow removing files and directories (e.g. via remove_file()) from the lower layer filesystems, this mechanism creates a `.whiteout` folder in the root of the upper level filesystem to mark removed files
///
#[derive(Debug, Clone)]
pub struct OverlayFS {
    layers: Vec<VfsPath>,
    case_sensitive: bool,
}

impl OverlayFS {
    /// Create a new overlay FileSystem from the given layers, only the first layer is written to
    pub fn new(layers: &[VfsPath]) -> Self {
        Self::new_with_case_sensitivity(layers, true)
    }

    /// Create a new overlay FileSystem with the requested path case sensitivity.
    ///
    /// When case-insensitive, entries whose names differ only by case are treated as the same
    /// entry. The first (highest-priority) layer determines the casing exposed by the overlay.
    pub fn new_with_case_sensitivity(layers: &[VfsPath], case_sensitive: bool) -> Self {
        if layers.is_empty() {
            panic!("OverlayFS needs at least one layer")
        }
        OverlayFS {
            layers: layers.to_vec(),
            case_sensitive,
        }
    }

    fn write_layer(&self) -> &VfsPath {
        &self.layers[0]
    }

    fn name_key(&self, name: &str) -> String {
        if self.case_sensitive {
            name.to_string()
        } else {
            name.to_lowercase()
        }
    }

    fn relative_path(&self, path: &str) -> String {
        let relative = path.strip_prefix('/').unwrap_or(path);
        if self.case_sensitive {
            relative.to_string()
        } else {
            relative.to_lowercase()
        }
    }

    fn resolve_path(&self, path: &str) -> VfsResult<ResolvedPath> {
        let components: Vec<_> = path.split('/').filter(|part| !part.is_empty()).collect();
        let mut layer_paths: Vec<Option<VfsPath>> = self.layers.iter().cloned().map(Some).collect();
        let mut visible_path = String::new();

        for component in components {
            let key = self.name_key(component);
            let mut matching_paths = vec![None; self.layers.len()];
            let mut visible_name = None;
            let mut visible_index = None;

            for (index, parent) in layer_paths.iter().enumerate() {
                let Some(parent) = parent else {
                    continue;
                };
                if !parent.exists()?
                    || parent.metadata()?.file_type != crate::VfsFileType::Directory
                {
                    continue;
                }

                let mut matching_name = None;
                for entry in parent.read_dir()? {
                    let name = entry.filename();
                    if self.name_key(&name) == key {
                        if name == component {
                            matching_name = Some(name);
                            break;
                        }
                        if matching_name.is_none() {
                            matching_name = Some(name);
                        }
                    }
                }

                if let Some(name) = matching_name {
                    let child = parent.join(&name)?;
                    if visible_name.is_none() {
                        visible_name = Some(name.clone());
                        visible_index = Some(index);
                    }
                    matching_paths[index] = Some(child);
                }
            }

            let visible_name = visible_name.ok_or(VfsErrorKind::FileNotFound)?;
            if !visible_path.is_empty() {
                visible_path.push('/');
            }
            visible_path.push_str(&visible_name);

            let visible_entry = matching_paths
                .iter()
                .flatten()
                .next()
                .ok_or(VfsErrorKind::FileNotFound)?;
            if visible_entry.metadata()?.file_type == crate::VfsFileType::Directory {
                for child in &mut matching_paths {
                    if let Some(path) = child {
                        if path.metadata()?.file_type != crate::VfsFileType::Directory {
                            *child = None;
                        }
                    }
                }
            } else {
                for (index, child) in matching_paths.iter_mut().enumerate() {
                    if Some(index) != visible_index {
                        *child = None;
                    }
                }
            }
            layer_paths = matching_paths;
        }

        let visible = layer_paths
            .iter()
            .flatten()
            .next()
            .cloned()
            .unwrap_or_else(|| self.layers[0].clone());
        Ok(ResolvedPath {
            layer_paths,
            visible_path,
            visible,
        })
    }

    fn read_path(&self, path: &str) -> VfsResult<VfsPath> {
        if self.whiteout_path(path)?.exists()? {
            return Err(VfsErrorKind::FileNotFound.into());
        }
        Ok(self.resolve_path(path)?.visible)
    }

    fn write_path(&self, path: &str) -> VfsResult<VfsPath> {
        if path.is_empty() {
            return Ok(self.layers[0].clone());
        }
        let relative_path = match self.resolve_path(path) {
            Ok(resolved) => resolved.visible_path,
            Err(error) if matches!(error.kind(), VfsErrorKind::FileNotFound) => {
                let separator = path.rfind('/');
                let (parent, name) = match separator {
                    Some(index) => (&path[..index], &path[index + 1..]),
                    None => ("", path),
                };
                let parent_path = self.resolve_path(parent)?;
                if parent_path.visible_path.is_empty() {
                    name.to_string()
                } else {
                    format!("{}/{}", parent_path.visible_path, name)
                }
            }
            Err(error) => return Err(error),
        };

        self.write_layer().join(&relative_path)
    }

    fn whiteout_path(&self, path: &str) -> VfsResult<VfsPath> {
        if path.is_empty() {
            return self.write_layer().join(".whiteout/_wo");
        }
        self.write_layer()
            .join(format!(".whiteout/{}_wo", self.relative_path(path)))
    }

    fn whiteout_dir_path(&self, path: &str) -> VfsResult<VfsPath> {
        let relative_path = self.relative_path(path);
        if relative_path.is_empty() {
            self.write_layer().join(".whiteout")
        } else {
            self.write_layer()
                .join(format!(".whiteout/{}", relative_path))
        }
    }

    fn ensure_has_parent(&self, path: &str) -> VfsResult<()> {
        let separator = path.rfind('/');
        if let Some(index) = separator {
            let parent_path = &path[..index];
            if self.exists(parent_path)? {
                self.write_path(parent_path)?.create_dir_all()?;
                return Ok(());
            }
        }
        Err(VfsErrorKind::Other("Parent path does not exist".into()).into())
    }
}

struct ResolvedPath {
    layer_paths: Vec<Option<VfsPath>>,
    visible_path: String,
    visible: VfsPath,
}

impl FileSystem for OverlayFS {
    fn read_dir(&self, path: &str) -> VfsResult<Box<dyn Iterator<Item = String> + Send>> {
        let resolved = self.resolve_path(path)?;
        if self.whiteout_path(path)?.exists()? {
            return Err(VfsErrorKind::FileNotFound.into());
        }
        if resolved.visible.metadata()?.file_type != crate::VfsFileType::Directory {
            return Err(VfsErrorKind::FileNotFound.into());
        }

        let mut entries = Vec::<String>::new();
        let mut entry_keys = HashSet::<String>::new();
        for layer_path in resolved.layer_paths.into_iter().flatten() {
            for entry in layer_path.read_dir()? {
                let filename = entry.filename();
                if entry_keys.insert(self.name_key(&filename)) {
                    entries.push(filename);
                }
            }
        }
        // remove whiteout entries that have been removed
        let whiteout_path = self.whiteout_dir_path(path)?;
        if whiteout_path.exists()? {
            for path in whiteout_path.read_dir()? {
                let filename = path.filename();
                if filename.ends_with("_wo") {
                    let key = self.name_key(&filename[..filename.len() - 3]);
                    entries.retain(|entry| self.name_key(entry) != key);
                }
            }
        }
        Ok(Box::new(entries.into_iter()))
    }

    fn create_dir(&self, path: &str) -> VfsResult<()> {
        self.ensure_has_parent(path)?;
        self.write_path(path)?.create_dir()?;
        let whiteout_path = self.whiteout_path(path)?;
        if whiteout_path.exists()? {
            whiteout_path.remove_file()?;
        }
        Ok(())
    }

    fn open_file(&self, path: &str) -> VfsResult<Box<dyn SeekAndRead + Send>> {
        self.read_path(path)?.open_file()
    }

    fn create_file(&self, path: &str) -> VfsResult<Box<dyn SeekAndWrite + Send>> {
        self.ensure_has_parent(path)?;
        let result = self.write_path(path)?.create_file()?;
        let whiteout_path = self.whiteout_path(path)?;
        if whiteout_path.exists()? {
            whiteout_path.remove_file()?;
        }
        Ok(result)
    }

    fn append_file(&self, path: &str) -> VfsResult<Box<dyn SeekAndWrite + Send>> {
        let write_path = self.write_path(path)?;
        if !write_path.exists()? {
            self.ensure_has_parent(path)?;
            self.read_path(path)?.copy_file(&write_path)?;
        }
        write_path.append_file()
    }

    fn metadata(&self, path: &str) -> VfsResult<VfsMetadata> {
        self.read_path(path)?.metadata()
    }

    fn set_creation_time(&self, path: &str, time: SystemTime) -> VfsResult<()> {
        self.write_path(path)?.set_creation_time(time)
    }

    fn set_modification_time(&self, path: &str, time: SystemTime) -> VfsResult<()> {
        self.write_path(path)?.set_modification_time(time)
    }

    fn set_access_time(&self, path: &str, time: SystemTime) -> VfsResult<()> {
        self.write_path(path)?.set_access_time(time)
    }

    fn exists(&self, path: &str) -> VfsResult<bool> {
        if self
            .whiteout_path(path)
            .map_err(|err| err.with_context(|| "whiteout_path"))?
            .exists()?
        {
            return Ok(false);
        }
        self.read_path(path)
            .map(|path| path.exists())
            .unwrap_or(Ok(false))
    }

    fn remove_file(&self, path: &str) -> VfsResult<()> {
        // Ensure path exists
        self.read_path(path)?;
        let write_path = self.write_path(path)?;
        if write_path.exists()? {
            write_path.remove_file()?;
        }
        let whiteout_path = self.whiteout_path(path)?;
        whiteout_path.parent().create_dir_all()?;
        whiteout_path.create_file()?;
        Ok(())
    }

    fn remove_dir(&self, path: &str) -> VfsResult<()> {
        // Ensure path exists
        self.read_path(path)?;
        let write_path = self.write_path(path)?;
        if write_path.exists()? {
            write_path.remove_dir()?;
        }
        let whiteout_path = self.whiteout_path(path)?;
        whiteout_path.parent().create_dir_all()?;
        whiteout_path.create_file()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryFS;
    test_vfs!({
        let upper_root: VfsPath = MemoryFS::new().into();
        let lower_root: VfsPath = MemoryFS::new().into();
        OverlayFS::new(&[upper_root, lower_root])
    });

    fn create_roots() -> (VfsPath, VfsPath, VfsPath) {
        let lower_root: VfsPath = MemoryFS::new().into();
        let upper_root: VfsPath = MemoryFS::new().into();
        let overlay_root: VfsPath =
            OverlayFS::new(&[upper_root.clone(), lower_root.clone()]).into();
        (lower_root, upper_root, overlay_root)
    }

    #[test]
    fn read() -> VfsResult<()> {
        let (lower_root, upper_root, overlay_root) = create_roots();
        let lower_path = lower_root.join("foo.txt")?;
        let upper_path = upper_root.join("foo.txt")?;
        let overlay_path = overlay_root.join("foo.txt")?;
        lower_path.create_file()?.write_all(b"Hello Lower")?;
        assert_eq!(&overlay_path.read_to_string()?, "Hello Lower");
        upper_path.create_file()?.write_all(b"Hello Upper")?;
        assert_eq!(&overlay_path.read_to_string()?, "Hello Upper");
        lower_path.remove_file()?;
        assert_eq!(&overlay_path.read_to_string()?, "Hello Upper");
        upper_path.remove_file()?;
        assert!(!overlay_path.exists()?, "File should not exist anymore");
        Ok(())
    }

    #[test]
    fn read_dir() -> VfsResult<()> {
        let (lower_root, upper_root, overlay_root) = create_roots();
        upper_root.join("foo/upper")?.create_dir_all()?;
        upper_root.join("foo/common")?.create_dir_all()?;
        lower_root.join("foo/common")?.create_dir_all()?;
        lower_root.join("foo/lower")?.create_dir_all()?;
        let entries: Vec<_> = overlay_root.join("foo")?.read_dir()?.collect();
        let mut paths: Vec<_> = entries.iter().map(|path| path.as_str()).collect();
        paths.sort();
        assert_eq!(paths, vec!["/foo/common", "/foo/lower", "/foo/upper"]);
        Ok(())
    }

    #[test]
    fn case_insensitive_directories_merge_using_uppercase() -> VfsResult<()> {
        let lower_root: VfsPath = MemoryFS::new().into();
        let upper_root: VfsPath = MemoryFS::new().into();
        let overlay_root: VfsPath =
            OverlayFS::new_with_case_sensitivity(&[upper_root.clone(), lower_root.clone()], false)
                .into();

        upper_root.join("sound/Music")?.create_dir_all()?;
        lower_root.join("Sound/music")?.create_dir_all()?;
        upper_root.join("sound/Music/upper.txt")?.create_file()?;
        lower_root.join("Sound/music/lower.txt")?.create_file()?;

        let root_entries: Vec<_> = overlay_root
            .read_dir()?
            .map(|path| path.as_str().to_string())
            .collect();
        assert_eq!(root_entries, vec!["/sound"]);
        let sound_entries: Vec<_> = overlay_root
            .join("SOUND")?
            .read_dir()?
            .map(|path| path.filename())
            .collect();
        assert_eq!(sound_entries, vec!["Music"]);
        let music_entries: Vec<_> = overlay_root
            .join("SOUND/mUsIc")?
            .read_dir()?
            .map(|path| path.filename())
            .collect();
        assert_eq!(music_entries.len(), 2);
        assert!(music_entries.iter().any(|entry| entry == "upper.txt"));
        assert!(music_entries.iter().any(|entry| entry == "lower.txt"));
        assert!(overlay_root.join("sOuNd/mUsIc/lOwEr.TxT")?.exists()?);
        Ok(())
    }

    #[test]
    fn case_sensitive_directories_remain_separate() -> VfsResult<()> {
        let lower_root: VfsPath = MemoryFS::new().into();
        let upper_root: VfsPath = MemoryFS::new().into();
        let overlay_root: VfsPath =
            OverlayFS::new_with_case_sensitivity(&[upper_root.clone(), lower_root.clone()], true)
                .into();

        upper_root.join("sound")?.create_dir_all()?;
        lower_root.join("Sound")?.create_dir_all()?;
        upper_root.join("sound/upper.txt")?.create_file()?;
        lower_root.join("Sound/lower.txt")?.create_file()?;

        let root_entries: Vec<_> = overlay_root
            .read_dir()?
            .map(|path| path.as_str().to_string())
            .collect();
        assert_eq!(root_entries.len(), 2);
        assert!(root_entries.iter().any(|entry| entry == "/sound"));
        assert!(root_entries.iter().any(|entry| entry == "/Sound"));
        Ok(())
    }

    #[test]
    fn read_dir_root() -> VfsResult<()> {
        let (lower_root, upper_root, overlay_root) = create_roots();
        upper_root.join("upper")?.create_dir_all()?;
        upper_root.join("common")?.create_dir_all()?;
        lower_root.join("common")?.create_dir_all()?;
        lower_root.join("lower")?.create_dir_all()?;
        let entries: Vec<_> = overlay_root.read_dir()?.collect();
        let mut paths: Vec<_> = entries.iter().map(|path| path.as_str()).collect();
        paths.sort();
        assert_eq!(paths, vec!["/common", "/lower", "/upper"]);
        Ok(())
    }

    #[test]
    fn create_dir() -> VfsResult<()> {
        let (lower_root, _upper_root, overlay_root) = create_roots();
        lower_root.join("foo")?.create_dir_all()?;
        assert!(overlay_root.join("foo")?.exists()?, "dir should exist");
        overlay_root.join("foo/bar")?.create_dir()?;
        assert!(overlay_root.join("foo/bar")?.exists()?, "dir should exist");
        Ok(())
    }

    #[test]
    fn create_file() -> VfsResult<()> {
        let (lower_root, _upper_root, overlay_root) = create_roots();
        lower_root.join("foo")?.create_dir_all()?;
        assert!(overlay_root.join("foo")?.exists()?, "dir should exist");
        overlay_root.join("foo/bar")?.create_file()?;
        assert!(overlay_root.join("foo/bar")?.exists()?, "file should exist");
        Ok(())
    }

    #[test]
    fn append_file() -> VfsResult<()> {
        let (lower_root, _upper_root, overlay_root) = create_roots();
        lower_root.join("foo")?.create_dir_all()?;
        lower_root
            .join("foo/bar.txt")?
            .create_file()?
            .write_all(b"Hello Lower\n")?;
        overlay_root
            .join("foo/bar.txt")?
            .append_file()?
            .write_all(b"Hello Overlay\n")?;
        assert_eq!(
            &overlay_root.join("foo/bar.txt")?.read_to_string()?,
            "Hello Lower\nHello Overlay\n"
        );
        Ok(())
    }

    #[test]
    fn remove_file() -> VfsResult<()> {
        let (lower_root, _upper_root, overlay_root) = create_roots();
        lower_root.join("foo")?.create_dir_all()?;
        lower_root
            .join("foo/bar.txt")?
            .create_file()?
            .write_all(b"Hello Lower\n")?;
        assert!(
            overlay_root.join("foo/bar.txt")?.exists()?,
            "file should exist"
        );

        overlay_root.join("foo/bar.txt")?.remove_file()?;
        assert!(
            !overlay_root.join("foo/bar.txt")?.exists()?,
            "file should not exist anymore"
        );

        overlay_root
            .join("foo/bar.txt")?
            .create_file()?
            .write_all(b"Hello Overlay\n")?;
        assert!(
            overlay_root.join("foo/bar.txt")?.exists()?,
            "file should exist"
        );
        assert_eq!(
            &overlay_root.join("foo/bar.txt")?.read_to_string()?,
            "Hello Overlay\n"
        );
        Ok(())
    }

    #[test]
    fn remove_dir() -> VfsResult<()> {
        let (lower_root, _upper_root, overlay_root) = create_roots();
        lower_root.join("foo")?.create_dir_all()?;
        lower_root.join("foo/bar")?.create_dir_all()?;
        assert!(overlay_root.join("foo/bar")?.exists()?, "dir should exist");

        overlay_root.join("foo/bar")?.remove_dir()?;
        assert!(
            !overlay_root.join("foo/bar")?.exists()?,
            "dir should not exist anymore"
        );

        overlay_root.join("foo/bar")?.create_dir()?;
        assert!(overlay_root.join("foo/bar")?.exists()?, "dir should exist");
        Ok(())
    }

    #[test]
    fn read_dir_removed_entries() -> VfsResult<()> {
        let (lower_root, _upper_root, overlay_root) = create_roots();
        lower_root.join("foo")?.create_dir_all()?;
        lower_root.join("foo/bar")?.create_dir_all()?;
        lower_root.join("foo/bar.txt")?.create_dir_all()?;

        let entries: Vec<_> = overlay_root.join("foo")?.read_dir()?.collect();
        let mut paths: Vec<_> = entries.iter().map(|path| path.as_str()).collect();
        paths.sort();
        assert_eq!(paths, vec!["/foo/bar", "/foo/bar.txt"]);
        overlay_root.join("foo/bar")?.remove_dir()?;
        overlay_root.join("foo/bar.txt")?.remove_file()?;

        let entries: Vec<_> = overlay_root.join("foo")?.read_dir()?.collect();
        let mut paths: Vec<_> = entries.iter().map(|path| path.as_str()).collect();
        paths.sort();
        assert_eq!(paths, vec![] as Vec<&str>);

        Ok(())
    }
}

#[cfg(test)]
mod tests_physical {
    use super::*;
    use crate::PhysicalFS;
    test_vfs!({
        let temp_dir = std::env::temp_dir();
        let dir = temp_dir.join(uuid::Uuid::new_v4().to_string());
        let lower_path = dir.join("lower");
        std::fs::create_dir_all(&lower_path).unwrap();
        let upper_path = dir.join("upper");
        std::fs::create_dir_all(&upper_path).unwrap();

        let upper_root: VfsPath = PhysicalFS::new(upper_path).into();
        let lower_root: VfsPath = PhysicalFS::new(lower_path).into();
        OverlayFS::new(&[upper_root, lower_root])
    });
}
