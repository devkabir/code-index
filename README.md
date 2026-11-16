# Code search

Code search is a small code search engine for AI agents. It strips comments and indentation from source files, indexes them with [tantivy](https://github.com/quickwit-oss/tantivy), and returns matching lines and full definitions in a compact text format.

## Build

```bash
cargo build --release
```

The binary is `target/release/cs`.

## Use

Run `cs` from the directory you want to search. Code search indexes and searches the current directory, and it stores the index in `./.code-index`.

```bash
cs index            # build or rebuild the index
cs "add_action"     # search
cs "add_action" wp-content/plugins   # search only inside one folder
cs mcp              # run as an MCP server on stdio
```

The first search builds the index if none exists. After that, only `cs index` updates the index.

## Output

`# <path>` names a file. `L<line>:<code>` is a matching line with its line number in the original file. When a line defines a query term with `function`, `class`, `interface`, `trait`, `enum`, `struct`, `fn`, or `def`, code search prints `[def]` followed by the whole block up to its closing brace, up to 80 lines. Code search lists files with a definition first.

```text
# wp-includes/pluggable.php
[def]
L744:function wp_logout() {
L745:$user_id = get_current_user_id();
L759:do_action( 'wp_logout', $user_id );
L760:}
L738:if ( ! function_exists( 'wp_logout' ) ) :
# wp-login.php
L799:wp_logout();
```

A search returns at most 10 files and 8 plain lines per file. Code search marks truncated or stale results with these lines:

| Marker | Meaning |
| :--- | :--- |
| `[+N more lines]` | This file has `N` more matching lines. |
| `[showing N of M files]` | More files match the query. `M+` means at least `M` files match. |
| `[cut at 80 lines: ...]` | The definition continues in the source file. |
| `[STALE: ...]` | The file changed or was deleted after indexing. Run `cs index`. |
| `NO MATCHES ...` | No file has a whole-identifier match. |
| `ERROR: ...` | The search or index failed. In MCP mode, code search also sets `isError: true`. |

## Ignore rules

Code search reads `.csignore` files in the searched directory tree using `.gitignore` syntax. Code search does not read `.gitignore` and does not skip hidden files. It always skips `.git` and `.code-index`. Add any other paths you want to skip to `.csignore`:

```gitignore
target/
node_modules/
*.min.js
```

## Queries

- Code search splits terms on punctuation and requires every term. `Foo::bar` and `Foo bar` run the same query.
- Terms match whole identifiers without case sensitivity. `wp_logout` does not match `wp_logout_url` or `wp-logout`.
- Code search shows lines that contain every term. If no line contains every term, code search shows lines that contain any term.
- A folder argument after the query limits the results to files under that folder, relative to the indexed directory.

## MCP tools

| Tool | Arguments | Result |
| :--- | :--- | :--- |
| `search_code` | `query` string, optional `folder` string | Same text as the CLI search |
| `index_code` | none | Rebuilds the index |

A failed call returns `ERROR: ...` with `isError: true`. An unknown tool returns an error, and an unknown method returns a JSON-RPC `-32601` reply.

### Register the server

The server searches the directory it starts in, so each client must start it in the directory you want to search. Replace `/path/to/cs` with the binary path and `/path/to/project` with that directory.

For Claude Code, create `.mcp.json` in the project root. Claude Code starts the server in the project directory:

```json
{
  "mcpServers": {
    "cs": { "command": "/path/to/cs", "args": ["mcp"] }
  }
}
```

For Codex, add this section to `.codex/config.toml` in a trusted project or to `~/.codex/config.toml`:

```toml
[mcp_servers.cs]
command = "/path/to/cs"
args = ["mcp"]
cwd = "/path/to/project"
```

For Antigravity, edit `~/.gemini/config/mcp_config.json`, or `~/.gemini/antigravity/mcp_config.json` on older versions. Add `cs` to the `mcpServers` object:

```json
{
  "mcpServers": {
    "cs": {
      "command": "sh",
      "args": ["-c", "cd /path/to/project && exec /path/to/cs mcp"]
    }
  }
}
```

## Docker

```bash
docker build -t cs .
docker run --rm -v "$PWD:/workspace" cs index
docker run --rm -v "$PWD:/workspace" cs "add_action"
```

Run `docker build` in this folder. Run `docker run` in the directory you want to search.

With Docker Compose, run these commands in the directory you want to search and point `-f` at this folder:

```bash
docker compose -f code-index/docker-compose.yml run --rm cs index
docker compose -f code-index/docker-compose.yml run --rm cs "add_action"
```

## Limits

- Code search strips comments based on the file extension. It strips `//` and `/* */` in `php`, `js`, `jsx`, `mjs`, `ts`, `tsx`, `rs`, `c`, `h`, `cpp`, `java`, `go`, `scss`, and `less` files. It strips `#` at the start of a line or after a space, except `#[`, in `php`, `py`, `sh`, `rb`, `yml`, `yaml`, and `toml` files. Other files keep every line.
- Code search tracks quotes per line and keeps comment markers inside strings. A string that spans multiple lines or a Rust lifetime such as `'a` can leave a trailing comment in the index.
- Code search trims lines without reformatting them. Because the output omits indentation and comments, read the original file before you quote or edit code.
- Code search finds the end of a definition block by counting braces. Braces inside strings can end a block early or late. For languages without braces, code search shows the `def` line and the two lines after it.
