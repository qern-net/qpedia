---
title: "Token refresh and expiration handling in auth service"
kind: concept
tags: ["auth", "tokens", "oauth"]
---

# Token refresh and expiration handling

The auth service issues short-lived access tokens and longer-lived refresh
tokens. When an access token **expires**, the client silently exchanges
its refresh token for a new access token.

## Expiration handling

- Access tokens expire after a short window.
- An expired access token returns a 401; the client refreshes and retries.
- Refresh tokens are rotated on use and revoked on logout.

This is how the system handles expired tokens without forcing the user to
sign in again.
