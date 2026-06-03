---
title: "Rollback runbook for v3.2 deployment"
kind: runbook
tags: ["deployment", "rollback", "v3.2"]
---

# Roll back the v3.2 deployment

This runbook **rolls back** (reverts) the v3.2 deployment to the previous
known-good release.

## Steps

1. Identify the last good release before v3.2.
2. Trigger the rollback pipeline targeting that release.
3. Verify the v3.2 changes are no longer live.
4. Confirm health checks pass on the reverted version.

Use this when v3.2 introduced a regression. To deploy v3.2 forward
instead, see the rollout runbook.
