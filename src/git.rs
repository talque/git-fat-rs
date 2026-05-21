/// Additional git utilities
use git2::{Repository, ObjectType, Oid};
use std::io;
use std::path::PathBuf;


pub struct ManagedFiles<'repo> {
    repo: &'repo Repository,
    stack: Option<Vec<(PathBuf, git2::Tree<'repo>, usize)>>,
}


/// Walk the HEAD tree and return (repo-relative path, fat-digest, size) for every
/// blob whose content is a fat placeholder.
impl<'repo> ManagedFiles<'repo> {
    pub fn new(repo: &'repo Repository) -> Self {
        Self { repo, stack: None }
    }
}


impl<'repo> Iterator for ManagedFiles<'repo> {
    type Item = io::Result<(PathBuf, Oid, usize)>; // path, git oid, blob size

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
            self.stack = Some(vec![(PathBuf::new(), tree, 0)]);
        };

        let stack = self.stack.as_mut().unwrap();

        while let Some((_, tree, index)) = stack.last_mut() {
            if *index >= tree.len() {
                stack.pop();
                continue;
            }

            let (id, kind, path) = {
                let (dir_path, tree, index) = stack.last_mut().unwrap();
                let entry = tree.get(*index)?;
                *index += 1;
                (entry.id(), entry.kind(), dir_path.join(entry.name().unwrap_or_default()))
            };

            match kind {
                Some(ObjectType::Blob) => {
                    match self.repo.odb()
                        .and_then(|odb| odb.read_header(id))
                        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
                    {
                        Ok((size, _)) => return Some(Ok((path, id, size))),
                        Err(e) => return Some(Err(e)),
                    }
                }
                Some(ObjectType::Tree) => {
                    if let Ok(subtree) = self.repo.find_tree(id) {
                        stack.push((path, subtree, 0));
                    }
                }
                _ => {}
            }
        }

        None
    }
}
