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
   `http://localhost:47613/oauth/google/callback` for local use, plus
   `https://<your-host>/oauth/google/callback` once it runs on a server. Copy the client ID and
   secret.

For a Google Workspace account, if linking is blocked by policy, trust the client in
**admin.google.com → Security → API controls → Manage third-party app access**, configuring
it by client ID.

## Configuration

Settings come from the environment, either exported in the shell or in a `.env` file next to
`docker-compose.yml`; see [`.env.example`](.env.example).

| Variable | Purpose |
|---|---|
| `GMAIL_GATEWAY_GOOGLE_CLIENT_ID`, `GMAIL_GATEWAY_GOOGLE_CLIENT_SECRET` | The Web application client. |
| `GMAIL_GATEWAY_PUBLIC_URL` | Address the browser uses; the redirect URI is `${GMAIL_GATEWAY_PUBLIC_URL}/oauth/google/callback`. HTTPS, or `http://localhost:<port>`. Defaults to `http://localhost:47613` under compose. |
| `GMAIL_GATEWAY_CLIENTS` | `name=token` pairs, comma separated, one per caller. Tokens are at least 32 characters. |
| `GMAIL_GATEWAY_ENCRYPTION_KEY` | 32 bytes, base64. Seals refresh tokens in `accounts.json`. Losing it means re-linking. |
| `GMAIL_GATEWAY_DATA_DIR` | Where `accounts.json` lives inside the container. Defaults to `/data`. |
| `GMAIL_GATEWAY_HOST_DATA_DIR` | Host folder mounted at `/data` by compose. Defaults to `~/.config/gmail-gateway`. |
| `GMAIL_GATEWAY_UID`, `GMAIL_GATEWAY_GID` | User compose runs the container as; must own the host folder. Default `1000`. |
| `GMAIL_GATEWAY_PORT` | Listen port. Defaults to `47613`. |
| `RUST_LOG` | Log filter. Defaults to `info`. |

Generate tokens and the key with `openssl rand -base64 32`.

## Running

```sh
mkdir -p -m 700 ~/.config/gmail-gateway
docker compose up -d --build
```

Create the data folder first: Docker creates a missing one owned by root, and the gateway then
refuses to start because it cannot write there. The container listens on `127.0.0.1:47613`.

**Locally**, register `http://localhost:47613/oauth/google/callback` on the OAuth client and
leave `GMAIL_GATEWAY_PUBLIC_URL` unset.

**On a server**, set `GMAIL_GATEWAY_PUBLIC_URL` to the public HTTPS address, point
`GMAIL_GATEWAY_HOST_DATA_DIR` and `GMAIL_GATEWAY_UID`/`GID` at the folder and its owner, add its callback
to the same OAuth client, and put a reverse proxy in front of port 47613 to terminate TLS. Only
`/healthz` and the OAuth callback are reachable without a bearer token.

## Linking accounts

Ask the gateway for a Google sign-in URL, open it in a browser, and approve access:

```sh
curl -s -X POST http://localhost:47613/accounts/link \
  -H "Authorization: Bearer $GMAIL_GATEWAY_TOKEN"
```

From OpenCode you can call the `link_account` tool instead. Repeat for each account (personal,
work). Linking an account again replaces its stored token, which is also how you recover one
whose authorization was revoked.

```sh
curl -s http://localhost:47613/accounts -H "Authorization: Bearer $GMAIL_GATEWAY_TOKEN"
curl -s -X DELETE http://localhost:47613/accounts/me@gmail.com -H "Authorization: Bearer $GMAIL_GATEWAY_TOKEN"
```

## OpenCode

Use the server's public HTTPS URL in place of localhost once it runs there.

```jsonc
{
  "mcp": {
    "servers": {
      "gmail": {
        "type": "remote",
        "url": "http://localhost:47613/mcp",
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
