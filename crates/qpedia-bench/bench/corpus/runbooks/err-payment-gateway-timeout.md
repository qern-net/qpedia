---
title: "Resolve ERR_PAYMENT_GATEWAY_TIMEOUT"
kind: runbook
tags: ["payments", "error-code", "gateway"]
---

# Runbook: `ERR_PAYMENT_GATEWAY_TIMEOUT` (payment-svc)

The payment gateway did not respond within the configured timeout window.

## Diagnosis

1. Check the gateway latency dashboard for the affected region.
2. Confirm the upstream gateway is reachable.
3. Inspect the connection pool for exhaustion.

If you see `ERR_PAYMENT_GATEWAY_REJECTED` instead, see that runbook — the
request reached the gateway but was declined.

## Resolution

Raise the timeout only as a last resort; prefer fixing the upstream
latency. Restart the gateway connector if the pool is wedged.
