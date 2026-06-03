---
title: "OAuth authorization-code flow design"
kind: concept
tags: ["auth", "oauth", "design"]
---

# OAuth authorization-code flow

This document describes how the authorization-code flow is wired through
the auth service: the redirect to the identity provider, the code
exchange, and the issuance of the initial token pair.

## Flow

1. The client redirects to the IdP with a PKCE challenge.
2. The IdP returns an authorization code.
3. The backend exchanges the code for an access + refresh token pair.

Ongoing token lifetime — refresh and expiration — is covered separately in
the token refresh document.
