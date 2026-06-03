---
title: "Resolve ERR_PAYMENT_GATEWAY_REJECTED"
kind: runbook
tags: ["payments", "error-code", "gateway"]
---

# Runbook: `ERR_PAYMENT_GATEWAY_REJECTED` (payment-svc)

The payment gateway received the request but declined it.

## Diagnosis

1. Check the decline reason code returned by the gateway.
2. Confirm the merchant account is in good standing.
3. Review fraud rules that may be rejecting the transaction.

If you see `ERR_PAYMENT_GATEWAY_TIMEOUT` instead, the request never got a
response — see that runbook.

## Resolution

Rejections are usually a data or policy problem, not an infrastructure
one. Do not retry blindly; resolve the decline reason first.
