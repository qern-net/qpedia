---
title: "Disable payment_v2_enforce in production"
kind: runbook
tags: ["payments", "feature-flag", "production"]
---

# Disable the `payment_v2_enforce` feature flag in production

This runbook describes how to **disable** (turn off) the
`payment_v2_enforce` feature flag in the production environment for the
payment service.

## Steps

1. Open the flag console.
2. Set `payment_v2_enforce = false` for the `production` environment.
3. Confirm traffic falls back to the legacy enforcement path.
4. Watch the payment error dashboard for five minutes.

Disabling this flag stops routing traffic through the v2 enforcement path.
