use sha1::{Sha1, Digest};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use tempfile::NamedTempFile;

use git2::{self, Repository, TreeWalkMode, TreeWalkResult};

use crate::backend::{self, Backend};

pub const BLOCK_SIZE: usize = 4096 * 1024;  // 4MB
pub const PREFIX_SIZE: usize = 1024;
pub const COOKIE: &str = "#$# git-fat ";
// Length of a git-fat placeholder: COOKIE + 40-hex + ' ' + 20-padded-decimal + '\n'
pub const MAGICLEN: usize = 74;


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
        let repo = Repository::open_from_env()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        let git_dir = repo.path().to_path_buf();
        let obj_dir = repo.commondir().join("fat/objects");

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

    pub fn filter_clean<R: Read, W: Write>(&self, mut instream: R, mut outstream: W) -> io::Result<()> {
        self.configure()?;
        let objdir = &self.obj_dir;
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

    pub fn filter_smudge<R: Read, W: Write>(&self, mut instream: R, mut outstream: W) -> io::Result<()> {
        let objdir = &self.obj_dir;
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

    /// Load the backend specified in `.gitfat`, optionally selecting by name.
    pub fn load_backend(&self, name: Option<&str>) -> io::Result<Box<dyn Backend>> {
        backend::load_backend(self.obj_dir.clone(), &self.config_path, name)
    }

    /// Return (digest, repo-relative path) for working-tree files that still
    /// contain a fat placeholder (i.e. have not been smudged).
    pub fn orphan_files(&self) -> io::Result<Vec<(String, PathBuf)>> {
        let workdir = self.repo.workdir()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "no working directory"))?
            .to_path_buf();

        let mut orphans = Vec::new();

        self.walk_tree(|dir, entry, odb| {
            if !is_fat_placeholder(entry, odb) { return Ok(()); }

            let path = PathBuf::from(dir).join(entry.name().unwrap_or_default());
            let full_path = workdir.join(&path);
            if !full_path.is_file() { return Ok(()); }

            let prefix = File::open(&full_path)
                .and_then(|mut f| read_prefix(&mut f, PREFIX_SIZE))?;
            if let Ok(digest) = extract_digest(&prefix) {
                orphans.push((digest, path));
            }
            Ok(())
        })?;

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

    /// Find any files over a size threshold in the repository.
    pub fn find(&self, min_size: usize) -> io::Result<()> {
        self.walk_tree(|dir, entry, odb| {
            if entry.kind() != Some(git2::ObjectType::Blob) { return Ok(()); }
            if let Ok((size, _)) = odb.read_header(entry.id()) {
                if size > min_size {
                    let path = PathBuf::from(dir).join(entry.name().unwrap_or_default());
                    println!("{} {} {}", entry.id(), size, path.display());
                }
            }
            Ok(())
        })
    }

    /// List all git-fat managed files: fat-digest and repo-relative path.
    pub fn list(&self) -> io::Result<()> {
        self.walk_tree(|dir, entry, odb| {
            if !is_fat_placeholder(entry, odb) { return Ok(()); }
            if let Ok(blob) = self.repo.find_blob(entry.id()) {
                if let Ok(digest) = extract_digest(blob.content()) {
                    let path = PathBuf::from(dir).join(entry.name().unwrap_or_default());
                    println!("{} {}", digest.trim(), path.display());
                }
            }
            Ok(())
        })
    }

    fn head_tree(&self) -> io::Result<Option<git2::Tree<'_>>> {
        match self.repo.head() {
            Err(_) => Ok(None),
            Ok(head) => Ok(Some(
                head.peel_to_tree()
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?
            )),
        }
    }

    // Walk the HEAD tree, passing each entry and the ODB to the callback.
    // Returning Err from the callback aborts the walk and propagates the error.
    fn walk_tree<F>(&self, mut callback: F) -> io::Result<()>
    where
        F: FnMut(&str, &git2::TreeEntry<'_>, &git2::Odb<'_>) -> io::Result<()>,
    {
        let tree = match self.head_tree()? {
            Some(t) => t,
            None => return Ok(()),
        };
        let odb = self.repo.odb()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        let mut walk_err: Option<io::Error> = None;

        let walk_result = tree.walk(TreeWalkMode::PreOrder, |dir, entry| {
            match callback(dir, entry, &odb) {
                Ok(()) => TreeWalkResult::Ok,
                Err(e) => { walk_err = Some(e); TreeWalkResult::Abort }
            }
        });

        if let Some(e) = walk_err { return Err(e); }
        walk_result.map_err(|e| io::Error::new(io::ErrorKind::Other, e))
    }
}


/// Misc utility functions
///

fn is_fat_placeholder(entry: &git2::TreeEntry<'_>, odb: &git2::Odb<'_>) -> bool {
    entry.kind() == Some(git2::ObjectType::Blob)
        && odb.read_header(entry.id()).map_or(false, |(size, _)| size == MAGICLEN)
}


// Shared helpers used by both GitFat methods and standalone filter functions
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
