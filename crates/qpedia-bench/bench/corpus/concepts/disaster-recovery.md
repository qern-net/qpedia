---
title: "Disaster recovery architecture"
kind: concept
tags: ["reliability", "dr", "failover"]
---

# Disaster recovery architecture

Our disaster-recovery strategy keeps the service available when an entire
region becomes unavailable. We run active-active replication across two
regions so traffic can shift without data loss.

## Principles

- Active-active: both regions serve traffic at all times.
- Replication lag is monitored and alerted.
- Failover is automated but reversible.

When a region goes offline, traffic drains to the healthy region while the
failed region recovers.
