//! Capability table: minting, attenuated derivation, validation and shared
//! budgets along the delegation chain.
//!
//! Invariants:
//! - Delegation is never wider than its parent in actions, resource scope,
//!   expiry, or delegability. Fresh-object creation is a distinct derivation:
//!   a checked factory authorizes new endpoint objects, never existing ones.
//! - A capability is valid only while it and every ancestor exist and are
//!   unexpired. Removing an ancestor therefore invalidates the whole subtree
//!   without visiting it.
//! - A call is charged to the capability used and to every ancestor, so
//!   delegating a budgeted capability shares the budget instead of copying it.

use std::collections::{BTreeMap, BTreeSet};

use nucleus_protocol::{ActionName, EndpointName, Error, ErrorCode, ProcessId, ResourcePath};

/// Kernel-internal capability identity. Never reused and never sent to a process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct CapId(u64);

/// Authority requested for a new capability, root or derived.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapSpec {
    pub endpoint: EndpointName,
    pub actions: BTreeSet<ActionName>,
    pub resource: ResourcePath,
    /// Lifetime from the moment of creation.
    pub ttl_ms: Option<u64>,
    pub delegable: bool,
    pub max_calls: Option<u64>,
}

#[derive(Clone, Debug)]
pub(crate) struct Capability {
    pub holder: ProcessId,
    pub endpoint: EndpointName,
    pub actions: BTreeSet<ActionName>,
    pub resource: ResourcePath,
    pub expires_at: Option<u64>,
    pub delegable: bool,
    pub max_calls: Option<u64>,
    parent: Option<CapId>,
    depth: u32,
    used: u64,
}

#[derive(Debug, Default)]
pub(crate) struct CapTable {
    caps: BTreeMap<CapId, Capability>,
    next: u64,
}

fn revoked(message: &str) -> Error {
    Error::new(ErrorCode::Revoked, message)
}

impl CapTable {
    fn insert(&mut self, cap: Capability) -> CapId {
        let id = CapId(self.next);
        self.next += 1;
        self.caps.insert(id, cap);
        id
    }

    /// Mints authority with no parent. Only the operator's boot configuration
    /// reaches this; no wire message does.
    pub fn mint_root(&mut self, holder: ProcessId, spec: CapSpec, now: u64) -> CapId {
        self.insert(Capability {
            holder,
            endpoint: spec.endpoint,
            actions: spec.actions,
            resource: spec.resource,
            expires_at: spec.ttl_ms.map(|ttl| now.saturating_add(ttl)),
            delegable: spec.delegable,
            max_calls: spec.max_calls,
            parent: None,
            depth: 0,
            used: 0,
        })
    }

    /// Checks that `spec` may be derived from `parent` and builds the result
    /// without inserting it, so a multi-grant spawn can be all-or-nothing.
    pub fn plan_derive(
        &self,
        parent: CapId,
        holder: ProcessId,
        spec: CapSpec,
        now: u64,
        max_depth: u32,
    ) -> Result<Capability, Error> {
        let source = self.validate(parent, now)?;
        let denied = |what: &str| Error::new(ErrorCode::PermissionDenied, format!("delegation {what}"));
        if !source.delegable {
            return Err(denied("from a non-delegable capability"));
        }
        if spec.endpoint != source.endpoint {
            return Err(denied("changes the endpoint"));
        }
        if spec.actions.is_empty() || !spec.actions.is_subset(&source.actions) {
            return Err(denied("widens actions"));
        }
        if !spec.resource.is_within(&source.resource) {
            return Err(denied("widens resource scope"));
        }
        if source.depth >= max_depth {
            return Err(Error::new(ErrorCode::LimitExceeded, "delegation chain too deep"));
        }
        let requested = spec.ttl_ms.map(|ttl| now.saturating_add(ttl));
        let expires_at = match (requested, source.expires_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        Ok(Capability {
            holder,
            endpoint: spec.endpoint,
            actions: spec.actions,
            resource: spec.resource,
            expires_at,
            delegable: spec.delegable,
            max_calls: spec.max_calls,
            parent: Some(parent),
            depth: source.depth + 1,
            used: 0,
        })
    }

    /// Derives one capability on a freshly allocated object. Only the kernel's
    /// checked creation operation may use this; ordinary delegation still
    /// cannot change an endpoint or widen actions. The factory's lifetime and
    /// aggregate budget remain ancestors of the result.
    #[allow(clippy::too_many_arguments)]
    pub fn plan_created(
        &self,
        factory: CapId,
        holder: ProcessId,
        endpoint: EndpointName,
        actions: BTreeSet<ActionName>,
        resource: ResourcePath,
        now: u64,
        max_depth: u32,
    ) -> Result<Capability, Error> {
        let source = self.validate(factory, now)?;
        if !source.delegable {
            return Err(Error::new(
                ErrorCode::PermissionDenied,
                "endpoint creation authority is not delegable",
            ));
        }
        if source.depth >= max_depth {
            return Err(Error::new(ErrorCode::LimitExceeded, "delegation chain too deep"));
        }
        Ok(Capability {
            holder,
            endpoint,
            actions,
            resource,
            expires_at: source.expires_at,
            delegable: true,
            max_calls: None,
            parent: Some(factory),
            depth: source.depth + 1,
            used: 0,
        })
    }

    /// Inserts a capability produced by [`Self::plan_derive`].
    pub fn commit_derive(&mut self, planned: Capability) -> CapId {
        self.insert(planned)
    }

    /// Endpoint a capability names, valid or not. For diagnostics and binding only.
    pub fn endpoint_of(&self, id: CapId) -> Option<&EndpointName> {
        self.caps.get(&id).map(|cap| &cap.endpoint)
    }

    /// Returns the capability if it and its whole ancestry are still good.
    pub fn validate(&self, id: CapId, now: u64) -> Result<&Capability, Error> {
        let cap = self.caps.get(&id).ok_or_else(|| revoked("capability no longer exists"))?;
        let mut cursor = Some(cap);
        while let Some(link) = cursor {
            if link.expires_at.is_some_and(|at| now >= at) {
                return Err(revoked("capability expired"));
            }
            cursor = match link.parent {
                None => None,
                Some(parent) => {
                    Some(self.caps.get(&parent).ok_or_else(|| revoked("delegating capability is gone"))?)
                }
            };
        }
        Ok(cap)
    }

    /// Validates and checks that `action` on `resource` is covered.
    pub fn authorize(
        &self,
        id: CapId,
        endpoint: &str,
        action: &ActionName,
        resource: &ResourcePath,
        now: u64,
    ) -> Result<&Capability, Error> {
        let cap = self.validate(id, now)?;
        let denied = |what: &str| Error::new(ErrorCode::PermissionDenied, what.to_owned());
        if cap.endpoint.as_str() != endpoint {
            return Err(denied("capability is for another endpoint"));
        }
        if !cap.actions.contains(action) {
            return Err(denied("action not granted"));
        }
        if !resource.is_within(&cap.resource) {
            return Err(denied("resource outside granted scope"));
        }
        Ok(cap)
    }

    fn chain(&self, id: CapId) -> Vec<CapId> {
        let mut ids = Vec::new();
        let mut cursor = Some(id);
        while let Some(current) = cursor {
            ids.push(current);
            cursor = self.caps.get(&current).and_then(|cap| cap.parent);
        }
        ids
    }

    /// True when the capability and every ancestor have budget left.
    pub fn has_budget(&self, id: CapId) -> bool {
        self.chain(id)
            .iter()
            .all(|link| self.caps.get(link).is_some_and(|cap| cap.max_calls.is_none_or(|max| cap.used < max)))
    }

    /// Charges one call to the capability and every ancestor.
    pub fn charge(&mut self, id: CapId) -> Result<(), Error> {
        if !self.has_budget(id) {
            return Err(Error::new(ErrorCode::BudgetExhausted, "call budget spent"));
        }
        for link in self.chain(id) {
            if let Some(cap) = self.caps.get_mut(&link) {
                cap.used += 1;
            }
        }
        Ok(())
    }

    fn descends_from(&self, id: CapId, ancestor: CapId) -> bool {
        self.chain(id).into_iter().skip(1).any(|link| link == ancestor)
    }

    pub fn belongs_to(&self, id: CapId, ancestor: CapId) -> bool {
        id == ancestor || self.descends_from(id, ancestor)
    }

    /// Drops one capability. Its descendants become invalid.
    pub fn remove(&mut self, id: CapId) {
        self.caps.remove(&id);
    }

    /// Drops everything `holder` held. Descendants held elsewhere become invalid.
    pub fn remove_held_by(&mut self, holder: ProcessId) {
        self.caps.retain(|_, cap| cap.holder != holder);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Error>;

    fn spec(actions: &[&str], resource: &[&str]) -> Result<CapSpec, Error> {
        Ok(CapSpec {
            endpoint: EndpointName::new("fs")?,
            actions: actions.iter().map(|a| ActionName::new(*a)).collect::<Result<_, _>>()?,
            resource: ResourcePath::new(resource.iter().copied())?,
            ttl_ms: None,
            delegable: true,
            max_calls: None,
        })
    }

    fn derive(table: &mut CapTable, parent: CapId, to: u64, spec: CapSpec, now: u64) -> Result<CapId, Error> {
        let planned = table.plan_derive(parent, ProcessId(to), spec, now, 4)?;
        Ok(table.commit_derive(planned))
    }

    #[test]
    fn widening_any_dimension_is_denied() -> TestResult {
        let mut table = CapTable::default();
        let root = table.mint_root(ProcessId(1), spec(&["read"], &["work"])?, 0);

        let wider_action = derive(&mut table, root, 2, spec(&["read", "write"], &["work"])?, 0);
        assert_eq!(wider_action.map_err(|e| e.code), Err(ErrorCode::PermissionDenied));
        let wider_scope = derive(&mut table, root, 2, spec(&["read"], &[])?, 0);
        assert_eq!(wider_scope.map_err(|e| e.code), Err(ErrorCode::PermissionDenied));
        let sibling_scope = derive(&mut table, root, 2, spec(&["read"], &["workshop"])?, 0);
        assert_eq!(sibling_scope.map_err(|e| e.code), Err(ErrorCode::PermissionDenied));
        let mut other = spec(&["read"], &["work"])?;
        other.endpoint = EndpointName::new("shell")?;
        assert_eq!(
            derive(&mut table, root, 2, other, 0).map_err(|e| e.code),
            Err(ErrorCode::PermissionDenied)
        );

        assert!(derive(&mut table, root, 2, spec(&["read"], &["work", "sub"])?, 0).is_ok());
        Ok(())
    }

    #[test]
    fn non_delegable_capabilities_cannot_be_delegated() -> TestResult {
        let mut table = CapTable::default();
        let root = table.mint_root(ProcessId(1), spec(&["read"], &[])?, 0);
        let mut leaf_spec = spec(&["read"], &[])?;
        leaf_spec.delegable = false;
        let leaf = derive(&mut table, root, 2, leaf_spec, 0)?;
        let again = derive(&mut table, leaf, 3, spec(&["read"], &[])?, 0);
        assert_eq!(again.map_err(|e| e.code), Err(ErrorCode::PermissionDenied));
        Ok(())
    }

    #[test]
    fn expiry_is_clamped_to_the_parent_and_enforced() -> TestResult {
        let mut table = CapTable::default();
        let mut root_spec = spec(&["read"], &[])?;
        root_spec.ttl_ms = Some(100);
        let root = table.mint_root(ProcessId(1), root_spec, 0);
        let mut long = spec(&["read"], &[])?;
        long.ttl_ms = Some(10_000);
        let child = derive(&mut table, root, 2, long, 10)?;
        assert!(table.validate(child, 99).is_ok());
        assert_eq!(table.validate(child, 100).map(|_| ()).map_err(|e| e.code), Err(ErrorCode::Revoked));
        Ok(())
    }

    #[test]
    fn removing_an_ancestor_invalidates_descendants() -> TestResult {
        let mut table = CapTable::default();
        let root = table.mint_root(ProcessId(1), spec(&["read"], &[])?, 0);
        let child = derive(&mut table, root, 2, spec(&["read"], &[])?, 0)?;
        table.remove_held_by(ProcessId(1));
        assert_eq!(table.validate(child, 0).map(|_| ()).map_err(|e| e.code), Err(ErrorCode::Revoked));
        Ok(())
    }

    #[test]
    fn delegated_budgets_are_shared_not_multiplied() -> TestResult {
        let mut table = CapTable::default();
        let mut budgeted = spec(&["read"], &[])?;
        budgeted.max_calls = Some(3);
        let root = table.mint_root(ProcessId(1), budgeted, 0);
        let mut generous = spec(&["read"], &[])?;
        generous.max_calls = Some(100);
        let a = derive(&mut table, root, 2, generous.clone(), 0)?;
        let b = derive(&mut table, root, 3, generous, 0)?;

        table.charge(a)?;
        table.charge(b)?;
        table.charge(a)?;
        assert_eq!(table.charge(b).map_err(|e| e.code), Err(ErrorCode::BudgetExhausted));
        assert_eq!(table.charge(root).map_err(|e| e.code), Err(ErrorCode::BudgetExhausted));
        Ok(())
    }

    #[test]
    fn delegation_depth_is_bounded() -> TestResult {
        let mut table = CapTable::default();
        let mut current = table.mint_root(ProcessId(1), spec(&["read"], &[])?, 0);
        for holder in 2..6 {
            current = derive(&mut table, current, holder, spec(&["read"], &[])?, 0)?;
        }
        let too_deep = derive(&mut table, current, 9, spec(&["read"], &[])?, 0);
        assert_eq!(too_deep.map_err(|e| e.code), Err(ErrorCode::LimitExceeded));
        Ok(())
    }
}
