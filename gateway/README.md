# AIxodia engine gateway

Gives every machine's engine its own path on **one** public hostname:

```
https://engines.aixodia.dpdns.org/<machine>/api/...   →   https://<machine>.aixodia.dpdns.org/api/...
```

`<machine>` is the name the engine gives itself: `m-` plus eight hex digits of
its `/etc/machine-id`, or whatever `AIXODIA_INSTANCE` overrides it with. It is
printed at startup (`machine m-…`) and returned by `/healthz` as `instance`.

The Worker reads the first path segment, looks the instance up in the Workers KV
namespace bound as `ENGINE_ORIGINS`, removes the segment, and forwards the request
to that instance's own Named Tunnel origin with the Cloudflare Access service
token attached. The engine therefore keeps serving the paths it always served
(`/api/...`, `/ws`, `/healthz`) and needs no knowledge of the shared address.

The shape of a name is the same one the engine enforces
(`aixodia_state::valid_instance_name`): at most 32 characters, lower-case letters,
digits and single hyphens, alphanumeric at both ends.

## Why a gateway instead of one hostname per engine

A tunnel per engine is still needed — each engine is a separate machine or a
separate process with its own credentials — but a client should not have to learn
a new hostname per engine, and a DNS record per engine is one more thing to
provision and protect. One hostname, one Access policy, one certificate, and a
path segment that names the instance.

## Cloudflare resources

- Zone `aixodia.dpdns.org` on the account.
- Worker route `engines.aixodia.dpdns.org/*` (change `wrangler.toml` if the
  gateway should live on `engine.aixodia.dpdns.org` instead — note that hostname
  is the single-instance engine's public address today).
- KV namespace bound as `ENGINE_ORIGINS`, one entry per machine:
  key `<machine>`, value `<machine>.aixodia.dpdns.org`.
- Access application protecting `*.aixodia.dpdns.org/*` with a service-token
  policy, and that service token stored as the `ACCESS_CLIENT_ID` /
  `ACCESS_CLIENT_SECRET` secrets.
- Access application protecting the gateway hostname itself, for whatever should
  be allowed to call the engines at all.

The Worker fails closed: no origin credential, no answer.

## Deploy

```bash
npx wrangler kv namespace create ENGINE_ORIGINS
# put the returned id in wrangler.toml
npx wrangler secret put ACCESS_CLIENT_ID
npx wrangler secret put ACCESS_CLIENT_SECRET
npx wrangler deploy
npx wrangler kv key put --binding=ENGINE_ORIGINS m-8a7d1192 m-8a7d1192.aixodia.dpdns.org
```

## Provisioning a machine

`<machine>` below is the engine's own name: `aixodia status` (or the log line
`machine m-…`) prints it after the engine has started once, and `/healthz`
returns it as `instance`. Nothing on the machine needs configuring.

1. Create a remotely-managed Named Tunnel for the machine.
2. Point its public hostname `<machine>.aixodia.dpdns.org` at
   `http://localhost:8787` — the engine's port is the same on every machine.
3. Protect that hostname with the Access service-token policy.
4. Write the KV entry: `ENGINE_ORIGINS <machine>` = `<machine>.aixodia.dpdns.org`.
5. Write the engine's own two keys: `instances/<machine>/tunnel_token` (the
   tunnel connector token) and `instances/<machine>/tunnel_url`
   (`https://<machine>.aixodia.dpdns.org`).
6. `aixodia start` — `CF_TOKEN` is the only variable it needs.

The engine publishes its live address to `instances/<machine>/daemon/url` and its
version to `instances/<machine>/daemon/version`.

## Test

```bash
npm test
```
