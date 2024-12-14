# snapshot-channel

This crate provides a tool that wraps a future and shares its output to arbitrary number of viewers.  

```rust
use pin_utils::pin_mut;

use atomex::StrictOrderings;
use snapshot_channel::{x_deps::{atomex, pin_utils}, Glimpse};

use tokio::sync::mpsc;

let (tx, mut rx) = mpsc::channel::<()>(1);
let glimpse = Glimpse::<_, StrictOrderings>::new(rx.recv());
let snapshot = glimpse.snapshot();
let snapshot_cloned = snapshot.clone();

assert!(snapshot.try_peek().is_none());
assert!(snapshot_cloned.try_peek().is_none());

pin_mut!(snapshot);
pin_mut!(snapshot_cloned);

assert!(tx.try_send(()).is_ok());
assert!(snapshot.as_mut().peek_async().await.is_some());

// Cloned snapshot should observe the same result
assert!(snapshot_cloned.peek_async().await.is_some());

// Multiple times of peek_async is legal and idempotent
assert!(snapshot.peek_async().await.is_some());
```
