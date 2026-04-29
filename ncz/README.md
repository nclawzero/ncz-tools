# ncz

`ncz` is the on-device nclawzero operator CLI.

## v0.2 Operator Surface

### API Credentials

Credentials live in `/etc/nclawzero/agent-env`, loaded by every agent quadlet
through `EnvironmentFile=`.

```bash
ncz api list
ncz api list --json --show-secrets
ncz api add TOGETHER_API_KEY "$TOGETHER_API_KEY"
ncz api add TOGETHER_API_KEY --value-env TOGETHER_API_KEY
printf '%s' "$OPENAI_API_KEY" | ncz api set OPENAI_API_KEY --value-stdin
ncz api set OPENAI_API_KEY env:OPENAI_API_KEY --agents=zeroclaw,hermes
ncz api set TOGETHER_API_KEY env:TOGETHER_API_KEY --providers=together
ncz api remove TOGETHER_API_KEY
ncz api remove OLD_KEY --force
```

`api add` and `api set` always write the shared `agent-env` file. When
`--agents` is supplied, v0.2 also writes `/etc/nclawzero/<agent>/.env` override
stub files for the named agents; the shared file remains the active default
credential surface. Literal values are accepted for compatibility with the
v0.2 command contract, but `--value-env`, `--value-stdin`, `env:VAR`, or `-`
avoid putting secrets in shell history. Successful credential mutations report
`restart_required=true` because systemd reads
`EnvironmentFile=` values when each agent service starts; restart the affected
agent units to apply or revoke the credential at runtime. `api remove` refuses to
delete a key still referenced by provider or MCP declarations unless `--force` is
specified. Values are written as systemd-compatible `EnvironmentFile=`
assignments; opaque secrets with spaces or symbols are quoted automatically.
Newline and NUL bytes are rejected. `--providers=name,...` records a non-secret
approval binding for existing provider declarations that reference the same
`key_env`; live model discovery will not send a shared `agent-env` credential to
a provider unless that provider name, key, and URL match the approval record.
`ncz selftest` verifies the active agent quadlet loads the shared
`EnvironmentFile=` before reporting a clean runtime baseline.

### Providers

Provider declarations are canonical JSON files in
`/etc/nclawzero/providers.d/<name>.json`.

```bash
ncz providers list
ncz providers add together \
  --url=https://api.together.xyz \
  --model=meta-llama/Llama-3.3-70B-Instruct-Turbo \
  --key-env=TOGETHER_API_KEY
ncz providers show together --json
ncz providers test together
ncz api set TOGETHER_API_KEY --value-env TOGETHER_API_KEY --providers=together
ncz providers set-primary together
ncz providers remove together
ncz providers remove legacy-local --drop-inline-credentials
```

`providers add` defaults `--type=openai-compat` and
`--health-path=/health`. Existing legacy `.env`, `.conf`, or schema-less JSON
provider files in `providers.d/` are read in place by read-only commands; they
are only migrated during mutating provider-bound flows such as `set-primary` or
`api set --providers`. `providers add` rejects any existing canonical or legacy
provider alias unless `--force` is passed. Keep credentials
in `key_env`; URLs with embedded userinfo, query strings, or fragments are
rejected. Health paths are path-only values, not full URLs. `set-primary`
requires a non-empty credential plus a matching provider approval binding; create
or refresh that binding with `ncz api set KEY --providers=name` before switching
to a canonical provider. `providers remove` preserves legacy inline credentials
by default; pass `--drop-inline-credentials` only when intentionally purging a
legacy declaration that still contains secret material. It updates the global
primary provider and the active agent's per-agent primary file; inactive
per-agent overrides are preserved.

### Models

`models list` and `models status` try each canonical provider's
OpenAI-compatible `/v1/models` endpoint with the current bound credential, then
fall back to a current discovery cache, static `models` entries declared in
provider JSON, or the configured model. `models discover` is the explicit cache
refresh path for one provider.

```bash
ncz models list
ncz models list --provider=together --show-unhealthy
ncz models status --provider=together
ncz models discover together
```

`models discover` refreshes
`/etc/nclawzero/providers.d/<name>.models.json` with the current catalog and a
timestamp. When a provider uses an `agent-env` credential, live catalog requests
require an approval binding created with `ncz api set KEY --providers=name` and
refuse plaintext remote HTTP endpoints; use HTTPS or a loopback URL for local
providers. Static `models` entries remain usable when the credential is
unavailable. Discovery caches written from credentialed `/v1/models` calls are
used only when the current bound credential can be validated against the cache.
For legacy inline credentials, `models discover` may use the legacy secret for
the explicit refresh but does not migrate provider files or rewrite
`agent-env`; run `ncz api set KEY --providers=name` or `providers set-primary`
for the mutating migration path.

### MCP Servers

MCP declarations live in `/etc/nclawzero/mcp.d/<name>.json`.

```bash
ncz mcp list
ncz mcp add filesystem --transport=stdio --command='mcp-filesystem /srv'
ncz mcp add search --transport=http --url=https://mcp.example.test \
  --auth-env=MCP_TOKEN --auth-value-env=MCP_TOKEN
ncz mcp show search --json
ncz mcp remove search
```

Default text and JSON output redacts secret-bearing values unless
`--show-secrets` is passed. HTTP MCP declarations that specify `--auth-env`
must use HTTPS unless the endpoint is loopback, and URL userinfo is rejected.
Authenticated declarations also require an approval binding for the exact MCP
name, auth env var, and endpoint: HTTP binds the URL, while stdio binds a hash of
the command. Supplying `--auth-value-env` during `mcp add` sets or refreshes the
shared credential and records that binding without placing the token on argv;
the same command can refresh the approval for an existing identical declaration.
Stdio commands reject inline auth flags, including header, user/pass, token,
cookie, and session-cookie arguments. `mcp remove` removes the binding for that
server. `api remove --force` reports any MCP bindings it revokes so the affected
servers can be reapproved in place; `mcp add --auth-value-env` reports provider
or MCP bindings it revokes when rotating a shared auth env var.
