---
tempo-common: patch
tempo-request: patch
---

Fixed `tempo request` failing on 402 responses that offer multiple `tempo`-method payment challenges (e.g. moderato + mainnet on the same endpoint). The CLI now decodes every offered challenge and picks the first one the wallet can actually satisfy — matching `--network` (when set) and the keystore's `(chain_id, currency)`. When no challenge matches, a new `NoCompatibleChallenge` error lists the offered and held options and suggests `tempo wallet fund` / `tempo wallet login` instead of failing later with a cryptic "No key configured" message.
