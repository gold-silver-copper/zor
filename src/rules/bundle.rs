use super::{RuleSet, load};
use anyhow::{Context, Result};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

pub fn load_all(extra: &[PathBuf]) -> Result<Vec<RuleSet>> {
    let mut sets = BTreeMap::new();
    if let Some(root) = std::env::var_os("XDG_CONFIG_HOME") {
        load_dir(&Path::new(&root).join("zor/rules"), &mut sets)?;
    }
    for directory in extra {
        load_dir(directory, &mut sets)?;
    }
    Ok(sets.into_values().collect())
}
fn load_dir(directory: &Path, sets: &mut BTreeMap<String, RuleSet>) -> Result<()> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read rules directory {}", directory.display()));
        }
    };
    let mut paths: Vec<_> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    paths.sort();
    for path in paths {
        let source =
            fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let set = load(&path, &source)?;
        sets.insert(set.id.clone(), set);
    }
    Ok(())
}
