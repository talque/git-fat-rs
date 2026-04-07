use sha1::{Sha1, Digest};
use std::env;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use tempfile::NamedTempFile;

pub const BLOCK_SIZE: usize = 4096 * 1024;  // 4MB
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
    let mut first_block = vec![0u8; 1024];
    let n = instream.read(&mut first_block)?;
    first_block.truncate(n);

    if first_block.starts_with(COOKIE.as_bytes()) {
        let parts: Vec<&[u8]> = first_block.split(|&b| b == b' ').collect();
        if parts.len() >= 3 {
            let digest = String::from_utf8_lossy(parts[2]);
            let objfile = objdir.join(digest.trim());
            if objfile.exists() {
                let mut f = File::open(&objfile)?;
                io::copy(&mut f, &mut outstream)?;
                return Ok(());
            }
        }
    }

    // Not a fat object; pass through as-is
    outstream.write_all(&first_block)?;
    io::copy(&mut instream, &mut outstream)?;
    Ok(())
}
