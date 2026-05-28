use log;
use sha1::{Digest, Sha1};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

use git2::{self, Repository, TreeWalkMode, TreeWalkResult};

use crate::backend::{self, Backend};
use crate::util::other_err;

pub const BLOCK_SIZE: usize = 4096 * 1024; // 4MB
pub const PREFIX_SIZE: usize = 1024;
pub const COOKIE: &str = "#$# git-fat ";
// Length of a git-fat placeholder: COOKIE + 40-hex + ' ' + 20-padded-decimal + '\n'
pub const MAGICLEN: usize = 74;

pub struct GitFat {
    repo: git2::Repository,
    obj_dir: PathBuf,
    config_path: PathBuf,
    #[allow(dead_code)]
    verbose: bool,
    #[allow(dead_code)]
    debug: bool,
}

impl GitFat {
    pub fn new(verbose: bool, debug: bool, config_path: Option<PathBuf>) -> io::Result<Self> {
        let repo = Repository::open_from_env().map_err(other_err)?;
        let git_dir = repo.path().to_path_buf();
        let obj_dir = repo.commondir().join("fat/objects");

        let config_path = config_path.unwrap_or_else(|| {
            repo.workdir()
                .unwrap_or_else(|| git_dir.as_path())
                .join(".gitfat")
        });

        let gf = GitFat {
            repo,
            obj_dir,
            config_path,
            verbose,
            debug,
        };
        gf.configure()?;
        Ok(gf)
    }

    /// Auto-configure git-fat for this repository.
    ///
    /// Sets filter.fat.clean and filter.fat.smudge in git config only if they
    /// are not already set; ensures the objdir is created.
    pub fn configure(&self) -> io::Result<()> {
        let mut cfg = self.repo.config().map_err(other_err)?;

        if cfg.get_string("filter.fat.clean").is_ok()
            && cfg.get_string("filter.fat.smudge").is_ok()
            && self.obj_dir.is_dir()
        {
            return Ok(());
        }

        println!("Setting filters in .git/config");
        cfg.set_str("filter.fat.clean", "git-fat filter-clean %f")
            .map_err(other_err)?;
        cfg.set_str("filter.fat.smudge", "git-fat filter-smudge %f")
            .map_err(other_err)?;
        println!("Creating {}", self.obj_dir.display());
        fs::create_dir_all(&self.obj_dir)?;

        println!("Initialized git-fat");
        Ok(())
    }

    pub fn filter_clean<R: Read, W: Write>(
        &self,
        filename: Option<&str>,
        mut instream: R,
        mut outstream: W,
    ) -> io::Result<()> {
        log::debug!("CLEAN: filename={filename:?}");

        if let Some(filename) = filename {
            log::info!("Adding {filename}");
        }

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
            log::info!("git-fat filter-clean: caching to {}", objfile.display());
        }

        outstream.write_all(&encode_placeholder(&digest, total_size))?;
        Ok(())
    }

    pub fn filter_smudge<R: Read, W: Write>(
        &self,
        filename: Option<&str>,
        mut instream: R,
        mut outstream: W,
    ) -> io::Result<()> {
        log::debug!("SMUDGE: filename={filename:?}");
        let objdir = &self.obj_dir;
        let first_block = read_prefix(&mut instream, PREFIX_SIZE)?;
        if let Ok(digest) = extract_digest(&first_block) {
            let objfile = objdir.join(digest.trim());
            if objfile.exists() {
                let mut f = File::open(&objfile)?;
                io::copy(&mut f, &mut outstream)?;
                log::info!(
                    "git-fat filter-smudge: restoring from {}",
                    objfile.display()
                );
                return Ok(());
            } else {
                log::info!("git-fat filter-smudge: fat object not found in cache")
            }
        } else {
            // Not a fat object; pass through as-is
            log::info!("git-fat filter-smudge: not a managed file")
        }

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
        let workdir = self
            .repo
            .workdir()
            .ok_or_else(|| other_err("no working directory"))?
            .to_path_buf();

        let mut orphans = Vec::new();

        self.walk_tree(|dir, entry, odb| {
            if !is_fat_placeholder(entry, odb) {
                return Ok(());
            }

            let path = PathBuf::from(dir).join(entry.name().unwrap_or_default());
            let full_path = workdir.join(&path);
            if !full_path.is_file() {
                return Ok(());
            }

            let prefix =
                File::open(&full_path).and_then(|mut f| read_prefix(&mut f, PREFIX_SIZE))?;
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
        let workdir = self
            .repo
            .workdir()
            .ok_or_else(|| other_err("no working directory"))?
            .to_path_buf();

        for (digest, path) in self.orphan_files()? {
            let obj_path = self.obj_dir.join(&digest);
            if obj_path.exists() {
                println!("Restoring {} -> {}", digest, path.display());
                fs::remove_file(workdir.join(&path))?;
                std::process::Command::new("git")
                    .args(["checkout-index", "--index", "--force", "--"])
                    .arg(&path)
                    .current_dir(&workdir)
                    .status()?;
            } else if show_orphans {
                println!("Data unavailable: {} {}", digest, path.display());
            }
        }
        Ok(())
    }

    /// Find any files over a size threshold in the repository.
    pub fn find(&self, min_size: usize) -> io::Result<()> {
        self.walk_tree(|dir, entry, odb| {
            if entry.kind() != Some(git2::ObjectType::Blob) {
                return Ok(());
            }
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
            if !is_fat_placeholder(entry, odb) {
                return Ok(());
            }
            if let Ok(blob) = self.repo.find_blob(entry.id()) {
                if let Ok(digest) = extract_digest(blob.content()) {
                    let path = PathBuf::from(dir).join(entry.name().unwrap_or_default());
                    println!("{} {}", digest.trim(), path.display());
                }
            }
            Ok(())
        })
    }

    /// Push locally cached fat objects that are referenced in HEAD to the remote.
    pub fn push(&self, backend_name: Option<&str>) -> io::Result<()> {
        let backend = self.load_backend(backend_name)?;
        let files: HashSet<String> = self
            .managed_objects()?
            .intersection(&self.cached_objects()?)
            .cloned()
            .collect();
        log::debug!("PUSH: pushing {} objects", files.len());
        if !backend.push_files(&files)? {
            return Err(other_err("push failed"));
        }
        Ok(())
    }

    /// Pull fat objects referenced in HEAD that are not yet cached locally,
    /// then checkout any newly available files.
    pub fn pull(&self, backend_name: Option<&str>) -> io::Result<()> {
        let backend = self.load_backend(backend_name)?;
        let files: HashSet<String> = self
            .managed_objects()?
            .difference(&self.cached_objects()?)
            .cloned()
            .collect();
        if files.is_empty() {
            return Ok(());
        }
        log::debug!("PULL: pulling {} objects", files.len());
        if !backend.pull_files(&files)? {
            return Err(other_err("pull failed"));
        }
        self.checkout(false)
    }

    /// Convert files to fat placeholders in the index, for use with
    /// `git filter-branch --index-filter`.
    ///
    /// Reads a list of filenames from `filelist`, then for each staged file
    /// whose name appears in that list, replaces its blob with a fat
    /// placeholder blob.  Optionally appends `filter=fat -text` lines to
    /// `.gitattributes` in the index.
    pub fn index_filter(&self, filelist: &Path, update_gitattributes: bool) -> io::Result<()> {
        let workdir = self
            .obj_dir
            .parent()
            .ok_or_else(|| other_err("invalid object dir"))?
            .join("index-filter");
        fs::create_dir_all(&workdir)?;

        let content = fs::read_to_string(filelist)?;
        let files_to_convert: HashSet<&str> = content.lines().collect();

        let mut index = self.repo.index().map_err(other_err)?;

        // Collect first: can't modify the index while iterating it
        let entries: Vec<git2::IndexEntry> = index.iter().collect();
        let mut newfiles: Vec<String> = Vec::new();

        for mut entry in entries {
            let filename = String::from_utf8_lossy(&entry.path).to_string();
            // mode == 0o120000 == symbolic link
            if entry.mode == 0o120000 || !files_to_convert.contains(filename.as_str()) {
                continue;
            }

            let cache_path = workdir.join(entry.id.to_string());
            let new_oid = if cache_path.exists() {
                let s = fs::read_to_string(&cache_path)?;
                git2::Oid::from_str(s.trim()).map_err(other_err)?
            } else {
                let blob = self.repo.find_blob(entry.id).map_err(other_err)?;
                let mut placeholder: Vec<u8> = Vec::new();
                self.filter_clean(None, blob.content(), &mut placeholder)?;
                let oid = self.repo.blob(&placeholder).map_err(other_err)?;
                fs::write(&cache_path, format!("{oid}\n"))?;
                oid
            };

            entry.id = new_oid;
            index.add(&entry).map_err(other_err)?;
            newfiles.push(filename);
        }

        if update_gitattributes && !newfiles.is_empty() {
            let (ga_mode, ga_content) = match index.get_path(Path::new(".gitattributes"), 0) {
                Some(e) => {
                    let content = self
                        .repo
                        .find_blob(e.id)
                        .map(|b| String::from_utf8_lossy(b.content()).to_string())
                        .unwrap_or_default();
                    (e.mode, content)
                }
                None => (0o100644u32, String::new()),
            };

            let mut new_ga = ga_content;
            for f in &newfiles {
                new_ga.push_str(&format!("{f} filter=fat -text\n"));
            }

            let ga_oid = self.repo.blob(new_ga.as_bytes()).map_err(other_err)?;
            #[rustfmt::skip]
            index.add(&git2::IndexEntry {
                ctime: git2::IndexTime::new(0, 0),
                mtime: git2::IndexTime::new(0, 0),
                dev: 0, ino: 0, uid: 0, gid: 0,
                mode: ga_mode,
                file_size: new_ga.len() as u32,
                id: ga_oid,
                flags: 0,
                flags_extended: 0,
                path: b".gitattributes".to_vec(),
            }).map_err(other_err)?;
        }

        index.write().map_err(other_err)?;
        Ok(())
    }

    /// Show orphan (in tree, but not in cache) and stale (in cache,
    /// but not in tree) objects, if any.
    pub fn status(&self) -> io::Result<()> {
        let cached = self.cached_objects()?;
        let managed = self.managed_objects()?;
        let mut stale = cached.difference(&managed).peekable();
        let mut orphans = managed.difference(&cached).peekable();

        if orphans.peek().is_some() {
            println!("Orphan objects:");
            orphans.for_each(|n| println!("\t {n}"));
        }

        if stale.peek().is_some() {
            println!("Stale objects:");
            stale.for_each(|n| println!("\t {n}"));
        }

        Ok(())
    }

    /// Return the set of object digests present in the local cache.
    fn cached_objects(&self) -> io::Result<HashSet<String>> {
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

    fn head_tree(&self) -> io::Result<Option<git2::Tree<'_>>> {
        match self.repo.head() {
            Err(_) => Ok(None),
            Ok(head) => Ok(Some(head.peel_to_tree().map_err(other_err)?)),
        }
    }

    // Return the set of fat content digests referenced in the HEAD tree
    fn managed_objects(&self) -> io::Result<HashSet<String>> {
        let mut set = HashSet::new();
        self.walk_tree(|_dir, entry, odb| {
            if !is_fat_placeholder(entry, odb) {
                return Ok(());
            }
            if let Ok(blob) = self.repo.find_blob(entry.id()) {
                if let Ok(digest) = extract_digest(blob.content()) {
                    set.insert(digest.trim().to_string());
                }
            }
            Ok(())
        })?;
        Ok(set)
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
        let odb = self.repo.odb().map_err(other_err)?;
        let mut walk_err: Option<io::Error> = None;

        let walk_result = tree.walk(TreeWalkMode::PreOrder, |dir, entry| {
            match callback(dir, entry, &odb) {
                Ok(()) => TreeWalkResult::Ok,
                Err(e) => {
                    walk_err = Some(e);
                    TreeWalkResult::Abort
                }
            }
        });

        if let Some(e) = walk_err {
            return Err(e);
        }
        walk_result.map_err(other_err)
    }
}

/// Misc utility functions
///

fn is_fat_placeholder(entry: &git2::TreeEntry<'_>, odb: &git2::Odb<'_>) -> bool {
    entry.kind() == Some(git2::ObjectType::Blob)
        && odb
            .read_header(entry.id())
            .map_or(false, |(size, _)| size == MAGICLEN)
}

// Shared helpers used by both GitFat methods and standalone filter functions
fn encode_placeholder(digest: &str, size: usize) -> Vec<u8> {
    format!("{}{} {:20}\n", COOKIE, digest, size).into_bytes()
}

fn read_prefix<R: Read>(reader: &mut R, size: usize) -> io::Result<Vec<u8>> {
    let mut first_block = vec![0u8; size];
    let n = reader.read(&mut first_block)?;
    first_block.truncate(n);
    Ok(first_block)
}

fn extract_digest(block: &[u8]) -> Result<String, &str> {
    if !block.starts_with(COOKIE.as_bytes()) {
        return Err("not a git-fat placeholder file");
    }

    let parts: Vec<&[u8]> = block.split(|&b| b == b' ').collect();
    if parts.len() < 3 {
        return Err("not a git-fat placeholder file");
    }

    Ok(String::from_utf8_lossy(parts[2]).to_string())
}

fn read_blocks<R: Read>(
    reader: &mut R,
) -> io::Result<impl Iterator<Item = io::Result<Vec<u8>>> + '_> {
    Ok(std::iter::from_fn(move || {
        let mut buf = vec![0u8; BLOCK_SIZE];
        match reader.read(&mut buf) {
            Ok(0) => None,
            Ok(n) => Some(Ok(buf[..n].to_vec())),
            Err(e) => Some(Err(e)),
        }
    }))
}
