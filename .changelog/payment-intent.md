---
tempo-request: minor
---

Add `--payment-intent auto|session|charge`. Auto mode prefers a session and reports an explicit,
bounded charge fallback after session failure without submitting the non-refundable charge.
