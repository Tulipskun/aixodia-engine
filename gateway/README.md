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

The namespace already exists and its id is in `wrangler.toml`; so does one KV entry
(`m-8a7d1192`). A fresh account needs:

```bash
npx wrangler kv namespace create ENGINE_ORIGINS   # then put the id in wrangler.toml
```

The Worker needs a service token to reach an Access-protected origin. Its secret is
shown once, when it is created, so keep a copy where the next session can find it:
this Worker's own KV namespace (`ENGINE_ORIGINS`) holds `gateway/access_client_id`
and `gateway/access_client_secret`, next to the Worker's `ACCESS_CLIENT_ID` /
`ACCESS_CLIENT_SECRET` secrets. Nothing auth-related is written here, and this
Worker reads nothing from the auth Worker's namespace.

```bash
curl -s -X POST -H "Authorization: Bearer $CF_TOKEN" -H 'content-type: application/json' \
  -d '{"name":"aixodia-engine-gateway","duration":"87600h"}' \
  "https://api.cloudflare.com/client/v4/accounts/$ACCOUNT/access/service_tokens"
# client_id and client_secret above -> KV, then:
printf '%s' "$CLIENT_ID"     | npx wrangler secret put ACCESS_CLIENT_ID
printf '%s' "$CLIENT_SECRET" | npx wrangler secret put ACCESS_CLIENT_SECRET
npx wrangler deploy
npx wrangler kv key put --binding=ENGINE_ORIGINS --remote m-8a7d1192 m-8a7d1192.aixodia.dpdns.org
```

The token also has to be allowed by the origin's Access application: add a
`non_identity` policy whose include is `service_token.token_id` = that token.

`engines.aixodia.dpdns.org` needs a proxied DNS record (a CNAME to `aixodia.dpdns.org`
is enough) or the route has no name to answer on.

## Provisioning a machine

`<machine>` below is the engine's own name: `aixodia status` (or the log line
`machine m-…`) prints it after the engine has started once, and `/healthz`
returns it as `instance`. Nothing on the machine needs configuring.

1. Create a remotely-managed Named Tunnel for the machine.
2. Point its public hostname `<machine>.aixodia.dpdns.org` at
   `http://localhost:8787` — the engine's port is the same on every machine.
   A machine that already runs a tunnel only needs one more ingress rule and a
   proxied CNAME to the tunnel's `…cfargotunnel.com` target.
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
