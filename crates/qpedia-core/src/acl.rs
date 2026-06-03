use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Access control = set of group identifiers (from the IdP) allowed to read.
/// See DESIGN.md §12. Empty ACL means "admins only".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Acl(pub BTreeSet<String>);

impl Acl {
    pub fn from_iter<I: IntoIterator<Item = String>>(iter: I) -> Self {
        Acl(iter.into_iter().collect())
    }

    pub fn union(&self, other: &Acl) -> Acl {
        Acl(self.0.union(&other.0).cloned().collect())
    }

    pub fn intersects(&self, user_groups: &[String]) -> bool {
        user_groups.iter().any(|g| self.0.contains(g))
    }
}
