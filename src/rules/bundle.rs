use super::{RuleSet, load};
use anyhow::{Context, Result};
use std::{
    collections::BTreeMap,
    fs,
    io::Read as _,
    path::{Path, PathBuf},
};

/// Maximum number of external TOML rule files loaded across all configured directories.
pub const MAX_RULE_FILES: usize = 256;
/// Maximum encoded size of one external TOML rule file.
pub const MAX_RULE_FILE_BYTES: usize = 1024 * 1024;
/// Maximum encoded size accepted by `zor check` for a captured fixture.
pub const MAX_FIXTURE_BYTES: usize = 4 * 1024 * 1024;

pub fn load_all(extra: &[PathBuf]) -> Result<Vec<RuleSet>> {
    let mut sets = BTreeMap::new();
    let mut files = 0usize;
    if let Some(root) = std::env::var_os("XDG_CONFIG_HOME") {
        load_dir(&Path::new(&root).join("zor/rules"), &mut sets, &mut files)?;
    }
    for directory in extra {
        load_dir(directory, &mut sets, &mut files)?;
    }
    Ok(sets.into_values().collect())
}
fn load_dir(
    directory: &Path,
    sets: &mut BTreeMap<String, RuleSet>,
    files: &mut usize,
) -> Result<()> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read rules directory {}", directory.display()));
        }
    };
    let mut paths = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_none_or(|ext| ext != "toml") {
            continue;
        }
        if (*files).saturating_add(paths.len()) >= MAX_RULE_FILES {
            anyhow::bail!("external rule file limit ({MAX_RULE_FILES}) exceeded");
        }
        paths.push(path);
    }
    paths.sort();
    for path in paths {
        *files += 1;
        let source = read_bounded_utf8(&path, MAX_RULE_FILE_BYTES)
            .with_context(|| format!("read {}", path.display()))?;
        let set = load(&path, &source)?;
        sets.insert(set.id.clone(), set);
    }
    Ok(())
}

pub fn read_bounded_utf8(path: &Path, limit: usize) -> Result<String> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        anyhow::bail!("input exceeds {limit} bytes");
    }
    String::from_utf8(bytes).map_err(Into::into)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn bounded_reader_rejects_oversized_input() {
        let path = std::env::temp_dir().join(format!("zor-bounded-read-{}", std::process::id()));
        std::fs::write(&path, [b'x'; 17]).expect("write");
        assert!(read_bounded_utf8(&path, 16).is_err());
        std::fs::remove_file(path).expect("cleanup");
    }

    #[test]
    fn aggregate_rule_file_count_is_bounded() {
        let directory = std::env::temp_dir().join(format!("zor-rule-count-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir(&directory).expect("directory");
        std::fs::write(directory.join("extra.toml"), b"id='extra'").expect("rule");
        let mut sets = BTreeMap::new();
        let mut files = MAX_RULE_FILES;
        assert!(load_dir(&directory, &mut sets, &mut files).is_err());
        std::fs::remove_dir_all(directory).expect("cleanup");
    }
}
