---
title: "Enable payment_v2_enforce in production"
kind: runbook
tags: ["payments", "feature-flag", "production"]
---

# Enable the `payment_v2_enforce` feature flag in production

This runbook describes how to **enable** (turn on) the
`payment_v2_enforce` feature flag in the production environment for the
payment service.

## Steps

1. Confirm the change ticket is approved.
2. In the flag console, set `payment_v2_enforce = true` for the
   `production` environment.
3. Watch the payment error dashboard for five minutes.
4. If error rates rise, see the disable runbook to roll the flag back.

Enabling this flag routes all traffic through the v2 enforcement path.
