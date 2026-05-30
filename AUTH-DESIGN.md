# Auth Design — Self-Serve Identity + Org/Team Workspaces

How Qpedia handles personal sign-up, team/org claiming, and SSO
enforcement — and why. The guiding rule mirrors what Linear, Notion,
Vercel, and WorkOS-backed products do:

> **Identity is separate from org membership.** Everyone signs up as an
> individual. Joining or running an org is a *separate, explicitly
> authorized* action — and any *domain-based* privilege (auto-join, SSO
> enforcement) is gated on **proving you control the domain**.

Status: design. The individual-workspace half is implemented; the
org/SSO half is staged below.

---

## 0. The one security rule that matters

The flow originally sketched — *"flip a Team switch, point it at an SSO
provider, the first person to log in via SSO becomes admin, and then the
whole org is forced onto that SSO"* — has a **domain-takeover hole**:

> Nothing stops `mallory@gmail.com` (or a single phished `junior@acme.com`)
> from creating an "acme.com org," wiring it to an IdP **they** control,
> and forcing every real acme.com user to authenticate against the
> attacker's IdP — harvesting credentials or silently joining their
> accounts to the attacker's org.

The fix is non-negotiable: **a domain must be *verified* before any
domain-scoped privilege** (auto-join, SSO enforcement). What varies is
*how* you verify. Several methods, strongest/easiest first:

### Verification methods

1. **IdP-admin auto-verification (preferred for Google / Microsoft).**
   The claimant signs in via the provider and we confirm — from the
   provider's *authoritative* API — that they are an **admin of that
   directory**, then read the directory's *already-verified* domains.
   - **Microsoft Entra / O365:** the OAuth token's `wids` claim lists
     directory-role template IDs; Global Administrator is the fixed GUID
     `62e90394-69f5-4237-9190-012177145e10`. Microsoft Graph
     `/organization` returns `verifiedDomains`. Admin + verified-domain
     in one round-trip.
   - **Google Workspace:** request the `admin.directory.domain.readonly`
     scope; only a Workspace admin can call Directory API `domains.list`
     for the customer. Success ⇒ admin; the returned domains are
     Google-verified.

   **The subtlety that makes this safe (equal-or-better than DNS):** we
   claim only domains the IdP itself reports as **verified**, *gated on
   confirming the user is an admin of that directory*. We piggyback on
   Microsoft/Google having already verified the domain; we just confirm
   admin status. A mere member (not Global Admin) gets nothing. Never
   claim a domain from the user's email address alone.

2. **SSO-config proof (Okta / Ping / generic SAML-OIDC).** Wiring up the
   SSO connection requires admin rights *in the IdP*, so completing the
   config + a successful test login is itself the proof. Folds into 4.3.

3. **DNS TXT (universal fallback).** Place `qpedia-verify=<nonce>` in the
   domain's DNS. For self-hosted / custom domains / smaller IdPs, or any
   time the admin APIs aren't available. Only someone controlling the
   domain's DNS (real IT) can do it.

With *any* of these as the gate, the rest of the sketched flow is fine:
verify domain → configure SSO → test login → that confirms you as the
org admin → enforce SSO for the verified domain.

---

## 1. Model

### Entities

| Entity | Purpose |
|---|---|
| **user** | Personal identity. One per human. Primary **verified** email; one or more linked auth identities (password, Google, GitHub, Microsoft, or an org's SAML/OIDC). |
| **workspace** (= tenant) | A data boundary. `kind = individual \| org`. Individual ones are `u-<uid>`; org ones get a chosen slug. RLS already isolates every workspace. |
| **workspace_member** | `(workspace_id, user_id, role: owner \| admin \| member)`. A user can belong to many workspaces. |
| **workspace_domain** | `(workspace_id, domain, verification_token, verified_at)`. Proof of domain ownership. |
| **workspace_sso** | `(workspace_id, kind: oidc \| saml, config, enforced: bool)`. The org's IdP + whether it's mandatory. |
| **workspace_invite** | `(workspace_id, email, role, token, expires_at)`. Non-domain joins. |

> Today qpedia has `tenants` (= workspaces) and `sessions`, with identity
> implied by the Firebase uid. The new tables are `users`,
> `workspace_members`, `workspace_domains`, `workspace_sso`,
> `workspace_invites`.

### Roles
- **owner** — created the workspace (or the verified-domain claimant).
  Can delete it, manage SSO/domains, transfer ownership.
- **admin** — manage members, connectors, ACLs.
- **member** — use the workspace per ACLs.

Every user is **owner** of their own individual workspace (already
implemented: the login grants `admin` in `u-<uid>`, scoped by RLS).

---

## 2. Flows

1. **Sign up (always individual).** Email+password *or* social (Google /
   GitHub / Microsoft). Firebase mints the identity; qpedia creates the
   user + their individual workspace; they're owner of it. A
   corporate-domain email is **no different** here — still individual.

2. **Create an org.** A signed-in user creates an org workspace, names
   it, picks a slug. They're owner; they're now a member of both their
   individual space and the org. Empty, isolated.

3. **Verify a domain.** Org owner adds `acme.com` → qpedia shows a DNS
   TXT token (`qpedia-verify=<nonce>`). A background/triggered check
   resolves the TXT; on success the domain is `verified`. *No
   domain-scoped feature is available until this passes.*

4. **Configure SSO.** Org owner sets up OIDC/SAML/Google/Microsoft for
   the org (provider metadata / client creds). They **test** it by
   logging in through it; a successful round-trip that returns an email
   in the verified domain confirms the config.

5. **Enforce SSO.** Once a domain is verified *and* SSO tested, the owner
   can flip "require SSO for @acme.com." From then on, any login by an
   `@acme.com` address is **routed to the org IdP**; password / personal-
   social logins for that domain are refused (or auto-redirected to SSO).
   New SSO logins **JIT-provision** as org members.

6. **Invite (any email, no domain needed).** Owner/admin invites
   `bob@gmail.com`; bob accepts via a tokened link and becomes a member,
   still using his personal login. Invites are how you add people outside
   the verified domain.

7. **Account linking.** Same **verified** email across methods = the same
   user. If `alice@acme.com` signed up with a password before acme
   enforced SSO, her next (now-SSO) login links to the existing user — she
   keeps her data. (Firebase "one account per email address" gives us
   this at the IdP layer; qpedia keys the `user` on the verified email.)

---

## 3. Security matrix

`D` = state of the email's domain. Behavior + the control that makes it
safe.

| # | Actor | Domain state | Action | Result | Control |
|---|---|---|---|---|---|
| 1 | anyone | public (gmail) | sign up | individual workspace | uid-keyed tenant; gmail users never share |
| 2 | anyone | corp, unclaimed | sign up | individual workspace | no auto-org; domain alone grants nothing |
| 3 | first corp user | corp, unclaimed | create org + verify domain | becomes org **owner** | DNS TXT proves control |
| 4 | attacker (gmail) | — | create "acme.com" org, try to verify | **verification fails** | can't write acme.com DNS |
| 5 | junior@acme | corp | create "acme" org, try to verify | fails unless they control DNS | DNS TXT = only IT can |
| 6 | 2nd claimant | corp, already verified by org A | try to verify same domain | **denied** | one verified owner per domain; rest via invite/SSO |
| 7 | acme owner | verified | enable SSO + enforce | @acme.com users forced to SSO | enforce only allowed post-verify |
| 8 | acme employee | verified, SSO enforced | tries password login | **refused / redirected to SSO** | enforcement check at login |
| 9 | acme employee | verified, SSO enforced | first SSO login | JIT-provisioned as member | membership auto-created |
| 10 | alice@acme (pre-existing password user) | becomes verified+enforced | next login via SSO | account **links** by verified email; data kept | email-keyed user identity |
| 11 | ex-employee | verified, SSO enforced | login after IdP removal | **denied** | IdP rejects; (SCIM/next-login revokes membership) |
| 12 | acme owner | verified | verification lapses (DNS TXT removed / admin loses role) | SSO enforcement **suspends** | periodic re-check; enforcement requires live verification |
| 13 | user | — | log in with GitHub (email private) | individual (no domain known) | can't domain-match without an email |
| 14 | member of N orgs | — | log in | lands in last-used workspace; can switch | workspace switcher; session carries active workspace |
| 15 | org admin | — | disable enforcement | members may use other methods again | reversible policy flag |
| 16 | attacker | verified domain | replays another user's SSO assertion | rejected | standard OIDC/SAML signature + nonce/audience checks |
| 17 | **Workspace Global Admin** | corp (Entra/Google) | claim domain via IdP-admin login | domain **instantly verified**; claimant = owner | `wids`=Global-Admin / Directory-API success; domains read from the IdP's *verified* list, not the email |
| 18 | **non-admin member** | corp (Entra/Google) | tries IdP-admin verification | **denied** | `wids` lacks the admin role / Directory API call fails for non-admins |

**Invariants to test:**
- No path places a user in a workspace they didn't create, get invited
  to, or SSO-provision into via a **verified** domain.
- No path lets a workspace enforce SSO for a domain it hasn't verified.
- RLS makes every one of the above fail *closed* even if app logic has a
  bug (cross-tenant read/write is impossible regardless).

---

## 4. Recommendation — don't hand-roll the IdP layer

The single most important architectural call, and the answer to "suggest
a better self/org auth": **separate the *federation* layer from the
*policy* layer, and buy the former.**

- **Federation (identity, multi-provider, SAML/OIDC, account linking):**
  use a provider. Two good fits:
  - **Firebase → Google Cloud Identity Platform (GCIP).** GCIP is the
    paid upgrade of the Firebase Auth we already use; it adds
    **multi-tenancy + SAML/OIDC enterprise providers** with the same SDK.
    Lowest-friction since the frontend already speaks Firebase.
  - **WorkOS / Stytch / Auth0.** WorkOS is purpose-built for exactly this
    ("SSO + Directory Sync + Admin Portal") and is what **Linear** uses.
    It gives you the SAML/SCIM/admin-portal surface as an API so you
    never touch raw SAML XML.

- **Policy (workspaces, membership, roles, domain verification, SSO
  enforcement, invites):** build in qpedia. This is the part that's
  *yours* and is small, well-understood code — the tables in §1 plus the
  enforcement checks in §3. RLS already backstops it.

**Why this is "better" than the sketch:**
1. SAML and SCIM are notoriously sharp to implement correctly; a single
   missed signature/audience check is a full auth bypass. Let a federation
   layer own that.
2. Domain verification + per-workspace SSO policy is the genuinely
   product-specific logic — and it's the part the original sketch was
   missing. Building *that* well is where the effort should go.
3. It keeps Qpedia's own surface (the `workspace_*` tables + a dozen
   policy checks) auditable and testable, which §3's matrix needs.

---

## 5. Staged implementation

Each stage ships and is testable on its own.

| Stage | Scope | Notes |
|---|---|---|
| **S0 (done)** | Everyone individual; owner-admin of `u-<uid>`; no env-var domains. | Current state after this commit. |
| **S1 (done)** | `workspace_members` + `workspace_invites`; workspace **switcher** UI; "Create org" → org workspace with the creator as owner; **invites** (email + token). | Org via invite only — no domain magic, zero takeover surface. |
| **S2** | `workspace_domains` + verification. **IdP-admin auto-verification** for Microsoft Entra (`wids` Global-Admin + Graph `verifiedDomains`) and Google Workspace (Directory API `domains.list`) as the primary path; **DNS-TXT** as the fallback for self-hosted / custom domains / smaller IdPs. Verified-domain **auto-join** (optional per org). | The security gate. Per §0, claim only IdP-*verified* domains, gated on confirming admin. |
| **S3** | `workspace_sso` via **GCIP or WorkOS**; test-login; **enforce SSO** for verified domains; JIT provisioning; account linking. | Federation bought, policy built. Implements the full sketch — safely. |
| **S4** | SCIM deprovisioning; audit of all auth events (already have `EventSink`); admin portal. | Enterprise polish. |

**Recommended first build: S1.** It gives genuine team workspaces with
*no* domain/SSO attack surface (invite-only), which is what most users
need, and it lays the `users`/`members` foundation everything else builds
on. S2's domain verification is the prerequisite for the SSO enforcement
in the original ask; S3 then delivers it without the takeover hole.

---

_See `ROADMAP.md` for where these stages sit relative to the rest of the
work, and `OPEN-CORE.md` for which pieces (enterprise SAML/SCIM) belong in
`qpedia-pvt`._
