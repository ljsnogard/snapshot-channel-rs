# snapshot-channel

This crate provides a tool that wraps a future and shares its output to arbitrary number of viewers.  

```rust
use atomex::StrictOrderings;
use snapshot_channel::{x_deps::atomex, Glimpse};

use tokio::sync::mpsc;

let (tx, mut rx) = mpsc::channel::<()>(1);
let glimpse = Glimpse::<_, StrictOrderings>::new(rx.recv());
let mut snapshot = glimpse.snapshot();
assert!(matches!(
    snapshot.try_peek(),
    Result::Ok(Option::None)),
);
assert!(tx.try_send(()).is_ok());
assert!(matches!(
    snapshot.peek_async().await,
    Result::Ok(_),
));
```