---
title: "Rollout runbook for v3.2 deployment"
kind: runbook
tags: ["deployment", "rollout", "v3.2"]
---

# Roll out the v3.2 deployment

This runbook **rolls out** (deploys forward) the v3.2 release to
production.

## Steps

1. Confirm v3.2 passed staging verification.
2. Trigger the rollout pipeline targeting v3.2.
3. Progress the canary from 5% to 100% while watching error rates.
4. Confirm health checks pass on v3.2.

Use this to ship v3.2. To revert v3.2 after a regression, see the
rollback runbook.
