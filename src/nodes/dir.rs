// Copyright 2018 Google Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License"); you may not
// use this file except in compliance with the License.  You may obtain a copy
// of the License at:
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS, WITHOUT
// WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.  See the
// License for the specific language governing permissions and limitations
// under the License.

extern crate fuse;
extern crate time;

use {Cache, IdGenerator};
use failure::{Error, ResultExt};
use nix::{errno, unistd};
use nodes::{KernelError, Node, NodeResult, conv};
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Takes the components of a path and returns the first normal component and the rest.
///
/// This assumes that the input path is normalized and that the very first component is a normal
/// component as defined by `Component::Normal`.
fn split_components<'a>(components: &'a [Component<'a>]) -> (&'a OsStr, &'a [Component<'a>]) {
    debug_assert!(!components.is_empty());
    let name = match components[0] {
        Component::Normal(name) => name,
        _ => panic!("Input list of components is not normalized"),
    };
    (name, &components[1..])
}

/// Representation of a directory entry.
struct Dirent {
    node: Arc<Node>,
    explicit_mapping: bool,
}

/// Representation of a directory node.
pub struct Dir {
    inode: u64,
    writable: bool,
    state: Mutex<MutableDir>,
}

/// Holds the mutable data of a directory node.
struct MutableDir {
    parent: u64,
    underlying_path: Option<PathBuf>,
    attr: fuse::FileAttr,
    children: HashMap<OsString, Dirent>,
}

impl Dir {
    /// Creates a new scaffold directory to represent an in-memory directory.
    ///
    /// The directory's timestamps are set to `now` and the ownership is set to the current user.
    pub fn new_empty(inode: u64, parent: Option<&Node>, now: time::Timespec) -> Arc<Node> {
        let attr = fuse::FileAttr {
            ino: inode,
            kind: fuse::FileType::Directory,
            nlink: 2,  // "." entry plus whichever initial named node points at this.
            size: 2,  // TODO(jmmv): Reevaluate what directory sizes should be.
            blocks: 1,  // TODO(jmmv): Reevaluate what directory blocks should be.
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
            perm: 0o555 as u16,  // Scaffold directories cannot be mutated by the user.
            uid: unistd::getuid().as_raw(),
            gid: unistd::getgid().as_raw(),
            rdev: 0,
            flags: 0,
        };

        let state = MutableDir {
            parent: parent.map_or(inode, |node| node.inode()),
            underlying_path: None,
            attr: attr,
            children: HashMap::new(),
        };

        Arc::new(Dir {
            inode: inode,
            writable: false,
            state: Mutex::from(state),
        })
    }

    /// Creates a new directory whose contents are backed by another directory.
    ///
    /// `inode` is the node number to assign to the created in-memory directory and has no relation
    /// to the underlying directory.  `underlying_path` indicates the path to the directory outside
    /// of the sandbox that backs this one.  `fs_attr` contains the stat data for the given path.
    ///
    /// `fs_attr` is an input parameter because, by the time we decide to instantiate a directory
    /// node (e.g. as we discover directory entries during readdir or lookup), we have already
    /// issued a stat on the underlying file system and we cannot re-do it for efficiency reasons.
    pub fn new_mapped(inode: u64, underlying_path: &Path, fs_attr: &fs::Metadata, writable: bool)
        -> Arc<Node> {
        if !fs_attr.is_dir() {
            panic!("Can only construct based on dirs");
        }
        let attr = conv::attr_fs_to_fuse(underlying_path, inode, &fs_attr);

        let state = MutableDir {
            parent: inode,
            underlying_path: Some(PathBuf::from(underlying_path)),
            attr: attr,
            children: HashMap::new(),
        };

        Arc::new(Dir { inode, writable, state: Mutex::from(state) })
    }

    /// Creates a new scaffold directory as a child of the current one.
    ///
    /// Errors are all logged, not reported.  The rationale is that a scaffold directory for an
    /// intermediate path component of a mapping has to always be created, as it takes preference
    /// over any other on-disk contents.
    ///
    /// This is purely a helper function for `map`.  As a result, the caller is responsible for
    /// inserting the new directory into the children of the current directory.
    fn new_scaffold_child(&self, underlying_path: Option<&PathBuf>, name: &OsStr, ids: &IdGenerator,
        now: time::Timespec) -> Arc<Node> {
        if let Some(path) = underlying_path {
            let child_path = path.join(name);
            match fs::symlink_metadata(&child_path) {
                Ok(fs_attr) => {
                    if fs_attr.is_dir() {
                        return Dir::new_mapped(ids.next(), &child_path, &fs_attr, self.writable);
                    }

                    info!("Mapping clobbers non-directory {} with an immutable directory",
                        child_path.display());
                },
                Err(e) => {
                    if e.kind() != io::ErrorKind::NotFound {
                        warn!("Mapping clobbers {} due to an error: {}", child_path.display(), e);
                    }
                },
            }
        }
        Dir::new_empty(ids.next(), Some(self), now)
    }
}

impl Node for Dir {
    fn inode(&self) -> u64 {
        self.inode
    }

    fn writable(&self) -> bool {
        self.writable
    }

    fn file_type_cached(&self) -> fuse::FileType {
        fuse::FileType::Directory
    }

    fn map(&self, components: &[Component], underlying_path: &Path, writable: bool,
        ids: &IdGenerator, cache: &Cache) -> Result<(), Error> {
        let (name, remainder) = split_components(components);

        let mut state = self.state.lock().unwrap();

        if let Some(dirent) = state.children.get(name) {
            // TODO(jmmv): We should probably mark this dirent as an explicit mapping if it already
            // wasn't, but the Go variant of this code doesn't do this -- so investigate later.
            ensure!(dirent.node.file_type_cached() == fuse::FileType::Directory, "Already mapped");
            return dirent.node.map(remainder, underlying_path, writable, ids, cache);
        }

        let child = if remainder.is_empty() {
            let fs_attr = fs::symlink_metadata(underlying_path)
                .context(format!("Stat failed for {:?}", underlying_path))?;
            cache.get_or_create(ids, underlying_path, &fs_attr, writable)
        } else {
            self.new_scaffold_child(state.underlying_path.as_ref(), name, ids, time::get_time())
        };

        let dirent = Dirent { node: child.clone(), explicit_mapping: true };
        state.children.insert(name.to_os_string(), dirent);

        if remainder.is_empty() {
            Ok(())
        } else {
            ensure!(child.file_type_cached() == fuse::FileType::Directory, "Already mapped");
            child.map(remainder, underlying_path, writable, ids, cache)
        }
    }

    fn getattr(&self) -> NodeResult<fuse::FileAttr> {
        let mut state = self.state.lock().unwrap();

        let new_attr = match state.underlying_path.as_ref() {
            Some(path) => {
                let fs_attr = fs::symlink_metadata(path)?;
                if !fs_attr.is_dir() {
                    warn!("Path {:?} backing a directory node is no longer a directory; got {:?}",
                        path, fs_attr.file_type());
                    return Err(KernelError::from_errno(errno::Errno::EIO));
                }
                Some(conv::attr_fs_to_fuse(path, self.inode, &fs_attr))
            },
            None => None,
        };
        if let Some(new_attr) = new_attr {
            state.attr = new_attr;
        }

        Ok(state.attr)
    }

    fn lookup(&self, name: &OsStr, ids: &IdGenerator, cache: &Cache)
        -> NodeResult<(Arc<Node>, fuse::FileAttr)> {
        let mut state = self.state.lock().unwrap();

        if let Some(dirent) = state.children.get(name) {
            let refreshed_attr = dirent.node.getattr()?;
            return Ok((dirent.node.clone(), refreshed_attr))
        }

        let (child, attr) = {
            let path = match state.underlying_path.as_ref() {
                Some(underlying_path) => underlying_path.join(name),
                None => return Err(KernelError::from_errno(errno::Errno::ENOENT)),
            };
            let fs_attr = fs::symlink_metadata(&path)?;
            let node = cache.get_or_create(ids, &path, &fs_attr, self.writable);
            let attr = conv::attr_fs_to_fuse(path.as_path(), node.inode(), &fs_attr);
            (node, attr)
        };
        let dirent = Dirent {
            node: child.clone(),
            explicit_mapping: false,
        };
        state.children.insert(name.to_os_string(), dirent);
        Ok((child, attr))
    }

    fn readdir(&self, ids: &IdGenerator, cache: &Cache, reply: &mut fuse::ReplyDirectory)
        -> NodeResult<()> {
        let mut state = self.state.lock().unwrap();

        reply.add(self.inode, 0, fuse::FileType::Directory, ".");
        reply.add(state.parent, 1, fuse::FileType::Directory, "..");
        let mut pos = 2;

        // First, return the entries that correspond to explicit mappings performed by the user at
        // either mount time or during a reconfiguration.  Those should clobber any on-disk
        // contents that we discover later when we issue the readdir on the underlying directory,
        // if any.
        for (name, dirent) in &state.children {
            if dirent.explicit_mapping {
                reply.add(dirent.node.inode(), pos, dirent.node.file_type_cached(), name);
                pos += 1;
            }
        }

        if state.underlying_path.as_ref().is_none() {
            return Ok(());
        }

        let entries = fs::read_dir(state.underlying_path.as_ref().unwrap())?;
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();

            if let Some(dirent) = state.children.get(&name) {
                if dirent.explicit_mapping {
                    // Found an on-disk entry that also corresponds to an explicit mapping by the
                    // user.  Nothing to do: we already handled this case above.
                    continue;
                }
            }

            let path = state.underlying_path.as_ref().unwrap().join(&name);
            let fs_attr = entry.metadata()?;
            let fs_type = conv::filetype_fs_to_fuse(&path, fs_attr.file_type());
            let child = cache.get_or_create(ids, &path, &fs_attr, self.writable);

            reply.add(child.inode(), pos, fs_type, &name);
            // Do the insertion into state.children after calling reply.add() to be able to move
            // the name into the key without having to copy it again.
            let dirent = Dirent {
                node: child.clone(),
                explicit_mapping: false,
            };
            // TODO(jmmv): We should remove stale entries at some point (possibly here), but the Go
            // variant does not do this so any implications of this are not tested.  The reason this
            // hasn't caused trouble yet is because: on readdir, we don't use any contents from
            // state.children that correspond to unmapped entries, and any stale entries visited
            // during lookup will result in an ENOENT.
            state.children.insert(name, dirent);

            pos += 1;
        }
        Ok(())
    }
}