# dbx

A terminal database client for PostgreSQL and SQL Server. Built because lazysql
gets two things wrong for my workflow:

- **You can get stuck.** Walk into a table view and there is no reliable way
  back to the tree. Here, `esc` always pops one layer, and `ctrl+h/j/k/l` move
  between panes from *anywhere* — including from inside insert mode in the query
  editor. There is no pane that can trap the cursor.
- **No connection grouping.** DBeaver-style projects. Connections are grouped
  under `[[project]]` headings and the tree renders them that way.

Vim motions throughout, one binary, no config-file surprises.

## Install

```sh
cargo install --path .
```

## Adding a connection

Press `a` in the tree. You get a form, not a URL box:

```
┌ new connection ──────────────────────────────────┐
│   project     Globex                             │
│ ▸ name        warehouse                          │
│   engine      mssql          ‹h/l›               │
│   host        sql.internal                       │
│   port        1433                               │
│   database    Reporting                          │
│   user        svc_report                         │
│   password    •••••••                            │
│   trust cert  no                                 │
│   read only   no                                 │
│                                                  │
│   ✓ ok — 1 row(s) returned by `select 1`         │
│                                                  │
│   enter edit · h/l change · ctrl+t test · ctrl+s save │
└──────────────────────────────────────────────────┘
```

- `j/k` or `tab` move between fields, `enter` edits a text field, `esc` leaves
  the field (not the form), `ctrl+u` clears it.
- `h/l` or `space` cycle `engine` and `sslmode` and flip the toggles. Switching
  engine follows that engine's default port and hides fields that do not apply.
- `ctrl+t` tests the connection right there, using the password you just typed
  even though it isn't saved yet. Failures come with the same hints `--check`
  gives.
- `ctrl+s` saves. Validation errors appear inline and nothing is written.
- The `project` field is free text — type an existing group to join it, or a new
  name to create it.

| In the tree | |
|---|---|
| `a` | new connection in the selected project |
| `A` | new project group |
| `e` | edit the selected connection, or rename the selected project |
| `D` | delete the selected connection or project (with a confirmation that spells out what goes) |
| `t` | test the selected connection without opening the form |

Saves are surgical: `toml_edit` rewrites only the table being changed, so your
comments, ordering and formatting survive. The file is written to a temp sibling
and renamed, so an interrupted save cannot truncate it.

## Configure

You can still edit the file by hand; the form and the file are the same source
of truth.

```sh
dbx --config     # prints the config path; a sample is written on first run
```

Config lives in `$DBX_CONFIG_DIR`, else `$XDG_CONFIG_HOME/dbx`, else
`~/.config/dbx`.

```toml
[ui]
page_size = 500          # rows per page of table data, and max rows per query
max_column_width = 48
confirm_writes = true    # ask before running anything that is not a read

[[project]]
name = "Acme"

  [[project.connection]]
  name = "acme-local"
  engine = "postgres"      # postgres | mssql
  host = "127.0.0.1"
  port = 5432
  database = "acme"
  user = "postgres"
  keyring = "acme-local"   # macOS Keychain / Secret Service, service "dbx"
  sslmode = "prefer"       # disable | prefer | require

  [[project.connection]]
  name = "acme-reporting"
  engine = "mssql"
  host = "sql.internal"
  database = "Reporting"
  user = "svc_report"
  password_command = "op read op://vault/sql/password"
  trust_cert = true        # accept a self-signed server certificate
  read_only = true         # refuse anything that is not a read
```

### Passwords

Resolved in this order, first hit wins:

1. `keyring = "<account>"` — read from the OS keychain, service `dbx`.
2. `password_command = "<shell>"` — first line of stdout is the password.
3. `password = "..."` — cleartext, discouraged.

A password typed into the form goes straight to the keyring under the account
`<project>/<name>`, and the config only gets the pointer. You never have to run
`security` by hand, but you can:

```sh
security add-generic-password -s dbx -a "Globex/warehouse" -w
```

Two consequences of the account being `<project>/<name>`:

- Renaming a connection or moving it between projects moves the secret too.
- Renaming a project migrates every secret it manages. If one cannot be moved
  (the keychain denied the read), that connection keeps pointing at the old
  account and you get told which one — nothing silently breaks.

An account you chose yourself (anything other than `<project>/<name>`) is left
alone by all of the above, as is `password_command`. Editing a connection only
touches the password when you actually type in the password field; until then it
shows `(unchanged)`.

MSSQL uses SQL logins only. No Windows or Azure AD auth.

## Keys

`?` inside the app shows everything, live from your keymap. Overrides go in
`keys.toml` next to `config.toml`; `dbx --keys` prints the defaults as a
starting point (also checked in as `keys.example.toml`).

| | |
|---|---|
| `ctrl+h` / `ctrl+l` | tree ⇄ workspace |
| `ctrl+k` / `ctrl+j` | query editor ⇄ results |
| `esc` | up one layer — closes an overlay, leaves insert mode, or leaves the pane |
| `ctrl+t` / `ctrl+e` / `ctrl+g` | jump straight to tree / editor / grid |
| `ctrl+n` / `ctrl+w` | new query tab / close tab |
| `alt+h` / `alt+l` | previous / next tab |
| `ctrl+c` | cancel the running statement |
| `:` | command line (`:q`, `:limit 200`, `:new`, `:refresh`) |

**Tree:** `j/k` move, `l/h` expand/collapse, `enter` open, `s` structure,
`J/K` jump between connections, `/` filter (auto-expands to reveal matches),
`gg`/`G`, `ctrl+d`/`ctrl+u`, and `a` `A` `e` `D` `t` to manage connections and
projects (see above).

**Grid:** `hjkl`, `0`/`$`, `gg`/`G`, `ctrl+d`/`ctrl+u`, `H`/`L` previous/next
page, `y` yank cell, `Y` yank row, `enter` inspect a cell full-width, `/` `n`
`N` search.

**Query editor:** `i/a/I/A/o/O` insert, `esc` normal, `w/b`, `dd`, `x`, `u`,
`ctrl+r` run the statement under the cursor, `ctrl+a` run the whole buffer,
`[`/`]` walk query history.

Yanking uses OSC 52, so it reaches your local clipboard over SSH, and falls
back to `pbcopy`/`xclip`.

## Troubleshooting

```sh
dbx --check                # every connection
dbx --check reporting      # one, by name or project/name
```

It reports the password source, its length and a SHA-256 prefix (never the
value), warns about the things that look right in a config file but break login
— a stored trailing newline, an empty value, stray non-ASCII — then attempts the
connection and runs `select 1`.

```
Globex/reporting  (mssql)
  target        sql.internal:1433/Reporting
  user          globex
  password      keyring → 18 chars, sha256:3c9e01a7
  warning       surrounding whitespace: 0 leading, 1 trailing byte(s) — ...
  auth          SQL login (Windows/Azure AD is not supported)
  connect       FAILED: Login failed for user 'globex'.
  hint          the server accepted the connection and rejected the credential ...
```

`Login failed for user` (SQL Server 18456) means host, port and TLS are all
fine — only the credential was rejected. Usual causes: a trailing newline in the
stored secret, a Windows/domain account (unsupported — SQL logins only), a
disabled login, or a server set to Windows-authentication-only mode.

## Notes

- One tokio task per connection, so the UI never blocks on a slow query.
  `ctrl+c` sends a real Postgres cancel; on SQL Server it drops and reconnects,
  because TDS has no out-of-band cancel.
- `page_size` caps rows per query, so `select * from huge_table` cannot pull the
  whole thing into memory. The title bar says `truncated` when it clipped.
- Statement splitting understands quotes, `--`/`/* */` comments and Postgres
  dollar-quoting, so `ctrl+r` picks the right statement out of a script.

## Development

```sh
cargo test
cargo clippy --all-targets
cargo fmt
```

## License

MIT — see [LICENSE](LICENSE).
