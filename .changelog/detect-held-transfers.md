---
tempo-wallet: patch
tempo-common: patch
---

`tempo wallet transfer` now waits for the transaction receipt and inspects its outcome instead of reporting success as soon as the transaction is broadcast. Reverted transfers return an error, and transfers held by a recipient's TIP-1028 receive policy report `status: "held"` along with the claim details emitted by the `ReceivePolicyGuard` precompile.
