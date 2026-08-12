use crate::config::Config;
use anyhow::{Context, Result, bail};
use chrono::Local;
use std::{
    fs::{self, File},
    io,
    path::{Path, PathBuf},
};
use zip::{ZipArchive, ZipWriter, write::SimpleFileOptions};

pub fn create() -> Result<PathBuf> {
    let destination = Config::backups_dir().join(format!(
        "omash-{}.zip",
        Local::now().format("%Y-%m-%d_%H-%M-%S")
    ));
    let file = File::create(&destination)?;
    let mut zip = ZipWriter::new(file);
    add_if_exists(&mut zip, &Config::profiles_path(), "profiles.yaml")?;
    add_if_exists(&mut zip, &Config::default_path(), "config.toml")?;
    for entry in fs::read_dir(Config::profiles_dir())?.filter_map(Result::ok) {
        if entry.file_type()?.is_file() {
            add_if_exists(
                &mut zip,
                &entry.path(),
                &format!("profiles/{}", entry.file_name().to_string_lossy()),
            )?;
        }
    }
    zip.finish()?;
    Ok(destination)
}

pub fn list() -> Result<Vec<PathBuf>> {
    let mut files: Vec<_> = fs::read_dir(Config::backups_dir())?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "zip"))
        .collect();
    files.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    Ok(files)
}

pub fn restore(path: &Path) -> Result<()> {
    if !path.starts_with(Config::backups_dir()) {
        bail!("backup must be inside the omash backup directory");
    }
    let mut archive = ZipArchive::new(File::open(path)?)?;
    for index in 0..archive.len() {
        let mut item = archive.by_index(index)?;
        let enclosed = item.enclosed_name().context("unsafe path in backup")?;
        let destination = match enclosed.to_str() {
            Some("profiles.yaml") => Config::profiles_path(),
            Some("config.toml") => Config::default_path(),
            Some(name) if name.starts_with("profiles/") => Config::data_dir().join(enclosed),
            _ => continue,
        };
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        io::copy(&mut item, &mut File::create(destination)?)?;
    }
    Ok(())
}

fn add_if_exists(zip: &mut ZipWriter<File>, path: &Path, name: &str) -> Result<()> {
    if path.exists() {
        zip.start_file(name, SimpleFileOptions::default())?;
        io::copy(&mut File::open(path)?, zip)?;
    }
    Ok(())
}
