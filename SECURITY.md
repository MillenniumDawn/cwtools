# Security Policy

## Supported versions

Only the latest release gets fixes. Older versions are not patched.

## Reporting a vulnerability

Use [private vulnerability reporting](https://github.com/MillenniumDawn/cwtools/security/advisories/new). That opens a private thread with the maintainers. Please don't file a public issue for a security problem.

Useful things to include: affected version or commit, steps to reproduce, and what an attacker actually gets. A proof of concept helps but isn't required.

This is a hobby project, so response times are best effort. If the report holds up we'll fix it and credit you in the advisory, unless you'd rather stay anonymous.

## Scope

This repo is the cwtools engine: the Rust language server in `cwtools-rs` and the rule files it reads. The VS Code extension that ships this server lives in [MillenniumDawn/cwtools-vscode](https://github.com/MillenniumDawn/cwtools-vscode). Report client and packaging bugs there.

The server parses untrusted mod files fed to it by an editor, so panics and hangs on malformed input, reading outside the workspace, and anything that gets code executing from a parsed file are all worth reporting.
