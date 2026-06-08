# Remote Access and Network Policy

## Overview

When `sandbox_exec` runs, the gateway **statically analyzes** the invoked code before execution to detect patterns that imply network access. If detected, execution is blocked pending operator approval.

This is a **deterministic** security check that does not rely on LLM self-declaration.

## Detection Categories

### Network Library Imports

| Pattern | Reason |
|---------|--------|
| `import requests` | HTTP client |
| `from urllib import urlopen` | URL handling |
| `import socket` | Low-level networking |
| `import httpx` | Async HTTP client |
| `import aiohttp` | Async HTTP client |
| `import ftplib` | FTP client |
| `import smtplib` | SMTP client |
| `import paramiko` | SSH client |
| `import boto3` | AWS SDK |
| `import google.cloud` | GCP SDK |

### Network Function Calls

| Pattern | Reason |
|---------|--------|
| `.connect()` | Socket connection |
| `.send()` / `.recv()` | Network transmission/reception |
| `urlopen()` | URL connection |
| `requests.get()` / `requests.post()` | HTTP requests |
| `httpx.get()` | Async HTTP |

## Approval Flow

1. Static analysis detects network patterns in the code
2. Gateway extracts concrete targets (hostnames, IPs, URLs)
3. Approval deduplication pipeline checks exec cache → session grants → existing approvals → flood cap
4. If no existing approval covers the targets, operator approval is requested
5. Once approved, execution proceeds with network access enabled

## Declaring Network Access in SKILL.md

Agents that need network access must declare it in their capabilities:

```yaml
capabilities:
  - type: "NetworkAccess"
    hosts: ["api.example.com", "*.github.com"]
```

The `hosts` list is a whitelist — only declared hosts are accessible. Use `["*"]` for unrestricted access (requires high-risk promotion gate).

## Remote Access Analysis for Artifacts

`artifact_exec` runs the same static analysis against the artifact's source files (not the shell command string). This means:
- Analysis is based on what the code actually does, not what the command string looks like
- Approval reuse is bound to the artifact's canonical digest
- Same artifact re-run with different arguments reuses the prior approval
