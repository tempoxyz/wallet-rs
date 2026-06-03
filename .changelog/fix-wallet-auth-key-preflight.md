---
tempo-request: patch
tempo-wallet: patch
---

Fix v0.4.3 wallet regressions by sending auth `chainId` in the backend-compatible hex/camelCase shape and by preventing transient key spending-limit preflight failures from forcing an already-provisioned access key through registration again.
