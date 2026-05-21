/// Additional git utilities
use git2::{Repository, ObjectType};
use std::env;
use std::io;
use std::path::{PathBuf};

use crate::fat;


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
        .unwrap_or(false) {
        return git_dir.parent().unwrap().parent().unwrap().to_path_buf();
    }
    git_dir.clone()
}


pub struct ManagedFiles<'repo> {
    repo: &'repo Repository,
    stack: Option<Vec<(PathBuf, git2::Tree<'repo>)>>,
    index: usize,
}


/// Walk the HEAD tree and return (repo-relative path, fat-digest, size) for every
/// blob whose content is a fat placeholder.
impl<'repo> ManagedFiles<'repo> {
    pub fn new(repo: &'repo Repository) -> Self {
        Self{ repo, stack: None, index: 0 }
    }
}


impl<'repo> Iterator for ManagedFiles<'repo> {
    type Item = io::Result<(PathBuf, String, usize)>; // path, digest, blob size

    fn next(&mut self) -> Option<Self::Item> {
        // Initialize if necessary
        if self.stack.is_none() {
            let tree = match self.repo.head() {
                Err(_) => return None,  // empty/unborn repo
                Ok(head) => match head.peel_to_commit().and_then(|c| c.tree()) {
                    Ok(tree) => tree,
                    Err(e) => return Some(Err(io::Error::new(io::ErrorKind::Other, e)))
                }
            };
            self.stack = Some(vec![(PathBuf::new(), tree)]);
        };

        let stack = self.stack.as_mut().unwrap();

        while let Some((_, tree)) = stack.last_mut() {
            if self.index >= tree.len() {
                stack.pop();
                self.index = 0;
                continue;
            }

            // Extract what we need from &'a mut tree so we can release
            // the &'a mut stack associated
            let (id, kind, path) = {
                let (dir_path, tree) = stack.last().unwrap();
                let entry = tree.get(self.index)?;
                self.index += 1;
                (entry.id(), entry.kind(), dir_path.join(entry.name().unwrap_or_default()))
            };

            match kind {
                Some(ObjectType::Blob) => {
                    match self.repo.find_blob(id) {
                        Ok(blob) => {
                            let size = blob.size();
                            let digest = fat::extract_digest(blob.content())
                                .unwrap_or_else(|_| "".to_string());
                            return Some(Ok((path, digest, size)));
                        }
                        Err(e) => return Some(Err(io::Error::new(io::ErrorKind::Other, e))),
                    }
                }
                Some(ObjectType::Tree) => {
                    if let Ok(subtree) = self.repo.find_tree(id) {
                        stack.push((path.clone(), subtree));
                    }
                }
                _ => {}
            }
        }

        None
    }
}
