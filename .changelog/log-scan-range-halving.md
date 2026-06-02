---
tempo-common: patch
---

Preserve both halves of an oversized session channel log scan when an RPC requires a smaller `eth_getLogs` range, avoiding skipped upper-half ranges during wallet session recovery.
