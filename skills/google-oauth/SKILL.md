---
name: google-oauth
description: >
  Use when a Google integration (Gmail, Docs, Sheets, Drive, Calendar) needs its
  OAuth client set up, its refresh token obtained again after expiry, or an
  access token minted at runtime. Use when adding a Google account or choosing
  among several accounts.
---

# Google OAuth

The `google_account.py` script reads the account table, mints access tokens, and adds accounts.

## Configuration

- Credentials location: `$CHARLIEBOT_HOME/credentials.yaml`; the default home is `~/.charliebot`.
- The `google` section holds the OAuth client and the default account.
- The default account name is `google.name`, or `default` when that key is absent.
- Each added account uses a `google_account_<name>` section.
- Account names match `[a-z][a-z0-9-]*` and are unique across the table.
- An added account can hold its own `client_id` and `client_secret` pair.
- `google.docs_default_folder_id` applies only to the default account.
- Access tokens are minted at runtime and are not persisted.

## Runtime Token Mint

Mint an access token before the integration's API calls:

```bash
ACCESS_TOKEN=$(python3 <skill-dir>/scripts/google_account.py token --account <name>)
```

For the default account, omit `--account`. The command prints the access token on stdout.

## Accounts

A host holds one or more Google accounts. Each account has a short name, unique on the host.

- The `google` section of `~/.charliebot/credentials.yaml` holds the OAuth client and the default account.
- Each added account has its own section, `google_account_<name>`.
- An added account section may carry its own `client_id` and `client_secret`.
- `python3 <skill-dir>/scripts/google_account.py list` prints each account name and description.

Choose the account before each Google API call:

- When the request names an account, use that account.
- Match the request words against each account name and description.
- When the request names no account, use the default account.
- When the request words match no account, stop and show the account list.
- Name the account in each reply that carries Google data.
- When the request names no account, also name in the reply the accounts not checked.
- Name the target account in each write draft that waits for take off.

Mint the access token for the chosen account:

    ACCESS_TOKEN=$(python3 <skill-dir>/scripts/google_account.py token --account <name>)

For the default account, omit `--account`.

## Add or Re-Authorize an Account

The account owner gives consent in a browser. Only the owner can do this step.

1. Run `google_account.py consent-url <name>`. When you know the email address of the account, add `--email <email address>`.
2. Send the URL to the account owner. When the email address is still unknown, ask the owner for it in the same message.
3. The owner signs in to that account and approves.
4. The owner copies the full URL from the browser address bar after the redirect to `http://localhost`.
5. Run `google_account.py add <name> --email <email address> --redirect-url '<redirect URL>' --description '<one line>'`.
6. Report the email address that `add` prints.

`add` writes the token only when the consenting account has the given email address.

When Google shows the error `org_internal`, the OAuth client accepts only users of its organization.
Then put a Desktop-app `client_id` and `client_secret` into the `google_account_<name>` section, and start again at step 1.

## Bootstrap / Re-Authorization

1. In Google Cloud Console, enable the APIs you need (Gmail, Docs, Sheets, Drive, Calendar).
2. Create an OAuth client of type **Desktop app**.
3. Follow **Add or Re-Authorize an Account** to authorize an account. The consent flow requests these scopes:

```text
https://www.googleapis.com/auth/spreadsheets
https://www.googleapis.com/auth/documents
https://www.googleapis.com/auth/drive.file
https://www.googleapis.com/auth/gmail.readonly
https://www.googleapis.com/auth/calendar.readonly
```

If the GCP project is in Testing mode, the refresh token expires in about seven days. Publish the OAuth consent screen to Production for non-expiring tokens.
