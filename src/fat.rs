use sha1::{Sha1, Digest};
use std::collections::HashSet;
use std::env;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use tempfile::NamedTempFile;

use crate::backend::{self, Backend};

pub const BLOCK_SIZE: usize = 4096 * 1024;  // 4MB
pub const PREFIX_SIZE: usize = 1024;
pub const COOKIE: &str = "#$# git-fat ";


pub struct GitFat {
    pub repo: git2::Repository,
    pub git_dir: PathBuf,
    pub obj_dir: PathBuf,
    pub config_path: PathBuf,
    pub verbose: bool,
    pub debug: bool,
}


impl GitFat {
    pub fn new(verbose: bool, debug: bool, config_path: Option<PathBuf>) -> io::Result<Self> {
        let repo = git2::Repository::open_from_env()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        let git_dir = git_dir();
        let obj_dir = git_common_dir(&git_dir).join("fat/objects");

        let config_path = config_path.unwrap_or_else(|| {
            repo.workdir()
                .unwrap_or_else(|| git_dir.as_path())
                .join(".gitfat")
        });

        Ok(GitFat { repo, git_dir, obj_dir, config_path, verbose, debug })
    }

    /// Auto-configure git-fat for this repository.
    ///
    /// Sets filter.fat.clean and filter.fat.smudge in git config only if they
    /// are not already set; ensures the objdir is created.
    pub fn configure(&self) -> io::Result<()> {
        let mut cfg = self.repo.config()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        if cfg.get_string("filter.fat.clean").is_err() {
            eprintln!("Setting filter.fat.clean in git config");
            cfg.set_str("filter.fat.clean", "git-fat filter-clean %f")
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        }
        if cfg.get_string("filter.fat.smudge").is_err() {
            eprintln!("Setting filter.fat.smudge in git config");
            cfg.set_str("filter.fat.smudge", "git-fat filter-smudge %f")
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        }

        if !self.obj_dir.exists() {
            eprintln!("Creating {}", self.obj_dir.display());
            fs::create_dir_all(&self.obj_dir)?;
        }

        Ok(())
    }

    pub fn filter_clean<R: Read, W: Write>(&self, instream: R, outstream: W) -> io::Result<()> {
        self.configure()?;
        filter_clean_impl(&self.obj_dir, instream, outstream)
    }

    pub fn filter_smudge<R: Read, W: Write>(&self, instream: R, outstream: W) -> io::Result<()> {
        filter_smudge_impl(&self.obj_dir, instream, outstream)
    }

    /// Load the backend specified in `.gitfat`, optionally selecting by name.
    pub fn load_backend(&self, name: Option<&str>) -> io::Result<Box<dyn Backend>> {
        backend::load_backend(self.obj_dir.clone(), &self.config_path, name)
    }

    /// Return the set of fat object digests present in the local cache.
    pub fn cached_objects(&self) -> io::Result<HashSet<String>> {
        if !self.obj_dir.exists() {
            return Ok(HashSet::new());
        }
        let mut set = HashSet::new();
        for entry in fs::read_dir(&self.obj_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                if let Some(name) = entry.file_name().to_str() {
                    set.insert(name.to_string());
                }
            }
        }
        Ok(set)
    }

    /// Walk the HEAD tree and return (repo-relative path, fat-digest) for every
    /// blob whose content is a fat placeholder.
    pub fn managed_files(&self) -> io::Result<Vec<(PathBuf, String)>> {
        let head = match self.repo.head() {
            Ok(h) => h,
            Err(_) => return Ok(vec![]),  // empty / unborn repo
        };
        let tree = head
            .peel_to_commit()
            .and_then(|c| c.tree())
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        let mut results: Vec<(PathBuf, String)> = Vec::new();

        tree.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
            if entry.kind() != Some(git2::ObjectType::Blob) {
                return git2::TreeWalkResult::Ok;
            }
            let path = PathBuf::from(dir).join(entry.name().unwrap_or(""));
            if let Ok(blob) = self.repo.find_blob(entry.id()) {
                if let Ok(digest) = extract_digest(blob.content()) {
                    results.push((path, digest));
                }
            }
            git2::TreeWalkResult::Ok
        })
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        Ok(results)
    }

    /// Return (digest, repo-relative path) for working-tree files that still
    /// contain a fat placeholder (i.e. have not been smudged).
    pub fn orphan_files(&self) -> io::Result<Vec<(String, PathBuf)>> {
        let workdir = self.repo.workdir()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "no working directory"))?
            .to_path_buf();

        let mut orphans = Vec::new();
        for (path, _head_digest) in self.managed_files()? {
            let full_path = workdir.join(&path);
            if !full_path.is_file() {
                continue;
            }
            // Read only enough bytes to detect the cookie
            let mut f = File::open(&full_path)?;
            let prefix = read_prefix(&mut f, PREFIX_SIZE)?;
            if let Ok(digest) = extract_digest(&prefix) {
                orphans.push((digest, path));
            }
        }
        Ok(orphans)
    }

    /// Restore working-tree files that are still fat placeholders but whose
    /// objects are present in the local cache.
    pub fn checkout(&self, show_orphans: bool) -> io::Result<()> {
        let workdir = self.repo.workdir()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "no working directory"))?
            .to_path_buf();

        for (digest, path) in self.orphan_files()? {
            let obj_path = self.obj_dir.join(&digest);
            if obj_path.exists() {
                eprintln!("Restoring {} -> {}", digest, path.display());
                fs::remove_file(workdir.join(&path))?;
                std::process::Command::new("git")
                    .args(["checkout-index", "--index", "--force", "--"])
                    .arg(&path)
                    .current_dir(&workdir)
                    .status()?;
            } else if show_orphans {
                eprintln!("Data unavailable: {} {}", digest, path.display());
            }
        }
        Ok(())
    }
}


// Shared helpers used by both GitFat methods and standalone filter functions
pub fn git_dir() -> PathBuf {
    match env::var("GIT_DIR") {
        Ok(val) => PathBuf::from(val),
        Err(_) => {
            eprintln!("GIT_DIR is not set; cannot determine git directory");
            std::process::exit(1);
        }
    }
}


pub fn git_common_dir(git_dir: &PathBuf) -> PathBuf {
    if git_dir.parent()
        .and_then(|p| p.file_name())
        .map(|f| f == "worktrees")
        .unwrap_or(false)
    {
        return git_dir.parent().unwrap().parent().unwrap().to_path_buf();
    }
    git_dir.clone()
}


pub fn encode_placeholder(digest: &str, size: usize) -> Vec<u8> {
    format!("{}{} {:20}\n", COOKIE, digest, size).into_bytes()
}


pub fn read_prefix<R: Read>(reader: &mut R, size: usize) -> io::Result<Vec<u8>> {
    let mut first_block = vec![0u8; size];
    let n = reader.read(&mut first_block)?;
    first_block.truncate(n);
    Ok(first_block)
}


pub fn extract_digest(block: &[u8]) -> Result<String, &str> {
    if !block.starts_with(COOKIE.as_bytes()) {
        return Err("not a git-fat placeholder file");
    }

    let parts: Vec<&[u8]> = block.split(|&b| b == b' ').collect();
    if parts.len() < 3 {
        return Err("not a git-fat placeholder file");
    }

    Ok(String::from_utf8_lossy(parts[2]).to_string())
}


pub fn read_blocks<R: Read>(reader: &mut R) -> io::Result<impl Iterator<Item=io::Result<Vec<u8>>> + '_> {
    Ok(std::iter::from_fn(move || {
        let mut buf = vec![0u8; BLOCK_SIZE];
        match reader.read(&mut buf) {
            Ok(0) => None,
            Ok(n) => Some(Ok(buf[..n].to_vec())),
            Err(e) => Some(Err(e)),
        }
    }))
}


pub fn filter_clean_impl<R: Read, W: Write>(
    objdir: &std::path::Path,
    mut instream: R,
    mut outstream: W,
) -> io::Result<()> {
    fs::create_dir_all(objdir)?;

    let mut temp = NamedTempFile::new_in(objdir)?;
    let mut hash = Sha1::new();
    let mut total_size = 0;

    for block in read_blocks(&mut instream)? {
        let block = block?;
        hash.update(&block);
        total_size += block.len();
        temp.write_all(&block)?;
    }

    let digest = hex::encode(hash.finalize());
    let objfile = objdir.join(&digest);

    if !objfile.exists() {
        temp.persist(&objfile)?;
        let mut perms = fs::metadata(&objfile)?.permissions();
        perms.set_readonly(true);
        fs::set_permissions(&objfile, perms)?;
    }

    outstream.write_all(&encode_placeholder(&digest, total_size))?;
    Ok(())
}


pub fn filter_smudge_impl<R: Read, W: Write>(
    objdir: &std::path::Path,
    mut instream: R,
    mut outstream: W,
) -> io::Result<()> {
    let first_block = read_prefix(&mut instream, PREFIX_SIZE)?;
    if let Ok(digest) = extract_digest(&first_block) {
        let objfile = objdir.join(digest.trim());
        if objfile.exists() {
            let mut f = File::open(&objfile)?;
            io::copy(&mut f, &mut outstream)?;
            return Ok(());
        }
    }

    // Not a fat object; pass through as-is
    outstream.write_all(&first_block)?;
    io::copy(&mut instream, &mut outstream)?;
    Ok(())
}
