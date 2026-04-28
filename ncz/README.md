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
Newline and NUL bytes are rejected.

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
ncz providers set-primary together
ncz providers remove together
```

`providers add` defaults `--type=openai-compat` and
`--health-path=/health`. Existing legacy `.env`, `.conf`, or schema-less JSON
provider files in `providers.d/` are migrated on first provider/model read:
canonical JSON is written, inline legacy secrets are moved to `agent-env` if the
target key is not already set, and the legacy file is removed after a successful
migration. `providers add` rejects any existing canonical or legacy provider
alias unless `--force` is passed. Keep credentials in `key_env`; URLs with
embedded userinfo, query strings, or fragments are rejected. Health paths are
path-only values, not full URLs.

### Models

`models` queries each provider's OpenAI-compatible `/v1/models` endpoint and
falls back to static `models` entries declared in provider JSON or cached
discovery data when the live catalog is unavailable.

```bash
ncz models list
ncz models list --provider=together --show-unhealthy
ncz models status --provider=together
ncz models discover together
```

`models discover` refreshes
`/etc/nclawzero/providers.d/<name>.models.json` with the current catalog and a
timestamp by calling the provider's OpenAI-compatible `/v1/models` endpoint.
When a provider uses an `agent-env` credential, discovery refuses plaintext
remote HTTP endpoints; use HTTPS or a loopback URL for local providers.

### MCP Servers

MCP declarations live in `/etc/nclawzero/mcp.d/<name>.json`.

```bash
ncz mcp list
ncz mcp add filesystem --transport=stdio --command='mcp-filesystem /srv'
ncz mcp add search --transport=http --url=https://mcp.example.test --auth-env=MCP_TOKEN
ncz mcp show search --json
ncz mcp remove search
```

Default text and JSON output redacts secret-bearing values unless
`--show-secrets` is passed. HTTP MCP declarations that specify `--auth-env`
must use HTTPS unless the endpoint is loopback, and URL userinfo is rejected.
