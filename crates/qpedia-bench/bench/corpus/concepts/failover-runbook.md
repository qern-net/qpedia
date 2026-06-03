---
title: "Regional failover runbook"
kind: runbook
tags: ["reliability", "failover", "on-call"]
---

# What to do when a region goes offline

This is the operational procedure the on-call engineer follows when a
whole region becomes unreachable.

## Steps

1. Confirm the region is actually down (not a monitoring blip).
2. Trigger the failover to the healthy region.
3. Announce the failover in the incident channel.
4. Monitor replication catch-up as the failed region returns.

This is the hands-on protocol; the architecture behind it is covered in
the disaster recovery design.
