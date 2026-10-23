//! A sandboxed process's explicit filesystem and network authority. The
//! trusted platform supplies canonical backing paths; the kernel compares them
//! as pure data at both configured and executable spawns. Guest paths alone
//! are insufficient: equal mount names may expose different host files.
//! Dynamic paths are also resolved and checked by the platform at actual
//! launch. A project's process is not sandboxed ([`Namespace::unconfined`]):
//! what it reaches is what its Linux user may reach.

use std::path::PathBuf;

use nucleus_protocol::{ChildBind, ChildProgram, Error, ErrorCode};

/// One directory or file visible to a process, as the process sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamespaceBind {
    /// Absolute guest path.
    pub path: String,
    /// Canonical backing path, supplied by the trusted platform. Never sent to programs.
    pub source: PathBuf,
    pub writable: bool,
}

/// A process's view of the filesystem and network. Empty by default: a process
/// with no binds can start no executable of its own.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Namespace {
    pub binds: Vec<NamespaceBind>,
    pub network: bool,
}

fn refuse(message: String) -> Error {
    Error::new(ErrorCode::PermissionDenied, message)
}

/// Splits an absolute, normalized guest path into its segments, rejecting
/// anything that is not absolute or contains an empty, `.`, or `..` segment.
pub fn segments(path: &str) -> Result<Vec<&str>, Error> {
    let Some(rest) = path.strip_prefix('/') else {
        return Err(Error::new(ErrorCode::InvalidInput, format!("{path}: guest paths are absolute")));
    };
    if rest.is_empty() {
        return Ok(Vec::new()); // "/" itself
    }
    let mut out = Vec::new();
    for segment in rest.split('/') {
        match segment {
            "" | "." | ".." => {
                return Err(Error::new(
                    ErrorCode::InvalidInput,
                    format!("{path}: guest paths have no empty, `.`, or `..` segments"),
                ));
            }
            _ => out.push(segment),
        }
    }
    Ok(out)
}

/// True when `path` is `base` or lies beneath it.
fn beneath(path: &[&str], base: &[&str]) -> bool {
    path.len() >= base.len() && path[..base.len()] == *base
}

impl Namespace {
    /// What a project's process sees: it is an ordinary process of its user,
    /// in no sandbox, so everything that user may reach, the network
    /// included. The one exception is a process its spawner started without
    /// network (`network: false`), which the platform cuts off: the discovery
    /// workers' shell.
    pub fn unconfined(network: bool) -> Self {
        Self {
            binds: vec![NamespaceBind { path: "/".to_owned(), source: "/".into(), writable: true }],
            network,
        }
    }

    /// The namespace of `child` started as a project identity from this
    /// one. Such a process is not sandboxed, so it takes no binds, and its
    /// executable must be one the spawner can itself see.
    ///
    /// Its network is its user's, not the spawner's to give: `chosen` says
    /// the spawner named the identity with `spawn_as`, the authority to start
    /// a process of that user, and it may start one with the VM's network
    /// whatever its own sandbox has, or one without (`network: false`),
    /// which is then cut off. A project's process starts its own children
    /// only as what it is, ordinary processes with network: `network: false`
    /// is refused rather than ignored, and a process that was cut off starts
    /// none, since its child would have the network it was denied.
    pub fn project(&self, child: &ChildProgram, chosen: bool) -> Result<Namespace, Error> {
        self.covering(&child.executable)?;
        if !child.binds.is_empty() {
            return Err(Error::new(
                ErrorCode::InvalidInput,
                "a project's process is not sandboxed and takes no binds: it sees the files its user may \
                 reach, at their real paths",
            ));
        }
        if !chosen {
            if !self.network {
                return Err(refuse(
                    "a process started without network starts no process of its own".to_owned(),
                ));
            }
            if !child.network {
                return Err(Error::new(
                    ErrorCode::InvalidInput,
                    "a project's process is an ordinary process of its user and has the VM's network: it \
                     cannot be started without (`network: false`)",
                ));
            }
        }
        Ok(Self::unconfined(child.network))
    }

    pub(crate) fn validate(&self) -> Result<(), Error> {
        let mut seen = std::collections::BTreeSet::new();
        for bind in &self.binds {
            segments(&bind.path)?;
            if !seen.insert(&bind.path) {
                return Err(Error::new(ErrorCode::InvalidInput, "duplicate guest mount in namespace"));
            }
        }
        Ok(())
    }

    /// The most specific of this namespace's binds that covers `path`, or an
    /// error if none does.
    fn covering(&self, path: &str) -> Result<&NamespaceBind, Error> {
        let target = segments(path)?;
        let mut best: Option<(usize, &NamespaceBind)> = None;
        for bind in &self.binds {
            let base = segments(&bind.path)?;
            if beneath(&target, &base) && best.is_none_or(|(len, _)| base.len() >= len) {
                best = Some((base.len(), bind));
            }
        }
        best.map(|(_, bind)| bind)
            .ok_or_else(|| refuse(format!("{path} is not beneath anything this process can see")))
    }

    /// Configured programs are launch recipes, not an additional source of authority.
    /// The check is the kernel's, not a service's, because a process sends
    /// `Spawn` to the kernel directly and so would bypass a service in front.
    /// Compare backing paths, not guest spellings: two `/data` mounts may expose
    /// completely different host directories. Overlaid parent subtrees cannot
    /// be rebound wholesale, because that would expose the hidden backing data.
    pub fn admits(&self, child: &Namespace) -> Result<(), Error> {
        self.validate()?;
        child.validate()?;
        if child.network && !self.network {
            return Err(refuse("the child asks for network the spawner does not have".to_owned()));
        }
        for target in &child.binds {
            segments(&target.path)?;
            let allowed = self.binds.iter().any(|source| {
                let Ok(suffix) = target.source.strip_prefix(&source.source) else { return false };
                let suffix = suffix.to_string_lossy();
                if target.writable && !source.writable {
                    return false;
                }
                let guest = format!("{}/{}", source.path.trim_end_matches('/'), suffix)
                    .trim_end_matches('/')
                    .to_owned();
                let under = |path: &str, base: &str| {
                    path == base || path.strip_prefix(base).is_some_and(|rest| rest.starts_with('/'))
                };
                // This source may itself be hidden by a more specific mount,
                // or contain a mount that masks part of its backing directory.
                !self.binds.iter().any(|overlay| {
                    overlay.path != source.path
                        && under(&overlay.path, &source.path)
                        && (under(&guest, &overlay.path) || under(&overlay.path, &guest))
                })
            });
            if !allowed {
                return Err(refuse(format!(
                    "{} is outside the spawner's visible filesystem authority",
                    target.path
                )));
            }
        }
        Ok(())
    }

    /// Checks that `child` fits within this namespace and returns the namespace
    /// the child would have. Pure: the executable's readability and any symlink
    /// escape are the platform's to verify at start.
    pub fn attenuate(&self, child: &ChildProgram) -> Result<Namespace, Error> {
        if child.network && !self.network {
            return Err(refuse("the child asks for network the spawner does not have".to_owned()));
        }
        // The executable must be something the spawner can itself see.
        self.covering(&child.executable)?;

        let mut binds = Vec::with_capacity(child.binds.len());
        for ChildBind { path, mount, writable } in &child.binds {
            let source = self.covering(path)?;
            if *writable && !source.writable {
                return Err(refuse(format!(
                    "{path} is read-only for the spawner, so the child cannot write it"
                )));
            }
            let mount = mount.as_deref().unwrap_or(path);
            // The mount point is validated so the child's own namespace stays normalized.
            segments(mount)?;
            let suffix: Vec<_> = segments(path)?.into_iter().skip(segments(&source.path)?.len()).collect();
            let backing = source.source.join(suffix.iter().collect::<PathBuf>());
            binds.push(NamespaceBind { path: mount.to_owned(), source: backing, writable: *writable });
        }
        let namespace = Namespace { binds, network: child.network };
        self.admits(&namespace)?;
        Ok(namespace)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn ns() -> Namespace {
        Namespace {
            binds: vec![
                NamespaceBind { path: "/work".to_owned(), source: "/work".into(), writable: true },
                NamespaceBind { path: "/plugins".to_owned(), source: "/plugins".into(), writable: false },
            ],
            network: false,
        }
    }
    fn child(executable: &str, binds: Vec<ChildBind>, network: bool) -> ChildProgram {
        ChildProgram { executable: executable.to_owned(), args: vec![], binds, network }
    }
    fn bind(path: &str, writable: bool) -> ChildBind {
        ChildBind { path: path.to_owned(), mount: None, writable }
    }

    #[test]
    fn a_child_within_the_namespace_is_accepted_and_narrowed() {
        let got = ns()
            .attenuate(&child(
                "/plugins/tool",
                vec![bind("/work/scratch", true), bind("/plugins", false)],
                false,
            ))
            .unwrap();
        assert_eq!(got.binds.len(), 2);
        assert!(!got.network);
        assert_eq!(
            got.binds[0],
            NamespaceBind {
                path: "/work/scratch".to_owned(),
                source: "/work/scratch".into(),
                writable: true
            }
        );
    }

    #[test]
    fn a_mount_point_remaps_the_child_view() {
        let got = ns()
            .attenuate(&ChildProgram {
                executable: "/plugins/tool".to_owned(),
                args: vec![],
                binds: vec![ChildBind {
                    path: "/work".to_owned(),
                    mount: Some("/data".to_owned()),
                    writable: true,
                }],
                network: false,
            })
            .unwrap();
        assert_eq!(
            got.binds[0],
            NamespaceBind { path: "/data".to_owned(), source: "/work".into(), writable: true }
        );
    }

    #[test]
    fn every_widening_is_refused() {
        let n = ns();
        // Executable the spawner cannot see.
        assert_eq!(
            n.attenuate(&child("/etc/passwd", vec![], false)).unwrap_err().code,
            ErrorCode::PermissionDenied
        );
        // A prefix that only looks like a bind ("/workspace" is not under "/work").
        assert_eq!(
            n.attenuate(&child("/workspace/x", vec![], false)).unwrap_err().code,
            ErrorCode::PermissionDenied
        );
        // A bind outside anything the spawner sees.
        assert_eq!(
            n.attenuate(&child("/work/t", vec![bind("/etc", false)], false)).unwrap_err().code,
            ErrorCode::PermissionDenied
        );
        // Writable where the spawner is read-only.
        assert_eq!(
            n.attenuate(&child("/work/t", vec![bind("/plugins", true)], false)).unwrap_err().code,
            ErrorCode::PermissionDenied
        );
        // Network the spawner does not have.
        assert_eq!(
            n.attenuate(&child("/work/t", vec![], true)).unwrap_err().code,
            ErrorCode::PermissionDenied
        );
    }

    #[test]
    fn traversal_in_a_path_is_rejected_as_invalid_input() {
        let n = ns();
        assert_eq!(
            n.attenuate(&child("/work/../etc/x", vec![], false)).unwrap_err().code,
            ErrorCode::InvalidInput
        );
        assert_eq!(n.attenuate(&child("work/x", vec![], false)).unwrap_err().code, ErrorCode::InvalidInput);
        assert_eq!(
            n.attenuate(&child("/work/t", vec![bind("/work/../secret", false)], false)).unwrap_err().code,
            ErrorCode::InvalidInput
        );
    }

    #[test]
    fn duplicate_guest_mounts_cannot_expose_hidden_sources_to_children() {
        let mut parent = ns();
        parent.binds.push(NamespaceBind {
            path: "/work".into(),
            source: "/replacement".into(),
            writable: false,
        });
        assert_eq!(parent.admits(&ns()).unwrap_err().code, ErrorCode::InvalidInput);
    }

    #[test]
    fn inherited_backing_subtrees_can_move_but_mount_overlays_cannot_be_bypassed() {
        let mut parent = ns();
        let child = Namespace {
            binds: vec![NamespaceBind { path: "/other".into(), source: "/work/sub".into(), writable: true }],
            network: false,
        };
        parent.admits(&child).unwrap();
        parent.binds.push(NamespaceBind {
            path: "/work/sub/private".into(),
            source: "/hidden".into(),
            writable: false,
        });
        assert_eq!(parent.admits(&child).unwrap_err().code, ErrorCode::PermissionDenied);
        assert_eq!(
            parent.attenuate(&child_program_for_overlay()).unwrap_err().code,
            ErrorCode::PermissionDenied
        );
    }

    fn child_program_for_overlay() -> ChildProgram {
        child("/plugins/tool", vec![bind("/work/sub", true)], false)
    }

    /// A project's process takes no binds and starts only what its spawner
    /// sees. A `spawn_as` holder starts it with the VM's network, whatever
    /// its own sandbox has, or without; a project's process starts children
    /// with network only, and one that was cut off starts none.
    #[test]
    fn a_project_process_is_unconfined_takes_no_binds_and_has_its_users_network() {
        let n = ns();
        let tool = |network: bool| child("/plugins/tool", vec![], network);
        let plain = n.project(&tool(true), true).unwrap();
        assert_eq!(plain, Namespace::unconfined(true), "from a spawner whose sandbox has no network");
        assert_eq!(
            n.project(&tool(false), true).unwrap(),
            Namespace::unconfined(false),
            "the workers' shell"
        );
        assert_eq!(
            n.project(&child("/plugins/tool", vec![bind("/work", true)], true), true).unwrap_err().code,
            ErrorCode::InvalidInput
        );
        assert_eq!(
            n.project(&child("/etc/passwd", vec![], true), true).unwrap_err().code,
            ErrorCode::PermissionDenied
        );
        // Its own children: anything it can see, with network like itself.
        let python = |network: bool| child("/usr/bin/python3", vec![], network);
        assert_eq!(plain.project(&python(true), false).unwrap(), Namespace::unconfined(true));
        assert_eq!(plain.project(&python(false), false).unwrap_err().code, ErrorCode::InvalidInput);
        let cut_off = Namespace::unconfined(false);
        for network in [false, true] {
            assert_eq!(
                cut_off.project(&python(network), false).unwrap_err().code,
                ErrorCode::PermissionDenied,
                "{network}"
            );
        }
    }

    #[test]
    fn network_passes_through_only_when_the_spawner_has_it() {
        let mut n = ns();
        n.network = true;
        assert!(n.attenuate(&child("/work/t", vec![], true)).unwrap().network);
        assert!(!n.attenuate(&child("/work/t", vec![], false)).unwrap().network);
    }
}
