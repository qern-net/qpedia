//! Observability access model: the pure effective-view + Grafana-org-role
//! resolver. See the OTel LGTM observability design §9/§10/§11.
//!
//! The user requested four observability views — `superadmin`, `admin`,
//! `owner`, `member` — but the real auth model (`auth.rs`) has no
//! `superadmin` role. We map the four requested views onto the real model:
//!
//! | Effective view | Real-model source                                   |
//! |----------------|-----------------------------------------------------|
//! | `superadmin`   | email in `QPEDIA_ADMIN_EMAILS` allowlist            |
//! | `admin`        | `admin` group (`User::is_admin()`) in an org tenant |
//! | `owner`        | `admin` group in own individual (`u-`) tenant       |
//! | `member`       | default (`member` group)                            |
//!
//! Because both the `owner` and `admin` membership roles collapse to the
//! `admin` group in the session, `owner` is distinguished from `admin` by
//! tenant kind: an `admin`-group user in their own individual (`u-`) tenant
//! is `owner`; an `admin`-group user in an org tenant is `admin`.
//!
//! The resolver is split into a **pure core** ([`resolve`]) that takes three
//! booleans (so it is trivially unit/property testable with no env or I/O),
//! and thin wrappers ([`resolve_user`], [`grafana_proxy_headers`]) that read
//! the real [`User`] shape — including the `QPEDIA_ADMIN_EMAILS` allowlist via
//! the existing [`augment_admin_by_email`] logic in `auth.rs`.
//!
//! The whole module is total and deterministic: every function returns for
//! every input, never panics, and the same input always yields the same
//! output. The privilege ordering `superadmin ⊃ admin ⊃ owner ⊃ member` is
//! preserved (Grafana role monotonically non-increasing, permitted-view set a
//! superset for each higher role, tenant scope monotonically non-widening) —
//! Property 10.

use crate::auth::{augment_admin_by_email, User};
use qpedia_core::tenant::Tenant;

/// Prefix that marks an individual (single-user) tenant. An `admin`-group
/// user inside a `u-` tenant is an `owner`; inside any other tenant they are
/// an `admin`.
pub const INDIVIDUAL_TENANT_PREFIX: &str = "u-";

/// The single effective observability view assigned to an authenticated user.
///
/// Ordered by privilege, highest first: `superadmin ⊃ admin ⊃ owner ⊃ member`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EffectiveView {
    /// Platform operator (email in the `QPEDIA_ADMIN_EMAILS` allowlist).
    Superadmin,
    /// `admin` group within an organization tenant.
    Admin,
    /// `admin` group within their own individual (`u-`) tenant.
    Owner,
    /// Default `member` group.
    Member,
}

/// The Grafana organization role injected by the auth proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GrafanaRole {
    Admin,
    Editor,
    Viewer,
}

/// One dashboard in the Observability Views Catalog (§11). The variant order
/// is the canonical catalog order and is also the order permitted-view sets
/// are returned in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CatalogView {
    ServiceOverview,
    LogsExplorer,
    TraceExplorer,
    DbDatastorePerformance,
    IngestionJobQueue,
    DependencyHealth,
    AnomaliesAlerts,
}

/// The data scope at which a view is presented. Narrows monotonically as
/// privilege decreases: `AllTenant ⊇ OrgTenant ⊇ OwnWorkspace ⊇ OwnVisibility`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TenantScope {
    /// No tenant filter — every tenant (superadmin).
    AllTenant,
    /// Pinned to the caller's organization tenant (admin).
    OrgTenant,
    /// Pinned to the caller's own individual (`u-`) workspace (owner).
    OwnWorkspace,
    /// Read-only, scoped to what the caller can already see (member).
    OwnVisibility,
}

/// The complete resolution for an authenticated user: the effective view, the
/// mapped Grafana org role, the permitted catalog-view set, and the tenant
/// scope. Pure data; cheap to compute and copy-friendly except the view list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewResolution {
    pub view: EffectiveView,
    pub grafana_role: GrafanaRole,
    pub permitted_views: Vec<CatalogView>,
    pub tenant_scope: TenantScope,
}

impl EffectiveView {
    /// Privilege rank, higher = more privileged. Lets callers (and the
    /// property test) assert the ordering `superadmin > admin > owner > member`.
    pub fn privilege_rank(self) -> u8 {
        match self {
            EffectiveView::Superadmin => 3,
            EffectiveView::Admin => 2,
            EffectiveView::Owner => 1,
            EffectiveView::Member => 0,
        }
    }

    /// Stable lowercase label (`"superadmin"`, `"admin"`, `"owner"`, `"member"`).
    pub fn as_str(self) -> &'static str {
        match self {
            EffectiveView::Superadmin => "superadmin",
            EffectiveView::Admin => "admin",
            EffectiveView::Owner => "owner",
            EffectiveView::Member => "member",
        }
    }

    /// The Grafana org role this view maps to: superadmin/admin → Admin,
    /// owner → Editor, member → Viewer (Req 10.4).
    pub fn grafana_role(self) -> GrafanaRole {
        match self {
            EffectiveView::Superadmin | EffectiveView::Admin => GrafanaRole::Admin,
            EffectiveView::Owner => GrafanaRole::Editor,
            EffectiveView::Member => GrafanaRole::Viewer,
        }
    }

    /// The data scope this view is presented at (Req 10.2).
    pub fn tenant_scope(self) -> TenantScope {
        match self {
            EffectiveView::Superadmin => TenantScope::AllTenant,
            EffectiveView::Admin => TenantScope::OrgTenant,
            EffectiveView::Owner => TenantScope::OwnWorkspace,
            EffectiveView::Member => TenantScope::OwnVisibility,
        }
    }

    /// The set of catalog views this effective view may see, in canonical
    /// catalog order (§11 Role → View matrix). The sets are monotone along
    /// the privilege ordering: each higher view's set is a superset of every
    /// lower view's set.
    pub fn permitted_views(self) -> Vec<CatalogView> {
        use CatalogView::*;
        match self {
            // superadmin and admin see all seven (scope differs, not set).
            EffectiveView::Superadmin | EffectiveView::Admin => vec![
                ServiceOverview,
                LogsExplorer,
                TraceExplorer,
                DbDatastorePerformance,
                IngestionJobQueue,
                DependencyHealth,
                AnomaliesAlerts,
            ],
            // owner: no trace/DB internals, no fleet anomalies.
            EffectiveView::Owner => vec![
                ServiceOverview,
                LogsExplorer,
                IngestionJobQueue,
                DependencyHealth,
            ],
            // member: read-only Service Overview + Ingestion status only.
            EffectiveView::Member => vec![ServiceOverview, IngestionJobQueue],
        }
    }

    /// Whether this effective view is permitted to see `view` (§11 matrix).
    pub fn permits(self, view: CatalogView) -> bool {
        self.permitted_views().contains(&view)
    }

    /// Full resolution (role + permitted views + scope) for this view.
    pub fn resolution(self) -> ViewResolution {
        ViewResolution {
            view: self,
            grafana_role: self.grafana_role(),
            permitted_views: self.permitted_views(),
            tenant_scope: self.tenant_scope(),
        }
    }
}

impl GrafanaRole {
    /// The exact value injected into the `X-WEBAUTH-ROLE` header. Grafana org
    /// roles are capitalized (`Admin`/`Editor`/`Viewer`).
    pub fn as_str(self) -> &'static str {
        match self {
            GrafanaRole::Admin => "Admin",
            GrafanaRole::Editor => "Editor",
            GrafanaRole::Viewer => "Viewer",
        }
    }

    /// Privilege rank, higher = more capable. `Admin > Editor > Viewer`. Lets
    /// callers assert the role is monotonically non-increasing along the
    /// effective-view privilege ordering.
    pub fn privilege_rank(self) -> u8 {
        match self {
            GrafanaRole::Admin => 2,
            GrafanaRole::Editor => 1,
            GrafanaRole::Viewer => 0,
        }
    }
}

impl CatalogView {
    /// URL-safe slug used to select the dashboard via the Grafana proxy.
    pub fn slug(self) -> &'static str {
        match self {
            CatalogView::ServiceOverview => "service-overview",
            CatalogView::LogsExplorer => "logs-explorer",
            CatalogView::TraceExplorer => "trace-explorer",
            CatalogView::DbDatastorePerformance => "db-datastore-performance",
            CatalogView::IngestionJobQueue => "ingestion-job-queue",
            CatalogView::DependencyHealth => "dependency-health",
            CatalogView::AnomaliesAlerts => "anomalies-alerts",
        }
    }

    /// Human-readable catalog title.
    pub fn title(self) -> &'static str {
        match self {
            CatalogView::ServiceOverview => "Service Overview",
            CatalogView::LogsExplorer => "Logs Explorer",
            CatalogView::TraceExplorer => "Trace Explorer",
            CatalogView::DbDatastorePerformance => "DB / Datastore Performance",
            CatalogView::IngestionJobQueue => "Ingestion & Job Queue Performance",
            CatalogView::DependencyHealth => "Dependency Health",
            CatalogView::AnomaliesAlerts => "Anomalies & Alerts",
        }
    }
}

/// Whether a tenant is an individual (single-user) tenant, identified by the
/// `u-` prefix. Org tenants are everything else.
pub fn is_individual_tenant(tenant: &Tenant) -> bool {
    tenant.as_str().starts_with(INDIVIDUAL_TENANT_PREFIX)
}

/// **Pure core** of the resolver. Maps the three primitive facts about a user
/// to an [`EffectiveView`] (Req 10.1):
///
/// - `is_superadmin`: email present in the `QPEDIA_ADMIN_EMAILS` allowlist.
/// - `is_admin_group`: user is in the `admin` group (`User::is_admin()`).
/// - `is_individual_tenant`: user's tenant is an individual (`u-`) tenant.
///
/// Precedence (highest first): superadmin → admin (group + org tenant) →
/// owner (group + individual tenant) → member. Total and deterministic;
/// never panics.
pub fn resolve_view(
    is_superadmin: bool,
    is_admin_group: bool,
    is_individual_tenant: bool,
) -> EffectiveView {
    if is_superadmin {
        EffectiveView::Superadmin
    } else if is_admin_group {
        if is_individual_tenant {
            EffectiveView::Owner
        } else {
            EffectiveView::Admin
        }
    } else {
        EffectiveView::Member
    }
}

/// Pure resolver returning the full [`ViewResolution`] from the three
/// primitive facts. Convenience over [`resolve_view`] + [`EffectiveView::resolution`].
pub fn resolve(
    is_superadmin: bool,
    is_admin_group: bool,
    is_individual_tenant: bool,
) -> ViewResolution {
    resolve_view(is_superadmin, is_admin_group, is_individual_tenant).resolution()
}

/// Whether the user qualifies as a `superadmin`, i.e. their email is in the
/// `QPEDIA_ADMIN_EMAILS` allowlist. Reuses the existing [`augment_admin_by_email`]
/// logic: if running the allowlist over an *empty* group set yields the
/// `admin` group, the email matched the allowlist.
pub fn is_superadmin_user(user: &User) -> bool {
    augment_admin_by_email(user.email.as_deref(), Vec::new())
        .iter()
        .any(|g| g == "admin")
}

/// Resolve the full [`ViewResolution`] for a real [`User`], reading the
/// `QPEDIA_ADMIN_EMAILS` allowlist for the superadmin determination and the
/// tenant kind for the owner/admin distinction. Total; never panics.
pub fn resolve_user(user: &User) -> ViewResolution {
    resolve(
        is_superadmin_user(user),
        user.is_admin(),
        is_individual_tenant(&user.tenant),
    )
}

/// The user identity injected into the `X-WEBAUTH-USER` header: the verified
/// email when present, otherwise the stable user id.
fn proxy_identity(user: &User) -> String {
    user.email.clone().unwrap_or_else(|| user.id.clone())
}

/// Build the Grafana auth-proxy headers for an authenticated user: the
/// `(X-WEBAUTH-USER value, mapped Grafana org role)` pair. Pure given the env
/// allowlist; never panics. The proxy strips any client-supplied
/// `X-WEBAUTH-*` headers before injecting these.
pub fn grafana_proxy_headers(user: &User) -> (String, GrafanaRole) {
    let role = resolve_user(user).grafana_role;
    (proxy_identity(user), role)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use qpedia_core::tenant::Tenant;

    fn user(email: Option<&str>, groups: &[&str], tenant: &str) -> User {
        User {
            id: "uid-1".into(),
            email: email.map(|s| s.to_string()),
            name: None,
            groups: groups.iter().map(|s| s.to_string()).collect(),
            tenant: Tenant::new(tenant),
        }
    }

    #[test]
    fn pure_core_precedence() {
        // superadmin wins regardless of group/tenant.
        assert_eq!(resolve_view(true, true, true), EffectiveView::Superadmin);
        assert_eq!(resolve_view(true, false, false), EffectiveView::Superadmin);
        // admin group + org tenant => admin.
        assert_eq!(resolve_view(false, true, false), EffectiveView::Admin);
        // admin group + individual tenant => owner.
        assert_eq!(resolve_view(false, true, true), EffectiveView::Owner);
        // no admin group => member.
        assert_eq!(resolve_view(false, false, false), EffectiveView::Member);
        assert_eq!(resolve_view(false, false, true), EffectiveView::Member);
    }

    #[test]
    fn grafana_role_mapping() {
        assert_eq!(EffectiveView::Superadmin.grafana_role(), GrafanaRole::Admin);
        assert_eq!(EffectiveView::Admin.grafana_role(), GrafanaRole::Admin);
        assert_eq!(EffectiveView::Owner.grafana_role(), GrafanaRole::Editor);
        assert_eq!(EffectiveView::Member.grafana_role(), GrafanaRole::Viewer);
    }

    #[test]
    fn tenant_scope_mapping() {
        assert_eq!(EffectiveView::Superadmin.tenant_scope(), TenantScope::AllTenant);
        assert_eq!(EffectiveView::Admin.tenant_scope(), TenantScope::OrgTenant);
        assert_eq!(EffectiveView::Owner.tenant_scope(), TenantScope::OwnWorkspace);
        assert_eq!(EffectiveView::Member.tenant_scope(), TenantScope::OwnVisibility);
    }

    #[test]
    fn permitted_view_sets_are_monotone() {
        let views = [
            EffectiveView::Member,
            EffectiveView::Owner,
            EffectiveView::Admin,
            EffectiveView::Superadmin,
        ];
        // Each higher view's permitted set is a superset of every lower one.
        for (i, lower) in views.iter().enumerate() {
            for higher in &views[i..] {
                for v in lower.permitted_views() {
                    assert!(
                        higher.permits(v),
                        "{:?} should permit everything {:?} permits ({:?})",
                        higher,
                        lower,
                        v
                    );
                }
            }
        }
    }

    #[test]
    fn member_and_superadmin_view_counts() {
        assert_eq!(EffectiveView::Superadmin.permitted_views().len(), 7);
        assert_eq!(EffectiveView::Admin.permitted_views().len(), 7);
        assert_eq!(EffectiveView::Owner.permitted_views().len(), 4);
        assert_eq!(EffectiveView::Member.permitted_views().len(), 2);
    }

    #[test]
    fn individual_tenant_detection() {
        assert!(is_individual_tenant(&Tenant::new("u-abc123")));
        assert!(!is_individual_tenant(&Tenant::new("acme-corp")));
        assert!(!is_individual_tenant(&Tenant::new("default")));
    }

    #[test]
    fn superadmin_from_allowlist() {
        // Use a unique env var snapshot; QPEDIA_ADMIN_EMAILS drives superadmin.
        std::env::set_var("QPEDIA_ADMIN_EMAILS", "boss@corp.com");
        let boss = user(Some("boss@corp.com"), &["member"], "acme");
        assert!(is_superadmin_user(&boss));
        assert_eq!(resolve_user(&boss).view, EffectiveView::Superadmin);

        let peon = user(Some("peon@corp.com"), &["member"], "acme");
        assert!(!is_superadmin_user(&peon));
        std::env::remove_var("QPEDIA_ADMIN_EMAILS");
    }

    #[test]
    fn resolve_user_owner_vs_admin_by_tenant() {
        std::env::remove_var("QPEDIA_ADMIN_EMAILS");
        let org_admin = user(Some("a@org.com"), &["admin"], "acme");
        assert_eq!(resolve_user(&org_admin).view, EffectiveView::Admin);

        let individual_owner = user(Some("a@org.com"), &["admin"], "u-a");
        assert_eq!(resolve_user(&individual_owner).view, EffectiveView::Owner);
    }

    #[test]
    fn grafana_proxy_headers_identity_and_role() {
        std::env::remove_var("QPEDIA_ADMIN_EMAILS");
        let u = user(Some("owner@x.com"), &["admin"], "u-x");
        let (ident, role) = grafana_proxy_headers(&u);
        assert_eq!(ident, "owner@x.com");
        assert_eq!(role, GrafanaRole::Editor);
        assert_eq!(role.as_str(), "Editor");

        // Falls back to id when no email.
        let no_email = user(None, &["member"], "acme");
        let (ident, _) = grafana_proxy_headers(&no_email);
        assert_eq!(ident, "uid-1");
    }

    #[test]
    fn grafana_role_monotonic_along_privilege() {
        let order = [
            EffectiveView::Superadmin,
            EffectiveView::Admin,
            EffectiveView::Owner,
            EffectiveView::Member,
        ];
        for win in order.windows(2) {
            assert!(win[0].privilege_rank() > win[1].privilege_rank());
            assert!(
                win[0].grafana_role().privilege_rank() >= win[1].grafana_role().privilege_rank()
            );
        }
    }

    // -----------------------------------------------------------------
    // Property 10: QPEDIA-role → Grafana-role (and effective-view) mapping is
    // total, deterministic, and order-preserving
    // Feature: otel-lgtm-observability, Property 10
    // Validates: Requirements 10.1, 10.2, 10.3
    // -----------------------------------------------------------------

    /// The four effective views in canonical privilege order, highest first:
    /// `superadmin ⊃ admin ⊃ owner ⊃ member`. The order-preservation property
    /// is asserted along this ordering.
    const PRIVILEGE_ORDER: [EffectiveView; 4] = [
        EffectiveView::Superadmin,
        EffectiveView::Admin,
        EffectiveView::Owner,
        EffectiveView::Member,
    ];

    /// Width rank of a tenant scope, higher = wider (covers more data).
    /// `AllTenant ⊇ OrgTenant ⊇ OwnWorkspace ⊇ OwnVisibility`. Used to assert
    /// the scope never widens as privilege decreases.
    fn scope_width(scope: TenantScope) -> u8 {
        match scope {
            TenantScope::AllTenant => 3,
            TenantScope::OrgTenant => 2,
            TenantScope::OwnWorkspace => 1,
            TenantScope::OwnVisibility => 0,
        }
    }

    proptest! {
        // Totality + determinism + exactly-one-view over arbitrary bool triples.
        #[test]
        fn prop10_resolution_is_total_deterministic_and_single(
            is_superadmin in any::<bool>(),
            is_admin_group in any::<bool>(),
            is_individual_tenant in any::<bool>(),
        ) {
            // Totality: the pure resolver returns for every input (never panics).
            let view = resolve_view(is_superadmin, is_admin_group, is_individual_tenant);

            // Determinism: same input → same output across repeated calls.
            let view2 = resolve_view(is_superadmin, is_admin_group, is_individual_tenant);
            prop_assert_eq!(view, view2);
            let res1 = resolve(is_superadmin, is_admin_group, is_individual_tenant);
            let res2 = resolve(is_superadmin, is_admin_group, is_individual_tenant);
            prop_assert_eq!(&res1, &res2);

            // Exactly-one-view: the resolved view matches exactly one of the four
            // canonical variants.
            let matches = PRIVILEGE_ORDER.iter().filter(|v| **v == view).count();
            prop_assert_eq!(matches, 1, "view {:?} must match exactly one variant", view);

            // The full resolution is the view's own resolution (the two pure
            // entry points agree).
            prop_assert_eq!(&res1, &view.resolution());
            prop_assert_eq!(res1.view, view);
            prop_assert_eq!(res1.grafana_role, view.grafana_role());
            prop_assert_eq!(res1.tenant_scope, view.tenant_scope());
            prop_assert_eq!(&res1.permitted_views, &view.permitted_views());
        }
    }

    proptest! {
        // Order-preservation along the privilege ordering. Pick two positions in
        // the canonical ordering; the higher-or-equal-privilege view must have a
        // Grafana role that is monotonically non-increasing, a permitted-view set
        // that is a superset, and a tenant scope that is no narrower (i.e. scope
        // is monotonically non-widening as privilege decreases).
        #[test]
        fn prop10_mapping_is_order_preserving(
            a in 0usize..PRIVILEGE_ORDER.len(),
            b in 0usize..PRIVILEGE_ORDER.len(),
        ) {
            // i = higher (or equal) privilege position, j = lower (or equal).
            let (i, j) = (a.min(b), a.max(b));
            let higher = PRIVILEGE_ORDER[i];
            let lower = PRIVILEGE_ORDER[j];

            // Privilege ranks reflect the canonical ordering.
            prop_assert!(higher.privilege_rank() >= lower.privilege_rank());

            // Grafana role privilege is monotonically non-increasing.
            prop_assert!(
                higher.grafana_role().privilege_rank() >= lower.grafana_role().privilege_rank(),
                "grafana role widened: {:?} -> {:?}",
                higher, lower
            );

            // Permitted-view set is a superset for the higher view.
            for v in lower.permitted_views() {
                prop_assert!(
                    higher.permitted_views().contains(&v),
                    "{:?} must permit {:?} permitted by {:?}",
                    higher, v, lower
                );
            }

            // Tenant scope is monotonically non-widening as privilege decreases.
            prop_assert!(
                scope_width(higher.tenant_scope()) >= scope_width(lower.tenant_scope()),
                "tenant scope widened from {:?} to {:?}",
                higher.tenant_scope(), lower.tenant_scope()
            );
        }
    }
}
