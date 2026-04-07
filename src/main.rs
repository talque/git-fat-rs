use sha1::{Sha1, Digest};
use std::env;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{PathBuf};
use tempfile::NamedTempFile;

const BLOCK_SIZE: usize = 4096 * 1024 * 1024;  // 4MB
const COOKIE: &str = "#$# git-fat ";


fn git_dir() -> PathBuf {
    match env::var("GIT_DIR") {
        Ok(val) => PathBuf::from(val),
        Err(_) => usage(Some("GIT_DIR is not set; cannot determine git directory"))
    }
}


fn git_common_dir() -> PathBuf {
    let git_dir = git_dir();

    // Check if in a worktree
    if git_dir.parent()
        .and_then(|p| p.file_name())
        .map(|f| f == "worktrees")
        .unwrap_or(false)
    {
        return git_dir.parent().unwrap().parent().unwrap().to_path_buf();
    }

    git_dir
}


fn obj_dir() -> PathBuf {
    let git_dir = git_common_dir();
    git_dir.join("fat/objects")
}


fn encode_placeholder(digest: &str, size: usize) -> Vec<u8> {
    format!("{}{} {:20}\n", COOKIE, digest, size).into_bytes()
}


fn read_blocks<R: Read>(reader: &mut R) -> io::Result<impl Iterator<Item=io::Result<Vec<u8>>>> {
    Ok(std::iter::from_fn(move || {
        let mut buf = vec![0u8; BLOCK_SIZE];
        match reader.read(&mut buf) {
            Ok(0) => None,
            Ok(n) => Some(Ok(buf[..n].to_vec())),
            Err(e) => Some(Err(e)),
        }
    }))
}


fn filter_clean<R: Read, W: Write>(mut instream: R, mut outstream: W) -> io::Result<()> {
    let objdir = obj_dir();
    fs::create_dir_all(&objdir)?;

    let mut temp = NamedTempFile::new_in(&objdir)?;
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


fn filter_smudge<R: Read, W: Write>(mut instream: R, mut outstream: W) -> io::Result<()> {
    let objdir = obj_dir();
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

    // Not a fat object, just copy the original data
    outstream.write_all(&first_block)?;
    io::copy(&mut instream, &mut outstream)?;
    Ok(())
}


fn usage(msg: Option<&str>) -> ! {
    msg.map(|m| eprintln!("{}", m));
    eprintln!("Usage: git-fat <filter-clean|filter-smudge>");
    std::process::exit(1);
}


fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        usage(None);
    }

    let cmd = &args[1];
    match cmd.as_str() {
        "filter-clean" => filter_clean(io::stdin(), io::stdout())?,
        "filter-smudge" => filter_smudge(io::stdin(), io::stdout())?,
        _ => {
            usage(Some(format!("Unknown command: {}", cmd).as_str()));
        }
    }
    Ok(())
}
