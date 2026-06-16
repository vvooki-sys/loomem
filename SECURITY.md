# Security Policy

## Reporting a vulnerability

Please report security vulnerabilities privately to **security@loomem.ai**.

Do not open public GitHub issues for security problems. We aim to acknowledge reports within 72 hours and to ship a fix or mitigation before any public disclosure.

When reporting, include:

- A description of the issue and its impact.
- Steps to reproduce (a minimal proof of concept helps a lot).
- The version / commit you tested against.

## Supported versions

Loomem is pre-1.0; only the latest release (and `main`) receive security fixes.

## Scope notes

- Loomem is designed to run as a **single-user, private instance**. Exposing an instance to the public internet without an API key (`LOOMEM_AUTH_TOKEN`) is unsupported and unsafe.
- Memory content is sensitive by nature. See [docs/SECURITY.md](docs/SECURITY.md) for the security model, encryption at rest, and logging guidance.
