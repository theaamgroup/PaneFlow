# Security Policy

## Supported versions

Security fixes land on the latest release. Always run the most recent build from
the [releases page](https://github.com/theaamgroup/panescli/releases/latest).

## Reporting a vulnerability

**Do not open a regular issue for a security report.**

Open a draft advisory at
[Security → Advisories](https://github.com/theaamgroup/panescli/security/advisories/new),
or raise it directly with the repository owner. Include:

- a description of the issue and its impact,
- steps to reproduce (or a proof of concept),
- the Paneflow version (`paneflow --version`), the macOS version, and the chip
  (Apple Silicon).

## Scope

Areas most relevant to Paneflow's threat model:

- the **JSON-RPC IPC server** (Unix domain socket) and any method it exposes,
- the **MCP bridge** (`list_panes` / `read_pane` / `search_pane`) and how pane
  output is wrapped as untrusted data,
- the **in-app updater** (download, minisign verification, atomic install),
- PTY handling and any path where untrusted agent or terminal output reaches a
  privileged surface (for example OS notifications).

Verifying release artifact signatures is documented in
[`docs/release/macos-signing.md`](../docs/release/macos-signing.md) (Developer ID
codesign plus notarization) and [`docs/self-update-signing.md`](../docs/self-update-signing.md)
(minisign, the updater's root of trust).
