# Security & Copyright Compliance Policy

## Reporting Security Vulnerabilities

Oathkeep executes native x86-64 user-supplied binaries inside the host Windows process (unsandboxed). Only run executables you trust and own.

If you discover a security vulnerability in Oathkeep itself (such as unsafe path traversal outside the configured data/cache directories, unintended network egress, or memory safety bugs in loaders/parsers), please report it privately via GitHub Security Advisories on this repository rather than opening a public issue.

## Copyright / DMCA Compliance

Oathkeep is strictly a clean-room interoperability project and hosts no copyrighted software, assets, keys, or circumvention tools (see [docs/asset-policy.md](docs/asset-policy.md)).

If you believe any file, commit, issue, or community post inadvertently violates copyright or trademark law, please open a high-priority notice via GitHub or contact the repository maintainer directly. Verified violations will be removed immediately and purged from git history.
