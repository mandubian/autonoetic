# credential-register smoke — classic case 5 of the validation study

End-to-end exercise of the credential lifecycle under the study protocol
(`docs/rfc/classic-harness-usecase-validation.md` §3.5):

1. A **mock weather service** (`mock_service.py`) runs on 127.0.0.1 and
   answers `GET /weather?city=X` only when the `X-Api-Key` header matches a
   demo secret. It never logs header values — only path, status, verdict.
2. A fresh gateway + `planner.default` receive one self-contained task:
   set up a credential for service `mockweather` (operator enters the secret
   at the approval gate — never in chat), then fetch Toulouse weather via
   `credential_request` with gateway-side header injection.
3. The auto-resolver plays operator: it approves gates, and attaches
   `--secret api_key=$DEMO_SECRET` to the CredentialPrompt approval (the
   non-interactive equivalent of the masked CLI/TUI prompt).
4. `verdict.py` decides PASS/FAIL from the gateway store, the mock service
   log, and a **leak scan**: the demo secret must appear nowhere except the
   encrypted vault — not in the reply, the gateway log, the session digest,
   causal events, or any bytes of `gateway.db`(+wal/shm).

## The invariant under test

A direct-loop harness puts the API key in the transcript and the provider
context window. Autonoetic's claim is that the secret transits
operator → vault → injected request without ever entering LLM-visible
state. This smoke fails loudly if that claim breaks.

## Running

```sh
smoke/credential-register/run_demo.sh
```

Env knobs: `CR_PORT` (base, default 4388), `CR_MODEL` (default
`deepseek-v4-flash`), `CR_MAX_*` budgets, `AUTONOETIC_BIN`. Requires
`OPENCODE_API_KEY` (LLM) and `python3` (stdlib only).

## Expected gates

- 1 × CredentialPrompt approval (secret entry) — auto-resolved with
  `--secret`.
- 0–1 × host approval for the `credential_request` call to 127.0.0.1
  (depends on `allowed_hosts` on the credential record).

More gates than that is a study finding, not a demo failure — see the RFC's
absurdity check for this case.

## Why a mock service and not a real API

Hermeticity and assertion strength: the mock's request log *proves* the
gateway sent the correct `X-Api-Key` header (injection worked end-to-end),
which a real API can only imply via a 200. The manual case-5 run in the RFC
uses a real service (GitHub PAT) to exercise the true human-entry gate.
