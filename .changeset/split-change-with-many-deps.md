---
"loro-crdt": patch
---

Fix an integer underflow when the change store splits a change whose deps alone are larger than a change block, for example a change that merges more than 1,000 concurrent heads. Debug builds panicked on commit and on import.
