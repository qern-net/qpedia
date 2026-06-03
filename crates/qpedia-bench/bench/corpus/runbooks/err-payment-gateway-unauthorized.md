---
title: "Resolve ERR_PAYMENT_GATEWAY_UNAUTHORIZED"
kind: runbook
tags: ["payments", "error-code", "gateway", "auth"]
---

# Runbook: `ERR_PAYMENT_GATEWAY_UNAUTHORIZED` (payment-svc)

The payment gateway rejected our credentials.

## Diagnosis

1. Check whether the gateway API key has expired or been rotated.
2. Confirm the credential in the secret store matches the gateway config.
3. Look for a recent deploy that changed the credential mount.

## Resolution

Rotate and re-deploy the gateway credential. This is an authentication
failure, distinct from `ERR_PAYMENT_GATEWAY_REJECTED` (a declined
transaction) and `ERR_PAYMENT_GATEWAY_TIMEOUT` (no response).
