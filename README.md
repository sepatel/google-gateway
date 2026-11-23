# google-gateway

One always-on service that holds the Google authorization for your accounts and exposes
their Gmail, Calendar and Contacts to other tools:

- **MCP** at `/mcp` (Streamable HTTP) for agents such as OpenCode.
- **HTTP** endpoints for account management, with a REST API for Post Office to follow.

It talks to Google's regular APIs, so it needs no Workspace Developer Preview enrolment, and it
keeps a refresh token per account, so nothing has to sign in again every hour or every week.
Gmail has full access; Calendar and Contacts are read-only.

## Google Cloud setup (once)

1. Create a project, for example `sejal-personal-agents`, and enable the **Gmail API**, the
   **Google Calendar API** and the **People API**.
2. **Google Auth Platform → Branding**: set an app name and support email.
3. **Audience**: user type **External**, then **Publish app** so it is *In production*.
   Testing mode expires refresh tokens after 7 days. Unverified production apps work for up
   to 100 users; you click through a "Google hasn't verified this app" warning when linking.
4. **Data Access**: add these scopes:

   | Scope | Grants |
   |---|---|
   | `https://mail.google.com/` | Full mailbox, including permanent delete |
   | `.../auth/calendar.readonly` | Reading calendars, events and free/busy |
   | `.../auth/contacts.readonly` | Reading saved contacts |
   | `.../auth/contacts.other.readonly` | Reading people Gmail saved automatically |
   | `.../auth/directory.readonly` | Reading a Workspace organisation's directory |
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
| `GOOGLE_GATEWAY_GOOGLE_CLIENT_ID`, `GOOGLE_GATEWAY_GOOGLE_CLIENT_SECRET` | The Web application client. |
| `GOOGLE_GATEWAY_PUBLIC_URL` | Address the browser uses; the redirect URI is `${GOOGLE_GATEWAY_PUBLIC_URL}/oauth/google/callback`. HTTPS, or `http://localhost:<port>`. Defaults to `http://localhost:47613` under compose. |
| `GOOGLE_GATEWAY_CLIENTS` | `name=token` pairs, comma separated, one per caller. Tokens are at least 32 characters. |
| `GOOGLE_GATEWAY_ENCRYPTION_KEY` | 32 bytes, base64. Seals refresh tokens in `accounts.json`. Losing it means re-linking. |
| `GOOGLE_GATEWAY_DATA_DIR` | Where `accounts.json` lives inside the container. Defaults to `/data`. |
| `GOOGLE_GATEWAY_HOST_DATA_DIR` | Host folder mounted at `/data` by compose. Defaults to `~/.config/google-gateway`. |
| `GOOGLE_GATEWAY_UID`, `GOOGLE_GATEWAY_GID` | User compose runs the container as; must own the host folder. Default `1000`. |
| `GOOGLE_GATEWAY_PORT` | Listen port. Defaults to `47613`. |
| `RUST_LOG` | Log filter. Defaults to `info`. |

Generate tokens and the key with `openssl rand -base64 32`.

## Running

```sh
mkdir -p -m 700 ~/.config/google-gateway
docker compose up -d --build
```

Create the data folder first: Docker creates a missing one owned by root, and the gateway then
refuses to start because it cannot write there. The container listens on `127.0.0.1:47613`.

**Locally**, register `http://localhost:47613/oauth/google/callback` on the OAuth client and
leave `GOOGLE_GATEWAY_PUBLIC_URL` unset.

**On a server**, set `GOOGLE_GATEWAY_PUBLIC_URL` to the public HTTPS address, point
`GOOGLE_GATEWAY_HOST_DATA_DIR` and `GOOGLE_GATEWAY_UID`/`GID` at the folder and its owner, add its callback
to the same OAuth client, and put a reverse proxy in front of port 47613 to terminate TLS. Only
`/healthz` and the OAuth callback are reachable without a bearer token.

## Linking accounts

Ask the gateway for a Google sign-in URL, open it in a browser, and approve access:

```sh
curl -s -X POST http://localhost:47613/accounts/link \
  -H "Authorization: Bearer $GOOGLE_GATEWAY_TOKEN"
```

From OpenCode you can call the `link_account` tool instead. Repeat for each account (personal,
work). Linking an account again replaces its stored token, which is also how you recover one
whose authorization was revoked or add a service you left unticked on the consent screen.
`/accounts` lists the services each account granted; Gmail is required.

```sh
curl -s http://localhost:47613/accounts -H "Authorization: Bearer $GOOGLE_GATEWAY_TOKEN"
curl -s -X DELETE http://localhost:47613/accounts/me@gmail.com -H "Authorization: Bearer $GOOGLE_GATEWAY_TOKEN"
```

## OpenCode

Each linked account has its own endpoint, `/mcp/<email>`, exposing only that account's mail,
calendar and contacts, so every account is a separate MCP server you can switch on and off. `/mcp` serves all accounts and adds
the `list_accounts` and `link_account` tools.

```jsonc
{
  "mcp": {
    "servers": {
      "google-work": {
        "type": "remote",
        "url": "http://localhost:47613/mcp/me@work.example",
        "oauth": false,
        "headers": { "Authorization": "Bearer {env:GOOGLE_GATEWAY_TOKEN}" }
      },
      "google-personal": {
        "type": "remote",
        "url": "http://localhost:47613/mcp/me@gmail.com",
        "oauth": false,
        "headers": { "Authorization": "Bearer {env:GOOGLE_GATEWAY_TOKEN}" }
      }
    }
  }
}
```

Use the server's public HTTPS URL in place of localhost once it runs there. An endpoint for an
account that is not linked answers 404, so the client shows it as failed.

## MCP tools

| Tool | Purpose |
|---|---|
| `list_accounts` | Linked accounts and the services each granted. `/mcp` only. |
| `link_account` | Returns a Google sign-in URL to link or re-link an account. `/mcp` only. |
| `gmail_search_threads` | Gmail search syntax; returns subject, sender, date, snippet and unread state per thread. |
| `gmail_get_thread` | Every message in a thread with plain-text bodies, oldest first. |
| `gmail_get_message` | One message with its body and attachment list. |
| `gmail_list_labels` | Labels and their IDs. |
| `calendar_list_calendars` | Calendars the account can see, with IDs and time zones. |
| `calendar_list_events` | Events in a time window, recurring events expanded, from now by default. |
| `calendar_get_event` | One event with its full description and attendees. |
| `calendar_free_busy` | Busy blocks for your calendars or colleagues' where shared. |
| `contacts_search` | People across saved contacts, auto-saved contacts and the Workspace directory. |
| `contacts_get` | One contact in full: addresses, birthday, notes, relations. |

On `/mcp`, every tool takes an optional `account`, which may be omitted while only one account
is linked. Account endpoints fix the account and drop the argument. A tool whose service the
account did not grant says so and asks for a re-link.

Message bodies are capped at 50,000 characters, and a thread at 100,000, dropping the oldest
bodies first. Event descriptions are shortened to 500 characters in lists.

## Roadmap

1. Gmail write tools: labels, archive, trash, permanent delete, read state, drafts.
2. REST API for Post Office over the same operations.
3. Gmail push notifications through Pub/Sub, forwarded to Post Office as webhooks.

## Development

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
```
