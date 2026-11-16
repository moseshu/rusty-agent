# Rama DNS compatibility patch

This copy of `rama-dns 0.3.0-alpha.4` preserves the API used by the Codex-derived
network proxy and resolves against Hickory 0.26.3 or later in the 0.26 series.
The adaptation uses Hickory's new resolver builder, nameserver presets, and
record accessors. The infallible Rama builder reports a Hickory initialization
failure through subsequent lookups. System initialization still returns errors
immediately and the default resolver still falls back to the Cloudflare preset.

The CLI and integration-test workspaces each apply the local `[patch.crates-io]`.
Cargo applies patches from the workspace root, so a separate workspace that uses
`ra-network-proxy` transitively must apply the same `rama-dns` patch at its root.
Remove this copy when the network proxy can use a compatible Rama release with
a fixed resolver dependency.

The original sources are from [plabayo/rama at 4733273][source]. The Hickory
API adaptation follows [Rama 0.3.0][adaptation]. The MIT and Apache-2.0 licenses
are retained alongside this file.

[source]: https://github.com/plabayo/rama/tree/4733273a10a791762e2b7727850032b0c9b8536d/rama-dns
[adaptation]: https://github.com/plabayo/rama/blob/rama-0.3.0/rama-dns/src/client/hickory.rs
