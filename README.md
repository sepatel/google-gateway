# gmail-gateway

One always-on service that holds the Google authorization for your Gmail accounts and
exposes them to other tools:

- **MCP** at `/mcp` (Streamable HTTP) for agents such as OpenCode.
- **HTTP** endpoints for account management, with a REST API for Post Office to follow.

It talks to the regular Gmail API, so it needs no Workspace Developer Preview enrolment, and it
keeps a refresh token per account, so nothing has to sign in again every hour or every week.

## Google Cloud setup (once)

1. Create a project, for example `sejal-personal-agents`, and enable the **Gmail API**.
2. **Google Auth Platform → Branding**: set an app name and support email.
3. **Audience**: user type **External**, then **Publish app** so it is *In production*.
   Testing mode expires refresh tokens after 7 days. Unverified production apps work for up
   to 100 users; you click through a "Google hasn't verified this app" warning when linking.
4. **Data Access**: add the scope `https://mail.google.com/` (full mailbox, including
   permanent delete).
5. **Clients → Create client**: type **Web application**, authorized redirect URI
   `https://<your-host>/oauth/google/callback`. Copy the client ID and secret.

For a Google Workspace account, if linking is blocked by policy, trust the client in
**admin.google.com → Security → API controls → Manage third-party app access**, configuring
it by client ID.

## Configuration

All settings come from the environment; see [`.env.example`](.env.example).

| Variable | Purpose |
|---|---|
| `GOOGLE_CLIENT_ID`, `GOOGLE_CLIENT_SECRET` | The Web application client. |
| `PUBLIC_URL` | Public HTTPS address; the redirect URI is `${PUBLIC_URL}/oauth/google/callback`. |
| `GATEWAY_CLIENTS` | `name=token` pairs, comma separated, one per caller. Tokens are at least 32 characters. |
| `GATEWAY_ENCRYPTION_KEY` | 32 bytes, base64. Seals refresh tokens in `accounts.json`. Losing it means re-linking. |
| `DATA_DIR` | Where `accounts.json` lives. Defaults to `/data`. |
| `PORT` | Listen port. Defaults to `8080`. |
| `RUST_LOG` | Log filter. Defaults to `info`. |

Generate tokens and the key with `openssl rand -base64 32`.

## Running on the NAS

```sh
cp .env.example .env   # fill it in
docker compose up -d --build
```

The compose file binds the container to `127.0.0.1:8080`. Put your reverse proxy in front of
it to terminate TLS on the public hostname; the gateway itself only speaks HTTP. Only
`/healthz` and the OAuth callback are reachable without a bearer token.

## Linking accounts

Ask the gateway for a Google sign-in URL, open it in a browser, and approve access:

```sh
curl -s -X POST https://<your-host>/accounts/link \
  -H "Authorization: Bearer $GMAIL_GATEWAY_TOKEN"
```

From OpenCode you can call the `link_account` tool instead. Repeat for each account (personal,
work). Linking an account again replaces its stored token, which is also how you recover one
whose authorization was revoked.

```sh
curl -s https://<your-host>/accounts -H "Authorization: Bearer $GMAIL_GATEWAY_TOKEN"
curl -s -X DELETE https://<your-host>/accounts/me@gmail.com -H "Authorization: Bearer $GMAIL_GATEWAY_TOKEN"
```

## OpenCode

```jsonc
{
  "mcp": {
    "servers": {
      "gmail": {
        "type": "remote",
        "url": "https://<your-host>/mcp",
        "oauth": false,
        "headers": { "Authorization": "Bearer {env:GMAIL_GATEWAY_TOKEN}" }
      }
    }
  }
}
```

## MCP tools

| Tool | Purpose |
|---|---|
| `list_accounts` | Linked accounts. |
| `link_account` | Returns a Google sign-in URL to link or re-link an account. |
| `search_threads` | Gmail search syntax; returns subject, sender, date, snippet and unread state per thread. |
| `get_thread` | Every message in a thread with plain-text bodies, oldest first. |
| `get_message` | One message with its body and attachment list. |
| `list_labels` | Labels and their IDs. |

Every mail tool takes an optional `account`; it may be omitted while only one account is linked.
Message bodies are capped at 50,000 characters, and a thread at 100,000, dropping the oldest
bodies first.

## Roadmap

1. Write tools: labels, archive, trash, permanent delete, read state, drafts.
2. REST API for Post Office over the same operations.
3. Gmail push notifications through Pub/Sub, forwarded to Post Office as webhooks.

## Development

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
```
