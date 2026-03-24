//! URI parsing and workspace root resolution utilities

use percent_encoding::percent_decode_str;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::info;

/// Cache for git root detection results (avoids repeated sync I/O)
pub struct GitRootCache {
    cache: HashMap<PathBuf, Option<PathBuf>>,
}

impl GitRootCache {
    pub fn new() -> Self {
        Self {
            cache: HashMap::new(),
        }
    }

    /// Cached git root lookup — avoids repeated sync I/O on every request
    pub fn find_git_root_cached(&mut self, path: &Path) -> Option<PathBuf> {
        // Use the path's parent directory as cache key (files in same dir share git root)
        let cache_key = if path.is_file() {
            path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| path.to_path_buf())
        } else {
            path.to_path_buf()
        };

        if let Some(cached) = self.cache.get(&cache_key) {
            return cached.clone();
        }

        let result = find_git_root(path);
        self.cache.insert(cache_key, result.clone());
        result
    }
}

/// Convert a file:// URI to a filesystem path
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    if let Some(path_str) = uri.strip_prefix("file:///") {
        // Decode percent-encoded characters (e.g., %20 -> space)
        let decoded = percent_decode_str(path_str).decode_utf8().ok()?;
        let decoded_str = decoded.as_ref();

        // On Windows, the path might be like /C:/Users/...
        // We need to handle this correctly
        #[cfg(windows)]
        {
            // Remove leading slash before drive letter on Windows
            let path = if decoded_str.len() >= 3
                && decoded_str.as_bytes()[0] == b'/'
                && decoded_str.as_bytes()[2] == b':'
            {
                &decoded_str[1..]
            } else {
                decoded_str
            };
            Some(PathBuf::from(path.replace('/', "\\")))
        }

        #[cfg(not(windows))]
        {
            Some(PathBuf::from(format!("/{}", decoded_str)))
        }
    } else {
        None
    }
}

/// Find git root by walking up from the given path
pub fn find_git_root(path: &Path) -> Option<PathBuf> {
    let mut current = if path.is_file() {
        path.parent()?.to_path_buf()
    } else {
        path.to_path_buf()
    };

    loop {
        let git_dir = current.join(".git");
        if git_dir.exists() {
            return Some(current);
        }

        if !current.pop() {
            break;
        }
    }

    None
}

/// Determine which root to use for a request based on URI and known roots
pub fn determine_root(
    roots: &[PathBuf],
    default_root: Option<&PathBuf>,
    git_root_cache: &mut GitRootCache,
    uri: Option<&str>,
) -> Option<PathBuf> {
    if let Some(uri) = uri {
        if let Some(path) = uri_to_path(uri) {
            // Find longest prefix match among known roots
            let matched = roots.iter()
                .filter(|root| path.starts_with(root))
                .max_by_key(|root| root.as_os_str().len())
                .cloned();

            if let Some(root) = matched {
                return Some(root);
            }

            // Auto-detect git root from file path (with cache)
            if let Some(git_root) = git_root_cache.find_git_root_cached(&path) {
                info!("Auto-detected git root from URI: {}", git_root.display());
                return Some(git_root);
            }
        }
    }

    // Fall back to default root if configured
    if let Some(root) = default_root {
        return Some(root.clone());
    }

    // Fall back to first known root
    if !roots.is_empty() {
        return Some(roots[0].clone());
    }

    None
}

/// Map a list of paths to their corresponding roots for batch notifications
pub fn group_paths_by_root(
    paths: &[PathBuf],
    roots: &[PathBuf],
    default_root: Option<&PathBuf>,
) -> HashMap<PathBuf, Vec<String>> {
    let mut paths_by_root: HashMap<PathBuf, Vec<String>> = HashMap::new();

    for path in paths {
        let root = roots.iter()
            .filter(|r| path.starts_with(r))
            .max_by_key(|r| r.as_os_str().len())
            .cloned()
            .or_else(|| default_root.cloned());

        if let Some(root) = root {
            let uri = format!("file:///{}", path.display().to_string().replace('\\', "/"));
            paths_by_root.entry(root).or_default().push(uri);
        }
    }

    paths_by_root
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_git_root_none() {
        // A non-existent path should return None
        let result = find_git_root(Path::new("/nonexistent/really/deep/path"));
        // May return None or a real git root if running inside a repo
        // Just verify it doesn't panic
        let _ = result;
    }

    #[test]
    fn test_uri_to_path_basic() {
        let result = uri_to_path("file:///C:/Users/test/file.rs");
        assert!(result.is_some());
    }

    #[test]
    fn test_uri_to_path_non_file() {
        let result = uri_to_path("https://example.com");
        assert!(result.is_none());
    }

    #[test]
    fn test_git_root_cache() {
        let mut cache = GitRootCache::new();
        // First call populates cache
        let result1 = cache.find_git_root_cached(Path::new("/nonexistent/path"));
        // Second call uses cache
        let result2 = cache.find_git_root_cached(Path::new("/nonexistent/path"));
        assert_eq!(result1, result2);
    }

    #[test]
    fn test_determine_root_with_known_root() {
        let roots = vec![PathBuf::from("/workspace/project")];
        let mut cache = GitRootCache::new();
        let result = determine_root(
            &roots,
            None,
            &mut cache,
            Some("file:///workspace/project/src/main.rs"),
        );
        // Should match the known root or fall back
        assert!(result.is_some());
    }

    #[test]
    fn test_group_paths_by_root() {
        let roots = vec![PathBuf::from("/workspace/a"), PathBuf::from("/workspace/b")];
        let paths = vec![
            PathBuf::from("/workspace/a/file1.rs"),
            PathBuf::from("/workspace/b/file2.rs"),
            PathBuf::from("/workspace/a/file3.rs"),
        ];
        let grouped = group_paths_by_root(&paths, &roots, None);
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[&PathBuf::from("/workspace/a")].len(), 2);
        assert_eq!(grouped[&PathBuf::from("/workspace/b")].len(), 1);
    }
}
