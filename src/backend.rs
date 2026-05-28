use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use log;

/// Backend trait
pub trait Backend {
    /// Upload the given set of fat objects (identified by digest) to the remote.
    fn push_files(&self, files: &HashSet<String>) -> io::Result<bool>;

    /// Download the given set of fat objects from the remote into the local cache.
    fn pull_files(&self, files: &HashSet<String>) -> io::Result<bool>;
}

/// CopyBackend: simple local-directory mirror
pub struct CopyBackend {
    /// Remote directory to copy to/from.
    remote: PathBuf,
    /// Local fat object directory (.git/fat/objects).
    base_dir: PathBuf,
}

impl CopyBackend {
    pub fn new(base_dir: PathBuf, remote: &str) -> io::Result<Self> {
        let remote = PathBuf::from(remote);
        if !remote.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("copy backend remote is not a directory: {}", remote.display()),
            ));
        }
        log::debug!("CopyBackend: other_path={}, base_dir={}",
            remote.display(), base_dir.display());
        Ok(CopyBackend { remote, base_dir })
    }
}

impl Backend for CopyBackend {
    fn pull_files(&self, files: &HashSet<String>) -> io::Result<bool> {
        for digest in files {
            let src = self.remote.join(digest);
            let dst = self.base_dir.join(digest);
            if dst.exists() {
                continue;
            }
            fs::copy(&src, &dst).map_err(|e| {
                io::Error::new(e.kind(), format!("copy {} -> {}: {}", src.display(), dst.display(), e))
            })?;
            set_readonly(&dst)?;
        }
        Ok(true)
    }

    fn push_files(&self, files: &HashSet<String>) -> io::Result<bool> {
        fs::create_dir_all(&self.remote)?;
        for digest in files {
            let src = self.base_dir.join(digest);
            let dst = self.remote.join(digest);
            if dst.exists() {
                continue;
            }
            fs::copy(&src, &dst).map_err(|e| {
                io::Error::new(e.kind(), format!("copy {} -> {}: {}", src.display(), dst.display(), e))
            })?;
        }
        Ok(true)
    }
}


/// RsyncBackend: push/pull via rsync over SSH or rsyncd
pub struct RsyncBackend {
    remote_url: String,
    ssh_user: Option<String>,
    ssh_port: Option<String>,
    /// Local fat object directory.
    base_dir: PathBuf,
    /// True when remote_url contains "::" (rsyncd protocol).
    is_rsyncd: bool,
}

impl RsyncBackend {
    pub fn new(
        base_dir: PathBuf,
        remote_url: &str,
        ssh_user: Option<String>,
        ssh_port: Option<String>,
    ) -> Self {
        let is_rsyncd = remote_url.contains("::");
        RsyncBackend {
            remote_url: remote_url.to_string(),
            ssh_user,
            ssh_port,
            base_dir,
            is_rsyncd,
        }
    }

    fn build_command(&self, push: bool) -> Command {
        let (src, dst) = if push {
            (format!("{}/", self.base_dir.display()), format!("{}/", self.remote_url))
        } else {
            (format!("{}/", self.remote_url), format!("{}/", self.base_dir.display()))
        };

        let mut cmd = Command::new("rsync");
        cmd.args([
            "-s",
            "--progress",
            "--ignore-existing",
            "--from0",
            "--files-from=-",
            &src,
            &dst,
        ]);

        if !self.is_rsyncd {
            // Build --rsh argument
            let mut rsh = env::var("GIT_SSH").unwrap_or_else(|_| "ssh".to_string());
            if let Some(user) = &self.ssh_user {
                rsh.push_str(&format!(" -l {}", user));
            }
            if let Some(port) = &self.ssh_port {
                rsh.push_str(&format!(" -p {}", port));
            }
            cmd.arg(format!("--rsh={}", rsh));
        }

        let cmd_name = if push { "push" } else { "pull" };
        log::debug!("rsync {cmd_name} command: {cmd:?}");

        cmd
    }

    fn run_rsync(&self, push: bool, files: &HashSet<String>) -> io::Result<bool> {
        let mut cmd = self.build_command(push);
        cmd.stdin(Stdio::piped());

        let mut child = cmd.spawn().map_err(|e| {
            io::Error::new(e.kind(), format!("failed to spawn rsync: {}", e))
        })?;

        {
            let stdin = child.stdin.as_mut().unwrap();
            let list: Vec<u8> = files.iter()
                .flat_map(|f| f.bytes().chain(std::iter::once(0u8)))
                .collect();
            stdin.write_all(&list)?;
        }

        let status = child.wait()?;
        Ok(status.success())
    }
}

impl Backend for RsyncBackend {
    fn pull_files(&self, files: &HashSet<String>) -> io::Result<bool> {
        if files.is_empty() {
            return Ok(true);
        }
        self.run_rsync(false, files)
    }

    fn push_files(&self, files: &HashSet<String>) -> io::Result<bool> {
        if files.is_empty() {
            return Ok(true);
        }
        self.run_rsync(true, files)
    }
}


/// Load a backend from the `.gitfat` config file.
///
/// `name` selects a specific section (e.g. "rsync" or "copy"); when `None`
/// the first section found in the file is used.
pub fn load_backend(
    base_dir: PathBuf,
    config_path: &Path,
    name: Option<&str>,
) -> io::Result<Box<dyn Backend>> {
    // We can use git2::Config for this since it is actually just a plain
    // INI-style parser.
    let cfg = git2::Config::open(config_path).map_err(|e| {
        log::warn!("This does not appear to be a repository managed by git-fat. \
                   Missing config file at: {}", config_path.display());
        io::Error::new(io::ErrorKind::Other, format!("cannot open {}: {}", config_path.display(), e))
    })?;

    // Collect all (section, key, value) entries so we can find sections.
    // git2 Config::entries() iterates every entry; we need to discover which
    // backend section(s) exist.
    let section = if let Some(n) = name {
        n.to_string()
    } else {
        // Scan entries to find the first top-level section name.
        let mut found: Option<String> = None;
        let entries = cfg.entries(None).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        entries.for_each(|entry| {
            if found.is_some() {
                return;
            }
            if let Some(name) = entry.name() {
                if let Some(sec) = name.split('.').next() {
                    found = Some(sec.to_string());
                }
            }
        }).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        found.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("no backend configuration found in {}", config_path.display()))
        })?
    };

    let remote_key = format!("{}.remote", section);
    let remote = cfg.get_string(&remote_key).map_err(|_| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("missing '{}' in {}", remote_key, config_path.display()),
        )
    })?;

    match section.as_str() {
        "copy" => Ok(Box::new(CopyBackend::new(base_dir, &remote)?)),
        "rsync" => {
            let ssh_user = cfg.get_string(&format!("{}.sshuser", section)).ok();
            let ssh_port = cfg.get_string(&format!("{}.sshport", section)).ok();
            Ok(Box::new(RsyncBackend::new(base_dir, &remote, ssh_user, ssh_port)))
        }
        other => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("unknown backend '{}' in {}", other, config_path.display()),
        )),
    }
}

// Helpers
fn set_readonly(path: &Path) -> io::Result<()> {
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_readonly(true);
    fs::set_permissions(path, perms)
}
