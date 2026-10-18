# SyncEngine LLD UML diagrams

This bundle contains two editable Graphviz DOT sources plus SVG and PNG renders.

- `client_lld.dot` / `.svg` / `.png`: core client-side class/module diagram.
- `server_lld.dot` / `.svg` / `.png`: server-side class/module diagram.

## Reading the diagrams

- Solid arrows indicate a use/call/flow relationship.
- Dashed arrows with a hollow triangle indicate a type implementing a trait/interface.
- `«struct»` and `«trait»` are Rust-oriented UML stereotypes.
- These are logical types/modules inside the client and server binaries, not a proposal to deploy every box as a separate service.

## Important design decisions reflected

- `OnlineTransport` and `LanTransport` implement the same `TransferTransport` abstraction, so chunking, retries, resume checkpoints, and integrity verification live in one shared `TransferEngine`.
- Local persistence is behind a `Store` trait, implemented by `SqliteStore`.
- `VersionVector` is used for causal ordering; modification time alone is not sufficient for conflict detection.
- Watcher events are hints; `Reconciler` performs periodic filesystem reconciliation. An incomplete scan must not be interpreted as mass deletion.
- Server relay forwards opaque encrypted frames and does not persistently store file contents. PostgreSQL stores metadata only.

## Editing / rendering

Edit the `.dot` source using any text editor, then render with Graphviz:

```bash
dot -Tsvg client_lld.dot -o client_lld.svg
dot -Tpng -Gdpi=170 client_lld.dot -o client_lld.png
```
