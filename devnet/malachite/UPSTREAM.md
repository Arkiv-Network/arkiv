Adapted from https://github.com/circlefin/malaketh-layered at 63ac0589972412cb856a633d708a3d83bdada9aa.
Apache-2.0; upstream LICENSE retained. Local changes implement Arkiv payload
validation, cryptographic value IDs, durable decisions, and local devnet operation.

All Malachite dependencies use the published informalsystems-malachitebft crates
at exactly version 0.5.0 from crates.io. The old git revision and patched channel
crate have been removed. The published engine replays its Listening notification
to subscribers, so the local startup workaround is unnecessary.

The adapter uses the 0.5.0 configuration and application callback APIs, certificate
signatures, value synchronization, and liveness messages. A small local CLI generates
the four-validator configuration and starts each node.
