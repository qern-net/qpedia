---
title: "Rollback runbook for v3.1 deployment"
kind: runbook
tags: ["deployment", "rollback", "v3.1"]
---

# Roll back the v3.1 deployment

This runbook **rolls back** the v3.1 deployment to the previous
known-good release.

## Steps

1. Identify the last good release before v3.1.
2. Trigger the rollback pipeline targeting that release.
3. Verify the v3.1 changes are no longer live.

This is the v3.1 procedure. For the current release, see the v3.2
rollback runbook.
