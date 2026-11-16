# cs

A small code search engine for AI agents. It strips comments and whitespace from source files, indexes them with [tantivy](https://github.com/quickwit-oss/tantivy), and returns matching lines in a compact text format.

## Build

```bash
cargo build --release
```

The binary is `target/release/cs`.

## Use

Run `cs` from the directory you want to search. It indexes and searches the current directory, and stores the index in `./.code-index`.

```bash
cs index            # build or rebuild the index
cs "add_action"     # search
cs "add_action" wp-content/plugins   # search only inside one folder
cs mcp              # run as an MCP server on stdio
```

The first search builds the index if none exists. After that, the index is only updated by `cs index`.

## Output

`# <path>` names a file. `L<line>:<code>` is a matching line with its line number in the original file.

```text
# src/Controller/Firewall.php
L42:public function __construct(){$this->register_routes();}
L89:add_action('init',[$this,'ip_lockout']);
```

A search returns at most 10 files and 8 lines per file.

## Ignore rules

`cs` reads `.csignore` files in the searched directory tree. They use `.gitignore` syntax. `cs` does not read `.gitignore`, and it does not skip hidden files, so list `.git/` and `.code-index/` yourself:

```gitignore
.git/
.code-index/
target/
node_modules/
*.min.js
```

## Queries

- Terms separated by spaces are OR'd.
- One identifier such as `register_routes` or `ip-lockout` matches as a whole.
- A query that contains `:` returns nothing. Search `Foo bar` instead of `Foo::bar`.
- A folder after the query limits the results to files under that folder. The folder is relative to the directory that was indexed.

## MCP tools

| Tool | Arguments | Result |
| :--- | :--- | :--- |
| `search_code` | `query` (string), optional `folder` (string) | Same text as the CLI search |
| `index_code` | none | Rebuilds the index |

### Register the server

The server searches the directory it starts in, so each client must start it in the directory you want to search. Replace `/path/to/cs` with the binary and `/path/to/project` with that directory.

Claude Code, `.mcp.json` in the project root (Claude Code starts the server in the project):

```json
{
  "mcpServers": {
    "cs": { "command": "/path/to/cs", "args": ["mcp"] }
  }
}
```

Codex, `.codex/config.toml` in a trusted project, or `~/.codex/config.toml`:

```toml
[mcp_servers.cs]
command = "/path/to/cs"
args = ["mcp"]
cwd = "/path/to/project"
```

Antigravity, the global `~/.gemini/config/mcp_config.json` (older versions: `~/.gemini/antigravity/mcp_config.json`). Add `cs` inside the existing `mcpServers` object:

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

With Compose, run these in the directory you want to search and point `-f` at this folder:

```bash
docker compose -f cs/docker-compose.yml run --rm cs index
docker compose -f cs/docker-compose.yml run --rm cs "add_action"
```

## Limits

The minifier does not parse strings. It cuts a line at `//`, also inside a URL, and at a `#` that starts a line or follows a space. Read the real file before you quote or edit code from a result.
