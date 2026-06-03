---
title: "System token rotation runbook"
kind: runbook
tags: ["security", "tokens", "rotation"]
---

# System token rotation

This runbook covers rotating the long-lived **system** tokens that
services use to authenticate to each other (not end-user auth tokens).

## Steps

1. Mint the new system token in the secret store.
2. Roll it out to consumers with both old and new accepted.
3. Revoke the old token once all consumers report the new one.

This is service-to-service token rotation. End-user token expiration is a
different topic — see the auth token refresh document.
