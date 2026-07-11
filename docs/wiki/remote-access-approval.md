# Remote Access and Network Policy

## Overview

When `sandbox_exec` runs, the gateway **statically analyzes** the invoked code before execution to detect patterns that imply network access. If detected, execution is blocked pending operator approval.

This is a **deterministic** security check that does not rely on LLM self-declaration.

## Detection Categories

### URL Literals

| Pattern | Reason |
|---------|--------|
| `"https://api.example.com/..."` | Concrete HTTP target |
| `"http://192.168.1.1/..."` | Concrete IP target |

The gateway extracts hostnames and IP addresses from string literals in the source and requires them to be covered by `NetworkAccess.hosts`.

### IP Address Literals

| Pattern | Reason |
|---------|--------|
| IPv4 literals (`1.2.3.4`) | Concrete network target |
| IPv6 literals (`::1`, `2001:db8::1`) | Concrete network target |

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

1. Policy/capability check — does the agent hold `NetworkAccess`?
2. Static analysis detects network patterns in the code
3. Gateway extracts concrete targets (hostnames, IPs, URLs) and checks them against the agent's declared `remote_access.targets`
4. Approval deduplication pipeline checks exec cache → plan grants → session grants → existing approvals → flood cap
5. If no existing approval covers the targets, operator approval is requested
6. Once approved, execution proceeds with network access enabled

## Declaring Network Access in SKILL.md

Agents that need network access must declare it in their capabilities:

```yaml
capabilities:
  - type: "NetworkAccess"
    hosts: ["api.example.com", "*.github.com"]
```

The `hosts` list is a whitelist — only declared hosts are accessible. `hosts: ["*"]` is only accepted when the agent also declares `open_web: true` (constitution P-1.5).

## Remote Access Analysis for Artifacts

`artifact_exec` runs the same static analysis against the artifact's source files (not the shell command string). This means:
- Analysis is based on what the code actually does, not what the command string looks like
- Approval reuse is bound to the artifact's canonical digest
- Same artifact re-run with different arguments reuses the prior approval
